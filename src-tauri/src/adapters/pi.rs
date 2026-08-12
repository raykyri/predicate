use super::{
    AdapterNotification, AdapterNotificationOutcome, AgentAdapter, ComposerPolicy, LaunchEnv,
    PrepareShellAgentLaunchRequest, PreparedShellAgentLaunch, ShellCommandIntegration,
    SpawnAgentRequest, apply_shell_cli_model, cli_flag_value, ensure_on_path,
    hook_transcript_path_acceptable, prepared_shell_agent, record_shell_session_lineage,
    reusable_session_agent, shell_cli_model, shell_quote_arg,
};
use crate::config::QmuxConfig;
use crate::events::QmuxEvent;
use crate::pty::{
    CommandPlan, InitialPaneSize, PaneMeta, agent_pane_envs, plan_to_spec, recoverable_dir,
    spawn_pty,
};
use crate::state::{AppState, PaneInfo, PaneKind};
use crate::transcript::{Turn, start_transcript_tail, string_field};
use crate::turn_queue::{IdleResolution, advance_after_idle, is_shell_escape_turn};
use crate::workspace::{
    AgentInfo, AgentStatus, PrepareAgentWorkspaceRequest, attach_agent_pane, mark_agent_failed,
    mark_agent_spawn_failed, prepare_agent_workspace, prepare_agent_workspace_with_parent,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Command;

const MIN_PI_MAJOR: u64 = 0;
const MIN_PI_MINOR: u64 = 80;

#[derive(Clone, Debug)]
pub struct PiAdapter {
    binary: String,
    extension_dir: PathBuf,
}

impl PiAdapter {
    pub fn new(config: &QmuxConfig) -> Self {
        Self {
            binary: config.pi_binary(),
            extension_dir: config.pi_extension_dir.clone(),
        }
    }

    fn ensure_binary(&self) -> Result<String, String> {
        let binary = ensure_on_path(&self.binary).ok_or_else(|| {
            format!(
                "Pi adapter binary '{}' was not found on PATH or standard macOS tool paths. Install Pi or update adapters.pi.binary in qmux.config.json.",
                self.binary
            )
        })?;
        Ok(binary.display().to_string())
    }

    fn ensure_compatible_binary(&self) -> Result<String, String> {
        let binary = self.ensure_binary()?;
        let output = Command::new(&binary)
            .arg("--version")
            .output()
            .map_err(|err| format!("failed to read Pi version from '{binary}': {err}"))?;
        if !output.status.success() {
            return Err(format!(
                "failed to read Pi version from '{binary}' (exit {})",
                output.status
            ));
        }
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !pi_version_is_compatible(&version) {
            return Err(format!(
                "qmux requires Pi {MIN_PI_MAJOR}.{MIN_PI_MINOR}.0 or newer; '{binary}' reported {version:?}"
            ));
        }
        Ok(binary)
    }

    fn extension_entrypoint(&self) -> Result<PathBuf, String> {
        let entrypoint = self.extension_dir.join("index.js");
        if !self.extension_dir.is_dir() || !entrypoint.is_file() {
            return Err(format!(
                "Pi integration extension was not found at {}. Reinstall qmux or set QMUX_PI_EXTENSION_DIR to the bundled qmux-pi-extension directory.",
                entrypoint.display()
            ));
        }
        Ok(entrypoint)
    }

    fn integration_args(&self) -> Result<Vec<String>, String> {
        Ok(vec![
            "--extension".to_string(),
            self.extension_entrypoint()?.display().to_string(),
        ])
    }

    fn spawn_pane(&self, state: &AppState, request: SpawnAgentRequest) -> Result<PaneInfo, String> {
        let binary = self.ensure_compatible_binary()?;
        let mut args = self.integration_args()?;
        let _options = PiLaunchOptions::from_value(request.options)?;
        let agent = prepare_agent_workspace_with_parent(
            state,
            PrepareAgentWorkspaceRequest {
                group_id: request.group_id,
                base_repo: request.base_repo,
                base_ref: request.base_ref,
                adapter: self.id().to_string(),
                // Pi owns model selection; qmux only observes it after startup.
                model: None,
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
                "Pi working directory {} does not exist",
                cwd.display()
            ));
        }

        let prompt = request.prompt.trim();
        if !prompt.is_empty() {
            args.push("--".to_string());
            args.push(prompt.to_string());
        }

        let pane_id = state.next_id("pane");
        let mut envs = agent_pane_envs(state, &pane_id, &agent.id)?;
        envs.push(("QMUX_ADAPTER_ID".to_string(), self.id().to_string()));
        attach_pi_agent_pane(state, &agent.id, pane_id.clone(), !prompt.is_empty())?;

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
        let binary = self.ensure_compatible_binary()?;
        let cwd = recoverable_dir(&agent.worktree_dir).ok_or_else(|| {
            format!(
                "agent worktree {} no longer exists; relaunch manually",
                agent.worktree_dir
            )
        })?;
        let mut args = self.integration_args()?;
        let resume_target = agent
            .transcript_path
            .as_deref()
            .or(agent.session_id.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let resumed = if let Some(target) = resume_target {
            args.push("--session".to_string());
            args.push(target.to_string());
            true
        } else {
            false
        };
        let mut envs = agent_pane_envs(state, &pane.id, &agent.id)?;
        envs.push(("QMUX_ADAPTER_ID".to_string(), self.id().to_string()));

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

        let mut restored = agent.clone();
        restored.pane_id = Some(pane.id.clone());
        restored.status = AgentStatus::Idle;
        state.update_agent(restored.clone())?;
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
        let binary = self.ensure_compatible_binary()?;
        let mut args = self.integration_args()?;
        validate_pi_supervised_args(&request.args)?;
        if !state.pane_exists(&request.pane_id)? {
            return Err(format!("pane {} was not found", request.pane_id));
        }
        let cwd = PathBuf::from(&request.cwd);
        if !cwd.is_dir() {
            return Err(format!(
                "Pi working directory {} does not exist",
                cwd.display()
            ));
        }
        let cwd_str = cwd.display().to_string();
        let pane_group_id = state
            .pane_group_id(&request.pane_id)?
            .ok_or_else(|| format!("pane {} was not found", request.pane_id))?;
        let resume_session_id = pi_resume_session_id(&request.args);
        let fork_point = cli_flag_value(&request.args, "--fork");
        let agent = match prepared_shell_agent(
            state,
            self.id(),
            request.prepared_agent_id.as_deref(),
            &request.pane_id,
            &pane_group_id,
            &cwd_str,
        )? {
            Some(prepared) => prepared,
            None => match reusable_session_agent(
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
                        effort: pi_shell_thinking(&request.args),
                        use_worktree: false,
                    },
                )?,
            },
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
        let agent = apply_pi_shell_thinking(state, agent, &request.args)?;
        let has_prompt = pi_args_contain_prompt(&request.args);
        let agent = attach_pi_agent_pane(state, &agent.id, request.pane_id.clone(), has_prompt)?;

        args.extend(request.args);
        let mut envs = agent_pane_envs(state, &request.pane_id, &agent.id)?;
        envs.push(("QMUX_ADAPTER_ID".to_string(), self.id().to_string()));
        if let Some(session_id) = resume_session_id {
            envs.push(("QMUX_ROOT_SESSION_ID".to_string(), session_id));
        }
        if let Some(fork_point) = fork_point {
            envs.push(("QMUX_FORK_POINT".to_string(), fork_point));
        }
        state.emit(QmuxEvent::new(
            "agent.spawned",
            Some(request.pane_id.clone()),
            Some(agent.id.clone()),
            json!({ "agent": agent, "source": "shell" }),
        ));
        Ok(PreparedShellAgentLaunch {
            binary,
            cwd: cwd_str,
            args,
            envs: envs
                .into_iter()
                .map(|(key, value)| LaunchEnv { key, value })
                .collect(),
            supervised: true,
        })
    }

    fn ingest_pi_notification(
        &self,
        state: &AppState,
        notification: AdapterNotification,
    ) -> Result<AdapterNotificationOutcome, String> {
        let pane_id = notification.pane_id.clone();
        let mut send_tracking = None;
        let agent = notification
            .agent_id
            .as_deref()
            .and_then(|agent_id| state.agent(agent_id).ok().flatten())
            .or_else(|| {
                pane_id
                    .as_deref()
                    .and_then(|pane_id| state.agent_by_pane(pane_id).ok().flatten())
            });
        let hook_event = notification.event.clone();
        let event_type = match hook_event.as_str() {
            "PiExtensionReady" => "agent.integration_ready",
            "PiSessionStart" => {
                if let Some(current) = agent.as_ref() {
                    let session_id = string_field(&notification.payload, "session_id");
                    let transcript_path = string_field(&notification.payload, "session_file")
                        .filter(|candidate| {
                            hook_transcript_path_acceptable(
                                current.transcript_path.as_deref(),
                                candidate,
                            )
                        });
                    let leaf_id = string_field(&notification.payload, "leaf_id");
                    let model = pi_model_from_payload(&notification.payload);
                    let effort = string_field(&notification.payload, "thinking_level");
                    let updated = state.mutate_agent(&current.id, |agent| {
                        if let Some(session_id) = session_id.clone() {
                            agent.session_id = Some(session_id);
                        }
                        if let Some(transcript_path) = transcript_path.clone() {
                            agent.transcript_path = Some(transcript_path);
                        }
                        agent.native_leaf_id = leaf_id.clone();
                        if model.is_some() {
                            agent.model = model.clone();
                        }
                        if effort.is_some() {
                            agent.effort = effort.clone();
                        }
                        if agent.status == AgentStatus::Starting {
                            agent.status = AgentStatus::Idle;
                        }
                    })?;
                    if let Some(path) = updated.and_then(|agent| agent.transcript_path) {
                        start_transcript_tail(
                            state.clone(),
                            current.id.clone(),
                            path,
                            self.id().to_string(),
                        );
                    }
                }
                "agent.session_start"
            }
            "PiPromptSubmit" => {
                if let Some(agent) = agent.as_ref() {
                    let prompt = string_field(&notification.payload, "prompt");
                    if !prompt.as_deref().is_some_and(is_shell_escape_turn) {
                        state.set_agent_status(&agent.id, AgentStatus::Running)?;
                    }
                    send_tracking =
                        Some(state.match_agent_prompt_submit(&agent.id, prompt.as_deref())?);
                }
                "agent.prompt_submitted"
            }
            "PiAgentStart" | "PiTurnStart" => {
                if let Some(agent) = agent.as_ref() {
                    state.set_agent_status(&agent.id, AgentStatus::Running)?;
                }
                "agent.running"
            }
            "PiAgentEnd" | "PiTurnEnd" => "agent.running",
            "PiAgentSettled" => {
                let drained = if let Some(agent) = agent.as_ref() {
                    finish_agent_after_settled(state, agent)?
                } else {
                    false
                };
                if drained {
                    "agent.running"
                } else {
                    "agent.done"
                }
            }
            "PiModelSelect" | "PiThinkingLevelSelect" => {
                if let Some(agent) = agent.as_ref() {
                    let model = pi_model_from_payload(&notification.payload);
                    let effort = string_field(&notification.payload, "thinking_level");
                    state.mutate_agent(&agent.id, |agent| {
                        if model.is_some() {
                            agent.model = model.clone();
                        }
                        if effort.is_some() {
                            agent.effort = effort.clone();
                        }
                    })?;
                }
                "agent.updated"
            }
            "PiSessionTree" | "PiSessionCompact" => {
                if let Some(agent) = agent.as_ref() {
                    let leaf_id = string_field(&notification.payload, "leaf_id");
                    state.mutate_agent(&agent.id, |agent| {
                        agent.native_leaf_id = leaf_id.clone();
                    })?;
                }
                "agent.updated"
            }
            "PiSessionInfoChanged" => "agent.updated",
            "PiSessionShutdown" => "agent.session_shutdown",
            _ => "agent.hook",
        };

        let agent = match agent {
            Some(agent) => state.agent(&agent.id)?.or(Some(agent)),
            None => None,
        };
        let mut payload = json!({
            "hookEvent": hook_event,
            "payload": notification.payload,
        });
        if let Some(send_tracking) = send_tracking
            && let Value::Object(payload) = &mut payload
        {
            payload.insert(
                "sendTracking".to_string(),
                serde_json::to_value(send_tracking)
                    .map_err(|err| format!("failed to encode send tracking: {err}"))?,
            );
        }
        if let (Value::Object(payload), Some(agent)) = (&mut payload, agent.as_ref()) {
            payload.insert(
                "agent".to_string(),
                serde_json::to_value(agent)
                    .map_err(|err| format!("failed to encode agent: {err}"))?,
            );
        }
        Ok(AdapterNotificationOutcome::Event(QmuxEvent::new(
            event_type,
            pane_id,
            agent.map(|agent| agent.id),
            payload,
        )))
    }
}

