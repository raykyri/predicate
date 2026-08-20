use crate::adapters::{
    AdapterNotification, PrepareShellAgentLaunchRequest, PrepareShellClaudeLaunchRequest,
    adapter_registry, agent_fork, agent_prepare_shell_launch, ingest_adapter_notification,
    notification_adapter_hint,
};
use crate::connection_limit::ConnectionLimiter;
use crate::events::QmuxEvent;
use crate::pty::{PaneWriteOptions, write_pane};
use crate::state::{AppState, canonical_loopback_artifact_url};
use crate::workspace::{
    LaunchOrigin, recover_shell_agent_from_session_start, validate_launch_workspace,
};
use qmux_proto::{ControlRequest, ControlResponse, PublicControlRequest};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const CONTROL_SOCKET_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Cap on concurrent client-handler threads. Connections are mostly one-shot
/// (hook notifies, CLI invocations) with a 5s idle timeout, so this needs to
/// cover simultaneous in-flight requests, not panes; 64 is far above any real
/// burst while keeping a connection-spamming local process from exhausting
/// threads/FDs. At the cap the supervisor leaves excess connections in the
/// kernel listen backlog and keeps polling so health checks still run.
const MAX_CONCURRENT_CLIENTS: usize = 64;
/// Backoff after a failed accept. Persistent accept errors (e.g. EMFILE under
/// FD exhaustion) would otherwise spin this loop hot.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);
#[cfg(not(test))]
const HEALTH_POLL_TIMEOUT: Duration = Duration::from_millis(500);
#[cfg(test)]
const HEALTH_POLL_TIMEOUT: Duration = Duration::from_millis(50);
#[cfg(not(test))]
const RECOVERY_BACKOFF: Duration = Duration::from_millis(100);
#[cfg(test)]
const RECOVERY_BACKOFF: Duration = Duration::from_millis(20);
const RECOVERY_BACKOFF_MAX: Duration = Duration::from_secs(2);
const REPEATED_RECOVERY_FAILURES_BEFORE_WARN: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SocketWatchState {
    Healthy,
    Missing,
    Recovering,
    Conflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathCheck {
    Healthy,
    Missing,
    Conflict,
    Unknown,
}

enum GenerationEnd {
    Stopped,
    Missing,
    Conflict,
}

struct SupervisorShared {
    stop: AtomicBool,
    generation: AtomicU64,
    #[cfg(test)]
    panic_next: AtomicBool,
    #[cfg(test)]
    transitions: Mutex<Vec<String>>,
}

/// Owns the control-socket supervisor thread. Shutdown must stop and join this
/// before removing the socket file; otherwise the watchdog can recreate the
/// path while the process is exiting.
pub struct ControlSocketRuntime {
    shared: Arc<SupervisorShared>,
    wakeup: Mutex<Option<UnixStream>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    state: AppState,
}

impl ControlSocketRuntime {
    /// Stop the supervisor, join it, then remove the pathname only if this
    /// process still owns the inode currently at the configured path.
    pub fn shutdown(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Ok(guard) = self.wakeup.lock()
            && let Some(wakeup) = guard.as_ref()
        {
            let _ = (&*wakeup).write_all(&[1]);
        }
        if let Ok(mut slot) = self.thread.lock()
            && let Some(handle) = slot.take()
            && handle.thread().id() != thread::current().id()
        {
            let _ = handle.join();
        }
        if self.state.owns_control_socket() {
            let _ = fs::remove_file(&self.state.config().socket_path);
        }
        self.state.clear_control_socket_identity();
    }

    #[cfg(test)]
    fn inject_panic(&self) {
        self.shared.panic_next.store(true, Ordering::SeqCst);
        if let Ok(guard) = self.wakeup.lock()
            && let Some(wakeup) = guard.as_ref()
        {
            let _ = (&*wakeup).write_all(&[1]);
        }
    }

    #[cfg(test)]
    fn transitions(&self) -> Vec<String> {
        self.shared
            .transitions
            .lock()
            .map(|events| events.clone())
            .unwrap_or_default()
    }
}

impl Drop for ControlSocketRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub fn start_control_socket(state: AppState) -> Result<ControlSocketRuntime, String> {
    crate::shell_jobs::start_shell_job_monitor(state.clone());
    start_control_socket_runtime(state, MAX_CONCURRENT_CLIENTS)
}

fn start_control_socket_runtime(
    state: AppState,
    max_clients: usize,
) -> Result<ControlSocketRuntime, String> {
    let socket_path = state.config().socket_path.clone();
    remove_stale_socket(&socket_path)?;
    let (listener, identity) = bind_control_socket(&socket_path)?;
    state.set_control_socket_identity(identity.0, identity.1);

    let (wakeup_reader, wakeup_writer) = UnixStream::pair()
        .map_err(|err| format!("failed to create control socket wakeup pipe: {err}"))?;
    wakeup_reader
        .set_nonblocking(true)
        .map_err(|err| format!("failed to configure control socket wakeup pipe: {err}"))?;
    wakeup_writer
        .set_nonblocking(true)
        .map_err(|err| format!("failed to configure control socket wakeup pipe: {err}"))?;

    let shared = Arc::new(SupervisorShared {
        stop: AtomicBool::new(false),
        generation: AtomicU64::new(1),
        #[cfg(test)]
        panic_next: AtomicBool::new(false),
        #[cfg(test)]
        transitions: Mutex::new(Vec::new()),
    });
    let thread_shared = Arc::clone(&shared);
    let thread_state = state.clone();
    let thread_path = socket_path.clone();
    let handle = thread::Builder::new()
        .name("qmux-control-socket".to_string())
        .spawn(move || {
            supervise_control_socket(
                thread_state,
                thread_path,
                listener,
                identity,
                wakeup_reader,
                thread_shared,
                max_clients,
            );
        })
        .map_err(|err| {
            if state.owns_control_socket() {
                let _ = fs::remove_file(&socket_path);
            }
            state.clear_control_socket_identity();
            format!("failed to start control socket supervisor: {err}")
        })?;

    Ok(ControlSocketRuntime {
        shared,
        wakeup: Mutex::new(Some(wakeup_writer)),
        thread: Mutex::new(Some(handle)),
        state,
    })
}

fn remove_stale_socket(socket_path: &Path) -> Result<(), String> {
    // Remove any stale socket unconditionally; a missing path is not an error.
    // Probing with exists() first would open a time-of-check/time-of-use window.
    match fs::remove_file(socket_path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!(
            "failed to remove stale socket {}: {err}",
            socket_path.display()
        )),
    }
}

fn bind_control_socket(socket_path: &Path) -> Result<(UnixListener, (u64, u64)), String> {
    // Restrict the socket's parent directory to the owning user *before* binding.
    // With the directory untraversable by other accounts, the socket is never
    // reachable by them even during the brief window between bind() and the
    // explicit chmod below, and no other user can pre-create the socket path.
    // Config sets this best-effort at startup; enforce it strictly here so we
    // fail loudly rather than expose the control plane on a world-traversable dir.
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create socket dir {}: {err}", parent.display()))?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|err| format!("failed to restrict socket dir {}: {err}", parent.display()))?;
    }

    let listener = UnixListener::bind(socket_path)
        .map_err(|err| format!("failed to bind socket {}: {err}", socket_path.display()))?;

    // Restrict the socket to the owning user so the per-pane token is not the only thing
    // standing between other local accounts and the control plane.
    if let Err(err) = fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600)) {
        drop(listener);
        let _ = fs::remove_file(socket_path);
        return Err(format!(
            "failed to restrict socket permissions {}: {err}",
            socket_path.display()
        ));
    }

    if let Err(err) = listener.set_nonblocking(true) {
        drop(listener);
        let _ = fs::remove_file(socket_path);
        return Err(format!(
            "failed to configure socket {} as nonblocking: {err}",
            socket_path.display()
        ));
    }

    let meta = match fs::symlink_metadata(socket_path) {
        Ok(meta) => meta,
        Err(err) => {
            drop(listener);
            let _ = fs::remove_file(socket_path);
            return Err(format!(
                "failed to stat bound socket {}: {err}",
                socket_path.display()
            ));
        }
    };
    Ok((listener, (meta.dev(), meta.ino())))
}

