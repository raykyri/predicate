use crate::adapters::{
    AdapterNotification, PrepareShellAgentLaunchRequest, PrepareShellClaudeLaunchRequest,
    adapter_registry, agent_fork, agent_prepare_shell_launch, ingest_adapter_notification,
    notification_adapter_hint,
};
use crate::connection_limit::ConnectionLimiter;
use crate::events::QmuxEvent;
use crate::pty::{PaneWriteOptions, write_pane};
use crate::state::AppState;
use crate::workspace::{
    LaunchOrigin, recover_shell_agent_from_session_start, validate_launch_workspace,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;
use std::time::Duration;

const CONTROL_SOCKET_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Cap on concurrent client-handler threads. Connections are mostly one-shot
/// (hook notifies, CLI invocations) with a 5s idle timeout, so this needs to
/// cover simultaneous in-flight requests, not panes; 64 is far above any real
/// burst while keeping a connection-spamming local process from exhausting
/// threads/FDs. At the cap the accept loop blocks and excess connections wait
/// in the kernel listen backlog.
const MAX_CONCURRENT_CLIENTS: usize = 64;
/// Backoff after a failed accept. Persistent accept errors (e.g. EMFILE under
/// FD exhaustion) would otherwise spin this loop hot and flood socket.error
/// events.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ControlRequest {
    token: String,
    command: String,
    #[serde(default)]
    payload: Value,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlResponse {
    ok: bool,
    data: Value,
    error: Option<String>,
}

pub fn start_control_socket(state: AppState) -> Result<(), String> {
    let socket_path = state.config().socket_path.clone();
    crate::shell_jobs::start_shell_job_monitor(state.clone());

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

    // Remove any stale socket unconditionally; a missing path is not an error.
    // Probing with exists() first would open a time-of-check/time-of-use window.
    match fs::remove_file(&socket_path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "failed to remove stale socket {}: {err}",
                socket_path.display()
            ));
        }
    }

    let listener = UnixListener::bind(&socket_path)
        .map_err(|err| format!("failed to bind socket {}: {err}", socket_path.display()))?;

    // Restrict the socket to the owning user so the per-pane token is not the only thing
    // standing between other local accounts and the control plane.
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600)).map_err(|err| {
        format!(
            "failed to restrict socket permissions {}: {err}",
            socket_path.display()
        )
    })?;

    // Remember which file we bound, so exit cleanup removes the socket only if the
    // path still points at it — not at a socket a later instance bound after
    // unlinking ours (the stale-socket removal above is exactly that action).
    {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::symlink_metadata(&socket_path).map_err(|err| {
            format!(
                "failed to stat bound socket {}: {err}",
                socket_path.display()
            )
        })?;
        state.set_control_socket_identity(meta.dev(), meta.ino());
    }

    thread::spawn(move || {
        let limiter = ConnectionLimiter::new(MAX_CONCURRENT_CLIENTS);
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let slot = limiter.acquire();
                    let state = state.clone();
                    thread::spawn(move || {
                        let _slot = slot;
                        handle_client(state, stream);
                    });
                }
                Err(err) => {
                    state.emit(QmuxEvent::new(
                        "socket.error",
                        None,
                        None,
                        json!({ "error": err.to_string() }),
                    ));
                    thread::sleep(ACCEPT_ERROR_BACKOFF);
                }
            }
        }
    });

    Ok(())
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
    let authed_pane = state
        .pane_for_token(&request.token)
        .ok_or_else(|| "invalid QMUX_TOKEN".to_string())?;

    match request.command.as_str() {
        "ping" => Ok(json!({ "status": "ok" })),
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

            if notification.event == "SessionStart" && state.agent_by_pane(&authed_pane)?.is_none()
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
            let outcome = ingest_adapter_notification(state, notification)?;
            for event in outcome.into_events() {
                state.emit(event);
            }
            Ok(json!({ "notified": true }))
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
                true,
                payload.prompt,
                None,
            )?;
            serde_json::to_value(pane).map_err(|err| format!("failed to encode forked pane: {err}"))
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
            // Resolve to a renderable URL: http(s) passes through (CSP gates it to
            // loopback); a file is validated under an allowed root and served via the
            // loopback file server. The overlay binds to the calling pane. `sandbox` is
            // true for served files so the overlay loads them in an opaque origin (the
            // served URL carries the access token, so its scripts must not be able to
            // read other files back); passthrough URLs are not sandboxed.
            let (url, sandbox) = resolve_browser_target(
                state,
                &authed_pane,
                payload.target.trim(),
                payload.cwd.as_deref(),
            )?;
            state.emit(QmuxEvent::new(
                "browser.open",
                Some(authed_pane.clone()),
                None,
                json!({ "url": url, "sandbox": sandbox }),
            ));
            Ok(json!({ "url": url, "sandbox": sandbox }))
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

