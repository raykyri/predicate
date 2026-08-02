//! Experimental Codex in-app Browser backend.
//!
//! The Browser plugin currently discovers JSON-RPC peers by scanning Unix
//! sockets in /tmp/codex-browser-use. This protocol is not a public OpenAI API,
//! so keep this adapter small and capability-conservative. It identifies qmux
//! to `agent.browsers.list()` and proxies the Browser client's tab/CDP requests
//! to an isolated chrome-headless-shell runtime. Sandboxed file previews remain
//! separate.

use crate::browser_engine::{BrowserEngine, ScreencastFrame};
use crate::state::AppState;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tauri::Emitter;

const DISCOVERY_DIR: &str = "/tmp/codex-browser-use";
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const CLIENT_EVENT_QUEUE: usize = 1024;
const CLIENT_REQUEST_QUEUE: usize = 128;
const CLIENT_REQUEST_WORKERS: usize = 4;
/// Shared by the streamed screencast and the polled fallback so the mirror
/// looks the same however a frame reached it.
const AGENT_MIRROR_JPEG_QUALITY: u32 = 80;
/// PiP previews are intentionally small and observation-only. A lower quality
/// keeps periodic frames cheap without affecting the full browser mirror.
const AGENT_PREVIEW_JPEG_QUALITY: u32 = 60;
/// Screencast frames waiting to reach the webview. Chromium keeps at most a
/// couple in flight, and a stale frame is worth less than the one behind it,
/// so this stays small on purpose.
const SCREENCAST_FRAME_QUEUE: usize = 2;
/// Tauri event carrying one mirrored frame to the browser overlay.
const SCREENCAST_FRAME_EVENT: &str = "browser-screencast-frame";
/// Ceiling on how often frames cross into the webview. Every frame is a full
/// base64 JPEG over the IPC bridge, and a page animating at the display's
/// refresh rate would spend more time shipping frames than rendering them.
/// Dropping here is safe: the engine has already acknowledged the frame, and
/// the overlay's settle capture redraws whatever a drop left behind.
const SCREENCAST_MIN_FRAME_INTERVAL: Duration = Duration::from_millis(33);

struct BrowserBackend {
    engine: Option<Arc<BrowserEngine>>,
    engine_error: Option<String>,
    app_state: Option<AppState>,
    pane_by_session: Mutex<HashMap<String, String>>,
    tab_by_pane: Mutex<HashMap<String, u64>>,
    pane_by_tab: Mutex<HashMap<u64, String>>,
    viewport_by_pane: Mutex<HashMap<String, BrowserViewport>>,
    /// Serializes transitions between actual Chromium screencasts. The map
    /// below is both the routing table for pushed frames and the record needed
    /// to stop a stream, so start/replace/stop must update it atomically with
    /// respect to one another.
    screencast_control: Mutex<()>,
    screencast_by_pane: Mutex<HashMap<String, BrowserViewport>>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct BrowserViewport {
    tab_id: u64,
    width: u32,
    height: u32,
    scale_factor: f64,
}

/// Managed by Tauri so a clean app shutdown removes the discoverable socket.
pub struct BrowserDiscoverySocket {
    socket_path: PathBuf,
    _backend: Arc<BrowserBackend>,
    shutdown: Arc<AtomicBool>,
}

impl BrowserDiscoverySocket {
    pub fn path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for BrowserDiscoverySocket {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = fs::remove_file(&self.socket_path);
    }
}

pub fn start_browser_discovery(
    app_state: Option<AppState>,
) -> Result<BrowserDiscoverySocket, String> {
    let directory = PathBuf::from(DISCOVERY_DIR);
    let created = !directory.exists();
    fs::create_dir_all(&directory)
        .map_err(|err| format!("failed to create {}: {err}", directory.display()))?;
    if created {
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o1777))
            .map_err(|err| format!("failed to secure {}: {err}", directory.display()))?;
    }
    remove_stale_qmux_sockets(&directory);

    let socket_path = directory.join(format!("qmux-{}.sock", std::process::id()));
    match fs::remove_file(&socket_path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "failed to reclaim {}: {err}",
                socket_path.display()
            ));
        }
    }
    let listener = UnixListener::bind(&socket_path)
        .map_err(|err| format!("failed to bind {}: {err}", socket_path.display()))?;
    if let Err(err) = fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600)) {
        drop(listener);
        let _ = fs::remove_file(&socket_path);
        return Err(format!("failed to secure {}: {err}", socket_path.display()));
    }
    if let Err(err) = listener.set_nonblocking(true) {
        drop(listener);
        let _ = fs::remove_file(&socket_path);
        return Err(format!(
            "failed to configure {} as nonblocking: {err}",
            socket_path.display()
        ));
    }

    let (engine, engine_error) = match BrowserEngine::start() {
        Ok(engine) => (Some(Arc::new(engine)), None),
        Err(err) => {
            eprintln!("qmux: Codex browser automation unavailable: {err}");
            (None, Some(err))
        }
    };
    let backend = Arc::new(BrowserBackend {
        engine,
        engine_error,
        app_state,
        pane_by_session: Mutex::new(HashMap::new()),
        tab_by_pane: Mutex::new(HashMap::new()),
        pane_by_tab: Mutex::new(HashMap::new()),
        viewport_by_pane: Mutex::new(HashMap::new()),
        screencast_control: Mutex::new(()),
        screencast_by_pane: Mutex::new(HashMap::new()),
    });
    start_screencast_pump(&backend)?;
    let listener_backend = Arc::clone(&backend);
    let shutdown = Arc::new(AtomicBool::new(false));
    let listener_shutdown = Arc::clone(&shutdown);

    if let Err(err) = thread::Builder::new()
        .name("qmux-browser-discovery".to_string())
        .spawn(move || {
            while !listener_shutdown.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Err(err) = stream.set_nonblocking(false) {
                            eprintln!(
                                "qmux: failed to configure browser discovery client socket: {err}"
                            );
                            continue;
                        }
                        let backend = Arc::clone(&listener_backend);
                        let _ = thread::Builder::new()
                            .name("qmux-browser-discovery-client".to_string())
                            .spawn(move || {
                                if let Err(err) = serve_connection(stream, backend) {
                                    eprintln!("qmux: browser discovery client failed: {err}");
                                }
                            });
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(25));
                    }
                    Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                    Err(err) => {
                        eprintln!("qmux: browser discovery accept failed: {err}");
                        break;
                    }
                }
            }
        })
    {
        let _ = fs::remove_file(&socket_path);
        return Err(format!("failed to start browser discovery listener: {err}"));
    }

    Ok(BrowserDiscoverySocket {
        socket_path,
        _backend: backend,
        shutdown,
    })
}

fn remove_stale_qmux_sockets(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_qmux_socket = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("qmux-") && name.ends_with(".sock"));
        if is_qmux_socket && UnixStream::connect(&path).is_err() {
            let _ = fs::remove_file(path);
        }
    }
}

