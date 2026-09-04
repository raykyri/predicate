//! Pane-less research runs for Claude, Codex, and Grok.

use crate::adapters::claude::ClaudeAdapter;
use crate::adapters::codex::CodexAdapter;
use crate::adapters::grok::{GrokAdapter, research_session_transcript_path};
use crate::adapters::new_uuid_v4;
use crate::claude_sdk::{
    self, ClaudeSdkSession, ClaudeSdkSpawnSpec, SdkMessage, assistant_message_is_end_turn,
    research_can_use_tool, stream_event_is_end_turn, stream_event_text_delta,
};
use crate::headless_process::{
    JsonlProcess, JsonlReceive, reconcile_session_id, validate_session_id,
};
use crate::persistence;
use crate::research::ResearchNode;
use crate::state::{AppState, now_millis};
use crate::transcript::{Turn, TurnBlock};
use crate::workspace::{PrepareAgentWorkspaceRequest, prepare_agent_workspace};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

struct SessionSlot {
    pid: Option<u32>,
    interrupt: std::sync::mpsc::Sender<()>,
}

static SESSIONS: OnceLock<Mutex<HashMap<String, SessionSlot>>> = OnceLock::new();

fn sessions() -> &'static Mutex<HashMap<String, SessionSlot>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn should_use_research_sdk(state: &AppState, adapter: &str) -> bool {
    matches!(adapter, "codex" | "grok")
        || (adapter == "claude"
            && persistence::research_sdk_harness_enabled(&state.config().workspace_root))
}

pub fn session_registered(node_id: &str) -> bool {
    sessions()
        .lock()
        .is_ok_and(|sessions| sessions.contains_key(node_id))
}

pub fn wait_for_session_stop(node_id: &str, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while session_registered(node_id) && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    !session_registered(node_id)
}

pub fn interrupt_session(node_id: &str) -> bool {
    let Ok(sessions) = sessions().lock() else {
        return false;
    };
    sessions
        .get(node_id)
        .is_some_and(|slot| slot.interrupt.send(()).is_ok())
}

pub fn kill_all_sessions() {
    let slots: Vec<SessionSlot> = sessions()
        .lock()
        .map(|mut map| map.drain().map(|(_, slot)| slot).collect())
        .unwrap_or_default();
    for slot in slots {
        let _ = slot.interrupt.send(());
        if let Some(pid) = slot.pid.filter(|pid| *pid > 0) {
            claude_sdk::terminate_process_tree(pid);
        }
    }
}

pub fn launch(
    state: &AppState,
    node: &ResearchNode,
    workspace: &crate::workspace::GroupInfo,
    prompt: String,
    resume: Option<String>,
    fork: bool,
) -> Result<ResearchNode, String> {
    let resume = resume
        .map(|session_id| validate_session_id(&session_id, &node.adapter))
        .transpose()
        .map_err(|err| {
            let _ = state.fail_research_node(&node.id, err.clone());
            err
        })?;
    match node.adapter.as_str() {
        "claude" => launch_claude(state, node, workspace, prompt, resume, fork),
        "codex" | "grok" => launch_jsonl(state, node, workspace, prompt, resume, fork),
        adapter => Err(format!("'{adapter}' is not a supported research agent")),
    }
}

fn launch_claude(
    state: &AppState,
    node: &ResearchNode,
    workspace: &crate::workspace::GroupInfo,
    prompt: String,
    resume: Option<String>,
    fork: bool,
) -> Result<ResearchNode, String> {
    let adapter = ClaudeAdapter::new(state.config());
    let binary = adapter.ensure_binary_for_sdk().map_err(|err| {
        let _ = state.fail_research_node(&node.id, err.clone());
        err
    })?;
    let version = claude_sdk::probe_claude_version(&binary).map_err(|err| {
        let _ = state.fail_research_node(&node.id, err.clone());
        err
    })?;
    eprintln!("qmux: research SDK using Claude Code {}", version.display());

    let agent = prepare_agent_workspace(
        state,
        PrepareAgentWorkspaceRequest {
            group_id: Some(workspace.id.clone()),
            base_repo: Some(workspace.dir.clone()),
            base_ref: Some("HEAD".to_string()),
            adapter: "claude".to_string(),
            model: node.model.clone(),
            effort: node.effort.clone(),
            use_worktree: false,
        },
    )
    .map_err(|err| {
        let _ = state.fail_research_node(&node.id, err.clone());
        err
    })?;

    let bound = state
        .bind_research_node_harness(&node.id, &agent)
        .map_err(|err| {
            let _ = state.fail_research_node(&node.id, err.clone());
            state.prune_agent(&agent.id);
            err
        })?;
    if bound.status.is_terminal() {
        if let Err(err) = state.finish_research_sdk_run(&node.id, &agent.id, false, None) {
            eprintln!("qmux: failed to preserve pre-launch research cancellation: {err}");
        }
        return Ok(bound);
    }

    let (interrupt_tx, interrupt_rx) = std::sync::mpsc::channel();
    {
        let mut map = match sessions().lock() {
            Ok(map) => map,
            Err(_) => {
                let err = "research SDK session table poisoned".to_string();
                let _ = state.fail_research_node(&node.id, err.clone());
                state.prune_agent(&agent.id);
                return Err(err);
            }
        };
        map.insert(
            node.id.clone(),
            SessionSlot {
                pid: None,
                interrupt: interrupt_tx,
            },
        );
    }
    let current = match state.research_node(&node.id) {
        Ok(current) => current,
        Err(err) => {
            unregister(&node.id);
            state.prune_agent(&agent.id);
            return Err(err);
        }
    };
    if current.status.is_terminal() {
        if let Err(err) = state.finish_research_sdk_run(&node.id, &agent.id, false, None) {
            eprintln!("qmux: failed to preserve pre-thread research cancellation: {err}");
        }
        unregister(&node.id);
        return Ok(current);
    }

    let log_dir = state
        .config()
        .workspace_root
        .join(".qmux")
        .join("research-logs");
    let stderr_log = log_dir.join(format!("{}.log", node.id));
    let spec = ClaudeSdkSpawnSpec {
        binary,
        cwd: PathBuf::from(&workspace.dir),
        model: node.model.clone(),
        effort: node.effort.clone(),
        resume,
        fork,
        stderr_log,
    };
    let runtime_state = state.clone();
    let node_id = node.id.clone();
    let agent_id = agent.id.clone();
    let prompt = prompt;
    let workspace_dir = PathBuf::from(&workspace.dir);
    let claude_version = version.display();

    thread::Builder::new()
        .name(format!("qmux-research-sdk-{node_id}"))
        .spawn(move || {
            run_session(
                runtime_state,
                node_id,
                agent_id,
                prompt,
                spec,
                workspace_dir,
                interrupt_rx,
                claude_version,
            );
        })
        .map_err(|err| {
            let err = format!("failed to start research SDK thread: {err}");
            if let Err(snapshot_err) =
                state.finish_research_sdk_run(&node.id, &agent.id, false, Some(err.clone()))
            {
                eprintln!("qmux: failed to preserve research thread-start failure: {snapshot_err}");
            }
            unregister(&node.id);
            err
        })?;

    state.research_node(&node.id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JsonlFlavor {
    Codex,
    Grok,
}

impl JsonlFlavor {
    fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Grok => "Grok",
        }
    }
}

