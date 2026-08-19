//! Native child-webview backend for qmux's human browser mode.
//!
//! External pages are top-level documents in their own WKWebView/WebView2/etc.,
//! never frames inside the privileged application document. The child labels
//! intentionally match no Tauri capability, and every navigation is checked
//! again here so the frontend is not the security boundary.

use crate::native_terminal;
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tauri::webview::{NewWindowResponse, PageLoadEvent, WebviewBuilder};
use tauri::{
    AppHandle, Emitter, EventTarget, LogicalPosition, LogicalSize, Manager, Rect, State, Url,
    Webview, WebviewUrl,
};

const MAIN_WEBVIEW_LABEL: &str = "main";
const HUMAN_BROWSER_EVENT: &str = "human-browser-event";
static NEXT_WEBVIEW_LABEL: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HumanBrowserSyncRequest {
    owner_id: String,
    url: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    visible: bool,
    generation: u64,
    revision: u64,
    navigation_revision: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HumanBrowserDestroyRequest {
    owner_id: String,
    generation: u64,
    revision: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HumanBrowserOwnerRequest {
    owner_id: String,
    generation: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HumanBrowserSnapshot {
    owner_id: String,
    url: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HumanBrowserEvent {
    owner_id: String,
    kind: &'static str,
    url: Option<String>,
    title: Option<String>,
    loading: Option<bool>,
}

#[derive(Clone)]
struct HumanBrowserView {
    webview: Webview,
    requested_url: String,
    navigation_revision: u64,
}

struct HumanBrowserInner {
    views: HashMap<String, HumanBrowserView>,
    active_owner: Option<String>,
    /// Frontend surface revisions are app-global, not per owner. That makes a
    /// delayed show from the previously active pane unable to cover the pane
    /// the user switched to while the first command was crossing the bridge.
    latest_surface_revision: u64,
    /// Lifecycle ordering is per owner. A destroy for pane A must not be
    /// discarded merely because a newer geometry update for pane B arrived
    /// first, while a genuinely stale destroy must not remove a reopened A.
    owner_revisions: HashMap<String, u64>,
    generation: u64,
}

impl Default for HumanBrowserInner {
    fn default() -> Self {
        Self {
            views: HashMap::new(),
            active_owner: None,
            latest_surface_revision: 0,
            owner_revisions: HashMap::new(),
            generation: 1,
        }
    }
}

impl HumanBrowserInner {
    fn accept_surface_request(&mut self, owner_id: &str, generation: u64, revision: u64) -> bool {
        if generation != self.generation
            || revision <= self.latest_surface_revision
            || revision <= self.owner_revisions.get(owner_id).copied().unwrap_or(0)
        {
            return false;
        }
        self.latest_surface_revision = revision;
        self.owner_revisions.insert(owner_id.to_string(), revision);
        true
    }

    fn accept_destroy_request(&mut self, owner_id: &str, generation: u64, revision: u64) -> bool {
        if generation != self.generation
            || revision <= self.owner_revisions.get(owner_id).copied().unwrap_or(0)
        {
            return false;
        }
        self.owner_revisions.insert(owner_id.to_string(), revision);
        true
    }

    fn surface_request_is_current(&self, owner_id: &str, generation: u64, revision: u64) -> bool {
        generation == self.generation
            && revision == self.latest_surface_revision
            && self.owner_revisions.get(owner_id) == Some(&revision)
    }

    fn advance_generation(&mut self) {
        self.active_owner = None;
        self.latest_surface_revision = 0;
        self.owner_revisions.clear();
        self.generation = self.generation.wrapping_add(1).max(1);
    }
}

#[derive(Default)]
pub struct HumanBrowserManager {
    inner: Mutex<HumanBrowserInner>,
    /// Native child-view transitions must never overlap. In particular,
    /// `add_child` temporarily yields to AppKit/WebKit while the state mutex is
    /// intentionally unlocked, so the mutex alone cannot serialize creation.
    lifecycle_busy: AtomicBool,
}

struct HumanBrowserLifecyclePermit<'a> {
    busy: &'a AtomicBool,
}

impl Drop for HumanBrowserLifecyclePermit<'_> {
    fn drop(&mut self) {
        self.busy.store(false, Ordering::Release);
    }
}

impl HumanBrowserManager {
    fn try_begin_lifecycle(&self) -> Result<HumanBrowserLifecyclePermit<'_>, String> {
        self.lifecycle_busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map(|_| HumanBrowserLifecyclePermit {
                busy: &self.lifecycle_busy,
            })
            .map_err(|_| "human browser lifecycle is busy; retry the request".to_string())
    }
}

fn emit_event(app: &AppHandle, event: HumanBrowserEvent) {
    let _ = app.emit_to(
        EventTarget::webview(MAIN_WEBVIEW_LABEL),
        HUMAN_BROWSER_EVENT,
        event,
    );
}

fn parse_http_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|error| format!("invalid browser URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("the human browser only navigates to http(s) URLs".to_string());
    }
    Ok(url)
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

fn is_qmux_file_server_url(url: &Url, port: Option<u16>) -> bool {
    let Some(port) = port else {
        return false;
    };
    url.scheme() == "http"
        && matches!(url.host_str(), Some("127.0.0.1" | "localhost"))
        && url.port_or_known_default() == Some(port)
}

fn validated_human_url(app: &AppHandle, state: &AppState, raw: &str) -> Result<Url, String> {
    let url = parse_http_url(raw)?;
    if is_qmux_file_server_url(&url, state.file_server_port()) {
        return Err(
            "protected qmux file previews must remain in the sandboxed preview".to_string(),
        );
    }
    // During development the privileged app origin is itself http://127.0.0.1.
    // Never let a child become that origin even though the child label is also
    // excluded from capabilities. This is defense in depth against future ACL
    // changes and against navigation to the bundled app origin on other targets.
    if let Some(main) = app.get_webview(MAIN_WEBVIEW_LABEL)
        && let Ok(app_url) = main.url()
        && same_origin(&url, &app_url)
    {
        return Err("refusing to navigate the human browser to qmux's app origin".to_string());
    }
    Ok(url)
}

fn validate_owner_id(owner_id: &str) -> Result<(), String> {
    if owner_id.is_empty() || owner_id.len() > 512 || owner_id.contains('\0') {
        return Err("invalid human browser owner id".to_string());
    }
    Ok(())
}

fn validated_bounds(request: &HumanBrowserSyncRequest) -> Result<Rect, String> {
    let values = [request.x, request.y, request.width, request.height];
    if values.iter().any(|value| !value.is_finite())
        || request.width < 0.0
        || request.height < 0.0
        || (request.visible && (request.width < 1.0 || request.height < 1.0))
    {
        return Err("invalid human browser bounds".to_string());
    }
    Ok(Rect {
        position: LogicalPosition::new(request.x, request.y).into(),
        size: LogicalSize::new(request.width, request.height).into(),
    })
}

#[cfg(target_os = "macos")]
fn set_native_browser_active(webview: &Webview, active: bool) -> Result<(), String> {
    webview
        .with_webview(move |platform| {
            if let Err(error) = native_terminal::set_human_browser_webview(platform.inner(), active)
            {
                eprintln!("qmux: failed to update human-browser shortcut routing: {error}");
            }
        })
        .map_err(|error| format!("failed to access the native human browser: {error}"))
}

#[cfg(not(target_os = "macos"))]
fn set_native_browser_active(_webview: &Webview, _active: bool) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn set_native_browser_loading_background(webview: &Webview, active: bool) -> Result<(), String> {
    webview
        .with_webview(move |platform| {
            if let Err(error) =
                native_terminal::set_human_browser_loading_background(platform.inner(), active)
            {
                eprintln!("qmux: failed to update human-browser loading background: {error}");
            }
        })
        .map_err(|error| format!("failed to access the native human browser: {error}"))
}

#[cfg(target_os = "macos")]
fn set_native_browser_loading_background_from_state(
    webview: &Webview,
    active: Arc<AtomicBool>,
) -> Result<(), String> {
    webview
        .with_webview(move |platform| {
            // with_webview can be dispatched to AppKit after add_child returns.
            // Read the state there so a very fast load cannot be overwritten
            // with the stale initial value.
            let active = active.load(Ordering::Acquire);
            if let Err(error) =
                native_terminal::set_human_browser_loading_background(platform.inner(), active)
            {
                eprintln!("qmux: failed to update human-browser loading background: {error}");
            }
        })
        .map_err(|error| format!("failed to access the native human browser: {error}"))
}

#[cfg(not(target_os = "macos"))]
fn set_native_browser_loading_background_from_state(
    _webview: &Webview,
    _active: Arc<AtomicBool>,
) -> Result<(), String> {
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn set_native_browser_loading_background(_webview: &Webview, _active: bool) -> Result<(), String> {
    Ok(())
}

fn deactivate_view(view: &HumanBrowserView) {
    let _ = set_native_browser_active(&view.webview, false);
    // A child WKWebView sits above the main document's compositor, so a hide
    // that is delayed or dropped by AppKit can leave its last page layer as a
    // rectangular afterimage. Collapse the native child first; the next visible
    // sync always publishes real bounds again before showing it.
    #[cfg(target_os = "macos")]
    let _ = view.webview.set_bounds(Rect {
        position: LogicalPosition::new(0.0, 0.0).into(),
        size: LogicalSize::new(0.0, 0.0).into(),
    });
    let _ = view.webview.hide();
}

fn create_webview(
    app: &AppHandle,
    state: &AppState,
    owner_id: &str,
    initial_url: Url,
    bounds: Rect,
) -> Result<Webview, String> {
    let label = format!(
        "human-browser-{}",
        NEXT_WEBVIEW_LABEL.fetch_add(1, Ordering::Relaxed)
    );
    let navigation_app = app.clone();
    let navigation_state = state.clone();
    let page_app = app.clone();
    let page_owner = owner_id.to_string();
    let title_app = app.clone();
    let title_owner = owner_id.to_string();
    let popup_app = app.clone();
    let popup_state = state.clone();
    let popup_owner = owner_id.to_string();
    let loading_background_active = Arc::new(AtomicBool::new(true));
    let page_loading_background_active = loading_background_active.clone();

    let builder = WebviewBuilder::new(&label, WebviewUrl::External(initial_url))
        .on_navigation(move |url| {
            validated_human_url(&navigation_app, &navigation_state, url.as_str()).is_ok()
        })
        .on_page_load(move |webview, payload| {
            let loading = payload.event() == PageLoadEvent::Started;
            page_loading_background_active.store(loading, Ordering::Release);
            let _ = set_native_browser_loading_background(&webview, loading);
            emit_event(
                &page_app,
                HumanBrowserEvent {
                    owner_id: page_owner.clone(),
                    kind: "navigation",
                    url: Some(payload.url().to_string()),
                    title: None,
                    loading: Some(loading),
                },
            );
        })
        .on_document_title_changed(move |webview, title| {
            emit_event(
                &title_app,
                HumanBrowserEvent {
                    owner_id: title_owner.clone(),
                    kind: "title",
                    url: webview.url().ok().map(|url| url.to_string()),
                    title: Some(title),
                    loading: None,
                },
            );
        })
        .on_new_window(move |url, _features| {
            if validated_human_url(&popup_app, &popup_state, url.as_str()).is_ok() {
                emit_event(
                    &popup_app,
                    HumanBrowserEvent {
                        owner_id: popup_owner.clone(),
                        kind: "newWindow",
                        url: Some(url.to_string()),
                        title: None,
                        loading: None,
                    },
                );
            }
            // Popups are routed back through the managed address/navigation
            // path. Never let a remote page create an unmanaged app window.
            NewWindowResponse::Deny
        })
        // Downloads need an explicit destination/confirmation flow before they
        // can be safely exposed as a human-browser feature.
        .on_download(|_webview, _event| false);

    let window = app
        .get_window(MAIN_WEBVIEW_LABEL)
        .ok_or_else(|| "the main window is unavailable".to_string())?;
    // Wry adds a child WKWebView to the view hierarchy during construction.
    // Give it no drawable area until the hide/background messages below have
    // reached AppKit; human_browser_sync applies the requested bounds later.
    #[cfg(target_os = "macos")]
    let initial_size = LogicalSize::new(0.0, 0.0);
    #[cfg(not(target_os = "macos"))]
    let initial_size = bounds.size;
    let webview = window
        .add_child(builder, bounds.position, initial_size)
        .map_err(|error| format!("failed to create the human browser: {error}"))?;
    // add_child creates a visible native view. Hide it before installing the
    // themed canvas; human_browser_sync positions and reveals it afterwards.
    if let Err(error) = webview.hide() {
        let _ = webview.close();
        return Err(format!("failed to hide the new human browser: {error}"));
    }
    let _ = set_native_browser_loading_background_from_state(&webview, loading_background_active);
    Ok(webview)
}

fn current_snapshot(owner_id: &str, view: &HumanBrowserView) -> HumanBrowserSnapshot {
    HumanBrowserSnapshot {
        owner_id: owner_id.to_string(),
        url: view
            .webview
            .url()
            .map(|url| url.to_string())
            .unwrap_or_else(|_| view.requested_url.clone()),
    }
}

#[tauri::command]
pub async fn human_browser_sync(
    request: HumanBrowserSyncRequest,
    app: AppHandle,
    state: State<'_, AppState>,
    manager: State<'_, HumanBrowserManager>,
) -> Result<Option<HumanBrowserSnapshot>, String> {
    validate_owner_id(&request.owner_id)?;
    let bounds = validated_bounds(&request)?;
    let url = validated_human_url(&app, &state, &request.url)?;
    let _lifecycle = manager.try_begin_lifecycle()?;
    // Tauri requires an async command when WebView2 may create a child webview;
    // a synchronous IPC handler can deadlock while add_child dispatches to the
    // Windows UI thread. The mutex is used only to prepare/commit state; no
    // Tauri dispatcher call occurs while it is held, so reset_all remains safe
    // while this command is in flight on the async runtime.
    let (accepted, previous, current) = {
        let mut inner = manager
            .inner
            .lock()
            .map_err(|_| "human browser state lock poisoned".to_string())?;
        if !inner.accept_surface_request(&request.owner_id, request.generation, request.revision) {
            (false, None, inner.views.get(&request.owner_id).cloned())
        } else if !request.visible {
            if inner.active_owner.as_deref() == Some(request.owner_id.as_str()) {
                inner.active_owner = None;
            }
            (true, None, inner.views.get(&request.owner_id).cloned())
        } else {
            let previous = if inner.active_owner.as_deref() == Some(request.owner_id.as_str()) {
                None
            } else {
                inner
                    .active_owner
                    .take()
                    .and_then(|owner| inner.views.get(&owner).cloned())
            };
            (true, previous, inner.views.get(&request.owner_id).cloned())
        }
    };

    if !accepted {
        return Ok(current
            .as_ref()
            .map(|view| current_snapshot(&request.owner_id, view)));
    }
    if !request.visible {
        // Visibility is a property of the requested native child, not of the
        // bookkeeping pointer. Always collapse it even if an interrupted load
        // or owner switch already cleared active_owner.
        if let Some(view) = current.as_ref() {
            deactivate_view(view);
        }
        return Ok(current
            .as_ref()
            .map(|view| current_snapshot(&request.owner_id, view)));
    }

    if let Some(previous) = previous.as_ref() {
        deactivate_view(previous);
    }

    let mut view = if let Some(current) = current {
        current
    } else {
        let webview = create_webview(&app, &state, &request.owner_id, url.clone(), bounds)?;
        let created = HumanBrowserView {
            webview,
            requested_url: url.to_string(),
            navigation_revision: request.navigation_revision,
        };
        let mut inner = manager
            .inner
            .lock()
            .map_err(|_| "human browser state lock poisoned".to_string())?;
        if !inner.surface_request_is_current(
            &request.owner_id,
            request.generation,
            request.revision,
        ) {
            drop(inner);
            let _ = created.webview.hide();
            let _ = created.webview.close();
            return Ok(None);
        }
        inner
            .views
            .insert(request.owner_id.clone(), created.clone());
        created
    };

    let update_result = (|| {
        view.webview
            .set_bounds(bounds)
            .map_err(|error| format!("failed to position the human browser: {error}"))?;

        if request.navigation_revision > view.navigation_revision {
            let _ = set_native_browser_loading_background(&view.webview, true);
            let current_url = view.webview.url().ok();
            if current_url.as_ref().is_some_and(|current| current == &url) {
                view.webview
                    .reload()
                    .map_err(|error| format!("failed to reload the human browser: {error}"))?;
            } else {
                view.webview
                    .navigate(url.clone())
                    .map_err(|error| format!("failed to navigate the human browser: {error}"))?;
            }
            view.requested_url = url.to_string();
            view.navigation_revision = request.navigation_revision;
        }

        set_native_browser_active(&view.webview, true)?;
        view.webview
            .show()
            .map_err(|error| format!("failed to show the human browser: {error}"))
    })();
    if let Err(error) = update_result {
        deactivate_view(&view);
        return Err(error);
    }

    let snapshot = current_snapshot(&request.owner_id, &view);
    let mut inner = manager
        .inner
        .lock()
        .map_err(|_| "human browser state lock poisoned".to_string())?;
    if !inner.surface_request_is_current(&request.owner_id, request.generation, request.revision) {
        drop(inner);
        deactivate_view(&view);
        return Ok(None);
    }
    if let Some(managed) = inner.views.get_mut(&request.owner_id) {
        managed.requested_url = view.requested_url;
        managed.navigation_revision = view.navigation_revision;
    }
    inner.active_owner = Some(request.owner_id.clone());
    Ok(Some(snapshot))
}

const HIDE_ALL_OWNER: &str = "__qmux_hide_all__";

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HumanBrowserHideAllRequest {
    generation: u64,
    revision: u64,
}

/// Collapse every native child. Used when React already thinks the overlay is
/// closed but an AppKit hide was dropped, leaving a white WKWebView square
/// over the terminal. Views stay in the map so a still-open owner can show
/// again; destroy retires them when the overlay is actually closed.
#[tauri::command]
pub fn human_browser_hide_all(
    request: HumanBrowserHideAllRequest,
    manager: State<'_, HumanBrowserManager>,
) -> Result<u32, String> {
    let _lifecycle = manager.try_begin_lifecycle()?;
    let views = {
        let mut inner = manager
            .inner
            .lock()
            .map_err(|_| "human browser state lock poisoned".to_string())?;
        if request.generation == inner.generation {
            let _ =
                inner.accept_surface_request(HIDE_ALL_OWNER, request.generation, request.revision);
        }
        inner.active_owner = None;
        inner.views.values().cloned().collect::<Vec<_>>()
    };
    for view in &views {
        deactivate_view(view);
    }
    Ok(views.len() as u32)
}

#[tauri::command]
pub fn human_browser_destroy(
    request: HumanBrowserDestroyRequest,
    manager: State<'_, HumanBrowserManager>,
) -> Result<(), String> {
    validate_owner_id(&request.owner_id)?;
    let _lifecycle = manager.try_begin_lifecycle()?;
    let mut inner = manager
        .inner
        .lock()
        .map_err(|_| "human browser state lock poisoned".to_string())?;
    if !inner.accept_destroy_request(&request.owner_id, request.generation, request.revision) {
        return Ok(());
    }
    let was_active = inner.active_owner.as_deref() == Some(request.owner_id.as_str());
    if was_active {
        inner.active_owner = None;
    }
    let view = inner.views.remove(&request.owner_id);
    drop(inner);
    if let Some(view) = view {
        deactivate_view(&view);
        let _ = view.webview.close();
    }
    Ok(())
}

#[tauri::command]
pub fn human_browser_generation(manager: State<'_, HumanBrowserManager>) -> Result<u64, String> {
    manager
        .inner
        .lock()
        .map(|inner| inner.generation)
        .map_err(|_| "human browser state lock poisoned".to_string())
}

#[tauri::command]
pub fn human_browser_snapshot(
    request: HumanBrowserOwnerRequest,
    manager: State<'_, HumanBrowserManager>,
) -> Result<Option<HumanBrowserSnapshot>, String> {
    validate_owner_id(&request.owner_id)?;
    let view = {
        let inner = manager
            .inner
            .lock()
            .map_err(|_| "human browser state lock poisoned".to_string())?;
        if request.generation != inner.generation {
            return Ok(None);
        }
        inner.views.get(&request.owner_id).cloned()
    };
    Ok(view
        .as_ref()
        .map(|view| current_snapshot(&request.owner_id, view)))
}

#[tauri::command]
pub fn human_browser_reload(
    request: HumanBrowserOwnerRequest,
    manager: State<'_, HumanBrowserManager>,
) -> Result<(), String> {
    validate_owner_id(&request.owner_id)?;
    let _lifecycle = manager.try_begin_lifecycle()?;
    let view = {
        let inner = manager
            .inner
            .lock()
            .map_err(|_| "human browser state lock poisoned".to_string())?;
        if request.generation != inner.generation {
            return Ok(());
        }
        inner.views.get(&request.owner_id).cloned()
    };
    if let Some(view) = view {
        view.webview
            .reload()
            .map_err(|error| format!("failed to reload the human browser: {error}"))?;
    }
    Ok(())
}

/// A main-document reload destroys the frontend authority for child visibility.
/// Close every child and advance the document generation so commands already
/// in flight from the old document cannot resurrect one over the reload.
pub fn reset_all(app: &AppHandle) {
    let Some(manager) = app.try_state::<HumanBrowserManager>() else {
        return;
    };
    let Ok(mut inner) = manager.inner.lock() else {
        return;
    };
    let views = inner
        .views
        .drain()
        .map(|(_, view)| view)
        .collect::<Vec<_>>();
    inner.advance_generation();
    drop(inner);
    for view in views {
        deactivate_view(&view);
        let _ = view.webview.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_accepts_network_http_urls() {
        assert!(parse_http_url("https://example.com/path").is_ok());
        assert!(parse_http_url("http://localhost:3000").is_ok());
        assert!(parse_http_url("file:///tmp/report.html").is_err());
        assert!(parse_http_url("javascript:alert(1)").is_err());
        assert!(parse_http_url("https://").is_err());
    }

    #[test]
    fn origin_comparison_normalizes_default_ports() {
        let a = Url::parse("https://example.com/path").unwrap();
        let b = Url::parse("https://example.com:443/other").unwrap();
        let c = Url::parse("http://example.com/").unwrap();
        assert!(same_origin(&a, &b));
        assert!(!same_origin(&a, &c));
    }

    #[test]
    fn recognizes_only_the_bound_file_server_port() {
        let protected = Url::parse("http://127.0.0.1:8123/token/file").unwrap();
        let dev = Url::parse("http://localhost:5173/").unwrap();
        assert!(is_qmux_file_server_url(&protected, Some(8123)));
        assert!(!is_qmux_file_server_url(&protected, Some(9000)));
        assert!(!is_qmux_file_server_url(&dev, Some(8123)));
    }

    #[test]
    fn owner_destroy_is_not_superseded_by_another_owners_surface_update() {
        let mut inner = HumanBrowserInner::default();
        assert!(inner.accept_surface_request("pane-a", 1, 1));
        assert!(inner.accept_surface_request("pane-b", 1, 3));
        assert!(inner.accept_destroy_request("pane-a", 1, 2));
        assert_eq!(inner.latest_surface_revision, 3);
        assert!(!inner.accept_surface_request("pane-a", 1, 1));
        assert!(inner.accept_surface_request("pane-a", 1, 4));
        assert!(!inner.accept_destroy_request("pane-a", 1, 2));
    }

    #[test]
    fn destroy_blocks_older_requests_for_its_owner_only() {
        let mut inner = HumanBrowserInner::default();
        assert!(inner.accept_surface_request("pane-a", 1, 1));
        assert!(inner.accept_destroy_request("pane-a", 1, 3));
        assert!(!inner.accept_surface_request("pane-a", 1, 2));
        assert!(inner.accept_surface_request("pane-b", 1, 2));
        assert!(inner.surface_request_is_current("pane-b", 1, 2));
    }

    #[test]
    fn newer_surface_request_invalidates_an_older_in_flight_commit() {
        let mut inner = HumanBrowserInner::default();
        assert!(inner.accept_surface_request("pane-a", 1, 1));
        assert!(inner.surface_request_is_current("pane-a", 1, 1));
        assert!(inner.accept_surface_request("pane-b", 1, 2));
        assert!(!inner.surface_request_is_current("pane-a", 1, 1));
        assert!(inner.surface_request_is_current("pane-b", 1, 2));
    }

    #[test]
    fn advancing_generation_rejects_old_document_commands() {
        let mut inner = HumanBrowserInner::default();
        assert!(inner.accept_surface_request("pane-a", 1, 1));
        inner.advance_generation();
        assert!(!inner.accept_surface_request("pane-a", 1, 2));
        assert!(!inner.accept_destroy_request("pane-a", 1, 3));
        assert!(inner.accept_surface_request("pane-a", 2, 1));
    }

    #[test]
    fn lifecycle_permit_rejects_overlap_and_recovers_after_drop() {
        let manager = HumanBrowserManager::default();
        let first = manager.try_begin_lifecycle().unwrap();
        assert!(manager.try_begin_lifecycle().is_err());
        drop(first);
        assert!(manager.try_begin_lifecycle().is_ok());
    }

    #[test]
    fn hide_all_uses_a_reserved_owner_that_cannot_collide_with_a_pane() {
        assert!(validate_owner_id(HIDE_ALL_OWNER).is_ok());
        let mut inner = HumanBrowserInner::default();
        assert!(inner.accept_surface_request("pane-a", 1, 1));
        assert!(inner.accept_surface_request(HIDE_ALL_OWNER, 1, 2));
        assert!(!inner.surface_request_is_current("pane-a", 1, 1));
        assert!(!inner.accept_surface_request("pane-a", 1, 1));
    }
}