/// Turns an `open` target into a URL the browser overlay can load, plus whether the
/// overlay should sandbox it. http(s) URLs pass through unchanged and unsandboxed
/// (covering localhost dev servers; the webview CSP restricts which actually render).
/// Anything else is treated as a file path: resolved against `cwd` when relative,
/// required to live under one of the requesting pane's own roots (not the global
/// union — so a pane can't open another pane's directory), served through the loopback
/// file server, and flagged `sandbox = true` (its URL carries the file-server token).
/// Whether an http(s) URL's host names a loopback address. Parses the authority
/// by hand (no `url` crate in this build) but defends against the usual spoofs:
/// `http://127.0.0.1@evil.com` (host is `evil.com`), `http://127.0.0.1.evil.com`
/// (not a loopback literal), and userinfo/port/IPv6-bracket forms. Intentionally
/// stricter than a browser — it accepts only `localhost` and parsed loopback IPs,
/// not oddball encodings a browser would still resolve to loopback — so it fails
/// closed.
///
/// A backslash is treated as an authority terminator too. WHATWG special-scheme
/// parsing (what WebKit and `new URL()` use) maps `\` to `/`, so without this a
/// target like `http://evil.com\@127.0.0.1/` would be judged loopback here (host
/// after the last `@` is `127.0.0.1`) while the webview resolves host `evil.com`.
/// Splitting on `\` makes this gate agree with WebKit and reject the spoof.
fn is_loopback_http_url(url: &str) -> bool {
    let Some((_scheme, rest)) = url.split_once("://") else {
        return false;
    };
    // The authority ends at the first '/', '\', '?' or '#'.
    let authority = rest.split(['/', '\\', '?', '#']).next().unwrap_or(rest);
    // Drop any userinfo: the host is whatever follows the last '@'.
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
    // Separate host from port, honoring the [ipv6]:port bracket form.
    let host = if let Some(after_bracket) = host_port.strip_prefix('[') {
        match after_bracket.split_once(']') {
            Some((inner, _)) => inner,
            None => return false,
        }
    } else {
        host_port.split(':').next().unwrap_or(host_port)
    };

    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(v4) = host.parse::<std::net::Ipv4Addr>() {
        return v4.is_loopback();
    }
    if let Ok(v6) = host.parse::<std::net::Ipv6Addr>() {
        return v6.is_loopback();
    }
    false
}