fn serve_connection(mut stream: UnixStream, backend: Arc<BrowserBackend>) -> Result<(), String> {
    let pane_id = backend
        .app_state
        .as_ref()
        .and_then(|state| peer_process_id(&stream).and_then(|pid| pane_for_process(state, pid)));
    let mut writer_stream = stream
        .try_clone()
        .map_err(|err| format!("failed to clone browser client socket: {err}"))?;
    let (writer_tx, writer_rx) = mpsc::sync_channel(CLIENT_EVENT_QUEUE);
    let writer_backend = Arc::clone(&backend);
    let writer_pane_id = pane_id.clone();
    thread::Builder::new()
        .name("qmux-browser-client-writer".to_string())
        .spawn(move || {
            while let Ok(message) = writer_rx.recv() {
                if !event_is_visible_to_pane(&writer_backend, writer_pane_id.as_deref(), &message) {
                    continue;
                }
                if write_frame(&mut writer_stream, &message).is_err() {
                    break;
                }
            }
        })
        .map_err(|err| format!("failed to start browser client writer: {err}"))?;
    let subscription_id = backend
        .engine
        .as_ref()
        .map(|engine| engine.subscribe(writer_tx.clone()));
    let (request_tx, request_rx) = mpsc::sync_channel(CLIENT_REQUEST_QUEUE);
    let request_rx = Arc::new(Mutex::new(request_rx));
    for worker_index in 0..CLIENT_REQUEST_WORKERS {
        let worker_backend = Arc::clone(&backend);
        let worker_pane_id = pane_id.clone();
        let worker_writer_tx = writer_tx.clone();
        let worker_request_rx = Arc::clone(&request_rx);
        thread::Builder::new()
            .name(format!("qmux-browser-client-worker-{worker_index}"))
            .spawn(move || {
                loop {
                    let work = {
                        let receiver = lock_or_recover(&worker_request_rx);
                        receiver.recv()
                    };
                    let Ok((request, id)) = work else {
                        break;
                    };
                    let response =
                        handle_request(&worker_backend, worker_pane_id.as_deref(), &request, id);
                    if worker_writer_tx.send(response).is_err() {
                        break;
                    }
                }
            })
            .map_err(|err| format!("failed to start browser client worker: {err}"))?;
    }

    loop {
        let request = match read_frame(&mut stream) {
            Ok(Some(request)) => request,
            Ok(None) => break,
            Err(err) => {
                if let (Some(engine), Some(subscription_id)) =
                    (backend.engine.as_ref(), subscription_id)
                {
                    engine.unsubscribe(subscription_id);
                }
                return Err(err);
            }
        };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        if request_tx.send((request, id)).is_err() {
            break;
        }
    }
    if let (Some(engine), Some(subscription_id)) = (backend.engine.as_ref(), subscription_id) {
        engine.unsubscribe(subscription_id);
    }
    Ok(())
}

fn handle_request(
    backend: &BrowserBackend,
    pane_id: Option<&str>,
    request: &Value,
    id: Value,
) -> Value {
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let session_id = request
        .pointer("/params/session_id")
        .and_then(Value::as_str);
    let owned_pane = pane_id.map(str::to_string).or_else(|| {
        session_id.and_then(|session_id| {
            lock_or_recover(&backend.pane_by_session)
                .get(session_id)
                .cloned()
        })
    });
    if let (Some(pane_id), Some(session_id)) = (owned_pane.as_deref(), session_id) {
        lock_or_recover(&backend.pane_by_session)
            .insert(session_id.to_string(), pane_id.to_string());
    }
    match method {
        "getInfo" => {
            let session_id = request
                .pointer("/params/session_id")
                .or_else(|| request.pointer("/params/sessionId"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let build_flavor =
                std::env::var("QMUX_CODEX_APP_BUILD_FLAVOR").unwrap_or_else(|_| "prod".to_string());
            let automation = if backend.engine.is_some() {
                "chrome-headless-shell-cdp"
            } else {
                "discovery-only"
            };
            let headless_shell_executable = backend
                .engine
                .as_ref()
                .map(|engine| engine.executable().display().to_string());
            let mut metadata = serde_json::Map::new();
            metadata.insert("codexSessionId".to_string(), json!(session_id));
            metadata.insert("codexAppBuildFlavor".to_string(), json!(build_flavor));
            metadata.insert("qmuxVersion".to_string(), json!(env!("CARGO_PKG_VERSION")));
            metadata.insert("automation".to_string(), json!(automation));
            if let Some(headless_shell_executable) = headless_shell_executable {
                metadata.insert(
                    "chromeHeadlessShellExecutable".to_string(),
                    json!(headless_shell_executable),
                );
            }
            if let Some(engine_error) = backend.engine_error.as_deref() {
                metadata.insert("automationError".to_string(), json!(engine_error));
            }
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "id": "qmux",
                    "name": "qmux Browser",
                    "type": "iab",
                    "family": "qmux",
                    "capabilities": {
                        "browser": [],
                        "tab": []
                    },
                    "metadata": metadata
                }
            })
        }
        "ping" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": "pong"
        }),
        _ if !tab_access_is_allowed(backend, owned_pane.as_deref(), request) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32002,
                "message": "browser tab belongs to a different qmux pane"
            }
        }),
        _ => match backend.engine.as_ref() {
            Some(engine) => {
                let mut params = request.get("params").cloned().unwrap_or_else(|| json!({}));
                // Chromium's finalizeTabs command is global, while qmux exposes
                // a pane-scoped browser. Tell the engine which owned tabs this
                // client is allowed to finalize so one terminal cannot close a
                // browser that is still being shown for another terminal.
                if method == "finalizeTabs"
                    && let (Some(pane_id), Some(params)) =
                        (owned_pane.as_deref(), params.as_object_mut())
                {
                    let scope = lock_or_recover(&backend.pane_by_tab)
                        .iter()
                        .filter(|(_, owner)| owner.as_str() == pane_id)
                        .map(|(tab_id, _)| json!(tab_id))
                        .collect::<Vec<_>>();
                    params.insert("qmuxScope".to_string(), Value::Array(scope));
                }
                match engine.call(method, params) {
                    Ok(result) => {
                        remember_tab_owner(
                            backend,
                            owned_pane.as_deref(),
                            method,
                            request,
                            &result,
                        );
                        let result =
                            scope_result_to_pane(backend, owned_pane.as_deref(), method, result);
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": result
                        })
                    }
                    Err(message) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32000,
                            "message": message
                        }
                    }),
                }
            }
            None => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32001,
                    "message": backend.engine_error.as_deref().unwrap_or(
                        "qmux chrome-headless-shell automation is unavailable"
                    )
                }
            }),
        },
    }
}

fn remember_tab_owner(
    backend: &BrowserBackend,
    pane_id: Option<&str>,
    method: &str,
    request: &Value,
    result: &Value,
) {
    let Some(pane_id) = pane_id else {
        return;
    };
    let tab_id = if method == "createTab" {
        result.get("id").and_then(Value::as_u64)
    } else {
        request
            .pointer("/params/tabId")
            .or_else(|| request.pointer("/params/target/tabId"))
            .and_then(Value::as_u64)
    };
    if let Some(tab_id) = tab_id {
        lock_or_recover(&backend.tab_by_pane).insert(pane_id.to_string(), tab_id);
        lock_or_recover(&backend.pane_by_tab).insert(tab_id, pane_id.to_string());
    }
    if method == "executeCdp"
        && request
            .pointer("/params/method")
            .and_then(Value::as_str)
            .is_some_and(|method| {
                matches!(
                    method,
                    "Emulation.setDeviceMetricsOverride"
                        | "Emulation.clearDeviceMetricsOverride"
                        | "Page.setDeviceMetricsOverride"
                )
            })
    {
        lock_or_recover(&backend.viewport_by_pane).remove(pane_id);
        // The frame size follows the client's own override now. Stop the
        // actual Chromium stream before forgetting it so closing the overlay
        // during the next heartbeat cannot strand an untracked screencast.
        let streaming = {
            lock_or_recover(&backend.screencast_by_pane)
                .get(pane_id)
                .copied()
        };
        if let Some(streaming) = streaming {
            if let Err(error) = stop_recorded_screencast(backend, pane_id, streaming) {
                eprintln!(
                    "qmux: failed to stop pane {pane_id} screencast after a device metrics override: {error}"
                );
            }
        }
    }
}

fn request_tab_id(request: &Value) -> Option<u64> {
    request
        .pointer("/params/tabId")
        .or_else(|| request.pointer("/params/target/tabId"))
        .and_then(Value::as_u64)
}

fn tab_access_is_allowed(backend: &BrowserBackend, pane_id: Option<&str>, request: &Value) -> bool {
    let (Some(pane_id), Some(tab_id)) = (pane_id, request_tab_id(request)) else {
        return true;
    };
    let mut owners = lock_or_recover(&backend.pane_by_tab);
    match owners.get(&tab_id) {
        Some(owner) if pane_is_live(backend, owner) => owner == pane_id,
        Some(_) => {
            owners.insert(tab_id, pane_id.to_string());
            true
        }
        None => {
            owners.insert(tab_id, pane_id.to_string());
            true
        }
    }
}

fn pane_is_live(backend: &BrowserBackend, pane_id: &str) -> bool {
    backend
        .app_state
        .as_ref()
        .is_none_or(|state| state.pane_exists(pane_id).unwrap_or(false))
}

