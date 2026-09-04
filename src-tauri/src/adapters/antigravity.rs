use super::{
    AdapterNotification, AdapterNotificationOutcome, AgentAdapter, ComposerPolicy, LaunchEnv,
    PaneInfo, PrepareShellAgentLaunchRequest, PreparedShellAgentLaunch, ShellCommandIntegration,
    SpawnAgentRequest, apply_shell_cli_model, ensure_on_path, hook_transcript_path_acceptable,
    native_timestamp_ms, prepared_shell_agent, record_shell_session_lineage,
    reusable_session_agent, shell_cli_model, shell_quote_arg, shell_quote_path,
};
use crate::config::QmuxConfig;
use crate::events::QmuxEvent;
use crate::pty::{
    CommandPlan, InitialPaneSize, PaneMeta, agent_pane_envs, plan_to_spec, recoverable_dir,
    spawn_pty,
};
use crate::state::{AppState, PaneKind};
use crate::transcript::{Turn, TurnBlock, rfc3339_to_epoch_ms, start_transcript_tail};
use crate::turn_queue::{IdleResolution, advance_after_idle};
use crate::workspace::{
    AgentInfo, AgentStatus, PrepareAgentWorkspaceRequest, attach_agent_pane, mark_agent_failed,
    mark_agent_spawn_failed, prepare_agent_workspace, prepare_agent_workspace_with_parent,
};
use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const ANTIGRAVITY_HOOK_EVENTS: &[&str] =
    &["PreInvocation", "PostInvocation", "PostToolUse", "Stop"];

#[derive(Clone, Debug)]
pub struct AntigravityAdapter {
    binary: String,
}

impl AntigravityAdapter {
    pub fn new(config: &QmuxConfig) -> Self {
        Self {
            binary: config.antigravity_binary(),
        }
    }

    pub(crate) fn ensure_binary(&self) -> Result<String, String> {
        let binary = ensure_on_path(&self.binary).ok_or_else(|| {
            format!(
                "Antigravity adapter binary '{}' was not found on PATH or standard tool paths. Install the Antigravity CLI (`agy`) or update adapters.antigravity.binary in qmux.config.json.",
                self.binary
            )
        })?;
        Ok(binary.display().to_string())
    }

