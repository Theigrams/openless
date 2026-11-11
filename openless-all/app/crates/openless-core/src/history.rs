//! Newest-first dictation history with retention and count caps.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::config::{Clock, SystemClock};
use crate::errors::{BackendError, BackendErrorCode};
use crate::host_document::{WindowContext, WindowScreenshot, WINDOW_SCREENSHOT_MAX_BYTES};
use crate::persistence::{atomic_write, persistence_error};
use crate::types::{DictationSession, HistorySource};

pub const HISTORY_CAP: usize = 200;

/// Content attached to the request that produced the final model output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryWindowContext {
    Screenshot(WindowScreenshot),
    AccessibilityText(WindowContext),
    NotAttached,
    ImageRejected,
}

impl HistoryWindowContext {
    pub(crate) fn from_attachment(
        text: Option<&WindowContext>,
        image: Option<&WindowScreenshot>,
    ) -> Option<Self> {
        image.cloned().map(Self::Screenshot).or_else(|| {
            text.and_then(WindowContext::for_prompt).map(Self::AccessibilityText)
        })
    }

    fn kind(&self) -> HistoryWindowContextKind {
        match self {
            Self::Screenshot(_) => HistoryWindowContextKind::Screenshot,
            Self::AccessibilityText(_) => HistoryWindowContextKind::AccessibilityText,
            Self::NotAttached => HistoryWindowContextKind::NotAttached,
            Self::ImageRejected => HistoryWindowContextKind::ImageRejected,
        }
    }