fn supervise_control_socket(
    state: AppState,
    socket_path: PathBuf,
    listener: UnixListener,
    mut identity: (u64, u64),
    wakeup: UnixStream,
    shared: Arc<SupervisorShared>,
    max_clients: usize,
) {
    let limiter = ConnectionLimiter::new(max_clients);
    let mut watch_state = SocketWatchState::Healthy;
    let mut recovery_failures = 0;
    let mut listener = Some(listener);

    loop {
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        let Some(current) = listener.take() else {
            match recover_missing_socket(
                &state,
                &socket_path,
                &shared,
                &mut watch_state,
                &mut recovery_failures,
            ) {
                Recovered::Stopped => break,
                Recovered::Bound(next_listener, next_identity) => {
                    listener = Some(next_listener);
                    identity = next_identity;
                }
                Recovered::Conflict => {
                    if wait_for_conflict_to_clear(
                        &state,
                        &socket_path,
                        &wakeup,
                        &shared,
                        &mut watch_state,
                    )
                    .is_none()
                    {
                        break;
                    }
                }
            }
            continue;
        };

        let outcome = catch_unwind(AssertUnwindSafe(|| {
            run_listener_generation(
                &state,
                &socket_path,
                &current,
                identity,
                &wakeup,
                &shared,
                &limiter,
                &mut watch_state,
            )
        }));
        drop(current);

        match outcome {
            Ok(GenerationEnd::Stopped) => break,
            Ok(GenerationEnd::Missing) => {}
            Ok(GenerationEnd::Conflict) => {
                state.clear_control_socket_identity();
                if wait_for_conflict_to_clear(
                    &state,
                    &socket_path,
                    &wakeup,
                    &shared,
                    &mut watch_state,
                )
                .is_none()
                {
                    break;
                }
            }
            Err(_) => {
                reclaim_owned_socket(&state, &socket_path);
                enter_watch_state(
                    &state,
                    &socket_path,
                    &shared,
                    &mut watch_state,
                    SocketWatchState::Recovering,
                    Some("control socket listener panicked"),
                );
            }
        }
    }
}

enum Recovered {
    Stopped,
    Bound(UnixListener, (u64, u64)),
    Conflict,
}

fn recover_missing_socket(
    state: &AppState,
    socket_path: &Path,
    shared: &SupervisorShared,
    watch_state: &mut SocketWatchState,
    recovery_failures: &mut u32,
) -> Recovered {
    let mut backoff = RECOVERY_BACKOFF;
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return Recovered::Stopped;
        }
        match inspect_socket_path(socket_path, state.control_socket_identity()) {
            PathCheck::Conflict => {
                enter_watch_state(
                    state,
                    socket_path,
                    shared,
                    watch_state,
                    SocketWatchState::Conflict,
                    Some("control socket path is occupied by another listener"),
                );
                state.clear_control_socket_identity();
                return Recovered::Conflict;
            }
            PathCheck::Healthy | PathCheck::Missing | PathCheck::Unknown => {}
        }

        match bind_control_socket(socket_path) {
            Ok((listener, identity)) => {
                state.set_control_socket_identity(identity.0, identity.1);
                shared.generation.fetch_add(1, Ordering::SeqCst);
                *recovery_failures = 0;
                enter_watch_state(
                    state,
                    socket_path,
                    shared,
                    watch_state,
                    SocketWatchState::Healthy,
                    None,
                );
                return Recovered::Bound(listener, identity);
            }
            Err(err) => {
                if matches!(inspect_socket_path(socket_path, None), PathCheck::Conflict) {
                    enter_watch_state(
                        state,
                        socket_path,
                        shared,
                        watch_state,
                        SocketWatchState::Conflict,
                        Some(&err),
                    );
                    state.clear_control_socket_identity();
                    return Recovered::Conflict;
                }
                *recovery_failures = recovery_failures.saturating_add(1);
                enter_watch_state(
                    state,
                    socket_path,
                    shared,
                    watch_state,
                    SocketWatchState::Recovering,
                    Some(&err),
                );
                if *recovery_failures == REPEATED_RECOVERY_FAILURES_BEFORE_WARN {
                    warn_control_socket(
                        state,
                        &format!(
                            "qmux could not restore the control socket at {}: {err}. CLI commands will fail until it recovers.",
                            socket_path.display()
                        ),
                    );
                }
                if shared.stop.load(Ordering::SeqCst) {
                    return Recovered::Stopped;
                }
                thread::sleep(backoff);
                backoff = (backoff * 2).min(RECOVERY_BACKOFF_MAX);
            }
        }
    }
}

fn wait_for_conflict_to_clear(
    state: &AppState,
    socket_path: &Path,
    wakeup: &UnixStream,
    shared: &SupervisorShared,
    watch_state: &mut SocketWatchState,
) -> Option<()> {
    enter_watch_state(
        state,
        socket_path,
        shared,
        watch_state,
        SocketWatchState::Conflict,
        Some("control socket path is occupied by another listener"),
    );
    warn_control_socket(
        state,
        &format!(
            "The qmux control socket at {} was replaced by another process. CLI commands will not reach this instance until that socket is removed. qmux will not delete it automatically.",
            socket_path.display()
        ),
    );
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return None;
        }
        match inspect_socket_path(socket_path, None) {
            PathCheck::Missing => {
                enter_watch_state(
                    state,
                    socket_path,
                    shared,
                    watch_state,
                    SocketWatchState::Missing,
                    None,
                );
                return Some(());
            }
            PathCheck::Healthy | PathCheck::Conflict | PathCheck::Unknown => {}
        }
        if poll_wakeup_only(wakeup, HEALTH_POLL_TIMEOUT).is_err()
            && shared.stop.load(Ordering::SeqCst)
        {
            return None;
        }
        drain_wakeup(wakeup);
    }
}

fn run_listener_generation(
    state: &AppState,
    socket_path: &Path,
    listener: &UnixListener,
    identity: (u64, u64),
    wakeup: &UnixStream,
    shared: &SupervisorShared,
    limiter: &ConnectionLimiter,
    watch_state: &mut SocketWatchState,
) -> GenerationEnd {
    let mut accept_error: Option<String> = None;
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return GenerationEnd::Stopped;
        }
        #[cfg(test)]
        if shared.panic_next.swap(false, Ordering::SeqCst) {
            panic!("test-injected control socket panic");
        }

        match poll_listener_and_wakeup(listener, wakeup, HEALTH_POLL_TIMEOUT) {
            Ok(ready) => {
                if ready.wakeup {
                    drain_wakeup(wakeup);
                    if shared.stop.load(Ordering::SeqCst) {
                        return GenerationEnd::Stopped;
                    }
                }
                if ready.listener {
                    match limiter.try_acquire() {
                        None => thread::sleep(ACCEPT_ERROR_BACKOFF),
                        Some(slot) => match listener.accept() {
                            Ok((stream, _)) => {
                                accept_error = None;
                                if let Err(err) = stream.set_nonblocking(false) {
                                    eprintln!(
                                        "qmux: failed to configure control socket client as blocking: {err}"
                                    );
                                    continue;
                                }
                                let state = state.clone();
                                let _ = thread::Builder::new()
                                    .name("qmux-control-client".to_string())
                                    .spawn(move || {
                                        let _slot = slot;
                                        handle_client(state, stream);
                                    });
                            }
                            Err(err) if err.kind() == ErrorKind::WouldBlock => {}
                            Err(err) if err.kind() == ErrorKind::Interrupted => {}
                            Err(err) => {
                                let message = err.to_string();
                                if accept_error.as_deref() != Some(message.as_str()) {
                                    accept_error = Some(message.clone());
                                    enter_watch_state(
                                        state,
                                        socket_path,
                                        shared,
                                        watch_state,
                                        SocketWatchState::Recovering,
                                        Some(&message),
                                    );
                                }
                                thread::sleep(ACCEPT_ERROR_BACKOFF);
                            }
                        },
                    }
                }
            }
            Err(err) => {
                let message = err.to_string();
                enter_watch_state(
                    state,
                    socket_path,
                    shared,
                    watch_state,
                    SocketWatchState::Recovering,
                    Some(&message),
                );
                thread::sleep(ACCEPT_ERROR_BACKOFF);
            }
        }

        match inspect_socket_path(socket_path, Some(identity)) {
            PathCheck::Healthy => {
                if *watch_state != SocketWatchState::Healthy {
                    enter_watch_state(
                        state,
                        socket_path,
                        shared,
                        watch_state,
                        SocketWatchState::Healthy,
                        None,
                    );
                }
            }
            PathCheck::Missing => {
                enter_watch_state(
                    state,
                    socket_path,
                    shared,
                    watch_state,
                    SocketWatchState::Missing,
                    None,
                );
                return GenerationEnd::Missing;
            }
            PathCheck::Conflict => {
                enter_watch_state(
                    state,
                    socket_path,
                    shared,
                    watch_state,
                    SocketWatchState::Conflict,
                    Some("control socket path is occupied by another listener"),
                );
                return GenerationEnd::Conflict;
            }
            PathCheck::Unknown => {}
        }
    }
}