    fn spawn_pane(&self, state: &AppState, request: SpawnAgentRequest) -> Result<PaneInfo, String> {
        let binary = self.ensure_binary()?;
        ensure_antigravity_integration()?;

        let agent = prepare_agent_workspace_with_parent(
            state,
            PrepareAgentWorkspaceRequest {
                group_id: request.group_id,
                base_repo: request.base_repo,
                base_ref: request.base_ref,
                adapter: self.id().to_string(),
                model: request.model.clone(),
                effort: None,
                use_worktree: request.use_worktree.unwrap_or(false),
            },
            request.parent_id.as_deref(),
        )?;
        let cwd = request
            .cwd
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&agent.worktree_dir));
        if !cwd.is_dir() {
            let _ = mark_agent_failed(state, &agent.id);
            return Err(format!(
                "Antigravity working directory {} does not exist",
                cwd.display()
            ));
        }

        let trimmed_prompt = request.prompt.trim();
        let has_initial_prompt = !trimmed_prompt.is_empty();
        let prompt_opt = if has_initial_prompt {
            Some(trimmed_prompt)
        } else {
            None
        };
        let args = build_antigravity_args(&cwd, request.model.as_deref(), prompt_opt);

        let pane_id = state.next_id("pane");
        let envs = agent_pane_envs(state, &pane_id, &agent.id)?;

        attach_antigravity_agent_pane(state, &agent.id, pane_id.clone(), has_initial_prompt)?;
        let spawn_result = plan_to_spec(
            state,
            PaneMeta {
                pane_id: Some(pane_id.clone()),
                agent_id: Some(agent.id.clone()),
                group_id: agent.group_id.clone(),
                kind: PaneKind::Agent,
                title: self.display_name().to_string(),
                last_osc_title: None,
                initial_size: request.initial_size,
                recovered: false,
            },
            CommandPlan {
                program: binary,
                args,
                cwd,
                envs,
                support_files: Vec::new(),
                support_file_fallback: None,
            },
        )
        .and_then(|spec| spawn_pty(state, spec));

        match spawn_result {
            Ok(pane) => Ok(pane),
            Err(err) => {
                let _ = mark_agent_spawn_failed(state, &agent.id, &pane_id);
                Err(err)
            }
        }
    }

    fn respawn_pane(
        &self,
        state: &AppState,
        pane: &PaneInfo,
        agent: &AgentInfo,
    ) -> Result<PaneInfo, String> {
        let binary = self.ensure_binary()?;
        ensure_antigravity_integration()?;
        let cwd = recoverable_dir(&agent.worktree_dir).ok_or_else(|| {
            format!(
                "agent worktree {} no longer exists; relaunch manually",
                agent.worktree_dir
            )
        })?;

        let (args, resumed) = build_antigravity_resume_args(
            &cwd,
            agent.model.as_deref(),
            agent.session_id.as_deref(),
        );

        let envs = agent_pane_envs(state, &pane.id, &agent.id)?;

        let spec = plan_to_spec(
            state,
            PaneMeta {
                pane_id: Some(pane.id.clone()),
                agent_id: Some(agent.id.clone()),
                group_id: agent.group_id.clone(),
                kind: PaneKind::Agent,
                title: pane.title.clone(),
                last_osc_title: pane.last_osc_title.clone(),
                initial_size: Some(InitialPaneSize {
                    cols: pane.cols,
                    rows: pane.rows,
                }),
                recovered: true,
            },
            CommandPlan {
                program: binary,
                args,
                cwd,
                envs,
                support_files: Vec::new(),
                support_file_fallback: None,
            },
        )?;

        let info = spawn_pty(state, spec)?;

        let restored = state.mutate_agent(&agent.id, |agent| {
            agent.pane_id = Some(pane.id.clone());
            agent.status = AgentStatus::Starting;
        })?;

        let restored = restored.unwrap_or_else(|| agent.clone());

        if let Some(transcript_path) = restored.transcript_path.clone() {
            start_transcript_tail(
                state.clone(),
                restored.id.clone(),
                transcript_path,
                self.id().to_string(),
            );
        }

        state.emit(QmuxEvent::new(
            "agent.recovered",
            Some(pane.id.clone()),
            Some(restored.id.clone()),
            json!({ "resumed": resumed, "agent": restored }),
        ));

        Ok(info)
    }

    fn prepare_shell_launch_inner(
        &self,
        state: &AppState,
        request: PrepareShellAgentLaunchRequest,
    ) -> Result<PreparedShellAgentLaunch, String> {
        let binary = self.ensure_binary()?;
        ensure_antigravity_integration()?;

        if !state.pane_exists(&request.pane_id)? {
            return Err(format!("pane {} was not found", request.pane_id));
        }

        let shell_cwd = PathBuf::from(&request.cwd);
        if !shell_cwd.is_dir() {
            return Err(format!(
                "Antigravity working directory {} does not exist",
                shell_cwd.display()
            ));
        }

        let cwd_str = shell_cwd.display().to_string();
        let pane_group_id = state
            .pane_group_id(&request.pane_id)?
            .ok_or_else(|| format!("pane {} was not found", request.pane_id))?;
        let resume_session_id = antigravity_resume_session_id(&request.args).map(str::to_string);
        let fork_point = None::<String>;

        let agent = match prepared_shell_agent(
            state,
            self.id(),
            request.prepared_agent_id.as_deref(),
            &request.pane_id,
            &pane_group_id,
            &cwd_str,
        )? {
            Some(prepared) => prepared,
            None => {
                match reusable_session_agent(
                    state,
                    self.id(),
                    resume_session_id.as_deref(),
                    &cwd_str,
                )? {
                    Some(existing) => existing,
                    None => prepare_agent_workspace(
                        state,
                        PrepareAgentWorkspaceRequest {
                            group_id: Some(pane_group_id),
                            base_repo: Some(cwd_str.clone()),
                            base_ref: Some("HEAD".to_string()),
                            adapter: self.id().to_string(),
                            model: shell_cli_model(&request.args),
                            effort: None,
                            use_worktree: false,
                        },
                    )?,
                }
            }
        };

        let agent = record_shell_session_lineage(
            state,
            agent,
            self.id(),
            fork_point.as_deref(),
            resume_session_id.as_deref(),
            &cwd_str,
        )?;
        let agent = apply_shell_cli_model(state, agent, &request.args)?;

        // If resume session id is provided, bind transcript path immediately
        if let Some(session_id) = resume_session_id.as_deref() {
            if let Ok(home) = antigravity_home() {
                let transcript_path = transcript_path_for_session(&home, session_id);
                if transcript_path.is_file() {
                    let path_str = transcript_path.display().to_string();
                    let _ = state.mutate_agent(&agent.id, |a| {
                        a.session_id = Some(session_id.to_string());
                        a.transcript_path = Some(path_str.clone());
                    });
                    start_transcript_tail(
                        state.clone(),
                        agent.id.clone(),
                        path_str,
                        self.id().to_string(),
                    );
                }
            }
        }

        let agent = attach_antigravity_agent_pane(
            state,
            &agent.id,
            request.pane_id.clone(),
            antigravity_args_contain_prompt(&request.args),
        )?;

        let mut envs = agent_pane_envs(state, &request.pane_id, &agent.id)?;
        if let Some(session_id) = resume_session_id {
            envs.push(("QMUX_ROOT_SESSION_ID".to_string(), session_id));
        }

        let agent_id = agent.id.clone();
        let launch_cwd = shell_cwd.display().to_string();

        state.emit(QmuxEvent::new(
            "agent.spawned",
            Some(request.pane_id),
            Some(agent_id),
            json!({ "agent": agent.clone(), "source": "shell" }),
        ));

        Ok(PreparedShellAgentLaunch {
            binary,
            cwd: launch_cwd,
            args: request.args,
            envs: envs
                .into_iter()
                .map(|(key, value)| LaunchEnv { key, value })
                .collect(),
            supervised: true,
        })
    }

    fn ingest_antigravity_notification(
        &self,
        state: &AppState,
        notification: AdapterNotification,
    ) -> Result<AdapterNotificationOutcome, String> {
        let agent = match (
            notification.agent_id.as_deref(),
            notification.pane_id.as_deref(),
        ) {
            (Some(agent_id), _) => state.agent(agent_id)?,
            (None, Some(pane_id)) => state.agent_by_pane(pane_id)?,
            (None, None) => None,
        };

        let Some(current) = agent else {
            return Ok(AdapterNotificationOutcome::Event(QmuxEvent::new(
                "agent.unknown",
                notification.pane_id,
                None,
                json!({
                    "event": notification.event,
                    "payload": notification.payload,
                }),
            )));
        };

        let session_id = super::string_field(&notification.payload, "conversationId")
            .or_else(|| super::string_field(&notification.payload, "conversation_id"))
            .or_else(|| super::string_field(&notification.payload, "sessionId"))
            .or_else(|| super::string_field(&notification.payload, "session_id"))
            .and_then(|session_id| valid_antigravity_session_id(&session_id).map(str::to_string));
        let reported_transcript_path = super::string_field(&notification.payload, "transcriptPath")
            .or_else(|| super::string_field(&notification.payload, "transcript_path"));
        let resolved_transcript_path = antigravity_notification_transcript_path(
            current.transcript_path.as_deref(),
            reported_transcript_path.as_deref(),
            session_id.as_deref(),
        );

        let is_stop = notification.event.as_str() == "Stop";
        let fully_idle_stop = is_stop
            && notification
                .payload
                .get("fullyIdle")
                .and_then(Value::as_bool)
                .unwrap_or(true);
        let next_status = match notification.event.as_str() {
            "PreInvocation" | "PreToolUse" | "PostToolUse" => Some(AgentStatus::Running),
            "Stop" => None,
            _ => None,
        };

        let mut needs_tail = false;
        let updated = state.mutate_agent(&current.id, |agent| {
            if let Some(sid) = session_id {
                agent.session_id = Some(sid);
            }
            if let Some(tp) = resolved_transcript_path {
                if agent.transcript_path.as_deref() != Some(&tp) {
                    agent.transcript_path = Some(tp);
                    needs_tail = true;
                }
            }
            if let Some(status) = next_status {
                agent.status = status;
            }
        })?;

        let drained = if fully_idle_stop {
            if let Some(agent) = updated.as_ref() {
                finish_agent_after_stop(state, agent)?
            } else {
                false
            }
        } else {
            false
        };

        let current_agent = if is_stop {
            state.agent(&current.id)?
        } else {
            updated
        };

        if needs_tail {
            if let Some(tp) = current_agent
                .as_ref()
                .and_then(|a| a.transcript_path.clone())
            {
                start_transcript_tail(state.clone(), current.id.clone(), tp, self.id().to_string());
            }
        }

        let event_type = if is_stop {
            if !fully_idle_stop || drained {
                "agent.running"
            } else {
                "agent.done"
            }
        } else {
            match notification.event.as_str() {
                "PreInvocation" => "agent.prompt_submitted",
                "PreToolUse" => "agent.tool_use",
                "PostToolUse" => "agent.tool_result",
                _ => "agent.running",
            }
        };

        let send_tracking = if notification.event == "PreInvocation" {
            let prompt = super::string_field(&notification.payload, "prompt").or_else(|| {
                current_agent
                    .as_ref()
                    .and_then(|agent| agent.transcript_path.as_deref())
                    .and_then(latest_antigravity_user_prompt)
            });
            Some(state.match_agent_prompt_submit(&current.id, prompt.as_deref())?)
        } else {
            None
        };
        let mut event_payload = json!({
            "hookEvent": notification.event,
            "agent": current_agent,
            "payload": notification.payload,
        });
        if let (Some(send_tracking), Value::Object(payload)) = (send_tracking, &mut event_payload) {
            payload.insert(
                "sendTracking".to_string(),
                serde_json::to_value(send_tracking)
                    .map_err(|err| format!("failed to encode send tracking: {err}"))?,
            );
        }
        Ok(AdapterNotificationOutcome::Event(QmuxEvent::new(
            event_type,
            notification.pane_id,
            Some(current.id.clone()),
            event_payload,
        )))
    }
}