    pub fn preview(self) -> HistoryWindowContextPreview {
        match self {
            Self::Screenshot(image) => HistoryWindowContextPreview::Screenshot {
                image: image.data_url(),
            },
            Self::AccessibilityText(text) => HistoryWindowContextPreview::AccessibilityText { text },
            Self::NotAttached => HistoryWindowContextPreview::NotAttached,
            Self::ImageRejected => HistoryWindowContextPreview::ImageRejected,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HistoryWindowContextKind {
    Screenshot,
    AccessibilityText,
    NotAttached,
    ImageRejected,
}

impl HistoryWindowContextKind {
    fn extension(self) -> &'static str {
        match self {
            Self::Screenshot => "png",
            Self::AccessibilityText => "json",
            Self::NotAttached => "none",
            Self::ImageRejected => "rejected",
        }
    }

    fn from_extension(extension: &str) -> Option<Self> {
        match extension {
            "png" => Some(Self::Screenshot),
            "json" => Some(Self::AccessibilityText),
            "none" => Some(Self::NotAttached),
            "rejected" => Some(Self::ImageRejected),
            _ => None,
        }
    }
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum HistoryWindowContextPreview {
    Screenshot { image: String },
    AccessibilityText { text: WindowContext },
    NotAttached,
    ImageRejected,
}

pub struct HistoryStore {
    path: PathBuf,
    lock: Mutex<()>,
    /// 保留裁剪和「最近 N 分钟」都只认这一个时钟，而不是各自去读
    /// `chrono::Utc::now()`：后者会让注入固定时钟的后端（测试、回放）里
    /// `created_at` 与裁剪依据分成两套时间，条目可能在写入的瞬间就超期。
    /// `BackendRepositories::open` 时还不知道时钟来源，所以由后端随后用
    /// `attach_clock` 补上；没补就是系统时钟。
    clock: Mutex<Arc<dyn Clock>>,
}

impl HistoryStore {
    pub fn at_data_dir(data_dir: impl AsRef<Path>) -> Self {
        Self::at_path(data_dir.as_ref().join("history.json"))
    }

    pub fn at_path(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
            clock: Mutex::new(Arc::new(SystemClock)),
        }
    }

    /// 把后端注入的时钟交给历史仓。同一个仓只会被后端组装一次。
    pub(crate) fn attach_clock(&self, clock: Arc<dyn Clock>) {
        *self.clock.lock().expect("history clock lock") = clock;
    }

    fn now_utc(&self) -> chrono::DateTime<chrono::Utc> {
        self.clock.lock().expect("history clock lock").now_utc()
    }

    pub fn list(&self) -> Result<Vec<DictationSession>, BackendError> {
        let _guard = self.lock_store()?;
        self.read_locked()
    }

    pub(crate) fn sync_snapshot(
        &self,
        permit: &crate::cloud_sync_e2ee_store::gate::ExclusivePermit,
    ) -> Result<Vec<DictationSession>, BackendError> {
        crate::cloud_sync_e2ee_store::gate::require_exclusive(&self.path, permit)?;
        let _guard = self.lock_store()?;
        crate::persistence::read_lossless_rows(&self.path, &[])
    }

    /// Whole logical replacement deliberately bypasses both age and entry-count retention.
    pub(crate) fn sync_replace_all(
        &self,
        records: &[DictationSession],
        permit: &crate::cloud_sync_e2ee_store::gate::ExclusivePermit,
    ) -> Result<(), BackendError> {
        crate::cloud_sync_e2ee_store::gate::require_exclusive(&self.path, permit)?;
        let _guard = self.lock_store()?;
        let bytes = serde_json::to_vec_pretty(records)
            .map_err(|_| persistence_error("encode restored history"))?;
        crate::persistence::atomic_write_for_sync(&self.path, &bytes, permit)
    }

    pub fn append_with_retention(
        &self,
        session: DictationSession,
        retention_days: u32,
        max_entries: Option<u32>,
    ) -> Result<(), BackendError> {
        crate::cloud_sync_e2ee_store::gate::with_registered_mutation(
            &self.path,
            crate::cloud_sync_e2ee_store::gate::ChangeOrigin::User,
            || {
                let _guard = self.lock_store()?;
                let mut sessions = self.read_locked()?;
                sessions.insert(0, session);
                retain_with_policy(&mut sessions, retention_days, max_entries, self.now_utc());
                self.write_locked(&sessions)
            },
        )
    }

    /// Replace an in-progress record when its terminal result arrives, or
    /// append it when no provisional record exists (legacy/recovery path).
    pub fn upsert_with_retention(
        &self,
        session: DictationSession,
        retention_days: u32,
        max_entries: Option<u32>,
    ) -> Result<(), BackendError> {
        self.upsert_with_window_context(session, retention_days, max_entries, None)
    }

    pub(crate) fn upsert_with_window_context(
        &self,
        session: DictationSession,
        retention_days: u32,
        max_entries: Option<u32>,
        window_context: Option<&HistoryWindowContext>,
    ) -> Result<(), BackendError> {
        crate::cloud_sync_e2ee_store::gate::with_registered_mutation(
            &self.path,
            crate::cloud_sync_e2ee_store::gate::ChangeOrigin::User,
            || {
                let _guard = self.lock_store()?;
                let mut sessions = self.read_locked()?;
                let session_id = session.id.clone();
                if let Some(existing) = sessions.iter_mut().find(|item| item.id == session.id) {
                    let mut replacement = session;
                    if replacement.has_audio_recording.is_none() {
                        replacement.has_audio_recording = existing.has_audio_recording;
                    }
                    *existing = replacement;
                } else {
                    sessions.insert(0, session);
                }
                retain_with_policy(&mut sessions, retention_days, max_entries, self.now_utc());
                self.write_locked(&sessions)?;
                if let Some(context) = window_context {
                    if sessions.iter().any(|entry| entry.id == session_id) {
                        if let Err(error) = self.save_window_context_locked(&session_id, context) {
                            log::warn!("failed to save local window context: {error}");
                        }
                    }
                }
                Ok(())
            },
        )
    }

    pub fn contains(&self, id: &str) -> Result<bool, BackendError> {
        let _guard = self.lock_store()?;
        Ok(self.read_locked()?.iter().any(|session| session.id == id))
    }

    pub fn read_entry(&self, id: &str) -> Result<Option<DictationSession>, BackendError> {
        let _guard = self.lock_store()?;
        Ok(self
            .read_locked()?
            .into_iter()
            .find(|session| session.id == id))
    }

    pub fn recent_within_minutes(
        &self,
        minutes: u32,
    ) -> Result<Vec<DictationSession>, BackendError> {
        if minutes == 0 {
            return Ok(Vec::new());
        }
        let _guard = self.lock_store()?;
        let sessions = self.read_locked()?;
        let cutoff = self.now_utc() - chrono::Duration::minutes(i64::from(minutes));
        Ok(sessions
            .into_iter()
            .take_while(|session| {
                chrono::DateTime::parse_from_rfc3339(&session.created_at)
                    .map(|time| time.with_timezone(&chrono::Utc) >= cutoff)
                    .unwrap_or(true)
            })
            .collect())
    }

    pub fn delete(&self, id: &str) -> Result<(), BackendError> {
        crate::cloud_sync_e2ee_store::gate::with_registered_mutation(
            &self.path,
            crate::cloud_sync_e2ee_store::gate::ChangeOrigin::User,
            || {
                let _guard = self.lock_store()?;
                let mut sessions = self.read_locked()?;
                let before = sessions.len();
                sessions.retain(|session| session.id != id);
                if sessions.len() != before {
                    self.write_locked(&sessions)?;
                }
                Ok(())
            },
        )
    }

    pub fn update_entry(&self, updated: DictationSession) -> Result<bool, BackendError> {
        crate::cloud_sync_e2ee_store::gate::with_registered_mutation(
            &self.path,
            crate::cloud_sync_e2ee_store::gate::ChangeOrigin::User,
            || {
                let _guard = self.lock_store()?;
                let mut sessions = self.read_locked()?;
                let Some(slot) = sessions.iter_mut().find(|session| session.id == updated.id)
                else {
                    return Ok(false);
                };
                *slot = updated;
                self.write_locked(&sessions)?;
                Ok(true)
            },
        )
    }

    pub fn clear(&self) -> Result<(), BackendError> {
        crate::cloud_sync_e2ee_store::gate::with_registered_mutation(
            &self.path,
            crate::cloud_sync_e2ee_store::gate::ChangeOrigin::User,
            || {
                let _guard = self.lock_store()?;
                let quick_notes = self
                    .read_locked()?
                    .into_iter()
                    .filter(|session| session.source == HistorySource::QuickNote)
                    .collect::<Vec<_>>();
                self.write_locked(&quick_notes)
            },
        )
    }

    fn window_context_directory(&self) -> PathBuf {
        self.path.with_file_name("history-window-context")
    }

    /// Sidecars are local and never enter the synchronized history records.
    fn window_context_files_locked(
        &self,
    ) -> impl Iterator<Item = (String, HistoryWindowContextKind, PathBuf)> {
        fs::read_dir(self.window_context_directory())
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                if !entry.file_type().ok()?.is_file() {
                    return None;
                }
                let path = entry.path();
                let id = path.file_stem()?.to_str()?.to_owned();
                let uuid = uuid::Uuid::parse_str(&id).ok()?;
                if uuid.to_string() != id {
                    return None;
                }
                let kind = HistoryWindowContextKind::from_extension(path.extension()?.to_str()?)?;
                Some((id, kind, path))
            })
    }

