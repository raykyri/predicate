pub mod claude;
pub mod codex;
pub mod grok;
pub mod opencode;

use crate::config::QmuxConfig;
use crate::events::QmuxEvent;
use crate::pty::{
    InitialPaneSize, ensure_shell_agent_startup_supported, spawn_shell_agent_command_pane,
};
use crate::state::{AppState, PaneInfo};
use crate::transcript::{Turn, TurnBlock};
// The canonical JSON string-field extractor. Re-exported so the adapters can reach it
// as `super::string_field` and share the one definition (see `transcript::string_field`).
pub(crate) use crate::transcript::string_field;
use crate::workspace::{
    AgentInfo, AgentStatus, PrepareAgentWorkspaceRequest, attach_agent_pane,
    mark_agent_spawn_failed, prepare_agent_workspace,
};
use claude::ClaudeAdapter;
use codex::CodexAdapter;
use grok::GrokAdapter;
use opencode::OpencodeAdapter;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub use claude::{PrepareShellClaudeLaunchRequest, SpawnClaudeRequest};

/// Single-quotes a path for safe interpolation into a POSIX shell command,
/// escaping embedded single quotes. Shared by the Claude and Codex adapters,
/// which both embed the qmux CLI path into generated hook commands.
pub(crate) fn shell_quote_path(path: &Path) -> String {
    let raw = path.display().to_string();
    format!("'{}'", raw.replace('\'', "'\\''"))
}

/// Single-quotes an arbitrary argument for safe interpolation into a POSIX shell
/// command. Used to embed a session id into an adapter's resume command line.
pub(crate) fn shell_quote_arg(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Whether a `transcript_path` reported by an adapter's hook notification may be
/// bound and tailed.
///
/// A hook arrives over the control socket carrying the pane's token, so a
/// prompt-injected agent can forge a `SessionStart` and point `transcript_path`
/// at any file. We can't fully validate the *first* path (SessionStart is how
/// qmux discovers it, and the agent may not have written the file yet), but we
/// constrain it several ways: a `.jsonl` extension, an absolute path (a relative
/// one would resolve against an unknown cwd), and — when the target already
/// exists — a regular file, so a forged hook can't aim the tailer at a directory,
/// a symlink, or a FIFO/device (which could block the tail thread). Once a
/// transcript is bound we additionally require any later path to be a sibling in
/// the same session directory. Adapters keep a session's rollouts in one flat
/// directory, so a legitimate rotation stays a sibling while a forged mid-session
/// hook can no longer relocate the tail to an unrelated file (another agent's
/// transcript, a device/FIFO, an arbitrary log). The Claude adapter delegates
/// here so the guard stays single-sourced.
pub(crate) fn hook_transcript_path_acceptable(current: Option<&str>, candidate: &str) -> bool {
    let candidate = Path::new(candidate);
    if candidate.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
        return false;
    }
    if !candidate.is_absolute() {
        return false;
    }
    // `symlink_metadata` does not follow the final component, so a symlink is seen as a
    // symlink (not a regular file) and rejected. A path that doesn't exist yet is allowed
    // through — the agent may not have written its transcript at SessionStart time.
    if let Ok(meta) = std::fs::symlink_metadata(candidate)
        && !meta.file_type().is_file()
    {
        return false;
    }
    match current {
        Some(current) => Path::new(current).parent() == candidate.parent(),
        None => true,
    }
}

/// Finds the existing unbound agent for `session_id` running in `cwd`, so a shell
/// resume (`claude --resume <id>` / `codex resume <id>`) rebinds the original agent
/// instead of minting a duplicate every restart. Scoped to the same adapter, the
/// same directory, and an agent not currently bound to a pane, so a manual resume
/// of a live session (bound elsewhere) or a different project still starts fresh.
pub(crate) fn reusable_session_agent(
    state: &AppState,
    adapter_id: &str,
    session_id: Option<&str>,
    cwd: &str,
) -> Result<Option<AgentInfo>, String> {
    let Some(session_id) = session_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    Ok(state.list_agents()?.into_iter().find(|agent| {
        agent.adapter == adapter_id
            && agent.pane_id.is_none()
            && same_dir(&agent.worktree_dir, cwd)
            && agent.session_id.as_deref() == Some(session_id)
    }))
}

/// Records a concrete native session id supplied to a shell-level resume command.
/// The CLI will keep this identity when it resumes, so make it visible before the
/// later SessionStart hook arrives. Parsers deliberately pass `None` for selectors
/// such as `--last` and for forks, whose live session gets a different identity.
pub(crate) fn record_shell_resume_identity(
    state: &AppState,
    agent: AgentInfo,
    session_id: Option<&str>,
) -> Result<AgentInfo, String> {
    let Some(session_id) = session_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return Ok(agent);
    };
    state
        .mutate_agent(&agent.id, |agent| {
            agent.session_id = Some(session_id.to_string());
        })?
        .ok_or_else(|| {
            format!(
                "agent {} disappeared while recording resumed session identity",
                agent.id
            )
        })
}

/// Records the shell-launch lineage for a resolved agent in one step — fork
/// provenance, then any concrete resumed session identity — so every adapter
/// makes a single call and cannot skip half of the ritual.
pub(crate) fn record_shell_session_lineage(
    state: &AppState,
    agent: AgentInfo,
    adapter_id: &str,
    fork_point: Option<&str>,
    resume_session_id: Option<&str>,
    cwd: &str,
) -> Result<AgentInfo, String> {
    let agent = record_shell_fork_lineage(state, agent, adapter_id, fork_point, cwd)?;
    record_shell_resume_identity(state, agent, resume_session_id)
}