fn finish_agent_after_stop(state: &AppState, agent: &AgentInfo) -> Result<bool, String> {
    match advance_after_idle(state, &agent.id) {
        Ok(IdleResolution::Drained) => Ok(true),
        Ok(IdleResolution::Paused | IdleResolution::Idle) => Ok(false),
        Err(err) => {
            state.emit(QmuxEvent::new(
                "agent.queue_error",
                agent.pane_id.clone(),
                Some(agent.id.clone()),
                json!({ "error": err }),
            ));
            Ok(false)
        }
    }
}

impl AgentAdapter for AntigravityAdapter {
    fn id(&self) -> &'static str {
        "antigravity"
    }

    fn display_name(&self) -> &'static str {
        "Antigravity"
    }

    fn configured_binary(&self) -> &str {
        &self.binary
    }

    fn launch(&self, state: &AppState, request: SpawnAgentRequest) -> Result<PaneInfo, String> {
        self.spawn_pane(state, request)
    }

    fn resume(
        &self,
        state: &AppState,
        pane: &PaneInfo,
        agent: &AgentInfo,
    ) -> Result<PaneInfo, String> {
        self.respawn_pane(state, pane, agent)
    }

    fn prepare_shell_launch(
        &self,
        state: &AppState,
        request: PrepareShellAgentLaunchRequest,
    ) -> Result<PreparedShellAgentLaunch, String> {
        self.prepare_shell_launch_inner(state, request)
    }

    fn shell_commands(&self) -> Vec<ShellCommandIntegration> {
        vec![
            ShellCommandIntegration {
                command_name: "agy",
                adapter_id: self.id(),
            },
            ShellCommandIntegration {
                command_name: "antigravity",
                adapter_id: self.id(),
            },
        ]
    }

    fn shell_resume_command(&self, session_id: &str) -> Option<String> {
        Some(format!(
            "agy --conversation {}",
            shell_quote_arg(session_id)
        ))
    }

    fn ingest_notification(
        &self,
        state: &AppState,
        notification: AdapterNotification,
    ) -> Result<AdapterNotificationOutcome, String> {
        self.ingest_antigravity_notification(state, notification)
    }

    fn parse_transcript_line(
        &self,
        agent_id: &str,
        source_index: usize,
        line: &str,
    ) -> Option<Turn> {
        parse_transcript_line(agent_id, source_index, line)
    }

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

    fn composer_policy(&self) -> ComposerPolicy {
        ComposerPolicy {
            ready_statuses: vec![
                AgentStatus::AwaitingInput,
                AgentStatus::Done,
                AgentStatus::Idle,
            ],
            queue_statuses: vec![
                AgentStatus::Starting,
                AgentStatus::Running,
                AgentStatus::AwaitingPermission,
            ],
            steer_statuses: vec![AgentStatus::Starting, AgentStatus::Running],
            permission_actions: Vec::new(),
        }
    }
}