impl AgentAdapter for PiAdapter {
    fn id(&self) -> &'static str {
        "pi"
    }

    fn display_name(&self) -> &'static str {
        "Pi"
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

    fn prepare_shell_passthrough(
        &self,
        request: &PrepareShellAgentLaunchRequest,
    ) -> Result<Option<PreparedShellAgentLaunch>, String> {
        match pi_shell_disposition(&request.args)? {
            PiShellDisposition::Supervised => Ok(None),
            PiShellDisposition::Passthrough => {
                let binary = self.ensure_binary()?;
                let cwd = PathBuf::from(&request.cwd);
                if !cwd.is_dir() {
                    return Err(format!(
                        "Pi working directory {} does not exist",
                        cwd.display()
                    ));
                }
                Ok(Some(PreparedShellAgentLaunch {
                    binary,
                    cwd: request.cwd.clone(),
                    args: request.args.clone(),
                    envs: Vec::new(),
                    supervised: false,
                }))
            }
        }
    }

    fn prepare_shell_launch(
        &self,
        state: &AppState,
        request: PrepareShellAgentLaunchRequest,
    ) -> Result<PreparedShellAgentLaunch, String> {
        self.prepare_shell_launch_inner(state, request)
    }

    fn shell_commands(&self) -> Vec<ShellCommandIntegration> {
        vec![ShellCommandIntegration {
            command_name: "pi",
            adapter_id: self.id(),
        }]
    }

    fn shell_resume_command(&self, session_id: &str) -> Option<String> {
        Some(format!("pi --session {}", shell_quote_arg(session_id)))
    }

    fn ingest_notification(
        &self,
        state: &AppState,
        notification: AdapterNotification,
    ) -> Result<AdapterNotificationOutcome, String> {
        self.ingest_pi_notification(state, notification)
    }

    fn parse_transcript_line(
        &self,
        _agent_id: &str,
        _source_index: usize,
        _line: &str,
    ) -> Option<Turn> {
        // Phase 4 resolves Pi's tree-shaped session as a whole.
        None
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

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PiLaunchOptions {}

impl PiLaunchOptions {
    fn from_value(value: Value) -> Result<Self, String> {
        if value.is_null() {
            return Ok(Self::default());
        }
        serde_json::from_value(value).map_err(|err| format!("invalid Pi adapter options: {err}"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PiShellDisposition {
    Supervised,
    Passthrough,
}

fn pi_shell_disposition(args: &[String]) -> Result<PiShellDisposition, String> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return supervised_pi_shell_disposition(args);
        }
        if matches!(
            arg.as_str(),
            "--help" | "-h" | "--version" | "-v" | "--list-models"
        ) || arg.starts_with("--list-models=")
            || arg == "--export"
            || arg.starts_with("--export=")
        {
            return Ok(PiShellDisposition::Passthrough);
        }
        if pi_value_flag(arg) {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        return if pi_management_command(arg) {
            Ok(PiShellDisposition::Passthrough)
        } else {
            supervised_pi_shell_disposition(args)
        };
    }
    supervised_pi_shell_disposition(args)
}

fn supervised_pi_shell_disposition(args: &[String]) -> Result<PiShellDisposition, String> {
    validate_pi_supervised_args(args)?;
    Ok(PiShellDisposition::Supervised)
}

fn validate_pi_supervised_args(args: &[String]) -> Result<(), String> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            break;
        }
        if matches!(arg.as_str(), "--no-session" | "--print" | "-p") {
            return Err(format!(
                "qmux Pi integration does not support {arg} because native session tracking requires an interactive persisted session"
            ));
        }
        if let Some(mode) = arg.strip_prefix("--mode=") {
            if mode != "text" {
                return Err(format!(
                    "qmux Pi integration does not support --mode {mode}; use Pi's native text TUI"
                ));
            }
        } else if arg == "--mode" {
            if let Some(mode) = args.get(index + 1)
                && mode != "text"
            {
                return Err(format!(
                    "qmux Pi integration does not support --mode {mode}; use Pi's native text TUI"
                ));
            }
            index += 1;
        }
        index += 1;
    }
    Ok(())
}

