use crate::adapters::{ShellCommandIntegration, adapter_registry};
use crate::events::QmuxEvent;
use crate::host::Host;
use crate::remote_terminal::{
    RemoteAttachment, RemoteAttachmentController, RemoteHistoryCheckpoint,
};
use crate::scrollback::{
    append_pane_scrollback, read_pane_scrollback, read_remote_history_checkpoint,
    remove_pane_scrollback, sanitize_scrollback_replay, write_remote_history_checkpoint,
};
use crate::state::{
    AppState, HostPtyBackend, PaneBackend, PaneInfo, PaneKind, PaneRuntime, PaneStatus,
    RemoteConnectionInfo, RemoteConnectionState, RemoteSessionIdentity, RemoteTmuxBackend,
    SharedBacklog, SharedChild, SharedWriter, ShellAgentResume,
};
use crate::turn_queue::{abort_fork_barrier_for_child, release_waiters_for_agent};
use crate::workspace::{
    CreateGroupRequest, RemoteRef, WorkspaceScope, capture_agent_worktree_removal, create_group,
    group_recoverable_dir, remove_captured_worktree,
};
use portable_pty::PtySize;
use portable_pty::{CommandBuilder, native_pty_system};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const SUBMIT_KEY: &[u8] = b"\r";
const REMOTE_BATCH_TIMEOUT: Duration = Duration::from_secs(55);
static VALIDATED_TMUX_VERSIONS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
// End by clearing Kitty keyboard enhancements. Historical agent output is
// sanitized before this is sent, but the explicit reset is defense in depth
// against an unrecognized keyboard-protocol form reaching the fresh surface.
// It runs before the new process's buffered startup output, so an agent resumed
// into the pane can still enable its desired live keyboard mode afterward.
const RESTORED_SCROLLBACK_TERMINAL_RESET: &[u8] = b"\x18\x1b>\x1b[0m\x1b(B\x1b[4l\x1b[?1l\x1b[?7h\x1b[?9l\x1b[?25h\x1b[?45l\x1b[?66l\x1b[?47l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l\x1b[?1047l\x1b[?2004l\x1b[?2026l\x1b[>4;0m\x1b[=0u";
// The subset of the reset that is safe to send to a *live* pane's surface —
// one an exited or suspended agent left behind for the surviving shell — as
// opposed to a fresh surface being rebuilt from scrollback. It clears only
// latched input
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
/// Timing for the native paste/submit handshake. Ghostty's approved-paste action
/// and synthesized key path both return before their in-memory-session callbacks
/// necessarily reach the PTY writer, so delivery is observed through bounded polls.
#[derive(Clone, Copy)]
struct NativeSubmitTiming {
    data_poll_interval: Duration,
    data_max_rechecks: usize,
    data_quiet_rechecks: usize,
    submit_key_delay: Duration,
    submit_poll_interval: Duration,
    submit_max_rechecks: usize,
}

const NATIVE_SUBMIT_TIMING: NativeSubmitTiming = NativeSubmitTiming {
    data_poll_interval: Duration::from_millis(25),
    data_max_rechecks: 8,
    data_quiet_rechecks: 2,
    submit_key_delay: SUBMIT_KEY_DELAY,
    submit_poll_interval: Duration::from_millis(50),
    submit_max_rechecks: 2,
};
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

/// A file the spawn backend must write on the host that runs the command,
/// before the command starts: generated shell rc scripts, per-spawn hook
/// settings. Declarative so the backend owns where and how files land — the
/// local backend writes them in `materialize_support_files`; a remote backend
/// must ship them to its host instead.
#[derive(Debug)]
pub struct SupportFile {
    /// Owner-only directory subtree the file lives under. Every component from
    /// here down to the file's parent is created and restricted to 0o700, so
    /// generated files are never reachable by other accounts even under a
    /// shared world-writable location like /tmp.
    pub root: PathBuf,
    /// Absolute path of the file itself; must sit below `root`.
    pub path: PathBuf,
    pub contents: String,
    /// Mode bits applied when the file is created.
    pub mode: u32,
    /// When true the write must create the file (O_CREAT|O_EXCL) and fail if
    /// the path already exists — for nonce-named files where a collision means
    /// a planted file that must never be followed or truncated.
    pub create_new: bool,
    /// Filename prefix to prune from the file's parent before writing, keeping
    /// per-pane scratch bounded to one live file across spawn/resume cycles.
    pub prune_prefix: Option<String>,
}

/// Command arguments and environment to use when support files are optional
/// and the launch backend cannot materialize them. Shell integration uses this
/// to preserve a usable plain shell when its generated rc file cannot be
/// written; launches whose support files are required leave this as `None`.
#[derive(Debug)]
pub struct SupportFileFallback {
    pub args: Vec<String>,
    pub envs: Vec<(String, String)>,
    /// Optional environment variable that receives the materialization error.
    pub error_env_key: Option<String>,
}

/// What should run for a pane — program, arguments, directory, environment,
/// and the support files the command depends on — independent of where it
/// runs. Adapters and the shell path build plans; `plan_to_spec` turns a plan
/// into an executable spec for the pane's group.
#[derive(Debug)]
pub struct CommandPlan {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub envs: Vec<(String, String)>,
    pub support_files: Vec<SupportFile>,
    pub support_file_fallback: Option<SupportFileFallback>,
}

/// Pane bookkeeping that accompanies a `CommandPlan` into `plan_to_spec`.
#[derive(Debug)]
pub struct PaneMeta {
    pub pane_id: Option<String>,
    pub agent_id: Option<String>,
    pub group_id: String,
    pub kind: PaneKind,
    pub title: String,
    pub last_osc_title: Option<String>,
    pub initial_size: Option<InitialPaneSize>,
    pub recovered: bool,
}

/// The single decision point between "what should run for this pane" and "how
/// it is executed". A local plan maps 1:1 onto a `PtySpawnSpec`; a remote
/// group's is wrapped in its transport — ssh, plus the multiplexer that lets the
/// pane survive a dropped connection — before it ever reaches the pty layer.
///
/// Doing it here rather than in each adapter is what makes remote panes an
/// adapter-agnostic property: the pty still runs one local process, it is just
/// `ssh` instead of the agent.
pub fn plan_to_spec(
    state: &AppState,
    meta: PaneMeta,
    mut plan: CommandPlan,
) -> Result<PtySpawnSpec, String> {
    let remote = state.group(&meta.group_id)?.and_then(|group| group.remote);
    let host = crate::host::for_group(remote.as_ref());
    let pane_id = meta.pane_id.clone();

    // An adapter has to have been built for this. The failure mode otherwise is
    // silent: the process starts over there with a binary path resolved here, a
    // plugin directory that exists only here, and its worktree not as its cwd.
    if !host.is_local()
        && let Some(agent_id) = meta.agent_id.as_deref()
        && let Some(agent) = state.agent(agent_id)?
    {
        let registry = crate::adapters::adapter_registry(state.config());
        if !registry.get(&agent.adapter)?.supports_remote() {
            return Err(format!(
                "the {} adapter cannot run on remote '{}' yet; it resolves paths on the machine qmux is running on",
                agent.adapter,
                host.label()
            ));
        }
    }
    if !host.is_local()
        && let Some(agent_id) = meta.agent_id.as_deref()
    {
        // A transcript path stored by an older build may name a local file even
        // though this process will run over SSH. Clear it before the remote token
        // becomes reachable; remote hooks record session identity without local IO.
        state.mutate_agent(agent_id, |agent| agent.transcript_path = None)?;
    }

    let remote = if let Some(target) = host.remote() {
        let pane_id = pane_id
            .as_deref()
            .ok_or_else(|| "a remote pane requires a stable pane id".to_string())?;
        let mut identity = RemoteSessionIdentity::new(&target.id, pane_id)?;
        // The tmux session includes a random nonce and is collision-resistant
        // across qmux workspaces/processes. Scope support files to that durable
        // identity rather than the sequential pane id, which can repeat.
        let support_dir =
            relocate_remote_support_files(&host, pane_id, &identity.tmux_session, &mut plan)?;
        identity.support_dir = support_dir;
        Some(RemoteSpawnSpec {
            host: host.clone(),
            identity,
        })
    } else {
        None
    };

    Ok(PtySpawnSpec {
        pane_id: meta.pane_id,
        agent_id: meta.agent_id,
        group_id: meta.group_id,
        kind: meta.kind,
        title: meta.title,
        last_osc_title: meta.last_osc_title,
        program: plan.program,
        args: plan.args,
        cwd: plan.cwd,
        envs: plan.envs,
        support_files: plan.support_files,
        support_file_fallback: plan.support_file_fallback,
        initial_size: meta.initial_size,
        recovered: meta.recovered,
        ssh_target: None,
        remote,
    })
}

fn relocate_remote_support_files(
    host: &Host,
    pane_id: &str,
    remote_scope: &str,
    plan: &mut CommandPlan,
) -> Result<Option<String>, String> {
    if plan.support_files.is_empty() {
        return Ok(None);
    }
    if pane_id.is_empty()
        || !pane_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("invalid pane id for remote support directory".to_string());
    }
    let global_root = PathBuf::from(
        host.remote_support_root()?
            .ok_or_else(|| "remote support files require a remote host".to_string())?,
    );
    if remote_scope.is_empty()
        || !remote_scope
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("invalid remote support scope".to_string());
    }
    let pane_root = global_root.join(remote_scope);
    let local_cli = plan
        .envs
        .iter()
        .find_map(|(key, value)| (key == "QMUX_CLI").then(|| value.clone()));
    let remote_cli = host.remote().map(|target| target.qmux_cli.clone());
    let mut root_rewrites = Vec::new();

    for file in &mut plan.support_files {
        let relative = support_file_relative(file)?;
        let (old_prefix, relative) = if let Ok(stripped) = relative.strip_prefix(pane_id) {
            (file.root.join(pane_id), stripped.to_path_buf())
        } else {
            (file.root.clone(), relative)
        };
        let old_root = old_prefix.display().to_string();
        let new_root = pane_root.display().to_string();
        file.root = pane_root.clone();
        file.path = pane_root.join(relative);
        root_rewrites.push((old_root, new_root));
    }
    root_rewrites.sort_by_key(|(old, _)| std::cmp::Reverse(old.len()));
    root_rewrites.dedup();

    let rewrite = |value: &mut String| {
        for (old, new) in &root_rewrites {
            if value == old || value.starts_with(&format!("{old}/")) {
                *value = format!("{new}{}", &value[old.len()..]);
                break;
            }
        }
    };
    for arg in &mut plan.args {
        rewrite(arg);
    }
    for (key, value) in &mut plan.envs {
        rewrite(value);
        if key == "QMUX_ORIGINAL_ZDOTDIR" {
            *value = host.expand_home("~")?;
        } else if key == "QMUX_ORIGINAL_BASHRC" {
            *value = host.expand_home("~/.bashrc")?;
        }
    }
    for file in &mut plan.support_files {
        for (old, new) in &root_rewrites {
            file.contents = file.contents.replace(old, new);
        }
        if let (Some(local), Some(remote)) = (&local_cli, &remote_cli) {
            file.contents = file.contents.replace(local, remote);
        }
    }
    Ok(Some(pane_root.display().to_string()))
}

#[derive(Debug)]
pub struct RemoteSpawnSpec {
    pub host: Host,
    pub identity: RemoteSessionIdentity,
}

#[derive(Debug)]
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
    pub support_files: Vec<SupportFile>,
    pub support_file_fallback: Option<SupportFileFallback>,
    pub initial_size: Option<InitialPaneSize>,
    pub recovered: bool,
    /// Client `ssh` destination when this pane is an SSH session inside a
    /// group that is not bound to that host. Copied onto `PaneInfo` so restart
    /// reopens `ssh` rather than a login shell.
    pub ssh_target: Option<String>,
    pub remote: Option<RemoteSpawnSpec>,
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
    spawn_shell_pane_at(state, initial_size, source_pane_id, group_id, None)
}

pub fn spawn_shell_pane_at(
    state: &AppState,
    initial_size: Option<InitialPaneSize>,
    source_pane_id: Option<&str>,
    group_id: Option<&str>,
    cwd_override: Option<&str>,
) -> Result<PaneInfo, String> {
    // A user-opened shell inherits the focused tab's current directory when that
    // directory sits inside the target group, or when the focused pane is a
    // same-group shell ("new tab here"). Otherwise it opens in the target group
    // directory / home, never the bare `/` a Finder/Dock launch inherits as its cwd.
    let source_group_id = source_pane_id.and_then(|id| state.pane_group_id(id).ok().flatten());
    let group = match group_id.or(source_group_id.as_deref()) {
        Some(group_id) => state
            .group(group_id)?
            .ok_or_else(|| format!("group {group_id} was not found"))?,
        None => create_group(
            state,
            CreateGroupRequest {
                remote_id: None,
                name: None,
                dir: None,
                after_group_id: None,
                base_repo: None,
                base_ref: None,
                remote: None,
            },
        )?,
    };
    if group.scope != WorkspaceScope::Terminal {
        return Err("ordinary shells cannot be opened in a research workspace".to_string());
    }
    // Prefer the current tab's cwd when it is inside this group's directory
    // (a tab that has `cd`'d into a subdirectory). Same-group shells still
    // inherit even after `cd`'ing elsewhere ("new tab here"). Opening into a
    // group from outside otherwise uses that group's most-recently-active
    // shell, then its creation-time seed dir (`group.dir`), then home.
    let cwd = state.resolve_shell_spawn_cwd(&group, source_pane_id, cwd_override)?;
    let pane_id = state.next_id("pane");
    spawn_pty(
        state,
        shell_spawn_spec(state, pane_id, group.id, cwd, initial_size, false, None)?,
    )
}

/// Opens an interactive `ssh` session to `remote` as a tab in the current
/// group. If that group is already bound to this machine, this is just a
/// normal remote shell. Otherwise the pane's process is `ssh -t -- host`, so
/// the group stays where it is.
pub fn spawn_ssh_shell_pane(
    state: &AppState,
    initial_size: Option<InitialPaneSize>,
    source_pane_id: Option<&str>,
    group_id: Option<&str>,
    remote: &RemoteRef,
) -> Result<PaneInfo, String> {
    let source_group_id = source_pane_id.and_then(|id| state.pane_group_id(id).ok().flatten());
    let group = match group_id.or(source_group_id.as_deref()) {
        Some(group_id) => state
            .group(group_id)?
            .ok_or_else(|| format!("group {group_id} was not found"))?,
        None => create_group(
            state,
            CreateGroupRequest {
                remote_id: None,
                name: None,
                dir: None,
                after_group_id: None,
                base_repo: None,
                base_ref: None,
                remote: None,
            },
        )?,
    };
    if group.scope != WorkspaceScope::Terminal {
        return Err("ordinary shells cannot be opened in a research workspace".to_string());
    }
    if group
        .remote
        .as_ref()
        .is_some_and(|bound| bound.id == remote.id)
    {
        return spawn_shell_pane(state, initial_size, source_pane_id, Some(&group.id));
    }
    let cwd = state.resolve_shell_spawn_cwd(&group, source_pane_id, None)?;
    let pane_id = state.next_id("pane");
    spawn_pty(
        state,
        ssh_client_spawn_spec(state, pane_id, group.id, cwd, initial_size, false, remote)?,
    )
}

fn ssh_client_command(host: &str) -> (String, Vec<String>) {
    (
        "ssh".to_string(),
        vec!["-t".to_string(), "--".to_string(), host.to_string()],
    )
}

fn ssh_client_spawn_spec(
    state: &AppState,
    pane_id: String,
    group_id: String,
    cwd: PathBuf,
    initial_size: Option<InitialPaneSize>,
    recovered: bool,
    remote: &RemoteRef,
) -> Result<PtySpawnSpec, String> {
    ssh_client_spawn_spec_for_host(
        state,
        pane_id,
        group_id,
        cwd,
        initial_size,
        recovered,
        &remote.host,
        &remote.label,
    )
}

