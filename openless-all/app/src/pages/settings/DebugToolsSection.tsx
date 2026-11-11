// Advanced → Debug tools: troubleshooting entries such as keeping raw recordings and exporting
// the error log.
// The recordAudioForDebug row was split out of RecordingSection in Settings.tsx;
// export-error-log migrated from AboutMini in SettingsModal — debug-related entries are
// centralized here.

import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { debugReadCursorContext, debugReadWindowContext, debugReadWindowScreenshot, exportErrorLog } from '../../lib/ipc';
import { useMobileLayout } from '../../lib/useMobileLayout';
import type { HostDocumentReadResult, WindowContextReadResult } from '../../lib/types';
import { useHotkeySettings } from '../../state/HotkeySettingsContext';
import { Btn, Card } from '../_atoms';
import { SettingRow, Toggle, inputStyle } from './shared';

const clamp = (n: number, min: number, max: number) => Math.max(min, Math.min(max, n));
type ProbeKind = 'cursor' | 'window' | 'screenshot';
const PROBE_DELAY_SECONDS: Record<ProbeKind, number> = { cursor: 5, window: 3, screenshot: 3 };
const probeResultStyle = {
  fontSize: 11,
  fontFamily: 'var(--ol-font-mono)',
  lineHeight: 1.7,
  padding: '8px 10px',
  borderRadius: 'var(--ol-control-radius)',
  background: 'var(--ol-surface-2)',
  border: '0.5px solid var(--ol-line-strong)',
  maxWidth: 420,
  overflowWrap: 'anywhere',
} as const;