fn pi_management_command(arg: &str) -> bool {
    matches!(
        arg,
        "install" | "remove" | "uninstall" | "update" | "list" | "config"
    )
}

fn pi_value_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--provider"
            | "--model"
            | "--api-key"
            | "--system-prompt"
            | "--append-system-prompt"
            | "--mode"
            | "--session"
            | "--session-id"
            | "--fork"
            | "--session-dir"
            | "--name"
            | "-n"
            | "--models"
            | "--tools"
            | "-t"
            | "--exclude-tools"
            | "-xt"
            | "--thinking"
            | "--extension"
            | "-e"
            | "--skill"
            | "--prompt-template"
            | "--theme"
            | "--export"
    )
}

fn pi_args_contain_prompt(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return args
                .get(index + 1)
                .is_some_and(|value| !value.trim().is_empty());
        }
        if pi_value_flag(arg) {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        return !pi_management_command(arg) && !arg.trim().is_empty();
    }
    false
}

fn pi_resume_session_id(args: &[String]) -> Option<String> {
    cli_flag_value(args, "--session").or_else(|| cli_flag_value(args, "--session-id"))
}

fn pi_shell_thinking(args: &[String]) -> Option<String> {
    cli_flag_value(args, "--thinking")
}

fn apply_pi_shell_thinking(
    state: &AppState,
    agent: AgentInfo,
    args: &[String],
) -> Result<AgentInfo, String> {
    let Some(effort) = pi_shell_thinking(args) else {
        return Ok(agent);
    };
    if agent.effort.as_deref() == Some(effort.as_str()) {
        return Ok(agent);
    }
    state
        .mutate_agent(&agent.id, |agent| agent.effort = Some(effort))?
        .ok_or_else(|| {
            format!(
                "agent {} disappeared while recording Pi thinking level",
                agent.id
            )
        })
}