fn inspect_socket_path(path: &Path, owned: Option<(u64, u64)>) -> PathCheck {
    match fs::symlink_metadata(path) {
        Ok(meta) => match owned {
            Some((device, inode)) if meta.dev() == device && meta.ino() == inode => {
                PathCheck::Healthy
            }
            Some(_) => PathCheck::Conflict,
            None => PathCheck::Conflict,
        },
        Err(err) if err.kind() == ErrorKind::NotFound => PathCheck::Missing,
        Err(_) => PathCheck::Unknown,
    }
}

fn reclaim_owned_socket(state: &AppState, socket_path: &Path) {
    if state.owns_control_socket() {
        let _ = fs::remove_file(socket_path);
    }
    state.clear_control_socket_identity();
}

fn enter_watch_state(
    state: &AppState,
    socket_path: &Path,
    shared: &SupervisorShared,
    current: &mut SocketWatchState,
    next: SocketWatchState,
    error: Option<&str>,
) {
    if *current == next {
        return;
    }
    *current = next;
    let event = match next {
        SocketWatchState::Healthy => "control_socket.recovered",
        SocketWatchState::Missing => "control_socket.missing",
        SocketWatchState::Recovering => "control_socket.error",
        SocketWatchState::Conflict => "control_socket.conflict",
    };
    if next == SocketWatchState::Healthy {
        eprintln!(
            "qmux: control socket recovered at {}",
            socket_path.display()
        );
    } else if let Some(error) = error {
        eprintln!("qmux: {event}: {error}");
    } else {
        eprintln!("qmux: {event} at {}", socket_path.display());
    }
    emit_watch_event(
        state,
        event,
        socket_path,
        shared.generation.load(Ordering::SeqCst),
        error,
    );
    #[cfg(test)]
    if let Ok(mut events) = shared.transitions.lock() {
        events.push(event.to_string());
    }
}

fn emit_watch_event(
    state: &AppState,
    event: &str,
    socket_path: &Path,
    generation: u64,
    error: Option<&str>,
) {
    let identity = |pair: Option<(u64, u64)>| {
        pair.map(|(device, inode)| json!({ "device": device, "inode": inode }))
    };
    state.emit(QmuxEvent::new(
        event,
        None,
        None,
        json!({
            "path": socket_path.display().to_string(),
            "generation": generation,
            "currentInode": identity(state.control_socket_identity()),
            "error": error,
        }),
    ));
}

fn warn_control_socket(state: &AppState, message: &str) {
    eprintln!("qmux: {message}");
    if let Some(app) = state.app_handle() {
        use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
        app.dialog()
            .message(message)
            .title("qmux")
            .kind(MessageDialogKind::Warning)
            .show(|_| {});
    }
}

struct PollReady {
    listener: bool,
    wakeup: bool,
}

fn poll_listener_and_wakeup(
    listener: &UnixListener,
    wakeup: &UnixStream,
    timeout: Duration,
) -> std::io::Result<PollReady> {
    let mut fds = [
        libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wakeup.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    poll_fds(&mut fds, timeout)?;
    Ok(PollReady {
        listener: fds[0].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0,
        wakeup: fds[1].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0,
    })
}

fn poll_wakeup_only(wakeup: &UnixStream, timeout: Duration) -> std::io::Result<()> {
    let mut fds = [libc::pollfd {
        fd: wakeup.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }];
    poll_fds(&mut fds, timeout).map(|_| ())
}

fn poll_fds(fds: &mut [libc::pollfd], timeout: Duration) -> std::io::Result<bool> {
    let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    loop {
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        return Ok(n > 0);
    }
}

fn drain_wakeup(wakeup: &UnixStream) {
    let mut buf = [0_u8; 32];
    loop {
        match (&*wakeup).read(&mut buf) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {
                break;
            }
            Err(_) => break,
        }
    }
}

fn handle_client(state: AppState, stream: UnixStream) {
    handle_client_with_timeout(state, stream, CONTROL_SOCKET_READ_TIMEOUT);
}

fn handle_client_with_timeout(state: AppState, mut stream: UnixStream, read_timeout: Duration) {
    let reader_stream = match stream.try_clone() {
        Ok(stream) => stream,
        Err(err) => {
            let _ = write_response(&mut stream, Err(format!("failed to clone socket: {err}")));
            return;
        }
    };
    if let Err(err) = reader_stream.set_read_timeout(Some(read_timeout)) {
        let _ = write_response(
            &mut stream,
            Err(format!("failed to set socket read timeout: {err}")),
        );
        return;
    }
    let reader = BufReader::new(reader_stream);

    for line in reader.lines() {
        let result = match line {
            Ok(line) => handle_line(&state, &line),
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return;
            }
            Err(err) => Err(format!("failed to read socket request: {err}")),
        };

        if write_response(&mut stream, result).is_err() {
            return;
        }
    }
}