fn resolve_browser_target(
    state: &AppState,
    authed_pane: &str,
    target: &str,
    cwd: Option<&str>,
) -> Result<(String, bool), String> {
    if target.is_empty() {
        return Err("nothing to open".to_string());
    }
    if target.starts_with("http://") || target.starts_with("https://") {
        // Only loopback origins may render unsandboxed. The webview CSP restricts
        // frame-src to 127.0.0.1/localhost too, but enforce it here as the first
        // gate so a prompt-injected agent can't force the overlay at an arbitrary
        // origin (and gets a clear error rather than a silently-blank iframe).
        if !is_loopback_http_url(target) {
            return Err(format!(
                "refusing to open '{target}': the browser overlay only loads http(s) URLs on localhost/127.0.0.1"
            ));
        }
        return Ok((target.to_string(), false));
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
    let canonical =
        crate::file_server::resolve_under_roots(&requested, &roots).ok_or_else(|| {
            format!(
                "'{target}' was not found under this pane's working directory and cannot be opened"
            )
        })?;
    let port = state
        .file_server_port()
        .ok_or_else(|| "the file server is not running".to_string())?;
    // The URL carries this pane's own file token, so the file server scopes the request
    // back to `pane_file_roots(authed_pane)` — a pane can never reach another pane's
    // files even if some pane has widened its cwd via `pane.set_cwd`.
    let token = state.pane_file_token(authed_pane)?;
    Ok((crate::file_server::file_url(port, &token, &canonical), true))
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
        OpencodeAdapterConfig, QmuxConfig,
    };

    #[test]
    fn loopback_url_check_accepts_localhost_and_rejects_spoofs() {
        // Loopback origins the overlay may render unsandboxed.
        assert!(is_loopback_http_url("http://localhost:3000/app"));
        assert!(is_loopback_http_url("http://127.0.0.1:8080"));
        assert!(is_loopback_http_url("https://127.0.0.1/"));
        assert!(is_loopback_http_url("http://127.1.2.3:5173/x?y=1#z"));
        assert!(is_loopback_http_url("http://[::1]:3000/"));
        assert!(is_loopback_http_url("http://LocalHost/"));

        // Non-loopback and spoofed hosts must be refused.
        assert!(!is_loopback_http_url("http://example.com/"));
        assert!(!is_loopback_http_url("http://127.0.0.1.evil.com/"));
        assert!(!is_loopback_http_url("http://127.0.0.1@evil.com/"));
        assert!(!is_loopback_http_url("http://evil.com/#127.0.0.1"));
        assert!(!is_loopback_http_url("http://evil.com/?x=127.0.0.1"));
        assert!(!is_loopback_http_url("http://0.0.0.0/"));
        assert!(!is_loopback_http_url("http://2130706433/"));
        assert!(!is_loopback_http_url("not-a-url"));

        // Backslash authority-terminator spoof: WebKit maps `\` to `/`, so the real
        // host is `evil.com`. This must be rejected, matching the webview parser.
        assert!(!is_loopback_http_url("http://evil.com\\@127.0.0.1/"));
        assert!(!is_loopback_http_url("http://evil.com\\127.0.0.1/"));
        assert!(!is_loopback_http_url("https://evil.com\\@localhost/app"));
        // A genuine loopback host followed by a backslash path is still loopback
        // (WebKit reads `http://127.0.0.1/@evil.com/`), so this stays allowed.
        assert!(is_loopback_http_url("http://127.0.0.1\\@evil.com/"));
    }

    use crate::workspace::{AgentInfo, AgentStatus};
    use std::io::Read;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_state() -> AppState {
        AppState::new(QmuxConfig {
            workspace_root: temp_dir(),
            socket_path: PathBuf::from("/tmp/qmux-control-test.sock"),
            adapters: AdapterConfigs {
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
            },
            legacy_claude_binary: None,
            claude_plugin_dir: std::path::PathBuf::new(),
            opencode_plugin_dir: std::path::PathBuf::new(),
        })
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
            id: "agent-1".to_string(),
            group_id: "group-1".to_string(),
            adapter: "claude".to_string(),
            worktree_dir: "/tmp/agent-1".to_string(),
            branch: None,
            pane_id: Some(pane_id.to_string()),
            orphaned_queue_pane_id: None,
            session_id: None,
            transcript_path: None,
            status: AgentStatus::Running,
            model: None,
            effort: None,
            parent_id: None,
            fork_point: None,
            root_session_id: None,
            thread_id: None,
            branch_id: None,
            paused: false,
            created_at: 0,
        }
    }

    fn request_line(token: &str, command: &str, payload: Value) -> String {
        json!({ "token": token, "command": command, "payload": payload }).to_string()
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
}
