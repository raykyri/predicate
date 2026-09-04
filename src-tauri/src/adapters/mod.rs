pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod cursor;
pub mod devin;
pub mod grok;
pub mod muse;
pub mod opencode;
pub mod pi;

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
    ActiveWorkspaceSource, AgentInfo, AgentStatus, PrepareAgentWorkspaceRequest, attach_agent_pane,
    mark_agent_spawn_failed, prepare_agent_workspace_with_parent,
    prepare_named_agent_workspace_with_parent,
};
use antigravity::AntigravityAdapter;
use claude::ClaudeAdapter;
use codex::CodexAdapter;
use cursor::CursorAdapter;
use devin::DevinAdapter;
use grok::GrokAdapter;
use muse::MuseAdapter;
use opencode::OpencodeAdapter;
use pi::PiAdapter;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
            && native_session_selector_matches(agent, session_id, cwd)
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

/// Value of a CLI flag from shell args: `--flag value` or `--flag=value`.
/// Stops at a bare `--` so positional prompts after the separator are not
/// mistaken for flag values.
pub(crate) fn cli_flag_value(args: &[String], flag: &str) -> Option<String> {
    let eq_prefix = format!("{flag}=");
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            break;
        }
        if let Some(value) = arg.strip_prefix(&eq_prefix) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        } else if arg == flag {
            if let Some(value) = args.get(index + 1) {
                if !value.starts_with('-') {
                    let value = value.trim();
                    if !value.is_empty() {
                        return Some(value.to_string());
                    }
                }
            }
        }
        index += 1;
    }
    None
}

/// Normalize a raw model id for storage/display. Maps Claude API-style ids
/// (`claude-fable-5`, `claude-opus-4-7`) to short launcher names (`fable`,
/// `opus`) when the family is recognized; otherwise keeps the trimmed raw id.
/// Returns `None` for empty values and Claude's synthetic placeholders.
pub(crate) fn normalize_agent_model(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("<synthetic>") {
        return None;
    }
    const CLAUDE_FAMILIES: &[&str] = &["fable", "opus", "sonnet", "haiku"];
    if CLAUDE_FAMILIES
        .iter()
        .any(|family| raw.eq_ignore_ascii_case(family))
    {
        return Some(raw.to_ascii_lowercase());
    }
    if let Some(rest) = raw
        .strip_prefix("claude-")
        .or_else(|| raw.strip_prefix("Claude-"))
    {
        // Prefer a known family token anywhere in the remainder so both
        // `claude-fable-5` and older `claude-3-5-sonnet-…` ids map cleanly.
        for part in rest.split('-') {
            let part_lower = part.to_ascii_lowercase();
            if CLAUDE_FAMILIES.iter().any(|family| *family == part_lower) {
                return Some(part_lower);
            }
        }
    }
    Some(raw.to_string())
}

/// Model declared on a Claude-native (or Grok Claude-compatible) transcript
/// line: `message.model` on assistant records. Ignores other line types.
pub(crate) fn model_from_claude_native_transcript_line(line: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    let is_assistant = value.get("type").and_then(Value::as_str) == Some("assistant")
        || value
            .get("message")
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            == Some("assistant");
    if !is_assistant {
        return None;
    }
    value
        .get("message")
        .and_then(|message| string_field(message, "model"))
        .or_else(|| string_field(&value, "model"))
}

/// Model declared on a Codex-style rollout line (`turn_context`,
/// `session_meta`, or `world_state` payload).
pub(crate) fn model_from_codex_transcript_line(line: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    let payload = value.get("payload")?;
    match value.get("type").and_then(Value::as_str)? {
        "turn_context" | "session_meta" => string_field(payload, "model"),
        "world_state" => payload
            .get("state")
            .and_then(|state| string_field(state, "model")),
        _ => None,
    }
}

/// Model from shell CLI args (`--model`), already normalized. Used when minting
/// a shell agent so the first `agent.spawned` event can carry it.
pub(crate) fn shell_cli_model(args: &[String]) -> Option<String> {
    cli_flag_value(args, "--model").and_then(|model| normalize_agent_model(&model))
}

/// Apply an explicit `--model` from shell args onto a resolved agent when it
/// differs from the stored value (covers prepared/resumed agents that did not
/// receive the model at mint time).
pub(crate) fn apply_shell_cli_model(
    state: &AppState,
    agent: AgentInfo,
    args: &[String],
) -> Result<AgentInfo, String> {
    let Some(model) = shell_cli_model(args) else {
        return Ok(agent);
    };
    if agent.model.as_deref() == Some(model.as_str()) {
        return Ok(agent);
    }
    state
        .mutate_agent(&agent.id, |agent| {
            agent.model = Some(model);
        })?
        .ok_or_else(|| {
            format!(
                "agent {} disappeared while recording shell launch model",
                agent.id
            )
        })
}

/// Record a model discovered from a tailed transcript line. Updates the agent
/// only when the normalized value differs, and emits `agent.updated` so the UI
/// can show it without a full agent-list refetch.
pub(crate) fn maybe_record_agent_model(
    state: &AppState,
    agent_id: &str,
    raw_model: &str,
) -> Result<Option<AgentInfo>, String> {
    let Some(model) = normalize_agent_model(raw_model) else {
        return Ok(None);
    };
    let Some(current) = state.agent(agent_id)? else {
        return Ok(None);
    };
    if current.model.as_deref() == Some(model.as_str()) {
        return Ok(None);
    }
    let updated = state.mutate_agent(agent_id, |agent| {
        agent.model = Some(model);
    })?;
    if let Some(agent) = updated.as_ref() {
        state.emit(QmuxEvent::new(
            "agent.updated",
            agent.pane_id.clone(),
            Some(agent.id.clone()),
            json!({ "agent": agent }),
        ));
    }
    Ok(updated)
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
            && same_dir(&candidate.worktree_dir, cwd)
            && native_session_selector_matches(candidate, fork_point, cwd)
    });
    let updated = state
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
        })?;
    if let Some(source) = source {
        match state.capture_conversation_history(&source, None) {
            Ok(Some(history)) => {
                if let Err(err) = state.record_conversation_history(&updated, history) {
                    eprintln!(
                        "qmux: could not record conversation history for shell fork {}: {err}",
                        updated.id
                    );
                }
            }
            Ok(None) => {}
            Err(err) => eprintln!(
                "qmux: could not capture conversation history for shell fork {}: {err}",
                updated.id
            ),
        }
    }
    Ok(updated)
}

/// True when both paths name the same directory. Canonicalization resolves symlinks,
/// `.`/`..`, trailing slashes, and (on case-insensitive volumes) the on-disk case, so a
/// shell's reported `$PWD` rebinds the original agent even when its spelling differs from
/// the recorded launch dir. Falls back to a raw compare when a side can't be canonicalized
/// (e.g. the directory no longer exists), preserving the previous exact-match behavior.
pub(crate) fn same_dir(a: &str, b: &str) -> bool {
    same_path(a, b)
}