fn launch_jsonl(
    state: &AppState,
    node: &ResearchNode,
    workspace: &crate::workspace::GroupInfo,
    prompt: String,
    resume: Option<String>,
    fork: bool,
) -> Result<ResearchNode, String> {
    let flavor = match node.adapter.as_str() {
        "codex" => JsonlFlavor::Codex,
        "grok" => JsonlFlavor::Grok,
        adapter => return Err(format!("'{adapter}' has no JSONL research runtime")),
    };
    if fork && resume.as_deref().map(str::trim).is_none_or(str::is_empty) {
        let err = format!(
            "{} research parent has no session id to fork",
            flavor.label()
        );
        let _ = state.fail_research_node(&node.id, err.clone());
        return Err(err);
    }
    let binary = match flavor {
        JsonlFlavor::Codex => CodexAdapter::new(state.config()).ensure_binary(),
        JsonlFlavor::Grok => GrokAdapter::new(state.config()).ensure_binary(),
    }
    .map_err(|err| {
        let _ = state.fail_research_node(&node.id, err.clone());
        err
    })?;
    let cwd = PathBuf::from(&workspace.dir);
    let grok_session_id = (flavor == JsonlFlavor::Grok)
        .then(new_uuid_v4)
        .transpose()
        .map_err(|err| {
            let _ = state.fail_research_node(&node.id, err.clone());
            err
        })?;

    let mut agent = prepare_agent_workspace(
        state,
        PrepareAgentWorkspaceRequest {
            group_id: Some(workspace.id.clone()),
            base_repo: Some(workspace.dir.clone()),
            base_ref: Some("HEAD".to_string()),
            adapter: node.adapter.clone(),
            model: node.model.clone(),
            effort: node.effort.clone(),
            use_worktree: false,
        },
    )
    .map_err(|err| {
        let _ = state.fail_research_node(&node.id, err.clone());
        err
    })?;
    if let Some(session_id) = grok_session_id.as_deref() {
        agent.session_id = Some(session_id.to_string());
        agent.transcript_path = research_session_transcript_path(&cwd, session_id)
            .map(|path| path.display().to_string());
        state.update_agent(agent.clone()).map_err(|err| {
            let _ = state.fail_research_node(&node.id, err.clone());
            state.prune_agent(&agent.id);
            err
        })?;
    }

    let bound = state
        .bind_research_node_harness(&node.id, &agent)
        .map_err(|err| {
            let _ = state.fail_research_node(&node.id, err.clone());
            state.prune_agent(&agent.id);
            err
        })?;
    if bound.status.is_terminal() {
        let _ = state.finish_research_sdk_run(&node.id, &agent.id, false, None);
        return Ok(bound);
    }

    let (interrupt_tx, interrupt_rx) = std::sync::mpsc::channel();
    {
        let mut map = match sessions().lock() {
            Ok(map) => map,
            Err(_) => {
                let err = "research SDK session table poisoned".to_string();
                let _ = state.fail_research_node(&node.id, err.clone());
                state.prune_agent(&agent.id);
                return Err(err);
            }
        };
        map.insert(
            node.id.clone(),
            SessionSlot {
                pid: None,
                interrupt: interrupt_tx,
            },
        );
    }
    let current = state.research_node(&node.id).map_err(|err| {
        unregister(&node.id);
        state.prune_agent(&agent.id);
        err
    })?;
    if current.status.is_terminal() {
        let _ = state.finish_research_sdk_run(&node.id, &agent.id, false, None);
        unregister(&node.id);
        return Ok(current);
    }

    let stderr_log = state
        .config()
        .workspace_root
        .join(".qmux")
        .join("research-logs")
        .join(format!("{}.log", node.id));
    let args = match flavor {
        JsonlFlavor::Codex => build_codex_exec_args(
            &prompt,
            node.model.as_deref(),
            node.effort.as_deref(),
            resume.as_deref(),
            fork,
        ),
        JsonlFlavor::Grok => build_grok_headless_args(
            &cwd,
            &prompt,
            node.model.as_deref(),
            node.effort.as_deref(),
            resume.as_deref(),
            fork,
            grok_session_id
                .as_deref()
                .expect("Grok session id was minted"),
        ),
    };
    let runtime_state = state.clone();
    let node_id = node.id.clone();
    let agent_id = agent.id.clone();
    let initial_session_id = grok_session_id;
    thread::Builder::new()
        .name(format!("qmux-research-jsonl-{node_id}"))
        .spawn(move || {
            run_jsonl_session(
                runtime_state,
                node_id,
                agent_id,
                flavor,
                binary,
                args,
                cwd,
                stderr_log,
                initial_session_id,
                interrupt_rx,
            );
        })
        .map_err(|err| {
            let err = format!("failed to start {} research thread: {err}", flavor.label());
            let _ = state.finish_research_sdk_run(&node.id, &agent.id, false, Some(err.clone()));
            unregister(&node.id);
            err
        })?;
    state.research_node(&node.id)
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn build_codex_exec_args(
    prompt: &str,
    model: Option<&str>,
    effort: Option<&str>,
    resume: Option<&str>,
    fork: bool,
) -> Vec<String> {
    let mut args = vec![
        "--search".to_string(),
        "--disable".to_string(),
        "hooks".to_string(),
        "--ask-for-approval".to_string(),
        "never".to_string(),
        "exec".to_string(),
    ];
    if fork {
        args.push("fork".to_string());
    }
    args.extend([
        "--json".to_string(),
        "--strict-config".to_string(),
        "--skip-git-repo-check".to_string(),
        "--ignore-user-config".to_string(),
        "--ignore-rules".to_string(),
    ]);
    if fork {
        args.extend(["-c".to_string(), "sandbox_mode=\"read-only\"".to_string()]);
    } else {
        args.extend(["--sandbox".to_string(), "read-only".to_string()]);
    }
    if let Some(model) = model.map(str::trim).filter(|value| !value.is_empty()) {
        args.extend(["--model".to_string(), model.to_string()]);
    }
    if let Some(effort) = effort.map(str::trim).filter(|value| !value.is_empty()) {
        args.extend([
            "-c".to_string(),
            format!("model_reasoning_effort={}", toml_string(effort)),
        ]);
    }
    if fork {
        if let Some(session_id) = resume.map(str::trim).filter(|value| !value.is_empty()) {
            args.push(session_id.to_string());
        }
    }
    args.extend(["--".to_string(), prompt.to_string()]);
    args
}

fn build_grok_headless_args(
    cwd: &std::path::Path,
    prompt: &str,
    model: Option<&str>,
    effort: Option<&str>,
    resume: Option<&str>,
    fork: bool,
    session_id: &str,
) -> Vec<String> {
    let mut args = vec![
        "--no-auto-update".to_string(),
        "--cwd".to_string(),
        cwd.display().to_string(),
        "--output-format".to_string(),
        "streaming-messages-json".to_string(),
        "--include-partial-messages".to_string(),
        "--permission-mode".to_string(),
        "dontAsk".to_string(),
        "--sandbox".to_string(),
        "read-only".to_string(),
        "--tools".to_string(),
        "read_file,grep,list_dir,web_search,web_fetch".to_string(),
        "--no-subagents".to_string(),
    ];
    // Permission rules use Claude-compatible prefixes, which are distinct
    // from the internal ids accepted by `--tools`. List/search are covered by
    // Read/Grep; web_search is intrinsically read-only in Grok.
    for tool in ["Read", "Grep", "WebFetch"] {
        args.extend(["--allow".to_string(), tool.to_string()]);
    }
    for tool in ["Bash", "Edit", "Write", "MCPTool"] {
        args.extend(["--deny".to_string(), tool.to_string()]);
    }
    if let Some(model) = model.map(str::trim).filter(|value| !value.is_empty()) {
        args.extend(["--model".to_string(), model.to_string()]);
    }
    if let Some(effort) = effort.map(str::trim).filter(|value| !value.is_empty()) {
        args.extend(["--reasoning-effort".to_string(), effort.to_string()]);
    }
    if fork {
        if let Some(parent) = resume.map(str::trim).filter(|value| !value.is_empty()) {
            args.extend([
                "--resume".to_string(),
                parent.to_string(),
                "--fork-session".to_string(),
            ]);
        }
    }
    args.extend([
        "--session-id".to_string(),
        session_id.to_string(),
        "-p".to_string(),
        prompt.to_string(),
    ]);
    args
}

#[allow(clippy::too_many_arguments)]
fn run_jsonl_session(
    state: AppState,
    node_id: String,
    agent_id: String,
    flavor: JsonlFlavor,
    binary: String,
    args: Vec<String>,
    cwd: PathBuf,
    stderr_log: PathBuf,
    mut session_id: Option<String>,
    interrupt_rx: std::sync::mpsc::Receiver<()>,
) {
    let mut process = match JsonlProcess::spawn(&binary, &args, &cwd, &stderr_log, flavor.label()) {
        Ok(process) => process,
        Err(err) => {
            finish_jsonl_failed(&state, &node_id, &agent_id, flavor, err, &stderr_log);
            unregister(&node_id);
            return;
        }
    };
    set_pid(&node_id, Some(process.pid()));
    let transcript_path = session_id.as_deref().and_then(|id| {
        (flavor == JsonlFlavor::Grok)
            .then(|| research_session_transcript_path(&cwd, id))
            .flatten()
            .map(|path| path.display().to_string())
    });
    if session_id.is_some() {
        if let Err(err) = state.record_research_sdk_session(
            &node_id,
            &agent_id,
            session_id.clone(),
            transcript_path,
        ) {
            process.kill();
            finish_jsonl_failed(&state, &node_id, &agent_id, flavor, err, &stderr_log);
            unregister(&node_id);
            return;
        }
    }

    let mut mapper = TurnMapper::new(agent_id.clone());
    let mut completed = false;
    let mut reported_error: Option<String> = None;
    loop {
        match interrupt_rx.try_recv() {
            Ok(()) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                process.kill();
                let _ = state.finish_research_sdk_run(&node_id, &agent_id, false, None);
                unregister(&node_id);
                return;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        match process.recv_timeout(Duration::from_millis(100)) {
            Ok(JsonlReceive::Timeout) => continue,
            Ok(JsonlReceive::Eof) => break,
            Err(err) => {
                process.kill();
                finish_jsonl_failed(&state, &node_id, &agent_id, flavor, err, &stderr_log);
                unregister(&node_id);
                return;
            }
            Ok(JsonlReceive::Value(value)) => {
                let outcome = match flavor {
                    JsonlFlavor::Codex => handle_codex_event(
                        &state,
                        &node_id,
                        &agent_id,
                        &mut mapper,
                        &mut session_id,
                        value,
                    ),
                    JsonlFlavor::Grok => handle_grok_event(
                        &state,
                        &node_id,
                        &agent_id,
                        &cwd,
                        &mut mapper,
                        &mut session_id,
                        value,
                    ),
                };
                match outcome {
                    Ok(EventOutcome::Continue) => {}
                    Ok(EventOutcome::Complete) => completed = true,
                    Ok(EventOutcome::Failed(err)) => reported_error = Some(err),
                    Err(err) => {
                        process.kill();
                        finish_jsonl_failed(&state, &node_id, &agent_id, flavor, err, &stderr_log);
                        unregister(&node_id);
                        return;
                    }
                }
            }
        }
    }

    let status = match process.finish(Duration::from_secs(2)) {
        Ok(status) => status,
        Err(err) => {
            finish_jsonl_failed(&state, &node_id, &agent_id, flavor, err, &stderr_log);
            unregister(&node_id);
            return;
        }
    };
    let success = status.success() && reported_error.is_none() && session_id.is_some() && completed;
    let error = if success {
        None
    } else {
        Some(reported_error.unwrap_or_else(|| {
            if status.success() && session_id.is_none() {
                format!("{} research did not report a session id", flavor.label())
            } else if status.success() {
                format!(
                    "{} research exited without a completed response",
                    flavor.label()
                )
            } else {
                format!("{} research exited with status {status}", flavor.label())
            }
        }))
    };
    let error = error.map(|err| map_jsonl_error(flavor, err, &stderr_log));
    if let Err(err) = state.finish_research_sdk_run(&node_id, &agent_id, success, error) {
        eprintln!(
            "qmux: failed to preserve {} research result: {err}",
            flavor.label()
        );
    }
    unregister(&node_id);
}

enum EventOutcome {
    Continue,
    Complete,
    Failed(String),
}

fn handle_codex_event(
    state: &AppState,
    node_id: &str,
    agent_id: &str,
    mapper: &mut TurnMapper,
    session_id: &mut Option<String>,
    value: Value,
) -> Result<EventOutcome, String> {
    match value.get("type").and_then(Value::as_str).unwrap_or("") {
        "thread.started" => {
            if let Some(id) = value.get("thread_id").and_then(Value::as_str) {
                reconcile_session_id(session_id, Some(id), "Codex")?;
                state.record_research_sdk_session(node_id, agent_id, session_id.clone(), None)?;
            }
            Ok(EventOutcome::Continue)
        }
        "item.completed" => {
            let item = value.get("item").unwrap_or(&Value::Null);
            if item.get("type").and_then(Value::as_str) == Some("agent_message")
                && let Some(text) = item.get("text").and_then(Value::as_str)
                && !text.is_empty()
            {
                mapper.commit_assistant(
                    &Value::String(text.to_string()),
                    item.get("id").and_then(Value::as_str).map(str::to_string),
                );
                state.append_harness_turn(mapper.last_committed())?;
            }
            Ok(EventOutcome::Continue)
        }
        "turn.completed" => Ok(EventOutcome::Complete),
        "turn.failed" | "error" => Ok(EventOutcome::Failed(json_event_error(&value))),
        _ => Ok(EventOutcome::Continue),
    }
}

fn handle_grok_event(
    state: &AppState,
    node_id: &str,
    agent_id: &str,
    cwd: &std::path::Path,
    mapper: &mut TurnMapper,
    session_id: &mut Option<String>,
    value: Value,
) -> Result<EventOutcome, String> {
    match claude_sdk::parse_sdk_value(value)? {
        SdkMessage::System {
            subtype,
            session_id: observed,
            ..
        } if subtype == "init" => {
            reconcile_session_id(session_id, observed.as_deref(), "Grok")?;
            let transcript_path = session_id
                .as_deref()
                .and_then(|id| research_session_transcript_path(cwd, id))
                .map(|path| path.display().to_string());
            state.record_research_sdk_session(
                node_id,
                agent_id,
                session_id.clone(),
                transcript_path,
            )?;
            Ok(EventOutcome::Continue)
        }
        SdkMessage::StreamEvent {
            event,
            session_id: observed,
            ..
        } => {
            reconcile_session_id(session_id, observed.as_deref(), "Grok")?;
            if let Some(delta) = stream_event_text_delta(&event) {
                mapper.push_text_delta(delta);
                state.append_harness_turn(mapper.in_flight_turn())?;
            }
            if stream_event_is_end_turn(&event) {
                Ok(EventOutcome::Complete)
            } else {
                Ok(EventOutcome::Continue)
            }
        }
        SdkMessage::Assistant {
            content,
            uuid: _,
            message_id,
            session_id: observed,
            raw,
            ..
        } => {
            reconcile_session_id(session_id, observed.as_deref(), "Grok")?;
            // Grok documents `uuid` as a freshly generated line id, not a
            // provider or message identity. Only the nested Messages API id is
            // stable enough to retain as the native message id.
            mapper.commit_assistant(&content, message_id);
            state.append_harness_turn(mapper.last_committed())?;
            if assistant_message_is_end_turn(&raw) {
                Ok(EventOutcome::Complete)
            } else {
                Ok(EventOutcome::Continue)
            }
        }
        SdkMessage::User {
            content,
            session_id: observed,
            ..
        } => {
            reconcile_session_id(session_id, observed.as_deref(), "Grok")?;
            if let Some(turn) = mapper.user_turn(&content) {
                state.append_harness_turn(turn)?;
            }
            Ok(EventOutcome::Continue)
        }
        SdkMessage::Result {
            is_error,
            errors,
            result_text,
            session_id: observed,
            ..
        } => {
            reconcile_session_id(session_id, observed.as_deref(), "Grok")?;
            if !is_error
                && !mapper.has_assistant_output()
                && let Some(text) = result_text.as_deref().filter(|text| !text.is_empty())
            {
                mapper.commit_assistant(&Value::String(text.to_string()), None);
                state.append_harness_turn(mapper.last_committed())?;
            }
            if is_error {
                Ok(EventOutcome::Failed(if errors.is_empty() {
                    result_text.unwrap_or_else(|| "Grok research failed".to_string())
                } else {
                    errors.join("\n")
                }))
            } else {
                Ok(EventOutcome::Complete)
            }
        }
        SdkMessage::ControlRequest { subtype, .. } => Err(format!(
            "Grok requested unsupported headless control input ({subtype})"
        )),
        SdkMessage::Other(value) => match value.get("type").and_then(Value::as_str) {
            Some("content_block_delta") => {
                if let Some(delta) = stream_event_text_delta(&value) {
                    mapper.push_text_delta(delta);
                    state.append_harness_turn(mapper.in_flight_turn())?;
                }
                Ok(EventOutcome::Continue)
            }
            Some("message_stop") => Ok(EventOutcome::Complete),
            Some("error") => Ok(EventOutcome::Failed(json_event_error(&value))),
            _ => Ok(EventOutcome::Continue),
        },
        _ => Ok(EventOutcome::Continue),
    }
}

fn json_event_error(value: &Value) -> String {
    value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.get("error").and_then(Value::as_str))
        .or_else(|| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
        })
        .unwrap_or("headless research failed")
        .to_string()
}