/// Resolves a qmux-reserved agent for an automatically started shell command.
/// Ordinary commands typed by the user do not carry `prepared_agent_id` and keep
/// the existing create/reuse behavior. A prepared id must describe an unbound
/// agent for this adapter, pane group, and effective cwd so a forged environment
/// value cannot claim another workspace's agent record.
pub(crate) fn prepared_shell_agent(
    state: &AppState,
    adapter_id: &str,
    prepared_agent_id: Option<&str>,
    pane_id: &str,
    pane_group_id: &str,
    cwd: &str,
) -> Result<Option<AgentInfo>, String> {
    let Some(agent_id) = prepared_agent_id
        .map(str::trim)
        .filter(|agent_id| !agent_id.is_empty())
    else {
        return Ok(None);
    };
    let agent = state
        .agent(agent_id)?
        .ok_or_else(|| format!("prepared agent {agent_id} was not found"))?;
    if agent.adapter != adapter_id {
        return Err(format!(
            "prepared agent {agent_id} uses adapter '{}', not '{adapter_id}'",
            agent.adapter
        ));
    }
    if agent.group_id != pane_group_id {
        return Err(format!(
            "prepared agent {agent_id} belongs to workspace {}, not {pane_group_id}",
            agent.group_id
        ));
    }
    if agent
        .pane_id
        .as_deref()
        .is_some_and(|bound| bound != pane_id)
    {
        return Err(format!(
            "prepared agent {agent_id} is already attached to a pane"
        ));
    }
    if !same_dir(&agent.worktree_dir, cwd) {
        return Err(format!(
            "prepared agent {agent_id} expects working directory {}, not {cwd}",
            agent.worktree_dir
        ));
    }
    Ok(Some(agent))
}

/// Records native lineage on a fresh agent created by a shell-level fork command.
/// The CLI resumes `fork_point` but creates a new native session, so the source qmux
/// record must remain separate. Preserve qmux parent/root lineage when its source is
/// known in the same workspace.
pub(crate) fn record_shell_fork_lineage(
    state: &AppState,
    agent: AgentInfo,
    adapter_id: &str,
    fork_point: Option<&str>,
    cwd: &str,
) -> Result<AgentInfo, String> {
    let Some(fork_point) = fork_point.map(str::trim).filter(|id| !id.is_empty()) else {
        return Ok(agent);
    };
    let source = state.list_agents()?.into_iter().find(|candidate| {
        candidate.id != agent.id
            && candidate.adapter == adapter_id
            && candidate.session_id.as_deref() == Some(fork_point)
            && same_dir(&candidate.worktree_dir, cwd)
    });
    state
        .mutate_agent(&agent.id, |agent| {
            agent.fork_point = Some(fork_point.to_string());
            agent.root_session_id = source
                .as_ref()
                .and_then(|source| source.root_session_id.clone())
                .or_else(|| agent.root_session_id.clone())
                .or_else(|| Some(fork_point.to_string()));
            agent.parent_id = source
                .as_ref()
                .map(|source| source.id.clone())
                .or_else(|| agent.parent_id.clone());
        })?
        .ok_or_else(|| {
            format!(
                "agent {} disappeared while recording fork lineage",
                agent.id
            )
        })
}

/// True when both paths name the same directory. Canonicalization resolves symlinks,
/// `.`/`..`, trailing slashes, and (on case-insensitive volumes) the on-disk case, so a
/// shell's reported `$PWD` rebinds the original agent even when its spelling differs from
/// the recorded launch dir. Falls back to a raw compare when a side can't be canonicalized
/// (e.g. the directory no longer exists), preserving the previous exact-match behavior.
fn same_dir(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Parse a single line from a Claude-style or Grok-native rollout transcript (the
/// JSONL format used by Claude Code and by Grok Build's Claude-compatible sessions).
/// This is used for the `transcript_path` that Grok reports via its SessionStart hook.
pub(crate) fn parse_claude_native_transcript_line(
    agent_id: &str,
    source_index: usize,
    line: &str,
) -> Option<Turn> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    parse_claude_native_transcript_value(agent_id, source_index, &value)
}

/// As [`parse_claude_native_transcript_line`], but over an already-parsed value.
/// Lets a caller that also inspects the same line for other shapes (the Grok
/// adapter's synthetic-format fallback) parse the JSON once instead of per attempt.
pub(crate) fn parse_claude_native_transcript_value(
    agent_id: &str,
    source_index: usize,
    value: &Value,
) -> Option<Turn> {
    let message = value.get("message").unwrap_or(value);
    let role = message
        .get("role")
        .or_else(|| value.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("event")
        .to_string();
    let session_id = string_field(value, "session_id").or_else(|| string_field(value, "sessionId"));
    let content = message.get("content").or_else(|| value.get("content"))?;
    let blocks = parse_claude_native_blocks(content);

    if blocks.is_empty() {
        return None;
    }

    Some(Turn {
        id: format!("{agent_id}-{source_index}"),
        agent_id: agent_id.to_string(),
        session_id,
        role,
        blocks,
        source_index,
        timestamp: native_timestamp_ms(value),
        status: None,
        status_reason: None,
        native_id: string_field(value, "uuid"),
        parent_native_id: string_field(value, "parentUuid")
            .or_else(|| string_field(value, "parent_uuid")),
        native_message_id: string_field(message, "id"),
    })
}

/// Best-effort per-record timestamp from a native transcript value, in
/// milliseconds since the Unix epoch: RFC3339 strings or numeric epochs under
/// the common field spellings. None when the record carries no usable time.
pub(crate) fn native_timestamp_ms(value: &Value) -> Option<i64> {
    let field = value
        .get("timestamp")
        .or_else(|| value.get("created_at"))
        .or_else(|| value.get("createdAt"))?;
    match field {
        Value::String(text) => crate::transcript::rfc3339_to_epoch_ms(text),
        Value::Number(number) => {
            let raw = number
                .as_i64()
                .or_else(|| number.as_f64().map(|float| float as i64))?;
            // Second-resolution epochs sit far below any millisecond epoch of
            // the same era; scale them up rather than misreading them as 1970.
            if raw >= 1_000_000_000_000 {
                Some(raw)
            } else {
                raw.checked_mul(1_000)
            }
        }
        _ => None,
    }
}

pub(crate) fn parse_claude_native_lifecycle_event(line: &str) -> Option<TranscriptLifecycleEvent> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    parse_claude_native_lifecycle_value(&value)
}

/// As [`parse_claude_native_lifecycle_event`], but over an already-parsed value (see
/// [`parse_claude_native_transcript_value`]).
pub(crate) fn parse_claude_native_lifecycle_value(
    value: &Value,
) -> Option<TranscriptLifecycleEvent> {
    if value.get("interruptedMessageId").is_some() || value.get("interrupted_message_id").is_some()
    {
        return Some(TranscriptLifecycleEvent::Interrupted);
    }

    let message = value.get("message").unwrap_or(value);
    let content = message.get("content").or_else(|| value.get("content"))?;
    claude_native_content_has_interruption_marker(content)
        .then_some(TranscriptLifecycleEvent::Interrupted)
}

fn claude_native_content_has_interruption_marker(content: &Value) -> bool {
    match content {
        Value::String(text) => is_claude_interruption_marker(text),
        Value::Array(items) => items.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("text")
                && item
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(is_claude_interruption_marker)
        }),
        _ => false,
    }
}