fn retain_live_browser_ownership(backend: &BrowserBackend, live_tab_ids: &HashSet<u64>) {
    lock_or_recover(&backend.pane_by_tab)
        .retain(|tab_id, owner| live_tab_ids.contains(tab_id) && pane_is_live(backend, owner));
    lock_or_recover(&backend.tab_by_pane)
        .retain(|owner, tab_id| live_tab_ids.contains(tab_id) && pane_is_live(backend, owner));
    let selected_tabs = lock_or_recover(&backend.tab_by_pane).clone();
    lock_or_recover(&backend.viewport_by_pane).retain(|owner, viewport| {
        selected_tabs.get(owner) == Some(&viewport.tab_id)
            && live_tab_ids.contains(&viewport.tab_id)
    });
    // A selected-tab change invalidates the viewport cache, but the active
    // screencast record must survive until the heartbeat stops that old tab.
    // Targets already gone need no stop; streams owned by panes that vanished
    // unexpectedly are actively reaped instead of merely forgotten.
    let orphaned_screencasts = {
        let mut streams = lock_or_recover(&backend.screencast_by_pane);
        streams.retain(|_, viewport| live_tab_ids.contains(&viewport.tab_id));
        streams
            .iter()
            .filter(|(owner, _)| !pane_is_live(backend, owner))
            .map(|(owner, viewport)| (owner.clone(), *viewport))
            .collect::<Vec<_>>()
    };
    for (owner, viewport) in orphaned_screencasts {
        if let Err(error) = stop_recorded_screencast(backend, &owner, viewport) {
            eprintln!("qmux: failed to stop orphaned pane {owner} screencast: {error}");
        }
    }
    lock_or_recover(&backend.pane_by_session).retain(|_, owner| pane_is_live(backend, owner));
}

fn scope_result_to_pane(
    backend: &BrowserBackend,
    pane_id: Option<&str>,
    method: &str,
    mut result: Value,
) -> Value {
    let (Some(pane_id), "getTabs", Some(tabs)) = (pane_id, method, result.as_array_mut()) else {
        return result;
    };
    let live_tab_ids = tabs
        .iter()
        .filter_map(|tab| tab.get("id").and_then(Value::as_u64))
        .collect::<HashSet<_>>();
    {
        let mut owners = lock_or_recover(&backend.pane_by_tab);
        owners
            .retain(|tab_id, owner| live_tab_ids.contains(tab_id) && pane_is_live(backend, owner));
        if !owners.values().any(|owner| owner == pane_id) {
            let claim = tabs
                .iter()
                .find(|tab| {
                    tab.get("active") == Some(&Value::Bool(true))
                        && tab
                            .get("id")
                            .and_then(Value::as_u64)
                            .is_some_and(|tab_id| !owners.contains_key(&tab_id))
                })
                .or_else(|| {
                    tabs.iter().find(|tab| {
                        tab.get("id")
                            .and_then(Value::as_u64)
                            .is_some_and(|tab_id| !owners.contains_key(&tab_id))
                    })
                })
                .and_then(|tab| tab.get("id"))
                .and_then(Value::as_u64);
            if let Some(tab_id) = claim {
                owners.insert(tab_id, pane_id.to_string());
                lock_or_recover(&backend.tab_by_pane).insert(pane_id.to_string(), tab_id);
            }
        }
    }
    retain_live_browser_ownership(backend, &live_tab_ids);
    let owners = lock_or_recover(&backend.pane_by_tab);
    tabs.retain(|tab| {
        tab.get("id")
            .and_then(Value::as_u64)
            .and_then(|tab_id| owners.get(&tab_id))
            .is_some_and(|owner| owner == pane_id)
    });
    let selected = lock_or_recover(&backend.tab_by_pane)
        .get(pane_id)
        .copied()
        .filter(|selected| {
            tabs.iter()
                .any(|tab| tab.get("id").and_then(Value::as_u64) == Some(*selected))
        })
        .or_else(|| {
            tabs.first()
                .and_then(|tab| tab.get("id"))
                .and_then(Value::as_u64)
        });
    drop(owners);
    if let Some(selected) = selected {
        lock_or_recover(&backend.tab_by_pane).insert(pane_id.to_string(), selected);
        for tab in tabs {
            if let Some(tab) = tab.as_object_mut() {
                let active = tab.get("id").and_then(Value::as_u64) == Some(selected);
                tab.insert("active".to_string(), Value::Bool(active));
            }
        }
    }
    result
}