fn finish_jsonl_failed(
    state: &AppState,
    node_id: &str,
    agent_id: &str,
    flavor: JsonlFlavor,
    err: String,
    stderr_log: &PathBuf,
) {
    let error = map_jsonl_error(flavor, err, stderr_log);
    if let Err(snapshot_err) = state.finish_research_sdk_run(node_id, agent_id, false, Some(error))
    {
        eprintln!("qmux: failed to preserve partial JSONL research response: {snapshot_err}");
    }
}

fn map_jsonl_error(flavor: JsonlFlavor, err: String, stderr_log: &PathBuf) -> String {
    let stderr = std::fs::read_to_string(stderr_log).unwrap_or_default();
    let combined = format!("{err}\n{stderr}");
    let lower = combined.to_ascii_lowercase();
    if lower.contains("not logged in")
        || lower.contains("not authenticated")
        || lower.contains("authentication required")
    {
        return format!(
            "{} is not logged in. Open a terminal tab, run {}, sign in, and retry this research.",
            flavor.label(),
            match flavor {
                JsonlFlavor::Codex => "codex",
                JsonlFlavor::Grok => "grok",
            }
        );
    }
    let detail = stderr.trim();
    if detail.is_empty() || err.contains(detail) {
        err
    } else {
        let mut start = detail.len().saturating_sub(4000);
        while !detail.is_char_boundary(start) {
            start += 1;
        }
        format!("{err}\n{}", &detail[start..])
    }
}