fn same_path(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Native CLIs may accept either a session id or a transcript path for resume
/// and fork selectors. Match both forms so qmux keeps one record and preserves
/// lineage regardless of which spelling the user gives the CLI.
fn native_session_selector_matches(agent: &AgentInfo, selector: &str, cwd: &str) -> bool {
    if agent.session_id.as_deref() == Some(selector) {
        return true;
    }
    let Some(transcript_path) = agent.transcript_path.as_deref() else {
        return false;
    };
    let selector_path = Path::new(selector);
    let resolved = if selector_path.is_absolute() {
        selector_path.to_path_buf()
    } else {
        Path::new(cwd).join(selector_path)
    };
    same_path(transcript_path, &resolved.display().to_string())
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
        context_status: None,
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
    /// Qmux lineage to persist before the new process is allowed to start.
    /// User launches omit it; orchestration surfaces set it to their caller.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Provider-native conversation selected by the backend history scanner.
    #[serde(default)]
    pub resume_session_id: Option<String>,
    /// Branch `resume_session_id` rather than continuing it in place.
    #[serde(default)]
    pub fork_session: bool,
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
    /// Whether `qmux agent-exec` should bind and supervise this process as an
    /// agent. Adapters can return `false` for utility invocations of a shared
    /// CLI (for example `pi install`) that must pass through the shell wrapper
    /// without creating an agent.
    pub supervised: bool,
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
    /// A native transcript record that the turn ended successfully. Used when
    /// the adapter's Stop/idle hook is missing, never fires, or fires too
    /// early (Cursor's `--plugin-dir` observer is skipped at the `stop` call
    /// site; Codex emits `Stop` between review jobs and internally queued
    /// prompts, then continues the same turn).
    TurnCompleted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceObservation {
    pub cwd: String,
    pub source: ActiveWorkspaceSource,
    pub session_id: Option<String>,
    pub observed_at_millis: Option<u128>,
}

impl TranscriptLifecycleEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            TranscriptLifecycleEvent::Interrupted => "interrupted",
            TranscriptLifecycleEvent::TurnStarted => "turnStarted",
            TranscriptLifecycleEvent::TurnCompleted => "turnCompleted",
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
    /// The configured executable name or path used by every launch and probe.
    fn configured_binary(&self) -> &str;

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

    /// Gives an adapter the first chance to classify a wrapped shell command
    /// as a utility invocation rather than an interactive agent. A returned
    /// launch must set `supervised` to false and must not mutate workspace or
    /// pane state. The default keeps every invocation on the normal supervised
    /// path, preserving existing adapters' behavior.
    fn prepare_shell_passthrough(
        &self,
        _request: &PrepareShellAgentLaunchRequest,
    ) -> Result<Option<PreparedShellAgentLaunch>, String> {
        Ok(None)
    }

    fn shell_commands(&self) -> Vec<ShellCommandIntegration>;

    /// Whether this adapter can launch into a remote group.
    ///
    /// Defaults to `false`, and deliberately so. A remote launch is not just an
    /// ssh wrapper: an adapter that resolves its binary against the local
    /// `PATH`, points a flag at a locally-materialized plugin directory, or
    /// relies on the pane's cwd being its worktree will start successfully over
    /// there and then be wrong in ways that look like the agent misbehaving.
    /// Opting in means an adapter has been checked for all three.
    fn supports_remote(&self) -> bool {
        false
    }

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

    /// Resolves a transcript with adapter-owned session-tree state. Linear
    /// adapters ignore the leaf and retain their existing resolver; tree-shaped
    /// adapters such as Pi override this to select the native active ancestry.
    fn resolve_transcript_turns_at_leaf(
        &self,
        agent_id: &str,
        source_index_offset: usize,
        lines: &[String],
        _native_leaf_id: Option<&str>,
    ) -> Vec<Turn> {
        self.resolve_transcript_turns(agent_id, source_index_offset, lines)
    }

    fn transcript_line_can_update_turn_status(&self, _line: &str) -> bool {
        false
    }

    fn parse_transcript_lifecycle_event(&self, _line: &str) -> Option<TranscriptLifecycleEvent> {
        None
    }

    /// Best-effort model id carried by a single transcript line, when the
    /// adapter's native format records one (e.g. Claude `message.model` on
    /// assistant turns). The transcript tailer uses this to fill in
    /// `AgentInfo.model` for bare shell launches that never passed `--model`.
    fn transcript_line_model(&self, _line: &str) -> Option<String> {
        None
    }

    /// Display-only command cwd carried by a native transcript record. Only
    /// adapters whose formats expose an authoritative cwd opt in; lifecycle
    /// behavior continues to use the agent's qmux-owned launch workspace.
    fn transcript_workspace_observation(&self, _line: &str) -> Option<WorkspaceObservation> {
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
        _target_cwd: &Path,
    ) -> Result<String, String> {
        Err(FORK_UNSUPPORTED_ERROR.to_string())
    }

    fn supports_fork(&self) -> bool {
        false
    }

    /// Whether this adapter has a supported research runtime. This is
    /// deliberately narrower than general terminal-session fork support.
    fn supports_research(&self) -> bool {
        false
    }

    fn supports_fork_at_message(&self) -> bool {
        false
    }

    fn shell_fork_args(
        &self,
        _source: &AgentInfo,
        _cwd: &Path,
        _prompt: Option<&str>,
    ) -> Result<Vec<String>, String> {
        Err(FORK_UNSUPPORTED_ERROR.to_string())
    }

    fn shell_fork_at_message_args(
        &self,
        _source: &AgentInfo,
        _seed_session_id: &str,
        _prompt: Option<&str>,
    ) -> Result<Vec<String>, String> {
        Err(FORK_AT_MESSAGE_UNSUPPORTED_ERROR.to_string())
    }

    fn fork_pane(
        &self,
        _state: &AppState,
        _source: &AgentInfo,
        _use_worktree: bool,
        _prompt: Option<&str>,
    ) -> Result<(PaneInfo, AgentInfo), String> {
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
            .map(|adapter| {
                let configured_binary = adapter.configured_binary().to_string();
                let resolved_binary =
                    ensure_on_path(&configured_binary).map(|path| path.display().to_string());
                let installed = resolved_binary.is_some();
                let login_command = adapter_login_command(adapter.id(), &configured_binary);
                AdapterMetadata {
                    id: adapter.id().to_string(),
                    label: adapter.display_name().to_string(),
                    default: adapter.id() == "claude",
                    supports_fork: adapter.supports_fork(),
                    supports_research: adapter.supports_research(),
                    supports_fork_at_message: adapter.supports_fork_at_message(),
                    supports_remote: adapter.supports_remote(),
                    configured_binary,
                    resolved_binary,
                    readiness: if installed {
                        AdapterReadiness::Ready
                    } else {
                        AdapterReadiness::Missing
                    },
                    message: (!installed).then(|| {
                        format!(
                            "{} was not found. Install it or configure its binary path.",
                            adapter.display_name()
                        )
                    }),
                    research_readiness: if installed {
                        AdapterReadiness::Ready
                    } else {
                        AdapterReadiness::Missing
                    },
                    version: None,
                    auth: AdapterAuthState::Unknown,
                    checked_at: None,
                    login_command,
                    install_url: adapter_install_url(adapter.id()).map(str::to_string),
                    update_command: adapter_update_command(
                        adapter.id(),
                        adapter.configured_binary(),
                    ),
                    instance_id: format!("local:{}", adapter.id()),
                    target: AdapterTargetMetadata::local(),
                }
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AdapterReadiness {
    Ready,
    Missing,
    NeedsAuth,
    UnsupportedVersion,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AdapterAuthState {
    Authenticated,
    Unauthenticated,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterMetadata {
    pub id: String,
    pub label: String,
    pub default: bool,
    /// Whether the adapter has a native fork command for terminal-session
    /// branching.
    pub supports_fork: bool,
    /// Whether the adapter can run and branch through the research harness.
    pub supports_research: bool,
    /// Whether the adapter can fork from a chosen message rather than the
    /// session head. Gates the transcript's per-message fork action, which is
    /// hidden rather than disabled for adapters without it.
    pub supports_fork_at_message: bool,
    pub supports_remote: bool,
    /// Executable name/path from qmux.config.json after home expansion.
    pub configured_binary: String,
    /// Resolved executable used for launch, absent when it cannot be found.
    pub resolved_binary: Option<String>,
    pub readiness: AdapterReadiness,
    pub message: Option<String>,
    /// Headless Research can have stricter version/auth requirements than an
    /// interactive terminal session.
    pub research_readiness: AdapterReadiness,
    pub version: Option<String>,
    pub auth: AdapterAuthState,
    pub checked_at: Option<u64>,
    pub login_command: Option<String>,
    pub install_url: Option<String>,
    pub update_command: Option<String>,
    /// Stable provider-instance identity. The first implementation has one
    /// configured instance per adapter and target; the contract does not need
    /// to change when account/home profiles are added later.
    pub instance_id: String,
    pub target: AdapterTargetMetadata,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterTargetMetadata {
    pub kind: String,
    pub id: Option<String>,
    pub label: String,
}

impl AdapterTargetMetadata {
    fn local() -> Self {
        Self {
            kind: "local".to_string(),
            id: None,
            label: "This Mac".to_string(),
        }
    }
}

struct ProbeOutput {
    success: bool,
    text: String,
}

fn adapter_version_args(adapter_id: &str) -> &'static [&'static str] {
    match adapter_id {
        "claude" => &["-v"],
        _ => &["--version"],
    }
}

fn adapter_auth_args(adapter_id: &str) -> Option<&'static [&'static str]> {
    match adapter_id {
        "claude" => Some(&["auth", "status", "--json"]),
        "codex" => Some(&["login", "status"]),
        "grok" => Some(&["auth", "status"]),
        _ => None,
    }
}

fn adapter_login_command(adapter_id: &str, binary: &str) -> Option<String> {
    let binary = shell_quote_arg(binary);
    match adapter_id {
        "claude" => Some(format!("{binary} auth login")),
        "codex" | "grok" => Some(format!("{binary} login")),
        "opencode" => Some(format!("{binary} auth login")),
        "cursor" => Some(format!("{binary} login")),
        "devin" => Some(format!("{binary} auth login")),
        "antigravity" => Some(binary),
        _ => None,
    }
}

fn adapter_install_url(adapter_id: &str) -> Option<&'static str> {
    match adapter_id {
        "claude" => Some("https://docs.anthropic.com/en/docs/claude-code/setup"),
        "codex" => Some("https://developers.openai.com/codex/cli"),
        "opencode" => Some("https://opencode.ai/docs/"),
        "grok" => Some("https://docs.x.ai/docs/grok-code-fast-1"),
        "pi" => Some("https://github.com/badlogic/pi-mono"),
        "cursor" => Some("https://cursor.com/docs/cli/overview"),
        "devin" => Some("https://docs.devin.ai/work-with-devin/devin-cli"),
        "antigravity" => Some("https://antigravity.google/docs/cli/reference"),
        _ => None,
    }
}

fn adapter_update_command(adapter_id: &str, binary: &str) -> Option<String> {
    let binary = shell_quote_arg(binary);
    match adapter_id {
        "claude" => Some(format!("{binary} update")),
        "codex" => Some("npm install -g @openai/codex@latest".to_string()),
        "opencode" => Some(format!("{binary} upgrade")),
        "pi" => Some("npm install -g @mariozechner/pi-coding-agent@latest".to_string()),
        _ => None,
    }
}

fn run_adapter_probe(binary: &str, args: &[&str]) -> Result<ProbeOutput, String> {
    let command = format!("{binary} {}", args.join(" "));
    let mut process = Command::new(binary);
    process.args(args);
    run_adapter_probe_command(process, &command)
}

fn run_remote_adapter_probe(
    host: &crate::host::Host,
    binary: &str,
    args: &[&str],
) -> Result<ProbeOutput, String> {
    let command = format!("{binary} {} on {}", args.join(" "), host.label());
    let process = host.command(crate::host::RemoteCommand {
        program: binary,
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        ..Default::default()
    });
    run_adapter_probe_command(process, &command)
}

fn run_remote_command_presence_probe(
    host: &crate::host::Host,
    program: &str,
) -> Result<ProbeOutput, String> {
    let command = format!("find {program} on {}", host.label());
    let process = host.command(crate::host::RemoteCommand {
        program: "sh",
        args: vec![
            "-c".to_string(),
            "command -v \"$1\" >/dev/null 2>&1".to_string(),
            "qmux-remote-probe".to_string(),
            program.to_string(),
        ],
        ..Default::default()
    });
    run_adapter_probe_command(process, &command)
}

fn run_adapter_probe_command(mut process: Command, command: &str) -> Result<ProbeOutput, String> {
    let mut child = process
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to run `{command}`: {err}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("`{command}` stdout was not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("`{command}` stderr was not piped"))?;
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.take(64 * 1024).read_to_end(&mut bytes);
        bytes
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.take(64 * 1024).read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("`{command}` timed out"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("failed to wait for `{command}`: {err}"));
            }
        }
    };
    let mut bytes = stdout_reader.join().unwrap_or_default();
    bytes.extend(stderr_reader.join().unwrap_or_default());
    Ok(ProbeOutput {
        success: status.success(),
        text: String::from_utf8_lossy(&bytes).trim().to_string(),
    })
}

fn first_probe_line(output: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| {
            line.chars()
                .filter(|character| !character.is_control())
                .take(160)
                .collect()
        })
}

fn classify_auth(adapter_id: &str, output: &ProbeOutput) -> AdapterAuthState {
    if adapter_id == "claude"
        && let Ok(value) = serde_json::from_str::<Value>(&output.text)
        && let Some(authenticated) = value
            .get("loggedIn")
            .or_else(|| value.get("authenticated"))
            .and_then(Value::as_bool)
    {
        return if authenticated {
            AdapterAuthState::Authenticated
        } else {
            AdapterAuthState::Unauthenticated
        };
    }
    let text = output.text.to_ascii_lowercase();
    if text.contains("not logged in")
        || text.contains("not authenticated")
        || text.contains("unauthenticated")
        || text.contains("login required")
        || text.contains("no credentials")
    {
        return AdapterAuthState::Unauthenticated;
    }
    if output.success
        && (text.contains("logged in")
            || text.contains("authenticated")
            || text.contains("authentication: valid"))
    {
        return AdapterAuthState::Authenticated;
    }
    AdapterAuthState::Unknown
}

fn probe_adapter_metadata(mut metadata: AdapterMetadata) -> AdapterMetadata {
    metadata.checked_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok());
    let Some(binary) = metadata.resolved_binary.clone() else {
        return metadata;
    };
    let version_output = match run_adapter_probe(&binary, adapter_version_args(&metadata.id)) {
        Ok(output) if output.success => output,
        Ok(output) => {
            metadata.readiness = AdapterReadiness::Error;
            metadata.research_readiness = AdapterReadiness::Error;
            metadata.message = Some(
                first_probe_line(&output.text)
                    .unwrap_or_else(|| format!("{} version check failed", metadata.label)),
            );
            return metadata;
        }
        Err(err) => {
            metadata.readiness = AdapterReadiness::Error;
            metadata.research_readiness = AdapterReadiness::Error;
            metadata.message = Some(err);
            return metadata;
        }
    };
    metadata.version = first_probe_line(&version_output.text);
    metadata.message = None;

    if metadata.id == "claude"
        && let Some(version) = crate::claude_sdk::parse_claude_version(&version_output.text)
        && !version.meets_floor()
    {
        metadata.research_readiness = AdapterReadiness::UnsupportedVersion;
        metadata.message = Some(format!(
            "Claude Code {} can open a terminal, but Research requires 2.1.0 or newer.",
            version.display()
        ));
    }

    if let Some(args) = adapter_auth_args(&metadata.id)
        && let Ok(output) = run_adapter_probe(&binary, args)
    {
        metadata.auth = classify_auth(&metadata.id, &output);
        if metadata.auth == AdapterAuthState::Unauthenticated {
            metadata.readiness = AdapterReadiness::NeedsAuth;
            metadata.research_readiness = AdapterReadiness::NeedsAuth;
            metadata.message = metadata.login_command.as_ref().map(|command| {
                format!(
                    "{} is not signed in. Run `{command}` and check again.",
                    metadata.label
                )
            });
        }
    }
    metadata
}

fn probe_remote_adapter_metadata(
    mut metadata: AdapterMetadata,
    host: &crate::host::Host,
) -> AdapterMetadata {
    metadata.checked_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok());
    let binary = metadata.configured_binary.trim().to_string();
    if binary.is_empty() {
        metadata.resolved_binary = None;
        metadata.readiness = AdapterReadiness::Missing;
        metadata.research_readiness = AdapterReadiness::Error;
        metadata.message = Some(format!(
            "{} has no binary configured for {}.",
            metadata.label,
            host.label()
        ));
        return metadata;
    }
    metadata.resolved_binary = Some(binary.clone());
    metadata.readiness = AdapterReadiness::Ready;
    // This increment supports interactive remote terminals. Research still
    // launches through a local SDK/transcript path and must not be advertised
    // merely because the remote CLI itself is installed.
    metadata.research_readiness = AdapterReadiness::Error;
    metadata.message = Some("Remote terminal ready; Research remains local-only.".to_string());

    let version_output =
        match run_remote_adapter_probe(host, &binary, adapter_version_args(&metadata.id)) {
            Ok(output) if output.success => output,
            Ok(output) => {
                metadata.readiness = AdapterReadiness::Error;
                metadata.message = Some(first_probe_line(&output.text).unwrap_or_else(|| {
                    format!(
                        "{} version check failed on {}",
                        metadata.label,
                        host.label()
                    )
                }));
                return metadata;
            }
            Err(err) => {
                metadata.readiness = AdapterReadiness::Error;
                metadata.message = Some(err);
                return metadata;
            }
        };
    metadata.version = first_probe_line(&version_output.text);

    if let Some(args) = adapter_auth_args(&metadata.id)
        && let Ok(output) = run_remote_adapter_probe(host, &binary, args)
    {
        metadata.auth = classify_auth(&metadata.id, &output);
        if metadata.auth == AdapterAuthState::Unauthenticated {
            metadata.readiness = AdapterReadiness::NeedsAuth;
            metadata.message = metadata.login_command.as_ref().map(|command| {
                format!(
                    "{} is not signed in on {}. Run `{command}` in a remote shell and check again.",
                    metadata.label,
                    host.label()
                )
            });
        }
    }
    metadata
}

