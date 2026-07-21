use crate::adapters::{ShellCommandIntegration, adapter_registry};
use crate::events::QmuxEvent;
use crate::scrollback::{append_pane_scrollback, read_pane_scrollback, sanitize_scrollback_replay};
use crate::state::{
    AppState, PaneBackend, PaneInfo, PaneKind, PaneRuntime, PaneStatus, SharedBacklog, SharedChild,
    SharedWriter, ShellAgentResume,
};
use crate::turn_queue::release_waiters_for_agent;
use crate::workspace::{
    CreateGroupRequest, WorkspaceScope, capture_agent_worktree_removal, create_group,
    remove_captured_worktree,
};
use portable_pty::PtySize;
use portable_pty::{CommandBuilder, native_pty_system};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const SUBMIT_KEY: &[u8] = b"\r";
// End by clearing Kitty keyboard enhancements. Historical agent output is
// sanitized before this is sent, but the explicit reset is defense in depth
// against an unrecognized keyboard-protocol form reaching the fresh surface.
// It runs before the new process's buffered startup output, so an agent resumed
// into the pane can still enable its desired live keyboard mode afterward.
const RESTORED_SCROLLBACK_TERMINAL_RESET: &[u8] = b"\x18\x1b>\x1b[0m\x1b(B\x1b[4l\x1b[?1l\x1b[?7h\x1b[?9l\x1b[?25h\x1b[?45l\x1b[?66l\x1b[?47l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l\x1b[?1047l\x1b[?2004l\x1b[?2026l\x1b[>4;0m\x1b[=0u";
// The subset of the reset that is safe to send to a *live* pane's surface —
// one an exited agent left behind for the surviving shell — as opposed to a
// fresh surface being rebuilt from scrollback. It clears only latched input
// and reporting modes (keypad, cursor-key, mouse, focus, bracketed paste,
// synchronized output, xterm modifyOtherKeys, and the Kitty keyboard flags)
// plus cursor-position-neutral display state (SGR, ASCII charset, insert mode,
// autowrap, reverse-wrap, cursor visibility). It deliberately omits every byte
// in the full reset that can move the cursor or swap the screen buffer: the
// leading CAN (`\x18`) and the alternate-screen exits (`\x1b[?47l`,
// `\x1b[?1047l`). Those are correct when rebuilding a fresh surface — the
// cursor is being reconstructed anyway and any historical alternate-screen
// entry must be closed — but on a live surface the shell has already regained
// control and is about to print its prompt at the current cursor; a screen
// swap that does not restore the cursor (47/1047 never do) or a CAN landing
// mid-sequence would strand that prompt at the wrong column. The durable log
// still records the full reset (see `reset_pane_terminal_modes`) so a future
// restore and any trim still close a mid-alternate-screen entry.
const LIVE_PANE_TERMINAL_MODE_RESET: &[u8] = b"\x1b>\x1b[0m\x1b(B\x1b[4l\x1b[?1l\x1b[?7h\x1b[?9l\x1b[?25h\x1b[?45l\x1b[?66l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l\x1b[?2004l\x1b[?2026l\x1b[>4;0m\x1b[=0u";
const SUBMIT_KEY_DELAY: Duration = Duration::from_millis(15);
/// Ghostty can accept a synthesized Return before its PTY write callback runs.
/// Retry only that Return a small, bounded number of times; the pasted payload
/// is deliberately outside this retry loop so a missing acknowledgement cannot
/// duplicate a queued turn.
const NATIVE_SUBMIT_ACK_ATTEMPTS: usize = 2;
const NATIVE_SUBMIT_ACK_RETRY_DELAY: Duration = Duration::from_millis(50);
const NATIVE_INPUT_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_PTY_COLS: u16 = 100;
const DEFAULT_PTY_ROWS: u16 = 24;
const MIN_INITIAL_COLS: u16 = 20;
const MIN_INITIAL_ROWS: u16 = 5;
const MAX_INITIAL_COLS: u16 = 500;
const MAX_INITIAL_ROWS: u16 = 200;
/// Cap on PTY output buffered before the frontend attaches. Recovered agent TUIs
/// can repaint a large transcript before the webview replays durable scrollback
/// and calls `pane_attach`; keeping the full repaint preserves SGR/background
/// state that later bytes in the same draw rely on.
#[cfg_attr(all(target_os = "macos", not(test)), allow(dead_code))]
const BACKLOG_CAP: usize = 8 * 1024 * 1024;

/// How often the per-pane child watcher checks whether the direct child (shell or
/// agent) has exited. Cheap — a non-blocking `try_wait` under the child lock — so
/// a couple of seconds keeps a stuck pane's "Running" state from lingering long
/// without meaningful cost.
#[cfg_attr(all(target_os = "macos", not(test)), allow(dead_code))]
const CHILD_WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// How many watch intervals between refreshes of a pane's descendant-pid
/// snapshot. Descendants that outlive their shell (dev servers, `sleep &`)
/// are long-lived, so a coarse ~16s refresh catches them while keeping the
/// `pgrep` walk off the steady-state path.
const DESCENDANT_REFRESH_TICKS: u32 = 8;

/// How many watch intervals the child watcher waits after its SIGTERM burst
/// before escalating to SIGKILL, for descendants that ignore SIGTERM while
/// holding the PTY slave open (which blocks the reader's EOF cleanup and
/// leaves a dead pane stuck "Running" forever).
const KILL_ESCALATION_TICKS: u32 = 2;

/// Panes whose attach was requested before their native surface had committed
/// real geometry. Replaying durable scrollback into a surface that still has
/// its zero-frame default grid renders history at the wrong width; the fit
/// that follows the first real layout then reflows those rows and scatters
/// restored lines mid-row (most visibly zsh's PROMPT_SP full-width padding,
/// which turns every restored prompt into a diagonal staircase). Attaches are
/// parked here and finished by `complete_pending_attach` once Swift reports
/// the surface fitted to a real frame. This set's lock is only ever held for
/// an insert/remove — never across FFI or another lock.
static DEFERRED_ATTACHES: std::sync::LazyLock<Mutex<HashSet<String>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));

/// Per-native-pane input senders, feeding each pane's writer thread (see
/// `start_native_input_writer`). Ghostty's input callback delivers every
/// keystroke and paste chunk through `write_native_host_input`; resolving the
/// sender here keeps that per-keystroke path off the global model lock, and
/// queueing keeps it from blocking on a full PTY buffer — a TUI stopped with
/// ^S/SIGSTOP would otherwise wedge the callback's thread until the child
/// drained. The map lock is only ever held for a lookup/insert/remove.
enum NativeInputMessage {
    Data(Vec<u8>),
    Flush(std::sync::mpsc::SyncSender<Result<u64, String>>),
}

static NATIVE_INPUT_SENDERS: std::sync::LazyLock<
    Mutex<HashMap<String, std::sync::mpsc::Sender<NativeInputMessage>>>,
