use crate::config::QmuxConfig;
use crate::diagnostics::Diagnostics;
use crate::encyclopedia;
use crate::events::QmuxEvent;
use crate::persistence::{self, PersistedState, STATE_VERSION};
use crate::research::{
    self, CreateResearchDocumentRequest, CreateResearchTreeRequest, ResearchBranchRemoval,
    ResearchHighlight, ResearchHighlightAnchor, ResearchNode, ResearchNodeCard,
    ResearchNodeContent, ResearchNodeKind, ResearchNodeOrigin, ResearchNodeStatus,
    ResearchPublicationProposal, ResearchTree, ResearchTreeDetail, ResearchTreeSummary,
    UpdateResearchDocumentRequest, UpdateResearchDocumentResult,
};
use crate::scrollback::{bounded_undo_scrollback, read_pane_scrollback, remove_pane_scrollback};
use crate::thread_graph;
use crate::transcript::Turn;
use crate::workspace::{AgentInfo, AgentStatus, GroupInfo, WorkspaceScope};
use portable_pty::{Child, MasterPty};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};

pub type SharedChild = Arc<Mutex<Box<dyn Child + Send + Sync>>>;
pub type SharedMaster = Arc<Mutex<Box<dyn MasterPty + Send>>>;
pub type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;
pub type SharedBacklog = Arc<Mutex<PaneBacklog>>;

pub enum PaneBackend {
    #[cfg_attr(all(target_os = "macos", not(test)), allow(dead_code))]
    HostPty {
        child: SharedChild,
        master: SharedMaster,
        writer: SharedWriter,
        backlog: SharedBacklog,
        /// The process/PTY is owned by qmux, but output is rendered by a native
        /// Ghostty host-managed surface instead of the webview renderer.
        native_surface: bool,
    },
}

/// Upper bound on a pane's reported working directory. Comfortably above any
/// real filesystem path (PATH_MAX is typically 1024–4096) while bounding what an
/// in-pane process can push into persisted state via the control socket.
const MAX_PANE_CWD_LEN: usize = 8192;

/// Upper bound on the parsed transcript turns retained in memory per agent. The
/// store feeds the UI timeline on (re)connect and crash recovery; without a cap a
/// long session — or selecting a large transcript, which reparses the whole file —
/// grows unbounded, since each turn can carry full tool inputs/results. Once over
/// the cap the oldest turns are dropped (the live timeline still streams every new
/// turn to the frontend as it arrives).
const MAX_TURNS_PER_AGENT: usize = 200;

/// Depth of the closed-pane undo stack. Each entry can carry a full pane snapshot
/// (agent, turns, queued prompts, scrollback), so this bounds transient memory while
/// still letting a run of accidental closes be reopened one at a time. Oldest entries
/// are dropped past the cap. Transient — the stack is never persisted across restart.
const MAX_CLOSED_PANE_UNDO: usize = 25;

/// Per-snapshot cap on the scrollback an undo entry keeps resident. A closed
/// pane's durable log is deleted, so the snapshot is the only surviving copy and
/// the `MAX_CLOSED_PANE_UNDO`-deep stack could otherwise pin ~25× the full log
/// (up to the trim trigger each) in memory. The newest slice restores plenty of
/// context on reopen; the rest is a convenience buffer not worth the RAM.
const MAX_UNDO_SCROLLBACK_BYTES: usize = 1024 * 1024;

/// Upper bound on pending turns queued for a single agent. This is a safety
/// ceiling against unbounded growth (memory plus a larger `state.json` rewritten
/// on every persist), not an expected limit — enqueue past it returns an error the
/// UI surfaces rather than silently swallowing the turn.
const MAX_QUEUED_TURNS_PER_AGENT: usize = 500;

/// Upper bound on durable recent-session entries. This keeps the home list fast and
/// prevents the persisted state from growing forever across months of work.
const MAX_RECENT_SESSIONS: usize = 80;

/// How long the persister thread lets a burst of mutations settle before taking
/// its snapshot. Long enough to fold an agent's status-hook storm (or a window
/// resize) into one write, short enough that a crash loses at most a blink of
/// bookkeeping — pane content itself lives in the PTYs, not in state.json.
const PERSIST_DEBOUNCE: Duration = Duration::from_millis(200);

/// OSC titles can change continuously (progress counters, spinners, build
/// percentages). Keep the newest value in memory immediately, but only make
/// title-only activity dirty on this coarser cadence so a busy terminal does
/// not force a full state.json rewrite every few hundred milliseconds.
const LAST_OSC_TITLE_PERSIST_INTERVAL: Duration = Duration::from_secs(3);

/// Matches the frontend's display cap. OSC titles are untrusted terminal
/// output, so normalize and bound them before they enter persisted state.
const MAX_LAST_OSC_TITLE_CHARS: usize = 160;

const RECENT_SESSION_PREVIEW_MAX_CHARS: usize = 90;

/// How far a recent session's `last_active_at` must drift before a touch that
/// changes nothing else re-stamps it (see upsert_recent_session_for_agent_locked).
const RECENT_SESSION_TOUCH_COARSENESS_MS: u128 = 5_000;

/// Holds PTY output produced before the webview's listener is attached.
///
/// A pane's reader thread starts emitting the instant the process spawns, but on
/// a cold start (and for panes recovered before the UI exists) that happens
/// before the frontend has registered its `qmux-event` listener, so the very
/// first prompt would be emitted into the void and lost. Until `ready` flips —
/// the frontend signals this via `pane_attach` once its listener is live — the
/// reader buffers here instead of emitting.
#[derive(Default)]
pub struct PaneBacklog {
    pub ready: bool,
    pub buffer: Vec<u8>,
    /// Whether durable scrollback has already been handed to this pane's native
    /// surface. `attach_pane` only releases `ready` after the whole attach
    /// succeeds, so a failed backlog flush makes the frontend retry the attach;
    /// without this flag the retry would replay the durable history a second
    /// time and double every restored line on screen. Set once the history is
    /// delivered, so retries skip replay and resume at the failed step.
    pub replayed: bool,
}

#[derive(Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    config: QmuxConfig,
    // Freeze diagnostics: durable record ring + JSONL log, emit-rate
    // instrumentation, and the frontend heartbeat monitor (see diagnostics.rs).
    diagnostics: Diagnostics,
    pane_tokens: Mutex<HashMap<String, String>>,
    // Per-pane file-server tokens, distinct from the control-socket `pane_tokens`.
    // A browser-overlay URL carries one of these in its path, so the loopback file
    // server can resolve which pane the request is for and scope the served roots to
    // that pane only (see `pane_for_file_token` / `pane_file_roots`).
    file_tokens: Mutex<HashMap<String, String>>,
    model: Mutex<Model>,
    transcript_tails: Mutex<HashSet<String>>,
    next_id: AtomicU64,
    app_handle: Mutex<Option<AppHandle>>,
    // Persistence stays off until restore_session() runs so constructing a state
    // (notably in tests) never touches disk. Once enabled, model mutations mark
    // the state dirty and the persister thread snapshots it to
    // workspace_root/.qmux/state.json on a short debounce.
    persist_enabled: AtomicBool,
    // Serializes the whole snapshot->write->rename in persist() so concurrent
    // saves commit in snapshot order. Without it, a slower older snapshot's
    // rename can land after a newer one and clobber it, losing the last change
    // (or re-sending an already-drained queued turn) across a restart.
    persist_lock: Mutex<()>,
    // Serializes document snapshot replacement with highlight mutations and
    // follow-up prompt capture. Those operations span both the in-memory model
    // and a response-snapshot file, so the model lock alone cannot make them a
    // coherent revision boundary without holding it across fsync'd IO.
    research_document_lock: Mutex<()>,
    // Debounced persistence. Mutations only mark this dirty flag and wake the
    // dedicated writer thread, which coalesces a burst of mutations (agent
    // status hooks, transcript appends, resize storms) into one snapshot+write
    // instead of a full-state serialize+fsync per mutation — the snapshot clone
    // runs under the model lock, so synchronous persists lengthened every lock
    // hold the input path contends with. A clean exit still writes its final
    // snapshot synchronously via `finalize_persistence_for_exit`; what the
    // debounce trades away is at most the last window of changes on a crash.
    persist_dirty: Mutex<bool>,
    persist_wake: Condvar,
    persister_spawned: AtomicBool,
    // At most one coarse OSC-title persistence timer is live at a time. The
    // normal state persister still performs the eventual atomic snapshot;
    // this only delays the dirty mark for title-only activity.
    last_osc_title_persist_scheduled: AtomicBool,
    // Why restore_session had to fall back or drop entries, held until startup
    // surfaces it in a GUI dialog — a Finder launch never shows stderr, and a
    // silently discarded session looks like the app ate the user's tabs.
    recovery_warning: Mutex<Option<String>>,
    // The state-file bytes the startup preflight already read, handed to
    // restore_session so hydration doesn't read and parse the same file a
    // second time. Taken (and dropped) on first use.
    preflighted_state: Mutex<Option<Vec<u8>>>,
    exit_confirmed: AtomicBool,
    // Set before the final exit snapshot is taken. Reader threads can observe PTY
    // EOF while kill_all_panes tears processes down; those removals must preserve
    // the journals referenced by the frozen snapshot for the next launch.
    exit_teardown_started: AtomicBool,
    // Loopback file-server port, set once at startup. The control socket pairs it with
    // a per-pane file token to build browser-overlay URLs.
    file_server: Mutex<Option<u16>>,
    // (device, inode) of the control socket this process bound, recorded at bind time
    // so exit cleanup can tell its own socket apart from one a later instance bound
    // at the same path (see `owns_control_socket`).
    control_socket_identity: Mutex<Option<(u64, u64)>>,
    // Per-pane "send" locks. `write_pane` holds one across a whole paste+submit
    // sequence so two concurrent submits to the same pane can't interleave into one
    // merged turn across the inter-write delay. Kept separate from the raw writer
    // lock so live keystrokes are never blocked behind a submit. Reclaimed in
    // `remove_pane`.
    pane_send_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Live shell-launched agent jobs. Process ids, process groups, and terminal
    /// foreground ownership die with this app process, so none of this belongs in
    /// state.json; recovered shells register their freshly resumed job again.
    shell_agent_jobs: Mutex<HashMap<String, ShellAgentJob>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ShellAgentJobState {
    Foreground,
    Backgrounded,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellAgentJobInfo {
    pub job_id: String,
    pub agent_id: String,
    pub pane_id: String,
    pub state: ShellAgentJobState,
}

#[derive(Clone, Debug)]
pub(crate) struct ShellAgentJobTarget {
    pub job_id: String,
    pub supervisor_pid: u32,
}

#[derive(Clone, Debug)]
struct ShellAgentJob {
    info: ShellAgentJobInfo,
    supervisor_pid: u32,
    missing_samples: u8,
}

#[derive(Default)]
struct ActiveSubagents {
    identified: HashSet<String>,
    anonymous: usize,
}

impl ActiveSubagents {
    fn count(&self) -> usize {
        self.identified.len().saturating_add(self.anonymous)
    }

    fn is_empty(&self) -> bool {
        self.identified.is_empty() && self.anonymous == 0
    }
}

#[derive(Default)]
struct Model {
    panes: HashMap<String, PaneRuntime>,
    pane_order: Vec<String>,
    /// Sidebar nesting depth per pane (0 = root). Source of truth for the tab tree;
    /// `ordered_panes` stamps it onto each returned `PaneInfo`. Absent id == depth 0.
    pane_depth: HashMap<String, u16>,
    pane_splits: Vec<PaneSplitInfo>,
    groups: HashMap<String, GroupInfo>,
    group_order: Vec<String>,
    agents: HashMap<String, AgentInfo>,
    turns: HashMap<String, Vec<Turn>>,
    threads: HashMap<String, thread_graph::ThreadRecord>,
    thread_focus: HashMap<String, String>,
    research_trees: HashMap<String, ResearchTree>,
    research_tree_order: Vec<String>,
    research_nodes: HashMap<String, ResearchNode>,
    /// Client-authored grouping of research trees into folders (plus stars and
    /// collapsed flags). Persisted with the trees it references so the two can
    /// never drift; reconciled against the live tree set at load and scrubbed
    /// when a tree is removed.
    research_folders: research::ResearchFolderState,
    /// Per-workspace encyclopedia settings, generation cursor, and in-flight
    /// update run. Persisted with the research records it derives from.
    encyclopedia: encyclopedia::EncyclopediaState,
    /// Pane ids with a backend retirement worker in flight. Transient and deduplicated.
    research_retiring_panes: HashSet<String>,
    agent_turn_queues: HashMap<String, VecDeque<QueuedTurn>>,
    /// Application-global prompt drafts (the home Drafts rail), oldest first.
    global_drafts: Vec<GlobalDraft>,
    /// A queued turn claimed for delivery but not yet confirmed on the PTY, per agent
    /// (at most one — `agent_draining` serializes drains). Popped out of the queue at
    /// claim and persisted here so a crash mid-delivery re-queues it on restart
    /// instead of losing it; cleared once the write lands. At-most-one per agent.
    agent_inflight: HashMap<String, QueuedTurn>,
    agent_send_tracking: HashMap<String, AgentSendTracking>,
    /// Monotonic per-agent counter bumped on every agent mutation and transcript
    /// write. Lets a watcher ask "did anything happen to this agent since I looked?"
    /// — the Esc-interrupt grace window uses it to stand down when hook or transcript
    /// activity proves the agent is still working. Transient (not persisted).
    agent_activity: HashMap<String, u64>,
    /// Monotonic per-agent counter bumped on every status hook/write, including writes
    /// that keep the same status. Unlike `agent_activity`, transcript writes do not
    /// touch this, so a delayed idle resolver can distinguish a new lifecycle hook from
    /// late transcript tailing. Transient (not persisted).
    agent_status_activity: HashMap<String, u64>,
    /// Adapter-reported background subagents still working for each parent.
    /// A parent Stop ends only its foreground turn while this is non-zero.
    /// Transient: hooks rebuild it for each running process.
    agent_active_subagents: HashMap<String, ActiveSubagents>,
    /// Agents whose most recent Stop reported still-running background tasks
    /// (Claude 2.1.145+ sends its live task registry on Stop). Unlike the
    /// hook-tracked subagent counter above, this snapshot is the only signal
    /// for background work that never emits Subagent hooks, and it must also
    /// hold the agent open at the idle-prompt boundary — otherwise the ~60s
    /// idle notification would settle Done and silently cancel the wait the
    /// Stop handler just established. Refreshed by every Stop that carries the
    /// field; cleared when a Stop reports no running tasks, on SessionEnd, and
    /// with the agent's other transient state. Transient (not persisted).
    agents_with_reported_background_tasks: HashSet<String>,
    /// Agents with an Esc-interrupt grace watch already in flight. Holding Esc (key
    /// repeat) fires `watch_agent_after_escape` per keystroke; this dedupes so a burst
    /// spawns one watcher thread, not dozens. Cleared when that thread resolves.
    /// Transient (not persisted).
    agent_escape_watch: HashSet<String>,
    /// `(agent_id, send_id)` pairs with a submit-confirmation watch already in
    /// flight. A drained queued/direct turn arms `watch_agent_after_queued_send` to
    /// recover a dropped Return; keying by the exact send means overlapping sends
    /// (a direct send shortly after a queued drain) each get their own confirmation
    /// instead of the second going unwatched, while re-arming the *same* send stays
    /// deduped. Cleared when the watcher thread resolves. Transient (not persisted).
    agent_submit_watch: HashSet<(String, u64)>,
    agent_drafts: HashMap<String, String>,
    recent_sessions: HashMap<String, RecentSessionInfo>,
    /// Agents whose currently-running (just-sent) queued turn requested a pause; when
    /// that turn finishes the agent enters paused mode. Transient (not persisted).
    agent_pending_pause: HashSet<String>,
    /// Agents whose user is actively typing (in the composer or terminal). While set,
    /// the queue is not auto-drained on idle, so a finishing turn can't spam a queued
    /// message into what the user is typing. Set/cleared by the frontend (debounced);
    /// transient (not persisted).
    agent_typing: HashSet<String>,
    /// Agents with a queued turn currently being drained (claimed and mid-send).
    /// Serializes draining per agent: a turn is claimed under the model lock and the
    /// agent id inserted here, so a concurrent drain trigger (idle hook, wait release,
    /// typing-clear, unpause, …) can't pop and send a second turn in the window before
    /// the first send marks the agent Running. Cleared once the send settles. Transient
    /// (not persisted).
    agent_draining: HashSet<String>,
    /// Agent-session resumes queued at restore, keyed by the recovered shell pane id;
    /// each is drained by that pane's respawn. Transient (not persisted).
    shell_agent_resumes: HashMap<String, ShellAgentResume>,
    /// The selected frontend tab, persisted so restarts return to the same place.
    /// The value is either a pane id or the frontend's Home tab sentinel.
    active_tab_id: Option<String>,
    /// Undo stack for explicitly closed tabs, most-recent last. Transient: closed tabs
    /// can be restored during the current app run (repeated undo reopens successive
    /// closes), but they are not resurrected after restart. Bounded by
    /// `MAX_CLOSED_PANE_UNDO`.
    closed_pane_stack: Vec<ClosedPaneSnapshot>,
}

/// A pending request to resume an agent session inside a recovered shell pane.
/// Captured during `restore_session` for a shell pane whose agent was still bound at
/// shutdown — the wrapper clears the binding when the agent process exits, so a
/// still-bound agent means it was running live — and consumed once by the pane's
/// respawn, which injects the adapter's resume command (`claude --resume <id>`,
/// `codex resume <id>`) into the new shell. Transient: never persisted.
#[derive(Clone, Debug)]
pub struct ShellAgentResume {
    pub adapter: String,
    pub session_id: String,
    /// The agent's original launch directory. Claude/Codex scope sessions by project
    /// dir, so the respawn must reopen here for `--resume` to resolve the session — and
    /// for the rebind to match — even if the pane's live cwd has since drifted via `cd`.
    pub cwd: String,
}

#[derive(Clone, Debug)]
pub struct ClosedPaneAgentSnapshot {
    pub agent: AgentInfo,
    pub turns: Vec<Turn>,
    pub queued_turns: Vec<QueuedTurn>,
    pub draft: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ClosedPaneSnapshot {
    pub pane: PaneInfo,
    pub group: Option<GroupInfo>,
    pub agent: Option<ClosedPaneAgentSnapshot>,
    pub orphaned_agents: Vec<ClosedPaneAgentSnapshot>,
    pub index: usize,
    pub scrollback: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentSessionInfo {
    pub id: String,
    pub adapter: String,
    pub group_id: Option<String>,
    pub session_id: Option<String>,
    pub transcript_path: Option<String>,
    pub worktree_dir: String,
    pub branch: Option<String>,
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub parent_id: Option<String>,
    pub fork_point: Option<String>,
    pub root_session_id: Option<String>,
    pub preview: Option<String>,
    #[serde(default)]
    pub line_count: usize,
    pub last_active_at: u128,
    pub created_at: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<AgentStatus>,
    #[serde(default)]
    pub missing: bool,
}

/// One session in a fork lineage, as shown in the turn pane's branch menu.
///
/// Flattens the two places a branch can live — a `RecentSessionInfo` row (the
/// durable record, which outlives the pane) and a live `AgentInfo` that has not
/// been recorded yet — into the single shape the frontend switches on.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchInfo {
    /// Handle `recent_session_resume` takes to revive this branch. `None` for a
    /// live agent with no recent-session row yet, which needs no reviving.
    pub recent_session_id: Option<String>,
    pub agent_id: Option<String>,
    pub pane_id: Option<String>,
    pub session_id: Option<String>,
    pub adapter: String,
    pub preview: Option<String>,
    pub status: Option<AgentStatus>,
    /// Session id this branch forked from; `None` for the lineage root.
    pub fork_point: Option<String>,
    /// True for the session every other branch descends from.
    pub is_root: bool,
    /// True while this branch still has an open pane.
    pub live: bool,
    /// True when the transcript or worktree is gone, so resuming would fail.
    pub missing: bool,
    pub last_active_at: u128,
    pub created_at: u128,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ResearchWorkspaceDependencies {
    pub tree_count: usize,
    pub has_active_runs: bool,
    pub has_live_panes: bool,
}

#[derive(Clone, Debug, Default)]
struct AgentSendTracking {
    outstanding_sends: VecDeque<AgentOutstandingSend>,
    ups_seq: u64,
    next_send_id: u64,
}

/// Backstop lifetime for an outstanding send that never echoes a UserPromptSubmit.
///
/// The primary cleanup is the per-idle `clear_agent_outstanding_sends` in
/// `advance_after_idle`: every turn boundary wipes the tracking, so an abandoned or
/// hookless send (the user cleared the pasted text with Esc, a slash command the TUI
/// ran without hooks, …) is gone by the next idle. This TTL only bounds the window
/// *between* idles, for an agent that stays busy without ever going idle.
///
/// It must be generous. A steer or queued send can legitimately sit un-echoed for
/// minutes — the TUI buffers it until the current turn boundary (a long tool call),
/// or is momentarily unresponsive (large paste replay, an open modal) when a queued
/// turn is drained into it. Pruning such a live send too early disarms the
/// double-drain guard at `transcript.rs` (`agent_has_outstanding_send_source`),
/// letting a late transcript abort marker drain a second turn on top of the first.
/// Five minutes comfortably clears any realistic single-turn delay while still
/// reaping a truly dead entry.
const OUTSTANDING_SEND_TTL_MS: u128 = 5 * 60 * 1_000;

impl AgentSendTracking {
    fn prune_expired(&mut self, now_ms: u128) {
        self.outstanding_sends
            .retain(|send| now_ms.saturating_sub(send.sent_at_ms) <= OUTSTANDING_SEND_TTL_MS);
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentSendSource {
    DirectSend,
    QueuedTurn,
    Steer,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentOutstandingSend {
    #[serde(default)]
    pub id: u64,
    pub text: String,
    pub sent_at_seq: u64,
    #[serde(default)]
    pub sent_at_ms: u128,
    pub source: AgentSendSource,
}

/// A prompt queued application-wide before it has an owner — the home view's
/// Drafts rail. Assigning one to an agent marks it consumed (kept for a while
/// as history) rather than deleting it, so an assignment that queues work is
/// still visible and a crash can never silently lose the text.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalDraft {
    pub id: String,
    pub text: String,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed: Option<GlobalDraftConsumed>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalDraftConsumed {
    pub agent_id: String,
    pub at: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedTurnWait {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Where a queued turn is delivered when it is reached. Absent on a turn means the
/// default: paste it into the owning agent's own pane. `Fork` resumes the source
/// session into a new forked pane launched with the turn text; `NewSession` starts
/// a fresh session of the same adapter in the source's directory. Either way the
/// source agent never runs the turn itself and stays idle.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind"
)]
pub enum QueuedTurnDelivery {
    Fork {
        #[serde(default)]
        use_worktree: bool,
    },
    NewSession,
}

static QUEUED_TURN_ID_SEQ: AtomicU64 = AtomicU64::new(0);

/// A stable, unique identity for a queued turn. Two queued turns can share the
/// same text and differ only in pause/wait/delivery metadata; without an id,
/// mutations that identify a turn by index+text can act on the wrong one when
/// duplicates shift position, so every turn carries an opaque id used for the
/// optimistic-concurrency guards. Random by default; the counter fallback keeps
/// ids unique if the CSPRNG is momentarily unavailable (this runs inside
/// infallible constructors, so it must never fail).
fn new_queued_turn_id() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_ok() {
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        return format!("qturn-{hex}");
    }
    let seq = QUEUED_TURN_ID_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("qturn-seq-{seq}")
}

/// A queued turn: an id, the text to send, plus optional directives controlling
/// when and where it should send. Deserializes from either a bare string (the
/// legacy persisted format) or a `{ text, pauseAfter, waitFor, delivery }`
/// object, so old state still loads; a turn persisted without an id is assigned
/// a fresh one on load.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedTurn {
    pub id: String,
    pub text: String,
    pub pause_after: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_for: Option<QueuedTurnWait>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<QueuedTurnDelivery>,
    /// Whether this turn's text may already be sitting in the pane's composer from
    /// a previous delivery attempt — a send whose paste landed but whose submit was
    /// never confirmed. Draining a tagged turn submits a bare Return instead of
    /// re-pasting, so a retry can never concatenate a second copy of the text onto
    /// the one already in the composer. The tag is process-local by design: it is
    /// never persisted, because the composer's contents do not survive an agent
    /// process restart, and a restored turn must re-paste normally.
    #[serde(skip)]
    pub possibly_pasted: bool,
}

impl QueuedTurn {
    pub fn new(text: String) -> Self {
        Self {
            id: new_queued_turn_id(),
            text,
            pause_after: false,
            wait_for: None,
            delivery: None,
            possibly_pasted: false,
        }
    }

    pub fn waiting(text: String, wait_for: QueuedTurnWait) -> Self {
        Self {
            id: new_queued_turn_id(),
            text,
            pause_after: false,
            wait_for: Some(wait_for),
            delivery: None,
            possibly_pasted: false,
        }
    }

    pub fn delivering(text: String, delivery: QueuedTurnDelivery) -> Self {
        Self {
            id: new_queued_turn_id(),
            text,
            pause_after: false,
            wait_for: None,
            delivery: Some(delivery),
            possibly_pasted: false,
        }
    }
}

impl<'de> Deserialize<'de> for QueuedTurn {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Text(String),
            Full {
                #[serde(default)]
                id: Option<String>,
                text: String,
                #[serde(default, rename = "pauseAfter")]
                pause_after: bool,
                #[serde(default, rename = "waitFor")]
                wait_for: Option<QueuedTurnWait>,
                #[serde(default)]
                delivery: Option<QueuedTurnDelivery>,
            },
        }
        Ok(match Repr::deserialize(deserializer)? {
            Repr::Text(text) => QueuedTurn {
                id: new_queued_turn_id(),
                text,
                pause_after: false,
                wait_for: None,
                delivery: None,
                possibly_pasted: false,
            },
            Repr::Full {
                id,
                text,
                pause_after,
                wait_for,
                delivery,
            } => QueuedTurn {
                // A turn persisted before turns had ids is migrated to a fresh
                // one; a stored id is preserved so it stays stable across loads.
                id: id.unwrap_or_else(new_queued_turn_id),
                text,
                pause_after,
                wait_for,
                delivery,
                // Deliberately never restored: the composer's contents do not
                // survive the agent process, so a loaded turn re-pastes normally.
                possibly_pasted: false,
            },
        })
    }
}

/// Result of [`AppState::claim_ready_agent_turn`].
pub enum AgentTurnClaim {
    /// A ready turn was claimed and popped; the agent is now marked draining. The caller
    /// must send it and then call [`AppState::finish_agent_drain`].
    Ready { turn: QueuedTurn, pending: usize },
    /// Another drain already holds this agent; the caller must not send or change status.
    Draining,
    /// Nothing is ready to send (empty queue or the front turn is still waiting).
    Idle,
}

/// Result of [`AppState::claim_next_turn_or_settle`].
pub enum IdleAdvance {
    /// A ready turn was claimed; the agent is marked draining and the caller must send it.
    Sent { turn: QueuedTurn, pending: usize },
    /// Another drain owns the agent; the caller must leave its status untouched.
    Busy,
    /// Nothing was sent; the agent has been settled to the requested ready status.
    Idle,
}

fn enqueue_queued_turn_locked(
    model: &mut Model,
    agent_id: &str,
    turn: QueuedTurn,
) -> Result<usize, String> {
    let queue = model
        .agent_turn_queues
        .entry(agent_id.to_string())
        .or_default();
    if queue.len() >= MAX_QUEUED_TURNS_PER_AGENT {
        return Err(format!(
            "turn queue is full ({MAX_QUEUED_TURNS_PER_AGENT} pending turns); wait for the agent to drain before queueing more"
        ));
    }
    queue.push_back(turn);
    Ok(queue.len())
}

fn wait_target_label_locked(model: &Model, target: &AgentInfo) -> Option<String> {
    target
        .pane_id
        .as_deref()
        .and_then(|pane_id| model.panes.get(pane_id))
        .map(|pane| pane.info.title.clone())
        .or_else(|| target.branch.clone())
        .or_else(|| target.model.clone())
}

/// Whether `ancestor` is `descendant` itself or one of its parent directories.
/// Canonicalizes both best-effort (falling back to the literal path when a side can't be
/// resolved, e.g. it doesn't exist) so symlinks and `.`/`..` can't defeat the check.
fn path_is_ancestor_or_equal(ancestor: &std::path::Path, descendant: &std::path::Path) -> bool {
    let ancestor = std::fs::canonicalize(ancestor).unwrap_or_else(|_| ancestor.to_path_buf());
    let descendant = std::fs::canonicalize(descendant).unwrap_or_else(|_| descendant.to_path_buf());
    descendant.starts_with(&ancestor)
}

fn queued_turn_wait_is_resolved_locked(model: &Model, wait_for: &QueuedTurnWait) -> bool {
    let Some(target) = model.agents.get(&wait_for.agent_id) else {
        // The target agent is gone entirely (e.g. its pane closed and the agent was
        // pruned). There is nothing left to wait on, so release the waiter rather than
        // block it on a ghost forever.
        return true;
    };
    // A Failed target keeps its waiters blocked, on purpose: "run this after X
    // finishes" must not silently fire when X errored out instead of completing.
    // Likewise an agent parked awaiting input or a permission prompt has not finished
    // its work, so its waiters stay blocked until it actually goes idle/done. These are
    // checked before the pane fallbacks below so a Failed target blocks even if its pane
    // binding was cleared — only the agent genuinely going away (above) releases a wait
    // on a failed target.
    if matches!(
        target.status,
        AgentStatus::Failed | AgentStatus::AwaitingInput | AgentStatus::AwaitingPermission
    ) {
        return false;
    }
    let Some(pane_id) = target.pane_id.as_deref() else {
        // The target has no pane (it was closed and parked). If it still carries an
        // orphaned queue, those turns are unfinished work: "run after X finishes its
        // queue" must stay blocked until that queue actually drains, not fire the moment
        // the pane closes. Only a parked target with an empty queue has nothing left to
        // finish, so it releases the waiter.
        return model
            .agent_turn_queues
            .get(&target.id)
            .is_none_or(|queue| queue.is_empty());
    };
    if !model.panes.contains_key(pane_id) {
        return true;
    }
    if model
        .agent_turn_queues
        .get(&target.id)
        .is_some_and(|queue| !queue.is_empty())
    {
        return false;
    }
    matches!(target.status, AgentStatus::Done | AgentStatus::Idle)
}

/// Pops the front queued turn for `agent_id` if it is ready to send — the queue is
/// non-empty and the front turn either has no wait dependency or its dependency has
/// resolved. Returns the popped turn and the remaining pending count, or `None` when
/// nothing is ready. Does not touch the draining guard; callers that serialize draining
/// manage that separately. Operates on an already-locked model.
fn pop_ready_locked(model: &mut Model, agent_id: &str) -> Option<(QueuedTurn, usize)> {
    let front_wait = {
        let queue = model.agent_turn_queues.get(agent_id)?;
        let front = queue.front()?;
        front.wait_for.clone()
    };
    if let Some(wait_for) = &front_wait
        && !queued_turn_wait_is_resolved_locked(model, wait_for)
    {
        return None;
    }
    let queue = model.agent_turn_queues.get_mut(agent_id)?;
    let turn = queue.pop_front()?;
    let pending_count = queue.len();
    if queue.is_empty() {
        model.agent_turn_queues.remove(agent_id);
        if let Some(agent) = model.agents.get_mut(agent_id) {
            agent.orphaned_queue_pane_id = None;
        }
    }
    Some((turn, pending_count))
}

fn sanitize_active_tab_id(tab_id: Option<String>) -> Option<String> {
    tab_id.and_then(|id| {
        let trimmed = id.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn sanitize_last_osc_title(raw_title: &str) -> Option<String> {
    let mut title = String::new();
    let mut chars = 0_usize;
    let mut pending_space = false;
    let mut truncated = false;

    for ch in raw_title.chars() {
        if ch.is_control() || ch.is_whitespace() {
            if !title.is_empty() {
                pending_space = true;
            }
            continue;
        }
        if pending_space {
            if chars >= MAX_LAST_OSC_TITLE_CHARS {
                truncated = true;
                break;
            }
            title.push(' ');
            chars += 1;
            pending_space = false;
        }
        if chars >= MAX_LAST_OSC_TITLE_CHARS {
            truncated = true;
            break;
        }
        title.push(ch);
        chars += 1;
    }

    if truncated {
        if title.ends_with(' ') {
            title.pop();
            chars -= 1;
        }
        if chars >= MAX_LAST_OSC_TITLE_CHARS {
            title.pop();
        }
        title.push('…');
    }

    (!title.is_empty()).then_some(title)
}

fn ensure_agent_thread_metadata(state: &AppState, model: &mut Model, agent: &mut AgentInfo) {
    let had_thread_id = agent
        .thread_id
        .as_deref()
        .is_some_and(|thread_id| !thread_id.trim().is_empty());
    if !had_thread_id {
        agent.thread_id = Some(state.next_id("thread"));
    }
    if agent
        .branch_id
        .as_deref()
        .is_none_or(|branch_id| branch_id.trim().is_empty())
    {
        agent.branch_id = Some(state.next_id("branch"));
    }
    if let (Some(thread_id), Some(branch_id)) = (&agent.thread_id, &agent.branch_id) {
        let default_focused_branch_id = model
            .thread_focus
            .get(thread_id)
            .cloned()
            .unwrap_or_else(|| branch_id.clone());
        model.threads.entry(thread_id.clone()).or_insert_with(|| {
            let workspace_root = &state.inner.config.workspace_root;
            let mut record = thread_graph::thread_record_for_agent(
                agent,
                &default_focused_branch_id,
                workspace_root,
            );
            // Builds that assigned agents thread ids before thread records
            // existed wrote graphs to <worktree>/.qmux/threads/<id>.json and
            // persisted no record, so the startup migration (which walks only
            // persisted records) never sees them. Minting a fresh global
            // record here would silently shadow that history behind an empty
            // graph — adopt the legacy worktree snapshot and migrate it into
            // global storage through the same machinery instead.
            if had_thread_id {
                let legacy_path = thread_graph::snapshot_path(&agent.worktree_dir, thread_id);
                if legacy_path.is_file() {
                    record.storage_root = agent.worktree_dir.clone();
                    record.snapshot_path = legacy_path.display().to_string();
                    if let Err(err) =
                        thread_graph::migrate_record_to_storage_root(&mut record, workspace_root)
                    {
                        // Keep the record pointed at the worktree copy: the
                        // history stays readable and the startup migration
                        // retries (and warns) on the next launch.
                        eprintln!(
                            "qmux: could not migrate legacy thread graph {}: {err}",
                            record.id
                        );
                    }
                }
            }
            record
        });
        model
            .thread_focus
            .entry(thread_id.clone())
            .or_insert_with(|| branch_id.clone());
    }
}

fn thread_store_for_agent_locked(
    model: &mut Model,
    agent: &AgentInfo,
    storage_root: &std::path::Path,
) -> (thread_graph::ThreadStore, bool) {
    let thread_id = thread_graph::agent_thread_id(agent);
    let branch_id = thread_graph::agent_branch_id(agent);
    let default_focused_branch_id = model
        .thread_focus
        .get(&thread_id)
        .cloned()
        .unwrap_or(branch_id);
    let existed = model.threads.contains_key(&thread_id);
    let record = model.threads.entry(thread_id).or_insert_with(|| {
        thread_graph::thread_record_for_agent(agent, &default_focused_branch_id, storage_root)
    });
    (
        thread_graph::ThreadStore::new(record.storage_root.clone()),
        !existed,
    )
}

fn migrate_thread_records_to_global(
    workspace_root: &std::path::Path,
    records: &mut HashMap<String, thread_graph::ThreadRecord>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for record in records.values_mut() {
        if let Err(err) = thread_graph::migrate_record_to_storage_root(record, workspace_root) {
            warnings.push(format!("could not migrate thread {}: {err}", record.id));
        }
    }
    warnings
}

fn wait_dependency_would_cycle_locked(model: &Model, source: &str, target: &str) -> bool {
    let mut seen = HashSet::new();
    let mut stack = vec![target.to_string()];
    while let Some(agent_id) = stack.pop() {
        if agent_id == source {
            return true;
        }
        if !seen.insert(agent_id.clone()) {
            continue;
        }
        if let Some(queue) = model.agent_turn_queues.get(&agent_id) {
            for turn in queue {
                let Some(wait_for) = turn.wait_for.as_ref() else {
                    continue;
                };
                if wait_for.agent_id == source
                    || !queued_turn_wait_is_resolved_locked(model, wait_for)
                {
                    stack.push(wait_for.agent_id.clone());
                }
            }
        }
    }
    false
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "status"
)]
pub enum AgentPromptSubmitMatch {
    Matched {
        source: AgentSendSource,
        outstanding_sends: usize,
    },
    Mismatched {
        expected: String,
        actual: String,
        outstanding_sends: usize,
    },
    Untracked {
        actual: String,
        outstanding_sends: usize,
    },
    MissingPrompt {
        outstanding_sends: usize,
    },
}

/// What a submit-confirmation watch observes when it re-checks a send it wrote to a
/// pane (see `check_agent_submit_watch`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitWatchStatus {
    /// The send's prompt-submit echo popped it (or an idle boundary cleared the
    /// tracking): the turn started, the watch stands down.
    Confirmed,
    /// The send is still outstanding, but *some* UserPromptSubmit arrived after it
    /// was written — most likely this very turn submitting with text that failed
    /// the exact match (a mangled or concatenated paste). Recovery must stand down:
    /// a Return nudge or a requeue on top of a turn that actually started risks a
    /// duplicate, and a visible stall is the safer failure.
    StillPendingWithPromptActivity,
    /// The send is still outstanding and no prompt of any kind has been submitted
    /// since it was written: the turn shows no sign of having started.
    StillPending,
}

pub struct PaneRuntime {
    pub info: PaneInfo,
    pub backend: PaneBackend,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneInfo {
    pub id: String,
    pub title: String,
    /// Last title reported by OSC 0/2. Kept separate from `title`: the latter is
    /// the durable user/generated name and must continue to override terminal
    /// programs when it differs from the pane's default title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_osc_title: Option<String>,
    pub kind: PaneKind,
    pub agent_id: Option<String>,
    pub group_id: String,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    pub status: PaneStatus,
    /// Wall-clock millis when this pane was last focused. Stamped at spawn and on
    /// every activation (`touch_pane_active`); consulted to pick a group's
    /// most-recently-active shell pane when resolving a spawn cwd. `#[serde(default)]`
    /// so pre-existing persisted state loads as 0 ("least recent until first focus").
    #[serde(default)]
    pub last_active_at: u128,
    /// True for panes recreated from persisted state on restart. Set at respawn
    /// time only; the persisted value is never consulted when reloading.
    #[serde(default)]
    pub recovered: bool,
    /// Sidebar nesting depth (0 = root). Stamped from `Model.pane_depth` by
    /// `ordered_panes`; persisted so the tree survives a restart.
    #[serde(default)]
    pub depth: u16,
}

/// Hard cap on nesting depth so a deep chain can never make the sidebar unusable.
/// Mirrored by `MAX_PANE_DEPTH` in the frontend's pane-tree helpers.
pub const MAX_PANE_DEPTH: u16 = 8;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneSplitInfo {
    pub id: String,
    pub pane_ids: Vec<String>,
    #[serde(default)]
    pub sizes: HashMap<String, f64>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub intent: HashMap<String, PaneSplitIntent>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneSplitIntent {
    pub kind: String,
    pub anchor_pane_id: String,
    pub position: String,
    pub source: String,
    #[serde(default)]
    pub created_at: f64,
}

/// One entry in a `set_pane_layout` request: a pane and its target nesting depth,
/// in sidebar order.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneLayoutEntry {
    pub pane_id: String,
    pub depth: u16,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PaneKind {
    Shell,
    Agent,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PaneStatus {
    Starting,
    Running,
    Exited,
    Killed,
    Failed,
}

impl AppState {
    pub fn new(config: QmuxConfig) -> Self {
        let diagnostics = Diagnostics::new(&config.workspace_root);
        Self {
            inner: Arc::new(AppStateInner {
                config,
                diagnostics,
                pane_tokens: Mutex::new(HashMap::new()),
                file_tokens: Mutex::new(HashMap::new()),
                model: Mutex::new(Model::default()),
                transcript_tails: Mutex::new(HashSet::new()),
                next_id: AtomicU64::new(1),
                app_handle: Mutex::new(None),
                persist_enabled: AtomicBool::new(false),
                persist_lock: Mutex::new(()),
                research_document_lock: Mutex::new(()),
                persist_dirty: Mutex::new(false),
                persist_wake: Condvar::new(),
                persister_spawned: AtomicBool::new(false),
                last_osc_title_persist_scheduled: AtomicBool::new(false),
                recovery_warning: Mutex::new(None),
                preflighted_state: Mutex::new(None),
                exit_confirmed: AtomicBool::new(false),
                exit_teardown_started: AtomicBool::new(false),
                file_server: Mutex::new(None),
                control_socket_identity: Mutex::new(None),
                pane_send_locks: Mutex::new(HashMap::new()),
                shell_agent_jobs: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn register_shell_agent_job(
        &self,
        job_id: String,
        agent_id: String,
        pane_id: String,
        supervisor_pid: u32,
    ) -> Result<ShellAgentJobInfo, String> {
        if job_id.trim().is_empty() || supervisor_pid == 0 {
            return Err("shell agent job metadata is invalid".to_string());
        }
        let info = ShellAgentJobInfo {
            job_id: job_id.clone(),
            agent_id,
            pane_id,
            state: ShellAgentJobState::Foreground,
        };
        let mut jobs = self
            .inner
            .shell_agent_jobs
            .lock()
            .map_err(|_| "shell agent job lock poisoned".to_string())?;
        jobs.insert(
            job_id,
            ShellAgentJob {
                info: info.clone(),
                supervisor_pid,
                missing_samples: 0,
            },
        );
        Ok(info)
    }

    pub fn list_shell_agent_jobs(&self) -> Result<Vec<ShellAgentJobInfo>, String> {
        let jobs = self
            .inner
            .shell_agent_jobs
            .lock()
            .map_err(|_| "shell agent job lock poisoned".to_string())?;
        Ok(jobs.values().map(|job| job.info.clone()).collect())
    }

    pub(crate) fn shell_agent_job_targets(&self) -> Vec<ShellAgentJobTarget> {
        self.inner
            .shell_agent_jobs
            .lock()
            .map(|jobs| {
                jobs.values()
                    .map(|job| ShellAgentJobTarget {
                        job_id: job.info.job_id.clone(),
                        supervisor_pid: job.supervisor_pid,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn update_shell_agent_job_sample(
        &self,
        job_id: &str,
        state: ShellAgentJobState,
    ) -> Option<ShellAgentJobInfo> {
        let mut jobs = self.inner.shell_agent_jobs.lock().ok()?;
        let job = jobs.get_mut(job_id)?;
        job.missing_samples = 0;
        if job.info.state == state {
            return None;
        }
        job.info.state = state;
        Some(job.info.clone())
    }

    /// Records that a supervisor was absent from one successful process-table
    /// sample. Two consecutive misses are required before retiring it so a fork/exit
    /// race or a transiently incomplete `ps` result cannot detach a live primary.
    pub(crate) fn note_shell_agent_job_missing(&self, job_id: &str) -> Option<ShellAgentJobInfo> {
        let mut jobs = self.inner.shell_agent_jobs.lock().ok()?;
        let job = jobs.get_mut(job_id)?;
        job.missing_samples = job.missing_samples.saturating_add(1);
        if job.missing_samples < 2 {
            return None;
        }
        jobs.remove(job_id).map(|job| job.info)
    }

    pub fn unregister_shell_agent_job(
        &self,
        job_id: &str,
        agent_id: Option<&str>,
        pane_id: Option<&str>,
    ) -> Option<ShellAgentJobInfo> {
        let mut jobs = self.inner.shell_agent_jobs.lock().ok()?;
        let matches = jobs.get(job_id).is_some_and(|job| {
            agent_id.is_none_or(|agent_id| job.info.agent_id == agent_id)
                && pane_id.is_none_or(|pane_id| job.info.pane_id == pane_id)
        });
        matches
            .then(|| jobs.remove(job_id))
            .flatten()
            .map(|job| job.info)
    }

    pub fn unregister_shell_agent_jobs_for_pane(&self, pane_id: &str) -> Vec<ShellAgentJobInfo> {
        let Ok(mut jobs) = self.inner.shell_agent_jobs.lock() else {
            return Vec::new();
        };
        let job_ids = jobs
            .iter()
            .filter(|(_, job)| job.info.pane_id == pane_id)
            .map(|(job_id, _)| job_id.clone())
            .collect::<Vec<_>>();
        job_ids
            .into_iter()
            .filter_map(|job_id| jobs.remove(&job_id).map(|job| job.info))
            .collect()
    }

    /// Records the loopback file server's port + access token (set once at startup).
    pub fn set_file_server(&self, port: u16) {
        if let Ok(mut slot) = self.inner.file_server.lock() {
            *slot = Some(port);
        }
    }

    pub fn file_server_port(&self) -> Option<u16> {
        self.inner.file_server.lock().ok().and_then(|slot| *slot)
    }

    /// Records the (device, inode) of the control socket this process bound (set once
    /// at startup, right after the bind).
    pub fn set_control_socket_identity(&self, device: u64, inode: u64) {
        if let Ok(mut slot) = self.inner.control_socket_identity.lock() {
            *slot = Some((device, inode));
        }
    }

    /// Whether the file currently at the control socket path is still the one this
    /// process bound. False when another instance has since unlinked and re-bound the
    /// path (its socket must not be deleted out from under it on our exit), and false
    /// when the path is gone or was never recorded — there is nothing of ours to
    /// reclaim either way.
    pub fn owns_control_socket(&self) -> bool {
        let Ok(slot) = self.inner.control_socket_identity.lock() else {
            return false;
        };
        let Some((device, inode)) = *slot else {
            return false;
        };
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(&self.inner.config.socket_path)
            .map(|meta| meta.dev() == device && meta.ino() == inode)
            .unwrap_or(false)
    }

    /// The directory a newly-created group opens in when the caller doesn't give an
    /// explicit path: the user's home directory, else the qmux process cwd. The home step
    /// keeps a Finder/Dock launch — whose process cwd is the filesystem root — from
    /// opening shells at `/`.
    pub fn default_open_dir(&self) -> std::path::PathBuf {
        if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from)
            && home.is_dir()
        {
            return home;
        }
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
    }

    /// Empty, qmux-managed working directory used when the user has not chosen
    /// a project for research. It is deliberately separate from `.qmux`, which
    /// contains private state and terminal credentials.
    pub fn default_research_dir(&self) -> std::path::PathBuf {
        self.inner
            .config
            .workspace_root
            .join(".research")
            .join("default")
    }

    /// The working directory a newly opened shell should inherit from `pane_id`: the
    /// live cwd of that pane when it is a shell whose directory still exists. Agent
    /// panes (rooted in a worktree) and stale or missing directories yield `None`, so
    /// the caller falls back to `default_open_dir`.
    pub fn inheritable_shell_cwd(&self, pane_id: &str) -> Option<std::path::PathBuf> {
        let model = self.inner.model.lock().ok()?;
        let pane = model.panes.get(pane_id)?;
        if !matches!(pane.info.kind, PaneKind::Shell) {
            return None;
        }
        let cwd = std::path::PathBuf::from(&pane.info.cwd);
        cwd.is_dir().then_some(cwd)
    }

    /// The advisory cwd for spawning into `group_id`: the live cwd of the group's
    /// most-recently-active shell pane. Groups are not directory-scoped, so this
    /// derives a sensible spawn directory from where work in the group actually is,
    /// rather than a stored group directory. Only shell panes with a still-existing
    /// cwd count (agent panes are rooted in worktrees; a stale dir is unusable), so
    /// an empty group — or one holding only agent panes — yields `None` and the
    /// caller falls back to `default_open_dir`. Ties on `last_active_at` (e.g. two
    /// panes stamped in the same millisecond) resolve arbitrarily; the recency
    /// signal is advisory.
    pub fn group_spawn_cwd(&self, group_id: &str) -> Option<std::path::PathBuf> {
        let model = self.inner.model.lock().ok()?;
        model
            .panes
            .values()
            .filter(|pane| pane.info.group_id == group_id)
            .filter(|pane| matches!(pane.info.kind, PaneKind::Shell))
            .filter_map(|pane| {
                let cwd = std::path::PathBuf::from(&pane.info.cwd);
                cwd.is_dir().then_some((pane.info.last_active_at, cwd))
            })
            .max_by_key(|(last_active_at, _)| *last_active_at)
            .map(|(_, cwd)| cwd)
    }

    /// Stamps `pane_id` as the most-recently-focused pane. Called on every
    /// activation from the frontend, so it must stay cheap: it mutates in memory
    /// only and deliberately does not `persist()` (a disk write per focus would be a
    /// write storm) nor emit an event (nothing renders off this yet). The fresh
    /// timestamp rides along on the next persist triggered by other activity; losing
    /// the last few stamps to a crash only nudges the spawn-cwd heuristic.
    pub fn touch_pane_active(&self, pane_id: &str) {
        if let Ok(mut model) = self.inner.model.lock()
            && let Some(pane) = model.panes.get_mut(pane_id)
        {
            pane.info.last_active_at = now_millis();
        }
    }

    /// Whether shells should run as login shells (sourcing the user's login
    /// profile files). Persisted in preferences; defaults to on when unset so a
    /// fresh install matches how terminal emulators launch shells. Read on the
    /// spawn path — including startup recovery, which runs before the frontend
    /// reconnects — so the persisted choice survives a restart.
    pub fn use_login_shell(&self) -> bool {
        persistence::load_preferences(&self.inner.config.workspace_root)
            .ok()
            .and_then(|prefs| prefs.use_login_shell)
            .unwrap_or(true)
    }

    /// Returns the file-server token scoped to a single pane, minting one on first use.
    /// This token rides in the path of the browser-overlay URLs that pane opens, so the
    /// loopback file server can map a request back to its pane and serve only that
    /// pane's roots (`pane_file_roots`) — never the union of every pane's directories.
    /// It is deliberately separate from the pane's control-socket token (`pane_token`):
    /// it lands in URLs that flow through the frontend, so its only authority is reading
    /// files under the one pane's roots, not driving the control socket.
    pub fn pane_file_token(&self, pane_id: &str) -> Result<String, String> {
        let mut tokens = self
            .inner
            .file_tokens
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if let Some(existing) = tokens.get(pane_id) {
            return Ok(existing.clone());
        }
        let token = random_token()?;
        Ok(tokens.entry(pane_id.to_string()).or_insert(token).clone())
    }

    /// Resolves the pane a presented file-server token belongs to, if any.
    pub fn pane_for_file_token(&self, token: &str) -> Option<String> {
        let tokens = self
            .inner
            .file_tokens
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        tokens
            .iter()
            .find_map(|(pane_id, pane_token)| (pane_token == token).then(|| pane_id.clone()))
    }

    /// Roots a `browser.open` from a specific pane may reach. A known pane is scoped
    /// strictly to its own working area — its group directory, its cwd, and (if any) its
    /// agent's worktree — and deliberately *not* the whole workspace root. The workspace
    /// root holds every group's directory, all transcripts, hook settings and
    /// `.qmux/state.json`, so serving it to every pane would let one pane's file token
    /// render another agent's private files. (Same-group panes intentionally share a
    /// group directory.)
    ///
    /// A pane that isn't in the model resolves to *zero* roots (fail closed), so the
    /// file server rejects every request (`resolve_under_roots` → 403) rather than
    /// serving the whole workspace tree. The documented invariant is that no live file
    /// token resolves to an out-of-model pane (`remove_pane` reclaims the token, and
    /// `pane_for_file_token` 404s a stale one before roots are consulted), so failing
    /// closed here costs nothing real while removing a fail-open widening that would
    /// otherwise expose every group's directory, all transcripts, and
    /// `.qmux/{state,preferences}.json` (the plaintext OpenRouter key) if that invariant
    /// were ever broken by a spawn/teardown race or a poisoned lock.
    pub fn pane_file_roots(&self, pane_id: &str) -> Vec<std::path::PathBuf> {
        // Recover a poisoned lock, matching the sibling token accessors
        // (`pane_token`/`pane_for_token`/…). Using `if let Ok(model)` here would fail
        // *open* to the whole workspace after any unrelated panic that held this lock;
        // recover instead so the per-pane scoping below still applies.
        let model = self
            .inner
            .model
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if let Some(pane) = model.panes.get(pane_id) {
            let mut roots = Vec::new();
            let group_dir = model
                .groups
                .get(&pane.info.group_id)
                .map(|group| std::path::PathBuf::from(&group.dir));
            if let Some(group_dir) = &group_dir {
                roots.push(group_dir.clone());
            }
            // Serve the pane's cwd so `qmux open ./file` resolves — but keep it from
            // widening the pane's scope past its own working area. `pane.set_cwd`
            // accepts any existing absolute directory (a pane legitimately runs in
            // arbitrary project dirs), so constrain what becomes a servable root:
            //  - never at or above the workspace root (`/`, `~`, or the workspace
            //    itself would expose every group's directory, all transcripts, and
            //    `.qmux/{state,preferences}.json`), and
            //  - never a *sibling group's* subtree. A cwd that lives *inside* the
            //    workspace root is only served when it is under this pane's own group
            //    dir; legitimate external project dirs live outside the workspace root
            //    and stay served. The group dir and agent worktree are served
            //    unconditionally above.
            let cwd = std::path::PathBuf::from(&pane.info.cwd);
            let workspace_root = &self.inner.config.workspace_root;
            let cwd_at_or_above_workspace = path_is_ancestor_or_equal(&cwd, workspace_root);
            let cwd_inside_workspace = path_is_ancestor_or_equal(workspace_root, &cwd);
            let cwd_under_own_group = group_dir
                .as_deref()
                .is_some_and(|group_dir| path_is_ancestor_or_equal(group_dir, &cwd));
            if !cwd_at_or_above_workspace && (!cwd_inside_workspace || cwd_under_own_group) {
                roots.push(cwd);
            }
            for agent in model.agents.values() {
                if agent.pane_id.as_deref() == Some(pane_id) {
                    roots.push(std::path::PathBuf::from(&agent.worktree_dir));
                }
            }
            return roots;
        }
        Vec::new()
    }

    pub fn config(&self) -> &QmuxConfig {
        &self.inner.config
    }

    /// Loads persisted metadata into the in-memory model and enables persistence.
    ///
    /// Groups, agents and queued turns are hydrated directly. Panes are *not*:
    /// their persisted runtimes are stale (the old PTYs died with the previous
    /// process), so the pane metadata is returned for the caller to respawn into
    /// fresh PTYs. Returns the recoverable pane infos in a stable order.
    /// Checks the persisted state file is readable before `restore_session`
    /// hydrates and enables saving. Returns `Err` with a user-facing message when
    /// the file exists but cannot be read, so startup can abort loudly instead of
    /// overwriting an intact session with an empty one. See
    /// [`persistence::preflight_state`].
    pub fn preflight_persisted_state(&self) -> Result<(), String> {
        let raw = persistence::preflight_state(&self.inner.config.workspace_root)?;
        // Keep the bytes for restore_session so hydration reuses this read
        // instead of re-reading and re-parsing the file.
        if let Ok(mut slot) = self.inner.preflighted_state.lock() {
            *slot = raw;
        }
        Ok(())
    }

    /// The warning produced while loading persisted state, if any, taken once so
    /// startup can show it in a GUI dialog.
    pub fn take_recovery_warning(&self) -> Option<String> {
        self.inner
            .recovery_warning
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }

    pub fn restore_session(&self) -> Vec<PaneInfo> {
        // Reuse the bytes preflight already read; when there was no preflight
        // (tests, or a first run with nothing on disk) this falls back to
        // reading the file itself.
        let preread = self
            .inner
            .preflighted_state
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        let outcome =
            persistence::load_with_diagnostics_from(&self.inner.config.workspace_root, preread);
        let source_version = outcome.source_version;
        let persistence_warning = outcome.warning.map(|warning| warning.message);
        let mut persisted = outcome.state;
        // Workspace migration allocates durable ids, so restore the allocator
        // before reconciliation rather than waiting until hydration completes.
        if persisted.next_id > self.inner.next_id.load(Ordering::Relaxed) {
            self.inner
                .next_id
                .store(persisted.next_id, Ordering::Relaxed);
        }
        let (mut research_reconciled, mut migration_warnings) =
            migrate_legacy_research_workspaces(self, &mut persisted);
        if source_version == Some(2)
            && let Err(err) =
                persistence::backup_v2_state_for_migration(&self.inner.config.workspace_root)
        {
            migration_warnings.push(err);
        }
        // The tree owns future execution. Node group ids are retained only as
        // compatibility/provenance fields, so reconcile stale copies to the
        // authoritative workspace before any recovery launch can consult them.
        let tree_workspaces = persisted
            .research_trees
            .iter()
            .map(|(tree_id, tree)| (tree_id.clone(), tree.workspace_id.clone()))
            .collect::<HashMap<_, _>>();
        for node in persisted.research_nodes.values_mut() {
            if let Some(workspace_id) = tree_workspaces
                .get(&node.tree_id)
                .filter(|workspace_id| !workspace_id.trim().is_empty())
                && node.group_id != *workspace_id
            {
                node.group_id = workspace_id.clone();
                research_reconciled = true;
            }
        }
        // Structural reconciliation, iterated to a fixpoint: a node needs an
        // existing tree and an existing same-tree parent; a tree needs a root
        // node that is actually its own parentless root. Each removal can
        // invalidate further references (a dropped parent orphans its
        // descendants, a dropped root drops its tree, which drops the tree's
        // remaining nodes), so one pass is not enough.
        loop {
            let mut changed = false;
            let valid_tree_ids = persisted
                .research_trees
                .keys()
                .cloned()
                .collect::<HashSet<_>>();
            let node_tree_by_id = persisted
                .research_nodes
                .iter()
                .map(|(id, node)| (id.clone(), node.tree_id.clone()))
                .collect::<HashMap<_, _>>();
            persisted.research_nodes.retain(|_, node| {
                let tree_ok = valid_tree_ids.contains(&node.tree_id);
                let parent_ok = node
                    .parent_node_id
                    .as_ref()
                    .is_none_or(|parent_id| node_tree_by_id.get(parent_id) == Some(&node.tree_id));
                let keep = tree_ok && parent_ok;
                changed |= !keep;
                keep
            });
            let nodes = &persisted.research_nodes;
            persisted.research_trees.retain(|tree_id, tree| {
                let keep = !tree.workspace_id.trim().is_empty()
                    && nodes.get(&tree.root_node_id).is_some_and(|root| {
                        root.tree_id == *tree_id && root.parent_node_id.is_none()
                    });
                changed |= !keep;
                keep
            });
            research_reconciled |= changed;
            if !changed {
                break;
            }
        }
        // Research runs never survive a restart. Every pane in a Research
        // workspace is a one-shot hidden launch (shells and ordinary agents are
        // rejected there) whose interrupted turn died with the old process; a
        // recovered adapter resumes *Idle*, which the agent sync would read as
        // Complete and permanently snapshot a partial answer. Drop the panes
        // from recovery outright — respawning a hidden TUI only to reclaim it
        // buys nothing — and settle every still-active node as failed, since
        // nothing that could finish it remains.
        let research_group_ids = persisted
            .groups
            .iter()
            .filter(|group| group.scope == WorkspaceScope::Research)
            .map(|group| group.id.as_str())
            .collect::<HashSet<_>>();
        let dropped_research_pane_ids = persisted
            .panes
            .iter()
            .filter(|pane| research_group_ids.contains(pane.group_id.as_str()))
            .map(|pane| pane.id.clone())
            .collect::<HashSet<_>>();
        if !dropped_research_pane_ids.is_empty() {
            persisted
                .panes
                .retain(|pane| !dropped_research_pane_ids.contains(&pane.id));
            // Mirror remove_pane's agent reclamation for panes that will never
            // pass through it: a dropped pane's agent has nothing left to own
            // (research runs cannot hold queued turns — guarded anyway), and
            // keeping the record accumulated one dead AgentInfo in state.json
            // per interrupted run, with nothing that would ever reap it.
            let dropped_agent_ids = persisted
                .agents
                .iter()
                .filter(|agent| {
                    agent
                        .pane_id
                        .as_deref()
                        .is_some_and(|pane_id| dropped_research_pane_ids.contains(pane_id))
                        && persisted
                            .queues
                            .get(&agent.id)
                            .is_none_or(|turns| turns.is_empty())
                        && !persisted.inflight.contains_key(&agent.id)
                })
                .map(|agent| agent.id.clone())
                .collect::<HashSet<_>>();
            persisted
                .agents
                .retain(|agent| !dropped_agent_ids.contains(&agent.id));
            for agent in &mut persisted.agents {
                // Kept only because it still holds recoverable queued work.
                if agent
                    .pane_id
                    .as_deref()
                    .is_some_and(|pane_id| dropped_research_pane_ids.contains(pane_id))
                {
                    agent.pane_id = None;
                }
            }
            for group in &mut persisted.groups {
                group
                    .agents
                    .retain(|agent_id| !dropped_agent_ids.contains(agent_id));
            }
            for agent_id in &dropped_agent_ids {
                persisted.queues.remove(agent_id);
                persisted.drafts.remove(agent_id);
            }
            research_reconciled = true;
        }
        for node in persisted.research_nodes.values_mut() {
            // Also covers bindings to panes that were never persisted (crash
            // during multi-stage removal): either way the pane is gone, and a
            // stale binding would count the node as an active run forever.
            if node.pane_id.take().is_some() {
                research_reconciled = true;
            }
            if node.status.is_active() {
                node.status = ResearchNodeStatus::Failed;
                node.error =
                    Some("research run was interrupted before it could resume".to_string());
                node.completed_at = Some(now_millis());
                research_reconciled = true;
            }
        }
        // Snapshots for nodes the passes above dropped (or that a crash left
        // behind mid tree-removal) have no other reaper. Prune against the
        // surviving node set now that it is final — but never off a degraded
        // load: a corrupt state file reads as "no nodes", and pruning against
        // that would destroy every snapshot the user might still recover.
        if persistence_warning.is_none() {
            let surviving_research_node_ids = persisted
                .research_nodes
                .keys()
                .cloned()
                .collect::<HashSet<_>>();
            if let Err(err) = research::prune_response_snapshots(
                &self.inner.config.workspace_root,
                &surviving_research_node_ids,
            ) {
                eprintln!("qmux: {err}");
            }
        }
        migration_warnings.extend(migrate_thread_records_to_global(
            &self.inner.config.workspace_root,
            &mut persisted.threads,
        ));
        let recovery_warning = match (persistence_warning, migration_warnings.is_empty()) {
            (Some(warning), true) => Some(warning),
            (Some(warning), false) => {
                Some(format!("{warning}\n\n{}", migration_warnings.join("\n")))
            }
            (None, false) => Some(migration_warnings.join("\n")),
            (None, true) => None,
        };
        if let Some(warning) = recovery_warning {
            eprintln!("qmux: {warning}");
            if let Ok(mut slot) = self.inner.recovery_warning.lock() {
                *slot = Some(warning);
            }
        }
        let active_tab_id = sanitize_active_tab_id(persisted.active_tab_id.clone());
        let shell_pane_ids = persisted
            .panes
            .iter()
            .filter(|&pane| matches!(pane.kind, PaneKind::Shell))
            .map(|pane| pane.id.clone())
            .collect::<HashSet<_>>();
        let queued_agent_ids = persisted
            .queues
            .iter()
            .filter(|&(_agent_id, turns)| !turns.is_empty())
            .map(|(agent_id, _turns)| agent_id.clone())
            // An in-flight turn (claimed pre-shutdown, delivery unconfirmed) is
            // re-queued below, so its agent counts as having pending work too.
            .chain(persisted.inflight.keys().cloned())
            .collect::<HashSet<_>>();

        let mut hydrated_agents = Vec::new();
        if let Ok(mut model) = self.inner.model.lock() {
            for group in persisted.groups {
                if !model.group_order.iter().any(|id| id == &group.id) {
                    model.group_order.push(group.id.clone());
                }
                model.groups.insert(group.id.clone(), group);
            }
            if !persisted.group_order.is_empty() {
                let mut seen = HashSet::new();
                model.group_order = persisted
                    .group_order
                    .into_iter()
                    .filter(|id| model.groups.contains_key(id) && seen.insert(id.clone()))
                    .collect();
                let mut missing = model
                    .groups
                    .keys()
                    .filter(|id| !seen.contains(*id))
                    .cloned()
                    .collect::<Vec<_>>();
                missing.sort();
                model.group_order.extend(missing);
            }
            model.threads = persisted.threads;
            model.thread_focus = persisted.thread_focus;
            model.research_trees = persisted.research_trees;
            model.research_tree_order = persisted.research_tree_order;
            let normalized_research_order = ordered_research_tree_ids(&model);
            if normalized_research_order != model.research_tree_order {
                research_reconciled = true;
                model.research_tree_order = normalized_research_order;
            }
            model.research_nodes = persisted.research_nodes;
            // Reconcile the grouping against the authoritative tree set now, under
            // the model lock with every recovered tree present — the one place a
            // prune is safe. Membership/stars for trees that vanished while the app
            // was off (deleted elsewhere) drop here instead of on every refresh
            // against a possibly-incomplete navigation snapshot.
            model.research_folders = persisted.research_folders;
            let known_research_tree_ids =
                model.research_trees.keys().cloned().collect::<HashSet<_>>();
            research::reconcile_research_folder_state(
                &mut model.research_folders,
                &known_research_tree_ids,
            );
            // Same discipline for the encyclopedia layer: reconcile against
            // the authoritative group set, under the model lock, at load.
            model.encyclopedia = persisted.encyclopedia;
            let known_group_ids = model.groups.keys().cloned().collect::<HashSet<_>>();
            encyclopedia::reconcile_encyclopedia_state(&mut model.encyclopedia, &known_group_ids);
            for mut agent in persisted.agents {
                ensure_agent_thread_metadata(self, &mut model, &mut agent);
                if let Some(pane_id) = agent
                    .pane_id
                    .clone()
                    .filter(|pane_id| shell_pane_ids.contains(pane_id))
                {
                    // The agent was still bound to its shell pane at shutdown, so it was
                    // running live (the wrapper detaches on the agent process exiting).
                    // Queue a resume for the pane's respawn when the session is still
                    // recoverable, before clearing the now-stale binding.
                    if let Some(resume) = shell_agent_resume(&agent) {
                        model.shell_agent_resumes.insert(pane_id.clone(), resume);
                    }
                    agent.pane_id = None;
                    agent.status = AgentStatus::Idle;
                    agent.orphaned_queue_pane_id =
                        queued_agent_ids.contains(&agent.id).then_some(pane_id);
                } else if !queued_agent_ids.contains(&agent.id) {
                    agent.orphaned_queue_pane_id = None;
                }
                model.agents.insert(agent.id.clone(), agent);
            }
            for (agent_id, turns) in persisted.queues {
                if !turns.is_empty() {
                    model
                        .agent_turn_queues
                        .insert(agent_id, turns.into_iter().collect());
                }
            }
            // Re-queue any in-flight turn (claimed for delivery but not confirmed before
            // shutdown) at the front of its agent's queue, so it's re-delivered rather
            // than lost. A crash in the tiny window after delivery but before the record
            // cleared re-sends it — at-least-once, preferred over a silent drop. Live
            // in-flight state starts empty after restore.
            for (agent_id, turn) in persisted.inflight {
                model
                    .agent_turn_queues
                    .entry(agent_id)
                    .or_default()
                    .push_front(turn);
            }
            for (agent_id, draft) in persisted.drafts {
                // Drop drafts whose agent no longer exists so dead entries don't
                // accumulate in state.json across restarts. (Agents are hydrated above.)
                if !draft.trim().is_empty() && model.agents.contains_key(&agent_id) {
                    model.agent_drafts.insert(agent_id, draft);
                }
            }
            model.global_drafts = persisted.global_drafts;
            for session in persisted.recent_sessions {
                if !session.id.trim().is_empty() {
                    model.recent_sessions.insert(session.id.clone(), session);
                }
            }
            model.active_tab_id = active_tab_id;
            model.pane_splits = persisted.pane_splits;
            hydrated_agents = model.agents.values().cloned().collect::<Vec<_>>();
            // Seed nesting depth from the persisted panes. Panes are re-inserted by
            // the respawn pass that follows; depths for panes that don't come back
            // (e.g. already-exited panes) are pruned by the post-respawn normalize.
            for pane in &persisted.panes {
                if pane.depth != 0 {
                    model.pane_depth.insert(pane.id.clone(), pane.depth);
                }
            }
        }

        // Backfill recent-session entries for the hydrated agents after the
        // hydrate lock is released: a cold entry's preview/line-count comes
        // from reading (and parsing the head of) its transcript file, and with
        // many recovered agents doing that under the model lock serialized
        // startup — and every early command — behind the file reads.
        let now = now_millis();
        for agent in &hydrated_agents {
            self.upsert_recent_session_for_agent(agent, now, false);
        }

        // Enable persistence only after hydration so loading does not rewrite the
        // file, but before respawn so respawned panes get persisted.
        self.inner.persist_enabled.store(true, Ordering::Relaxed);
        self.spawn_persister();
        if research_reconciled {
            // Migration/reconciliation may have created durable workspace
            // manifests. Commit the matching state snapshot before recovery
            // continues so a crash cannot leave only half of that relationship.
            self.persist_now();
        }

        persisted.panes
    }

    /// Records that the model changed and needs persisting. The write itself is
    /// debounced onto the persister thread (see `spawn_persister`); in tests it
    /// runs synchronously so state files can be asserted right after a mutation.
    /// Best-effort either way: a failed write is logged but never propagated, so
    /// it cannot break a mutation.
    fn persist(&self) {
        // Cheap early-out while persistence is disabled (hydration, bare test
        // states). The writer re-checks the flag under `persist_lock`, so this
        // unlocked read can never race `finalize_persistence_for_exit` into
        // clobbering the final snapshot — at worst a mutation made while
        // disabled marks nothing, which is today's behavior too.
        if !self.inner.persist_enabled.load(Ordering::Relaxed) {
            return;
        }
        if cfg!(test) {
            self.persist_now();
            return;
        }
        let mut dirty = self
            .inner
            .persist_dirty
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *dirty = true;
        self.inner.persist_wake.notify_one();
    }

    /// Starts the background writer that turns dirty marks into debounced
    /// snapshots. Called once when persistence is enabled; a second call is a
    /// no-op. The thread parks on the condvar between bursts, so an idle app
    /// costs nothing.
    fn spawn_persister(&self) {
        if cfg!(test)
            || self
                .inner
                .persister_spawned
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return;
        }
        let state = self.clone();
        std::thread::spawn(move || {
            loop {
                {
                    let mut dirty = state
                        .inner
                        .persist_dirty
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    while !*dirty {
                        dirty = state
                            .inner
                            .persist_wake
                            .wait(dirty)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                    }
                    *dirty = false;
                }
                // Coalescing window: let the rest of the burst (status hooks,
                // transcript appends, a resize storm) land before snapshotting.
                std::thread::sleep(PERSIST_DEBOUNCE);
                // Absorb marks made during the window — the snapshot below will
                // include them, so they must not schedule another write.
                {
                    let mut dirty = state
                        .inner
                        .persist_dirty
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    *dirty = false;
                }
                state.persist_now();
            }
        });
    }

    /// Snapshots the model to disk when persistence is enabled.
    fn persist_now(&self) {
        // Hold the persist lock across snapshot + write + rename so concurrent
        // persists commit in snapshot order. The snapshot must be taken *inside*
        // the lock: otherwise two threads could snapshot as S1,S2 but acquire the
        // lock as 2,1 and rename S2 then S1. Recover from poisoning — a persist
        // that panicked mid-write left the on-disk file intact (temp-then-rename),
        // so the guard's data (nothing) is still fine to reuse.
        let _persist_guard = self
            .inner
            .persist_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // Check the enabled flag *under the lock*, not before it. A persist that read
        // the flag before locking could pass the check, block on the lock while
        // `finalize_persistence_for_exit` writes the final snapshot and clears the
        // flag, then wake and snapshot a model that `kill_all_panes` has since
        // stripped — overwriting the final state with the tabs deleted. Reading the
        // flag here means such a persist observes the cleared flag and bails.
        if !self.inner.persist_enabled.load(Ordering::Relaxed) {
            return;
        }

        if let Err(err) = self.persist_snapshot_locked() {
            eprintln!("qmux: failed to persist session state: {err}");
        }
    }

    /// Snapshots the model and writes it to disk. Assumes the caller holds
    /// `persist_lock`; does not consult `persist_enabled`. Shared by `persist` (which
    /// gates on the flag) and `finalize_persistence_for_exit` (which writes the final
    /// snapshot before clearing the flag, both under the lock).
    fn persist_snapshot_locked(&self) -> Result<(), String> {
        let snapshot = {
            let model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            PersistedState {
                // The lowest version whose readers understand every node kind
                // present, so sessions without conversation nodes stay
                // loadable by pre-conversations builds — same tiering as
                // research::detached_archive_version.
                version: if model
                    .research_nodes
                    .values()
                    .any(|node| node.kind == ResearchNodeKind::Conversation)
                {
                    STATE_VERSION
                } else {
                    persistence::STATE_VERSION_PRE_CONVERSATIONS
                },
                next_id: self.inner.next_id.load(Ordering::Relaxed),
                panes: ordered_panes(&model),
                groups: model.groups.values().cloned().collect(),
                group_order: ordered_group_ids(&model),
                agents: model.agents.values().cloned().collect(),
                queues: model
                    .agent_turn_queues
                    .iter()
                    .map(|(agent_id, queue)| (agent_id.clone(), queue.iter().cloned().collect()))
                    .collect(),
                recent_sessions: recent_sessions_sorted(&model),
                drafts: model.agent_drafts.clone(),
                global_drafts: model.global_drafts.clone(),
                inflight: model.agent_inflight.clone(),
                pane_splits: normalized_pane_splits(&model, model.pane_splits.clone(), false)
                    .unwrap_or_default(),
                active_tab_id: model.active_tab_id.clone(),
                threads: model.threads.clone(),
                thread_focus: model.thread_focus.clone(),
                research_trees: model.research_trees.clone(),
                research_tree_order: ordered_research_tree_ids(&model),
                research_nodes: model.research_nodes.clone(),
                research_folders: model.research_folders.clone(),
                encyclopedia: model.encyclopedia.clone(),
            }
        };
        persistence::save(&self.inner.config.workspace_root, &snapshot)
    }

    /// Called once when the process is really exiting, before exit-time pane
    /// teardown. Commits a final snapshot, then disables persistence for good:
    /// `kill_all_panes` is about to take down every pane's PTY, and each reader thread
    /// reacts to that EOF with the natural-exit `remove_pane` path. Left enabled, those
    /// removals race the dying process and rewrite state.json with the panes stripped
    /// out — quitting would erase the very tabs a relaunch should restore.
    ///
    /// The final snapshot and the flag clear happen together under `persist_lock`, so
    /// any other persist either ran fully before this (its snapshot superseded here) or
    /// blocks on the lock and, on waking, sees the cleared flag and bails — it can
    /// never commit a post-`kill_all_panes` snapshot over this one.
    pub fn finalize_persistence_for_exit(&self) {
        // Publish this before snapshotting. Once exit begins, a concurrent natural
        // EOF may remove a pane from the in-memory model at any point; preserving an
        // extra journal is harmless, while deleting the journal for a pane that made
        // it into the frozen snapshot would permanently lose its restored history.
        self.inner
            .exit_teardown_started
            .store(true, Ordering::SeqCst);
        let _persist_guard = self
            .inner
            .persist_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(err) = self.persist_snapshot_locked() {
            eprintln!("qmux: failed to persist final session state: {err}");
        }
        self.inner.persist_enabled.store(false, Ordering::Relaxed);
        // Thread-graph writes are debounced the same way state.json is; commit
        // anything still buffered so a clean quit never loses graph updates.
        thread_graph::flush_dirty_thread_graphs();
    }

    /// Returns the control-socket token scoped to a single pane, minting one on first
    /// use. Each pane gets its own unguessable token so a process running in one pane
    /// cannot drive another pane (or the control plane) through the socket.
    pub fn pane_token(&self, pane_id: &str) -> Result<String, String> {
        let mut tokens = self
            .inner
            .pane_tokens
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if let Some(existing) = tokens.get(pane_id) {
            return Ok(existing.clone());
        }
        // Mint outside the entry API so a CSPRNG failure returns an error to this one
        // call rather than panicking inside or_insert_with and aborting the whole
        // app (killing every running agent and unsaved draft).
        let token = random_token()?;
        Ok(tokens.entry(pane_id.to_string()).or_insert(token).clone())
    }

    /// Resolves the pane a presented control token is authorized for, if any.
    pub fn pane_for_token(&self, token: &str) -> Option<String> {
        let tokens = self
            .inner
            .pane_tokens
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        tokens
            .iter()
            .find_map(|(pane_id, pane_token)| (pane_token == token).then(|| pane_id.clone()))
    }

    pub fn attach_app(&self, app_handle: AppHandle) -> Result<(), String> {
        let mut handle = self
            .inner
            .app_handle
            .lock()
            .map_err(|_| "app handle lock poisoned".to_string())?;
        *handle = Some(app_handle);
        Ok(())
    }

    pub fn next_id(&self, prefix: &str) -> String {
        let seq = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or_default();
        format!("{prefix}-{millis}-{seq}")
    }

    pub fn emit(&self, event: QmuxEvent) {
        // Clone the handle under the lock but emit outside it. emit() serializes
        // the payload (turn.updated events carry whole turn arrays) and enqueues
        // the IPC; holding the mutex across that serialized every event in the
        // process behind one lock — including main-thread native-input callbacks
        // contending with transcript tails mid-serialize.
        let app_handle = self
            .inner
            .app_handle
            .lock()
            .ok()
            .and_then(|handle| handle.as_ref().cloned());
        if let Some(app_handle) = app_handle {
            // Time the emit itself: it serializes the payload and enqueues the
            // IPC, so a slow call here means the payload was huge or the webview
            // side is backed up — both prime freeze suspects. The same call
            // feeds the event-rate flood detector (see diagnostics.rs).
            let event_type = event.event_type.clone();
            let started = Instant::now();
            let _ = app_handle.emit("qmux-event", event);
            self.inner
                .diagnostics
                .note_emit(&event_type, started.elapsed());
        }
    }

    pub fn diagnostics(&self) -> &Diagnostics {
        &self.inner.diagnostics
    }

    pub fn mark_exit_confirmed(&self) {
        self.inner.exit_confirmed.store(true, Ordering::Relaxed);
    }

    pub fn should_confirm_exit(&self) -> bool {
        if self.inner.exit_confirmed.load(Ordering::Relaxed) {
            return false;
        }
        self.open_pane_count() > 0
    }

    pub fn request_exit_confirmation(&self) {
        let pane_count = self.open_pane_count();
        if pane_count == 0 {
            return;
        }
        self.emit(QmuxEvent::new(
            "app.exit_confirmation_requested",
            None,
            None,
            json!({ "paneCount": pane_count }),
        ));
    }

    fn open_pane_count(&self) -> usize {
        self.inner
            .model
            .lock()
            .map(|model| {
                model
                    .panes
                    .values()
                    .filter(|pane| {
                        matches!(pane.info.status, PaneStatus::Starting | PaneStatus::Running)
                    })
                    .count()
            })
            .unwrap_or_default()
    }

    pub fn list_panes(&self) -> Result<Vec<PaneInfo>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(ordered_panes(&model))
    }

    pub fn active_tab_id(&self) -> Result<Option<String>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model.active_tab_id.clone())
    }

    pub fn set_active_tab_id(&self, tab_id: Option<String>) -> Result<(), String> {
        let tab_id = sanitize_active_tab_id(tab_id);
        let changed = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if model.active_tab_id == tab_id {
                false
            } else {
                model.active_tab_id = tab_id;
                true
            }
        };
        if changed {
            self.persist();
        }
        Ok(())
    }

    pub fn pane_splits(&self) -> Result<Vec<PaneSplitInfo>, String> {
        let mut model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        normalize_pane_splits_locked(&mut model);
        Ok(model.pane_splits.clone())
    }

    pub fn set_pane_splits(
        &self,
        splits: Vec<PaneSplitInfo>,
    ) -> Result<Vec<PaneSplitInfo>, String> {
        let normalized = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            model.pane_splits = normalized_pane_splits(&model, splits, true)?;
            model.pane_splits.clone()
        };
        self.persist();
        Ok(normalized)
    }

    pub fn list_groups(&self) -> Result<Vec<GroupInfo>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(ordered_groups(&model))
    }

    pub fn reorder_groups(&self, group_ids: Vec<String>) -> Result<Vec<GroupInfo>, String> {
        let groups = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if group_ids.len() != model.groups.len() {
                return Err("group order is stale; refresh before reordering".to_string());
            }

            let mut seen = HashSet::with_capacity(group_ids.len());
            for group_id in &group_ids {
                if !seen.insert(group_id.clone()) {
                    return Err("group order contains a duplicate group".to_string());
                }
                if !model.groups.contains_key(group_id) {
                    return Err(format!("group {group_id} was not found"));
                }
            }

            model.group_order = group_ids;
            ordered_groups(&model)
        };
        self.persist();
        Ok(groups)
    }

    pub fn list_research_workspaces(&self) -> Result<Vec<GroupInfo>, String> {
        Ok(self
            .list_groups()?
            .into_iter()
            .filter(|group| group.scope == WorkspaceScope::Research)
            .collect())
    }

    pub fn list_agents(&self) -> Result<Vec<AgentInfo>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model.agents.values().cloned().collect())
    }

    pub fn list_recent_sessions(&self, limit: usize) -> Result<Vec<RecentSessionInfo>, String> {
        let mut sessions = {
            let model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            recent_sessions_sorted(&model)
                .into_iter()
                .map(|session| enrich_recent_session_locked(&model, session))
                .take(limit.min(MAX_RECENT_SESSIONS))
                .collect::<Vec<_>>()
        };

        for session in &mut sessions {
            session.missing = recent_session_missing(session);
        }
        Ok(sessions)
    }

    pub fn recent_session(&self, session_id: &str) -> Result<Option<RecentSessionInfo>, String> {
        let session = {
            let model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            model
                .recent_sessions
                .get(session_id)
                .cloned()
                .map(|session| enrich_recent_session_locked(&model, session))
        };
        Ok(session.map(|mut session| {
            session.missing = recent_session_missing(&session);
            session
        }))
    }

    /// Every session in `agent_id`'s fork lineage — the root it descends from and
    /// every fork of that root — root first, then most recently active first.
    ///
    /// Returns empty (rather than a one-entry list) when the agent has no session
    /// id yet, so the caller can hide the branch menu instead of showing a lineage
    /// of one that will change identity the moment SessionStart lands.
    pub fn list_agent_branches(&self, agent_id: &str) -> Result<Vec<BranchInfo>, String> {
        let (root, mut sessions, unrecorded) = {
            let model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let agent = model
                .agents
                .get(agent_id)
                .ok_or_else(|| format!("unknown agent {agent_id}"))?;
            let Some(root) = lineage_root_session_id(agent) else {
                return Ok(Vec::new());
            };
            let sessions = recent_sessions_sorted(&model)
                .into_iter()
                .filter(|session| recent_session_in_lineage(session, &root))
                .map(|session| enrich_recent_session_locked(&model, session))
                .collect::<Vec<_>>();
            let recorded = sessions
                .iter()
                .filter_map(|session| session.agent_id.clone())
                .collect::<HashSet<_>>();
            // A fork is live from spawn but only gets a recent-session row once its
            // SessionStart hook records a session id. Without this pass the fork
            // would be missing from its own branch menu until its first turn landed.
            let unrecorded = model
                .agents
                .values()
                .filter(|agent| !recorded.contains(&agent.id) && agent_in_lineage(agent, &root))
                .cloned()
                .collect::<Vec<_>>();
            (root, sessions, unrecorded)
        };

        // Off the model lock: this stats the worktree and transcript per branch.
        for session in &mut sessions {
            session.missing = recent_session_missing(session);
        }

        let mut branches = sessions
            .into_iter()
            .map(|session| branch_from_recent_session(session, &root))
            .chain(
                unrecorded
                    .iter()
                    .map(|agent| branch_from_agent(agent, &root)),
            )
            .collect::<Vec<_>>();
        branches.sort_by(|left, right| {
            right
                .is_root
                .cmp(&left.is_root)
                .then(right.last_active_at.cmp(&left.last_active_at))
                .then(right.created_at.cmp(&left.created_at))
        });
        Ok(branches)
    }

    /// Removes and returns the agent-session resume queued for `pane_id` at restore, if
    /// any. One-shot: consumed by the pane's respawn so a later relaunch of the same
    /// pane id never re-triggers it.
    pub fn take_shell_agent_resume(&self, pane_id: &str) -> Option<ShellAgentResume> {
        let mut model = self.inner.model.lock().ok()?;
        model.shell_agent_resumes.remove(pane_id)
    }

    pub fn list_turns(&self, agent_id: Option<&str>) -> Result<Vec<Turn>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        if let Some(agent_id) = agent_id {
            Ok(model.turns.get(agent_id).cloned().unwrap_or_default())
        } else {
            Ok(model
                .turns
                .values()
                .flat_map(|turns| turns.iter().cloned())
                .collect())
        }
    }

    pub fn list_thread_graphs(&self) -> Result<Vec<thread_graph::ThreadGraph>, String> {
        let records = {
            let model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            model.threads.values().cloned().collect::<Vec<_>>()
        };

        let mut graphs = Vec::new();
        for record in records {
            let store = thread_graph::ThreadStore::new(record.storage_root);
            if let Some(graph) = store.read_thread(&record.id)? {
                graphs.push(graph);
            }
        }
        Ok(graphs)
    }

    /// Reads a single thread's graph, so streaming turn activity can refresh just
    /// the affected thread instead of re-reading (and re-serializing) every graph
    /// in the workspace. Returns `None` for an unknown thread or one whose graph
    /// snapshot doesn't exist yet.
    pub fn thread_graph(
        &self,
        thread_id: &str,
    ) -> Result<Option<thread_graph::ThreadGraph>, String> {
        let record = {
            let model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            model.threads.get(thread_id).cloned()
        };
        let Some(record) = record else {
            return Ok(None);
        };
        let store = thread_graph::ThreadStore::new(record.storage_root);
        store.read_thread(&record.id)
    }

    pub fn list_research_trees(&self) -> Result<Vec<ResearchTreeSummary>, String> {
        self.list_research_trees_with_archived(false)
    }

    pub fn list_research_trees_with_archived(
        &self,
        include_archived: bool,
    ) -> Result<Vec<ResearchTreeSummary>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        let mut summaries = model
            .research_trees
            .values()
            .filter(|tree| include_archived || tree.archived_at.is_none())
            .map(|tree| {
                let nodes = model
                    .research_nodes
                    .values()
                    .filter(|node| node.tree_id == tree.id);
                fn merge(left: Option<u128>, right: Option<u128>) -> Option<u128> {
                    match (left, right) {
                        (Some(left), Some(right)) => Some(left.max(right)),
                        (None, right) => right,
                        (left, None) => left,
                    }
                }
                let (
                    running_count,
                    failed_count,
                    completed_count,
                    cancelled_count,
                    latest_settlement,
                    latest_failure,
                ) = nodes.fold((0, 0, 0, 0, None::<u128>, None::<u128>), |counts, node| {
                    let failed = node.status == ResearchNodeStatus::Failed;
                    (
                        counts.0 + usize::from(node.status.is_active()),
                        counts.1 + usize::from(failed),
                        counts.2 + usize::from(node.status == ResearchNodeStatus::Complete),
                        counts.3 + usize::from(node.status == ResearchNodeStatus::Cancelled),
                        merge(counts.4, node.completed_at),
                        if failed {
                            merge(counts.5, node.completed_at)
                        } else {
                            counts.5
                        },
                    )
                });
                let unseen = |settled_at: Option<u128>| {
                    settled_at.is_some_and(|settled_at| {
                        tree.last_viewed_at
                            .is_none_or(|last_viewed_at| settled_at > last_viewed_at)
                    })
                };
                ResearchTreeSummary {
                    id: tree.id.clone(),
                    title: tree.title.clone(),
                    root_node_id: tree.root_node_id.clone(),
                    kind: model
                        .research_nodes
                        .get(&tree.root_node_id)
                        .map(|root| root.kind)
                        .unwrap_or_default(),
                    workspace_id: tree.workspace_id.clone(),
                    running_count,
                    failed_count,
                    completed_count,
                    cancelled_count,
                    updated_at: tree.updated_at,
                    archived_at: tree.archived_at,
                    has_unseen_update: unseen(latest_settlement),
                    // Viewing the tree acknowledges the failure; the lifetime
                    // failed_count stays for detail displays but must not brand
                    // the sidebar forever.
                    has_unseen_failure: unseen(latest_failure),
                }
            })
            .collect::<Vec<_>>();
        let order = ordered_research_tree_ids(&model)
            .into_iter()
            .enumerate()
            .map(|(index, tree_id)| (tree_id, index))
            .collect::<HashMap<_, _>>();
        summaries.sort_by_key(|summary| order.get(&summary.id).copied().unwrap_or(usize::MAX));
        Ok(summaries)
    }

    /// Reorders exactly one visible Research sidebar section. Replacing only
    /// that folder/status subsequence leaves hidden archived trees and other
    /// folders at their existing positions in the master order.
    pub fn reorder_research_trees(
        &self,
        workspace_id: &str,
        archived: bool,
        tree_ids: Vec<String>,
    ) -> Result<(), String> {
        {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let expected = ordered_research_tree_ids(&model)
                .into_iter()
                .filter(|tree_id| {
                    model.research_trees.get(tree_id).is_some_and(|tree| {
                        tree.workspace_id == workspace_id && tree.archived_at.is_some() == archived
                    })
                })
                .collect::<Vec<_>>();
            if tree_ids.len() != expected.len() {
                return Err("research tree order is stale; refresh before reordering".to_string());
            }
            let expected_ids = expected.iter().cloned().collect::<HashSet<_>>();
            let mut seen = HashSet::with_capacity(tree_ids.len());
            for tree_id in &tree_ids {
                if !seen.insert(tree_id.clone()) {
                    return Err("research tree order contains a duplicate tree".to_string());
                }
                if !expected_ids.contains(tree_id) {
                    return Err(format!(
                        "research tree {tree_id} is not in the requested sidebar section"
                    ));
                }
            }
            if tree_ids == expected {
                return Ok(());
            }

            let mut replacements = tree_ids.into_iter();
            let mut next_order = ordered_research_tree_ids(&model);
            for tree_id in &mut next_order {
                let replace = model.research_trees.get(tree_id).is_some_and(|tree| {
                    tree.workspace_id == workspace_id && tree.archived_at.is_some() == archived
                });
                if replace {
                    *tree_id = replacements.next().expect("validated replacement count");
                }
            }
            model.research_tree_order = next_order;
        }
        self.persist();
        Ok(())
    }

    pub fn research_folders(&self) -> Result<research::ResearchFolderState, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model.research_folders.clone())
    }

    /// Replaces the stored grouping with a client-supplied one. Structural
    /// normalization only (dedupe, drop membership/collapsed that point at
    /// folders not in the payload) — it deliberately does NOT prune by tree
    /// existence. Tree-existence reconciliation belongs at load and at actual
    /// tree removal, under the authoritative tree set; pruning here against a
    /// caller that momentarily sees fewer trees is exactly the loss this work
    /// removes. Returns the normalized state the frontend should adopt.
    pub fn set_research_folders(
        &self,
        mut folders: research::ResearchFolderState,
    ) -> Result<research::ResearchFolderState, String> {
        research::normalize_research_folder_state(&mut folders);
        {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if model.research_folders == folders {
                return Ok(folders);
            }
            model.research_folders = folders.clone();
        }
        self.persist();
        Ok(folders)
    }

    /// Applies a settings change and announces it. Only `enabled` and
    /// `auto_update` are caller-writable; the cursor and run record belong to
    /// the update lifecycle.
    pub fn set_encyclopedia_settings(
        &self,
        workspace_id: &str,
        enabled: bool,
        auto_update: bool,
    ) -> Result<encyclopedia::WorkspaceEncyclopedia, String> {
        let record = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if !model.groups.contains_key(workspace_id) {
                return Err(format!("workspace {workspace_id} was not found"));
            }
            let record = model
                .encyclopedia
                .workspaces
                .entry(workspace_id.to_string())
                .or_default();
            if record.enabled == enabled && record.auto_update == auto_update {
                return Ok(record.clone());
            }
            record.enabled = enabled;
            record.auto_update = auto_update;
            record.clone()
        };
        self.persist();
        self.emit_encyclopedia_changed(workspace_id, &record);
        Ok(record)
    }

    /// Records a just-launched encyclopedia update run. Refuses while another
    /// run is still recorded — reap it first — so a workspace never has two
    /// generation agents racing over the same files.
    pub fn begin_encyclopedia_run(
        &self,
        workspace_id: &str,
        run: encyclopedia::EncyclopediaRun,
    ) -> Result<encyclopedia::WorkspaceEncyclopedia, String> {
        let record = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if !model.groups.contains_key(workspace_id) {
                return Err(format!("workspace {workspace_id} was not found"));
            }
            let record = model
                .encyclopedia
                .workspaces
                .entry(workspace_id.to_string())
                .or_default();
            if record.active_run.is_some() {
                return Err("an encyclopedia update is already running".to_string());
            }
            record.active_run = Some(run);
            record.last_error = None;
            record.clone()
        };
        self.persist();
        self.emit_encyclopedia_changed(workspace_id, &record);
        Ok(record)
    }

    /// Settles the workspace's update run if its agent has finished (or
    /// vanished): advances the cursor to the run's cutoff, clears the run,
    /// and reclaims the run's pane. Safe to call at any time — a still-active
    /// run is left untouched. Returns the current record either way.
    pub fn reap_encyclopedia_run(
        &self,
        workspace_id: &str,
    ) -> Result<encyclopedia::WorkspaceEncyclopedia, String> {
        let (record, retired_pane) = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if !model.groups.contains_key(workspace_id) {
                return Err(format!("workspace {workspace_id} was not found"));
            }
            let Some(run) = model
                .encyclopedia
                .workspaces
                .get(workspace_id)
                .and_then(|record| record.active_run.clone())
            else {
                return Ok(model
                    .encyclopedia
                    .workspaces
                    .get(workspace_id)
                    .cloned()
                    .unwrap_or_default());
            };
            let agent_status = model.agents.get(&run.agent_id).map(|agent| agent.status);
            let finished = match agent_status {
                // Agent record gone: the pane closed (user or crash); the run
                // is over regardless of how it ended.
                None => true,
                Some(status) => matches!(
                    status,
                    AgentStatus::Done | AgentStatus::Idle | AgentStatus::Failed
                ),
            };
            let record = model
                .encyclopedia
                .workspaces
                .get_mut(workspace_id)
                .expect("record existed above under the same lock");
            if !finished {
                return Ok(record.clone());
            }
            let failed = matches!(agent_status, Some(AgentStatus::Failed));
            if failed {
                // Keep the cursor where it was so the material this run never
                // digested is offered again to the next update.
                record.last_error = Some("the encyclopedia update agent failed".to_string());
            } else {
                record.last_generated_at = record.last_generated_at.max(run.cutoff);
            }
            record.active_run = None;
            let record = record.clone();
            let retired_pane = model
                .panes
                .contains_key(&run.pane_id)
                .then_some(run.pane_id);
            (record, retired_pane)
        };
        // Reclaim the run's pane outside the model lock. Automated
        // retirement is not a user close and must not be undoable.
        if let Some(pane_id) = retired_pane {
            match crate::pty::kill_pane(self, pane_id.clone()) {
                Ok(()) => self.clear_last_closed_pane_for_pane(&pane_id),
                Err(err) => {
                    if self.pane_exists(&pane_id).unwrap_or(false) {
                        eprintln!("qmux: failed to retire encyclopedia pane {pane_id}: {err}");
                    }
                }
            }
        }
        self.persist();
        self.emit_encyclopedia_changed(workspace_id, &record);
        Ok(record)
    }

    /// Cancels an in-flight update run without advancing the cursor, so the
    /// undigested material is offered again next time.
    pub fn cancel_encyclopedia_run(
        &self,
        workspace_id: &str,
    ) -> Result<encyclopedia::WorkspaceEncyclopedia, String> {
        let (record, cancelled_pane) = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if !model.groups.contains_key(workspace_id) {
                return Err(format!("workspace {workspace_id} was not found"));
            }
            let Some(record) = model.encyclopedia.workspaces.get_mut(workspace_id) else {
                return Ok(encyclopedia::WorkspaceEncyclopedia::default());
            };
            let Some(run) = record.active_run.take() else {
                return Ok(record.clone());
            };
            let record = record.clone();
            let cancelled_pane = model
                .panes
                .contains_key(&run.pane_id)
                .then_some(run.pane_id);
            (record, cancelled_pane)
        };
        if let Some(pane_id) = cancelled_pane {
            match crate::pty::kill_pane(self, pane_id.clone()) {
                Ok(()) => self.clear_last_closed_pane_for_pane(&pane_id),
                Err(err) => {
                    if self.pane_exists(&pane_id).unwrap_or(false) {
                        eprintln!("qmux: failed to cancel encyclopedia pane {pane_id}: {err}");
                    }
                }
            }
        }
        self.persist();
        self.emit_encyclopedia_changed(workspace_id, &record);
        Ok(record)
    }

    /// Records a launch failure so the sidebar can surface it.
    pub fn set_encyclopedia_error(&self, workspace_id: &str, error: String) {
        let record = {
            let Ok(mut model) = self.inner.model.lock() else {
                return;
            };
            let Some(record) = model.encyclopedia.workspaces.get_mut(workspace_id) else {
                return;
            };
            record.last_error = Some(error);
            record.clone()
        };
        self.persist();
        self.emit_encyclopedia_changed(workspace_id, &record);
    }

    /// Completed research material in this workspace newer than `since`,
    /// oldest first: the metadata half of an update digest (snapshot bodies
    /// are read outside the model lock). Also serves as the pending-source
    /// count for status. Newest `MAX_ENCYCLOPEDIA_SOURCES` are kept.
    pub fn encyclopedia_source_nodes(
        &self,
        workspace_id: &str,
        since: u128,
    ) -> Result<Vec<(ResearchNode, String)>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        if !model.groups.contains_key(workspace_id) {
            return Err(format!("workspace {workspace_id} was not found"));
        }
        let mut sources = model
            .research_nodes
            .values()
            .filter(|node| node.status == ResearchNodeStatus::Complete)
            .filter(|node| encyclopedia::node_source_time(node) > since)
            .filter_map(|node| {
                let tree = model.research_trees.get(&node.tree_id)?;
                (tree.workspace_id == workspace_id).then(|| (node.clone(), tree.title.clone()))
            })
            .collect::<Vec<_>>();
        sources.sort_by_key(|(node, _)| (encyclopedia::node_source_time(node), node.id.clone()));
        if sources.len() > encyclopedia::MAX_ENCYCLOPEDIA_SOURCES {
            let drop = sources.len() - encyclopedia::MAX_ENCYCLOPEDIA_SOURCES;
            sources.drain(..drop);
        }
        Ok(sources)
    }

    fn emit_encyclopedia_changed(
        &self,
        workspace_id: &str,
        record: &encyclopedia::WorkspaceEncyclopedia,
    ) {
        self.emit(QmuxEvent::new(
            "research.encyclopedia.changed",
            None,
            None,
            serde_json::json!({
                "workspaceId": workspace_id,
                "enabled": record.enabled,
                "autoUpdate": record.auto_update,
                "updating": record.active_run.is_some(),
            }),
        ));
    }

    pub fn list_research_activity(&self) -> Result<Vec<ResearchNode>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        let mut nodes = model
            .research_nodes
            .values()
            // Queued launch-in-flight nodes are active even before a pane is
            // bound. Keep them visible to exit cancellation and activity UI.
            .filter(|node| node.pane_id.is_some() || node.status.is_active())
            .cloned()
            .collect::<Vec<_>>();
        nodes.sort_by_key(|node| (node.created_at, node.id.clone()));
        Ok(nodes)
    }

    pub fn research_tree(&self, tree_id: &str) -> Result<ResearchTreeDetail, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        let tree = model
            .research_trees
            .get(tree_id)
            .cloned()
            .ok_or_else(|| format!("research tree {tree_id} was not found"))?;
        let mut nodes = model
            .research_nodes
            .values()
            .filter(|node| node.tree_id == tree_id)
            .cloned()
            .collect::<Vec<_>>();
        nodes.sort_by_key(|node| (node.created_at, node.id.clone()));
        Ok(ResearchTreeDetail { tree, nodes })
    }

    pub fn research_node(&self, node_id: &str) -> Result<ResearchNode, String> {
        self.inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?
            .research_nodes
            .get(node_id)
            .cloned()
            .ok_or_else(|| format!("research node {node_id} was not found"))
    }

    /// Prompts of the node's ancestor chain, nearest parent first, for
    /// response-boundary matching against replayed forked history.
    pub fn research_node_ancestor_prompts(&self, node_id: &str) -> Result<Vec<String>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        let node = model
            .research_nodes
            .get(node_id)
            .ok_or_else(|| format!("research node {node_id} was not found"))?;
        Ok(research::ancestor_prompts(node, |id| {
            model.research_nodes.get(id)
        }))
    }

    pub fn research_node_content(&self, node_id: &str) -> Result<ResearchNodeContent, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        let node = model
            .research_nodes
            .get(node_id)
            .cloned()
            .ok_or_else(|| format!("research node {node_id} was not found"))?;
        let ancestor_prompts = research::ancestor_prompts(&node, |id| model.research_nodes.get(id));
        let turns = node
            .agent_id
            .as_deref()
            .and_then(|agent_id| model.turns.get(agent_id))
            .map(|turns| {
                research::response_turns(
                    turns,
                    node.prompt_native_id.as_deref(),
                    &node.prompt,
                    &ancestor_prompts,
                )
            })
            .unwrap_or_default();
        let mut children = model
            .research_nodes
            .values()
            .filter(|child| child.parent_node_id.as_deref() == Some(node_id))
            .map(|child| ResearchNodeCard {
                id: child.id.clone(),
                prompt: child.prompt.clone(),
                response_preview: child.response_preview.clone(),
                status: child.status,
                created_at: child.created_at,
            })
            .collect::<Vec<_>>();
        children.sort_by_key(|child| (child.created_at, child.id.clone()));
        Ok(ResearchNodeContent {
            node,
            turns,
            children,
            source_error: None,
            response_revision: None,
        })
    }

    /// A caller-provided title, trimmed, or the fallback when absent or
    /// blank. One owner for the idiom every research creator shares.
    fn resolved_research_title(
        provided: Option<String>,
        fallback: impl FnOnce() -> String,
    ) -> String {
        provided
            .map(|title| title.trim().to_string())
            .filter(|title| !title.is_empty())
            .unwrap_or_else(fallback)
    }

    /// Admits a new root research tree under the model lock: resolves the run
    /// directory from the durable workspace record (never from the caller),
    /// requires Research scope, and inserts the tree at the top of the
    /// sidebar order. Shared by every root creator — runs, documents, and
    /// conversation exports — so admission semantics cannot drift between
    /// them. Persisting and eventing stay with the caller, which may have a
    /// snapshot to reclaim on failure first.
    fn admit_research_root(
        &self,
        tree: &ResearchTree,
        node: &mut ResearchNode,
    ) -> Result<(), String> {
        let mut model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        let workspace = model
            .groups
            .get(&node.group_id)
            .ok_or_else(|| format!("research workspace {} was not found", node.group_id))?;
        if workspace.scope != WorkspaceScope::Research {
            return Err("research requires a Research-scoped workspace".to_string());
        }
        node.worktree_dir = workspace.dir.clone();
        model.research_tree_order.retain(|id| id != &tree.id);
        model.research_tree_order.insert(0, tree.id.clone());
        model.research_trees.insert(tree.id.clone(), tree.clone());
        model.research_nodes.insert(node.id.clone(), node.clone());
        Ok(())
    }

    pub fn create_research_tree(
        &self,
        request: CreateResearchTreeRequest,
    ) -> Result<ResearchTreeDetail, String> {
        let prompt = request.prompt.trim().to_string();
        if prompt.is_empty() {
            return Err("research prompt cannot be empty".to_string());
        }
        if request.adapter.trim().is_empty() {
            return Err("research adapter cannot be empty".to_string());
        }
        // Branching is the defining research feature; accepting an adapter
        // without a native fork command would only be discovered when the
        // first follow-up fails after a completed root run.
        if !crate::adapters::adapter_supports_fork(&request.adapter) {
            return Err(format!(
                "research requires an adapter with follow-up (fork) support; '{}' has none",
                request.adapter
            ));
        }
        if request.group_id.trim().is_empty() {
            return Err("research workspace cannot be empty".to_string());
        }
        let tree_id = self.next_id("research");
        let node_id = self.next_id("research-node");
        let now = now_millis();
        let title =
            Self::resolved_research_title(request.title, || research::default_title(&prompt));
        let tree = ResearchTree {
            id: tree_id.clone(),
            title,
            root_node_id: node_id.clone(),
            workspace_id: request.group_id.clone(),
            created_at: now,
            updated_at: now,
            archived_at: None,
            last_viewed_at: Some(now),
        };
        let mut node = ResearchNode {
            id: node_id.clone(),
            tree_id: tree_id.clone(),
            parent_node_id: None,
            publication_proposal: None,
            query_anchor: None,
            inline: false,
            prompt,
            title: None,
            response_preview: None,
            adapter: request.adapter,
            model: request.model,
            effort: request.effort,
            group_id: request.group_id,
            worktree_dir: String::new(),
            native_session_id: None,
            transcript_path: None,
            prompt_native_id: None,
            agent_id: None,
            pane_id: None,
            thread_id: None,
            kind: ResearchNodeKind::Run,
            origin: None,
            status: ResearchNodeStatus::Queued,
            error: None,
            response_snapshot_at: None,
            created_at: now,
            started_at: None,
            completed_at: None,
            highlights: Vec::new(),
        };
        self.admit_research_root(&tree, &mut node)?;
        self.persist();
        self.emit(QmuxEvent::new(
            "research.tree.created",
            None,
            None,
            json!({ "tree": tree, "node": node }),
        ));
        self.research_tree(&tree_id)
    }

    /// Creates a document as a single-node research tree: the root node is the
    /// document, its markdown persisted through the same response-snapshot
    /// pipeline as run responses. Nothing launches — the node is born
    /// `Complete` with its snapshot already durable, so viewers, archives, and
    /// pruning treat it exactly like a settled run. The caller must hold the
    /// research workspace-mutation guard, matching `create_research_tree`.
    pub fn create_research_document(
        &self,
        request: CreateResearchDocumentRequest,
    ) -> Result<ResearchTreeDetail, String> {
        let markdown = request.markdown.trim().to_string();
        research::validate_document_markdown(&markdown)?;
        if request.group_id.trim().is_empty() {
            return Err("research workspace cannot be empty".to_string());
        }
        let title = Self::resolved_research_title(request.title, || {
            research::document_default_title(&markdown)
        });
        let tree_id = self.next_id("research");
        let node_id = self.next_id("research-node");
        let now = now_millis();
        let turns = vec![research::document_turn(&node_id, &markdown)];
        // Durable content lands before the records that point at it: a crash
        // here strands only an orphan snapshot, which prune_response_snapshots
        // reclaims. The reverse order would commit a document whose body never
        // existed. The verified write keeps the records from ever pointing at
        // a snapshot that did not round-trip.
        research::write_response_snapshot_verified(
            &self.inner.config.workspace_root,
            &node_id,
            &turns,
        )?;
        let tree = ResearchTree {
            id: tree_id.clone(),
            title,
            root_node_id: node_id.clone(),
            workspace_id: request.group_id.clone(),
            created_at: now,
            updated_at: now,
            archived_at: None,
            last_viewed_at: Some(now),
        };
        let mut node = ResearchNode {
            id: node_id.clone(),
            tree_id: tree_id.clone(),
            parent_node_id: None,
            publication_proposal: None,
            query_anchor: None,
            inline: false,
            prompt: String::new(),
            title: None,
            response_preview: research::response_preview(&turns, None, "", &[]),
            adapter: String::new(),
            model: None,
            effort: None,
            group_id: request.group_id,
            worktree_dir: String::new(),
            native_session_id: None,
            transcript_path: None,
            prompt_native_id: None,
            agent_id: None,
            pane_id: None,
            thread_id: None,
            kind: ResearchNodeKind::Document,
            origin: None,
            status: ResearchNodeStatus::Complete,
            error: None,
            response_snapshot_at: Some(now),
            created_at: now,
            started_at: None,
            completed_at: Some(now),
            highlights: Vec::new(),
        };
        if let Err(err) = self.admit_research_root(&tree, &mut node) {
            // Nothing references the snapshot yet; reclaim it now rather than
            // waiting for the next structural prune.
            let _ = research::remove_response_snapshot(&self.inner.config.workspace_root, &node_id);
            return Err(err);
        }
        self.persist();
        self.emit(QmuxEvent::new(
            "research.tree.created",
            None,
            None,
            json!({ "tree": tree, "node": node }),
        ));
        self.research_tree(&tree_id)
    }

    /// Stage one of exporting a terminal pane's conversation to research:
    /// read and sanitize the source and make the verified snapshot durable,
    /// all without the research workspace-mutation guard — the transcript
    /// read and snapshot write are the slow parts and need no exclusion
    /// against folder mutations. The terminal is left untouched; this is a
    /// copy, not a move, so repeating the export creates another independent
    /// tree. A prepared export whose commit never happens strands only an
    /// orphan snapshot, which prune_response_snapshots reclaims.
    pub fn prepare_pane_export(
        &self,
        pane_id: &str,
    ) -> Result<research::PreparedPaneExport, String> {
        let agent = {
            let model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let pane = model
                .panes
                .get(pane_id)
                .ok_or_else(|| format!("pane {pane_id} was not found"))?;
            // Research runs live in Research-scoped groups, and their hidden
            // panes must not round-trip back into a conversation node. The
            // scope check runs under the same lock as the pane lookup, so it
            // also covers the launch-to-bind window in which a research
            // run's node does not yet name its agent.
            let scope = model
                .groups
                .get(&pane.info.group_id)
                .map(|group| group.scope);
            if scope != Some(WorkspaceScope::Terminal) {
                return Err("only terminal panes can be exported to research".to_string());
            }
            // A pane owns an agent either by launch (pane.info.agent_id, set
            // when the pane spawned as an agent pane) or by adoption (a
            // shell pane whose `claude`/`codex` process was recovered — only
            // agent.pane_id records that binding). The frontend offers the
            // export for both, so both must resolve here.
            let agent = pane
                .info
                .agent_id
                .as_deref()
                .and_then(|agent_id| model.agents.get(agent_id))
                .or_else(|| {
                    model
                        .agents
                        .values()
                        .find(|agent| agent.pane_id.as_deref() == Some(pane_id))
                })
                .cloned()
                .ok_or_else(|| "only agent panes can be exported to research".to_string())?;
            agent
        };
        // Transcript-preferred source: the file is the complete native
        // record, while the in-memory timeline can be a truncated live view.
        // Only a file that has vanished falls back to that view — read
        // failures, including the too-large-to-snapshot guard, surface
        // instead of silently exporting a partial conversation as complete.
        let (mut source_turns, source_stable) = match agent.transcript_path.as_deref() {
            Some(path) if std::path::Path::new(path).exists() => {
                self.transcript_turns_with_stability(&agent, path)?
            }
            _ => {
                let model = self
                    .inner
                    .model
                    .lock()
                    .map_err(|_| "model lock poisoned".to_string())?;
                (
                    model.turns.get(&agent.id).cloned().unwrap_or_default(),
                    true,
                )
            }
        };
        // The busy check runs after the (slow) source read so the status is
        // as fresh as it can be: a stale Running would silently drop a
        // delivered final exchange, a stale settled status would export a
        // half-streamed one.
        let status = {
            let model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            model
                .agents
                .get(&agent.id)
                .map(|agent| agent.status)
                .ok_or_else(|| "the pane closed while the export was running".to_string())?
        };
        if status.is_at_rest() {
            // An at-rest agent's transcript must parse identically twice —
            // the adapter may still be flushing its final records (the same
            // reason snapshot_research_response demands a stable read).
            if !source_stable {
                return Err(
                    "the conversation is still being written; try the export again in a moment"
                        .to_string(),
                );
            }
        } else {
            // A busy agent's in-flight exchange is half-streamed and must not
            // persist as delivered content; instability in the trailing
            // records is cut away with it.
            match research::completed_exchange_boundary(&source_turns) {
                Some(0) => {
                    return Err(
                        "the conversation's only exchange is still in progress; wait for the answer to finish before exporting"
                            .to_string(),
                    );
                }
                Some(boundary) => source_turns.truncate(boundary),
                None => {}
            }
        }
        let tree_id = self.next_id("research");
        let node_id = self.next_id("research-node");
        let turns = research::conversation_export_turns(&node_id, &source_turns)?;
        drop(source_turns);
        let prompt = research::conversation_prompt(&turns);
        let response_preview = research::conversation_preview(&turns);
        // Durable content lands before the records that point at it, same as
        // create_research_document; the verified write keeps the records from
        // ever pointing at a snapshot that did not round-trip.
        research::write_response_snapshot_verified(
            &self.inner.config.workspace_root,
            &node_id,
            &turns,
        )?;
        Ok(research::PreparedPaneExport {
            tree_id,
            node_id,
            prompt,
            response_preview,
            adapter: agent.adapter,
            model: agent.model,
            effort: agent.effort,
            agent_created_at: agent.created_at,
        })
    }

    /// Reads the transcript until two consecutive parses agree, reporting
    /// whether they did. Bounded: a source that keeps changing (a streaming
    /// agent) comes back unstable rather than looping, and the caller
    /// decides — an at-rest agent must retry, a busy one truncates the
    /// unstable tail with the in-flight exchange.
    fn transcript_turns_with_stability(
        &self,
        agent: &AgentInfo,
        path: &str,
    ) -> Result<(Vec<Turn>, bool), String> {
        let mut previous =
            research::load_transcript_turns(&self.inner.config, &agent.adapter, &agent.id, path)?;
        for _ in 0..3 {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let current = research::load_transcript_turns(
                &self.inner.config,
                &agent.adapter,
                &agent.id,
                path,
            )?;
            if current == previous {
                return Ok((current, true));
            }
            previous = current;
        }
        Ok((previous, false))
    }

    /// Stage two: admits the prepared export as a Complete conversation tree.
    /// The caller must hold the research workspace-mutation guard, matching
    /// `create_research_document`. On admission failure the prepared snapshot
    /// is reclaimed.
    pub fn commit_pane_export(
        &self,
        prepared: &research::PreparedPaneExport,
        group_id: String,
        title: Option<String>,
    ) -> Result<ResearchTreeDetail, String> {
        if group_id.trim().is_empty() {
            self.discard_pane_export(prepared);
            return Err("research workspace cannot be empty".to_string());
        }
        let now = now_millis();
        let title =
            Self::resolved_research_title(title, || research::default_title(&prepared.prompt));
        let tree = ResearchTree {
            id: prepared.tree_id.clone(),
            title,
            root_node_id: prepared.node_id.clone(),
            workspace_id: group_id.clone(),
            created_at: now,
            updated_at: now,
            archived_at: None,
            last_viewed_at: Some(now),
        };
        let mut node = ResearchNode {
            id: prepared.node_id.clone(),
            tree_id: prepared.tree_id.clone(),
            parent_node_id: None,
            publication_proposal: None,
            query_anchor: None,
            inline: false,
            prompt: prepared.prompt.clone(),
            title: None,
            response_preview: prepared.response_preview.clone(),
            adapter: prepared.adapter.clone(),
            model: prepared.model.clone(),
            effort: prepared.effort.clone(),
            group_id,
            worktree_dir: String::new(),
            native_session_id: None,
            transcript_path: None,
            prompt_native_id: None,
            agent_id: None,
            pane_id: None,
            thread_id: None,
            kind: ResearchNodeKind::Conversation,
            origin: Some(ResearchNodeOrigin::TerminalExport),
            status: ResearchNodeStatus::Complete,
            error: None,
            response_snapshot_at: Some(now),
            created_at: now,
            started_at: Some(prepared.agent_created_at.min(now)),
            completed_at: Some(now),
            highlights: Vec::new(),
        };
        if let Err(err) = self.admit_research_root(&tree, &mut node) {
            self.discard_pane_export(prepared);
            return Err(err);
        }
        self.persist();
        self.emit(QmuxEvent::new(
            "research.tree.created",
            None,
            None,
            json!({ "tree": tree, "node": node }),
        ));
        self.research_tree(&tree.id)
    }

    /// Reclaims a prepared export whose records never committed (validation
    /// or admission failed after the snapshot landed).
    pub fn discard_pane_export(&self, prepared: &research::PreparedPaneExport) {
        let _ = research::remove_response_snapshot(
            &self.inner.config.workspace_root,
            &prepared.node_id,
        );
    }

    /// Replaces a root document's durable Markdown in place. Existing child
    /// runs are intentionally untouched: their agents already received a copy
    /// of the document in their launch prompt. A body replacement invalidates
    /// every highlight on this node because anchors are revision-bound; a
    /// title-only edit preserves both the snapshot and its highlights.
    pub fn update_research_document(
        &self,
        request: UpdateResearchDocumentRequest,
    ) -> Result<UpdateResearchDocumentResult, String> {
        let _document_guard = self
            .inner
            .research_document_lock
            .lock()
            .map_err(|_| "research document lock poisoned".to_string())?;
        let markdown = request.markdown.trim().to_string();
        research::validate_document_markdown(&markdown)?;

        let (current_node, current_tree) = {
            let model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let node = model
                .research_nodes
                .get(&request.node_id)
                .cloned()
                .ok_or_else(|| format!("research node {} was not found", request.node_id))?;
            let tree = model
                .research_trees
                .get(&node.tree_id)
                .cloned()
                .ok_or_else(|| format!("research tree {} was not found", node.tree_id))?;
            (node, tree)
        };
        if current_node.kind != ResearchNodeKind::Document
            || current_node.parent_node_id.is_some()
            || current_tree.root_node_id != current_node.id
        {
            return Err("only root research documents can be edited".to_string());
        }
        if current_tree.archived_at.is_some() {
            return Err("restore archived research before editing its document".to_string());
        }
        if current_tree.title != request.expected_title {
            return Err(
                "the document title changed while you were editing; reopen the editor and try again"
                    .to_string(),
            );
        }

        let current_snapshot = research::read_response_snapshot_with_revision(
            &self.inner.config.workspace_root,
            &current_node.id,
        )?
        .ok_or_else(|| "the document's content is unavailable".to_string())?;
        if current_snapshot.revision != request.expected_response_revision {
            return Err(
                "the document changed while you were editing; reopen the editor and try again"
                    .to_string(),
            );
        }
        let current_markdown = research::document_markdown_from_turns(&current_snapshot.turns)
            .ok_or_else(|| "the document's content is unavailable".to_string())?;
        let title = request
            .title
            .map(|title| title.trim().to_string())
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| research::document_default_title(&markdown));
        let markdown_changed = current_markdown != markdown;
        if markdown_changed {
            let expected_highlight_ids = request
                .expected_highlight_ids
                .iter()
                .map(String::as_str)
                .collect::<HashSet<_>>();
            let current_highlight_ids = current_node
                .highlights
                .iter()
                .map(|highlight| highlight.id.as_str())
                .collect::<HashSet<_>>();
            if current_highlight_ids != expected_highlight_ids {
                return Err(
                    "the document's highlights changed while you were editing; reopen the editor and try again"
                        .to_string(),
                );
            }
        }
        let (turns, response_revision) = if markdown_changed {
            let turns = vec![research::document_turn(&current_node.id, &markdown)];
            let revision = research::response_revision(&turns)?;
            // The file commit is atomic. Nothing in the model changes if it
            // fails, so the old document, title, and highlights remain valid.
            research::write_response_snapshot(
                &self.inner.config.workspace_root,
                &current_node.id,
                &turns,
            )?;
            (Some(turns), revision)
        } else {
            (None, current_snapshot.revision)
        };

        let now = now_millis();
        let (tree, node, removed_highlight_count) = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let node = model
                .research_nodes
                .get_mut(&current_node.id)
                .ok_or_else(|| format!("research node {} was not found", current_node.id))?;
            let removed = if let Some(turns) = turns.as_deref() {
                let removed = node.highlights.len();
                node.highlights.clear();
                node.response_preview = research::response_preview(turns, None, "", &[]);
                node.response_snapshot_at = Some(
                    node.response_snapshot_at
                        .map_or(now, |previous| now.max(previous.saturating_add(1))),
                );
                removed
            } else {
                0
            };
            let node = node.clone();
            let tree = model
                .research_trees
                .get_mut(&current_tree.id)
                .ok_or_else(|| format!("research tree {} was not found", current_tree.id))?;
            tree.title = title;
            tree.updated_at = now.max(tree.updated_at.saturating_add(1));
            (tree.clone(), node, removed)
        };
        // The response snapshot above is already durable. Persist its matching
        // title, revision timestamp, and cleared-highlight metadata before the
        // command returns instead of leaving a debounce-sized crash window in
        // which state.json still describes the previous document.
        self.persist_now();
        self.emit(QmuxEvent::new(
            "research.document.updated",
            None,
            None,
            json!({
                "tree": tree,
                "node": node,
                "responseRevision": response_revision,
                "markdownChanged": markdown_changed,
                "removedHighlightCount": removed_highlight_count,
            }),
        ));
        Ok(UpdateResearchDocumentResult {
            tree,
            node,
            response_revision,
            markdown_changed,
            removed_highlight_count,
        })
    }

    /// Captures one coherent document version for a new direct follow-up. The
    /// returned launch prompt owns its Markdown string, so releasing the lock
    /// before the agent spawn cannot let a later edit rewrite that child.
    pub fn research_document_followup_prompt(
        &self,
        node_id: &str,
        question: &str,
    ) -> Result<String, String> {
        let _document_guard = self
            .inner
            .research_document_lock
            .lock()
            .map_err(|_| "research document lock poisoned".to_string())?;
        let (node, title) = {
            let model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let node = model
                .research_nodes
                .get(node_id)
                .cloned()
                .ok_or_else(|| format!("research node {node_id} was not found"))?;
            let tree = model
                .research_trees
                .get(&node.tree_id)
                .ok_or_else(|| format!("research tree {} was not found", node.tree_id))?;
            (node, tree.title.clone())
        };
        if node.kind != ResearchNodeKind::Document {
            return Err("the research node is not a document".to_string());
        }
        let turns = research::read_response_snapshot(&self.inner.config.workspace_root, node_id)?
            .ok_or_else(|| "the document's content is unavailable".to_string())?;
        let markdown = research::document_markdown_from_turns(&turns)
            .ok_or_else(|| "the document's content is unavailable".to_string())?;
        research::document_followup_prompt(&title, markdown, question)
    }

    /// The launch prompt for a follow-up on an exported conversation: the
    /// serialized conversation rides along as context, since exports are
    /// severed from their source session and there is nothing to fork.
    /// Conversation snapshots are immutable, so unlike documents no editor
    /// lock is needed to capture a coherent version.
    pub fn research_conversation_followup_prompt(
        &self,
        node_id: &str,
        question: &str,
    ) -> Result<String, String> {
        let (node, title) = {
            let model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let node = model
                .research_nodes
                .get(node_id)
                .cloned()
                .ok_or_else(|| format!("research node {node_id} was not found"))?;
            let tree = model
                .research_trees
                .get(&node.tree_id)
                .ok_or_else(|| format!("research tree {} was not found", node.tree_id))?;
            (node, tree.title.clone())
        };
        if node.kind != ResearchNodeKind::Conversation {
            return Err("the research node is not an exported conversation".to_string());
        }
        let turns = research::read_response_snapshot(&self.inner.config.workspace_root, node_id)?
            .ok_or_else(|| "the conversation's content is unavailable".to_string())?;
        research::conversation_followup_prompt(&title, &turns, question)
    }

    pub fn create_research_child(
        &self,
        parent_node_id: &str,
        prompt: String,
        query_anchor: Option<ResearchHighlightAnchor>,
        inline: bool,
    ) -> Result<ResearchNode, String> {
        if let Some(anchor) = &query_anchor {
            research::validate_highlight_anchor(anchor)?;
        }
        self.create_research_child_with_options(parent_node_id, prompt, None, query_anchor, inline)
    }

    pub fn create_research_child_for_proposal(
        &self,
        parent_node_id: &str,
        prompt: String,
        proposal: ResearchPublicationProposal,
    ) -> Result<ResearchNode, String> {
        if !(8..=128).contains(&proposal.publication_id.len())
            || !proposal
                .publication_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
            || proposal.comment_id == 0
        {
            return Err("publication proposal reference is invalid".to_string());
        }
        // Accepted community proposals are always branches: an inline slot is
        // the owner's conversation to continue, not a contribution target.
        self.create_research_child_with_options(parent_node_id, prompt, Some(proposal), None, false)
    }

    fn create_research_child_with_options(
        &self,
        parent_node_id: &str,
        prompt: String,
        publication_proposal: Option<ResearchPublicationProposal>,
        query_anchor: Option<ResearchHighlightAnchor>,
        inline: bool,
    ) -> Result<ResearchNode, String> {
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() {
            return Err("research prompt cannot be empty".to_string());
        }
        let node_id = self.next_id("research-node");
        let now = now_millis();
        let node = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let parent = model
                .research_nodes
                .get(parent_node_id)
                .cloned()
                .ok_or_else(|| format!("research node {parent_node_id} was not found"))?;
            let tree = model
                .research_trees
                .get(&parent.tree_id)
                .cloned()
                .ok_or_else(|| format!("research tree {} was not found", parent.tree_id))?;
            if tree.archived_at.is_some() {
                return Err("restore archived research before creating a follow-up".to_string());
            }
            if parent.status != ResearchNodeStatus::Complete {
                return Err("research follow-ups require a completed parent response".to_string());
            }
            // One inline follow-up per answer, whatever its status — a failed
            // or cancelled continuation stays visible in the thread until it
            // is deliberately removed, which reopens the slot. Checked inside
            // the model lock so two concurrent submissions cannot both pass.
            if inline
                && model.research_nodes.values().any(|node| {
                    node.parent_node_id.as_deref() == Some(parent_node_id) && node.inline
                })
            {
                return Err("this answer already has an inline follow-up".to_string());
            }
            // A document has no session to fork — its follow-ups launch fresh
            // runs on the default adapter, so only run parents need the
            // checkpoint (and only they carry an adapter to inherit).
            let (adapter, parent_model, parent_effort) = match parent.kind {
                ResearchNodeKind::Document => (
                    crate::adapters::default_fork_adapter(&self.inner.config)?,
                    None,
                    None,
                ),
                // An exported conversation is severed from its session by
                // design, so its follow-ups also launch fresh runs, with
                // the serialized conversation as context. The source
                // terminal's adapter carries over when it can fork —
                // children are run nodes whose own follow-ups branch —
                // else the default fork-capable adapter takes over.
                ResearchNodeKind::Conversation => {
                    // No highlights exist on conversations (no answer-v1
                    // projection), and an anchored quote would re-enter the
                    // launch prompt outside the serializer's tag
                    // neutralization.
                    if query_anchor.is_some() {
                        return Err(
                            "highlight follow-ups are not available on exported conversations"
                                .to_string(),
                        );
                    }
                    if crate::adapters::adapter_supports_fork(&parent.adapter) {
                        (parent.adapter, parent.model, parent.effort)
                    } else {
                        (
                            crate::adapters::default_fork_adapter(&self.inner.config)?,
                            None,
                            None,
                        )
                    }
                }
                ResearchNodeKind::Run => {
                    if parent.native_session_id.is_none() {
                        return Err(
                            "research follow-ups require a recorded parent checkpoint".to_string()
                        );
                    }
                    (parent.adapter, parent.model, parent.effort)
                }
            };
            let workspace = model
                .groups
                .get(&tree.workspace_id)
                .ok_or_else(|| format!("research workspace {} was not found", tree.workspace_id))?;
            if workspace.scope != WorkspaceScope::Research {
                return Err("research requires a Research-scoped workspace".to_string());
            }
            let node = ResearchNode {
                id: node_id.clone(),
                tree_id: parent.tree_id.clone(),
                parent_node_id: Some(parent.id),
                publication_proposal,
                query_anchor,
                inline,
                prompt,
                title: None,
                response_preview: None,
                adapter,
                model: parent_model,
                effort: parent_effort,
                group_id: workspace.id.clone(),
                worktree_dir: workspace.dir.clone(),
                native_session_id: None,
                transcript_path: None,
                prompt_native_id: None,
                agent_id: None,
                pane_id: None,
                thread_id: None,
                kind: ResearchNodeKind::Run,
                origin: None,
                status: ResearchNodeStatus::Queued,
                error: None,
                response_snapshot_at: None,
                created_at: now,
                started_at: None,
                completed_at: None,
                highlights: Vec::new(),
            };
            model.research_nodes.insert(node_id, node.clone());
            touch_research_tree_locked(&mut model, &node.tree_id, now);
            node
        };
        self.persist();
        self.emit(QmuxEvent::new(
            "research.node.created",
            None,
            None,
            json!({ "node": node }),
        ));
        Ok(node)
    }

    pub fn bind_research_node_run(
        &self,
        node_id: &str,
        agent: &AgentInfo,
        pane_id: &str,
    ) -> Result<ResearchNode, String> {
        let node = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let node_snapshot = model
                .research_nodes
                .get(node_id)
                .cloned()
                .ok_or_else(|| format!("research node {node_id} was not found"))?;
            // Documents and exported conversations are snapshot-only by
            // contract — a conversation in particular is severed from its
            // source session, and binding a live agent here would hand the
            // shared read paths live-looking pointers the kind promises it
            // does not have.
            if node_snapshot.kind != ResearchNodeKind::Run {
                return Err(format!(
                    "research node {node_id} is not a run and cannot bind a live agent"
                ));
            }
            let tree = model
                .research_trees
                .get(&node_snapshot.tree_id)
                .ok_or_else(|| format!("research tree {} was not found", node_snapshot.tree_id))?;
            let workspace = model
                .groups
                .get(&tree.workspace_id)
                .ok_or_else(|| format!("research workspace {} was not found", tree.workspace_id))?;
            if workspace.scope != WorkspaceScope::Research || agent.group_id != workspace.id {
                return Err("research launch did not use the tree's current workspace".to_string());
            }
            // An instantly-exiting process (missing binary, adapter arg error)
            // can EOF and run the whole remove_pane teardown before the launch
            // path gets here. That teardown's research detach found nothing
            // bound — this bind hadn't happened — so binding the dead pane id
            // now would create a run nothing ever settles or unbinds: a
            // phantom "active" node that pins its tree (blocking
            // archive/remove and folder changes) until the user cancels it by
            // hand or restarts. Checked under the same model lock remove_pane
            // takes, so either the pane is still present (and its later detach
            // will observe this binding), or it is gone for good and the run
            // must settle here.
            let pane_exists = model.panes.contains_key(pane_id);
            let has_active_subagents = model
                .agent_active_subagents
                .get(&agent.id)
                .is_some_and(|active| !active.is_empty());
            let node = model
                .research_nodes
                .get_mut(node_id)
                .expect("research node was checked above");
            node.agent_id = Some(agent.id.clone());
            // Recorded whether or not the pane survived: the run's agent
            // minted its thread record during launch either way, and tree
            // removal reaps that record through this link.
            node.thread_id = agent.thread_id.clone();
            node.native_session_id = agent.session_id.clone();
            node.transcript_path = agent.transcript_path.clone();
            let now = now_millis();
            node.started_at.get_or_insert(now);
            if pane_exists {
                node.pane_id = Some(pane_id.to_string());
                // Launch and cancellation race: the user can settle a Queued node
                // while its spawn is still in flight. Binding must still record the
                // pane and agent — the caller reclaims them — but a settled outcome
                // is monotonic and the bind must not resurrect the run.
                if !node.status.is_terminal() {
                    node.status = research_status_for_agent(agent.status, has_active_subagents);
                    node.error = None;
                    if node.status.is_terminal() {
                        node.completed_at.get_or_insert(now);
                    }
                }
            } else if node.status.is_active() {
                // Mirror detach_research_pane's settle for the teardown that
                // already ran: the agent snapshot was captured after the spawn,
                // so Done/Idle means the run finished before its pane closed.
                if matches!(agent.status, AgentStatus::Done | AgentStatus::Idle)
                    && !has_active_subagents
                {
                    node.status = ResearchNodeStatus::Complete;
                } else {
                    node.status = ResearchNodeStatus::Failed;
                    node.error = Some("Research process exited before completion".to_string());
                }
                node.completed_at = Some(now);
            }
            let node = node.clone();
            touch_research_tree_locked(&mut model, &node.tree_id, now);
            node
        };
        self.persist();
        self.emit(QmuxEvent::new(
            "research.node.updated",
            Some(pane_id.to_string()),
            Some(agent.id.clone()),
            json!({ "node": node }),
        ));
        self.maybe_schedule_research_retirement(&node);
        Ok(node)
    }

    /// Arms the pre-session watchdog for a just-launched research run. Agent
    /// status is entirely hook-driven, and a CLI blocked on startup UI that
    /// predates its session — a workspace-trust dialog, a login prompt, an
    /// update gate — fires no hooks at all, so the agent would sit `Starting`
    /// forever while the research pane's read-only policy keeps the user from
    /// answering the very prompt it is stuck on. If the agent is still
    /// pre-session after the delay, flag it `AwaitingInput`: the pane's
    /// keyboard unlocks and the node stays live, and the first real hook
    /// moves the status on as usual. Deliberately adapter-agnostic — every
    /// harness wedges the same way here and gets the same recovery.
    pub fn schedule_research_startup_watchdog(&self, agent_id: String) {
        // Long enough that a healthy launch has bound its native session id
        // (SessionStart on a fresh spawn, the first turn's hook payload on a
        // fork); a false flag only unlocks the pane early and heals on the
        // next hook.
        const RESEARCH_STARTUP_WATCHDOG_DELAY_MS: u64 = 10_000;
        let state = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(
                RESEARCH_STARTUP_WATCHDOG_DELAY_MS,
            ));
            match state.flag_stalled_research_startup(&agent_id) {
                Ok(Some(agent)) => {
                    // Mirrors the hook pipeline's event shape (type + attached
                    // agent) so the frontend applies the status surgically.
                    state.emit(QmuxEvent::new(
                        "agent.awaiting_input",
                        agent.pane_id.clone(),
                        Some(agent.id.clone()),
                        json!({ "agent": agent, "source": "research-startup-watchdog" }),
                    ));
                }
                Ok(None) => {}
                Err(err) => {
                    eprintln!("qmux: research startup watchdog for {agent_id} failed: {err}");
                }
            }
        });
    }

    /// The watchdog's check-and-flip. Returns the updated agent when the run
    /// was still pre-session and got flagged `AwaitingInput`; `None` when it
    /// moved on, settled, or lost its pane (exit teardown owns that outcome).
    ///
    /// Pre-session has two observed signatures, so both flag:
    /// - `Starting`: no lifecycle hook has landed at all.
    /// - `Running` with no bound session: Claude fires `UserPromptSubmit`
    ///   for a launch-argument prompt even while startup UI (the
    ///   workspace-trust dialog) still blocks the session, so the status
    ///   promotes while `SessionStart` never delivers a session id. Every
    ///   adapter binds the session id within seconds on a healthy launch
    ///   (forks heal theirs from the first turn's hook payload), so
    ///   session-less `Running` this long after spawn means startup UI is
    ///   blocking — and a rare false flag only unlocks the pane early and
    ///   heals on the next hook.
    ///
    /// The check and the status write share one model lock so a hook racing
    /// this flip cannot have its fresher status stomped back to
    /// `AwaitingInput`.
    pub(crate) fn flag_stalled_research_startup(
        &self,
        agent_id: &str,
    ) -> Result<Option<AgentInfo>, String> {
        let updated = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let node_live = model.research_nodes.values().any(|node| {
                node.agent_id.as_deref() == Some(agent_id) && !node.status.is_terminal()
            });
            if !node_live {
                return Ok(None);
            }
            let stalled = model.agents.get(agent_id).is_some_and(|agent| {
                let presession = match agent.status {
                    AgentStatus::Starting => true,
                    AgentStatus::Running => agent.session_id.is_none(),
                    _ => false,
                };
                presession
                    && agent
                        .pane_id
                        .as_deref()
                        .is_some_and(|pane_id| model.panes.contains_key(pane_id))
            });
            if !stalled {
                return Ok(None);
            }
            let agent = model
                .agents
                .get_mut(agent_id)
                .expect("agent was checked above");
            agent.status = AgentStatus::AwaitingInput;
            let updated = agent.clone();
            bump_agent_activity_locked(&mut model, agent_id);
            bump_agent_status_activity_locked(&mut model, agent_id);
            updated
        };
        // No recent-session upsert on purpose: research sessions are excluded
        // from the recents pool, and this is not real agent activity anyway.
        self.sync_research_node_from_agent(&updated)?;
        self.persist();
        Ok(Some(updated))
    }

    pub fn research_workspace_for_node(&self, node_id: &str) -> Result<GroupInfo, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        let node = model
            .research_nodes
            .get(node_id)
            .ok_or_else(|| format!("research node {node_id} was not found"))?;
        let tree = model
            .research_trees
            .get(&node.tree_id)
            .ok_or_else(|| format!("research tree {} was not found", node.tree_id))?;
        let workspace = model
            .groups
            .get(&tree.workspace_id)
            .ok_or_else(|| format!("research workspace {} was not found", tree.workspace_id))?;
        if workspace.scope != WorkspaceScope::Research {
            return Err("research requires a Research-scoped workspace".to_string());
        }
        validate_research_workspace_available(workspace)?;
        Ok(workspace.clone())
    }

    pub fn fail_research_node(&self, node_id: &str, error: String) -> Result<ResearchNode, String> {
        let node = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let node = model
                .research_nodes
                .get_mut(node_id)
                .ok_or_else(|| format!("research node {node_id} was not found"))?;
            // Failure settles an active run (and may refine the error on one
            // that already failed), but a Complete or Cancelled outcome the
            // user can already see must not be rewritten by a late launch
            // cleanup racing that settlement.
            if node.status.is_terminal() && node.status != ResearchNodeStatus::Failed {
                return Ok(node.clone());
            }
            let now = now_millis();
            node.status = ResearchNodeStatus::Failed;
            node.error = Some(error);
            node.completed_at = Some(now);
            let node = node.clone();
            touch_research_tree_locked(&mut model, &node.tree_id, now);
            node
        };
        self.persist();
        self.emit(QmuxEvent::new(
            "research.node.updated",
            node.pane_id.clone(),
            node.agent_id.clone(),
            json!({ "node": node }),
        ));
        Ok(node)
    }

    /// User-driven cancellation of an active run: settles the node as
    /// `Cancelled` and reclaims its pane. Also reclaims a still-bound pane on
    /// an already-settled node (a kill that failed on a previous cancel), so
    /// a stuck binding cannot pin the tree forever.
    pub fn cancel_research_node(&self, node_id: &str) -> Result<ResearchNode, String> {
        let (node, pane_id) = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let node = model
                .research_nodes
                .get_mut(node_id)
                .ok_or_else(|| format!("research node {node_id} was not found"))?;
            let active = node.status.is_active();
            if !active && node.pane_id.is_none() {
                return Err("research run is not active".to_string());
            }
            if active {
                node.status = ResearchNodeStatus::Cancelled;
                node.error = None;
                node.completed_at = Some(now_millis());
            }
            let pane_id = node.pane_id.clone();
            let node = node.clone();
            touch_research_tree_locked(&mut model, &node.tree_id, now_millis());
            (node, pane_id)
        };
        self.persist();
        self.emit(QmuxEvent::new(
            "research.node.updated",
            node.pane_id.clone(),
            node.agent_id.clone(),
            json!({ "node": node }),
        ));
        if let Some(pane_id) = pane_id {
            // The pane detach path clears the binding; a Cancelled node is
            // already settled, so detach leaves its status alone.
            if let Err(err) = crate::pty::kill_pane(self, pane_id.clone()) {
                if self.pane_exists(&pane_id).unwrap_or(false) {
                    // Keep the Cancelled outcome monotonic, but report the partial
                    // failure. The UI keeps cancellation available while pane_id
                    // remains bound, so the user can retry instead of leaving an
                    // invisible process that pins the tree until restart.
                    return Err(format!(
                        "research was cancelled, but its terminal could not be closed: {err}"
                    ));
                }
                // The pane record no longer exists, so no EOF/teardown is left
                // to run the detach for us. Clear the binding here — this is
                // the reclaim path the doc comment above promises — or the
                // settled node keeps counting as an active run (blocking
                // archive/remove and folder changes) until restart.
                if let Err(err) = self.detach_research_pane(&pane_id) {
                    eprintln!("qmux: failed to detach research pane {pane_id}: {err}");
                }
            }
        }
        self.research_node(node_id)
    }

    pub fn active_research_node_for_pane(
        &self,
        pane_id: &str,
    ) -> Result<Option<ResearchNode>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model
            .research_nodes
            .values()
            .find(|node| node.pane_id.as_deref() == Some(pane_id) && node.status.is_active())
            .cloned())
    }

    pub fn close_pane_for_user(&self, pane_id: &str) -> Result<(), String> {
        if let Some(node) = self.active_research_node_for_pane(pane_id)? {
            self.cancel_research_node(&node.id).map(|_| ())
        } else {
            crate::pty::kill_pane(self, pane_id.to_string())
        }
    }

    /// Records a native-surface user close before its delegate removes the pane.
    /// The delegate already owns teardown, so this settles only the node and lets
    /// the ordinary remove path clear runtime bindings without rewriting it Failed.
    pub fn settle_research_pane_cancelled(&self, pane_id: &str) -> Result<bool, String> {
        let updated = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let node_id = model
                .research_nodes
                .values()
                .find(|node| node.pane_id.as_deref() == Some(pane_id) && node.status.is_active())
                .map(|node| node.id.clone());
            node_id.and_then(|node_id| {
                let now = now_millis();
                let node = model.research_nodes.get_mut(&node_id)?;
                node.status = ResearchNodeStatus::Cancelled;
                node.error = None;
                node.completed_at = Some(now);
                let node = node.clone();
                touch_research_tree_locked(&mut model, &node.tree_id, now);
                Some(node)
            })
        };
        let Some(node) = updated else {
            return Ok(false);
        };
        self.persist();
        self.emit(QmuxEvent::new(
            "research.node.updated",
            node.pane_id.clone(),
            node.agent_id.clone(),
            json!({ "node": node }),
        ));
        Ok(true)
    }

    pub fn detach_research_pane(&self, pane_id: &str) -> Result<Option<ResearchNode>, String> {
        self.detach_research_pane_inner(pane_id, None)
    }

    /// `removed_agent` carries the bound agent's id and status as captured by
    /// `remove_pane` before it pruned the record: by the time the detach runs
    /// on the teardown path the agent is already gone from the model (and on
    /// the kept-for-queue path its status has been parked Idle), so reading
    /// the live record here could never see the real end-of-turn status.
    fn detach_research_pane_inner(
        &self,
        pane_id: &str,
        removed_agent: Option<(&str, AgentStatus, bool)>,
    ) -> Result<Option<ResearchNode>, String> {
        let updated = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let node_id = model
                .research_nodes
                .values()
                .find(|node| node.pane_id.as_deref() == Some(pane_id))
                .map(|node| node.id.clone());
            node_id.and_then(|node_id| {
                let now = now_millis();
                // An adapter whose process exits the moment its turn ends can
                // race its own Done notification: the pane teardown lands here
                // while the node is still nominally active. If the agent has
                // already reported end-of-turn, the run *finished* — settling
                // it Failed would brand a delivered answer, and monotonic
                // terminal statuses would keep it branded forever.
                let agent_finished = model
                    .research_nodes
                    .get(&node_id)
                    .and_then(|node| node.agent_id.as_deref())
                    .and_then(|agent_id| {
                        removed_agent
                            .filter(|(removed_id, _, _)| *removed_id == agent_id)
                            .map(|(_, status, active)| (status, active))
                            .or_else(|| {
                                model.agents.get(agent_id).map(|agent| {
                                    let active = model
                                        .agent_active_subagents
                                        .get(agent_id)
                                        .is_some_and(|active| !active.is_empty());
                                    (agent.status, active)
                                })
                            })
                    })
                    .is_some_and(|(status, active)| {
                        matches!(status, AgentStatus::Done | AgentStatus::Idle) && !active
                    });
                let node = model.research_nodes.get_mut(&node_id)?;
                node.pane_id = None;
                if node.status.is_active() {
                    if agent_finished {
                        node.status = ResearchNodeStatus::Complete;
                    } else {
                        node.status = ResearchNodeStatus::Failed;
                        node.error = Some("Research process exited before completion".to_string());
                    }
                    node.completed_at = Some(now);
                }
                let node = node.clone();
                touch_research_tree_locked(&mut model, &node.tree_id, now);
                Some(node)
            })
        };
        if let Some(node) = &updated {
            self.persist();
            self.emit(QmuxEvent::new(
                "research.node.updated",
                None,
                None,
                json!({ "node": node }),
            ));
        }
        Ok(updated)
    }

    pub fn rename_research_tree(
        &self,
        tree_id: &str,
        title: String,
    ) -> Result<ResearchTree, String> {
        let _document_guard = self
            .inner
            .research_document_lock
            .lock()
            .map_err(|_| "research document lock poisoned".to_string())?;
        let title = title.trim().to_string();
        if title.is_empty() {
            return Err("research title cannot be empty".to_string());
        }
        let tree = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let tree = model
                .research_trees
                .get_mut(tree_id)
                .ok_or_else(|| format!("research tree {tree_id} was not found"))?;
            tree.title = title;
            tree.updated_at = now_millis();
            tree.clone()
        };
        self.persist();
        self.emit(QmuxEvent::new(
            "research.tree.updated",
            None,
            None,
            json!({ "tree": tree }),
        ));
        Ok(tree)
    }

    pub fn set_research_node_title(
        &self,
        node_id: &str,
        title: String,
    ) -> Result<ResearchNode, String> {
        let title = title.trim().to_string();
        if title.is_empty() {
            return Err("research node title cannot be empty".to_string());
        }
        let node = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let node = model
                .research_nodes
                .get_mut(node_id)
                .ok_or_else(|| format!("research node {node_id} was not found"))?;
            node.title = Some(title);
            node.clone()
        };
        self.persist();
        self.emit(QmuxEvent::new(
            "research.node.updated",
            None,
            None,
            json!({ "node": node }),
        ));
        Ok(node)
    }

    pub fn create_research_highlight(
        &self,
        node_id: &str,
        anchor: ResearchHighlightAnchor,
    ) -> Result<ResearchHighlight, String> {
        let _document_guard = self
            .inner
            .research_document_lock
            .lock()
            .map_err(|_| "research document lock poisoned".to_string())?;
        research::validate_highlight_anchor(&anchor)?;
        self.research_node(node_id)?;
        let snapshot = research::read_response_snapshot_with_revision(
            &self.inner.config.workspace_root,
            node_id,
        )?
        .ok_or_else(|| {
            "research highlights require a durable full response snapshot".to_string()
        })?;
        if snapshot.revision != anchor.response_revision {
            return Err("the research response changed; select the text again".to_string());
        }

        let mut highlight = ResearchHighlight {
            id: self.next_id("research-highlight"),
            anchor,
            created_at: now_millis(),
        };
        {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let total_bytes = model
                .research_nodes
                .values()
                .flat_map(|node| node.highlights.iter())
                .fold(0usize, |total, highlight| {
                    total.saturating_add(research::highlight_storage_bytes(highlight))
                });
            while model
                .research_nodes
                .values()
                .any(|node| node.highlights.iter().any(|saved| saved.id == highlight.id))
            {
                highlight.id = self.next_id("research-highlight");
            }
            let added_bytes = research::highlight_storage_bytes(&highlight);
            if total_bytes.saturating_add(added_bytes)
                > research::MAX_RESEARCH_HIGHLIGHT_BYTES_TOTAL
            {
                return Err("qmux contains too much saved research highlight data".to_string());
            }
            let node = model
                .research_nodes
                .get_mut(node_id)
                .ok_or_else(|| format!("research node {node_id} was not found"))?;
            let mut next_highlights = node.highlights.clone();
            next_highlights.push(highlight.clone());
            research::validate_highlight_collection(&next_highlights)?;
            node.highlights.push(highlight.clone());
        }
        self.persist();
        self.emit(QmuxEvent::new(
            "research.highlight.created",
            None,
            None,
            json!({ "nodeId": node_id, "highlight": highlight }),
        ));
        Ok(highlight)
    }

    pub fn remove_research_highlight(
        &self,
        node_id: &str,
        highlight_id: &str,
    ) -> Result<ResearchHighlight, String> {
        let _document_guard = self
            .inner
            .research_document_lock
            .lock()
            .map_err(|_| "research document lock poisoned".to_string())?;
        let removed = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let node = model
                .research_nodes
                .get_mut(node_id)
                .ok_or_else(|| format!("research node {node_id} was not found"))?;
            let index = node
                .highlights
                .iter()
                .position(|highlight| highlight.id == highlight_id)
                .ok_or_else(|| format!("research highlight {highlight_id} was not found"))?;
            node.highlights.remove(index)
        };
        self.persist();
        self.emit(QmuxEvent::new(
            "research.highlight.removed",
            None,
            None,
            json!({ "nodeId": node_id, "highlightId": highlight_id }),
        ));
        Ok(removed)
    }

    pub fn remove_research_highlights(
        &self,
        node_id: &str,
        highlight_ids: &[String],
    ) -> Result<Vec<ResearchHighlight>, String> {
        if highlight_ids.len() > research::MAX_RESEARCH_HIGHLIGHTS_PER_NODE {
            return Err(format!(
                "cannot remove more than {} research highlights at once",
                research::MAX_RESEARCH_HIGHLIGHTS_PER_NODE
            ));
        }
        let _document_guard = self
            .inner
            .research_document_lock
            .lock()
            .map_err(|_| "research document lock poisoned".to_string())?;
        let requested = highlight_ids
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let removed = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let node = model
                .research_nodes
                .get_mut(node_id)
                .ok_or_else(|| format!("research node {node_id} was not found"))?;
            let mut removed = Vec::new();
            node.highlights.retain(|highlight| {
                if requested.contains(highlight.id.as_str()) {
                    removed.push(highlight.clone());
                    false
                } else {
                    true
                }
            });
            removed
        };
        if removed.is_empty() {
            return Ok(removed);
        }
        self.persist();
        self.emit(QmuxEvent::new(
            "research.highlights.removed",
            None,
            None,
            json!({
                "nodeId": node_id,
                "highlightIds": removed.iter().map(|highlight| &highlight.id).collect::<Vec<_>>(),
            }),
        ));
        Ok(removed)
    }

    pub fn mark_research_tree_viewed(&self, tree_id: &str) -> Result<ResearchTree, String> {
        let (tree, changed) = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let latest_settlement = model
                .research_nodes
                .values()
                .filter(|node| node.tree_id == tree_id)
                .filter_map(|node| node.completed_at)
                .max();
            let tree = model
                .research_trees
                .get_mut(tree_id)
                .ok_or_else(|| format!("research tree {tree_id} was not found"))?;
            let changed = latest_settlement.is_some_and(|settled_at| {
                tree.last_viewed_at
                    .is_none_or(|last_viewed_at| settled_at > last_viewed_at)
            });
            if changed {
                let viewed_at = now_millis().max(latest_settlement.unwrap_or_default());
                tree.last_viewed_at = Some(viewed_at);
            }
            (tree.clone(), changed)
        };
        if changed {
            self.persist();
        }
        Ok(tree)
    }

    pub fn archive_research_tree(&self, tree_id: &str) -> Result<ResearchTree, String> {
        let _document_guard = self
            .inner
            .research_document_lock
            .lock()
            .map_err(|_| "research document lock poisoned".to_string())?;
        let tree = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if model.research_nodes.values().any(|node| {
                node.tree_id == tree_id && (node.pane_id.is_some() || node.status.is_active())
            }) {
                return Err("cannot archive research while it has active runs".to_string());
            }
            let tree = model
                .research_trees
                .get_mut(tree_id)
                .ok_or_else(|| format!("research tree {tree_id} was not found"))?;
            if tree.archived_at.is_none() {
                let now = now_millis();
                tree.archived_at = Some(now);
                tree.last_viewed_at = Some(now);
            }
            tree.clone()
        };
        self.persist();
        self.emit(QmuxEvent::new(
            "research.tree.archived",
            None,
            None,
            json!({ "tree": tree }),
        ));
        Ok(tree)
    }

    pub fn restore_research_tree(&self, tree_id: &str) -> Result<ResearchTree, String> {
        let _document_guard = self
            .inner
            .research_document_lock
            .lock()
            .map_err(|_| "research document lock poisoned".to_string())?;
        let tree = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let tree = model
                .research_trees
                .get_mut(tree_id)
                .ok_or_else(|| format!("research tree {tree_id} was not found"))?;
            tree.archived_at = None;
            tree.last_viewed_at = Some(now_millis());
            tree.clone()
        };
        self.persist();
        self.emit(QmuxEvent::new(
            "research.tree.restored",
            None,
            None,
            json!({ "tree": tree }),
        ));
        Ok(tree)
    }

    pub fn remove_research_tree(&self, tree_id: &str) -> Result<(), String> {
        let _document_guard = self
            .inner
            .research_document_lock
            .lock()
            .map_err(|_| "research document lock poisoned".to_string())?;
        let (removed, removed_node_ids, reaped_thread_records) = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if model.research_nodes.values().any(|node| {
                node.tree_id == tree_id && (node.pane_id.is_some() || node.status.is_active())
            }) {
                return Err("cannot remove a research tree while it has active runs".to_string());
            }
            let node_ids = model
                .research_nodes
                .values()
                .filter(|node| node.tree_id == tree_id)
                .map(|node| node.id.clone())
                .collect::<Vec<_>>();
            // Each run minted a thread record (and an on-disk graph snapshot)
            // via the ordinary agent machinery, and nothing else ever reaps
            // them once the run's agent is pruned — deleting the tree is the
            // last point where the node still links run to record. Skip any
            // record a live agent still references (a pane teardown may be
            // settling concurrently); it is re-reaped only if its tree is
            // removed again, so erring towards keeping is safe.
            let thread_ids = model
                .research_nodes
                .values()
                .filter(|node| node.tree_id == tree_id)
                .filter_map(|node| node.thread_id.clone())
                .filter(|thread_id| {
                    !model
                        .agents
                        .values()
                        .any(|agent| agent.thread_id.as_deref() == Some(thread_id))
                })
                .collect::<Vec<_>>();
            let removed = model.research_trees.remove(tree_id).is_some();
            let mut reaped_records = Vec::new();
            if removed {
                model.research_tree_order.retain(|id| id != tree_id);
                // The grouping must not outlive the tree: drop its membership and
                // star, and prune a folder left with no members. Doing it here
                // keeps the persisted grouping clean without the per-refresh prune.
                research::remove_trees_from_research_folders(
                    &mut model.research_folders,
                    &HashSet::from([tree_id.to_string()]),
                );
                model
                    .research_nodes
                    .retain(|_, node| node.tree_id != tree_id);
                for thread_id in &thread_ids {
                    if let Some(record) = model.threads.remove(thread_id) {
                        reaped_records.push(record);
                    }
                    model.thread_focus.remove(thread_id);
                }
            }
            // A research tree references its durable workspace; it does not own
            // it. Other trees may use the same directory, so deleting a tree
            // never deletes the workspace record or anything in that directory.
            (removed, node_ids, reaped_records)
        };
        if !removed {
            return Err(format!("research tree {tree_id} was not found"));
        }
        self.persist();
        self.emit(QmuxEvent::new(
            "research.tree.removed",
            None,
            None,
            json!({ "treeId": tree_id }),
        ));
        for node_id in removed_node_ids {
            if let Err(err) =
                research::remove_response_snapshot(&self.inner.config.workspace_root, &node_id)
            {
                eprintln!("qmux: failed to remove research response {node_id}: {err}");
            }
        }
        // Best-effort: the graph snapshots are unreachable once their records
        // are gone, and a leftover file is only clutter.
        for record in reaped_thread_records {
            let path = std::path::Path::new(&record.snapshot_path);
            if let Err(err) = std::fs::remove_file(path)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                eprintln!(
                    "qmux: failed to remove research thread graph {}: {err}",
                    record.snapshot_path
                );
            }
        }
        Ok(())
    }

    pub fn remove_research_branch(&self, node_id: &str) -> Result<ResearchBranchRemoval, String> {
        let (removal, removed_node_ids, reaped_thread_records) = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let target = model
                .research_nodes
                .get(node_id)
                .cloned()
                .ok_or_else(|| format!("research node {node_id} was not found"))?;
            let tree = model
                .research_trees
                .get(&target.tree_id)
                .ok_or_else(|| format!("research tree {} was not found", target.tree_id))?;
            if tree.root_node_id == target.id || target.parent_node_id.is_none() {
                return Err(
                    "the root research cannot be deleted as a branch; delete the research instead"
                        .to_string(),
                );
            }

            let mut subtree_ids = HashSet::from([target.id.clone()]);
            loop {
                let descendants = model
                    .research_nodes
                    .values()
                    .filter(|node| {
                        node.tree_id == target.tree_id
                            && node
                                .parent_node_id
                                .as_ref()
                                .is_some_and(|parent_id| subtree_ids.contains(parent_id))
                            && !subtree_ids.contains(&node.id)
                    })
                    .map(|node| node.id.clone())
                    .collect::<Vec<_>>();
                if descendants.is_empty() {
                    break;
                }
                subtree_ids.extend(descendants);
            }

            if model.research_nodes.values().any(|node| {
                subtree_ids.contains(&node.id)
                    && (node.pane_id.is_some() || node.status.is_active())
            }) {
                return Err("cannot delete a research branch while it has active runs".to_string());
            }

            let thread_ids = model
                .research_nodes
                .values()
                .filter(|node| subtree_ids.contains(&node.id))
                .filter_map(|node| node.thread_id.clone())
                .filter(|thread_id| {
                    !model
                        .agents
                        .values()
                        .any(|agent| agent.thread_id.as_deref() == Some(thread_id))
                })
                .collect::<HashSet<_>>();
            let mut removed_node_ids = subtree_ids.into_iter().collect::<Vec<_>>();
            removed_node_ids.sort_by_key(|id| {
                model
                    .research_nodes
                    .get(id)
                    .map(|node| (node.created_at, node.id.clone()))
            });
            model
                .research_nodes
                .retain(|id, _| !removed_node_ids.contains(id));
            let reaped_thread_records = thread_ids
                .into_iter()
                .filter_map(|thread_id| {
                    model.thread_focus.remove(&thread_id);
                    model.threads.remove(&thread_id)
                })
                .collect::<Vec<_>>();
            touch_research_tree_locked(&mut model, &target.tree_id, now_millis());
            (
                ResearchBranchRemoval {
                    tree_id: target.tree_id,
                    parent_node_id: target.parent_node_id.expect("non-root target has a parent"),
                    removed_node_ids: removed_node_ids.clone(),
                },
                removed_node_ids,
                reaped_thread_records,
            )
        };
        self.persist();
        self.emit(QmuxEvent::new(
            "research.node.removed",
            None,
            None,
            json!({
                "treeId": removal.tree_id,
                "parentNodeId": removal.parent_node_id,
                "removedNodeIds": removal.removed_node_ids,
            }),
        ));
        for node_id in removed_node_ids {
            if let Err(err) =
                research::remove_response_snapshot(&self.inner.config.workspace_root, &node_id)
            {
                eprintln!("qmux: failed to remove research response {node_id}: {err}");
            }
        }
        for record in reaped_thread_records {
            let path = std::path::Path::new(&record.snapshot_path);
            if let Err(err) = std::fs::remove_file(path)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                eprintln!(
                    "qmux: failed to remove research thread graph {}: {err}",
                    record.snapshot_path
                );
            }
        }
        Ok(removal)
    }

    pub fn group(&self, group_id: &str) -> Result<Option<GroupInfo>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model.groups.get(group_id).cloned())
    }

    pub fn research_workspace_dependencies(
        &self,
        workspace_id: &str,
    ) -> Result<ResearchWorkspaceDependencies, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        let tree_ids = model
            .research_trees
            .values()
            .filter(|tree| tree.workspace_id == workspace_id)
            .map(|tree| tree.id.as_str())
            .collect::<HashSet<_>>();
        Ok(ResearchWorkspaceDependencies {
            tree_count: tree_ids.len(),
            has_active_runs: model.research_nodes.values().any(|node| {
                tree_ids.contains(node.tree_id.as_str())
                    && (node.status.is_active() || node.pane_id.is_some())
            }),
            has_live_panes: model
                .panes
                .values()
                .any(|pane| pane.info.group_id == workspace_id),
        })
    }

    pub fn detached_research_archive(
        &self,
        workspace_id: &str,
    ) -> Result<research::DetachedResearchArchive, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        let mut workspace = model
            .groups
            .get(workspace_id)
            .filter(|group| group.scope == WorkspaceScope::Research)
            .cloned()
            .ok_or_else(|| format!("research workspace {workspace_id} was not found"))?;
        // managed_dir is installation-local bookkeeping and is deleted after
        // detach. Runtime agent membership must not cross an import boundary.
        workspace.managed_dir.clear();
        workspace.agents.clear();
        workspace.imported_research_archive_id = None;
        let mut trees = model
            .research_trees
            .values()
            .filter(|tree| tree.workspace_id == workspace_id)
            .cloned()
            .collect::<Vec<_>>();
        trees.sort_by_key(|tree| (tree.created_at, tree.id.clone()));
        let tree_ids = trees
            .iter()
            .map(|tree| tree.id.as_str())
            .collect::<HashSet<_>>();
        let tree_order = ordered_research_tree_ids(&model)
            .into_iter()
            .filter(|tree_id| tree_ids.contains(tree_id.as_str()))
            .collect::<Vec<_>>();
        let mut nodes = model
            .research_nodes
            .values()
            .filter(|node| tree_ids.contains(node.tree_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        nodes.sort_by_key(|node| (node.created_at, node.id.clone()));
        // Carry the grouping for this workspace's trees so an import restores the
        // folders too. Scope to this workspace's folders and to membership whose
        // tree actually travels in the archive; stars/collapsed are per-install
        // view state and stay behind.
        let mut folders = model
            .research_folders
            .folders
            .iter()
            .filter(|folder| folder.workspace_id == workspace_id)
            .cloned()
            .collect::<Vec<_>>();
        folders.sort_by(|left, right| left.id.cmp(&right.id));
        let folder_ids = folders
            .iter()
            .map(|folder| folder.id.as_str())
            .collect::<HashSet<_>>();
        let membership = model
            .research_folders
            .membership
            .iter()
            .filter(|(tree_id, folder_id)| {
                tree_ids.contains(tree_id.as_str()) && folder_ids.contains(folder_id.as_str())
            })
            .map(|(tree_id, folder_id)| (tree_id.clone(), folder_id.clone()))
            .collect::<HashMap<_, _>>();
        Ok(research::DetachedResearchArchive {
            version: research::detached_archive_version(&nodes),
            archive_id: research::new_detached_research_archive_id()?,
            workspace,
            trees,
            tree_order,
            folders,
            membership,
            nodes,
            exported_at: now_millis(),
        })
    }

    /// Removes a Research workspace and all of its durable records after its
    /// portable archive has been verified. The checked persistence barrier is
    /// the commit point: on failure the in-memory records are restored and the
    /// caller leaves the pending folder archive in place for a safe retry.
    pub fn commit_research_workspace_detach(
        &self,
        workspace_id: &str,
        expected: &research::DetachedResearchArchive,
    ) -> Result<Vec<String>, String> {
        let _persist_guard = self
            .inner
            .persist_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (
            workspace,
            trees,
            nodes,
            group_order,
            research_tree_order,
            research_folders,
            recent_sessions,
            reaped_thread_records,
        ) = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if model
                .panes
                .values()
                .any(|pane| pane.info.group_id == workspace_id)
            {
                return Err("research folder still has live terminals".to_string());
            }
            let workspace = model
                .groups
                .get(workspace_id)
                .filter(|group| group.scope == WorkspaceScope::Research)
                .cloned()
                .ok_or_else(|| format!("research workspace {workspace_id} was not found"))?;
            let tree_ids = model
                .research_trees
                .values()
                .filter(|tree| tree.workspace_id == workspace_id)
                .map(|tree| tree.id.clone())
                .collect::<HashSet<_>>();
            let active = model.research_nodes.values().any(|node| {
                tree_ids.contains(&node.tree_id)
                    && (node.status.is_active() || node.pane_id.is_some())
            });
            if active {
                return Err("research folder still has active runs".to_string());
            }
            if model
                .agents
                .values()
                .any(|agent| agent.group_id == workspace_id)
            {
                return Err("research folder still has a live agent record".to_string());
            }
            let mut current_workspace = workspace.clone();
            current_workspace.managed_dir.clear();
            current_workspace.agents.clear();
            current_workspace.imported_research_archive_id = None;
            let mut current_trees = tree_ids
                .iter()
                .filter_map(|id| model.research_trees.get(id).cloned())
                .collect::<Vec<_>>();
            current_trees.sort_by_key(|tree| (tree.created_at, tree.id.clone()));
            let current_tree_ids = current_trees
                .iter()
                .map(|tree| tree.id.as_str())
                .collect::<HashSet<_>>();
            let current_tree_order = ordered_research_tree_ids(&model)
                .into_iter()
                .filter(|tree_id| current_tree_ids.contains(tree_id.as_str()))
                .collect::<Vec<_>>();
            let mut current_nodes = model
                .research_nodes
                .values()
                .filter(|node| current_tree_ids.contains(node.tree_id.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            current_nodes.sort_by_key(|node| (node.created_at, node.id.clone()));
            if current_workspace != expected.workspace
                || current_trees != expected.trees
                || (!expected.tree_order.is_empty() && current_tree_order != expected.tree_order)
                || current_nodes != expected.nodes
            {
                return Err(
                    "research changed while its folder archive was being prepared; try removing the folder again"
                        .to_string(),
                );
            }
            let trees = tree_ids
                .iter()
                .filter_map(|id| {
                    model
                        .research_trees
                        .remove(id)
                        .map(|tree| (id.clone(), tree))
                })
                .collect::<Vec<_>>();
            let research_tree_order = model.research_tree_order.clone();
            model
                .research_tree_order
                .retain(|tree_id| !tree_ids.contains(tree_id));
            // The workspace's folders leave with it: scrub its trees' membership
            // and stars, then drop any of its folders that remain (an empty one
            // had no member to carry it out). Snapshot first for the rollback.
            let research_folders = model.research_folders.clone();
            research::remove_trees_from_research_folders(&mut model.research_folders, &tree_ids);
            research::remove_research_workspace_folders(&mut model.research_folders, workspace_id);
            let node_ids = model
                .research_nodes
                .values()
                .filter(|node| tree_ids.contains(&node.tree_id))
                .map(|node| node.id.clone())
                .collect::<HashSet<_>>();
            let nodes = node_ids
                .iter()
                .filter_map(|id| {
                    model
                        .research_nodes
                        .remove(id)
                        .map(|node| (id.clone(), node))
                })
                .collect::<Vec<_>>();
            // Folder detach bypasses remove_research_tree, so reap the same
            // installation-local thread records here before the nodes carrying
            // their ids disappear. Preserve anything a live agent still uses.
            // Keep removed focus entries alongside the records so a failed
            // persistence commit can restore the model exactly.
            let thread_ids = nodes
                .iter()
                .filter_map(|(_, node)| node.thread_id.clone())
                .filter(|thread_id| {
                    !model
                        .agents
                        .values()
                        .any(|agent| agent.thread_id.as_deref() == Some(thread_id))
                })
                .collect::<HashSet<_>>();
            let reaped_thread_records = thread_ids
                .into_iter()
                .map(|thread_id| {
                    let record = model.threads.remove(&thread_id);
                    let focus = model.thread_focus.remove(&thread_id);
                    (thread_id, record, focus)
                })
                .collect::<Vec<_>>();
            let agent_ids = nodes
                .iter()
                .filter_map(|(_, node)| node.agent_id.clone())
                .collect::<HashSet<_>>();
            let session_ids = nodes
                .iter()
                .filter_map(|(_, node)| node.native_session_id.clone())
                .collect::<HashSet<_>>();
            let transcript_paths = nodes
                .iter()
                .filter_map(|(_, node)| node.transcript_path.clone())
                .collect::<HashSet<_>>();
            let recent_sessions = model.recent_sessions.clone();
            model.recent_sessions.retain(|_, session| {
                !session
                    .agent_id
                    .as_ref()
                    .is_some_and(|id| agent_ids.contains(id))
                    && !session
                        .session_id
                        .as_ref()
                        .is_some_and(|id| session_ids.contains(id))
                    && !session
                        .transcript_path
                        .as_ref()
                        .is_some_and(|path| transcript_paths.contains(path))
            });
            let group_order = model.group_order.clone();
            model.groups.remove(workspace_id);
            model.group_order.retain(|id| id != workspace_id);
            (
                workspace,
                trees,
                nodes,
                group_order,
                research_tree_order,
                research_folders,
                recent_sessions,
                reaped_thread_records,
            )
        };

        let persist_result = if self.inner.persist_enabled.load(Ordering::Relaxed) {
            self.persist_snapshot_locked()
        } else {
            Ok(())
        };
        if let Err(err) = persist_result {
            if let Ok(mut model) = self.inner.model.lock() {
                model.groups.insert(workspace.id.clone(), workspace);
                model.group_order = group_order;
                model.research_tree_order = research_tree_order;
                model.research_folders = research_folders;
                for (id, tree) in trees {
                    model.research_trees.insert(id, tree);
                }
                for (id, node) in nodes {
                    model.research_nodes.insert(id, node);
                }
                for (thread_id, record, focus) in reaped_thread_records {
                    if let Some(record) = record {
                        model.threads.insert(thread_id.clone(), record);
                    }
                    if let Some(focus) = focus {
                        model.thread_focus.insert(thread_id, focus);
                    }
                }
                model.recent_sessions = recent_sessions;
            }
            return Err(format!("failed to commit global research detach: {err}"));
        }
        let node_ids = nodes.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
        self.emit(QmuxEvent::new(
            "group.removed",
            None,
            None,
            json!({ "groupId": workspace_id }),
        ));
        // Best-effort after the durable commit: the records are unreachable,
        // and a leftover graph file is only disk clutter.
        for (_, record, _) in reaped_thread_records {
            let Some(record) = record else {
                continue;
            };
            let path = std::path::Path::new(&record.snapshot_path);
            if let Err(err) = std::fs::remove_file(path)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                eprintln!(
                    "qmux: failed to remove detached research thread graph {}: {err}",
                    record.snapshot_path
                );
            }
        }
        Ok(node_ids)
    }

    pub fn import_detached_research(
        &self,
        workspace: GroupInfo,
        tree_order: Vec<String>,
        mut trees: Vec<ResearchTree>,
        folders: Vec<research::ResearchFolder>,
        membership: HashMap<String, String>,
        mut nodes: Vec<ResearchNode>,
        responses: HashMap<String, Vec<Turn>>,
    ) -> Result<GroupInfo, String> {
        // Imported nodes bypass create_research_child's one-inline-child
        // check; repair the slot invariant here so a tampered archive cannot
        // admit a permanently occupied slot the viewer cannot free.
        research::normalize_inline_slots(&mut nodes);
        // Import rewrites response JSON with this build's serializer. Retarget
        // anchors to those exact bytes so a schema-preserving app upgrade does
        // not make otherwise valid portable highlights disappear.
        for node in &mut nodes {
            let Some(turns) = responses.get(&node.id) else {
                continue;
            };
            let revision = research::response_revision(turns)?;
            for highlight in &mut node.highlights {
                highlight.anchor.response_revision = revision.clone();
            }
        }
        let incoming_highlight_bytes = nodes
            .iter()
            .flat_map(|node| node.highlights.iter())
            .fold(0usize, |total, highlight| {
                total.saturating_add(research::highlight_storage_bytes(highlight))
            });
        let (tree_ids_in_use, mut node_ids_in_use, folder_ids_in_use) = {
            let model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let existing_highlight_bytes = model
                .research_nodes
                .values()
                .flat_map(|node| node.highlights.iter())
                .fold(0usize, |total, highlight| {
                    total.saturating_add(research::highlight_storage_bytes(highlight))
                });
            if existing_highlight_bytes.saturating_add(incoming_highlight_bytes)
                > research::MAX_RESEARCH_HIGHLIGHT_BYTES_TOTAL
            {
                return Err(
                    "import would exceed qmux's research highlight storage limit".to_string(),
                );
            }
            (
                model.research_trees.keys().cloned().collect::<HashSet<_>>(),
                model.research_nodes.keys().cloned().collect::<HashSet<_>>(),
                model
                    .research_folders
                    .folders
                    .iter()
                    .map(|folder| folder.id.clone())
                    .collect::<HashSet<_>>(),
            )
        };
        for node in &nodes {
            if !matches!(
                research::read_response_snapshot(&self.inner.config.workspace_root, &node.id),
                Ok(None)
            ) {
                node_ids_in_use.insert(node.id.clone());
            }
        }
        let source_tree_order = if tree_order.is_empty() {
            let mut legacy_order = trees.iter().collect::<Vec<_>>();
            legacy_order.sort_by(|left, right| {
                right
                    .updated_at
                    .cmp(&left.updated_at)
                    .then(left.id.cmp(&right.id))
            });
            legacy_order
                .into_iter()
                .map(|tree| tree.id.clone())
                .collect::<Vec<_>>()
        } else {
            tree_order
        };
        let mut tree_map = HashMap::new();
        let mut reserved_tree_ids = tree_ids_in_use;
        for tree in &trees {
            let id = if reserved_tree_ids.insert(tree.id.clone()) {
                tree.id.clone()
            } else {
                loop {
                    let candidate = self.next_id("research");
                    if reserved_tree_ids.insert(candidate.clone()) {
                        break candidate;
                    }
                }
            };
            tree_map.insert(tree.id.clone(), id);
        }
        let imported_tree_order = source_tree_order
            .iter()
            .filter_map(|tree_id| tree_map.get(tree_id).cloned())
            .collect::<Vec<_>>();
        // Remap folder ids the same way trees are: keep the archive's id unless
        // it collides with a local folder, then mint a fresh one. Folders are
        // re-homed onto the imported workspace, and membership is retargeted
        // through both id maps. Membership whose tree or folder didn't survive
        // the archive is dropped rather than dangling.
        let mut folder_map = HashMap::new();
        let mut reserved_folder_ids = folder_ids_in_use;
        let mut imported_folders = Vec::with_capacity(folders.len());
        for folder in folders {
            let id = if reserved_folder_ids.insert(folder.id.clone()) {
                folder.id.clone()
            } else {
                loop {
                    let candidate = self.next_id("research-folder");
                    if reserved_folder_ids.insert(candidate.clone()) {
                        break candidate;
                    }
                }
            };
            folder_map.insert(folder.id.clone(), id.clone());
            imported_folders.push(research::ResearchFolder {
                id,
                name: folder.name,
                workspace_id: workspace.id.clone(),
            });
        }
        let imported_membership = membership
            .into_iter()
            .filter_map(|(tree_id, folder_id)| {
                Some((
                    tree_map.get(&tree_id)?.clone(),
                    folder_map.get(&folder_id)?.clone(),
                ))
            })
            .collect::<Vec<_>>();
        let mut node_map = HashMap::new();
        let mut reserved_node_ids = node_ids_in_use;
        for node in &nodes {
            let id = if reserved_node_ids.insert(node.id.clone()) {
                node.id.clone()
            } else {
                loop {
                    let candidate = self.next_id("research-node");
                    if reserved_node_ids.insert(candidate.clone()) {
                        break candidate;
                    }
                }
            };
            node_map.insert(node.id.clone(), id);
        }
        for tree in &mut trees {
            tree.id = tree_map[&tree.id].clone();
            tree.root_node_id = node_map
                .get(&tree.root_node_id)
                .cloned()
                .ok_or_else(|| "research archive root node mapping is incomplete".to_string())?;
            tree.workspace_id = workspace.id.clone();
        }
        for node in &mut nodes {
            let old_id = node.id.clone();
            node.id = node_map[&old_id].clone();
            node.tree_id = tree_map
                .get(&node.tree_id)
                .cloned()
                .ok_or_else(|| "research archive tree mapping is incomplete".to_string())?;
            node.parent_node_id =
                node.parent_node_id
                    .as_ref()
                    .map(|id| {
                        node_map.get(id).cloned().ok_or_else(|| {
                            "research archive parent mapping is incomplete".to_string()
                        })
                    })
                    .transpose()?;
            node.group_id = workspace.id.clone();
            node.worktree_dir = workspace.dir.clone();
            // Runtime bindings never survive a detach/import boundary. Keeping
            // an old agent id could accidentally bind a restored follow-up to
            // an unrelated live agent whose installation-local id collides.
            node.pane_id = None;
            node.agent_id = None;
            node.transcript_path = None;
            // Thread records and their graph snapshots are installation-local
            // and are not part of the portable archive. Retaining a foreign id
            // could make later tree removal delete an unrelated local record
            // whose generated id happens to collide.
            node.thread_id = None;
            // Only responses that actually travelled in the archive get
            // written back below; a node imported without one must not keep
            // a snapshot stamp claiming a durable answer exists.
            if !responses.contains_key(&old_id) {
                node.response_snapshot_at = None;
            }
        }

        let mut written_node_ids = Vec::new();
        let write_result = (|| -> Result<(), String> {
            for (old_id, turns) in responses {
                let Some(new_id) = node_map.get(&old_id) else {
                    continue;
                };
                research::write_response_snapshot(
                    &self.inner.config.workspace_root,
                    new_id,
                    &turns,
                )?;
                written_node_ids.push(new_id.clone());
            }
            Ok(())
        })();
        if let Err(err) = write_result {
            for node_id in written_node_ids {
                let _ =
                    research::remove_response_snapshot(&self.inner.config.workspace_root, &node_id);
            }
            return Err(err);
        }
        let _persist_guard = self
            .inner
            .persist_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let insert_result = (|| -> Result<(), String> {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if model.groups.contains_key(&workspace.id) {
                return Err(format!("workspace {} already exists", workspace.id));
            }
            model.group_order.push(workspace.id.clone());
            model.groups.insert(workspace.id.clone(), workspace.clone());
            model
                .research_tree_order
                .extend(imported_tree_order.iter().cloned());
            for tree in &trees {
                model.research_trees.insert(tree.id.clone(), tree.clone());
            }
            for node in &nodes {
                model.research_nodes.insert(node.id.clone(), node.clone());
            }
            model
                .research_folders
                .folders
                .extend(imported_folders.iter().cloned());
            for (tree_id, folder_id) in &imported_membership {
                model
                    .research_folders
                    .membership
                    .insert(tree_id.clone(), folder_id.clone());
            }
            Ok(())
        })();
        if let Err(err) = insert_result {
            for node_id in written_node_ids {
                let _ =
                    research::remove_response_snapshot(&self.inner.config.workspace_root, &node_id);
            }
            return Err(err);
        }
        let persist_result = if self.inner.persist_enabled.load(Ordering::Relaxed) {
            self.persist_snapshot_locked()
        } else {
            Ok(())
        };
        if let Err(err) = persist_result {
            if let Ok(mut model) = self.inner.model.lock() {
                model.groups.remove(&workspace.id);
                model.group_order.retain(|id| id != &workspace.id);
                model
                    .research_tree_order
                    .retain(|id| !imported_tree_order.contains(id));
                let imported_folder_ids = imported_folders
                    .iter()
                    .map(|folder| folder.id.as_str())
                    .collect::<HashSet<_>>();
                model
                    .research_folders
                    .folders
                    .retain(|folder| !imported_folder_ids.contains(folder.id.as_str()));
                for (tree_id, _) in &imported_membership {
                    model.research_folders.membership.remove(tree_id);
                }
                for tree in &trees {
                    model.research_trees.remove(&tree.id);
                }
                for node in &nodes {
                    model.research_nodes.remove(&node.id);
                }
            }
            for node_id in written_node_ids {
                let _ =
                    research::remove_response_snapshot(&self.inner.config.workspace_root, &node_id);
            }
            return Err(format!("failed to commit imported research: {err}"));
        }
        self.emit(QmuxEvent::new(
            "group.created",
            None,
            None,
            json!({ "group": workspace.clone() }),
        ));
        Ok(workspace)
    }

    pub fn pane_group_id(&self, pane_id: &str) -> Result<Option<String>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model
            .panes
            .get(pane_id)
            .map(|runtime| runtime.info.group_id.clone()))
    }

    pub fn agent(&self, agent_id: &str) -> Result<Option<AgentInfo>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model.agents.get(agent_id).cloned())
    }

    pub fn agent_by_pane(&self, pane_id: &str) -> Result<Option<AgentInfo>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model
            .agents
            .values()
            .find(|agent| agent.pane_id.as_deref() == Some(pane_id))
            .cloned())
    }

    pub fn insert_pane(&self, pane: PaneRuntime) -> Result<(), String> {
        {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let pane_id = pane.info.id.clone();
            let is_new = !model.panes.contains_key(&pane_id);
            model.panes.insert(pane_id.clone(), pane);
            if is_new && !model.pane_order.iter().any(|id| id == &pane_id) {
                model.pane_order.push(pane_id);
            }
        }
        self.persist();
        Ok(())
    }

    pub fn capture_last_closed_pane(&self, pane_id: &str) -> Result<(), String> {
        let mut snapshot = {
            let model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            // A pane bound to a research node is reclaimed by research lifecycle
            // handling (settlement, cancellation, or failed-launch cleanup), so
            // it must not become a restorable "closed tab". Skipping the
            // capture here, rather than clearing it after kill_pane returns,
            // closes the window in which a restore request could still see it.
            if model
                .research_nodes
                .values()
                .any(|node| node.pane_id.as_deref() == Some(pane_id))
            {
                return Ok(());
            }
            let runtime = model
                .panes
                .get(pane_id)
                .ok_or_else(|| format!("pane {pane_id} was not found"))?;
            let ordered_ids = ordered_pane_ids(&model);
            let index = ordered_ids
                .iter()
                .position(|id| id == pane_id)
                .unwrap_or(ordered_ids.len());

            let mut pane = runtime.info.clone();
            pane.depth = model.pane_depth.get(pane_id).copied().unwrap_or(0);
            let group = model.groups.get(&pane.group_id).cloned();

            let group_pane_count = model
                .panes
                .values()
                .filter(|candidate| candidate.info.group_id == pane.group_id)
                .count();
            let closing_last_group_pane = group_pane_count == 1;
            let pane_agent_id = pane.agent_id.clone();
            let snapshot_agent = |agent: &AgentInfo| {
                let turns = model.turns.get(&agent.id).cloned().unwrap_or_default();
                let queued_turns = model
                    .agent_turn_queues
                    .get(&agent.id)
                    .map(|queue| queue.iter().cloned().collect())
                    .unwrap_or_default();
                let draft = model.agent_drafts.get(&agent.id).cloned();
                ClosedPaneAgentSnapshot {
                    agent: agent.clone(),
                    turns,
                    queued_turns,
                    draft,
                }
            };
            let agent = pane_agent_id
                .as_deref()
                .and_then(|agent_id| model.agents.get(agent_id))
                .or_else(|| {
                    model
                        .agents
                        .values()
                        .find(|agent| agent.pane_id.as_deref() == Some(pane_id))
                })
                .cloned()
                .map(|agent| snapshot_agent(&agent));
            let captured_agent_id = agent
                .as_ref()
                .map(|agent_snapshot| agent_snapshot.agent.id.as_str());
            let orphaned_agents = model
                .agents
                .values()
                .filter(|agent| Some(agent.id.as_str()) != captured_agent_id)
                .filter(|agent| {
                    agent.orphaned_queue_pane_id.as_deref() == Some(pane_id)
                        || (closing_last_group_pane
                            && agent.group_id == pane.group_id
                            && agent.pane_id.is_none())
                })
                // Only agents that still carry a queue are worth preserving across the
                // close: a queue-less one restores with no pane and no orphaned-queue
                // binding (see `restore_closed_pane_metadata`), an invisible, unreachable
                // agent. Such agents are pruned on close and stay resumable via recent
                // sessions instead.
                .filter(|agent| {
                    model
                        .agent_turn_queues
                        .get(&agent.id)
                        .is_some_and(|queue| !queue.is_empty())
                })
                .map(snapshot_agent)
                .collect();

            ClosedPaneSnapshot {
                pane,
                group,
                agent,
                orphaned_agents,
                index,
                scrollback: Vec::new(),
            }
        };

        snapshot.scrollback =
            match read_pane_scrollback(&self.inner.config.workspace_root, &snapshot.pane.id) {
                Ok(bytes) => bounded_undo_scrollback(&bytes, MAX_UNDO_SCROLLBACK_BYTES),
                Err(err) => {
                    eprintln!(
                        "qmux: failed to capture scrollback for closed pane {}: {err}",
                        snapshot.pane.id
                    );
                    Vec::new()
                }
            };

        let mut model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        model.closed_pane_stack.push(snapshot);
        // Drop the oldest closes once past the cap so the stack can't grow unbounded.
        let overflow = model
            .closed_pane_stack
            .len()
            .saturating_sub(MAX_CLOSED_PANE_UNDO);
        if overflow > 0 {
            model.closed_pane_stack.drain(0..overflow);
        }
        Ok(())
    }

    /// Pops the most recently closed pane for undo, or `None` when the stack is empty.
    pub fn take_last_closed_pane(&self) -> Result<Option<ClosedPaneSnapshot>, String> {
        let mut model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model.closed_pane_stack.pop())
    }

    /// Pushes a snapshot back onto the undo stack (used when a restore attempt fails, so
    /// the just-popped close remains reopenable).
    pub fn remember_last_closed_pane(&self, snapshot: ClosedPaneSnapshot) -> Result<(), String> {
        let mut model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        model.closed_pane_stack.push(snapshot);
        Ok(())
    }

    /// Drops any undo entry for `pane_id` — used when a close is aborted, so a stale
    /// snapshot can't be reopened. Pane ids are unique per run, so this matches at most
    /// one entry.
    pub fn clear_last_closed_pane_for_pane(&self, pane_id: &str) {
        if let Ok(mut model) = self.inner.model.lock() {
            model
                .closed_pane_stack
                .retain(|snapshot| snapshot.pane.id != pane_id);
        }
    }

    /// Drops any undo entry whose captured agent is `agent_id` — used when the agent is
    /// permanently gone, so its snapshot isn't offered for reopen.
    pub fn clear_last_closed_pane_for_agent(&self, agent_id: &str) {
        if let Ok(mut model) = self.inner.model.lock() {
            model.closed_pane_stack.retain(|snapshot| {
                snapshot
                    .agent
                    .as_ref()
                    .is_none_or(|agent_snapshot| agent_snapshot.agent.id != agent_id)
            });
        }
    }

    pub fn restore_closed_pane_metadata(
        &self,
        snapshot: &ClosedPaneSnapshot,
    ) -> Result<(), String> {
        if matches!(snapshot.pane.kind, PaneKind::Agent) && snapshot.agent.is_none() {
            return Err(format!(
                "closed agent pane {} is missing its agent",
                snapshot.pane.id
            ));
        }

        {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if !model.groups.contains_key(&snapshot.pane.group_id)
                && let Some(group) = snapshot.group.clone()
            {
                let group_id = group.id.clone();
                model.groups.insert(group_id.clone(), group);
                if !model.group_order.iter().any(|id| id == &group_id) {
                    model.group_order.push(group_id);
                }
            }
            model.shell_agent_resumes.remove(&snapshot.pane.id);

            if let Some(agent_snapshot) = &snapshot.agent {
                restore_closed_agent_snapshot_locked(
                    &mut model,
                    &snapshot.pane,
                    agent_snapshot,
                    matches!(snapshot.pane.kind, PaneKind::Agent),
                    matches!(snapshot.pane.kind, PaneKind::Shell),
                );
            }
            for agent_snapshot in &snapshot.orphaned_agents {
                restore_closed_agent_snapshot_locked(
                    &mut model,
                    &snapshot.pane,
                    agent_snapshot,
                    false,
                    false,
                );
            }
        }
        self.persist();
        Ok(())
    }

    pub fn place_restored_pane(
        &self,
        pane_id: &str,
        index: usize,
        depth: u16,
    ) -> Result<Vec<PaneInfo>, String> {
        let panes = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if !model.panes.contains_key(pane_id) {
                return Err(format!("pane {pane_id} was not found"));
            }

            let mut ids = ordered_pane_ids(&model);
            ids.retain(|id| id != pane_id);
            ids.insert(index.min(ids.len()), pane_id.to_string());
            model.pane_order = ids;
            if depth == 0 {
                model.pane_depth.remove(pane_id);
            } else {
                model.pane_depth.insert(pane_id.to_string(), depth);
            }
            normalize_pane_depths(&mut model);
            normalize_pane_splits_locked(&mut model);
            ordered_panes(&model)
        };
        self.persist();
        Ok(panes)
    }

    /// True when `pane_id` is the only remaining pane in its group and that group still
    /// owns an agent with queued turns. Removing such a pane prunes the group's agents
    /// (closing the group with it), so a caller that does not first capture a close
    /// snapshot — the natural PTY-exit path, unlike `kill_pane` — would discard that
    /// pending work irrecoverably. Used to decide whether to snapshot before removal.
    pub fn closing_pane_would_strand_queued_work(&self, pane_id: &str) -> Result<bool, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        let Some(group_id) = model
            .panes
            .get(pane_id)
            .map(|pane| pane.info.group_id.clone())
        else {
            return Ok(false);
        };
        let is_last_pane = !model
            .panes
            .values()
            .any(|other| other.info.id != pane_id && other.info.group_id == group_id);
        if !is_last_pane {
            return Ok(false);
        }
        let has_queued_work = model.agents.values().any(|agent| {
            agent.group_id == group_id
                && model
                    .agent_turn_queues
                    .get(&agent.id)
                    .is_some_and(|queue| !queue.is_empty())
        });
        Ok(has_queued_work)
    }

    pub fn remove_pane(&self, pane_id: &str) -> Result<(), String> {
        // The bound agent's identity and status at the moment the pane went
        // away, captured before the pruning below rewrites or removes the
        // record. The research detach at the end of this function needs it to
        // tell a finished run (process exits at end of turn) from a crashed
        // one; reading the model there is too late.
        let mut departing_agent: Option<(String, AgentStatus, bool)> = None;
        let removed_group_id = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let removed_group_id = model
                .panes
                .get(pane_id)
                .map(|pane| pane.info.group_id.clone());
            model.panes.remove(pane_id);
            model.pane_order.retain(|id| id != pane_id);
            model.research_retiring_panes.remove(pane_id);

            // The pane is gone for good (kill or PTY EOF — never a respawn), so reclaim
            // the agent it owned and its per-agent state, which would otherwise live for
            // the rest of the process. Always drop the purely-runtime tracking; if the
            // agent has no queued turns, drop it entirely (its transcript tail then
            // self-stops, since `tail_should_continue` is false once the agent is gone).
            // An agent with queued turns is kept so its queue stays restart-recoverable
            // via the orphaned-queue panel.
            if let Some(agent_id) = model
                .agents
                .values()
                .find(|agent| agent.pane_id.as_deref() == Some(pane_id))
                .map(|agent| agent.id.clone())
            {
                if let Some(agent) = model.agents.get(&agent_id).cloned() {
                    let has_active_subagents = model
                        .agent_active_subagents
                        .get(&agent.id)
                        .is_some_and(|active| !active.is_empty());
                    departing_agent = Some((agent.id.clone(), agent.status, has_active_subagents));
                    upsert_recent_session_for_agent_locked(
                        &mut model,
                        &agent,
                        now_millis(),
                        true,
                        RecentSessionMeta::CacheOnly,
                    );
                }
                clear_recent_session_binding_locked(&mut model, Some(&agent_id), Some(pane_id));
                model.agent_typing.remove(&agent_id);
                model.agent_pending_pause.remove(&agent_id);
                model.agent_draining.remove(&agent_id);
                model.agent_send_tracking.remove(&agent_id);
                model.agent_activity.remove(&agent_id);
                model.agent_status_activity.remove(&agent_id);
                model.agent_active_subagents.remove(&agent_id);
                model
                    .agents_with_reported_background_tasks
                    .remove(&agent_id);
                model.agent_escape_watch.remove(&agent_id);
                model
                    .agent_submit_watch
                    .retain(|(watched_agent, _)| watched_agent != &agent_id);
                // A turn claimed for delivery but not yet settled when the pane goes
                // away: roll it back to the front of the queue so it isn't lost (and so
                // the has_queue check below keeps the agent for restart recovery).
                if let Some(turn) = model.agent_inflight.remove(&agent_id) {
                    model
                        .agent_turn_queues
                        .entry(agent_id.clone())
                        .or_default()
                        .push_front(turn);
                }
                let has_queue = model
                    .agent_turn_queues
                    .get(&agent_id)
                    .is_some_and(|queue| !queue.is_empty());
                if !has_queue {
                    model.agents.remove(&agent_id);
                    model.turns.remove(&agent_id);
                    model.agent_drafts.remove(&agent_id);
                    model.agent_turn_queues.remove(&agent_id);
                } else {
                    // Kept for restart recovery via the orphaned-queue panel. Park it
                    // the same way `detach_pane_agent` and
                    // `restore_closed_agent_snapshot_locked` do: detach from the
                    // now-removed pane and mark idle. Leaving `pane_id` pointing at the
                    // dead pane (and status Running) both misrepresents the agent to the
                    // panel/recovery and keeps its transcript tail polling the
                    // now-static/deleted file for the rest of the process — the tail
                    // stops once the agent is gone, rotates its transcript, or (now) is
                    // parked like this.
                    //
                    // Bind the orphaned queue to a still-open pane in the same group when
                    // one exists, so it stays visible in that group's recovered-queue
                    // panel. Binding it to the just-closed (dead) pane id — as before —
                    // left it matching no live surface while siblings stayed open, so it
                    // silently vanished from the UI. When this was the group's last pane,
                    // keep the dead id: the queue is then captured into the closed-pane
                    // undo snapshot and re-homed on restore/restart.
                    let surviving_sibling = removed_group_id.as_deref().and_then(|group_id| {
                        model
                            .panes
                            .values()
                            .find(|pane| pane.info.group_id == group_id)
                            .map(|pane| pane.info.id.clone())
                    });
                    if let Some(agent) = model.agents.get_mut(&agent_id) {
                        agent.pane_id = None;
                        agent.orphaned_queue_pane_id =
                            Some(surviving_sibling.unwrap_or_else(|| pane_id.to_string()));
                        agent.status = AgentStatus::Idle;
                    }
                }
            }

            // Re-level any children orphaned by the removal so the tree stays valid
            // (a closed parent must not leave its children at an unreachable depth).
            normalize_pane_depths(&mut model);
            normalize_pane_splits_locked(&mut model);
            removed_group_id.filter(|group_id| {
                remove_group_without_open_panes_locked(&mut model, group_id, true)
            })
        };
        // The pane's control-socket token is captured by its in-pane process as
        // QMUX_TOKEN; once the pane is gone for good it can never legitimately be used
        // again, so drop it rather than leave a live credential resolving (via
        // `pane_for_token`) to a pane that no longer exists. Separate lock from `model`.
        if let Ok(mut tokens) = self.inner.pane_tokens.lock() {
            tokens.remove(pane_id);
        }
        // The pane's file-server token can never be used again once the pane is gone
        // (it resolves only via `pane_for_file_token`), so reclaim it rather than let
        // a live credential outlive the pane it scopes. Separate lock from `model`.
        if let Ok(mut tokens) = self.inner.file_tokens.lock() {
            tokens.remove(pane_id);
        }
        // Drop the pane's send lock so the map doesn't grow for the process lifetime.
        // Separate lock from `model`.
        if let Ok(mut locks) = self.inner.pane_send_locks.lock() {
            locks.remove(pane_id);
        }
        for info in self.unregister_shell_agent_jobs_for_pane(pane_id) {
            crate::shell_jobs::emit_job_removed(self, &info);
        }
        if let Err(err) = self.detach_research_pane_inner(
            pane_id,
            departing_agent
                .as_ref()
                .map(|(agent_id, status, active)| (agent_id.as_str(), *status, *active)),
        ) {
            eprintln!("qmux: failed to detach research pane {pane_id}: {err}");
        }
        if !self.inner.exit_teardown_started.load(Ordering::SeqCst)
            && let Err(err) = remove_pane_scrollback(&self.inner.config.workspace_root, pane_id)
        {
            eprintln!("qmux: failed to remove scrollback for pane {pane_id}: {err}");
        }
        self.persist();
        self.emit(QmuxEvent::pane_removed(pane_id.to_string()));
        if let Some(group_id) = removed_group_id {
            self.emit(QmuxEvent::new(
                "group.removed",
                None,
                None,
                json!({ "groupId": group_id }),
            ));
        }
        Ok(())
    }

    pub fn reorder_panes(&self, pane_ids: Vec<String>) -> Result<Vec<PaneInfo>, String> {
        let panes = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if pane_ids.len() != model.panes.len() {
                return Err("pane order is stale; refresh before reordering".to_string());
            }

            let mut seen = HashSet::with_capacity(pane_ids.len());
            for pane_id in &pane_ids {
                if !seen.insert(pane_id.clone()) {
                    return Err("pane order contains a duplicate pane".to_string());
                }
                if !model.panes.contains_key(pane_id) {
                    return Err(format!("pane {pane_id} was not found"));
                }
            }

            model.pane_order = pane_ids;
            // A bare reorder can move a nested pane to a position its depth no longer
            // fits (e.g. a child to the top), so re-level depths to stay a valid tree.
            normalize_pane_depths(&mut model);
            normalize_pane_splits_locked(&mut model);
            ordered_panes(&model)
        };
        self.persist();
        Ok(panes)
    }

    /// Atomically replaces the full sidebar tab tree: order + nesting depth. The
    /// `layout` must list exactly the current panes (no missing/duplicate/unknown id)
    /// and form a valid tree (first depth 0; each depth <= previous + 1; capped at
    /// `MAX_PANE_DEPTH`). Every structural tab operation — reorder, indent, outdent,
    /// nest — is expressed as one of these layouts so a multi-pane indent applies in
    /// a single locked mutation.
    pub fn set_pane_layout(&self, layout: Vec<PaneLayoutEntry>) -> Result<Vec<PaneInfo>, String> {
        let panes = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if layout.len() != model.panes.len() {
                return Err("pane layout is stale; refresh before updating".to_string());
            }

            let mut seen = HashSet::with_capacity(layout.len());
            let mut prev_depth_by_group: HashMap<String, u16> = HashMap::new();
            for entry in &layout {
                if !seen.insert(entry.pane_id.clone()) {
                    return Err("pane layout contains a duplicate pane".to_string());
                }
                let Some(pane) = model.panes.get(&entry.pane_id) else {
                    return Err(format!("pane {} was not found", entry.pane_id));
                };
                if entry.depth > MAX_PANE_DEPTH {
                    return Err(format!(
                        "pane depth {} exceeds the maximum of {MAX_PANE_DEPTH}",
                        entry.depth
                    ));
                }
                let group_id = pane.info.group_id.clone();
                let ceiling = prev_depth_by_group
                    .get(&group_id)
                    .map_or(0, |prev| prev + 1);
                if entry.depth > ceiling {
                    return Err(
                        "pane layout is not a valid tree (a depth skips a level)".to_string()
                    );
                }
                prev_depth_by_group.insert(group_id, entry.depth);
            }

            model.pane_order = layout.iter().map(|entry| entry.pane_id.clone()).collect();
            model.pane_depth = layout
                .iter()
                .filter(|entry| entry.depth != 0)
                .map(|entry| (entry.pane_id.clone(), entry.depth))
                .collect();
            normalize_pane_splits_locked(&mut model);
            ordered_panes(&model)
        };
        self.persist();
        Ok(panes)
    }

    /// Moves a plain shell tab (and its nested subtree) into another terminal group,
    /// applying `layout` as the resulting full tab tree (order + depth) in the same
    /// locked mutation. Agent tabs — or subtrees containing one — are rejected: an
    /// agent's worktree, branch, and queue bookkeeping are bound to its group, and
    /// this move deliberately doesn't touch them. When the move empties the source
    /// group it is removed, mirroring the close-last-pane path.
    pub fn move_pane_to_group(
        &self,
        pane_id: &str,
        target_group_id: &str,
        layout: Vec<PaneLayoutEntry>,
    ) -> Result<Vec<PaneInfo>, String> {
        let (panes, removed_source_group_id) = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let source_group_id = model
                .panes
                .get(pane_id)
                .map(|pane| pane.info.group_id.clone())
                .ok_or_else(|| format!("pane {pane_id} was not found"))?;
            if source_group_id == target_group_id {
                return Err(format!(
                    "pane {pane_id} is already in group {target_group_id}"
                ));
            }
            let source_scope = model.groups.get(&source_group_id).map(|group| group.scope);
            let target_scope = model
                .groups
                .get(target_group_id)
                .map(|group| group.scope)
                .ok_or_else(|| format!("group {target_group_id} was not found"))?;
            if source_scope != Some(WorkspaceScope::Terminal)
                || target_scope != WorkspaceScope::Terminal
            {
                return Err("tabs can only move between terminal groups".to_string());
            }

            // The moved unit is the pane's whole sidebar subtree: the contiguous run
            // of following same-group panes nested deeper than it.
            let group_panes = ordered_panes(&model)
                .into_iter()
                .filter(|candidate| candidate.group_id == source_group_id)
                .collect::<Vec<_>>();
            let start = group_panes
                .iter()
                .position(|candidate| candidate.id == pane_id)
                .ok_or_else(|| format!("pane {pane_id} was not found"))?;
            let root_depth = group_panes[start].depth;
            let moved_ids = std::iter::once(&group_panes[start])
                .chain(
                    group_panes[start + 1..]
                        .iter()
                        .take_while(|candidate| candidate.depth > root_depth),
                )
                .map(|candidate| candidate.id.clone())
                .collect::<HashSet<_>>();
            for moved_id in &moved_ids {
                let moved = model
                    .panes
                    .get(moved_id)
                    .ok_or_else(|| format!("pane {moved_id} was not found"))?;
                if !matches!(moved.info.kind, PaneKind::Shell) || moved.info.agent_id.is_some() {
                    return Err("agent tabs can't move to another group".to_string());
                }
            }

            // Same validation as `set_pane_layout`, with the moved subtree counted
            // against its prospective group.
            if layout.len() != model.panes.len() {
                return Err("pane layout is stale; refresh before updating".to_string());
            }
            let mut seen = HashSet::with_capacity(layout.len());
            let mut prev_depth_by_group: HashMap<String, u16> = HashMap::new();
            for entry in &layout {
                if !seen.insert(entry.pane_id.clone()) {
                    return Err("pane layout contains a duplicate pane".to_string());
                }
                let Some(pane) = model.panes.get(&entry.pane_id) else {
                    return Err(format!("pane {} was not found", entry.pane_id));
                };
                if entry.depth > MAX_PANE_DEPTH {
                    return Err(format!(
                        "pane depth {} exceeds the maximum of {MAX_PANE_DEPTH}",
                        entry.depth
                    ));
                }
                let group_id = if moved_ids.contains(&entry.pane_id) {
                    target_group_id.to_string()
                } else {
                    pane.info.group_id.clone()
                };
                let ceiling = prev_depth_by_group
                    .get(&group_id)
                    .map_or(0, |prev| prev + 1);
                if entry.depth > ceiling {
                    return Err(
                        "pane layout is not a valid tree (a depth skips a level)".to_string()
                    );
                }
                prev_depth_by_group.insert(group_id, entry.depth);
            }

            for moved_id in &moved_ids {
                if let Some(moved) = model.panes.get_mut(moved_id) {
                    moved.info.group_id = target_group_id.to_string();
                }
            }
            model.pane_order = layout.iter().map(|entry| entry.pane_id.clone()).collect();
            model.pane_depth = layout
                .iter()
                .filter(|entry| entry.depth != 0)
                .map(|entry| (entry.pane_id.clone(), entry.depth))
                .collect();
            // A moved pane's split memberships can't survive the group change; the
            // normalizer drops it from any split it belonged to.
            normalize_pane_splits_locked(&mut model);
            let removed_source_group_id =
                remove_group_without_open_panes_locked(&mut model, &source_group_id, true)
                    .then_some(source_group_id);
            (ordered_panes(&model), removed_source_group_id)
        };
        self.persist();
        if let Some(group_id) = removed_source_group_id {
            self.emit(QmuxEvent::new(
                "group.removed",
                None,
                None,
                json!({ "groupId": group_id }),
            ));
        }
        Ok(panes)
    }

    /// Moves `pane_id` to sit immediately after `parent_pane_id` at one level deeper
    /// (its first child), then re-levels the tree. Used when a fork should appear
    /// nested under the session it forked from.
    pub fn nest_pane_under(
        &self,
        pane_id: &str,
        parent_pane_id: &str,
    ) -> Result<Vec<PaneInfo>, String> {
        let panes = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if !model.panes.contains_key(pane_id) {
                return Err(format!("pane {pane_id} was not found"));
            }
            if !model.panes.contains_key(parent_pane_id) {
                return Err(format!("pane {parent_pane_id} was not found"));
            }

            let mut ids = ordered_pane_ids(&model);
            ids.retain(|id| id != pane_id);
            let parent_index = ids
                .iter()
                .position(|id| id == parent_pane_id)
                .ok_or_else(|| format!("pane {parent_pane_id} was not found"))?;
            ids.insert(parent_index + 1, pane_id.to_string());

            let parent_depth = model.pane_depth.get(parent_pane_id).copied().unwrap_or(0);
            let new_depth = (parent_depth + 1).min(MAX_PANE_DEPTH);
            model.pane_order = ids;
            model.pane_depth.insert(pane_id.to_string(), new_depth);
            normalize_pane_depths(&mut model);
            normalize_pane_splits_locked(&mut model);
            ordered_panes(&model)
        };
        self.persist();
        Ok(panes)
    }

    /// Moves `pane_id` to sit immediately after `sibling_pane_id` at the same depth
    /// (a sibling, not a child), then re-levels the tree. Used when a fork should
    /// appear as a new tab right after the session it forked from.
    pub fn place_pane_after(
        &self,
        pane_id: &str,
        sibling_pane_id: &str,
    ) -> Result<Vec<PaneInfo>, String> {
        let panes = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if !model.panes.contains_key(pane_id) {
                return Err(format!("pane {pane_id} was not found"));
            }
            if !model.panes.contains_key(sibling_pane_id) {
                return Err(format!("pane {sibling_pane_id} was not found"));
            }

            let mut ids = ordered_pane_ids(&model);
            ids.retain(|id| id != pane_id);
            let sibling_index = ids
                .iter()
                .position(|id| id == sibling_pane_id)
                .ok_or_else(|| format!("pane {sibling_pane_id} was not found"))?;
            ids.insert(sibling_index + 1, pane_id.to_string());

            let sibling_depth = model.pane_depth.get(sibling_pane_id).copied().unwrap_or(0);
            model.pane_order = ids;
            model.pane_depth.insert(pane_id.to_string(), sibling_depth);
            normalize_pane_depths(&mut model);
            normalize_pane_splits_locked(&mut model);
            ordered_panes(&model)
        };
        self.persist();
        Ok(panes)
    }

    /// Clamps persisted nesting depths to a valid tree over the panes that actually
    /// exist. Called once after session restore/respawn, since some persisted panes
    /// (already-exited ones) are intentionally not recreated.
    pub fn normalize_pane_layout(&self) {
        {
            let Ok(mut model) = self.inner.model.lock() else {
                return;
            };
            normalize_pane_depths(&mut model);
            normalize_pane_splits_locked(&mut model);
        }
        self.persist();
    }

    pub fn insert_group_after(
        &self,
        group: GroupInfo,
        after_group_id: Option<&str>,
    ) -> Result<(), String> {
        {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let group_id = group.id.clone();
            let is_new = !model.groups.contains_key(&group_id);
            model.groups.insert(group_id.clone(), group);
            if is_new {
                model.group_order.retain(|id| id != &group_id);
                if let Some(after_group_id) = after_group_id
                    && let Some(index) =
                        model.group_order.iter().position(|id| id == after_group_id)
                {
                    model.group_order.insert(index + 1, group_id);
                } else {
                    model.group_order.push(group_id);
                }
            }
        }
        self.persist();
        Ok(())
    }

    /// Upserts an agent's recent-session entry, filling the preview/line-count
    /// from the transcript file when the cache has neither — with the disk read
    /// done *between* two short model-lock sections, never under one. Returns
    /// whether the stored entry changed. Best-effort bookkeeping: a poisoned
    /// model lock skips the upsert rather than propagating.
    fn upsert_recent_session_for_agent(&self, agent: &AgentInfo, now: u128, touch: bool) -> bool {
        let first = match self.inner.model.lock() {
            Ok(mut model) => upsert_recent_session_for_agent_locked(
                &mut model,
                agent,
                now,
                touch,
                RecentSessionMeta::CacheOnly,
            ),
            Err(_) => return false,
        };
        let mut changed = first.changed;
        if let Some(path) = first.wants_disk_meta {
            let (preview, line_count) =
                crate::transcript::read_transcript_meta(std::path::Path::new(&path));
            if (preview.is_some() || line_count > 0)
                && let Ok(mut model) = self.inner.model.lock()
            {
                changed |= upsert_recent_session_for_agent_locked(
                    &mut model,
                    agent,
                    now,
                    touch,
                    RecentSessionMeta::Loaded {
                        preview,
                        line_count,
                    },
                )
                .changed;
            }
        }
        if changed && let Ok(mut model) = self.inner.model.lock() {
            // The prune only matters after an insert grew the map; unchanged
            // upserts skip the sort-and-clone entirely.
            prune_recent_sessions_locked(&mut model);
        }
        changed
    }

    pub fn insert_agent(&self, mut agent: AgentInfo) -> Result<(), String> {
        let agent_for_sessions = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            ensure_agent_thread_metadata(self, &mut model, &mut agent);
            let agent_for_sessions = agent.clone();
            model.agents.insert(agent.id.clone(), agent);
            agent_for_sessions
        };
        self.upsert_recent_session_for_agent(&agent_for_sessions, now_millis(), true);
        self.persist();
        Ok(())
    }

    pub fn update_group(&self, group: GroupInfo) -> Result<(), String> {
        {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if !model.group_order.iter().any(|id| id == &group.id) {
                model.group_order.push(group.id.clone());
            }
            model.groups.insert(group.id.clone(), group);
        }
        self.persist();
        Ok(())
    }

    pub fn remove_group(&self, group_id: &str) -> Result<(), String> {
        let removed = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if model
                .panes
                .values()
                .any(|pane| pane.info.group_id == group_id)
            {
                return Err("group still has open panes".to_string());
            }
            if model
                .research_trees
                .values()
                .any(|tree| tree.workspace_id == group_id)
            {
                return Err("group is retained by a research tree".to_string());
            }
            remove_group_without_open_panes_locked(&mut model, group_id, false)
        };
        if removed {
            self.persist();
            self.emit(QmuxEvent::new(
                "group.removed",
                None,
                None,
                json!({ "groupId": group_id }),
            ));
        }
        Ok(())
    }

    pub fn update_agent(&self, mut agent: AgentInfo) -> Result<(), String> {
        let agent_for_sessions = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            ensure_agent_thread_metadata(self, &mut model, &mut agent);
            bump_agent_activity_locked(&mut model, &agent.id);
            let agent_for_sessions = agent.clone();
            model.agents.insert(agent.id.clone(), agent);
            agent_for_sessions
        };
        self.upsert_recent_session_for_agent(&agent_for_sessions, now_millis(), true);
        self.sync_research_node_from_agent(&agent_for_sessions)?;
        self.persist();
        Ok(())
    }

    /// Mutates an agent in place under the lock, applying `f` to the live entry and
    /// leaving every field `f` doesn't touch exactly as it stands. Unlike `update_agent`
    /// (which inserts a whole struct snapshot the caller read earlier, outside the lock),
    /// this can't clobber a field a concurrent writer set in the meantime — e.g. the
    /// `session_id` / `transcript_path` a freshly spawned agent's SessionStart hook
    /// records on another thread while `attach_agent_pane` is binding its pane. Returns
    /// the updated agent, or `None` if it no longer exists.
    pub fn mutate_agent<F>(&self, agent_id: &str, f: F) -> Result<Option<AgentInfo>, String>
    where
        F: FnOnce(&mut AgentInfo),
    {
        let updated = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            match model.agents.get_mut(agent_id) {
                Some(agent) => {
                    f(agent);
                    let updated = agent.clone();
                    bump_agent_activity_locked(&mut model, agent_id);
                    Some(updated)
                }
                None => None,
            }
        };
        if let Some(agent) = updated.as_ref() {
            self.upsert_recent_session_for_agent(agent, now_millis(), true);
            self.sync_research_node_from_agent(agent)?;
            self.persist();
        }
        Ok(updated)
    }

    /// Reserves the Esc-interrupt grace watch for an agent, returning `true` when the
    /// caller should spawn the watcher and `false` when one is already in flight (so a
    /// held-Esc burst spawns a single thread). Best-effort: a poisoned lock returns
    /// `false`, skipping the watch rather than racing.
    pub fn begin_agent_escape_watch(&self, agent_id: &str) -> bool {
        let Ok(mut model) = self.inner.model.lock() else {
            return false;
        };
        model.agent_escape_watch.insert(agent_id.to_string())
    }

    /// Clears the Esc-interrupt grace watch reservation once the watcher thread
    /// resolves. Best-effort: a poisoned lock just leaves the entry, which only costs
    /// the next Esc burst its watch until the agent is next removed.
    pub fn end_agent_escape_watch(&self, agent_id: &str) {
        if let Ok(mut model) = self.inner.model.lock() {
            model.agent_escape_watch.remove(agent_id);
        }
    }

    /// Reserves the submit-confirmation watch for one exact send, returning `true` when
    /// the caller should spawn the watcher and `false` when that send is already being
    /// watched. Best-effort: a poisoned lock returns `false`, skipping the watch rather
    /// than racing.
    pub fn begin_agent_submit_watch(&self, agent_id: &str, send_id: u64) -> bool {
        let Ok(mut model) = self.inner.model.lock() else {
            return false;
        };
        model
            .agent_submit_watch
            .insert((agent_id.to_string(), send_id))
    }

    /// Clears a submit-confirmation watch reservation once its watcher thread
    /// resolves. Best-effort: a poisoned lock just leaves the entry, which only costs
    /// a re-arm of that exact send its watch until the agent is next removed.
    pub fn end_agent_submit_watch(&self, agent_id: &str, send_id: u64) {
        if let Ok(mut model) = self.inner.model.lock() {
            model
                .agent_submit_watch
                .remove(&(agent_id.to_string(), send_id));
        }
    }

    /// Field-scoped status write — a thin wrapper over [`AppState::mutate_agent`] that
    /// touches only `status`. Returns the updated agent, or `None` if it no longer
    /// exists.
    pub fn set_agent_status(
        &self,
        agent_id: &str,
        status: AgentStatus,
    ) -> Result<Option<AgentInfo>, String> {
        let (updated, status_changed) = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            match model.agents.get_mut(agent_id) {
                Some(agent) => {
                    // Hooks re-assert the current status several times a second
                    // for a busy agent (PreToolUse/PostToolUse both map to
                    // Running). Only a real transition — or a material
                    // recent-session change — marks the state file dirty, so a
                    // streaming agent no longer keeps the debounced persister
                    // rewriting state.json for its whole run. The in-memory
                    // activity bumps still happen on every call; they feed the
                    // escape/idle watchers, not persistence.
                    let status_changed = agent.status != status;
                    agent.status = status;
                    let updated = agent.clone();
                    bump_agent_activity_locked(&mut model, agent_id);
                    bump_agent_status_activity_locked(&mut model, agent_id);
                    (Some(updated), status_changed)
                }
                None => (None, false),
            }
        };
        let (research_changed, session_changed) = match updated.as_ref() {
            Some(agent) => {
                let research_changed = self.sync_research_node_from_agent(agent)?;
                let session_changed =
                    self.upsert_recent_session_for_agent(agent, now_millis(), true);
                (research_changed, session_changed)
            }
            None => (false, false),
        };
        if status_changed || session_changed || research_changed {
            self.persist();
        }
        Ok(updated)
    }

    /// Records a background subagent starting under `agent_id`. The lifecycle
    /// bump also invalidates a delayed parent-Stop resolver that raced this hook.
    pub fn agent_subagent_started(
        &self,
        agent_id: &str,
        subagent_id: Option<&str>,
    ) -> Result<usize, String> {
        let mut model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        let active = model
            .agent_active_subagents
            .entry(agent_id.to_string())
            .or_default();
        match subagent_id.map(str::trim).filter(|id| !id.is_empty()) {
            Some(id) => {
                active.identified.insert(id.to_string());
            }
            None => active.anonymous = active.anonymous.saturating_add(1),
        }
        let count = active.count();
        bump_agent_activity_locked(&mut model, agent_id);
        bump_agent_status_activity_locked(&mut model, agent_id);
        Ok(count)
    }

    /// Records one background subagent settling. Reaching zero does not finish
    /// the parent: it still needs a synthesis turn and a later parent Stop.
    ///
    /// Returns `Some(remaining)` when the stop matched tracked work, `None` for
    /// a stop with nothing tracked (late, duplicate, or never-started) so
    /// callers can leave the parent's status alone. Start/stop id asymmetry —
    /// one side of the pair carrying an id the other lacks — still settles one
    /// tracked subagent rather than leaving the counter wedged above zero.
    pub fn agent_subagent_stopped(
        &self,
        agent_id: &str,
        subagent_id: Option<&str>,
    ) -> Result<Option<usize>, String> {
        let mut model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        let remaining = match model.agent_active_subagents.get_mut(agent_id) {
            Some(active) => {
                match subagent_id.map(str::trim).filter(|id| !id.is_empty()) {
                    Some(id) => {
                        if !active.identified.remove(id) {
                            active.anonymous = active.anonymous.saturating_sub(1);
                        }
                    }
                    None => {
                        if active.anonymous > 0 {
                            active.anonymous -= 1;
                        } else if let Some(any) = active.identified.iter().next().cloned() {
                            // An anonymous stop still means one subagent settled;
                            // which tracked id it was is unknowable, so retire any.
                            active.identified.remove(&any);
                        }
                    }
                }
                Some(active.count())
            }
            None => None,
        };
        if remaining.is_none_or(|remaining| remaining == 0) {
            model.agent_active_subagents.remove(agent_id);
        }
        bump_agent_activity_locked(&mut model, agent_id);
        bump_agent_status_activity_locked(&mut model, agent_id);
        Ok(remaining)
    }

    pub fn agent_has_active_subagents(&self, agent_id: &str) -> Result<bool, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model
            .agent_active_subagents
            .get(agent_id)
            .is_some_and(|active| !active.is_empty()))
    }

    pub fn clear_agent_subagents(&self, agent_id: &str) {
        if let Ok(mut model) = self.inner.model.lock() {
            model.agent_active_subagents.remove(agent_id);
        }
    }

    /// Records whether the agent's most recent Stop reported still-running
    /// background tasks, so the idle-prompt boundary can honor the same wait
    /// the Stop handler established (see the field's doc for lifecycle).
    pub fn set_agent_background_tasks_reported(
        &self,
        agent_id: &str,
        reported: bool,
    ) -> Result<(), String> {
        let mut model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        if reported {
            model
                .agents_with_reported_background_tasks
                .insert(agent_id.to_string());
        } else {
            model.agents_with_reported_background_tasks.remove(agent_id);
        }
        Ok(())
    }

    pub fn agent_has_reported_background_tasks(&self, agent_id: &str) -> Result<bool, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model
            .agents_with_reported_background_tasks
            .contains(agent_id))
    }

    fn sync_research_node_from_agent(&self, agent: &AgentInfo) -> Result<bool, String> {
        let updated = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let node_id = model
                .research_nodes
                .values()
                .find(|node| node.agent_id.as_deref() == Some(&agent.id))
                .map(|node| node.id.clone());
            let Some(node_id) = node_id else {
                return Ok(false);
            };
            let (node_prompt, existing_prompt_id) = model
                .research_nodes
                .get(&node_id)
                .map(|node| (node.prompt.clone(), node.prompt_native_id.clone()))
                .expect("node exists");
            let ancestor_prompts = model
                .research_nodes
                .get(&node_id)
                .map(|node| research::ancestor_prompts(node, |id| model.research_nodes.get(id)))
                .unwrap_or_default();
            let (prompt_id, preview) = model.turns.get(&agent.id).map_or((None, None), |turns| {
                let prompt_id = research::prompt_native_id(turns, &node_prompt);
                let preview = research::response_preview(
                    turns,
                    prompt_id.as_deref().or(existing_prompt_id.as_deref()),
                    &node_prompt,
                    &ancestor_prompts,
                );
                (prompt_id, preview)
            });
            let has_active_subagents = model
                .agent_active_subagents
                .get(&agent.id)
                .is_some_and(|active| !active.is_empty());
            let now = now_millis();
            let node = model.research_nodes.get_mut(&node_id).expect("node exists");
            let before = node.clone();
            node.native_session_id = agent.session_id.clone();
            node.transcript_path = agent.transcript_path.clone();
            if agent.thread_id.is_some() {
                node.thread_id = agent.thread_id.clone();
            }
            // A sync is built from an agent snapshot taken under a previously
            // released lock, so it can land after pane teardown already ran
            // detach_research_pane. Rewriting pane_id would re-bind the dead
            // pane to a settled node — a state nothing clears until restart,
            // and one that pins the tree (archive/remove/folder ops treat a
            // bound pane as an active run). Terminal nodes keep whatever
            // binding teardown left them; the checkpoint fields above still
            // flow, since the native session id and transcript path trail the
            // Complete status by design.
            if !node.status.is_terminal() {
                node.pane_id = agent.pane_id.clone();
            }
            if prompt_id.is_some() {
                node.prompt_native_id = prompt_id;
            }
            if preview.is_some() {
                node.response_preview = preview;
            }
            // Hooks and transcript tailing deliver agent events asynchronously,
            // so a generic Running/Idle update can arrive after the run has
            // settled — most visibly after a user cancellation, where rewriting
            // the status would resurrect the run and let the pane teardown
            // re-settle it as Failed. Terminal outcomes stay as written.
            if !node.status.is_terminal() {
                node.status = research_status_for_agent(agent.status, has_active_subagents);
                if node.status.is_terminal() && node.completed_at.is_none() {
                    node.completed_at = Some(now);
                }
            }
            let changed = *node != before;
            // Recency (and with it the sidebar sort) moves only on lifecycle
            // transitions. Preview/session churn arrives several times a
            // second while streaming, and bumping updated_at for each made
            // concurrently-running trees swap positions under the cursor.
            let lifecycle_changed =
                node.status != before.status || node.completed_at != before.completed_at;
            let node = node.clone();
            if lifecycle_changed {
                touch_research_tree_locked(&mut model, &node.tree_id, now);
            }
            changed.then_some(node)
        };
        let changed = updated.is_some();
        if let Some(node) = updated {
            self.maybe_schedule_research_retirement(&node);
            self.emit(QmuxEvent::new(
                "research.node.updated",
                node.pane_id.clone(),
                node.agent_id.clone(),
                json!({ "node": node }),
            ));
        }
        Ok(changed)
    }

    fn maybe_schedule_research_retirement(&self, node: &ResearchNode) {
        let Some(pane_id) = node.pane_id.clone() else {
            return;
        };
        if !matches!(
            node.status,
            ResearchNodeStatus::Complete | ResearchNodeStatus::Failed
        ) {
            return;
        }
        if node.status == ResearchNodeStatus::Complete
            && node
                .agent_id
                .as_deref()
                .is_some_and(|agent_id| self.agent_has_active_subagents(agent_id).unwrap_or(false))
        {
            return;
        }
        let scheduled = self
            .inner
            .model
            .lock()
            .map(|mut model| {
                model.panes.contains_key(&pane_id)
                    && model.research_retiring_panes.insert(pane_id.clone())
            })
            .unwrap_or(false);
        if !scheduled {
            return;
        }
        let state = self.clone();
        let node_id = node.id.clone();
        std::thread::spawn(move || {
            let mut last_error = None;
            let mut last_candidate = None;
            for attempt in 0..5_u32 {
                // The first delay lets the adapter flush its final lifecycle record;
                // later delays provide bounded recovery from transient file/process races.
                let delay_ms = 250_u64.saturating_mul(1_u64 << attempt).min(4_000);
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                let current_node = state.research_node(&node_id).ok();
                let still_settled = current_node.as_ref().is_some_and(|node| {
                    matches!(
                        node.status,
                        ResearchNodeStatus::Complete | ResearchNodeStatus::Failed
                    )
                });
                let active_subagents = current_node
                    .as_ref()
                    .filter(|node| node.status == ResearchNodeStatus::Complete)
                    .and_then(|node| node.agent_id.as_deref())
                    .is_some_and(|agent_id| {
                        state.agent_has_active_subagents(agent_id).unwrap_or(false)
                    });
                if !still_settled || active_subagents {
                    if let Ok(mut model) = state.inner.model.lock() {
                        model.research_retiring_panes.remove(&pane_id);
                    }
                    return;
                }
                // Re-read per attempt rather than capturing at schedule time:
                // the native checkpoint (session id / transcript path) usually
                // trails the Complete status by a beat, and a fresh read lets a
                // late checkpoint feed the snapshot. Waiting for it *before*
                // scheduling leaked the hidden pane forever when it never
                // arrived; now the pane retires after the bounded retries and
                // the snapshot falls back to the live turns, so the answer
                // stays viewable even though follow-ups remain blocked.
                let should_snapshot = state
                    .research_node(&node_id)
                    .map(|node| node.status == ResearchNodeStatus::Complete)
                    .unwrap_or(false);
                if should_snapshot {
                    if let Err(err) =
                        state.snapshot_research_response(&node_id, &mut last_candidate)
                    {
                        last_error = Some(format!("snapshot failed: {err}"));
                        // Keep the pane alive while retries remain — the snapshot
                        // wants the live turns — but a deterministic failure (e.g.
                        // a response over the snapshot size cap) would otherwise
                        // skip kill_pane on every attempt and nothing re-triggers
                        // retirement once the flag is cleared. On the last attempt
                        // reclaim the pane anyway; the adapter transcript remains
                        // as the viewing fallback.
                        if attempt < 4 {
                            continue;
                        }
                        eprintln!(
                            "qmux: retiring research pane {pane_id} without a response snapshot: {err}"
                        );
                    }
                }
                match crate::pty::kill_pane(&state, pane_id.clone()) {
                    Ok(()) => {
                        // Automated retirement is not a user close and must not be undoable.
                        state.clear_last_closed_pane_for_pane(&pane_id);
                        return;
                    }
                    Err(err) => {
                        if !state.pane_exists(&pane_id).unwrap_or(true) {
                            return;
                        }
                        last_error = Some(format!("pane close failed: {err}"));
                    }
                }
            }
            eprintln!(
                "qmux: failed to retire settled research pane {pane_id} after retries: {}",
                last_error.unwrap_or_else(|| "unknown error".to_string())
            );
            if let Ok(mut model) = state.inner.model.lock() {
                model.research_retiring_panes.remove(&pane_id);
            }
        });
    }

    /// Writes the node's durable response snapshot once the response is actually
    /// final. The agent reporting Done only means its lifecycle ended — the
    /// adapter may still be flushing transcript records — so a successfully
    /// *parsed* response is not yet a *complete* one. Two guards close that gap:
    /// the response must contain an assistant turn (an empty or prompt-only
    /// tail is never a finished answer), and it must read back identically on
    /// two consecutive attempts (`last_candidate` carries the previous read
    /// across the caller's retry loop). Either failure returns `Err` so the
    /// retry loop backs off and re-reads instead of committing a partial
    /// response as the permanent snapshot.
    fn snapshot_research_response(
        &self,
        node_id: &str,
        last_candidate: &mut Option<Vec<Turn>>,
    ) -> Result<(), String> {
        if research::read_response_snapshot(&self.inner.config.workspace_root, node_id)?.is_some() {
            self.mark_research_response_snapshotted(node_id)?;
            return Ok(());
        }
        let content = self.research_node_content(node_id)?;
        let ancestor_prompts = self
            .research_node_ancestor_prompts(node_id)
            .unwrap_or_default();
        let turns = research::load_transcript_response(
            &self.inner.config,
            &content.node,
            &ancestor_prompts,
        )
        .or_else(|_| {
            (!content.turns.is_empty())
                .then_some(content.turns)
                .ok_or_else(|| "completed research response is not available yet".to_string())
        })?;
        if !turns.iter().any(|turn| turn.role == "assistant") {
            *last_candidate = Some(turns);
            return Err("research response has no assistant turn yet".to_string());
        }
        if last_candidate.as_ref() != Some(&turns) {
            *last_candidate = Some(turns);
            return Err("research response has not settled yet".to_string());
        }
        research::write_response_snapshot(&self.inner.config.workspace_root, node_id, &turns)?;
        self.mark_research_response_snapshotted(node_id)
    }

    /// Records that the node's durable snapshot exists and announces it. The
    /// node was typically marked Complete *before* the adapter finished
    /// flushing, so a viewer that fetched content on the status transition may
    /// hold a truncated response; the stamped update is its refetch signal.
    fn mark_research_response_snapshotted(&self, node_id: &str) -> Result<(), String> {
        let updated = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let node = model
                .research_nodes
                .get_mut(node_id)
                .ok_or_else(|| format!("research node {node_id} was not found"))?;
            if node.response_snapshot_at.is_some() {
                None
            } else {
                node.response_snapshot_at = Some(now_millis());
                Some(node.clone())
            }
        };
        if let Some(node) = updated {
            self.persist();
            self.emit(QmuxEvent::new(
                "research.node.updated",
                node.pane_id.clone(),
                node.agent_id.clone(),
                json!({ "node": node }),
            ));
        }
        Ok(())
    }

    /// Unconditional append, kept for tests: production tails go through
    /// [`Self::append_turn_for_transcript`] so a rebind can't splice a dead
    /// file's parse over the new timeline.
    #[cfg(test)]
    pub fn append_turn(&self, turn: Turn) -> Result<(), String> {
        self.append_turn_internal(turn, None).map(|_| ())
    }

    /// Tail-scoped append: applies only while `transcript_path` is still the
    /// agent's bound transcript, and reports whether it applied. A tail checks
    /// its binding at the top of each poll, but a rebind (rewind rotation, a
    /// session picker choice, recovery) can land between that check and this
    /// write — an unconditional write would splice the dead file's parse over
    /// the new tail's timeline. Checking under the model lock closes that race.
    pub fn append_turn_for_transcript(
        &self,
        turn: Turn,
        transcript_path: &str,
    ) -> Result<bool, String> {
        self.append_turn_internal(turn, Some(transcript_path))
    }

    fn append_turn_internal(
        &self,
        turn: Turn,
        bound_transcript_path: Option<&str>,
    ) -> Result<bool, String> {
        let (should_persist_state, agent_for_graph, graph_store) = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let agent_id = turn.agent_id.clone();
            if let Some(bound_transcript_path) = bound_transcript_path {
                let still_bound = model.agents.get(&agent_id).is_some_and(|agent| {
                    agent.transcript_path.as_deref() == Some(bound_transcript_path)
                });
                if !still_bound {
                    return Ok(false);
                }
            }
            let is_user_turn = turn.role == "user";
            bump_agent_activity_locked(&mut model, &agent_id);
            let turns = model.turns.entry(agent_id.clone()).or_default();
            // Positional turn ids can be reused across a transcript rewrite or
            // rebind; the appended turn is the newest content for its id, so drop
            // any stale same-id entry rather than duplicating it mid-list.
            turns.retain(|existing| existing.id != turn.id);
            turns.push(turn.clone());
            if turns.len() > MAX_TURNS_PER_AGENT {
                let overflow = turns.len() - MAX_TURNS_PER_AGENT;
                turns.drain(..overflow);
            }
            let agent_for_graph = model.agents.get(&agent_id).cloned();
            let agent_is_research = agent_for_graph.as_ref().is_some_and(|agent| {
                model
                    .groups
                    .get(&agent.group_id)
                    .is_some_and(|group| group.scope == WorkspaceScope::Research)
            });
            let should_persist_recent = if is_user_turn && !agent_is_research {
                agent_for_graph.clone().is_some_and(|agent| {
                    // CacheOnly: the turn just appended supplies the in-memory
                    // preview/line-count, so the disk fallback has nothing to add
                    // — and this runs under the model lock.
                    upsert_recent_session_for_agent_locked(
                        &mut model,
                        &agent,
                        now_millis(),
                        true,
                        RecentSessionMeta::CacheOnly,
                    )
                    .changed
                })
            } else {
                false
            };
            let mut graph_store = None;
            let mut created_thread_record = false;
            if let Some(agent) = agent_for_graph.as_ref().filter(|_| !agent_is_research) {
                let (store, created) = thread_store_for_agent_locked(
                    &mut model,
                    agent,
                    &self.inner.config.workspace_root,
                );
                graph_store = Some(store);
                created_thread_record = created;
            }
            (
                should_persist_recent || created_thread_record,
                agent_for_graph,
                graph_store,
            )
        };
        if let (Some(agent), Some(store)) = (agent_for_graph, graph_store)
            && let Err(err) = store.append_turn_node(&agent, &turn)
        {
            eprintln!(
                "qmux: failed to append thread graph for agent {}: {err}",
                agent.id
            );
        }
        if should_persist_state {
            self.persist();
        }
        if let Some(agent) = self.agent(&turn.agent_id)?
            && self.sync_research_node_from_agent(&agent)?
        {
            self.persist();
        }
        Ok(true)
    }

    /// Unconditional replace, kept for tests: production tails go through
    /// [`Self::replace_turns_for_transcript`] (see there for the race).
    #[cfg(test)]
    pub fn replace_turns(&self, agent_id: &str, turns: Vec<Turn>) -> Result<(), String> {
        self.replace_turns_internal(agent_id, turns, None)
            .map(|_| ())
    }

    /// Tail-scoped replace: applies only while `transcript_path` is still the
    /// agent's bound transcript, and reports whether it applied. See
    /// [`Self::append_turn_for_transcript`] for the race this closes — for a
    /// replace the stakes are higher, since a stale tail's late full-window
    /// refresh would wholesale swap the new transcript's timeline for the dead
    /// file's parse.
    pub fn replace_turns_for_transcript(
        &self,
        agent_id: &str,
        transcript_path: &str,
        turns: Vec<Turn>,
    ) -> Result<bool, String> {
        self.replace_turns_internal(agent_id, turns, Some(transcript_path))
    }

    fn replace_turns_internal(
        &self,
        agent_id: &str,
        mut turns: Vec<Turn>,
        bound_transcript_path: Option<&str>,
    ) -> Result<bool, String> {
        let turns_for_graph = turns.clone();
        let (should_persist_state, agent_for_graph, graph_store) = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if let Some(bound_transcript_path) = bound_transcript_path {
                let still_bound = model.agents.get(agent_id).is_some_and(|agent| {
                    agent.transcript_path.as_deref() == Some(bound_transcript_path)
                });
                if !still_bound {
                    return Ok(false);
                }
            }
            if turns.len() > MAX_TURNS_PER_AGENT {
                let overflow = turns.len() - MAX_TURNS_PER_AGENT;
                turns.drain(..overflow);
            }
            bump_agent_activity_locked(&mut model, agent_id);
            model.turns.insert(agent_id.to_string(), turns);
            let agent_for_graph = model.agents.get(agent_id).cloned();
            let agent_is_research = agent_for_graph.as_ref().is_some_and(|agent| {
                model
                    .groups
                    .get(&agent.group_id)
                    .is_some_and(|group| group.scope == WorkspaceScope::Research)
            });
            let should_persist_recent = !agent_is_research
                && agent_for_graph.clone().is_some_and(|agent| {
                    upsert_recent_session_for_agent_locked(
                        &mut model,
                        &agent,
                        now_millis(),
                        true,
                        RecentSessionMeta::CacheOnly,
                    )
                    .changed
                });
            let mut graph_store = None;
            let mut created_thread_record = false;
            if let Some(agent) = agent_for_graph.as_ref().filter(|_| !agent_is_research) {
                let (store, created) = thread_store_for_agent_locked(
                    &mut model,
                    agent,
                    &self.inner.config.workspace_root,
                );
                graph_store = Some(store);
                created_thread_record = created;
            }
            (
                should_persist_recent || created_thread_record,
                agent_for_graph,
                graph_store,
            )
        };
        if let (Some(agent), Some(store)) = (agent_for_graph, graph_store)
            && let Err(err) = store.replace_agent_branch_turns(&agent, &turns_for_graph)
        {
            eprintln!(
                "qmux: failed to write thread graph for agent {}: {err}",
                agent.id
            );
        }
        if should_persist_state {
            self.persist();
        }
        if let Some(agent) = self.agent(agent_id)?
            && self.sync_research_node_from_agent(&agent)?
        {
            self.persist();
        }
        Ok(true)
    }

    /// Test convenience: queues a plain text turn with no directives. Production
    /// callers build a [`QueuedTurn`] and use [`Self::enqueue_agent_queued_turn`].
    #[cfg(test)]
    pub fn enqueue_agent_turn(&self, agent_id: &str, data: String) -> Result<usize, String> {
        self.enqueue_agent_queued_turn(agent_id, QueuedTurn::new(data))
    }

    pub fn enqueue_agent_wait_turn_with_target_label(
        &self,
        agent_id: &str,
        data: String,
        wait_for_agent_id: &str,
        wait_for_pane_id: Option<&str>,
        wait_for_label: Option<&str>,
    ) -> Result<usize, String> {
        if agent_id == wait_for_agent_id {
            return Err("a queued turn cannot wait on its own agent".to_string());
        }

        let len = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;

            if !model.agents.contains_key(agent_id) {
                return Err(format!("agent {agent_id} was not found"));
            }
            let target = model
                .agents
                .get(wait_for_agent_id)
                .ok_or_else(|| format!("agent {wait_for_agent_id} was not found"))?;
            if wait_dependency_would_cycle_locked(&model, agent_id, wait_for_agent_id) {
                return Err("that wait would create a queue dependency cycle".to_string());
            }

            let supplied_label = wait_for_pane_id.and_then(|pane_id| {
                if target.pane_id.as_deref() != Some(pane_id) {
                    return None;
                }
                wait_for_label
                    .map(str::trim)
                    .filter(|label| !label.is_empty())
                    .map(ToString::to_string)
            });
            let label = supplied_label.or_else(|| wait_target_label_locked(&model, target));
            let wait_for = QueuedTurnWait {
                agent_id: wait_for_agent_id.to_string(),
                pane_id: target.pane_id.clone(),
                label,
            };
            enqueue_queued_turn_locked(&mut model, agent_id, QueuedTurn::waiting(data, wait_for))?
        };
        self.persist();
        Ok(len)
    }

    /// Queues a fully-formed turn (text plus any pause/wait/delivery directives).
    pub fn enqueue_agent_queued_turn(
        &self,
        agent_id: &str,
        turn: QueuedTurn,
    ) -> Result<usize, String> {
        let len = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            enqueue_queued_turn_locked(&mut model, agent_id, turn)?
        };
        self.persist();
        Ok(len)
    }

    /// Queued turn texts only — used by the drain path, expected-data matching, and
    /// tests. The structured view (with pause flags) is `agent_queued_turns`.
    pub fn list_agent_turn_queue(&self, agent_id: &str) -> Result<Vec<String>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model
            .agent_turn_queues
            .get(agent_id)
            .map(|queue| queue.iter().map(|turn| turn.text.clone()).collect())
            .unwrap_or_default())
    }

    /// Structured queued turns (text + pause flag) for events, command results, and
    /// the frontend.
    pub fn agent_queued_turns(&self, agent_id: &str) -> Result<Vec<QueuedTurn>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model
            .agent_turn_queues
            .get(agent_id)
            .map(|queue| queue.iter().cloned().collect())
            .unwrap_or_default())
    }

    /// Toggles the pause-after-send flag on a single queued turn, guarding against a
    /// stale index with the expected text. Returns the updated structured queue.
    pub fn set_queued_turn_pause(
        &self,
        agent_id: &str,
        index: usize,
        pause_after: bool,
        expected_text: Option<&str>,
        expected_id: Option<&str>,
    ) -> Result<Vec<QueuedTurn>, String> {
        let queued_turns = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let queue = model
                .agent_turn_queues
                .get_mut(agent_id)
                .ok_or_else(|| format!("agent {agent_id} does not have queued turns"))?;
            let turn = queue
                .get_mut(index)
                .ok_or_else(|| format!("queued turn {index} was not found"))?;
            // The id is the authoritative identity: duplicate-text turns are
            // indistinguishable by text alone, so a shifted duplicate would pass
            // the text guard. Both are checked when supplied.
            if let Some(expected_id) = expected_id
                && turn.id != expected_id
            {
                return Err("queued turn changed; refresh before updating".to_string());
            }
            if let Some(expected_text) = expected_text
                && turn.text != expected_text
            {
                return Err("queued turn changed; refresh before updating".to_string());
            }
            turn.pause_after = pause_after;
            queue.iter().cloned().collect::<Vec<_>>()
        };
        self.persist();
        Ok(queued_turns)
    }

    pub fn remove_agent_turn_queue_item(
        &self,
        agent_id: &str,
        index: usize,
        expected_data: Option<&str>,
        expected_id: Option<&str>,
    ) -> Result<(QueuedTurn, Vec<QueuedTurn>), String> {
        let mut model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;

        let (removed, queued_turns, is_empty) = {
            let queue = model
                .agent_turn_queues
                .get_mut(agent_id)
                .ok_or_else(|| format!("agent {agent_id} does not have queued turns"))?;
            let current = queue
                .get(index)
                .ok_or_else(|| format!("queued turn {index} was not found"))?;
            if let Some(expected_id) = expected_id
                && current.id != expected_id
            {
                return Err("queued turn changed; refresh before editing".to_string());
            }
            if let Some(expected_data) = expected_data
                && current.text != expected_data
            {
                return Err("queued turn changed; refresh before editing".to_string());
            }

            let removed = queue
                .remove(index)
                .ok_or_else(|| format!("queued turn {index} was not found"))?;
            if index == 0
                && removed
                    .wait_for
                    .as_ref()
                    .is_some_and(|wait_for| wait_for.agent_id != agent_id)
                && let Some(next) = queue.front_mut()
            {
                next.wait_for = removed.wait_for.clone();
            }
            let queued_turns = queue.iter().cloned().collect::<Vec<_>>();
            (removed, queued_turns, queue.is_empty())
        };

        if is_empty {
            model.agent_turn_queues.remove(agent_id);
            if let Some(agent) = model.agents.get_mut(agent_id) {
                agent.orphaned_queue_pane_id = None;
            }
        }

        drop(model);
        self.persist();
        Ok((removed, queued_turns))
    }

    pub fn reorder_agent_turn_queue_item(
        &self,
        agent_id: &str,
        from: usize,
        to: usize,
        expected_data: Option<&str>,
        expected_id: Option<&str>,
    ) -> Result<Vec<QueuedTurn>, String> {
        let queued_turns = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let queue = model
                .agent_turn_queues
                .get_mut(agent_id)
                .ok_or_else(|| format!("agent {agent_id} does not have queued turns"))?;
            let len = queue.len();
            if from >= len || to >= len {
                return Err(format!("queued turn index out of range (len {len})"));
            }
            if expected_id.is_some() || expected_data.is_some() {
                let current = queue
                    .get(from)
                    .ok_or_else(|| format!("queued turn {from} was not found"))?;
                if let Some(expected_id) = expected_id
                    && current.id != expected_id
                {
                    return Err("queued turn changed; refresh before reordering".to_string());
                }
                if let Some(expected_data) = expected_data
                    && current.text != expected_data
                {
                    return Err("queued turn changed; refresh before reordering".to_string());
                }
            }
            let moved = queue
                .remove(from)
                .ok_or_else(|| format!("queued turn {from} was not found"))?;
            queue.insert(to, moved);
            queue.iter().cloned().collect::<Vec<_>>()
        };
        self.persist();
        Ok(queued_turns)
    }

    /// Claims the next ready queued turn for draining, marking the agent as draining so
    /// no concurrent trigger can claim a second turn until [`finish_agent_drain`] runs.
    /// Returns [`AgentTurnClaim::Draining`] when another drain already holds the agent,
    /// [`AgentTurnClaim::Idle`] when nothing is ready, else [`AgentTurnClaim::Ready`]
    /// with the popped turn. The check-and-claim is atomic under the model lock, which
    /// is what prevents the double-send race.
    pub fn claim_ready_agent_turn(&self, agent_id: &str) -> Result<AgentTurnClaim, String> {
        let claim = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if model.agent_draining.contains(agent_id) {
                return Ok(AgentTurnClaim::Draining);
            }
            match pop_ready_locked(&mut model, agent_id) {
                Some((turn, pending)) => {
                    model.agent_draining.insert(agent_id.to_string());
                    // Keep a durable copy until delivery confirms, so a crash mid-send
                    // re-queues the turn on restart instead of dropping it.
                    model
                        .agent_inflight
                        .insert(agent_id.to_string(), turn.clone());
                    AgentTurnClaim::Ready { turn, pending }
                }
                None => return Ok(AgentTurnClaim::Idle),
            }
        };
        self.persist();
        Ok(claim)
    }

    /// The idle-handler variant of [`claim_ready_agent_turn`]: atomically decides, under
    /// the model lock, what an agent reaching a ready state should do. Returns `Busy`
    /// when another drain already owns the agent (the caller must not touch its status),
    /// `Sent` after claiming a ready turn for the caller to send, or `Idle` after settling
    /// the agent to `settled_status`. Crucially the typing check and status write happen
    /// under the same lock, so a racing `set_agent_typing(false)` that clears the flag and
    /// re-reads the status observes a ready state and drains the held turn — closing the
    /// lost-wakeup where it would otherwise see stale `Running`, skip its drain, and
    /// strand the queue.
    pub fn claim_next_turn_or_settle(
        &self,
        agent_id: &str,
        settled_status: AgentStatus,
    ) -> Result<IdleAdvance, String> {
        debug_assert!(matches!(
            settled_status,
            AgentStatus::AwaitingInput | AgentStatus::Done | AgentStatus::Idle
        ));
        let (outcome, settled_agent) = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if model.agent_draining.contains(agent_id) {
                // Another drain is mid-send; it owns the status transition. Leave the
                // agent untouched (do not persist) so we can't clobber its Running.
                return Ok(IdleAdvance::Busy);
            }
            if model.agent_typing.contains(agent_id) {
                // User is mid-keystroke: hold the queue and settle atomically with
                // reading the typing flag (see the doc comment).
                let settled_agent = model.agents.get_mut(agent_id).map(|agent| {
                    agent.status = settled_status;
                    agent.clone()
                });
                (IdleAdvance::Idle, settled_agent)
            } else if let Some((turn, pending)) = pop_ready_locked(&mut model, agent_id) {
                model.agent_draining.insert(agent_id.to_string());
                // Durable copy until delivery confirms (see claim_ready_agent_turn).
                model
                    .agent_inflight
                    .insert(agent_id.to_string(), turn.clone());
                (IdleAdvance::Sent { turn, pending }, None)
            } else {
                let settled_agent = model.agents.get_mut(agent_id).map(|agent| {
                    agent.status = settled_status;
                    agent.clone()
                });
                (IdleAdvance::Idle, settled_agent)
            }
        };
        // The status write above must stay inside the queue/typing decision's lock.
        // Run the same post-write synchronization after releasing it so a background
        // research pane can react to a terminal status without waiting for another
        // transcript or focus-triggered update.
        if let Some(agent) = settled_agent.as_ref() {
            self.sync_research_node_from_agent(agent)?;
        }
        self.persist();
        Ok(outcome)
    }

    /// Clears the draining guard set by a successful claim, allowing the next drain to
    /// proceed. Best-effort: a poisoned lock just leaves the guard set, which fails safe
    /// (no further auto-drain) rather than risking a double-send.
    pub fn finish_agent_drain(&self, agent_id: &str) {
        if let Ok(mut model) = self.inner.model.lock() {
            model.agent_draining.remove(agent_id);
        }
    }

    /// Reserves the draining guard for a direct (user-initiated) send, serializing it
    /// against queue drains through the same `agent_draining` flag. Returns `false`
    /// when a drain — or another direct send — already owns the agent, so the caller
    /// should queue behind it instead of writing a second turn into the same pane
    /// concurrently. Pair every `true` with [`finish_agent_drain`].
    pub fn begin_direct_send(&self, agent_id: &str) -> Result<bool, String> {
        let mut model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        if model.agent_draining.contains(agent_id) {
            return Ok(false);
        }
        model.agent_draining.insert(agent_id.to_string());
        Ok(true)
    }

    /// Clears a delivered turn's in-flight record. Called once its bytes reach the PTY,
    /// so a crash before this leaves the turn in the persisted queue (via
    /// `restore_session`) to be re-delivered rather than lost.
    pub fn clear_agent_inflight(&self, agent_id: &str) {
        let changed = match self.inner.model.lock() {
            Ok(mut model) => model.agent_inflight.remove(agent_id).is_some(),
            Err(_) => false,
        };
        if changed {
            self.persist();
        }
    }

    /// Rolls a turn that failed to send back to the front of its queue and clears its
    /// in-flight record in one locked step, so the persisted snapshot never holds the
    /// same turn in both places (which would double-send it on restart).
    pub fn requeue_inflight_after_failed_drain(&self, agent_id: &str, turn: QueuedTurn) {
        let ok = match self.inner.model.lock() {
            Ok(mut model) => {
                model.agent_inflight.remove(agent_id);
                model
                    .agent_turn_queues
                    .entry(agent_id.to_string())
                    .or_default()
                    .push_front(turn);
                true
            }
            Err(_) => false,
        };
        if ok {
            self.persist();
        } else {
            eprintln!(
                "qmux: dropped queued turn for agent {agent_id} after failed re-queue (model lock poisoned)"
            );
        }
    }

    /// Test-only direct pop of the next ready turn (no draining guard), used to assert
    /// wait-resolution semantics without the serialized-drain bookkeeping.
    #[cfg(test)]
    pub fn pop_ready_agent_turn(
        &self,
        agent_id: &str,
    ) -> Result<Option<(QueuedTurn, usize)>, String> {
        let popped = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            match pop_ready_locked(&mut model, agent_id) {
                Some(result) => result,
                None => return Ok(None),
            }
        };
        self.persist();
        Ok(Some(popped))
    }

    pub fn agents_with_front_wait_for(&self, target_agent_id: &str) -> Result<Vec<String>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model
            .agent_turn_queues
            .iter()
            .filter_map(|(agent_id, queue)| {
                let waits_for_target = queue
                    .front()
                    .and_then(|turn| turn.wait_for.as_ref())
                    .is_some_and(|wait| wait.agent_id == target_agent_id);
                waits_for_target.then(|| agent_id.clone())
            })
            .collect())
    }

    /// Inserts a turn into an agent's queue at `index` (clamped to the queue length),
    /// returning the new length. Used to roll a moved turn back to its original spot
    /// when handing it to another agent fails (preserving its queue directives).
    pub fn insert_agent_turn_at(
        &self,
        agent_id: &str,
        index: usize,
        turn: QueuedTurn,
    ) -> Result<usize, String> {
        let len = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let queue = model
                .agent_turn_queues
                .entry(agent_id.to_string())
                .or_default();
            let at = index.min(queue.len());
            queue.insert(at, turn);
            queue.len()
        };
        self.persist();
        Ok(len)
    }

    /// Sets an agent's paused flag without disturbing its other fields (a field-scoped
    /// write, so a concurrent hook update can't clobber it). Returns the updated agent.
    pub fn set_agent_paused(
        &self,
        agent_id: &str,
        paused: bool,
    ) -> Result<Option<AgentInfo>, String> {
        let updated = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            match model.agents.get_mut(agent_id) {
                Some(agent) => {
                    agent.paused = paused;
                    Some(agent.clone())
                }
                None => None,
            }
        };
        if updated.is_some() {
            self.persist();
        }
        Ok(updated)
    }

    /// Marks that the agent's currently-running queued turn requested a pause; the
    /// agent enters paused mode when that turn finishes (see `take_agent_pending_pause`).
    pub fn mark_agent_pending_pause(&self, agent_id: &str) -> Result<(), String> {
        let mut model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        model.agent_pending_pause.insert(agent_id.to_string());
        Ok(())
    }

    /// Consumes the pending-pause marker, returning whether one was set.
    pub fn take_agent_pending_pause(&self, agent_id: &str) -> Result<bool, String> {
        let mut model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model.agent_pending_pause.remove(agent_id))
    }

    pub fn agent_is_paused(&self, agent_id: &str) -> Result<bool, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model
            .agents
            .get(agent_id)
            .map(|agent| agent.paused)
            .unwrap_or(false))
    }

    /// Records whether the user is actively typing for an agent; while set, the idle
    /// handler holds off auto-draining the queue.
    pub fn set_agent_typing(&self, agent_id: &str, typing: bool) -> Result<(), String> {
        let mut model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        if typing {
            model.agent_typing.insert(agent_id.to_string());
        } else {
            model.agent_typing.remove(agent_id);
        }
        Ok(())
    }

    pub fn agent_is_typing(&self, agent_id: &str) -> Result<bool, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model.agent_typing.contains(agent_id))
    }

    /// Stores the agent's composer draft and snapshots it to disk. A trimmed-empty
    /// draft drops the entry so recovery never restores stray whitespace and the
    /// map does not grow an entry per cleared composer.
    /// The current millisecond wall clock, matching QmuxEvent timestamps.
    fn now_millis() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default()
    }

    pub fn global_drafts(&self) -> Result<Vec<GlobalDraft>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model.global_drafts.clone())
    }

    /// Emits the full drafts list — the store is small and global, so every
    /// mutation broadcasts the whole truth instead of deltas.
    fn emit_global_drafts(&self, drafts: &[GlobalDraft]) {
        self.emit(QmuxEvent::new(
            "drafts.changed",
            None,
            None,
            json!({ "drafts": drafts }),
        ));
    }

    pub fn create_global_draft(&self, text: String) -> Result<GlobalDraft, String> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err("Draft text cannot be empty".to_string());
        }
        let draft = GlobalDraft {
            id: self.next_id("draft"),
            text: trimmed.to_string(),
            created_at: Self::now_millis(),
            consumed: None,
        };
        let drafts = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            model.global_drafts.push(draft.clone());
            model.global_drafts.clone()
        };
        self.persist();
        self.emit_global_drafts(&drafts);
        Ok(draft)
    }

    pub fn update_global_draft(&self, draft_id: &str, text: String) -> Result<GlobalDraft, String> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err("Draft text cannot be empty".to_string());
        }
        let (draft, drafts) = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let draft = model
                .global_drafts
                .iter_mut()
                .find(|draft| draft.id == draft_id)
                .ok_or_else(|| format!("Draft {draft_id} was not found"))?;
            if draft.consumed.is_some() {
                return Err("Draft was already assigned".to_string());
            }
            draft.text = trimmed.to_string();
            (draft.clone(), model.global_drafts.clone())
        };
        self.persist();
        self.emit_global_drafts(&drafts);
        Ok(draft)
    }

    pub fn delete_global_draft(&self, draft_id: &str) -> Result<Vec<GlobalDraft>, String> {
        let drafts = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let before = model.global_drafts.len();
            model.global_drafts.retain(|draft| draft.id != draft_id);
            if model.global_drafts.len() == before {
                return Err(format!("Draft {draft_id} was not found"));
            }
            model.global_drafts.clone()
        };
        self.persist();
        self.emit_global_drafts(&drafts);
        Ok(drafts)
    }

    /// Atomically claims a draft for assignment: marks it consumed only if it
    /// wasn't already, so two concurrent assigns can't both deliver the text.
    /// The claim happens before the submit; `unclaim_global_draft` rolls it
    /// back if the submit fails (the move_queued_agent_turn shape).
    pub fn claim_global_draft(
        &self,
        draft_id: &str,
        agent_id: &str,
    ) -> Result<GlobalDraft, String> {
        let (draft, drafts) = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let draft = model
                .global_drafts
                .iter_mut()
                .find(|draft| draft.id == draft_id)
                .ok_or_else(|| format!("Draft {draft_id} was not found"))?;
            if draft.consumed.is_some() {
                return Err("Draft was already assigned".to_string());
            }
            draft.consumed = Some(GlobalDraftConsumed {
                agent_id: agent_id.to_string(),
                at: Self::now_millis(),
            });
            (draft.clone(), model.global_drafts.clone())
        };
        self.persist();
        self.emit_global_drafts(&drafts);
        Ok(draft)
    }

    pub fn unclaim_global_draft(&self, draft_id: &str) -> Result<(), String> {
        let drafts = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let Some(draft) = model
                .global_drafts
                .iter_mut()
                .find(|draft| draft.id == draft_id)
            else {
                return Ok(());
            };
            draft.consumed = None;
            model.global_drafts.clone()
        };
        self.persist();
        self.emit_global_drafts(&drafts);
        Ok(())
    }

    pub fn set_agent_draft(&self, agent_id: &str, draft: String) -> Result<(), String> {
        {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if draft.trim().is_empty() {
                model.agent_drafts.remove(agent_id);
            } else {
                model.agent_drafts.insert(agent_id.to_string(), draft);
            }
        }
        self.persist();
        Ok(())
    }

    pub fn agent_draft(&self, agent_id: &str) -> Result<Option<String>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model.agent_drafts.get(agent_id).cloned())
    }

    pub fn record_agent_send(
        &self,
        agent_id: &str,
        text: String,
        source: AgentSendSource,
    ) -> Result<u64, String> {
        let mut model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        let tracking = model
            .agent_send_tracking
            .entry(agent_id.to_string())
            .or_default();
        tracking.prune_expired(now_millis());
        tracking.next_send_id = tracking.next_send_id.wrapping_add(1).max(1);
        let send_id = tracking.next_send_id;
        tracking.outstanding_sends.push_back(AgentOutstandingSend {
            id: send_id,
            text,
            sent_at_seq: tracking.ups_seq,
            sent_at_ms: now_millis(),
            source,
        });
        Ok(send_id)
    }

    pub fn match_agent_prompt_submit(
        &self,
        agent_id: &str,
        prompt: Option<&str>,
    ) -> Result<AgentPromptSubmitMatch, String> {
        let mut model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        let tracking = model
            .agent_send_tracking
            .entry(agent_id.to_string())
            .or_default();
        tracking.prune_expired(now_millis());
        tracking.ups_seq = tracking.ups_seq.saturating_add(1);
        let outstanding_count = tracking.outstanding_sends.len();

        let Some(prompt) = prompt else {
            return Ok(AgentPromptSubmitMatch::MissingPrompt {
                outstanding_sends: outstanding_count,
            });
        };

        let Some(front) = tracking.outstanding_sends.front() else {
            return Ok(AgentPromptSubmitMatch::Untracked {
                actual: prompt.to_string(),
                outstanding_sends: 0,
            });
        };

        if prompts_match(prompt, &front.text) {
            let matched = tracking
                .outstanding_sends
                .pop_front()
                .expect("front checked above");
            Ok(AgentPromptSubmitMatch::Matched {
                source: matched.source,
                outstanding_sends: tracking.outstanding_sends.len(),
            })
        } else {
            Ok(AgentPromptSubmitMatch::Mismatched {
                expected: front.text.clone(),
                actual: prompt.to_string(),
                outstanding_sends: outstanding_count,
            })
        }
    }

    // Only the test suite inspects the full outstanding-send queue; production
    // code reads the count via match_agent_prompt_submit and clears it via
    // clear_agent_outstanding_sends. Gated to keep it out of the release binary.
    #[cfg(test)]
    pub fn outstanding_agent_sends(
        &self,
        agent_id: &str,
    ) -> Result<Vec<AgentOutstandingSend>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model
            .agent_send_tracking
            .get(agent_id)
            .map(|tracking| tracking.outstanding_sends.iter().cloned().collect())
            .unwrap_or_default())
    }

    // Rewinds recorded send times so tests can cross OUTSTANDING_SEND_TTL_MS without
    // sleeping through it.
    #[cfg(test)]
    pub fn age_agent_outstanding_sends(&self, agent_id: &str, by_ms: u128) -> Result<(), String> {
        let mut model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        if let Some(tracking) = model.agent_send_tracking.get_mut(agent_id) {
            for send in &mut tracking.outstanding_sends {
                send.sent_at_ms = send.sent_at_ms.saturating_sub(by_ms);
            }
        }
        Ok(())
    }

    pub fn clear_agent_outstanding_sends(&self, agent_id: &str) -> Result<usize, String> {
        let mut model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        let Some(tracking) = model.agent_send_tracking.get_mut(agent_id) else {
            return Ok(0);
        };
        let cleared = tracking.outstanding_sends.len();
        tracking.outstanding_sends.clear();
        Ok(cleared)
    }

    /// Current value of the agent's activity counter (see `Model::agent_activity`).
    /// An agent with no recorded activity reads as 0.
    pub fn agent_activity_seq(&self, agent_id: &str) -> Result<u64, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model.agent_activity.get(agent_id).copied().unwrap_or(0))
    }

    /// Current value of the agent's status/lifecycle counter. This is intentionally
    /// narrower than `agent_activity_seq`: transcript updates do not bump it.
    pub fn agent_status_activity_seq(&self, agent_id: &str) -> Result<u64, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model
            .agent_status_activity
            .get(agent_id)
            .copied()
            .unwrap_or(0))
    }

    pub fn agent_has_outstanding_send_source(
        &self,
        agent_id: &str,
        source: AgentSendSource,
    ) -> Result<bool, String> {
        let mut model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model
            .agent_send_tracking
            .get_mut(agent_id)
            .is_some_and(|tracking| {
                tracking.prune_expired(now_millis());
                tracking
                    .outstanding_sends
                    .iter()
                    .any(|send| send.source == source)
            }))
    }

    /// What a submit-confirmation watch should conclude about one exact send: gone
    /// (confirmed), still outstanding with prompt activity after it, or still
    /// outstanding with no prompt submitted since. The distinction matters because
    /// `match_agent_prompt_submit` only pops on an exact text match — a turn that
    /// submitted with mangled text leaves its record outstanding forever, and
    /// recovery must not treat that as "never started".
    pub fn check_agent_submit_watch(
        &self,
        agent_id: &str,
        send_id: u64,
    ) -> Result<SubmitWatchStatus, String> {
        let mut model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        let Some(tracking) = model.agent_send_tracking.get_mut(agent_id) else {
            return Ok(SubmitWatchStatus::Confirmed);
        };
        tracking.prune_expired(now_millis());
        let Some(send) = tracking
            .outstanding_sends
            .iter()
            .find(|send| send.id == send_id)
        else {
            return Ok(SubmitWatchStatus::Confirmed);
        };
        if tracking.ups_seq > send.sent_at_seq {
            Ok(SubmitWatchStatus::StillPendingWithPromptActivity)
        } else {
            Ok(SubmitWatchStatus::StillPending)
        }
    }

    /// Reclaims a send that was written to a pane but never echoed a prompt submit:
    /// atomically removes the exact outstanding-send record and puts the turn back
    /// at the front of its agent's queue, returning the new queue snapshot. Returns
    /// `Ok(None)` — without touching the queue — when the record is already gone
    /// (its echo won the race, or an idle boundary cleared it), so a turn that did
    /// start can never be requeued into a duplicate. Callers should tag the turn
    /// `possibly_pasted` so its retry submits without re-pasting.
    pub fn requeue_unconfirmed_send(
        &self,
        agent_id: &str,
        send_id: u64,
        turn: QueuedTurn,
    ) -> Result<Option<Vec<QueuedTurn>>, String> {
        let queued_turns = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let Some(tracking) = model.agent_send_tracking.get_mut(agent_id) else {
                return Ok(None);
            };
            let Some(index) = tracking
                .outstanding_sends
                .iter()
                .position(|send| send.id == send_id)
            else {
                return Ok(None);
            };
            tracking.outstanding_sends.remove(index);
            let queue = model
                .agent_turn_queues
                .entry(agent_id.to_string())
                .or_default();
            queue.push_front(turn);
            queue.iter().cloned().collect::<Vec<_>>()
        };
        self.persist();
        Ok(Some(queued_turns))
    }

    /// Removes one exact advisory send record, used to roll back tracking when
    /// the pane write fails after the record was reserved but before delivery.
    pub fn remove_agent_outstanding_send_id(
        &self,
        agent_id: &str,
        send_id: u64,
    ) -> Result<bool, String> {
        let mut model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        let Some(tracking) = model.agent_send_tracking.get_mut(agent_id) else {
            return Ok(false);
        };
        let Some(index) = tracking
            .outstanding_sends
            .iter()
            .position(|send| send.id == send_id)
        else {
            return Ok(false);
        };
        tracking.outstanding_sends.remove(index);
        Ok(true)
    }

    pub fn mark_transcript_tail(&self, agent_id: &str, path: &str) -> Result<bool, String> {
        let key = format!("{agent_id}:{path}");
        let mut tails = self
            .inner
            .transcript_tails
            .lock()
            .map_err(|_| "transcript tail lock poisoned".to_string())?;
        Ok(tails.insert(key))
    }

    /// Drops the marker for a tail that is stopping (its file rotated away or its
    /// agent went away) so the same `(agent_id, path)` can be tailed again if the
    /// agent ever returns to that file. Best-effort: a poisoned lock is ignored
    /// rather than propagated, since this only runs as a tail unwinds.
    pub fn clear_transcript_tail(&self, agent_id: &str, path: &str) {
        let key = format!("{agent_id}:{path}");
        if let Ok(mut tails) = self.inner.transcript_tails.lock() {
            tails.remove(&key);
        }
    }

    /// Whether a pane is currently registered, regardless of backend.
    pub fn pane_exists(&self, pane_id: &str) -> Result<bool, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model.panes.contains_key(pane_id))
    }

    pub fn pane_writer(&self, pane_id: &str) -> Result<Option<SharedWriter>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model.panes.get(pane_id).map(|pane| match &pane.backend {
            PaneBackend::HostPty { writer, .. } => writer.clone(),
        }))
    }

    pub fn pane_master(&self, pane_id: &str) -> Result<Option<SharedMaster>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model.panes.get(pane_id).map(|pane| match &pane.backend {
            PaneBackend::HostPty { master, .. } => master.clone(),
        }))
    }

    pub fn pane_child(&self, pane_id: &str) -> Result<Option<SharedChild>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model.panes.get(pane_id).map(|pane| match &pane.backend {
            PaneBackend::HostPty { child, .. } => child.clone(),
        }))
    }

    /// Snapshots every live pane's id and child handle. Used by the app-exit
    /// teardown to take down each pane's process tree, since quit bypasses the
    /// per-pane `kill_pane` path.
    pub fn all_pane_children(&self) -> Result<Vec<(String, SharedChild)>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model
            .panes
            .iter()
            .map(|(pane_id, pane)| match &pane.backend {
                PaneBackend::HostPty { child, .. } => (pane_id.clone(), child.clone()),
            })
            .collect())
    }

    /// Returns the per-pane send lock, minting one on first use. `write_pane` holds
    /// it across a paste+submit sequence so concurrent submits don't interleave. See
    /// the `pane_send_locks` field.
    pub fn pane_send_lock(&self, pane_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self
            .inner
            .pane_send_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        locks.entry(pane_id.to_string()).or_default().clone()
    }

    pub fn pane_backlog(&self, pane_id: &str) -> Result<Option<SharedBacklog>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model.panes.get(pane_id).map(|pane| match &pane.backend {
            PaneBackend::HostPty { backlog, .. } => backlog.clone(),
        }))
    }

    pub fn pane_is_native(&self, pane_id: &str) -> Result<Option<bool>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model.panes.get(pane_id).map(|pane| match &pane.backend {
            PaneBackend::HostPty { native_surface, .. } => *native_surface,
        }))
    }

    pub fn pane_has_host_pty(&self, pane_id: &str) -> Result<Option<bool>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model
            .panes
            .get(pane_id)
            .map(|pane| matches!(pane.backend, PaneBackend::HostPty { .. })))
    }

    pub fn research_pane_accepts_input(&self, pane_id: &str) -> Result<Option<bool>, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        let Some(node) = model
            .research_nodes
            .values()
            .find(|node| node.pane_id.as_deref() == Some(pane_id))
        else {
            return Ok(None);
        };
        let allowed = node
            .agent_id
            .as_deref()
            .and_then(|agent_id| model.agents.get(agent_id))
            .is_some_and(|agent| {
                matches!(
                    agent.status,
                    AgentStatus::AwaitingPermission | AgentStatus::AwaitingInput
                )
            });
        Ok(Some(allowed))
    }

    /// Whether the agent is (or was) the run behind a research node. Research
    /// runs take exactly one prompt at launch; queued turns can never drain
    /// into them and would park the agent past pane retirement.
    pub fn agent_is_research_run(&self, agent_id: &str) -> Result<bool, String> {
        let model = self
            .inner
            .model
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        Ok(model
            .research_nodes
            .values()
            .any(|node| node.agent_id.as_deref() == Some(agent_id)))
    }

    pub fn update_pane_size(&self, pane_id: &str, cols: u16, rows: u16) -> Result<(), String> {
        {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let pane = model
                .panes
                .get_mut(pane_id)
                .ok_or_else(|| format!("pane {pane_id} was not found"))?;
            pane.info.cols = cols;
            pane.info.rows = rows;
        }
        self.persist();
        Ok(())
    }

    /// Updates a pane's last-known working directory, reported by shell
    /// integration on directory changes so a restarted shell reopens where it
    /// left off rather than at its spawn-time cwd. No-op for unknown panes.
    pub fn update_pane_cwd(&self, pane_id: &str, cwd: String) -> Result<(), String> {
        // This value arrives over the control socket from in-pane shell
        // integration, so treat it as untrusted: reject control characters
        // (newlines, NULs, escape sequences) and absurd lengths before letting
        // it into persisted state and the UI. A legitimate working directory
        // never contains them.
        if cwd.len() > MAX_PANE_CWD_LEN {
            return Err(format!(
                "pane cwd exceeds {MAX_PANE_CWD_LEN} bytes; refusing to persist"
            ));
        }
        if cwd.chars().any(|ch| ch.is_control()) {
            return Err("pane cwd contains control characters; refusing to persist".to_string());
        }
        // The cwd becomes a file-server root (see `pane_file_roots`), so only accept
        // a value that names a real directory. A legitimate shell-integration report
        // is always an existing absolute directory; rejecting anything else keeps a
        // malformed or non-existent path (or a plain file) from being installed as a
        // servable root. (This intentionally does not constrain *which* directory —
        // panes legitimately run in arbitrary project dirs outside the workspace.)
        let candidate = std::path::Path::new(&cwd);
        if !candidate.is_absolute() {
            return Err("pane cwd must be an absolute path; refusing to persist".to_string());
        }
        if !candidate.is_dir() {
            return Err("pane cwd is not an existing directory; refusing to persist".to_string());
        }
        let changed = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            match model.panes.get_mut(pane_id) {
                Some(pane) if pane.info.cwd != cwd => {
                    pane.info.cwd = cwd.clone();
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.persist();
            // Persisting alone makes the cwd survive a restart, but the live UI only
            // learns a pane's cwd at spawn and on a full pane-list refetch. Without
            // this event a tab's shown directory (and the context-menu working dir)
            // would stay pinned to the spawn-time cwd for the rest of the session,
            // only catching up on the next full load (e.g. a restart). Emit a
            // surgical update so each tab tracks the directory it is actually in.
            self.emit(QmuxEvent::new(
                "pane.cwd_changed",
                Some(pane_id.to_string()),
                None,
                json!({ "paneId": pane_id, "cwd": cwd }),
            ));
        }
        Ok(())
    }

    /// Records the newest OSC 0/2 title for a pane without changing its durable
    /// user/generated `title`. Live callers receive the normalized value so the
    /// event stream and the recovery snapshot use identical text.
    pub fn update_last_osc_title(
        &self,
        pane_id: &str,
        raw_title: &str,
    ) -> Result<Option<String>, String> {
        let title = sanitize_last_osc_title(raw_title);
        let changed = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let Some(pane) = model.panes.get_mut(pane_id) else {
                // Native title callbacks can arrive after pane teardown. Treat
                // that as a harmless late delivery rather than surfacing an
                // error from the AppKit main thread.
                return Ok(title);
            };
            if pane.info.last_osc_title == title {
                false
            } else {
                pane.info.last_osc_title = title.clone();
                true
            }
        };
        if changed {
            self.schedule_last_osc_title_persist();
        }
        Ok(title)
    }

    fn schedule_last_osc_title_persist(&self) {
        if !self.inner.persist_enabled.load(Ordering::Relaxed) {
            return;
        }
        if cfg!(test) {
            self.persist_now();
            return;
        }
        if self
            .inner
            .last_osc_title_persist_scheduled
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let state = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(LAST_OSC_TITLE_PERSIST_INTERVAL);
            state
                .inner
                .last_osc_title_persist_scheduled
                .store(false, Ordering::SeqCst);
            // The ordinary persister adds its short coalescing window and owns
            // snapshot ordering. A clean exit may already have committed and
            // disabled persistence, in which case this is a no-op.
            state.persist();
        });
    }

    pub fn rename_pane(&self, pane_id: &str, title: String) -> Result<PaneInfo, String> {
        let title = title.trim().to_string();
        if title.is_empty() {
            return Err("tab name cannot be empty".to_string());
        }
        let info = {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let pane = model
                .panes
                .get_mut(pane_id)
                .ok_or_else(|| format!("pane {pane_id} was not found"))?;
            pane.info.title = title;
            pane.info.clone()
        };
        self.persist();
        Ok(info)
    }

    pub fn set_pane_recovered(&self, pane_id: &str, recovered: bool) -> Result<(), String> {
        {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            let pane = model
                .panes
                .get_mut(pane_id)
                .ok_or_else(|| format!("pane {pane_id} was not found"))?;
            pane.info.recovered = recovered;
        }
        self.persist();
        Ok(())
    }

    #[cfg(test)]
    pub fn mark_pane_status(&self, pane_id: &str, status: PaneStatus) -> Result<(), String> {
        {
            let mut model = self
                .inner
                .model
                .lock()
                .map_err(|_| "model lock poisoned".to_string())?;
            if let Some(pane) = model.panes.get_mut(pane_id) {
                pane.info.status = status;
            }
        }
        self.persist();
        Ok(())
    }
}

/// Builds a resume request for an agent that was bound to a shell pane at shutdown,
/// when its session is still resumable. Requires a non-empty session id and skips a
/// session whose recorded transcript file no longer exists, since resuming a deleted
/// session would just error out in the new shell.
fn shell_agent_resume(agent: &AgentInfo) -> Option<ShellAgentResume> {
    let session_id = agent
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())?;
    if let Some(transcript_path) = agent.transcript_path.as_deref()
        && !std::path::Path::new(transcript_path).exists()
    {
        return None;
    }
    Some(ShellAgentResume {
        adapter: agent.adapter.clone(),
        session_id: session_id.to_string(),
        cwd: agent.worktree_dir.clone(),
    })
}

pub(crate) fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

pub(crate) fn recent_session_key(
    adapter: &str,
    session_id: Option<&str>,
    transcript_path: Option<&str>,
) -> Option<String> {
    if let Some(session_id) = session_id.map(str::trim).filter(|id| !id.is_empty()) {
        return Some(format!("{adapter}:session:{session_id}"));
    }
    transcript_path
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(|path| format!("{adapter}:transcript:{path}"))
}

fn agent_recent_session_key(agent: &AgentInfo) -> Option<String> {
    recent_session_key(
        &agent.adapter,
        agent.session_id.as_deref(),
        agent.transcript_path.as_deref(),
    )
}

/// Where `upsert_recent_session_for_agent_locked` may take a preview/line-count
/// fallback from when neither the in-memory turns nor the cached entry have one.
enum RecentSessionMeta {
    /// Never touch the disk. Callers inside long-lived lock scopes use this;
    /// the returned `wants_disk_meta` tells them (via
    /// `AppState::upsert_recent_session_for_agent`) that a read would help.
    CacheOnly,
    /// Transcript meta the caller read from disk *outside* the model lock.
    Loaded {
        preview: Option<String>,
        line_count: usize,
    },
}

struct RecentSessionUpsert {
    changed: bool,
    /// The transcript path worth reading for preview/line-count, set only in
    /// `CacheOnly` mode when the cache had neither.
    wants_disk_meta: Option<String>,
}

impl RecentSessionUpsert {
    fn unchanged() -> Self {
        Self {
            changed: false,
            wants_disk_meta: None,
        }
    }
}

fn upsert_recent_session_for_agent_locked(
    model: &mut Model,
    agent: &AgentInfo,
    now: u128,
    touch: bool,
    meta: RecentSessionMeta,
) -> RecentSessionUpsert {
    if model
        .groups
        .get(&agent.group_id)
        .is_some_and(|group| group.scope == WorkspaceScope::Research)
    {
        return RecentSessionUpsert::unchanged();
    }
    let Some(key) = agent_recent_session_key(agent) else {
        return RecentSessionUpsert::unchanged();
    };

    if agent
        .session_id
        .as_deref()
        .is_some_and(|id| !id.trim().is_empty())
        && let Some(transcript_path) = agent.transcript_path.as_deref()
        && let Some(transcript_key) =
            recent_session_key(&agent.adapter, None, Some(transcript_path))
        && transcript_key != key
    {
        model.recent_sessions.remove(&transcript_key);
    }

    let existing = model.recent_sessions.get(&key).cloned();
    let turns = model.turns.get(&agent.id);
    let turn_preview = turns.and_then(|turns| first_user_turn_preview(turns));
    let mut line_count = turns.map(Vec::len).unwrap_or(0);
    let mut preview = turn_preview.or_else(|| {
        existing
            .as_ref()
            .and_then(|session| session.preview.clone())
    });

    // Prefer the line count cached on the previous recent-session entry before
    // considering the disk. An actively-growing session keeps its turns in
    // memory (line_count above), so the on-disk fallback below only serves cold
    // sessions whose files aren't changing — making the cached count a faithful
    // substitute.
    if line_count == 0 {
        line_count = existing
            .as_ref()
            .map(|session| session.line_count)
            .unwrap_or(0);
    }

    // This runs under the model lock, so the transcript file is never read
    // here: reading and parsing a whole (possibly cold, possibly huge) JSONL
    // would stall every other thread — including main-thread input handling —
    // behind that I/O. Callers either supply meta they read outside the lock
    // (`Loaded`) or get the path back and re-enter with the data
    // (`AppState::upsert_recent_session_for_agent`).
    let mut wants_disk_meta = None;
    if (preview.is_none() || line_count == 0)
        && let Some(transcript_path) = agent.transcript_path.as_deref()
    {
        match &meta {
            RecentSessionMeta::CacheOnly => {
                wants_disk_meta = Some(transcript_path.to_string());
            }
            RecentSessionMeta::Loaded {
                preview: disk_preview,
                line_count: disk_line_count,
            } => {
                if preview.is_none() {
                    preview = disk_preview.clone();
                }
                if line_count == 0 {
                    line_count = *disk_line_count;
                }
            }
        }
    }

    let created_at = existing
        .as_ref()
        .map(|session| session.created_at)
        .unwrap_or(agent.created_at);
    let previous_active_at = existing
        .as_ref()
        .map(|session| session.last_active_at)
        .unwrap_or(agent.created_at);
    let last_active_at = if touch { now } else { previous_active_at };

    let next = RecentSessionInfo {
        id: key.clone(),
        adapter: agent.adapter.clone(),
        group_id: Some(agent.group_id.clone()),
        session_id: agent.session_id.clone(),
        transcript_path: agent.transcript_path.clone(),
        worktree_dir: agent.worktree_dir.clone(),
        branch: agent.branch.clone(),
        model: agent.model.clone(),
        effort: agent.effort.clone(),
        parent_id: agent.parent_id.clone(),
        fork_point: agent.fork_point.clone(),
        root_session_id: agent.root_session_id.clone(),
        preview,
        line_count,
        last_active_at,
        created_at,
        pane_id: agent.pane_id.clone(),
        agent_id: Some(agent.id.clone()),
        status: Some(agent.status),
        missing: false,
    };

    if existing.as_ref() == Some(&next) {
        return RecentSessionUpsert {
            changed: false,
            wants_disk_meta,
        };
    }
    // Coarsen pure re-touches. A busy agent's hooks re-touch its session
    // several times a second for the whole run; each fresh `last_active_at`
    // made the entry differ, marked the state file dirty, and kept the
    // debounced persister rewriting (and fsyncing) state.json every window
    // for the duration. When nothing but the activity stamp moved, only
    // re-stamp once it has drifted by the coarseness — recency ordering
    // (Home, spawn-cwd inheritance) is unaffected by a few seconds of slack,
    // and any real change (status, transcript, preview) still lands with a
    // fresh stamp immediately via the comparison below.
    if touch
        && let Some(existing) = existing.as_ref()
        && now.saturating_sub(previous_active_at) < RECENT_SESSION_TOUCH_COARSENESS_MS
    {
        let comparable = RecentSessionInfo {
            last_active_at: previous_active_at,
            ..next.clone()
        };
        if *existing == comparable {
            return RecentSessionUpsert {
                changed: false,
                wants_disk_meta,
            };
        }
    }
    model.recent_sessions.insert(key, next);
    RecentSessionUpsert {
        changed: true,
        wants_disk_meta,
    }
}

fn clear_recent_session_binding_locked(
    model: &mut Model,
    agent_id: Option<&str>,
    pane_id: Option<&str>,
) {
    for session in model.recent_sessions.values_mut() {
        if agent_id.is_some_and(|agent_id| session.agent_id.as_deref() == Some(agent_id))
            || pane_id.is_some_and(|pane_id| session.pane_id.as_deref() == Some(pane_id))
        {
            session.agent_id = None;
            session.pane_id = None;
            session.status = None;
        }
    }
}

fn enrich_recent_session_locked(
    model: &Model,
    mut session: RecentSessionInfo,
) -> RecentSessionInfo {
    session.agent_id = None;
    session.pane_id = None;
    session.status = None;

    if let Some(agent) = model
        .agents
        .values()
        .find(|agent| recent_session_matches_agent(&session, agent) && agent.pane_id.is_some())
        .or_else(|| {
            model
                .agents
                .values()
                .find(|agent| recent_session_matches_agent(&session, agent))
        })
    {
        session.agent_id = Some(agent.id.clone());
        session.pane_id = agent.pane_id.clone();
        session.status = Some(agent.status);
        session.worktree_dir = agent.worktree_dir.clone();
        session.branch = agent.branch.clone();
        session.model = agent.model.clone();
    }

    session
}

fn recent_session_matches_agent(session: &RecentSessionInfo, agent: &AgentInfo) -> bool {
    if session.adapter != agent.adapter {
        return false;
    }
    match (session.session_id.as_deref(), agent.session_id.as_deref()) {
        (Some(left), Some(right)) if !left.trim().is_empty() && left == right => return true,
        _ => {}
    }
    matches!(
        (
        session.transcript_path.as_deref(),
        agent.transcript_path.as_deref(),
        ),
        (Some(left), Some(right)) if !left.trim().is_empty() && left == right
    )
}

fn recent_session_missing(session: &RecentSessionInfo) -> bool {
    if session.pane_id.is_some() {
        return false;
    }
    if !std::path::Path::new(&session.worktree_dir).is_dir() {
        return true;
    }
    session
        .transcript_path
        .as_deref()
        .is_some_and(|path| !std::path::Path::new(path).is_file())
}

fn trimmed_id(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|id| !id.is_empty())
}

/// The session every branch in `agent`'s lineage descends from.
///
/// Forking stamps `root_session_id` on the child only — the root keeps `None` and
/// names itself by its own `session_id` — so lineage membership can't be a plain
/// equality check on `root_session_id`. Both sides fall back the same way.
fn lineage_root_session_id(agent: &AgentInfo) -> Option<String> {
    trimmed_id(agent.root_session_id.as_deref())
        .or_else(|| trimmed_id(agent.session_id.as_deref()))
        .map(str::to_string)
}

fn recent_session_in_lineage(session: &RecentSessionInfo, root: &str) -> bool {
    trimmed_id(session.root_session_id.as_deref()) == Some(root)
        || trimmed_id(session.session_id.as_deref()) == Some(root)
}

fn agent_in_lineage(agent: &AgentInfo, root: &str) -> bool {
    trimmed_id(agent.root_session_id.as_deref()) == Some(root)
        || trimmed_id(agent.session_id.as_deref()) == Some(root)
}

fn branch_from_recent_session(session: RecentSessionInfo, root: &str) -> BranchInfo {
    BranchInfo {
        is_root: trimmed_id(session.session_id.as_deref()) == Some(root),
        live: session.pane_id.is_some(),
        recent_session_id: Some(session.id),
        agent_id: session.agent_id,
        pane_id: session.pane_id,
        session_id: session.session_id,
        adapter: session.adapter,
        preview: session.preview,
        status: session.status,
        fork_point: session.fork_point,
        missing: session.missing,
        last_active_at: session.last_active_at,
        created_at: session.created_at,
    }
}

fn branch_from_agent(agent: &AgentInfo, root: &str) -> BranchInfo {
    BranchInfo {
        recent_session_id: None,
        agent_id: Some(agent.id.clone()),
        pane_id: agent.pane_id.clone(),
        session_id: agent.session_id.clone(),
        adapter: agent.adapter.clone(),
        preview: None,
        status: Some(agent.status),
        fork_point: agent.fork_point.clone(),
        is_root: trimmed_id(agent.session_id.as_deref()) == Some(root),
        live: agent.pane_id.is_some(),
        // A live agent's files are open by definition; nothing to resume, so
        // nothing to be missing.
        missing: false,
        last_active_at: agent.created_at,
        created_at: agent.created_at,
    }
}

fn recent_sessions_sorted(model: &Model) -> Vec<RecentSessionInfo> {
    let mut sessions = model.recent_sessions.values().cloned().collect::<Vec<_>>();
    sessions.sort_by(|left, right| {
        right
            .last_active_at
            .cmp(&left.last_active_at)
            .then(right.created_at.cmp(&left.created_at))
            .then(left.id.cmp(&right.id))
    });
    sessions
}

fn prune_recent_sessions_locked(model: &mut Model) {
    let keep = recent_sessions_sorted(model)
        .into_iter()
        .take(MAX_RECENT_SESSIONS)
        .map(|session| session.id)
        .collect::<HashSet<_>>();
    model
        .recent_sessions
        .retain(|session_id, _session| keep.contains(session_id));
}

fn first_user_turn_preview(turns: &[Turn]) -> Option<String> {
    turns
        .iter()
        .filter(|turn| turn.role == "user")
        .find_map(|turn| {
            turn.blocks.iter().find_map(|block| match block {
                crate::transcript::TurnBlock::Text { text } => preview_text(text),
                _ => None,
            })
        })
}

fn preview_text(raw: &str) -> Option<String> {
    let normalized = raw
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.is_empty() {
        return None;
    }
    let chars = normalized.chars().collect::<Vec<_>>();
    if chars.len() <= RECENT_SESSION_PREVIEW_MAX_CHARS {
        return Some(normalized);
    }
    Some(
        chars
            .into_iter()
            .take(RECENT_SESSION_PREVIEW_MAX_CHARS.saturating_sub(3))
            .collect::<String>()
            .trim_end()
            .to_string()
            + "...",
    )
}

/// The effective sidebar order: every live pane id, `pane_order` first, then any
/// panes missing from it (sorted by id for determinism). Shared by `ordered_panes`
/// and depth normalization so they always agree on ordering.
fn ordered_pane_ids(model: &Model) -> Vec<String> {
    let mut ids = Vec::with_capacity(model.panes.len());
    let mut seen = HashSet::with_capacity(model.panes.len());

    for pane_id in &model.pane_order {
        if model.panes.contains_key(pane_id) && seen.insert(pane_id.clone()) {
            ids.push(pane_id.clone());
        }
    }

    let mut missing_from_order = model
        .panes
        .keys()
        .filter(|pane_id| !seen.contains(*pane_id))
        .cloned()
        .collect::<Vec<_>>();
    missing_from_order.sort();
    ids.extend(missing_from_order);

    ids
}

fn ordered_group_ids(model: &Model) -> Vec<String> {
    let mut ids = Vec::with_capacity(model.groups.len());
    let mut seen = HashSet::with_capacity(model.groups.len());

    for group_id in &model.group_order {
        if model.groups.contains_key(group_id) && seen.insert(group_id.clone()) {
            ids.push(group_id.clone());
        }
    }

    let mut missing_from_order = model
        .groups
        .keys()
        .filter(|group_id| !seen.contains(*group_id))
        .cloned()
        .collect::<Vec<_>>();
    missing_from_order.sort();
    ids.extend(missing_from_order);
    ids
}

/// The durable Research sidebar order. Older state files have no explicit
/// vector, so missing ids fall back to the legacy updated-at order once, then
/// hydration persists that normalized result.
fn ordered_research_tree_ids(model: &Model) -> Vec<String> {
    let mut ids = Vec::with_capacity(model.research_trees.len());
    let mut seen = HashSet::with_capacity(model.research_trees.len());

    for tree_id in &model.research_tree_order {
        if model.research_trees.contains_key(tree_id) && seen.insert(tree_id.clone()) {
            ids.push(tree_id.clone());
        }
    }

    let mut missing = model
        .research_trees
        .values()
        .filter(|tree| !seen.contains(&tree.id))
        .collect::<Vec<_>>();
    missing.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then(left.id.cmp(&right.id))
    });
    ids.extend(missing.into_iter().map(|tree| tree.id.clone()));
    ids
}

fn ordered_groups(model: &Model) -> Vec<GroupInfo> {
    ordered_group_ids(model)
        .into_iter()
        .filter_map(|group_id| model.groups.get(&group_id).cloned())
        .collect()
}

fn restore_closed_agent_snapshot_locked(
    model: &mut Model,
    pane: &PaneInfo,
    agent_snapshot: &ClosedPaneAgentSnapshot,
    attach_to_pane: bool,
    queue_shell_resume: bool,
) {
    let mut agent = agent_snapshot.agent.clone();
    if attach_to_pane {
        agent.pane_id = Some(pane.id.clone());
        agent.orphaned_queue_pane_id = None;
    } else {
        let has_queue = !agent_snapshot.queued_turns.is_empty();
        agent.pane_id = None;
        agent.orphaned_queue_pane_id = has_queue.then(|| pane.id.clone());
        agent.status = AgentStatus::Idle;
    }

    if queue_shell_resume && let Some(resume) = shell_agent_resume(&agent) {
        model.shell_agent_resumes.insert(pane.id.clone(), resume);
    }

    let agent_id = agent.id.clone();
    model.agents.insert(agent_id.clone(), agent);
    if agent_snapshot.turns.is_empty() {
        model.turns.remove(&agent_id);
    } else {
        model
            .turns
            .insert(agent_id.clone(), agent_snapshot.turns.clone());
    }
    if agent_snapshot.queued_turns.is_empty() {
        model.agent_turn_queues.remove(&agent_id);
    } else {
        model.agent_turn_queues.insert(
            agent_id.clone(),
            agent_snapshot.queued_turns.iter().cloned().collect(),
        );
    }
    match agent_snapshot
        .draft
        .clone()
        .filter(|draft| !draft.trim().is_empty())
    {
        Some(draft) => {
            model.agent_drafts.insert(agent_id, draft);
        }
        None => {
            model.agent_drafts.remove(&agent_id);
        }
    }
}

fn prune_agent_locked(model: &mut Model, agent_id: &str) {
    if let Some(agent) = model.agents.get(agent_id).cloned() {
        upsert_recent_session_for_agent_locked(
            model,
            &agent,
            now_millis(),
            true,
            RecentSessionMeta::CacheOnly,
        );
    }
    model.agents.remove(agent_id);
    model.turns.remove(agent_id);
    model.agent_turn_queues.remove(agent_id);
    model.agent_drafts.remove(agent_id);
    model.agent_typing.remove(agent_id);
    model.agent_pending_pause.remove(agent_id);
    model.agent_draining.remove(agent_id);
    model.agent_send_tracking.remove(agent_id);
    model.agent_activity.remove(agent_id);
    model.agent_status_activity.remove(agent_id);
    model.agent_active_subagents.remove(agent_id);
    model.agent_escape_watch.remove(agent_id);
    model
        .agent_submit_watch
        .retain(|(watched_agent, _)| watched_agent != agent_id);
    clear_recent_session_binding_locked(model, Some(agent_id), None);
}

/// Bumps the per-agent activity counter; see `Model::agent_activity`.
fn bump_agent_activity_locked(model: &mut Model, agent_id: &str) {
    let seq = model
        .agent_activity
        .entry(agent_id.to_string())
        .or_insert(0);
    *seq = seq.wrapping_add(1);
}

/// Bumps the per-agent status/lifecycle counter; see `Model::agent_status_activity`.
fn bump_agent_status_activity_locked(model: &mut Model, agent_id: &str) {
    let seq = model
        .agent_status_activity
        .entry(agent_id.to_string())
        .or_insert(0);
    *seq = seq.wrapping_add(1);
}

/// Every research-node mutation is a tree mutation for ordering/recency
/// purposes; failure and detachment paths previously skipped this, leaving
/// `updated_at` stale exactly when a tree last changed by failing.
fn touch_research_tree_locked(model: &mut Model, tree_id: &str, now: u128) {
    if let Some(tree) = model.research_trees.get_mut(tree_id) {
        tree.updated_at = now;
    }
}

/// Splits research-owned panes and agents out of the ordinary groups used by
/// the original research implementation. The migration runs for version-2
/// state and for version-3 snapshots written by the short-lived transitional
/// build where scope existed but research still referenced Terminal groups.
fn migrate_legacy_research_workspaces(
    state: &AppState,
    persisted: &mut PersistedState,
) -> (bool, Vec<String>) {
    let mut changed = drop_research_recent_sessions(persisted);
    let mut warnings = Vec::new();

    // Backfill tree ownership from its root node before deciding which groups
    // need to split. A missing root is handled by structural reconciliation.
    for tree in persisted.research_trees.values_mut() {
        if tree.workspace_id.trim().is_empty()
            && let Some(root) = persisted.research_nodes.get(&tree.root_node_id)
        {
            tree.workspace_id = root.group_id.clone();
            changed = true;
        }
    }

    let group_scope = persisted
        .groups
        .iter()
        .map(|group| (group.id.clone(), group.scope))
        .collect::<HashMap<_, _>>();
    let mut legacy_group_ids = persisted
        .research_trees
        .values()
        .filter_map(|tree| {
            (group_scope.get(&tree.workspace_id) != Some(&WorkspaceScope::Research))
                .then_some(tree.workspace_id.clone())
        })
        .filter(|id| !id.is_empty())
        .collect::<Vec<_>>();
    legacy_group_ids.sort();
    legacy_group_ids.dedup();

    for legacy_group_id in legacy_group_ids {
        let tree_ids = persisted
            .research_trees
            .values()
            .filter(|tree| tree.workspace_id == legacy_group_id)
            .map(|tree| tree.id.clone())
            .collect::<HashSet<_>>();
        let source_index = persisted
            .groups
            .iter()
            .position(|group| group.id == legacy_group_id);
        let legacy_dir = source_index
            .map(|index| persisted.groups[index].dir.clone())
            .or_else(|| {
                persisted
                    .research_trees
                    .values()
                    .filter(|tree| tree_ids.contains(&tree.id))
                    .filter_map(|tree| persisted.research_nodes.get(&tree.root_node_id))
                    .map(|root| root.worktree_dir.clone())
                    .find(|dir| !dir.trim().is_empty())
            });
        let Some(legacy_dir) = legacy_dir else {
            warnings.push(format!(
                "research workspace migration could not recover a folder for legacy group {legacy_group_id}"
            ));
            continue;
        };
        let source = source_index
            .map(|index| persisted.groups[index].clone())
            .unwrap_or_else(|| {
                let name = std::path::Path::new(&legacy_dir)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .filter(|name| !name.is_empty())
                    .unwrap_or("Recovered")
                    .to_string();
                GroupInfo {
                    id: legacy_group_id.clone(),
                    name,
                    name_override: Some("Recovered Research".to_string()),
                    dir: legacy_dir.clone(),
                    managed_dir: String::new(),
                    base_repo: None,
                    base_ref: Some("HEAD".to_string()),
                    parent_id: None,
                    created_at: now_millis(),
                    collapsed: false,
                    scope: WorkspaceScope::Terminal,
                    imported_research_archive_id: None,
                    agents: Vec::new(),
                }
            });
        let dir_key = research_workspace_dir_key(&legacy_dir);
        let existing_research_group = persisted
            .groups
            .iter()
            .find(|group| {
                group.scope == WorkspaceScope::Research
                    && research_workspace_dir_key(&group.dir) == dir_key
            })
            .cloned();
        let created_research_group = existing_research_group.is_none();
        let mut research_group = match existing_research_group {
            Some(group) => group,
            None => match crate::workspace::clone_group_record_for_scope(
                state,
                &source,
                WorkspaceScope::Research,
            ) {
                Ok(group) => group,
                Err(err) => {
                    warnings.push(format!(
                        "could not isolate legacy research group {legacy_group_id}: {err}"
                    ));
                    continue;
                }
            },
        };

        let research_pane_ids = persisted
            .research_nodes
            .values()
            .filter(|node| tree_ids.contains(&node.tree_id))
            .filter_map(|node| node.pane_id.clone())
            .collect::<HashSet<_>>();
        let research_agent_ids = persisted
            .research_nodes
            .values()
            .filter(|node| tree_ids.contains(&node.tree_id))
            .filter_map(|node| node.agent_id.clone())
            .collect::<HashSet<_>>();
        research_group
            .agents
            .extend(research_agent_ids.iter().cloned());
        research_group.agents.sort();
        research_group.agents.dedup();
        let mut updated_source = source_index.map(|index| persisted.groups[index].clone());
        if let Some(source) = &mut updated_source {
            source
                .agents
                .retain(|agent_id| !research_agent_ids.contains(agent_id));
        }
        if let Err(err) = crate::workspace::write_group_manifest(&research_group) {
            warnings.push(format!(
                "could not finish migrated research workspace {}: {err}",
                research_group.id
            ));
            if created_research_group {
                let _ = std::fs::remove_dir_all(&research_group.managed_dir);
            }
            continue;
        }
        if let Some(source) = &updated_source
            && let Err(err) = crate::workspace::write_group_manifest(source)
        {
            warnings.push(format!(
                "could not update legacy terminal workspace {legacy_group_id}: {err}"
            ));
            if created_research_group {
                let _ = std::fs::remove_dir_all(&research_group.managed_dir);
            } else if let Some(original) = persisted
                .groups
                .iter()
                .find(|group| group.id == research_group.id)
            {
                let _ = crate::workspace::write_group_manifest(original);
            }
            continue;
        }

        let research_group_id = research_group.id.clone();
        for tree in persisted.research_trees.values_mut() {
            if tree.workspace_id == legacy_group_id {
                tree.workspace_id = research_group_id.clone();
            }
        }
        for node in persisted.research_nodes.values_mut() {
            if tree_ids.contains(&node.tree_id) {
                node.group_id = research_group_id.clone();
            }
        }
        for pane in &mut persisted.panes {
            if pane.group_id == legacy_group_id && research_pane_ids.contains(&pane.id) {
                pane.group_id = research_group_id.clone();
                pane.depth = 0;
            }
        }
        for agent in &mut persisted.agents {
            if agent.group_id == legacy_group_id && research_agent_ids.contains(&agent.id) {
                agent.group_id = research_group_id.clone();
            }
        }
        if let Some(index) = persisted
            .groups
            .iter()
            .position(|group| group.id == research_group_id)
        {
            persisted.groups[index] = research_group;
        } else {
            let insert_index = source_index.map_or(persisted.groups.len(), |index| index + 1);
            persisted.groups.insert(insert_index, research_group);
            if let Some(order_index) = persisted
                .group_order
                .iter()
                .position(|id| id == &legacy_group_id)
            {
                persisted
                    .group_order
                    .insert(order_index + 1, research_group_id.clone());
            } else {
                persisted.group_order.push(research_group_id.clone());
            }
        }
        if let (Some(index), Some(source)) = (source_index, updated_source) {
            persisted.groups[index] = source;
        }
        let source_still_used = persisted
            .panes
            .iter()
            .any(|pane| pane.group_id == legacy_group_id)
            || persisted
                .agents
                .iter()
                .any(|agent| agent.group_id == legacy_group_id);
        if !source_still_used {
            persisted.groups.retain(|group| group.id != legacy_group_id);
            persisted.group_order.retain(|id| id != &legacy_group_id);
        }
        changed = true;
    }

    // Split groups are viewport constructs and cannot span modes. Drop only the
    // invalid split; the normal layout reconciliation keeps all valid siblings.
    let pane_group = persisted
        .panes
        .iter()
        .map(|pane| (pane.id.clone(), pane.group_id.clone()))
        .collect::<HashMap<_, _>>();
    let scope_by_group = persisted
        .groups
        .iter()
        .map(|group| (group.id.clone(), group.scope))
        .collect::<HashMap<_, _>>();
    let split_count = persisted.pane_splits.len();
    persisted.pane_splits.retain(|split| {
        let mut scopes = split.pane_ids.iter().filter_map(|pane_id| {
            pane_group
                .get(pane_id)
                .and_then(|group_id| scope_by_group.get(group_id))
        });
        let first = scopes.next();
        first.is_none_or(|first| scopes.all(|scope| scope == first))
    });
    changed |= persisted.pane_splits.len() != split_count;

    (changed, warnings)
}

fn research_workspace_dir_key(dir: &str) -> std::path::PathBuf {
    let path = std::path::PathBuf::from(dir);
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn drop_research_recent_sessions(persisted: &mut PersistedState) -> bool {
    let agent_ids = persisted
        .research_nodes
        .values()
        .filter_map(|node| node.agent_id.clone())
        .collect::<HashSet<_>>();
    let pane_ids = persisted
        .research_nodes
        .values()
        .filter_map(|node| node.pane_id.clone())
        .collect::<HashSet<_>>();
    let session_ids = persisted
        .research_nodes
        .values()
        .filter_map(|node| node.native_session_id.clone())
        .collect::<HashSet<_>>();
    let transcript_paths = persisted
        .research_nodes
        .values()
        .filter_map(|node| node.transcript_path.clone())
        .collect::<HashSet<_>>();
    let before = persisted.recent_sessions.len();
    persisted.recent_sessions.retain(|session| {
        !session
            .agent_id
            .as_ref()
            .is_some_and(|id| agent_ids.contains(id))
            && !session
                .pane_id
                .as_ref()
                .is_some_and(|id| pane_ids.contains(id))
            && !session
                .session_id
                .as_ref()
                .is_some_and(|id| session_ids.contains(id))
            && !session
                .transcript_path
                .as_ref()
                .is_some_and(|path| transcript_paths.contains(path))
    });
    before != persisted.recent_sessions.len()
}

/// Adapter contract this mapping (and research completion as a whole) depends
/// on: a research-capable adapter must report `Done`/`Idle` at end-of-turn
/// while its process stays alive, and must report subagent start/stop boundaries
/// when foreground idleness can coexist with background work. An adapter that instead *rests* at
/// `AwaitingInput` after a normal turn would leave its nodes Researching…
/// forever (no completion, no snapshot, no retirement, no follow-ups); one
/// whose process exits on completion relies on `detach_research_pane`'s
/// agent-finished check to settle Complete instead of Failed.
fn research_status_for_agent(
    status: AgentStatus,
    has_active_subagents: bool,
) -> ResearchNodeStatus {
    match status {
        AgentStatus::Starting => ResearchNodeStatus::Starting,
        // AwaitingInput is a mid-turn pause (elicitation / clarifying question),
        // not completion: the adapters return to Running once the user answers,
        // so the node must stay live or retirement would kill the waiting agent.
        AgentStatus::Running | AgentStatus::AwaitingPermission | AgentStatus::AwaitingInput => {
            ResearchNodeStatus::Running
        }
        AgentStatus::Done | AgentStatus::Idle if has_active_subagents => {
            ResearchNodeStatus::Running
        }
        AgentStatus::Done | AgentStatus::Idle => ResearchNodeStatus::Complete,
        AgentStatus::Failed => ResearchNodeStatus::Failed,
    }
}

fn validate_research_workspace_available(workspace: &GroupInfo) -> Result<(), String> {
    let dir = std::path::Path::new(&workspace.dir);
    if !dir.is_dir() {
        return Err(format!(
            "research folder '{}' is unavailable; restore it at that path before launching another run for '{}'",
            workspace.dir,
            workspace
                .name_override
                .as_deref()
                .unwrap_or(&workspace.name)
        ));
    }
    Ok(())
}

fn remove_group_without_open_panes_locked(
    model: &mut Model,
    group_id: &str,
    preserve_research: bool,
) -> bool {
    if model
        .panes
        .values()
        .any(|pane| pane.info.group_id == group_id)
    {
        return false;
    }

    if preserve_research
        && model
            .groups
            .get(group_id)
            .is_some_and(|group| group.scope == WorkspaceScope::Research)
    {
        return false;
    }

    // Legacy safeguard: after workspace migration every research node should
    // reference a Research-scoped group, but keep old or partially recovered
    // state from losing its launch context.
    if model
        .research_trees
        .values()
        .any(|tree| tree.workspace_id == group_id)
    {
        return false;
    }

    let agent_ids = model
        .agents
        .values()
        .filter(|agent| agent.group_id == group_id)
        .map(|agent| agent.id.clone())
        .collect::<Vec<_>>();
    let pruned_agents = !agent_ids.is_empty();
    for agent_id in agent_ids {
        prune_agent_locked(model, &agent_id);
    }
    let removed = model.groups.remove(group_id).is_some();
    let order_len_before = model.group_order.len();
    model.group_order.retain(|id| id != group_id);
    removed || pruned_agents || order_len_before != model.group_order.len()
}

fn ordered_panes(model: &Model) -> Vec<PaneInfo> {
    ordered_pane_ids(model)
        .into_iter()
        .filter_map(|pane_id| {
            model.panes.get(&pane_id).map(|pane| {
                let mut info = pane.info.clone();
                info.depth = model.pane_depth.get(&pane_id).copied().unwrap_or(0);
                info
            })
        })
        .collect()
}

/// Clamps `pane_depth` to the validity invariant along the effective order (first
/// pane depth 0; each depth <= previous + 1; capped at `MAX_PANE_DEPTH`) and drops
/// entries for panes that no longer exist. Idempotent. This is what re-levels
/// orphaned children when their parent pane is removed.
fn normalize_pane_depths(model: &mut Model) {
    let ids = ordered_pane_ids(model);
    let id_set: HashSet<&String> = ids.iter().collect();
    model.pane_depth.retain(|id, _| id_set.contains(id));

    let mut prev_depth_by_group: HashMap<String, u16> = HashMap::new();
    for id in ids.iter() {
        let group_id = model
            .panes
            .get(id)
            .map(|pane| pane.info.group_id.clone())
            .unwrap_or_default();
        let raw = model.pane_depth.get(id).copied().unwrap_or(0);
        let ceiling = prev_depth_by_group
            .get(&group_id)
            .map_or(0, |prev| (prev + 1).min(MAX_PANE_DEPTH));
        let depth = raw.min(ceiling);
        if depth == 0 {
            model.pane_depth.remove(id);
        } else {
            model.pane_depth.insert(id.clone(), depth);
        }
        prev_depth_by_group.insert(group_id, depth);
    }
}

fn normalize_pane_splits_locked(model: &mut Model) {
    model.pane_splits =
        normalized_pane_splits(model, model.pane_splits.clone(), false).unwrap_or_default();
}

fn normalized_pane_splits(
    model: &Model,
    splits: Vec<PaneSplitInfo>,
    strict: bool,
) -> Result<Vec<PaneSplitInfo>, String> {
    let ordered = ordered_panes(model);
    let mut pane_positions: HashMap<String, (String, usize)> = HashMap::new();
    let mut group_indexes: HashMap<String, usize> = HashMap::new();
    for pane in ordered {
        let index = group_indexes.entry(pane.group_id.clone()).or_default();
        pane_positions.insert(pane.id, (pane.group_id, *index));
        *index += 1;
    }

    let mut result = Vec::new();
    let mut used_panes = HashSet::new();
    let mut used_split_ids = HashSet::new();

    for split in splits {
        let id = split.id.trim().to_string();
        if id.is_empty() {
            if strict {
                return Err("pane split id cannot be empty".to_string());
            }
            continue;
        }
        if used_split_ids.contains(&id) {
            if strict {
                return Err(format!("pane split {id} is duplicated"));
            }
            continue;
        }

        let mut pane_ids = Vec::new();
        let mut local_seen = HashSet::new();
        for pane_id in split.pane_ids {
            if !local_seen.insert(pane_id.clone()) {
                if strict {
                    return Err(format!("pane split {id} contains duplicate pane {pane_id}"));
                }
                continue;
            }
            if !pane_positions.contains_key(&pane_id) {
                if strict {
                    return Err(format!("pane split {id} references missing pane {pane_id}"));
                }
                continue;
            }
            if used_panes.contains(&pane_id) {
                if strict {
                    return Err(format!("pane {pane_id} appears in multiple splits"));
                }
                continue;
            }
            pane_ids.push(pane_id);
        }

        if pane_ids.len() < 2 {
            continue;
        }

        let Some((group_id, _)) = pane_positions.get(&pane_ids[0]).cloned() else {
            continue;
        };
        if pane_ids
            .iter()
            .any(|pane_id| pane_positions.get(pane_id).map(|(group, _)| group) != Some(&group_id))
        {
            if strict {
                return Err(format!("pane split {id} spans multiple groups"));
            }
            continue;
        }

        pane_ids.sort_by_key(|pane_id| {
            pane_positions
                .get(pane_id)
                .map(|(_, index)| *index)
                .unwrap_or(usize::MAX)
        });
        let contiguous = pane_ids.windows(2).all(|pair| {
            let Some((_, left)) = pane_positions.get(&pair[0]) else {
                return false;
            };
            let Some((_, right)) = pane_positions.get(&pair[1]) else {
                return false;
            };
            *right == *left + 1
        });
        if !contiguous {
            if strict {
                return Err(format!("pane split {id} must contain adjacent tabs"));
            }
            continue;
        }

        for pane_id in &pane_ids {
            used_panes.insert(pane_id.clone());
        }
        used_split_ids.insert(id.clone());
        let pane_id_set = pane_ids.iter().cloned().collect::<HashSet<_>>();
        let sizes = split
            .sizes
            .into_iter()
            .filter(|(pane_id, size)| {
                pane_id_set.contains(pane_id) && size.is_finite() && *size > 0.0
            })
            .collect();
        let intent = split
            .intent
            .into_iter()
            .filter(|(pane_id, entry)| {
                pane_id_set.contains(pane_id)
                    && entry.kind == "inserted-relative"
                    && pane_id != &entry.anchor_pane_id
                    && pane_id_set.contains(&entry.anchor_pane_id)
                    && matches!(entry.position.as_str(), "above" | "below")
                    && matches!(
                        entry.source.as_str(),
                        "command" | "join" | "drag-half" | "drag-divider"
                    )
                    && entry.created_at.is_finite()
                    && entry.created_at >= 0.0
            })
            .collect();

        result.push(PaneSplitInfo {
            id,
            pane_ids,
            sizes,
            intent,
        });
    }

    Ok(result)
}

fn prompts_match(actual: &str, expected: &str) -> bool {
    let actual = normalize_prompt(actual);
    let expected = normalize_prompt(expected);
    actual == expected
}

fn normalize_prompt(prompt: &str) -> String {
    prompt.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn random_token() -> Result<String, String> {
    // 256 bits from the OS CSPRNG (getentropy/getrandom on macOS and Linux). A
    // failure here is rare (no secure entropy source) but can be transient in some
    // sandboxes, so retry a few times before giving up. We never fall back to a
    // predictable time/pid-derived secret that would leave the control socket
    // guessable; instead the error propagates so a single pane fails to launch
    // rather than the whole process aborting.
    let mut bytes = [0u8; 32];
    let mut last_err = None;
    for _ in 0..3 {
        match getrandom::getrandom(&mut bytes) {
            Ok(()) => return Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect()),
            Err(err) => last_err = Some(err),
        }
    }
    Err(format!(
        "OS CSPRNG unavailable; cannot mint a control token: {}",
        last_err
            .map(|err| err.to_string())
            .unwrap_or_else(|| "unknown error".to_string())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AdapterConfigs, ClaudeAdapterConfig, CodexAdapterConfig, GrokAdapterConfig,
        OpencodeAdapterConfig,
    };
    use crate::persistence::PersistedState;
    use crate::scrollback::{append_pane_scrollback, read_pane_scrollback};
    use crate::workspace::{AgentStatus, WorkspaceScope};
    use portable_pty::{Child, ChildKiller, ExitStatus, PtySize, native_pty_system};
    use std::io;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_workspace() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("qmux-state-{nanos}-{seq}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_config(workspace_root: PathBuf) -> QmuxConfig {
        QmuxConfig {
            workspace_root,
            socket_path: PathBuf::from("/tmp/qmux-test.sock"),
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
        }
    }

    fn sample_agent(id: &str) -> AgentInfo {
        AgentInfo {
            id: id.to_string(),
            group_id: "group-1".to_string(),
            adapter: "claude".to_string(),
            worktree_dir: "/tmp/work/agent-1".to_string(),
            branch: Some("qmux/group-1/agent-1".to_string()),
            pane_id: Some("pane-7".to_string()),
            orphaned_queue_pane_id: None,
            session_id: Some("session-abc".to_string()),
            transcript_path: Some("/tmp/transcript.jsonl".to_string()),
            status: AgentStatus::Running,
            model: Some("opus".to_string()),
            effort: None,
            parent_id: None,
            fork_point: None,
            root_session_id: None,
            thread_id: None,
            branch_id: None,
            paused: false,
            created_at: 1,
        }
    }

    fn sample_group() -> GroupInfo {
        GroupInfo {
            id: "group-1".to_string(),
            name: "group-1".to_string(),
            name_override: None,
            dir: "/tmp/work".to_string(),
            managed_dir: "/tmp/qmux-workspaces/group-1".to_string(),
            base_repo: Some("/tmp/repo".to_string()),
            base_ref: Some("HEAD".to_string()),
            parent_id: None,
            created_at: 1,
            collapsed: false,
            scope: WorkspaceScope::Research,
            imported_research_archive_id: None,
            agents: vec!["agent-1".to_string()],
        }
    }

    fn sample_group_with_id(id: &str) -> GroupInfo {
        let mut group = sample_group();
        group.scope = WorkspaceScope::Terminal;
        group.id = id.to_string();
        group.name = id.to_string();
        group.managed_dir = format!("/tmp/qmux-workspaces/{id}");
        group.agents.clear();
        group
    }

    #[test]
    fn global_drafts_crud_and_claim() {
        let state = AppState::new(test_config(PathBuf::from("/tmp/qmux-state-global-drafts")));

        assert!(state.create_global_draft("   ".to_string()).is_err());
        let draft = state
            .create_global_draft("  review the diff  ".to_string())
            .unwrap();
        assert_eq!(draft.text, "review the diff");
        assert!(draft.consumed.is_none());
        assert_eq!(state.global_drafts().unwrap().len(), 1);

        let updated = state
            .update_global_draft(&draft.id, "review the whole diff".to_string())
            .unwrap();
        assert_eq!(updated.text, "review the whole diff");

        // A claim marks the draft consumed exactly once; a second claim (the
        // concurrent double-assign race) must fail rather than double-deliver.
        let claimed = state.claim_global_draft(&draft.id, "agent-1").unwrap();
        assert_eq!(claimed.consumed.as_ref().unwrap().agent_id, "agent-1");
        assert!(state.claim_global_draft(&draft.id, "agent-2").is_err());
        // A consumed draft is history: no edits.
        assert!(
            state
                .update_global_draft(&draft.id, "too late".to_string())
                .is_err()
        );

        // Unclaim (assign rollback) reopens it for a later assign.
        state.unclaim_global_draft(&draft.id).unwrap();
        assert!(state.global_drafts().unwrap()[0].consumed.is_none());
        state.claim_global_draft(&draft.id, "agent-2").unwrap();

        assert!(state.delete_global_draft("missing").is_err());
        assert!(state.delete_global_draft(&draft.id).unwrap().is_empty());
    }

    #[test]
    fn detached_research_import_remaps_tree_and_node_id_collisions() {
        let root = temp_workspace();
        let state = AppState::new(test_config(root.clone()));
        let mut existing_group = sample_group();
        existing_group.dir = root.display().to_string();
        existing_group.managed_dir = root.join("managed-existing").display().to_string();
        existing_group.agents.clear();
        state
            .insert_group_after(existing_group.clone(), None)
            .unwrap();
        let existing = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Existing".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: existing_group.id.clone(),
            })
            .unwrap();
        let mut imported_group = sample_group();
        imported_group.id = "group-imported".to_string();
        imported_group.dir = root.display().to_string();
        imported_group.managed_dir = root.join("managed-imported").display().to_string();
        imported_group.agents.clear();
        let mut imported_tree = existing.tree.clone();
        imported_tree.title = "Imported".to_string();
        let mut imported_node = existing.nodes[0].clone();
        imported_node.prompt = "Imported".to_string();
        imported_node.status = ResearchNodeStatus::Failed;
        imported_node.agent_id = Some("agent-colliding".to_string());
        imported_node.pane_id = None;
        imported_node.thread_id = Some("thread-from-another-installation".to_string());

        state
            .import_detached_research(
                imported_group.clone(),
                Vec::new(),
                vec![imported_tree],
                Vec::new(),
                HashMap::new(),
                vec![imported_node],
                HashMap::new(),
            )
            .unwrap();

        let imported = state
            .list_research_trees_with_archived(true)
            .unwrap()
            .into_iter()
            .find(|tree| tree.title == "Imported")
            .expect("imported tree");
        assert_ne!(imported.id, existing.tree.id);
        assert_eq!(imported.workspace_id, imported_group.id);
        let detail = state.research_tree(&imported.id).unwrap();
        assert_ne!(detail.nodes[0].id, existing.nodes[0].id);
        assert!(detail.nodes[0].agent_id.is_none());
        assert!(detail.nodes[0].thread_id.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn research_detach_rejects_records_changed_after_archive_snapshot() {
        let root = temp_workspace();
        let state = AppState::new(test_config(root.clone()));
        let mut group = sample_group();
        group.dir = root.display().to_string();
        group.managed_dir = root.join("managed").display().to_string();
        group.agents.clear();
        state.insert_group_after(group.clone(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Question".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: group.id.clone(),
            })
            .unwrap();
        state
            .fail_research_node(&detail.tree.root_node_id, "settled".to_string())
            .unwrap();
        let archive = state.detached_research_archive(&group.id).unwrap();
        state
            .rename_research_tree(&detail.tree.id, "Changed title".to_string())
            .unwrap();

        let error = state
            .commit_research_workspace_detach(&group.id, &archive)
            .unwrap_err();

        assert!(error.contains("changed while"), "{error}");
        assert!(state.group(&group.id).unwrap().is_some());
        assert_eq!(
            state.research_tree(&detail.tree.id).unwrap().tree.title,
            "Changed title"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    fn sample_terminal_group() -> GroupInfo {
        let mut group = sample_group();
        group.scope = WorkspaceScope::Terminal;
        group
    }

    fn sample_pane(id: &str, agent_id: Option<&str>) -> PaneInfo {
        PaneInfo {
            id: id.to_string(),
            title: "Shell".to_string(),
            last_osc_title: None,
            kind: PaneKind::Shell,
            agent_id: agent_id.map(ToString::to_string),
            group_id: "group-1".to_string(),
            cwd: "/tmp/work/agent-1".to_string(),
            cols: 132,
            rows: 43,
            status: PaneStatus::Running,
            last_active_at: 0,
            recovered: false,
            depth: 0,
        }
    }

    #[test]
    fn research_tree_crud_keeps_nodes_scoped_to_the_tree() {
        let state = AppState::new(test_config(PathBuf::from("/tmp/qmux-state-research-crud")));
        state.insert_group_after(sample_group(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "  Compare the available approaches  ".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: Some("opus".to_string()),
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();

        assert_eq!(detail.tree.title, "Compare the available approaches");
        assert_eq!(detail.nodes.len(), 1);
        assert_eq!(detail.nodes[0].prompt, "Compare the available approaches");
        assert_eq!(detail.nodes[0].status, ResearchNodeStatus::Queued);
        assert_eq!(state.list_research_trees().unwrap()[0].running_count, 1);
        assert_eq!(
            state.list_research_activity().unwrap()[0].id,
            detail.tree.root_node_id,
            "launch-in-flight work is active before its pane binds"
        );

        let renamed = state
            .rename_research_tree(&detail.tree.id, "Approach comparison".to_string())
            .unwrap();
        assert_eq!(renamed.title, "Approach comparison");
        state
            .fail_research_node(&detail.tree.root_node_id, "Launch cancelled".to_string())
            .unwrap();
        state.remove_research_tree(&detail.tree.id).unwrap();
        assert!(state.list_research_trees().unwrap().is_empty());
        assert!(state.research_tree(&detail.tree.id).is_err());
    }

    #[test]
    fn research_tree_order_is_scoped_stable_and_persisted() {
        let workspace = temp_workspace();
        let config = test_config(workspace.clone());
        let expected_order = {
            let state = AppState::new(config.clone());
            assert!(state.restore_session().is_empty());
            let mut group = sample_group();
            group.dir = workspace.display().to_string();
            group.managed_dir = workspace.join("managed").display().to_string();
            group.agents.clear();
            state.insert_group_after(group.clone(), None).unwrap();
            let create = |prompt: &str| {
                state
                    .create_research_tree(CreateResearchTreeRequest {
                        prompt: prompt.to_string(),
                        title: Some(prompt.to_string()),
                        adapter: "claude".to_string(),
                        model: None,
                        effort: None,
                        group_id: group.id.clone(),
                    })
                    .unwrap()
            };
            let first = create("First");
            let second = create("Second");
            let third = create("Third");
            assert_eq!(
                state
                    .list_research_trees()
                    .unwrap()
                    .into_iter()
                    .map(|tree| tree.id)
                    .collect::<Vec<_>>(),
                vec![
                    third.tree.id.clone(),
                    second.tree.id.clone(),
                    first.tree.id.clone()
                ],
                "new research defaults to the top"
            );

            let expected = vec![
                first.tree.id.clone(),
                third.tree.id.clone(),
                second.tree.id.clone(),
            ];
            state
                .reorder_research_trees(&group.id, false, expected.clone())
                .unwrap();
            state
                .fail_research_node(&first.tree.root_node_id, "settled later".to_string())
                .unwrap();
            assert_eq!(
                state
                    .list_research_trees()
                    .unwrap()
                    .into_iter()
                    .map(|tree| tree.id)
                    .collect::<Vec<_>>(),
                expected,
                "activity does not overwrite manual order"
            );
            assert_eq!(
                state
                    .detached_research_archive(&group.id)
                    .unwrap()
                    .tree_order,
                expected,
                "folder archives preserve the custom order"
            );
            assert!(
                state
                    .reorder_research_trees(
                        &group.id,
                        false,
                        vec![
                            first.tree.id.clone(),
                            first.tree.id.clone(),
                            second.tree.id.clone()
                        ]
                    )
                    .unwrap_err()
                    .contains("duplicate")
            );
            expected
        };

        let restored = AppState::new(config);
        restored.restore_session();
        assert_eq!(
            restored
                .list_research_trees()
                .unwrap()
                .into_iter()
                .map(|tree| tree.id)
                .collect::<Vec<_>>(),
            expected_order
        );
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn missing_research_tree_order_migrates_from_legacy_recency() {
        let workspace = temp_workspace();
        let config = test_config(workspace.clone());
        let (older_id, newer_id) = {
            let state = AppState::new(config.clone());
            state.restore_session();
            let mut group = sample_group();
            group.dir = workspace.display().to_string();
            group.managed_dir = workspace.join("managed").display().to_string();
            group.agents.clear();
            state.insert_group_after(group.clone(), None).unwrap();
            let create = |title: &str| {
                state
                    .create_research_tree(CreateResearchTreeRequest {
                        prompt: title.to_string(),
                        title: Some(title.to_string()),
                        adapter: "claude".to_string(),
                        model: None,
                        effort: None,
                        group_id: group.id.clone(),
                    })
                    .unwrap()
            };
            let older = create("Older");
            let newer = create("Newer");
            (older.tree.id, newer.tree.id)
        };

        let path = persistence::state_path(&workspace);
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value.as_object_mut().unwrap().remove("researchTreeOrder");
        value["researchTrees"][&older_id]["updatedAt"] = serde_json::json!(10);
        value["researchTrees"][&newer_id]["updatedAt"] = serde_json::json!(20);
        std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        let restored = AppState::new(config);
        restored.restore_session();
        let expected = vec![newer_id, older_id];
        assert_eq!(
            restored
                .list_research_trees()
                .unwrap()
                .into_iter()
                .map(|tree| tree.id)
                .collect::<Vec<_>>(),
            expected
        );
        let migrated: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(migrated["researchTreeOrder"], serde_json::json!(expected));
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn remove_research_branch_deletes_descendants_and_preserves_siblings() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace.clone()));
        state.insert_group_after(sample_group(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let settle = |node_id: &str| {
            let mut model = state.inner.model.lock().unwrap();
            let node = model.research_nodes.get_mut(node_id).unwrap();
            node.status = ResearchNodeStatus::Complete;
            node.native_session_id = Some(format!("session-{node_id}"));
            node.completed_at = Some(now_millis());
        };
        settle(&detail.tree.root_node_id);
        let branch = state
            .create_research_child(&detail.tree.root_node_id, "Branch".to_string(), None, false)
            .unwrap();
        settle(&branch.id);
        let descendant = state
            .create_research_child(&branch.id, "Descendant".to_string(), None, false)
            .unwrap();
        state
            .fail_research_node(&descendant.id, "settled".to_string())
            .unwrap();
        let sibling = state
            .create_research_child(
                &detail.tree.root_node_id,
                "Sibling".to_string(),
                None,
                false,
            )
            .unwrap();
        state
            .fail_research_node(&sibling.id, "settled".to_string())
            .unwrap();
        research::write_response_snapshot(
            &workspace,
            &branch.id,
            &[sample_user_turn("branch-agent", "Branch")],
        )
        .unwrap();
        research::write_response_snapshot(
            &workspace,
            &descendant.id,
            &[sample_user_turn("descendant-agent", "Descendant")],
        )
        .unwrap();

        let removal = state.remove_research_branch(&branch.id).unwrap();
        assert_eq!(removal.tree_id, detail.tree.id);
        assert_eq!(removal.parent_node_id, detail.tree.root_node_id);
        assert_eq!(
            removal.removed_node_ids.into_iter().collect::<HashSet<_>>(),
            HashSet::from([branch.id.clone(), descendant.id.clone()])
        );
        let remaining = state.research_tree(&detail.tree.id).unwrap();
        assert!(
            remaining
                .nodes
                .iter()
                .any(|node| node.id == detail.tree.root_node_id)
        );
        assert!(remaining.nodes.iter().any(|node| node.id == sibling.id));
        assert!(!remaining.nodes.iter().any(|node| node.id == branch.id));
        assert!(!remaining.nodes.iter().any(|node| node.id == descendant.id));
        assert!(
            research::read_response_snapshot(&workspace, &branch.id)
                .unwrap()
                .is_none()
        );
        assert!(
            research::read_response_snapshot(&workspace, &descendant.id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn inline_follow_up_slot_is_exclusive_until_removed() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace.clone()));
        state.insert_group_after(sample_group(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let root_id = detail.tree.root_node_id.clone();
        let settle = |node_id: &str| {
            let mut model = state.inner.model.lock().unwrap();
            let node = model.research_nodes.get_mut(node_id).unwrap();
            node.status = ResearchNodeStatus::Complete;
            node.native_session_id = Some(format!("session-{node_id}"));
            node.completed_at = Some(now_millis());
        };
        settle(&root_id);

        let inline_child = state
            .create_research_child(&root_id, "Continue".to_string(), None, true)
            .unwrap();
        assert!(inline_child.inline);
        // The flag serializes only when set, so pre-existing trees keep their
        // byte-identical encoding.
        let inline_json = serde_json::to_value(&inline_child).unwrap();
        assert_eq!(inline_json["inline"], serde_json::json!(true));

        // A queued (not yet settled) inline child already holds the slot.
        assert!(
            state
                .create_research_child(&root_id, "Again".to_string(), None, true)
                .unwrap_err()
                .contains("already has an inline follow-up")
        );
        // Branches are unaffected by the occupied slot and never hold it.
        let branch = state
            .create_research_child(&root_id, "Aside".to_string(), None, false)
            .unwrap();
        assert!(!branch.inline);
        assert!(
            !serde_json::to_value(&branch)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("inline")
        );

        // A settled inline child still holds the slot, and chains: its own
        // answer takes an inline follow-up of its own.
        settle(&inline_child.id);
        assert!(
            state
                .create_research_child(&root_id, "Again".to_string(), None, true)
                .is_err()
        );
        let grandchild = state
            .create_research_child(&inline_child.id, "Deeper".to_string(), None, true)
            .unwrap();
        assert!(grandchild.inline);
        assert_eq!(
            grandchild.parent_node_id.as_deref(),
            Some(inline_child.id.as_str())
        );

        // A failed inline child keeps holding the slot until it is removed;
        // removal reopens it.
        state
            .fail_research_node(&grandchild.id, "settled".to_string())
            .unwrap();
        assert!(
            state
                .create_research_child(&inline_child.id, "Retry".to_string(), None, true)
                .unwrap_err()
                .contains("already has an inline follow-up")
        );
        state.remove_research_branch(&grandchild.id).unwrap();
        let retry = state
            .create_research_child(&inline_child.id, "Retry".to_string(), None, true)
            .unwrap();
        assert!(retry.inline);

        // Accepted community proposals are always branches.
        let proposal_child = state
            .create_research_child_for_proposal(
                &root_id,
                "Contributed".to_string(),
                ResearchPublicationProposal {
                    publication_id: "publication-1".to_string(),
                    comment_id: 7,
                },
            )
            .unwrap();
        assert!(!proposal_child.inline);
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn remove_research_branch_rejects_roots_and_active_descendants_atomically() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        assert!(
            state
                .remove_research_branch(&detail.tree.root_node_id)
                .unwrap_err()
                .contains("root research")
        );
        {
            let mut model = state.inner.model.lock().unwrap();
            let root = model
                .research_nodes
                .get_mut(&detail.tree.root_node_id)
                .unwrap();
            root.status = ResearchNodeStatus::Complete;
            root.native_session_id = Some("root-session".to_string());
        }
        let branch = state
            .create_research_child(&detail.tree.root_node_id, "Branch".to_string(), None, false)
            .unwrap();
        assert!(
            state
                .remove_research_branch(&branch.id)
                .unwrap_err()
                .contains("active runs")
        );
        assert!(state.research_node(&branch.id).is_ok());
    }

    #[test]
    fn research_archive_and_view_state_are_durable_navigation_metadata() {
        let workspace = temp_workspace();
        let config = test_config(workspace.clone());
        let state = AppState::new(config.clone());
        state.restore_session();
        state.insert_group_after(sample_group(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Compare options".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();

        assert!(!state.list_research_trees().unwrap()[0].has_unseen_update);
        std::thread::sleep(Duration::from_millis(2));
        state
            .rename_research_tree(&detail.tree.id, "Renamed without settlement".to_string())
            .unwrap();
        assert!(
            !state.list_research_trees().unwrap()[0].has_unseen_update,
            "metadata-only updates must not raise settlement attention"
        );
        assert!(
            state
                .archive_research_tree(&detail.tree.id)
                .unwrap_err()
                .contains("active runs")
        );

        std::thread::sleep(Duration::from_millis(2));
        state
            .fail_research_node(&detail.tree.root_node_id, "stopped".to_string())
            .unwrap();
        let summary = state
            .list_research_trees()
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert!(summary.has_unseen_update);
        assert_eq!(summary.failed_count, 1);
        assert_eq!(summary.completed_count, 0);
        assert_eq!(summary.cancelled_count, 0);

        state.mark_research_tree_viewed(&detail.tree.id).unwrap();
        assert!(!state.list_research_trees().unwrap()[0].has_unseen_update);

        let archived = state.archive_research_tree(&detail.tree.id).unwrap();
        assert!(archived.archived_at.is_some());
        assert!(state.list_research_trees().unwrap().is_empty());
        assert!(
            state
                .create_research_child(&detail.tree.root_node_id, "More".to_string(), None, false)
                .unwrap_err()
                .contains("restore archived research")
        );
        let restored_state = AppState::new(config);
        restored_state.restore_session();
        let all = restored_state
            .list_research_trees_with_archived(true)
            .unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].archived_at.is_some());

        let restored = restored_state
            .restore_research_tree(&detail.tree.id)
            .unwrap();
        assert!(restored.archived_at.is_none());
        assert_eq!(restored_state.list_research_trees().unwrap().len(), 1);
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn research_tree_creation_requires_an_existing_group() {
        let state = AppState::new(test_config(PathBuf::from(
            "/tmp/qmux-state-research-missing",
        )));
        let err = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Question".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "missing".to_string(),
            })
            .unwrap_err();
        assert!(err.contains("research workspace missing was not found"));
    }

    #[test]
    fn research_tree_creation_rejects_a_terminal_workspace() {
        let state = AppState::new(test_config(temp_workspace()));
        state
            .insert_group_after(sample_terminal_group(), None)
            .unwrap();
        let err = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Question".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap_err();

        assert!(err.contains("Research-scoped workspace"));
        assert!(state.list_research_trees().unwrap().is_empty());
    }

    #[test]
    fn empty_research_workspace_survives_automatic_pane_cleanup() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();

        state.remove_pane("pane-7").unwrap();

        assert!(state.group("group-1").unwrap().is_some());
        state.remove_group("group-1").unwrap();
        assert!(state.group("group-1").unwrap().is_none());
    }

    #[test]
    fn research_tree_creation_requires_a_forkable_adapter() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        let err = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Question".to_string(),
                title: None,
                adapter: "shell-only".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap_err();
        assert!(err.contains("follow-up (fork) support"), "{err}");
        assert!(state.list_research_trees().unwrap().is_empty());
    }

    #[test]
    fn research_run_directory_comes_from_the_workspace_group() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Question".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        assert_eq!(detail.nodes[0].worktree_dir, sample_group().dir);
    }

    #[test]
    fn research_node_tracks_agent_status_and_response_preview() {
        let state = AppState::new(test_config(PathBuf::from("/tmp/qmux-state-research-run")));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Question".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let root_id = detail.tree.root_node_id;
        let agent = sample_agent("research-agent");
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&root_id, &agent, "pane-7")
            .unwrap();

        state
            .append_turn(sample_user_turn("research-agent", "Question"))
            .unwrap();
        let mut answer = sample_user_turn("research-agent", "A concise answer");
        answer.id = "research-agent-1".to_string();
        answer.role = "assistant".to_string();
        answer.source_index = 1;
        state.append_turn(answer).unwrap();
        state
            .set_agent_status("research-agent", AgentStatus::Done)
            .unwrap();

        let content = state.research_node_content(&root_id).unwrap();
        assert_eq!(content.node.status, ResearchNodeStatus::Complete);
        assert_eq!(
            content.node.response_preview.as_deref(),
            Some("A concise answer")
        );
        assert_eq!(content.turns.len(), 1);
        assert_eq!(content.turns[0].role, "assistant");
    }

    #[test]
    fn startup_watchdog_flags_presession_research_agent() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Question".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let root_id = detail.tree.root_node_id;
        let mut agent = sample_agent("research-agent");
        agent.status = AgentStatus::Starting;
        agent.session_id = None;
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&root_id, &agent, "pane-7")
            .unwrap();

        let flagged = state
            .flag_stalled_research_startup("research-agent")
            .unwrap()
            .expect("pre-session agent should be flagged");
        assert!(matches!(flagged.status, AgentStatus::AwaitingInput));
        assert!(matches!(
            state.agent("research-agent").unwrap().unwrap().status,
            AgentStatus::AwaitingInput
        ));
        // The node must stay live: AwaitingInput maps to Running, so
        // retirement never reaps the run while it waits on the user.
        assert_eq!(
            state.research_node(&root_id).unwrap().status,
            ResearchNodeStatus::Running
        );
    }

    #[test]
    fn startup_watchdog_leaves_started_runs_alone() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Question".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let root_id = detail.tree.root_node_id;
        // sample_agent is Running with a bound session id: the launch is past
        // startup UI and mid-turn, exactly what the watchdog must not touch.
        let agent = sample_agent("research-agent");
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&root_id, &agent, "pane-7")
            .unwrap();

        assert!(
            state
                .flag_stalled_research_startup("research-agent")
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            state.agent("research-agent").unwrap().unwrap().status,
            AgentStatus::Running
        ));
    }

    #[test]
    fn startup_watchdog_flags_sessionless_running_agent() {
        // The trust-dialog wedge as observed live: Claude fires
        // UserPromptSubmit for the launch-argument prompt (promoting the
        // agent to Running) while startup UI still blocks the session, so no
        // session id is ever bound. That shape must flag too.
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Question".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let root_id = detail.tree.root_node_id;
        let mut agent = sample_agent("research-agent");
        agent.status = AgentStatus::Running;
        agent.session_id = None;
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&root_id, &agent, "pane-7")
            .unwrap();

        let flagged = state
            .flag_stalled_research_startup("research-agent")
            .unwrap()
            .expect("session-less running agent should be flagged");
        assert!(matches!(flagged.status, AgentStatus::AwaitingInput));
    }

    #[test]
    fn startup_watchdog_ignores_settled_runs() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Question".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let root_id = detail.tree.root_node_id;
        let mut agent = sample_agent("research-agent");
        agent.status = AgentStatus::Starting;
        agent.session_id = None;
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&root_id, &agent, "pane-7")
            .unwrap();
        state.cancel_research_node(&root_id).unwrap();

        // A watchdog firing after the user already settled the run must not
        // resurrect it by flipping its (possibly still-recorded) agent.
        assert!(
            state
                .flag_stalled_research_startup("research-agent")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn generating_followup_never_previews_the_parent_answer() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        state.insert_pane(sample_pane_runtime("pane-8")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root question".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let root_id = detail.tree.root_node_id.clone();
        let agent = sample_agent("research-agent");
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&root_id, &agent, "pane-7")
            .unwrap();
        state
            .append_turn(sample_user_turn("research-agent", "Root question"))
            .unwrap();
        let mut answer = sample_user_turn("research-agent", "Root answer");
        answer.role = "assistant".to_string();
        answer.id = "research-agent-answer".to_string();
        answer.source_index = 1;
        state.append_turn(answer).unwrap();
        state
            .set_agent_status("research-agent", AgentStatus::Done)
            .unwrap();

        // The follow-up runs in a forked session: its transcript replays the
        // parent exchange first, and while the answer is being generated the
        // child's own prompt has not reached the transcript yet.
        let child = state
            .create_research_child(&root_id, "Follow-up question".to_string(), None, false)
            .unwrap();
        let child_agent = sample_agent("child-agent");
        state.insert_agent(child_agent.clone()).unwrap();
        state
            .bind_research_node_run(&child.id, &child_agent, "pane-8")
            .unwrap();
        state
            .append_turn(sample_user_turn("child-agent", "Root question"))
            .unwrap();
        let mut replayed = sample_user_turn("child-agent", "Root answer");
        replayed.role = "assistant".to_string();
        replayed.id = "child-agent-replayed".to_string();
        replayed.source_index = 1;
        state.append_turn(replayed).unwrap();

        // The parent's answer must not stand in as the child's preview or
        // response; the card and pane keep their generating placeholders.
        let content = state.research_node_content(&child.id).unwrap();
        assert_eq!(content.node.response_preview, None);
        assert!(content.turns.is_empty());

        // Once the child's own (adapter-rewritten) prompt and answer land,
        // the preview follows the child's response as before.
        let mut child_prompt = sample_user_turn("child-agent", "[wrapped] follow-up (rewritten)");
        child_prompt.id = "child-agent-prompt".to_string();
        child_prompt.source_index = 2;
        state.append_turn(child_prompt).unwrap();
        let mut child_answer = sample_user_turn("child-agent", "Child answer");
        child_answer.role = "assistant".to_string();
        child_answer.id = "child-agent-answer".to_string();
        child_answer.source_index = 3;
        state.append_turn(child_answer).unwrap();
        let content = state.research_node_content(&child.id).unwrap();
        assert_eq!(
            content.node.response_preview.as_deref(),
            Some("Child answer")
        );
        assert_eq!(content.turns.len(), 1);
        assert_eq!(content.turns[0].role, "assistant");
    }

    #[test]
    fn research_waits_for_subagents_and_a_later_parent_completion() {
        let state = AppState::new(test_config(PathBuf::from(
            "/tmp/qmux-state-research-subagents",
        )));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Question".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let root_id = detail.tree.root_node_id;
        let agent = sample_agent("research-agent");
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&root_id, &agent, "pane-7")
            .unwrap();

        assert_eq!(
            state
                .agent_subagent_started("research-agent", Some(" child-1 "))
                .unwrap(),
            1
        );
        // Duplicate identified hooks are idempotent.
        assert_eq!(
            state
                .agent_subagent_started("research-agent", Some("child-1"))
                .unwrap(),
            1
        );
        state
            .set_agent_status("research-agent", AgentStatus::Done)
            .unwrap();
        let waiting = state.research_node(&root_id).unwrap();
        assert_eq!(waiting.status, ResearchNodeStatus::Running);
        assert!(waiting.completed_at.is_none());

        assert_eq!(
            state
                .agent_subagent_stopped("research-agent", Some("child-1"))
                .unwrap(),
            Some(0)
        );
        // Child completion alone is not the parent completion boundary.
        assert_eq!(
            state.research_node(&root_id).unwrap().status,
            ResearchNodeStatus::Running
        );

        state
            .set_agent_status("research-agent", AgentStatus::Running)
            .unwrap();
        state
            .set_agent_status("research-agent", AgentStatus::Done)
            .unwrap();
        assert_eq!(
            state.research_node(&root_id).unwrap().status,
            ResearchNodeStatus::Complete
        );
    }

    #[test]
    fn anonymous_subagent_tracking_saturates_and_is_parent_scoped() {
        let state = AppState::new(test_config(temp_workspace()));
        assert_eq!(state.agent_subagent_started("parent-1", None).unwrap(), 1);
        assert_eq!(state.agent_subagent_started("parent-1", None).unwrap(), 2);
        assert!(!state.agent_has_active_subagents("parent-2").unwrap());
        assert_eq!(
            state.agent_subagent_stopped("parent-1", None).unwrap(),
            Some(1)
        );
        assert_eq!(
            state.agent_subagent_stopped("parent-1", None).unwrap(),
            Some(0)
        );
        // A stop with nothing tracked reports as such so callers leave the
        // parent's status alone.
        assert_eq!(
            state.agent_subagent_stopped("parent-1", None).unwrap(),
            None
        );
        assert!(!state.agent_has_active_subagents("parent-1").unwrap());
    }

    // A stop hook whose payload lost (or never had) the id its start carried
    // must still settle one tracked subagent — a permanently non-zero counter
    // suppresses every future parent Stop.
    #[test]
    fn asymmetric_subagent_ids_still_settle_tracked_work() {
        let state = AppState::new(test_config(temp_workspace()));

        // Identified start, anonymous stop.
        state
            .agent_subagent_started("parent-1", Some("child-1"))
            .unwrap();
        assert_eq!(
            state.agent_subagent_stopped("parent-1", None).unwrap(),
            Some(0)
        );
        assert!(!state.agent_has_active_subagents("parent-1").unwrap());

        // Anonymous start, identified stop.
        state.agent_subagent_started("parent-2", None).unwrap();
        assert_eq!(
            state
                .agent_subagent_stopped("parent-2", Some("child-9"))
                .unwrap(),
            Some(0)
        );
        assert!(!state.agent_has_active_subagents("parent-2").unwrap());
    }

    #[test]
    fn research_child_inherits_parent_launch_context() {
        let state = AppState::new(test_config(PathBuf::from("/tmp/qmux-state-research-child")));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "codex".to_string(),
                model: Some("gpt-5".to_string()),
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let agent = sample_agent("research-agent");
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-7")
            .unwrap();
        state
            .append_turn(sample_user_turn("research-agent", "Root"))
            .unwrap();
        let mut answer = sample_user_turn("research-agent", "Durable response");
        answer.role = "assistant".to_string();
        answer.id = "research-agent-answer".to_string();
        state.append_turn(answer).unwrap();
        state
            .set_agent_status("research-agent", AgentStatus::Done)
            .unwrap();
        let proposal = ResearchPublicationProposal {
            publication_id: "pub_research123".to_string(),
            comment_id: 42,
        };
        let child = state
            .create_research_child_for_proposal(
                &detail.tree.root_node_id,
                "Follow up".to_string(),
                proposal.clone(),
            )
            .unwrap();
        assert_eq!(
            child.parent_node_id.as_deref(),
            Some(detail.tree.root_node_id.as_str())
        );
        assert_eq!(child.adapter, "codex");
        assert_eq!(child.model.as_deref(), Some("gpt-5"));
        assert_eq!(child.publication_proposal, Some(proposal));
        assert_eq!(state.research_tree(&detail.tree.id).unwrap().nodes.len(), 2);
    }

    #[test]
    fn research_child_requires_a_completed_checkpoint() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();

        let err = state
            .create_research_child(
                &detail.tree.root_node_id,
                "Too soon".to_string(),
                None,
                false,
            )
            .unwrap_err();
        assert!(err.contains("completed parent"));
        assert_eq!(state.research_tree(&detail.tree.id).unwrap().nodes.len(), 1);
    }

    #[test]
    fn detaching_completed_research_pane_preserves_native_checkpoint() {
        let state = AppState::new(test_config(PathBuf::from(
            "/tmp/qmux-state-research-detach",
        )));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let mut agent = sample_agent("research-agent");
        agent.status = AgentStatus::Done;
        state.insert_agent(agent.clone()).unwrap();
        let bound = state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-7")
            .unwrap();
        assert_eq!(bound.status, ResearchNodeStatus::Complete);
        state
            .set_agent_status("research-agent", AgentStatus::Done)
            .unwrap();

        let detached = state.detach_research_pane("pane-7").unwrap().unwrap();
        assert_eq!(detached.status, ResearchNodeStatus::Complete);
        assert_eq!(detached.agent_id.as_deref(), Some("research-agent"));
        assert!(detached.pane_id.is_none());
        assert_eq!(detached.native_session_id.as_deref(), Some("session-abc"));
        assert!(state.list_research_activity().unwrap().is_empty());
    }

    #[test]
    fn research_tree_removal_releases_but_never_deletes_the_group() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();

        state.remove_pane("pane-7").unwrap();
        assert!(state.group("group-1").unwrap().is_some());
        assert!(
            state
                .remove_group("group-1")
                .unwrap_err()
                .contains("research tree")
        );
        state
            .fail_research_node(&detail.tree.root_node_id, "Launch cancelled".to_string())
            .unwrap();
        state.remove_research_tree(&detail.tree.id).unwrap();
        // The user picked this pre-existing group for the research run; the
        // tree never owned it, so removing the tree must not delete it (or
        // prune agents retained in it) — it only lifts the retention guard.
        assert!(state.group("group-1").unwrap().is_some());
        state.remove_group("group-1").unwrap();
        assert!(state.group("group-1").unwrap().is_none());
    }

    #[test]
    fn active_research_tree_cannot_be_removed() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();

        let err = state.remove_research_tree(&detail.tree.id).unwrap_err();
        assert!(err.contains("active runs"));
        assert!(state.research_tree(&detail.tree.id).is_ok());
    }

    #[test]
    fn restart_fails_interrupted_research_and_drops_its_pane_from_recovery() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace.clone()));
        assert!(state.restore_session().is_empty());
        state.insert_group_after(sample_group(), None).unwrap();
        let mut pane = sample_pane_runtime("pane-7");
        pane.info.kind = PaneKind::Agent;
        pane.info.agent_id = Some("research-agent".to_string());
        state.insert_pane(pane).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let agent = sample_agent("research-agent");
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-7")
            .unwrap();
        state.finalize_persistence_for_exit();

        // The interrupted turn died with the old process, and a recovered
        // adapter resumes Idle — which the agent sync would misread as a
        // *completed* answer and permanently snapshot a partial response.
        // The run settles Failed and its hidden pane is dropped from
        // recovery, not respawned just to be reclaimed.
        let restored = AppState::new(test_config(workspace));
        let panes = restored.restore_session();
        assert!(panes.iter().all(|pane| pane.id != "pane-7"));
        let node = restored.research_node(&detail.tree.root_node_id).unwrap();
        assert_eq!(node.status, ResearchNodeStatus::Failed);
        assert!(
            node.error
                .as_deref()
                .unwrap_or_default()
                .contains("interrupted"),
            "{:?}",
            node.error
        );
        assert!(node.pane_id.is_none());
        // The agent is reclaimed with its pane, exactly as remove_pane would
        // have done — a dropped run must not leave a dead AgentInfo behind.
        assert!(restored.agent("research-agent").unwrap().is_none());
        // Nothing active or bound remains, so the tree is immediately removable.
        restored.remove_research_tree(&detail.tree.id).unwrap();
    }

    #[test]
    fn transcript_updates_persist_research_preview_without_a_status_change() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace.clone()));
        assert!(state.restore_session().is_empty());
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Question".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let agent = sample_agent("research-agent");
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-7")
            .unwrap();
        state
            .append_turn(sample_user_turn("research-agent", "Question"))
            .unwrap();
        let updated_at_before_preview = state.list_research_trees().unwrap()[0].updated_at;
        std::thread::sleep(std::time::Duration::from_millis(2));
        let mut answer = sample_user_turn("research-agent", "Persisted preview");
        answer.id = "research-agent-1".to_string();
        answer.role = "assistant".to_string();
        answer.source_index = 1;
        state.append_turn(answer).unwrap();
        assert!(
            !state.list_research_trees().unwrap()[0].has_unseen_update,
            "streaming preview churn must not raise settlement attention"
        );
        assert_eq!(
            state.list_research_trees().unwrap()[0].updated_at,
            updated_at_before_preview,
            "streaming preview churn must not resort the sidebar"
        );
        state.finalize_persistence_for_exit();

        let restored = AppState::new(test_config(workspace));
        restored.restore_session();
        let node = restored.research_node(&detail.tree.root_node_id).unwrap();
        assert_eq!(node.response_preview.as_deref(), Some("Persisted preview"));
    }

    #[test]
    fn research_panes_reject_terminal_writes() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let agent = sample_agent("research-agent");
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-7")
            .unwrap();

        let err = crate::pty::write_pane(
            &state,
            crate::pty::PaneWriteOptions {
                pane_id: "pane-7".to_string(),
                data: "another prompt".to_string(),
                paste: false,
                submit: true,
            },
        )
        .unwrap_err();
        assert!(err.contains("read-only"));

        state
            .set_agent_status("research-agent", AgentStatus::AwaitingPermission)
            .unwrap();
        assert_eq!(
            state.research_pane_accepts_input("pane-7").unwrap(),
            Some(true)
        );

        // An elicitation pause is mid-turn: the user must be able to answer,
        // and the node must stay live rather than complete-and-retire.
        state
            .set_agent_status("research-agent", AgentStatus::AwaitingInput)
            .unwrap();
        assert_eq!(
            state.research_pane_accepts_input("pane-7").unwrap(),
            Some(true)
        );
        let node = state.research_node(&detail.tree.root_node_id).unwrap();
        assert_eq!(node.status, ResearchNodeStatus::Running);
    }

    #[test]
    fn research_agents_reject_queued_turns() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let agent = sample_agent("research-agent");
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-7")
            .unwrap();

        // Queueing bypasses write_pane, so it must be rejected on its own: a
        // research run never drains a queue, and an accepted turn would park
        // the agent past retirement.
        let err = crate::turn_queue::submit_agent_turn(
            &state,
            crate::turn_queue::SubmitAgentTurnRequest {
                agent_id: "research-agent".to_string(),
                data: "queued follow-up".to_string(),
                mode: Some(crate::turn_queue::SubmitAgentTurnMode::Queue),
            },
        )
        .unwrap_err();
        assert!(err.contains("read-only"));
        assert!(
            state
                .agent_queued_turns("research-agent")
                .unwrap()
                .is_empty()
        );
    }

    fn exportable_terminal_setup(state: &AppState, agent_status: AgentStatus) {
        let mut terminal_group = sample_group_with_id("term-1");
        terminal_group.scope = WorkspaceScope::Terminal;
        state.insert_group_after(terminal_group, None).unwrap();
        state.insert_group_after(sample_group(), None).unwrap();
        let mut pane = sample_pane_runtime("pane-1");
        pane.info.agent_id = Some("term-agent".to_string());
        pane.info.group_id = "term-1".to_string();
        state.insert_pane(pane).unwrap();
        let mut agent = sample_agent("term-agent");
        agent.group_id = "term-1".to_string();
        agent.pane_id = Some("pane-1".to_string());
        agent.transcript_path = None;
        agent.status = agent_status;
        state.insert_agent(agent).unwrap();
    }

    fn append_terminal_exchange(state: &AppState, index: usize, question: &str, answer: &str) {
        let mut user = sample_user_turn("term-agent", question);
        user.id = format!("term-agent-{}", index * 2);
        user.source_index = index * 2;
        state.append_turn(user).unwrap();
        let mut assistant = sample_user_turn("term-agent", answer);
        assistant.id = format!("term-agent-{}", index * 2 + 1);
        assistant.source_index = index * 2 + 1;
        assistant.role = "assistant".to_string();
        state.append_turn(assistant).unwrap();
    }

    fn export_pane(
        state: &AppState,
        pane_id: &str,
        group_id: &str,
        title: Option<&str>,
    ) -> Result<ResearchTreeDetail, String> {
        let prepared = state.prepare_pane_export(pane_id)?;
        state.commit_pane_export(&prepared, group_id.to_string(), title.map(str::to_string))
    }

    #[test]
    fn export_pane_to_research_creates_a_severed_conversation_tree() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace.clone()));
        assert!(state.restore_session().is_empty());
        exportable_terminal_setup(&state, AgentStatus::Idle);
        append_terminal_exchange(&state, 0, "Question", "Answer");
        // No conversation nodes yet: the state file keeps the widest
        // downgrade compatibility.
        let version_of = |workspace: &PathBuf| {
            let raw = std::fs::read(persistence::state_path(workspace)).unwrap();
            serde_json::from_slice::<serde_json::Value>(&raw).unwrap()["version"]
                .as_u64()
                .unwrap()
        };
        assert_eq!(version_of(&workspace), 4);

        let detail = export_pane(&state, "pane-1", "group-1", None).unwrap();
        assert_eq!(detail.tree.workspace_id, "group-1");
        let node = &detail.nodes[0];
        assert_eq!(node.kind, ResearchNodeKind::Conversation);
        assert_eq!(node.origin, Some(ResearchNodeOrigin::TerminalExport));
        assert_eq!(node.status, ResearchNodeStatus::Complete);
        assert_eq!(node.prompt, "Question");
        assert_eq!(node.response_preview.as_deref(), Some("Answer"));
        assert_eq!(node.adapter, "claude");
        // Severed: no pointer back to the source session, pane, or thread.
        assert!(node.native_session_id.is_none());
        assert!(node.transcript_path.is_none());
        assert!(node.agent_id.is_none());
        assert!(node.pane_id.is_none());
        assert!(node.thread_id.is_none());
        // The sidebar surfaces the new kind.
        let summary = &state.list_research_trees().unwrap()[0];
        assert_eq!(summary.kind, ResearchNodeKind::Conversation);

        // The snapshot is durable, reissued under the node's identity.
        let turns = research::read_response_snapshot(&state.config().workspace_root, &node.id)
            .unwrap()
            .unwrap();
        assert_eq!(turns.len(), 2);
        assert!(turns.iter().all(|turn| {
            turn.agent_id == node.id && turn.session_id.is_none() && turn.native_id.is_none()
        }));

        // The terminal is untouched: this was a copy, not a move.
        assert!(state.agent("term-agent").unwrap().is_some());
        assert!(state.pane_exists("pane-1").unwrap());

        // The exported records and snapshot survive a restart unchanged, and
        // the state file now marks the conversation for older builds.
        state.finalize_persistence_for_exit();
        assert_eq!(version_of(&workspace), 5);
        let restored = AppState::new(test_config(workspace));
        restored.restore_session();
        let restored_node = restored.research_node(&node.id).unwrap();
        assert_eq!(restored_node.kind, ResearchNodeKind::Conversation);
        assert_eq!(
            restored_node.origin,
            Some(ResearchNodeOrigin::TerminalExport)
        );
        assert_eq!(restored_node.status, ResearchNodeStatus::Complete);
    }

    #[test]
    fn export_pane_to_research_mid_turn_keeps_completed_exchanges_only() {
        let state = AppState::new(test_config(temp_workspace()));
        exportable_terminal_setup(&state, AgentStatus::Running);
        append_terminal_exchange(&state, 0, "First question", "First answer");
        append_terminal_exchange(&state, 1, "Second question", "Half-streamed answer");

        let detail = export_pane(&state, "pane-1", "group-1", Some("Mid-turn export")).unwrap();
        assert_eq!(detail.tree.title, "Mid-turn export");
        let node = &detail.nodes[0];
        let turns = research::read_response_snapshot(&state.config().workspace_root, &node.id)
            .unwrap()
            .unwrap();
        let encoded = serde_json::to_string(&turns).unwrap();
        assert_eq!(turns.len(), 2, "{encoded}");
        assert!(!encoded.contains("Second question"), "{encoded}");
        assert!(!encoded.contains("Half-streamed"), "{encoded}");
    }

    #[test]
    fn export_pane_to_research_awaiting_input_exports_delivered_content() {
        // AwaitingInput is an at-rest status (adapters assign it after
        // notifications and interruptions); treating it as busy would
        // silently drop the final delivered exchange.
        let state = AppState::new(test_config(temp_workspace()));
        exportable_terminal_setup(&state, AgentStatus::AwaitingInput);
        append_terminal_exchange(&state, 0, "First question", "First answer");
        append_terminal_exchange(&state, 1, "Second question", "Second answer");

        let detail = export_pane(&state, "pane-1", "group-1", None).unwrap();
        let node = &detail.nodes[0];
        let turns = research::read_response_snapshot(&state.config().workspace_root, &node.id)
            .unwrap()
            .unwrap();
        assert_eq!(turns.len(), 4);
        assert_eq!(node.response_preview.as_deref(), Some("Second answer"));
    }

    #[test]
    fn export_pane_to_research_reports_an_in_flight_only_exchange() {
        let state = AppState::new(test_config(temp_workspace()));
        exportable_terminal_setup(&state, AgentStatus::Running);
        append_terminal_exchange(&state, 0, "Only question", "Streaming answer");

        // The prompt is visibly on screen, so the error must say the answer
        // is in progress — not that there is no prompt.
        let err = export_pane(&state, "pane-1", "group-1", None).unwrap_err();
        assert!(err.contains("still in progress"), "{err}");
    }

    #[test]
    fn export_pane_to_research_prefers_the_transcript_file() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace.clone()));
        exportable_terminal_setup(&state, AgentStatus::Idle);
        // The live timeline is a decoy; the file is the complete record.
        append_terminal_exchange(&state, 0, "Live question", "Live answer");
        let transcript_path = workspace.join("terminal-transcript.jsonl");
        let lines = [
            serde_json::json!({
                "type": "user",
                "uuid": "u1",
                "sessionId": "session-abc",
                "message": { "role": "user", "content": "File question" },
            }),
            serde_json::json!({
                "type": "assistant",
                "uuid": "a1",
                "parentUuid": "u1",
                "sessionId": "session-abc",
                "message": {
                    "id": "m1",
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "File answer" }],
                },
            }),
        ]
        .map(|line| line.to_string())
        .join("\n");
        std::fs::write(&transcript_path, format!("{lines}\n")).unwrap();
        let mut agent = sample_agent("term-agent");
        agent.group_id = "term-1".to_string();
        agent.pane_id = Some("pane-1".to_string());
        agent.status = AgentStatus::Idle;
        agent.transcript_path = Some(transcript_path.display().to_string());
        state.insert_agent(agent).unwrap();

        let detail = export_pane(&state, "pane-1", "group-1", None).unwrap();
        let node = &detail.nodes[0];
        assert_eq!(node.prompt, "File question");
        let turns = research::read_response_snapshot(&state.config().workspace_root, &node.id)
            .unwrap()
            .unwrap();
        let encoded = serde_json::to_string(&turns).unwrap();
        assert!(encoded.contains("File answer"), "{encoded}");
        assert!(!encoded.contains("Live answer"), "{encoded}");
    }

    #[test]
    fn export_pane_to_research_rejects_shells_research_panes_and_empty_timelines() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        let mut terminal_group = sample_group_with_id("term-1");
        terminal_group.scope = WorkspaceScope::Terminal;
        state.insert_group_after(terminal_group, None).unwrap();

        // A shell pane in a terminal workspace has no agent to export.
        let mut shell = sample_pane_runtime("pane-9");
        shell.info.group_id = "term-1".to_string();
        state.insert_pane(shell).unwrap();
        let err = export_pane(&state, "pane-9", "group-1", None).unwrap_err();
        assert!(err.contains("only agent panes"), "{err}");

        // A research run's hidden pane is rejected by workspace scope — in
        // production order (pane inserted before the node binds its agent),
        // both before and after the bind, so the launch-to-bind window is
        // covered.
        let mut research_pane = sample_pane_runtime("pane-7");
        research_pane.info.agent_id = Some("research-agent".to_string());
        state.insert_pane(research_pane).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let mut research_agent = sample_agent("research-agent");
        research_agent.transcript_path = None;
        state.insert_agent(research_agent.clone()).unwrap();
        let err = export_pane(&state, "pane-7", "group-1", None).unwrap_err();
        assert!(err.contains("only terminal panes"), "{err}");
        state
            .bind_research_node_run(&detail.tree.root_node_id, &research_agent, "pane-7")
            .unwrap();
        let err = export_pane(&state, "pane-7", "group-1", None).unwrap_err();
        assert!(err.contains("only terminal panes"), "{err}");

        // An agent that never produced an exchange has nothing to export.
        let mut pane = sample_pane_runtime("pane-1");
        pane.info.agent_id = Some("term-agent".to_string());
        pane.info.group_id = "term-1".to_string();
        state.insert_pane(pane).unwrap();
        let mut agent = sample_agent("term-agent");
        agent.group_id = "term-1".to_string();
        agent.pane_id = Some("pane-1".to_string());
        agent.transcript_path = None;
        agent.status = AgentStatus::Idle;
        state.insert_agent(agent).unwrap();
        let err = export_pane(&state, "pane-1", "group-1", None).unwrap_err();
        assert!(err.contains("user prompt"), "{err}");
    }

    #[test]
    fn conversation_followups_admit_children_with_serialized_context() {
        let state = AppState::new(test_config(temp_workspace()));
        exportable_terminal_setup(&state, AgentStatus::Idle);
        append_terminal_exchange(&state, 0, "Question", "Answer");
        let detail = export_pane(&state, "pane-1", "group-1", None).unwrap();
        let root_id = detail.tree.root_node_id.clone();

        let child = state
            .create_research_child(&root_id, "Follow-up question".to_string(), None, false)
            .unwrap();
        assert_eq!(child.kind, ResearchNodeKind::Run);
        assert_eq!(child.parent_node_id.as_deref(), Some(root_id.as_str()));
        // The source terminal's adapter carries over (claude can fork, which
        // the run child's own follow-ups will need).
        assert_eq!(child.adapter, "claude");
        assert_eq!(child.status, ResearchNodeStatus::Queued);

        let prompt = state
            .research_conversation_followup_prompt(&root_id, "Follow-up question")
            .unwrap();
        assert!(prompt.contains("<conversation title="), "{prompt}");
        assert!(prompt.contains("Question"), "{prompt}");
        assert!(prompt.contains("Answer"), "{prompt}");
        // The bare question stays a suffix so the child's response boundary
        // still matches it inside the sent prompt.
        assert!(prompt.ends_with("Follow-up question"), "{prompt}");

        // Run nodes are not servable by the conversation prompt path.
        let err = state
            .research_conversation_followup_prompt(&child.id, "Q")
            .unwrap_err();
        assert!(err.contains("not an exported conversation"), "{err}");

        // Highlight-targeted follow-ups are refused on conversation parents:
        // no highlights exist there, and an anchored quote would bypass the
        // serializer's tag neutralization.
        let anchor = crate::research::ResearchHighlightAnchor {
            version: 1,
            projection: "answer-v1".to_string(),
            response_revision: "a".repeat(64),
            start: 0,
            end: 6,
            exact: "Answer".to_string(),
            prefix: String::new(),
            suffix: String::new(),
        };
        let err = state
            .create_research_child(
                &root_id,
                "Anchored question".to_string(),
                Some(anchor),
                false,
            )
            .unwrap_err();
        assert!(
            err.contains("not available on exported conversations"),
            "{err}"
        );
    }

    #[test]
    fn restore_fails_research_nodes_that_never_launched() {
        let workspace = temp_workspace();
        let detail = {
            let state = AppState::new(test_config(workspace.clone()));
            assert!(state.restore_session().is_empty());
            state.insert_group_after(sample_group(), None).unwrap();
            // Persisted as Queued with no agent/pane bound — the crash window
            // between create_research_tree() and the command's spawn/bind.
            let detail = state
                .create_research_tree(CreateResearchTreeRequest {
                    prompt: "Never launched".to_string(),
                    title: None,
                    adapter: "claude".to_string(),
                    model: None,
                    effort: None,
                    group_id: "group-1".to_string(),
                })
                .unwrap();
            state.finalize_persistence_for_exit();
            detail
        };

        let restored = AppState::new(test_config(workspace));
        restored.restore_session();
        let node = restored.research_node(&detail.tree.root_node_id).unwrap();
        assert_eq!(node.status, ResearchNodeStatus::Failed);
        // A settled node no longer counts as an active run, so the tree stays
        // removable instead of being pinned by a phantom launch.
        restored.remove_research_tree(&detail.tree.id).unwrap();
    }

    #[test]
    fn failure_and_detachment_paths_touch_the_tree_timestamp() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        // Force a stale timestamp so the bump is observable even within one
        // millisecond of the creation.
        {
            let mut model = state.inner.model.lock().unwrap();
            model
                .research_trees
                .get_mut(&detail.tree.id)
                .unwrap()
                .updated_at = 0;
        }

        state
            .fail_research_node(&detail.tree.root_node_id, "boom".to_string())
            .unwrap();
        let updated_after_failure = state
            .research_tree(&detail.tree.id)
            .unwrap()
            .tree
            .updated_at;
        assert!(updated_after_failure > 0, "failure must touch updated_at");
    }

    #[test]
    fn user_close_of_active_research_run_cancels_and_reclaims_the_pane() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let agent = sample_agent("research-agent");
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-7")
            .unwrap();

        state.close_pane_for_user("pane-7").unwrap();
        let cancelled = state.research_node(&detail.tree.root_node_id).unwrap();
        assert_eq!(cancelled.status, ResearchNodeStatus::Cancelled);
        assert_eq!(state.list_research_trees().unwrap()[0].cancelled_count, 1);
        assert!(cancelled.pane_id.is_none());
        assert!(cancelled.completed_at.is_some());
        assert!(state.list_panes().unwrap().is_empty());
        // No undo entry: cancellation reclaims the pane for good.
        assert!(state.take_last_closed_pane().unwrap().is_none());
        // A settled tree is removable, and double-cancel is rejected cleanly.
        assert!(
            state
                .cancel_research_node(&detail.tree.root_node_id)
                .unwrap_err()
                .contains("not active")
        );
        state.remove_research_tree(&detail.tree.id).unwrap();
    }

    #[test]
    fn cancelled_research_run_ignores_stale_agent_status_updates() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let root_id = detail.tree.root_node_id;
        let agent = sample_agent("research-agent");
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&root_id, &agent, "pane-7")
            .unwrap();
        let cancelled = state.cancel_research_node(&root_id).unwrap();
        assert_eq!(cancelled.status, ResearchNodeStatus::Cancelled);

        // Hooks deliver status asynchronously: a Running update from the dying
        // process arrives after the user's cancellation has settled the run.
        state
            .set_agent_status("research-agent", AgentStatus::Running)
            .unwrap();
        let node = state.research_node(&root_id).unwrap();
        assert_eq!(node.status, ResearchNodeStatus::Cancelled);
        assert_eq!(node.completed_at, cancelled.completed_at);

        // A late launch-cleanup failure must not rewrite the outcome either.
        state
            .fail_research_node(&root_id, "launch cleanup".to_string())
            .unwrap();
        let node = state.research_node(&root_id).unwrap();
        assert_eq!(node.status, ResearchNodeStatus::Cancelled);
        assert!(node.error.is_none());
        state.remove_research_tree(&detail.tree.id).unwrap();
    }

    #[test]
    fn binding_after_cancellation_does_not_resurrect_the_run() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let root_id = detail.tree.root_node_id;
        // The user cancels the Queued node while its spawn is still in flight.
        let cancelled = state.cancel_research_node(&root_id).unwrap();
        assert_eq!(cancelled.status, ResearchNodeStatus::Cancelled);

        // The spawn then completes and binds. The pane and agent are recorded
        // (the launch path reclaims them), but the outcome stands.
        let agent = sample_agent("research-agent");
        state.insert_agent(agent.clone()).unwrap();
        let bound = state
            .bind_research_node_run(&root_id, &agent, "pane-7")
            .unwrap();
        assert_eq!(bound.status, ResearchNodeStatus::Cancelled);
        assert!(bound.error.is_none());
        assert_eq!(bound.pane_id.as_deref(), Some("pane-7"));
        assert_eq!(bound.completed_at, cancelled.completed_at);
    }

    #[test]
    fn restore_reconciles_broken_research_references() {
        let persisted_node = |id: &str, tree_id: &str, parent: Option<&str>| ResearchNode {
            id: id.to_string(),
            tree_id: tree_id.to_string(),
            parent_node_id: parent.map(str::to_string),
            publication_proposal: None,
            query_anchor: None,
            inline: false,
            prompt: "Q".to_string(),
            title: None,
            response_preview: None,
            adapter: "claude".to_string(),
            model: None,
            effort: None,
            group_id: "group-1".to_string(),
            worktree_dir: "/tmp/work".to_string(),
            native_session_id: Some("session".to_string()),
            transcript_path: None,
            prompt_native_id: None,
            agent_id: None,
            pane_id: None,
            thread_id: None,
            kind: ResearchNodeKind::Run,
            origin: None,
            status: ResearchNodeStatus::Complete,
            error: None,
            response_snapshot_at: None,
            created_at: 1,
            started_at: Some(1),
            completed_at: Some(2),
            highlights: Vec::new(),
        };
        let persisted_tree = |id: &str, root: &str| ResearchTree {
            id: id.to_string(),
            title: id.to_string(),
            root_node_id: root.to_string(),
            workspace_id: "group-1".to_string(),
            created_at: 1,
            updated_at: 1,
            archived_at: None,
            last_viewed_at: None,
        };

        let workspace = temp_workspace();
        let mut state = PersistedState::default();
        state.groups.push(sample_group());
        // A valid tree with a valid child, plus a completed node still bound
        // to a pane that no longer exists (crash during multi-stage removal).
        state
            .research_trees
            .insert("tree-a".to_string(), persisted_tree("tree-a", "root-a"));
        let mut root_a = persisted_node("root-a", "tree-a", None);
        root_a.pane_id = Some("ghost-pane".to_string());
        state.research_nodes.insert("root-a".to_string(), root_a);
        state.research_nodes.insert(
            "child-a".to_string(),
            persisted_node("child-a", "tree-a", Some("root-a")),
        );
        // A node whose parent vanished, and a descendant hanging off it: both
        // must go (the fixpoint pass, not just one sweep).
        state.research_nodes.insert(
            "orphan-a".to_string(),
            persisted_node("orphan-a", "tree-a", Some("ghost")),
        );
        state.research_nodes.insert(
            "orphan-child-a".to_string(),
            persisted_node("orphan-child-a", "tree-a", Some("orphan-a")),
        );
        // A tree claiming another tree's root, with a node of its own.
        state
            .research_trees
            .insert("tree-b".to_string(), persisted_tree("tree-b", "root-a"));
        state.research_nodes.insert(
            "node-b".to_string(),
            persisted_node("node-b", "tree-b", None),
        );
        persistence::save(&workspace, &state).unwrap();

        let restored = AppState::new(test_config(workspace));
        restored.restore_session();

        let detail = restored.research_tree("tree-a").unwrap();
        let mut kept = detail
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>();
        kept.sort_unstable();
        assert_eq!(kept, ["child-a", "root-a"]);
        // The dangling pane binding is cleared, so the tree is removable
        // instead of being pinned by a phantom active run.
        assert!(restored.research_node("root-a").unwrap().pane_id.is_none());
        assert!(restored.research_tree("tree-b").is_err());
        assert!(restored.research_node("node-b").is_err());
        restored.remove_research_tree("tree-a").unwrap();
    }

    #[test]
    fn restore_splits_legacy_research_runtime_out_of_a_terminal_group() {
        let workspace = temp_workspace();
        let managed = workspace.join("legacy-managed");
        std::fs::create_dir_all(managed.join(".qmux")).unwrap();
        let mut group = sample_terminal_group();
        group.dir = workspace.display().to_string();
        group.managed_dir = managed.display().to_string();
        group.agents = vec!["research-agent".to_string()];

        let terminal_pane = sample_pane("pane-terminal", None);
        let mut research_pane = sample_pane("pane-research", Some("research-agent"));
        research_pane.depth = 1;
        let mut agent = sample_agent("research-agent");
        agent.pane_id = Some(research_pane.id.clone());
        agent.group_id = group.id.clone();
        agent.worktree_dir = workspace.display().to_string();
        let tree = ResearchTree {
            id: "tree-1".to_string(),
            title: "Legacy research".to_string(),
            root_node_id: "node-1".to_string(),
            workspace_id: String::new(),
            created_at: 1,
            updated_at: 1,
            archived_at: None,
            last_viewed_at: None,
        };
        let node = ResearchNode {
            id: "node-1".to_string(),
            tree_id: tree.id.clone(),
            parent_node_id: None,
            publication_proposal: None,
            query_anchor: None,
            inline: false,
            prompt: "Question".to_string(),
            title: None,
            response_preview: None,
            adapter: "claude".to_string(),
            model: None,
            effort: None,
            group_id: group.id.clone(),
            worktree_dir: workspace.display().to_string(),
            native_session_id: Some("session-abc".to_string()),
            transcript_path: Some("/tmp/transcript.jsonl".to_string()),
            prompt_native_id: None,
            agent_id: Some(agent.id.clone()),
            pane_id: Some(research_pane.id.clone()),
            thread_id: None,
            kind: ResearchNodeKind::Run,
            origin: None,
            status: ResearchNodeStatus::Running,
            error: None,
            response_snapshot_at: None,
            created_at: 1,
            started_at: Some(1),
            completed_at: None,
            highlights: Vec::new(),
        };
        let persisted = PersistedState {
            next_id: 100,
            panes: vec![terminal_pane.clone(), research_pane.clone()],
            groups: vec![group],
            group_order: vec!["group-1".to_string()],
            agents: vec![agent],
            pane_splits: vec![PaneSplitInfo {
                id: "split-1".to_string(),
                pane_ids: vec![terminal_pane.id.clone(), research_pane.id.clone()],
                sizes: HashMap::new(),
                intent: HashMap::new(),
            }],
            research_trees: HashMap::from([(tree.id.clone(), tree)]),
            research_nodes: HashMap::from([(node.id.clone(), node)]),
            ..PersistedState::default()
        };
        persistence::save(&workspace, &persisted).unwrap();

        let restored = AppState::new(test_config(workspace.clone()));
        let recovered_panes = restored.restore_session();
        let detail = restored.research_tree("tree-1").unwrap();
        let research_workspace = restored.group(&detail.tree.workspace_id).unwrap().unwrap();

        assert_eq!(research_workspace.scope, WorkspaceScope::Research);
        assert_ne!(research_workspace.id, "group-1");
        assert_eq!(detail.nodes[0].group_id, research_workspace.id);
        // The migrated run cannot resume across the restart: its pane is
        // dropped from recovery (research panes never respawn) and the node
        // settles Failed instead of resurrecting as a live run.
        assert!(
            recovered_panes
                .iter()
                .all(|pane| pane.id != "pane-research")
        );
        assert_eq!(detail.nodes[0].status, ResearchNodeStatus::Failed);
        assert!(detail.nodes[0].pane_id.is_none());
        assert_eq!(
            recovered_panes
                .iter()
                .find(|pane| pane.id == "pane-terminal")
                .unwrap()
                .group_id,
            "group-1"
        );
        assert!(restored.pane_splits().unwrap().is_empty());
        // The migrated run's agent is reclaimed along with its dropped pane;
        // only the durable node (Failed) records that the run existed.
        assert!(restored.agent("research-agent").unwrap().is_none());
    }

    #[test]
    fn research_workspace_manifest_failure_leaves_legacy_binding_for_retry() {
        let workspace = temp_workspace();
        let mut group = sample_terminal_group();
        group.dir = workspace.display().to_string();
        // The target manifest can be staged, but updating this source manifest
        // must fail. Reconciliation must therefore leave the in-memory shape
        // untouched and remove the staged target directory.
        group.managed_dir = "/dev/null".to_string();
        group.agents.clear();
        let tree = ResearchTree {
            id: "tree-retry".to_string(),
            title: "Retry migration".to_string(),
            root_node_id: "node-retry".to_string(),
            workspace_id: group.id.clone(),
            created_at: 1,
            updated_at: 2,
            archived_at: None,
            last_viewed_at: None,
        };
        let node = ResearchNode {
            id: tree.root_node_id.clone(),
            tree_id: tree.id.clone(),
            parent_node_id: None,
            publication_proposal: None,
            query_anchor: None,
            inline: false,
            prompt: "Question".to_string(),
            title: None,
            response_preview: None,
            adapter: "claude".to_string(),
            model: None,
            effort: None,
            group_id: group.id.clone(),
            worktree_dir: group.dir.clone(),
            native_session_id: Some("session".to_string()),
            transcript_path: None,
            prompt_native_id: None,
            agent_id: None,
            pane_id: None,
            thread_id: None,
            kind: ResearchNodeKind::Run,
            origin: None,
            status: ResearchNodeStatus::Complete,
            error: None,
            response_snapshot_at: None,
            created_at: 1,
            started_at: Some(1),
            completed_at: Some(2),
            highlights: Vec::new(),
        };
        let mut persisted = PersistedState {
            groups: vec![group.clone()],
            group_order: vec![group.id.clone()],
            research_trees: HashMap::from([(tree.id.clone(), tree)]),
            research_nodes: HashMap::from([(node.id.clone(), node)]),
            ..PersistedState::default()
        };
        let state = AppState::new(test_config(workspace.clone()));

        let (changed, warnings) = migrate_legacy_research_workspaces(&state, &mut persisted);

        assert!(!changed);
        assert_eq!(persisted.groups.len(), 1);
        assert_eq!(persisted.groups[0].scope, WorkspaceScope::Terminal);
        assert_eq!(
            persisted.research_trees["tree-retry"].workspace_id,
            group.id
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("could not update legacy"))
        );
        assert!(
            std::fs::read_dir(&workspace).unwrap().all(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                == ".qmux")
        );
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn restore_rehomes_missing_legacy_group_from_root_provenance() {
        let workspace = temp_workspace();
        let legacy_dir = workspace.join("moved-project");
        let tree = ResearchTree {
            id: "tree-missing".to_string(),
            title: "Recovered research".to_string(),
            root_node_id: "node-missing".to_string(),
            workspace_id: "missing-group".to_string(),
            created_at: 1,
            updated_at: 2,
            archived_at: None,
            last_viewed_at: None,
        };
        let node = ResearchNode {
            id: "node-missing".to_string(),
            tree_id: tree.id.clone(),
            parent_node_id: None,
            publication_proposal: None,
            query_anchor: None,
            inline: false,
            prompt: "Question".to_string(),
            title: None,
            response_preview: Some("Answer".to_string()),
            adapter: "claude".to_string(),
            model: None,
            effort: None,
            group_id: "missing-group".to_string(),
            worktree_dir: legacy_dir.display().to_string(),
            native_session_id: Some("session".to_string()),
            transcript_path: None,
            prompt_native_id: None,
            agent_id: None,
            pane_id: None,
            thread_id: None,
            kind: ResearchNodeKind::Run,
            origin: None,
            status: ResearchNodeStatus::Complete,
            error: None,
            response_snapshot_at: None,
            created_at: 1,
            started_at: Some(1),
            completed_at: Some(2),
            highlights: Vec::new(),
        };
        let persisted = PersistedState {
            research_trees: HashMap::from([(tree.id.clone(), tree)]),
            research_nodes: HashMap::from([(node.id.clone(), node)]),
            ..PersistedState::default()
        };
        persistence::save(&workspace, &persisted).unwrap();

        let restored = AppState::new(test_config(workspace.clone()));
        restored.restore_session();
        let detail = restored.research_tree("tree-missing").unwrap();
        let research_workspace = restored
            .group(&detail.tree.workspace_id)
            .unwrap()
            .expect("provenance creates a replacement workspace record");
        assert_eq!(research_workspace.scope, WorkspaceScope::Research);
        assert_eq!(research_workspace.dir, legacy_dir.display().to_string());
        assert_eq!(detail.nodes[0].group_id, research_workspace.id);
        assert!(restored.take_recovery_warning().is_none());
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn restore_merges_legacy_groups_that_share_one_directory() {
        let workspace = temp_workspace();
        let shared_dir = workspace.join("shared-project");
        std::fs::create_dir_all(&shared_dir).unwrap();
        let mut groups = Vec::new();
        let mut trees = HashMap::new();
        let mut nodes = HashMap::new();
        for index in 1..=2 {
            let group_id = format!("legacy-{index}");
            let managed_dir = workspace.join(format!("managed-{index}"));
            std::fs::create_dir_all(managed_dir.join(".qmux")).unwrap();
            let mut group = sample_terminal_group();
            group.id = group_id.clone();
            group.dir = shared_dir.display().to_string();
            group.managed_dir = managed_dir.display().to_string();
            group.agents.clear();
            groups.push(group);
            let tree_id = format!("tree-{index}");
            let node_id = format!("node-{index}");
            trees.insert(
                tree_id.clone(),
                ResearchTree {
                    id: tree_id.clone(),
                    title: tree_id.clone(),
                    root_node_id: node_id.clone(),
                    workspace_id: group_id.clone(),
                    created_at: 1,
                    updated_at: 2,
                    archived_at: None,
                    last_viewed_at: None,
                },
            );
            nodes.insert(
                node_id.clone(),
                ResearchNode {
                    id: node_id,
                    tree_id,
                    parent_node_id: None,
                    publication_proposal: None,
                    query_anchor: None,
                    inline: false,
                    prompt: "Question".to_string(),
                    title: None,
                    response_preview: None,
                    adapter: "claude".to_string(),
                    model: None,
                    effort: None,
                    group_id,
                    worktree_dir: shared_dir.display().to_string(),
                    native_session_id: Some(format!("session-{index}")),
                    transcript_path: None,
                    prompt_native_id: None,
                    agent_id: None,
                    pane_id: None,
                    thread_id: None,
                    kind: ResearchNodeKind::Run,
                    origin: None,
                    status: ResearchNodeStatus::Complete,
                    error: None,
                    response_snapshot_at: None,
                    created_at: 1,
                    started_at: Some(1),
                    completed_at: Some(2),
                    highlights: Vec::new(),
                },
            );
        }
        let persisted = PersistedState {
            groups,
            group_order: vec!["legacy-1".to_string(), "legacy-2".to_string()],
            research_trees: trees,
            research_nodes: nodes,
            ..PersistedState::default()
        };
        persistence::save(&workspace, &persisted).unwrap();

        let restored = AppState::new(test_config(workspace.clone()));
        restored.restore_session();
        let first = restored.research_tree("tree-1").unwrap().tree.workspace_id;
        let second = restored.research_tree("tree-2").unwrap().tree.workspace_id;
        assert_eq!(first, second);
        assert_eq!(restored.list_research_workspaces().unwrap().len(), 1);
        let restored_again = AppState::new(test_config(workspace.clone()));
        restored_again.restore_session();
        assert_eq!(
            restored_again
                .research_tree("tree-1")
                .unwrap()
                .tree
                .workspace_id,
            first
        );
        assert_eq!(restored_again.list_research_workspaces().unwrap().len(), 1);
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn queue_idle_completion_retires_research_pane_without_creating_an_undo_entry() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let agent = sample_agent("research-agent");
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-7")
            .unwrap();
        state
            .append_turn(sample_user_turn("research-agent", "Root"))
            .unwrap();
        let mut answer = sample_user_turn("research-agent", "Durable response");
        answer.role = "assistant".to_string();
        answer.id = "research-agent-answer".to_string();
        state.append_turn(answer).unwrap();
        // Codex's deferred Stop resolver reaches Done through this atomic
        // queue/typing decision rather than set_agent_status. That path must
        // still settle the research node and start automatic retirement.
        assert!(matches!(
            state
                .claim_next_turn_or_settle("research-agent", AgentStatus::Done)
                .unwrap(),
            IdleAdvance::Idle
        ));

        // Retirement now needs at least two snapshot reads (250ms + 500ms
        // backoff) to prove the response is stable before it closes the pane.
        for _ in 0..500 {
            if state.list_panes().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(state.list_panes().unwrap().is_empty());
        let node = state.research_node(&detail.tree.root_node_id).unwrap();
        assert!(node.pane_id.is_none());
        assert_eq!(node.agent_id.as_deref(), Some("research-agent"));
        assert!(state.take_last_closed_pane().unwrap().is_none());
        assert!(state.group("group-1").unwrap().is_some());
        let snapshot = research::read_response_snapshot(
            &state.config().workspace_root,
            &detail.tree.root_node_id,
        )
        .unwrap()
        .unwrap();
        assert_eq!(snapshot.len(), 1);
    }

    #[test]
    fn complete_run_without_checkpoint_still_retires_and_snapshots_live_turns() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        // An adapter whose session hooks never fired: no session id, no
        // transcript path. Waiting for the checkpoint before scheduling
        // retirement leaked this (hidden) pane forever.
        let mut agent = sample_agent("research-agent");
        agent.session_id = None;
        agent.transcript_path = None;
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-7")
            .unwrap();
        state
            .append_turn(sample_user_turn("research-agent", "Root"))
            .unwrap();
        let mut answer = sample_user_turn("research-agent", "Answer without a checkpoint");
        answer.role = "assistant".to_string();
        answer.id = "research-agent-answer".to_string();
        state.append_turn(answer).unwrap();
        state
            .set_agent_status("research-agent", AgentStatus::Done)
            .unwrap();

        for _ in 0..500 {
            if state.list_panes().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(state.list_panes().unwrap().is_empty());
        let node = state.research_node(&detail.tree.root_node_id).unwrap();
        assert_eq!(node.status, ResearchNodeStatus::Complete);
        assert!(node.pane_id.is_none());
        assert!(node.native_session_id.is_none());
        // The answer survives durably via the live turns even though the
        // adapter transcript never materialized.
        let snapshot = research::read_response_snapshot(
            &state.config().workspace_root,
            &detail.tree.root_node_id,
        )
        .unwrap()
        .unwrap();
        assert_eq!(snapshot.len(), 1);
        assert!(node.response_snapshot_at.is_some());
    }

    #[test]
    fn research_document_edits_replace_content_clear_highlights_and_preserve_children() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace.clone()));
        state.restore_session();
        let mut group = sample_group();
        group.dir = workspace.display().to_string();
        group.managed_dir = workspace.join("managed").display().to_string();
        group.agents.clear();
        state.insert_group_after(group, None).unwrap();
        let detail = state
            .create_research_document(CreateResearchDocumentRequest {
                markdown: "# Original\n\nBody".to_string(),
                title: Some("Original title".to_string()),
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let node_id = detail.tree.root_node_id.clone();
        let original_snapshot =
            research::read_response_snapshot_with_revision(&workspace, &node_id)
                .unwrap()
                .unwrap();
        let highlight = state
            .create_research_highlight(
                &node_id,
                ResearchHighlightAnchor {
                    version: 1,
                    projection: "answer-v1".to_string(),
                    response_revision: original_snapshot.revision.clone(),
                    start: 0,
                    end: 10,
                    exact: "# Original".to_string(),
                    prefix: String::new(),
                    suffix: "\n\nBody".to_string(),
                },
            )
            .unwrap();
        let child = state
            .create_research_child(&node_id, "What changed?".to_string(), None, false)
            .unwrap();
        let captured_before_edit = state
            .research_document_followup_prompt(&node_id, &child.prompt)
            .unwrap();
        assert!(captured_before_edit.contains("# Original\n\nBody"));

        let updated = state
            .update_research_document(UpdateResearchDocumentRequest {
                node_id: node_id.clone(),
                markdown: "# Revised\n\nNew body".to_string(),
                title: Some("Revised title".to_string()),
                expected_response_revision: original_snapshot.revision.clone(),
                expected_title: "Original title".to_string(),
                expected_highlight_ids: vec![highlight.id.clone()],
            })
            .unwrap();
        assert!(updated.markdown_changed);
        assert_eq!(updated.removed_highlight_count, 1);
        assert_ne!(updated.response_revision, original_snapshot.revision);
        assert_eq!(updated.tree.title, "Revised title");
        assert!(updated.node.highlights.is_empty());
        assert_eq!(
            state.research_node(&child.id).unwrap().prompt,
            "What changed?"
        );
        // The string already captured for the child owns the old document;
        // future direct follow-ups read the new snapshot.
        assert!(!captured_before_edit.contains("# Revised"));
        let captured_after_edit = state
            .research_document_followup_prompt(&node_id, "What now?")
            .unwrap();
        assert!(captured_after_edit.contains("# Revised\n\nNew body"));
        let revised_snapshot = research::read_response_snapshot_with_revision(&workspace, &node_id)
            .unwrap()
            .unwrap();
        assert_eq!(revised_snapshot.revision, updated.response_revision);
        assert_eq!(
            research::document_markdown_from_turns(&revised_snapshot.turns),
            Some("# Revised\n\nNew body")
        );

        let stale_title = state
            .update_research_document(UpdateResearchDocumentRequest {
                node_id: node_id.clone(),
                markdown: "stale overwrite".to_string(),
                title: Some("stale title".to_string()),
                expected_response_revision: revised_snapshot.revision.clone(),
                expected_title: "Original title".to_string(),
                expected_highlight_ids: Vec::new(),
            })
            .unwrap_err();
        assert!(stale_title.contains("title changed"));
        let stale_body = state
            .update_research_document(UpdateResearchDocumentRequest {
                node_id: node_id.clone(),
                markdown: "stale overwrite".to_string(),
                title: Some("stale title".to_string()),
                expected_response_revision: original_snapshot.revision,
                expected_title: "Revised title".to_string(),
                expected_highlight_ids: Vec::new(),
            })
            .unwrap_err();
        assert!(stale_body.contains("document changed"));

        let preserved = state
            .create_research_highlight(
                &node_id,
                ResearchHighlightAnchor {
                    response_revision: revised_snapshot.revision.clone(),
                    ..highlight.anchor
                },
            )
            .unwrap();
        let title_only = state
            .update_research_document(UpdateResearchDocumentRequest {
                node_id: node_id.clone(),
                markdown: "# Revised\n\nNew body".to_string(),
                title: Some("Title only".to_string()),
                expected_response_revision: revised_snapshot.revision.clone(),
                expected_title: "Revised title".to_string(),
                expected_highlight_ids: vec![preserved.id.clone()],
            })
            .unwrap();
        assert!(!title_only.markdown_changed);
        assert_eq!(title_only.response_revision, revised_snapshot.revision);
        assert_eq!(title_only.removed_highlight_count, 0);
        assert_eq!(title_only.node.highlights, vec![preserved]);

        // A highlight created after the editor opened was never represented in
        // its warning. Refuse to erase that unseen highlight with a body save.
        let highlight_ids_at_open = title_only
            .node
            .highlights
            .iter()
            .map(|highlight| highlight.id.clone())
            .collect::<Vec<_>>();
        let concurrent_highlight = state
            .create_research_highlight(
                &node_id,
                ResearchHighlightAnchor {
                    version: 1,
                    projection: "answer-v1".to_string(),
                    response_revision: revised_snapshot.revision.clone(),
                    start: 11,
                    end: 19,
                    exact: "New body".to_string(),
                    prefix: "# Revised\n\n".to_string(),
                    suffix: String::new(),
                },
            )
            .unwrap();
        let stale_highlights = state
            .update_research_document(UpdateResearchDocumentRequest {
                node_id: node_id.clone(),
                markdown: "# Another revision".to_string(),
                title: Some("Another revision".to_string()),
                expected_response_revision: revised_snapshot.revision.clone(),
                expected_title: "Title only".to_string(),
                expected_highlight_ids: highlight_ids_at_open,
            })
            .unwrap_err();
        assert!(stale_highlights.contains("highlights changed"));
        assert_eq!(state.research_node(&node_id).unwrap().highlights.len(), 2);
        assert_eq!(
            research::read_response_snapshot_with_revision(&workspace, &node_id)
                .unwrap()
                .unwrap()
                .revision,
            revised_snapshot.revision
        );
        state
            .remove_research_highlight(&node_id, &concurrent_highlight.id)
            .unwrap();

        state.cancel_research_node(&child.id).unwrap();
        state.archive_research_tree(&detail.tree.id).unwrap();
        let archived = state
            .update_research_document(UpdateResearchDocumentRequest {
                node_id: node_id.clone(),
                markdown: "another body".to_string(),
                title: Some("Archived edit".to_string()),
                expected_response_revision: title_only.response_revision,
                expected_title: "Title only".to_string(),
                expected_highlight_ids: title_only
                    .node
                    .highlights
                    .iter()
                    .map(|highlight| highlight.id.clone())
                    .collect(),
            })
            .unwrap_err();
        assert!(archived.contains("restore archived"));

        let reloaded = AppState::new(test_config(workspace.clone()));
        reloaded.restore_session();
        let reloaded_detail = reloaded.research_tree(&detail.tree.id).unwrap();
        assert_eq!(reloaded_detail.tree.title, "Title only");
        assert_eq!(
            reloaded.research_node(&node_id).unwrap().highlights.len(),
            1
        );
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn research_highlights_require_and_track_a_durable_snapshot() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace.clone()));
        state.restore_session();
        state.insert_group_after(sample_group(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let node_id = detail.tree.root_node_id;
        let mut anchor = ResearchHighlightAnchor {
            version: 1,
            projection: "answer-v1".to_string(),
            response_revision: "0".repeat(64),
            start: 0,
            end: 6,
            exact: "Answer".to_string(),
            prefix: String::new(),
            suffix: " text".to_string(),
        };

        let err = state
            .create_research_highlight(&node_id, anchor.clone())
            .unwrap_err();
        assert!(err.contains("durable full response snapshot"));

        let mut answer = sample_user_turn("research-agent", "Answer text");
        answer.role = "assistant".to_string();
        let turns = vec![answer];
        research::write_response_snapshot(&workspace, &node_id, &turns).unwrap();

        let err = state
            .create_research_highlight(&node_id, anchor.clone())
            .unwrap_err();
        assert!(err.contains("response changed"));

        anchor.response_revision = research::response_revision(&turns).unwrap();
        let highlight = state
            .create_research_highlight(&node_id, anchor.clone())
            .unwrap();
        assert_eq!(highlight.anchor, anchor);
        // The snapshot file itself is the durability authority. Creation must
        // still work after a crash between committing it and stamping the node.
        assert!(
            state
                .research_node(&node_id)
                .unwrap()
                .response_snapshot_at
                .is_none()
        );

        {
            let mut model = state.inner.model.lock().unwrap();
            let node = model.research_nodes.get_mut(&node_id).unwrap();
            node.highlights = vec![highlight.clone(); research::MAX_RESEARCH_HIGHLIGHTS_PER_NODE];
        }
        let err = state
            .create_research_highlight(&node_id, anchor.clone())
            .unwrap_err();
        assert!(err.contains("at most"));
        {
            let mut model = state.inner.model.lock().unwrap();
            let node = model.research_nodes.get_mut(&node_id).unwrap();
            node.highlights = vec![highlight.clone()];
        }

        let reloaded = AppState::new(test_config(workspace.clone()));
        reloaded.restore_session();
        let saved_node = reloaded.research_node(&node_id).unwrap();
        assert_eq!(saved_node.highlights, vec![highlight.clone()]);

        let second = reloaded
            .create_research_highlight(&node_id, anchor.clone())
            .unwrap();
        let preserved = reloaded
            .create_research_highlight(&node_id, anchor.clone())
            .unwrap();
        let removed = reloaded
            .remove_research_highlights(
                &node_id,
                &[
                    highlight.id.clone(),
                    second.id.clone(),
                    highlight.id.clone(),
                    "already-removed".to_string(),
                ],
            )
            .unwrap();
        assert_eq!(removed, vec![highlight, second]);
        assert_eq!(
            reloaded.research_node(&node_id).unwrap().highlights,
            vec![preserved.clone()]
        );
        let reloaded = AppState::new(test_config(workspace.clone()));
        reloaded.restore_session();
        assert_eq!(
            reloaded.research_node(&node_id).unwrap().highlights,
            vec![preserved.clone()]
        );
        let removed = reloaded
            .remove_research_highlight(&node_id, &preserved.id)
            .unwrap();
        assert_eq!(removed, preserved);

        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn detach_settles_a_finished_agents_run_complete_not_failed() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let mut agent = sample_agent("research-agent");
        agent.status = AgentStatus::Running;
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-7")
            .unwrap();
        // The process exits right after finishing its turn: the agent record
        // already says Done, but the node sync lost the race with the pane
        // teardown (insert_agent does not sync research nodes, mirroring it).
        agent.status = AgentStatus::Done;
        state.insert_agent(agent).unwrap();

        let node = state.detach_research_pane("pane-7").unwrap().unwrap();
        assert_eq!(node.status, ResearchNodeStatus::Complete);
        assert!(node.error.is_none());

        // A genuine crash — the agent never reported end-of-turn — still
        // settles Failed. (The Complete parent above carries the checkpoint
        // the bind recorded, so a follow-up child can be created from it.)
        state.insert_pane(sample_pane_runtime("pane-8")).unwrap();
        let crash = state
            .create_research_child(
                &detail.tree.root_node_id,
                "Follow-up".to_string(),
                None,
                false,
            )
            .unwrap();
        let mut crash_agent = sample_agent("crash-agent");
        crash_agent.pane_id = Some("pane-8".to_string());
        crash_agent.status = AgentStatus::Running;
        state.insert_agent(crash_agent.clone()).unwrap();
        state
            .bind_research_node_run(&crash.id, &crash_agent, "pane-8")
            .unwrap();
        let node = state.detach_research_pane("pane-8").unwrap().unwrap();
        assert_eq!(node.status, ResearchNodeStatus::Failed);
        assert!(
            node.error
                .as_deref()
                .unwrap_or_default()
                .contains("exited before completion")
        );
    }

    #[test]
    fn remove_pane_settles_a_finished_agents_run_complete_not_failed() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let mut agent = sample_agent("research-agent");
        agent.status = AgentStatus::Running;
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-7")
            .unwrap();
        // The process exits right after finishing its turn: the Stop hook
        // recorded Done on the agent record, but the node sync lost the race
        // with the pane teardown. Unlike the direct-detach test above, the
        // production path — remove_pane — prunes the agent record before the
        // detach runs, so the detach must read the pre-removal status.
        agent.status = AgentStatus::Done;
        state.insert_agent(agent).unwrap();

        state.remove_pane("pane-7").unwrap();

        let node = state.research_node(&detail.tree.root_node_id).unwrap();
        assert_eq!(node.status, ResearchNodeStatus::Complete);
        assert!(node.error.is_none());
        assert!(node.pane_id.is_none());
    }

    #[test]
    fn late_agent_sync_does_not_rebind_a_settled_nodes_removed_pane() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let mut agent = sample_agent("research-agent");
        agent.status = AgentStatus::Running;
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-7")
            .unwrap();
        // Teardown settles the node and clears the binding...
        state.remove_pane("pane-7").unwrap();
        let node = state.research_node(&detail.tree.root_node_id).unwrap();
        assert!(node.status.is_terminal());
        assert!(node.pane_id.is_none());
        // ...then a hook-thread sync built from a snapshot taken before the
        // teardown lands late. It must not re-bind the removed pane: nothing
        // would ever clear it again, and the tree would count as having an
        // active run (blocking removal) until restart.
        state.sync_research_node_from_agent(&agent).unwrap();
        let node = state.research_node(&detail.tree.root_node_id).unwrap();
        assert!(node.pane_id.is_none());
        state.remove_research_tree(&detail.tree.id).unwrap();
    }

    #[test]
    fn queue_wait_turn_is_rejected_for_research_runs() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let mut agent = sample_agent("research-agent");
        agent.status = AgentStatus::Running;
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-7")
            .unwrap();
        let mut target = sample_agent("target-agent");
        target.pane_id = Some("pane-8".to_string());
        state.insert_agent(target).unwrap();

        // A research run never drains its queue, so a wait-for turn accepted
        // here would park the agent as an orphaned-queue zombie at retirement.
        let err = crate::turn_queue::queue_wait_agent_turn(
            &state,
            crate::turn_queue::QueueWaitAgentTurnRequest {
                agent_id: "research-agent".to_string(),
                data: "after the other agent".to_string(),
                wait_for_agent_id: "target-agent".to_string(),
                wait_for_pane_id: None,
                wait_for_label: None,
            },
        )
        .unwrap_err();
        assert!(err.contains("read-only"));
        assert!(
            state
                .agent_queued_turns("research-agent")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn remove_research_tree_reaps_the_runs_thread_records() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let mut agent = sample_agent("research-agent");
        agent.status = AgentStatus::Running;
        agent.thread_id = Some("thread-research".to_string());
        agent.branch_id = Some("branch-research".to_string());
        state.insert_agent(agent.clone()).unwrap();
        // Mint the thread record and graph snapshot the way a live run does:
        // the transcript tail appends turns through the thread store.
        state
            .append_turn(sample_user_turn("research-agent", "Root"))
            .unwrap();
        state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-7")
            .unwrap();
        state.remove_pane("pane-7").unwrap();

        let snapshot_path = {
            let model = state.inner.model.lock().unwrap();
            let record = model
                .threads
                .get("thread-research")
                .expect("run minted a thread record");
            record.snapshot_path.clone()
        };

        state.remove_research_tree(&detail.tree.id).unwrap();

        let model = state.inner.model.lock().unwrap();
        assert!(!model.threads.contains_key("thread-research"));
        assert!(!model.thread_focus.contains_key("thread-research"));
        drop(model);
        assert!(!std::path::Path::new(&snapshot_path).exists());
    }

    #[test]
    fn research_workspace_detach_reaps_the_runs_thread_records() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let mut agent = sample_agent("research-agent");
        agent.status = AgentStatus::Running;
        agent.thread_id = Some("thread-research".to_string());
        agent.branch_id = Some("branch-research".to_string());
        state.insert_agent(agent.clone()).unwrap();
        state
            .append_turn(sample_user_turn("research-agent", "Root"))
            .unwrap();
        state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-7")
            .unwrap();
        state.remove_pane("pane-7").unwrap();

        let snapshot_path = {
            let model = state.inner.model.lock().unwrap();
            model
                .threads
                .get("thread-research")
                .expect("run minted a thread record")
                .snapshot_path
                .clone()
        };
        let archive = state.detached_research_archive("group-1").unwrap();

        state
            .commit_research_workspace_detach("group-1", &archive)
            .unwrap();

        let model = state.inner.model.lock().unwrap();
        assert!(!model.threads.contains_key("thread-research"));
        assert!(!model.thread_focus.contains_key("thread-research"));
        drop(model);
        assert!(!std::path::Path::new(&snapshot_path).exists());
    }

    #[test]
    fn cancel_clears_a_dangling_binding_when_the_pane_record_is_gone() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let mut agent = sample_agent("research-agent");
        agent.status = AgentStatus::Running;
        agent.pane_id = Some("pane-ghost".to_string());
        state.insert_agent(agent.clone()).unwrap();
        state
            .insert_pane(sample_pane_runtime("pane-ghost"))
            .unwrap();
        state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-ghost")
            .unwrap();
        // Leave the node bound to a pane whose record no longer exists (the
        // stuck-binding shape: a teardown that lost its research detach, e.g.
        // a kill that failed after the pane record was already pruned). Cancel
        // must still reclaim the binding — there is no EOF/teardown left to do
        // it — or the settled node pins the tree as an active run until
        // restart. Dropped directly because every ordinary removal path now
        // runs the detach itself.
        state.inner.model.lock().unwrap().panes.remove("pane-ghost");

        let node = state
            .cancel_research_node(&detail.tree.root_node_id)
            .unwrap();
        assert_eq!(node.status, ResearchNodeStatus::Cancelled);
        assert!(node.pane_id.is_none());
        state.remove_research_tree(&detail.tree.id).unwrap();
    }

    #[test]
    fn binding_after_the_panes_teardown_settles_instead_of_pinning_the_tree() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        // An instantly-exiting process (missing binary, adapter arg error):
        // the reader thread's EOF teardown ran the whole remove_pane —
        // including its research detach, which found nothing bound — before
        // the launch path could bind. The bind must not resurrect the dead
        // pane id: nothing would ever settle or unbind the node again, and
        // the phantom "active" run would pin the tree until a manual cancel
        // or restart.
        let mut agent = sample_agent("research-agent");
        agent.status = AgentStatus::Running;
        let node = state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-7")
            .unwrap();
        assert!(node.pane_id.is_none());
        assert_eq!(node.status, ResearchNodeStatus::Failed);
        assert!(node.error.is_some());
        // The launch context is still recorded for diagnostics/fallbacks.
        assert_eq!(node.agent_id.as_deref(), Some("research-agent"));
        // Not pinned: the settled tree can be removed without a restart.
        state.remove_research_tree(&detail.tree.id).unwrap();
    }

    #[test]
    fn binding_after_teardown_keeps_a_finished_agents_run_complete() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        // Same teardown-before-bind ordering, but the agent snapshot already
        // carries end-of-turn: the run finished, so settling it Failed would
        // brand a delivered answer (mirrors detach_research_pane's check).
        let mut agent = sample_agent("research-agent");
        agent.status = AgentStatus::Done;
        let node = state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-7")
            .unwrap();
        assert!(node.pane_id.is_none());
        assert_eq!(node.status, ResearchNodeStatus::Complete);
        assert!(node.error.is_none());
        state.remove_research_tree(&detail.tree.id).unwrap();
    }

    #[test]
    fn unseen_failure_badge_clears_when_the_tree_is_viewed() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        // Creation stamps last_viewed_at; the failure must settle strictly
        // later for the unseen comparison (millisecond clock) to see it.
        std::thread::sleep(std::time::Duration::from_millis(2));
        state
            .fail_research_node(&detail.tree.root_node_id, "boom".to_string())
            .unwrap();

        let summary = state.list_research_trees().unwrap().remove(0);
        assert_eq!(summary.failed_count, 1);
        assert!(summary.has_unseen_failure);
        assert!(summary.has_unseen_update);

        state.mark_research_tree_viewed(&detail.tree.id).unwrap();
        let summary = state.list_research_trees().unwrap().remove(0);
        // Viewing acknowledges the failure; the lifetime count remains for
        // detail displays but the attention flags clear.
        assert_eq!(summary.failed_count, 1);
        assert!(!summary.has_unseen_failure);
        assert!(!summary.has_unseen_update);
    }

    #[test]
    fn failed_research_pane_retires_instead_of_becoming_hidden_orphan() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let agent = sample_agent("research-agent");
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-7")
            .unwrap();
        state
            .set_agent_status("research-agent", AgentStatus::Failed)
            .unwrap();

        for _ in 0..200 {
            if state.list_panes().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(state.list_panes().unwrap().is_empty());
        let node = state.research_node(&detail.tree.root_node_id).unwrap();
        assert_eq!(node.status, ResearchNodeStatus::Failed);
        assert!(node.pane_id.is_none());
        assert!(state.take_last_closed_pane().unwrap().is_none());
    }

    #[test]
    fn research_snapshot_requires_a_stable_response_with_an_assistant_turn() {
        let state = AppState::new(test_config(temp_workspace()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "Root".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let root_id = detail.tree.root_node_id.clone();
        let agent = sample_agent("research-agent");
        state.insert_agent(agent.clone()).unwrap();
        state
            .bind_research_node_run(&root_id, &agent, "pane-7")
            .unwrap();
        state
            .append_turn(sample_user_turn("research-agent", "Root"))
            .unwrap();

        // A prompt-only transcript (the adapter has not flushed the answer yet)
        // must never become the durable snapshot.
        let mut candidate = None;
        let err = state
            .snapshot_research_response(&root_id, &mut candidate)
            .unwrap_err();
        assert!(err.contains("not available yet"), "{err}");

        // A response tail without any assistant turn (e.g. only a flushed tool
        // result so far) is a partial response, not a finished answer.
        let mut tool_result = sample_user_turn("research-agent", "tool");
        tool_result.id = "research-agent-tool".to_string();
        tool_result.source_index = 1;
        tool_result.blocks = vec![crate::transcript::TurnBlock::ToolResult {
            tool_use_id: Some("tool-1".to_string()),
            content: serde_json::json!("output"),
            is_error: false,
        }];
        state.append_turn(tool_result).unwrap();
        let err = state
            .snapshot_research_response(&root_id, &mut candidate)
            .unwrap_err();
        assert!(err.contains("no assistant turn"), "{err}");

        let mut answer = sample_user_turn("research-agent", "Partial answer");
        answer.id = "research-agent-1".to_string();
        answer.role = "assistant".to_string();
        answer.source_index = 2;
        state.append_turn(answer).unwrap();

        // The first read of a parseable response is only a candidate; nothing
        // is committed until a second read proves it stopped changing.
        let err = state
            .snapshot_research_response(&root_id, &mut candidate)
            .unwrap_err();
        assert!(err.contains("not settled"), "{err}");
        assert!(
            research::read_response_snapshot(&state.config().workspace_root, &root_id)
                .unwrap()
                .is_none()
        );

        // A response that grew between reads restarts the stability window.
        let mut more = sample_user_turn("research-agent", "The full answer");
        more.id = "research-agent-2".to_string();
        more.role = "assistant".to_string();
        more.source_index = 3;
        state.append_turn(more).unwrap();
        let err = state
            .snapshot_research_response(&root_id, &mut candidate)
            .unwrap_err();
        assert!(err.contains("not settled"), "{err}");

        // Two identical consecutive reads finally commit the snapshot.
        state
            .snapshot_research_response(&root_id, &mut candidate)
            .unwrap();
        let snapshot = research::read_response_snapshot(&state.config().workspace_root, &root_id)
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.len(), 3);
        assert_eq!(snapshot[2].id, "research-agent-2");
    }

    #[derive(Debug)]
    struct FakeChild;

    impl ChildKiller for FakeChild {
        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(FakeChild)
        }
    }

    impl Child for FakeChild {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            Ok(None)
        }

        fn wait(&mut self) -> io::Result<ExitStatus> {
            Ok(ExitStatus::with_exit_code(0))
        }

        fn process_id(&self) -> Option<u32> {
            None
        }
    }

    fn sample_pane_runtime(id: &str) -> PaneRuntime {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        drop(pair.slave);

        PaneRuntime {
            info: sample_pane(id, None),
            backend: PaneBackend::HostPty {
                child: Arc::new(Mutex::new(Box::new(FakeChild))),
                master: Arc::new(Mutex::new(pair.master)),
                writer: Arc::new(Mutex::new(Box::new(io::sink()))),
                backlog: Default::default(),
                native_surface: false,
            },
        }
    }

    fn sample_user_turn(agent_id: &str, text: &str) -> Turn {
        Turn {
            id: format!("{agent_id}-0"),
            agent_id: agent_id.to_string(),
            session_id: Some("session-abc".to_string()),
            role: "user".to_string(),
            blocks: vec![crate::transcript::TurnBlock::Text {
                text: text.to_string(),
            }],
            source_index: 0,
            timestamp: None,
            status: None,
            status_reason: None,
            native_id: None,
            parent_native_id: None,
            native_message_id: None,
        }
    }

    // A tail re-checks its binding inside the write: a rebind (rewind
    // rotation, session picker choice, recovery) landing between a tail's
    // loop-top check and its write must not let the dead file's parse land
    // over the new transcript's timeline.
    #[test]
    fn transcript_scoped_turn_writes_apply_only_while_bound() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace.clone()));
        let mut agent = sample_agent("agent-1");
        agent.worktree_dir = workspace.display().to_string();
        agent.transcript_path = Some("/tmp/current.jsonl".to_string());
        state.insert_agent(agent).unwrap();

        assert!(
            state
                .append_turn_for_transcript(
                    sample_user_turn("agent-1", "live"),
                    "/tmp/current.jsonl"
                )
                .unwrap()
        );

        let mut stale = sample_user_turn("agent-1", "stale");
        stale.id = "agent-1-9".to_string();
        stale.source_index = 9;
        assert!(
            !state
                .append_turn_for_transcript(stale, "/tmp/old.jsonl")
                .unwrap()
        );
        assert!(
            !state
                .replace_turns_for_transcript(
                    "agent-1",
                    "/tmp/old.jsonl",
                    vec![sample_user_turn("agent-1", "stale history")],
                )
                .unwrap()
        );

        let turns = state.list_turns(Some("agent-1")).unwrap();
        assert_eq!(turns.len(), 1);
        match turns[0].blocks.as_slice() {
            [crate::transcript::TurnBlock::Text { text }] => assert_eq!(text, "live"),
            blocks => panic!("unexpected blocks: {blocks:?}"),
        }

        assert!(
            state
                .replace_turns_for_transcript(
                    "agent-1",
                    "/tmp/current.jsonl",
                    vec![sample_user_turn("agent-1", "refreshed")],
                )
                .unwrap()
        );
        let turns = state.list_turns(Some("agent-1")).unwrap();
        assert_eq!(turns.len(), 1);
        match turns[0].blocks.as_slice() {
            [crate::transcript::TurnBlock::Text { text }] => assert_eq!(text, "refreshed"),
            blocks => panic!("unexpected blocks: {blocks:?}"),
        }

        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn shared_thread_turn_writes_use_global_storage_root() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace.clone()));
        let source_root = workspace.join("source-worktree");
        let target_root = workspace.join("target-worktree");
        let source_root_string = source_root.display().to_string();
        let target_root_string = target_root.display().to_string();

        let mut source = sample_agent("source");
        source.worktree_dir = source_root_string.clone();
        source.thread_id = Some("thread-shared".to_string());
        source.branch_id = Some("branch-source".to_string());
        let mut target = sample_agent("target");
        target.worktree_dir = target_root_string.clone();
        target.thread_id = Some("thread-shared".to_string());
        target.branch_id = Some("branch-target".to_string());

        state.insert_agent(source).unwrap();
        state.insert_agent(target).unwrap();
        state
            .append_turn(sample_user_turn("source", "source turn"))
            .unwrap();
        state
            .append_turn(sample_user_turn("target", "target turn"))
            .unwrap();

        let workspace_string = workspace.display().to_string();
        let shared_graph = thread_graph::read_snapshot(&workspace_string, "thread-shared")
            .unwrap()
            .expect("shared graph exists at global thread root");
        assert!(shared_graph.nodes.contains_key("source-0"));
        assert!(shared_graph.nodes.contains_key("target-0"));
        assert!(shared_graph.branches.contains_key("branch-source"));
        assert!(shared_graph.branches.contains_key("branch-target"));
        assert!(
            thread_graph::read_snapshot(&source_root_string, "thread-shared")
                .unwrap()
                .is_none()
        );
        assert!(
            thread_graph::read_snapshot(&target_root_string, "thread-shared")
                .unwrap()
                .is_none()
        );
        let model = state.inner.model.lock().unwrap();
        let record = model.threads.get("thread-shared").unwrap();
        assert_eq!(record.storage_root, workspace_string);
        drop(model);

        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn restore_migrates_legacy_thread_record_to_global_storage() {
        let workspace = temp_workspace();
        let legacy_root = workspace.join("legacy-worktree");
        let mut agent = sample_agent("legacy");
        agent.worktree_dir = legacy_root.display().to_string();
        agent.thread_id = Some("thread-legacy".to_string());
        agent.branch_id = Some("branch-legacy".to_string());
        thread_graph::ThreadStore::new(legacy_root.clone())
            .append_turn_node(&agent, &sample_user_turn("legacy", "legacy turn"))
            .unwrap();

        let mut persisted = PersistedState::default();
        persisted.threads.insert(
            "thread-legacy".to_string(),
            thread_graph::thread_record_for_agent(&agent, "branch-legacy", &legacy_root),
        );
        persistence::save(&workspace, &persisted).unwrap();

        let state = AppState::new(test_config(workspace.clone()));
        state.restore_session();

        let workspace_string = workspace.display().to_string();
        let model = state.inner.model.lock().unwrap();
        let record = model.threads.get("thread-legacy").unwrap();
        assert_eq!(record.storage_root, workspace_string);
        drop(model);
        assert!(
            thread_graph::read_snapshot(&workspace.display().to_string(), "thread-legacy")
                .unwrap()
                .expect("migrated global graph exists")
                .nodes
                .contains_key("legacy-0")
        );
        assert!(
            thread_graph::read_snapshot(&legacy_root.display().to_string(), "thread-legacy")
                .unwrap()
                .is_some(),
            "legacy graph remains as a recovery copy"
        );
        assert!(state.take_recovery_warning().is_none());

        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn restore_adopts_pre_record_worktree_thread_graph() {
        let workspace = temp_workspace();
        let legacy_root = workspace.join("legacy-worktree");
        let mut agent = sample_agent("legacy");
        agent.worktree_dir = legacy_root.display().to_string();
        agent.thread_id = Some("thread-prerecord".to_string());
        agent.branch_id = Some("branch-prerecord".to_string());
        thread_graph::ThreadStore::new(legacy_root.clone())
            .append_turn_node(&agent, &sample_user_turn("legacy", "legacy turn"))
            .unwrap();

        // Builds that predate thread records persisted agents (with thread
        // ids) and worktree-local graphs but no `threads` map at all, so the
        // record-walking startup migration never sees them.
        let mut persisted = PersistedState::default();
        persisted.agents.push(agent);
        persistence::save(&workspace, &persisted).unwrap();

        let state = AppState::new(test_config(workspace.clone()));
        state.restore_session();

        let workspace_string = workspace.display().to_string();
        let model = state.inner.model.lock().unwrap();
        let record = model.threads.get("thread-prerecord").unwrap();
        assert_eq!(record.storage_root, workspace_string);
        drop(model);
        assert!(
            thread_graph::read_snapshot(&workspace_string, "thread-prerecord")
                .unwrap()
                .expect("adopted graph migrated to global storage")
                .nodes
                .contains_key("legacy-0")
        );

        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn restore_keeps_legacy_record_and_warns_when_migration_fails() {
        let workspace = temp_workspace();
        let legacy_root = workspace.join("corrupt-legacy-worktree");
        let mut agent = sample_agent("legacy-corrupt");
        agent.worktree_dir = legacy_root.display().to_string();
        agent.thread_id = Some("thread-corrupt".to_string());
        agent.branch_id = Some("branch-corrupt".to_string());
        let legacy_path =
            thread_graph::snapshot_path(&legacy_root.display().to_string(), "thread-corrupt");
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        std::fs::write(&legacy_path, b"{").unwrap();

        let mut persisted = PersistedState::default();
        persisted.threads.insert(
            "thread-corrupt".to_string(),
            thread_graph::thread_record_for_agent(&agent, "branch-corrupt", &legacy_root),
        );
        persistence::save(&workspace, &persisted).unwrap();

        let state = AppState::new(test_config(workspace.clone()));
        state.restore_session();

        let model = state.inner.model.lock().unwrap();
        let record = model.threads.get("thread-corrupt").unwrap();
        assert_eq!(record.storage_root, legacy_root.display().to_string());
        drop(model);
        assert!(
            thread_graph::read_snapshot(&workspace.display().to_string(), "thread-corrupt")
                .unwrap()
                .is_none()
        );
        let warning = state.take_recovery_warning().expect("migration warning");
        assert!(warning.contains("could not migrate thread thread-corrupt"));
        assert!(warning.contains("invalid thread graph"));

        std::fs::remove_dir_all(workspace).unwrap();
    }

    fn enqueue_wait_turn(
        state: &AppState,
        agent_id: &str,
        data: &str,
        wait_for_agent_id: &str,
    ) -> Result<usize, String> {
        state.enqueue_agent_wait_turn_with_target_label(
            agent_id,
            data.to_string(),
            wait_for_agent_id,
            None,
            None,
        )
    }

    #[test]
    fn owns_control_socket_tracks_the_bound_inode() {
        use std::os::unix::fs::MetadataExt;

        let workspace = temp_workspace();
        let mut config = test_config(workspace.clone());
        config.socket_path = workspace.join("qmux-test.sock");
        let state = AppState::new(config.clone());

        // Nothing recorded yet: never claim ownership.
        assert!(!state.owns_control_socket());

        // Simulate the bind: create the file at the socket path and record it.
        std::fs::write(&config.socket_path, b"").unwrap();
        let meta = std::fs::symlink_metadata(&config.socket_path).unwrap();
        state.set_control_socket_identity(meta.dev(), meta.ino());
        assert!(state.owns_control_socket());

        // Another instance replaces the socket (created elsewhere then renamed over
        // the path, so its inode is guaranteed to differ from the recorded one):
        // this process no longer owns what lives at the path.
        let replacement = workspace.join("replacement.sock");
        std::fs::write(&replacement, b"").unwrap();
        std::fs::rename(&replacement, &config.socket_path).unwrap();
        assert!(!state.owns_control_socket());

        // A missing path is not ours to reclaim either.
        std::fs::remove_file(&config.socket_path).unwrap();
        assert!(!state.owns_control_socket());
    }

    #[test]
    fn recent_session_round_trips_through_persistence() {
        let workspace = temp_workspace();
        let transcript_path = workspace.join("session-abc.jsonl");
        std::fs::write(
            &transcript_path,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Plan recent session history"}]}}"#,
        )
        .unwrap();
        let config = test_config(workspace.clone());

        {
            let state = AppState::new(config.clone());
            state.restore_session();
            let mut agent = sample_agent("agent-1");
            agent.worktree_dir = workspace.display().to_string();
            agent.transcript_path = Some(transcript_path.display().to_string());
            state.insert_agent(agent).unwrap();
            state
                .replace_turns(
                    "agent-1",
                    vec![sample_user_turn("agent-1", "Plan recent session history")],
                )
                .unwrap();

            let sessions = state.list_recent_sessions(10).unwrap();
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].session_id.as_deref(), Some("session-abc"));
            assert_eq!(
                sessions[0].preview.as_deref(),
                Some("Plan recent session history")
            );
        }

        let state = AppState::new(config);
        state.restore_session();
        let sessions = state.list_recent_sessions(10).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].preview.as_deref(),
            Some("Plan recent session history")
        );
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn closing_agent_pane_keeps_recent_session_without_live_binding() {
        let workspace = temp_workspace();
        let transcript_path = workspace.join("session-abc.jsonl");
        std::fs::write(&transcript_path, "{}\n").unwrap();
        let state = AppState::new(test_config(workspace.clone()));
        state.restore_session();

        let mut agent = sample_agent("agent-1");
        agent.worktree_dir = workspace.display().to_string();
        agent.transcript_path = Some(transcript_path.display().to_string());
        agent.pane_id = Some("pane-1".to_string());
        state.insert_agent(agent).unwrap();
        state
            .replace_turns(
                "agent-1",
                vec![sample_user_turn("agent-1", "Keep me in Home")],
            )
            .unwrap();

        let mut pane = sample_pane_runtime("pane-1");
        pane.info.kind = PaneKind::Agent;
        pane.info.agent_id = Some("agent-1".to_string());
        pane.info.cwd = workspace.display().to_string();
        state.insert_pane(pane).unwrap();

        state.remove_pane("pane-1").unwrap();
        assert!(state.agent("agent-1").unwrap().is_none());

        let sessions = state.list_recent_sessions(10).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].pane_id, None);
        assert_eq!(sessions[0].agent_id, None);
        assert_eq!(sessions[0].preview.as_deref(), Some("Keep me in Home"));
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn research_sessions_are_not_exposed_as_terminal_recents() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace.clone()));
        state.restore_session();
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_agent(sample_agent("research-agent")).unwrap();

        assert!(state.list_recent_sessions(10).unwrap().is_empty());
        assert!(state.inner.model.lock().unwrap().recent_sessions.is_empty());
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn queued_turn_pause_flag_and_pending_pause() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state
            .enqueue_agent_turn("agent-1", "a".to_string())
            .unwrap();
        state
            .enqueue_agent_turn("agent-1", "b".to_string())
            .unwrap();

        let items = state
            .set_queued_turn_pause("agent-1", 1, true, Some("b"), None)
            .unwrap();
        assert!(!items[0].pause_after);
        assert!(items[1].pause_after);
        // The text list is unaffected by the flag.
        assert_eq!(
            state.list_agent_turn_queue("agent-1").unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );

        // A stale expected-text guards against editing the wrong item.
        assert!(
            state
                .set_queued_turn_pause("agent-1", 1, false, Some("wrong"), None)
                .is_err()
        );

        // Pending-pause is a one-shot marker.
        assert!(!state.take_agent_pending_pause("agent-1").unwrap());
        state.mark_agent_pending_pause("agent-1").unwrap();
        assert!(state.take_agent_pending_pause("agent-1").unwrap());
        assert!(!state.take_agent_pending_pause("agent-1").unwrap());
    }

    #[test]
    fn queue_mutations_round_trip_through_persistence() {
        let workspace = temp_workspace();
        let config = test_config(workspace.clone());

        // First process: build up a queue through enqueue/remove with persistence on.
        {
            let state = AppState::new(config.clone());
            assert!(state.restore_session().is_empty());
            state
                .enqueue_agent_turn("agent-1", "first".to_string())
                .unwrap();
            state
                .enqueue_agent_turn("agent-1", "second".to_string())
                .unwrap();
            state
                .enqueue_agent_turn("agent-1", "third".to_string())
                .unwrap();
            // Drop "second" from the middle.
            state
                .remove_agent_turn_queue_item("agent-1", 1, Some("second"), None)
                .unwrap();
        }

        // Second process: the surviving queue order must reload intact.
        let popped = {
            let state = AppState::new(config.clone());
            state.restore_session();
            assert_eq!(
                state.list_agent_turn_queue("agent-1").unwrap(),
                vec!["first".to_string(), "third".to_string()]
            );
            let (data, pending) = state.pop_ready_agent_turn("agent-1").unwrap().unwrap();
            assert_eq!(data.text, "first");
            assert_eq!(pending, 1);
            data.text
        };
        assert_eq!(popped, "first");

        // Third process: the pop must also have been persisted.
        let state = AppState::new(config);
        state.restore_session();
        assert_eq!(
            state.list_agent_turn_queue("agent-1").unwrap(),
            vec!["third".to_string()]
        );
    }

    #[test]
    fn queued_turn_id_guards_the_right_duplicate() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        assert!(state.restore_session().is_empty());
        // Two queued turns with identical text but distinct identities.
        state
            .enqueue_agent_turn("agent-1", "same".to_string())
            .unwrap();
        state
            .enqueue_agent_turn("agent-1", "same".to_string())
            .unwrap();
        let queue = state.agent_queued_turns("agent-1").unwrap();
        assert_eq!(queue.len(), 2);
        assert_ne!(queue[0].id, queue[1].id);

        // Text matches at index 0, but a wrong id is still rejected.
        assert!(
            state
                .remove_agent_turn_queue_item("agent-1", 0, Some("same"), Some("does-not-exist"))
                .is_err()
        );
        // The correct id removes exactly that turn, leaving the other duplicate.
        let second_id = queue[1].id.clone();
        let (removed, remaining) = state
            .remove_agent_turn_queue_item("agent-1", 1, Some("same"), Some(&second_id))
            .unwrap();
        assert_eq!(removed.id, second_id);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, queue[0].id);
    }

    #[test]
    fn queued_turns_persisted_without_ids_are_migrated_on_load() {
        // A turn stored by an older build (no `id` field) still loads, gaining a
        // fresh id, so mutations can identify it afterward.
        let turn: QueuedTurn =
            serde_json::from_str(r#"{"text":"legacy","pauseAfter":true}"#).unwrap();
        assert_eq!(turn.text, "legacy");
        assert!(turn.pause_after);
        assert!(turn.id.starts_with("qturn-"));
        // The legacy bare-string form is migrated too.
        let bare: QueuedTurn = serde_json::from_str(r#""just text""#).unwrap();
        assert_eq!(bare.text, "just text");
        assert!(bare.id.starts_with("qturn-"));
    }

    #[test]
    fn queued_wait_turn_waits_until_target_is_done() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));

        let mut source = sample_agent("source");
        source.status = AgentStatus::Done;
        source.pane_id = Some("source-pane".to_string());
        let mut target = sample_agent("target");
        target.status = AgentStatus::Running;
        target.pane_id = Some("target-pane".to_string());
        let mut target_pane = sample_pane_runtime("target-pane");
        target_pane.info.agent_id = Some("target".to_string());
        state.insert_agent(source).unwrap();
        state.insert_agent(target).unwrap();
        state.insert_pane(target_pane).unwrap();

        enqueue_wait_turn(&state, "source", "after target", "target").unwrap();
        assert!(state.pop_ready_agent_turn("source").unwrap().is_none());

        state
            .set_agent_status("target", AgentStatus::AwaitingInput)
            .unwrap();
        assert!(state.pop_ready_agent_turn("source").unwrap().is_none());

        state
            .set_agent_status("target", AgentStatus::AwaitingPermission)
            .unwrap();
        assert!(state.pop_ready_agent_turn("source").unwrap().is_none());

        state.set_agent_status("target", AgentStatus::Done).unwrap();
        let (turn, pending) = state.pop_ready_agent_turn("source").unwrap().unwrap();
        assert_eq!(turn.text, "after target");
        assert_eq!(pending, 0);
    }

    #[test]
    fn queued_wait_turn_blocks_later_turns_until_target_is_done() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));

        let mut source = sample_agent("source");
        source.status = AgentStatus::Done;
        source.pane_id = Some("source-pane".to_string());
        let mut target = sample_agent("target");
        target.status = AgentStatus::Running;
        target.pane_id = Some("target-pane".to_string());
        let mut target_pane = sample_pane_runtime("target-pane");
        target_pane.info.agent_id = Some("target".to_string());
        state.insert_agent(source).unwrap();
        state.insert_agent(target).unwrap();
        state.insert_pane(target_pane).unwrap();

        enqueue_wait_turn(&state, "source", "after target", "target").unwrap();
        state
            .enqueue_agent_turn("source", "then this".to_string())
            .unwrap();

        assert!(state.pop_ready_agent_turn("source").unwrap().is_none());

        state.set_agent_status("target", AgentStatus::Done).unwrap();
        let (first, first_pending) = state.pop_ready_agent_turn("source").unwrap().unwrap();
        assert_eq!(first.text, "after target");
        assert_eq!(first_pending, 1);

        let (second, second_pending) = state.pop_ready_agent_turn("source").unwrap().unwrap();
        assert_eq!(second.text, "then this");
        assert_eq!(second_pending, 0);
    }

    #[test]
    fn removing_front_wait_turn_moves_wait_to_next_turn() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));

        let mut source = sample_agent("source");
        source.status = AgentStatus::Done;
        source.pane_id = Some("source-pane".to_string());
        let mut target = sample_agent("target");
        target.status = AgentStatus::Running;
        target.pane_id = Some("target-pane".to_string());
        let mut target_pane = sample_pane_runtime("target-pane");
        target_pane.info.agent_id = Some("target".to_string());
        state.insert_agent(source).unwrap();
        state.insert_agent(target).unwrap();
        state.insert_pane(target_pane).unwrap();

        enqueue_wait_turn(&state, "source", "remove me", "target").unwrap();
        state
            .enqueue_agent_turn("source", "keep waiting".to_string())
            .unwrap();

        let (removed, queued) = state
            .remove_agent_turn_queue_item("source", 0, Some("remove me"), None)
            .unwrap();
        assert_eq!(removed.text, "remove me");
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].text, "keep waiting");
        let propagated_wait = queued[0].wait_for.as_ref().unwrap();
        assert_eq!(propagated_wait.agent_id, "target");

        assert!(state.pop_ready_agent_turn("source").unwrap().is_none());

        state.set_agent_status("target", AgentStatus::Done).unwrap();
        let (turn, pending) = state.pop_ready_agent_turn("source").unwrap().unwrap();
        assert_eq!(turn.text, "keep waiting");
        assert_eq!(pending, 0);
    }

    #[test]
    fn queued_wait_turn_waits_for_target_queue_after_target_is_done() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));

        let mut source = sample_agent("source");
        source.status = AgentStatus::Done;
        source.pane_id = Some("source-pane".to_string());
        let mut target = sample_agent("target");
        target.status = AgentStatus::Done;
        target.pane_id = Some("target-pane".to_string());
        let mut target_pane = sample_pane_runtime("target-pane");
        target_pane.info.agent_id = Some("target".to_string());
        state.insert_agent(source).unwrap();
        state.insert_agent(target).unwrap();
        state.insert_pane(target_pane).unwrap();

        state
            .enqueue_agent_turn("target", "target queued".to_string())
            .unwrap();
        enqueue_wait_turn(&state, "source", "after target", "target").unwrap();

        assert!(state.pop_ready_agent_turn("source").unwrap().is_none());

        let (target_turn, target_pending) = state.pop_ready_agent_turn("target").unwrap().unwrap();
        assert_eq!(target_turn.text, "target queued");
        assert_eq!(target_pending, 0);

        let (source_turn, source_pending) = state.pop_ready_agent_turn("source").unwrap().unwrap();
        assert_eq!(source_turn.text, "after target");
        assert_eq!(source_pending, 0);
    }

    #[test]
    fn queued_wait_turn_uses_supplied_label_when_target_pane_matches() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));

        let mut source = sample_agent("source");
        source.status = AgentStatus::Done;
        let mut target = sample_agent("target");
        target.status = AgentStatus::Running;
        target.pane_id = Some("target-pane".to_string());
        let mut target_pane = sample_pane_runtime("target-pane");
        target_pane.info.title = "Shell".to_string();
        target_pane.info.agent_id = Some("target".to_string());
        state.insert_agent(source).unwrap();
        state.insert_agent(target).unwrap();
        state.insert_pane(target_pane).unwrap();

        state
            .enqueue_agent_wait_turn_with_target_label(
                "source",
                "after target".to_string(),
                "target",
                Some("target-pane"),
                Some("Dynamic terminal title"),
            )
            .unwrap();

        let queued = state.agent_queued_turns("source").unwrap();
        let wait_for = queued[0].wait_for.as_ref().unwrap();
        assert_eq!(wait_for.label.as_deref(), Some("Dynamic terminal title"));
    }

    #[test]
    fn queued_wait_turn_ignores_supplied_label_when_target_pane_is_stale() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));

        let mut source = sample_agent("source");
        source.status = AgentStatus::Done;
        let mut target = sample_agent("target");
        target.status = AgentStatus::Running;
        target.pane_id = Some("target-pane".to_string());
        let mut target_pane = sample_pane_runtime("target-pane");
        target_pane.info.title = "Backend title".to_string();
        target_pane.info.agent_id = Some("target".to_string());
        state.insert_agent(source).unwrap();
        state.insert_agent(target).unwrap();
        state.insert_pane(target_pane).unwrap();

        state
            .enqueue_agent_wait_turn_with_target_label(
                "source",
                "after target".to_string(),
                "target",
                Some("stale-pane"),
                Some("Dynamic terminal title"),
            )
            .unwrap();

        let queued = state.agent_queued_turns("source").unwrap();
        let wait_for = queued[0].wait_for.as_ref().unwrap();
        assert_eq!(wait_for.label.as_deref(), Some("Backend title"));
    }

    #[test]
    fn queued_wait_turn_resolves_when_target_pane_is_gone() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));

        let mut source = sample_agent("source");
        source.status = AgentStatus::Done;
        let mut target = sample_agent("target");
        target.status = AgentStatus::Running;
        target.pane_id = Some("missing-pane".to_string());
        state.insert_agent(source).unwrap();
        state.insert_agent(target).unwrap();

        enqueue_wait_turn(&state, "source", "after close", "target").unwrap();
        let (turn, pending) = state.pop_ready_agent_turn("source").unwrap().unwrap();
        assert_eq!(turn.text, "after close");
        assert_eq!(pending, 0);
    }

    #[test]
    fn queued_wait_turn_blocks_when_target_failed() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));

        let mut source = sample_agent("source");
        source.status = AgentStatus::Done;
        source.pane_id = Some("source-pane".to_string());
        let mut target = sample_agent("target");
        target.status = AgentStatus::Failed;
        target.pane_id = Some("target-pane".to_string());
        let mut target_pane = sample_pane_runtime("target-pane");
        target_pane.info.agent_id = Some("target".to_string());
        state.insert_agent(source).unwrap();
        state.insert_agent(target).unwrap();
        state.insert_pane(target_pane).unwrap();

        enqueue_wait_turn(&state, "source", "after target", "target").unwrap();

        // A failed target intentionally keeps its waiters blocked.
        assert!(state.pop_ready_agent_turn("source").unwrap().is_none());
    }

    #[test]
    fn claim_ready_agent_turn_serializes_concurrent_drains() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state.insert_agent(sample_agent("agent-1")).unwrap();
        state
            .enqueue_agent_turn("agent-1", "first".to_string())
            .unwrap();
        state
            .enqueue_agent_turn("agent-1", "second".to_string())
            .unwrap();

        // First claim pops the front turn and marks the agent draining.
        match state.claim_ready_agent_turn("agent-1").unwrap() {
            AgentTurnClaim::Ready { turn, .. } => assert_eq!(turn.text, "first"),
            _ => panic!("expected the first turn to be claimed"),
        }
        // A concurrent claim is refused while the first drain is in flight, even though
        // "second" is itself ready — this is what prevents the double-send.
        assert!(matches!(
            state.claim_ready_agent_turn("agent-1").unwrap(),
            AgentTurnClaim::Draining
        ));
        // Finishing the first drain lets the next one proceed.
        state.finish_agent_drain("agent-1");
        match state.claim_ready_agent_turn("agent-1").unwrap() {
            AgentTurnClaim::Ready { turn, .. } => assert_eq!(turn.text, "second"),
            _ => panic!("expected the second turn to be claimed"),
        }
    }

    #[test]
    fn begin_direct_send_is_refused_while_a_drain_owns_the_agent() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace.clone()));
        state.insert_agent(sample_agent("agent-1")).unwrap();

        // With nothing draining, a direct send reserves the guard.
        assert!(state.begin_direct_send("agent-1").unwrap());
        // A second direct send is refused while the first still owns the agent...
        assert!(!state.begin_direct_send("agent-1").unwrap());
        // ...and a queue drain is refused too, so neither can write a second turn into
        // the pane concurrently.
        state
            .enqueue_agent_turn("agent-1", "queued".to_string())
            .unwrap();
        assert!(matches!(
            state.claim_ready_agent_turn("agent-1").unwrap(),
            AgentTurnClaim::Draining
        ));
        // Releasing the guard lets the drain proceed.
        state.finish_agent_drain("agent-1");
        assert!(matches!(
            state.claim_ready_agent_turn("agent-1").unwrap(),
            AgentTurnClaim::Ready { .. }
        ));
        std::fs::remove_dir_all(workspace).ok();
    }

    #[test]
    fn in_flight_turn_is_recovered_to_the_front_of_the_queue_on_restart() {
        let workspace = temp_workspace();
        // First run: enqueue two turns, claim the front (an in-flight send that never
        // confirms), then "crash" by dropping without delivering or clearing it.
        {
            let state = AppState::new(test_config(workspace.clone()));
            state.restore_session();
            state.insert_agent(sample_agent("agent-1")).unwrap();
            state
                .enqueue_agent_turn("agent-1", "first".to_string())
                .unwrap();
            state
                .enqueue_agent_turn("agent-1", "second".to_string())
                .unwrap();
            match state.claim_ready_agent_turn("agent-1").unwrap() {
                AgentTurnClaim::Ready { turn, .. } => assert_eq!(turn.text, "first"),
                _ => panic!("expected the first turn to be claimed"),
            }
        }
        // Second run: the in-flight "first" is re-queued ahead of "second" rather than
        // lost, so it will be re-delivered.
        {
            let state = AppState::new(test_config(workspace.clone()));
            state.restore_session();
            assert_eq!(
                state.list_agent_turn_queue("agent-1").unwrap(),
                vec!["first".to_string(), "second".to_string()]
            );
        }
        std::fs::remove_dir_all(workspace).ok();
    }

    #[test]
    fn claim_next_turn_or_settle_holds_for_typing_then_drains() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state.insert_agent(sample_agent("agent-1")).unwrap();
        state
            .enqueue_agent_turn("agent-1", "queued".to_string())
            .unwrap();
        state.set_agent_typing("agent-1", true).unwrap();

        // While the user is typing the idle advance settles to Done and holds the queue,
        // setting the status atomically with reading the typing flag.
        assert!(matches!(
            state
                .claim_next_turn_or_settle("agent-1", AgentStatus::Done)
                .unwrap(),
            IdleAdvance::Idle
        ));
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Done
        ));
        assert_eq!(
            state.list_agent_turn_queue("agent-1").unwrap(),
            vec!["queued".to_string()]
        );

        // Once typing clears, the next advance claims the held turn instead of stalling.
        state.set_agent_typing("agent-1", false).unwrap();
        match state
            .claim_next_turn_or_settle("agent-1", AgentStatus::Done)
            .unwrap()
        {
            IdleAdvance::Sent { turn, .. } => assert_eq!(turn.text, "queued"),
            _ => panic!("expected the held turn to drain once typing cleared"),
        }
    }

    #[test]
    fn queued_wait_turn_rejects_dependency_cycles() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));

        let mut agent_a = sample_agent("agent-a");
        agent_a.pane_id = Some("pane-a".to_string());
        let mut agent_b = sample_agent("agent-b");
        agent_b.pane_id = Some("pane-b".to_string());
        let mut pane_a = sample_pane_runtime("pane-a");
        pane_a.info.agent_id = Some("agent-a".to_string());
        let mut pane_b = sample_pane_runtime("pane-b");
        pane_b.info.agent_id = Some("agent-b".to_string());
        state.insert_agent(agent_a).unwrap();
        state.insert_agent(agent_b).unwrap();
        state.insert_pane(pane_a).unwrap();
        state.insert_pane(pane_b).unwrap();

        enqueue_wait_turn(&state, "agent-a", "wait a", "agent-b").unwrap();
        let err = enqueue_wait_turn(&state, "agent-b", "wait b", "agent-a").unwrap_err();
        assert!(err.contains("cycle"));
    }

    #[test]
    fn queued_wait_turn_rejects_cycle_through_idle_target_queue() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));

        let mut agent_a = sample_agent("agent-a");
        agent_a.status = AgentStatus::Done;
        agent_a.pane_id = Some("pane-a".to_string());
        let mut agent_b = sample_agent("agent-b");
        agent_b.status = AgentStatus::Done;
        agent_b.pane_id = Some("pane-b".to_string());
        let mut pane_a = sample_pane_runtime("pane-a");
        pane_a.info.agent_id = Some("agent-a".to_string());
        let mut pane_b = sample_pane_runtime("pane-b");
        pane_b.info.agent_id = Some("agent-b".to_string());
        state.insert_agent(agent_a).unwrap();
        state.insert_agent(agent_b).unwrap();
        state.insert_pane(pane_a).unwrap();
        state.insert_pane(pane_b).unwrap();

        enqueue_wait_turn(&state, "agent-b", "wait for a", "agent-a").unwrap();
        let err = enqueue_wait_turn(&state, "agent-a", "wait for b", "agent-b").unwrap_err();
        assert!(err.contains("cycle"));
    }

    #[test]
    fn queued_wait_turn_round_trips_through_persistence() {
        let workspace = temp_workspace();
        let config = test_config(workspace.clone());

        {
            let state = AppState::new(config.clone());
            assert!(state.restore_session().is_empty());
            let mut source = sample_agent("source");
            source.status = AgentStatus::Done;
            let mut target = sample_agent("target");
            target.status = AgentStatus::Running;
            target.pane_id = Some("target-pane".to_string());
            let mut target_pane = sample_pane_runtime("target-pane");
            target_pane.info.title = "Target pane".to_string();
            target_pane.info.agent_id = Some("target".to_string());
            state.insert_agent(source).unwrap();
            state.insert_agent(target).unwrap();
            state.insert_pane(target_pane).unwrap();
            enqueue_wait_turn(&state, "source", "persisted wait", "target").unwrap();
        }

        let state = AppState::new(config);
        state.restore_session();
        let queued = state.agent_queued_turns("source").unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].text, "persisted wait");
        let wait_for = queued[0].wait_for.as_ref().unwrap();
        assert_eq!(wait_for.agent_id, "target");
        assert_eq!(wait_for.label.as_deref(), Some("Target pane"));
    }

    #[test]
    fn panes_list_in_inserted_order() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));

        state.insert_pane(sample_pane_runtime("pane-b")).unwrap();
        state.insert_pane(sample_pane_runtime("pane-a")).unwrap();
        state.insert_pane(sample_pane_runtime("pane-c")).unwrap();

        assert_eq!(
            state
                .list_panes()
                .unwrap()
                .into_iter()
                .map(|pane| pane.id)
                .collect::<Vec<_>>(),
            vec![
                "pane-b".to_string(),
                "pane-a".to_string(),
                "pane-c".to_string()
            ]
        );
    }

    #[test]
    fn pane_splits_require_adjacent_tabs_and_prune_on_layout_change() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));

        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
        state.insert_pane(sample_pane_runtime("pane-2")).unwrap();
        state.insert_pane(sample_pane_runtime("pane-3")).unwrap();

        let invalid = state
            .set_pane_splits(vec![PaneSplitInfo {
                id: "split-a".to_string(),
                pane_ids: vec!["pane-1".to_string(), "pane-3".to_string()],
                sizes: HashMap::new(),
                intent: HashMap::new(),
            }])
            .unwrap_err();
        assert!(invalid.contains("adjacent"));

        let splits = state
            .set_pane_splits(vec![PaneSplitInfo {
                id: "split-a".to_string(),
                pane_ids: vec!["pane-1".to_string(), "pane-2".to_string()],
                sizes: HashMap::from([("pane-1".to_string(), 0.4), ("pane-2".to_string(), 0.6)]),
                intent: HashMap::new(),
            }])
            .unwrap();
        assert_eq!(splits.len(), 1);
        assert_eq!(splits[0].pane_ids, vec!["pane-1", "pane-2"]);
        assert_eq!(splits[0].sizes.get("pane-1"), Some(&0.4));

        state
            .set_pane_layout(layout(&[("pane-1", 0), ("pane-3", 0), ("pane-2", 0)]))
            .unwrap();

        assert!(state.pane_splits().unwrap().is_empty());
    }

    #[test]
    fn pane_splits_preserve_valid_intent_and_prune_stale_intent() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));

        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
        state.insert_pane(sample_pane_runtime("pane-2")).unwrap();
        state.insert_pane(sample_pane_runtime("pane-3")).unwrap();

        let splits = state
            .set_pane_splits(vec![PaneSplitInfo {
                id: "split-a".to_string(),
                pane_ids: vec![
                    "pane-1".to_string(),
                    "pane-2".to_string(),
                    "pane-3".to_string(),
                ],
                sizes: HashMap::new(),
                intent: HashMap::from([
                    (
                        "pane-2".to_string(),
                        PaneSplitIntent {
                            kind: "inserted-relative".to_string(),
                            anchor_pane_id: "pane-1".to_string(),
                            position: "below".to_string(),
                            source: "command".to_string(),
                            created_at: 1.0,
                        },
                    ),
                    (
                        "pane-3".to_string(),
                        PaneSplitIntent {
                            kind: "inserted-relative".to_string(),
                            anchor_pane_id: "pane-missing".to_string(),
                            position: "below".to_string(),
                            source: "drag-half".to_string(),
                            created_at: 2.0,
                        },
                    ),
                ]),
            }])
            .unwrap();

        assert_eq!(splits.len(), 1);
        assert_eq!(
            splits[0].intent.get("pane-2"),
            Some(&PaneSplitIntent {
                kind: "inserted-relative".to_string(),
                anchor_pane_id: "pane-1".to_string(),
                position: "below".to_string(),
                source: "command".to_string(),
                created_at: 1.0,
            })
        );
        assert!(!splits[0].intent.contains_key("pane-3"));
    }

    #[test]
    fn update_pane_cwd_rejects_untrusted_values() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();

        // A normal path (an existing absolute directory) is accepted and stored.
        let real_dir = std::env::temp_dir().display().to_string();
        state.update_pane_cwd("pane-1", real_dir.clone()).unwrap();
        assert_eq!(state.list_panes().unwrap()[0].cwd, real_dir);

        // Control characters (here a newline) are rejected and leave the stored
        // value untouched.
        assert!(
            state
                .update_pane_cwd("pane-1", "/tmp/evil\nmalicious".to_string())
                .is_err()
        );
        // An oversized value is rejected too.
        assert!(
            state
                .update_pane_cwd("pane-1", "/".repeat(MAX_PANE_CWD_LEN + 1))
                .is_err()
        );
        // A non-existent path and a relative path are rejected (an installed
        // file-server root must be a real, absolute directory).
        assert!(
            state
                .update_pane_cwd("pane-1", "/no/such/qmux/dir/at/all".to_string())
                .is_err()
        );
        assert!(
            state
                .update_pane_cwd("pane-1", "relative/dir".to_string())
                .is_err()
        );
        assert_eq!(state.list_panes().unwrap()[0].cwd, real_dir);
    }

    #[test]
    fn group_spawn_cwd_prefers_most_recent_shell_pane() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));

        let base = std::env::temp_dir().join(format!("qmux-gsc-{}", std::process::id()));
        let dir_old = base.join("old");
        let dir_new = base.join("new");
        let dir_agent = base.join("agent");
        for dir in [&dir_old, &dir_new, &dir_agent] {
            std::fs::create_dir_all(dir).unwrap();
        }

        let mut older = sample_pane_runtime("pane-old");
        older.info.group_id = "group-1".to_string();
        older.info.cwd = dir_old.display().to_string();
        older.info.last_active_at = 100;
        state.insert_pane(older).unwrap();

        let mut newer = sample_pane_runtime("pane-new");
        newer.info.group_id = "group-1".to_string();
        newer.info.cwd = dir_new.display().to_string();
        newer.info.last_active_at = 200;
        state.insert_pane(newer).unwrap();

        // A more-recently-active agent pane is ignored: it is worktree-rooted, not a
        // shell, so it must never steer a new spawn's cwd.
        let mut agent = sample_pane_runtime("pane-agent");
        agent.info.group_id = "group-1".to_string();
        agent.info.kind = PaneKind::Agent;
        agent.info.agent_id = Some("agent-1".to_string());
        agent.info.cwd = dir_agent.display().to_string();
        agent.info.last_active_at = 300;
        state.insert_pane(agent).unwrap();

        // The most-recently-active shell pane wins.
        assert_eq!(state.group_spawn_cwd("group-1"), Some(dir_new));

        // touch_pane_active re-stamps the older pane as most recent → it now wins.
        state.touch_pane_active("pane-old");
        assert_eq!(state.group_spawn_cwd("group-1"), Some(dir_old));

        // A group with no shell panes (or no panes at all) yields None.
        assert_eq!(state.group_spawn_cwd("group-empty"), None);
    }

    #[test]
    fn exit_confirmation_counts_only_live_panes() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));

        let mut starting = sample_pane_runtime("pane-starting");
        starting.info.status = PaneStatus::Starting;
        state.insert_pane(starting).unwrap();
        assert!(state.should_confirm_exit());

        state
            .mark_pane_status("pane-starting", PaneStatus::Exited)
            .unwrap();
        assert!(!state.should_confirm_exit());

        state
            .insert_pane(sample_pane_runtime("pane-running"))
            .unwrap();
        assert!(state.should_confirm_exit());

        state
            .mark_pane_status("pane-running", PaneStatus::Killed)
            .unwrap();
        assert!(!state.should_confirm_exit());

        state
            .insert_pane(sample_pane_runtime("pane-failed"))
            .unwrap();
        state
            .mark_pane_status("pane-failed", PaneStatus::Failed)
            .unwrap();
        assert!(!state.should_confirm_exit());
    }

    #[test]
    fn pane_reorder_round_trips_through_persistence() {
        let workspace = temp_workspace();
        let config = test_config(workspace.clone());

        {
            let state = AppState::new(config.clone());
            assert!(state.restore_session().is_empty());
            state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
            state.insert_pane(sample_pane_runtime("pane-2")).unwrap();
            state.insert_pane(sample_pane_runtime("pane-3")).unwrap();

            let reordered = state
                .reorder_panes(vec![
                    "pane-3".to_string(),
                    "pane-1".to_string(),
                    "pane-2".to_string(),
                ])
                .unwrap();
            assert_eq!(
                reordered
                    .into_iter()
                    .map(|pane| pane.id)
                    .collect::<Vec<_>>(),
                vec![
                    "pane-3".to_string(),
                    "pane-1".to_string(),
                    "pane-2".to_string()
                ]
            );
        }

        let state = AppState::new(config);
        let recovered = state.restore_session();
        assert_eq!(
            recovered
                .into_iter()
                .map(|pane| pane.id)
                .collect::<Vec<_>>(),
            vec![
                "pane-3".to_string(),
                "pane-1".to_string(),
                "pane-2".to_string()
            ]
        );
    }

    #[test]
    fn pane_reorder_rejects_stale_or_duplicate_orders() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
        state.insert_pane(sample_pane_runtime("pane-2")).unwrap();

        let duplicate = state
            .reorder_panes(vec!["pane-1".to_string(), "pane-1".to_string()])
            .unwrap_err();
        assert!(duplicate.contains("duplicate"));

        let stale = state.reorder_panes(vec!["pane-1".to_string()]).unwrap_err();
        assert!(stale.contains("stale"));
    }

    #[test]
    fn group_reorder_round_trips_through_persistence_and_rejects_stale_orders() {
        let workspace = temp_workspace();
        let config = test_config(workspace.clone());

        {
            let state = AppState::new(config.clone());
            assert!(state.restore_session().is_empty());
            state
                .insert_group_after(sample_group_with_id("group-1"), None)
                .unwrap();
            state
                .insert_group_after(sample_group_with_id("group-2"), Some("group-1"))
                .unwrap();
            state
                .insert_group_after(sample_group_with_id("group-3"), Some("group-2"))
                .unwrap();

            let reordered = state
                .reorder_groups(vec![
                    "group-3".to_string(),
                    "group-1".to_string(),
                    "group-2".to_string(),
                ])
                .unwrap();
            assert_eq!(
                reordered
                    .into_iter()
                    .map(|group| group.id)
                    .collect::<Vec<_>>(),
                vec![
                    "group-3".to_string(),
                    "group-1".to_string(),
                    "group-2".to_string()
                ]
            );

            let duplicate = state
                .reorder_groups(vec![
                    "group-3".to_string(),
                    "group-3".to_string(),
                    "group-2".to_string(),
                ])
                .unwrap_err();
            assert!(duplicate.contains("duplicate"));

            let stale = state
                .reorder_groups(vec!["group-3".to_string()])
                .unwrap_err();
            assert!(stale.contains("stale"));
        }

        let state = AppState::new(config);
        assert!(state.restore_session().is_empty());
        assert_eq!(
            state
                .list_groups()
                .unwrap()
                .into_iter()
                .map(|group| group.id)
                .collect::<Vec<_>>(),
            vec![
                "group-3".to_string(),
                "group-1".to_string(),
                "group-2".to_string()
            ]
        );
    }

    fn layout(items: &[(&str, u16)]) -> Vec<PaneLayoutEntry> {
        items
            .iter()
            .map(|(id, depth)| PaneLayoutEntry {
                pane_id: id.to_string(),
                depth: *depth,
            })
            .collect()
    }

    fn id_depths(panes: &[PaneInfo]) -> Vec<(String, u16)> {
        panes
            .iter()
            .map(|pane| (pane.id.clone(), pane.depth))
            .collect()
    }

    #[test]
    fn set_pane_layout_applies_and_round_trips_depth() {
        let workspace = temp_workspace();
        let config = test_config(workspace.clone());

        {
            let state = AppState::new(config.clone());
            assert!(state.restore_session().is_empty());
            state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
            state.insert_pane(sample_pane_runtime("pane-2")).unwrap();
            state.insert_pane(sample_pane_runtime("pane-3")).unwrap();

            let panes = state
                .set_pane_layout(layout(&[("pane-3", 0), ("pane-1", 1), ("pane-2", 2)]))
                .unwrap();
            assert_eq!(
                id_depths(&panes),
                vec![
                    ("pane-3".to_string(), 0),
                    ("pane-1".to_string(), 1),
                    ("pane-2".to_string(), 2),
                ]
            );
        }

        // Depth and order survive a restart via the persisted pane list.
        let state = AppState::new(config);
        let recovered = state.restore_session();
        assert_eq!(
            id_depths(&recovered),
            vec![
                ("pane-3".to_string(), 0),
                ("pane-1".to_string(), 1),
                ("pane-2".to_string(), 2),
            ]
        );
    }

    #[test]
    fn set_pane_layout_rejects_invalid_layouts() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
        state.insert_pane(sample_pane_runtime("pane-2")).unwrap();

        // The first pane must be at the root.
        assert!(
            state
                .set_pane_layout(layout(&[("pane-1", 1), ("pane-2", 1)]))
                .unwrap_err()
                .contains("valid tree")
        );
        // A depth may not skip a level.
        assert!(
            state
                .set_pane_layout(layout(&[("pane-1", 0), ("pane-2", 2)]))
                .unwrap_err()
                .contains("valid tree")
        );
        // Depth is capped.
        assert!(
            state
                .set_pane_layout(layout(&[("pane-1", 0), ("pane-2", MAX_PANE_DEPTH + 1)]))
                .unwrap_err()
                .contains("maximum")
        );
        // Membership must match the live panes exactly.
        assert!(
            state
                .set_pane_layout(layout(&[("pane-1", 0), ("pane-1", 0)]))
                .unwrap_err()
                .contains("duplicate")
        );
        assert!(
            state
                .set_pane_layout(layout(&[("pane-1", 0)]))
                .unwrap_err()
                .contains("stale")
        );
    }

    #[test]
    fn move_pane_to_group_moves_shell_subtree_and_removes_emptied_group() {
        let workspace = temp_workspace();
        let config = test_config(workspace.clone());

        {
            let state = AppState::new(config.clone());
            assert!(state.restore_session().is_empty());
            state
                .insert_group_after(sample_group_with_id("group-1"), None)
                .unwrap();
            state
                .insert_group_after(sample_group_with_id("group-2"), Some("group-1"))
                .unwrap();
            state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
            state.insert_pane(sample_pane_runtime("pane-2")).unwrap();
            let mut other = sample_pane_runtime("pane-3");
            other.info.group_id = "group-2".to_string();
            state.insert_pane(other).unwrap();
            state
                .set_pane_layout(layout(&[("pane-1", 0), ("pane-2", 1), ("pane-3", 0)]))
                .unwrap();

            // The whole subtree re-homes to the target group with the given layout.
            let panes = state
                .move_pane_to_group(
                    "pane-1",
                    "group-2",
                    layout(&[("pane-3", 0), ("pane-1", 0), ("pane-2", 1)]),
                )
                .unwrap();
            assert_eq!(
                id_depths(&panes),
                vec![
                    ("pane-3".to_string(), 0),
                    ("pane-1".to_string(), 0),
                    ("pane-2".to_string(), 1),
                ]
            );
            assert!(panes.iter().all(|pane| pane.group_id == "group-2"));

            // The move emptied group-1, so it's removed like closing its last pane.
            let groups = state.list_groups().unwrap();
            assert_eq!(
                groups
                    .iter()
                    .map(|group| group.id.clone())
                    .collect::<Vec<_>>(),
                vec!["group-2".to_string()]
            );
        }

        // The new group membership, order, and group removal all survive a restart.
        let state = AppState::new(config);
        let recovered = state.restore_session();
        assert_eq!(
            id_depths(&recovered),
            vec![
                ("pane-3".to_string(), 0),
                ("pane-1".to_string(), 0),
                ("pane-2".to_string(), 1),
            ]
        );
        assert!(recovered.iter().all(|pane| pane.group_id == "group-2"));
        assert_eq!(state.list_groups().unwrap().len(), 1);
    }

    #[test]
    fn move_pane_to_group_rejects_agent_tabs_and_non_terminal_targets() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state
            .insert_group_after(sample_group_with_id("group-1"), None)
            .unwrap();
        state
            .insert_group_after(sample_group_with_id("group-2"), Some("group-1"))
            .unwrap();
        let mut research = sample_group_with_id("group-research");
        research.scope = WorkspaceScope::Research;
        state.insert_group_after(research, Some("group-2")).unwrap();

        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
        let mut agent = sample_pane_runtime("pane-agent");
        agent.info.kind = PaneKind::Agent;
        agent.info.agent_id = Some("agent-1".to_string());
        state.insert_pane(agent).unwrap();
        state.insert_pane(sample_pane_runtime("pane-2")).unwrap();
        state
            .set_pane_layout(layout(&[("pane-1", 0), ("pane-agent", 1), ("pane-2", 0)]))
            .unwrap();
        let full = || layout(&[("pane-1", 0), ("pane-agent", 1), ("pane-2", 0)]);

        // An agent tab can't move, and neither can a shell tab whose subtree holds one.
        assert!(
            state
                .move_pane_to_group("pane-agent", "group-2", full())
                .unwrap_err()
                .contains("agent tabs")
        );
        assert!(
            state
                .move_pane_to_group("pane-1", "group-2", full())
                .unwrap_err()
                .contains("agent tabs")
        );

        // Only terminal-to-terminal moves are valid, and both groups must exist.
        assert!(
            state
                .move_pane_to_group("pane-2", "group-research", full())
                .unwrap_err()
                .contains("terminal groups")
        );
        assert!(
            state
                .move_pane_to_group("pane-2", "group-1", full())
                .unwrap_err()
                .contains("already in")
        );
        assert!(
            state
                .move_pane_to_group("pane-2", "group-missing", full())
                .unwrap_err()
                .contains("not found")
        );

        // A plain shell tab does move, and the source group survives while its
        // other panes remain.
        let panes = state
            .move_pane_to_group("pane-2", "group-2", full())
            .unwrap();
        let moved = panes.iter().find(|pane| pane.id == "pane-2").unwrap();
        assert_eq!(moved.group_id, "group-2");
        assert_eq!(state.list_groups().unwrap().len(), 3);
    }

    #[test]
    fn remove_pane_relevels_orphaned_children() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
        state.insert_pane(sample_pane_runtime("pane-2")).unwrap();
        state.insert_pane(sample_pane_runtime("pane-3")).unwrap();
        state
            .set_pane_layout(layout(&[("pane-1", 0), ("pane-2", 1), ("pane-3", 2)]))
            .unwrap();

        // Closing the root parent promotes its subtree so the tree stays valid.
        state.remove_pane("pane-1").unwrap();
        assert_eq!(
            id_depths(&state.list_panes().unwrap()),
            vec![("pane-2".to_string(), 0), ("pane-3".to_string(), 1)]
        );
    }

    #[test]
    fn closed_pane_undo_stack_pops_most_recent_first_and_survives_extra_closes() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
        state.insert_pane(sample_pane_runtime("pane-2")).unwrap();
        state.insert_pane(sample_pane_runtime("pane-3")).unwrap();

        // Three successive closes stack up (unlike the old single slot, the earlier ones
        // aren't discarded by the next close).
        state.capture_last_closed_pane("pane-1").unwrap();
        state.capture_last_closed_pane("pane-2").unwrap();
        state.capture_last_closed_pane("pane-3").unwrap();

        // Undo reopens them most-recent first.
        assert_eq!(
            state.take_last_closed_pane().unwrap().unwrap().pane.id,
            "pane-3"
        );
        assert_eq!(
            state.take_last_closed_pane().unwrap().unwrap().pane.id,
            "pane-2"
        );
        assert_eq!(
            state.take_last_closed_pane().unwrap().unwrap().pane.id,
            "pane-1"
        );
        assert!(state.take_last_closed_pane().unwrap().is_none());
    }

    #[test]
    fn closed_pane_undo_stack_is_bounded() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        // Capture more closes than the cap; the oldest are dropped and only the most
        // recent MAX_CLOSED_PANE_UNDO remain reopenable.
        for index in 0..(MAX_CLOSED_PANE_UNDO + 5) {
            let pane_id = format!("pane-{index}");
            state.insert_pane(sample_pane_runtime(&pane_id)).unwrap();
            state.capture_last_closed_pane(&pane_id).unwrap();
        }
        let mut popped = 0;
        let mut newest_first = Vec::new();
        while let Some(snapshot) = state.take_last_closed_pane().unwrap() {
            newest_first.push(snapshot.pane.id);
            popped += 1;
        }
        assert_eq!(popped, MAX_CLOSED_PANE_UNDO);
        // The newest close is still first out; the oldest five were evicted.
        assert_eq!(
            newest_first.first().map(String::as_str),
            Some(format!("pane-{}", MAX_CLOSED_PANE_UNDO + 4).as_str())
        );
        assert!(!newest_first.contains(&"pane-0".to_string()));
    }

    #[test]
    fn capture_last_closed_pane_records_layout_agent_state_and_scrollback() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace.clone()));
        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
        let mut pane_2 = sample_pane_runtime("pane-2");
        pane_2.info.kind = PaneKind::Agent;
        pane_2.info.agent_id = Some("agent-1".to_string());
        state.insert_pane(pane_2).unwrap();
        state.insert_pane(sample_pane_runtime("pane-3")).unwrap();
        state
            .set_pane_layout(layout(&[("pane-1", 0), ("pane-2", 1), ("pane-3", 1)]))
            .unwrap();
        let mut agent = sample_agent("agent-1");
        agent.pane_id = Some("pane-2".to_string());
        state.insert_agent(agent).unwrap();
        state
            .enqueue_agent_turn("agent-1", "later".to_string())
            .unwrap();
        state
            .set_agent_draft("agent-1", "draft text".to_string())
            .unwrap();
        append_pane_scrollback(&workspace, "pane-2", b"old output").unwrap();

        state.capture_last_closed_pane("pane-2").unwrap();

        let snapshot = state.take_last_closed_pane().unwrap().unwrap();
        assert_eq!(snapshot.pane.id, "pane-2");
        assert_eq!(snapshot.pane.depth, 1);
        assert_eq!(snapshot.group.as_ref().map(|group| group.id.as_str()), None);
        assert_eq!(snapshot.index, 1);
        assert_eq!(snapshot.scrollback, b"old output");
        let agent = snapshot.agent.unwrap();
        assert_eq!(agent.agent.id, "agent-1");
        assert_eq!(agent.queued_turns.len(), 1);
        assert_eq!(agent.queued_turns[0].text, "later");
        assert_eq!(agent.draft.as_deref(), Some("draft text"));
    }

    #[test]
    fn capture_last_closed_pane_caps_large_scrollback() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace.clone()));
        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
        state.set_pane_layout(layout(&[("pane-1", 0)])).unwrap();
        // Well past the undo cap so the snapshot must keep only the tail rather
        // than pin the whole log. Line-delimited so the cut lands cleanly.
        let line = b"scrollback line of terminal output\n";
        let mut big = Vec::new();
        while big.len() < MAX_UNDO_SCROLLBACK_BYTES + line.len() * 2 {
            big.extend_from_slice(line);
        }
        append_pane_scrollback(&workspace, "pane-1", &big).unwrap();

        state.capture_last_closed_pane("pane-1").unwrap();

        let snapshot = state.take_last_closed_pane().unwrap().unwrap();
        assert!(
            snapshot.scrollback.len() <= MAX_UNDO_SCROLLBACK_BYTES,
            "undo snapshot must not pin the full log ({} bytes)",
            snapshot.scrollback.len()
        );
        assert!(snapshot.scrollback.starts_with(line));
        assert!(snapshot.scrollback.ends_with(line));
    }

    #[test]
    fn pane_removal_deletes_scrollback_during_normal_runtime() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace.clone()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
        append_pane_scrollback(&workspace, "pane-1", b"old output").unwrap();

        state.remove_pane("pane-1").unwrap();

        assert!(
            read_pane_scrollback(&workspace, "pane-1")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn exit_teardown_preserves_scrollback_for_the_frozen_session() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace.clone()));
        state.insert_group_after(sample_group(), None).unwrap();
        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
        append_pane_scrollback(&workspace, "pane-1", b"old output").unwrap();

        state.finalize_persistence_for_exit();
        // This is the same removal the reader thread performs after kill_all_panes
        // closes the PTY and delivers EOF during application shutdown.
        state.remove_pane("pane-1").unwrap();

        assert_eq!(
            read_pane_scrollback(&workspace, "pane-1").unwrap(),
            b"old output"
        );
    }

    #[test]
    fn capture_last_group_pane_records_orphaned_agents_for_restore() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state
            .insert_group_after(sample_terminal_group(), None)
            .unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let mut agent = sample_agent("agent-1");
        agent.pane_id = None;
        agent.orphaned_queue_pane_id = Some("pane-7".to_string());
        state.insert_agent(agent).unwrap();
        state
            .enqueue_agent_turn("agent-1", "recover me".to_string())
            .unwrap();

        state.capture_last_closed_pane("pane-7").unwrap();

        let snapshot = state.take_last_closed_pane().unwrap().unwrap();
        assert!(snapshot.agent.is_none());
        assert_eq!(snapshot.orphaned_agents.len(), 1);
        assert_eq!(snapshot.orphaned_agents[0].agent.id, "agent-1");
        assert_eq!(
            snapshot.orphaned_agents[0].queued_turns[0].text,
            "recover me"
        );
        assert_eq!(
            snapshot.group.as_ref().map(|group| group.id.as_str()),
            Some("group-1")
        );
    }

    #[test]
    fn capture_last_group_pane_skips_queueless_orphaned_agents() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state
            .insert_group_after(sample_terminal_group(), None)
            .unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        // A pane-less sibling with no queue: restoring it would only resurrect an
        // invisible, unreachable agent, so it must not be captured.
        let mut idle_sibling = sample_agent("agent-1");
        idle_sibling.pane_id = None;
        state.insert_agent(idle_sibling).unwrap();

        state.capture_last_closed_pane("pane-7").unwrap();

        let snapshot = state.take_last_closed_pane().unwrap().unwrap();
        assert!(snapshot.orphaned_agents.is_empty());
    }

    #[test]
    fn closing_pane_would_strand_queued_work_only_for_last_pane_with_a_queue() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state
            .insert_group_after(sample_terminal_group(), None)
            .unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        state.insert_agent(sample_agent("agent-1")).unwrap();

        // Last pane, but no queued work yet.
        assert!(
            !state
                .closing_pane_would_strand_queued_work("pane-7")
                .unwrap()
        );

        // Last pane with a queued agent: closing it would strand the queue.
        state
            .enqueue_agent_turn("agent-1", "later".to_string())
            .unwrap();
        assert!(
            state
                .closing_pane_would_strand_queued_work("pane-7")
                .unwrap()
        );

        // A sibling pane keeps the group alive, so nothing is stranded.
        state.insert_pane(sample_pane_runtime("pane-8")).unwrap();
        assert!(
            !state
                .closing_pane_would_strand_queued_work("pane-7")
                .unwrap()
        );
    }

    #[test]
    fn restore_closed_pane_metadata_reinserts_pruned_agent_and_layout() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
        let mut pane_2 = sample_pane_runtime("pane-2");
        pane_2.info.kind = PaneKind::Agent;
        pane_2.info.agent_id = Some("agent-1".to_string());
        state.insert_pane(pane_2).unwrap();
        state.insert_pane(sample_pane_runtime("pane-3")).unwrap();
        state
            .set_pane_layout(layout(&[("pane-1", 0), ("pane-2", 1), ("pane-3", 1)]))
            .unwrap();
        let mut agent = sample_agent("agent-1");
        agent.pane_id = Some("pane-2".to_string());
        state.insert_agent(agent).unwrap();
        state
            .set_agent_draft("agent-1", "draft text".to_string())
            .unwrap();
        state.capture_last_closed_pane("pane-2").unwrap();
        let snapshot = state.take_last_closed_pane().unwrap().unwrap();

        state.remove_pane("pane-2").unwrap();
        assert!(state.agent("agent-1").unwrap().is_none());

        state.restore_closed_pane_metadata(&snapshot).unwrap();
        let mut restored_pane = sample_pane_runtime("pane-2");
        restored_pane.info = snapshot.pane.clone();
        state.insert_pane(restored_pane).unwrap();
        state
            .place_restored_pane(&snapshot.pane.id, snapshot.index, snapshot.pane.depth)
            .unwrap();

        assert_eq!(
            id_depths(&state.list_panes().unwrap()),
            vec![
                ("pane-1".to_string(), 0),
                ("pane-2".to_string(), 1),
                ("pane-3".to_string(), 1),
            ]
        );
        let restored_agent = state.agent("agent-1").unwrap().unwrap();
        assert_eq!(restored_agent.pane_id.as_deref(), Some("pane-2"));
        assert_eq!(
            state.agent_draft("agent-1").unwrap().as_deref(),
            Some("draft text")
        );
    }

    #[test]
    fn remove_pane_prunes_its_idle_agent_and_runtime_state() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        state.insert_agent(sample_agent("agent-1")).unwrap();
        state.set_agent_typing("agent-1", true).unwrap();
        state.mark_agent_pending_pause("agent-1").unwrap();

        state.remove_pane("pane-7").unwrap();

        // The closed pane's agent (no queued turns) is reclaimed with its runtime state.
        assert!(state.agent("agent-1").unwrap().is_none());
        assert!(!state.agent_is_typing("agent-1").unwrap());
    }

    #[test]
    fn remove_pane_keeps_queued_agent_while_sibling_pane_remains() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state
            .insert_group_after(sample_terminal_group(), None)
            .unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        state.insert_pane(sample_pane_runtime("pane-8")).unwrap();
        state.insert_agent(sample_agent("agent-1")).unwrap();
        state
            .enqueue_agent_turn("agent-1", "later".to_string())
            .unwrap();

        state.remove_pane("pane-7").unwrap();

        // Kept so the queue stays restart-recoverable via the orphaned-queue panel.
        assert!(state.agent("agent-1").unwrap().is_some());
        assert_eq!(
            state.list_agent_turn_queue("agent-1").unwrap(),
            vec!["later".to_string()]
        );
    }

    #[test]
    fn remove_group_removes_empty_group() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state
            .insert_group_after(sample_terminal_group(), None)
            .unwrap();

        state.remove_group("group-1").unwrap();

        assert!(state.list_groups().unwrap().is_empty());
    }

    #[test]
    fn remove_pane_removes_group_when_last_pane_closes() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state
            .insert_group_after(sample_terminal_group(), None)
            .unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();

        state.remove_pane("pane-7").unwrap();

        assert!(state.list_groups().unwrap().is_empty());
    }

    #[test]
    fn remove_pane_keeps_group_when_sibling_panes_remain() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state
            .insert_group_after(sample_terminal_group(), None)
            .unwrap();
        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
        state.insert_pane(sample_pane_runtime("pane-2")).unwrap();

        state.remove_pane("pane-1").unwrap();
        assert_eq!(state.list_groups().unwrap().len(), 1);
        state.remove_pane("pane-2").unwrap();

        assert!(state.list_groups().unwrap().is_empty());
    }

    #[test]
    fn restore_closed_pane_metadata_recreates_removed_group() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state
            .insert_group_after(sample_terminal_group(), None)
            .unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        state.capture_last_closed_pane("pane-7").unwrap();
        let snapshot = state.take_last_closed_pane().unwrap().unwrap();

        state.remove_pane("pane-7").unwrap();
        assert!(state.list_groups().unwrap().is_empty());

        state.restore_closed_pane_metadata(&snapshot).unwrap();
        assert_eq!(state.list_groups().unwrap()[0].id, "group-1");
    }

    #[test]
    fn last_agent_pane_close_removes_group_and_restore_recreates_it() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state
            .insert_group_after(sample_terminal_group(), None)
            .unwrap();
        let mut pane = sample_pane_runtime("pane-7");
        pane.info.kind = PaneKind::Agent;
        pane.info.agent_id = Some("agent-1".to_string());
        state.insert_pane(pane).unwrap();
        state.insert_agent(sample_agent("agent-1")).unwrap();
        state
            .enqueue_agent_turn("agent-1", "queued restore".to_string())
            .unwrap();
        state.capture_last_closed_pane("pane-7").unwrap();
        let snapshot = state.take_last_closed_pane().unwrap().unwrap();

        state.remove_pane("pane-7").unwrap();

        assert!(state.list_groups().unwrap().is_empty());
        assert!(state.agent("agent-1").unwrap().is_none());

        state.restore_closed_pane_metadata(&snapshot).unwrap();
        let mut restored_pane = sample_pane_runtime("pane-7");
        restored_pane.info = snapshot.pane.clone();
        state.insert_pane(restored_pane).unwrap();
        state
            .place_restored_pane(&snapshot.pane.id, snapshot.index, snapshot.pane.depth)
            .unwrap();

        assert_eq!(state.list_groups().unwrap()[0].id, "group-1");
        let restored_agent = state.agent("agent-1").unwrap().unwrap();
        assert_eq!(restored_agent.pane_id.as_deref(), Some("pane-7"));
        assert_eq!(
            state.list_agent_turn_queue("agent-1").unwrap(),
            vec!["queued restore".to_string()]
        );
    }

    #[test]
    fn last_pane_close_prunes_orphaned_agents_and_restore_rehydrates_them() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state
            .insert_group_after(sample_terminal_group(), None)
            .unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();
        let mut agent = sample_agent("agent-1");
        agent.pane_id = None;
        agent.orphaned_queue_pane_id = Some("pane-7".to_string());
        state.insert_agent(agent).unwrap();
        state
            .enqueue_agent_turn("agent-1", "queued restore".to_string())
            .unwrap();
        state.capture_last_closed_pane("pane-7").unwrap();
        let snapshot = state.take_last_closed_pane().unwrap().unwrap();

        state.remove_pane("pane-7").unwrap();

        assert!(state.list_groups().unwrap().is_empty());
        assert!(state.agent("agent-1").unwrap().is_none());

        state.restore_closed_pane_metadata(&snapshot).unwrap();
        let mut restored_pane = sample_pane_runtime("pane-7");
        restored_pane.info = snapshot.pane.clone();
        state.insert_pane(restored_pane).unwrap();
        state
            .place_restored_pane(&snapshot.pane.id, snapshot.index, snapshot.pane.depth)
            .unwrap();

        assert_eq!(state.list_groups().unwrap()[0].id, "group-1");
        let restored_agent = state.agent("agent-1").unwrap().unwrap();
        assert_eq!(restored_agent.pane_id, None);
        assert_eq!(
            restored_agent.orphaned_queue_pane_id.as_deref(),
            Some("pane-7")
        );
        assert_eq!(
            state.list_agent_turn_queue("agent-1").unwrap(),
            vec!["queued restore".to_string()]
        );
    }

    #[test]
    fn remove_group_refuses_open_panes_but_prunes_recoverable_agents() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state
            .insert_group_after(sample_terminal_group(), None)
            .unwrap();
        state.insert_pane(sample_pane_runtime("pane-7")).unwrap();

        assert_eq!(
            state.remove_group("group-1").unwrap_err(),
            "group still has open panes"
        );
        let state = AppState::new(test_config(temp_workspace()));
        state
            .insert_group_after(sample_terminal_group(), None)
            .unwrap();
        state.insert_agent(sample_agent("agent-1")).unwrap();
        state.remove_group("group-1").unwrap();
        assert!(state.list_groups().unwrap().is_empty());
        assert!(state.agent("agent-1").unwrap().is_none());
    }

    #[test]
    fn remove_group_prunes_agents_when_group_row_is_already_missing_and_persists() {
        let workspace = temp_workspace();
        let config = test_config(workspace.clone());

        {
            let state = AppState::new(config.clone());
            state.restore_session();
            state.insert_agent(sample_agent("agent-1")).unwrap();

            state.remove_group("group-1").unwrap();

            assert!(state.agent("agent-1").unwrap().is_none());
        }

        let state = AppState::new(config);
        state.restore_session();
        assert!(state.agent("agent-1").unwrap().is_none());
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn pane_file_roots_are_scoped_to_the_requesting_pane() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
        state.insert_pane(sample_pane_runtime("pane-2")).unwrap();
        // update_pane_cwd requires a real directory, so create two to stand in for
        // each pane's working directory.
        let proj_a = std::env::temp_dir().join("qmux-test-proj-a");
        let proj_b = std::env::temp_dir().join("qmux-test-proj-b");
        std::fs::create_dir_all(&proj_a).unwrap();
        std::fs::create_dir_all(&proj_b).unwrap();
        state
            .update_pane_cwd("pane-1", proj_a.display().to_string())
            .unwrap();
        state
            .update_pane_cwd("pane-2", proj_b.display().to_string())
            .unwrap();

        // A pane's browser-open roots include its own cwd but not another pane's.
        let roots = state.pane_file_roots("pane-1");
        assert!(roots.iter().any(|r| r == proj_a.as_path()));
        assert!(!roots.iter().any(|r| r == proj_b.as_path()));
    }

    #[test]
    fn pane_file_roots_exclude_a_cwd_at_or_above_the_workspace_root() {
        let workspace = temp_workspace();
        let workspace_parent = workspace.parent().unwrap().to_path_buf();
        let state = AppState::new(test_config(workspace.clone()));
        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();

        // A pane that cd'd to an ancestor of the workspace (here its parent) must not turn
        // its file token into a servable root over the whole workspace tree — that would
        // expose every other group's directory and .qmux/state.json.
        state
            .update_pane_cwd("pane-1", workspace_parent.display().to_string())
            .unwrap();
        let roots = state.pane_file_roots("pane-1");
        assert!(
            !roots
                .iter()
                .any(|r| path_is_ancestor_or_equal(r, &workspace)),
            "no served root should sit at or above the workspace root: {roots:?}"
        );

        // Setting it to the workspace root itself is likewise excluded.
        state
            .update_pane_cwd("pane-1", workspace.display().to_string())
            .unwrap();
        let roots = state.pane_file_roots("pane-1");
        assert!(
            !roots.iter().any(|r| {
                std::fs::canonicalize(r).ok() == std::fs::canonicalize(&workspace).ok()
            })
        );
    }

    #[test]
    fn pane_file_roots_are_empty_for_an_out_of_model_pane() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        // No pane inserted: pane_file_roots must fail *closed* (zero roots) rather than
        // fall back to serving the whole workspace root, so a token that ever resolves
        // to an out-of-model pane (spawn/teardown race, poisoned lock) reads nothing.
        assert!(state.pane_file_roots("ghost-pane").is_empty());
    }

    #[test]
    fn pane_file_roots_exclude_a_sibling_group_cwd() {
        let workspace = temp_workspace();
        let group1_dir = workspace.join("group-1");
        let group2_dir = workspace.join("group-2");
        std::fs::create_dir_all(&group1_dir).unwrap();
        std::fs::create_dir_all(&group2_dir).unwrap();

        let state = AppState::new(test_config(workspace));
        let mut group1 = sample_group_with_id("group-1");
        group1.dir = group1_dir.display().to_string();
        let mut group2 = sample_group_with_id("group-2");
        group2.dir = group2_dir.display().to_string();
        state.insert_group_after(group1, None).unwrap();
        state.insert_group_after(group2, None).unwrap();

        // pane-1 belongs to group-1 (sample_pane default group_id).
        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
        // A pane cd'ing into a *sibling* group's directory (a descendant of the
        // workspace root) must not turn its file token into a servable root over that
        // group's files — only its own group dir is served for in-workspace cwds.
        state
            .update_pane_cwd("pane-1", group2_dir.display().to_string())
            .unwrap();

        let roots = state.pane_file_roots("pane-1");
        assert!(
            roots.iter().any(|r| r == group1_dir.as_path()),
            "the pane's own group dir should be served: {roots:?}"
        );
        assert!(
            !roots.iter().any(|r| r == group2_dir.as_path()),
            "a sibling group's dir must never be served: {roots:?}"
        );
    }

    #[test]
    fn pane_file_token_round_trips_and_is_reclaimed_on_close() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();

        let token = state.pane_file_token("pane-1").unwrap();
        // Stable across calls and resolvable back to its own pane.
        assert_eq!(state.pane_file_token("pane-1").unwrap(), token);
        assert_eq!(state.pane_for_file_token(&token).as_deref(), Some("pane-1"));
        // A different pane gets a different token, distinct from the control token.
        assert_ne!(state.pane_file_token("pane-2").unwrap(), token);
        assert_ne!(state.pane_token("pane-1").unwrap(), token);

        // Closing the pane reclaims the token so a live file credential can't outlive
        // the pane it scopes.
        state.remove_pane("pane-1").unwrap();
        assert!(state.pane_for_file_token(&token).is_none());
    }

    #[test]
    fn remove_pane_reclaims_its_control_token() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();

        let token = state.pane_token("pane-1").unwrap();
        assert_eq!(state.pane_for_token(&token).as_deref(), Some("pane-1"));

        // The captured QMUX_TOKEN must not outlive its pane.
        state.remove_pane("pane-1").unwrap();
        assert!(state.pane_for_token(&token).is_none());
    }

    #[test]
    fn nest_pane_under_moves_and_indents_beneath_parent() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
        state.insert_pane(sample_pane_runtime("pane-2")).unwrap();
        state.insert_pane(sample_pane_runtime("pane-3")).unwrap();

        // Nest the last pane under the first; it moves directly after it at depth 1.
        let panes = state.nest_pane_under("pane-3", "pane-1").unwrap();
        assert_eq!(
            id_depths(&panes),
            vec![
                ("pane-1".to_string(), 0),
                ("pane-3".to_string(), 1),
                ("pane-2".to_string(), 0),
            ]
        );

        // Nesting under a deeper parent indents one further (a child of the child).
        let panes = state.nest_pane_under("pane-2", "pane-3").unwrap();
        assert_eq!(
            id_depths(&panes),
            vec![
                ("pane-1".to_string(), 0),
                ("pane-3".to_string(), 1),
                ("pane-2".to_string(), 2),
            ]
        );
    }

    #[test]
    fn set_agent_status_preserves_other_fields() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        let mut agent = AgentInfo {
            id: "agent-1".to_string(),
            group_id: "group-1".to_string(),
            adapter: "claude".to_string(),
            worktree_dir: "/tmp/x".to_string(),
            branch: None,
            pane_id: Some("pane-1".to_string()),
            orphaned_queue_pane_id: None,
            session_id: None,
            transcript_path: None,
            status: AgentStatus::Starting,
            model: None,
            effort: None,
            parent_id: Some("agent-0".to_string()),
            fork_point: Some("sess-src".to_string()),
            root_session_id: Some("sess-src".to_string()),
            thread_id: None,
            branch_id: None,
            paused: false,
            created_at: 1,
        };
        state.insert_agent(agent.clone()).unwrap();

        // Simulate the spawned fork's SessionStart landing: it records the new
        // session id and transcript on the agent.
        agent.session_id = Some("sess-fork".to_string());
        agent.transcript_path = Some("/tmp/fork.jsonl".to_string());
        agent.status = AgentStatus::Running;
        state.update_agent(agent).unwrap();

        // The post-attach status reset must not wipe what SessionStart just wrote.
        let updated = state
            .set_agent_status("agent-1", AgentStatus::AwaitingInput)
            .unwrap()
            .expect("agent exists");
        assert!(matches!(updated.status, AgentStatus::AwaitingInput));
        assert_eq!(updated.session_id.as_deref(), Some("sess-fork"));
        assert_eq!(updated.transcript_path.as_deref(), Some("/tmp/fork.jsonl"));
        assert_eq!(updated.parent_id.as_deref(), Some("agent-0"));

        assert!(
            state
                .set_agent_status("missing", AgentStatus::Idle)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn same_status_hooks_only_restamp_recent_session_after_coarseness() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        let agent = AgentInfo {
            id: "agent-1".to_string(),
            group_id: "group-1".to_string(),
            adapter: "claude".to_string(),
            worktree_dir: "/tmp/x".to_string(),
            branch: None,
            pane_id: Some("pane-1".to_string()),
            orphaned_queue_pane_id: None,
            session_id: Some("sess-1".to_string()),
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
        };
        state.insert_agent(agent).unwrap();

        let stamp = |state: &AppState| {
            state
                .list_recent_sessions(10)
                .unwrap()
                .into_iter()
                .find(|session| session.session_id.as_deref() == Some("sess-1"))
                .expect("recent session exists")
                .last_active_at
        };
        let initial = stamp(&state);

        // A hook re-asserting the same status inside the coarseness window is
        // bookkeeping-neutral: no fresh activity stamp (and so no dirty mark).
        state
            .set_agent_status("agent-1", AgentStatus::Running)
            .unwrap();
        assert_eq!(stamp(&state), initial);

        // A real transition still lands immediately, with a fresh stamp.
        std::thread::sleep(Duration::from_millis(5));
        state
            .set_agent_status("agent-1", AgentStatus::AwaitingInput)
            .unwrap();
        assert!(stamp(&state) > initial);
    }

    #[test]
    fn expired_outstanding_sends_are_pruned() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));

        state
            .record_agent_send(
                "agent-1",
                "queued turn".to_string(),
                AgentSendSource::QueuedTurn,
            )
            .unwrap();
        assert!(
            state
                .agent_has_outstanding_send_source("agent-1", AgentSendSource::QueuedTurn)
                .unwrap()
        );

        // A send that never echoes a UserPromptSubmit (e.g. the user cleared the
        // pasted text with Esc) must expire rather than suppress the
        // transcript-interruption fallback until the next hard idle.
        state
            .age_agent_outstanding_sends("agent-1", OUTSTANDING_SEND_TTL_MS + 1)
            .unwrap();
        assert!(
            !state
                .agent_has_outstanding_send_source("agent-1", AgentSendSource::QueuedTurn)
                .unwrap()
        );
    }

    #[test]
    fn stale_front_send_does_not_poison_prompt_matching() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));

        // A dead send at the front of the queue (never echoed) used to make every
        // later prompt report Mismatched; once expired, the next real send matches.
        state
            .record_agent_send("agent-1", "/model".to_string(), AgentSendSource::DirectSend)
            .unwrap();
        state
            .age_agent_outstanding_sends("agent-1", OUTSTANDING_SEND_TTL_MS + 1)
            .unwrap();
        state
            .record_agent_send(
                "agent-1",
                "real prompt".to_string(),
                AgentSendSource::DirectSend,
            )
            .unwrap();

        let matched = state
            .match_agent_prompt_submit("agent-1", Some("real prompt"))
            .unwrap();
        assert!(matches!(matched, AgentPromptSubmitMatch::Matched { .. }));
    }

    #[test]
    fn mutate_agent_only_touches_fields_the_closure_writes() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        let agent = AgentInfo {
            id: "agent-1".to_string(),
            group_id: "group-1".to_string(),
            adapter: "claude".to_string(),
            worktree_dir: "/tmp/x".to_string(),
            branch: None,
            pane_id: None,
            orphaned_queue_pane_id: None,
            session_id: None,
            transcript_path: None,
            status: AgentStatus::Starting,
            model: None,
            effort: None,
            parent_id: None,
            fork_point: None,
            root_session_id: None,
            thread_id: None,
            branch_id: None,
            paused: false,
            created_at: 1,
        };
        state.insert_agent(agent).unwrap();

        // Two interleaved field-scoped writers on a freshly spawned agent: the
        // SessionStart hook records the session id/transcript, then attach_agent_pane
        // binds the pane. Because each only writes its own fields, neither clobbers the
        // other — the bug a full-struct update_agent (read snapshot, write it back) had.
        state
            .mutate_agent("agent-1", |agent| {
                agent.session_id = Some("sess-1".to_string());
                agent.transcript_path = Some("/tmp/a.jsonl".to_string());
                agent.status = AgentStatus::Running;
            })
            .unwrap()
            .expect("agent exists");
        let bound = state
            .mutate_agent("agent-1", |agent| {
                agent.pane_id = Some("pane-1".to_string());
                agent.status = AgentStatus::Running;
            })
            .unwrap()
            .expect("agent exists");

        assert_eq!(bound.pane_id.as_deref(), Some("pane-1"));
        assert_eq!(bound.session_id.as_deref(), Some("sess-1"));
        assert_eq!(bound.transcript_path.as_deref(), Some("/tmp/a.jsonl"));

        // A missing agent yields None and never persists.
        assert!(
            state
                .mutate_agent("missing", |agent| agent.paused = true)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reorder_panes_renormalizes_depth() {
        let workspace = temp_workspace();
        let state = AppState::new(test_config(workspace));
        state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
        state.insert_pane(sample_pane_runtime("pane-2")).unwrap();
        state.insert_pane(sample_pane_runtime("pane-3")).unwrap();
        state
            .set_pane_layout(layout(&[("pane-1", 0), ("pane-2", 1), ("pane-3", 0)]))
            .unwrap();

        // Moving the nested child to the front would leave it at depth 1 with no
        // parent above; the reorder must re-level it to a valid tree.
        let panes = state
            .reorder_panes(vec![
                "pane-2".to_string(),
                "pane-1".to_string(),
                "pane-3".to_string(),
            ])
            .unwrap();
        assert_eq!(
            id_depths(&panes),
            vec![
                ("pane-2".to_string(), 0),
                ("pane-1".to_string(), 0),
                ("pane-3".to_string(), 0),
            ]
        );
    }

    #[test]
    fn restore_rehydrates_metadata_but_not_pane_runtimes() {
        let workspace = temp_workspace();
        let config = test_config(workspace.clone());

        // Stand in for a previous process having persisted a full session.
        let persisted = PersistedState {
            next_id: 99,
            groups: vec![sample_terminal_group()],
            agents: vec![sample_agent("agent-1")],
            panes: vec![sample_pane("pane-7", Some("agent-1"))],
            queues: HashMap::from([(
                "agent-1".to_string(),
                vec![QueuedTurn::new("queued turn".to_string())],
            )]),
            ..PersistedState::default()
        };
        crate::persistence::save(&workspace, &persisted).unwrap();

        let state = AppState::new(config);
        let recovered = state.restore_session();

        // Pane metadata is returned for respawning, with fields intact...
        assert_eq!(recovered.len(), 1);
        let pane = &recovered[0];
        assert_eq!(pane.id, "pane-7");
        assert_eq!(pane.cwd, "/tmp/work/agent-1");
        assert_eq!(pane.cols, 132);
        assert_eq!(pane.rows, 43);

        // ...but the stale runtime is NOT trusted: no live pane exists until respawn.
        assert!(state.list_panes().unwrap().is_empty());
        assert!(state.pane_writer("pane-7").unwrap().is_none());

        // Groups, agents and queues are hydrated directly into the live model.
        assert_eq!(state.list_groups().unwrap().len(), 1);
        let agent = state.agent("agent-1").unwrap().expect("agent restored");
        assert_eq!(agent.session_id.as_deref(), Some("session-abc"));
        assert_eq!(agent.pane_id, None);
        assert_eq!(agent.orphaned_queue_pane_id.as_deref(), Some("pane-7"));
        assert!(matches!(agent.status, AgentStatus::Idle));
        assert_eq!(
            state.list_agent_turn_queue("agent-1").unwrap(),
            vec!["queued turn".to_string()]
        );
        state
            .remove_agent_turn_queue_item("agent-1", 0, Some("queued turn"), None)
            .unwrap();
        let agent = state.agent("agent-1").unwrap().expect("agent restored");
        assert_eq!(agent.orphaned_queue_pane_id, None);

        // next_id is advanced past the persisted high-water mark so ids never alias.
        assert!(state.next_id("pane").starts_with("pane-"));
        let raw = state.next_id("pane");
        let seq: u64 = raw.rsplit('-').next().unwrap().parse().unwrap();
        assert!(seq >= 99, "expected next_id >= persisted high-water mark");
    }

    #[test]
    fn restore_captures_a_one_shot_resume_for_a_live_shell_agent() {
        let workspace = temp_workspace();
        let config = test_config(workspace.clone());

        // A transcript on disk marks the session as still resumable.
        let transcript = workspace.join("session-abc.jsonl");
        std::fs::write(&transcript, b"{}\n").unwrap();
        let mut agent = sample_agent("agent-1");
        agent.branch = None;
        agent.transcript_path = Some(transcript.display().to_string());

        let persisted = PersistedState {
            next_id: 99,
            groups: vec![sample_terminal_group()],
            // The agent is still bound to its shell pane (it was running at shutdown).
            agents: vec![agent],
            panes: vec![sample_pane("pane-7", None)],
            ..PersistedState::default()
        };
        crate::persistence::save(&workspace, &persisted).unwrap();

        let state = AppState::new(config);
        state.restore_session();

        let resume = state
            .take_shell_agent_resume("pane-7")
            .expect("a resume was captured for the live shell agent");
        assert_eq!(resume.adapter, "claude");
        assert_eq!(resume.session_id, "session-abc");
        // One-shot: a later relaunch of the same pane id never re-triggers the resume.
        assert!(state.take_shell_agent_resume("pane-7").is_none());
    }

    #[test]
    fn restore_skips_resume_when_the_session_transcript_is_gone() {
        let workspace = temp_workspace();
        let config = test_config(workspace.clone());

        // sample_agent points at a transcript that does not exist; resuming it would
        // only error in the new shell, so no resume should be captured.
        let persisted = PersistedState {
            next_id: 99,
            groups: vec![sample_terminal_group()],
            agents: vec![sample_agent("agent-1")],
            panes: vec![sample_pane("pane-7", None)],
            ..PersistedState::default()
        };
        crate::persistence::save(&workspace, &persisted).unwrap();

        let state = AppState::new(config);
        state.restore_session();

        assert!(state.take_shell_agent_resume("pane-7").is_none());
    }

    #[test]
    fn persistence_stays_off_until_restore() {
        let workspace = temp_workspace();
        let config = test_config(workspace.clone());

        // Without restore_session(), mutations must not touch disk (keeps tests and
        // ad-hoc AppState construction hermetic).
        let state = AppState::new(config);
        state
            .enqueue_agent_turn("agent-1", "ghost".to_string())
            .unwrap();
        assert!(!crate::persistence::state_path(&workspace).exists());
    }

    #[test]
    fn osc_title_sanitization_matches_the_frontend_contract() {
        assert_eq!(
            sanitize_last_osc_title("  Build\u{1b}\n  42%  ").as_deref(),
            Some("Build 42%")
        );
        assert_eq!(sanitize_last_osc_title(" \n\t\u{7f} "), None);
        let truncated = format!("{}…", "x".repeat(MAX_LAST_OSC_TITLE_CHARS - 1));
        assert_eq!(
            sanitize_last_osc_title(&"x".repeat(MAX_LAST_OSC_TITLE_CHARS + 20)).as_deref(),
            Some(truncated.as_str())
        );
        assert_eq!(
            sanitize_last_osc_title(&format!(
                "{}   more",
                "x".repeat(MAX_LAST_OSC_TITLE_CHARS - 1)
            ))
            .expect("non-empty title")
            .chars()
            .count(),
            MAX_LAST_OSC_TITLE_CHARS
        );
    }

    #[test]
    fn last_osc_title_round_trips_without_replacing_the_base_title() {
        let workspace = temp_workspace();
        let config = test_config(workspace.clone());

        {
            let state = AppState::new(config.clone());
            assert!(state.restore_session().is_empty());
            let mut pane = sample_pane_runtime("pane-1");
            pane.info.title = "Shell".to_string();
            state.insert_pane(pane).unwrap();

            assert_eq!(
                state
                    .update_last_osc_title("pane-1", "  Reviewing\u{1b}\nchanges  ")
                    .unwrap()
                    .as_deref(),
                Some("Reviewing changes")
            );
            let current = state.list_panes().unwrap();
            assert_eq!(current[0].title, "Shell");
            assert_eq!(
                current[0].last_osc_title.as_deref(),
                Some("Reviewing changes")
            );
        }

        let restored = AppState::new(config);
        let panes = restored.restore_session();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].title, "Shell");
        assert_eq!(
            panes[0].last_osc_title.as_deref(),
            Some("Reviewing changes")
        );
    }

    #[test]
    fn active_tab_round_trips_through_persistence() {
        let workspace = temp_workspace();
        let config = test_config(workspace.clone());

        {
            let state = AppState::new(config.clone());
            assert!(state.restore_session().is_empty());
            state.insert_pane(sample_pane_runtime("pane-1")).unwrap();
            state.insert_pane(sample_pane_runtime("pane-2")).unwrap();

            state
                .set_active_tab_id(Some(" pane-2 ".to_string()))
                .unwrap();
            assert_eq!(state.active_tab_id().unwrap().as_deref(), Some("pane-2"));

            let saved = crate::persistence::load_with_diagnostics(&workspace).state;
            assert_eq!(saved.active_tab_id.as_deref(), Some("pane-2"));
        }

        let state = AppState::new(config);
        state.restore_session();
        assert_eq!(state.active_tab_id().unwrap().as_deref(), Some("pane-2"));

        state.set_active_tab_id(Some("   ".to_string())).unwrap();
        assert_eq!(state.active_tab_id().unwrap(), None);
    }

    #[test]
    fn agent_draft_round_trips_and_clears_through_persistence() {
        let workspace = temp_workspace();
        let config = test_config(workspace.clone());

        // First process: stash a draft for one agent. The agent must exist so the draft
        // survives restore's orphaned-draft pruning (a real draft always has a live
        // agent — the frontend only drafts for agents it knows about).
        {
            let state = AppState::new(config.clone());
            assert!(state.restore_session().is_empty());
            state.insert_agent(sample_agent("agent-1")).unwrap();
            state
                .set_agent_draft("agent-1", "half-written thought".to_string())
                .unwrap();
        }

        // Second process: the draft reloads from disk and a trimmed-empty value
        // clears it (so recovery never restores stray whitespace).
        {
            let state = AppState::new(config.clone());
            state.restore_session();
            assert_eq!(
                state.agent_draft("agent-1").unwrap().as_deref(),
                Some("half-written thought")
            );
            state.set_agent_draft("agent-1", "   ".to_string()).unwrap();
            assert_eq!(state.agent_draft("agent-1").unwrap(), None);
        }

        // Third process: the clear was persisted too.
        let state = AppState::new(config);
        state.restore_session();
        assert_eq!(state.agent_draft("agent-1").unwrap(), None);
    }

    #[test]
    fn shell_agent_jobs_track_transitions_and_require_two_missing_samples() {
        let state = AppState::new(test_config(temp_workspace()));
        let registered = state
            .register_shell_agent_job(
                "job-1".to_string(),
                "agent-1".to_string(),
                "pane-1".to_string(),
                42,
            )
            .unwrap();
        assert_eq!(registered.state, ShellAgentJobState::Foreground);
        assert!(
            state
                .update_shell_agent_job_sample("job-1", ShellAgentJobState::Foreground)
                .is_none()
        );
        assert_eq!(
            state
                .update_shell_agent_job_sample("job-1", ShellAgentJobState::Backgrounded)
                .unwrap()
                .state,
            ShellAgentJobState::Backgrounded
        );
        assert!(state.note_shell_agent_job_missing("job-1").is_none());
        assert_eq!(
            state
                .note_shell_agent_job_missing("job-1")
                .unwrap()
                .agent_id,
            "agent-1"
        );
        assert!(state.list_shell_agent_jobs().unwrap().is_empty());
    }

    #[test]
    fn stale_shell_job_cleanup_must_match_its_agent() {
        let state = AppState::new(test_config(temp_workspace()));
        state
            .register_shell_agent_job(
                "job-1".to_string(),
                "agent-old".to_string(),
                "pane-1".to_string(),
                42,
            )
            .unwrap();
        assert!(
            state
                .unregister_shell_agent_job("job-1", Some("agent-new"), Some("pane-1"))
                .is_none()
        );
        assert_eq!(state.list_shell_agent_jobs().unwrap().len(), 1);
        assert!(
            state
                .unregister_shell_agent_job("job-1", Some("agent-old"), Some("pane-1"))
                .is_some()
        );
    }

    fn sample_recent_session(id: &str, session_id: &str) -> RecentSessionInfo {
        RecentSessionInfo {
            id: id.to_string(),
            adapter: "claude".to_string(),
            group_id: Some("group-1".to_string()),
            session_id: Some(session_id.to_string()),
            transcript_path: None,
            worktree_dir: "/tmp/work/agent-1".to_string(),
            branch: None,
            model: None,
            effort: None,
            parent_id: None,
            fork_point: None,
            root_session_id: None,
            preview: None,
            line_count: 0,
            last_active_at: 1,
            created_at: 1,
            pane_id: None,
            agent_id: None,
            status: None,
            missing: false,
        }
    }

    #[test]
    fn lineage_root_falls_back_to_the_agents_own_session_id() {
        // The root of a lineage is stamped with root_session_id: None and has to
        // name itself, otherwise a root agent would resolve to no lineage at all.
        let root = sample_agent("agent-root");
        assert_eq!(
            lineage_root_session_id(&root),
            Some("session-abc".to_string())
        );

        let mut fork = sample_agent("agent-fork");
        fork.session_id = Some("session-fork".to_string());
        fork.root_session_id = Some("session-abc".to_string());
        assert_eq!(
            lineage_root_session_id(&fork),
            Some("session-abc".to_string())
        );
    }

    #[test]
    fn lineage_membership_covers_both_the_root_and_its_forks() {
        let root = sample_agent("agent-root");
        let mut fork = sample_agent("agent-fork");
        fork.session_id = Some("session-fork".to_string());
        fork.root_session_id = Some("session-abc".to_string());
        let mut stranger = sample_agent("agent-stranger");
        stranger.session_id = Some("session-other".to_string());

        assert!(agent_in_lineage(&root, "session-abc"));
        assert!(agent_in_lineage(&fork, "session-abc"));
        assert!(!agent_in_lineage(&stranger, "session-abc"));

        let mut recent_fork = sample_recent_session("recent-fork", "session-fork");
        recent_fork.root_session_id = Some("session-abc".to_string());
        assert!(recent_session_in_lineage(&recent_fork, "session-abc"));
        assert!(recent_session_in_lineage(
            &sample_recent_session("recent-root", "session-abc"),
            "session-abc"
        ));
        assert!(!recent_session_in_lineage(
            &sample_recent_session("recent-other", "session-other"),
            "session-abc"
        ));
    }

    #[test]
    fn an_agent_without_a_session_id_has_no_lineage() {
        // Pre-SessionStart the agent has no identity to group on; the frontend
        // hides the branch menu rather than showing a lineage of one.
        let mut agent = sample_agent("agent-new");
        agent.session_id = None;
        assert_eq!(lineage_root_session_id(&agent), None);

        agent.session_id = Some("   ".to_string());
        assert_eq!(lineage_root_session_id(&agent), None);
    }

    #[test]
    fn only_the_root_session_is_flagged_as_root() {
        let root = branch_from_recent_session(
            sample_recent_session("recent-root", "session-abc"),
            "session-abc",
        );
        assert!(root.is_root);

        let fork = branch_from_recent_session(
            sample_recent_session("recent-fork", "session-fork"),
            "session-abc",
        );
        assert!(!fork.is_root);

        // A live-but-unrecorded fork resolves the same way off the agent record.
        let mut agent = sample_agent("agent-fork");
        agent.session_id = Some("session-fork".to_string());
        agent.root_session_id = Some("session-abc".to_string());
        let live = branch_from_agent(&agent, "session-abc");
        assert!(!live.is_root);
        assert!(live.live);
        assert_eq!(live.recent_session_id, None);
    }
}