    pub fn window_context_kinds(
        &self,
    ) -> Result<BTreeMap<String, HistoryWindowContextKind>, BackendError> {
        let _guard = self.lock_store()?;
        let sessions = self.read_locked()?;
        let ids: HashSet<&str> = sessions.iter().map(|entry| entry.id.as_str()).collect();
        Ok(self.window_context_files_locked()
            .filter(|(id, _, _)| ids.contains(id.as_str()))
            .map(|(id, kind, _)| (id, kind))
            .collect())
    }

    pub fn read_window_context(&self, id: &str) -> Result<Option<HistoryWindowContext>, BackendError> {
        let _guard = self.lock_store()?;
        if !self.read_locked()?.iter().any(|entry| entry.id == id) {
            return Ok(None);
        }
        let Some((_, kind, path)) = self.window_context_files_locked()
            .find(|(entry_id, _, _)| entry_id == id) else {
            return Ok(None);
        };
        let read = || {
            // A PNG is the largest supported payload. Bound reads even if a file was modified.
            use std::io::Read;
            let file = fs::File::open(&path).map_err(|_| persistence_error("open local window context"))?;
            let mut bytes = Vec::new();
            file.take(WINDOW_SCREENSHOT_MAX_BYTES as u64 + 1).read_to_end(&mut bytes)
                .map_err(|_| persistence_error("read local window context"))?;
            if bytes.len() > WINDOW_SCREENSHOT_MAX_BYTES {
                return Err(persistence_error("local window context too large"));
            }
            Ok(bytes)
        };
        let context = match kind {
            HistoryWindowContextKind::Screenshot => HistoryWindowContext::Screenshot(
                WindowScreenshot::from_png(&read()?)
                    .ok_or_else(|| persistence_error("decode local window screenshot"))?,
            ),
            HistoryWindowContextKind::AccessibilityText => HistoryWindowContext::AccessibilityText(
                serde_json::from_slice(&read()?)
                    .map_err(|_| persistence_error("decode local window text"))?,
            ),
            HistoryWindowContextKind::NotAttached => HistoryWindowContext::NotAttached,
            HistoryWindowContextKind::ImageRejected => HistoryWindowContext::ImageRejected,
        };
        Ok(Some(context))
    }