fn unregister(node_id: &str) {
    if let Ok(mut map) = sessions().lock() {
        map.remove(node_id);
    }
}

fn set_pid(node_id: &str, pid: Option<u32>) {
    if let Ok(mut map) = sessions().lock()
        && let Some(slot) = map.get_mut(node_id)
    {
        slot.pid = pid;
    }
}

fn result_after_end_turn_stall() -> Duration {
    #[cfg(test)]
    {
        Duration::from_millis(400)
    }
    #[cfg(not(test))]
    {
        claude_sdk::RESULT_AFTER_END_TURN_STALL
    }
}

fn run_session(
    state: AppState,
    node_id: String,
    agent_id: String,
    prompt: String,
    spec: ClaudeSdkSpawnSpec,
    workspace_dir: PathBuf,
    interrupt_rx: std::sync::mpsc::Receiver<()>,
    claude_version: String,
) {
    let stderr_log = spec.stderr_log.clone();
    let mut session = match ClaudeSdkSession::spawn(spec) {
        Ok(session) => session,
        Err(err) => {
            finish_failed(&state, &node_id, &agent_id, err, &stderr_log);
            unregister(&node_id);
            return;
        }
    };
    set_pid(&node_id, session.pid());

    if let Err(err) = session.write_initialize() {
        session.kill();
        finish_failed(&state, &node_id, &agent_id, err, &stderr_log);
        unregister(&node_id);
        return;
    }

    let mut mapper = TurnMapper::new(agent_id.clone());
    let mut initialize_deadline = Some(std::time::Instant::now() + claude_sdk::INITIALIZE_TIMEOUT);
    let mut session_init_deadline: Option<std::time::Instant> = None;
    let mut interrupt_deadline: Option<std::time::Instant> = None;
    let mut result_deadline: Option<std::time::Instant> = None;
    let mut sent_prompt = false;
    let mut cancelled = false;
    let mut saw_session_init = false;

    loop {
        let cancellation_requested = match interrupt_rx.try_recv() {
            Ok(()) | Err(std::sync::mpsc::TryRecvError::Disconnected) => true,
            Err(std::sync::mpsc::TryRecvError::Empty) => false,
        };
        if cancellation_requested && !cancelled {
            cancelled = true;
            if session.interrupt_receipt {
                eprintln!("qmux: research SDK sending interrupt (receipt capability advertised)");
            } else {
                eprintln!("qmux: research SDK sending interrupt (no receipt capability)");
            }
            let _ = session.write_interrupt();
            interrupt_deadline = Some(std::time::Instant::now() + claude_sdk::INTERRUPT_GRACE);
        }
        if let Some(deadline) = interrupt_deadline
            && std::time::Instant::now() >= deadline
        {
            session.kill();
            break;
        }

        let timeout = if initialize_deadline.is_some() && !session.initialized {
            Duration::from_millis(200)
        } else {
            Duration::from_millis(100)
        };
        let message = match session.recv_timeout(timeout) {
            Ok(message) => message,
            Err(err) => {
                if cancelled {
                    session.kill();
                    break;
                }
                session.kill();
                finish_failed(&state, &node_id, &agent_id, err, &stderr_log);
                unregister(&node_id);
                return;
            }
        };

        if let Some(deadline) = initialize_deadline
            && !session.initialized
            && std::time::Instant::now() >= deadline
            && !matches!(&message, Some(SdkMessage::ControlResponse { .. }))
        {
            session.kill();
            finish_failed(
                &state,
                &node_id,
                &agent_id,
                format!("Claude Code initialize timed out (claude {claude_version})"),
                &stderr_log,
            );
            unregister(&node_id);
            return;
        }

        if let Some(deadline) = session_init_deadline
            && !saw_session_init
            && std::time::Instant::now() >= deadline
            && !matches!(
                &message,
                Some(SdkMessage::System { subtype, .. }) if subtype == "init"
            )
        {
            session.kill();
            finish_failed(
                &state,
                &node_id,
                &agent_id,
                format!(
                    "Claude Code did not start a research session after initialize (claude {claude_version})"
                ),
                &stderr_log,
            );
            unregister(&node_id);
            return;
        }

        if let Some(deadline) = result_deadline
            && std::time::Instant::now() >= deadline
            && !matches!(&message, Some(SdkMessage::Result { .. }))
        {
            eprintln!(
                "qmux: research SDK result watchdog expired after assistant end_turn; preserving the completed response"
            );
            session.kill();
            session.note_transcript_candidate(&workspace_dir);
            let _ = state.record_research_sdk_session(
                &node_id,
                &agent_id,
                session.session_id.clone(),
                session.transcript_path.clone(),
            );
            if let Err(err) = state.finish_research_sdk_run(&node_id, &agent_id, true, None) {
                eprintln!(
                    "qmux: failed to preserve watchdog-settled response for {node_id}: {err}"
                );
            }
            unregister(&node_id);
            return;
        }

        let Some(message) = message else {
            continue;
        };

        match message {
            SdkMessage::ControlResponse { subtype, error, .. } => {
                if !session.initialized && subtype == "error" {
                    session.kill();
                    finish_failed(
                        &state,
                        &node_id,
                        &agent_id,
                        error.unwrap_or_else(|| "Claude Code initialize failed".to_string()),
                        &stderr_log,
                    );
                    unregister(&node_id);
                    return;
                }
                if session.initialized && !sent_prompt && !cancelled {
                    initialize_deadline = None;
                    if let Err(err) = session.send_user_prompt(&prompt) {
                        session.kill();
                        finish_failed(&state, &node_id, &agent_id, err, &stderr_log);
                        unregister(&node_id);
                        return;
                    }
                    sent_prompt = true;
                    session_init_deadline =
                        Some(std::time::Instant::now() + claude_sdk::SESSION_INIT_STALL);
                }
            }
            SdkMessage::ControlRequest {
                request_id,
                subtype,
                request,
            } => {
                if subtype == "can_use_tool" {
                    let tool_name = request
                        .get("tool_name")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let input = request.get("input").cloned().unwrap_or(json!({}));
                    let reply = if cancelled {
                        session.reply_can_use_tool(
                            &request_id,
                            false,
                            &input,
                            "research run was cancelled",
                        )
                    } else {
                        match research_can_use_tool(tool_name, &input) {
                            Ok(()) => {
                                eprintln!("qmux: research canUseTool allow {tool_name}");
                                session.reply_can_use_tool(&request_id, true, &input, "")
                            }
                            Err(message) => {
                                eprintln!("qmux: research canUseTool deny {tool_name}: {message}");
                                session.reply_can_use_tool(&request_id, false, &input, &message)
                            }
                        }
                    };
                    if let Err(err) = reply
                        && !cancelled
                    {
                        session.kill();
                        finish_failed(&state, &node_id, &agent_id, err, &stderr_log);
                        unregister(&node_id);
                        return;
                    }
                } else {
                    if let Err(err) = session.reply_control_error(
                        &request_id,
                        &format!("unsupported control request {subtype}"),
                    ) && !cancelled
                    {
                        session.kill();
                        finish_failed(&state, &node_id, &agent_id, err, &stderr_log);
                        unregister(&node_id);
                        return;
                    }
                }
            }
            SdkMessage::System { subtype, .. } if subtype == "init" => {
                saw_session_init = true;
                session_init_deadline = None;
                session.note_transcript_candidate(&workspace_dir);
                let _ = state.record_research_sdk_session(
                    &node_id,
                    &agent_id,
                    session.session_id.clone(),
                    session.transcript_path.clone(),
                );
            }
            SdkMessage::StreamEvent { event, .. } => {
                if stream_event_is_end_turn(&event) {
                    result_deadline =
                        Some(std::time::Instant::now() + result_after_end_turn_stall());
                }
                if let Some(delta) = stream_event_text_delta(&event) {
                    mapper.push_text_delta(delta);
                    if let Err(err) = state.append_harness_turn(mapper.in_flight_turn()) {
                        session.kill();
                        finish_failed(&state, &node_id, &agent_id, err, &stderr_log);
                        unregister(&node_id);
                        return;
                    }
                }
            }
            SdkMessage::Assistant {
                content,
                uuid,
                message_id,
                session_id,
                raw,
                ..
            } => {
                if session.session_id.is_none() {
                    session.session_id = session_id;
                }
                mapper.commit_assistant(&content, uuid.or(message_id));
                if let Err(err) = state.append_harness_turn(mapper.last_committed()) {
                    session.kill();
                    finish_failed(&state, &node_id, &agent_id, err, &stderr_log);
                    unregister(&node_id);
                    return;
                }
                session.note_transcript_candidate(&workspace_dir);
                if assistant_message_is_end_turn(&raw) {
                    result_deadline =
                        Some(std::time::Instant::now() + result_after_end_turn_stall());
                }
            }
            SdkMessage::User { content, .. } => {
                if let Some(turn) = mapper.user_turn(&content)
                    && let Err(err) = state.append_harness_turn(turn)
                {
                    session.kill();
                    finish_failed(&state, &node_id, &agent_id, err, &stderr_log);
                    unregister(&node_id);
                    return;
                }
            }
            SdkMessage::Result {
                is_error,
                errors,
                result_text,
                session_id,
                ..
            } => {
                if session.session_id.is_none() {
                    session.session_id = session_id.clone();
                }
                if !is_error
                    && !mapper.has_assistant_output()
                    && let Some(text) = result_text
                        .as_deref()
                        .filter(|text| !text.trim().is_empty())
                {
                    mapper.commit_assistant(&Value::String(text.to_string()), None);
                    if let Err(err) = state.append_harness_turn(mapper.last_committed()) {
                        session.kill();
                        finish_failed(&state, &node_id, &agent_id, err, &stderr_log);
                        unregister(&node_id);
                        return;
                    }
                }
                session.end_input();
                let wait_until = std::time::Instant::now() + Duration::from_secs(2);
                while session.try_wait().is_none() && std::time::Instant::now() < wait_until {
                    thread::sleep(Duration::from_millis(50));
                }
                if session.try_wait().is_none() {
                    session.kill();
                }
                session.finish_output();
                session.note_transcript_candidate(&workspace_dir);
                let _ = state.record_research_sdk_session(
                    &node_id,
                    &agent_id,
                    session.session_id.clone(),
                    session.transcript_path.clone(),
                );
                let outcome = if cancelled {
                    state.finish_research_sdk_run(&node_id, &agent_id, false, None)
                } else if is_error {
                    let err = if errors.is_empty() {
                        result_text.unwrap_or_else(|| "Claude research run failed".to_string())
                    } else {
                        errors.join("\n")
                    };
                    state.finish_research_sdk_run(
                        &node_id,
                        &agent_id,
                        false,
                        Some(map_research_error(err, &stderr_log)),
                    )
                } else {
                    state.finish_research_sdk_run(&node_id, &agent_id, true, None)
                };
                if let Err(err) = outcome {
                    eprintln!("qmux: failed to preserve research SDK result for {node_id}: {err}");
                }
                unregister(&node_id);
                return;
            }
            _ => {}
        }
    }

    if cancelled {
        let _ = state.finish_research_sdk_run(&node_id, &agent_id, false, None);
    }
    unregister(&node_id);
}