> = std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InitialPaneSize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneActivity {
    pub kind: PaneActivityKind,
    pub process_count: usize,
    pub process_summary: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PaneActivityKind {
    Idle,
    RunningProcess,
}

impl PaneActivity {
    fn idle() -> Self {
        Self {
            kind: PaneActivityKind::Idle,
            process_count: 0,
            process_summary: None,
        }
    }

    fn running_process(process_count: usize, process_summary: Option<String>) -> Self {
        Self {
            kind: PaneActivityKind::RunningProcess,
            process_count,
            process_summary,
        }
    }
}

pub struct PtySpawnSpec {
    pub pane_id: Option<String>,
    pub agent_id: Option<String>,
    pub group_id: String,
    pub kind: PaneKind,
    pub title: String,
    pub last_osc_title: Option<String>,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub envs: Vec<(String, String)>,
    pub initial_size: Option<InitialPaneSize>,
    pub recovered: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneWriteOptions {
    pub pane_id: String,
    pub data: String,
    pub paste: bool,
    pub submit: bool,
}

/// A pane write failure that records how far the paste-then-submit sequence got.
/// `data_delivered` distinguishes "the payload never reached the pane" from "the
/// payload landed but the trailing submit (or a post-payload barrier) failed". The
/// turn queue uses the flag to requeue a failed turn as possibly-pasted, so its
/// retry submits a bare Return instead of pasting a second copy of the text onto
/// the one already sitting in the composer.
#[derive(Debug)]
pub struct PaneWriteFailure {
    pub error: String,
    pub data_delivered: bool,
}

impl PaneWriteFailure {
    fn before_data(error: String) -> Self {
        Self {
            error,
            data_delivered: false,
        }
    }

    fn after_data(error: String) -> Self {
        Self {
            error,
            data_delivered: true,
        }
    }
}

pub fn spawn_shell_pane(
    state: &AppState,
    initial_size: Option<InitialPaneSize>,
    source_pane_id: Option<&str>,
    group_id: Option<&str>,
) -> Result<PaneInfo, String> {
    // A user-opened shell inherits the focused shell's current directory when one is
    // given and still valid (matching how terminal emulators open a "new tab here");
    // otherwise it opens in the target group directory / home, never the bare `/` a
    // Finder/Dock launch inherits as its cwd.
    let source_group_id = source_pane_id.and_then(|id| state.pane_group_id(id).ok().flatten());
    let group = match group_id.or(source_group_id.as_deref()) {
        Some(group_id) => state
            .group(group_id)?
            .ok_or_else(|| format!("group {group_id} was not found"))?,
        None => create_group(
            state,
            CreateGroupRequest {
                name: None,
                dir: None,
                after_group_id: None,
                base_repo: None,
                base_ref: None,
            },
        )?,
    };
    if group.scope != WorkspaceScope::Terminal {
        return Err("ordinary shells cannot be opened in a research workspace".to_string());
    }
    // Inherit the focused shell's cwd only when it belongs to the group we are
    // spawning into ("new tab here"). When opening into a group from *outside* it,
    // derive the cwd from that group's own most-recently-active shell pane instead
    // of the foreign pane the user happened to be in. A brand-new group has no shell
    // panes yet, so fall back to its creation-time seed dir (`group.dir`) — the
    // directory the group was opened for — before the default home dir; otherwise the
    // first terminal would land in ~ and every sibling would copy that.
    let cwd = source_pane_id
        .filter(|&id| {
            state
                .pane_group_id(id)
                .ok()
                .flatten()
                .is_some_and(|gid| gid == group.id)
        })
        .and_then(|id| state.inheritable_shell_cwd(id))
        .or_else(|| state.group_spawn_cwd(&group.id))
        .or_else(|| recoverable_dir(&group.dir))
        .unwrap_or_else(|| state.default_open_dir());
    let pane_id = state.next_id("pane");
    spawn_pty(
        state,
        shell_spawn_spec(state, pane_id, group.id, cwd, initial_size, false, None)?,
    )
}

pub fn ensure_shell_agent_startup_supported() -> Result<(), String> {
    let shell = pane_shell();
    ensure_shell_agent_startup_supported_for(&shell)
}

fn ensure_shell_agent_startup_supported_for(shell: &str) -> Result<(), String> {
    if matches!(shell_kind(&shell), ShellKind::Unsupported) {
        return Err(format!(
            "persistent-shell agent forks require zsh or bash; configured shell '{}' is unsupported",
            shell
        ));
    }
    Ok(())
}

/// Opens a regular shell pane and automatically launches an agent command after
/// the user's shell configuration has loaded. The command deliberately does not
/// `exec`: `qmux agent-exec` supervises the adapter and returns to the live shell
/// prompt when the agent exits, matching an agent typed manually in a terminal.
pub fn spawn_shell_agent_command_pane(
    state: &AppState,
    pane_id: String,
    group_id: String,
    cwd: PathBuf,
    adapter_id: &str,
    agent_args: &[String],
    prepared_agent_id: &str,
) -> Result<PaneInfo, String> {
    let qmux_cli = env::current_exe()
        .map_err(|err| format!("failed to resolve qmux executable for fork launch: {err}"))?;
    let startup_command =
        shell_agent_exec_command(&qmux_cli, adapter_id, agent_args, prepared_agent_id);
    let spec = shell_spawn_spec(
        state,
        pane_id,
        group_id,
        cwd,
        None,
        false,
        Some(startup_command),
    )?;
    spawn_pty(state, spec)
}

fn shell_agent_exec_command(
    qmux_cli: &Path,
    adapter_id: &str,
    agent_args: &[String],
    prepared_agent_id: &str,
) -> String {
    let mut command = vec![
        format!(
            "QMUX_PREPARED_AGENT_ID={}",
            shell_quote_str(prepared_agent_id)
        ),
        shell_quote(&qmux_cli),
        "agent-exec".to_string(),
        shell_quote_str(adapter_id),
    ];
    command.extend(agent_args.iter().map(|arg| shell_quote_str(arg)));
    command.join(" ")
}

/// Recreates a previously persisted shell pane: same pane id (so UI mappings and
/// queues keep lining up), reopened in its last-known cwd when that still exists,
/// at its persisted geometry. Marked recovered so the UI can label it.
pub fn respawn_shell_pane(state: &AppState, pane: &PaneInfo) -> Result<PaneInfo, String> {
    // A queued resume rebinds the agent that was live in this pane at shutdown. Its
    // session is keyed to the original launch dir (Claude/Codex scope sessions by project
    // dir), and the resume command runs in whatever cwd this shell reopens in, so reopen
    // there rather than the pane's last cwd — which `cd` may have moved away from since
    // launch (`update_pane_cwd` tracks the live directory). Reopening at the drifted cwd
    // would both fail to resolve the session and miss the agent rebind, minting a
    // duplicate on every restart. The hint is taken (drained) either way so it can't
    // linger and fire on a later relaunch of the same pane id; the resume only proceeds
    // when that original dir still exists.
    let resume = state.take_shell_agent_resume(&pane.id);
    let resume_dir = resume
        .as_ref()
        .and_then(|resume| recoverable_dir(&resume.cwd));
    // A recovered shell whose last dir was deleted between sessions reopens near the
    // group's other work (its most-recently-active shell pane), else the group's
    // creation-time seed dir, else the default dir / home rather than the bare `/` a
    // Finder/Dock launch inherits. During startup recovery siblings may not be
    // respawned yet, in which case group_spawn_cwd yields None and the seed/default
    // apply.
    let group_seed_dir = state
        .group(&pane.group_id)
        .ok()
        .flatten()
        .and_then(|group| recoverable_dir(&group.dir));
    let cwd = resume_dir
        .clone()
        .or_else(|| recoverable_dir(&pane.cwd))
        .or_else(|| state.group_spawn_cwd(&pane.group_id))
        .or(group_seed_dir)
        .unwrap_or_else(|| state.default_open_dir());
    let resume_command = resume
        .filter(|_| resume_dir.is_some())
        .and_then(|resume| shell_resume_command(state, &resume));
    let initial_size = Some(InitialPaneSize {
        cols: pane.cols,
        rows: pane.rows,
    });
    let mut spec = shell_spawn_spec(
        state,
        pane.id.clone(),
        pane.group_id.clone(),
        cwd,
        initial_size,
        true,
        resume_command,
    )?;
    // A shell pane may carry a manual/generated base title as well as a cached
    // OSC title. Recovery previously rebuilt every shell as literal "Shell",
    // discarding even explicitly renamed tabs.
    apply_recovered_shell_titles(&mut spec, pane);
    spawn_pty(state, spec)
}

fn apply_recovered_shell_titles(spec: &mut PtySpawnSpec, pane: &PaneInfo) {
    spec.title.clone_from(&pane.title);
    spec.last_osc_title.clone_from(&pane.last_osc_title);
}

/// Resolves the shell command that resumes a captured agent session through its
/// adapter's injected wrapper (e.g. `claude --resume <id>`). `None` when the adapter
/// has no resume command.
fn shell_resume_command(state: &AppState, resume: &ShellAgentResume) -> Option<String> {
    adapter_registry(state.config())
        .get(&resume.adapter)
        .ok()?
        .shell_resume_command(&resume.session_id)
}

/// The shell for new panes: `$SHELL` when set (terminal launches), else the
/// user's login shell from the password database (GUI launches don't inherit
/// `SHELL`), else a platform default.
fn pane_shell() -> String {
    if let Ok(shell) = env::var("SHELL")
        && !shell.trim().is_empty()
    {
        return shell;
    }
    if let Some(shell) = passwd_login_shell() {
        return shell;
    }
    let fallback = if cfg!(target_os = "macos") {
        "/bin/zsh"
    } else {
        "/bin/sh"
    };
    fallback.to_string()
}

/// Reads the current user's login shell from the password database via the
/// reentrant `getpwuid_r` (pane spawns can run concurrently on command threads).
fn passwd_login_shell() -> Option<String> {
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = [0 as libc::c_char; 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() || pwd.pw_shell.is_null() {
        return None;
    }
    let shell = unsafe { std::ffi::CStr::from_ptr(pwd.pw_shell) }
        .to_str()
        .ok()?
        .trim();
    (!shell.is_empty()).then(|| shell.to_string())
}

/// Builds the spawn spec for a shell pane, including adapter wrapper-function
/// injection. Shared by fresh spawns and recovery respawns so both stay in sync.
fn shell_spawn_spec(
    state: &AppState,
    pane_id: String,
    group_id: String,
    cwd: PathBuf,
    initial_size: Option<InitialPaneSize>,
    recovered: bool,
    startup_command: Option<String>,
) -> Result<PtySpawnSpec, String> {
    let shell = pane_shell();
    let mut envs = shell_pane_envs(state, &pane_id)?;
    let mut args = Vec::new();

    let shell_commands = adapter_registry(state.config()).shell_commands();
    let login_shell = state.use_login_shell();
    match agent_shell_function_injection(
        &shell,
        &pane_id,
        &shell_commands,
        startup_command.as_deref(),
        login_shell,
    ) {
        Ok(Some(injection)) => {
            args = injection.args;
            envs.extend(injection.envs);
            envs.push(("QMUX_AGENT_FUNCTIONS".to_string(), "1".to_string()));
        }
        // A plain shell degrades gracefully when integration can't be set up —
        // the pane still works, only the wrapper functions are missing. A
        // fresh fork launch cannot: its startup command lives in the rcfile,
        // so spawning without it would open an ordinary shell while the
        // reserved fork agent stays bound and Running forever (no agent-exec
        // ever reaches the backend to settle it). Fail that spawn instead so
        // the caller's spawn-failure settlement marks the agent and tells the
        // user. Recovery respawns (`recovered`) keep degrading: losing a
        // best-effort session resume is better than losing the pane.
        Ok(None) => {
            if startup_command.is_some() && !recovered {
                return Err(format!(
                    "persistent-shell agent forks require zsh or bash; configured shell '{shell}' is unsupported"
                ));
            }
            envs.push((
                "QMUX_AGENT_FUNCTIONS".to_string(),
                "unsupported".to_string(),
            ));
        }
        Err(err) => {
            if startup_command.is_some() && !recovered {
                return Err(err);
            }
            envs.push(("QMUX_AGENT_FUNCTIONS".to_string(), "failed".to_string()));
            envs.push(("QMUX_AGENT_FUNCTIONS_ERROR".to_string(), err));
        }
    }

    Ok(PtySpawnSpec {
        pane_id: Some(pane_id),
        agent_id: None,
        group_id,
        kind: PaneKind::Shell,
        title: "Shell".to_string(),
        last_osc_title: None,
        program: shell,
        args,
        cwd,
        envs,
        initial_size,
        recovered,
    })
}

/// Returns the path only when it still resolves to a directory, so recovery can
/// fall back gracefully when a persisted cwd or worktree has since been removed.
pub fn recoverable_dir(path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(path);
    path.is_dir().then_some(path)
}

pub fn qmux_pane_envs(state: &AppState, pane_id: &str) -> Result<Vec<(String, String)>, String> {
    let mut envs = vec![
        ("QMUX_PANE_ID".to_string(), pane_id.to_string()),
        (
            "QMUX_SOCK".to_string(),
            state.config().socket_path.display().to_string(),
        ),
        ("QMUX_TOKEN".to_string(), state.pane_token(pane_id)?),
        (
            "QMUX_WORKSPACE_ROOT".to_string(),
            state.config().workspace_root.display().to_string(),
        ),
    ];
    // Expose the qmux executable so in-pane tooling (e.g. the fork skill) can call it
    // without depending on `qmux` being on PATH. Best-effort: omitted if unresolved.
    if let Ok(exe) = std::env::current_exe() {
        envs.push(("QMUX_CLI".to_string(), exe.display().to_string()));
    }
    Ok(envs)
}

fn shell_pane_envs(state: &AppState, pane_id: &str) -> Result<Vec<(String, String)>, String> {
    let mut envs = qmux_pane_envs(state, pane_id)?;
    envs.push(("QMUX_SHELL_INTEGRATION".to_string(), "1".to_string()));
    Ok(envs)
}

struct ShellFunctionInjection {
    args: Vec<String>,
    envs: Vec<(String, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellKind {
    Bash,
    Zsh,
    Unsupported,
}

/// Per-pane scratch directory holding generated shell rc files. The location is
/// derived purely from the pane id so teardown can find it without consulting
/// pane state.
fn shell_integration_dir(pane_id: &str) -> PathBuf {
    env::temp_dir().join("qmux-shell-init").join(pane_id)
}

/// Creates the per-pane shell integration directory restricted to the owning
/// user. The shared parent is locked to `0o700` first so other local accounts
/// cannot pre-create (or symlink) a pane's subdirectory and redirect the rc
/// files we are about to write; the per-pane dir is then created `0o700` too so
/// its generated scripts are never world-readable in a shared /tmp.
fn create_shell_integration_dir(pane_id: &str) -> Result<PathBuf, String> {
    let parent = env::temp_dir().join("qmux-shell-init");
    fs::create_dir_all(&parent).map_err(|err| {
        format!(
            "failed to create shell integration root {}: {err}",
            parent.display()
        )
    })?;
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).map_err(|err| {
        format!(
            "failed to restrict shell integration root {}: {err}",
            parent.display()
        )
    })?;

    let root = parent.join(pane_id);
    fs::create_dir_all(&root).map_err(|err| {
        format!(
            "failed to create shell integration dir {}: {err}",
            root.display()
        )
    })?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).map_err(|err| {
        format!(
            "failed to restrict shell integration dir {}: {err}",
            root.display()
        )
    })?;
    Ok(root)
}

/// Removes a pane's shell integration scratch directory on teardown. Best
/// effort: a missing directory (non-shell pane, or one that never spawned a
/// supported shell) is expected and ignored.
fn remove_shell_integration_dir(pane_id: &str) {
    let root = shell_integration_dir(pane_id);
    match fs::remove_dir_all(&root) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            // A stale scratch dir is non-fatal and not worth surfacing to the UI.
            eprintln!(
                "qmux: failed to clean up shell integration dir {}: {err}",
                root.display()
            );
        }
    }
}

fn agent_shell_function_injection(
    shell: &str,
    pane_id: &str,
    shell_commands: &[ShellCommandIntegration],
    startup_command: Option<&str>,
    login_shell: bool,
) -> Result<Option<ShellFunctionInjection>, String> {
    let shell_kind = shell_kind(shell);
    if matches!(shell_kind, ShellKind::Unsupported) {
        return Ok(None);
    }

    let qmux_cli = env::current_exe()
        .map_err(|err| format!("failed to resolve qmux executable for shell integration: {err}"))?;
    let root = create_shell_integration_dir(pane_id)?;

    match shell_kind {
        ShellKind::Zsh => {
            let zdotdir = root.join("zsh");
            fs::create_dir_all(&zdotdir).map_err(|err| {
                format!(
                    "failed to create zsh integration dir {}: {err}",
                    zdotdir.display()
                )
            })?;
            let rcfile = zdotdir.join(".zshrc");
            fs::write(
                &rcfile,
                zsh_init_script(&qmux_cli, shell_commands, startup_command, login_shell),
            )
            .map_err(|err| {
                format!(
                    "failed to write zsh integration file {}: {err}",
                    rcfile.display()
                )
            })?;
            let mut envs = vec![("ZDOTDIR".to_string(), zdotdir.display().to_string())];
            if let Some(zdotdir) = original_zdotdir() {
                envs.push(("QMUX_ORIGINAL_ZDOTDIR".to_string(), zdotdir));
            }
            Ok(Some(ShellFunctionInjection {
                args: vec!["-i".to_string()],
                envs,
            }))
        }
        ShellKind::Bash => {
            let rcfile = root.join("bashrc");
            fs::write(
                &rcfile,
                bash_init_script(&qmux_cli, shell_commands, startup_command, login_shell),
            )
            .map_err(|err| {
                format!(
                    "failed to write bash integration file {}: {err}",
                    rcfile.display()
                )
            })?;
            let mut envs = Vec::new();
            if let Some(bashrc) = original_bashrc() {
                envs.push(("QMUX_ORIGINAL_BASHRC".to_string(), bashrc));
            }
            Ok(Some(ShellFunctionInjection {
                args: vec![
                    "--rcfile".to_string(),
                    rcfile.display().to_string(),
                    "-i".to_string(),
                ],
                envs,
            }))
        }
        ShellKind::Unsupported => Ok(None),
    }
}

fn shell_kind(shell: &str) -> ShellKind {
    match Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(shell)
    {
        "bash" => ShellKind::Bash,
        "zsh" => ShellKind::Zsh,
        _ => ShellKind::Unsupported,
    }
}

fn zsh_init_script(
    qmux_cli: &Path,
    shell_commands: &[ShellCommandIntegration],
    startup_command: Option<&str>,
    login_shell: bool,
) -> String {
    let cli = shell_quote(qmux_cli);
    let qmux_function = shell_qmux_function(&cli);
    let agent_functions = shell_agent_functions(&cli, shell_commands);
    let startup = zsh_startup_command(startup_command);
    // A login shell also sources the user's .zprofile (before .zshrc) and .zlogin
    // (after), matching zsh's login startup order. We source these ourselves rather
    // than passing `-l`: ZDOTDIR is redirected to the per-pane integration dir during
    // early startup, so zsh's own login-file lookup would miss the user's copies.
    // Sourcing here, after ZDOTDIR is restored, loads the right files and keeps bash
    // and zsh behaving identically.
    let user_config = if login_shell {
        r#"  if [ -r "$ZDOTDIR/.zprofile" ]; then
    source "$ZDOTDIR/.zprofile"
  fi
  if [ -r "$ZDOTDIR/.zshrc" ]; then
    source "$ZDOTDIR/.zshrc"
  fi
  if [ -r "$ZDOTDIR/.zlogin" ]; then
    source "$ZDOTDIR/.zlogin"
  fi"#
    } else {
        r#"  if [ -r "$ZDOTDIR/.zshrc" ]; then
    source "$ZDOTDIR/.zshrc"
  fi"#
    };
    format!(
        r#"# Generated by qmux. Do not edit.
if [ -n "${{QMUX_ORIGINAL_ZDOTDIR:-}}" ]; then
  __qmux_zdotdir="$ZDOTDIR"
  export ZDOTDIR="$QMUX_ORIGINAL_ZDOTDIR"
  # /etc/zshrc ran while ZDOTDIR was the per-pane integration dir, so on macOS
  # HISTFILE points at a scratch file that is deleted with the pane. Re-derive
  # it from the restored ZDOTDIR; the user's .zshrc below can still override.
  case "${{HISTFILE:-}}" in
    "$__qmux_zdotdir"/*) HISTFILE="$ZDOTDIR/.zsh_history" ;;
  esac
  unset __qmux_zdotdir
{user_config}
fi
{qmux_function}
{agent_functions}
if [ -n "${{QMUX_PANE_ID:-}}" ]; then
  typeset -g __qmux_last_pwd=""
  __qmux_report_cwd() {{
    if [ "$PWD" != "$__qmux_last_pwd" ]; then
      __qmux_last_pwd="$PWD"
      {cli} cwd >/dev/null 2>&1
    fi
  }}
  autoload -Uz add-zsh-hook 2>/dev/null && add-zsh-hook precmd __qmux_report_cwd
fi
{startup}"#,
    )
}

fn bash_init_script(
    qmux_cli: &Path,
    shell_commands: &[ShellCommandIntegration],
    startup_command: Option<&str>,
    login_shell: bool,
) -> String {
    let cli = shell_quote(qmux_cli);
    let qmux_function = shell_qmux_function(&cli);
    let agent_functions = shell_agent_functions(&cli, shell_commands);
    let startup = bash_startup_command(startup_command);
    // A login shell sources the first existing of the user's login profile files —
    // the same set, in the same order, a real `bash -l` consults — which by
    // convention pulls in ~/.bashrc itself. We can't pass `--login` because bash
    // ignores `--rcfile` (where our integration lives) for login shells, so we
    // reproduce the login file lookup here instead. A non-login shell sources
    // ~/.bashrc directly, as bash does for interactive non-login shells.
    let user_config = if login_shell {
        r#"for __qmux_login_rc in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile"; do
  if [ -r "$__qmux_login_rc" ]; then
    . "$__qmux_login_rc"
    break
  fi
done
unset __qmux_login_rc"#
    } else {
        r#"if [ -n "${QMUX_ORIGINAL_BASHRC:-}" ] && [ -r "$QMUX_ORIGINAL_BASHRC" ]; then
  . "$QMUX_ORIGINAL_BASHRC"
fi"#
    };
    format!(
        r#"# Generated by qmux. Do not edit.
{user_config}
{qmux_function}
{agent_functions}
if [ -n "${{QMUX_PANE_ID:-}}" ]; then
  __qmux_last_pwd=""
  __qmux_report_cwd() {{
    if [ "$PWD" != "$__qmux_last_pwd" ]; then
      __qmux_last_pwd="$PWD"
      {cli} cwd >/dev/null 2>&1
    fi
  }}
  case "$PROMPT_COMMAND" in
    *__qmux_report_cwd*) ;;
    *) PROMPT_COMMAND="__qmux_report_cwd${{PROMPT_COMMAND:+; $PROMPT_COMMAND}}" ;;
  esac
fi
{startup}"#,
    )
}

/// Installs a one-shot zsh `precmd` hook after the user's configuration has
/// registered its own hooks. Environment managers such as direnv/mise therefore
/// finish preparing the first prompt before a recovered or forked agent starts.
fn zsh_startup_command(startup_command: Option<&str>) -> String {
    let Some(command) = startup_command else {
        return String::new();
    };
    format!(
        r#"__qmux_startup_command() {{
  add-zsh-hook -d precmd __qmux_startup_command 2>/dev/null || true
  unfunction __qmux_startup_command 2>/dev/null || true
  {command}
}}
if autoload -Uz add-zsh-hook 2>/dev/null; then
  add-zsh-hook precmd __qmux_startup_command || __qmux_startup_command
else
  __qmux_startup_command
fi
"#
    )
}

/// Appends a one-shot function to Bash's `PROMPT_COMMAND`, after any user and
/// qmux cwd-reporting hooks. The function removes itself before launching so the
/// agent runs only for the first prompt and the surviving shell remains normal.
fn bash_startup_command(startup_command: Option<&str>) -> String {
    let Some(command) = startup_command else {
        return String::new();
    };
    format!(
        r#"__qmux_startup_command() {{
  case "$PROMPT_COMMAND" in
    "__qmux_startup_command") PROMPT_COMMAND="" ;;
    *"; __qmux_startup_command") PROMPT_COMMAND="${{PROMPT_COMMAND%; __qmux_startup_command}}" ;;
  esac
  unset -f __qmux_startup_command
  {command}
}}
PROMPT_COMMAND="${{PROMPT_COMMAND:+$PROMPT_COMMAND; }}__qmux_startup_command"
"#
    )
}

fn shell_agent_functions(cli: &str, shell_commands: &[ShellCommandIntegration]) -> String {
    shell_commands
        .iter()
        .map(|command| {
            // `agent-exec` supervises the real adapter process and detaches only after
            // wait() observes a true exit. Keeping cleanup out of this shell function is
            // important for job control: a stopped/backgrounded foreground job can hand
            // control back to the shell before it has exited.
            format!(
                "unalias {name} 2>/dev/null || true\n{name}() {{\n  {cli} agent-exec {adapter} \"$@\"\n}}",
                name = command.command_name,
                adapter = command.adapter_id,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Defines `qmux` as a passthrough to the bundled CLI so the user can run subcommands
/// (e.g. `qmux open <file>` to load a file in the browser overlay) from the shell
/// prompt without `qmux` being on PATH — mirroring the injected agent functions.
fn shell_qmux_function(cli: &str) -> String {
    format!("unalias qmux 2>/dev/null || true\nqmux() {{\n  {cli} \"$@\"\n}}")
}

fn original_zdotdir() -> Option<String> {
    env::var("ZDOTDIR").ok().or_else(|| env::var("HOME").ok())
}

fn original_bashrc() -> Option<String> {
    env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".bashrc").display().to_string())
}

fn shell_quote(path: &Path) -> String {
    shell_quote_str(&path.display().to_string())
}

fn shell_quote_str(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn resolved_initial_size(initial_size: Option<InitialPaneSize>) -> InitialPaneSize {
    let size = initial_size.unwrap_or(InitialPaneSize {
        cols: DEFAULT_PTY_COLS,
        rows: DEFAULT_PTY_ROWS,
    });

    InitialPaneSize {
        cols: size.cols.clamp(MIN_INITIAL_COLS, MAX_INITIAL_COLS),
        rows: size.rows.clamp(MIN_INITIAL_ROWS, MAX_INITIAL_ROWS),
    }
}

pub fn spawn_pty(state: &AppState, spec: PtySpawnSpec) -> Result<PaneInfo, String> {
    spawn_portable_pty(state, spec, cfg!(all(target_os = "macos", not(test))))
}

/// The base environment shared by both renderers: the resolved child PATH,
/// 24-bit color capability, and a UTF-8 locale backfill. TERM is added by the
/// host-owned PTY spawn below; using the widely installed xterm-256color entry
/// avoids depending on a separate Ghostty app installation for terminfo.
fn base_child_envs() -> Vec<(String, String)> {
    let mut envs = Vec::new();
    if let Some(path) = crate::launch_path::child_path() {
        envs.push(("PATH".to_string(), path));
    }
    envs.push(("COLORTERM".to_string(), "truecolor".to_string()));
    // Backfill a UTF-8 locale only when one wasn't inherited — a GUI launch
    // gets no LANG, defaulting programs to the C locale and breaking Unicode,
    // while a deliberately-set locale from a dev shell is left untouched.
    if env::var_os("LANG").is_none() {
        envs.push(("LANG".to_string(), "en_US.UTF-8".to_string()));
    }
    envs
}

fn spawn_portable_pty(
    state: &AppState,
    spec: PtySpawnSpec,
    native_surface: bool,
) -> Result<PaneInfo, String> {
    let pane_id = spec.pane_id.unwrap_or_else(|| state.next_id("pane"));
    let initial_size = resolved_initial_size(spec.initial_size);
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: initial_size.rows,
            cols: initial_size.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| format!("failed to open PTY: {err}"))?;

    let mut command = CommandBuilder::new(spec.program);
    command.args(spec.args);
    command.cwd(spec.cwd.clone());
    for (key, value) in base_child_envs() {
        command.env(key, value);
    }
    // Describe the renderer to the child rather than inheriting the outer
    // terminal's TERM. A Finder/Dock-launched app inherits launchd's bare
    // environment with no TERM at all (breaking color), and even when launched
    // from a terminal the inherited TERM names *that* emulator, not this
    // backend. Every real terminal emulator sets this itself for the same
    // reason; the portable renderer is xterm-256color-compatible.
    command.env("TERM", "xterm-256color");
    for (key, value) in spec.envs {
        command.env(key, value);
    }

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|err| format!("failed to clone PTY reader: {err}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|err| format!("failed to open PTY writer: {err}"))?;
    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|err| format!("failed to spawn PTY command: {err}"))?;

    drop(pair.slave);

    let child = Arc::new(Mutex::new(child));
    let master = Arc::new(Mutex::new(pair.master));
    let writer = Arc::new(Mutex::new(writer));
    let backlog: SharedBacklog = Arc::new(Mutex::new(Default::default()));

    let pane = PaneInfo {
        id: pane_id.clone(),
        title: spec.title,
        last_osc_title: spec.last_osc_title,
        kind: spec.kind,
        agent_id: spec.agent_id,
        group_id: spec.group_id,
        cwd: spec.cwd.display().to_string(),
        cols: initial_size.cols,
        rows: initial_size.rows,
        status: PaneStatus::Running,
        // A freshly spawned pane is immediately the group's most-recent, so the next
        // spawn into the group inherits its cwd even before the frontend's activation
        // stamp round-trips. Every real pane (shell and agent) flows through here.
        last_active_at: crate::state::now_millis(),
        recovered: spec.recovered,
        // Real depth is stamped from Model.pane_depth by ordered_panes; the runtime
        // copy is never consulted for it.
        depth: 0,
    };

    let runtime = PaneRuntime {
        info: pane.clone(),
        backend: PaneBackend::HostPty {
            child: child.clone(),
            master,
            writer: writer.clone(),
            backlog: backlog.clone(),
            native_surface,
        },
    };

    state.insert_pane(runtime)?;
    if native_surface {
        if let Err(err) = crate::native_terminal::create_host_managed(&pane_id, Some(&pane.cwd)) {
            let _ = kill_child(&pane_id, child.clone());
            let _ = state.remove_pane(&pane_id);
            return Err(err);
        }
        register_native_input_writer(&pane_id, writer);
    }
    // Capture the direct child's pid for the watcher before handing the child to
    // the reader/watcher threads; the watcher uses it to reach descendants that
    // outlive a naturally-exiting shell.
    let root_pid = child.lock().ok().and_then(|guard| guard.process_id());
    start_reader_thread(
        state.clone(),
        pane_id.clone(),
        reader,
        backlog,
        native_surface,
    );
    start_child_watcher(state.clone(), pane_id, child, root_pid);

    Ok(pane)
}

/// Marks a pane's frontend listener as live and flushes any output buffered
/// before it attached. Called once per pane, after the webview registers its
/// `qmux-event` listener, so the cold-start prompt is never lost to a startup
/// race. The buffered bytes are flushed before `ready` releases the reader to
/// deliver live, preserving output order. For native surfaces the flush also
/// waits for the surface's first real geometry fit (see `DEFERRED_ATTACHES`);
/// a call that arrives earlier returns Ok and is finished later by
/// `complete_pending_attach`.
pub fn attach_pane(state: &AppState, pane_id: String) -> Result<(), String> {
    let native_surface = state.pane_is_native(&pane_id)? == Some(true);
    let backlog = state
        .pane_backlog(&pane_id)?
        .ok_or_else(|| format!("pane {pane_id} was not found"))?;
    let mut backlog = backlog
        .lock()
        .map_err(|_| format!("pane {pane_id} backlog lock poisoned"))?;
    if !backlog.ready {
        if native_surface {
            // Never replay into a surface that still has its pre-layout
            // default grid: the fit after the first real layout would reflow
            // the replayed rows at a different width and scramble restored
            // history. Park the attach instead; the geometry-commit callback
            // finishes it. Register the deferral before probing readiness so
            // a commit landing between the probe and the return still finds
            // this pane parked.
            if let Ok(mut deferred) = DEFERRED_ATTACHES.lock() {
                deferred.insert(pane_id.clone());
            }
            if !crate::native_terminal::is_ready_for_replay(&pane_id)? {
                return Ok(());
            }
            if let Ok(mut deferred) = DEFERRED_ATTACHES.lock() {
                deferred.remove(&pane_id);
            }
            // Replay durable scrollback exactly once. `ready` only flips after
            // the backlog flush below succeeds, so a failed flush makes the
            // frontend retry the whole attach; without the `replayed` guard the
            // retry would hand this history to the surface again and double
            // every restored line.
            if !backlog.replayed {
                let restored = read_pane_scrollback(&state.config().workspace_root, &pane_id)?;
                if restored.is_empty() {
                    backlog.replayed = true;
                } else {
                    let restored = sanitize_scrollback_replay(&restored);
                    if !restored.is_empty() {
                        crate::native_terminal::receive(&pane_id, &restored, true)?;
                    }
                    // History is on the surface now. Flip `replayed` before the
                    // reset and backlog steps so their failure-triggered retries
                    // never render it twice. `receive` is all-or-nothing, so a
                    // failure above left the surface untouched with `replayed`
                    // still false, leaving the retry a clean re-delivery.
                    backlog.replayed = true;
                    crate::native_terminal::receive(
                        &pane_id,
                        RESTORED_SCROLLBACK_TERMINAL_RESET,
                        true,
                    )?;
                }
            }
        }
        if !backlog.buffer.is_empty() {
            let pending = std::mem::take(&mut backlog.buffer);
            if native_surface
                && let Err(err) = crate::native_terminal::receive(&pane_id, &pending, false)
            {
                // Keep startup output available for the attach retry. It
                // has not been recorded yet, so a successful retry cannot
                // duplicate these bytes in durable history.
                backlog.buffer = pending;
                return Err(err);
            }
            // Without a native surface (non-macOS) there is no renderer: the
            // webview dropped the old per-chunk pty.data events unread, so the
            // backlog goes straight to durable scrollback.
            record_scrollback(state, &pane_id, &pending);
        }
        // Only release the reader after every startup byte was accepted. A
        // failed native receive leaves this false so the frontend's attach
        // retry cannot turn a transient surface-creation race into a blank pane.
        backlog.ready = true;
    }
    Ok(())
}

/// Clears terminal modes a program may have left active in a pane that
/// outlives it. A shell-launched agent (`qmux agent-exec codex ...`) that is
/// killed or crashes never restores what its TUI pushed — kitty keyboard
/// flags, mouse/focus reporting, bracketed paste, the alternate screen — and
/// the surviving shell's surface keeps all of it: the replay reset in
/// `attach_pane` only runs when a fresh surface restores scrollback, never
/// for a live one. Stuck kitty flags in particular turn later unclaimed
/// command chords into CSI-u garbage at the prompt instead of inert
/// fall-through.
///
/// The live surface and the durable log get *different* bytes. The surface —
/// where the surviving shell is already about to draw its prompt at the
/// current cursor — receives only `LIVE_PANE_TERMINAL_MODE_RESET`, the
/// cursor-position-neutral subset: sending the full reset's alternate-screen
/// exits (`\x1b[?47l`/`\x1b[?1047l`) into a live surface strands the shell
/// prompt, because those never restore the cursor and, when the agent already
/// exited its alternate screen cleanly (the common Ctrl-C case), needlessly
/// perturb a cursor that was already correct. The durable log still records
/// the *full* `RESTORED_SCROLLBACK_TERMINAL_RESET`: a later restore replays it
/// into a fresh surface (where the cursor is rebuilt regardless), and a trim's
/// sanitizer needs the alternate-screen exit to stop discarding everything the
/// shell prints after a TUI that died mid-alternate-screen. The bytes go to
/// the renderer, never the pty child — they are emulator state, not program
/// input.
pub fn reset_pane_terminal_modes(state: &AppState, pane_id: &str) -> Result<(), String> {
    // A pane that is already gone has no surface or log left to reset.
    let Some(native_surface) = state.pane_is_native(pane_id)? else {
        return Ok(());
    };
    record_scrollback(state, pane_id, RESTORED_SCROLLBACK_TERMINAL_RESET);
    if native_surface {
        crate::native_terminal::receive(pane_id, LIVE_PANE_TERMINAL_MODE_RESET, false)?;
    }
    Ok(())
}

/// Finishes an attach that `attach_pane` parked while the native surface still
/// had its pre-layout default grid. Called from native callbacks (geometry
/// commit, grid resize) that fire on the main thread, so it only touches the
/// deferral set inline; the flush itself runs on a worker because it reads
/// scrollback from disk and hops back to the main thread to hand Ghostty the
/// bytes. If the surface is still not ready, the re-run of `attach_pane`
/// re-parks the pane, so a premature trigger loses nothing.
pub fn complete_pending_attach(state: &AppState, pane_id: &str) {
    let registered = DEFERRED_ATTACHES
        .lock()
        .map(|mut deferred| deferred.remove(pane_id))
        .unwrap_or(false);
    if !registered {
        return;
    }
    let state = state.clone();
    let pane_id = pane_id.to_string();
    std::thread::spawn(move || {
        if let Err(err) = attach_pane(&state, pane_id.clone()) {
            eprintln!("qmux: failed to complete deferred attach for pane {pane_id}: {err}");
        }
    });
}

pub fn write_pane(state: &AppState, options: PaneWriteOptions) -> Result<(), String> {
    write_pane_detailed(state, options).map_err(|failure| failure.error)
}

/// The full-fidelity variant of [`write_pane`]: on failure, reports whether the
/// payload had already reached the pane (see [`PaneWriteFailure`]). Turn sends use
/// this so a submit-leg failure can be retried without re-pasting the text.
pub fn write_pane_detailed(
    state: &AppState,
    options: PaneWriteOptions,
) -> Result<(), PaneWriteFailure> {
    if state
        .research_pane_accepts_input(&options.pane_id)
        .map_err(PaneWriteFailure::before_data)?
        == Some(false)
    {
        return Err(PaneWriteFailure::before_data(
            "research terminals are read-only; create a follow-up branch instead".to_string(),
        ));
    }
    if state
        .pane_is_native(&options.pane_id)
        .map_err(PaneWriteFailure::before_data)?
        == Some(true)
    {
        // Runs on the calling (background) thread; each bridge call hops to
        // the main thread internally (`DispatchQueue.main.sync`) for just the
        // AppKit work. A submit holds the per-pane send lock across those
        // hops plus the 15ms submit-key delay, so this sequence must never
        // run while the main thread can contend for a send lock — a parked
        // main thread would deadlock the holder's main-thread hop. That
        // invariant holds because every path that reaches a send lock is off
        // the main thread: pane_write and all agent turn-queue commands are
        // `(async)` Tauri commands (see main.rs), control-socket and
        // transcript-tail callers run on their own threads, and Ghostty's
        // close delegate defers its queue-draining work to a spawned thread
        // (see qmux_native_terminal_did_close). Keeping the sequence here —
        // instead of the previous hop-to-main-and-block — means a composer
        // send or queued-turn drain no longer stalls the main thread for the
        // duration of the delay.
        return dispatch_native_pane_input(state, &options);
    }
    let writer = state
        .pane_writer(&options.pane_id)
        .map_err(PaneWriteFailure::before_data)?
        .ok_or_else(|| {
            PaneWriteFailure::before_data(format!("pane {} was not found", options.pane_id))
        })?;

    // Write the data (and paste markers) under the writer lock, then release it before
    // the submit-key delay. The bracketed-paste body stays atomic within this first
    // locked section; only the trailing Return is sent in a second short section, so
    // live keystrokes aren't stalled behind the delay.
    write_pane_sequenced(
        state,
        &options,
        |options| {
            let mut writer = writer
                .lock()
                .map_err(|_| format!("pane {} writer lock poisoned", options.pane_id))?;
            write_pane_data(&mut *writer, options)
        },
        || {
            let mut writer = writer
                .lock()
                .map_err(|_| format!("pane {} writer lock poisoned", options.pane_id))?;
            write_pane_submit(&mut *writer)
        },
    )
}

/// Binds the shared native sequencing to the concrete bridge calls for
/// `options.pane_id`.
fn dispatch_native_pane_input(
    state: &AppState,
    options: &PaneWriteOptions,
) -> Result<(), PaneWriteFailure> {
    // Native input takes an extra asynchronous hop through the per-pane PTY writer.
    // A successful Ghostty paste/keypress call therefore only means its bytes were
    // queued, not that the child received them. Put acknowledged writer barriers on
    // both sides of the submit delay: the paste must be written and flushed before the
    // delay starts, and Return must be written and flushed before this turn is reported
    // delivered. The barriers also stop the writer's ordinary keystroke coalescer from
    // merging the paste and Return back into one PTY write.
    if options.submit {
        let send_lock = state.pane_send_lock(&options.pane_id);
        let _send_guard = send_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let data = strip_bracketed_paste_markers(&options.data);
        return write_native_data_and_submit(
            &data,
            || {
                if options.paste {
                    crate::native_terminal::paste_approved_text(&options.pane_id, &data)
                } else {
                    crate::native_terminal::send_text(&options.pane_id, &data)
                }
            },
            || crate::native_terminal::submit(&options.pane_id),
            || flush_native_host_input(&options.pane_id),
            SUBMIT_KEY_DELAY,
            NATIVE_SUBMIT_ACK_RETRY_DELAY,
            NATIVE_SUBMIT_ACK_ATTEMPTS,
        );
    }
    write_native_pane_input(
        state,
        options,
        |data| crate::native_terminal::send_text(&options.pane_id, data),
        |data| crate::native_terminal::paste_approved_text(&options.pane_id, data),
        || crate::native_terminal::submit(&options.pane_id),
    )
}

fn write_native_data_and_submit(
    data: &str,
    emit_data: impl FnOnce() -> Result<(), String>,
    mut submit: impl FnMut() -> Result<(), String>,
    mut flush_input: impl FnMut() -> Result<u64, String>,
    submit_key_delay: Duration,
    submit_retry_delay: Duration,
    submit_attempts: usize,
) -> Result<(), PaneWriteFailure> {
    let before_data = flush_input().map_err(PaneWriteFailure::before_data)?;
    emit_data().map_err(PaneWriteFailure::before_data)?;
    // Once the payload action succeeded, its bytes are queued toward the PTY, so any
    // later failure — a barrier, the submit key — reports the data as delivered.
    let after_data = flush_input().map_err(PaneWriteFailure::after_data)?;
    if !data.is_empty() && after_data <= before_data {
        return Err(PaneWriteFailure::before_data(
            "native terminal accepted input action but emitted no PTY input".to_string(),
        ));
    }

    if !submit_key_delay.is_zero() {
        thread::sleep(submit_key_delay);
    }
    let submit_attempts = submit_attempts.max(1);
    for attempt in 1..=submit_attempts {
        submit().map_err(PaneWriteFailure::after_data)?;
        if flush_input().map_err(PaneWriteFailure::after_data)? > after_data {
            return Ok(());
        }

        // Give a deferred Ghostty input callback a chance to reach the PTY before
        // sending another Return. A second barrier after the pause distinguishes a
        // late acknowledgement from a genuinely missing write and avoids an
        // unnecessary duplicate keypress in the common race.
        if !submit_retry_delay.is_zero() {
            thread::sleep(submit_retry_delay);
        }
        if flush_input().map_err(PaneWriteFailure::after_data)? > after_data {
            return Ok(());
        }

        if attempt == submit_attempts {
            // The paste is already durable in the composer and the Return has been
            // issued; we simply could not observe its bytes reach the PTY within the
            // local window. Do NOT fail here: an error would make the turn queue treat
            // the whole turn as undelivered and requeue it (see send_claimed_turn),
            // re-pasting duplicate text even though the Return usually lands moments
            // later. Report best-effort delivery and let the outstanding-send watchdog
            // (watch_agent_after_queued_send) confirm the submit — and re-send a bare
            // Return, never a re-paste — if it was genuinely dropped.
            eprintln!(
                "qmux: native submit not acknowledged after {submit_attempts} attempts; \
                 treating as delivered and deferring to the submit watchdog"
            );
            return Ok(());
        }
    }
    unreachable!("submit attempt loop always returns")
}

fn write_native_pane_input(
    state: &AppState,
    options: &PaneWriteOptions,
    send_text: impl FnOnce(&str) -> Result<(), String>,
    paste_approved_text: impl FnOnce(&str) -> Result<(), String>,
    submit: impl FnOnce() -> Result<(), String>,
) -> Result<(), PaneWriteFailure> {
    write_pane_sequenced(
        state,
        options,
        |options| write_native_pane_data(options, send_text, paste_approved_text),
        submit,
    )
}

/// Routes native-pane payloads through Ghostty's matching input API. Paste
/// framing is terminal state, not ordinary text: Ghostty must generate it via
/// its approved clipboard action so TUIs interpret the boundary instead of
/// receiving a literal `[200~... [201~` string.
fn write_native_pane_data(
    options: &PaneWriteOptions,
    send_text: impl FnOnce(&str) -> Result<(), String>,
    paste_approved_text: impl FnOnce(&str) -> Result<(), String>,
) -> Result<(), String> {
    if options.paste {
        let data = strip_bracketed_paste_markers(&options.data);
        paste_approved_text(&data)
    } else {
        send_text(&options.data)
    }
}

/// The submit sequencing shared by both pane backends, parameterized over how
/// bytes reach the terminal: emit the payload, then after a short delay the
/// trailing Return, then arm the escape watch.
fn write_pane_sequenced(
    state: &AppState,
    options: &PaneWriteOptions,
    emit_data: impl FnOnce(&PaneWriteOptions) -> Result<(), String>,
    emit_submit: impl FnOnce() -> Result<(), String>,
) -> Result<(), PaneWriteFailure> {
    // A submit is a multi-write sequence — paste body, a short delay, then Return —
    // so two submits racing to the same pane could interleave as `…A……B…\r\r`,
    // merging both turns onto one line and dropping a Return. Hold the per-pane
    // *send* lock across the whole sequence so submits serialize against each
    // other; keystrokes (submit=false) skip it and stay unblocked. Recover from
    // poisoning — the lock guards ordering only. `send_lock` is bound first so it
    // outlives the guard that borrows it.
    let send_lock = options
        .submit
        .then(|| state.pane_send_lock(&options.pane_id));
    let _send_guard = send_lock
        .as_deref()
        .map(|lock| lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));

    emit_data(options).map_err(PaneWriteFailure::before_data)?;

    if options.submit {
        if !SUBMIT_KEY_DELAY.is_zero() {
            thread::sleep(SUBMIT_KEY_DELAY);
        }
        emit_submit().map_err(PaneWriteFailure::after_data)?;
    }

    // A lone Esc keystroke (exactly ESC — arrow keys and other sequences arrive as
    // longer chunks) typed into a working agent's pane is the TUI's interrupt key,
    // and an interrupt during the thinking phase emits no hook and no transcript
    // line. Watch the agent so its Running status can't stick forever.
    if !options.paste && !options.submit && options.data == "\x1b" {
        crate::workspace::watch_agent_after_escape(state, &options.pane_id);
    }

    Ok(())
}

/// Removes embedded bracketed-paste markers from paste-mode payload data.
///
/// The paste boundary must be unforgeable: an embedded `ESC[201~` in the data
/// would terminate the bracketed paste early, so the receiving program (shell,
/// agent TUI) treats everything after it as *typed* input rather than pasted
/// text — turning attacker-controlled paste/turn content into command
/// injection. We strip the end marker (the standard terminal defense) and the
/// start marker too so the framing stays well-formed. Borrows unchanged in the
/// common case where no markers are present.
///
/// Stripping runs to a fixed point: a single non-overlapping `replace` pass can
/// leave a fresh marker behind when the input nests them (e.g. `\x1b[201\x1b[201~~`
/// collapses to a live `\x1b[201~`), so we repeat until no marker remains.
pub(crate) fn strip_bracketed_paste_markers(data: &str) -> Cow<'_, str> {
    if !data.contains("\x1b[200~") && !data.contains("\x1b[201~") {
        return Cow::Borrowed(data);
    }
    let mut cleaned = data.replace("\x1b[200~", "").replace("\x1b[201~", "");
    while cleaned.contains("\x1b[200~") || cleaned.contains("\x1b[201~") {
        cleaned = cleaned.replace("\x1b[200~", "").replace("\x1b[201~", "");
    }
    Cow::Owned(cleaned)
}

fn write_pane_data<W: Write + ?Sized>(
    writer: &mut W,
    options: &PaneWriteOptions,
) -> Result<(), String> {
    if options.paste {
        let data = strip_bracketed_paste_markers(&options.data);
        writer
            .write_all(b"\x1b[200~")
            .map_err(|err| format!("failed to write paste start: {err}"))?;
        writer
            .write_all(data.as_bytes())
            .map_err(|err| format!("failed to write paste data: {err}"))?;
        writer
            .write_all(b"\x1b[201~")
            .map_err(|err| format!("failed to write paste end: {err}"))?;
    } else {
        writer
            .write_all(options.data.as_bytes())
            .map_err(|err| format!("failed to write to pane: {err}"))?;
    }

    writer
        .flush()
        .map_err(|err| format!("failed to flush pane input: {err}"))
}

fn write_pane_submit<W: Write + ?Sized>(writer: &mut W) -> Result<(), String> {
    writer
        .write_all(SUBMIT_KEY)
        .map_err(|err| format!("failed to submit pane input: {err}"))?;
    writer
        .flush()
        .map_err(|err| format!("failed to flush pane submit key: {err}"))
}

/// Composes the data and submit-key writes for tests, mirroring `write_pane`'s
/// sequencing without the per-pane lock handling. `submit_key_delay` lets a test
/// skip the inter-write sleep.
#[cfg(test)]
fn write_pane_input<W: Write + ?Sized>(
    writer: &mut W,
    options: &PaneWriteOptions,
    submit_key_delay: Duration,
) -> Result<(), String> {
    write_pane_data(writer, options)?;
    if options.submit {
        if !submit_key_delay.is_zero() {
            thread::sleep(submit_key_delay);
        }
        write_pane_submit(writer)?;
    }
    Ok(())
}

pub fn resize_pane(state: &AppState, pane_id: String, cols: u16, rows: u16) -> Result<(), String> {
    let master = state
        .pane_master(&pane_id)?
        .ok_or_else(|| format!("pane {pane_id} was not found"))?;
    let master = master
        .lock()
        .map_err(|_| format!("pane {pane_id} master lock poisoned"))?;
    master
        .resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| format!("failed to resize pane {pane_id}: {err}"))?;
    state.update_pane_size(&pane_id, cols, rows)
}

pub fn resize_native_host_pane(
    state: &AppState,
    pane_id: &str,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    if state.pane_has_host_pty(pane_id)? != Some(true) {
        return state.update_pane_size(pane_id, cols, rows);
    }
    resize_pane(state, pane_id.to_string(), cols, rows)
}

pub fn write_native_host_input(
    state: &AppState,
    pane_id: &str,
    bytes: Vec<u8>,
) -> Result<(), String> {
    // Fast path: hand the bytes to the pane's writer thread. Write errors on
    // this path surface asynchronously (logged by the writer thread) — the
    // caller is Ghostty's synchronous input callback, which only logs them
    // anyway.
    let sender = NATIVE_INPUT_SENDERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(pane_id)
        .cloned();
    let bytes = match sender {
        Some(sender) => match sender.send(NativeInputMessage::Data(bytes)) {
            Ok(()) => return Ok(()),
            // The writer thread exited (write failure, teardown race); fall
            // through to the synchronous write so the error surfaces here.
            Err(std::sync::mpsc::SendError(NativeInputMessage::Data(bytes))) => bytes,
            Err(std::sync::mpsc::SendError(NativeInputMessage::Flush(_))) => unreachable!(),
        },
        None => bytes,
    };
    let writer = state
        .pane_writer(pane_id)?
        .ok_or_else(|| format!("pane {pane_id} was not found"))?;
    let mut writer = writer
        .lock()
        .map_err(|_| format!("pane {pane_id} writer lock poisoned"))?;
    writer
        .write_all(&bytes)
        .and_then(|()| writer.flush())
        .map_err(|err| format!("failed to write native pane {pane_id}: {err}"))
}

/// Waits until every native input message queued before this call has reached
/// the PTY and returns the cumulative number of bytes written by this pane's
/// input worker. The position lets programmatic paste/submit callers verify
/// that Ghostty actually emitted bytes for each accepted action.
fn flush_native_host_input(pane_id: &str) -> Result<u64, String> {
    let sender = NATIVE_INPUT_SENDERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(pane_id)
        .cloned()
        .ok_or_else(|| format!("native input writer for pane {pane_id} is unavailable"))?;
    let (acknowledge, result) = std::sync::mpsc::sync_channel(0);
    sender
        .send(NativeInputMessage::Flush(acknowledge))
        .map_err(|_| format!("native input writer for pane {pane_id} stopped before flush"))?;
    result
        .recv_timeout(NATIVE_INPUT_FLUSH_TIMEOUT)
        .map_err(|_| format!("timed out flushing native input for pane {pane_id}"))?
}

/// Registers a native pane's input writer thread, replacing (and thereby
/// shutting down) any stale thread left by a reused pane id.
fn register_native_input_writer(pane_id: &str, writer: SharedWriter) {
    let sender = start_native_input_writer(pane_id.to_string(), writer);
    NATIVE_INPUT_SENDERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(pane_id.to_string(), sender);
}

/// Drops the pane's persistent sender so its writer thread drains what is
/// already queued and exits.
fn remove_native_input_writer(pane_id: &str) {
    NATIVE_INPUT_SENDERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(pane_id);
}

/// One writer thread per native pane, draining queued input into the PTY.
/// Ghostty's input callback stays non-blocking regardless of PTY buffer state,
/// and input ordering is preserved because every native write funnels through
/// this single channel. Exits when the registry's sender is dropped or the
/// PTY write fails (the fallback in `write_native_host_input` then reports
/// subsequent failures synchronously).
fn start_native_input_writer(
    pane_id: String,
    writer: SharedWriter,
) -> std::sync::mpsc::Sender<NativeInputMessage> {
    let (sender, receiver) = std::sync::mpsc::channel::<NativeInputMessage>();
    thread::spawn(move || {
        let mut written_bytes = 0_u64;
        while let Ok(message) = receiver.recv() {
            let (mut pending, mut acknowledge) = match message {
                NativeInputMessage::Data(data) => (data, None),
                NativeInputMessage::Flush(acknowledge) => {
                    let result = writer
                        .lock()
                        .map_err(|_| format!("native pane {pane_id} writer lock poisoned"))
                        .and_then(|mut writer| {
                            writer.flush().map_err(|err| {
                                format!("failed to flush native pane {pane_id}: {err}")
                            })
                        })
                        .map(|()| written_bytes);
                    let failed = result.is_err();
                    let _ = acknowledge.send(result);
                    if failed {
                        return;
                    }
                    continue;
                }
            };
            // Coalesce adjacent data messages, but never cross an acknowledged
            // flush barrier. Programmatic paste/submit uses that barrier to retain
            // its PTY-level separation; ordinary keystroke bursts stay amortized.
            while let Ok(more) = receiver.try_recv() {
                match more {
                    NativeInputMessage::Data(data) => pending.extend_from_slice(&data),
                    NativeInputMessage::Flush(flush) => {
                        acknowledge = Some(flush);
                        break;
                    }
                }
            }
            let result = writer
                .lock()
                .map_err(|_| format!("native pane {pane_id} writer lock poisoned"))
                .and_then(|mut writer| {
                    writer
                        .write_all(&pending)
                        .and_then(|()| writer.flush())
                        .map_err(|err| {
                            format!("failed to write input to native pane {pane_id}: {err}")
                        })
                });
            match result {
                Ok(()) => {
                    written_bytes = written_bytes.saturating_add(pending.len() as u64);
                    if let Some(acknowledge) = acknowledge {
                        let _ = acknowledge.send(Ok(written_bytes));
                    }
                }
                Err(err) => {
                    if let Some(acknowledge) = acknowledge {
                        let _ = acknowledge.send(Err(err.clone()));
                    }
                    eprintln!("qmux: {err}");
                    return;
                }
            }
        }
    });
    sender
}

pub fn pane_activity(state: &AppState, pane_id: String) -> Result<PaneActivity, String> {
    // Validate the pane id against the model before inspecting the child handle. Process
    // inspection below is best-effort, but a genuinely missing pane is still a caller error.
    if !state.list_panes()?.iter().any(|pane| pane.id == pane_id) {
        return Err(format!("pane {pane_id} was not found"));
    }

    let child = state
        .pane_child(&pane_id)?
        .ok_or_else(|| format!("pane {pane_id} was not found"))?;
    let root_pid = {
        let mut child = child
            .lock()
            .map_err(|_| format!("pane {pane_id} child lock poisoned"))?;

        if child
            .try_wait()
            .map_err(|err| format!("failed to inspect pane {pane_id}: {err}"))?
            .is_some()
        {
            return Ok(PaneActivity::idle());
        }

        child.process_id()
    };

    let Some(root_pid) = root_pid else {
        return Ok(PaneActivity::idle());
    };
    // The qmux bridge is implementation plumbing, not user work. Do not make
    // it inflate the close-warning count or trigger a warning on its own.
    let processes = user_running_processes(running_descendant_processes(root_pid));
    if processes.is_empty() {
        Ok(PaneActivity::idle())
    } else {
        Ok(PaneActivity::running_process(
            processes.len(),
            processes.first().map(|process| process.name.clone()),
        ))
    }
}

pub fn kill_pane(state: &AppState, pane_id: String) -> Result<(), String> {
    let native_surface = state.pane_is_native(&pane_id)? == Some(true);
    let child = state
        .pane_child(&pane_id)?
        .ok_or_else(|| format!("pane {pane_id} was not found"))?;
    let pane_agent_id = state.agent_by_pane(&pane_id)?.map(|agent| agent.id);
    if let Err(err) = state.capture_last_closed_pane(&pane_id) {
        eprintln!("qmux: failed to capture closed pane {pane_id}: {err}");
    }
    if let Err(err) = kill_child(&pane_id, child) {
        // The kill couldn't confirm the child dead. If it has since exited — it may
        // have died from the group SIGTERM just after kill_child gave up — reap and
        // reclaim the pane now instead of stranding it in the model. Otherwise leave
        // it in place: the reader thread's EOF path reaps the still-live process when
        // it finally exits, and removing it here would drop the child handle and orphan
        // a zombie.
        let exited = state
            .pane_child(&pane_id)
            .ok()
            .flatten()
            .and_then(|child| {
                child
                    .lock()
                    .ok()
                    .and_then(|mut child| child.try_wait().ok().flatten())
            })
            .is_some();
        if !exited {
            state.clear_last_closed_pane_for_pane(&pane_id);
            return Err(err);
        }
        eprintln!(
            "qmux: kill for pane {pane_id} errored but the child has exited; reclaiming: {err}"
        );
    }
    state.remove_pane(&pane_id)?;
    if native_surface {
        let _ = crate::native_terminal::remove(&pane_id);
        remove_native_input_writer(&pane_id);
    }
    if let Some(agent_id) = pane_agent_id
        && let Err(err) = release_waiters_for_agent(state, &agent_id)
    {
        eprintln!("qmux: failed to release waiters for closed agent {agent_id}: {err}");
    }
    Ok(())
}

pub fn native_pane_did_close(state: &AppState, pane_id: &str, process_alive: bool) {
    if process_alive && let Err(err) = state.settle_research_pane_cancelled(pane_id) {
        eprintln!("qmux: failed to cancel user-closed research pane {pane_id}: {err}");
    }
    // A delegate delivery for a pane no longer in the model (a late or
    // duplicate close) has nothing left to tear down.
    if state.pane_has_host_pty(pane_id).ok().flatten() == Some(true)
        && let Err(err) = kill_pane(state, pane_id.to_string())
    {
        eprintln!("qmux: failed to close host-managed pane {pane_id}: {err}");
    }
}

/// Best-effort teardown of every pane's process tree on app exit.
///
/// Quitting the app just calls `app.exit`, which bypasses the per-pane
/// `kill_pane` path: nothing signals the panes' children, so anything an agent
/// left running that survives the PTY hangup — dev servers, MCP/language
/// servers, `setsid`/disowned jobs — is reparented to launchd and leaks across
/// every quit. This runs the same process-group signal + descendant walk as
/// closing a pane on each live pane, skipping the model/undo bookkeeping since
/// the process is about to exit anyway. It cannot help a hard SIGKILL/force-quit,
/// which no in-process handler can intercept.
pub fn kill_all_panes(state: &AppState) {
    let children = match state.all_pane_children() {
        Ok(children) => children,
        Err(err) => {
            eprintln!("qmux: failed to enumerate panes for exit cleanup: {err}");
            return;
        }
    };
    for (pane_id, child) in children {
        if let Err(err) = kill_child(&pane_id, child) {
            eprintln!("qmux: failed to kill pane {pane_id} on exit: {err}");
        }
        // The reader-thread EOF path that normally removes these dirs won't run once
        // the process is exiting, so clean them up here instead of leaking them into
        // /tmp until the OS clears it.
        remove_shell_integration_dir(&pane_id);
    }
}

pub fn close_worktree_pane(
    state: &AppState,
    agent_id: &str,
    delete_worktree: bool,
) -> Result<(), String> {
    let agent = state
        .agent(agent_id)?
        .ok_or_else(|| format!("agent {agent_id} was not found"))?;
    let pane_id = agent
        .pane_id
        .clone()
        .ok_or_else(|| format!("agent {agent_id} has no pane to close"))?;
    let worktree_removal = if delete_worktree {
        Some(capture_agent_worktree_removal(state, &agent)?)
    } else {
        None
    };

    kill_pane(state, pane_id)?;

    if let Some(removal) = worktree_removal {
        remove_captured_worktree(removal)?;
        state.clear_last_closed_pane_for_agent(agent_id);
    }

    Ok(())
}

#[cfg_attr(all(target_os = "macos", not(test)), allow(dead_code))]
fn start_reader_thread(
    state: AppState,
    pane_id: String,
    mut reader: Box<dyn Read + Send>,
    backlog: SharedBacklog,
    native_surface: bool,
) {
    thread::spawn(move || {
        // 64KB per read: every chunk pays fixed costs beyond the syscall — the
        // durable scrollback append and, for native surfaces, the FFI handoff
        // with its buffer copy — so at the old 8KB a bulk producer (builds,
        // `cat` of a large file) paid that overhead 8x as often. Heap-allocated
        // to keep the reader thread's stack frame small.
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let chunk = &buffer[..count];
                    // Hold the backlog lock only long enough to decide; emitting
                    // live happens after releasing it. `attach_pane` flips `ready`
                    // (and drains the buffer) under the same lock, so no chunk is
                    // ever both buffered and emitted, and order is preserved.
                    let live = match backlog.lock() {
                        Ok(mut backlog) => {
                            if backlog.ready {
                                true
                            } else {
                                append_capped(&mut backlog.buffer, chunk);
                                false
                            }
                        }
                        Err(_) => true,
                    };
                    if live {
                        // Hand the surface its bytes before touching disk: the
                        // durable append (and its occasional multi-MB trim) is
                        // recovery bookkeeping, and running it first put disk
                        // latency in front of every rendered chunk — including
                        // keystroke echo. Without a native surface (non-macOS)
                        // there is no renderer — the webview dropped the old
                        // per-chunk pty.data events unread — so output is only
                        // recorded.
                        if native_surface
                            && let Err(err) =
                                crate::native_terminal::receive(&pane_id, chunk, false)
                        {
                            eprintln!(
                                "qmux: failed to render output for native pane {pane_id}: {err}"
                            );
                        }
                        record_scrollback(&state, &pane_id, chunk);
                    }
                }
                Err(err) => {
                    state.emit(QmuxEvent::new(
                        "pty.read_error",
                        Some(pane_id.clone()),
                        None,
                        serde_json::json!({ "error": err.to_string() }),
                    ));
                    break;
                }
            }
        }
        // The PTY hit EOF, so the child has exited (or is about to). Reap it before
        // dropping the handle so it does not linger as a zombie occupying a PID slot
        // for the life of the qmux process, and report its real exit code rather
        // than a blanket `None`. A pane killed via `kill_pane` is already reaped and
        // removed there, so this returns None and emits the exit with no code.
        let exit_code = reap_pane_child(&state, &pane_id);
        let pane_agent_id = state
            .agent_by_pane(&pane_id)
            .ok()
            .flatten()
            .map(|agent| agent.id);
        // A natural exit normally leaves no undo snapshot (unlike `kill_pane`), but if this
        // is the group's last pane and the group still has queued turns, removing it would
        // prune that pending work with no way back. Capture a close snapshot first so it
        // can be reopened, matching the explicit-close path.
        if state
            .closing_pane_would_strand_queued_work(&pane_id)
            .unwrap_or(false)
            && let Err(err) = state.capture_last_closed_pane(&pane_id)
        {
            eprintln!("qmux: failed to capture exited pane {pane_id}: {err}");
        }
        if let Err(err) = state.remove_pane(&pane_id) {
            // A failure here (e.g. a poisoned model lock) leaves a dead pane in
            // state; log it so the stale entry has a trace rather than vanishing.
            eprintln!("qmux: failed to remove exited pane {pane_id}: {err}");
        }
        if native_surface {
            let _ = crate::native_terminal::remove(&pane_id);
            remove_native_input_writer(&pane_id);
        }
        if let Some(agent_id) = pane_agent_id
            && let Err(err) = release_waiters_for_agent(&state, &agent_id)
        {
            eprintln!("qmux: failed to release waiters for exited agent {agent_id}: {err}");
        }
        remove_shell_integration_dir(&pane_id);
        state.emit(QmuxEvent::pty_exit(pane_id, exit_code));
    });
}

/// Watches a pane's direct child so a pane whose shell exits while a backgrounded
/// descendant still holds the PTY slave open is torn down instead of hanging.
///
/// The reader thread only unblocks (and runs teardown) on PTY EOF, which never
/// arrives while any descendant keeps a slave fd open. Left alone, such a pane
/// leaks its reader thread, leaves the exited shell as a zombie, and stays stuck
/// "Running" in the UI. This watcher notices the direct child has exited and
/// forces the surviving descendants down so the slave closes, the reader hits
/// EOF, and the existing per-pane cleanup runs.
///
/// A backgrounded job gets its own process group and is reparented off the shell
/// the instant the shell exits, so after the fact neither the shell's process
/// group nor a live ppid walk can find it. We therefore keep a recent snapshot of
/// the descendant pids (refreshed while the child is alive) and signal that
/// snapshot — plus the process group, for anything still in it — on exit. A job
/// spawned and orphaned within a single refresh window can still be missed, which
/// leaves the same state as before this watcher existed for that one narrow case.
#[cfg_attr(all(target_os = "macos", not(test)), allow(dead_code))]
fn start_child_watcher(
    state: AppState,
    pane_id: String,
    child: SharedChild,
    root_pid: Option<u32>,
) {
    let Some(root_pid) = root_pid else {
        return;
    };
    thread::spawn(move || {
        let mut descendants = watcher_descendant_process_ids(root_pid);
        let mut tick: u32 = 0;
        loop {
            thread::sleep(CHILD_WATCH_INTERVAL);
            // The pane's child Arc is the liveness handle: once `kill_pane` or the
            // reader's EOF cleanup removes the pane — or a respawn replaces it with
            // a fresh child under a reused id — this watcher has nothing left to do.
            match state.pane_child(&pane_id) {
                Ok(Some(current)) if Arc::ptr_eq(&current, &child) => {}
                _ => break,
            }
            let exited = {
                let Ok(mut guard) = child.lock() else {
                    break;
                };
                match guard.try_wait() {
                    Ok(status) => status.is_some(),
                    Err(_) => break,
                }
            };
            if !exited {
                tick = tick.wrapping_add(1);
                if tick.is_multiple_of(DESCENDANT_REFRESH_TICKS) {
                    descendants = watcher_descendant_process_ids(root_pid);
                }
                continue;
            }
            // Direct child gone but the pane is still present: a descendant is
            // holding the PTY slave open and the reader is blocked on read(). Force
            // the tree down (best-effort) so the slave closes, the reader hits EOF,
            // and the normal cleanup runs.
            let _ = unsafe { libc::kill(-(root_pid as libc::pid_t), libc::SIGTERM) };
            for pid in &descendants {
                let _ = unsafe { libc::kill(*pid as libc::pid_t, libc::SIGTERM) };
            }
            // A descendant that ignores SIGTERM (a hung agent, an uninterruptible
            // helper) keeps the slave open and the dead pane lingering as
            // "Running" indefinitely. Give the tree a couple of intervals to
            // unwind, then escalate to SIGKILL. The pane check is the same
            // liveness handle as above: if the reader's EOF cleanup already ran,
            // there is nothing left to escalate against.
            for _ in 0..KILL_ESCALATION_TICKS {
                thread::sleep(CHILD_WATCH_INTERVAL);
                match state.pane_child(&pane_id) {
                    Ok(Some(current)) if Arc::ptr_eq(&current, &child) => {}
                    _ => return,
                }
            }
            let _ = unsafe { libc::kill(-(root_pid as libc::pid_t), libc::SIGKILL) };
            for pid in &descendants {
                let _ = unsafe { libc::kill(*pid as libc::pid_t, libc::SIGKILL) };
            }
            break;
        }
    });
}

/// Waits on a pane's child so the exited process is reaped (no zombie) and returns
/// its exit code. Best-effort: a pane already removed (e.g. by `kill_pane`) or a
/// poisoned child lock yields `None`.
#[cfg_attr(all(target_os = "macos", not(test)), allow(dead_code))]
fn reap_pane_child(state: &AppState, pane_id: &str) -> Option<i32> {
    let child = state.pane_child(pane_id).ok().flatten()?;
    let mut child = child.lock().ok()?;
    child.wait().ok().map(|status| status.exit_code() as i32)
}

/// How far below `BACKLOG_CAP` an over-cap backlog is trimmed. Draining the
/// front of the buffer is an O(len) memmove, and trimming to the cap exactly
/// re-ran it on every subsequent chunk of a saturated backlog — a multi-MB
/// memmove per PTY read. The slack amortizes that to one memmove per
/// `BACKLOG_TRIM_SLACK` bytes of overflow, at the cost of a saturated backlog
/// retaining slightly less than the cap.
#[cfg_attr(all(target_os = "macos", not(test)), allow(dead_code))]
const BACKLOG_TRIM_SLACK: usize = BACKLOG_CAP / 8;

/// Appends to the pre-attach backlog, dropping the oldest bytes once it exceeds
/// the cap so a runaway pre-attach burst can't grow unbounded.
#[cfg_attr(all(target_os = "macos", not(test)), allow(dead_code))]
fn append_capped(buffer: &mut Vec<u8>, chunk: &[u8]) {
    buffer.extend_from_slice(chunk);
    if buffer.len() > BACKLOG_CAP {
        let overflow = buffer.len() - (BACKLOG_CAP - BACKLOG_TRIM_SLACK);
        buffer.drain(..overflow);
    }
}

fn record_scrollback(state: &AppState, pane_id: &str, chunk: &[u8]) {
    if let Err(err) = append_pane_scrollback(&state.config().workspace_root, pane_id, chunk) {
        eprintln!("qmux: failed to record scrollback for pane {pane_id}: {err}");
    }
}

fn kill_child(pane_id: &str, child: SharedChild) -> Result<(), String> {
    let mut child = child
        .lock()
        .map_err(|_| format!("pane {pane_id} child lock poisoned"))?;

    if child
        .try_wait()
        .map_err(|err| format!("failed to inspect pane {pane_id}: {err}"))?
        .is_some()
    {
        return Ok(());
    }

    if let Some(pid) = child.process_id() {
        // Signal the whole process group first. The group id is the session
        // leader's pid, which we still hold open via `child`, so it can't be
        // recycled out from under us — unlike the individual descendant pids
        // below. Delivering the group signal up front also begins tearing the tree
        // down before we enumerate it, shrinking the window in which an enumerated
        // descendant could exit and have its pid reused by an unrelated process
        // before we signal it.
        let _ = unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGTERM) };
        // Best-effort backstop for descendants that escaped the group (e.g. via
        // setsid). This walks live pids, so it is inherently subject to pid reuse
        // and is intentionally secondary to the group signal above.
        terminate_descendants(pid);
    }

    match child.kill() {
        Ok(()) => {
            // Reap the just-killed child while we still hold its handle, so it does
            // not become a zombie. The kill above signals it; wait collects it.
            let _ = child.wait();
            Ok(())
        }
        Err(err) => {
            if child
                .try_wait()
                .map_err(|wait_err| format!("failed to inspect pane {pane_id}: {wait_err}"))?
                .is_some()
            {
                Ok(())
            } else {
                Err(format!("failed to kill pane {pane_id}: {err}"))
            }
        }
    }
}

fn terminate_descendants(pid: u32) {
    // Reversing the pre-order walk signals every process before its parent
    // (deepest first), preserving the old recursive kill order without a
    // subprocess per descendant.
    for child_pid in descendant_process_ids(pid).into_iter().rev() {
        let _ = unsafe { libc::kill(child_pid as libc::pid_t, libc::SIGTERM) };
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RunningProcess {
    name: String,
}

fn running_descendant_processes(pid: u32) -> Vec<RunningProcess> {
    running_processes(&descendant_process_ids(pid))
}

fn user_running_processes(processes: Vec<RunningProcess>) -> Vec<RunningProcess> {
    processes
        .into_iter()
        .filter(|process| !process.name.eq_ignore_ascii_case("qmux"))
        .collect()
}

/// Filters `pids` down to live, non-zombie processes with a single `ps`
/// invocation — one subprocess total instead of one per pid — returning each
/// one's executable basename. When a requested pid is already gone, `ps`
/// still prints rows for the live ones but its exit status is
/// platform-dependent, so stdout is parsed regardless of exit status.
fn running_processes(pids: &[u32]) -> Vec<RunningProcess> {
    if pids.is_empty() {
        return Vec::new();
    }
    let pid_list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let Ok(output) = Command::new("/bin/ps")
        .arg("-p")
        .arg(pid_list)
        .arg("-o")
        .arg("stat=")
        .arg("-o")
        .arg("comm=")
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(running_process_from_line)
        .collect()
}

/// Walks a process's live descendants from a single `ps` snapshot. The previous
/// implementation forked one `pgrep -P` per tree node, so inspecting a shell
/// running a dev server with a handful of children cost a fork/exec (~5-15ms on
/// macOS) per process — every pane close and watcher refresh paid tens to
/// hundreds of milliseconds. One `ps` is a single subprocess regardless of tree
/// size.
fn descendant_process_ids(pid: u32) -> Vec<u32> {
    descendants_from_parent_pairs(pid, &process_parent_snapshot())
}

/// How stale the shared process-table snapshot may be for pane-watcher
/// refreshes. Watcher refreshes track long-lived descendants (dev servers,
/// backgrounded jobs), so a snapshot a few seconds old is as good as a fresh
/// one — while the kill/close paths keep forking their own fresh `ps`, since
/// they act on what they see.
const WATCHER_SNAPSHOT_MAX_AGE: Duration = Duration::from_secs(5);

/// A process-table snapshot (every live process's pid and ppid) plus when it
/// was taken.
type TimestampedProcessSnapshot = (std::time::Instant, Arc<Vec<(u32, u32)>>);

/// The most recent shared snapshot, timestamped. Holding the lock across the
/// `ps` fork is deliberate: concurrent watcher refreshes then wait for one
/// snapshot instead of racing to fork their own.
static WATCHER_SNAPSHOT: std::sync::LazyLock<Mutex<Option<TimestampedProcessSnapshot>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

/// `descendant_process_ids` for pane watchers: resolves against a briefly
/// cached process-table snapshot so N panes' watchers cost at most one `ps`
/// fork per cache window between them, instead of one fork per pane per
/// refresh tick.
fn watcher_descendant_process_ids(pid: u32) -> Vec<u32> {
    descendants_from_parent_pairs(pid, &shared_process_parent_snapshot())
}

fn shared_process_parent_snapshot() -> Arc<Vec<(u32, u32)>> {
    let mut cache = WATCHER_SNAPSHOT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((taken_at, snapshot)) = cache.as_ref()
        && taken_at.elapsed() < WATCHER_SNAPSHOT_MAX_AGE
    {
        return snapshot.clone();
    }
    let snapshot = Arc::new(process_parent_snapshot());
    *cache = Some((std::time::Instant::now(), snapshot.clone()));
    snapshot
}

/// Every live process's (pid, ppid), from one `ps` invocation.
fn process_parent_snapshot() -> Vec<(u32, u32)> {
    let Ok(output) = Command::new("/bin/ps")
        .arg("-axo")
        .arg("pid=,ppid=")
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse::<u32>().ok()?;
            let ppid = parts.next()?.parse::<u32>().ok()?;
            Some((pid, ppid))
        })
        .collect()
}

/// Pre-order walk (each process before its own descendants) so callers can
/// reverse the list for a deepest-first teardown. The `seen` guard keeps a
/// cyclic snapshot — possible if a pid was recycled mid-`ps` — from looping.
fn descendants_from_parent_pairs(root: u32, parent_pairs: &[(u32, u32)]) -> Vec<u32> {
    let mut children_by_parent: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, ppid) in parent_pairs {
        children_by_parent.entry(*ppid).or_default().push(*pid);
    }
    let mut descendants = Vec::new();
    let mut seen = HashSet::from([root]);
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        let Some(children) = children_by_parent.get(&pid) else {
            continue;
        };
        for child_pid in children {
            if seen.insert(*child_pid) {
                descendants.push(*child_pid);
                stack.push(*child_pid);
            }
        }
    }
    descendants
}

fn running_process_from_line(line: &str) -> Option<RunningProcess> {
    let mut parts = line.split_whitespace();
    let status = parts.next()?;
    if status.starts_with('Z') {
        return None;
    }
    let command = parts.collect::<Vec<_>>().join(" ");
    let name = Path::new(&command)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(command.trim())
        .trim()
        .to_string();
    if name.is_empty() {
        return None;
    }

    Some(RunningProcess { name })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AdapterConfigs, ClaudeAdapterConfig, CodexAdapterConfig, GrokAdapterConfig,
        OpencodeAdapterConfig, QmuxConfig,
    };
    use crate::scrollback::read_pane_scrollback;
    use crate::workspace::{AgentInfo, AgentStatus, GroupInfo, WorkspaceScope};
    use std::cell::{Cell, RefCell};
    use std::io;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn windows_contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    fn passwd_login_shell_resolves_for_current_user() {
        let shell = passwd_login_shell().expect("current user should have a passwd entry");
        assert!(shell.starts_with('/'), "expected an absolute path: {shell}");
    }

    #[test]
    fn pane_shell_is_never_empty() {
        assert!(!pane_shell().is_empty());
    }

    #[test]
    fn restored_scrollback_reset_clears_kitty_keyboard_flags() {
        // ... and xterm modifyOtherKeys, the other latched key-encoding mode.
        assert!(RESTORED_SCROLLBACK_TERMINAL_RESET.ends_with(b"\x1b[>4;0m\x1b[=0u"));
    }

    #[test]
    fn restored_scrollback_reset_restores_numeric_keypad() {
        // DECKPNM, undoing a DECKPAM (ESC =) a dead TUI left latched. The
        // replay sanitizer also strips both from history; this is the same
        // defense in depth the kitty reset above provides.
        assert!(RESTORED_SCROLLBACK_TERMINAL_RESET.starts_with(b"\x18\x1b>"));
    }

    #[test]
    fn live_pane_reset_clears_input_modes_without_moving_the_cursor() {
        // The live-surface reset still latches off the key-encoding modes that
        // otherwise garble a surviving shell's prompt (Kitty flags and xterm
        // modifyOtherKeys), and still turns the cursor back on.
        assert!(LIVE_PANE_TERMINAL_MODE_RESET.ends_with(b"\x1b[>4;0m\x1b[=0u"));
        assert!(windows_contains(
            LIVE_PANE_TERMINAL_MODE_RESET,
            b"\x1b[?25h"
        ));

        // But it must never move the cursor or swap the screen buffer on a
        // live surface the shell is about to draw its prompt into: no leading
        // CAN and no alternate-screen exits. Those stay in the full reset,
        // which only ever rebuilds a fresh surface or is recorded for trims.
        assert!(!LIVE_PANE_TERMINAL_MODE_RESET.starts_with(b"\x18"));
        assert!(!windows_contains(
            LIVE_PANE_TERMINAL_MODE_RESET,
            b"\x1b[?47l"
        ));
        assert!(!windows_contains(
            LIVE_PANE_TERMINAL_MODE_RESET,
            b"\x1b[?1047l"
        ));
        assert!(!windows_contains(
            LIVE_PANE_TERMINAL_MODE_RESET,
            b"\x1b[?1049l"
        ));
        // The full reset keeps them — the two are otherwise the same reset.
        assert!(windows_contains(
            RESTORED_SCROLLBACK_TERMINAL_RESET,
            b"\x1b[?1047l"
        ));
    }

    #[test]
    fn reset_pane_terminal_modes_records_the_reset_for_live_panes_only() {
        let workspace = temp_workspace();
        let state = test_state_with_workspace(workspace.clone());
        let pane = spawn_test_pty(
            &state,
            "pane-mode-reset",
            vec!["-c".to_string(), "sleep 30".to_string()],
        );

        // The pane was never attached, so the reader thread is still buffering
        // (pre-attach output is not recorded): the reset is the only scrollback
        // writer here and the log contents are exact, not racy.
        reset_pane_terminal_modes(&state, &pane.id).unwrap();
        assert_eq!(
            read_pane_scrollback(&workspace, &pane.id).unwrap(),
            RESTORED_SCROLLBACK_TERMINAL_RESET
        );

        // A pane that no longer exists is a quiet no-op — the detach that
        // triggers the reset can race pane teardown — and must not mint a
        // scrollback log for the dead pane id.
        reset_pane_terminal_modes(&state, "pane-gone").unwrap();
        assert!(
            read_pane_scrollback(&workspace, "pane-gone")
                .unwrap()
                .is_empty()
        );

        kill_pane(&state, pane.id).expect("cleanup test pane");
    }

    #[derive(Default)]
    struct RecordingWriter {
        bytes: Vec<u8>,
        flush_offsets: Vec<usize>,
    }

    impl Write for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flush_offsets.push(self.bytes.len());
            Ok(())
        }
    }

    fn write_options(data: &str, paste: bool, submit: bool) -> PaneWriteOptions {
        PaneWriteOptions {
            pane_id: "pane-1".to_string(),
            data: data.to_string(),
            paste,
            submit,
        }
    }

    /// A `Write` sink whose bytes are observable from the test thread while the
    /// pane's writer thread drains into it.
    struct SharedSink(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn native_input_writer_drains_in_order_off_the_calling_thread() {
        let pane_id = "pane-native-input-order";
        let sink = Arc::new(Mutex::new(Vec::new()));
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(SharedSink(sink.clone()))));
        register_native_input_writer(pane_id, writer);
        let state = test_state();

        // The registered fast path must accept both writes without consulting
        // pane state (no pane exists in this test AppState).
        write_native_host_input(&state, pane_id, b"hello ".to_vec()).unwrap();
        assert_eq!(flush_native_host_input(pane_id).unwrap(), 6);
        write_native_host_input(&state, pane_id, b"world".to_vec()).unwrap();
        assert_eq!(flush_native_host_input(pane_id).unwrap(), 11);

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while sink.lock().unwrap().len() < 11 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(*sink.lock().unwrap(), b"hello world");

        // Once the registration is gone, the fallback path reports the missing
        // pane synchronously instead of silently dropping input.
        remove_native_input_writer(pane_id);
        assert!(write_native_host_input(&state, pane_id, b"late".to_vec()).is_err());
    }

    fn test_state() -> AppState {
        AppState::new(QmuxConfig {
            workspace_root: PathBuf::from("/tmp/qmux-workspaces"),
            socket_path: PathBuf::from("/tmp/qmux.sock"),
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

    fn test_state_with_workspace(workspace_root: PathBuf) -> AppState {
        AppState::new(QmuxConfig {
            workspace_root,
            socket_path: PathBuf::from("/tmp/qmux.sock"),
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

    fn temp_workspace() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("qmux-pty-scrollback-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn env_value(envs: &[(String, String)], key: &str) -> Option<String> {
        envs.iter()
            .find_map(|(env_key, value)| (env_key == key).then(|| value.clone()))
    }

    #[test]
    fn shell_kind_detects_supported_shells_by_basename() {
        assert_eq!(shell_kind("/bin/zsh"), ShellKind::Zsh);
        assert_eq!(shell_kind("/opt/homebrew/bin/bash"), ShellKind::Bash);
        assert_eq!(shell_kind("/opt/homebrew/bin/fish"), ShellKind::Unsupported);
        assert!(ensure_shell_agent_startup_supported_for("/bin/zsh").is_ok());
        assert!(
            ensure_shell_agent_startup_supported_for("/opt/homebrew/bin/fish")
                .unwrap_err()
                .contains("unsupported")
        );
    }

    #[test]
    fn init_scripts_define_agent_functions_through_qmux() {
        let qmux_cli = PathBuf::from("/Applications/qmux app/qmux");
        let shell_commands = [
            ShellCommandIntegration {
                command_name: "codex",
                adapter_id: "codex",
            },
            ShellCommandIntegration {
                command_name: "claude",
                adapter_id: "claude",
            },
        ];

        let zsh_script = zsh_init_script(&qmux_cli, &shell_commands, None, true);
        let bash_script = bash_init_script(&qmux_cli, &shell_commands, None, true);

        for script in [zsh_script, bash_script] {
            assert!(script.contains("codex() {"));
            assert!(script.contains("'/Applications/qmux app/qmux' agent-exec codex \"$@\""));
            assert!(script.contains("unalias codex"));
            assert!(script.contains("claude() {"));
            assert!(script.contains("'/Applications/qmux app/qmux' agent-exec claude \"$@\""));
            assert!(script.contains("unalias claude"));
            // Detach is handled by agent-exec after the adapter process truly exits.
            // The shell wrapper must not detach after job-control stop/background.
            assert!(!script.contains("agent-detach"));
            assert!(!script.contains("local __qmux_status=$?"));
            assert!(!script.contains("return $__qmux_status"));
            // `qmux` itself is a passthrough so `qmux open <file>` works at the prompt
            // without qmux being on PATH.
            assert!(script.contains("unalias qmux"));
            assert!(script.contains("qmux() {"));
            assert!(script.contains("'/Applications/qmux app/qmux' \"$@\""));
            // Shell integration reports cwd changes so restarts reopen the last dir.
            assert!(script.contains("'/Applications/qmux app/qmux' cwd"));
            assert!(script.contains("__qmux_report_cwd"));
            // No resume requested: the script must not auto-run an agent on startup.
            assert!(!script.contains("--resume"));
        }
    }

    #[test]
    fn zsh_init_script_resets_histfile_left_pointing_at_integration_dir() {
        let qmux_cli = PathBuf::from("/Applications/qmux app/qmux");

        let script = zsh_init_script(&qmux_cli, &[], None, false);

        // macOS's /etc/zshrc sets HISTFILE from ZDOTDIR before our rc runs, so a
        // pane would otherwise read/write history in the deleted-on-close scratch
        // dir. The reset must happen before the user's .zshrc is sourced so a
        // user-set HISTFILE still wins.
        assert!(script.contains(r#"case "${HISTFILE:-}" in"#));
        assert!(script.contains(r#""$__qmux_zdotdir"/*) HISTFILE="$ZDOTDIR/.zsh_history" ;;"#));
        let reset_pos = script.find("HISTFILE=\"$ZDOTDIR/.zsh_history\"").unwrap();
        let source_pos = script.find(r#"source "$ZDOTDIR/.zshrc""#).unwrap();
        assert!(reset_pos < source_pos);
    }

    #[test]
    fn shell_agent_exec_command_quotes_every_dynamic_argument() {
        let command = shell_agent_exec_command(
            Path::new("/Applications/qmux app/qmux"),
            "codex",
            &[
                "fork".to_string(),
                "sess'1".to_string(),
                "line one\nline two".to_string(),
            ],
            "agent-42",
        );

        assert_eq!(
            command,
            "QMUX_PREPARED_AGENT_ID='agent-42' '/Applications/qmux app/qmux' agent-exec 'codex' 'fork' 'sess'\\''1' 'line one\nline two'"
        );
    }

    #[test]
    fn init_scripts_run_startup_command_from_one_shot_prompt_hooks() {
        let qmux_cli = PathBuf::from("/Applications/qmux app/qmux");
        let shell_commands = [ShellCommandIntegration {
            command_name: "claude",
            adapter_id: "claude",
        }];
        let resume = "claude --resume 'sess-1'";

        let zsh_script = zsh_init_script(&qmux_cli, &shell_commands, Some(resume), true);
        let bash_script = bash_init_script(&qmux_cli, &shell_commands, Some(resume), true);

        assert!(zsh_script.contains("claude() {"));
        assert!(zsh_script.contains("__qmux_startup_command() {"));
        assert!(zsh_script.contains("add-zsh-hook precmd __qmux_startup_command"));
        assert!(zsh_script.contains(resume));

        assert!(bash_script.contains("claude() {"));
        assert!(bash_script.contains("__qmux_startup_command() {"));
        assert!(bash_script.contains(
            r#"PROMPT_COMMAND="${PROMPT_COMMAND:+$PROMPT_COMMAND; }__qmux_startup_command""#
        ));
        assert!(bash_script.contains(resume));

        for script in [zsh_script, bash_script] {
            let user_rc = script.find("source \"$ZDOTDIR/.zshrc\"").or_else(|| {
                script
                    .find(". \"$__qmux_login_rc\"")
                    .or_else(|| script.find(". \"$QMUX_ORIGINAL_BASHRC\""))
            });
            let startup_hook = script.find("__qmux_startup_command() {").unwrap();
            assert!(user_rc.is_some_and(|user_rc| user_rc < startup_hook));
        }
    }

    #[test]
    fn init_scripts_source_login_files_only_in_login_mode() {
        let qmux_cli = PathBuf::from("/Applications/qmux app/qmux");
        let shell_commands = [ShellCommandIntegration {
            command_name: "claude",
            adapter_id: "claude",
        }];

        // Login zsh sources .zprofile and .zlogin around the always-sourced .zshrc;
        // a non-login shell sources only .zshrc.
        let zsh_login = zsh_init_script(&qmux_cli, &shell_commands, None, true);
        assert!(zsh_login.contains("source \"$ZDOTDIR/.zprofile\""));
        assert!(zsh_login.contains("source \"$ZDOTDIR/.zshrc\""));
        assert!(zsh_login.contains("source \"$ZDOTDIR/.zlogin\""));

        let zsh_plain = zsh_init_script(&qmux_cli, &shell_commands, None, false);
        assert!(zsh_plain.contains("source \"$ZDOTDIR/.zshrc\""));
        assert!(!zsh_plain.contains(".zprofile"));
        assert!(!zsh_plain.contains(".zlogin"));

        // Login bash reproduces bash's own login-file lookup (which conventionally
        // pulls in .bashrc); a non-login shell sources the captured .bashrc directly.
        let bash_login = bash_init_script(&qmux_cli, &shell_commands, None, true);
        assert!(bash_login.contains("$HOME/.bash_profile"));
        assert!(bash_login.contains("$HOME/.bash_login"));
        assert!(bash_login.contains("$HOME/.profile"));
        assert!(!bash_login.contains("QMUX_ORIGINAL_BASHRC"));

        let bash_plain = bash_init_script(&qmux_cli, &shell_commands, None, false);
        assert!(bash_plain.contains("QMUX_ORIGINAL_BASHRC"));
        assert!(!bash_plain.contains(".bash_profile"));
    }

    #[test]
    fn base_qmux_envs_include_pane_socket_token_and_workspace() {
        let state = test_state();
        let envs = qmux_pane_envs(&state, "pane-123").expect("envs mint a token");

        assert_eq!(
            env_value(&envs, "QMUX_PANE_ID"),
            Some("pane-123".to_string())
        );
        assert_eq!(
            env_value(&envs, "QMUX_SOCK"),
            Some("/tmp/qmux.sock".to_string())
        );
        let token = env_value(&envs, "QMUX_TOKEN").expect("pane token env is present");
        assert_eq!(token, state.pane_token("pane-123").unwrap());
        assert_eq!(token.len(), 64);
        assert_ne!(
            state.pane_token("pane-123").unwrap(),
            state.pane_token("other-pane").unwrap()
        );
        assert_eq!(
            env_value(&envs, "QMUX_WORKSPACE_ROOT"),
            Some("/tmp/qmux-workspaces".to_string())
        );
    }

    #[test]
    fn shell_pane_envs_enable_shell_integration() {
        let state = test_state();
        let envs = shell_pane_envs(&state, "pane-123").expect("envs mint a token");

        assert_eq!(
            env_value(&envs, "QMUX_SHELL_INTEGRATION"),
            Some("1".to_string())
        );
        assert!(env_value(&envs, "QMUX_AGENT_ID").is_none());
    }

    #[test]
    fn initial_pty_size_defaults_to_legacy_geometry() {
        assert_eq!(
            resolved_initial_size(None),
            InitialPaneSize {
                cols: DEFAULT_PTY_COLS,
                rows: DEFAULT_PTY_ROWS
            }
        );
    }

    #[test]
    fn initial_pty_size_is_clamped_to_safe_bounds() {
        assert_eq!(
            resolved_initial_size(Some(InitialPaneSize { cols: 1, rows: 1 })),
            InitialPaneSize {
                cols: MIN_INITIAL_COLS,
                rows: MIN_INITIAL_ROWS
            }
        );
        assert_eq!(
            resolved_initial_size(Some(InitialPaneSize {
                cols: u16::MAX,
                rows: u16::MAX
            })),
            InitialPaneSize {
                cols: MAX_INITIAL_COLS,
                rows: MAX_INITIAL_ROWS
            }
        );
    }

    #[test]
    fn submit_after_bracketed_paste_flushes_before_return() {
        let mut writer = RecordingWriter::default();
        let options = write_options("turn text", true, true);

        write_pane_input(&mut writer, &options, Duration::ZERO).unwrap();

        let pasted = b"\x1b[200~turn text\x1b[201~";
        assert_eq!(writer.bytes, b"\x1b[200~turn text\x1b[201~\r");
        assert_eq!(writer.flush_offsets, vec![pasted.len(), pasted.len() + 1]);
    }

    #[test]
    fn bracketed_paste_strips_embedded_end_marker() {
        let mut writer = RecordingWriter::default();
        // Payload carries a forged paste terminator followed by a command.
        let options = write_options("safe\x1b[201~\nrm -rf ~\n", true, false);

        write_pane_input(&mut writer, &options, Duration::ZERO).unwrap();

        // The embedded ESC[201~ is removed, so the paste stays framed as a
        // single inert block and the trailing bytes cannot escape to be typed.
        assert_eq!(writer.bytes, b"\x1b[200~safe\nrm -rf ~\n\x1b[201~");
    }

    #[test]
    fn bracketed_paste_strips_nested_end_marker() {
        let mut writer = RecordingWriter::default();
        // A nested marker that a single non-overlapping pass would leave a live
        // ESC[201~ behind — the strip must run to a fixed point.
        let options = write_options("safe\x1b[201\x1b[201~~\nrm -rf ~\n", true, false);

        write_pane_input(&mut writer, &options, Duration::ZERO).unwrap();

        assert_eq!(writer.bytes, b"\x1b[200~safe\nrm -rf ~\n\x1b[201~");
    }

    #[test]
    fn bracketed_paste_leaves_marker_free_data_untouched() {
        let mut writer = RecordingWriter::default();
        let options = write_options("ordinary multi\nline text", true, false);

        write_pane_input(&mut writer, &options, Duration::ZERO).unwrap();

        assert_eq!(writer.bytes, b"\x1b[200~ordinary multi\nline text\x1b[201~");
    }

    #[test]
    fn submit_after_plain_write_sends_return_after_text() {
        let mut writer = RecordingWriter::default();
        let options = write_options("y", false, true);

        write_pane_input(&mut writer, &options, Duration::ZERO).unwrap();

        assert_eq!(writer.bytes, b"y\r");
        assert_eq!(writer.flush_offsets, vec![1, 2]);
    }

    #[test]
    fn native_paste_uses_ghostty_paste_action_without_manual_markers() {
        let options = write_options("test", true, true);
        let mut raw_text = None;
        let mut approved_paste = None;

        write_native_pane_data(
            &options,
            |data| {
                raw_text = Some(data.to_string());
                Ok(())
            },
            |data| {
                approved_paste = Some(data.to_string());
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(raw_text, None);
        assert_eq!(approved_paste.as_deref(), Some("test"));
    }

    #[test]
    fn native_submission_flushes_paste_and_submit_separately() {
        let calls = RefCell::new(Vec::new());
        let positions = RefCell::new(vec![0_u64, 12, 13].into_iter());

        write_native_data_and_submit(
            "test",
            || {
                calls.borrow_mut().push("paste".to_string());
                Ok(())
            },
            || {
                calls.borrow_mut().push("submit".to_string());
                Ok(())
            },
            || {
                calls.borrow_mut().push("flush".to_string());
                positions
                    .borrow_mut()
                    .next()
                    .ok_or_else(|| "unexpected flush".to_string())
            },
            Duration::ZERO,
            Duration::ZERO,
            NATIVE_SUBMIT_ACK_ATTEMPTS,
        )
        .unwrap();

        assert_eq!(
            calls.into_inner(),
            ["flush", "paste", "flush", "submit", "flush"]
        );
    }

    #[test]
    fn native_submission_accepts_a_late_submit_ack_without_retrying_return() {
        let submit_calls = Cell::new(0);
        let positions = RefCell::new(vec![0_u64, 12, 12, 13].into_iter());

        write_native_data_and_submit(
            "test",
            || Ok(()),
            || {
                submit_calls.set(submit_calls.get() + 1);
                Ok(())
            },
            || {
                positions
                    .borrow_mut()
                    .next()
                    .ok_or_else(|| "unexpected flush".to_string())
            },
            Duration::ZERO,
            Duration::ZERO,
            NATIVE_SUBMIT_ACK_ATTEMPTS,
        )
        .unwrap();

        assert_eq!(submit_calls.get(), 1);
    }

    #[test]
    fn native_submission_retries_only_return_after_missing_ack() {
        let data_calls = Cell::new(0);
        let submit_calls = Cell::new(0);
        let positions = RefCell::new(vec![0_u64, 12, 12, 12, 13].into_iter());

        write_native_data_and_submit(
            "test",
            || {
                data_calls.set(data_calls.get() + 1);
                Ok(())
            },
            || {
                submit_calls.set(submit_calls.get() + 1);
                Ok(())
            },
            || {
                positions
                    .borrow_mut()
                    .next()
                    .ok_or_else(|| "unexpected flush".to_string())
            },
            Duration::ZERO,
            Duration::ZERO,
            NATIVE_SUBMIT_ACK_ATTEMPTS,
        )
        .unwrap();

        assert_eq!(data_calls.get(), 1);
        assert_eq!(submit_calls.get(), 2);
    }

    #[test]
    fn native_submission_defers_unacknowledged_submit_to_the_watchdog() {
        let submit_calls = Cell::new(0);
        let positions = RefCell::new(vec![0_u64, 12, 12, 12, 12, 12].into_iter());

        // The paste landed (12 > 0) but the Return's bytes never showed up at the PTY
        // within the local window. This must NOT error: an error requeues the turn and
        // re-pastes duplicate text. Instead we exhaust the bounded Return retries and
        // report best-effort delivery, leaving final confirmation to the submit watchdog.
        write_native_data_and_submit(
            "test",
            || Ok(()),
            || {
                submit_calls.set(submit_calls.get() + 1);
                Ok(())
            },
            || {
                positions
                    .borrow_mut()
                    .next()
                    .ok_or_else(|| "unexpected flush".to_string())
            },
            Duration::ZERO,
            Duration::ZERO,
            NATIVE_SUBMIT_ACK_ATTEMPTS,
        )
        .unwrap();

        assert_eq!(submit_calls.get(), NATIVE_SUBMIT_ACK_ATTEMPTS);
    }

    #[test]
    fn native_submit_leg_failure_reports_payload_delivered() {
        let positions = RefCell::new(vec![0_u64, 12].into_iter());

        // The paste landed (12 > 0) before the submit key errored: the turn queue
        // must learn the text is already in the composer so the retry doesn't
        // paste a duplicate copy.
        let failure = write_native_data_and_submit(
            "test",
            || Ok(()),
            || Err("bridge lost".to_string()),
            || {
                positions
                    .borrow_mut()
                    .next()
                    .ok_or_else(|| "unexpected flush".to_string())
            },
            Duration::ZERO,
            Duration::ZERO,
            NATIVE_SUBMIT_ACK_ATTEMPTS,
        )
        .unwrap_err();

        assert!(failure.data_delivered);
        assert!(failure.error.contains("bridge lost"));
    }

    #[test]
    fn native_payload_failure_reports_payload_undelivered() {
        // The paste action itself failed: nothing reached the composer, so the
        // retry must re-paste the full text.
        let failure = write_native_data_and_submit(
            "test",
            || Err("paste rejected".to_string()),
            || Ok(()),
            || Ok(0),
            Duration::ZERO,
            Duration::ZERO,
            NATIVE_SUBMIT_ACK_ATTEMPTS,
        )
        .unwrap_err();
        assert!(!failure.data_delivered);

        // The paste was accepted but verifiably never emitted PTY input: also
        // undelivered.
        let failure = write_native_data_and_submit(
            "test",
            || Ok(()),
            || Ok(()),
            || Ok(0),
            Duration::ZERO,
            Duration::ZERO,
            NATIVE_SUBMIT_ACK_ATTEMPTS,
        )
        .unwrap_err();
        assert!(!failure.data_delivered);
    }

    fn spawn_test_pty(state: &AppState, pane_id: &str, args: Vec<String>) -> PaneInfo {
        spawn_pty(
            state,
            PtySpawnSpec {
                pane_id: Some(pane_id.to_string()),
                agent_id: None,
                group_id: "group-1".to_string(),
                kind: PaneKind::Shell,
                title: "test".to_string(),
                last_osc_title: None,
                program: "/bin/sh".to_string(),
                args,
                cwd: std::env::temp_dir(),
                envs: Vec::new(),
                initial_size: None,
                recovered: false,
            },
        )
        .expect("spawning a test PTY")
    }

    #[test]
    fn recovered_shell_preserves_base_and_osc_titles() {
        let mut spec = PtySpawnSpec {
            pane_id: Some("pane-1".to_string()),
            agent_id: None,
            group_id: "group-1".to_string(),
            kind: PaneKind::Shell,
            title: "Shell".to_string(),
            last_osc_title: None,
            program: "/bin/sh".to_string(),
            args: Vec::new(),
            cwd: std::env::temp_dir(),
            envs: Vec::new(),
            initial_size: None,
            recovered: true,
        };
        let pane = PaneInfo {
            id: "pane-1".to_string(),
            title: "Generated review title".to_string(),
            last_osc_title: Some("Agent OSC title".to_string()),
            kind: PaneKind::Shell,
            agent_id: None,
            group_id: "group-1".to_string(),
            cwd: std::env::temp_dir().display().to_string(),
            cols: 100,
            rows: 24,
            status: PaneStatus::Running,
            last_active_at: 0,
            recovered: true,
            depth: 0,
        };

        apply_recovered_shell_titles(&mut spec, &pane);

        assert_eq!(spec.title, "Generated review title");
        assert_eq!(spec.last_osc_title.as_deref(), Some("Agent OSC title"));
    }

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn close_worktree_pane_deletes_after_agent_is_pruned() {
        let workspace = temp_workspace();
        let repo = workspace.join("repo");
        let worktree = workspace.join("agent-worktree");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "qmux test"]);
        git(&repo, &["commit", "--allow-empty", "-m", "init"]);

        let branch = "qmux/test-agent";
        let worktree_arg = worktree.to_string_lossy().to_string();
        git(
            &repo,
            &["worktree", "add", "-b", branch, &worktree_arg, "HEAD"],
        );

        let state = test_state_with_workspace(workspace.clone());
        state
            .insert_group_after(
                GroupInfo {
                    id: "group-1".to_string(),
                    name: "group".to_string(),
                    name_override: None,
                    dir: workspace.to_string_lossy().to_string(),
                    managed_dir: workspace.join("managed").to_string_lossy().to_string(),
                    base_repo: Some(repo.to_string_lossy().to_string()),
                    base_ref: None,
                    parent_id: None,
                    created_at: 1,
                    collapsed: false,
                    scope: WorkspaceScope::Terminal,
                    imported_research_archive_id: None,
                    agents: vec!["agent-1".to_string()],
                },
                None,
            )
            .unwrap();
        let pane = spawn_test_pty(
            &state,
            "pane-worktree",
            vec!["-c".to_string(), "sleep 30".to_string()],
        );
        state
            .insert_agent(AgentInfo {
                id: "agent-1".to_string(),
                group_id: "group-1".to_string(),
                adapter: "claude".to_string(),
                worktree_dir: worktree.to_string_lossy().to_string(),
                branch: Some(branch.to_string()),
                pane_id: Some(pane.id),
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
                created_at: 1,
            })
            .unwrap();

        close_worktree_pane(&state, "agent-1", true).unwrap();

        assert!(state.agent("agent-1").unwrap().is_none());
        assert!(!worktree.exists());

        fs::remove_dir_all(workspace).ok();
    }

    #[test]
    fn reader_thread_reaps_and_removes_pane_after_child_exits() {
        let state = test_state();
        let pane = spawn_test_pty(
            &state,
            "pane-exit",
            vec!["-c".to_string(), "exit 0".to_string()],
        );

        // The child exits immediately; the reader thread should observe EOF, reap the
        // child (no zombie), and remove the pane from state.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.pane_child(&pane.id).unwrap().is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "pane was not removed after the child exited"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn kill_pane_terminates_a_running_child_and_removes_it() {
        let state = test_state();
        let pane = spawn_test_pty(
            &state,
            "pane-kill",
            vec!["-c".to_string(), "sleep 30".to_string()],
        );
        assert!(state.pane_child(&pane.id).unwrap().is_some());

        kill_pane(&state, pane.id.clone()).expect("killing the pane");
        assert!(
            state.pane_child(&pane.id).unwrap().is_none(),
            "pane should be gone after kill_pane"
        );
    }

    #[test]
    fn pane_activity_is_idle_for_shell_without_children() {
        let state = test_state();
        let pane = spawn_test_pty(&state, "pane-idle", Vec::new());

        assert_eq!(
            pane_activity(&state, pane.id.clone()).unwrap(),
            PaneActivity::idle()
        );

        kill_pane(&state, pane.id).expect("cleanup test pane");
    }

    #[test]
    fn pane_activity_detects_running_descendant_processes() {
        let state = test_state();
        let pane = spawn_test_pty(
            &state,
            "pane-busy",
            vec!["-c".to_string(), "sleep 30 & wait".to_string()],
        );

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let activity = pane_activity(&state, pane.id.clone()).unwrap();
            if matches!(activity.kind, PaneActivityKind::RunningProcess) {
                assert!(activity.process_count >= 1);
                assert!(activity.process_summary.is_some());
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "pane activity never detected the child process"
            );
            thread::sleep(Duration::from_millis(20));
        }

        kill_pane(&state, pane.id).expect("cleanup test pane");
    }

    #[test]
    fn descendant_walk_follows_parent_pairs_and_survives_cycles() {
        // 1 → {2, 3}, 2 → {4}, plus an unrelated 9→10 subtree and a stale
        // cycle (4 → 1) a mid-snapshot pid reuse could produce.
        let pairs = vec![(2, 1), (3, 1), (4, 2), (10, 9), (1, 4)];
        let mut descendants = descendants_from_parent_pairs(1, &pairs);
        // Each process must appear before its own descendants so a reversed
        // walk kills deepest-first; sibling order is unspecified.
        let position = |pid: u32| {
            descendants
                .iter()
                .position(|candidate| *candidate == pid)
                .unwrap_or_else(|| panic!("pid {pid} missing from walk"))
        };
        assert!(position(2) < position(4));
        descendants.sort_unstable();
        assert_eq!(descendants, vec![2, 3, 4]);

        assert_eq!(descendants_from_parent_pairs(9, &pairs), vec![10]);
        assert!(descendants_from_parent_pairs(42, &pairs).is_empty());
    }

    #[test]
    fn pane_activity_process_filter_excludes_qmux() {
        let processes = user_running_processes(vec![
            RunningProcess {
                name: "qmux".to_string(),
            },
            RunningProcess {
                name: "QMUX".to_string(),
            },
            RunningProcess {
                name: "node".to_string(),
            },
        ]);

        assert_eq!(
            processes,
            vec![RunningProcess {
                name: "node".to_string(),
            }]
        );
    }

    #[test]
    fn process_parent_snapshot_includes_this_process() {
        let pid = std::process::id();
        assert!(
            process_parent_snapshot()
                .iter()
                .any(|(candidate, _)| *candidate == pid),
            "ps snapshot did not include the test process"
        );
    }

    #[test]
    fn pre_attach_output_is_recorded_only_when_attach_flushes_it() {
        let workspace = temp_workspace();
        let state = test_state_with_workspace(workspace.clone());
        let pane = spawn_test_pty(
            &state,
            "pane-scrollback",
            vec![
                "-c".to_string(),
                "printf 'restored\\n'; sleep 5".to_string(),
            ],
        );

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let backlog = state
                .pane_backlog(&pane.id)
                .unwrap()
                .expect("pane has backlog");
            let has_output = backlog
                .lock()
                .unwrap()
                .buffer
                .windows("restored".len())
                .any(|window| window == b"restored");
            if has_output {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "pane did not buffer pre-attach output"
            );
            thread::sleep(Duration::from_millis(20));
        }

        assert!(
            read_pane_scrollback(&workspace, &pane.id)
                .unwrap()
                .is_empty(),
            "pre-attach output must not be visible in the durable log before replay"
        );

        attach_pane(&state, pane.id.clone()).expect("attaching pane flushes backlog");
        let restored = read_pane_scrollback(&workspace, &pane.id).unwrap();
        assert!(
            restored
                .windows("restored".len())
                .any(|window| window == b"restored"),
            "attach should record the flushed backlog"
        );

        kill_pane(&state, pane.id.clone()).expect("cleanup test pane");
        assert!(
            read_pane_scrollback(&workspace, &pane.id)
                .unwrap()
                .is_empty(),
            "closing a pane should remove its scrollback log"
        );
    }

    #[test]
    fn append_capped_keeps_recent_bytes_under_cap() {
        let mut buffer = Vec::new();
        append_capped(&mut buffer, b"hello");
        append_capped(&mut buffer, b" world");
        assert_eq!(buffer, b"hello world");
    }

    #[test]
    fn append_capped_keeps_large_recovered_tui_repaint() {
        let mut buffer = Vec::new();
        let repaint = vec![b'x'; 512 * 1024];
        append_capped(&mut buffer, &repaint);

        assert_eq!(buffer.len(), repaint.len());
        assert_eq!(buffer[0], b'x');
    }

    #[test]
    fn append_capped_drops_oldest_when_over_cap() {
        let mut buffer = Vec::new();
        let first = vec![b'a'; BACKLOG_CAP];
        append_capped(&mut buffer, &first);
        append_capped(&mut buffer, b"tail");

        // The trim overshoots the cap by the slack so a saturated backlog pays
        // one front-memmove per slack's worth of chunks, not one per chunk.
        assert_eq!(buffer.len(), BACKLOG_CAP - BACKLOG_TRIM_SLACK);
        // The oldest bytes were dropped to make room; the most recent bytes win.
        assert_eq!(&buffer[buffer.len() - 4..], b"tail");
        assert_eq!(buffer[0], b'a');

        // Appends within the reopened slack must not re-trim.
        append_capped(&mut buffer, b"-more");
        assert_eq!(buffer.len(), BACKLOG_CAP - BACKLOG_TRIM_SLACK + 5);
    }
}
