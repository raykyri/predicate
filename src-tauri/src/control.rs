//! Shared public control surface for the human CLI and restricted agent CLI.
//!
//! The Unix-socket token is resolved before this module is entered. Callers
//! receive an immutable context derived from live qmux state; no public payload
//! may claim a different principal, pane, agent, or workspace.

use crate::events::QmuxEvent;
use crate::state::{AppState, PaneSplitAxis, PaneSplitInfo};
use crate::workspace::{AgentInfo, CreateGroupRequest, create_group, rename_group};
use qmux_proto::{PUBLIC_API_VERSION, PublicControlError, PublicControlResponse};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const MAX_CONCURRENT_PUBLIC_WAITS: usize = 16;
const MIN_SPLIT_FRACTION: f64 = 0.12;
static ACTIVE_PUBLIC_WAITS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ControlPrincipal {
    User,
    Agent,
    /// A paired device over the remote transport. Reads everything —
    /// every workspace, pane, and agent — and writes everywhere unless the
    /// device was paired read-only (docs/remote-control-plan.md).
    Remote,
}

#[derive(Clone, Debug)]
pub struct ControlContext {
    pub principal: ControlPrincipal,
    pub pane_id: String,
    pub workspace_id: String,
    pub agent: Option<AgentInfo>,
    /// True only for a Remote principal whose device was paired read-only.
    pub read_only: bool,
}

#[derive(Clone, Debug)]
pub struct ControlFailure {
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

impl ControlFailure {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: Value::Null,
        }
    }
}

type ControlResult = Result<Value, ControlFailure>;

pub fn handle_call(
    state: &AppState,
    pane_id: &str,
    user_credential: bool,
    operation: &str,
    arguments: Value,
) -> Value {
    let result = context_for(state, pane_id, user_credential)
        .and_then(|context| dispatch(state, &context, None, operation, arguments));
    encode_response(result)
}

/// Entry point for calls arriving over the remote transport. The session's
/// pairing already authenticated the device; this derives its context and
/// dispatches with the Remote principal.
pub fn handle_remote_call(
    state: &AppState,
    session: &crate::remote::session::RemoteSession,
    operation: &str,
    arguments: Value,
) -> Value {
    let result = remote_context_for(state, session)
        .and_then(|context| dispatch(state, &context, Some(session), operation, arguments));
    encode_response(result)
}

fn encode_response(result: ControlResult) -> Value {
    match result {
        Ok(result) => serde_json::to_value(PublicControlResponse {
            ok: true,
            api_version: PUBLIC_API_VERSION,
            result,
            error: None,
        })
        .unwrap_or_else(|_| json!({ "ok": false, "apiVersion": PUBLIC_API_VERSION })),
        Err(error) => serde_json::to_value(PublicControlResponse {
            ok: false,
            api_version: PUBLIC_API_VERSION,
            result: Value::Null,
            error: Some(PublicControlError {
                code: error.code.to_string(),
                message: error.message,
                details: error.details,
            }),
        })
        .unwrap_or_else(|_| json!({ "ok": false, "apiVersion": PUBLIC_API_VERSION })),
    }
}

/// Derives a remote session's context. The focus pane names "the current
/// pane" for operations that need one: the session's own choice when it
/// still exists, else the app's active pane, else the most recently active
/// pane. A qmux with no panes at all still gets a context (anchored to the
/// first workspace) so listing operations work; pane-anchored ones then
/// fail closed with not-found.
fn remote_context_for(
    state: &AppState,
    session: &crate::remote::session::RemoteSession,
) -> Result<ControlContext, ControlFailure> {
    let panes = state.list_panes().map_err(internal)?;
    let focus = session
        .focus_pane()
        .and_then(|id| panes.iter().find(|pane| pane.id == id))
        .or_else(|| {
            state
                .active_tab_id()
                .ok()
                .flatten()
                .and_then(|id| panes.iter().find(|pane| pane.id == id))
        })
        .or_else(|| panes.iter().max_by_key(|pane| pane.last_active_at));
    let (pane_id, workspace_id, agent) = match focus {
        Some(pane) => (
            pane.id.clone(),
            pane.group_id.clone(),
            state.agent_by_pane(&pane.id).map_err(internal)?,
        ),
        None => {
            let workspace_id = state
                .list_groups()
                .map_err(internal)?
                .first()
                .map(|group| group.id.clone())
                .ok_or_else(|| ControlFailure::new("no_workspace", "qmux has no workspaces yet"))?;
            (String::new(), workspace_id, None)
        }
    };
    Ok(ControlContext {
        principal: ControlPrincipal::Remote,
        pane_id,
        workspace_id,
        agent,
        read_only: session.read_only,
    })
}

fn context_for(
    state: &AppState,
    pane_id: &str,
    user_credential: bool,
) -> Result<ControlContext, ControlFailure> {
    let workspace_id = state
        .pane_group_id(pane_id)
        .map_err(internal)?
        .ok_or_else(|| {
            ControlFailure::new("pane_not_found", format!("pane {pane_id} was not found"))
        })?;
    let agent = state.agent_by_pane(pane_id).map_err(internal)?;
    let principal = if user_credential {
        ControlPrincipal::User
    } else if agent.is_some() {
        ControlPrincipal::Agent
    } else {
        return Err(ControlFailure::new(
            "user_credential_required",
            "public control from a shell pane requires QMUX_USER_TOKEN",
        ));
    };
    Ok(ControlContext {
        principal,
        pane_id: pane_id.to_string(),
        workspace_id,
        agent,
        read_only: false,
    })
}