fn finish_failed(
    state: &AppState,
    node_id: &str,
    agent_id: &str,
    err: String,
    stderr_log: &PathBuf,
) {
    let error = map_research_error(err, stderr_log);
    if let Err(snapshot_err) = state.finish_research_sdk_run(node_id, agent_id, false, Some(error))
    {
        eprintln!("qmux: failed to preserve partial research SDK response: {snapshot_err}");
    }
}

fn map_research_error(err: String, stderr_log: &PathBuf) -> String {
    let stderr = std::fs::read_to_string(stderr_log).unwrap_or_default();
    let combined = format!("{err}\n{stderr}");
    claude_sdk::login_error_message(&combined).unwrap_or(err)
}

struct TurnMapper {
    agent_id: String,
    assistant_index: usize,
    in_flight_text: String,
    source_index: usize,
    last_committed: Option<Turn>,
}

impl TurnMapper {
    fn new(agent_id: String) -> Self {
        Self {
            agent_id,
            assistant_index: 0,
            in_flight_text: String::new(),
            source_index: 0,
            last_committed: None,
        }
    }

    fn assistant_id(&self) -> String {
        format!("{}-sdk-assistant-{}", self.agent_id, self.assistant_index)
    }

    fn has_assistant_output(&self) -> bool {
        self.assistant_index > 0 || !self.in_flight_text.is_empty()
    }

    fn push_text_delta(&mut self, delta: &str) {
        self.in_flight_text.push_str(delta);
    }

    fn in_flight_turn(&self) -> Turn {
        Turn {
            id: self.assistant_id(),
            agent_id: self.agent_id.clone(),
            session_id: None,
            role: "assistant".to_string(),
            blocks: vec![TurnBlock::Text {
                text: self.in_flight_text.clone(),
            }],
            source_index: self.source_index,
            timestamp: Some(now_millis() as i64),
            status: None,
            status_reason: None,
            context_status: None,
            native_id: None,
            parent_native_id: None,
            native_message_id: None,
        }
    }