fn event_is_visible_to_pane(
    backend: &BrowserBackend,
    pane_id: Option<&str>,
    message: &Value,
) -> bool {
    let Some(pane_id) = pane_id else {
        return true;
    };
    if message.get("id").is_some() {
        return true;
    }
    let tab_id = message
        .pointer("/params/source/tabId")
        .or_else(|| message.pointer("/params/tabId"))
        .and_then(Value::as_u64);
    let Some(tab_id) = tab_id else {
        return false;
    };
    lock_or_recover(&backend.pane_by_tab)
        .get(&tab_id)
        .is_some_and(|owner| owner == pane_id)
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn pane_for_process(state: &AppState, process_id: u32) -> Option<String> {
    for (pane_id, child) in state.all_pane_children().ok()? {
        let Some(root_pid) = child.lock().ok().and_then(|guard| guard.process_id()) else {
            continue;
        };
        if root_pid == process_id
            || crate::pty::descendant_process_ids(root_pid).contains(&process_id)
        {
            return Some(pane_id);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn peer_process_id(stream: &UnixStream) -> Option<u32> {
    let mut pid: libc::pid_t = 0;
    let mut length = std::mem::size_of_val(&pid) as libc::socklen_t;
    // SAFETY: pid and length point to writable storage of the exact types and
    // sizes required by LOCAL_PEERPID; stream owns a valid Unix socket fd.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut length,
        )
    };
    (result == 0 && pid > 0).then_some(pid as u32)
}

#[cfg(target_os = "linux")]
fn peer_process_id(stream: &UnixStream) -> Option<u32> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of_val(&credentials) as libc::socklen_t;
    // SAFETY: credentials and length are valid writable storage for SO_PEERCRED.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    (result == 0 && credentials.pid > 0).then_some(credentials.pid as u32)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn peer_process_id(_stream: &UnixStream) -> Option<u32> {
    None
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserAutomationSnapshot {
    available: bool,
    tab_id: Option<u64>,
    url: Option<String>,
    title: Option<String>,
    image_data_url: Option<String>,
    width: u64,
    height: u64,
    error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserAutomationTarget {
    pane_id: String,
    tab_id: u64,
    url: Option<String>,
    title: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserAutomationPreview {
    pane_id: String,
    tab_id: u64,
    image_data_url: String,
    width: u32,
    height: u32,
}

fn owned_browser_targets(backend: &BrowserBackend, tabs: &[Value]) -> Vec<BrowserAutomationTarget> {
    let live_tab_ids = tabs
        .iter()
        .filter_map(|tab| tab.get("id").and_then(Value::as_u64))
        .collect::<HashSet<_>>();
    retain_live_browser_ownership(backend, &live_tab_ids);

    let selected = lock_or_recover(&backend.tab_by_pane).clone();
    let owners = lock_or_recover(&backend.pane_by_tab);
    let mut targets = selected
        .into_iter()
        .filter_map(|(pane_id, tab_id)| {
            if owners.get(&tab_id).map(String::as_str) != Some(pane_id.as_str()) {
                return None;
            }
            let tab = tabs
                .iter()
                .find(|tab| tab.get("id").and_then(Value::as_u64) == Some(tab_id))?;
            Some(BrowserAutomationTarget {
                pane_id,
                tab_id,
                url: tab.get("url").and_then(Value::as_str).map(str::to_string),
                title: tab.get("title").and_then(Value::as_str).map(str::to_string),
            })
        })
        .collect::<Vec<_>>();
    targets.sort_by(|left, right| left.pane_id.cmp(&right.pane_id));
    targets
}

/// List only tabs that a live qmux pane already owns. Unlike the full mirror
/// commands, this discovery path never creates or claims a Chromium target.
#[tauri::command(async)]
pub fn browser_automation_targets(
    browser: tauri::State<'_, BrowserDiscoverySocket>,
) -> Result<Vec<BrowserAutomationTarget>, String> {
    let tabs = browser.engine()?.call("getTabs", json!({}))?;
    let tabs = tabs
        .as_array()
        .ok_or_else(|| "qmux chrome-headless-shell returned an invalid tab list".to_string())?;
    Ok(owned_browser_targets(&browser._backend, tabs))
}

fn metric_number(metrics: &Value, visual_field: &str, layout_field: &str) -> Option<f64> {
    metrics
        .pointer(visual_field)
        .or_else(|| metrics.pointer(layout_field))
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
}

fn preview_clip(
    metrics: &Value,
    max_width: u32,
    max_height: u32,
) -> Result<(Value, u32, u32), String> {
    let source_width = metric_number(
        metrics,
        "/cssVisualViewport/clientWidth",
        "/cssLayoutViewport/clientWidth",
    )
    .filter(|value| *value > 0.0)
    .ok_or_else(|| "Page.getLayoutMetrics returned no viewport width".to_string())?;
    let source_height = metric_number(
        metrics,
        "/cssVisualViewport/clientHeight",
        "/cssLayoutViewport/clientHeight",
    )
    .filter(|value| *value > 0.0)
    .ok_or_else(|| "Page.getLayoutMetrics returned no viewport height".to_string())?;
    let page_x = metric_number(
        metrics,
        "/cssVisualViewport/pageX",
        "/cssLayoutViewport/pageX",
    )
    .unwrap_or(0.0);
    let page_y = metric_number(
        metrics,
        "/cssVisualViewport/pageY",
        "/cssLayoutViewport/pageY",
    )
    .unwrap_or(0.0);
    let scale = (f64::from(max_width) / source_width)
        .min(f64::from(max_height) / source_height)
        .min(1.0)
        .max(0.01);
    let width = (source_width * scale).round().max(1.0) as u32;
    let height = (source_height * scale).round().max(1.0) as u32;
    Ok((
        json!({
            "x": page_x,
            "y": page_y,
            "width": source_width,
            "height": source_height,
            "scale": scale
        }),
        width,
        height,
    ))
}

/// Capture a small image of an existing pane-owned tab without changing its
/// emulated metrics. This keeps the PiP observational: responsive layout and
/// pointer coordinates remain under the browser client's control.
#[tauri::command(async)]
pub fn browser_automation_preview(
    pane_id: String,
    tab_id: u64,
    max_width: u32,
    max_height: u32,
    browser: tauri::State<'_, BrowserDiscoverySocket>,
) -> Result<BrowserAutomationPreview, String> {
    if !pane_is_live(&browser._backend, &pane_id)
        || lock_or_recover(&browser._backend.tab_by_pane).get(&pane_id) != Some(&tab_id)
        || lock_or_recover(&browser._backend.pane_by_tab).get(&tab_id) != Some(&pane_id)
    {
        return Err("browser tab is not owned by this qmux pane".to_string());
    }
    let max_width = max_width.clamp(120, 640);
    let max_height = max_height.clamp(80, 480);
    let metrics = browser.execute_for_tab(tab_id, "Page.getLayoutMetrics", json!({}))?;
    let (clip, width, height) = preview_clip(&metrics, max_width, max_height)?;
    let screenshot = browser.execute_for_tab(
        tab_id,
        "Page.captureScreenshot",
        json!({
            "format": "jpeg",
            "quality": AGENT_PREVIEW_JPEG_QUALITY,
            "clip": clip,
            "fromSurface": true,
            "captureBeyondViewport": false,
            "optimizeForSpeed": true
        }),
    )?;
    let data = screenshot
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| "Page.captureScreenshot returned no image".to_string())?;
    Ok(BrowserAutomationPreview {
        pane_id,
        tab_id,
        image_data_url: format!("data:image/jpeg;base64,{data}"),
        width,
        height,
    })
}

impl BrowserDiscoverySocket {
    fn engine(&self) -> Result<&BrowserEngine, String> {
        self._backend.engine.as_deref().ok_or_else(|| {
            self._backend.engine_error.clone().unwrap_or_else(|| {
                "qmux chrome-headless-shell automation is unavailable".to_string()
            })
        })
    }

    fn tab_id_for_pane(&self, pane_id: &str) -> Result<u64, String> {
        if let Some(tab_id) = lock_or_recover(&self._backend.tab_by_pane)
            .get(pane_id)
            .copied()
        {
            return Ok(tab_id);
        }
        let created = self.engine()?.call("createTab", json!({}))?;
        let selected = created
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| "qmux chrome-headless-shell returned an invalid tab".to_string())?;
        lock_or_recover(&self._backend.tab_by_pane).insert(pane_id.to_string(), selected);
        lock_or_recover(&self._backend.pane_by_tab).insert(selected, pane_id.to_string());
        Ok(selected)
    }

    fn execute_for_pane(
        &self,
        pane_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let tab_id = self.tab_id_for_pane(pane_id)?;
        self.execute_for_tab(tab_id, method, params)
    }

    fn execute_for_tab(&self, tab_id: u64, method: &str, params: Value) -> Result<Value, String> {
        execute_for_engine_tab(self.engine()?, tab_id, method, params)
    }
}

fn execute_for_engine_tab(
    engine: &BrowserEngine,
    tab_id: u64,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    engine.call(
        "executeCdp",
        json!({
            "target": { "tabId": tab_id },
            "method": method,
            "commandParams": params,
            "timeoutMs": 10_000
        }),
    )
}

fn stop_recorded_screencast(
    backend: &BrowserBackend,
    pane_id: &str,
    expected: BrowserViewport,
) -> Result<(), String> {
    let _control = lock_or_recover(&backend.screencast_control);
    if lock_or_recover(&backend.screencast_by_pane)
        .get(pane_id)
        .copied()
        != Some(expected)
    {
        return Ok(());
    }
    let result = backend.engine.as_deref().map_or(Ok(Value::Null), |engine| {
        execute_for_engine_tab(engine, expected.tab_id, "Page.stopScreencast", json!({}))
    });
    match result {
        Ok(_) => {
            remove_matching_screencast(backend, pane_id, expected);
            Ok(())
        }
        Err(error) if browser_tab_no_longer_exists(&error) => {
            remove_matching_screencast(backend, pane_id, expected);
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn browser_scale_factor(scale_factor: f64) -> f64 {
    if scale_factor.is_finite() {
        scale_factor.clamp(1.0, 2.0)
    } else {
        1.0
    }
}

/// The mirrored tab a pane is currently looking at, with Chromium already
/// emulating the overlay's size and the display's scale.
struct AgentMirrorTarget {
    viewport: BrowserViewport,
    url: Option<String>,
    title: Option<String>,
}

impl AgentMirrorTarget {
    fn into_snapshot(self, image_data_url: Option<String>) -> BrowserAutomationSnapshot {
        BrowserAutomationSnapshot {
            available: true,
            tab_id: Some(self.viewport.tab_id),
            url: self.url,
            title: self.title,
            image_data_url,
            width: u64::from(self.viewport.width),
            height: u64::from(self.viewport.height),
            error: None,
        }
    }
}

/// Resolve the pane's tab and make Chromium render at `width`x`height` CSS
/// pixels with `scale_factor` device pixels each. The metrics override is only
/// re-sent when something actually changed, so this is cheap to call on a
/// heartbeat.
fn prepare_agent_mirror(
    browser: &BrowserDiscoverySocket,
    pane_id: &str,
    width: u32,
    height: u32,
    scale_factor: f64,
) -> Result<AgentMirrorTarget, String> {
    let mut tab_id = browser.tab_id_for_pane(pane_id)?;
    let mut tabs = browser.engine()?.call("getTabs", json!({}))?;
    let mut tab = tabs
        .as_array()
        .and_then(|tabs| {
            tabs.iter()
                .find(|tab| tab.get("id").and_then(Value::as_u64) == Some(tab_id))
        })
        .cloned();
    if tab.is_none() {
        lock_or_recover(&browser._backend.pane_by_tab).remove(&tab_id);
        lock_or_recover(&browser._backend.tab_by_pane).remove(pane_id);
        lock_or_recover(&browser._backend.viewport_by_pane).remove(pane_id);
        lock_or_recover(&browser._backend.screencast_by_pane).remove(pane_id);
        tab_id = browser.tab_id_for_pane(pane_id)?;
        tabs = browser.engine()?.call("getTabs", json!({}))?;
        tab = tabs
            .as_array()
            .and_then(|tabs| {
                tabs.iter()
                    .find(|tab| tab.get("id").and_then(Value::as_u64) == Some(tab_id))
            })
            .cloned();
    }
    let tab = tab.ok_or_else(|| format!("browser tab {tab_id} no longer exists"))?;
    let viewport = BrowserViewport {
        tab_id,
        width,
        height,
        scale_factor,
    };
    let viewport_changed = lock_or_recover(&browser._backend.viewport_by_pane)
        .get(pane_id)
        .copied()
        != Some(viewport);
    if viewport_changed {
        browser.execute_for_pane(
            pane_id,
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": width,
                "height": height,
                "deviceScaleFactor": scale_factor,
                "mobile": false,
                "screenWidth": width,
                "screenHeight": height
            }),
        )?;
        lock_or_recover(&browser._backend.viewport_by_pane).insert(pane_id.to_string(), viewport);
    }
    Ok(AgentMirrorTarget {
        viewport,
        url: tab.get("url").and_then(Value::as_str).map(str::to_string),
        title: tab.get("title").and_then(Value::as_str).map(str::to_string),
    })
}

fn unavailable_snapshot(width: u32, height: u32, error: String) -> BrowserAutomationSnapshot {
    BrowserAutomationSnapshot {
        available: false,
        tab_id: None,
        url: None,
        title: None,
        image_data_url: None,
        width: u64::from(width.clamp(320, 4096)),
        height: u64::from(height.clamp(240, 4096)),
        error: Some(error),
    }
}

#[tauri::command(async)]
pub fn browser_automation_snapshot(
    pane_id: String,
    width: u32,
    height: u32,
    scale_factor: f64,
    browser: tauri::State<'_, BrowserDiscoverySocket>,
) -> BrowserAutomationSnapshot {
    let result = (|| -> Result<BrowserAutomationSnapshot, String> {
        let target = prepare_agent_mirror(
            &browser,
            &pane_id,
            width.clamp(320, 4096),
            height.clamp(240, 4096),
            browser_scale_factor(scale_factor),
        )?;
        let screenshot = browser.execute_for_pane(
            &pane_id,
            "Page.captureScreenshot",
            json!({
                "format": "jpeg",
                "quality": AGENT_MIRROR_JPEG_QUALITY,
                "fromSurface": true,
                "captureBeyondViewport": false,
                "optimizeForSpeed": true
            }),
        )?;
        let data = screenshot
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| "Page.captureScreenshot returned no image".to_string())?;
        Ok(target.into_snapshot(Some(format!("data:image/jpeg;base64,{data}"))))
    })();
    result.unwrap_or_else(|error| unavailable_snapshot(width, height, error))
}

/// Never stream more pixels than the display can show. Chromium's screencast
/// scale only ever shrinks a frame, so this is a ceiling rather than a target:
/// it bounds an unexpectedly large surface without shrinking ordinary frames.
fn screencast_max_dimensions(viewport: BrowserViewport) -> (u64, u64) {
    let width = f64::from(viewport.width) * viewport.scale_factor;
    let height = f64::from(viewport.height) * viewport.scale_factor;
    (width.round() as u64, height.round() as u64)
}

/// Start — or reconfigure — the pane's screencast and report the mirrored tab.
///
/// Chromium is only touched when the tab, overlay size, or display scale
/// changed, so the overlay can call this on a slow heartbeat to keep its
/// address bar current and to recover after the mirrored tab is replaced.
///
/// Streamed frames are CSS-resolution however the display scales — Chromium's
/// screencast can only scale a frame down — so the mirror is a two-speed
/// image: frames carry motion, and `browser_automation_snapshot` supplies the
/// Retina-scale capture the overlay settles on once the page stops painting.
#[tauri::command(async)]
pub fn browser_automation_start_screencast(
    pane_id: String,
    width: u32,
    height: u32,
    scale_factor: f64,
    browser: tauri::State<'_, BrowserDiscoverySocket>,
) -> BrowserAutomationSnapshot {
    let result = (|| -> Result<BrowserAutomationSnapshot, String> {
        let _control = lock_or_recover(&browser._backend.screencast_control);
        let target = prepare_agent_mirror(
            &browser,
            &pane_id,
            width.clamp(320, 4096),
            height.clamp(240, 4096),
            browser_scale_factor(scale_factor),
        )?;
        let viewport = target.viewport;
        let streaming = lock_or_recover(&browser._backend.screencast_by_pane)
            .get(&pane_id)
            .copied();
        if streaming != Some(viewport) {
            if let Some(previous) = streaming {
                // Stop a pane's previous tab (or previous size configuration)
                // before replacing the only record that can address it.
                match browser.execute_for_tab(previous.tab_id, "Page.stopScreencast", json!({})) {
                    Ok(_) => {
                        remove_matching_screencast(&browser._backend, &pane_id, previous);
                    }
                    // A closed target already stopped its own screencast. Its
                    // stale backend record must not prevent the replacement
                    // tab from starting.
                    Err(error)
                        if previous.tab_id != viewport.tab_id
                            && browser_tab_no_longer_exists(&error) =>
                    {
                        remove_matching_screencast(&browser._backend, &pane_id, previous);
                    }
                    Err(error) => return Err(error),
                }
            }
            let (max_width, max_height) = screencast_max_dimensions(viewport);
            // Register routing before issuing Page.startScreencast. Chromium
            // can send the initial frame as soon as the command is accepted,
            // on the controller thread that is waking this caller.
            lock_or_recover(&browser._backend.screencast_by_pane).insert(pane_id.clone(), viewport);
            if let Err(error) = browser.execute_for_pane(
                &pane_id,
                "Page.startScreencast",
                json!({
                    "format": "jpeg",
                    "quality": AGENT_MIRROR_JPEG_QUALITY,
                    "maxWidth": max_width,
                    "maxHeight": max_height,
                    "everyNthFrame": 1
                }),
            ) {
                remove_matching_screencast(&browser._backend, &pane_id, viewport);
                return Err(error);
            }
        }
        Ok(target.into_snapshot(None))
    })();
    result.unwrap_or_else(|error| unavailable_snapshot(width, height, error))
}

#[tauri::command(async)]
pub fn browser_automation_stop_screencast(
    pane_id: String,
    browser: tauri::State<'_, BrowserDiscoverySocket>,
) -> Result<(), String> {
    let Some(streaming) = lock_or_recover(&browser._backend.screencast_by_pane)
        .get(&pane_id)
        .copied()
    else {
        return Ok(());
    };
    // Address the recorded tab rather than the pane: resolving a pane with no
    // tab left would open one, and closing an overlay must never do that.
    stop_recorded_screencast(&browser._backend, &pane_id, streaming)
}

fn remove_matching_screencast(backend: &BrowserBackend, pane_id: &str, expected: BrowserViewport) {
    let mut streams = lock_or_recover(&backend.screencast_by_pane);
    if streams.get(pane_id).copied() == Some(expected) {
        streams.remove(pane_id);
    }
}

fn browser_tab_no_longer_exists(error: &str) -> bool {
    error.contains(" does not exist")
}

/// Forward screencast frames from the CDP controller to the pane that owns the
/// streaming tab. Frames are dropped rather than queued when no mirror wants
/// them, so a closed overlay costs nothing until its screencast is stopped.
fn start_screencast_pump(backend: &Arc<BrowserBackend>) -> Result<(), String> {
    let Some(engine) = backend.engine.as_ref() else {
        return Ok(());
    };
    let (frames_tx, frames_rx) = mpsc::sync_channel(SCREENCAST_FRAME_QUEUE);
    engine.set_screencast_sink(Some(frames_tx));
    let pump_backend = Arc::clone(backend);
    thread::Builder::new()
        .name("qmux-browser-screencast".to_string())
        .spawn(move || {
            // One budget across every mirrored pane, so the bridge's total
            // cost is bounded however many panes are streaming at once.
            let mut emitted_at: Option<Instant> = None;
            while let Ok(frame) = frames_rx.recv() {
                let now = Instant::now();
                if emitted_at
                    .is_some_and(|last| now.duration_since(last) < SCREENCAST_MIN_FRAME_INTERVAL)
                {
                    continue;
                }
                if emit_screencast_frame(&pump_backend, &frame) {
                    emitted_at = Some(now);
                }
            }
        })
        .map_err(|err| format!("failed to start the browser screencast pump: {err}"))?;
    Ok(())
}

/// Returns whether the frame reached a pane that wanted it.
fn emit_screencast_frame(backend: &BrowserBackend, frame: &ScreencastFrame) -> bool {
    let Some(pane_id) = lock_or_recover(&backend.pane_by_tab)
        .get(&frame.tab_id)
        .cloned()
    else {
        return false;
    };
    let Some(viewport) = lock_or_recover(&backend.screencast_by_pane)
        .get(&pane_id)
        .copied()
        .filter(|viewport| viewport.tab_id == frame.tab_id)
    else {
        return false;
    };
    let Some(app_handle) = backend
        .app_state
        .as_ref()
        .and_then(|state| state.app_handle())
    else {
        return false;
    };
    let _ = app_handle.emit(
        SCREENCAST_FRAME_EVENT,
        json!({
            "paneId": pane_id,
            "tabId": frame.tab_id,
            "url": frame.url,
            "title": frame.title,
            "width": viewport.width,
            "height": viewport.height,
            "imageDataUrl": format!("data:image/jpeg;base64,{}", frame.data),
        }),
    );
    true
}

#[tauri::command(async)]
pub fn browser_automation_navigate(
    pane_id: String,
    url: String,
    browser: tauri::State<'_, BrowserDiscoverySocket>,
) -> Result<(), String> {
    browser.execute_for_pane(&pane_id, "Page.navigate", json!({ "url": url }))?;
    Ok(())
}

#[tauri::command(async)]
pub fn browser_automation_reload(
    pane_id: String,
    browser: tauri::State<'_, BrowserDiscoverySocket>,
) -> Result<(), String> {
    browser.execute_for_pane(&pane_id, "Page.reload", json!({}))?;
    Ok(())
}

#[tauri::command(async)]
pub fn browser_automation_mouse(
    pane_id: String,
    kind: String,
    x: f64,
    y: f64,
    delta_x: Option<f64>,
    delta_y: Option<f64>,
    button: Option<String>,
    buttons: Option<u32>,
    modifiers: Option<u32>,
    browser: tauri::State<'_, BrowserDiscoverySocket>,
) -> Result<(), String> {
    if !x.is_finite() || !y.is_finite() {
        return Err("browser pointer coordinates must be finite".to_string());
    }
    let viewport = lock_or_recover(&browser._backend.viewport_by_pane)
        .get(&pane_id)
        .copied();
    let x = x.clamp(
        0.0,
        f64::from(viewport.map_or(1280, |viewport| viewport.width)),
    );
    let y = y.clamp(
        0.0,
        f64::from(viewport.map_or(900, |viewport| viewport.height)),
    );
    let delta_x = delta_x.unwrap_or(0.0);
    let delta_y = delta_y.unwrap_or(0.0);
    if !delta_x.is_finite() || !delta_y.is_finite() {
        return Err("browser scroll deltas must be finite".to_string());
    }
    let button = button.unwrap_or_else(|| "none".to_string());
    if !matches!(button.as_str(), "none" | "left" | "middle" | "right") {
        return Err(format!("unsupported browser mouse button '{button}'"));
    }
    let buttons = buttons.unwrap_or(0).min(7);
    let modifiers = modifiers.unwrap_or(0).min(15);
    match kind.as_str() {
        "move" => {
            browser.execute_for_pane(
                &pane_id,
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseMoved",
                    "x": x,
                    "y": y,
                    "button": "none",
                    "buttons": buttons,
                    "modifiers": modifiers
                }),
            )?;
        }
        "down" | "up" => {
            browser.execute_for_pane(
                &pane_id,
                "Input.dispatchMouseEvent",
                json!({
                    "type": if kind == "down" { "mousePressed" } else { "mouseReleased" },
                    "x": x,
                    "y": y,
                    "button": button,
                    "buttons": buttons,
                    "modifiers": modifiers,
                    "clickCount": 1
                }),
            )?;
        }
        "click" => {
            for event_type in ["mousePressed", "mouseReleased"] {
                browser.execute_for_pane(
                    &pane_id,
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": event_type,
                        "x": x,
                        "y": y,
                        "button": "left",
                        "modifiers": modifiers,
                        "clickCount": 1
                    }),
                )?;
            }
        }
        "scroll" => {
            browser.execute_for_pane(
                &pane_id,
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseWheel",
                    "x": x,
                    "y": y,
                    "deltaX": delta_x.clamp(-10_000.0, 10_000.0),
                    "deltaY": delta_y.clamp(-10_000.0, 10_000.0),
                    "modifiers": modifiers
                }),
            )?;
        }
        _ => return Err(format!("unsupported browser mouse event '{kind}'")),
    }
    Ok(())
}

