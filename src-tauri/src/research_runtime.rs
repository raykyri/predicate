//! Pane-less Claude research runs driven by [`crate::claude_sdk`].

use crate::adapters::claude::ClaudeAdapter;
use crate::claude_sdk::{
    self, ClaudeSdkSession, ClaudeSdkSpawnSpec, SdkMessage, assistant_message_is_end_turn,
    research_can_use_tool, stream_event_is_end_turn, stream_event_text_delta,
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
    adapter == "claude" && persistence::research_sdk_harness_enabled(&state.config().workspace_root)
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
                let outcome = if is_error {
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

    fn test_config(workspace_root: PathBuf, claude: &Path) -> QmuxConfig {
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
            claude_plugin_dir: PathBuf::new(),
            opencode_plugin_dir: PathBuf::new(),
            pi_extension_dir: PathBuf::new(),
            cursor_plugin_dir: PathBuf::new(),
        }
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
}