fn handle_line(state: &AppState, line: &str) -> Result<Value, String> {
    let request: ControlRequest =
        serde_json::from_str(line).map_err(|err| format!("invalid control request: {err}"))?;
    // A control token authorizes exactly one pane. Resolving it here means every
    // command below acts only on the caller's own pane: a process in one pane cannot
    // write to, or impersonate hooks for, any other pane.
    let pane_auth = state.pane_for_token(&request.token);
    let user_auth = state.pane_for_user_token(&request.token);
    let (authed_pane, user_credential) = match (pane_auth, user_auth) {
        (Some(pane), _) => (pane, false),
        (None, Some(pane)) => (pane, true),
        (None, None) => return Err("invalid QMUX_TOKEN".to_string()),
    };

    if user_credential && request.command != "cli.call" {
        return Err("QMUX_USER_TOKEN is valid only for public CLI operations".to_string());
    }

    match request.command.as_str() {
        "ping" => Ok(json!({ "status": "ok" })),
        "cli.call" => {
            let payload = serde_json::from_value::<PublicControlRequest>(request.payload)
                .map_err(|err| format!("invalid cli.call payload: {err}"))?;
            Ok(crate::control::handle_call(
                state,
                &authed_pane,
                user_credential,
                &payload.operation,
                payload.arguments,
            ))
        }
        "pane.write" => {
            let options = serde_json::from_value::<PaneWriteOptions>(request.payload)
                .map_err(|err| format!("invalid pane.write payload: {err}"))?;
            ensure_pane_scope(&authed_pane, &options.pane_id)?;
            write_pane(state, options)?;
            Ok(json!({ "written": true }))
        }
        "pane.set_cwd" => {
            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct SetCwdPayload {
                cwd: String,
            }
            let payload = serde_json::from_value::<SetCwdPayload>(request.payload)
                .map_err(|err| format!("invalid pane.set_cwd payload: {err}"))?;
            // Bind the cwd update to the authenticated pane regardless of any claimed
            // paneId, mirroring hook.notify's scoping.
            state.update_pane_cwd(&authed_pane, payload.cwd)?;
            Ok(json!({ "updated": true }))
        }
        "agent.prepare_shell_launch" => {
            let launch = serde_json::from_value::<PrepareShellAgentLaunchRequest>(request.payload)
                .map_err(|err| format!("invalid agent.prepare_shell_launch payload: {err}"))?;
            ensure_pane_scope(&authed_pane, &launch.pane_id)?;
            validate_control_launch_workspace(state, &authed_pane)?;
            let prepared = agent_prepare_shell_launch(state, launch)?;
            serde_json::to_value(prepared)
                .map_err(|err| format!("failed to encode prepared agent launch: {err}"))
        }
        "agent.detach_pane" => {
            // A shell-launched agent's process has exited while its host shell — and so
            // this pane — lives on. Detach the agent bound to the authenticated pane so
            // the tab reverts to a plain shell instead of lingering with a stale agent
            // status. Scoped to the authed pane like pane.set_cwd; any claimed paneId is
            // advisory, so the wrapper can only ever detach its own pane's agent.
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct DetachShellAgentPayload {
                job_id: Option<String>,
                agent_id: Option<String>,
            }
            let payload = serde_json::from_value::<DetachShellAgentPayload>(request.payload)
                .map_err(|err| format!("invalid agent detach payload: {err}"))?;
            let detached = match (payload.job_id.as_deref(), payload.agent_id.as_deref()) {
                (Some(job_id), Some(agent_id)) => {
                    let Some(info) = state.unregister_shell_agent_job(
                        job_id,
                        Some(agent_id),
                        Some(&authed_pane),
                    ) else {
                        return Ok(json!({ "detached": false }));
                    };
                    crate::shell_jobs::emit_job_removed(state, &info);
                    crate::workspace::detach_pane_agent_if_matches(state, &authed_pane, agent_id)?
                }
                // Compatibility for a shell wrapper launched by an older qmux build.
                _ => crate::workspace::detach_pane_agent(state, &authed_pane)?,
            };
            // The exited agent may have left its TUI's terminal modes active in
            // the surviving shell's surface (kitty keyboard flags, mouse/focus
            // reporting, the alternate screen) — this detach is the only moment
            // the host learns the foreground program is gone, so clear them
            // here. Best-effort: the detach itself already succeeded.
            if detached.is_some()
                && let Err(err) = crate::pty::reset_pane_terminal_modes(state, &authed_pane)
            {
                eprintln!("qmux: failed to reset terminal modes for pane {authed_pane}: {err}");
            }
            Ok(json!({ "detached": detached.is_some() }))
        }
        "claude.prepare_shell_launch" => {
            let launch = serde_json::from_value::<PrepareShellClaudeLaunchRequest>(request.payload)
                .map_err(|err| format!("invalid claude.prepare_shell_launch payload: {err}"))?;
            ensure_pane_scope(&authed_pane, &launch.pane_id)?;
            validate_control_launch_workspace(state, &authed_pane)?;
            let prepared = agent_prepare_shell_launch(
                state,
                PrepareShellAgentLaunchRequest {
                    adapter_id: "claude".to_string(),
                    pane_id: launch.pane_id,
                    cwd: launch.cwd,
                    args: launch.args,
                    shell_job_id: None,
                    supervisor_pid: None,
                    prepared_agent_id: None,
                },
            )?;
            let settings_path = prepared
                .args
                .windows(2)
                .find_map(|args| (args[0] == "--settings").then(|| args[1].clone()))
                .ok_or_else(|| "prepared Claude launch did not include --settings".to_string())?;
            Ok(json!({
                "claudeBinary": prepared.binary,
                "cwd": prepared.cwd,
                "settingsPath": settings_path,
                "envs": prepared.envs,
            }))
        }
        "hook.notify" => {
            let mut notification = serde_json::from_value::<AdapterNotification>(request.payload)
                .map_err(|err| format!("invalid hook.notify payload: {err}"))?;
            // Bind the notification to the authenticated pane regardless of what the
            // caller claims, so hook status can only be reported for its own pane.
            notification.pane_id = Some(authed_pane.clone());

            if matches!(notification.event.as_str(), "SessionStart" | "sessionStart")
                && state.agent_by_pane(&authed_pane)?.is_none()
            {
                let adapter_id =
                    notification_adapter_hint(state, &notification)?.ok_or_else(|| {
                        "SessionStart cannot recover a missing agent without an adapter id"
                            .to_string()
                    })?;
                // Validate before creating durable state; an authenticated pane may
                // recover only one of qmux's configured agent adapters.
                adapter_registry(state.config()).get(&adapter_id)?;
                let pane = state
                    .list_panes()?
                    .into_iter()
                    .find(|pane| pane.id == authed_pane)
                    .ok_or_else(|| format!("pane {authed_pane} was not found"))?;
                let recovered = recover_shell_agent_from_session_start(
                    state,
                    &pane,
                    &adapter_id,
                    notification.agent_id.as_deref(),
                )?;
                notification.agent_id = Some(recovered.id);
            }

            if let Some(agent_id) = notification.agent_id.as_deref() {
                if state.agent(agent_id)?.is_some() {
                    ensure_agent_scope(state, &authed_pane, agent_id)?;
                } else if let Some(bound) = state.agent_by_pane(&authed_pane)? {
                    // A recovered record may have a new qmux id while the already
                    // running process keeps reporting the stale/missing prepared id.
                    // The pane token is the authority boundary, so route that unknown
                    // id to the agent now bound to the same authenticated pane.
                    notification.agent_id = Some(bound.id);
                } else {
                    return Err(format!("agent {agent_id} was not found"));
                }
            }
            // Older hook shims may omit agentId and rely on their authenticated pane.
            // Resolve the same fallback the adapter uses so those notifications can
            // still complete a queued fork barrier.
            let notified_agent_id = match notification.agent_id.clone() {
                Some(agent_id) => Some(agent_id),
                None => state.agent_by_pane(&authed_pane)?.map(|agent| agent.id),
            };
            let outcome = ingest_adapter_notification(state, notification)?;
            for event in outcome.into_events() {
                state.emit(event);
            }
            // Adapter ingestion records session identity and UserPromptSubmit activity
            // before returning. Re-check every hook so either arrival order (identity
            // first or prompt first) can release a queued-fork barrier without a lost
            // wakeup. A resume failure must not make the agent's best-effort hook call
            // fail; surface it through the queue event/log path instead.
            if let Some(agent_id) = notified_agent_id
                && let Err(err) =
                    crate::turn_queue::release_ready_fork_barrier_for_child(state, &agent_id)
            {
                eprintln!(
                    "qmux: failed to release fork barrier after hook from agent {agent_id}: {err}"
                );
            }
            Ok(json!({ "notified": true }))
        }
        "transcript.append" => {
            #[derive(Debug, Deserialize)]
            struct AppendPayload {
                #[serde(default)]
                lines: Vec<String>,
            }
            let payload = serde_json::from_value::<AppendPayload>(request.payload)
                .map_err(|err| format!("invalid transcript.append payload: {err}"))?;

            // The destination is qmux's own record for the agent bound to the
            // authenticated pane. The caller never names a path, so a forged
            // request cannot aim writes at another agent's transcript — or
            // anywhere else on disk.
            let agent = state
                .agent_by_pane(&authed_pane)?
                .ok_or_else(|| format!("no agent is bound to pane {authed_pane}"))?;
            let path = agent
                .transcript_path
                .clone()
                .ok_or_else(|| format!("agent {} has no transcript to append to", agent.id))?;
            let appended =
                crate::transcript::append_transcript_lines(Path::new(&path), &payload.lines)?;

            // The tail may not be running yet on a recovered pane, and starting
            // it is what turns these lines into turns. It is idempotent, but it
            // also rebuilds the adapter registry to validate, so only pay for
            // that when a line actually landed.
            if appended > 0 {
                crate::transcript::start_transcript_tail(
                    state.clone(),
                    agent.id.clone(),
                    path,
                    agent.adapter.clone(),
                );
            }
            Ok(json!({ "appended": appended }))
        }
        "agent.fork" => {
            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct ForkPayload {
                #[serde(default)]
                use_worktree: bool,
                #[serde(default)]
                prompt: Option<String>,
            }
            let payload = serde_json::from_value::<ForkPayload>(request.payload)
                .map_err(|err| format!("invalid agent.fork payload: {err}"))?;
            validate_control_launch_workspace(state, &authed_pane)?;
            // The one spawn the control plane allows: it forks ONLY the authenticated
            // pane's own session (the source is resolved from the token, not the
            // payload), so a token can never spawn off another pane's session. This is
            // the same authority the user already has acting in their own terminal.
            // Always forks at the session head: anchoring at a message is a UI
            // action, and the payload carries no anchor to honour. Keeping it
            // that way means the control plane cannot ask for a synthesized
            // transcript, so this path never writes into an agent's own state
            // directory.
            let pane = agent_fork(
                state,
                &authed_pane,
                payload.use_worktree,
                payload.prompt,
                None,
            )?;
            serde_json::to_value(pane).map_err(|err| format!("failed to encode forked pane: {err}"))
        }
        "mcp.call" => {
            #[derive(Debug, Deserialize)]
            struct McpCallPayload {
                name: String,
                #[serde(default)]
                arguments: Value,
            }
            let payload = serde_json::from_value::<McpCallPayload>(request.payload)
                .map_err(|err| format!("invalid mcp.call payload: {err}"))?;
            crate::mcp::handle_call(state, &authed_pane, &payload.name, payload.arguments)
        }
        "browser.open" => {
            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct BrowserOpen {
                target: String,
                #[serde(default)]
                cwd: Option<String>,
            }
            let payload = serde_json::from_value::<BrowserOpen>(request.payload)
                .map_err(|err| format!("invalid browser.open payload: {err}"))?;
            let resolved = resolve_browser_target(
                state,
                &authed_pane,
                payload.target.trim(),
                payload.cwd.as_deref(),
            )?;
            state.emit(QmuxEvent::new(
                "browser.open",
                Some(authed_pane.clone()),
                None,
                json!({ "url": resolved.url, "sandbox": resolved.sandbox }),
            ));
            // Panes with an attached agent also collect the target into the
            // workspace artifact tray. This deliberately covers both callers a
            // pane token can represent: the agent itself, and the user typing
            // `qmux open` while that agent is backgrounded — the PTY offers no
            // way to tell them apart, and both belong in the tray. Best-effort:
            // a tray failure must not fail the open.
            if state.agent_by_pane(&authed_pane)?.is_some() {
                let path = resolved
                    .path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned());
                let url = path.is_none().then(|| resolved.url.clone());
                if let Err(err) = state.record_artifact(&authed_pane, path, url) {
                    eprintln!("failed to record artifact for pane {authed_pane}: {err}");
                }
            }
            Ok(json!({ "url": resolved.url, "sandbox": resolved.sandbox }))
        }
        // Other agent spawning and turn queueing are management operations that belong
        // to the trusted GUI (Tauri commands), not to processes holding a pane token.
        other => Err(format!("unknown control command '{other}'")),
    }
}