#[tauri::command(async)]
pub fn browser_automation_insert_text(
    pane_id: String,
    text: String,
    browser: tauri::State<'_, BrowserDiscoverySocket>,
) -> Result<(), String> {
    if text.len() > 1024 * 1024 {
        return Err("browser text input exceeds 1 MiB".to_string());
    }
    browser.execute_for_pane(&pane_id, "Input.insertText", json!({ "text": text }))?;
    Ok(())
}

#[tauri::command(async)]
pub fn browser_automation_key(
    pane_id: String,
    key: String,
    code: String,
    windows_virtual_key_code: u32,
    modifiers: u32,
    browser: tauri::State<'_, BrowserDiscoverySocket>,
) -> Result<(), String> {
    if windows_virtual_key_code > 255 {
        return Err("browser virtual key code is out of range".to_string());
    }
    if modifiers > 15 {
        return Err("browser key modifiers are out of range".to_string());
    }
    for event_type in ["rawKeyDown", "keyUp"] {
        browser.execute_for_pane(
            &pane_id,
            "Input.dispatchKeyEvent",
            json!({
                "type": event_type,
                "key": key,
                "code": code,
                "windowsVirtualKeyCode": windows_virtual_key_code,
                "nativeVirtualKeyCode": windows_virtual_key_code,
                "modifiers": modifiers
            }),
        )?;
    }
    Ok(())
}