    fn save_window_context_locked(
        &self,
        id: &str,
        context: &HistoryWindowContext,
    ) -> Result<(), BackendError> {
        let uuid = uuid::Uuid::parse_str(id)
            .map_err(|_| persistence_error("invalid local window context identifier"))?;
        if uuid.to_string() != id {
            return Err(persistence_error("invalid local window context identifier"));
        }
        let path = self.window_context_directory().join(format!("{id}.{}", context.kind().extension()));
        match context {
            HistoryWindowContext::Screenshot(image) => atomic_write(&path, image.png())?,
            HistoryWindowContext::AccessibilityText(text) => atomic_write(
                &path,
                &serde_json::to_vec(text).map_err(|_| persistence_error("encode local window text"))?,
            )?,
            HistoryWindowContext::NotAttached | HistoryWindowContext::ImageRejected => {
                atomic_write(&path, &[])?
            }
        }
        for (entry_id, _, existing) in self.window_context_files_locked() {
            if entry_id == id && existing != path {
                self.remove_window_context_file(&existing);
            }
        }
        Ok(())
    }

    fn remove_window_context_file(&self, path: &Path) {
        if let Err(error) = fs::remove_file(path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!("failed to remove local window context: {}", error.kind());
            }
        }
    }

    pub(crate) fn prune_window_context_after_restore(&self) -> Result<(), BackendError> {
        let _guard = self.lock_store()?;
        self.prune_window_context_locked(&self.read_locked()?);
        Ok(())
    }

    fn prune_window_context_locked(&self, sessions: &[DictationSession]) {
        let ids: HashSet<&str> = sessions.iter().map(|entry| entry.id.as_str()).collect();
        for (id, _, path) in self.window_context_files_locked() {
            if !ids.contains(id.as_str()) {
                self.remove_window_context_file(&path);
            }
        }
    }

    fn lock_store(&self) -> Result<std::sync::MutexGuard<'_, ()>, BackendError> {
        self.lock.lock().map_err(|_| {
            BackendError::new(BackendErrorCode::Internal, "history store lock poisoned")
        })
    }

    fn read_locked(&self) -> Result<Vec<DictationSession>, BackendError> {
        crate::persistence::read_lossless_rows(&self.path, &[])
    }

    fn write_locked(&self, sessions: &[DictationSession]) -> Result<(), BackendError> {
        let json = serde_json::to_vec_pretty(sessions)
            .map_err(|_| persistence_error("encode history entries"))?;
        atomic_write(&self.path, &json)?;
        self.prune_window_context_locked(sessions);
        Ok(())
    }
}