export function DebugToolsSection({ showWindowProbe }: { showWindowProbe: boolean }) {
  const { t } = useTranslation();
  const mobile = useMobileLayout();
  const { prefs, updatePrefs: savePrefs } = useHotkeySettings();
  const [exportStatus, setExportStatus] = useState<'idle' | 'busy' | 'ok' | 'err'>('idle');
  const [exportMessage, setExportMessage] = useState<string>('');
  const exportTimerRef = useRef<number | null>(null);
  const [activeProbe, setActiveProbe] = useState<ProbeKind | null>(null);
  const [probeCountdown, setProbeCountdown] = useState(0);
  const [probeResult, setProbeResult] = useState<HostDocumentReadResult | null>(null);
  const [windowProbeResult, setWindowProbeResult] = useState<WindowContextReadResult | null>(null);
  const [screenshot, setScreenshot] = useState<string | null>(null);
  const [probeError, setProbeError] = useState<{ kind: ProbeKind; message: string } | null>(null);
  const probeTimerRef = useRef<number | null>(null);

  useEffect(
    () => () => {
      if (exportTimerRef.current) clearTimeout(exportTimerRef.current);
      if (probeTimerRef.current) clearInterval(probeTimerRef.current);
    },
    [],
  );

  // Delay the native read until the user has switched away from OpenLess settings.
  const onProbeContext = async (kind: ProbeKind) => {
    const delaySeconds = PROBE_DELAY_SECONDS[kind];
    if (kind === 'cursor') setProbeResult(null);
    else if (kind === 'window') setWindowProbeResult(null);
    else setScreenshot(null);
    setProbeError(null);
    setActiveProbe(kind);
    setProbeCountdown(delaySeconds);
    if (probeTimerRef.current) clearInterval(probeTimerRef.current);
    probeTimerRef.current = window.setInterval(() => {
      setProbeCountdown((prev) => {
        if (prev <= 1 && probeTimerRef.current) clearInterval(probeTimerRef.current);
        return Math.max(0, prev - 1);
      });
    }, 1000);
    try {
      const delayMs = delaySeconds * 1000;
      if (kind === 'cursor') setProbeResult(await debugReadCursorContext(delayMs));
      else if (kind === 'window') setWindowProbeResult(await debugReadWindowContext(delayMs));
      else setScreenshot(await debugReadWindowScreenshot(delayMs));
    } catch (err) {
      setProbeError({ kind, message: err instanceof Error ? err.message : String(err) });
    } finally {
      setActiveProbe(null);
      setProbeCountdown(0);
      if (probeTimerRef.current) clearInterval(probeTimerRef.current);
    }
  };

  const probeButton = (kind: ProbeKind) => (
    <Btn
      variant="ghost"
      size="sm"
      disabled={activeProbe !== null}
      onClick={() => void onProbeContext(kind)}
    >
      {activeProbe !== kind
        ? t(kind === 'screenshot' ? 'settings.dataStorage.windowScreenshotProbeBtn' : 'settings.debug.cursorProbeBtn', { n: PROBE_DELAY_SECONDS[kind] })
        : probeCountdown > 0
          ? t('settings.debug.cursorProbeCountdown', { n: probeCountdown })
          : t('common.loading')}
    </Btn>
  );

  const renderProbeStatus = ({
    status,
    reason,
    elapsedMs,
  }: HostDocumentReadResult | WindowContextReadResult) => (
    <div>
      <b>{status}</b>
      {reason ? ` — ${reason}` : ''}
      {` · ${elapsedMs}ms`}
    </div>
  );

  const renderProbeError = (kind: ProbeKind) =>
    probeError?.kind === kind && (
      <div role="alert" style={{ fontSize: 11, color: 'var(--ol-err)' }}>
        {probeError.message}
      </div>
    );

  const onExportLog = async () => {
    setExportStatus('busy');
    setExportMessage('');
    try {
      const ts = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19);
      const target = await exportErrorLog(`openless-${ts}.log`);
      if (target == null) {
        setExportStatus('idle');
        return;
      }
      setExportStatus('ok');
      setExportMessage(target);
      if (exportTimerRef.current) clearTimeout(exportTimerRef.current);
      exportTimerRef.current = window.setTimeout(() => setExportStatus('idle'), 4000);
    } catch (err) {
      setExportStatus('err');
      setExportMessage(err instanceof Error ? err.message : String(err));
    }
  };

  if (!prefs) {
    return (
      <Card>
        <div style={{ fontSize: 12, color: 'var(--ol-ink-4)' }}>{t('common.loading')}</div>
      </Card>
    );
  }

  const onRecordAudioForDebugChange = (recordAudioForDebug: boolean) => {
    // 打开主开关时如果两项都没勾，默认勾上「处理后」，否则开了开关却什么都不写。
    const keepSomething =
      prefs.debugArchiveProcessed || prefs.debugArchiveRaw || !recordAudioForDebug;
    void savePrefs({
      ...prefs,
      recordAudioForDebug,
      debugArchiveProcessed: keepSomething ? prefs.debugArchiveProcessed : true,
    });
  };
  const onDebugArchiveProcessedChange = (debugArchiveProcessed: boolean) =>
    void savePrefs({ ...prefs, debugArchiveProcessed });
  const onDebugArchiveRawChange = (debugArchiveRaw: boolean) =>
    void savePrefs({ ...prefs, debugArchiveRaw });
  // Empty means unlimited, falling back to null → the backend applies its 200 default.
  const onAudioRecordingMaxEntriesChange = (raw: string) => {
    const trimmed = raw.trim();
    if (trimmed === '') {
      void savePrefs({ ...prefs, audioRecordingMaxEntries: null });
      return;
    }
    const parsed = Number.parseInt(trimmed, 10);
    if (Number.isNaN(parsed)) return;
    void savePrefs({ ...prefs, audioRecordingMaxEntries: clamp(parsed, 1, 200) });
  };

  return (
    <Card>
      <SettingRow label={t('settings.recording.recordAudioForDebugLabel')}>
        <Toggle on={prefs.recordAudioForDebug} onToggle={onRecordAudioForDebugChange} />
      </SettingRow>
      {prefs.recordAudioForDebug && (
        <>
          <SettingRow
            label={t('settings.recording.debugArchiveProcessedLabel')}
            desc={t('settings.recording.debugArchiveProcessedDesc')}
          >
            <Toggle
              on={prefs.debugArchiveProcessed}
              onToggle={() => onDebugArchiveProcessedChange(!prefs.debugArchiveProcessed)}
            />
          </SettingRow>
          <SettingRow
            label={t('settings.recording.debugArchiveRawLabel')}
            desc={t('settings.recording.debugArchiveRawDesc')}
          >
            <Toggle
              on={prefs.debugArchiveRaw}
              onToggle={() => onDebugArchiveRawChange(!prefs.debugArchiveRaw)}
            />
          </SettingRow>
        </>
      )}
      <SettingRow label={t('settings.recording.audioRecordingMaxEntriesLabel')}>
        <div
          style={{
            display: 'flex',
            flexDirection: mobile ? 'column' : 'row',
            gap: 8,
            alignItems: mobile ? 'stretch' : 'center',
            width: '100%',
          }}
        >
          <input
            type="number"
            min={1}
            max={200}
            placeholder="200"
            value={prefs.audioRecordingMaxEntries ?? ''}
            onChange={(e) => onAudioRecordingMaxEntriesChange(e.target.value)}
            style={{ ...inputStyle, width: mobile ? '100%' : 80, textAlign: 'right' }}
            disabled={!prefs.recordAudioForDebug}
          />
          {mobile && (
            <span style={{ fontSize: 11, color: 'var(--ol-ink-4)', lineHeight: 1.45 }}>
              {t('settings.recording.audioRecordingMaxEntriesDesc')}
            </span>
          )}
        </div>
      </SettingRow>
      {/* Cursor context probe. Milestone 1's deliverable "see with your own eyes what it reads in
          each app" — without this entry point, that command might as well not exist. */}
      <SettingRow
        label={t('settings.debug.cursorProbeLabel')}
        desc={t('settings.debug.cursorProbeDesc')}
      >
        <div style={{ display: 'grid', gap: 6, minWidth: 0 }}>
          <div>{probeButton('cursor')}</div>
          {renderProbeError('cursor')}
          {probeResult && (
            <div style={probeResultStyle}>
              {renderProbeStatus(probeResult)}
              <div style={{ color: 'var(--ol-ink-4)' }}>
                {probeResult.appName ?? '?'} ({probeResult.bundleId ?? '?'})
              </div>
              {probeResult.window && (
                <div style={{ marginTop: 4, whiteSpace: 'pre-wrap' }}>
                  {probeResult.window.text.slice(0, probeResult.window.cursor)}
                  <span
                    style={{ color: 'var(--ol-blue)', fontWeight: 'var(--ol-font-weight-bold)' }}
                  >
                    ⟦{t('settings.debug.cursorLabel')}⟧
                  </span>
                  {probeResult.window.text.slice(probeResult.window.cursor)}
                </div>
              )}
            </div>
          )}
        </div>
      </SettingRow>
      {showWindowProbe && (
        <SettingRow
          label={t('settings.debug.windowProbeLabel')}
          desc={t('settings.debug.windowProbeDesc', { n: PROBE_DELAY_SECONDS.window })}
        >
          <div style={{ display: 'grid', gap: 6, minWidth: 0 }}>
            <div>{probeButton('window')}</div>
            {renderProbeError('window')}
            {windowProbeResult && (
              <div style={probeResultStyle}>
                {renderProbeStatus(windowProbeResult)}
                <div style={{ color: 'var(--ol-ink-4)' }}>
                  {windowProbeResult.appName ?? windowProbeResult.bundleId ?? '?'}
                </div>
                {windowProbeResult.window && (
                  <>
                    <div>
                      {t('settings.debug.windowProbeTitle')} ·{' '}
                      {windowProbeResult.window.title ?? '—'}
                    </div>
                    <div>
                      {t('settings.debug.windowProbeDomain')} ·{' '}
                      {windowProbeResult.window.domain ?? '—'}
                    </div>
                    <div style={{ marginTop: 4 }}>{t('settings.debug.windowProbeText')}</div>
                    <div style={{ maxHeight: 240, overflowY: 'auto', whiteSpace: 'pre-wrap' }}>
                      {windowProbeResult.window.visibleText}
                    </div>
                  </>
                )}
              </div>
            )}
          </div>
        </SettingRow>
      )}
      {showWindowProbe && (
        <SettingRow
          label={t('settings.dataStorage.windowContextScreenshot')}
          desc={t('settings.dataStorage.windowScreenshotProbeDesc')}
        >
          <div style={{ display: 'grid', gap: 6, minWidth: 0 }}>
            <div>{probeButton('screenshot')}</div>
            {renderProbeError('screenshot')}
            {screenshot && (
              <img
                src={screenshot}
                alt={t('settings.dataStorage.windowContextScreenshot')}
                style={{ display: 'block', maxWidth: 'min(420px, 100%)', maxHeight: 300 }}
              />
            )}
          </div>
        </SettingRow>
      )}
      <SettingRow label={t('modal.about.exportErrorLog')}>
        <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
          <Btn variant="ghost" size="sm" disabled={exportStatus === 'busy'} onClick={onExportLog}>
            {exportStatus === 'busy'
              ? t('modal.about.exporting')
              : t('modal.about.exportErrorLogBtn')}
          </Btn>
          {exportStatus === 'ok' && (
            <span
              style={{
                fontSize: 11,
                color: 'var(--ol-ok)',
                whiteSpace: mobile ? 'normal' : 'nowrap',
                overflow: mobile ? 'visible' : 'hidden',
                textOverflow: mobile ? 'clip' : 'ellipsis',
                wordBreak: 'break-word',
                maxWidth: mobile ? '100%' : 220,
              }}
              title={exportMessage}
            >
              {t('modal.about.exportSuccess')}
              {exportMessage ? `：${exportMessage}` : ''}
            </span>
          )}
          {exportStatus === 'err' && (
            <span
              style={{
                fontSize: 11,
                color: 'var(--ol-err)',
                lineHeight: 1.45,
                wordBreak: 'break-word',
                maxWidth: mobile ? '100%' : 280,
              }}
              title={exportMessage}
            >
              {t('modal.about.exportFailed')}
              {exportMessage ? `：${exportMessage}` : ''}
            </span>
          )}
        </div>
      </SettingRow>
    </Card>
  );
}