fn attach_pi_agent_pane(
    state: &AppState,
    agent_id: &str,
    pane_id: String,
    has_initial_prompt: bool,
) -> Result<AgentInfo, String> {
    attach_agent_pane(state, agent_id, pane_id)?;
    state
        .mutate_agent(agent_id, |agent| {
            agent.status = if has_initial_prompt {
                AgentStatus::Running
            } else {
                AgentStatus::Starting
            };
        })?
        .ok_or_else(|| format!("agent {agent_id} disappeared while attaching its Pi pane"))
}

fn finish_agent_after_settled(state: &AppState, agent: &AgentInfo) -> Result<bool, String> {
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

fn pi_model_from_payload(payload: &Value) -> Option<String> {
    let model = string_field(payload, "model")?;
    match string_field(payload, "provider") {
        Some(provider) => Some(format!("{provider}/{model}")),
        None => Some(model),
    }
}

fn pi_version_is_compatible(version: &str) -> bool {
    let mut components = version.trim().trim_start_matches('v').split('.');
    let Some(major) = components.next().and_then(|part| part.parse::<u64>().ok()) else {
        return false;
    };
    let Some(minor) = components.next().and_then(|part| part.parse::<u64>().ok()) else {
        return false;
    };
    major > MIN_PI_MAJOR || (major == MIN_PI_MAJOR && minor >= MIN_PI_MINOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn compatibility_floor_is_pi_0_80() {
        assert!(!pi_version_is_compatible("0.79.9"));
        assert!(pi_version_is_compatible("0.80.0"));
        assert!(pi_version_is_compatible("0.80.6"));
        assert!(pi_version_is_compatible("1.0.0"));
        assert!(!pi_version_is_compatible("unknown"));
    }

    #[test]
    fn shell_utilities_pass_through_without_agent_supervision() {
        for command in [
            args(&["install", "npm:pkg"]),
            args(&["--offline", "update"]),
            args(&["config", "-l"]),
            args(&["--version"]),
            args(&["--list-models", "sonnet"]),
            args(&["--export", "session.jsonl"]),
        ] {
            assert_eq!(
                pi_shell_disposition(&command).unwrap(),
                PiShellDisposition::Passthrough,
                "{command:?}"
            );
        }
    }

    #[test]
    fn interactive_pi_invocations_are_supervised() {
        for command in [
            args(&[]),
            args(&["hello"]),
            args(&["--session", "abc"]),
            args(&["--offline", "--", "list"]),
            args(&["--extension", "custom.ts", "hello"]),
        ] {
            assert_eq!(
                pi_shell_disposition(&command).unwrap(),
                PiShellDisposition::Supervised,
                "{command:?}"
            );
        }
    }

    #[test]
    fn noninteractive_or_ephemeral_modes_are_rejected() {
        for command in [
            args(&["--no-session"]),
            args(&["-p", "hello"]),
            args(&["--mode", "json", "hello"]),
            args(&["--mode=rpc"]),
        ] {
            assert!(pi_shell_disposition(&command).is_err(), "{command:?}");
        }
        assert!(pi_shell_disposition(&args(&["--mode", "text"])).is_ok());
        assert_eq!(
            pi_shell_disposition(&args(&["--help", "--no-session"])).unwrap(),
            PiShellDisposition::Passthrough
        );
    }

    #[test]
    fn prompt_detection_skips_pi_flag_values() {
        assert!(!pi_args_contain_prompt(&args(&["--session", "abc"])));
        assert!(pi_args_contain_prompt(&args(&[
            "--session",
            "abc",
            "continue"
        ])));
        assert!(pi_args_contain_prompt(&args(&[
            "--",
            "--looks-like-a-flag"
        ])));
    }
}