fn attach_antigravity_agent_pane(
    state: &AppState,
    agent_id: &str,
    pane_id: String,
    has_initial_prompt: bool,
) -> Result<AgentInfo, String> {
    let agent = attach_agent_pane(state, agent_id, pane_id)?;
    if !has_initial_prompt
        && let Some(updated) = state.set_agent_status(agent_id, AgentStatus::Idle)?
    {
        return Ok(updated);
    }
    Ok(agent)
}

fn build_antigravity_args(_cwd: &Path, model: Option<&str>, prompt: Option<&str>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(prompt) = prompt.map(str::trim).filter(|prompt| !prompt.is_empty()) {
        args.push("--prompt-interactive".to_string());
        args.push(prompt.to_string());
    }
    args
}

fn antigravity_value_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--add-dir"
            | "--agent"
            | "--conversation"
            | "--effort"
            | "--input-format"
            | "--json-schema"
            | "--log-file"
            | "--mode"
            | "--model"
            | "--output-format"
            | "--print-timeout"
            | "--project"
    )
}

fn antigravity_args_contain_prompt(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if matches!(
            arg.as_str(),
            "--prompt-interactive" | "-i" | "--prompt" | "--print" | "-p"
        ) {
            return args
                .get(index + 1)
                .is_some_and(|value| !value.trim().is_empty());
        }
        if arg.starts_with("--prompt-interactive=")
            || arg.starts_with("--prompt=")
            || arg.starts_with("--print=")
        {
            return arg
                .split_once('=')
                .is_some_and(|(_, value)| !value.trim().is_empty());
        }
        if arg == "--" {
            return false;
        }
        if antigravity_value_flag(arg) {
            index += 2;
            continue;
        }
        index += 1;
    }
    false
}

fn build_antigravity_resume_args(
    _cwd: &Path,
    model: Option<&str>,
    session_id: Option<&str>,
) -> (Vec<String>, bool) {
    let mut args = Vec::new();
    let mut resumed = false;
    if let Some(session_id) = session_id.and_then(valid_antigravity_session_id) {
        args.push("--conversation".to_string());
        args.push(session_id.to_string());
        resumed = true;
    }
    if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    (args, resumed)
}

fn antigravity_resume_session_id(args: &[String]) -> Option<&str> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return None;
        }
        if arg == "--conversation" {
            return args
                .get(index + 1)
                .and_then(|session_id| valid_antigravity_session_id(session_id));
        }
        if let Some(session_id) = arg.strip_prefix("--conversation=") {
            return valid_antigravity_session_id(session_id);
        }
        index += 1;
    }
    None
}

fn valid_antigravity_session_id(session_id: &str) -> Option<&str> {
    let session_id = session_id.trim();
    (!session_id.is_empty()
        && session_id.len() <= 128
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'))
    .then_some(session_id)
}

pub(crate) fn antigravity_home() -> Result<PathBuf, String> {
    env::var_os("ANTIGRAVITY_APP_DATA_DIR")
        .or_else(|| env::var_os("ANTIGRAVITY_HOME"))
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".gemini").join("antigravity-cli"))
        })
        .ok_or_else(|| "HOME is not set; cannot resolve Antigravity home".to_string())
}

pub(crate) fn antigravity_config_dir() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".gemini").join("config"))
        .ok_or_else(|| "HOME is not set; cannot resolve Antigravity config directory".to_string())
}

pub(crate) fn transcript_path_for_session(antigravity_home: &Path, session_id: &str) -> PathBuf {
    antigravity_home
        .join("brain")
        .join(session_id)
        .join(".system_generated")
        .join("logs")
        .join("transcript.jsonl")
}

fn antigravity_notification_transcript_path(
    current: Option<&str>,
    reported: Option<&str>,
    session_id: Option<&str>,
) -> Option<String> {
    antigravity_notification_transcript_path_in(
        &antigravity_home().ok()?,
        current,
        reported,
        session_id,
    )
}

fn antigravity_notification_transcript_path_in(
    home: &Path,
    current: Option<&str>,
    reported: Option<&str>,
    session_id: Option<&str>,
) -> Option<String> {
    let session_id = session_id.and_then(valid_antigravity_session_id)?;
    let expected = transcript_path_for_session(home, session_id);
    let candidate = reported
        .map(PathBuf::from)
        .unwrap_or_else(|| expected.clone());
    let valid_name = candidate
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "transcript.jsonl" | "transcript_full.jsonl"));
    if !valid_name || candidate.parent() != expected.parent() {
        return None;
    }
    let candidate = candidate.display().to_string();
    hook_transcript_path_acceptable(current, &candidate).then_some(candidate)
}

