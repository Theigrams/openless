//! Recent ASR hint payloads for the local debug panel.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Mutex;

use serde::Serialize;
use serde_json::Value;

use crate::types::SessionId;

const MAX_REQUESTS: usize = 20;
static RECENT: Mutex<VecDeque<AsrHotwordRequest>> = Mutex::new(VecDeque::new());

tokio::task_local! {
    static REQUEST: (SessionId, String);
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AsrHotwordRequest {
    session_id: SessionId,
    provider: String,
    sent_at: String,
    hints: Value,
}

pub(crate) async fn scope<T>(
    session_id: SessionId,
    provider: String,
    future: impl Future<Output = T>,
) -> T {
    REQUEST.scope((session_id, provider), future).await
}

/// Record after the request is sent. Identical payloads for subsequent chunks
/// of one session need only one entry.
pub(crate) fn record(hints: Value) {
    let _ = REQUEST.try_with(|(session_id, provider)| {
        let mut entries = RECENT.lock().unwrap_or_else(|error| error.into_inner());
        if entries.iter().any(|entry| {
            entry.session_id == *session_id && entry.provider == *provider && entry.hints == hints
        }) {
            return;
        }
        entries.push_front(AsrHotwordRequest {
            session_id: *session_id,
            provider: provider.clone(),
            sent_at: chrono::Utc::now().to_rfc3339(),
            hints,
        });
        entries.truncate(MAX_REQUESTS);
    });
}

pub fn recent_requests() -> Vec<AsrHotwordRequest> {
    RECENT
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .cloned()
        .collect()
}