fn validate_control_launch_workspace(state: &AppState, pane_id: &str) -> Result<(), String> {
    let group_id = state
        .pane_group_id(pane_id)?
        .ok_or_else(|| format!("pane {pane_id} has no workspace"))?;
    validate_launch_workspace(state, Some(&group_id), LaunchOrigin::Terminal)?;
    Ok(())
}

/// A browser-overlay target resolved for one pane. `path` carries the canonical
/// filesystem path for file targets — the artifact tray persists that instead of
/// `url`, whose file-server token goes stale across runs — and is None for
/// loopback http(s) URLs.
#[derive(Debug)]
pub(crate) struct ResolvedBrowserTarget {
    pub url: String,
    pub sandbox: bool,
    pub path: Option<std::path::PathBuf>,
}

/// Resolve a browser-overlay target for `pane_id`: either a loopback http(s)
/// URL (returned as-is, unsandboxed) or a path under the pane's file roots
/// (minted into a token-bearing file-server URL and sandboxed). Shared by the
/// control-socket `browser.open` path and the trusted GUI command that opens
/// local file links from transcript markdown.
pub(crate) fn resolve_browser_target(
    state: &AppState,
    authed_pane: &str,
    target: &str,
    cwd: Option<&str>,
) -> Result<ResolvedBrowserTarget, String> {
    if target.is_empty() {
        return Err("nothing to open".to_string());
    }
    if target.starts_with("http://") || target.starts_with("https://") {
        let url = canonical_loopback_artifact_url(target).ok_or_else(|| {
            format!(
                "refusing to open '{target}': the browser overlay only loads http(s) URLs on localhost/127.0.0.1"
            )
        })?;
        return Ok(ResolvedBrowserTarget {
            url,
            sandbox: false,
            path: None,
        });
    }

    let requested = {
        let path = std::path::Path::new(target);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            let base = cwd.ok_or_else(|| {
                "cannot resolve a relative path without a working directory".to_string()
            })?;
            std::path::Path::new(base).join(path)
        }
    };

    let roots = state.pane_file_roots(authed_pane);
    // Transcript and research-document links commonly carry a grep-style
    // `path:line` or `path:line:col` suffix (e.g. `src/session.rs:8`). The
    // literal path is tried first so a file actually named that way still
    // wins; only when it does not resolve is the suffix dropped, and the line
    // number rides along as an `#L<n>` fragment — the file server renders
    // source files with `L<n>` row anchors, so the overlay scrolls to and
    // highlights that line.
    let (canonical, target_line) = match crate::file_server::resolve_under_roots(&requested, &roots)
    {
        Some(canonical) => (canonical, None),
        None => strip_line_column_suffix(&requested)
            .and_then(|(stripped, line)| {
                crate::file_server::resolve_under_roots(&stripped, &roots)
                    .map(|canonical| (canonical, Some(line)))
            })
            .ok_or_else(|| format!("'{target}' was not found"))?,
    };
    let port = state
        .file_server_port()
        .ok_or_else(|| "the file server is not running".to_string())?;
    let token = state.pane_file_token(authed_pane)?;
    let mut url = crate::file_server::file_url(port, &token, &canonical);
    if let Some(line) = target_line {
        url.push_str(&format!("#L{line}"));
    }
    Ok(ResolvedBrowserTarget {
        url,
        sandbox: true,
        path: Some(canonical),
    })
}

/// `path:line` or `path:line:col` with all-digit trailing segments → the bare
/// path and the line number. None when the path carries no such suffix (so
/// callers can tell "nothing to strip" from a stripped candidate). Only the
/// final component is inspected — rsplitting on ':' never reaches into parent
/// directories because paths containing '/' after the colon fail the
/// all-digits check.
fn strip_line_column_suffix(path: &std::path::Path) -> Option<(std::path::PathBuf, u64)> {
    let original = path.to_str()?;
    let mut remaining = original;
    let mut line = None;
    // Strip at most two segments, column first: the last one stripped is the line.
    for _ in 0..2 {
        match remaining.rsplit_once(':') {
            Some((head, tail))
                if !head.is_empty()
                    && !tail.is_empty()
                    && tail.bytes().all(|byte| byte.is_ascii_digit()) =>
            {
                // An unparseable digit run (u64 overflow) is not a real
                // location; leave it as part of the name.
                let Ok(parsed) = tail.parse::<u64>() else {
                    break;
                };
                line = Some(parsed);
                remaining = head;
            }
            _ => break,
        }
    }
    Some((std::path::PathBuf::from(remaining), line?))
}