fn latest_antigravity_user_prompt(transcript_path: &str) -> Option<String> {
    fs::read_to_string(transcript_path)
        .ok()?
        .lines()
        .rev()
        .find_map(|line| {
            let value = serde_json::from_str::<Value>(line).ok()?;
            if value.get("type").and_then(Value::as_str) != Some("USER_INPUT") {
                return None;
            }
            let prompt = unwrap_user_request(value.get("content")?.as_str()?);
            (!prompt.is_empty()).then(|| prompt.to_string())
        })
}

pub(crate) fn unwrap_user_request(raw: &str) -> &str {
    if let Some(start) = raw.find("<USER_REQUEST>") {
        let after = &raw[start + "<USER_REQUEST>".len()..];
        if let Some(end) = after.find("</USER_REQUEST>") {
            return after[..end].trim();
        }
    }
    raw.trim()
}

fn is_tool_error(status: Option<&str>, content: &str) -> bool {
    if status == Some("ERROR") {
        return true;
    }
    if content.starts_with("Error:") || content.starts_with("Encountered error") {
        return true;
    }
    if let Some(idx) = content.find("The command exited with code ") {
        let rest = &content[idx + "The command exited with code ".len()..];
        let code = rest
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>();
        if let Ok(code_num) = code.parse::<i32>() {
            if code_num != 0 {
                return true;
            }
        }
    }
    false
}

pub(crate) fn parse_transcript_line(
    agent_id: &str,
    source_index: usize,
    line: &str,
) -> Option<Turn> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    let record_type = value.get("type").and_then(Value::as_str)?;
    let created_at = value
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(rfc3339_to_epoch_ms)
        .or_else(|| native_timestamp_ms(&value));
    let step_index = value.get("step_index").and_then(Value::as_u64);

    match record_type {
        "USER_INPUT" => {
            let raw_content = value.get("content").and_then(Value::as_str).unwrap_or("");
            let clean_prompt = unwrap_user_request(raw_content);
            if clean_prompt.is_empty() {
                return None;
            }
            Some(Turn {
                id: format!("{agent_id}-{source_index}"),
                agent_id: agent_id.to_string(),
                session_id: None,
                role: "user".to_string(),
                blocks: vec![TurnBlock::Text {
                    text: clean_prompt.to_string(),
                }],
                source_index,
                timestamp: created_at,
                status: None,
                status_reason: None,
                context_status: None,
                native_id: step_index.map(|idx| idx.to_string()),
                parent_native_id: None,
                native_message_id: None,
            })
        }
        "PLANNER_RESPONSE" => {
            let mut blocks = Vec::new();
            if let Some(thinking) = value.get("thinking").and_then(Value::as_str) {
                let trimmed = thinking.trim();
                if !trimmed.is_empty() {
                    blocks.push(TurnBlock::Raw {
                        value: json!({
                            "type": "thinking",
                            "thinking": trimmed,
                        }),
                    });
                }
            }
            if let Some(content) = value.get("content").and_then(Value::as_str) {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    blocks.push(TurnBlock::Text {
                        text: trimmed.to_string(),
                    });
                }
            }
            if let Some(tool_calls) = value.get("tool_calls").and_then(Value::as_array) {
                for call in tool_calls {
                    let name = call
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_string();
                    let input = call.get("args").cloned().unwrap_or(Value::Null);
                    blocks.push(TurnBlock::ToolUse {
                        id: None,
                        name,
                        input,
                    });
                }
            }
            if blocks.is_empty() {
                return None;
            }
            Some(Turn {
                id: format!("{agent_id}-{source_index}"),
                agent_id: agent_id.to_string(),
                session_id: None,
                role: "assistant".to_string(),
                blocks,
                source_index,
                timestamp: created_at,
                status: None,
                status_reason: None,
                context_status: None,
                native_id: step_index.map(|idx| idx.to_string()),
                parent_native_id: None,
                native_message_id: None,
            })
        }
        "GENERIC" => {
            let content_str = match value.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            if content_str.is_empty() {
                return None;
            }
            let is_error = is_tool_error(value.get("status").and_then(Value::as_str), &content_str);
            Some(Turn {
                id: format!("{agent_id}-{source_index}"),
                agent_id: agent_id.to_string(),
                session_id: None,
                role: "tool".to_string(),
                blocks: vec![TurnBlock::ToolResult {
                    tool_use_id: None,
                    content: Value::String(content_str),
                    is_error,
                }],
                source_index,
                timestamp: created_at,
                status: None,
                status_reason: None,
                context_status: None,
                native_id: step_index.map(|idx| idx.to_string()),
                parent_native_id: None,
                native_message_id: None,
            })
        }
        _ => None,
    }
}

pub(crate) fn antigravity_hook_shim() -> &'static str {
    r#"#!/bin/sh
event="${1:-}"
respond() {
  if [ "$event" = "Stop" ]; then
    echo '{"decision":""}'
  else
    echo '{}'
  fi
}
if [ -z "$event" ]; then
  respond
  exit 0
fi
if [ -z "${QMUX_SOCK:-}" ] || [ -z "${QMUX_TOKEN:-}" ] || [ -z "${QMUX_PANE_ID:-}" ] || [ -z "${QMUX_AGENT_ID:-}" ] || [ -z "${QMUX_CLI:-}" ]; then
  respond
  exit 0
fi
"$QMUX_CLI" notify "$event"
respond
"#
}