/// Runs provider probes concurrently so one slow CLI consumes only its own
/// timeout rather than serially delaying every status card and launcher.
struct AdapterProbeCacheEntry {
    stored_at: Instant,
    metadata: Vec<AdapterMetadata>,
}

static ADAPTER_PROBE_CACHE: OnceLock<Mutex<HashMap<String, AdapterProbeCacheEntry>>> =
    OnceLock::new();
const ADAPTER_PROBE_CACHE_TTL: Duration = Duration::from_secs(30);

fn adapter_probe_cache() -> &'static Mutex<HashMap<String, AdapterProbeCacheEntry>> {
    ADAPTER_PROBE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn adapter_probe_cache_key(metadata: &[AdapterMetadata], remote_id: Option<&str>) -> String {
    let target = remote_id.unwrap_or("local");
    format!(
        "{target}\n{}",
        metadata
            .iter()
            .map(|adapter| format!("{}={}", adapter.id, adapter.configured_binary))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn cached_adapter_metadata(key: &str) -> Option<Vec<AdapterMetadata>> {
    let cache = adapter_probe_cache().lock().ok()?;
    let entry = cache.get(key)?;
    (entry.stored_at.elapsed() <= ADAPTER_PROBE_CACHE_TTL).then(|| entry.metadata.clone())
}

pub fn probe_adapter_metadata_for_config(
    config: &QmuxConfig,
    remote_target: Option<&crate::workspace::RemoteRef>,
    force: bool,
) -> Result<Vec<AdapterMetadata>, String> {
    let mut base = adapter_registry(config).metadata();
    let remote_cache_target = remote_target.map(|remote| format!("{}@{}", remote.id, remote.host));
    let key = adapter_probe_cache_key(&base, remote_cache_target.as_deref());
    if !force && let Some(metadata) = cached_adapter_metadata(&key) {
        return Ok(metadata);
    }

    let metadata: Vec<AdapterMetadata> = if let Some(remote) = remote_target {
        let remote_id = remote.id.as_str();
        let remote_label = remote.label.as_str();
        let checked_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_millis()).ok());
        for adapter in &mut base {
            adapter.target = AdapterTargetMetadata {
                kind: "remote".to_string(),
                id: Some(remote_id.to_string()),
                label: remote_label.to_string(),
            };
            adapter.instance_id = format!("remote:{remote_id}:{}", adapter.id);
            adapter.checked_at = checked_at;
            if !adapter.supports_remote {
                adapter.readiness = AdapterReadiness::Error;
                adapter.research_readiness = AdapterReadiness::Error;
                adapter.message = Some(format!(
                    "{} cannot run on remote '{}' yet; its integration currently requires local paths and hooks.",
                    adapter.label, remote_label
                ));
            } else {
                // Establish a remote-safe fallback before the probe thread is
                // spawned. If that worker panics, never leak the registry's
                // local PATH/auth result into a remote launcher.
                adapter.resolved_binary = None;
                adapter.readiness = AdapterReadiness::Error;
                adapter.research_readiness = AdapterReadiness::Error;
                adapter.message = Some(format!(
                    "Could not verify {} on remote '{}'.",
                    adapter.label, remote_label
                ));
            }
        }
        // Probe the group's snapshotted binding, not the current saved-remote
        // entry. Editing or removing a config entry must never silently move or
        // disable an existing workspace that still points at its original host.
        let host = crate::host::for_group(Some(remote));
        let qmux_cli_error = host
            .remote()
            .map(|target| target.qmux_cli.as_str())
            .and_then(|qmux_cli| match run_remote_command_presence_probe(&host, qmux_cli) {
                Ok(output) if output.success => None,
                Ok(_) => Some(format!(
                    "Remote '{}' does not provide the configured qmuxCli '{}'; install qmux-cli there or update the remote configuration.",
                    remote_label, qmux_cli
                )),
                Err(err) => Some(err),
            });
        let handles = base
            .into_iter()
            .map(|metadata| {
                if !metadata.supports_remote {
                    return (metadata, None);
                }
                if let Some(err) = qmux_cli_error.as_ref() {
                    let mut metadata = metadata;
                    metadata.message = Some(err.clone());
                    return (metadata, None);
                }
                let fallback = metadata.clone();
                let host = host.clone();
                let handle =
                    std::thread::spawn(move || probe_remote_adapter_metadata(metadata, &host));
                (fallback, Some(handle))
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|(fallback, handle)| {
                handle
                    .and_then(|handle| handle.join().ok())
                    .unwrap_or(fallback)
            })
            .collect()
    } else {
        let handles = base
            .into_iter()
            .map(|metadata| {
                let fallback = metadata.clone();
                let handle = std::thread::spawn(move || probe_adapter_metadata(metadata));
                (fallback, handle)
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|(fallback, handle)| handle.join().unwrap_or(fallback))
            .collect()
    };
    if let Ok(mut cache) = adapter_probe_cache().lock() {
        cache.insert(
            key,
            AdapterProbeCacheEntry {
                stored_at: Instant::now(),
                metadata: metadata.clone(),
            },
        );
    }
    Ok(metadata)
}

pub fn ensure_adapter_ready_for_research(
    config: &QmuxConfig,
    adapter_id: &str,
) -> Result<AdapterMetadata, String> {
    let metadata = probe_adapter_metadata_for_config(config, None, false)?
        .into_iter()
        .find(|metadata| metadata.id == adapter_id)
        .ok_or_else(|| format!("unknown agent adapter '{adapter_id}'"))?;
    if metadata.research_readiness == AdapterReadiness::Ready {
        Ok(metadata)
    } else {
        Err(metadata
            .message
            .clone()
            .unwrap_or_else(|| format!("{} is not ready to run Research.", metadata.label)))
    }
}

pub fn adapter_registry(config: &QmuxConfig) -> AdapterRegistry {
    AdapterRegistry::new(vec![
        Box::new(ClaudeAdapter::new(config)),
        Box::new(CodexAdapter::new(config)),
        Box::new(OpencodeAdapter::new(config)),
        Box::new(GrokAdapter::new(config)),
        Box::new(MuseAdapter::new(config)),
        Box::new(PiAdapter::new(config)),
        Box::new(CursorAdapter::new(config)),
        Box::new(DevinAdapter::new(config)),
        Box::new(AntigravityAdapter::new(config)),
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

/// Forks the agent running in `authed_pane` into a new tab immediately after it and
/// resumes its session. The source is resolved from the
/// authenticated pane (never caller input), so a pane can only fork its own session.
/// When `prompt` is set, the fork is launched with that initial user message.
pub fn agent_fork(
    state: &AppState,
    authed_pane: &str,
    use_worktree: bool,
    prompt: Option<String>,
    anchor: Option<MessageAnchor>,
    worktree_name: Option<String>,
) -> Result<PaneInfo, String> {
    let source = state
        .agent_by_pane(authed_pane)?
        .ok_or_else(|| "no agent is running in this pane to fork".to_string())?;
    fork_agent_in_shell(
        state,
        &source,
        use_worktree,
        prompt.as_deref(),
        anchor.as_ref(),
        worktree_name.as_deref(),
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
    prompt: Option<&str>,
    anchor: Option<&MessageAnchor>,
    worktree_name: Option<&str>,
) -> Result<PaneInfo, String> {
    if !use_worktree && worktree_name.is_some() {
        return Err("a worktree name requires a worktree fork".to_string());
    }
    let registry = adapter_registry(state.config());
    let adapter = registry
        .get(&source.adapter)
        .map_err(|_| FORK_UNSUPPORTED_ERROR.to_string())?;
    if !adapter.supports_fork() {
        return Err(FORK_UNSUPPORTED_ERROR.to_string());
    }
    if anchor.is_some() && !adapter.supports_fork_at_message() {
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
    let conversation_history = match state.capture_conversation_history(source, anchor) {
        Ok(history) => history,
        Err(err) => {
            eprintln!(
                "qmux: could not capture conversation history for fork of {}: {err}",
                source.id
            );
            None
        }
    };
    let workspace_request = PrepareAgentWorkspaceRequest {
        group_id: Some(source.group_id.clone()),
        base_repo: if use_worktree && worktree_name.is_none() {
            None
        } else {
            Some(source.worktree_dir.clone())
        },
        base_ref: Some("HEAD".to_string()),
        adapter: source.adapter.clone(),
        model: source.model.clone(),
        effort: source.effort.clone(),
        use_worktree,
    };
    let mut agent = match worktree_name {
        Some(name) => prepare_named_agent_workspace_with_parent(
            state,
            workspace_request,
            Some(&source.id),
            name,
        )?,
        None => prepare_agent_workspace_with_parent(state, workspace_request, Some(&source.id))?,
    };
    agent.fork_point = Some(session_id.clone());
    agent.root_session_id = source
        .root_session_id
        .clone()
        .or_else(|| Some(session_id.clone()));
    agent.status = AgentStatus::Idle;
    state.update_agent(agent.clone())?;
    if let Some(history) = conversation_history
        && let Err(err) = state.record_conversation_history(&agent, history)
    {
        eprintln!(
            "qmux: could not record conversation history for fork {}: {err}",
            agent.id
        );
    }

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
        (_, Some(anchor)) => {
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
            let seed_session_id =
                adapter.synthesize_truncated_session(Path::new(transcript_path), anchor, &cwd)?;
            adapter.shell_fork_at_message_args(source, &seed_session_id, prompt)?
        }
        (_, None) => adapter.shell_fork_args(source, &cwd, prompt)?,
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
    finish_fork_spawn(state, source, pane, agent)
}

/// Adapters with a native fork command. Owns the fork-eligibility check (and its
/// error message) for both the dispatch below and the queue engine's fail-fast
/// validation, so a new forkable adapter is added in one place.
pub fn adapter_supports_fork(config: &QmuxConfig, adapter_id: &str) -> bool {
    adapter_registry(config)
        .get(adapter_id)
        .is_ok_and(|adapter| adapter.supports_fork())
}

pub fn adapter_supports_research(config: &QmuxConfig, adapter_id: &str) -> bool {
    adapter_registry(config)
        .get(adapter_id)
        .is_ok_and(|adapter| adapter.supports_research())
}

/// Adapters that can fork from a chosen message rather than the session head.
/// Neither CLI has a flag for this — `claude --fork-session` and `codex fork`
/// both branch at the head — so the adapter synthesizes a truncated copy of the
/// native transcript and resumes that instead. Only adapters whose transcript
/// format we can safely truncate qualify; the rest inherit the trait's default
/// `Err` and are filtered out of the UI by `supports_fork_at_message`.
#[cfg(test)]
pub fn adapter_supports_fork_at_message(config: &QmuxConfig, adapter_id: &str) -> bool {
    adapter_registry(config)
        .get(adapter_id)
        .is_ok_and(|adapter| adapter.supports_fork_at_message())
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
    let base = adapter_registry(config).metadata();
    let key = adapter_probe_cache_key(&base, None);
    let metadata = cached_adapter_metadata(&key).unwrap_or(base);
    metadata
        .iter()
        .find(|adapter| {
            adapter.default
                && adapter.supports_research
                && adapter.research_readiness == AdapterReadiness::Ready
        })
        .or_else(|| {
            metadata.iter().find(|adapter| {
                adapter.supports_research && adapter.research_readiness == AdapterReadiness::Ready
            })
        })
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
    prompt: Option<&str>,
) -> Result<PaneInfo, String> {
    fork_agent_source_with_placement(state, source, use_worktree, prompt)
}

fn fork_agent_source_with_placement(
    state: &AppState,
    source: &AgentInfo,
    use_worktree: bool,
    prompt: Option<&str>,
) -> Result<PaneInfo, String> {
    let conversation_history = match state.capture_conversation_history(source, None) {
        Ok(history) => history,
        Err(err) => {
            eprintln!(
                "qmux: could not capture conversation history for fork of {}: {err}",
                source.id
            );
            None
        }
    };
    let (pane, agent) = adapter_registry(state.config())
        .get(&source.adapter)
        .map_err(|_| FORK_UNSUPPORTED_ERROR.to_string())?
        .fork_pane(state, source, use_worktree, prompt)?;
    if let Some(history) = conversation_history
        && let Err(err) = state.record_conversation_history(&agent, history)
    {
        eprintln!(
            "qmux: could not record conversation history for fork {}: {err}",
            agent.id
        );
    }
    finish_fork_spawn(state, source, pane, agent)
}

fn finish_fork_spawn(
    state: &AppState,
    source: &AgentInfo,
    pane: PaneInfo,
    agent: AgentInfo,
) -> Result<PaneInfo, String> {
    if let Some(source_pane) = source.pane_id.as_deref() {
        // Placement is cosmetic and the fork has already spawned. The source
        // pane can legitimately vanish between the fork and this point —
        // research retirement closes a completed parent pane the moment its
        // node completes, exactly when follow-ups become possible — and
        // propagating the placement error would report failure for a live
        // pane+agent the caller then can neither see nor clean up. Leave the
        // new pane at the end of the order instead.
        let placed = state.place_pane_after(&pane.id, source_pane);
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
        json!({
            "agent": agent,
            "pane": pane,
            "sourceAgentId": source.id,
            "sourcePaneId": source.pane_id,
        }),
    ));
    Ok(pane)
}

/// Starts a fresh session of `source`'s adapter in the source's own directory,
/// launched with `prompt` as its first message, and places the new pane after the
/// source. Used by the queue engine for new-session-delivery turns. Emits
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
                parent_id: Some(source.id.clone()),
                resume_session_id: None,
                fork_session: false,
            },
        )?;
    if let Some(source_pane) = source.pane_id.as_deref() {
        // Best-effort, like fork placement above: the session has already
        // spawned, and a source pane closed in the meantime must not turn a
        // live pane into a reported failure.
        if let Err(err) = state.place_pane_after(&pane.id, source_pane) {
            eprintln!(
                "qmux: sibling session for agent {} spawned but could not be placed after pane {source_pane}: {err}",
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
    let registry = adapter_registry(state.config());
    let adapter = registry.get(&request.adapter_id)?;
    if let Some(passthrough) = adapter.prepare_shell_passthrough(&request)? {
        if passthrough.supervised {
            return Err("adapter shell passthrough was incorrectly marked supervised".to_string());
        }
        return Ok(passthrough);
    }
    let prepared = match adapter.prepare_shell_launch(state, request) {
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
        MuseAdapterConfig, OpencodeAdapterConfig,
    };
    use std::path::PathBuf;

    fn test_config() -> QmuxConfig {
        QmuxConfig {
            remotes: Default::default(),
            workspace_root: PathBuf::from("/tmp/qmux-adapter-tests"),
            socket_path: PathBuf::from("/tmp/qmux-adapter-tests.sock"),
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
            claude_plugin_dir: PathBuf::new(),
            opencode_plugin_dir: PathBuf::new(),
            pi_extension_dir: PathBuf::new(),
            cursor_plugin_dir: PathBuf::new(),
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
        assert_eq!(metadata.len(), 9);
        assert_eq!(metadata[0].id, "claude");
        assert!(metadata[0].default);
        assert_eq!(metadata[1].id, "codex");
        assert!(!metadata[1].default);
        assert_eq!(metadata[2].id, "opencode");
        assert!(!metadata[2].default);
        assert_eq!(metadata[3].id, "grok");
        assert!(!metadata[3].default);
        assert_eq!(metadata[4].id, "muse");
        assert!(!metadata[4].default);
        assert_eq!(metadata[5].id, "pi");
        assert!(!metadata[5].default);
        assert_eq!(metadata[6].id, "cursor");
        assert!(!metadata[6].default);
        assert_eq!(metadata[7].id, "devin");
        assert!(!metadata[7].default);
        assert_eq!(metadata[8].id, "antigravity");
        assert!(!metadata[8].default);
        assert_eq!(metadata[8].login_command.as_deref(), Some("'agy'"));
        assert!(
            metadata
                .iter()
                .find(|adapter| adapter.id == "claude")
                .is_some_and(|adapter| adapter.supports_remote)
        );
        assert!(
            metadata
                .iter()
                .filter(|adapter| !matches!(adapter.id.as_str(), "claude" | "codex"))
                .all(|adapter| !adapter.supports_remote)
        );
        assert!(
            metadata
                .iter()
                .filter(|adapter| matches!(adapter.id.as_str(), "claude" | "codex"))
                .all(|adapter| adapter.supports_remote)
        );
        assert!(
            metadata
                .iter()
                .all(|adapter| adapter.target.kind == "local")
        );
        let config = test_config();
        assert!(!adapter_supports_fork(&config, "cursor"));
        assert!(!adapter_supports_fork_at_message(&config, "cursor"));
        assert!(adapter_supports_fork(&config, "grok"));
        assert!(adapter_supports_fork(&config, "opencode"));
        assert!(adapter_supports_fork(&config, "pi"));
        assert!(adapter_supports_research(&config, "claude"));
        assert!(adapter_supports_research(&config, "codex"));
        assert!(adapter_supports_research(&config, "grok"));
        assert!(!adapter_supports_research(&config, "opencode"));
        assert!(!adapter_supports_research(&config, "pi"));
        assert!(!adapter_supports_research(&config, "antigravity"));
        assert!(adapter_supports_fork_at_message(&config, "pi"));
        // Muse has no fork command either — no `--fork-session` flag and no
        // `fork` subcommand — so branching a session is not offered.
        assert!(!adapter_supports_fork(&config, "muse"));
        assert!(!adapter_supports_fork_at_message(&config, "muse"));
        assert!(!adapter_supports_fork(&config, "devin"));
        assert!(!adapter_supports_fork_at_message(&config, "devin"));
        assert!(!adapter_supports_fork(&config, "antigravity"));
        assert!(!adapter_supports_fork_at_message(&config, "antigravity"));
    }

    #[test]
    fn runtime_metadata_reports_a_missing_configured_binary() {
        let mut config = test_config();
        config.adapters.claude.binary =
            Some("/definitely/missing/qmux-test-provider-binary".to_string());

        let metadata = adapter_registry(&config).metadata();
        let claude = metadata
            .iter()
            .find(|adapter| adapter.id == "claude")
            .expect("claude metadata");
        assert_eq!(
            claude.configured_binary,
            "/definitely/missing/qmux-test-provider-binary"
        );
        assert_eq!(claude.resolved_binary, None);
        assert_eq!(claude.readiness, AdapterReadiness::Missing);
        assert!(
            claude
                .message
                .as_deref()
                .is_some_and(|message| { message.contains("Claude was not found") })
        );
    }

    #[test]
    fn auth_probe_requires_definitive_status_evidence() {
        assert_eq!(
            classify_auth(
                "claude",
                &ProbeOutput {
                    success: true,
                    text: r#"{"loggedIn":true}"#.to_string(),
                },
            ),
            AdapterAuthState::Authenticated
        );
        assert_eq!(
            classify_auth(
                "codex",
                &ProbeOutput {
                    success: false,
                    text: "Not logged in".to_string(),
                },
            ),
            AdapterAuthState::Unauthenticated
        );
        // Custom providers and older CLIs can reject the probe command even
        // while their environment credentials work. Do not turn ambiguity
        // into a false sign-out gate.
        assert_eq!(
            classify_auth(
                "grok",
                &ProbeOutput {
                    success: false,
                    text: "unknown command: auth".to_string(),
                },
            ),
            AdapterAuthState::Unknown
        );
    }

    #[test]
    fn remote_prerequisite_probe_treats_the_program_as_one_opaque_argument() {
        let host = crate::host::Host::Local;
        assert!(
            run_remote_command_presence_probe(&host, "/bin/sh")
                .unwrap()
                .success
        );
        assert!(
            !run_remote_command_presence_probe(&host, "/definitely missing/qmux-cli")
                .unwrap()
                .success
        );
    }

    #[test]
    fn remote_probe_never_reuses_local_provider_readiness() {
        let config = test_config();
        let remote = crate::config::SavedRemote {
            host: "127.0.0.1".to_string(),
            label: Some("Build host".to_string()),
            multiplexer: crate::workspace::RemoteMultiplexer::Tmux,
            qmux_cli: None,
            workspace_root: None,
        }
        .to_ref("build-host");

        let metadata = probe_adapter_metadata_for_config(&config, Some(&remote), true)
            .expect("remote metadata");
        assert_eq!(metadata.len(), 9);
        assert!(metadata.iter().all(|adapter| {
            adapter.target.kind == "remote" && adapter.target.id.as_deref() == Some("build-host")
        }));
        assert!(
            metadata
                .iter()
                .filter(|adapter| matches!(adapter.id.as_str(), "claude" | "codex"))
                .all(|adapter| adapter.readiness == AdapterReadiness::Error
                    && adapter
                        .message
                        .as_deref()
                        .is_some_and(|message| !message.contains("was not found")))
        );
        assert!(
            metadata
                .iter()
                .filter(|adapter| !matches!(adapter.id.as_str(), "claude" | "codex"))
                .all(|adapter| adapter.readiness == AdapterReadiness::Error
                    && adapter
                        .message
                        .as_deref()
                        .is_some_and(|message| message.contains("cannot run on remote")))
        );
    }

    #[test]
    fn agent_fork_requires_a_supported_agent_in_the_pane() {
        let state = AppState::new(test_config());

        // No agent bound to the pane: nothing to fork.
        let err = agent_fork(&state, "pane-1", false, None, None, None).unwrap_err();
        assert!(err.contains("no agent"), "unexpected error: {err}");

        // An adapter without a native fork command is rejected before any spawn is attempted.
        state
            .insert_agent(AgentInfo {
                id: "agent-1".to_string(),
                group_id: "group-1".to_string(),
                adapter: "unsupported".to_string(),
                worktree_dir: "/tmp/qmux-adapter-tests".to_string(),
                branch: None,
                active_workspace: None,
                pane_id: Some("pane-1".to_string()),
                orphaned_queue_pane_id: None,
                session_id: Some("session-1".to_string()),
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
        let err = agent_fork(&state, "pane-1", false, None, None, None).unwrap_err();
        assert_eq!(err, FORK_UNSUPPORTED_ERROR);
    }

    fn session_agent(id: &str, pane_id: Option<&str>, dir: &str, session: &str) -> AgentInfo {
        AgentInfo {
            id: id.to_string(),
            group_id: "group-1".to_string(),
            adapter: "claude".to_string(),
            worktree_dir: dir.to_string(),
            branch: None,
            active_workspace: None,
            pane_id: pane_id.map(ToString::to_string),
            orphaned_queue_pane_id: None,
            session_id: Some(session.to_string()),
            transcript_path: None,
            status: AgentStatus::Idle,
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
        let mut existing = session_agent("agent-1", None, "/work", "sess-1");
        existing.transcript_path = Some("/work/session.jsonl".to_string());
        state.insert_agent(existing).unwrap();

        let found = reusable_session_agent(&state, "claude", Some("sess-1"), "/work").unwrap();
        assert_eq!(
            found.as_ref().map(|agent| agent.id.as_str()),
            Some("agent-1")
        );
        let found_by_path =
            reusable_session_agent(&state, "claude", Some("session.jsonl"), "/work").unwrap();
        assert_eq!(
            found_by_path.as_ref().map(|agent| agent.id.as_str()),
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
    fn cli_flag_value_reads_space_and_equals_forms() {
        assert_eq!(
            cli_flag_value(
                &["--model".into(), "fable".into(), "hello".into()],
                "--model"
            )
            .as_deref(),
            Some("fable")
        );
        assert_eq!(
            cli_flag_value(&["--model=opus".into(), "hello".into()], "--model").as_deref(),
            Some("opus")
        );
        // Values after `--` are positional, not flag args.
        assert_eq!(
            cli_flag_value(&["--".into(), "--model".into(), "fable".into()], "--model"),
            None
        );
        assert_eq!(cli_flag_value(&["--model".into()], "--model"), None);
    }

    #[test]
    fn normalize_agent_model_maps_claude_families_and_keeps_unknown_ids() {
        assert_eq!(
            normalize_agent_model("claude-fable-5").as_deref(),
            Some("fable")
        );
        assert_eq!(
            normalize_agent_model("claude-opus-4-7").as_deref(),
            Some("opus")
        );
        assert_eq!(
            normalize_agent_model("claude-3-5-sonnet-20241022").as_deref(),
            Some("sonnet")
        );
        assert_eq!(normalize_agent_model("Fable").as_deref(), Some("fable"));
        assert_eq!(
            normalize_agent_model("gpt-5.6-sol").as_deref(),
            Some("gpt-5.6-sol")
        );
        assert_eq!(normalize_agent_model("<synthetic>"), None);
        assert_eq!(normalize_agent_model("  "), None);
    }

    #[test]
    fn claude_native_transcript_line_exposes_assistant_model() {
        let assistant = r#"{"type":"assistant","message":{"role":"assistant","model":"claude-fable-5","content":[{"type":"text","text":"hi"}]}}"#;
        assert_eq!(
            model_from_claude_native_transcript_line(assistant).as_deref(),
            Some("claude-fable-5")
        );
        let user =
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#;
        assert_eq!(model_from_claude_native_transcript_line(user), None);
        let synthetic = r#"{"type":"assistant","message":{"role":"assistant","model":"<synthetic>","content":[]}}"#;
        assert_eq!(
            model_from_claude_native_transcript_line(synthetic).as_deref(),
            Some("<synthetic>")
        );
    }

    #[test]
    fn codex_transcript_line_exposes_turn_context_model() {
        let turn_context =
            r#"{"type":"turn_context","payload":{"model":"gpt-5.6-sol","turn_id":"t1"}}"#;
        assert_eq!(
            model_from_codex_transcript_line(turn_context).as_deref(),
            Some("gpt-5.6-sol")
        );
        let world = r#"{"type":"world_state","payload":{"state":{"model":"gpt-5.4"}}}"#;
        assert_eq!(
            model_from_codex_transcript_line(world).as_deref(),
            Some("gpt-5.4")
        );
        let other = r#"{"type":"event_msg","payload":{"type":"task_started"}}"#;
        assert_eq!(model_from_codex_transcript_line(other), None);
    }

    #[test]
    fn maybe_record_agent_model_updates_once_and_normalizes() {
        let state = AppState::new(test_config());
        let mut agent = session_agent("agent-1", Some("pane-1"), "/work", "sess-1");
        agent.model = None;
        state.insert_agent(agent).unwrap();

        let updated = maybe_record_agent_model(&state, "agent-1", "claude-fable-5")
            .unwrap()
            .expect("should update empty model");
        assert_eq!(updated.model.as_deref(), Some("fable"));

        // Same normalized value is a no-op.
        assert!(
            maybe_record_agent_model(&state, "agent-1", "claude-fable-5")
                .unwrap()
                .is_none()
        );
        // Synthetic placeholders are ignored.
        assert!(
            maybe_record_agent_model(&state, "agent-1", "<synthetic>")
                .unwrap()
                .is_none()
        );
        // A real model change (e.g. /model) updates again.
        let switched = maybe_record_agent_model(&state, "agent-1", "claude-opus-5")
            .unwrap()
            .expect("should update changed model");
        assert_eq!(switched.model.as_deref(), Some("opus"));
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
        let mut source = session_agent(
            "source-agent",
            Some("pane-source"),
            "/work",
            "source-session",
        );
        source.transcript_path = Some("/work/source.jsonl".to_string());
        state.insert_agent(source).unwrap();
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

        let mut path_fork = session_agent("path-fork-agent", None, "/work", "placeholder");
        path_fork.session_id = None;
        state.insert_agent(path_fork.clone()).unwrap();
        let path_fork =
            record_shell_fork_lineage(&state, path_fork, "claude", Some("source.jsonl"), "/work")
                .unwrap();
        assert_eq!(path_fork.parent_id.as_deref(), Some("source-agent"));
    }
}