fn ensure_pane_scope(authed_pane: &str, requested_pane: &str) -> Result<(), String> {
    if authed_pane == requested_pane {
        Ok(())
    } else {
        Err("control token is not authorized for that pane".to_string())
    }
}

fn ensure_agent_scope(state: &AppState, authed_pane: &str, agent_id: &str) -> Result<(), String> {
    let agent = state
        .agent(agent_id)?
        .ok_or_else(|| format!("agent {agent_id} was not found"))?;
    if agent.pane_id.as_deref() == Some(authed_pane) {
        Ok(())
    } else {
        Err("control token is not authorized for that agent".to_string())
    }
}

fn write_response(stream: &mut UnixStream, result: Result<Value, String>) -> std::io::Result<()> {
    let response = match result {
        Ok(data) => ControlResponse {
            ok: true,
            data,
            error: None,
        },
        Err(error) => ControlResponse {
            ok: false,
            data: Value::Null,
            error: Some(error),
        },
    };
    serde_json::to_writer(&mut *stream, &response)?;
    stream.write_all(b"\n")?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AdapterConfigs, ClaudeAdapterConfig, CodexAdapterConfig, GrokAdapterConfig,
        MuseAdapterConfig, OpencodeAdapterConfig, QmuxConfig,
    };

    use crate::workspace::{AgentInfo, AgentStatus};
    use std::io::Read;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_state() -> AppState {
        runtime_state(temp_dir(), PathBuf::from("/tmp/qmux-control-test.sock")).0
    }

    fn runtime_fixture() -> (AppState, PathBuf) {
        let dir = temp_dir();
        let socket_path = dir.join("qmux.sock");
        runtime_state(dir, socket_path)
    }

    fn runtime_state(workspace_root: PathBuf, socket_path: PathBuf) -> (AppState, PathBuf) {
        let state = AppState::new(QmuxConfig {
            remotes: Default::default(),
            workspace_root,
            socket_path: socket_path.clone(),
            adapters: AdapterConfigs {
                acp: Default::default(),
                pi: Default::default(),
                claude: ClaudeAdapterConfig {
                    binary: Some("claude".to_string()),
                },
                codex: CodexAdapterConfig {
                    binary: Some("codex".to_string()),
                },
                opencode: OpencodeAdapterConfig {
                    binary: Some("opencode".to_string()),
                },
                grok: GrokAdapterConfig {
                    binary: Some("grok".to_string()),
                },
                muse: MuseAdapterConfig {
                    binary: Some("muse".to_string()),
                },
                cursor: Default::default(),
                devin: Default::default(),
            },
            legacy_claude_binary: None,
            claude_plugin_dir: std::path::PathBuf::new(),
            opencode_plugin_dir: std::path::PathBuf::new(),
            pi_extension_dir: std::path::PathBuf::new(),
            cursor_plugin_dir: std::path::PathBuf::new(),
        });
        (state, socket_path)
    }

    fn ping_control_socket(path: &Path, token: &str) -> Result<ControlResponse, String> {
        let mut client = UnixStream::connect(path)
            .map_err(|err| format!("connect {}: {err}", path.display()))?;
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .map_err(|err| err.to_string())?;
        client
            .set_write_timeout(Some(Duration::from_secs(2)))
            .map_err(|err| err.to_string())?;
        let request = json!({ "token": token, "command": "ping", "payload": Value::Null });
        serde_json::to_writer(&mut client, &request).map_err(|err| err.to_string())?;
        client.write_all(b"\n").map_err(|err| err.to_string())?;
        client.flush().map_err(|err| err.to_string())?;
        let mut response = String::new();
        BufReader::new(client)
            .read_line(&mut response)
            .map_err(|err| err.to_string())?;
        serde_json::from_str::<ControlResponse>(&response).map_err(|err| err.to_string())
    }

    fn wait_for_ping(path: &Path, token: &str, timeout: Duration) -> ControlResponse {
        let started = std::time::Instant::now();
        let mut last = "no attempt".to_string();
        while started.elapsed() < timeout {
            match ping_control_socket(path, token) {
                Ok(response) if response.ok => return response,
                Ok(response) => last = format!("{:?}", response.error),
                Err(err) => last = err,
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "control socket did not accept ping at {} within {timeout:?}: {last}",
            path.display()
        );
    }

    fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool, message: &str) {
        let started = std::time::Instant::now();
        while started.elapsed() < timeout {
            if pred() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("{message}");
    }

    fn fd_count() -> usize {
        std::fs::read_dir("/dev/fd")
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("qmux-control-{nanos}-{seq}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn agent_bound_to(pane_id: &str) -> AgentInfo {
        AgentInfo {
            acp_config_options: Vec::new(),
            acp_agent: None,
            id: "agent-1".to_string(),
            group_id: "group-1".to_string(),
            adapter: "claude".to_string(),
            worktree_dir: "/tmp/agent-1".to_string(),
            branch: None,
            active_workspace: None,
            pane_id: Some(pane_id.to_string()),
            orphaned_queue_pane_id: None,
            session_id: None,
            transcript_path: None,
            status: AgentStatus::Running,
            model: None,
            effort: None,
            approval_mode: None,
            parent_id: None,
            fork_point: None,
            root_session_id: None,
            thread_id: None,
            branch_id: None,
            native_leaf_id: None,
            paused: false,
            created_at: 0,
        }
    }

    fn request_line(token: &str, command: &str, payload: Value) -> String {
        json!({ "token": token, "command": command, "payload": payload }).to_string()
    }

    #[test]
    fn browser_target_accepts_complete_loopback_urls_and_rejects_fragments() {
        assert_eq!(
            canonical_loopback_artifact_url("http://localhost:3000/app").as_deref(),
            Some("http://localhost:3000/app")
        );
        assert!(canonical_loopback_artifact_url("http://127.0.0.1:8080").is_some());
        assert!(canonical_loopback_artifact_url("http://[::1]:3000/").is_some());

        for invalid in [
            "http://example.com/",
            "http://127.0.0.1.evil.com/",
            "http://127.0.0.1@evil.com/",
            "http://evil.com\\@127.0.0.1/",
            "http://l",
            "http://lo",
            "http://localhos",
            "http://127.0.0",
            "http://localhost:5555|",
            "http://localhost:5556|localhost:5555|",
            "http://localhost:5557/\\",
        ] {
            assert!(
                canonical_loopback_artifact_url(invalid).is_none(),
                "accepted malformed loopback URL: {invalid}"
            );
        }
    }

    #[test]
    fn browser_target_rejects_absolute_paths_outside_the_pane_roots() {
        let state = test_state();
        // No pane has been inserted, so roots are empty and any absolute path
        // is refused rather than minting a token URL that would 403 later.
        let err = resolve_browser_target(
            &state,
            "missing-pane",
            "/Users/raymond/Code/multitool/dev/menubar-design-variants.html",
            None,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "'/Users/raymond/Code/multitool/dev/menubar-design-variants.html' was not found"
        );
    }

    #[test]
    fn browser_target_rejects_relative_paths_without_a_cwd() {
        let state = test_state();
        let err = resolve_browser_target(&state, "pane-1", "report.html", None).unwrap_err();
        assert!(err.contains("working directory"), "unexpected error: {err}");
    }

    #[test]
    fn line_column_suffixes_strip_only_trailing_digit_segments() {
        let strip = |s: &str| {
            strip_line_column_suffix(std::path::Path::new(s))
                .map(|(p, line)| (p.to_string_lossy().into_owned(), line))
        };
        assert_eq!(
            strip("/repo/src/session.rs:8"),
            Some(("/repo/src/session.rs".to_string(), 8))
        );
        // `path:line:col` keeps the line, drops the column.
        assert_eq!(
            strip("/repo/src/session.rs:8:14"),
            Some(("/repo/src/session.rs".to_string(), 8))
        );
        // No suffix → None, so callers can tell nothing was stripped.
        assert_eq!(strip("/repo/src/session.rs"), None);
        // Non-numeric colon segments are part of the name, not a location.
        assert_eq!(strip("/repo/notes:draft.md"), None);
        // A colon in a parent directory never triggers a strip.
        assert_eq!(strip("/repo/a:8/c.rs"), None);
        assert_eq!(
            strip("/repo/backup:2/c.rs:12"),
            Some(("/repo/backup:2/c.rs".to_string(), 12))
        );
    }

    #[test]
    fn browser_target_falls_back_past_a_line_suffix() {
        let dir = temp_dir();
        let file = dir.join("session.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let roots = vec![dir.clone()];
        // The literal path with the suffix does not exist…
        let suffixed = dir.join("session.rs:8");
        assert!(crate::file_server::resolve_under_roots(&suffixed, &roots).is_none());
        // …but the stripped candidate resolves to the real file, carrying the line.
        let (stripped, line) = strip_line_column_suffix(&suffixed).unwrap();
        assert_eq!(line, 8);
        assert_eq!(
            crate::file_server::resolve_under_roots(&stripped, &roots),
            Some(std::fs::canonicalize(&file).unwrap())
        );
    }

    #[test]
    fn handle_line_rejects_an_unknown_token() {
        let state = test_state();
        let err = handle_line(&state, &request_line("nope", "ping", Value::Null)).unwrap_err();
        assert!(
            err.contains("invalid QMUX_TOKEN"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn handle_line_accepts_ping_from_a_valid_token() {
        let state = test_state();
        let token = state.pane_token("pane-1").unwrap();
        let data = handle_line(&state, &request_line(&token, "ping", Value::Null)).unwrap();
        assert_eq!(data, json!({ "status": "ok" }));
    }

    #[test]
    fn interactive_user_token_is_confined_to_the_public_cli() {
        let state = test_state();
        let token = state.pane_user_token("pane-1").unwrap();

        let err = handle_line(&state, &request_line(&token, "ping", Value::Null)).unwrap_err();
        assert!(err.contains("valid only for public CLI operations"));

        let data = handle_line(
            &state,
            &request_line(
                &token,
                "cli.call",
                json!({ "operation": "ping", "arguments": {} }),
            ),
        )
        .unwrap();
        // The token reaches the public dispatcher. This synthetic test pane is
        // absent from the model, so context derivation then fails closed.
        assert_eq!(data["ok"], false);
        assert_eq!(data["error"]["code"], "pane_not_found");
    }

    #[test]
    fn pane_write_rejects_a_cross_pane_token() {
        let state = test_state();
        // pane-1's token must not be able to drive pane-2.
        let token = state.pane_token("pane-1").unwrap();
        let payload = json!({ "paneId": "pane-2", "data": "x", "paste": false, "submit": false });
        let err = handle_line(&state, &request_line(&token, "pane.write", payload)).unwrap_err();
        assert!(
            err.contains("not authorized for that pane"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn hook_notify_rejects_a_token_for_another_agents_pane() {
        let state = test_state();
        // The agent lives in pane-2, but the caller presents pane-1's token.
        state.insert_agent(agent_bound_to("pane-2")).unwrap();
        let token = state.pane_token("pane-1").unwrap();
        let payload = json!({ "event": "Stop", "agentId": "agent-1", "payload": Value::Null });
        let err = handle_line(&state, &request_line(&token, "hook.notify", payload)).unwrap_err();
        assert!(
            err.contains("not authorized for that agent"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn browser_open_records_an_artifact_only_for_agent_panes() {
        let state = test_state();
        let payload = json!({ "target": "http://localhost:5173/dash" });

        // A plain terminal pane opens the overlay without feeding the tray.
        let no_agent_token = state.pane_token("pane-1").unwrap();
        handle_line(
            &state,
            &request_line(&no_agent_token, "browser.open", payload.clone()),
        )
        .unwrap();
        assert!(state.list_artifacts().unwrap().is_empty());

        // A pane with an attached agent records the target — whether the agent
        // ran `qmux open` or the user did while the agent was backgrounded.
        state.insert_agent(agent_bound_to("pane-2")).unwrap();
        let agent_token = state.pane_token("pane-2").unwrap();
        handle_line(&state, &request_line(&agent_token, "browser.open", payload)).unwrap();
        let artifacts = state.list_artifacts().unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].pane_id, "pane-2");
        assert_eq!(
            artifacts[0].url.as_deref(),
            Some("http://localhost:5173/dash")
        );
        assert!(artifacts[0].path.is_none());
    }

    #[test]
    fn mcp_requires_an_agent_bound_to_the_authenticated_pane() {
        let state = test_state();
        let token = state.pane_token("pane-1").unwrap();
        let err = handle_line(
            &state,
            &request_line(
                &token,
                "mcp.call",
                json!({ "name": "whoami", "arguments": {} }),
            ),
        )
        .unwrap_err();
        assert!(err.contains("active agent pane"), "unexpected error: {err}");
    }

    #[test]
    fn mcp_whoami_resolves_identity_from_the_authenticated_pane() {
        let state = test_state();
        state.insert_agent(agent_bound_to("pane-2")).unwrap();
        let token = state.pane_token("pane-2").unwrap();
        let value = handle_line(
            &state,
            &request_line(
                &token,
                "mcp.call",
                json!({ "name": "whoami", "arguments": {} }),
            ),
        )
        .unwrap();
        assert_eq!(value["agent"]["id"], "agent-1");
        assert_eq!(
            value["capabilities"]["write"],
            "direct parent and direct children only"
        );
    }

    #[test]
    fn handle_line_rejects_an_unknown_command() {
        let state = test_state();
        let token = state.pane_token("pane-1").unwrap();
        let err = handle_line(&state, &request_line(&token, "bogus", Value::Null)).unwrap_err();
        assert!(
            err.contains("unknown control command"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn partial_request_times_out_server_reader() {
        let state = test_state();
        let (mut client, server) = UnixStream::pair().unwrap();
        let (done_tx, done_rx) = mpsc::channel();

        thread::spawn(move || {
            handle_client_with_timeout(state, server, Duration::from_millis(50));
            done_tx.send(()).unwrap();
        });

        client.write_all(b"{").unwrap();
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("server reader should exit after the read timeout");

        let mut buf = [0_u8; 1];
        assert_eq!(client.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn complete_request_still_receives_response() {
        let state = test_state();
        let token = state.pane_token("pane-1").unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        let (done_tx, done_rx) = mpsc::channel();

        thread::spawn(move || {
            handle_client_with_timeout(state, server, Duration::from_secs(1));
            done_tx.send(()).unwrap();
        });

        let request = json!({
            "token": token,
            "command": "ping",
            "payload": Value::Null,
        });
        serde_json::to_writer(&mut client, &request).unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let mut response = String::new();
        BufReader::new(client.try_clone().unwrap())
            .read_line(&mut response)
            .unwrap();
        let response = serde_json::from_str::<ControlResponse>(&response).unwrap();
        assert!(response.ok);
        assert_eq!(response.data, json!({ "status": "ok" }));

        drop(client);
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("server reader should exit after the client closes");
    }

    #[test]
    fn inspect_socket_path_classifies_missing_owned_and_replaced_inodes() {
        let dir = temp_dir();
        let path = dir.join("qmux.sock");
        assert_eq!(inspect_socket_path(&path, None), PathCheck::Missing);

        let _listener = UnixListener::bind(&path).unwrap();
        let meta = std::fs::symlink_metadata(&path).unwrap();
        let owned = (meta.dev(), meta.ino());
        assert_eq!(inspect_socket_path(&path, Some(owned)), PathCheck::Healthy);

        let replacement = dir.join("other.sock");
        let _other = UnixListener::bind(&replacement).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_eq!(inspect_socket_path(&path, Some(owned)), PathCheck::Conflict);
        assert_eq!(inspect_socket_path(&path, None), PathCheck::Conflict);
    }

    #[test]
    fn rebind_after_unlink_does_not_remove_a_replacement_socket() {
        let dir = temp_dir();
        let path = dir.join("qmux.sock");
        let _original = UnixListener::bind(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        let replacement = dir.join("other.sock");
        let _other = UnixListener::bind(&replacement).unwrap();
        let replacement_meta = std::fs::symlink_metadata(&replacement).unwrap();
        std::fs::rename(&replacement, &path).unwrap();

        assert!(bind_control_socket(&path).is_err());
        let current = std::fs::symlink_metadata(&path).unwrap();
        assert_eq!(current.dev(), replacement_meta.dev());
        assert_eq!(current.ino(), replacement_meta.ino());
    }

    #[test]
    fn supervisor_rebinds_after_the_socket_file_is_unlinked() {
        let (state, socket_path) = runtime_fixture();
        let token = state.pane_token("pane-1").unwrap();
        let runtime = start_control_socket_runtime(state, MAX_CONCURRENT_CLIENTS).unwrap();
        wait_for_ping(&socket_path, &token, Duration::from_secs(1));

        std::fs::remove_file(&socket_path).unwrap();
        let response = wait_for_ping(&socket_path, &token, Duration::from_secs(1));
        assert_eq!(response.data, json!({ "status": "ok" }));

        let events = runtime.transitions();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.as_str() == "control_socket.missing")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.as_str() == "control_socket.recovered")
                .count(),
            1
        );
        runtime.shutdown();
    }

    #[test]
    fn supervisor_repeated_unlink_recovery_does_not_grow_fds() {
        let (state, socket_path) = runtime_fixture();
        let token = state.pane_token("pane-1").unwrap();
        let runtime = start_control_socket_runtime(state, MAX_CONCURRENT_CLIENTS).unwrap();
        wait_for_ping(&socket_path, &token, Duration::from_secs(1));
        let before = fd_count();

        for _ in 0..8 {
            std::fs::remove_file(&socket_path).unwrap();
            wait_for_ping(&socket_path, &token, Duration::from_secs(1));
        }

        let after = fd_count();
        assert!(
            after <= before + 4,
            "fd count grew from {before} to {after} after repeated recovery"
        );
        runtime.shutdown();
    }

    #[test]
    fn supervisor_reports_conflict_and_does_not_delete_a_replacement() {
        let (state, socket_path) = runtime_fixture();
        let token = state.pane_token("pane-1").unwrap();
        let runtime = start_control_socket_runtime(state.clone(), MAX_CONCURRENT_CLIENTS).unwrap();
        wait_for_ping(&socket_path, &token, Duration::from_secs(1));

        let replacement_path = socket_path.with_file_name("other.sock");
        let replacement = UnixListener::bind(&replacement_path).unwrap();
        let replacement_meta = std::fs::symlink_metadata(&replacement_path).unwrap();
        std::fs::rename(&replacement_path, &socket_path).unwrap();

        wait_until(
            Duration::from_secs(1),
            || {
                runtime
                    .transitions()
                    .iter()
                    .any(|event| event == "control_socket.conflict")
            },
            "expected a control_socket.conflict transition",
        );
        let current = std::fs::symlink_metadata(&socket_path).unwrap();
        assert_eq!(current.dev(), replacement_meta.dev());
        assert_eq!(current.ino(), replacement_meta.ino());
        assert!(!state.owns_control_socket());

        thread::sleep(HEALTH_POLL_TIMEOUT.saturating_mul(3));
        let current = std::fs::symlink_metadata(&socket_path).unwrap();
        assert_eq!(current.ino(), replacement_meta.ino());
        assert_eq!(
            runtime
                .transitions()
                .iter()
                .filter(|event| event.as_str() == "control_socket.conflict")
                .count(),
            1
        );

        drop(replacement);
        let _ = std::fs::remove_file(&socket_path);
        wait_for_ping(&socket_path, &token, Duration::from_secs(1));
        runtime.shutdown();
    }

    #[test]
    fn supervisor_restarts_after_a_listener_panic() {
        let (state, socket_path) = runtime_fixture();
        let token = state.pane_token("pane-1").unwrap();
        let runtime = start_control_socket_runtime(state, MAX_CONCURRENT_CLIENTS).unwrap();
        wait_for_ping(&socket_path, &token, Duration::from_secs(1));

        runtime.inject_panic();
        wait_until(
            Duration::from_secs(1),
            || {
                runtime
                    .transitions()
                    .iter()
                    .any(|event| event == "control_socket.error")
            },
            "expected a control_socket.error transition after the injected panic",
        );
        wait_for_ping(&socket_path, &token, Duration::from_secs(1));
        runtime.shutdown();
    }

    #[test]
    fn supervisor_does_not_recreate_the_socket_after_shutdown() {
        let (state, socket_path) = runtime_fixture();
        let token = state.pane_token("pane-1").unwrap();
        let runtime = start_control_socket_runtime(state, MAX_CONCURRENT_CLIENTS).unwrap();
        wait_for_ping(&socket_path, &token, Duration::from_secs(1));

        std::fs::remove_file(&socket_path).unwrap();
        runtime.shutdown();
        thread::sleep(HEALTH_POLL_TIMEOUT.saturating_mul(4));
        assert!(
            !socket_path.exists(),
            "watchdog recreated {} after shutdown",
            socket_path.display()
        );
    }

    #[test]
    fn accepted_connections_survive_a_listener_rebind() {
        let (state, socket_path) = runtime_fixture();
        let token = state.pane_token("pane-1").unwrap();
        let runtime = start_control_socket_runtime(state, MAX_CONCURRENT_CLIENTS).unwrap();
        wait_for_ping(&socket_path, &token, Duration::from_secs(1));

        let mut held = UnixStream::connect(&socket_path).unwrap();
        // connect() can complete while the connection is still in the listen
        // backlog; wait long enough for accept() so dropping the old listener
        // does not discard this client.
        thread::sleep(Duration::from_millis(80));
        std::fs::remove_file(&socket_path).unwrap();
        wait_for_ping(&socket_path, &token, Duration::from_secs(1));

        let request = json!({ "token": token, "command": "ping", "payload": Value::Null });
        serde_json::to_writer(&mut held, &request).unwrap();
        held.write_all(b"\n").unwrap();
        held.flush().unwrap();
        let mut response = String::new();
        BufReader::new(held.try_clone().unwrap())
            .read_line(&mut response)
            .unwrap();
        let response = serde_json::from_str::<ControlResponse>(&response).unwrap();
        assert!(response.ok);
        assert_eq!(response.data, json!({ "status": "ok" }));
        runtime.shutdown();
    }

    #[test]
    fn supervisor_still_recovers_while_the_client_cap_is_held() {
        let (state, socket_path) = runtime_fixture();
        let token = state.pane_token("pane-1").unwrap();
        let runtime = start_control_socket_runtime(state, 2).unwrap();
        wait_for_ping(&socket_path, &token, Duration::from_secs(1));

        let _held_a = UnixStream::connect(&socket_path).unwrap();
        let _held_b = UnixStream::connect(&socket_path).unwrap();
        thread::sleep(Duration::from_millis(80));

        std::fs::remove_file(&socket_path).unwrap();
        wait_until(
            Duration::from_secs(1),
            || socket_path.exists(),
            "socket path should return while client slots are held",
        );
        drop(_held_a);
        drop(_held_b);
        wait_for_ping(&socket_path, &token, Duration::from_secs(1));
        runtime.shutdown();
    }
}