fn antigravity_hooks_spec(shim_path: &Path) -> Value {
    let command_prefix = shell_quote_path(shim_path);
    let mut hooks_map = serde_json::Map::new();
    for event in ANTIGRAVITY_HOOK_EVENTS {
        let handler = json!({
            "type": "command",
            "command": format!("{command_prefix} {event}"),
        });
        let handlers = if matches!(*event, "PreToolUse" | "PostToolUse") {
            json!([{
                "matcher": "*",
                "hooks": [handler],
            }])
        } else {
            json!([handler])
        };
        hooks_map.insert(event.to_string(), handlers);
    }
    Value::Object(hooks_map)
}

fn merge_hooks_json(existing: &str, shim_path: &Path) -> Result<String, String> {
    let mut parsed = if existing.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str::<Value>(existing)
            .map_err(|err| format!("existing hooks file is invalid JSON: {err}"))?
    };
    let map = parsed
        .as_object_mut()
        .ok_or_else(|| "existing hooks file must contain a JSON object".to_string())?;
    map.insert("qmux".to_string(), antigravity_hooks_spec(shim_path));
    let mut formatted = serde_json::to_string_pretty(&parsed)
        .map_err(|err| format!("failed to encode Antigravity hooks: {err}"))?;
    formatted.push('\n');
    Ok(formatted)
}

pub(crate) fn ensure_antigravity_integration() -> Result<(), String> {
    let home = antigravity_home()?;
    let qmux_dir = home.join("qmux");
    fs::create_dir_all(&qmux_dir)
        .map_err(|err| format!("failed to create {}: {err}", qmux_dir.display()))?;

    let shim_path = qmux_dir.join("qmux-antigravity-hook");
    let shim_content = antigravity_hook_shim();
    let write_shim = match fs::read_to_string(&shim_path) {
        Ok(existing) => existing != shim_content,
        Err(_) => true,
    };
    if write_shim {
        fs::write(&shim_path, shim_content)
            .map_err(|err| format!("failed to write {}: {err}", shim_path.display()))?;
        fs::set_permissions(&shim_path, fs::Permissions::from_mode(0o755))
            .map_err(|err| format!("failed to chmod {}: {err}", shim_path.display()))?;
    }

    let config_dir = antigravity_config_dir()?;
    fs::create_dir_all(&config_dir)
        .map_err(|err| format!("failed to create {}: {err}", config_dir.display()))?;
    install_hooks_file(&config_dir.join("hooks.json"), &shim_path)
}

