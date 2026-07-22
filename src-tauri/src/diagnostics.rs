//! Freeze diagnostics.
//!
//! The app can appear to hang when a heavy agent run (research/encyclopedia
//! tasks are the densest event producers) coincides with a surface switch to a
//! native terminal pane. When the webview's main thread stalls, nothing on the
//! frontend can report it — so the durable record lives here, on the backend:
//!
//! - every diagnostic record is appended (JSONL) to
//!   `<workspace_root>/.qmux/diagnostics.log` as well as an in-memory ring, so
//!   a freeze the user has to kill the app over still leaves evidence on disk;
//! - `note_emit` instruments the one choke point every backend event passes
//!   through (`AppState::emit`), recording sustained event floods and
//!   individually slow IPC emits — the backend-side half of the picture;
//! - a heartbeat watchdog turns the *absence* of frontend heartbeats into a
//!   `frontend.stalled` record annotated with what the backend was emitting at
//!   the time, which is exactly the correlation a freeze investigation needs.
//!
//! The frontend half (heartbeats, breadcrumbs, main-thread stall detection)
//! reports into this module through the `diagnostics_*` commands below.

use crate::state::AppState;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Most recent records kept in memory for `diagnostics_snapshot`.
const RING_CAPACITY: usize = 1000;
/// Rotate the JSONL log once it grows past this (a single `.1` generation is
/// kept, so disk use is bounded at ~2x this).
const LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;
/// A single `app_handle.emit` call slower than this gets its own record: emit
/// serializes the payload and enqueues the IPC, so a slow one usually means the
/// webview side is backed up or the payload is enormous.
const SLOW_EMIT_MS: u128 = 100;
/// Event-rate accounting window.
const RATE_WINDOW_MS: u128 = 5_000;
/// Windows with at least this many events get a `backend.event_rate` record.
/// 250 per 5s window (50/s sustained) is well past normal interactive load and
/// squarely in "streaming agent burst" territory.
const RATE_RECORD_THRESHOLD: usize = 250;
/// How long the frontend may go without a heartbeat before the watchdog calls
/// it stalled. Heartbeats are sent every second; 6s absorbs GC pauses and
/// ordinary long frames while catching real beachballs quickly.
const STALL_AFTER_MS: u128 = 6_000;
/// Watchdog poll cadence.
const WATCHDOG_TICK_MS: u64 = 1_000;
/// Largest frontend record batch accepted per call, to bound memory/log growth
/// if a frontend loop goes haywire.
const MAX_BATCH_LEN: usize = 200;
/// Event types listed in a rate summary (highest count first).
const RATE_SUMMARY_TOP: usize = 8;

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticRecord {
    /// Milliseconds since the Unix epoch.
    pub timestamp: u128,
    /// "backend" or "frontend" — who observed this.
    pub source: String,
    /// Stable machine-readable kind, e.g. "backend.event_rate",
    /// "frontend.stalled", "frontend.main_thread_stall".
    pub category: String,
    /// Human-readable one-liner for reading the log directly.
    pub message: String,
    /// Structured context (counts, durations, breadcrumbs).
    #[serde(default)]
    pub data: Value,
}

/// Rolling per-event-type counters over a fixed window. Pure logic (caller
/// supplies timestamps) so the flood-detection thresholds are unit-testable.
struct EmitRateWindow {
    window_started_ms: u128,
    counts: HashMap<String, usize>,
    total: usize,
}

#[derive(Debug, PartialEq)]
struct EmitRateSummary {
    window_ms: u128,
    total: usize,
    /// (event type, count), highest count first, capped at RATE_SUMMARY_TOP.
    top: Vec<(String, usize)>,
}

impl EmitRateWindow {
    fn new(now_ms: u128) -> Self {
        Self {
            window_started_ms: now_ms,
            counts: HashMap::new(),
            total: 0,
        }
    }

    /// Counts one event. When the current window has elapsed, the window is
    /// reset (with this event as its first entry) and a summary of the closed
    /// window is returned iff it crossed the flood threshold.
    fn note(&mut self, event_type: &str, now_ms: u128) -> Option<EmitRateSummary> {
        let mut closed = None;
        if now_ms.saturating_sub(self.window_started_ms) >= RATE_WINDOW_MS {
            if self.total >= RATE_RECORD_THRESHOLD {
                closed = Some(self.summary(now_ms));
            }
            self.counts.clear();
            self.total = 0;
            self.window_started_ms = now_ms;
        }
        *self.counts.entry(event_type.to_string()).or_insert(0) += 1;
        self.total += 1;
        closed
    }