    fn commit_assistant(&mut self, content: &Value, native_id: Option<String>) {
        let mut blocks = Vec::new();
        if let Some(items) = content.as_array() {
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = item.get("text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            blocks.push(TurnBlock::Text {
                                text: text.to_string(),
                            });
                        }
                    }
                    Some("tool_use") => {
                        blocks.push(TurnBlock::ToolUse {
                            id: item.get("id").and_then(Value::as_str).map(str::to_string),
                            name: item
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("tool")
                                .to_string(),
                            input: item.get("input").cloned().unwrap_or(json!({})),
                        });
                    }
                    _ => {}
                }
            }
        } else if let Some(text) = content.as_str() {
            blocks.push(TurnBlock::Text {
                text: text.to_string(),
            });
        }
        if blocks.is_empty() && !self.in_flight_text.is_empty() {
            blocks.push(TurnBlock::Text {
                text: self.in_flight_text.clone(),
            });
        }
        let turn = Turn {
            id: self.assistant_id(),
            agent_id: self.agent_id.clone(),
            session_id: None,
            role: "assistant".to_string(),
            blocks,
            source_index: self.source_index,
            timestamp: Some(now_millis() as i64),
            status: None,
            status_reason: None,
            context_status: None,
            native_id,
            parent_native_id: None,
            native_message_id: None,
        };
        self.last_committed = Some(turn);
        self.assistant_index += 1;
        self.source_index += 1;
        self.in_flight_text.clear();
    }

    fn last_committed(&self) -> Turn {
        self.last_committed
            .clone()
            .expect("commit_assistant stores a turn")
    }

    fn user_turn(&mut self, content: &Value) -> Option<Turn> {
        let mut blocks = Vec::new();
        if let Some(text) = content.as_str().filter(|text| !text.is_empty()) {
            blocks.push(TurnBlock::Text {
                text: text.to_string(),
            });
        }
        if let Some(items) = content.as_array() {
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = item.get("text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            blocks.push(TurnBlock::Text {
                                text: text.to_string(),
                            });
                        }
                    }
                    Some("tool_result") => blocks.push(TurnBlock::ToolResult {
                        tool_use_id: item
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        content: item.get("content").cloned().unwrap_or(Value::Null),
                        is_error: item
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    }),
                    _ => {}
                }
            }
        }
        if blocks.is_empty() {
            return None;
        }
        let turn = Turn {
            id: format!("{}-sdk-user-{}", self.agent_id, self.source_index),
            agent_id: self.agent_id.clone(),
            session_id: None,
            role: "user".to_string(),
            blocks,
            source_index: self.source_index,
            timestamp: Some(now_millis() as i64),
            status: None,
            status_reason: None,
            context_status: None,
            native_id: None,
            parent_native_id: None,
            native_message_id: None,
        };
        self.source_index += 1;
        Some(turn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AdapterConfigs, ClaudeAdapterConfig, CodexAdapterConfig, GrokAdapterConfig,
        MuseAdapterConfig, OpencodeAdapterConfig, QmuxConfig,
    };
    use crate::research::{CreateResearchTreeRequest, ResearchNodeStatus, ResearchRuntime};
    use crate::workspace::{GroupInfo, WorkspaceScope};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    static RUNTIME_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("qmux-research-sdk-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn user_turn_keeps_replayed_prompt_text_and_tool_results() {
        let mut mapper = TurnMapper::new("agent-1".to_string());
        let turn = mapper
            .user_turn(&json!([
                {"type": "text", "text": "follow-up prompt"},
                {
                    "type": "tool_result",
                    "tool_use_id": "tool-1",
                    "content": "ok",
                    "is_error": false
                }
            ]))
            .expect("supported user blocks produce a turn");

        assert_eq!(turn.id, "agent-1-sdk-user-0");
        assert_eq!(turn.role, "user");
        assert!(matches!(
            turn.blocks.as_slice(),
            [
                TurnBlock::Text { text },
                TurnBlock::ToolResult {
                    tool_use_id: Some(tool_use_id),
                    content,
                    is_error: false,
                }
            ] if text == "follow-up prompt" && tool_use_id == "tool-1" && content == "ok"
        ));
    }

    fn write_fake_claude(dir: &Path) -> PathBuf {
        let path = dir.join("fake-claude");
        fs::write(
            &path,
            r#"#!/usr/bin/env python3
import json, sys
if "-v" in sys.argv or "--version" in sys.argv:
    print("2.1.239 (Claude Code)")
    sys.exit(0)
def read():
    line = sys.stdin.readline()
    if not line:
        sys.exit(1)
    return json.loads(line)
init = read()
assert init.get("type") == "control_request"
assert init.get("request", {}).get("subtype") == "initialize"
rid = init["request_id"]
print(json.dumps({
    "type": "control_response",
    "response": {
        "subtype": "success",
        "request_id": rid,
        "response": {"commands": [{"name": "commit"}], "agents": [], "output_style": "default"}
    }
}), flush=True)
user = read()
assert user.get("type") == "user"
print(json.dumps({
    "type": "system",
    "subtype": "init",
    "session_id": "sess-1",
    "capabilities": ["interrupt_receipt_v1"]
}), flush=True)
print(json.dumps({
    "type": "assistant",
    "session_id": "sess-1",
    "uuid": "asst-1",
    "message": {"id": "msg-1", "content": [{"type": "text", "text": "done"}]}
}), flush=True)
print(json.dumps({
    "type": "result",
    "subtype": "success",
    "session_id": "sess-1",
    "result": "done"
}), flush=True)
"#,
        )
        .unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn write_fake_claude_delayed_initialize(dir: &Path) -> PathBuf {
        let path = dir.join("fake-claude-delayed-init");
        fs::write(
            &path,
            r#"#!/usr/bin/env python3
import json, os, select, sys, time
if "-v" in sys.argv or "--version" in sys.argv:
    print("2.1.239 (Claude Code)")
    sys.exit(0)
def read():
    line = sys.stdin.readline()
    if not line:
        sys.exit(0)
    return json.loads(line)
init = read()
time.sleep(0.4)
print(json.dumps({
    "type": "control_response",
    "response": {
        "subtype": "success",
        "request_id": init["request_id"],
        "response": {"commands": []}
    }
}), flush=True)
first = read()
if first.get("type") == "user":
    open("prompt-sent", "w").write("user")
if first.get("request", {}).get("subtype") == "interrupt":
    print(json.dumps({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": first["request_id"],
            "response": {}
        }
    }), flush=True)
ready, _, _ = select.select([sys.stdin], [], [], 0.6)
if ready:
    later = read()
    if later.get("type") == "user":
        open("prompt-sent", "w").write("user")
"#,
        )
        .unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn write_fake_claude_result_only(dir: &Path) -> PathBuf {
        let path = dir.join("fake-claude-result-only");
        fs::write(
            &path,
            r#"#!/usr/bin/env python3
import json, sys
if "-v" in sys.argv or "--version" in sys.argv:
    print("2.1.239 (Claude Code)")
    sys.exit(0)
init = json.loads(sys.stdin.readline())
print(json.dumps({
    "type": "control_response",
    "response": {
        "subtype": "success",
        "request_id": init["request_id"],
        "response": {"commands": []}
    }
}), flush=True)
json.loads(sys.stdin.readline())
print(json.dumps({
    "type": "system",
    "subtype": "init",
    "session_id": "sess-result-only"
}), flush=True)
print(json.dumps({
    "type": "result",
    "subtype": "success",
    "session_id": "sess-result-only",
    "result": "result-only answer"
}), flush=True)
"#,
        )
        .unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn write_fake_claude_cancel_result(dir: &Path) -> PathBuf {
        let path = dir.join("fake-claude-cancel-result");
        fs::write(
            &path,
            r#"#!/usr/bin/env python3
import json, sys
if "-v" in sys.argv or "--version" in sys.argv:
    print("2.1.239 (Claude Code)")
    sys.exit(0)
init = json.loads(sys.stdin.readline())
print(json.dumps({
    "type": "control_response",
    "response": {
        "subtype": "success",
        "request_id": init["request_id"],
        "response": {"commands": []}
    }
}), flush=True)
json.loads(sys.stdin.readline())
open("prompt-sent", "w").write("user")
print(json.dumps({
    "type": "system",
    "subtype": "init",
    "session_id": "sess-cancel-result"
}), flush=True)
interrupt = json.loads(sys.stdin.readline())
assert interrupt.get("request", {}).get("subtype") == "interrupt"
print(json.dumps({
    "type": "result",
    "subtype": "error_during_execution",
    "is_error": True,
    "session_id": "sess-cancel-result",
    "errors": ["interrupted"]
}), flush=True)
"#,
        )
        .unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn write_fake_claude_end_turn_without_result(dir: &Path) -> PathBuf {
        let path = dir.join("fake-claude-end-turn");
        fs::write(
            &path,
            r#"#!/usr/bin/env python3
import json, sys, time
if "-v" in sys.argv or "--version" in sys.argv:
    print("2.1.240 (Claude Code)")
    sys.exit(0)
init = json.loads(sys.stdin.readline())
print(json.dumps({
    "type": "control_response",
    "response": {
        "subtype": "success",
        "request_id": init["request_id"],
        "response": {"commands": []}
    }
}), flush=True)
json.loads(sys.stdin.readline())
print(json.dumps({
    "type": "system",
    "subtype": "init",
    "session_id": "sess-end-turn"
}), flush=True)
print(json.dumps({
    "type": "assistant",
    "session_id": "sess-end-turn",
    "message": {
        "id": "msg-end-turn",
        "stop_reason": "end_turn",
        "content": [{"type": "text", "text": "answer before missing result"}]
    }
}), flush=True)
time.sleep(2)
"#,
        )
        .unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn test_config_with_binaries(
        workspace_root: PathBuf,
        claude: &Path,
        codex: &Path,
        grok: &Path,
    ) -> QmuxConfig {
        QmuxConfig {
            remotes: Default::default(),
            workspace_root,
            socket_path: PathBuf::from("/tmp/qmux-research-sdk-test.sock"),
            adapters: AdapterConfigs {
                pi: Default::default(),
                claude: ClaudeAdapterConfig {
                    binary: Some(claude.display().to_string()),
                },
                codex: CodexAdapterConfig {
                    binary: Some(codex.display().to_string()),
                },
                opencode: OpencodeAdapterConfig {
                    binary: Some("opencode".to_string()),
                },
                grok: GrokAdapterConfig {
                    binary: Some(grok.display().to_string()),
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

    fn test_config(workspace_root: PathBuf, claude: &Path) -> QmuxConfig {
        test_config_with_binaries(
            workspace_root,
            claude,
            Path::new("codex"),
            Path::new("grok"),
        )
    }

    fn test_group(root: &Path, workspace_dir: &Path) -> GroupInfo {
        GroupInfo {
            id: "group-1".to_string(),
            name: "group-1".to_string(),
            name_override: None,
            dir: workspace_dir.display().to_string(),
            managed_dir: root.join("managed").display().to_string(),
            base_repo: Some(workspace_dir.display().to_string()),
            base_ref: Some("HEAD".to_string()),
            parent_id: None,
            created_at: 1,
            collapsed: false,
            scope: WorkspaceScope::Research,
            imported_research_archive_id: None,
            remote: None,
            agents: Vec::new(),
        }
    }

    fn write_fake_codex(dir: &Path) -> PathBuf {
        let path = dir.join("fake-codex");
        fs::write(
            &path,
            r#"#!/usr/bin/env python3
import json, sys
args = sys.argv[1:]
assert args[:6] == ["--search", "--disable", "hooks", "--ask-for-approval", "never", "exec"]
assert "--json" in args
assert "--strict-config" in args
assert "--skip-git-repo-check" in args
assert "--ignore-user-config" in args
assert "--ignore-rules" in args
assert args[args.index("--sandbox") + 1] == "read-only"
assert args[-2:] == ["--", "hello"]
print(json.dumps({"type":"thread.started","thread_id":"codex-thread-1"}), flush=True)
print(json.dumps({"type":"turn.started"}), flush=True)
print(json.dumps({"type":"item.completed","item":{"id":"answer-1","type":"agent_message","text":"codex answer"}}), flush=True)
print(json.dumps({"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}), flush=True)
"#,
        )
        .unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn write_fake_grok(dir: &Path) -> PathBuf {
        let path = dir.join("fake-grok");
        fs::write(
            &path,
            r#"#!/usr/bin/env python3
import json, sys
args = sys.argv[1:]
assert "--no-auto-update" in args
assert args[args.index("--output-format") + 1] == "streaming-messages-json"
assert args[args.index("--permission-mode") + 1] == "dontAsk"
assert args[args.index("--sandbox") + 1] == "read-only"
assert args[args.index("--tools") + 1] == "read_file,grep,list_dir,web_search,web_fetch"
assert "Bash" in args and "Edit" in args and "MCPTool" in args
assert args[args.index("-p") + 1] == "hello"
session_id = args[args.index("--session-id") + 1]
print(json.dumps({"type":"system","subtype":"init","session_id":session_id}), flush=True)
print(json.dumps({"type":"assistant","session_id":session_id,"uuid":"line-tool","message":{"id":"message-tool","stop_reason":"tool_use","content":[{"type":"tool_use","id":"tool-1","name":"read_file","input":{"path":"README.md"}}]}}), flush=True)
print(json.dumps({"type":"user","session_id":session_id,"uuid":"line-result","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-1","content":"contents","is_error":False}]}}), flush=True)
print(json.dumps({"type":"stream_event","session_id":session_id,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"grok "}}}), flush=True)
print(json.dumps({"type":"assistant","session_id":session_id,"uuid":"answer-1","message":{"id":"message-1","stop_reason":"end_turn","content":[{"type":"text","text":"grok answer"}]}}), flush=True)
print(json.dumps({"type":"result","subtype":"success","session_id":session_id,"result":"grok answer"}), flush=True)
"#,
        )
        .unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[test]
    fn launch_completes_a_fake_claude_sdk_session() {
        let _guard = RUNTIME_TEST_LOCK.lock().unwrap();
        let root = temp_dir();
        let binary = write_fake_claude(&root);
        let workspace_dir = root.join("workspace");
        fs::create_dir_all(&workspace_dir).unwrap();
        let state = AppState::new(test_config(root.clone(), &binary));
        let group = GroupInfo {
            id: "group-1".to_string(),
            name: "group-1".to_string(),
            name_override: None,
            dir: workspace_dir.display().to_string(),
            managed_dir: root.join("managed").display().to_string(),
            base_repo: Some(workspace_dir.display().to_string()),
            base_ref: Some("HEAD".to_string()),
            parent_id: None,
            created_at: 1,
            collapsed: false,
            scope: WorkspaceScope::Research,
            imported_research_archive_id: None,
            remote: None,
            agents: Vec::new(),
        };
        state.insert_group_after(group.clone(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "hello".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let node = detail.nodes[0].clone();
        let launched = launch(&state, &node, &group, "hello".to_string(), None, false).unwrap();
        assert_eq!(launched.runtime, ResearchRuntime::Sdk);
        assert!(launched.pane_id.is_none());
        let agent_id = launched
            .agent_id
            .clone()
            .expect("sdk bind records agent_id");

        let deadline = Instant::now() + Duration::from_secs(8);
        let finished = loop {
            let current = state.research_node(&node.id).unwrap();
            if current.status.is_terminal() {
                break current;
            }
            assert!(
                Instant::now() < deadline,
                "sdk launch did not settle: {:?}",
                state.research_node(&node.id).unwrap()
            );
            thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(finished.status, ResearchNodeStatus::Complete);
        assert_eq!(finished.native_session_id.as_deref(), Some("sess-1"));
        assert!(finished.pane_id.is_none());
        assert!(state.agent(&agent_id).unwrap().is_none());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn cancellation_during_initialize_never_sends_the_prompt() {
        let _guard = RUNTIME_TEST_LOCK.lock().unwrap();
        let root = temp_dir();
        let binary = write_fake_claude_delayed_initialize(&root);
        let workspace_dir = root.join("workspace");
        fs::create_dir_all(&workspace_dir).unwrap();
        let state = AppState::new(test_config(root.clone(), &binary));
        let group = GroupInfo {
            id: "group-1".to_string(),
            name: "group-1".to_string(),
            name_override: None,
            dir: workspace_dir.display().to_string(),
            managed_dir: root.join("managed").display().to_string(),
            base_repo: Some(workspace_dir.display().to_string()),
            base_ref: Some("HEAD".to_string()),
            parent_id: None,
            created_at: 1,
            collapsed: false,
            scope: WorkspaceScope::Research,
            imported_research_archive_id: None,
            remote: None,
            agents: Vec::new(),
        };
        state.insert_group_after(group.clone(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "hello".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let node = detail.nodes[0].clone();
        launch(&state, &node, &group, "hello".to_string(), None, false).unwrap();
        state.cancel_research_node(&node.id).unwrap();
        assert!(wait_for_session_stop(&node.id, Duration::from_secs(5)));
        assert_eq!(
            state.research_node(&node.id).unwrap().status,
            ResearchNodeStatus::Cancelled
        );
        assert!(!workspace_dir.join("prompt-sent").exists());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn cancellation_wins_over_a_racing_claude_error_result() {
        let _guard = RUNTIME_TEST_LOCK.lock().unwrap();
        let root = temp_dir();
        let binary = write_fake_claude_cancel_result(&root);
        let workspace_dir = root.join("workspace");
        fs::create_dir_all(&workspace_dir).unwrap();
        let state = AppState::new(test_config(root.clone(), &binary));
        let group = test_group(&root, &workspace_dir);
        state.insert_group_after(group.clone(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "hello".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: group.id.clone(),
            })
            .unwrap();
        let node = detail.nodes[0].clone();
        launch(&state, &node, &group, "hello".to_string(), None, false).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while !workspace_dir.join("prompt-sent").is_file() {
            assert!(Instant::now() < deadline, "Claude prompt was never sent");
            thread::sleep(Duration::from_millis(20));
        }
        state.cancel_research_node(&node.id).unwrap();
        assert!(wait_for_session_stop(&node.id, Duration::from_secs(5)));
        let finished = state.research_node(&node.id).unwrap();
        assert_eq!(finished.status, ResearchNodeStatus::Cancelled);
        assert!(finished.error.is_none());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn result_only_success_is_snapshotted_as_an_assistant_turn() {
        let _guard = RUNTIME_TEST_LOCK.lock().unwrap();
        let root = temp_dir();
        let binary = write_fake_claude_result_only(&root);
        let workspace_dir = root.join("workspace");
        fs::create_dir_all(&workspace_dir).unwrap();
        let state = AppState::new(test_config(root.clone(), &binary));
        let group = GroupInfo {
            id: "group-1".to_string(),
            name: "group-1".to_string(),
            name_override: None,
            dir: workspace_dir.display().to_string(),
            managed_dir: root.join("managed").display().to_string(),
            base_repo: Some(workspace_dir.display().to_string()),
            base_ref: Some("HEAD".to_string()),
            parent_id: None,
            created_at: 1,
            collapsed: false,
            scope: WorkspaceScope::Research,
            imported_research_archive_id: None,
            remote: None,
            agents: Vec::new(),
        };
        state.insert_group_after(group.clone(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "hello".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let node = detail.nodes[0].clone();
        launch(&state, &node, &group, "hello".to_string(), None, false).unwrap();
        assert!(wait_for_session_stop(&node.id, Duration::from_secs(5)));
        assert_eq!(
            state.research_node(&node.id).unwrap().status,
            ResearchNodeStatus::Complete
        );
        let snapshot = crate::research::read_response_snapshot(&root, &node.id)
            .unwrap()
            .unwrap();
        assert!(matches!(
            snapshot[0].blocks.as_slice(),
            [TurnBlock::Text { text }] if text == "result-only answer"
        ));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn end_turn_without_result_is_settled_by_the_watchdog() {
        let _guard = RUNTIME_TEST_LOCK.lock().unwrap();
        let root = temp_dir();
        let binary = write_fake_claude_end_turn_without_result(&root);
        let workspace_dir = root.join("workspace");
        fs::create_dir_all(&workspace_dir).unwrap();
        let state = AppState::new(test_config(root.clone(), &binary));
        let group = GroupInfo {
            id: "group-1".to_string(),
            name: "group-1".to_string(),
            name_override: None,
            dir: workspace_dir.display().to_string(),
            managed_dir: root.join("managed").display().to_string(),
            base_repo: Some(workspace_dir.display().to_string()),
            base_ref: Some("HEAD".to_string()),
            parent_id: None,
            created_at: 1,
            collapsed: false,
            scope: WorkspaceScope::Research,
            imported_research_archive_id: None,
            remote: None,
            agents: Vec::new(),
        };
        state.insert_group_after(group.clone(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "hello".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: "group-1".to_string(),
            })
            .unwrap();
        let node = detail.nodes[0].clone();
        launch(&state, &node, &group, "hello".to_string(), None, false).unwrap();
        assert!(wait_for_session_stop(&node.id, Duration::from_secs(5)));
        assert_eq!(
            state.research_node(&node.id).unwrap().status,
            ResearchNodeStatus::Complete
        );
        let snapshot = crate::research::read_response_snapshot(&root, &node.id)
            .unwrap()
            .unwrap();
        assert!(matches!(
            snapshot[0].blocks.as_slice(),
            [TurnBlock::Text { text }] if text == "answer before missing result"
        ));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn codex_exec_args_are_headless_searchable_and_fail_closed() {
        let args =
            build_codex_exec_args("question", Some("gpt-5.6-sol"), Some("high"), None, false);
        assert_eq!(
            &args[..6],
            [
                "--search",
                "--disable",
                "hooks",
                "--ask-for-approval",
                "never",
                "exec"
            ]
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--sandbox", "read-only"])
        );
        assert!(args.iter().any(|arg| arg == "--skip-git-repo-check"));
        assert!(args.iter().any(|arg| arg == "--strict-config"));
        assert_eq!(
            args.iter()
                .filter(|arg| arg.as_str() == "--ignore-user-config")
                .count(),
            1
        );
        assert!(
            args.iter()
                .any(|arg| arg == "model_reasoning_effort=\"high\"")
        );
        assert_eq!(&args[args.len() - 2..], ["--", "question"]);

        let fork = build_codex_exec_args("branch", None, None, Some("parent-1"), true);
        assert_eq!(
            &fork[..7],
            [
                "--search",
                "--disable",
                "hooks",
                "--ask-for-approval",
                "never",
                "exec",
                "fork"
            ]
        );
        assert!(fork.iter().any(|arg| arg == "sandbox_mode=\"read-only\""));
        assert_eq!(&fork[fork.len() - 3..], ["parent-1", "--", "branch"]);
    }

    #[test]
    fn grok_headless_args_are_streaming_and_fail_closed() {
        let args = build_grok_headless_args(
            Path::new("/tmp/research"),
            "question",
            Some("grok-build"),
            Some("high"),
            Some("parent-1"),
            true,
            "child-1",
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--output-format", "streaming-messages-json"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--permission-mode", "dontAsk"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--sandbox", "read-only"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--resume", "parent-1"]));
        assert!(args.iter().any(|arg| arg == "--fork-session"));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--session-id", "child-1"])
        );
        for denied in ["Bash", "Edit", "Write", "MCPTool"] {
            assert!(
                args.windows(2)
                    .any(|pair| pair[0] == "--deny" && pair[1] == denied)
            );
        }
        assert_eq!(&args[args.len() - 2..], ["-p", "question"]);
    }

    #[test]
    fn headless_session_ids_must_be_safe_and_consistent() {
        let mut session_id = Some("requested-1".to_string());
        reconcile_session_id(&mut session_id, Some("requested-1"), "Grok").unwrap();
        assert_eq!(session_id.as_deref(), Some("requested-1"));

        let mismatch = reconcile_session_id(&mut session_id, Some("different-1"), "Grok")
            .expect_err("a CLI must not redirect qmux to a different session");
        assert!(mismatch.contains("different session id"));
        let mut empty = None;
        assert!(reconcile_session_id(&mut empty, Some("../../unsafe"), "Grok").is_err());
    }

    #[test]
    fn launch_completes_a_fake_codex_exec_session() {
        let _guard = RUNTIME_TEST_LOCK.lock().unwrap();
        let root = temp_dir();
        let claude = write_fake_claude(&root);
        let codex = write_fake_codex(&root);
        let workspace_dir = root.join("workspace");
        fs::create_dir_all(&workspace_dir).unwrap();
        let config = test_config_with_binaries(root.clone(), &claude, &codex, Path::new("grok"));
        let state = AppState::new(config);
        let group = test_group(&root, &workspace_dir);
        state.insert_group_after(group.clone(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "hello".to_string(),
                title: None,
                adapter: "codex".to_string(),
                model: None,
                effort: None,
                group_id: group.id.clone(),
            })
            .unwrap();
        let node = detail.nodes[0].clone();
        launch(&state, &node, &group, "hello".to_string(), None, false).unwrap();
        assert!(wait_for_session_stop(&node.id, Duration::from_secs(5)));
        let finished = state.research_node(&node.id).unwrap();
        assert_eq!(finished.status, ResearchNodeStatus::Complete);
        assert_eq!(
            finished.native_session_id.as_deref(),
            Some("codex-thread-1")
        );
        assert!(finished.pane_id.is_none());
        let snapshot = crate::research::read_response_snapshot(&root, &node.id)
            .unwrap()
            .unwrap();
        assert!(matches!(
            snapshot[0].blocks.as_slice(),
            [TurnBlock::Text { text }] if text == "codex answer"
        ));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn launch_completes_a_fake_grok_streaming_session() {
        let _guard = RUNTIME_TEST_LOCK.lock().unwrap();
        let root = temp_dir();
        let claude = write_fake_claude(&root);
        let grok = write_fake_grok(&root);
        let workspace_dir = root.join("workspace");
        fs::create_dir_all(&workspace_dir).unwrap();
        let config = test_config_with_binaries(root.clone(), &claude, Path::new("codex"), &grok);
        let state = AppState::new(config);
        let group = test_group(&root, &workspace_dir);
        state.insert_group_after(group.clone(), None).unwrap();
        let detail = state
            .create_research_tree(CreateResearchTreeRequest {
                prompt: "hello".to_string(),
                title: None,
                adapter: "grok".to_string(),
                model: None,
                effort: None,
                group_id: group.id.clone(),
            })
            .unwrap();
        let node = detail.nodes[0].clone();
        launch(&state, &node, &group, "hello".to_string(), None, false).unwrap();
        assert!(wait_for_session_stop(&node.id, Duration::from_secs(5)));
        let finished = state.research_node(&node.id).unwrap();
        assert_eq!(finished.status, ResearchNodeStatus::Complete);
        assert!(finished.native_session_id.is_some());
        assert!(finished.pane_id.is_none());
        let snapshot = crate::research::read_response_snapshot(&root, &node.id)
            .unwrap()
            .unwrap();
        assert!(snapshot.iter().any(|turn| matches!(
            turn.blocks.as_slice(),
            [TurnBlock::ToolResult { tool_use_id: Some(tool_use_id), content, is_error: false }]
                if tool_use_id == "tool-1" && content == "contents"
        )));
        let answer = snapshot
            .iter()
            .find(|turn| matches!(turn.blocks.as_slice(), [TurnBlock::Text { text }] if text == "grok answer"))
            .expect("the final Grok answer is preserved");
        assert_eq!(answer.native_id.as_deref(), Some("message-1"));
        fs::remove_dir_all(root).ok();
    }
}
