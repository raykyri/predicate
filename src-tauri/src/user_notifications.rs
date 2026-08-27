use crate::events::QmuxEvent;
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::Manager;

const MAX_TITLE_CHARS: usize = 120;
const MAX_BODY_CHARS: usize = 4_096;
const MIN_TIMEOUT_MS: u64 = 1_000;
const MAX_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const RATE_WINDOW: Duration = Duration::from_secs(10);
const RATE_LIMIT: usize = 10;
const EXTERNAL_RATE_KEY: &str = "\0external";
const MAX_LOG_ENTRIES: usize = 200;

static RECENT_REQUESTS: LazyLock<Mutex<HashMap<String, VecDeque<Instant>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum NotificationMode {
    Auto,
    Native,
    Overlay,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum NotificationTone {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SendNotificationRequest {
    #[serde(default)]
    pub title: Option<String>,
    pub body: String,
    #[serde(default = "default_mode")]
    pub mode: NotificationMode,
    #[serde(default = "default_tone")]
    pub tone: NotificationTone,
    #[serde(default)]
    pub sound: Option<bool>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationPermissionInfo {
    pub supported: bool,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NotificationLogEntry {
    pub id: String,
    pub title: String,
    pub body: String,
    pub tone: NotificationTone,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    pub created_at: i64,
    #[serde(default)]
    pub read: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationLog {
    #[serde(default)]
    pub entries: Vec<NotificationLogEntry>,
}

impl NotificationLog {
    /// serde skip guard: an untouched log serializes to nothing, so state
    /// files from builds that predate it round-trip byte-identically.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

pub(crate) fn record_log_entry(log: &mut NotificationLog, entry: NotificationLogEntry) {
    log.entries.retain(|existing| existing.id != entry.id);
    log.entries.push(entry);
    let extra = log.entries.len().saturating_sub(MAX_LOG_ENTRIES);
    if extra > 0 {
        log.entries.drain(..extra);
    }
}

fn default_mode() -> NotificationMode {
    NotificationMode::Auto
}

fn default_tone() -> NotificationTone {
    NotificationTone::Info
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

fn validate_text(
    value: &str,
    label: &str,
    max_chars: usize,
    allow_empty: bool,
) -> Result<(), String> {
    let count = value.chars().count();
    if !allow_empty && value.trim().is_empty() {
        return Err(format!("notification {label} must not be empty"));
    }
    if count > max_chars {
        return Err(format!(
            "notification {label} is too long ({count} characters; maximum {max_chars})"
        ));
    }
    if value.chars().any(|character| character == '\0') {
        return Err(format!(
            "notification {label} must not contain NUL characters"
        ));
    }
    Ok(())
}

fn validate_request(request: &SendNotificationRequest) -> Result<(), String> {
    validate_text(&request.body, "body", MAX_BODY_CHARS, false)?;
    if request
        .body
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err("notification body contains unsupported control characters".to_string());
    }
    if let Some(title) = request.title.as_deref() {
        validate_text(title, "title", MAX_TITLE_CHARS, true)?;
        if title.chars().any(char::is_control) {
            return Err("notification title must be a single line".to_string());
        }
    }
    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&request.timeout_ms) {
        return Err(format!(
            "notification timeout must be between {} and {} seconds",
            MIN_TIMEOUT_MS / 1_000,
            MAX_TIMEOUT_MS / 1_000
        ));
    }
    Ok(())
}

fn check_rate_limit(source_pane_id: Option<&str>) -> Result<(), String> {
    let key = source_pane_id.unwrap_or(EXTERNAL_RATE_KEY).to_string();
    let now = Instant::now();
    let mut all = RECENT_REQUESTS
        .lock()
        .map_err(|_| "notification rate limiter lock poisoned".to_string())?;
    all.retain(|_, recent| {
        while recent
            .front()
            .is_some_and(|instant| now.duration_since(*instant) >= RATE_WINDOW)
        {
            recent.pop_front();
        }
        !recent.is_empty()
    });
    let recent = all.entry(key).or_default();
    if recent.len() >= RATE_LIMIT {
        return Err("notification rate limit exceeded; try again in a few seconds".to_string());
    }
    recent.push_back(now);
    Ok(())
}

fn default_title(state: &AppState, source_pane_id: Option<&str>) -> String {
    let candidate = source_pane_id
        .and_then(|pane_id| {
            state
                .list_panes()
                .ok()?
                .into_iter()
                .find(|pane| pane.id == pane_id)
        })
        .map(|pane| pane.title)
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| "qmux".to_string());
    // Terminal titles originate in child processes, not in this command's
    // validated payload. Collapse control/whitespace and cap them before they
    // cross into either AppKit or the DOM.
    let without_controls = candidate
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let normalized = without_controls
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let normalized = normalized.chars().take(MAX_TITLE_CHARS).collect::<String>();
    if normalized.is_empty() {
        "qmux".to_string()
    } else {
        normalized
    }
}

fn overlay_event(
    state: &AppState,
    source_pane_id: Option<&str>,
    id: &str,
    title: &str,
    created_at: i64,
    request: &SendNotificationRequest,
) {
    state.emit(QmuxEvent::new(
        "app.notification_requested",
        source_pane_id.map(str::to_string),
        None,
        json!({
            "id": id,
            "title": title,
            "body": request.body,
            "tone": request.tone,
            "sound": request.sound.unwrap_or(false),
            "timeoutMs": request.timeout_ms,
            "createdAt": created_at,
        }),
    ));
}

fn emit_log_changed(state: &AppState, log: &NotificationLog) {
    state.emit(QmuxEvent::new(
        "app.notification_log_changed",
        None,
        None,
        json!({ "entries": log.entries }),
    ));
}

fn main_window_is_focused(state: &AppState) -> bool {
    let Some(app) = state.app_handle() else {
        return false;
    };
    let Some(window) = app.get_webview_window("main") else {
        return false;
    };
    window.is_visible().unwrap_or(false)
        && !window.is_minimized().unwrap_or(true)
        && window.is_focused().unwrap_or(false)
}

pub fn dispatch(
    state: &AppState,
    source_pane_id: Option<&str>,
    request: SendNotificationRequest,
) -> Result<serde_json::Value, String> {
    validate_request(&request)?;
    check_rate_limit(source_pane_id)?;
    let title = request
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| default_title(state, source_pane_id));

    let id = state.next_id("notification");
    let created_at = now_ms();
    let entry = NotificationLogEntry {
        id: id.clone(),
        title: title.clone(),
        body: request.body.clone(),
        tone: request.tone,
        pane_id: source_pane_id.map(str::to_string),
        created_at,
        read: false,
    };
    let log = state.append_notification_log(entry)?;
    emit_log_changed(state, &log);

    let wants_overlay = request.mode == NotificationMode::Overlay
        || (request.mode == NotificationMode::Auto && main_window_is_focused(state));
    if wants_overlay {
        overlay_event(state, source_pane_id, &id, &title, created_at, &request);
        return Ok(json!({ "accepted": true, "delivery": "overlay" }));
    }

    match show_native(state, source_pane_id, &title, &request) {
        Ok(()) => Ok(json!({ "accepted": true, "delivery": "native" })),
        Err(error) => {
            overlay_event(state, source_pane_id, &id, &title, created_at, &request);
            Ok(json!({
                "accepted": true,
                "delivery": "overlay",
                "fallbackReason": error,
            }))
        }
    }
}

#[cfg(target_os = "macos")]
fn show_native(
    state: &AppState,
    source_pane_id: Option<&str>,
    title: &str,
    request: &SendNotificationRequest,
) -> Result<(), String> {
    use mac_usernotifications::{AuthorizationStatus, Notification};

    let settings = mac_usernotifications::blocking::get_notification_settings()
        .map_err(|error| format!("native notification settings unavailable: {error}"))?;
    if !matches!(
        settings.authorization_status,
        AuthorizationStatus::Authorized
            | AuthorizationStatus::Provisional
            | AuthorizationStatus::Ephemeral
    ) {
        return Err(format!(
            "native notification permission is {:?}",
            settings.authorization_status
        ));
    }

    let mut notification = Notification::new()
        .title(title)
        .message(&request.body)
        // Bound response bookkeeping even if Notification Center never reports a
        // dismissal (for example, Clear All). The notification itself is allowed
        // to remain useful for a full day.
        .timeout(Duration::from_secs(24 * 60 * 60));
    if request.sound.unwrap_or(true) {
        notification = notification.default_sound();
    }
    let handle = notification
        .send_blocking()
        .map_err(|error| format!("native notification delivery failed: {error}"))?;

    if let (Some(pane_id), Some(app)) = (source_pane_id.map(str::to_string), state.app_handle()) {
        let state = state.clone();
        tauri::async_runtime::spawn(async move {
            if let Ok(response) = handle.response().await
                && response.is_default_action()
                && state
                    .list_panes()
                    .is_ok_and(|panes| panes.iter().any(|pane| pane.id == pane_id))
            {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.unminimize();
                    let _ = window.set_focus();
                }
                state.emit(QmuxEvent::new(
                    "app.notification_open_pane",
                    Some(pane_id.clone()),
                    None,
                    json!({ "paneId": pane_id }),
                ));
            }
        });
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn show_native(
    _state: &AppState,
    _source_pane_id: Option<&str>,
    _title: &str,
    _request: &SendNotificationRequest,
) -> Result<(), String> {
    Err("native notifications are unavailable on this platform".to_string())
}

#[tauri::command]
pub fn notification_log_get(state: tauri::State<'_, AppState>) -> Result<NotificationLog, String> {
    state.notification_log()
}

#[tauri::command]
pub fn notification_log_mark_read(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<NotificationLog, String> {
    let log = state.mark_notification_read(&id)?;
    emit_log_changed(&state, &log);
    Ok(log)
}

#[tauri::command]
pub fn notification_log_mark_all_read(
    state: tauri::State<'_, AppState>,
) -> Result<NotificationLog, String> {
    let log = state.mark_all_notifications_read()?;
    emit_log_changed(&state, &log);
    Ok(log)
}

#[tauri::command]
pub fn notification_log_clear(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<NotificationLog, String> {
    let log = state.clear_notification(&id)?;
    emit_log_changed(&state, &log);
    Ok(log)
}

#[tauri::command(async)]
pub async fn notification_permission_status() -> Result<NotificationPermissionInfo, String> {
    permission_status().await
}

#[tauri::command(async)]
pub async fn notification_request_permission() -> Result<NotificationPermissionInfo, String> {
    request_permission().await
}

#[cfg(target_os = "macos")]
async fn permission_status() -> Result<NotificationPermissionInfo, String> {
    let settings = mac_usernotifications::get_notification_settings()
        .await
        .map_err(|error| format!("failed to read notification permission: {error}"))?;
    Ok(NotificationPermissionInfo {
        supported: true,
        status: format!("{:?}", settings.authorization_status),
    })
}

#[cfg(not(target_os = "macos"))]
async fn permission_status() -> Result<NotificationPermissionInfo, String> {
    Ok(NotificationPermissionInfo {
        supported: false,
        status: "Unavailable".to_string(),
    })
}

#[cfg(target_os = "macos")]
async fn request_permission() -> Result<NotificationPermissionInfo, String> {
    mac_usernotifications::request_auth()
        .await
        .map_err(|error| format!("failed to request notification permission: {error}"))?;
    permission_status().await
}

#[cfg(not(target_os = "macos"))]
async fn request_permission() -> Result<NotificationPermissionInfo, String> {
    permission_status().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_validation_rejects_empty_and_oversized_content() {
        let request = SendNotificationRequest {
            title: None,
            body: "  ".into(),
            mode: NotificationMode::Auto,
            tone: NotificationTone::Info,
            sound: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        };
        assert!(
            validate_request(&request)
                .unwrap_err()
                .contains("must not be empty")
        );

        let request = SendNotificationRequest {
            body: "x".repeat(MAX_BODY_CHARS + 1),
            ..request
        };
        assert!(validate_request(&request).unwrap_err().contains("too long"));
    }

    #[test]
    fn request_validation_counts_characters_not_utf8_bytes() {
        let request = SendNotificationRequest {
            title: None,
            body: "🦀".repeat(MAX_BODY_CHARS),
            mode: NotificationMode::Overlay,
            tone: NotificationTone::Success,
            sound: Some(false),
            timeout_ms: MAX_TIMEOUT_MS,
        };
        assert!(validate_request(&request).is_ok());
    }

    #[test]
    fn request_validation_rejects_title_lines_and_body_controls() {
        let title_lines = SendNotificationRequest {
            title: Some("first\nsecond".into()),
            body: "body".into(),
            mode: NotificationMode::Overlay,
            tone: NotificationTone::Info,
            sound: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        };
        assert!(
            validate_request(&title_lines)
                .unwrap_err()
                .contains("single line")
        );

        let body_control = SendNotificationRequest {
            title: None,
            body: "body\u{1b}[31m".into(),
            ..title_lines
        };
        assert!(
            validate_request(&body_control)
                .unwrap_err()
                .contains("control characters")
        );
    }

    fn sample_entry(id: &str, created_at: i64) -> NotificationLogEntry {
        NotificationLogEntry {
            id: id.into(),
            title: "agent".into(),
            body: "done".into(),
            tone: NotificationTone::Info,
            pane_id: None,
            created_at,
            read: false,
        }
    }

    #[test]
    fn append_log_entry_replaces_by_id_and_drops_oldest_past_the_cap() {
        let mut log = NotificationLog::default();
        record_log_entry(&mut log, sample_entry("n1", 1));
        record_log_entry(&mut log, sample_entry("n2", 2));
        let mut replacement = sample_entry("n1", 3);
        replacement.body = "updated".into();
        record_log_entry(&mut log, replacement);
        assert_eq!(log.entries.len(), 2);
        assert_eq!(log.entries[0].id, "n2");
        assert_eq!(log.entries[1].id, "n1");
        assert_eq!(log.entries[1].body, "updated");

        for index in 0..(MAX_LOG_ENTRIES + 5) {
            record_log_entry(
                &mut log,
                sample_entry(&format!("cap-{index}"), index as i64),
            );
        }
        assert_eq!(log.entries.len(), MAX_LOG_ENTRIES);
        assert_eq!(log.entries[0].id, "cap-5");
        assert_eq!(
            log.entries.last().map(|entry| entry.id.as_str()),
            Some("cap-204")
        );
    }
}