    /// Snapshot of the in-progress window, for annotating stall records with
    /// what the backend was emitting while the frontend was silent.
    fn summary(&self, now_ms: u128) -> EmitRateSummary {
        let mut top: Vec<(String, usize)> = self
            .counts
            .iter()
            .map(|(event_type, count)| (event_type.clone(), *count))
            .collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        top.truncate(RATE_SUMMARY_TOP);
        EmitRateSummary {
            window_ms: now_ms.saturating_sub(self.window_started_ms),
            total: self.total,
            top,
        }
    }
}

impl EmitRateSummary {
    fn to_json(&self) -> Value {
        json!({
            "windowMs": self.window_ms,
            "total": self.total,
            "byType": self
                .top
                .iter()
                .map(|(event_type, count)| json!({ "type": event_type, "count": count }))
                .collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, PartialEq)]
enum HeartbeatTransition {
    Stalled { gap_ms: u128 },
    Recovered { gap_ms: u128 },
}

/// Tracks frontend liveness from heartbeat timestamps. Pure logic (caller
/// supplies timestamps) so stall/recovery transitions are unit-testable.
/// Until the first beat arrives nothing is ever reported — a webview that
/// hasn't booted yet is not a stalled one.
struct HeartbeatMonitor {
    last_beat_ms: Option<u128>,
    stall_reported: bool,
}

impl HeartbeatMonitor {
    fn new() -> Self {
        Self {
            last_beat_ms: None,
            stall_reported: false,
        }
    }

    /// Frontend heartbeat arrived. Returns Recovered when it ends a reported
    /// stall, carrying the full silent gap.
    fn beat(&mut self, now_ms: u128) -> Option<HeartbeatTransition> {
        let transition = if self.stall_reported {
            self.stall_reported = false;
            Some(HeartbeatTransition::Recovered {
                gap_ms: now_ms.saturating_sub(self.last_beat_ms.unwrap_or(now_ms)),
            })
        } else {
            None
        };
        self.last_beat_ms = Some(now_ms);
        transition
    }

    /// Watchdog tick. Reports Stalled exactly once per silent period.
    fn check(&mut self, now_ms: u128) -> Option<HeartbeatTransition> {
        let last = self.last_beat_ms?;
        if self.stall_reported {
            return None;
        }
        let gap_ms = now_ms.saturating_sub(last);
        if gap_ms >= STALL_AFTER_MS {
            self.stall_reported = true;
            return Some(HeartbeatTransition::Stalled { gap_ms });
        }
        None
    }

    /// The whole process was suspended (machine slept, debugger paused it):
    /// pretend the frontend just beat so the dead time isn't misreported as a
    /// frontend stall.
    fn suppress_gap(&mut self, now_ms: u128) {
        if self.last_beat_ms.is_some() && !self.stall_reported {
            self.last_beat_ms = Some(now_ms);
        }
    }
}

struct DiagnosticsInner {
    log_path: PathBuf,
    log_max_bytes: u64,
    ring: Mutex<VecDeque<DiagnosticRecord>>,
    emit_window: Mutex<EmitRateWindow>,
    heartbeat: Mutex<HeartbeatMonitor>,
    /// Serializes append+rotate so two writers can't interleave a rotation.
    log_write: Mutex<()>,
    watchdog_spawned: AtomicBool,
}

#[derive(Clone)]
pub struct Diagnostics {
    inner: Arc<DiagnosticsInner>,
}

impl Diagnostics {
    pub fn new(workspace_root: &Path) -> Self {
        Self::with_log(
            workspace_root.join(".qmux").join("diagnostics.log"),
            LOG_MAX_BYTES,
        )
    }

    fn with_log(log_path: PathBuf, log_max_bytes: u64) -> Self {
        Self {
            inner: Arc::new(DiagnosticsInner {
                log_path,
                log_max_bytes,
                ring: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
                emit_window: Mutex::new(EmitRateWindow::new(now_ms())),
                heartbeat: Mutex::new(HeartbeatMonitor::new()),
                log_write: Mutex::new(()),
                watchdog_spawned: AtomicBool::new(false),
            }),
        }
    }

    pub fn log_path(&self) -> &Path {
        &self.inner.log_path
    }

    pub fn record(&self, source: &str, category: &str, message: impl Into<String>, data: Value) {
        self.push(DiagnosticRecord {
            timestamp: now_ms(),
            source: source.to_string(),
            category: category.to_string(),
            message: message.into(),
            data,
        });
    }

    fn push(&self, record: DiagnosticRecord) {
        if let Ok(mut ring) = self.inner.ring.lock() {
            if ring.len() >= RING_CAPACITY {
                ring.pop_front();
            }
            ring.push_back(record.clone());
        }
        self.append_to_log(&record);
    }

    /// Best-effort durable append. Diagnostics must never take the app down,
    /// so every IO failure here is swallowed.
    fn append_to_log(&self, record: &DiagnosticRecord) {
        let Ok(line) = serde_json::to_string(record) else {
            return;
        };
        let Ok(_guard) = self.inner.log_write.lock() else {
            return;
        };
        let path = &self.inner.log_path;
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(metadata) = fs::metadata(path) {
            if metadata.len() >= self.inner.log_max_bytes {
                let _ = fs::rename(path, path.with_extension("log.1"));
            }
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{line}");
        }
    }

    /// Called from `AppState::emit` for every backend event: feeds the flood
    /// detector and records individually slow IPC emits.
    pub fn note_emit(&self, event_type: &str, emit_duration: Duration) {
        let now = now_ms();
        let closed = self
            .inner
            .emit_window
            .lock()
            .ok()
            .and_then(|mut window| window.note(event_type, now));
        if let Some(summary) = closed {
            self.record(
                "backend",
                "backend.event_rate",
                format!(
                    "high event rate: {} events in {}ms",
                    summary.total, summary.window_ms
                ),
                summary.to_json(),
            );
        }
        if emit_duration.as_millis() >= SLOW_EMIT_MS {
            self.record(
                "backend",
                "backend.slow_emit",
                format!("emitting {event_type} took {}ms", emit_duration.as_millis()),
                json!({
                    "eventType": event_type,
                    "durationMs": emit_duration.as_millis(),
                }),
            );
        }
    }

    /// Frontend heartbeat (see `diagnostics_heartbeat`). Ends a reported stall
    /// with a `frontend.recovered` record carrying the silent gap.
    pub fn heartbeat_from_frontend(&self) {
        let now = now_ms();
        let transition = self
            .inner
            .heartbeat
            .lock()
            .ok()
            .and_then(|mut monitor| monitor.beat(now));
        if let Some(HeartbeatTransition::Recovered { gap_ms }) = transition {
            self.record(
                "backend",
                "frontend.recovered",
                format!("frontend heartbeats resumed after {gap_ms}ms"),
                json!({ "gapMs": gap_ms }),
            );
        }
    }

    /// Watchdog tick: flags a silent frontend, annotated with the event types
    /// the backend emitted during the in-progress rate window — the freeze's
    /// backend-side context.
    fn check_heartbeat(&self) {
        let now = now_ms();
        let transition = self
            .inner
            .heartbeat
            .lock()
            .ok()
            .and_then(|mut monitor| monitor.check(now));
        if let Some(HeartbeatTransition::Stalled { gap_ms }) = transition {
            let emitting = self
                .inner
                .emit_window
                .lock()
                .ok()
                .map(|window| window.summary(now).to_json())
                .unwrap_or(Value::Null);
            self.record(
                "backend",
                "frontend.stalled",
                format!("no frontend heartbeat for {gap_ms}ms"),
                json!({ "gapMs": gap_ms, "recentEmits": emitting }),
            );
        }
    }

    fn suppress_heartbeat_gap(&self) {
        if let Ok(mut monitor) = self.inner.heartbeat.lock() {
            monitor.suppress_gap(now_ms());
        }
    }

    pub fn snapshot(&self, limit: usize) -> Vec<DiagnosticRecord> {
        self.inner
            .ring
            .lock()
            .map(|ring| {
                let skip = ring.len().saturating_sub(limit);
                ring.iter().skip(skip).cloned().collect()
            })
            .unwrap_or_default()
    }
}

/// Starts the heartbeat watchdog thread (idempotent). Kept out of
/// `Diagnostics::new` so constructing an `AppState` in tests never spawns
/// threads.
pub fn spawn_watchdog(state: AppState) {
    let diagnostics = state.diagnostics().clone();
    if diagnostics
        .inner
        .watchdog_spawned
        .swap(true, Ordering::SeqCst)
    {
        return;
    }
    std::thread::Builder::new()
        .name("qmux-diagnostics-watchdog".into())
        .spawn(move || {
            let tick = Duration::from_millis(WATCHDOG_TICK_MS);
            let mut last_tick = now_ms();
            loop {
                std::thread::park_timeout(tick);
                let now = now_ms();
                // If this thread itself was silent for several ticks the whole
                // process was suspended (sleep, SIGSTOP); the frontend wasn't
                // stalled, the world was. Don't turn that into a false report.
                if now.saturating_sub(last_tick) >= 3 * WATCHDOG_TICK_MS as u128 {
                    diagnostics.suppress_heartbeat_gap();
                }
                last_tick = now;
                diagnostics.check_heartbeat();
            }
        })
        .map(|_| ())
        .unwrap_or_else(|err| {
            eprintln!("qmux: failed to spawn diagnostics watchdog: {err}");
        });
}

/// One record forwarded from the frontend diagnostics buffer.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrontendDiagnosticEntry {
    /// Frontend-observed time (ms since epoch); backend time is used if absent.
    pub timestamp: Option<u128>,
    pub category: String,
    pub message: String,
    #[serde(default)]
    pub data: Value,
}

#[tauri::command(async)]
pub fn diagnostics_record_batch(
    state: tauri::State<'_, AppState>,
    entries: Vec<FrontendDiagnosticEntry>,
) -> Result<(), String> {
    let diagnostics = state.diagnostics();
    for entry in entries.into_iter().take(MAX_BATCH_LEN) {
        diagnostics.push(DiagnosticRecord {
            timestamp: entry.timestamp.unwrap_or_else(now_ms),
            source: "frontend".to_string(),
            category: entry.category,
            message: entry.message,
            data: entry.data,
        });
    }
    Ok(())
}

#[tauri::command(async)]
pub fn diagnostics_heartbeat(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.diagnostics().heartbeat_from_frontend();
    Ok(())
}

#[tauri::command(async)]
pub fn diagnostics_snapshot(
    state: tauri::State<'_, AppState>,
    limit: Option<usize>,
) -> Result<Vec<DiagnosticRecord>, String> {
    Ok(state
        .diagnostics()
        .snapshot(limit.unwrap_or(RING_CAPACITY).min(RING_CAPACITY)))
}

#[tauri::command(async)]
pub fn diagnostics_log_path(state: tauri::State<'_, AppState>) -> Result<String, String> {
    Ok(state.diagnostics().log_path().display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_window_stays_quiet_under_the_threshold() {
        let mut window = EmitRateWindow::new(0);
        for i in 0..(RATE_RECORD_THRESHOLD - 1) {
            assert_eq!(window.note("turn.appended", (i as u128) % 100), None);
        }
        // Window elapses with total below threshold: reset, no summary.
        assert_eq!(window.note("turn.appended", RATE_WINDOW_MS), None);
        assert_eq!(window.total, 1);
    }

    #[test]
    fn rate_window_reports_a_flood_when_the_window_closes() {
        let mut window = EmitRateWindow::new(0);
        for _ in 0..RATE_RECORD_THRESHOLD {
            assert_eq!(window.note("turn.appended", 10), None);
        }
        for _ in 0..5 {
            assert_eq!(window.note("agent.status", 10), None);
        }
        let summary = window
            .note("pane.removed", RATE_WINDOW_MS + 1)
            .expect("closing a flooded window yields a summary");
        assert_eq!(summary.total, RATE_RECORD_THRESHOLD + 5);
        assert_eq!(summary.top[0].0, "turn.appended");
        assert_eq!(summary.top[0].1, RATE_RECORD_THRESHOLD);
        assert_eq!(summary.top[1], ("agent.status".to_string(), 5));
        // The closing event seeds the fresh window.
        assert_eq!(window.total, 1);
        assert_eq!(window.counts.get("pane.removed"), Some(&1));
    }

    #[test]
    fn rate_summary_caps_listed_types_and_sorts_by_count() {
        let mut window = EmitRateWindow::new(0);
        for i in 0..20 {
            for _ in 0..=i {
                window.note(&format!("type-{i:02}"), 1);
            }
        }
        let summary = window.summary(2);
        assert_eq!(summary.top.len(), RATE_SUMMARY_TOP);
        assert_eq!(summary.top[0], ("type-19".to_string(), 20));
        assert!(summary.top.windows(2).all(|pair| pair[0].1 >= pair[1].1));
    }

    #[test]
    fn heartbeat_monitor_never_reports_before_the_first_beat() {
        let mut monitor = HeartbeatMonitor::new();
        assert_eq!(monitor.check(STALL_AFTER_MS * 10), None);
    }

    #[test]
    fn heartbeat_monitor_reports_a_stall_once_and_the_recovery_gap() {
        let mut monitor = HeartbeatMonitor::new();
        assert_eq!(monitor.beat(1_000), None);
        assert_eq!(monitor.check(2_000), None);
        assert_eq!(
            monitor.check(1_000 + STALL_AFTER_MS),
            Some(HeartbeatTransition::Stalled {
                gap_ms: STALL_AFTER_MS
            })
        );
        // Still silent: no duplicate report.
        assert_eq!(monitor.check(1_000 + STALL_AFTER_MS * 2), None);
        // Heartbeats resume: recovery carries the full silent gap.
        assert_eq!(
            monitor.beat(1_000 + STALL_AFTER_MS * 3),
            Some(HeartbeatTransition::Recovered {
                gap_ms: STALL_AFTER_MS * 3
            })
        );
        // The next silence is a fresh stall.
        assert_eq!(
            monitor.check(1_000 + STALL_AFTER_MS * 4),
            Some(HeartbeatTransition::Stalled {
                gap_ms: STALL_AFTER_MS
            })
        );
    }

    #[test]
    fn suppressed_gaps_do_not_report_a_stall() {
        let mut monitor = HeartbeatMonitor::new();
        monitor.beat(1_000);
        monitor.suppress_gap(1_000 + STALL_AFTER_MS * 5);
        assert_eq!(monitor.check(1_000 + STALL_AFTER_MS * 5 + 1), None);
    }

    #[test]
    fn ring_keeps_only_the_newest_records() {
        let dir = std::env::temp_dir().join(format!("qmux-diag-ring-{}", std::process::id()));
        let diagnostics = Diagnostics::with_log(dir.join("diagnostics.log"), u64::MAX);
        for i in 0..(RING_CAPACITY + 10) {
            diagnostics.record("backend", "test", format!("record {i}"), Value::Null);
        }
        let snapshot = diagnostics.snapshot(RING_CAPACITY);
        assert_eq!(snapshot.len(), RING_CAPACITY);
        assert_eq!(snapshot[0].message, "record 10");
        assert_eq!(
            snapshot.last().map(|record| record.message.as_str()),
            Some(&*format!("record {}", RING_CAPACITY + 9))
        );
        let limited = diagnostics.snapshot(3);
        assert_eq!(limited.len(), 3);
        assert_eq!(limited[0].message, format!("record {}", RING_CAPACITY + 7));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn records_land_in_the_log_as_parseable_jsonl_and_rotate() {
        let dir = std::env::temp_dir().join(format!("qmux-diag-log-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let log_path = dir.join("diagnostics.log");
        // Tiny cap so the second record trips rotation.
        let diagnostics = Diagnostics::with_log(log_path.clone(), 10);
        diagnostics.record("backend", "test.first", "first", json!({ "n": 1 }));
        let line = fs::read_to_string(&log_path).expect("log written");
        let parsed: DiagnosticRecord =
            serde_json::from_str(line.lines().next().expect("one line")).expect("parseable");
        assert_eq!(parsed.category, "test.first");
        assert_eq!(parsed.data, json!({ "n": 1 }));

        diagnostics.record("backend", "test.second", "second", Value::Null);
        let rotated = fs::read_to_string(log_path.with_extension("log.1")).expect("rotated");
        assert!(rotated.contains("test.first"));
        let current = fs::read_to_string(&log_path).expect("fresh log");
        assert!(current.contains("test.second"));
        assert!(!current.contains("test.first"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn slow_emits_produce_records() {
        let dir = std::env::temp_dir().join(format!("qmux-diag-emit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let diagnostics = Diagnostics::with_log(dir.join("diagnostics.log"), u64::MAX);
        diagnostics.note_emit("turn.updated", Duration::from_millis(SLOW_EMIT_MS as u64));
        let snapshot = diagnostics.snapshot(10);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].category, "backend.slow_emit");
        assert_eq!(snapshot[0].data["eventType"], "turn.updated");
        let _ = fs::remove_dir_all(dir);
    }
}