#[allow(clippy::too_many_arguments)]
fn ssh_client_spawn_spec_for_host(
    state: &AppState,
    pane_id: String,
    group_id: String,
    cwd: PathBuf,
    initial_size: Option<InitialPaneSize>,
    recovered: bool,
    host: &str,
    title: &str,
) -> Result<PtySpawnSpec, String> {
    let (program, args) = ssh_client_command(host);
    let envs = shell_pane_envs(state, &pane_id)?;
    let mut spec = plan_to_spec(
        state,
        PaneMeta {
            pane_id: Some(pane_id),
            agent_id: None,
            group_id,
            kind: PaneKind::Shell,
            title: title.to_string(),
            last_osc_title: None,
            initial_size,
            recovered,
        },
        CommandPlan {
            program,
            args,
            cwd,
            envs,
            support_files: Vec::new(),
            support_file_fallback: None,
        },
    )?;
    spec.ssh_target = Some(host.to_string());
    Ok(spec)
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
    let qmux_cli = crate::launch_path::qmux_cli_path()
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
    // A recovered shell whose last dir was deleted between sessions reopens near the
    // group's other work (its most-recently-active shell pane), else the group's
    // creation-time seed dir, else the default dir / home rather than the bare `/` a
    // Finder/Dock launch inherits. During startup recovery siblings may not be
    // respawned yet, in which case group_spawn_cwd yields None and the seed/default
    // apply.
    let group = state.group(&pane.group_id).ok().flatten();
    let group_remote = group.as_ref().and_then(|group| group.remote.as_ref());
    // Every dir probe below is group-aware: a remote group's paths live on its
    // host, so a local stat would discard the resume dir (dropping the resume
    // command with it) and then discard the pane's own cwd.
    let resume_dir = resume
        .as_ref()
        .and_then(|resume| group_recoverable_dir(group_remote, &resume.cwd));
    let group_seed_dir = group
        .as_ref()
        .and_then(|group| group_recoverable_dir(group.remote.as_ref(), &group.dir));
    let cwd = resume_dir
        .clone()
        .or_else(|| group_recoverable_dir(group_remote, &pane.cwd))
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
    let mut spec = if let Some(host) = pane.ssh_target.as_deref() {
        ssh_client_spawn_spec_for_host(
            state,
            pane.id.clone(),
            pane.group_id.clone(),
            cwd,
            initial_size,
            true,
            host,
            if pane.title.trim().is_empty() {
                host
            } else {
                pane.title.as_str()
            },
        )?
    } else {
        shell_spawn_spec(
            state,
            pane.id.clone(),
            pane.group_id.clone(),
            cwd,
            initial_size,
            true,
            resume_command,
        )?
    };
    // A shell pane may carry a manual/generated base title as well as a cached
    // OSC title. Recovery previously rebuilt every shell as literal "Shell",
    // discarding even explicitly renamed tabs.
    apply_recovered_shell_titles(&mut spec, pane);
    spawn_pty(state, spec)
}