fn install_hooks_file(hooks_path: &Path, shim_path: &Path) -> Result<(), String> {
    let existing_raw = match fs::read_to_string(hooks_path) {
        Ok(existing) => existing,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("failed to read {}: {err}", hooks_path.display())),
    };
    let updated = merge_hooks_json(&existing_raw, shim_path)
        .map_err(|err| format!("failed to update {}: {err}", hooks_path.display()))?;
    if existing_raw != updated {
        fs::write(hooks_path, updated)
            .map_err(|err| format!("failed to write {}: {err}", hooks_path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_user_input_unwraps_user_request() {
        let line = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-09-04T02:47:31.989209-07:00","content":"<USER_REQUEST>\nhello world\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\ntime\n</ADDITIONAL_METADATA>"}"#;
        let turn = parse_transcript_line("agent-1", 0, line).expect("turn should parse");
        assert_eq!(turn.role, "user");
        assert_eq!(turn.id, "agent-1-0");
        assert_eq!(turn.blocks.len(), 1);
        match &turn.blocks[0] {
            TurnBlock::Text { text } => assert_eq!(text, "hello world"),
            _ => panic!("expected text block"),
        }
    }

    #[test]
    fn parse_user_input_plain_text() {
        let line = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-09-04T02:47:31.989209-07:00","content":"plain prompt"}"#;
        let turn = parse_transcript_line("agent-1", 0, line).expect("turn should parse");
        assert_eq!(turn.role, "user");
        match &turn.blocks[0] {
            TurnBlock::Text { text } => assert_eq!(text, "plain prompt"),
            _ => panic!("expected text block"),
        }
    }

    #[test]
    fn parse_planner_response_with_thinking_and_tool() {
        let line = r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-09-04T02:48:10.518659-07:00","content":null,"thinking":"I should list the files.","tool_calls":[{"name":"list_dir","args":{"DirectoryPath":"/Users/test"}}]}"#;
        let turn = parse_transcript_line("agent-1", 1, line).expect("turn should parse");
        assert_eq!(turn.role, "assistant");
        assert_eq!(turn.blocks.len(), 2);
        match &turn.blocks[0] {
            TurnBlock::Raw { value } => {
                assert_eq!(value["type"], "thinking");
                assert_eq!(value["thinking"], "I should list the files.");
            }
            _ => panic!("expected thinking raw block"),
        }
        match &turn.blocks[1] {
            TurnBlock::ToolUse { name, input, id } => {
                assert_eq!(name, "list_dir");
                assert_eq!(id, &None);
                assert_eq!(input["DirectoryPath"], "/Users/test");
            }
            _ => panic!("expected tool use block"),
        }
    }

    #[test]
    fn parse_planner_response_prose_content() {
        let line = r#"{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-09-04T02:48:15.000000-07:00","content":"Here is the answer.","thinking":null,"tool_calls":[]}"#;
        let turn = parse_transcript_line("agent-1", 2, line).expect("turn should parse");
        assert_eq!(turn.role, "assistant");
        assert_eq!(turn.blocks.len(), 1);
        match &turn.blocks[0] {
            TurnBlock::Text { text } => assert_eq!(text, "Here is the answer."),
            _ => panic!("expected text block"),
        }
    }

    #[test]
    fn parse_generic_tool_result_success() {
        let line = r#"{"step_index":3,"source":"USER_EXPLICIT","type":"GENERIC","status":"DONE","created_at":"2026-09-04T02:48:20.000000-07:00","content":"{\"name\":\"file.txt\"}"}"#;
        let turn = parse_transcript_line("agent-1", 3, line).expect("turn should parse");
        assert_eq!(turn.role, "tool");
        assert_eq!(turn.blocks.len(), 1);
        match &turn.blocks[0] {
            TurnBlock::ToolResult {
                content, is_error, ..
            } => {
                assert!(!is_error);
                assert_eq!(content, &json!("{\"name\":\"file.txt\"}"));
            }
            _ => panic!("expected tool result block"),
        }
    }

    #[test]
    fn parse_generic_tool_result_error() {
        let line = r#"{"step_index":4,"source":"USER_EXPLICIT","type":"GENERIC","status":"ERROR","created_at":"2026-09-04T02:48:25.000000-07:00","content":"Error: file not found"}"#;
        let turn = parse_transcript_line("agent-1", 4, line).expect("turn should parse");
        assert_eq!(turn.role, "tool");
        match &turn.blocks[0] {
            TurnBlock::ToolResult { is_error, .. } => assert!(is_error),
            _ => panic!("expected tool result block"),
        }
    }

    #[test]
    fn ignores_checkpoint_records() {
        let line = r#"{"step_index":5,"source":"SYSTEM","type":"CHECKPOINT","status":"DONE","created_at":"2026-09-04T02:48:30.000000-07:00","content":"summary"}"#;
        assert!(parse_transcript_line("agent-1", 5, line).is_none());
    }

    fn test_config() -> QmuxConfig {
        QmuxConfig {
            remotes: Default::default(),
            workspace_root: PathBuf::from("/tmp/qmux-antigravity-tests"),
            socket_path: PathBuf::from("/tmp/qmux-antigravity-tests.sock"),
            adapters: Default::default(),
            legacy_claude_binary: None,
            claude_plugin_dir: PathBuf::new(),
            opencode_plugin_dir: PathBuf::new(),
            pi_extension_dir: PathBuf::new(),
            cursor_plugin_dir: PathBuf::new(),
        }
    }

    #[test]
    fn shell_commands_registration() {
        let config = test_config();
        let adapter = AntigravityAdapter::new(&config);
        let commands = adapter.shell_commands();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].command_name, "agy");
        assert_eq!(commands[0].adapter_id, "antigravity");
        assert_eq!(commands[1].command_name, "antigravity");
        assert_eq!(commands[1].adapter_id, "antigravity");
    }

    #[test]
    fn launch_and_resume_args_follow_agy_cli_contract() {
        assert_eq!(
            build_antigravity_args(Path::new("/tmp"), Some("gemini-test"), Some(" fix it ")),
            vec!["--model", "gemini-test", "--prompt-interactive", "fix it"]
        );
        let (resume, resumed) = build_antigravity_resume_args(
            Path::new("/tmp"),
            Some("gemini-test"),
            Some("conversation-123"),
        );
        assert!(resumed);
        assert_eq!(
            resume,
            vec![
                "--conversation",
                "conversation-123",
                "--model",
                "gemini-test"
            ]
        );
        assert_eq!(
            antigravity_resume_session_id(&["--conversation=conversation-123".to_string()]),
            Some("conversation-123")
        );
        assert_eq!(
            AntigravityAdapter::new(&test_config())
                .shell_resume_command("conversation-123")
                .as_deref(),
            Some("agy --conversation 'conversation-123'")
        );
    }

    #[test]
    fn shell_prompt_detection_only_accepts_agy_prompt_flags() {
        let args = |values: &[&str]| values.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert!(antigravity_args_contain_prompt(&args(&["-i", "fix it"])));
        assert!(antigravity_args_contain_prompt(&args(&[
            "--prompt-interactive=fix it"
        ])));
        assert!(antigravity_args_contain_prompt(&args(&[
            "--print", "answer"
        ])));
        assert!(!antigravity_args_contain_prompt(&args(&[
            "--conversation",
            "conversation-123"
        ])));
        assert!(!antigravity_args_contain_prompt(&args(&["models"])));
    }

    #[test]
    fn hook_schema_distinguishes_lifecycle_and_tool_handlers() {
        let hooks = antigravity_hooks_spec(Path::new("/bin/qmux-antigravity-hook"));
        assert_eq!(hooks["PreInvocation"][0]["type"], "command");
        assert!(hooks["PreInvocation"][0].get("hooks").is_none());
        assert_eq!(hooks["PostToolUse"][0]["matcher"], "*");
        assert_eq!(hooks["PostToolUse"][0]["hooks"][0]["type"], "command");
        assert_eq!(hooks["Stop"][0]["type"], "command");
        assert!(hooks.get("PreToolUse").is_none());
    }

    #[test]
    fn merge_hooks_json_preserves_other_keys() {
        let existing = r#"{"custom-checker":{"PreToolUse":[]}}"#;
        let shim = Path::new("/bin/qmux-antigravity-hook");
        let merged = merge_hooks_json(existing, shim).unwrap();
        let val: Value = serde_json::from_str(&merged).unwrap();
        assert!(val.get("custom-checker").is_some());
        assert!(val.get("qmux").is_some());
    }

    #[test]
    fn invalid_hooks_json_is_not_replaced() {
        let dir = std::env::temp_dir().join(format!(
            "qmux-antigravity-hooks-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let hooks_path = dir.join("hooks.json");
        fs::write(&hooks_path, "{ invalid").unwrap();
        assert!(install_hooks_file(&hooks_path, Path::new("/bin/hook")).is_err());
        assert_eq!(fs::read_to_string(&hooks_path).unwrap(), "{ invalid");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn transcript_paths_are_confined_to_the_reported_conversation() {
        let home = Path::new("/tmp/qmux-antigravity-home");
        let expected = transcript_path_for_session(home, "conversation-123");
        assert_eq!(
            antigravity_notification_transcript_path_in(
                home,
                None,
                Some(expected.to_str().unwrap()),
                Some("conversation-123"),
            )
            .as_deref(),
            expected.to_str()
        );
        let full = expected.with_file_name("transcript_full.jsonl");
        assert_eq!(
            antigravity_notification_transcript_path_in(
                home,
                Some(expected.to_str().unwrap()),
                Some(full.to_str().unwrap()),
                Some("conversation-123"),
            )
            .as_deref(),
            full.to_str()
        );
        assert!(
            antigravity_notification_transcript_path_in(
                home,
                None,
                Some("/tmp/other.jsonl"),
                Some("conversation-123"),
            )
            .is_none()
        );
        assert!(
            antigravity_notification_transcript_path_in(home, None, None, Some("../../escape"),)
                .is_none()
        );
    }

    #[test]
    fn latest_user_prompt_reads_the_native_transcript_envelope() {
        let path = std::env::temp_dir().join(format!(
            "qmux-antigravity-transcript-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(
            &path,
            concat!(
                "{\"type\":\"USER_INPUT\",\"content\":\"older\"}\n",
                "{\"type\":\"PLANNER_RESPONSE\",\"content\":\"answer\"}\n",
                "{\"type\":\"USER_INPUT\",\"content\":\"<USER_REQUEST>new prompt</USER_REQUEST>\"}\n"
            ),
        )
        .unwrap();
        assert_eq!(
            latest_antigravity_user_prompt(path.to_str().unwrap()).as_deref(),
            Some("new prompt")
        );
        fs::remove_file(path).unwrap();
    }

    fn sample_agent() -> AgentInfo {
        AgentInfo {
            id: "agent-1".to_string(),
            group_id: "group-1".to_string(),
            adapter: "antigravity".to_string(),
            worktree_dir: "/tmp/qmux-antigravity-tests".to_string(),
            branch: None,
            active_workspace: None,
            pane_id: None,
            orphaned_queue_pane_id: None,
            session_id: None,
            transcript_path: None,
            status: AgentStatus::Starting,
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
    fn notification_ingest_handles_pre_invocation_and_stop() {
        let state = AppState::new(test_config());
        let adapter = AntigravityAdapter::new(&test_config());
        state.insert_agent(sample_agent()).unwrap();
        state
            .record_agent_send(
                "agent-1",
                "queued prompt".to_string(),
                crate::state::AgentSendSource::QueuedTurn,
            )
            .unwrap();

        // PreInvocation sets status to Running and records conversationId
        let pre_outcome = adapter
            .ingest_antigravity_notification(
                &state,
                AdapterNotification {
                    adapter_id: Some("antigravity".to_string()),
                    agent_id: Some("agent-1".to_string()),
                    pane_id: None,
                    event: "PreInvocation".to_string(),
                    payload: json!({
                        "conversationId": "sess-12345",
                        "prompt": "queued prompt",
                    }),
                },
            )
            .unwrap();

        let AdapterNotificationOutcome::Event(evt) = pre_outcome;
        assert_eq!(evt.event_type, "agent.prompt_submitted");
        assert_eq!(evt.payload["sendTracking"]["status"], "matched");
        assert!(state.outstanding_agent_sends("agent-1").unwrap().is_empty());
        let agent = state.agent("agent-1").unwrap().unwrap();
        assert_eq!(agent.status, AgentStatus::Running);
        assert_eq!(agent.session_id.as_deref(), Some("sess-12345"));

        let background_stop = adapter
            .ingest_antigravity_notification(
                &state,
                AdapterNotification {
                    adapter_id: Some("antigravity".to_string()),
                    agent_id: Some("agent-1".to_string()),
                    pane_id: None,
                    event: "Stop".to_string(),
                    payload: json!({ "fullyIdle": false }),
                },
            )
            .unwrap();
        let AdapterNotificationOutcome::Event(evt) = background_stop;
        assert_eq!(evt.event_type, "agent.running");
        assert_eq!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        );

        // Stop transitions status to Done via finish_agent_after_stop
        let stop_outcome = adapter
            .ingest_antigravity_notification(
                &state,
                AdapterNotification {
                    adapter_id: Some("antigravity".to_string()),
                    agent_id: Some("agent-1".to_string()),
                    pane_id: None,
                    event: "Stop".to_string(),
                    payload: json!({ "fullyIdle": true }),
                },
            )
            .unwrap();

        let AdapterNotificationOutcome::Event(evt) = stop_outcome;
        assert_eq!(evt.event_type, "agent.done");
        let agent = state.agent("agent-1").unwrap().unwrap();
        assert_eq!(agent.status, AgentStatus::Done);
    }
}