fn retain_with_policy(
    sessions: &mut Vec<DictationSession>,
    retention_days: u32,
    max_entries: Option<u32>,
    now: chrono::DateTime<chrono::Utc>,
) {
    // Quick notes are intentionally outside the ordinary history retention
    // policy. A recording-start draft is also protected until it reaches a
    // terminal state, because its audio may be the only recoverable artifact
    // after a process crash.
    if retention_days > 0 {
        let cutoff = now - chrono::Duration::days(i64::from(retention_days));
        sessions.retain(|session| {
            session.source == HistorySource::QuickNote
                || session.error_code.as_deref() == Some("recording")
                || chrono::DateTime::parse_from_rfc3339(&session.created_at)
                    .map(|time| time.with_timezone(&chrono::Utc) >= cutoff)
                    .unwrap_or(true)
        });
    }
    let cap = max_entries
        .map(|count| (count as usize).clamp(5, HISTORY_CAP))
        .unwrap_or(HISTORY_CAP);
    let mut ordinary_seen = 0usize;
    sessions.retain(|session| {
        if session.source == HistorySource::QuickNote
            || session.error_code.as_deref() == Some("recording")
        {
            true
        } else {
            ordinary_seen += 1;
            ordinary_seen <= cap
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{HistoryInsertStatus, HistorySource, PolishMode};

    fn session(id: &str, created_at: String) -> DictationSession {
        DictationSession {
            id: id.into(),
            created_at,
            source: HistorySource::Voice,
            raw_transcript: "raw".into(),
            asr_transcript: None,
            final_text: "final".into(),
            mode: PolishMode::Light,
            style_pack_id: None,
            translation_active: false,
            polish_source: None,
            app_bundle_id: None,
            app_name: None,
            insert_status: HistoryInsertStatus::Inserted,
            error_code: None,
            duration_ms: Some(1000),
            dictionary_entry_count: None,
            has_audio_recording: None,
            asr_provider: None,
            asr_model: None,
            llm_provider: None,
            llm_model: None,
            pipeline_mode: None,
            asr_ms: None,
            polish_ms: None,
        }
    }

    #[test]
    fn window_context_previews_are_local_and_follow_history_retention() {
        use base64::Engine;
        let dir = std::env::temp_dir().join(format!("openless-window-history-{}", uuid::Uuid::new_v4()));
        let store = HistoryStore::at_data_dir(&dir);
        let image_id = uuid::Uuid::new_v4().to_string();
        let text_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now();
        let png = base64::engine::general_purpose::STANDARD.decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC",
        ).unwrap();
        let screenshot = HistoryWindowContext::Screenshot(WindowScreenshot::from_png(&png).unwrap());
        let text = WindowContext {
            title: Some("fixture window".into()),
            domain: None,
            visible_text: format!("  {}  ", "窗".repeat(4000)),
        };
        let text = HistoryWindowContext::from_attachment(Some(&text), None).unwrap();
        store.upsert_with_window_context(
            session(&image_id, (now - chrono::Duration::days(4)).to_rfc3339()),
            0, None, Some(&screenshot),
        ).unwrap();
        store.upsert_with_window_context(
            session(&text_id, now.to_rfc3339()), 0, None, Some(&text),
        ).unwrap();

        let kinds = store.window_context_kinds().unwrap();
        assert_eq!(kinds.get(&image_id), Some(&HistoryWindowContextKind::Screenshot));
        assert_eq!(kinds.get(&text_id), Some(&HistoryWindowContextKind::AccessibilityText));
        assert_eq!(store.read_window_context(&image_id).unwrap(), Some(screenshot));
        assert_eq!(store.read_window_context(&text_id).unwrap(), Some(text));
        let json = fs::read_to_string(dir.join("history.json")).unwrap();
        assert!(!json.contains("fixture window") && !json.contains("screenshot"));
        let preview = store.read_window_context(&text_id).unwrap().unwrap().preview();
        let json = serde_json::to_value(preview).unwrap();
        assert_eq!(json["kind"], "accessibilityText");
        assert_eq!(
            json["text"]["visibleText"].as_str().unwrap().chars().count(),
            crate::host_document::WINDOW_CONTEXT_PROMPT_CHARS,
        );

        let retry_id = uuid::Uuid::new_v4().to_string();
        store.upsert_with_window_context(
            session(&retry_id, now.to_rfc3339()),
            2, None, Some(&HistoryWindowContext::ImageRejected),
        ).unwrap();
        assert_eq!(store.read_window_context(&image_id).unwrap(), None);
        assert!(!dir.join("history-window-context").join(format!("{image_id}.png")).exists());
        assert_eq!(store.read_window_context(&retry_id).unwrap(), Some(HistoryWindowContext::ImageRejected));

        store.delete(&text_id).unwrap();
        assert_eq!(store.read_window_context(&text_id).unwrap(), None);
        assert!(!dir.join("history-window-context").join(format!("{text_id}.json")).exists());
        store.clear().unwrap();
        assert!(store.window_context_kinds().unwrap().is_empty());
        assert_eq!(fs::read_dir(dir.join("history-window-context")).unwrap().count(), 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_quick_note_history_survives_read_and_append() {
        let path = std::env::temp_dir().join(format!(
            "openless-legacy-history-{}.json",
            uuid::Uuid::new_v4()
        ));
        let mut legacy =
            serde_json::to_value(session("legacy", chrono::Utc::now().to_rfc3339())).unwrap();
        legacy["source"] = "quick_note".into();
        let bytes = serde_json::to_vec(&vec![legacy]).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let store = HistoryStore::at_path(path.clone());
        assert_eq!(store.list().unwrap()[0].source, HistorySource::QuickNote);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "reading must not rewrite history"
        );
        store
            .append_with_retention(session("new", chrono::Utc::now().to_rfc3339()), 0, None)
            .unwrap();
        let entries = store.list().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].source, HistorySource::QuickNote);
        assert_eq!(entries[1].raw_transcript, "raw");
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted[1]["source"], "quick_note");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn append_orders_caps_and_filters_retention() {
        let path = std::env::temp_dir().join(format!(
            "openless-core-history-{}.json",
            uuid::Uuid::new_v4().simple()
        ));
        let store = HistoryStore::at_path(path.clone());
        let old = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
        store
            .append_with_retention(session("old", old), 0, None)
            .unwrap();
        for index in 0..7 {
            store
                .append_with_retention(
                    session(&format!("new-{index}"), chrono::Utc::now().to_rfc3339()),
                    7,
                    Some(5),
                )
                .unwrap();
        }
        let sessions = store.list().unwrap();
        assert_eq!(sessions.len(), 5);
        assert_eq!(sessions[0].id, "new-6");
        assert!(sessions.iter().all(|session| session.id != "old"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn retention_follows_the_injected_clock_instead_of_the_wall_clock() {
        // CI 2026-09-27 之后这个场景一直是红的：裁剪偷读 `chrono::Utc::now()`，
        // 于是一个 injected 固定时钟（2026-08-28）+ retention 30 天的后端，会把
        // 刚写入的条目当成过期档删掉。
        let path = std::env::temp_dir().join(format!(
            "openless-history-clock-{}.json",
            uuid::Uuid::new_v4().simple()
        ));
        let store = HistoryStore::at_path(path.clone());
        let fixed = chrono::DateTime::parse_from_rfc3339("2026-08-28T12:34:56+00:00")
            .unwrap()
            .with_timezone(&chrono::Utc);
        store.attach_clock(Arc::new(crate::testing::FixedClock::new(
            fixed,
            fixed.date_naive(),
        )));

        store
            .append_with_retention(session("kept", fixed.to_rfc3339()), 30, Some(20))
            .unwrap();
        assert_eq!(
            store.list().unwrap().len(),
            1,
            "a fresh entry is never stale"
        );
        assert_eq!(
            store.recent_within_minutes(5).unwrap().len(),
            1,
            "the polish context window reads the same clock"
        );

        // 功能本身不能被放宽：同一个固定时钟下超期的条目照旧被裁掉。
        let stale = (fixed - chrono::Duration::days(31)).to_rfc3339();
        store
            .append_with_retention(session("stale", stale), 30, None)
            .unwrap();
        let ids: Vec<String> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        assert_eq!(ids, vec!["kept".to_string()]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn update_delete_clear_and_recent_queries_are_stable() {
        let path = std::env::temp_dir().join(format!(
            "openless-core-history-{}.json",
            uuid::Uuid::new_v4().simple()
        ));
        let store = HistoryStore::at_path(path.clone());
        let mut entry = session("one", chrono::Utc::now().to_rfc3339());
        store.append_with_retention(entry.clone(), 0, None).unwrap();
        assert_eq!(store.recent_within_minutes(5).unwrap(), vec![entry.clone()]);
        entry.final_text = "updated".into();
        assert!(store.update_entry(entry.clone()).unwrap());
        assert_eq!(store.list().unwrap(), vec![entry]);
        store.delete("one").unwrap();
        store.delete("missing").unwrap();
        assert!(store.list().unwrap().is_empty());
        store.clear().unwrap();
        assert!(store.recent_within_minutes(0).unwrap().is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn quick_notes_survive_ordinary_history_cap() {
        let path = std::env::temp_dir().join(format!(
            "openless-core-quick-note-retention-{}.json",
            uuid::Uuid::new_v4().simple()
        ));
        let store = HistoryStore::at_path(path.clone());
        let mut note = session("quick", chrono::Utc::now().to_rfc3339());
        note.source = HistorySource::QuickNote;
        store.append_with_retention(note, 1, Some(5)).unwrap();
        for index in 0..8 {
            store
                .append_with_retention(
                    session(
                        &format!("ordinary-{index}"),
                        chrono::Utc::now().to_rfc3339(),
                    ),
                    1,
                    Some(5),
                )
                .unwrap();
        }
        let sessions = store.list().unwrap();
        assert!(sessions.iter().any(|entry| entry.id == "quick"));
        assert_eq!(
            sessions
                .iter()
                .filter(|entry| entry.source == HistorySource::Voice)
                .count(),
            5
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn clearing_ordinary_history_preserves_quick_notes() {
        let path = std::env::temp_dir().join(format!(
            "openless-core-quick-note-clear-{}.json",
            uuid::Uuid::new_v4().simple()
        ));
        let store = HistoryStore::at_path(path.clone());
        store
            .append_with_retention(
                session("ordinary", chrono::Utc::now().to_rfc3339()),
                0,
                None,
            )
            .unwrap();
        let mut note = session("quick", chrono::Utc::now().to_rfc3339());
        note.source = HistorySource::QuickNote;
        store.append_with_retention(note, 0, None).unwrap();
        store.clear().unwrap();
        let entries = store.list().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "quick");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn upsert_preserves_an_existing_archived_audio_flag_when_update_omits_it() {
        let path = std::env::temp_dir().join(format!(
            "openless-core-quick-note-audio-flag-{}.json",
            uuid::Uuid::new_v4().simple()
        ));
        let store = HistoryStore::at_path(path.clone());
        let mut original = session("audio", chrono::Utc::now().to_rfc3339());
        original.has_audio_recording = Some(true);
        store.append_with_retention(original, 0, None).unwrap();
        let mut replacement = session("audio", chrono::Utc::now().to_rfc3339());
        replacement.has_audio_recording = None;
        store.upsert_with_retention(replacement, 0, None).unwrap();
        assert_eq!(store.list().unwrap()[0].has_audio_recording, Some(true));
        let _ = std::fs::remove_file(path);
    }
}