/// Rebuilds only the local attachment for a persisted remote pane. This path
/// never has a create command: a missing tmux identity is a recovery failure,
/// not permission to launch a replacement process.
pub fn reattach_remote_pane(state: &AppState, pane: &PaneInfo) -> Result<PaneInfo, String> {
    let identity = pane
        .remote_session
        .clone()
        .ok_or_else(|| format!("pane {} has no remote session identity", pane.id))?;
    let group = state
        .group(&pane.group_id)?
        .ok_or_else(|| format!("group {} was not found", pane.group_id))?;
    let remote = group
        .remote
        .as_ref()
        .ok_or_else(|| format!("group {} is no longer remote", pane.group_id))?;
    if remote.id != identity.remote_id {
        return Err(format!(
            "pane {} belongs to remote '{}' but its group uses '{}'",
            pane.id, identity.remote_id, remote.id
        ));
    }
    let host = crate::host::for_group(Some(remote));
    let commands = host.existing_tmux_session_commands(
        &identity,
        &state.config().socket_path.display().to_string(),
    )?;
    ensure_remote_tmux_version(&commands.version_argv)?;
    let probe = probe_remote_session(&commands.probe_argv);
    if probe == Ok(false) {
        return Err(format!(
            "remote tmux session '{}' no longer exists",
            identity.tmux_session
        ));
    }

    let native_surface = cfg!(all(target_os = "macos", not(test)));
    let controller = RemoteAttachmentController::new();
    let history = RemoteHistoryCheckpoint::new(read_remote_history_checkpoint(
        &state.config().workspace_root,
        &pane.id,
    )?);
    let backlog: SharedBacklog = Arc::new(Mutex::new(Default::default()));
    let mut recovered = pane.clone();
    recovered.recovered = true;
    recovered.remote_connection = Some(match &probe {
        Ok(true) => RemoteConnectionInfo {
            state: RemoteConnectionState::Connecting,
            message: None,
        },
        Err(err) => RemoteConnectionInfo {
            state: RemoteConnectionState::Disconnected,
            message: Some(err.clone()),
        },
        Ok(false) => unreachable!("missing sessions returned above"),
    });
    let backend = RemoteTmuxBackend::new(
        controller.clone(),
        history.clone(),
        backlog.clone(),
        commands.clone(),
        native_surface,
    );
    let stable_writer = backend.writer.clone();
    state.insert_pane(PaneRuntime {
        info: recovered.clone(),
        backend: PaneBackend::RemoteTmux(backend),
        cwd_observation_seq: 0,
    })?;
    if native_surface {
        if let Err(err) = crate::native_terminal::create_host_managed(&pane.id, None) {
            let _ = state.remove_pane(&pane.id);
            return Err(err);
        }
        register_native_input_writer(&pane.id, stable_writer);
    }

    match probe {
        Ok(true) => {
            if let Err(err) = attach_remote_generation(
                state,
                &pane.id,
                controller.clone(),
                history.clone(),
                commands.clone(),
                backlog.clone(),
                native_surface,
            ) {
                schedule_remote_reconnect(
                    state.clone(),
                    pane.id.clone(),
                    controller,
                    history,
                    commands,
                    backlog,
                    native_surface,
                    err,
                );
            }
        }
        Err(err) => schedule_remote_reconnect(
            state.clone(),
            pane.id.clone(),
            controller,
            history,
            commands,
            backlog,
            native_surface,
            err,
        ),
        Ok(false) => unreachable!("missing sessions returned above"),
    }
    Ok(recovered)
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
    let remote = state.group(&group_id)?.and_then(|group| group.remote);
    let host = crate::host::for_group(remote.as_ref());
    let shell = host.interactive_shell(&pane_shell())?;
    let qmux_cli = if let Some(target) = host.remote() {
        PathBuf::from(&target.qmux_cli)
    } else {
        crate::launch_path::qmux_cli_path().map_err(|err| {
            format!("failed to resolve qmux executable for shell integration: {err}")
        })?
    };
    let mut envs = shell_pane_envs(state, &pane_id)?;
    let plain_shell_envs = envs.clone();
    let mut args = Vec::new();
    let mut support_files = Vec::new();
    let mut support_file_fallback = None;

    let shell_commands = adapter_registry(state.config()).shell_commands();
    let login_shell = state.use_login_shell();
    match agent_shell_function_injection(
        &shell,
        &qmux_cli,
        &pane_id,
        &shell_commands,
        startup_command.as_deref(),
        login_shell,
    ) {
        Ok(Some(injection)) => {
            args = injection.args;
            envs.extend(injection.envs);
            if host.remote().is_some() {
                let remote_home = host.expand_home("~")?;
                let remote_bashrc = host.expand_home("~/.bashrc")?;
                match shell_kind(&shell) {
                    ShellKind::Zsh => upsert_env(&mut envs, "QMUX_ORIGINAL_ZDOTDIR", remote_home),
                    ShellKind::Bash => upsert_env(&mut envs, "QMUX_ORIGINAL_BASHRC", remote_bashrc),
                    ShellKind::Unsupported => {}
                }
            }
            support_files = injection.support_files;
            envs.push(("QMUX_AGENT_FUNCTIONS".to_string(), "1".to_string()));
            if startup_command.is_none() || recovered {
                let mut fallback_envs = plain_shell_envs;
                fallback_envs.push(("QMUX_AGENT_FUNCTIONS".to_string(), "failed".to_string()));
                support_file_fallback = Some(SupportFileFallback {
                    args: Vec::new(),
                    envs: fallback_envs,
                    error_env_key: Some("QMUX_AGENT_FUNCTIONS_ERROR".to_string()),
                });
            }
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

    plan_to_spec(
        state,
        PaneMeta {
            pane_id: Some(pane_id),
            agent_id: None,
            group_id,
            kind: PaneKind::Shell,
            title: "Shell".to_string(),
            last_osc_title: None,
            initial_size,
            recovered,
        },
        CommandPlan {
            program: shell,
            args,
            cwd,
            envs,
            support_files,
            support_file_fallback,
        },
    )
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
        ("QMUX_ENV".to_string(), "1".to_string()),
    ];
    // Expose the qmux executable so in-pane tooling (hooks, agent wrappers, the
    // fork skill) can call back without depending on `qmux` being on PATH. The
    // one place any pane's QMUX_CLI is set, and required rather than
    // best-effort: every launch path already fails without a resolvable CLI, so
    // a pane that started with the variable missing would only fail later and
    // less legibly.
    envs.push((
        "QMUX_CLI".to_string(),
        crate::launch_path::qmux_cli_path()?.display().to_string(),
    ));
    Ok(envs)
}

/// Envs for an agent pane: the standard pane wiring plus the agent binding.
/// The one place the `QMUX_AGENT_ID` pairing is added, so no launch path can
/// forget it or spell it differently.
pub fn agent_pane_envs(
    state: &AppState,
    pane_id: &str,
    agent_id: &str,
) -> Result<Vec<(String, String)>, String> {
    let mut envs = qmux_pane_envs(state, pane_id)?;
    envs.push(("QMUX_AGENT_ID".to_string(), agent_id.to_string()));
    Ok(envs)
}

fn shell_pane_envs(state: &AppState, pane_id: &str) -> Result<Vec<(String, String)>, String> {
    let mut envs = qmux_pane_envs(state, pane_id)?;
    envs.push((
        "QMUX_USER_TOKEN".to_string(),
        state.pane_user_token(pane_id)?,
    ));
    envs.push(("QMUX_SHELL_INTEGRATION".to_string(), "1".to_string()));
    Ok(envs)
}

fn upsert_env(envs: &mut Vec<(String, String)>, key: &str, value: String) {
    if let Some((_, current)) = envs.iter_mut().find(|(name, _)| name == key) {
        *current = value;
    } else {
        envs.push((key.to_string(), value));
    }
}

struct ShellFunctionInjection {
    args: Vec<String>,
    envs: Vec<(String, String)>,
    support_files: Vec<SupportFile>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellKind {
    Bash,
    Zsh,
    Unsupported,
}

/// Shared parent for all per-pane shell integration scratch directories. The
/// `SupportFile` root, so materialization keeps the whole subtree owner-only.
fn shell_integration_root() -> PathBuf {
    env::temp_dir().join("qmux-shell-init")
}

/// Per-pane scratch directory holding generated shell rc files. The location is
/// derived purely from the pane id so teardown can find it without consulting
/// pane state.
fn shell_integration_dir(pane_id: &str) -> PathBuf {
    shell_integration_root().join(pane_id)
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

/// Plans the shell-integration wrapper for a pane: the extra shell arguments,
/// environment, and generated rc files. Purely declarative — nothing is
/// written here; the rc files come back as `SupportFile`s for the spawn
/// backend to materialize on whichever host runs the shell.
fn agent_shell_function_injection(
    shell: &str,
    qmux_cli: &Path,
    pane_id: &str,
    shell_commands: &[ShellCommandIntegration],
    startup_command: Option<&str>,
    login_shell: bool,
) -> Result<Option<ShellFunctionInjection>, String> {
    let shell_kind = shell_kind(shell);
    if matches!(shell_kind, ShellKind::Unsupported) {
        return Ok(None);
    }

    let root = shell_integration_dir(pane_id);

    match shell_kind {
        ShellKind::Zsh => {
            let zdotdir = root.join("zsh");
            let rcfile = zdotdir.join(".zshrc");
            let support_files = vec![SupportFile {
                root: shell_integration_root(),
                path: rcfile,
                contents: zsh_init_script(qmux_cli, shell_commands, startup_command, login_shell),
                mode: 0o644,
                create_new: false,
                prune_prefix: None,
            }];
            let mut envs = vec![("ZDOTDIR".to_string(), zdotdir.display().to_string())];
            if let Some(zdotdir) = original_zdotdir() {
                envs.push(("QMUX_ORIGINAL_ZDOTDIR".to_string(), zdotdir));
            }
            Ok(Some(ShellFunctionInjection {
                args: vec!["-i".to_string()],
                envs,
                support_files,
            }))
        }
        ShellKind::Bash => {
            let rcfile = root.join("bashrc");
            let support_files = vec![SupportFile {
                root: shell_integration_root(),
                path: rcfile.clone(),
                contents: bash_init_script(qmux_cli, shell_commands, startup_command, login_shell),
                mode: 0o644,
                create_new: false,
                prune_prefix: None,
            }];
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
                support_files,
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
  __qmux_report_cwd() {{
    {cli} cwd >/dev/null 2>&1
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
  __qmux_report_cwd() {{
    {cli} cwd >/dev/null 2>&1
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

/// Defines `qmux` as a passthrough to the bundled CLI so the user can run
/// subcommands such as `qmux fork` from the shell prompt without `qmux` being
/// on PATH — mirroring the injected agent functions.
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
fn base_child_envs(state: &AppState) -> Result<Vec<(String, String)>, String> {
    let mut envs = Vec::new();
    envs.push((
        "PATH".to_string(),
        crate::launch_path::pane_child_path(&state.config().socket_path)?,
    ));
    envs.push(("COLORTERM".to_string(), "truecolor".to_string()));
    // Backfill a UTF-8 locale only when one wasn't inherited — a GUI launch
    // gets no LANG, defaulting programs to the C locale and breaking Unicode,
    // while a deliberately-set locale from a dev shell is left untouched.
    if env::var_os("LANG").is_none() {
        envs.push(("LANG".to_string(), "en_US.UTF-8".to_string()));
    }
    Ok(envs)
}

/// Creates `dir` if it is missing and restricts it to the owning user. Callers
/// walk a chain top-down with this so no level is ever reachable by another
/// account, even for the instant between its creation and its `chmod`.
fn create_owner_only_dir(dir: &Path) -> Result<(), String> {
    match fs::create_dir(dir) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
        Err(err) => return Err(format!("failed to create {}: {err}", dir.display())),
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .map_err(|err| format!("failed to restrict {}: {err}", dir.display()))
}

fn support_file_relative(file: &SupportFile) -> Result<PathBuf, String> {
    let relative = file.path.strip_prefix(&file.root).map_err(|_| {
        format!(
            "support file {} escapes its root {}",
            file.path.display(),
            file.root.display()
        )
    })?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "support file {} escapes its root {}",
            file.path.display(),
            file.root.display()
        ));
    }
    Ok(relative.to_path_buf())
}

/// Writes a spec's support files on the local filesystem: the local half of the
/// `SupportFile` contract. Carries the pruning and O_EXCL semantics of the
/// inline writers this replaced, and tightens their directory handling: every
/// level from the shared root down to the file's parent is owner-only from the
/// moment it exists, and a descriptor whose path does not genuinely resolve
/// below its declared root is refused rather than written.
pub(crate) fn materialize_support_files(files: &[SupportFile]) -> Result<(), String> {
    for file in files {
        let parent = file
            .path
            .parent()
            .ok_or_else(|| format!("support file {} has no parent", file.path.display()))?;
        let relative = support_file_relative(file)?;
        // `strip_prefix` is purely lexical: it happily hands back a relative
        // path that starts with `..`, which would satisfy the containment check
        // above while resolving outside the root. Require plain names so the
        // path really does sit below the root it claims.
        // Create the chain one component at a time, restricting each level
        // before the next is created inside it, so another local account can
        // never pre-create (or traverse into) a directory we are about to write
        // generated files under. Creating the whole chain up front and only then
        // walking down would leave the intermediate levels briefly
        // world-traversable under a shared location like /tmp.
        if let Some(root_parent) = file.root.parent() {
            fs::create_dir_all(root_parent)
                .map_err(|err| format!("failed to create {}: {err}", root_parent.display()))?;
        }
        create_owner_only_dir(&file.root)?;
        let mut dir = file.root.clone();
        if let Some(relative_parent) = relative.parent() {
            for component in relative_parent.components() {
                dir.push(component);
                create_owner_only_dir(&dir)?;
            }
        }
        if let Some(prefix) = &file.prune_prefix
            && let Ok(entries) = fs::read_dir(parent)
        {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(prefix.as_str()))
                {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).mode(file.mode);
        if file.create_new {
            options.create_new(true);
        } else {
            options.create(true).truncate(true);
        }
        let mut handle = options
            .open(&file.path)
            .map_err(|err| format!("failed to create {}: {err}", file.path.display()))?;
        handle
            .write_all(file.contents.as_bytes())
            .map_err(|err| format!("failed to write {}: {err}", file.path.display()))?;
    }
    Ok(())
}

/// Materializes a local spec's support files, applying its declared plain-command
/// fallback when those files are optional. Required support files still fail the
/// spawn, while the fallback receives the original error for diagnostics.
fn materialize_support_files_or_fallback(spec: &mut PtySpawnSpec) -> Result<(), String> {
    let Err(err) = materialize_support_files(&spec.support_files) else {
        return Ok(());
    };
    let Some(fallback) = spec.support_file_fallback.take() else {
        return Err(err);
    };
    spec.args = fallback.args;
    spec.envs = fallback.envs;
    if let Some(key) = fallback.error_env_key {
        spec.envs.push((key, err));
    }
    spec.support_files.clear();
    Ok(())
}

const REMOTE_SUPPORT_FILE_SCRIPT: &str = r#"set -eu
managed_root=$1
root=$2
path=$3
mode=$4
exclusive=$5
prefix=$6
case "$managed_root" in /*) ;; *) echo 'managed support root is not absolute' >&2; exit 64 ;; esac
case "$root" in /*) ;; *) echo 'support root is not absolute' >&2; exit 64 ;; esac
case "$root" in
  "$managed_root"/*)
    scope=${root#"$managed_root"/}
    case "$scope" in ''|*/*) echo 'support root is not one managed directory' >&2; exit 64 ;; esac
    ;;
  *) echo 'support root escapes managed root' >&2; exit 64 ;;
esac
case "$path" in "$root"/*) ;; *) echo 'support path escapes root' >&2; exit 64 ;; esac
umask 077
mkdir -p -- "$managed_root"
[ -d "$managed_root" ] && [ ! -L "$managed_root" ] || { echo 'managed support root is unsafe' >&2; exit 65; }
chmod 700 -- "$managed_root"
if [ ! -e "$root" ] && [ ! -L "$root" ]; then
  mkdir -- "$root"
fi
[ -d "$root" ] && [ ! -L "$root" ] || { echo 'support root is unsafe' >&2; exit 65; }
chmod 700 -- "$root"
relative=${path#"$root"/}
parent=${path%/*}
current=$root
relative_parent=${relative%/*}
if [ "$relative_parent" != "$relative" ]; then
  old_ifs=$IFS
  IFS=/
  set -f
  for component in $relative_parent; do
    current=$current/$component
    if [ ! -e "$current" ] && [ ! -L "$current" ]; then
      mkdir -- "$current"
    fi
    [ -d "$current" ] && [ ! -L "$current" ] || { echo 'support directory is unsafe' >&2; exit 65; }
    chmod 700 -- "$current"
  done
  set +f
  IFS=$old_ifs
fi
if [ -n "$prefix" ]; then
  for candidate in "$parent"/"$prefix"*; do
    [ -e "$candidate" ] || [ -L "$candidate" ] || continue
    rm -rf -- "$candidate"
  done
fi
tmp_dir=$parent/.qmux-upload.$$
if ! mkdir -- "$tmp_dir"; then
  echo 'could not reserve support upload directory' >&2
  exit 66
fi
trap 'rm -rf -- "$tmp_dir"' EXIT HUP INT TERM
tmp=$tmp_dir/file
cat > "$tmp"
chmod "$mode" -- "$tmp"
if [ "$exclusive" = 1 ]; then
  # Hard-link creation is atomic and refuses every existing final entry,
  # including symlinks and FIFOs. The temporary file lives in a freshly-created
  # 0700 directory, so the upload never writes through an attacker-controlled
  # final path.
  ln -- "$tmp" "$path"
  rm -f -- "$tmp"
else
  # rename replaces a final symlink rather than following it.
  mv -f -- "$tmp" "$path"
fi
rm -rf -- "$tmp_dir"
trap - EXIT HUP INT TERM
"#;

fn materialize_remote_support_files(host: &Host, files: &[SupportFile]) -> Result<(), String> {
    let managed_root = host
        .remote_support_root()?
        .ok_or_else(|| "remote support files require a remote host".to_string())?;
    for file in files {
        support_file_relative(file)?;
        if file.mode & !0o777 != 0 {
            return Err(format!("invalid support file mode {:o}", file.mode));
        }
        let root = file
            .root
            .to_str()
            .ok_or_else(|| format!("support root {} is not UTF-8", file.root.display()))?;
        let path = file
            .path
            .to_str()
            .ok_or_else(|| format!("support path {} is not UTF-8", file.path.display()))?;
        let prefix = file.prune_prefix.as_deref().unwrap_or_default();
        if prefix.contains('/') || prefix.contains(['\r', '\n']) {
            return Err("remote support prune prefix contains a path separator".to_string());
        }
        let command = host.command(crate::host::RemoteCommand {
            program: "sh",
            args: vec![
                "-c".to_string(),
                REMOTE_SUPPORT_FILE_SCRIPT.to_string(),
                "qmux-support".to_string(),
                managed_root.clone(),
                root.to_string(),
                path.to_string(),
                format!("{:o}", file.mode),
                if file.create_new { "1" } else { "0" }.to_string(),
                prefix.to_string(),
            ],
            ..Default::default()
        });
        let output = remote_command_output(
            command,
            Some(file.contents.as_bytes().to_vec()),
            &format!("provision {path} on {}", host.label()),
        )?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(if detail.is_empty() {
                format!(
                    "failed to provision {path} on {}: {}",
                    host.label(),
                    output.status
                )
            } else {
                format!("failed to provision {path} on {}: {detail}", host.label())
            });
        }
    }
    Ok(())
}

fn materialize_remote_support_files_or_fallback(
    host: &Host,
    spec: &mut PtySpawnSpec,
) -> Result<(), String> {
    let Err(err) = materialize_remote_support_files(host, &spec.support_files) else {
        return Ok(());
    };
    let Some(fallback) = spec.support_file_fallback.take() else {
        return Err(err);
    };
    spec.args = fallback.args;
    spec.envs = fallback.envs;
    if let Some(key) = fallback.error_env_key {
        spec.envs.push((key, err));
    }
    spec.support_files.clear();
    Ok(())
}

/// Materializes support files for a command that will be exec'd inside an
/// already-running pane rather than spawned through [`spawn_pty`]. Shell agent
/// wrappers use this path: their preparation request is handled by the local
/// control server, but the returned command may execute in a remote tmux pane.
/// Keep the same relocation, CLI rewriting, ownership, and fallback semantics
/// as a fresh pane spawn.
pub(crate) fn materialize_in_pane_support_files(
    host: &Host,
    pane_id: &str,
    remote_scope: &str,
    plan: &mut CommandPlan,
) -> Result<(), String> {
    if host.is_local() {
        let Err(err) = materialize_support_files(&plan.support_files) else {
            return Ok(());
        };
        let Some(fallback) = plan.support_file_fallback.take() else {
            return Err(err);
        };
        plan.args = fallback.args;
        plan.envs = fallback.envs;
        if let Some(key) = fallback.error_env_key {
            plan.envs.push((key, err));
        }
        plan.support_files.clear();
        return Ok(());
    }
    relocate_remote_support_files(host, pane_id, remote_scope, plan)?;
    let Err(err) = materialize_remote_support_files(host, &plan.support_files) else {
        return Ok(());
    };
    let Some(fallback) = plan.support_file_fallback.take() else {
        return Err(err);
    };
    plan.args = fallback.args;
    plan.envs = fallback.envs;
    if let Some(key) = fallback.error_env_key {
        plan.envs.push((key, err));
    }
    plan.support_files.clear();
    Ok(())
}

fn spawn_portable_pty(
    state: &AppState,
    mut spec: PtySpawnSpec,
    native_surface: bool,
) -> Result<PaneInfo, String> {
    if let Some(remote) = spec.remote.take() {
        return spawn_remote_tmux(state, spec, remote, native_surface);
    }
    let base_envs = base_child_envs(state)?;
    materialize_support_files_or_fallback(&mut spec)?;
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
    scrub_inherited_qmux_context(&mut command);
    for (key, value) in base_envs {
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
        // Shell tabs get their worktree badge from a single git probe at
        // spawn; agent tabs leave this unset and rely on transcript tailing.
        active_workspace: match spec.kind {
            PaneKind::Shell => {
                crate::workspace::resolve_pane_workspace(spec.cwd.to_str().unwrap_or_default())
            }
            PaneKind::Agent => None,
        },
        remote_session: None,
        remote_connection: None,
        cols: initial_size.cols,
        rows: initial_size.rows,
        status: PaneStatus::Running,
        // A freshly spawned pane is immediately the group's most-recent, so the next
        // spawn into the group inherits its cwd even before the frontend's activation
        // stamp round-trips. Every real pane (shell and agent) flows through here.
        last_active_at: crate::state::now_millis(),
        recovered: spec.recovered,
        ssh_target: spec.ssh_target.clone(),
        // Deprecated compatibility field; flat tab layouts always use zero.
        depth: 0,
    };

    let runtime = PaneRuntime {
        info: pane.clone(),
        backend: PaneBackend::HostPty(HostPtyBackend {
            child: child.clone(),
            master,
            writer: writer.clone(),
            backlog: backlog.clone(),
            native_surface,
        }),
        cwd_observation_seq: 0,
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
        None,
    );
    start_child_watcher(state.clone(), pane_id, child, root_pid);

    Ok(pane)
}

struct RemoteReaderContext {
    controller: Arc<RemoteAttachmentController>,
    history: Arc<RemoteHistoryCheckpoint>,
    generation: u64,
    commands: crate::host::RemoteTmuxCommands,
}

fn spawn_remote_tmux(
    state: &AppState,
    mut spec: PtySpawnSpec,
    remote: RemoteSpawnSpec,
    native_surface: bool,
) -> Result<PaneInfo, String> {
    if let Err(err) = materialize_remote_support_files_or_fallback(&remote.host, &mut spec) {
        cleanup_remote_support_dir(&remote.host, remote.identity.support_dir.as_deref());
        return Err(err);
    }
    let pane_id = spec
        .pane_id
        .clone()
        .ok_or_else(|| "a remote pane requires a stable pane id".to_string())?;
    let remote_cwd = spec
        .cwd
        .to_str()
        .ok_or_else(|| "remote pane working directory is not valid UTF-8".to_string())?;
    let initial_size = resolved_initial_size(spec.initial_size);
    let remote_token = state.pane_remote_token(&pane_id)?;
    let commands = match remote.host.tmux_session_commands(
        &remote.identity,
        &state.config().socket_path.display().to_string(),
        &remote_token,
        remote_cwd,
        initial_size.cols,
        initial_size.rows,
        &spec.program,
        &spec.args,
        &spec.envs,
    ) {
        Ok(commands) => commands,
        Err(err) => {
            cleanup_remote_support_dir(&remote.host, remote.identity.support_dir.as_deref());
            return Err(err);
        }
    };
    if let Err(err) = ensure_remote_tmux_version(&commands.version_argv) {
        cleanup_remote_support(&commands);
        return Err(err);
    }
    create_remote_tmux_session(&commands)?;
    if let Err(err) = run_remote_argv(&commands.configure_argv, "configure remote tmux session") {
        cleanup_remote_launch(&commands);
        return Err(err);
    }
    let initial_history = match capture_remote_history(&commands.capture_argv) {
        Ok(history) => history,
        Err(err) => {
            cleanup_remote_launch(&commands);
            return Err(err);
        }
    };
    let history = RemoteHistoryCheckpoint::new(Vec::new());
    let backlog: SharedBacklog = Arc::new(Mutex::new(Default::default()));
    let initial_replay = terminalize_tmux_history(&history.delta(&initial_history));
    if !initial_replay.is_empty() {
        let mut pending = match backlog.lock() {
            Ok(pending) => pending,
            Err(_) => {
                cleanup_remote_launch(&commands);
                return Err(format!(
                    "pane {pane_id} backlog lock poisoned during remote launch"
                ));
            }
        };
        append_capped(&mut pending.buffer, &initial_replay);
    }
    if let Err(err) = persist_remote_history_checkpoint(state, &pane_id, &history, &initial_history)
    {
        cleanup_remote_launch(&commands);
        return Err(err);
    }
    if let Err(err) = run_remote_argv(
        &commands.forward_cleanup_argv,
        "remove stale remote hook socket",
    ) {
        cleanup_remote_launch(&commands);
        return Err(err);
    }
    let attachment = match spawn_remote_attachment(state, &commands.attach_argv, initial_size) {
        Ok(attachment) => attachment,
        Err(err) => {
            cleanup_remote_launch(&commands);
            let _ = remove_pane_scrollback(&state.config().workspace_root, &pane_id);
            return Err(err);
        }
    };
    let reader = match attachment
        .master
        .lock()
        .map_err(|_| format!("pane {pane_id} master lock poisoned"))
        .and_then(|master| {
            master
                .try_clone_reader()
                .map_err(|err| format!("failed to clone remote PTY reader: {err}"))
        }) {
        Ok(reader) => reader,
        Err(err) => {
            let _ = kill_child(&pane_id, attachment.child);
            cleanup_remote_launch(&commands);
            let _ = remove_pane_scrollback(&state.config().workspace_root, &pane_id);
            return Err(err);
        }
    };
    let controller = RemoteAttachmentController::new();
    let (generation, previous) = controller.begin_generation();
    debug_assert!(previous.is_none());
    if controller
        .install_if_current(generation, attachment.clone())
        .is_err()
    {
        let _ = kill_child(&pane_id, attachment.child);
        cleanup_remote_launch(&commands);
        let _ = remove_pane_scrollback(&state.config().workspace_root, &pane_id);
        return Err("remote attachment was superseded during initial launch".to_string());
    }

    let pane = PaneInfo {
        id: pane_id.clone(),
        title: spec.title,
        last_osc_title: spec.last_osc_title,
        kind: spec.kind,
        agent_id: spec.agent_id,
        group_id: spec.group_id,
        cwd: spec.cwd.display().to_string(),
        // Remote filesystem observations must never be resolved locally.
        active_workspace: None,
        remote_session: Some(remote.identity),
        remote_connection: Some(RemoteConnectionInfo {
            state: RemoteConnectionState::Connecting,
            message: None,
        }),
        cols: initial_size.cols,
        rows: initial_size.rows,
        status: PaneStatus::Running,
        last_active_at: crate::state::now_millis(),
        recovered: spec.recovered,
        ssh_target: spec.ssh_target.clone(),
        depth: 0,
    };
    let backend = RemoteTmuxBackend::new(
        controller.clone(),
        history.clone(),
        backlog.clone(),
        commands.clone(),
        native_surface,
    );
    let stable_writer = backend.writer.clone();
    if let Err(err) = state.insert_pane(PaneRuntime {
        info: pane.clone(),
        backend: PaneBackend::RemoteTmux(backend),
        cwd_observation_seq: 0,
    }) {
        let _ = controller.begin_generation();
        let _ = kill_child(&pane_id, attachment.child);
        cleanup_remote_launch(&commands);
        let _ = remove_pane_scrollback(&state.config().workspace_root, &pane_id);
        return Err(err);
    }

    crate::remote_transcript::restore(state, &pane_id);

    if native_surface {
        if let Err(err) = crate::native_terminal::create_host_managed(&pane_id, None) {
            let _ = controller.begin_generation();
            let _ = kill_child(&pane_id, attachment.child);
            cleanup_remote_launch(&commands);
            let _ = state.remove_pane(&pane_id);
            return Err(err);
        }
        register_native_input_writer(&pane_id, stable_writer);
    }
    start_reader_thread(
        state.clone(),
        pane_id,
        reader,
        backlog,
        native_surface,
        Some(RemoteReaderContext {
            controller,
            history,
            generation,
            commands,
        }),
    );
    Ok(pane)
}

fn spawn_remote_attachment(
    state: &AppState,
    attach_argv: &[String],
    initial_size: InitialPaneSize,
) -> Result<RemoteAttachment, String> {
    let (program, args) = attach_argv
        .split_first()
        .ok_or_else(|| "remote attach command is empty".to_string())?;
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: initial_size.rows,
            cols: initial_size.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| format!("failed to open remote attachment PTY: {err}"))?;
    let mut command = CommandBuilder::new(program);
    command.args(args);
    command.cwd(state.default_open_dir());
    scrub_inherited_qmux_context(&mut command);
    command.env("TERM", "xterm-256color");
    let writer = pair
        .master
        .take_writer()
        .map_err(|err| format!("failed to open remote PTY writer: {err}"))?;
    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|err| format!("failed to spawn remote tmux attachment: {err}"))?;
    drop(pair.slave);
    Ok(RemoteAttachment {
        child: Arc::new(Mutex::new(child)),
        master: Arc::new(Mutex::new(pair.master)),
        writer: Arc::new(Mutex::new(writer)),
    })
}

/// Runs one non-interactive SSH helper with a wall-clock deadline while
/// draining both output pipes concurrently. `ConnectTimeout` alone covers only
/// connection establishment; this also bounds a server that accepts the TCP
/// connection and then stops responding. Supplying stdin on its own thread
/// prevents a large support-file upload from blocking the launch thread before
/// the deadline can be enforced.
pub(crate) fn remote_command_output(
    command: Command,
    input: Option<Vec<u8>>,
    action: &str,
) -> Result<Output, String> {
    remote_command_output_with_timeout(command, input, action, REMOTE_BATCH_TIMEOUT)
}

pub(crate) fn remote_command_output_with_timeout(
    mut command: Command,
    input: Option<Vec<u8>>,
    action: &str,
    timeout: Duration,
) -> Result<Output, String> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|err| format!("failed to {action}: {err}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("failed to capture output while trying to {action}"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("failed to capture errors while trying to {action}"))?;
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stdout = stdout;
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stderr = stderr;
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let input_writer = input.map(|bytes| {
        let mut stdin = child.stdin.take();
        thread::spawn(move || -> Result<(), String> {
            stdin
                .as_mut()
                .ok_or_else(|| "failed to open remote command stdin".to_string())?
                .write_all(&bytes)
                .map_err(|err| format!("failed to upload remote command input: {err}"))
        })
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(writer) = input_writer {
                    let _ = writer.join();
                }
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!(
                    "failed to {action}: timed out after {} seconds",
                    timeout.as_secs_f64()
                ));
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(writer) = input_writer {
                    let _ = writer.join();
                }
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("failed to {action}: {err}"));
            }
        }
    };
    if let Some(writer) = input_writer {
        writer
            .join()
            .map_err(|_| format!("failed to {action}: input writer panicked"))??;
    }
    let stdout = stdout_reader
        .join()
        .map_err(|_| format!("failed to {action}: output reader panicked"))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| format!("failed to {action}: error reader panicked"))?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn run_remote_argv(argv: &[String], action: &str) -> Result<(), String> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| format!("cannot {action}: command is empty"))?;
    let mut command = Command::new(program);
    command.args(args);
    let output = remote_command_output(command, None, action)?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if detail.is_empty() {
        format!("failed to {action}: {}", output.status)
    } else {
        format!("failed to {action}: {detail}")
    })
}

fn create_remote_tmux_session(commands: &crate::host::RemoteTmuxCommands) -> Result<(), String> {
    if let Err(err) = run_remote_argv(&commands.create_argv, "create remote tmux session") {
        // A failed or timed-out SSH helper has an uncertain remote result: tmux
        // may have created its detached session before the acknowledgement was
        // lost. The nonce-named target belongs only to this launch, so always
        // issue the exact compensating kill before returning the original error.
        cleanup_remote_launch(commands);
        return Err(err);
    }
    Ok(())
}

fn cleanup_remote_launch(commands: &crate::host::RemoteTmuxCommands) {
    let _ = run_remote_argv(&commands.kill_argv, "clean up remote tmux session");
    cleanup_remote_support(commands);
}

fn cleanup_remote_support(commands: &crate::host::RemoteTmuxCommands) {
    if !commands.support_cleanup_argv.is_empty()
        && let Err(err) = run_remote_argv(
            &commands.support_cleanup_argv,
            "remove remote pane support files",
        )
    {
        eprintln!("qmux: {err}");
    }
}

fn cleanup_remote_support_dir(host: &Host, support_dir: Option<&str>) {
    let Some(support_dir) = support_dir else {
        return;
    };
    let argv = match host.remote_support_cleanup_argv(support_dir) {
        Ok(argv) => argv,
        Err(err) => {
            eprintln!("qmux: refused unsafe remote support cleanup: {err}");
            return;
        }
    };
    if let Err(err) = run_remote_argv(&argv, "remove remote pane support files") {
        eprintln!("qmux: {err}");
    }
}

fn capture_remote_history(argv: &[String]) -> Result<Vec<u8>, String> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| "remote history capture command is empty".to_string())?;
    let mut command = Command::new(program);
    command.args(args);
    let output = remote_command_output(command, None, "capture remote tmux history")?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if detail.is_empty() {
        format!("failed to capture remote tmux history: {}", output.status)
    } else {
        format!("failed to capture remote tmux history: {detail}")
    })
}

fn persist_remote_history_checkpoint(
    state: &AppState,
    pane_id: &str,
    history: &RemoteHistoryCheckpoint,
    capture: &[u8],
) -> Result<(), String> {
    let tail = history.advance(capture);
    write_remote_history_checkpoint(&state.config().workspace_root, pane_id, &tail)
}

fn synchronize_remote_history(
    state: &AppState,
    pane_id: &str,
    history: &RemoteHistoryCheckpoint,
    capture_argv: &[String],
    capture_full_argv: &[String],
    backlog: &SharedBacklog,
    native_surface: bool,
) -> Result<(), String> {
    let mut capture = capture_remote_history(capture_argv)?;
    if !history.has_trustworthy_overlap(&capture) {
        // The ordinary probe is capped to the most recent 8192 lines. Fall
        // back to the full configured tmux history only when rollover or an
        // unusually large disconnected burst erased every safe overlap.
        capture = capture_remote_history(capture_full_argv)?;
    }
    let delta = history.delta(&capture);
    if !delta.is_empty() {
        let replay = terminalize_tmux_history(&delta);
        let live = match backlog.lock() {
            Ok(mut backlog) => {
                if backlog.ready {
                    true
                } else {
                    append_capped(&mut backlog.buffer, &replay);
                    false
                }
            }
            Err(_) => true,
        };
        if live {
            if native_surface {
                crate::native_terminal::receive(pane_id, &replay, true)?;
            }
            record_scrollback(state, pane_id, &replay);
        }
    }
    // Advance only after the delta has been accepted by the surface/backlog.
    // A checkpoint write failure is a recovery-quality failure, not a reason to
    // tear down a healthy attachment; retain the in-memory cursor and report it.
    let tail = history.advance(&capture);
    if let Err(err) =
        write_remote_history_checkpoint(&state.config().workspace_root, pane_id, &tail)
    {
        eprintln!("qmux: failed to persist remote history checkpoint for pane {pane_id}: {err}");
    }
    Ok(())
}

/// `capture-pane -p` separates rows with LF, while a terminal LF preserves the
/// current column. Turn those row boundaries into CRLF before replay so history
/// starts in column zero without altering an existing CRLF pair.
fn terminalize_tmux_history(capture: &[u8]) -> Vec<u8> {
    let mut replay = Vec::with_capacity(capture.len() + capture.len() / 40);
    let mut previous = None;
    for byte in capture {
        if *byte == b'\n' && previous != Some(b'\r') {
            replay.push(b'\r');
        }
        replay.push(*byte);
        previous = Some(*byte);
    }
    replay
}

fn probe_remote_session(argv: &[String]) -> Result<bool, String> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| "remote session probe command is empty".to_string())?;
    let mut command = Command::new(program);
    command.args(args);
    let output = remote_command_output(command, None, "check remote session")?;
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    match output.status.code() {
        Some(0) => Ok(true),
        // Exit 1 is also used for permission/config/socket failures. Only the
        // two tmux diagnostics that actually mean absence may delete pane state.
        Some(1)
            if detail.contains("can't find session") || detail.contains("no server running") =>
        {
            Ok(false)
        }
        _ => Err(if detail.is_empty() {
            format!("could not check remote session: {}", output.status)
        } else {
            format!("could not check remote session: {detail}")
        }),
    }
}

fn ensure_remote_tmux_version(argv: &[String]) -> Result<(), String> {
    let cache_key = argv.join("\u{1f}");
    if VALIDATED_TMUX_VERSIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(&cache_key)
    {
        return Ok(());
    }
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| "remote tmux version command is empty".to_string())?;
    let mut command = Command::new(program);
    command.args(args);
    let output = remote_command_output(command, None, "check remote tmux version")?;
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            format!("failed to check remote tmux version: {}", output.status)
        } else {
            format!("failed to check remote tmux version: {detail}")
        });
    }
    validate_remote_tmux_version(&raw)?;
    VALIDATED_TMUX_VERSIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(cache_key);
    Ok(())
}

pub(crate) fn validate_remote_tmux_version(raw: &str) -> Result<(), String> {
    let version = raw
        .strip_prefix("tmux ")
        .and_then(|version| {
            let numeric = version
                .trim()
                .trim_end_matches(|character: char| character.is_ascii_alphabetic());
            let (major, minor) = numeric.split_once('.')?;
            Some((major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?))
        })
        .ok_or_else(|| format!("remote host returned an unrecognized tmux version: {raw:?}"))?;
    if version < (3, 2) {
        return Err(format!(
            "remote panes require tmux 3.2 or newer (remote reported {raw})"
        ));
    }
    Ok(())
}

const REMOTE_RECONNECT_INITIAL_DELAY: Duration = Duration::from_millis(250);
const REMOTE_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(10);

fn schedule_remote_reconnect(
    state: AppState,
    pane_id: String,
    controller: Arc<RemoteAttachmentController>,
    history: Arc<RemoteHistoryCheckpoint>,
    commands: crate::host::RemoteTmuxCommands,
    backlog: SharedBacklog,
    native_surface: bool,
    message: String,
) {
    let _ = state.update_remote_connection(
        &pane_id,
        RemoteConnectionState::Reconnecting,
        Some(message),
    );
    thread::spawn(move || {
        loop {
            let delay = controller
                .next_reconnect_delay(REMOTE_RECONNECT_INITIAL_DELAY, REMOTE_RECONNECT_MAX_DELAY);
            thread::sleep(delay);
            let Ok(Some((current, current_history, _))) = state.pane_remote_control(&pane_id)
            else {
                return;
            };
            if !Arc::ptr_eq(&current, &controller) || !Arc::ptr_eq(&current_history, &history) {
                return;
            }
            match attach_remote_generation(
                &state,
                &pane_id,
                controller.clone(),
                history.clone(),
                commands.clone(),
                backlog.clone(),
                native_surface,
            ) {
                Ok(()) => return,
                Err(err) => {
                    let _ = state.update_remote_connection(
                        &pane_id,
                        RemoteConnectionState::Reconnecting,
                        Some(err),
                    );
                }
            }
        }
    });
}

fn attach_remote_generation(
    state: &AppState,
    pane_id: &str,
    controller: Arc<RemoteAttachmentController>,
    history: Arc<RemoteHistoryCheckpoint>,
    commands: crate::host::RemoteTmuxCommands,
    backlog: SharedBacklog,
    native_surface: bool,
) -> Result<(), String> {
    let pane = state
        .list_panes()?
        .into_iter()
        .find(|pane| pane.id == pane_id)
        .ok_or_else(|| format!("pane {pane_id} was not found"))?;
    let initial_size = resolved_initial_size(Some(InitialPaneSize {
        cols: pane.cols,
        rows: pane.rows,
    }));
    run_remote_argv(&commands.configure_argv, "configure remote tmux session")?;
    crate::remote_transcript::restore(state, pane_id);
    synchronize_remote_history(
        state,
        pane_id,
        &history,
        &commands.capture_argv,
        &commands.capture_full_argv,
        &backlog,
        native_surface,
    )?;
    let (generation, previous) = controller.begin_generation();
    if let Some(previous) = previous {
        let _ = kill_child(pane_id, previous.child);
    }
    run_remote_argv(
        &commands.forward_cleanup_argv,
        "remove stale remote hook socket",
    )?;
    let attachment = spawn_remote_attachment(state, &commands.attach_argv, initial_size)?;
    let reader = match attachment
        .master
        .lock()
        .map_err(|_| format!("pane {pane_id} master lock poisoned"))
        .and_then(|master| {
            master
                .try_clone_reader()
                .map_err(|err| format!("failed to clone remote PTY reader: {err}"))
        }) {
        Ok(reader) => reader,
        Err(err) => {
            let _ = kill_child(pane_id, attachment.child);
            return Err(err);
        }
    };
    if let Err(stale) = controller.install_if_current(generation, attachment) {
        let _ = kill_child(pane_id, stale.child);
        return Err("remote attachment was superseded before installation".to_string());
    }
    start_reader_thread(
        state.clone(),
        pane_id.to_string(),
        reader,
        backlog,
        native_surface,
        Some(RemoteReaderContext {
            controller,
            history,
            generation,
            commands,
        }),
    );
    Ok(())
}

/// A qmux process can itself be launched from inside another qmux pane. The PTY
/// command builder inherits that outer process environment, so remove every
/// pane/session credential before applying this pane's freshly-derived envs.
/// Shell panes receive their new user credential below; agent panes never do.
fn scrub_inherited_qmux_context(command: &mut CommandBuilder) {
    const CONTEXT_KEYS: &[&str] = &[
        "QMUX_ENV",
        "QMUX_PANE_ID",
        "QMUX_AGENT_ID",
        "QMUX_ADAPTER_ID",
        "QMUX_TOKEN",
        "QMUX_USER_TOKEN",
        "QMUX_SOCK",
        "QMUX_CLI",
        "QMUX_WORKSPACE_ROOT",
        "QMUX_SHELL_INTEGRATION",
        "QMUX_PREPARED_AGENT_ID",
        "QMUX_FORK_POINT",
        "QMUX_ROOT_SESSION_ID",
        "QMUX_AGENT_FUNCTIONS",
        "QMUX_AGENT_FUNCTIONS_ERROR",
        "QMUX_ORIGINAL_ZDOTDIR",
        "QMUX_ORIGINAL_BASHRC",
    ];
    for key in CONTEXT_KEYS {
        command.env_remove(key);
    }
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

/// Makes a live pane usable by its shell after a foreground TUI is stopped or
/// moved into the background. Unlike `reset_pane_terminal_modes`, this does not
/// write the full reset to durable scrollback: the agent is still alive and may
/// resume its existing alternate-screen session with `fg`. Only the live,
/// cursor-neutral input/reporting reset is sent to the renderer; a well-behaved
/// TUI re-enables its preferred modes when it handles SIGCONT.
pub fn reset_live_pane_terminal_modes(state: &AppState, pane_id: &str) -> Result<(), String> {
    let Some(native_surface) = state.pane_is_native(pane_id)? else {
        return Ok(());
    };
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
            || write_acknowledged_native_host_input(&options.pane_id, SUBMIT_KEY),
            || flush_native_host_input(&options.pane_id),
            NATIVE_SUBMIT_TIMING,
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
    submit: impl FnOnce() -> Result<(), String>,
    submit_bytes: impl FnOnce() -> Result<(), String>,
    mut flush_input: impl FnMut() -> Result<u64, String>,
    timing: NativeSubmitTiming,
) -> Result<(), PaneWriteFailure> {
    let before_data = flush_input().map_err(PaneWriteFailure::before_data)?;
    emit_data().map_err(PaneWriteFailure::before_data)?;
    // The action being accepted does not mean Ghostty's deferred write callbacks have
    // finished. Wait for input to arrive and then for the PTY position to stay still:
    // the first advance may be only the opening paste marker or body, and treating a
    // later paste chunk as Return would produce a false submit acknowledgement.
    let mut after_data = flush_input().map_err(PaneWriteFailure::after_data)?;
    if !data.is_empty() {
        let mut observed_data = after_data > before_data;
        let mut quiet_rechecks = 0;
        for _ in 0..timing.data_max_rechecks {
            if observed_data && quiet_rechecks >= timing.data_quiet_rechecks {
                break;
            }
            if !timing.data_poll_interval.is_zero() {
                thread::sleep(timing.data_poll_interval);
            }
            let position = flush_input().map_err(PaneWriteFailure::after_data)?;
            if position > after_data {
                observed_data = true;
                quiet_rechecks = 0;
                after_data = position;
            } else if observed_data {
                quiet_rechecks += 1;
            }
        }

        if !observed_data {
            return Err(PaneWriteFailure::before_data(
                "native terminal accepted input action but emitted no PTY input".to_string(),
            ));
        }
        if quiet_rechecks < timing.data_quiet_rechecks {
            eprintln!(
                "qmux: native paste input did not become quiescent after {} rechecks; \
                 submitting from the latest observed PTY boundary",
                timing.data_max_rechecks
            );
        }
    }

    if !timing.submit_key_delay.is_zero() {
        thread::sleep(timing.submit_key_delay);
    }
    let synthetic_submit_error = match submit() {
        Ok(()) => {
            // Send the Ghostty-encoded key only once. Polling gives its deferred
            // callback time to arrive without risking multiple late Returns.
            for recheck in 0..=timing.submit_max_rechecks {
                if flush_input().map_err(PaneWriteFailure::after_data)? > after_data {
                    return Ok(());
                }
                if recheck < timing.submit_max_rechecks && !timing.submit_poll_interval.is_zero() {
                    thread::sleep(timing.submit_poll_interval);
                }
            }
            None
        }
        Err(err) => Some(err),
    };

    // The correctly encoded key either failed or produced no observable bytes.
    // Queue a raw carriage return directly behind all prior native input and wait
    // for the writer's barrier; success now acknowledges this exact fallback write,
    // rather than inferring it from an unrelated cumulative-position increase.
    match submit_bytes() {
        Ok(()) => {
            if let Some(err) = synthetic_submit_error {
                eprintln!(
                    "qmux: native synthetic submit failed ({err}); delivered the raw submit byte \
                     through the pane writer instead"
                );
            } else {
                eprintln!(
                    "qmux: native synthetic submit emitted no acknowledged PTY input; delivered \
                     the raw submit byte through the pane writer instead"
                );
            }
            Ok(())
        }
        Err(raw_err) => {
            let error = synthetic_submit_error.map_or_else(
                || {
                    format!(
                        "native terminal emitted no PTY input for Return and the raw submit \
                         fallback failed: {raw_err}"
                    )
                },
                |synthetic_err| {
                    format!(
                        "native terminal submit failed: {synthetic_err}; raw submit fallback \
                         failed: {raw_err}"
                    )
                },
            );
            Err(PaneWriteFailure::after_data(error))
        }
    }
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
    // Some agent TUIs nested under agent-exec do not reliably wake from the
    // SIGWINCH that TIOCSWINSZ is documented to generate. Resolve whether an
    // agent owns this pane before taking the master lock so this path never
    // inverts the model -> PTY lock order. Do not send a second WINCH to an
    // ordinary interactive shell: zsh redraws its live prompt in response,
    // and that output can race Ghostty's screen-height resize and displace the
    // prompt/viewport when a split is created.
    #[cfg(target_os = "macos")]
    let explicitly_notify_resize = state
        .agent_by_pane(&pane_id)
        .ok()
        .flatten()
        .is_some_and(|agent| agent_needs_explicit_resize_signal(Some(&agent.adapter)));
    let Some(master) = state.pane_master(&pane_id)? else {
        // A remote pane deliberately has no local master between attachment
        // generations. Persist the desired geometry so the next SSH PTY is
        // created at the latest size rather than treating "offline" as
        // "missing" and permanently dropping this resize.
        if state.pane_exists(&pane_id)? {
            return state.update_pane_size(&pane_id, cols, rows);
        }
        return Err(format!("pane {pane_id} was not found"));
    };
    let master = master
        .lock()
        .map_err(|_| format!("pane {pane_id} master lock poisoned"))?;
    // A no-op TIOCSWINSZ is not free: some kernels still SIGWINCH, and a
    // duplicate report from Ghostty's session + surface callbacks would
    // wake a TUI for a size it already has.
    let size_changed = master
        .get_size()
        .map(|size| size.cols != cols || size.rows != rows)
        .unwrap_or(true);
    let resize_result = if size_changed {
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| format!("failed to resize pane {pane_id}: {err}"))
    } else {
        Ok(())
    };
    #[cfg(target_os = "macos")]
    if resize_result.is_ok()
        && size_changed
        && explicitly_notify_resize
        && let Some(process_group) = master.process_group_leader()
    {
        // Signal the foreground group, not just the pane's direct child. An
        // agent launched from a shell pane runs below qmux's agent-exec
        // supervisor, while a dedicated agent pane has the same process as
        // its group leader. SIGWINCH is ignored by default, so an exit/detach
        // race is harmless.
        let result = unsafe { libc::kill(-process_group, libc::SIGWINCH) };
        if result != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                eprintln!(
                    "qmux: failed to explicitly notify agent pane {pane_id} of resize: {err}"
                );
            }
        }
    }
    drop(master);
    // Persist even when an attachment vanished during the ioctl. Reconnect
    // will apply this geometry to the replacement PTY; the caller still gets
    // the immediate resize error for observability.
    state.update_pane_size(&pane_id, cols, rows)?;
    resize_result
}

#[cfg(target_os = "macos")]
fn agent_needs_explicit_resize_signal(adapter_id: Option<&str>) -> bool {
    adapter_id.is_some()
}

pub fn resize_native_host_pane(
    state: &AppState,
    pane_id: &str,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    // Ghostty-driven callers (NativeTerminalPane) already delayed this until
    // after the new IOSurface present. Keep the ioctl itself synchronous:
    // `pane_resize` and other non-Ghostty callers must not wait on a frame.
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

/// Writes bytes through a native pane's ordered input worker and waits until the
/// worker has written and flushed that exact preceding message. Unlike comparing
/// cumulative writer positions around a Ghostty action, the successful barrier
/// cannot be satisfied by unrelated user input: FIFO ordering guarantees these
/// bytes were processed before the acknowledgement was sent.
fn write_acknowledged_native_host_input(pane_id: &str, bytes: &[u8]) -> Result<(), String> {
    let sender = NATIVE_INPUT_SENDERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(pane_id)
        .cloned()
        .ok_or_else(|| format!("native input writer for pane {pane_id} is unavailable"))?;
    let (acknowledge, result) = std::sync::mpsc::sync_channel(0);
    sender
        .send(NativeInputMessage::Data(bytes.to_vec()))
        .map_err(|_| format!("native input writer for pane {pane_id} stopped before write"))?;
    sender
        .send(NativeInputMessage::Flush(acknowledge))
        .map_err(|_| format!("native input writer for pane {pane_id} stopped before flush"))?;
    result
        .recv_timeout(NATIVE_INPUT_FLUSH_TIMEOUT)
        .map_err(|_| format!("timed out flushing native input for pane {pane_id}"))??;
    Ok(())
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
    if let Some((_, _, commands)) = state.pane_remote_control(&pane_id)? {
        let (program, args) = commands
            .activity_argv
            .split_first()
            .ok_or_else(|| "remote pane activity command is empty".to_string())?;
        let mut command = Command::new(program);
        command.args(args);
        let output = remote_command_output(command, None, "inspect remote pane activity")?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(if detail.is_empty() {
                format!("failed to inspect remote pane activity: {}", output.status)
            } else {
                format!("failed to inspect remote pane activity: {detail}")
            });
        }
        let raw = String::from_utf8_lossy(&output.stdout);
        let mut fields = raw.trim().split('\t');
        let command = fields.next().unwrap_or_default().trim();
        let _pane_pid = fields.next().unwrap_or_default();
        let dead = fields.next().unwrap_or_default().trim() == "1";
        let idle_shell = matches!(
            command,
            "sh" | "bash" | "dash" | "zsh" | "fish" | "ksh" | "tcsh" | "csh" | "nu"
        );
        return if dead || idle_shell || command.is_empty() {
            Ok(PaneActivity::idle())
        } else {
            Ok(PaneActivity::running_process(1, Some(command.to_string())))
        };
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
    let pane_agent_id = state.agent_by_pane(&pane_id)?.map(|agent| agent.id);
    if let Err(err) = state.capture_last_closed_pane(&pane_id) {
        eprintln!("qmux: failed to capture closed pane {pane_id}: {err}");
    }
    if let Some((controller, _, commands)) = state.pane_remote_control(&pane_id)? {
        if let Err(err) = run_remote_argv(&commands.kill_argv, "kill remote tmux session")
            && probe_remote_session(&commands.probe_argv) != Ok(false)
        {
            state.clear_last_closed_pane_for_pane(&pane_id);
            let _ = state.update_remote_connection(
                &pane_id,
                RemoteConnectionState::Failed,
                Some(err.clone()),
            );
            return Err(err);
        }
        // Fence and tear down the disposable attachment only after the remote
        // session is confirmed gone. If the kill/probe transport failed, leave
        // the live generation installed so the user can keep working or retry
        // close instead of stranding a Failed pane with no reconnect path.
        let (_, attachment) = controller.begin_generation();
        if let Some(attachment) = attachment {
            let _ = kill_child(&pane_id, attachment.child);
        }
        cleanup_remote_support(&commands);
        if let Some(agent_id) = pane_agent_id.as_deref()
            && let Err(err) = abort_fork_barrier_for_child(
                state,
                agent_id,
                "Forked terminal exited before its initial prompt was accepted",
            )
        {
            eprintln!("qmux: failed to release fork barrier for closed agent {agent_id}: {err}");
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
        return Ok(());
    }
    let child = state
        .pane_child(&pane_id)?
        .ok_or_else(|| format!("pane {pane_id} was not found"))?;
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
    if let Some(agent_id) = pane_agent_id.as_deref()
        && let Err(err) = abort_fork_barrier_for_child(
            state,
            agent_id,
            "Forked terminal exited before its initial prompt was accepted",
        )
    {
        eprintln!("qmux: failed to release fork barrier for closed agent {agent_id}: {err}");
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
    if (state.pane_has_host_pty(pane_id).ok().flatten() == Some(true)
        || state.pane_remote_control(pane_id).ok().flatten().is_some())
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
    // Keep each pane's established signal, descendant walk, escalation, and reap
    // sequence intact, but overlap those sequences across panes. portable-pty can
    // spend up to 200ms waiting for one direct child to honor SIGHUP before it
    // escalates; doing that serially made clean-exit latency scale with the number
    // of stubborn panes. The scoped threads are all joined before returning, so the
    // app still completes every best-effort teardown before its process exits.
    for_each_concurrently(children, |(pane_id, child)| {
        if let Err(err) = kill_child(&pane_id, child) {
            eprintln!("qmux: failed to kill pane {pane_id} on exit: {err}");
        }
        // The reader-thread EOF path that normally removes these dirs won't run once
        // the process is exiting, so clean them up here instead of leaking them into
        // /tmp until the OS clears it.
        remove_shell_integration_dir(&pane_id);
    });
}

fn for_each_concurrently<T, F>(items: Vec<T>, action: F)
where
    T: Send,
    F: Fn(T) + Sync,
{
    thread::scope(|scope| {
        for item in items {
            let action = &action;
            scope.spawn(move || action(item));
        }
    });
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

/// Color probes buffered before attachment cannot be answered promptly. A late
/// OSC reply can become editor input (ESC ] even opens an editor in some TUIs).
/// Drop only those probes, preserving rendering and live queries. Keep partial
/// matches across reads, including a probe straddling the attach boundary.
#[derive(Default)]
struct StartupColorQueries {
    pending: Vec<u8>,
}

impl StartupColorQueries {
    fn filter<'a>(&mut self, bytes: &'a [u8], buffering: bool) -> Cow<'a, [u8]> {
        const QUERIES: &[&[u8]] = &[
            b"\x1b]10;?\x07",
            b"\x1b]10;?\x1b\\",
            b"\x1b]11;?\x07",
            b"\x1b]11;?\x1b\\",
            b"\x1b]12;?\x07",
            b"\x1b]12;?\x1b\\",
        ];
        if !buffering && self.pending.is_empty() {
            return Cow::Borrowed(bytes);
        }
        let mut output = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            if self.pending.is_empty() && !buffering {
                output.push(byte);
                continue;
            }
            self.pending.push(byte);
            loop {
                if QUERIES.contains(&self.pending.as_slice()) {
                    self.pending.clear();
                    break;
                }
                if QUERIES.iter().any(|query| query.starts_with(&self.pending)) {
                    break;
                }
                output.push(self.pending.remove(0));
                if self.pending.is_empty() {
                    break;
                }
            }
        }
        Cow::Owned(output)
    }
}

#[cfg_attr(all(target_os = "macos", not(test)), allow(dead_code))]
fn start_reader_thread(
    state: AppState,
    pane_id: String,
    mut reader: Box<dyn Read + Send>,
    backlog: SharedBacklog,
    native_surface: bool,
    remote: Option<RemoteReaderContext>,
) {
    thread::spawn(move || {
        // 64KB per read: every chunk pays fixed costs beyond the syscall — the
        // durable scrollback append and, for native surfaces, the FFI handoff
        // with its buffer copy — so at the old 8KB a bulk producer (builds,
        // `cat` of a large file) paid that overhead 8x as often. Heap-allocated
        // to keep the reader thread's stack frame small.
        let mut buffer = vec![0_u8; 64 * 1024];
        let mut remote_confirmed = false;
        let mut startup_color_queries = StartupColorQueries::default();
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let chunk = &buffer[..count];
                    if !remote_confirmed
                        && let Some(remote) = remote.as_ref()
                        && remote
                            .controller
                            .mark_attachment_live_if_current(remote.generation)
                    {
                        let _ = state.update_remote_connection(
                            &pane_id,
                            RemoteConnectionState::Connected,
                            None,
                        );
                        remote_confirmed = true;
                    }
                    // Hold the backlog lock only long enough to decide; emitting
                    // live happens after releasing it. `attach_pane` flips `ready`
                    // (and drains the buffer) under the same lock, so no chunk is
                    // ever both buffered and emitted, and order is preserved.
                    let (live, chunk) = match backlog.lock() {
                        Ok(mut backlog) => {
                            let chunk = startup_color_queries
                                .filter(chunk, native_surface && !backlog.ready);
                            if backlog.ready {
                                (true, chunk)
                            } else {
                                append_capped(&mut backlog.buffer, &chunk);
                                (false, chunk)
                            }
                        }
                        Err(_) => (true, startup_color_queries.filter(chunk, false)),
                    };
                    let chunk = chunk.as_ref();
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
        let exit_code = if let Some(remote) = remote {
            let Some(attachment) = remote.controller.clear_if_current(remote.generation) else {
                // A newer attachment owns the pane. This generation's late EOF
                // must not alter its state or remove its surface.
                return;
            };
            let exit_code = attachment
                .child
                .lock()
                .ok()
                .and_then(|mut child| child.wait().ok())
                .map(|status| status.exit_code() as i32);
            // Reconcile before probing/retrying. The remote program continues
            // while these batch SSH calls run, so merely advancing the cursor at
            // EOF could mark post-disconnect output as seen even though it never
            // traversed the dead attachment. Accepting the capture into local
            // history first favors a possible repeated connected line over
            // silently losing a disconnected one.
            let _ = synchronize_remote_history(
                &state,
                &pane_id,
                &remote.history,
                &remote.commands.capture_argv,
                &remote.commands.capture_full_argv,
                &backlog,
                native_surface,
            );
            match probe_remote_session(&remote.commands.probe_argv) {
                Ok(true) => {
                    schedule_remote_reconnect(
                        state.clone(),
                        pane_id.clone(),
                        remote.controller,
                        remote.history,
                        remote.commands,
                        backlog.clone(),
                        native_surface,
                        "SSH connection closed; reconnecting".to_string(),
                    );
                    return;
                }
                Err(err) => {
                    schedule_remote_reconnect(
                        state.clone(),
                        pane_id.clone(),
                        remote.controller,
                        remote.history,
                        remote.commands,
                        backlog.clone(),
                        native_surface,
                        err,
                    );
                    return;
                }
                Ok(false) => {
                    cleanup_remote_support(&remote.commands);
                    exit_code
                }
            }
        } else {
            reap_pane_child(&state, &pane_id)
        };
        let pane_agent_id = state
            .agent_by_pane(&pane_id)
            .ok()
            .flatten()
            .map(|agent| agent.id);
        if let Some(agent_id) = pane_agent_id.as_deref()
            && let Err(err) = abort_fork_barrier_for_child(
                &state,
                agent_id,
                "Forked terminal exited before its initial prompt was accepted",
            )
        {
            eprintln!("qmux: failed to release fork barrier for exited agent {agent_id}: {err}");
        }
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
pub(crate) fn descendant_process_ids(pid: u32) -> Vec<u32> {
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
        MuseAdapterConfig, OpencodeAdapterConfig, QmuxConfig,
    };
    use crate::scrollback::read_pane_scrollback;
    use crate::workspace::{AgentInfo, AgentStatus, GroupInfo, WorkspaceScope};
    use std::cell::{Cell, RefCell};
    use std::io;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn startup_color_queries_drop_probes_across_every_read_boundary() {
        let input = b"\x1b[?1049h\x1b]10;?\x07\x1b]11;?\x1b\\\x1b]12;?\x07\x1b[31mDevin\x1b[0m";
        let expected = b"\x1b[?1049h\x1b[31mDevin\x1b[0m";
        for split in 0..=input.len() {
            let mut filter = StartupColorQueries::default();
            let mut output = filter.filter(&input[..split], true).into_owned();
            output.extend_from_slice(&filter.filter(&input[split..], true));
            assert_eq!(output, expected, "split {split}");
        }
    }

    #[test]
    fn startup_color_queries_finish_pending_probe_after_attach_but_keep_live_queries() {
        let query = b"\x1b]11;?\x1b\\";
        for split in 1..query.len() {
            let mut filter = StartupColorQueries::default();
            assert!(filter.filter(&query[..split], true).is_empty());
            let mut live = query[split..].to_vec();
            live.extend_from_slice(query);
            live.extend_from_slice(b"prompt");
            let mut expected = query.to_vec();
            expected.extend_from_slice(b"prompt");
            assert_eq!(
                filter.filter(&live, false).as_ref(),
                expected,
                "split {split}"
            );
        }
    }

    #[test]
    fn startup_color_queries_preserve_colors_titles_modes_and_non_queries() {
        let input = b"\x1b]10;rgb:ffff/0000/0000\x07\x1b]2;Devin\x07\x1b[?2004h\x1b]110;?\x07text";
        let mut filter = StartupColorQueries::default();
        let mut output = Vec::new();
        for byte in input {
            output.extend_from_slice(&filter.filter(std::slice::from_ref(byte), true));
        }
        assert_eq!(output, input);
        let live_query = b"\x1b]10;?\x07";
        assert_eq!(filter.filter(live_query, false).as_ref(), live_query);
    }

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

    #[cfg(target_os = "macos")]
    #[test]
    fn only_agent_panes_need_an_explicit_resize_signal() {
        assert!(agent_needs_explicit_resize_signal(Some("grok")));
        assert!(agent_needs_explicit_resize_signal(Some("codex")));
        assert!(agent_needs_explicit_resize_signal(Some("claude")));
        assert!(agent_needs_explicit_resize_signal(Some("future-adapter")));
        assert!(!agent_needs_explicit_resize_signal(None));
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

        // A job-control handoff touches only the live renderer. The still-live
        // TUI may resume its alternate screen, so its durable log must remain
        // untouched.
        reset_live_pane_terminal_modes(&state, &pane.id).unwrap();
        assert!(
            read_pane_scrollback(&workspace, &pane.id)
                .unwrap()
                .is_empty()
        );

        // The pane was never attached, so the reader thread is still buffering
        // (pre-attach output is not recorded): the exit reset is the only
        // scrollback writer here and the log contents are exact, not racy.
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
        write_acknowledged_native_host_input(pane_id, SUBMIT_KEY).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while sink.lock().unwrap().len() < 12 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(*sink.lock().unwrap(), b"hello world\r");

        // Once the registration is gone, the fallback path reports the missing
        // pane synchronously instead of silently dropping input.
        remove_native_input_writer(pane_id);
        assert!(write_native_host_input(&state, pane_id, b"late".to_vec()).is_err());
    }

    fn test_state() -> AppState {
        AppState::new(QmuxConfig {
            remotes: Default::default(),
            workspace_root: PathBuf::from("/tmp/qmux-workspaces"),
            socket_path: PathBuf::from("/tmp/qmux.sock"),
            adapters: AdapterConfigs {
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
                antigravity: Default::default(),
            },
            legacy_claude_binary: None,
            claude_plugin_dir: std::path::PathBuf::new(),
            opencode_plugin_dir: std::path::PathBuf::new(),
            pi_extension_dir: std::path::PathBuf::new(),
            cursor_plugin_dir: std::path::PathBuf::new(),
        })
    }

    fn test_state_with_workspace(workspace_root: PathBuf) -> AppState {
        let socket_path = workspace_root.join("qmux-test.sock");
        AppState::new(QmuxConfig {
            remotes: Default::default(),
            workspace_root,
            socket_path,
            adapters: AdapterConfigs {
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
                antigravity: Default::default(),
            },
            legacy_claude_binary: None,
            claude_plugin_dir: std::path::PathBuf::new(),
            opencode_plugin_dir: std::path::PathBuf::new(),
            pi_extension_dir: std::path::PathBuf::new(),
            cursor_plugin_dir: std::path::PathBuf::new(),
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

    fn sample_remote_agent(group_id: &str) -> crate::workspace::AgentInfo {
        crate::workspace::AgentInfo {
            id: "agent-remote".to_string(),
            group_id: group_id.to_string(),
            adapter: "claude".to_string(),
            worktree_dir: "/srv/code/project".to_string(),
            branch: None,
            active_workspace: None,
            pane_id: None,
            orphaned_queue_pane_id: None,
            session_id: None,
            transcript_path: None,
            status: crate::workspace::AgentStatus::Starting,
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

    fn test_remote() -> crate::workspace::RemoteRef {
        crate::workspace::RemoteRef {
            id: "remote-1".to_string(),
            label: "workbox".to_string(),
            host: "workbox".to_string(),
            multiplexer: crate::workspace::RemoteMultiplexer::Tmux,
            qmux_cli: None,
            workspace_root: Some("/srv/qmux".to_string()),
        }
    }

    // Characterization: pins the spawn plan the shell path produces today, so
    // the planner refactor (and any future launch-target work) can't silently
    // change what actually executes. The recovery respawn shares this exact
    // builder, so parity between fresh and recovered specs is asserted too.
    #[test]
    fn shell_spawn_spec_pins_program_envs_and_recovery_parity() {
        let workspace = temp_workspace();
        let state = test_state_with_workspace(workspace.clone());
        let group = create_group(
            &state,
            CreateGroupRequest {
                remote_id: None,
                name: None,
                dir: None,
                after_group_id: None,
                base_repo: None,
                base_ref: None,
                remote: None,
            },
        )
        .unwrap();

        let spec = shell_spawn_spec(
            &state,
            "pane-spec".to_string(),
            group.id.clone(),
            std::env::temp_dir(),
            None,
            false,
            None,
        )
        .unwrap();
        assert_eq!(spec.program, pane_shell());
        assert_eq!(spec.cwd, std::env::temp_dir());
        assert_eq!(env_value(&spec.envs, "QMUX_PANE_ID").unwrap(), "pane-spec");
        assert!(env_value(&spec.envs, "QMUX_SOCK").is_some());
        assert!(env_value(&spec.envs, "QMUX_TOKEN").is_some());
        assert_eq!(
            env_value(&spec.envs, "QMUX_WORKSPACE_ROOT").unwrap(),
            workspace.display().to_string()
        );
        assert_eq!(
            env_value(&spec.envs, "QMUX_SHELL_INTEGRATION").unwrap(),
            "1"
        );
        // Integration availability depends on the environment's shell, but the
        // plan must always record the outcome one way or the other.
        match shell_kind(&spec.program) {
            ShellKind::Zsh | ShellKind::Bash => {
                assert_eq!(env_value(&spec.envs, "QMUX_AGENT_FUNCTIONS").unwrap(), "1");
                assert_eq!(spec.support_files.len(), 1);
                assert!(spec.support_file_fallback.is_some());
                assert!(
                    spec.support_files[0]
                        .path
                        .starts_with(shell_integration_root())
                );
            }
            ShellKind::Unsupported => {
                assert_eq!(
                    env_value(&spec.envs, "QMUX_AGENT_FUNCTIONS").unwrap(),
                    "unsupported"
                );
                assert!(spec.support_files.is_empty());
                assert!(spec.support_file_fallback.is_none());
            }
        }

        // Same pane id, recovered: identical command plan.
        let recovered = shell_spawn_spec(
            &state,
            "pane-spec".to_string(),
            group.id.clone(),
            std::env::temp_dir(),
            None,
            true,
            None,
        )
        .unwrap();
        assert_eq!(recovered.program, spec.program);
        assert_eq!(recovered.args, spec.args);
        assert_eq!(
            recovered.support_file_fallback.is_some(),
            spec.support_file_fallback.is_some()
        );
        assert!(recovered.recovered);
    }

    #[test]
    fn ssh_client_spec_stays_local_and_targets_the_host() {
        let state = test_state_with_workspace(temp_workspace());
        let group = create_group(
            &state,
            CreateGroupRequest {
                remote_id: None,
                name: None,
                dir: None,
                after_group_id: None,
                base_repo: None,
                base_ref: None,
                remote: None,
            },
        )
        .unwrap();
        let remote = test_remote();
        let spec = ssh_client_spawn_spec(
            &state,
            "pane-ssh".to_string(),
            group.id,
            std::env::temp_dir(),
            None,
            false,
            &remote,
        )
        .unwrap();
        assert_eq!(spec.program, "ssh");
        assert_eq!(spec.args, vec!["-t", "--", "workbox"]);
        assert!(spec.remote.is_none());
        assert_eq!(spec.ssh_target.as_deref(), Some("workbox"));
        assert_eq!(spec.title, "workbox");
    }

    #[test]
    fn fresh_shell_agent_startup_keeps_support_files_required() {
        let state = test_state_with_workspace(temp_workspace());
        let group = create_group(
            &state,
            CreateGroupRequest {
                remote_id: None,
                name: None,
                dir: None,
                after_group_id: None,
                base_repo: None,
                base_ref: None,
                remote: None,
            },
        )
        .unwrap();
        let planned = shell_spawn_spec(
            &state,
            "pane-required".to_string(),
            group.id,
            std::env::temp_dir(),
            None,
            false,
            Some("qmux-agent-startup".to_string()),
        );

        match shell_kind(&pane_shell()) {
            ShellKind::Zsh | ShellKind::Bash => {
                let spec = planned.unwrap();
                assert!(!spec.support_files.is_empty());
                assert!(spec.support_file_fallback.is_none());
            }
            ShellKind::Unsupported => {
                assert!(planned.unwrap_err().contains("require zsh or bash"));
            }
        }
    }

    #[test]
    fn zsh_injection_plans_rc_support_file_and_zdotdir() {
        let injection = agent_shell_function_injection(
            "/bin/zsh",
            Path::new("/Applications/qmux.app/qmux"),
            "pane-z",
            &[],
            None,
            false,
        )
        .unwrap()
        .expect("zsh is a supported shell");
        assert_eq!(injection.args, vec!["-i".to_string()]);
        let zdotdir = env_value(&injection.envs, "ZDOTDIR").unwrap();
        assert!(
            zdotdir.ends_with("pane-z/zsh"),
            "unexpected ZDOTDIR: {zdotdir}"
        );
        assert_eq!(injection.support_files.len(), 1);
        let file = &injection.support_files[0];
        assert_eq!(file.path, PathBuf::from(&zdotdir).join(".zshrc"));
        assert_eq!(file.root, shell_integration_root());
        assert_eq!(file.mode, 0o644);
        assert!(!file.create_new);
        assert!(file.prune_prefix.is_none());
        assert!(file.contents.contains("Generated by qmux"));
    }

    #[test]
    fn bash_injection_plans_rcfile_argument_and_support_file() {
        let injection = agent_shell_function_injection(
            "/bin/bash",
            Path::new("/Applications/qmux.app/qmux"),
            "pane-b",
            &[],
            None,
            false,
        )
        .unwrap()
        .expect("bash is a supported shell");
        assert_eq!(injection.args.len(), 3);
        assert_eq!(injection.args[0], "--rcfile");
        assert_eq!(injection.args[2], "-i");
        assert_eq!(injection.support_files.len(), 1);
        let file = &injection.support_files[0];
        // The rcfile the shell is told to load is exactly the file the backend
        // will materialize.
        assert_eq!(file.path.display().to_string(), injection.args[1]);
        assert_eq!(file.root, shell_integration_root());
        assert!(file.contents.contains("Generated by qmux"));
    }

    #[test]
    fn agent_pane_envs_extends_pane_envs_with_the_agent_binding() {
        let state = test_state();
        let envs = agent_pane_envs(&state, "pane-a", "agent-7").unwrap();
        let base = qmux_pane_envs(&state, "pane-a").unwrap();
        assert_eq!(envs[..base.len()], base[..]);
        assert_eq!(env_value(&envs, "QMUX_AGENT_ID").unwrap(), "agent-7");
        assert!(env_value(&envs, "QMUX_USER_TOKEN").is_none());
    }

    #[test]
    fn remote_support_files_are_relocated_with_their_references() {
        let state = test_state_with_workspace(temp_workspace());
        let group = create_group(
            &state,
            CreateGroupRequest {
                remote_id: None,
                name: None,
                // A remote path that does not exist locally: creation must not
                // stat it, and the spawn layer must refuse rather than fall
                // through to a local spawn.
                dir: Some("/no/such/dir/on/this/machine".to_string()),
                after_group_id: None,
                base_repo: None,
                base_ref: None,
                remote: Some(test_remote()),
            },
        )
        .unwrap();
        assert_eq!(group.dir, "/no/such/dir/on/this/machine");

        let local_root = shell_integration_root();
        let local_path = local_root.join("pane-remote").join("bashrc");
        let spec = plan_to_spec(
            &state,
            PaneMeta {
                pane_id: Some("pane-remote".to_string()),
                agent_id: None,
                group_id: group.id,
                kind: PaneKind::Shell,
                title: "Remote shell".to_string(),
                last_osc_title: None,
                initial_size: None,
                recovered: false,
            },
            CommandPlan {
                program: "/bin/bash".to_string(),
                args: vec!["--rcfile".to_string(), local_path.display().to_string()],
                cwd: PathBuf::from("/srv/qmux"),
                envs: vec![(
                    "ZDOTDIR".to_string(),
                    local_root.join("pane-remote").display().to_string(),
                )],
                support_files: vec![SupportFile {
                    root: local_root,
                    path: local_path,
                    contents: "source generated".to_string(),
                    mode: 0o600,
                    create_new: false,
                    prune_prefix: None,
                }],
                support_file_fallback: None,
            },
        )
        .unwrap();
        let remote = spec.remote.as_ref().expect("remote plan");
        let support_dir = remote.identity.support_dir.as_deref().unwrap();
        assert!(support_dir.starts_with("/srv/qmux/.qmux/support/qmux-pane-remote-"));
        let remote_path = format!("{support_dir}/bashrc");
        assert_eq!(spec.args[1], remote_path);
        assert_eq!(spec.support_files[0].path, PathBuf::from(&remote_path));
        assert_eq!(remote.identity.support_dir.as_deref(), Some(support_dir));
    }

    #[test]
    fn plan_to_spec_refuses_an_adapter_that_is_not_remote_ready() {
        let state = test_state_with_workspace(temp_workspace());
        let group = create_group(
            &state,
            CreateGroupRequest {
                name: None,
                dir: Some("/srv/code/project".to_string()),
                after_group_id: None,
                base_repo: None,
                base_ref: None,
                remote: Some(test_remote()),
                remote_id: None,
            },
        )
        .unwrap();

        let mut agent = sample_remote_agent(&group.id);
        agent.adapter = "pi".to_string();
        state.insert_agent(agent.clone()).unwrap();

        let error = plan_to_spec(
            &state,
            PaneMeta {
                pane_id: Some("pane-9".to_string()),
                agent_id: Some(agent.id.clone()),
                group_id: group.id.clone(),
                kind: PaneKind::Agent,
                title: "Claude".to_string(),
                last_osc_title: None,
                initial_size: None,
                recovered: false,
            },
            CommandPlan {
                program: "/usr/local/bin/pi".to_string(),
                args: Vec::new(),
                cwd: PathBuf::from("/srv/code/project"),
                envs: Vec::new(),
                support_files: Vec::new(),
                support_file_fallback: None,
            },
        )
        .unwrap_err();

        // Silently succeeding would launch this adapter over there with paths
        // and integration files that were resolved only on the local machine.
        assert!(error.contains("pi"), "{error}");
        assert!(error.contains("cannot run on remote"), "{error}");
    }

    #[test]
    fn plan_to_spec_preserves_remote_command_and_allocates_tmux_identity() {
        let state = test_state_with_workspace(temp_workspace());
        let group = create_group(
            &state,
            CreateGroupRequest {
                name: None,
                dir: Some("/srv/code/project".to_string()),
                after_group_id: None,
                base_repo: None,
                base_ref: None,
                remote: Some(test_remote()),
                remote_id: None,
            },
        )
        .unwrap();
        let spec = plan_to_spec(
            &state,
            PaneMeta {
                pane_id: Some("pane-remote".to_string()),
                agent_id: None,
                group_id: group.id,
                kind: PaneKind::Shell,
                title: "Remote shell".to_string(),
                last_osc_title: None,
                initial_size: Some(InitialPaneSize { cols: 91, rows: 27 }),
                recovered: false,
            },
            CommandPlan {
                program: "/bin/sh".to_string(),
                args: vec!["-l".to_string()],
                cwd: PathBuf::from("/srv/code/project"),
                envs: vec![("REMOTE_TEST".to_string(), "yes".to_string())],
                support_files: Vec::new(),
                support_file_fallback: None,
            },
        )
        .unwrap();

        assert_eq!(spec.program, "/bin/sh");
        assert_eq!(spec.args, ["-l"]);
        assert_eq!(spec.cwd, PathBuf::from("/srv/code/project"));
        let remote = spec.remote.expect("remote execution plan");
        assert_eq!(remote.identity.remote_id, "remote-1");
        assert_eq!(remote.identity.tmux_server, "qmux");
        assert!(
            remote
                .identity
                .tmux_session
                .starts_with("qmux-pane-remote-")
        );
    }

    #[test]
    fn remote_probe_distinguishes_exists_missing_and_transport_failure() {
        assert_eq!(
            probe_remote_session(&[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "exit 0".to_string()
            ]),
            Ok(true)
        );
        assert_eq!(
            probe_remote_session(&[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf \"can't find session: exact\" >&2; exit 1".to_string()
            ]),
            Ok(false)
        );
        assert!(
            probe_remote_session(&[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf permission-denied >&2; exit 1".to_string(),
            ])
            .unwrap_err()
            .contains("permission-denied")
        );
        assert!(
            probe_remote_session(&[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf transport-failed >&2; exit 255".to_string(),
            ])
            .unwrap_err()
            .contains("transport-failed")
        );
    }

    #[test]
    fn remote_tmux_version_gate_explains_the_minimum() {
        assert!(
            ensure_remote_tmux_version(&[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf 'tmux 3.1\\n'".to_string(),
            ])
            .unwrap_err()
            .contains("3.2 or newer")
        );
        ensure_remote_tmux_version(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf 'tmux 3.4a\\n'".to_string(),
        ])
        .unwrap();
    }

    #[test]
    fn remote_batch_helper_kills_a_command_at_its_wall_clock_deadline() {
        let command = Command::new("/bin/sleep");
        let mut command = command;
        command.arg("5");
        let started = Instant::now();
        let error = remote_command_output_with_timeout(
            command,
            None,
            "wait for stalled remote",
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn remote_create_failure_compensates_for_an_uncertain_remote_result() {
        let marker = temp_workspace().join("created-session");
        let marker_arg = marker.display().to_string();
        let commands = crate::host::RemoteTmuxCommands {
            version_argv: Vec::new(),
            create_argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf created > \"$1\"; exit 1".to_string(),
                "qmux-create-test".to_string(),
                marker_arg.clone(),
            ],
            configure_argv: Vec::new(),
            attach_argv: Vec::new(),
            probe_argv: Vec::new(),
            capture_argv: Vec::new(),
            capture_full_argv: Vec::new(),
            activity_argv: Vec::new(),
            kill_argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "rm -f -- \"$1\"".to_string(),
                "qmux-cleanup-test".to_string(),
                marker_arg,
            ],
            forward_cleanup_argv: Vec::new(),
            support_cleanup_argv: Vec::new(),
            remote_socket_path: "/tmp/qmux-create-test.sock".to_string(),
        };

        let error = create_remote_tmux_session(&commands).unwrap_err();

        assert!(error.contains("create remote tmux session"), "{error}");
        assert!(
            !marker.exists(),
            "the compensating kill must remove a session created before SSH reports failure"
        );
    }

    #[test]
    fn tmux_history_rows_replay_from_column_zero_without_doubling_crlf() {
        assert_eq!(
            terminalize_tmux_history(b"one\ntwo\r\nthree"),
            b"one\r\ntwo\r\nthree"
        );
    }

    #[test]
    #[ignore = "requires QMUX_TEST_SSH_TARGET and mutates a qmux-prefixed tmux session there"]
    fn managed_remote_tmux_round_trip() {
        let target = std::env::var("QMUX_TEST_SSH_TARGET").expect("QMUX_TEST_SSH_TARGET");
        let workspace = temp_workspace();
        let state = test_state_with_workspace(workspace.clone());
        // The SSH attachment is required to fail closed when its reverse hook
        // forward cannot be created, so exercise it against a real qmux control
        // listener rather than the stale/nonexistent path older versions of
        // this test tolerated.
        let _control = crate::control_socket::start_control_socket(state.clone()).unwrap();
        let remote_ref = crate::workspace::RemoteRef {
            id: "integration-remote".to_string(),
            label: target.clone(),
            host: target,
            multiplexer: crate::workspace::RemoteMultiplexer::Tmux,
            qmux_cli: None,
            workspace_root: Some("/tmp/qmux-integration-workspaces".to_string()),
        };
        let group = create_group(
            &state,
            CreateGroupRequest {
                name: Some("Remote integration".to_string()),
                dir: Some("/tmp".to_string()),
                after_group_id: None,
                base_repo: None,
                base_ref: None,
                remote: Some(remote_ref.clone()),
                remote_id: None,
            },
        )
        .unwrap();
        let pane_id = "pane-remote-integration".to_string();
        let support_dir =
            PathBuf::from("/tmp/qmux-integration-workspaces/.qmux/support/pane-remote-integration");
        let support_path = support_dir.join("probe.txt");
        let mut identity = RemoteSessionIdentity::new("integration-remote", &pane_id).unwrap();
        identity.support_dir = Some(support_dir.display().to_string());
        let pane = spawn_pty(
            &state,
            PtySpawnSpec {
                pane_id: Some(pane_id.clone()),
                agent_id: None,
                group_id: group.id,
                kind: PaneKind::Shell,
                title: "Remote integration".to_string(),
                last_osc_title: None,
                program: "/bin/sh".to_string(),
                args: vec![
                    "-lc".to_string(),
                    format!("test \"$(cat '{}')\" = 'provisioned remotely' || exit 91; printf 'remote-ready\\n'; IFS= read -r trigger; printf 'remote-gap-armed\\n'; sleep 0.2; gap=1; while [ \"$gap\" -le 40 ]; do printf 'remote-offline-gap-%s\\n' \"$gap\"; gap=$((gap + 1)); done; count=1; while [ \"$count\" -le 2 ]; do IFS= read -r line; printf 'remote-got-%s:%s\\n' \"$count\" \"$line\"; count=$((count + 1)); done; sleep 300", support_path.display()),
                ],
                cwd: PathBuf::from("/tmp"),
                envs: Vec::new(),
                support_files: vec![SupportFile {
                    root: support_dir.clone(),
                    path: support_path,
                    contents: "provisioned remotely".to_string(),
                    mode: 0o600,
                    create_new: true,
                    prune_prefix: Some("probe".to_string()),
                }],
                support_file_fallback: None,
                initial_size: Some(InitialPaneSize { cols: 90, rows: 10 }),
                recovered: false,
                ssh_target: None,
                remote: Some(RemoteSpawnSpec {
                    host: crate::host::for_group(Some(&remote_ref)),
                    identity,
                }),
            },
        )
        .unwrap();

        let wait_for = |needle: &[u8]| {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            loop {
                let backlog = state.pane_backlog(&pane.id).unwrap().unwrap();
                let buffered = windows_contains(&backlog.lock().unwrap().buffer, needle);
                let durable = read_pane_scrollback(&workspace, &pane.id)
                    .map(|bytes| windows_contains(&bytes, needle))
                    .unwrap_or(false);
                if buffered || durable {
                    return true;
                }
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                thread::sleep(Duration::from_millis(25));
            }
        };
        let ready = wait_for(b"remote-ready");
        if ready {
            state
                .pane_writer(&pane.id)
                .unwrap()
                .unwrap()
                .lock()
                .unwrap()
                .write_all(b"arm gap\r")
                .unwrap();
        }
        let gap_armed = ready && wait_for(b"remote-gap-armed");
        let (first_controller, _, _) = state
            .pane_remote_control(&pane.id)
            .unwrap()
            .expect("remote controller");
        let first_child = first_controller.current_child().expect("first SSH child");
        first_child.lock().unwrap().kill().unwrap();
        let reconnect_deadline = std::time::Instant::now() + Duration::from_secs(20);
        let reconnected = loop {
            if let Some(child) = first_controller.current_child()
                && !Arc::ptr_eq(&child, &first_child)
            {
                break true;
            }
            if std::time::Instant::now() >= reconnect_deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(25));
        };
        let disconnected_history = reconnected && wait_for(b"remote-offline-gap-40");
        attach_pane(&state, pane.id.clone()).unwrap();
        if gap_armed && reconnected {
            state
                .pane_writer(&pane.id)
                .unwrap()
                .unwrap()
                .lock()
                .unwrap()
                .write_all(b"hello from qmux\r")
                .unwrap();
        }
        let round_trip = gap_armed && reconnected && wait_for(b"remote-got-1:hello from qmux");

        // Replace the process-local runtime with the persisted pane identity,
        // simulating a qmux restart. Recovery must attach the existing session
        // and must not execute any create command.
        let snapshot = state
            .list_panes()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == pane.id)
            .unwrap();
        let (_, attachment) = first_controller.begin_generation();
        if let Some(attachment) = attachment {
            let _ = kill_child(&pane.id, attachment.child);
        }
        let restart_recovered = reattach_remote_pane(&state, &snapshot).unwrap();
        let (restart_controller, _, commands) = state
            .pane_remote_control(&pane.id)
            .unwrap()
            .expect("recovered remote controller");
        assert!(!Arc::ptr_eq(&first_controller, &restart_controller));
        assert!(commands.create_argv.is_empty());
        state
            .pane_writer(&pane.id)
            .unwrap()
            .unwrap()
            .lock()
            .unwrap()
            .write_all(b"after restart\r")
            .unwrap();
        let restart_round_trip = wait_for(b"remote-got-2:after restart");
        let durable_history = read_pane_scrollback(&workspace, &pane.id).unwrap();
        let cleanup = kill_pane(&state, pane.id.clone());
        let support_removed = crate::host::for_group(Some(&remote_ref))
            .command(crate::host::RemoteCommand {
                program: "test",
                args: vec![
                    "!".to_string(),
                    "-e".to_string(),
                    support_dir.display().to_string(),
                ],
                ..Default::default()
            })
            .status()
            .is_ok_and(|status| status.success());
        let _ = fs::remove_dir_all(workspace);

        assert!(ready, "remote pane never produced its startup output");
        assert!(
            round_trip,
            "remote pane did not reconnect and round trip through tmux"
        );
        assert!(
            disconnected_history && windows_contains(&durable_history, b"remote-offline-gap-40"),
            "output produced while SSH was detached was not reconciled into local scrollback"
        );
        assert!(restart_recovered.recovered);
        assert!(
            support_removed,
            "remote support directory was not cleaned up"
        );
        assert!(
            restart_round_trip,
            "attach-only restart recovery did not reach the existing process"
        );
        cleanup.unwrap();
    }

    #[test]
    fn materialize_support_files_rejects_paths_outside_their_root() {
        let error = materialize_support_files(&[SupportFile {
            root: PathBuf::from("/tmp/qmux-sf-root"),
            path: PathBuf::from("/etc/passwd"),
            contents: String::new(),
            mode: 0o600,
            create_new: false,
            prune_prefix: None,
        }])
        .unwrap_err();
        assert!(error.contains("escapes"), "{error}");
    }

    // The containment check cannot be lexical alone: `strip_prefix` succeeds on
    // a path that walks back out through `..`, so a descriptor built from an
    // unsanitized id would pass the prefix test and write anywhere on disk.
    #[test]
    fn materialize_support_files_rejects_parent_traversal_within_their_root() {
        let root = std::env::temp_dir().join("qmux-sf-traversal");
        let escapee = root.join("..").join("qmux-sf-escaped.json");
        let _ = fs::remove_file(&escapee);
        let error = materialize_support_files(&[SupportFile {
            root: root.clone(),
            path: escapee.clone(),
            contents: "planted".to_string(),
            mode: 0o600,
            create_new: false,
            prune_prefix: None,
        }])
        .unwrap_err();
        assert!(error.contains("escapes"), "{error}");
        assert!(!escapee.exists());
    }

    // Every level from the shared root down to the file's parent must be
    // owner-only by the time the file lands, so a generated script is never
    // reachable by another account under a world-writable /tmp.
    #[test]
    fn materialize_support_files_locks_down_the_whole_directory_chain() {
        let root = std::env::temp_dir().join("qmux-sf-chain");
        let _ = fs::remove_dir_all(&root);
        let nested = root.join("pane-chain").join("zsh");
        let path = nested.join(".zshrc");
        materialize_support_files(&[SupportFile {
            root: root.clone(),
            path: path.clone(),
            contents: "# generated".to_string(),
            mode: 0o644,
            create_new: false,
            prune_prefix: None,
        }])
        .unwrap();

        for dir in [&root, &root.join("pane-chain"), &nested] {
            let mode = fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{} is {mode:o}", dir.display());
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), "# generated");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn optional_support_file_failure_applies_plain_command_fallback() {
        let mut spec = PtySpawnSpec {
            pane_id: Some("pane-fallback".to_string()),
            agent_id: None,
            group_id: "group-1".to_string(),
            kind: PaneKind::Shell,
            title: "Shell".to_string(),
            last_osc_title: None,
            program: "/bin/zsh".to_string(),
            args: vec!["-i".to_string()],
            cwd: std::env::temp_dir(),
            envs: vec![("ZDOTDIR".to_string(), "/tmp/generated".to_string())],
            support_files: vec![SupportFile {
                root: PathBuf::from("/tmp/qmux-sf-root"),
                path: PathBuf::from("/etc/passwd"),
                contents: String::new(),
                mode: 0o600,
                create_new: false,
                prune_prefix: None,
            }],
            support_file_fallback: Some(SupportFileFallback {
                args: Vec::new(),
                envs: vec![("QMUX_AGENT_FUNCTIONS".to_string(), "failed".to_string())],
                error_env_key: Some("QMUX_AGENT_FUNCTIONS_ERROR".to_string()),
            }),
            initial_size: None,
            recovered: true,
            ssh_target: None,
            remote: None,
        };

        materialize_support_files_or_fallback(&mut spec).unwrap();

        assert!(spec.args.is_empty());
        assert!(spec.support_files.is_empty());
        assert_eq!(
            env_value(&spec.envs, "QMUX_AGENT_FUNCTIONS").as_deref(),
            Some("failed")
        );
        assert!(
            env_value(&spec.envs, "QMUX_AGENT_FUNCTIONS_ERROR")
                .is_some_and(|error| error.contains("escapes"))
        );
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
            // Every prompt reports, even when PWD is unchanged, so an in-place
            // `git switch` refreshes the branch shown on the tab.
            assert!(!script.contains("__qmux_last_pwd"));
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
    fn child_context_scrub_removes_outer_qmux_identity_before_fresh_envs() {
        let mut command = CommandBuilder::new("/usr/bin/true");
        command.env("QMUX_USER_TOKEN", "outer-user-token");
        command.env("QMUX_AGENT_ID", "outer-agent");
        command.env("QMUX_FORK_POINT", "outer-fork-point");

        scrub_inherited_qmux_context(&mut command);

        assert!(command.get_env("QMUX_USER_TOKEN").is_none());
        assert!(command.get_env("QMUX_AGENT_ID").is_none());
        assert!(command.get_env("QMUX_FORK_POINT").is_none());
        command.env("QMUX_TOKEN", "fresh-pane-token");
        assert_eq!(
            command.get_env("QMUX_TOKEN"),
            Some("fresh-pane-token".as_ref())
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

    const TEST_NATIVE_SUBMIT_TIMING: NativeSubmitTiming = NativeSubmitTiming {
        data_poll_interval: Duration::ZERO,
        data_max_rechecks: 8,
        data_quiet_rechecks: 2,
        submit_key_delay: Duration::ZERO,
        submit_poll_interval: Duration::ZERO,
        submit_max_rechecks: 2,
    };

    #[test]
    fn native_submission_flushes_paste_and_submit_separately() {
        let calls = RefCell::new(Vec::new());
        let positions = RefCell::new(vec![0_u64, 12, 12, 12, 13].into_iter());

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
            || panic!("an acknowledged synthetic Return must not use the raw fallback"),
            || {
                calls.borrow_mut().push("flush".to_string());
                positions
                    .borrow_mut()
                    .next()
                    .ok_or_else(|| "unexpected flush".to_string())
            },
            TEST_NATIVE_SUBMIT_TIMING,
        )
        .unwrap();

        assert_eq!(
            calls.into_inner(),
            [
                "flush", "paste", "flush", "flush", "flush", "submit", "flush"
            ]
        );
    }

    #[test]
    fn native_submission_waits_for_the_payload_to_arrive_and_become_quiet() {
        let data_calls = Cell::new(0);
        let submit_calls = Cell::new(0);
        // The first post-paste barrier overtakes Ghostty's deferred input callback.
        // Ghostty then emits the paste in two chunks; submission must wait through
        // two unchanged positions after the final chunk rather than treating that
        // chunk as acknowledgement of Return.
        let positions = RefCell::new(vec![0_u64, 0, 5, 12, 12, 12, 13].into_iter());

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
            || panic!("an acknowledged synthetic Return must not use the raw fallback"),
            || {
                positions
                    .borrow_mut()
                    .next()
                    .ok_or_else(|| "unexpected flush".to_string())
            },
            TEST_NATIVE_SUBMIT_TIMING,
        )
        .unwrap();

        assert_eq!(data_calls.get(), 1);
        assert_eq!(submit_calls.get(), 1);
    }

    #[test]
    fn native_submission_accepts_a_late_submit_ack_without_retrying_return() {
        let submit_calls = Cell::new(0);
        let positions = RefCell::new(vec![0_u64, 12, 12, 12, 12, 13].into_iter());

        write_native_data_and_submit(
            "test",
            || Ok(()),
            || {
                submit_calls.set(submit_calls.get() + 1);
                Ok(())
            },
            || panic!("a late synthetic acknowledgement must not use the raw fallback"),
            || {
                positions
                    .borrow_mut()
                    .next()
                    .ok_or_else(|| "unexpected flush".to_string())
            },
            TEST_NATIVE_SUBMIT_TIMING,
        )
        .unwrap();

        assert_eq!(submit_calls.get(), 1);
    }

    #[test]
    fn native_submission_falls_back_to_one_acknowledged_raw_return() {
        let data_calls = Cell::new(0);
        let submit_calls = Cell::new(0);
        let raw_submit_calls = Cell::new(0);
        let positions = RefCell::new(vec![0_u64, 12, 12, 12, 12, 12, 12].into_iter());

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
                raw_submit_calls.set(raw_submit_calls.get() + 1);
                Ok(())
            },
            || {
                positions
                    .borrow_mut()
                    .next()
                    .ok_or_else(|| "unexpected flush".to_string())
            },
            TEST_NATIVE_SUBMIT_TIMING,
        )
        .unwrap();

        assert_eq!(data_calls.get(), 1);
        assert_eq!(submit_calls.get(), 1);
        assert_eq!(raw_submit_calls.get(), 1);
    }

    #[test]
    fn native_submission_uses_raw_return_when_the_synthetic_bridge_fails() {
        let raw_submit_calls = Cell::new(0);
        let positions = RefCell::new(vec![0_u64, 12, 12, 12].into_iter());

        write_native_data_and_submit(
            "test",
            || Ok(()),
            || Err("bridge lost".to_string()),
            || {
                raw_submit_calls.set(raw_submit_calls.get() + 1);
                Ok(())
            },
            || {
                positions
                    .borrow_mut()
                    .next()
                    .ok_or_else(|| "unexpected flush".to_string())
            },
            TEST_NATIVE_SUBMIT_TIMING,
        )
        .unwrap();

        assert_eq!(raw_submit_calls.get(), 1);
    }

    #[test]
    fn native_submit_fallback_failure_reports_payload_delivered() {
        let positions = RefCell::new(vec![0_u64, 12, 12, 12, 12, 12, 12].into_iter());

        // The paste landed before both submit paths failed: the turn queue must
        // learn that the text is already in the composer so its retry sends only
        // Return instead of pasting a duplicate copy.
        let failure = write_native_data_and_submit(
            "test",
            || Ok(()),
            || Ok(()),
            || Err("writer lost".to_string()),
            || {
                positions
                    .borrow_mut()
                    .next()
                    .ok_or_else(|| "unexpected flush".to_string())
            },
            TEST_NATIVE_SUBMIT_TIMING,
        )
        .unwrap_err();

        assert!(failure.data_delivered);
        assert!(failure.error.contains("writer lost"));
    }

    #[test]
    fn native_payload_failure_reports_payload_undelivered() {
        // The paste action itself failed: nothing reached the composer, so the
        // retry must re-paste the full text.
        let failure = write_native_data_and_submit(
            "test",
            || Err("paste rejected".to_string()),
            || Ok(()),
            || panic!("an undelivered payload must not reach the raw submit fallback"),
            || Ok(0),
            TEST_NATIVE_SUBMIT_TIMING,
        )
        .unwrap_err();
        assert!(!failure.data_delivered);

        // The paste was accepted but verifiably never emitted PTY input: also
        // undelivered.
        let failure = write_native_data_and_submit(
            "test",
            || Ok(()),
            || Ok(()),
            || panic!("an undelivered payload must not reach the raw submit fallback"),
            || Ok(0),
            TEST_NATIVE_SUBMIT_TIMING,
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
                support_files: Vec::new(),
                support_file_fallback: None,
                initial_size: None,
                recovered: false,
                ssh_target: None,
                remote: None,
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
            support_files: Vec::new(),
            support_file_fallback: None,
            initial_size: None,
            recovered: true,
            ssh_target: None,
            remote: None,
        };
        let pane = PaneInfo {
            id: "pane-1".to_string(),
            title: "Generated review title".to_string(),
            last_osc_title: Some("Agent OSC title".to_string()),
            kind: PaneKind::Shell,
            agent_id: None,
            group_id: "group-1".to_string(),
            cwd: std::env::temp_dir().display().to_string(),
            active_workspace: None,
            remote_session: None,
            remote_connection: None,
            cols: 100,
            rows: 24,
            status: PaneStatus::Running,
            last_active_at: 0,
            recovered: true,
            ssh_target: None,
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
                    remote: None,
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
                active_workspace: None,
                pane_id: Some(pane.id),
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
                created_at: 1,
            })
            .unwrap();

        close_worktree_pane(&state, "agent-1", true).unwrap();

        assert!(state.agent("agent-1").unwrap().is_none());
        assert!(!worktree.exists());

        fs::remove_dir_all(workspace).ok();
    }

    #[test]
    fn resize_pane_is_a_noop_when_the_grid_is_unchanged() {
        let state = test_state();
        let pane = spawn_test_pty(
            &state,
            "pane-resize-noop",
            vec!["-c".to_string(), "sleep 30".to_string()],
        );
        resize_pane(&state, pane.id.clone(), 100, 40).unwrap();
        let master = state.pane_master(&pane.id).unwrap().unwrap();
        {
            let size = master.lock().unwrap().get_size().unwrap();
            assert_eq!(size.cols, 100);
            assert_eq!(size.rows, 40);
        }
        resize_pane(&state, pane.id.clone(), 100, 40).unwrap();
        let size = master.lock().unwrap().get_size().unwrap();
        assert_eq!(size.cols, 100);
        assert_eq!(size.rows, 40);
        let _ = kill_pane(&state, pane.id);
    }

    #[test]
    fn disconnected_remote_resize_is_persisted_for_the_next_attachment() {
        let state = test_state();
        let controller = RemoteAttachmentController::new();
        let history = RemoteHistoryCheckpoint::new(Vec::new());
        let backlog: SharedBacklog = Arc::new(Mutex::new(Default::default()));
        let commands = crate::host::RemoteTmuxCommands {
            version_argv: Vec::new(),
            create_argv: Vec::new(),
            configure_argv: Vec::new(),
            attach_argv: Vec::new(),
            probe_argv: Vec::new(),
            capture_argv: Vec::new(),
            capture_full_argv: Vec::new(),
            activity_argv: Vec::new(),
            kill_argv: Vec::new(),
            forward_cleanup_argv: Vec::new(),
            support_cleanup_argv: Vec::new(),
            remote_socket_path: "/tmp/qmux-offline.sock".to_string(),
        };
        state
            .insert_pane(PaneRuntime {
                info: PaneInfo {
                    id: "pane-offline-resize".to_string(),
                    title: "Remote".to_string(),
                    last_osc_title: None,
                    kind: PaneKind::Shell,
                    agent_id: None,
                    group_id: "group-1".to_string(),
                    cwd: "/srv/project".to_string(),
                    active_workspace: None,
                    remote_session: Some(
                        RemoteSessionIdentity::new("remote-1", "pane-offline-resize").unwrap(),
                    ),
                    remote_connection: Some(RemoteConnectionInfo {
                        state: RemoteConnectionState::Reconnecting,
                        message: None,
                    }),
                    cols: 80,
                    rows: 24,
                    status: PaneStatus::Running,
                    last_active_at: 0,
                    recovered: false,
                    ssh_target: None,
                    depth: 0,
                },
                backend: PaneBackend::RemoteTmux(RemoteTmuxBackend::new(
                    controller, history, backlog, commands, false,
                )),
                cwd_observation_seq: 0,
            })
            .unwrap();

        resize_pane(&state, "pane-offline-resize".to_string(), 132, 41).unwrap();
        let pane = state
            .list_panes()
            .unwrap()
            .into_iter()
            .find(|pane| pane.id == "pane-offline-resize")
            .unwrap();
        assert_eq!((pane.cols, pane.rows), (132, 41));
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
    fn concurrent_teardown_starts_every_pane_before_waiting_for_completion() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));

        let teardown = thread::spawn(move || {
            for_each_concurrently(vec!["pane-1", "pane-2"], move |pane_id| {
                started_tx.send(pane_id).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
            });
        });

        let first = started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first pane teardown should start");
        let second = started_rx.recv_timeout(Duration::from_secs(1));
        // Always release both actions before asserting so a serial regression fails
        // cleanly instead of leaving the test's coordinator thread parked forever.
        release_tx.send(()).unwrap();
        release_tx.send(()).unwrap();
        teardown.join().unwrap();

        let second = second.expect("pane teardowns should overlap");
        assert_ne!(first, second);
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