pub(crate) fn is_claude_interruption_marker(text: &str) -> bool {
    matches!(
        text.trim(),
        "[Request interrupted by user]" | "[Request interrupted by user for tool use]"
    )
}

fn parse_claude_native_blocks(content: &Value) -> Vec<TurnBlock> {
    match content {
        Value::String(text) => vec![TurnBlock::Text { text: text.clone() }],
        Value::Array(items) => items.iter().filter_map(parse_claude_native_block).collect(),
        other => vec![TurnBlock::Raw {
            value: other.clone(),
        }],
    }
}

fn parse_claude_native_block(value: &Value) -> Option<TurnBlock> {
    let block_type = value.get("type").and_then(Value::as_str);
    match block_type {
        Some("text") => value
            .get("text")
            .and_then(Value::as_str)
            .map(|text| TurnBlock::Text {
                text: text.to_string(),
            }),
        Some("tool_use") => Some(TurnBlock::ToolUse {
            id: string_field(value, "id"),
            name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string(),
            input: value.get("input").cloned().unwrap_or(Value::Null),
        }),
        Some("tool_result") => Some(TurnBlock::ToolResult {
            tool_use_id: string_field(value, "tool_use_id")
                .or_else(|| string_field(value, "toolUseId")),
            content: value.get("content").cloned().unwrap_or(Value::Null),
            is_error: value
                .get("is_error")
                .or_else(|| value.get("isError"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        Some(_) => Some(TurnBlock::Raw {
            value: value.clone(),
        }),
        None => value.as_str().map(|text| TurnBlock::Text {
            text: text.to_string(),
        }),
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnAgentRequest {
    pub adapter_id: String,
    pub prompt: String,
    pub group_id: Option<String>,
    pub base_repo: Option<String>,
    pub base_ref: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub initial_size: Option<InitialPaneSize>,
    /// Opt in to an isolated git worktree; defaults to false (run in place).
    pub use_worktree: Option<bool>,
    #[serde(default)]
    pub options: Value,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepareShellAgentLaunchRequest {
    pub adapter_id: String,
    pub pane_id: String,
    pub cwd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub shell_job_id: Option<String>,
    #[serde(default)]
    pub supervisor_pid: Option<u32>,
    #[serde(default)]
    pub prepared_agent_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedShellAgentLaunch {
    pub binary: String,
    pub cwd: String,
    pub args: Vec<String>,
    pub envs: Vec<LaunchEnv>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchEnv {
    pub key: String,
    pub value: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterNotification {
    pub adapter_id: Option<String>,
    pub event: String,
    pub pane_id: Option<String>,
    pub agent_id: Option<String>,
    #[serde(default)]
    pub payload: Value,
}

pub(crate) fn subagent_id(payload: &Value) -> Option<&str> {
    ["agent_id", "agentId", "subagent_id", "subagentId"]
        .into_iter()
        .find_map(|key| payload.get(key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

pub enum AdapterNotificationOutcome {
    Event(QmuxEvent),
}

impl AdapterNotificationOutcome {
    pub fn into_events(self) -> Vec<QmuxEvent> {
        match self {
            AdapterNotificationOutcome::Event(event) => vec![event],
        }
    }
}

#[derive(Clone, Debug)]
pub struct ShellCommandIntegration {
    pub command_name: &'static str,
    pub adapter_id: &'static str,
}

#[derive(Clone, Debug)]
pub struct PermissionAction {
    #[allow(dead_code)]
    pub id: &'static str,
    #[allow(dead_code)]
    pub label: &'static str,
    #[allow(dead_code)]
    pub input: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptLifecycleEvent {
    Interrupted,
    TurnStarted,
}

impl TranscriptLifecycleEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            TranscriptLifecycleEvent::Interrupted => "interrupted",
            TranscriptLifecycleEvent::TurnStarted => "turnStarted",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ComposerPolicy {
    pub ready_statuses: Vec<AgentStatus>,
    pub queue_statuses: Vec<AgentStatus>,
    pub steer_statuses: Vec<AgentStatus>,
    #[allow(dead_code)]
    pub permission_actions: Vec<PermissionAction>,
}

impl ComposerPolicy {
    pub fn can_send(&self, status: AgentStatus) -> bool {
        self.ready_statuses.contains(&status)
    }

    pub fn should_queue(&self, status: AgentStatus) -> bool {
        self.queue_statuses.contains(&status)
    }

    pub fn can_steer(&self, status: AgentStatus) -> bool {
        self.steer_statuses.contains(&status)
    }
}

pub trait AgentAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;

    fn launch(&self, state: &AppState, request: SpawnAgentRequest) -> Result<PaneInfo, String>;

    fn resume(
        &self,
        state: &AppState,
        pane: &PaneInfo,
        agent: &AgentInfo,
    ) -> Result<PaneInfo, String>;

    fn prepare_shell_launch(
        &self,
        state: &AppState,
        request: PrepareShellAgentLaunchRequest,
    ) -> Result<PreparedShellAgentLaunch, String>;

    fn shell_commands(&self) -> Vec<ShellCommandIntegration>;

    /// The shell command that resumes `session_id` through this adapter's injected
    /// wrapper function (e.g. `claude --resume <id>`), used to re-launch the agent in
    /// a recovered shell pane. Defaults to `None` for adapters without a resume command.
    fn shell_resume_command(&self, _session_id: &str) -> Option<String> {
        None
    }

    fn ingest_notification(
        &self,
        state: &AppState,
        notification: AdapterNotification,
    ) -> Result<AdapterNotificationOutcome, String>;

    fn parse_transcript_line(
        &self,
        agent_id: &str,
        source_index: usize,
        line: &str,
    ) -> Option<Turn>;

    fn resolve_transcript_turns(
        &self,
        agent_id: &str,
        source_index_offset: usize,
        lines: &[String],
    ) -> Vec<Turn> {
        lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| {
                self.parse_transcript_line(agent_id, source_index_offset + index, line)
            })
            .collect()
    }

    fn transcript_line_can_update_turn_status(&self, _line: &str) -> bool {
        false
    }

    fn parse_transcript_lifecycle_event(&self, _line: &str) -> Option<TranscriptLifecycleEvent> {
        None
    }

    /// Writes a copy of `transcript_path` truncated to exclude `anchor` and
    /// everything after it, returning the new session's id. The copy lands
    /// beside the source so the CLI discovers it the same way it finds its own
    /// sessions — the id is the only handle a resume needs. Adapters that
    /// cannot be truncated safely keep this default and are reported as such by
    /// `adapter_supports_fork_at_message`.
    fn synthesize_truncated_session(
        &self,
        _transcript_path: &Path,
        _anchor: &MessageAnchor,
    ) -> Result<String, String> {
        Err(FORK_UNSUPPORTED_ERROR.to_string())
    }

    fn composer_policy(&self) -> ComposerPolicy;
}

pub struct AdapterRegistry {
    adapters: Vec<Box<dyn AgentAdapter>>,
}

impl AdapterRegistry {
    pub fn new(adapters: Vec<Box<dyn AgentAdapter>>) -> Self {
        Self { adapters }
    }

    pub fn get(&self, adapter_id: &str) -> Result<&dyn AgentAdapter, String> {
        self.adapters
            .iter()
            .find(|adapter| adapter.id() == adapter_id)
            .map(|adapter| adapter.as_ref())
            .ok_or_else(|| format!("unknown agent adapter '{adapter_id}'"))
    }

    pub fn shell_commands(&self) -> Vec<ShellCommandIntegration> {
        self.adapters
            .iter()
            .flat_map(|adapter| adapter.shell_commands())
            .collect()
    }

    pub fn metadata(&self) -> Vec<AdapterMetadata> {
        self.adapters
            .iter()
            .map(|adapter| AdapterMetadata {
                id: adapter.id().to_string(),
                label: adapter.display_name().to_string(),
                default: adapter.id() == "claude",
                supports_fork: adapter_supports_fork(adapter.id()),
                supports_fork_at_message: adapter_supports_fork_at_message(adapter.id()),
            })
            .collect()
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterMetadata {
    pub id: String,
    pub label: String,
    pub default: bool,
    /// Whether the adapter has a native fork command. Surfaces the capability
    /// to the frontend so features built on branching (research follow-ups)
    /// can filter their adapter choices instead of discovering the gap after
    /// a long root run.
    pub supports_fork: bool,
    /// Whether the adapter can fork from a chosen message rather than the
    /// session head. Gates the transcript's per-message fork action, which is
    /// hidden rather than disabled for adapters without it.
    pub supports_fork_at_message: bool,
}

pub fn adapter_registry(config: &QmuxConfig) -> AdapterRegistry {
    AdapterRegistry::new(vec![
        Box::new(ClaudeAdapter::new(config)),
        Box::new(CodexAdapter::new(config)),
        Box::new(OpencodeAdapter::new(config)),
        Box::new(GrokAdapter::new(config)),
    ])
}

pub(crate) fn ensure_on_path(binary: &str) -> Option<PathBuf> {
    let binary_path = Path::new(binary);
    if binary_path.components().count() > 1 {
        return binary_path.is_file().then(|| binary_path.to_path_buf());
    }

    crate::launch_path::resolve_binary(binary)
}

pub fn agent_spawn(state: &AppState, request: SpawnAgentRequest) -> Result<PaneInfo, String> {
    adapter_registry(state.config())
        .get(&request.adapter_id)?
        .launch(state, request)
}

/// Forks the agent running in `authed_pane` into a new tab and resumes its session.
/// With `nest`, the new tab is nested under the source (a child); otherwise it lands
/// immediately after the source as a sibling. The source is resolved from the
/// authenticated pane (never caller input), so a pane can only fork its own session.
/// When `prompt` is set, the fork is launched with that initial user message.
pub fn agent_fork(
    state: &AppState,
    authed_pane: &str,
    use_worktree: bool,
    nest: bool,
    prompt: Option<String>,
    anchor: Option<MessageAnchor>,
) -> Result<PaneInfo, String> {
    let source = state
        .agent_by_pane(authed_pane)?
        .ok_or_else(|| "no agent is running in this pane to fork".to_string())?;
    fork_agent_in_shell(
        state,
        &source,
        use_worktree,
        nest,
        prompt.as_deref(),
        anchor.as_ref(),
    )
}

/// Forks an ordinary terminal agent into a new persistent shell pane. The child
/// agent/worktree is reserved up front, then the shell starts the adapter through
/// the same `qmux agent-exec` path used when a user types the command manually.
/// When the adapter exits, the supervisor detaches it and leaves the shell prompt.
fn fork_agent_in_shell(
    state: &AppState,
    source: &AgentInfo,
    use_worktree: bool,
    nest: bool,
    prompt: Option<&str>,
    anchor: Option<&MessageAnchor>,
) -> Result<PaneInfo, String> {
    if !adapter_supports_fork(&source.adapter) {
        return Err(FORK_UNSUPPORTED_ERROR.to_string());
    }
    if anchor.is_some() && !adapter_supports_fork_at_message(&source.adapter) {
        return Err(FORK_AT_MESSAGE_UNSUPPORTED_ERROR.to_string());
    }
    ensure_shell_agent_startup_supported()?;
    let session_id = source
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|session_id| !session_id.is_empty())
        .ok_or_else(|| {
            format!(
                "this {} session isn't ready to fork yet (no session id); send a turn first",
                source.adapter
            )
        })?
        .to_string();
    let mut agent = prepare_agent_workspace(
        state,
        PrepareAgentWorkspaceRequest {
            group_id: Some(source.group_id.clone()),
            base_repo: if use_worktree {
                None
            } else {
                Some(source.worktree_dir.clone())
            },
            base_ref: Some("HEAD".to_string()),
            adapter: source.adapter.clone(),
            model: source.model.clone(),
            effort: source.effort.clone(),
            use_worktree,
        },
    )?;
    agent.parent_id = Some(source.id.clone());
    agent.fork_point = Some(session_id.clone());
    agent.root_session_id = source
        .root_session_id
        .clone()
        .or_else(|| Some(session_id.clone()));
    agent.status = AgentStatus::Idle;
    state.update_agent(agent.clone())?;

    let cwd = PathBuf::from(&agent.worktree_dir);
    if !cwd.is_dir() {
        return Err(format!(
            "fork working directory {} does not exist",
            cwd.display()
        ));
    }
    let args = match (source.adapter.as_str(), anchor) {
        // An anchored fork resumes a transcript synthesized from the source's
        // own file. Resolving the path from `source` — never from the caller —
        // keeps a forged anchor confined to the transcript of the pane that
        // sent it.
        (adapter_id, Some(anchor)) => {
            let transcript_path = source
                .transcript_path
                .as_deref()
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .ok_or_else(|| {
                    format!(
                        "this {} session has no transcript yet; send a turn first",
                        source.adapter
                    )
                })?;
            let seed_session_id = adapter_registry(state.config())
                .get(adapter_id)?
                .synthesize_truncated_session(Path::new(transcript_path), anchor)?;
            match adapter_id {
                "claude" => ClaudeAdapter::new(state.config()).shell_fork_at_message_args(
                    source,
                    &seed_session_id,
                    prompt,
                ),
                "codex" => CodexAdapter::new(state.config()).shell_fork_at_message_args(
                    source,
                    &seed_session_id,
                    prompt,
                ),
                _ => return Err(FORK_AT_MESSAGE_UNSUPPORTED_ERROR.to_string()),
            }
        }
        ("claude", None) => {
            ClaudeAdapter::new(state.config()).shell_fork_args(source, &cwd, prompt)?
        }
        ("codex", None) => {
            CodexAdapter::new(state.config()).shell_fork_args(source, &cwd, prompt)?
        }
        ("opencode", None) => {
            OpencodeAdapter::new(state.config()).shell_fork_args(source, &cwd, prompt)?
        }
        ("grok", None) => GrokAdapter::new(state.config()).shell_fork_args(source, &cwd, prompt)?,
        _ => return Err(FORK_UNSUPPORTED_ERROR.to_string()),
    };
    let pane_id = state.next_id("pane");
    let agent = attach_agent_pane(state, &agent.id, pane_id.clone())?;
    let pane = match spawn_shell_agent_command_pane(
        state,
        pane_id.clone(),
        agent.group_id.clone(),
        cwd,
        &source.adapter,
        &args,
        &agent.id,
    ) {
        Ok(pane) => pane,
        Err(err) => {
            let failed = mark_agent_spawn_failed(state, &agent.id, &pane_id)?;
            state.emit(QmuxEvent::new(
                "agent.spawn_failed",
                Some(pane_id),
                Some(failed.id.clone()),
                json!({ "agent": failed, "error": err.clone() }),
            ));
            return Err(err);
        }
    };
    finish_fork_spawn(state, source, pane, agent, nest)
}

/// Adapters with a native fork command. Owns the fork-eligibility check (and its
/// error message) for both the dispatch below and the queue engine's fail-fast
/// validation, so a new forkable adapter is added in one place.
pub fn adapter_supports_fork(adapter_id: &str) -> bool {
    matches!(adapter_id, "claude" | "codex" | "opencode" | "grok")
}

/// Adapters that can fork from a chosen message rather than the session head.
/// Neither CLI has a flag for this — `claude --fork-session` and `codex fork`
/// both branch at the head — so the adapter synthesizes a truncated copy of the
/// native transcript and resumes that instead. Only adapters whose transcript
/// format we can safely truncate qualify; the rest inherit the trait's default
/// `Err` and are filtered out of the UI by `supports_fork_at_message`.
pub fn adapter_supports_fork_at_message(adapter_id: &str) -> bool {
    matches!(adapter_id, "claude" | "codex")
}

/// Identifies the message a fork branches from. Carries every anchor the
/// frontend already has on `Turn` because the adapters key off different ones:
/// Claude walks the `parentUuid` chain, Codex cuts positionally on the line
/// index. Each adapter validates the field it needs and ignores the rest.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageAnchor {
    /// Native record uuid of the message being forked from.
    pub native_id: Option<String>,
    /// The message's parent uuid — the last record the fork keeps.
    pub parent_native_id: Option<String>,
    /// Transcript line index of the message being forked from.
    pub source_index: usize,
}

/// Refused when the chosen message has no history before it. Truncating there
/// would leave a transcript with no turns, which is a new session rather than a
/// fork — the caller should start a fresh agent instead.
pub const FORK_AT_MESSAGE_EMPTY_ERROR: &str =
    "Cannot fork from the first message; start a new agent instead";

pub const FORK_AT_MESSAGE_UNSUPPORTED_ERROR: &str =
    "Forking from a message is not supported for this agent adapter";

/// Parses a native transcript into `(line, value)` pairs, skipping records that
/// do not parse. Tolerating bad lines is deliberate: forking from a live session
/// races the CLI's own writes, so the final line can be torn mid-write. Dropping
/// it loses at most the in-flight record, which is after the fork point anyway.
pub(crate) fn parse_transcript_records(contents: &str) -> Vec<(&str, Value)> {
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok().map(|v| (line, v)))
        .collect()
}

/// Mints a random v4 UUID. Both adapters name synthesized sessions this way:
/// Claude derives the session id from the transcript filename, and Codex parses
/// it out of the `rollout-<timestamp>-<id>.jsonl` convention.
pub(crate) fn new_uuid_v4() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|err| format!("OS CSPRNG unavailable; cannot mint a session id: {err}"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

/// The adapter used when research must launch a run without an explicit
/// choice — a follow-up on a document, which has no adapter of its own. The
/// default adapter when it can fork (the new run's own follow-ups branch from
/// its session), else the first fork-capable adapter. The frontend mirrors
/// this preference to resolve adapter-specific composer affordances.
pub fn default_fork_adapter(config: &QmuxConfig) -> Result<String, String> {
    let metadata = adapter_registry(config).metadata();
    metadata
        .iter()
        .find(|adapter| adapter.default && adapter.supports_fork)
        .or_else(|| metadata.iter().find(|adapter| adapter.supports_fork))
        .map(|adapter| adapter.id.clone())
        .ok_or_else(|| "no installed agent supports research follow-ups".to_string())
}

pub const FORK_UNSUPPORTED_ERROR: &str = "Fork is not supported for this agent adapter";

/// Forks directly into a dedicated agent pane. Research runs use this path so
/// their transient execution panes retain the existing lifecycle; the queue
/// engine also uses it when dispatching fork-delivery turns without a calling
/// pane to authenticate. Ordinary UI/control-socket forks go through
/// [`fork_agent_in_shell`] instead.
pub fn fork_agent_source(
    state: &AppState,
    source: &AgentInfo,
    use_worktree: bool,
    nest: bool,
    prompt: Option<&str>,
) -> Result<PaneInfo, String> {
    let (pane, agent) = match source.adapter.as_str() {
        "claude" => {
            ClaudeAdapter::new(state.config()).fork_pane(state, source, use_worktree, prompt)?
        }
        "codex" => {
            CodexAdapter::new(state.config()).fork_pane(state, source, use_worktree, prompt)?
        }
        "opencode" => {
            OpencodeAdapter::new(state.config()).fork_pane(state, source, use_worktree, prompt)?
        }
        "grok" => {
            GrokAdapter::new(state.config()).fork_pane(state, source, use_worktree, prompt)?
        }
        _ => return Err(FORK_UNSUPPORTED_ERROR.to_string()),
    };
    finish_fork_spawn(state, source, pane, agent, nest)
}

fn finish_fork_spawn(
    state: &AppState,
    source: &AgentInfo,
    pane: PaneInfo,
    agent: AgentInfo,
    nest: bool,
) -> Result<PaneInfo, String> {
    if let Some(source_pane) = source.pane_id.as_deref() {
        // Placement is cosmetic and the fork has already spawned. The source
        // pane can legitimately vanish between the fork and this point —
        // research retirement closes a completed parent pane the moment its
        // node completes, exactly when follow-ups become possible — and
        // propagating the placement error would report failure for a live
        // pane+agent the caller then can neither see nor clean up. Leave the
        // new pane at the end of the order instead.
        let placed = if nest {
            state.nest_pane_under(&pane.id, source_pane)
        } else {
            state.place_pane_after(&pane.id, source_pane)
        };
        if let Err(err) = placed {
            eprintln!(
                "qmux: fork of agent {} spawned but could not be placed relative to pane {source_pane}: {err}",
                source.id
            );
        }
    }
    state.emit(QmuxEvent::new(
        "agent.forked",
        Some(pane.id.clone()),
        Some(agent.id.clone()),
        json!({ "agent": agent, "pane": pane, "sourceAgentId": source.id }),
    ));
    Ok(pane)
}

/// Starts a fresh session of `source`'s adapter in the source's own directory,
/// launched with `prompt` as its first message, and nests the new pane under the
/// source's. Used by the queue engine for new-session-delivery turns. Emits
/// `agent.spawned` with source "queue" so the frontend refreshes its pane list
/// (unlike launcher spawns, no frontend caller holds the returned pane).
pub fn spawn_sibling_agent_session(
    state: &AppState,
    source: &AgentInfo,
    prompt: &str,
) -> Result<PaneInfo, String> {
    let pane = adapter_registry(state.config())
        .get(&source.adapter)?
        .launch(
            state,
            SpawnAgentRequest {
                adapter_id: source.adapter.clone(),
                prompt: prompt.to_string(),
                group_id: Some(source.group_id.clone()),
                // Run in the source's directory (no worktree), like an in-place fork.
                base_repo: Some(source.worktree_dir.clone()),
                base_ref: Some("HEAD".to_string()),
                cwd: None,
                model: source.model.clone(),
                initial_size: None,
                use_worktree: Some(false),
                options: Value::Null,
            },
        )?;
    if let Some(source_pane) = source.pane_id.as_deref() {
        // Best-effort, like fork placement above: the session has already
        // spawned, and a source pane closed in the meantime must not turn a
        // live pane into a reported failure.
        if let Err(err) = state.nest_pane_under(&pane.id, source_pane) {
            eprintln!(
                "qmux: sibling session for agent {} spawned but could not be nested under pane {source_pane}: {err}",
                source.id
            );
        }
    }
    let agent = state.agent_by_pane(&pane.id)?;
    state.emit(QmuxEvent::new(
        "agent.spawned",
        Some(pane.id.clone()),
        agent.as_ref().map(|agent| agent.id.clone()),
        json!({ "agent": agent, "pane": pane, "source": "queue" }),
    ));
    Ok(pane)
}

pub fn agent_prepare_shell_launch(
    state: &AppState,
    request: PrepareShellAgentLaunchRequest,
) -> Result<PreparedShellAgentLaunch, String> {
    let shell_job_id = request.shell_job_id.clone();
    let supervisor_pid = request.supervisor_pid;
    let pane_id = request.pane_id.clone();
    let prepared_agent_id = request.prepared_agent_id.clone();
    let prepared = match adapter_registry(state.config())
        .get(&request.adapter_id)?
        .prepare_shell_launch(state, request)
    {
        Ok(prepared) => prepared,
        Err(err) => {
            // The caller authenticated for this pane only, and `prepared_agent_id`
            // is a caller-supplied environment value — the very thing
            // `prepared_shell_agent` refuses to honor across workspaces or panes
            // on the success path (and whose validation error may be exactly why
            // we are here). Apply the same ownership scope before recording a
            // failure, so one pane's token can't flip an unrelated agent to
            // Failed by naming it in a failing prepare.
            if let Some(agent_id) = prepared_agent_id
                && let Some(agent) = state.agent(&agent_id)?
                && state.pane_group_id(&pane_id)?.as_deref() == Some(agent.group_id.as_str())
                && agent
                    .pane_id
                    .as_deref()
                    .is_none_or(|bound| bound == pane_id)
            {
                let failed = mark_agent_spawn_failed(state, &agent_id, &pane_id)?;
                state.emit(QmuxEvent::new(
                    "agent.spawn_failed",
                    Some(pane_id),
                    Some(failed.id.clone()),
                    json!({ "agent": failed, "error": err.clone() }),
                ));
            }
            return Err(err);
        }
    };
    if let (Some(job_id), Some(supervisor_pid)) = (shell_job_id, supervisor_pid) {
        let agent_id = prepared
            .envs
            .iter()
            .find(|env| env.key == "QMUX_AGENT_ID")
            .map(|env| env.value.clone())
            .ok_or_else(|| "prepared shell launch is missing its agent id".to_string())?;
        let info = state.register_shell_agent_job(job_id, agent_id, pane_id, supervisor_pid)?;
        crate::shell_jobs::emit_job_state(state, &info);
    }
    Ok(prepared)
}

pub fn agent_composer_policy(
    state: &AppState,
    agent: &AgentInfo,
) -> Result<ComposerPolicy, String> {
    Ok(adapter_registry(state.config())
        .get(&agent.adapter)?
        .composer_policy())
}

pub fn ingest_adapter_notification(
    state: &AppState,
    notification: AdapterNotification,
) -> Result<AdapterNotificationOutcome, String> {
    let adapter_id = notification_adapter_id(state, &notification)?;
    adapter_registry(state.config())
        .get(&adapter_id)?
        .ingest_notification(state, notification)
}

fn notification_adapter_id(
    state: &AppState,
    notification: &AdapterNotification,
) -> Result<String, String> {
    if let Some(agent_id) = notification.agent_id.as_deref() {
        let agent = state
            .agent(agent_id)?
            .ok_or_else(|| format!("agent {agent_id} was not found"))?;
        return Ok(agent.adapter);
    }

    notification_adapter_fallback(state, notification)?
        .ok_or_else(|| "hook.notify could not resolve an agent adapter for this pane".to_string())
}

/// Like `notification_adapter_id`, but tolerant of a claimed agent id whose
/// record no longer exists: SessionStart recovery resolves the adapter from
/// the remaining hints instead of failing on the stale id. Kept beside the
/// strict resolver so the two hint chains cannot drift.
pub(crate) fn notification_adapter_hint(
    state: &AppState,
    notification: &AdapterNotification,
) -> Result<Option<String>, String> {
    if let Some(agent_id) = notification.agent_id.as_deref()
        && let Some(agent) = state.agent(agent_id)?
    {
        return Ok(Some(agent.adapter));
    }

    notification_adapter_fallback(state, notification)
}

fn notification_adapter_fallback(
    state: &AppState,
    notification: &AdapterNotification,
) -> Result<Option<String>, String> {
    if let Some(pane_id) = notification.pane_id.as_deref()
        && let Some(agent) = state.agent_by_pane(pane_id)?
    {
        return Ok(Some(agent.adapter));
    }

    Ok(notification.adapter_id.clone().or_else(|| {
        notification
            .payload
            .get("adapterId")
            .and_then(Value::as_str)
            .map(ToString::to_string)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AdapterConfigs, ClaudeAdapterConfig, CodexAdapterConfig, GrokAdapterConfig,
        OpencodeAdapterConfig,
    };
    use std::path::PathBuf;

    fn test_config() -> QmuxConfig {
        QmuxConfig {
            workspace_root: PathBuf::from("/tmp/qmux-adapter-tests"),
            socket_path: PathBuf::from("/tmp/qmux-adapter-tests.sock"),
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
            claude_plugin_dir: PathBuf::new(),
            opencode_plugin_dir: PathBuf::new(),
        }
    }

    #[test]
    fn registry_rejects_unknown_adapter() {
        let registry = adapter_registry(&test_config());

        let err = match registry.get("missing") {
            Ok(_) => panic!("missing adapter should be rejected"),
            Err(err) => err,
        };

        assert_eq!(err, "unknown agent adapter 'missing'");
    }

    #[test]
    fn runtime_metadata_marks_claude_as_default() {
        let registry = adapter_registry(&test_config());

        let metadata = registry.metadata();
        assert_eq!(metadata.len(), 4);
        assert_eq!(metadata[0].id, "claude");
        assert!(metadata[0].default);
        assert_eq!(metadata[1].id, "codex");
        assert!(!metadata[1].default);
        assert_eq!(metadata[2].id, "opencode");
        assert!(!metadata[2].default);
        assert_eq!(metadata[3].id, "grok");
        assert!(!metadata[3].default);
        assert!(adapter_supports_fork("grok"));
        assert!(adapter_supports_fork("opencode"));
    }

    #[test]
    fn agent_fork_requires_a_supported_agent_in_the_pane() {
        let state = AppState::new(test_config());

        // No agent bound to the pane: nothing to fork.
        let err = agent_fork(&state, "pane-1", false, true, None, None).unwrap_err();
        assert!(err.contains("no agent"), "unexpected error: {err}");

        // An adapter without a native fork command is rejected before any spawn is attempted.
        state
            .insert_agent(AgentInfo {
                id: "agent-1".to_string(),
                group_id: "group-1".to_string(),
                adapter: "unsupported".to_string(),
                worktree_dir: "/tmp/qmux-adapter-tests".to_string(),
                branch: None,
                pane_id: Some("pane-1".to_string()),
                orphaned_queue_pane_id: None,
                session_id: Some("session-1".to_string()),
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
        let err = agent_fork(&state, "pane-1", false, true, None, None).unwrap_err();
        assert_eq!(err, FORK_UNSUPPORTED_ERROR);
    }

    fn session_agent(id: &str, pane_id: Option<&str>, dir: &str, session: &str) -> AgentInfo {
        AgentInfo {
            id: id.to_string(),
            group_id: "group-1".to_string(),
            adapter: "claude".to_string(),
            worktree_dir: dir.to_string(),
            branch: None,
            pane_id: pane_id.map(ToString::to_string),
            orphaned_queue_pane_id: None,
            session_id: Some(session.to_string()),
            transcript_path: None,
            status: AgentStatus::Idle,
            model: None,
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

    // A failing prepare names its agent via a caller-controlled environment
    // value. The failure path must apply the same ownership scope as the
    // success path: one pane's token must not flip an agent bound elsewhere
    // (or in another workspace) to Failed by naming it in a doomed prepare.
    #[test]
    fn failing_prepare_does_not_fail_an_out_of_scope_prepared_agent() {
        let state = AppState::new(test_config());
        state
            .insert_agent(session_agent("agent-1", Some("pane-9"), "/work", "sess-1"))
            .unwrap();

        let err = agent_prepare_shell_launch(
            &state,
            PrepareShellAgentLaunchRequest {
                adapter_id: "claude".to_string(),
                pane_id: "pane-1".to_string(),
                cwd: "/work".to_string(),
                args: Vec::new(),
                shell_job_id: None,
                supervisor_pid: None,
                prepared_agent_id: Some("agent-1".to_string()),
            },
        )
        .unwrap_err();
        assert!(!err.is_empty());

        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(
            matches!(agent.status, AgentStatus::Idle),
            "an agent outside the failing pane's scope must keep its status"
        );
        assert_eq!(agent.pane_id.as_deref(), Some("pane-9"));
    }

    #[test]
    fn reusable_session_agent_matches_an_unbound_same_dir_session() {
        let state = AppState::new(test_config());
        state
            .insert_agent(session_agent("agent-1", None, "/work", "sess-1"))
            .unwrap();

        let found = reusable_session_agent(&state, "claude", Some("sess-1"), "/work").unwrap();
        assert_eq!(
            found.as_ref().map(|agent| agent.id.as_str()),
            Some("agent-1")
        );

        // No session id, a different session, a different dir, or a different adapter
        // all start fresh instead of reusing.
        assert!(
            reusable_session_agent(&state, "claude", None, "/work")
                .unwrap()
                .is_none()
        );
        assert!(
            reusable_session_agent(&state, "claude", Some("other"), "/work")
                .unwrap()
                .is_none()
        );
        assert!(
            reusable_session_agent(&state, "claude", Some("sess-1"), "/elsewhere")
                .unwrap()
                .is_none()
        );
        assert!(
            reusable_session_agent(&state, "codex", Some("sess-1"), "/work")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reusable_session_agent_rebinds_across_equivalent_dir_spellings() {
        let state = AppState::new(test_config());
        // A real directory so both the recorded launch dir and the shell's reported $PWD
        // can be canonicalized to the same target.
        let base = std::env::temp_dir().join(format!("qmux-reuse-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let launch_dir = base.display().to_string();
        state
            .insert_agent(session_agent("agent-1", None, &launch_dir, "sess-1"))
            .unwrap();

        // A trailing `/.` (the kind of drift a `cd` round-trip can leave in $PWD) is not a
        // byte-for-byte match, so the rebind now leans on canonicalization to recognize it
        // as the same directory rather than minting a duplicate agent.
        let equivalent_spelling = base.join(".").display().to_string();
        assert_ne!(launch_dir, equivalent_spelling);
        let found =
            reusable_session_agent(&state, "claude", Some("sess-1"), &equivalent_spelling).unwrap();
        assert_eq!(
            found.as_ref().map(|agent| agent.id.as_str()),
            Some("agent-1")
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn reusable_session_agent_never_hijacks_a_bound_session() {
        let state = AppState::new(test_config());
        state
            .insert_agent(session_agent("agent-1", Some("pane-9"), "/work", "sess-1"))
            .unwrap();

        // A session still bound to a live pane must not be stolen by a resume.
        assert!(
            reusable_session_agent(&state, "claude", Some("sess-1"), "/work")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn shell_resume_identity_is_visible_before_the_start_hook() {
        let state = AppState::new(test_config());
        let mut agent = session_agent("agent-1", None, "/work", "placeholder");
        agent.session_id = None;
        state.insert_agent(agent.clone()).unwrap();

        let agent = record_shell_resume_identity(&state, agent, Some(" resumed-session ")).unwrap();

        assert_eq!(agent.session_id.as_deref(), Some("resumed-session"));
        assert_eq!(
            state
                .agent("agent-1")
                .unwrap()
                .unwrap()
                .session_id
                .as_deref(),
            Some("resumed-session")
        );
    }

    #[test]
    fn shell_resume_identity_waits_for_hook_without_a_concrete_id() {
        let state = AppState::new(test_config());
        let agent = session_agent("agent-1", None, "/work", "known-session");
        state.insert_agent(agent.clone()).unwrap();

        let agent = record_shell_resume_identity(&state, agent, None).unwrap();

        assert_eq!(agent.session_id.as_deref(), Some("known-session"));
    }

    #[test]
    fn prepared_shell_agent_requires_matching_unbound_identity() {
        let state = AppState::new(test_config());
        state
            .insert_agent(session_agent(
                "prepared-agent",
                None,
                "/tmp/qmux-adapter-tests",
                "placeholder",
            ))
            .unwrap();

        let prepared = prepared_shell_agent(
            &state,
            "claude",
            Some("prepared-agent"),
            "pane-new",
            "group-1",
            "/tmp/qmux-adapter-tests",
        )
        .unwrap()
        .unwrap();
        assert_eq!(prepared.id, "prepared-agent");

        let err = prepared_shell_agent(
            &state,
            "codex",
            Some("prepared-agent"),
            "pane-new",
            "group-1",
            "/tmp/qmux-adapter-tests",
        )
        .unwrap_err();
        assert!(err.contains("uses adapter"));

        state
            .mutate_agent("prepared-agent", |agent| {
                agent.pane_id = Some("pane-live".to_string());
            })
            .unwrap();
        assert!(
            prepared_shell_agent(
                &state,
                "claude",
                Some("prepared-agent"),
                "pane-live",
                "group-1",
                "/tmp/qmux-adapter-tests",
            )
            .unwrap()
            .is_some()
        );
        let err = prepared_shell_agent(
            &state,
            "claude",
            Some("prepared-agent"),
            "pane-new",
            "group-1",
            "/tmp/qmux-adapter-tests",
        )
        .unwrap_err();
        assert!(err.contains("already attached"));
    }

    #[test]
    fn shell_fork_lineage_links_a_fresh_agent_without_reusing_the_source() {
        let state = AppState::new(test_config());
        state
            .insert_agent(session_agent(
                "source-agent",
                Some("pane-source"),
                "/work",
                "source-session",
            ))
            .unwrap();
        let mut fork = session_agent("fork-agent", None, "/work", "placeholder");
        fork.session_id = None;
        state.insert_agent(fork.clone()).unwrap();

        let fork =
            record_shell_fork_lineage(&state, fork, "claude", Some("source-session"), "/work")
                .unwrap();

        assert_eq!(fork.parent_id.as_deref(), Some("source-agent"));
        assert_eq!(fork.fork_point.as_deref(), Some("source-session"));
        assert_eq!(fork.root_session_id.as_deref(), Some("source-session"));
        assert_eq!(
            state
                .agent("source-agent")
                .unwrap()
                .unwrap()
                .session_id
                .as_deref(),
            Some("source-session")
        );
    }
}