fn read_frame(stream: &mut UnixStream) -> Result<Option<Value>, String> {
    let mut length = [0_u8; 4];
    match stream.read_exact(&mut length) {
        Ok(()) => {}
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::UnexpectedEof | ErrorKind::ConnectionReset
            ) =>
        {
            return Ok(None);
        }
        Err(err) => return Err(format!("failed to read frame length: {err}")),
    }
    let length = u32::from_ne_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(format!("invalid browser discovery frame length {length}"));
    }
    let mut payload = vec![0_u8; length];
    stream
        .read_exact(&mut payload)
        .map_err(|err| format!("failed to read browser discovery frame: {err}"))?;
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|err| format!("invalid browser discovery JSON: {err}"))
}

fn write_frame(stream: &mut UnixStream, value: &Value) -> Result<(), String> {
    let payload = serde_json::to_vec(value)
        .map_err(|err| format!("failed to encode browser discovery response: {err}"))?;
    let length = u32::try_from(payload.len())
        .map_err(|_| "browser discovery response is too large".to_string())?;
    stream
        .write_all(&length.to_ne_bytes())
        .and_then(|_| stream.write_all(&payload))
        .map_err(|err| format!("failed to write browser discovery response: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn unavailable_backend() -> BrowserBackend {
        BrowserBackend {
            engine: None,
            engine_error: Some("test browser unavailable".to_string()),
            app_state: None,
            pane_by_session: Mutex::new(HashMap::new()),
            tab_by_pane: Mutex::new(HashMap::new()),
            pane_by_tab: Mutex::new(HashMap::new()),
            viewport_by_pane: Mutex::new(HashMap::new()),
            screencast_control: Mutex::new(()),
            screencast_by_pane: Mutex::new(HashMap::new()),
        }
    }

    #[test]
    fn get_info_echoes_codex_session_metadata() {
        let response = handle_request(
            &unavailable_backend(),
            None,
            &json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "getInfo",
                "params": { "session_id": "session-123" }
            }),
            json!(7),
        );
        assert_eq!(response["result"]["type"], "iab");
        assert_eq!(
            response["result"]["metadata"]["codexSessionId"],
            "session-123"
        );
        assert_eq!(response["result"]["capabilities"]["browser"], json!([]));
    }

    #[test]
    fn native_length_prefixed_json_round_trips() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let request = json!({"jsonrpc": "2.0", "id": "a", "method": "ping"});
        write_frame(&mut client, &request).unwrap();
        assert_eq!(read_frame(&mut server).unwrap(), Some(request));
    }

    #[test]
    fn unknown_methods_return_json_rpc_error() {
        let response = handle_request(
            &unavailable_backend(),
            None,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "createTab"}),
            json!(1),
        );
        assert_eq!(response["error"]["code"], -32001);
    }

    #[test]
    fn browser_targets_report_only_existing_pane_owned_tabs() {
        let backend = unavailable_backend();
        lock_or_recover(&backend.tab_by_pane).insert("pane-b".to_string(), 5);
        lock_or_recover(&backend.tab_by_pane).insert("pane-a".to_string(), 4);
        lock_or_recover(&backend.tab_by_pane).insert("stale-pane".to_string(), 8);
        lock_or_recover(&backend.pane_by_tab).insert(4, "pane-a".to_string());
        lock_or_recover(&backend.pane_by_tab).insert(5, "pane-b".to_string());
        lock_or_recover(&backend.pane_by_tab).insert(8, "stale-pane".to_string());
        let targets = owned_browser_targets(
            &backend,
            &[
                json!({ "id": 4, "url": "https://a.test/", "title": "A" }),
                json!({ "id": 5, "url": "https://b.test/", "title": "B" }),
                // A live but unowned Chromium target must not be claimed just
                // because the PiP discovery path observed it.
                json!({ "id": 9, "url": "about:blank", "title": "" }),
            ],
        );

        assert_eq!(
            targets,
            vec![
                BrowserAutomationTarget {
                    pane_id: "pane-a".to_string(),
                    tab_id: 4,
                    url: Some("https://a.test/".to_string()),
                    title: Some("A".to_string()),
                },
                BrowserAutomationTarget {
                    pane_id: "pane-b".to_string(),
                    tab_id: 5,
                    url: Some("https://b.test/".to_string()),
                    title: Some("B".to_string()),
                },
            ]
        );
        assert!(!lock_or_recover(&backend.pane_by_tab).contains_key(&8));
        assert!(!lock_or_recover(&backend.tab_by_pane).contains_key("stale-pane"));
        assert!(!lock_or_recover(&backend.pane_by_tab).contains_key(&9));
    }

    #[test]
    fn browser_preview_clip_fits_without_upscaling() {
        let metrics = json!({
            "cssVisualViewport": {
                "pageX": 12.0,
                "pageY": 34.0,
                "clientWidth": 1280.0,
                "clientHeight": 900.0
            }
        });
        let (clip, width, height) = preview_clip(&metrics, 240, 150).unwrap();
        assert_eq!((width, height), (213, 150));
        assert_eq!(clip["x"], 12.0);
        assert_eq!(clip["y"], 34.0);
        assert_eq!(clip["width"], 1280.0);
        assert_eq!(clip["height"], 900.0);
        assert_eq!(clip["scale"].as_f64(), Some(1.0 / 6.0));

        let (clip, width, height) = preview_clip(
            &json!({
                "cssLayoutViewport": { "clientWidth": 100.0, "clientHeight": 50.0 }
            }),
            240,
            150,
        )
        .unwrap();
        assert_eq!((width, height), (100, 50));
        assert_eq!(clip["scale"], 1.0);
    }

    #[test]
    #[ignore = "launches an installed chrome-headless-shell"]
    fn browser_preview_capture_preserves_existing_metrics() {
        let engine = BrowserEngine::start().unwrap();
        let tab = engine.call("createTab", json!({})).unwrap();
        let tab_id = tab["id"].as_u64().unwrap();
        execute_for_engine_tab(
            &engine,
            tab_id,
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": 800,
                "height": 600,
                "deviceScaleFactor": 1,
                "mobile": false,
                "screenWidth": 800,
                "screenHeight": 600
            }),
        )
        .unwrap();
        let before =
            execute_for_engine_tab(&engine, tab_id, "Page.getLayoutMetrics", json!({})).unwrap();
        let (clip, width, height) = preview_clip(&before, 200, 120).unwrap();
        let screenshot = execute_for_engine_tab(
            &engine,
            tab_id,
            "Page.captureScreenshot",
            json!({
                "format": "jpeg",
                "quality": AGENT_PREVIEW_JPEG_QUALITY,
                "clip": clip,
                "fromSurface": true,
                "captureBeyondViewport": false,
                "optimizeForSpeed": true
            }),
        )
        .unwrap();
        assert!(!screenshot["data"].as_str().unwrap_or_default().is_empty());
        assert_eq!((width, height), (160, 120));

        let after =
            execute_for_engine_tab(&engine, tab_id, "Page.getLayoutMetrics", json!({})).unwrap();
        assert_eq!(
            before.pointer("/cssVisualViewport/clientWidth"),
            after.pointer("/cssVisualViewport/clientWidth")
        );
        assert_eq!(
            before.pointer("/cssVisualViewport/clientHeight"),
            after.pointer("/cssVisualViewport/clientHeight")
        );
    }

    #[test]
    fn agent_viewport_changes_invalidate_the_mirror_cache() {
        let backend = unavailable_backend();
        let viewport = BrowserViewport {
            tab_id: 7,
            width: 1280,
            height: 900,
            scale_factor: 2.0,
        };
        lock_or_recover(&backend.viewport_by_pane).insert("pane-1".to_string(), viewport);
        lock_or_recover(&backend.screencast_by_pane).insert("pane-1".to_string(), viewport);

        remember_tab_owner(
            &backend,
            Some("pane-1"),
            "executeCdp",
            &json!({
                "params": {
                    "target": { "tabId": 7 },
                    "method": "Emulation.setDeviceMetricsOverride"
                }
            }),
            &Value::Null,
        );

        assert!(lock_or_recover(&backend.viewport_by_pane).is_empty());
        // The client owns the frame size now; the next heartbeat has to restart
        // the screencast rather than keep streaming at the stale bounds.
        assert!(lock_or_recover(&backend.screencast_by_pane).is_empty());
    }

    #[test]
    fn a_closed_tab_drops_its_pane_screencast() {
        let backend = unavailable_backend();
        lock_or_recover(&backend.pane_by_tab).insert(4, "pane-1".to_string());
        lock_or_recover(&backend.tab_by_pane).insert("pane-1".to_string(), 4);
        lock_or_recover(&backend.screencast_by_pane).insert(
            "pane-1".to_string(),
            BrowserViewport {
                tab_id: 4,
                width: 800,
                height: 600,
                scale_factor: 2.0,
            },
        );

        scope_result_to_pane(
            &backend,
            Some("pane-1"),
            "getTabs",
            json!([{ "id": 9, "url": "about:blank", "title": "", "active": true }]),
        );

        assert!(lock_or_recover(&backend.screencast_by_pane).is_empty());
    }

    #[test]
    fn a_live_previous_tab_keeps_its_screencast_record_until_replacement() {
        let backend = unavailable_backend();
        lock_or_recover(&backend.pane_by_tab).insert(4, "pane-1".to_string());
        lock_or_recover(&backend.pane_by_tab).insert(9, "pane-1".to_string());
        lock_or_recover(&backend.tab_by_pane).insert("pane-1".to_string(), 9);
        let previous = BrowserViewport {
            tab_id: 4,
            width: 800,
            height: 600,
            scale_factor: 2.0,
        };
        lock_or_recover(&backend.screencast_by_pane).insert("pane-1".to_string(), previous);

        scope_result_to_pane(
            &backend,
            Some("pane-1"),
            "getTabs",
            json!([
                { "id": 4, "url": "https://old.example", "title": "old", "active": false },
                { "id": 9, "url": "about:blank", "title": "", "active": true }
            ]),
        );

        assert_eq!(
            lock_or_recover(&backend.screencast_by_pane)
                .get("pane-1")
                .copied(),
            Some(previous)
        );
    }

    #[test]
    fn screencast_frames_are_requested_at_the_display_resolution() {
        let retina = BrowserViewport {
            tab_id: 1,
            width: 1280,
            height: 900,
            scale_factor: 2.0,
        };
        assert_eq!(screencast_max_dimensions(retina), (2560, 1800));
        assert_eq!(
            screencast_max_dimensions(BrowserViewport {
                scale_factor: 1.0,
                ..retina
            }),
            (1280, 900)
        );
    }

    #[test]
    fn agent_snapshot_scale_factor_is_capped_for_retina() {
        assert_eq!(browser_scale_factor(0.5), 1.0);
        assert_eq!(browser_scale_factor(1.5), 1.5);
        assert_eq!(browser_scale_factor(3.0), 2.0);
        assert_eq!(browser_scale_factor(f64::NAN), 1.0);
    }

    #[test]
    fn tab_lists_and_events_are_isolated_by_pane() {
        let backend = unavailable_backend();
        let tabs = json!([
            { "id": 1, "url": "about:blank", "title": "one", "active": true },
            { "id": 2, "url": "about:blank", "title": "two", "active": false }
        ]);
        let pane_a = scope_result_to_pane(&backend, Some("pane-a"), "getTabs", tabs.clone());
        let pane_b = scope_result_to_pane(&backend, Some("pane-b"), "getTabs", tabs);
        assert_eq!(pane_a.as_array().unwrap().len(), 1);
        assert_eq!(pane_a[0]["id"], 1);
        assert_eq!(pane_b.as_array().unwrap().len(), 1);
        assert_eq!(pane_b[0]["id"], 2);
        assert!(!tab_access_is_allowed(
            &backend,
            Some("pane-b"),
            &json!({ "params": { "tabId": 1 } })
        ));

        let event = json!({
            "jsonrpc": "2.0",
            "method": "onCDPEvent",
            "params": { "source": { "tabId": 1 }, "method": "Page.loadEventFired" }
        });
        assert!(event_is_visible_to_pane(&backend, Some("pane-a"), &event));
        assert!(!event_is_visible_to_pane(&backend, Some("pane-b"), &event));
    }

    fn write_rpc_request(stream: &mut UnixStream, id: u64, method: &str, params: Value) {
        write_frame(
            stream,
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params
            }),
        )
        .unwrap();
    }

    fn rpc_call(stream: &mut UnixStream, id: u64, method: &str, params: Value) -> Value {
        write_rpc_request(stream, id, method, params);
        loop {
            let response = read_frame(stream).unwrap().expect("browser socket closed");
            if response.get("id").and_then(Value::as_u64) == Some(id) {
                return response;
            }
        }
    }

    #[test]
    #[ignore = "launches an installed chrome-headless-shell"]
    fn codex_socket_automates_headless_shell_end_to_end() {
        let socket = start_browser_discovery(None).unwrap();
        let mut stream = UnixStream::connect(socket.path()).unwrap();
        let info = rpc_call(
            &mut stream,
            1,
            "getInfo",
            json!({ "session_id": "socket-test", "turn_id": "turn-1" }),
        );
        assert_eq!(
            info["result"]["metadata"]["automation"],
            "chrome-headless-shell-cdp"
        );
        assert!(
            info["result"]["metadata"]["chromeHeadlessShellExecutable"]
                .as_str()
                .and_then(|path| Path::new(path).file_name())
                .is_some_and(|name| name == "chrome-headless-shell" || name == "headless_shell")
        );

        let created = rpc_call(
            &mut stream,
            2,
            "createTab",
            json!({ "session_id": "socket-test", "turn_id": "turn-1" }),
        );
        let tab_id = created["result"]["id"].as_u64().unwrap();
        let attached = rpc_call(
            &mut stream,
            3,
            "attach",
            json!({
                "tabId": tab_id,
                "session_id": "socket-test",
                "turn_id": "turn-1"
            }),
        );
        assert!(attached.get("error").is_none());
        let evaluated = rpc_call(
            &mut stream,
            4,
            "executeCdp",
            json!({
                "target": { "tabId": tab_id },
                "method": "Runtime.evaluate",
                "commandParams": {
                    "expression": "21 * 2",
                    "returnByValue": true
                },
                "session_id": "socket-test",
                "turn_id": "turn-1"
            }),
        );
        assert_eq!(evaluated["result"]["result"]["value"], 42);
    }

    #[test]
    #[ignore = "launches an installed chrome-headless-shell"]
    fn codex_socket_services_paused_navigation_concurrently() {
        let socket = start_browser_discovery(None).unwrap();
        let mut stream = UnixStream::connect(socket.path()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let session = json!({ "session_id": "paused-navigation-test", "turn_id": "turn-1" });

        let created = rpc_call(&mut stream, 1, "createTab", session.clone());
        let tab_id = created["result"]["id"].as_u64().unwrap();
        let target = json!({ "tabId": tab_id });
        assert!(
            rpc_call(
                &mut stream,
                2,
                "attach",
                json!({
                    "tabId": tab_id,
                    "session_id": "paused-navigation-test",
                    "turn_id": "turn-1"
                }),
            )
            .get("error")
            .is_none()
        );
        for (id, method) in [(3, "Page.enable"), (4, "Fetch.enable")] {
            assert!(
                rpc_call(
                    &mut stream,
                    id,
                    "executeCdp",
                    json!({
                        "target": target,
                        "method": method,
                        "commandParams": {},
                        "session_id": "paused-navigation-test",
                        "turn_id": "turn-1"
                    }),
                )
                .get("error")
                .is_none()
            );
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = connection.read(&mut request).unwrap();
            connection
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
        });

        write_rpc_request(
            &mut stream,
            5,
            "executeCdp",
            json!({
                "target": target,
                "method": "Page.navigate",
                "commandParams": { "url": format!("http://{address}/") },
                "timeoutMs": 5_000,
                "session_id": "paused-navigation-test",
                "turn_id": "turn-1"
            }),
        );
        let request_id = loop {
            let event = read_frame(&mut stream)
                .unwrap()
                .expect("browser socket closed before Fetch.requestPaused");
            if event.pointer("/params/method").and_then(Value::as_str)
                == Some("Fetch.requestPaused")
            {
                break event
                    .pointer("/params/params/requestId")
                    .and_then(Value::as_str)
                    .unwrap()
                    .to_string();
            }
        };
        write_rpc_request(
            &mut stream,
            6,
            "executeCdp",
            json!({
                "target": target,
                "method": "Fetch.continueRequest",
                "commandParams": { "requestId": request_id },
                "timeoutMs": 5_000,
                "session_id": "paused-navigation-test",
                "turn_id": "turn-1"
            }),
        );

        let mut navigation_completed = false;
        let mut continue_completed = false;
        while !navigation_completed || !continue_completed {
            let response = read_frame(&mut stream)
                .unwrap()
                .expect("browser socket closed before navigation completed");
            match response.get("id").and_then(Value::as_u64) {
                Some(5) => {
                    assert!(response.get("error").is_none(), "{response}");
                    navigation_completed = true;
                }
                Some(6) => {
                    assert!(response.get("error").is_none(), "{response}");
                    continue_completed = true;
                }
                _ => {}
            }
        }
        server.join().unwrap();
    }

    /// Manual compatibility probe for the private Browser-plugin discovery
    /// protocol. Run this ignored test, then call agent.browsers.list() from a
    /// Codex turn within its wait window.
    #[test]
    #[ignore = "manual Codex Browser-plugin compatibility probe"]
    fn expose_discovery_socket_for_manual_codex_probe() {
        let socket = start_browser_discovery(None).unwrap();
        eprintln!("probe socket: {}", socket.path().display());
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
}