fn dispatch(
    state: &AppState,
    context: &ControlContext,
    session: Option<&crate::remote::session::RemoteSession>,
    operation: &str,
    arguments: Value,
) -> ControlResult {
    match operation {
        "session.focus" => session_focus(state, session, arguments),
        "ping" => {
            ensure_no_arguments(arguments, "ping")?;
            Ok(json!({ "status": "ok", "principal": context.principal }))
        }
        "context" => {
            ensure_no_arguments(arguments, "context")?;
            context_snapshot(state, context)
        }
        "workspace.list" => {
            ensure_no_arguments(arguments, "workspace.list")?;
            workspace_list(state, context)
        }
        "workspace.get" => workspace_get(state, context, arguments),
        "workspace.create" => workspace_create(state, context, arguments),
        "workspace.rename" => workspace_rename(state, context, arguments),
        "pane.list" => {
            ensure_no_arguments(arguments, "pane.list")?;
            pane_list(state, context)
        }
        "pane.current" => {
            ensure_no_arguments(arguments, "pane.current")?;
            pane_get_by_id(state, context, &context.pane_id)
        }
        "pane.get" => {
            let args: IdArgs = parse(arguments, "pane.get")?;
            pane_get_by_id(state, context, &args.id)
        }
        "pane.read" => pane_read(state, context, arguments),
        "pane.snapshot" => pane_snapshot(state, context, arguments),
        "pane.create" => pane_create(state, context, arguments),
        "pane.send" => pane_send(state, context, arguments, false),
        "pane.run" => pane_send(state, context, arguments, true),
        "pane.waitOutput" => pane_wait_output(state, context, arguments),
        "pane.rename" => pane_rename(state, context, arguments),
        "pane.focus" => pane_focus(state, context, arguments),
        "pane.close" => pane_close(state, context, arguments),
        "agent.list" => {
            ensure_no_arguments(arguments, "agent.list")?;
            agent_list(state, context)
        }
        "agent.get" => {
            let args: IdArgs = parse(arguments, "agent.get")?;
            agent_get_by_id(state, context, &args.id)
        }
        "agent.read" => agent_read(state, context, arguments),
        "agent.start" => agent_start(state, context, arguments),
        "agent.fork" => agent_fork(state, context, arguments),
        "agent.prompt" => agent_prompt(state, context, arguments),
        "agent.submit" => agent_submit(state, context, arguments),
        "agent.permission" => agent_permission(state, context, arguments),
        "agent.queue.list" => agent_queue_list(state, context, arguments),
        "agent.queue.remove" => agent_queue_remove(state, context, arguments),
        "agent.queue.reorder" => agent_queue_reorder(state, context, arguments),
        "agent.queue.sendNext" => agent_queue_send_next(state, context, arguments),
        "agent.queue.pause" => agent_queue_pause(state, context, arguments),
        "agent.queue.unpause" => agent_queue_unpause(state, context, arguments),
        "adapter.policy" => adapter_policy(state, context, arguments),
        "agent.wait" => agent_wait(state, context, arguments),
        "agent.focus" => agent_focus(state, context, arguments),
        "agent.release" => agent_release(state, context, arguments),
        "artifact.list" => {
            ensure_no_arguments(arguments, "artifact.list")?;
            artifact_list(state, context)
        }
        "artifact.open" => artifact_open(state, context, arguments),
        "split.list" => {
            ensure_no_arguments(arguments, "split.list")?;
            split_list(state, context)
        }
        "split.join" => split_join(state, context, arguments),
        "split.leave" => split_leave(state, context, arguments),
        "split.resize" => split_resize(state, context, arguments),
        other => Err(ControlFailure::new(
            "unknown_operation",
            format!("unknown public control operation '{other}'"),
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdArgs {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReadArgs {
    id: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    lines: Option<usize>,
    #[serde(default)]
    turns: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkspaceCreateArgs {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    dir: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameArgs {
    id: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PaneCreateArgs {
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PaneSendArgs {
    id: String,
    text: String,
    #[serde(default)]
    submit: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SplitJoinArgs {
    id: String,
    other: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SplitResizeArgs {
    id: String,
    pane: String,
    fraction: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WaitOutputArgs {
    id: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    regex: Option<String>,
    #[serde(default = "default_wait_timeout_ms")]
    timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentStartArgs {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    adapter: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    use_worktree: bool,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentForkArgs {
    id: String,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    use_worktree: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentPromptArgs {
    id: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentSubmitArgs {
    id: String,
    text: String,
    mode: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentPermissionArgs {
    id: String,
    action: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QueueIndexArgs {
    id: String,
    index: usize,
    #[serde(default)]
    expected_id: Option<String>,
    #[serde(default)]
    expected_data: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QueueReorderArgs {
    id: String,
    from_index: usize,
    to_index: usize,
    #[serde(default)]
    expected_id: Option<String>,
    #[serde(default)]
    expected_data: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QueuePauseArgs {
    id: String,
    index: usize,
    pause_after: bool,
    #[serde(default)]
    expected_id: Option<String>,
    #[serde(default)]
    expected_data: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AdapterPolicyArgs {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    adapter: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentWaitArgs {
    id: String,
    #[serde(default = "default_agent_until")]
    until: String,
    #[serde(default = "default_wait_timeout_ms")]
    timeout_ms: u64,
}

fn default_agent_until() -> String {
    "settled".to_string()
}

fn default_wait_timeout_ms() -> u64 {
    30_000
}

/// Points a remote session's context at a pane. Deliberately allowed for
/// read-only devices: it is session-local navigation, not app state.
fn session_focus(
    state: &AppState,
    session: Option<&crate::remote::session::RemoteSession>,
    arguments: Value,
) -> ControlResult {
    let Some(session) = session else {
        return Err(ControlFailure::new(
            "invalid_operation",
            "session.focus is available only to remote sessions",
        ));
    };
    let args: IdArgs = parse(arguments, "session.focus")?;
    let pane = find_pane(state, &args.id)?;
    session.set_focus_pane(pane.id.clone());
    Ok(json!({ "focusPane": pane.id }))
}

fn context_snapshot(state: &AppState, context: &ControlContext) -> ControlResult {
    let pane = if context.pane_id.is_empty() {
        None
    } else {
        Some(find_pane(state, &context.pane_id)?)
    };
    let workspace = state
        .group(&context.workspace_id)
        .map_err(internal)?
        .ok_or_else(|| not_found("workspace", &context.workspace_id))?;
    Ok(json!({
        "principal": context.principal,
        "pane": pane,
        "workspace": workspace,
        "agent": context.agent,
        "capabilities": match context.principal {
            ControlPrincipal::User => json!({
                "read": "all workspaces; panes and agents in the current workspace",
                "write": "current terminal workspace"
            }),
            ControlPrincipal::Agent => json!({
                "read": "self and live descendants in this workspace",
                "write": "direct parent and direct children only"
            }),
            ControlPrincipal::Remote => json!({
                "read": "every workspace, pane, and agent",
                "write": if context.read_only {
                    "none: this device is paired read-only"
                } else {
                    "every workspace"
                }
            }),
        }
    }))
}

fn workspace_list(state: &AppState, context: &ControlContext) -> ControlResult {
    let mut workspaces = state.list_groups().map_err(internal)?;
    if context.principal == ControlPrincipal::Agent {
        workspaces.retain(|workspace| workspace.id == context.workspace_id);
    }
    Ok(json!({ "workspaces": workspaces, "count": workspaces.len() }))
}

fn workspace_get(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    let args: IdArgs = parse(arguments, "workspace.get")?;
    if context.principal == ControlPrincipal::Agent && args.id != context.workspace_id {
        return Err(denied("agents may inspect only their current workspace"));
    }
    let workspace = state
        .group(&args.id)
        .map_err(internal)?
        .ok_or_else(|| not_found("workspace", &args.id))?;
    Ok(json!({ "workspace": workspace }))
}

fn workspace_create(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    require_write(context)?;
    let args: WorkspaceCreateArgs = parse(arguments, "workspace.create")?;
    let workspace = create_group(
        state,
        CreateGroupRequest {
            name: args.name,
            dir: args.dir,
            after_group_id: Some(context.workspace_id.clone()),
            base_repo: None,
            base_ref: None,
            remote: None,
            remote_id: None,
        },
    )
    .map_err(internal)?;
    Ok(json!({ "workspace": workspace }))
}

fn workspace_rename(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    require_write(context)?;
    let args: RenameArgs = parse(arguments, "workspace.rename")?;
    if context.principal == ControlPrincipal::User && args.id != context.workspace_id {
        return Err(denied(
            "the interactive credential may rename only its current workspace",
        ));
    }
    let workspace = rename_group(state, &args.id, Some(args.name)).map_err(internal)?;
    Ok(json!({ "workspace": workspace }))
}

fn pane_list(state: &AppState, context: &ControlContext) -> ControlResult {
    let allowed = allowed_pane_ids(state, context)?;
    let panes = state
        .list_panes()
        .map_err(internal)?
        .into_iter()
        .filter(|pane| allowed.contains(&pane.id))
        .collect::<Vec<_>>();
    Ok(json!({ "panes": panes, "count": panes.len() }))
}

fn pane_get_by_id(state: &AppState, context: &ControlContext, pane_id: &str) -> ControlResult {
    ensure_pane_read(state, context, pane_id)?;
    Ok(json!({ "pane": find_pane(state, pane_id)? }))
}

fn pane_read(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    let args: ReadArgs = parse(arguments, "pane.read")?;
    ensure_pane_read(state, context, &args.id)?;
    let source = args.source.as_deref().unwrap_or("terminal");
    let lines = args.lines.unwrap_or(100).clamp(1, 1000);
    let output = match source {
        "terminal" => {
            let raw =
                crate::scrollback::read_pane_scrollback(&state.config().workspace_root, &args.id)
                    .map_err(internal)?;
            crate::mcp::terminal_text_tail(&raw, lines)
        }
        "viewport" => crate::native_terminal::native_terminal_read_viewport_text(args.id.clone())
            .map_err(internal)?,
        _ => {
            return Err(ControlFailure::new(
                "invalid_argument",
                "pane.read source must be terminal or viewport",
            ));
        }
    };
    Ok(json!({ "paneId": args.id, "source": source, "lines": lines, "output": output }))
}

/// Full sanitized terminal replay plus dimensions — what a remote client
/// primes its emulator from before (or after a gap in) the live stream.
fn pane_snapshot(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    let args: IdArgs = parse(arguments, "pane.snapshot")?;
    ensure_pane_read(state, context, &args.id)?;
    let pane = find_pane(state, &args.id)?;
    let raw = crate::scrollback::read_pane_scrollback(&state.config().workspace_root, &args.id)
        .map_err(internal)?;
    let replay = crate::scrollback::sanitize_scrollback_replay(&raw);
    use base64::Engine as _;
    Ok(json!({
        "paneId": args.id,
        "rows": pane.rows,
        "cols": pane.cols,
        "bytesBase64": base64::engine::general_purpose::STANDARD.encode(replay),
    }))
}

fn pane_create(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    require_write(context)?;
    let args: PaneCreateArgs = parse(arguments, "pane.create")?;
    let workspace_id = args
        .workspace_id
        .as_deref()
        .unwrap_or(&context.workspace_id);
    if context.principal == ControlPrincipal::User && workspace_id != context.workspace_id {
        return Err(denied(
            "the interactive credential may create panes only in its current workspace",
        ));
    }
    let after_pane = (!context.pane_id.is_empty()).then_some(context.pane_id.as_str());
    let pane = crate::pty::spawn_shell_pane_at(
        state,
        None,
        after_pane,
        Some(workspace_id),
        args.cwd.as_deref(),
    )
    .map_err(internal)?;
    state.emit(QmuxEvent::new(
        "pane.created",
        Some(pane.id.clone()),
        None,
        json!({ "pane": pane }),
    ));
    Ok(json!({ "pane": pane }))
}

fn pane_send(
    state: &AppState,
    context: &ControlContext,
    arguments: Value,
    run: bool,
) -> ControlResult {
    require_write(context)?;
    let args: PaneSendArgs = parse(arguments, if run { "pane.run" } else { "pane.send" })?;
    ensure_pane_read(state, context, &args.id)?;
    let submit = if run {
        true
    } else {
        args.submit.unwrap_or(false)
    };
    crate::pty::write_pane(
        state,
        crate::pty::PaneWriteOptions {
            pane_id: args.id.clone(),
            data: args.text,
            paste: true,
            submit,
        },
    )
    .map_err(internal)?;
    Ok(json!({ "paneId": args.id, "written": true, "submitted": submit }))
}

fn pane_wait_output(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    let args: WaitOutputArgs = parse(arguments, "pane.waitOutput")?;
    ensure_pane_read(state, context, &args.id)?;
    if args.text.is_some() == args.regex.is_some() {
        return Err(ControlFailure::new(
            "invalid_argument",
            "pane wait-output requires exactly one of --match or --regex",
        ));
    }
    let pattern = args
        .regex
        .as_deref()
        .map(regex::Regex::new)
        .transpose()
        .map_err(|error| ControlFailure::new("invalid_regex", error.to_string()))?;
    let timeout_ms = args.timeout_ms.min(600_000);
    let _wait_slot = PublicWaitSlot::acquire()?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let raw = crate::scrollback::read_pane_scrollback(&state.config().workspace_root, &args.id)
            .map_err(internal)?;
        let searchable_output = crate::mcp::terminal_text_tail(&raw, usize::MAX);
        let complete = args
            .text
            .as_ref()
            .is_some_and(|text| searchable_output.contains(text))
            || pattern
                .as_ref()
                .is_some_and(|regex| regex.is_match(&searchable_output));
        if complete || Instant::now() >= deadline {
            let output = searchable_output
                .lines()
                .rev()
                .take(200)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(json!({
                "paneId": args.id,
                "complete": complete,
                "timedOut": !complete,
                "timeoutMs": timeout_ms,
                "output": output
            }));
        }
        thread::sleep(Duration::from_millis(150));
    }
}

fn pane_rename(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    require_write(context)?;
    let args: RenameArgs = parse(arguments, "pane.rename")?;
    ensure_pane_read(state, context, &args.id)?;
    let pane = state.rename_pane(&args.id, args.name).map_err(internal)?;
    state.emit(QmuxEvent::new(
        "pane.renamed",
        Some(pane.id.clone()),
        pane.agent_id.clone(),
        json!({ "pane": pane }),
    ));
    Ok(json!({ "pane": pane }))
}

fn pane_focus(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    require_write(context)?;
    let args: IdArgs = parse(arguments, "pane.focus")?;
    ensure_pane_read(state, context, &args.id)?;
    request_pane_focus(state, &args.id);
    Ok(json!({ "paneId": args.id, "focusRequested": true }))
}

fn pane_close(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    require_write(context)?;
    let args: IdArgs = parse(arguments, "pane.close")?;
    ensure_pane_read(state, context, &args.id)?;
    state.close_pane_for_user(&args.id).map_err(internal)?;
    Ok(json!({ "paneId": args.id, "closed": true }))
}

fn agent_list(state: &AppState, context: &ControlContext) -> ControlResult {
    let allowed = allowed_agent_ids(state, context)?;
    let agents = state
        .list_agents()
        .map_err(internal)?
        .into_iter()
        .filter(|agent| allowed.contains(&agent.id))
        .collect::<Vec<_>>();
    Ok(json!({ "agents": agents, "count": agents.len() }))
}

fn agent_get_by_id(state: &AppState, context: &ControlContext, agent_id: &str) -> ControlResult {
    ensure_agent_read(state, context, agent_id)?;
    let agent = state
        .agent(agent_id)
        .map_err(internal)?
        .ok_or_else(|| not_found("agent", agent_id))?;
    Ok(json!({ "agent": agent }))
}

fn agent_read(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    let args: ReadArgs = parse(arguments, "agent.read")?;
    ensure_agent_read(state, context, &args.id)?;
    let agent = state
        .agent(&args.id)
        .map_err(internal)?
        .ok_or_else(|| not_found("agent", &args.id))?;
    let source = args.source.as_deref().unwrap_or("transcript");
    match source {
        "transcript" => {
            let count = args.turns.unwrap_or(4).clamp(1, 100);
            let turns = state.list_turns(Some(&args.id)).map_err(internal)?;
            let start = turns.len().saturating_sub(count);
            Ok(json!({ "agent": agent, "source": source, "turns": &turns[start..] }))
        }
        "terminal" => {
            let pane_id = agent
                .pane_id
                .as_deref()
                .ok_or_else(|| ControlFailure::new("agent_exited", "agent has no live pane"))?;
            pane_read(
                state,
                context,
                json!({ "id": pane_id, "source": "terminal", "lines": args.lines }),
            )
        }
        _ => Err(ControlFailure::new(
            "invalid_argument",
            "agent.read source must be transcript or terminal",
        )),
    }
}

fn agent_start(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    let args: AgentStartArgs = parse(arguments, "agent.start")?;
    if context.principal == ControlPrincipal::Agent {
        if args.name.is_some()
            || args.model.is_some()
            || args.effort.is_some()
            || args.cwd.is_some()
        {
            return Err(ControlFailure::new(
                "invalid_argument",
                "agent principals cannot set name, model, effort, or cwd when starting a child",
            ));
        }
        return crate::mcp::handle_call(
            state,
            &context.pane_id,
            "spawn_agent",
            json!({
                "adapter": args.adapter,
                "prompt": args.prompt,
                "useWorktree": args.use_worktree
            }),
        )
        .map_err(internal);
    }
    require_write(context)?;
    let adapter = args.adapter.unwrap_or_else(|| "claude".to_string());
    let options = match (adapter.as_str(), args.effort.as_deref()) {
        ("claude", Some(effort)) => json!({ "effort": effort }),
        ("codex", Some(effort)) => json!({ "reasoningEffort": effort }),
        _ => Value::Null,
    };
    let workspace = state
        .group(&context.workspace_id)
        .map_err(internal)?
        .ok_or_else(|| not_found("workspace", &context.workspace_id))?;
    let pane = crate::adapters::agent_spawn(
        state,
        crate::adapters::SpawnAgentRequest {
            adapter_id: adapter,
            prompt: args.prompt.unwrap_or_default(),
            group_id: Some(context.workspace_id.clone()),
            base_repo: Some(args.cwd.clone().unwrap_or(workspace.dir)),
            base_ref: Some("HEAD".to_string()),
            cwd: args.cwd,
            model: args.model,
            initial_size: None,
            use_worktree: Some(args.use_worktree),
            options,
            parent_id: None,
            resume_session_id: None,
            fork_session: false,
        },
    )
    .map_err(internal)?;
    let pane = if let Some(name) = args.name {
        state.rename_pane(&pane.id, name).map_err(internal)?
    } else {
        pane
    };
    let agent = state.agent_by_pane(&pane.id).map_err(internal)?;
    Ok(json!({ "pane": pane, "agent": agent }))
}

fn agent_fork(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    let args: AgentForkArgs = parse(arguments, "agent.fork")?;
    ensure_agent_read(state, context, &args.id)?;
    if context.principal == ControlPrincipal::Agent
        && context.agent.as_ref().map(|agent| agent.id.as_str()) != Some(args.id.as_str())
    {
        return Err(denied("agent principals may fork only themselves"));
    }
    if context.principal == ControlPrincipal::Remote {
        require_write(context)?;
    }
    let target = state
        .agent(&args.id)
        .map_err(internal)?
        .ok_or_else(|| not_found("agent", &args.id))?;
    let pane_id = target
        .pane_id
        .as_deref()
        .ok_or_else(|| ControlFailure::new("agent_exited", "agent has no live pane"))?;
    let pane =
        crate::adapters::agent_fork(state, pane_id, args.use_worktree, args.prompt, None, None)
            .map_err(internal)?;
    let agent = state.agent_by_pane(&pane.id).map_err(internal)?;
    Ok(json!({ "pane": pane, "agent": agent }))
}

fn agent_prompt(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    let args: AgentPromptArgs = parse(arguments, "agent.prompt")?;
    if context.principal == ControlPrincipal::Agent {
        return crate::mcp::handle_call(
            state,
            &context.pane_id,
            "send_prompt",
            json!({ "agentId": args.id, "text": args.text }),
        )
        .map_err(internal);
    }
    require_write(context)?;
    ensure_agent_read(state, context, &args.id)?;
    let delivery = crate::turn_queue::submit_agent_turn(
        state,
        crate::turn_queue::SubmitAgentTurnRequest {
            agent_id: args.id,
            data: args.text,
            mode: Some(crate::turn_queue::SubmitAgentTurnMode::Auto),
        },
    )
    .map_err(internal)?;
    serde_json::to_value(delivery).map_err(|error| internal(error.to_string()))
}

/// `agent.prompt` with an explicit delivery mode — the composer's own
/// send/queue/steer verbs, for clients that render those buttons.
fn agent_submit(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    let args: AgentSubmitArgs = parse(arguments, "agent.submit")?;
    require_write(context)?;
    ensure_agent_read(state, context, &args.id)?;
    let mode = match args.mode.as_str() {
        "send" => crate::turn_queue::SubmitAgentTurnMode::Send,
        "queue" => crate::turn_queue::SubmitAgentTurnMode::Queue,
        "steer" => crate::turn_queue::SubmitAgentTurnMode::Steer,
        other => {
            return Err(ControlFailure::new(
                "invalid_argument",
                format!("agent.submit mode must be send, queue, or steer (got '{other}')"),
            ));
        }
    };
    let delivery = crate::turn_queue::submit_agent_turn(
        state,
        crate::turn_queue::SubmitAgentTurnRequest {
            agent_id: args.id,
            data: args.text,
            mode: Some(mode),
        },
    )
    .map_err(internal)?;
    serde_json::to_value(delivery).map_err(|error| internal(error.to_string()))
}

/// Answers a permission prompt with the adapter's own keystroke — exactly
/// the raw pane write the desktop buttons make, bypassing the turn queue.
fn agent_permission(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    let args: AgentPermissionArgs = parse(arguments, "agent.permission")?;
    require_write(context)?;
    ensure_agent_read(state, context, &args.id)?;
    let agent = state
        .agent(&args.id)
        .map_err(internal)?
        .ok_or_else(|| not_found("agent", &args.id))?;
    if agent.status != crate::workspace::AgentStatus::AwaitingPermission {
        return Err(ControlFailure::new(
            "not_awaiting_permission",
            "the agent is not waiting on a permission prompt",
        ));
    }
    let policy = crate::adapters::agent_composer_policy(state, &agent).map_err(internal)?;
    let action = policy
        .permission_actions
        .iter()
        .find(|action| action.id == args.action)
        .ok_or_else(|| {
            let valid = policy
                .permission_actions
                .iter()
                .map(|action| action.id)
                .collect::<Vec<_>>()
                .join(", ");
            ControlFailure::new(
                "invalid_argument",
                if valid.is_empty() {
                    format!(
                        "the {} adapter has no composer permission actions",
                        agent.adapter
                    )
                } else {
                    format!(
                        "unknown permission action '{}' (valid: {valid})",
                        args.action
                    )
                },
            )
        })?;
    let pane_id = agent
        .pane_id
        .clone()
        .ok_or_else(|| ControlFailure::new("agent_exited", "agent has no live pane"))?;
    if !state.claim_agent_permission(&args.id).map_err(internal)? {
        return Err(ControlFailure::new(
            "not_awaiting_permission",
            "the permission prompt was already answered or is no longer waiting",
        ));
    }
    if let Err(error) = crate::pty::write_pane(
        state,
        crate::pty::PaneWriteOptions {
            pane_id: pane_id.clone(),
            data: action.input.to_string(),
            paste: true,
            submit: true,
        },
    ) {
        state.release_agent_permission_claim(&args.id);
        return Err(internal(error));
    }
    Ok(json!({
        "agentId": args.id,
        "paneId": pane_id,
        "action": action.id,
        "answered": true
    }))
}

fn agent_queue_list(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    let args: IdArgs = parse(arguments, "agent.queue.list")?;
    ensure_agent_read(state, context, &args.id)?;
    let turns = state.agent_queued_turns(&args.id).map_err(internal)?;
    Ok(json!({ "agentId": args.id, "count": turns.len(), "queuedTurns": turns }))
}

fn agent_queue_remove(
    state: &AppState,
    context: &ControlContext,
    arguments: Value,
) -> ControlResult {
    let args: QueueIndexArgs = parse(arguments, "agent.queue.remove")?;
    require_write(context)?;
    ensure_agent_read(state, context, &args.id)?;
    let result = crate::turn_queue::remove_queued_agent_turn(
        state,
        crate::turn_queue::RemoveQueuedAgentTurnRequest {
            agent_id: args.id,
            index: args.index,
            expected_data: args.expected_data,
            expected_id: args.expected_id,
        },
    )
    .map_err(invalid_argument)?;
    serde_json::to_value(result).map_err(|error| internal(error.to_string()))
}

fn agent_queue_reorder(
    state: &AppState,
    context: &ControlContext,
    arguments: Value,
) -> ControlResult {
    let args: QueueReorderArgs = parse(arguments, "agent.queue.reorder")?;
    require_write(context)?;
    ensure_agent_read(state, context, &args.id)?;
    let result = crate::turn_queue::reorder_queued_agent_turn(
        state,
        crate::turn_queue::ReorderQueuedAgentTurnRequest {
            agent_id: args.id,
            from_index: args.from_index,
            to_index: args.to_index,
            expected_data: args.expected_data,
            expected_id: args.expected_id,
        },
    )
    .map_err(invalid_argument)?;
    serde_json::to_value(result).map_err(|error| internal(error.to_string()))
}

fn agent_queue_send_next(
    state: &AppState,
    context: &ControlContext,
    arguments: Value,
) -> ControlResult {
    let args: IdArgs = parse(arguments, "agent.queue.sendNext")?;
    require_write(context)?;
    ensure_agent_read(state, context, &args.id)?;
    let result =
        crate::turn_queue::send_next_queued_agent_turn(state, &args.id).map_err(internal)?;
    serde_json::to_value(result).map_err(|error| internal(error.to_string()))
}

fn agent_queue_pause(
    state: &AppState,
    context: &ControlContext,
    arguments: Value,
) -> ControlResult {
    let args: QueuePauseArgs = parse(arguments, "agent.queue.pause")?;
    require_write(context)?;
    ensure_agent_read(state, context, &args.id)?;
    let queued_turns = crate::turn_queue::set_queued_turn_pause(
        state,
        &args.id,
        args.index,
        args.pause_after,
        args.expected_data.as_deref(),
        args.expected_id.as_deref(),
    )
    .map_err(invalid_argument)?;
    Ok(json!({
        "agentId": args.id,
        "pendingTurns": queued_turns.len(),
        "queuedTurns": queued_turns
    }))
}

fn agent_queue_unpause(
    state: &AppState,
    context: &ControlContext,
    arguments: Value,
) -> ControlResult {
    let args: IdArgs = parse(arguments, "agent.queue.unpause")?;
    require_write(context)?;
    ensure_agent_read(state, context, &args.id)?;
    let result = crate::turn_queue::unpause_agent(state, &args.id).map_err(internal)?;
    serde_json::to_value(result).map_err(|error| internal(error.to_string()))
}

/// The adapter's composer gating and permission actions — served so a client
/// renders exactly the buttons the backend will honor, instead of guessing.
fn adapter_policy(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    let args: AdapterPolicyArgs = parse(arguments, "adapter.policy")?;
    let registry = crate::adapters::adapter_registry(state.config());
    match (args.agent, args.adapter) {
        (Some(agent_id), None) => {
            ensure_agent_read(state, context, &agent_id)?;
            let agent = state
                .agent(&agent_id)
                .map_err(internal)?
                .ok_or_else(|| not_found("agent", &agent_id))?;
            let adapter = registry.get(&agent.adapter).map_err(internal)?;
            let can_fork = agent.session_id.is_some()
                && adapter.supports_fork()
                && adapter.can_fork_agent(&agent);
            Ok(json!({
                "adapterId": adapter.id(),
                "policy": adapter.composer_policy(),
                "supportsFork": adapter.supports_fork(),
                "supportsForkAtMessage": adapter.supports_fork_at_message(),
                "canFork": can_fork,
                "agentStatus": agent.status,
            }))
        }
        (None, Some(adapter_id)) => {
            let adapter = registry
                .get(&adapter_id)
                .map_err(|_| not_found("adapter", &adapter_id))?;
            Ok(json!({
                "adapterId": adapter.id(),
                "policy": adapter.composer_policy(),
                "supportsFork": adapter.supports_fork(),
                "supportsForkAtMessage": adapter.supports_fork_at_message(),
            }))
        }
        _ => Err(ControlFailure::new(
            "invalid_argument",
            "adapter.policy takes exactly one of agent or adapter",
        )),
    }
}

fn agent_wait(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    let args: AgentWaitArgs = parse(arguments, "agent.wait")?;
    ensure_agent_read(state, context, &args.id)?;
    if !matches!(
        args.until.as_str(),
        "settled" | "input" | "permission" | "done" | "failed" | "exited"
    ) {
        return Err(ControlFailure::new(
            "invalid_argument",
            "agent wait state must be settled, input, permission, done, failed, or exited",
        ));
    }
    let timeout_ms = args.timeout_ms.min(600_000);
    let _wait_slot = PublicWaitSlot::acquire()?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let agent = state.agent(&args.id).map_err(internal)?;
        let complete = match (args.until.as_str(), agent.as_ref()) {
            ("exited", None) => true,
            ("settled" | "done", None) => true,
            ("exited", Some(agent)) => agent.pane_id.is_none(),
            ("settled" | "done", Some(agent)) if agent.pane_id.is_none() => true,
            ("settled", Some(agent)) => {
                agent.status.is_at_rest() || agent.status == crate::workspace::AgentStatus::Failed
            }
            ("input", Some(agent)) => agent.status == crate::workspace::AgentStatus::AwaitingInput,
            ("permission", Some(agent)) => {
                agent.status == crate::workspace::AgentStatus::AwaitingPermission
            }
            ("done", Some(agent)) => matches!(
                agent.status,
                crate::workspace::AgentStatus::Done | crate::workspace::AgentStatus::Idle
            ),
            ("failed", Some(agent)) => agent.status == crate::workspace::AgentStatus::Failed,
            _ => false,
        };
        if complete || Instant::now() >= deadline {
            return Ok(json!({
                "agentId": args.id,
                "agent": agent,
                "until": args.until,
                "complete": complete,
                "timedOut": !complete,
                "timeoutMs": timeout_ms
            }));
        }
        thread::sleep(Duration::from_millis(150));
    }
}

fn agent_focus(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    require_write(context)?;
    let args: IdArgs = parse(arguments, "agent.focus")?;
    ensure_agent_read(state, context, &args.id)?;
    let agent = state
        .agent(&args.id)
        .map_err(internal)?
        .ok_or_else(|| not_found("agent", &args.id))?;
    let pane_id = agent
        .pane_id
        .ok_or_else(|| ControlFailure::new("agent_exited", "agent has no live pane"))?;
    request_pane_focus(state, &pane_id);
    Ok(json!({ "agentId": args.id, "paneId": pane_id, "focusRequested": true }))
}

fn agent_release(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    let args: IdArgs = parse(arguments, "agent.release")?;
    if context.principal == ControlPrincipal::Agent {
        return crate::mcp::handle_call(
            state,
            &context.pane_id,
            "release_agent",
            json!({ "agentId": args.id }),
        )
        .map_err(internal);
    }
    require_write(context)?;
    ensure_agent_read(state, context, &args.id)?;
    let agents = state.list_agents().map_err(internal)?;
    let mut pending = VecDeque::from([args.id.clone()]);
    let mut seen = HashSet::from([args.id.clone()]);
    let mut descendants = Vec::new();
    while let Some(parent) = pending.pop_front() {
        for child in agents
            .iter()
            .filter(|agent| agent.parent_id.as_deref() == Some(parent.as_str()))
        {
            if seen.insert(child.id.clone()) {
                if child.pane_id.is_some() {
                    descendants.push(child.id.clone());
                }
                pending.push_back(child.id.clone());
            }
        }
    }
    if !descendants.is_empty() {
        return Ok(json!({
            "agentId": args.id,
            "released": false,
            "blockedByLiveDescendants": true,
            "liveDescendantAgentIds": descendants
        }));
    }
    let agent = state
        .agent(&args.id)
        .map_err(internal)?
        .ok_or_else(|| not_found("agent", &args.id))?;
    let pane_id = agent
        .pane_id
        .ok_or_else(|| ControlFailure::new("agent_exited", "agent has no live pane"))?;
    state.close_pane_for_user(&pane_id).map_err(internal)?;
    state.clear_last_closed_pane_for_pane(&pane_id);
    Ok(json!({
        "agentId": args.id,
        "released": true,
        "blockedByLiveDescendants": false,
        "liveDescendantAgentIds": []
    }))
}

fn artifact_list(state: &AppState, context: &ControlContext) -> ControlResult {
    let allowed = allowed_pane_ids(state, context)?;
    let artifacts = state
        .list_artifacts()
        .map_err(internal)?
        .into_iter()
        .filter(|artifact| allowed.contains(&artifact.pane_id))
        .collect::<Vec<_>>();
    Ok(json!({ "artifacts": artifacts, "count": artifacts.len() }))
}

fn artifact_open(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    if context.principal == ControlPrincipal::Remote {
        // Opening an artifact drives the Mac's own browser panel; that is a
        // write to the interactive surface, not a read.
        require_write(context)?;
    }
    let args: IdArgs = parse(arguments, "artifact.open")?;
    let artifact = state
        .list_artifacts()
        .map_err(internal)?
        .into_iter()
        .find(|artifact| artifact.id == args.id)
        .ok_or_else(|| not_found("artifact", &args.id))?;
    ensure_pane_read(state, context, &artifact.pane_id)?;
    let target = artifact
        .path
        .as_deref()
        .or(artifact.url.as_deref())
        .ok_or_else(|| ControlFailure::new("invalid_artifact", "artifact has no target"))?;
    let resolved =
        crate::control_socket::resolve_browser_target(state, &artifact.pane_id, target, None)
            .map_err(internal)?;
    state.emit(QmuxEvent::new(
        "browser.open",
        Some(artifact.pane_id.clone()),
        None,
        json!({
            "url": resolved.url,
            "sandbox": resolved.sandbox,
            "artifactId": artifact.id
        }),
    ));
    Ok(json!({
        "artifact": artifact,
        "url": resolved.url,
        "sandbox": resolved.sandbox,
        "openRequested": true
    }))
}

fn split_list(state: &AppState, context: &ControlContext) -> ControlResult {
    require_interactive(context)?;
    let allowed = allowed_pane_ids(state, context)?;
    let splits = state
        .pane_splits()
        .map_err(internal)?
        .into_iter()
        .filter(|split| {
            split
                .pane_ids
                .iter()
                .all(|pane_id| allowed.contains(pane_id))
        })
        .collect::<Vec<_>>();
    Ok(json!({ "splits": splits, "count": splits.len() }))
}

fn split_join(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    require_write(context)?;
    let args: SplitJoinArgs = parse(arguments, "split.join")?;
    if args.id == args.other {
        return Err(ControlFailure::new(
            "invalid_argument",
            "split join requires two different panes",
        ));
    }
    ensure_pane_read(state, context, &args.id)?;
    ensure_pane_read(state, context, &args.other)?;
    let panes = state.list_panes().map_err(internal)?;
    let left = panes
        .iter()
        .find(|pane| pane.id == args.id)
        .ok_or_else(|| not_found("pane", &args.id))?;
    let right = panes
        .iter()
        .find(|pane| pane.id == args.other)
        .ok_or_else(|| not_found("pane", &args.other))?;
    if left.group_id != right.group_id {
        return Err(ControlFailure::new(
            "invalid_argument",
            "a split cannot span workspaces",
        ));
    }

    let mut splits = state.pane_splits().map_err(internal)?;
    let mut consumed = HashSet::new();
    let mut pane_ids = vec![args.id.clone(), args.other.clone()];
    let mut sizes = HashMap::new();
    let mut split_id = None;
    let mut axis = PaneSplitAxis::Vertical;
    for split in &splits {
        if split.pane_ids.contains(&args.id) || split.pane_ids.contains(&args.other) {
            consumed.insert(split.id.clone());
            split_id.get_or_insert_with(|| split.id.clone());
            if split.pane_ids.contains(&args.id) {
                axis = split.axis;
            } else if consumed.len() == 1 {
                axis = split.axis;
            }
            for pane_id in &split.pane_ids {
                if !pane_ids.contains(pane_id) {
                    pane_ids.push(pane_id.clone());
                }
                let size = split
                    .sizes
                    .get(pane_id)
                    .copied()
                    .unwrap_or(1.0 / split.pane_ids.len() as f64);
                sizes.insert(pane_id.clone(), size);
            }
        }
    }
    let positions = panes
        .iter()
        .enumerate()
        .map(|(index, pane)| (pane.id.as_str(), index))
        .collect::<HashMap<_, _>>();
    pane_ids.sort_by_key(|pane_id| {
        positions
            .get(pane_id.as_str())
            .copied()
            .unwrap_or(usize::MAX)
    });
    let candidate_default = positive_size_total(&sizes) / sizes.len().max(1) as f64;
    let default_size = if candidate_default.is_finite() && candidate_default > 0.0 {
        candidate_default
    } else {
        1.0
    };
    for pane_id in &pane_ids {
        sizes.entry(pane_id.clone()).or_insert(default_size);
    }
    let sizes = allocate_split_sizes(&sizes, &pane_ids, 1.0, MIN_SPLIT_FRACTION);
    let joined = PaneSplitInfo {
        id: split_id.unwrap_or_else(|| state.next_id("split")),
        pane_ids,
        sizes,
        intent: HashMap::new(),
        axis,
        // A join rewrites membership wholesale, so there is no tree left to
        // extend: the control API produces flat splits.
        root: None,
    };
    splits.retain(|split| !consumed.contains(&split.id));
    splits.push(joined.clone());
    let persisted = state.set_pane_splits(splits).map_err(invalid_argument)?;
    let split = persisted
        .into_iter()
        .find(|split| split.id == joined.id)
        .ok_or_else(|| ControlFailure::new("invalid_argument", "split requires adjacent panes"))?;
    Ok(json!({ "split": split }))
}

fn split_leave(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    require_write(context)?;
    let args: IdArgs = parse(arguments, "split.leave")?;
    ensure_pane_read(state, context, &args.id)?;
    let mut found = false;
    let mut splits = Vec::new();
    for split in state.pane_splits().map_err(internal)? {
        let Some(segments) = remaining_split_segments(&split.pane_ids, &args.id) else {
            splits.push(split);
            continue;
        };
        found = true;
        for (segment_index, segment) in segments.into_iter().enumerate() {
            let pane_ids = segment;
            let sizes = allocate_split_sizes(&split.sizes, &pane_ids, 1.0, MIN_SPLIT_FRACTION);
            splits.push(PaneSplitInfo {
                id: if segment_index == 0 {
                    split.id.clone()
                } else {
                    state.next_id("split")
                },
                pane_ids,
                sizes,
                intent: HashMap::new(),
                axis: split.axis,
                // Carried through so leaving from an edge keeps a nested layout;
                // normalization drops the tree if the remaining panes no longer
                // match it.
                root: split.root.clone(),
            });
        }
    }
    if !found {
        return Err(ControlFailure::new(
            "split_not_found",
            format!("pane {} does not belong to a split", args.id),
        ));
    }
    let splits = state.set_pane_splits(splits).map_err(invalid_argument)?;
    Ok(json!({ "paneId": args.id, "left": true, "splits": splits }))
}

fn split_resize(state: &AppState, context: &ControlContext, arguments: Value) -> ControlResult {
    require_write(context)?;
    let args: SplitResizeArgs = parse(arguments, "split.resize")?;
    if !args.fraction.is_finite() {
        return Err(ControlFailure::new(
            "invalid_argument",
            "split fraction must be finite",
        ));
    }
    let mut splits = state.pane_splits().map_err(internal)?;
    let split = splits
        .iter_mut()
        .find(|split| split.id == args.id)
        .ok_or_else(|| not_found("split", &args.id))?;
    if !split.pane_ids.contains(&args.pane) {
        return Err(ControlFailure::new(
            "invalid_argument",
            format!("pane {} does not belong to split {}", args.pane, args.id),
        ));
    }
    // A nested split's sizes are derived from its tree, so writing this flat
    // map would be silently discarded. Refuse rather than report a no-op as a
    // success; nested layouts resize by their dividers in the app.
    if split.root.is_some() {
        return Err(ControlFailure::new(
            "invalid_argument",
            format!(
                "split {} is nested; resize its dividers in the app",
                args.id
            ),
        ));
    }
    ensure_pane_read(state, context, &args.pane)?;
    let minimum = MIN_SPLIT_FRACTION.min(1.0 / split.pane_ids.len() as f64);
    let maximum = 1.0 - minimum * (split.pane_ids.len() - 1) as f64;
    if args.fraction < minimum || args.fraction > maximum {
        return Err(ControlFailure::new(
            "invalid_argument",
            format!("split fraction must be between {minimum} and {maximum}"),
        ));
    }
    let other_pane_ids = split
        .pane_ids
        .iter()
        .filter(|pane_id| *pane_id != &args.pane)
        .cloned()
        .collect::<Vec<_>>();
    split.sizes = allocate_split_sizes(&split.sizes, &other_pane_ids, 1.0 - args.fraction, minimum);
    split.sizes.insert(args.pane.clone(), args.fraction);
    let split_id = split.id.clone();
    let persisted = state.set_pane_splits(splits).map_err(invalid_argument)?;
    let split = persisted
        .into_iter()
        .find(|split| split.id == split_id)
        .ok_or_else(|| not_found("split", &split_id))?;
    Ok(json!({ "split": split }))
}

fn request_pane_focus(state: &AppState, pane_id: &str) {
    state.touch_pane_active(pane_id);
    state.emit(QmuxEvent::new(
        "pane.focus_requested",
        Some(pane_id.to_string()),
        None,
        json!({}),
    ));
}

fn positive_size_total(sizes: &HashMap<String, f64>) -> f64 {
    sizes
        .values()
        .filter(|size| size.is_finite() && **size > 0.0)
        .sum()
}

fn remaining_split_segments(pane_ids: &[String], removed: &str) -> Option<Vec<Vec<String>>> {
    let index = pane_ids.iter().position(|pane_id| pane_id == removed)?;
    Some(
        [&pane_ids[..index], &pane_ids[index + 1..]]
            .into_iter()
            .filter(|segment| segment.len() >= 2)
            .map(<[String]>::to_vec)
            .collect(),
    )
}

fn allocate_split_sizes(
    weights: &HashMap<String, f64>,
    pane_ids: &[String],
    total: f64,
    minimum: f64,
) -> HashMap<String, f64> {
    if pane_ids.is_empty() {
        return HashMap::new();
    }
    let minimum = minimum.min(total / pane_ids.len() as f64);
    let mut pending = pane_ids
        .iter()
        .map(|pane_id| {
            let weight = weights
                .get(pane_id)
                .copied()
                .filter(|weight| weight.is_finite() && *weight > 0.0)
                .unwrap_or(1.0);
            (pane_id.clone(), weight)
        })
        .collect::<Vec<_>>();
    let mut result = HashMap::new();
    let mut remaining = total;
    while !pending.is_empty() {
        let weight_total = pending.iter().map(|(_, weight)| *weight).sum::<f64>();
        let below_minimum = pending
            .iter()
            .filter(|(_, weight)| remaining * *weight / weight_total < minimum)
            .map(|(pane_id, _)| pane_id.clone())
            .collect::<HashSet<_>>();
        if below_minimum.is_empty() {
            for (pane_id, weight) in pending {
                result.insert(pane_id, remaining * weight / weight_total);
            }
            break;
        }
        for pane_id in &below_minimum {
            result.insert(pane_id.clone(), minimum);
            remaining -= minimum;
        }
        pending.retain(|(pane_id, _)| !below_minimum.contains(pane_id));
    }
    result
}

struct PublicWaitSlot;

impl PublicWaitSlot {
    fn acquire() -> Result<Self, ControlFailure> {
        let acquired = ACTIVE_PUBLIC_WAITS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_CONCURRENT_PUBLIC_WAITS).then_some(active + 1)
            })
            .is_ok();
        if acquired {
            Ok(Self)
        } else {
            Err(ControlFailure::new(
                "too_many_waits",
                "too many qmux CLI waits are already active",
            ))
        }
    }
}

impl Drop for PublicWaitSlot {
    fn drop(&mut self) {
        ACTIVE_PUBLIC_WAITS.fetch_sub(1, Ordering::AcqRel);
    }
}

fn allowed_agent_ids(
    state: &AppState,
    context: &ControlContext,
) -> Result<HashSet<String>, ControlFailure> {
    let mut agents = state.list_agents().map_err(internal)?;
    if context.principal == ControlPrincipal::Remote {
        // A paired device is trusted with the whole app or it is not on the
        // list at all; there is no per-workspace scope to enforce.
        return Ok(agents.into_iter().map(|agent| agent.id).collect());
    }
    if context.principal == ControlPrincipal::User {
        return Ok(agents
            .into_iter()
            .filter(|agent| agent.group_id == context.workspace_id)
            .map(|agent| agent.id)
            .collect());
    }
    let caller = context
        .agent
        .as_ref()
        .ok_or_else(|| denied("agent principal has no live agent"))?;
    agents.retain(|agent| agent.group_id == caller.group_id);
    let by_parent = agents
        .iter()
        .fold(HashMap::<String, Vec<String>>::new(), |mut map, agent| {
            if let Some(parent) = agent.parent_id.as_ref() {
                map.entry(parent.clone())
                    .or_default()
                    .push(agent.id.clone());
            }
            map
        });
    let mut allowed = HashSet::from([caller.id.clone()]);
    let mut pending = VecDeque::from([caller.id.clone()]);
    while let Some(parent) = pending.pop_front() {
        for child in by_parent.get(&parent).into_iter().flatten() {
            if allowed.insert(child.clone()) {
                pending.push_back(child.clone());
            }
        }
    }
    Ok(allowed)
}

fn allowed_pane_ids(
    state: &AppState,
    context: &ControlContext,
) -> Result<HashSet<String>, ControlFailure> {
    if context.principal == ControlPrincipal::Remote {
        return Ok(state
            .list_panes()
            .map_err(internal)?
            .into_iter()
            .map(|pane| pane.id)
            .collect());
    }
    if context.principal == ControlPrincipal::User {
        return Ok(state
            .list_panes()
            .map_err(internal)?
            .into_iter()
            .filter(|pane| pane.group_id == context.workspace_id)
            .map(|pane| pane.id)
            .collect());
    }
    let allowed_agents = allowed_agent_ids(state, context)?;
    Ok(state
        .list_agents()
        .map_err(internal)?
        .into_iter()
        .filter(|agent| allowed_agents.contains(&agent.id))
        .filter_map(|agent| agent.pane_id)
        .collect())
}

fn ensure_pane_read(
    state: &AppState,
    context: &ControlContext,
    pane_id: &str,
) -> Result<(), ControlFailure> {
    if allowed_pane_ids(state, context)?.contains(pane_id) {
        Ok(())
    } else {
        Err(denied("pane is outside the caller's readable scope"))
    }
}

fn ensure_agent_read(
    state: &AppState,
    context: &ControlContext,
    agent_id: &str,
) -> Result<(), ControlFailure> {
    if allowed_agent_ids(state, context)?.contains(agent_id) {
        Ok(())
    } else {
        Err(denied("agent is outside the caller's readable scope"))
    }
}

fn find_pane(state: &AppState, pane_id: &str) -> Result<crate::state::PaneInfo, ControlFailure> {
    state
        .list_panes()
        .map_err(internal)?
        .into_iter()
        .find(|pane| pane.id == pane_id)
        .ok_or_else(|| not_found("pane", pane_id))
}

fn parse<T: for<'de> Deserialize<'de>>(value: Value, operation: &str) -> Result<T, ControlFailure> {
    serde_json::from_value(value).map_err(|error| {
        ControlFailure::new(
            "invalid_argument",
            format!("invalid {operation} arguments: {error}"),
        )
    })
}

fn ensure_no_arguments(value: Value, operation: &str) -> Result<(), ControlFailure> {
    if value.is_null() || value.as_object().is_some_and(serde_json::Map::is_empty) {
        Ok(())
    } else {
        Err(ControlFailure::new(
            "invalid_argument",
            format!("{operation} does not accept arguments"),
        ))
    }
}

fn denied(message: impl Into<String>) -> ControlFailure {
    ControlFailure::new("permission_denied", message)
}

/// Gate for operations that change state. Agents never pass (their writes go
/// through the scoped MCP paths); a read-only remote device never passes.
fn require_write(context: &ControlContext) -> Result<(), ControlFailure> {
    match context.principal {
        ControlPrincipal::User => Ok(()),
        ControlPrincipal::Remote => {
            if context.read_only {
                Err(denied("this device is paired read-only"))
            } else {
                Ok(())
            }
        }
        ControlPrincipal::Agent => Err(denied(
            "this operation requires an interactive user credential",
        )),
    }
}

/// Gate for reads that are interactive-surface concerns (split layout):
/// people and paired devices see them, agents do not.
fn require_interactive(context: &ControlContext) -> Result<(), ControlFailure> {
    match context.principal {
        ControlPrincipal::User | ControlPrincipal::Remote => Ok(()),
        ControlPrincipal::Agent => Err(denied(
            "this operation requires an interactive user credential",
        )),
    }
}

fn not_found(kind: &str, id: &str) -> ControlFailure {
    ControlFailure::new(format_code(kind), format!("{kind} {id} was not found"))
}

fn format_code(kind: &str) -> &'static str {
    match kind {
        "pane" => "pane_not_found",
        "agent" => "agent_not_found",
        "workspace" => "workspace_not_found",
        "artifact" => "artifact_not_found",
        "split" => "split_not_found",
        "adapter" => "adapter_not_found",
        _ => "not_found",
    }
}

fn invalid_argument(message: String) -> ControlFailure {
    ControlFailure::new("invalid_argument", message)
}

fn internal(message: String) -> ControlFailure {
    ControlFailure::new("internal_error", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::session::RemoteSession;
    use crate::state::test_support;
    use crate::workspace::AgentStatus;

    /// Two workspaces, one pane each, so scope differences are observable.
    fn remote_fixture(name: &str) -> AppState {
        let state = AppState::new(test_support::config(std::path::PathBuf::from(format!(
            "/tmp/qmux-control-remote-{name}"
        ))));
        state
            .insert_group_after(test_support::group("group-1"), None)
            .unwrap();
        state
            .insert_group_after(test_support::group("group-2"), Some("group-1"))
            .unwrap();
        state
            .insert_pane(test_support::pane_runtime("pane-1", "group-1"))
            .unwrap();
        state
            .insert_pane(test_support::pane_runtime("pane-2", "group-2"))
            .unwrap();
        state
    }

    fn call(state: &AppState, session: &RemoteSession, operation: &str, arguments: Value) -> Value {
        handle_remote_call(state, session, operation, arguments)
    }

    #[test]
    fn remote_principal_reads_every_workspace_and_pane() {
        let state = remote_fixture("read-scope");
        let session = RemoteSession::new("iphone", false);

        let workspaces = call(&state, &session, "workspace.list", Value::Null);
        assert_eq!(
            workspaces["ok"], true,
            "workspace.list failed: {workspaces}"
        );
        assert_eq!(workspaces["result"]["count"], 2);

        let panes = call(&state, &session, "pane.list", Value::Null);
        assert_eq!(panes["ok"], true);
        assert_eq!(
            panes["result"]["count"], 2,
            "a paired device sees panes across workspaces: {panes}"
        );

        let snapshot = call(&state, &session, "context", Value::Null);
        assert_eq!(snapshot["ok"], true);
        assert_eq!(snapshot["result"]["principal"], "remote");
        assert_eq!(
            snapshot["result"]["capabilities"]["read"],
            "every workspace, pane, and agent"
        );
    }

    #[test]
    fn session_focus_moves_the_current_pane_across_workspaces() {
        let state = remote_fixture("focus");
        let session = RemoteSession::new("iphone", false);

        // Unset focus falls back deterministically to a live pane.
        let current = call(&state, &session, "pane.current", Value::Null);
        assert_eq!(current["ok"], true, "pane.current failed: {current}");

        let focus = call(&state, &session, "session.focus", json!({ "id": "pane-2" }));
        assert_eq!(focus["ok"], true, "session.focus failed: {focus}");
        assert_eq!(focus["result"]["focusPane"], "pane-2");

        let current = call(&state, &session, "pane.current", Value::Null);
        assert_eq!(current["result"]["pane"]["id"], "pane-2");
        // The context's workspace follows the focus pane, so creation and
        // placement default to where the device is looking.
        let snapshot = call(&state, &session, "context", Value::Null);
        assert_eq!(snapshot["result"]["workspace"]["id"], "group-2");

        let missing = call(&state, &session, "session.focus", json!({ "id": "pane-9" }));
        assert_eq!(missing["ok"], false);
        assert_eq!(missing["error"]["code"], "pane_not_found");

        // The socket principals never see the op.
        let via_socket = handle_call(
            &state,
            "pane-1",
            true,
            "session.focus",
            json!({"id": "pane-1"}),
        );
        assert_eq!(via_socket["ok"], false);
        assert_eq!(via_socket["error"]["code"], "invalid_operation");
    }

    #[test]
    fn a_stale_focus_falls_back_to_a_live_pane() {
        let state = remote_fixture("stale-focus");
        let session = RemoteSession::new("iphone", false);
        session.set_focus_pane("pane-2".to_string());
        state.remove_pane("pane-2").unwrap();

        let current = call(&state, &session, "pane.current", Value::Null);
        assert_eq!(current["ok"], true, "stale focus must fall back: {current}");
        assert_eq!(current["result"]["pane"]["id"], "pane-1");
    }

    #[test]
    fn read_only_devices_read_everything_and_write_nothing() {
        let state = remote_fixture("read-only");
        let session = RemoteSession::new("ipad", true);

        let panes = call(&state, &session, "pane.list", Value::Null);
        assert_eq!(panes["result"]["count"], 2);
        let read = call(
            &state,
            &session,
            "pane.read",
            json!({ "id": "pane-1", "lines": 5 }),
        );
        assert_eq!(read["ok"], true, "reads must pass: {read}");
        // Splits are interactive-surface reads: visible to devices.
        let splits = call(&state, &session, "split.list", Value::Null);
        assert_eq!(splits["ok"], true, "split.list is a read: {splits}");

        for (operation, arguments) in [
            ("pane.send", json!({ "id": "pane-1", "text": "rm -rf /" })),
            ("pane.run", json!({ "id": "pane-1", "text": "ls" })),
            ("pane.rename", json!({ "id": "pane-1", "name": "x" })),
            ("pane.close", json!({ "id": "pane-1" })),
            ("pane.create", json!({})),
            ("workspace.create", json!({})),
            ("workspace.rename", json!({ "id": "group-1", "name": "x" })),
            ("agent.start", json!({})),
            ("agent.prompt", json!({ "id": "agent-1", "text": "hi" })),
            ("agent.release", json!({ "id": "agent-1" })),
            (
                "agent.submit",
                json!({ "id": "agent-1", "text": "hi", "mode": "send" }),
            ),
            (
                "agent.permission",
                json!({ "id": "agent-1", "action": "approve" }),
            ),
            ("agent.queue.remove", json!({ "id": "agent-1", "index": 0 })),
            (
                "agent.queue.reorder",
                json!({ "id": "agent-1", "fromIndex": 0, "toIndex": 1 }),
            ),
            ("agent.queue.sendNext", json!({ "id": "agent-1" })),
            (
                "agent.queue.pause",
                json!({ "id": "agent-1", "index": 0, "pauseAfter": true }),
            ),
            ("agent.queue.unpause", json!({ "id": "agent-1" })),
        ] {
            let response = call(&state, &session, operation, arguments);
            assert_eq!(response["ok"], false, "{operation} must be denied");
            assert_eq!(
                response["error"]["code"], "permission_denied",
                "{operation} must be denied read-only, got: {response}"
            );
            assert!(
                response["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("read-only"),
                "{operation}: {response}"
            );
        }
    }

    fn agent_fixture(id: &str, adapter: &str, pane_id: &str, status: AgentStatus) -> AgentInfo {
        AgentInfo {
            id: id.to_string(),
            group_id: "group-1".to_string(),
            adapter: adapter.to_string(),
            worktree_dir: "/tmp/work".to_string(),
            branch: None,
            active_workspace: None,
            pane_id: Some(pane_id.to_string()),
            orphaned_queue_pane_id: None,
            session_id: Some("session-abc".to_string()),
            transcript_path: None,
            status,
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

    #[test]
    fn adapter_policy_serves_the_one_true_table_and_fork_gates() {
        let state = remote_fixture("adapter-policy");
        state
            .insert_agent(agent_fixture(
                "agent-1",
                "claude",
                "pane-1",
                AgentStatus::Running,
            ))
            .unwrap();
        let mut pi_agent = agent_fixture("agent-2", "pi", "pane-2", AgentStatus::Idle);
        pi_agent.group_id = "group-2".to_string();
        state.insert_agent(pi_agent).unwrap();
        let session = RemoteSession::new("iphone", false);

        let policy = call(
            &state,
            &session,
            "adapter.policy",
            json!({ "agent": "agent-1" }),
        );
        assert_eq!(policy["ok"], true, "adapter.policy failed: {policy}");
        let result = &policy["result"];
        assert_eq!(result["adapterId"], "claude");
        assert_eq!(result["policy"]["readyStatuses"][0], "awaitingInput");
        assert_eq!(result["policy"]["permissionActions"][0]["input"], "y");
        assert_eq!(result["policy"]["permissionActions"][1]["id"], "deny");
        assert_eq!(result["canFork"], true);
        assert_eq!(result["agentStatus"], "running");

        // Pi's fork predicate is agent-dependent: no transcript or native
        // leaf id means no fork, even with supports_fork true.
        let pi = call(
            &state,
            &session,
            "adapter.policy",
            json!({ "agent": "agent-2" }),
        );
        assert_eq!(pi["result"]["supportsFork"], true);
        assert_eq!(pi["result"]["canFork"], false);

        let by_adapter = call(
            &state,
            &session,
            "adapter.policy",
            json!({ "adapter": "codex" }),
        );
        assert_eq!(
            by_adapter["result"]["policy"]["permissionActions"],
            json!([])
        );
        let missing = call(
            &state,
            &session,
            "adapter.policy",
            json!({ "adapter": "nope" }),
        );
        assert_eq!(missing["error"]["code"], "adapter_not_found");
        let both = call(
            &state,
            &session,
            "adapter.policy",
            json!({ "agent": "agent-1", "adapter": "claude" }),
        );
        assert_eq!(both["error"]["code"], "invalid_argument");
    }

    #[test]
    fn the_queue_ops_wrap_the_composers_own_paths() {
        let state = remote_fixture("queue-ops");
        state
            .insert_agent(agent_fixture(
                "agent-1",
                "claude",
                "pane-1",
                AgentStatus::Running,
            ))
            .unwrap();
        let session = RemoteSession::new("iphone", false);

        // A running claude queues per its own policy.
        for text in ["first", "second"] {
            let queued = call(
                &state,
                &session,
                "agent.submit",
                json!({ "id": "agent-1", "text": text, "mode": "queue" }),
            );
            assert_eq!(queued["ok"], true, "queue submit failed: {queued}");
            assert_eq!(queued["result"]["queued"], true);
        }
        let listed = call(
            &state,
            &session,
            "agent.queue.list",
            json!({ "id": "agent-1" }),
        );
        assert_eq!(listed["result"]["count"], 2);
        let first_id = listed["result"]["queuedTurns"][0]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let reordered = call(
            &state,
            &session,
            "agent.queue.reorder",
            json!({ "id": "agent-1", "fromIndex": 0, "toIndex": 1, "expectedId": first_id }),
        );
        assert_eq!(reordered["ok"], true, "reorder failed: {reordered}");
        let listed = call(
            &state,
            &session,
            "agent.queue.list",
            json!({ "id": "agent-1" }),
        );
        assert_eq!(listed["result"]["queuedTurns"][1]["text"], "first");

        let paused = call(
            &state,
            &session,
            "agent.queue.pause",
            json!({ "id": "agent-1", "index": 0, "pauseAfter": true }),
        );
        assert_eq!(paused["ok"], true, "pause failed: {paused}");
        assert_eq!(paused["result"]["queuedTurns"][0]["pauseAfter"], true);

        let removed = call(
            &state,
            &session,
            "agent.queue.remove",
            json!({ "id": "agent-1", "index": 1 }),
        );
        assert_eq!(removed["ok"], true, "remove failed: {removed}");
        assert_eq!(removed["result"]["removedTurn"], "first");
        assert_eq!(removed["result"]["pendingTurns"], 1);

        // A stale expectation is refused rather than acting on the wrong turn.
        let stale = call(
            &state,
            &session,
            "agent.queue.remove",
            json!({ "id": "agent-1", "index": 0, "expectedData": "not this" }),
        );
        assert_eq!(stale["ok"], false);
        assert_eq!(stale["error"]["code"], "invalid_argument");

        let mode_error = call(
            &state,
            &session,
            "agent.submit",
            json!({ "id": "agent-1", "text": "x", "mode": "yeet" }),
        );
        assert_eq!(mode_error["error"]["code"], "invalid_argument");

        let auto_error = call(
            &state,
            &session,
            "agent.submit",
            json!({ "id": "agent-1", "text": "x", "mode": "auto" }),
        );
        assert_eq!(auto_error["error"]["code"], "invalid_argument");

        let missing_mode = call(
            &state,
            &session,
            "agent.submit",
            json!({ "id": "agent-1", "text": "x" }),
        );
        assert_eq!(missing_mode["error"]["code"], "invalid_argument");
    }

    #[test]
    fn permission_answers_use_the_adapters_own_keystrokes() {
        let state = remote_fixture("permission");
        state
            .insert_agent(agent_fixture(
                "agent-1",
                "claude",
                "pane-1",
                AgentStatus::AwaitingPermission,
            ))
            .unwrap();
        let mut running = agent_fixture("agent-2", "claude", "pane-2", AgentStatus::Running);
        running.group_id = "group-2".to_string();
        state.insert_agent(running).unwrap();
        let session = RemoteSession::new("iphone", false);

        let approved = call(
            &state,
            &session,
            "agent.permission",
            json!({ "id": "agent-1", "action": "approve" }),
        );
        assert_eq!(approved["ok"], true, "approve failed: {approved}");
        assert_eq!(approved["result"]["answered"], true);
        assert_eq!(approved["result"]["paneId"], "pane-1");

        // A second device observing the same status snapshot cannot type a
        // conflicting answer before the adapter reports the first decision.
        let duplicate = call(
            &state,
            &session,
            "agent.permission",
            json!({ "id": "agent-1", "action": "deny" }),
        );
        assert_eq!(duplicate["error"]["code"], "not_awaiting_permission");

        // Only a prompt that is actually waiting may be answered: anything
        // else would type stray keys into a working agent.
        let not_waiting = call(
            &state,
            &session,
            "agent.permission",
            json!({ "id": "agent-2", "action": "approve" }),
        );
        assert_eq!(not_waiting["error"]["code"], "not_awaiting_permission");

        let unknown = call(
            &state,
            &session,
            "agent.permission",
            json!({ "id": "agent-1", "action": "maybe" }),
        );
        assert_eq!(unknown["error"]["code"], "invalid_argument");
        assert!(
            unknown["error"]["message"]
                .as_str()
                .unwrap()
                .contains("approve"),
            "the error should name the valid actions: {unknown}"
        );

        // Leaving and re-entering the permission state opens a new prompt.
        state
            .set_agent_status("agent-1", AgentStatus::Running)
            .unwrap();
        state
            .set_agent_status("agent-1", AgentStatus::AwaitingPermission)
            .unwrap();
        let next_prompt = call(
            &state,
            &session,
            "agent.permission",
            json!({ "id": "agent-1", "action": "deny" }),
        );
        assert_eq!(next_prompt["ok"], true, "new prompt failed: {next_prompt}");
    }

    #[test]
    fn a_writable_device_reaches_other_workspaces() {
        let state = remote_fixture("write-scope");
        let session = RemoteSession::new("iphone", false);

        // Renaming a workspace the focus is NOT in: allowed for a device,
        // still refused for the interactive pane credential.
        session.set_focus_pane("pane-1".to_string());
        let renamed = call(
            &state,
            &session,
            "workspace.rename",
            json!({ "id": "group-2", "name": "renamed" }),
        );
        assert_eq!(renamed["ok"], true, "remote rename failed: {renamed}");
        // rename_group records the user's name as an override; the base name
        // stays the id.
        assert_eq!(renamed["result"]["workspace"]["nameOverride"], "renamed");

        let sent = call(
            &state,
            &session,
            "pane.send",
            json!({ "id": "pane-2", "text": "echo hi" }),
        );
        assert_eq!(sent["ok"], true, "cross-workspace send failed: {sent}");
    }

    #[test]
    fn failures_have_stable_codes_and_details() {
        let failure = ControlFailure {
            code: "denied",
            message: "no".into(),
            details: json!({ "scope": "workspace" }),
        };
        assert_eq!(failure.code, "denied");
        assert_eq!(failure.details["scope"], "workspace");
    }

    #[test]
    fn public_arguments_reject_unknown_fields() {
        let error = parse::<IdArgs>(json!({ "id": "pane-1", "idd": "typo" }), "pane.get")
            .err()
            .expect("unknown fields must fail closed");
        assert_eq!(error.code, "invalid_argument");
        assert!(error.message.contains("unknown field `idd`"));
        assert!(ensure_no_arguments(json!({ "unexpected": true }), "pane.list").is_err());
    }

    #[test]
    fn removing_a_middle_split_pane_preserves_each_contiguous_side() {
        let pane_ids = ["a", "b", "c", "d", "e"].map(str::to_string).to_vec();
        assert_eq!(
            remaining_split_segments(&pane_ids, "c").unwrap(),
            vec![
                vec!["a".to_string(), "b".to_string()],
                vec!["d".to_string(), "e".to_string()]
            ]
        );
        assert!(remaining_split_segments(&pane_ids, "missing").is_none());
    }

    #[test]
    fn split_allocation_preserves_weight_while_flooring_small_members() {
        let pane_ids = ["a", "b", "c"].map(str::to_string).to_vec();
        let weights = HashMap::from([
            ("a".to_string(), 0.85),
            ("b".to_string(), 0.10),
            ("c".to_string(), 0.05),
        ]);
        let sizes = allocate_split_sizes(&weights, &pane_ids, 1.0, MIN_SPLIT_FRACTION);
        assert!((sizes.values().sum::<f64>() - 1.0).abs() < 1e-9);
        assert!(sizes.values().all(|size| *size >= MIN_SPLIT_FRACTION));
        assert!(sizes["a"] > sizes["b"]);
    }
}
