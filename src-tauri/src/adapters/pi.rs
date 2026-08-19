use super::{
    AdapterNotification, AdapterNotificationOutcome, AgentAdapter, ComposerPolicy,
    FORK_AT_MESSAGE_EMPTY_ERROR, LaunchEnv, MessageAnchor, PrepareShellAgentLaunchRequest,
    PreparedShellAgentLaunch, ShellCommandIntegration, SpawnAgentRequest, apply_shell_cli_model,
    cli_flag_value, ensure_on_path, hook_transcript_path_acceptable, prepared_shell_agent,
    record_shell_session_lineage, reusable_session_agent, shell_cli_model, shell_quote_arg,
};
use crate::config::QmuxConfig;
use crate::events::QmuxEvent;
use crate::pty::{
    CommandPlan, InitialPaneSize, PaneMeta, agent_pane_envs, plan_to_spec, recoverable_dir,
    spawn_pty,
};
use crate::state::{AppState, PaneInfo, PaneKind};
use crate::transcript::{
    Turn, TurnBlock, refresh_transcript_turns, start_transcript_tail, string_field,
};
use crate::turn_queue::{IdleResolution, advance_after_idle, is_shell_escape_turn};
use crate::workspace::{
    AgentInfo, AgentStatus, PrepareAgentWorkspaceRequest, attach_agent_pane, mark_agent_failed,
    mark_agent_spawn_failed, prepare_agent_workspace, prepare_agent_workspace_with_parent,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

const MIN_PI_MAJOR: u64 = 0;
const MIN_PI_MINOR: u64 = 80;
const MIN_PI_PATCH: u64 = 5;

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
        let mut command = Command::new(&binary);
        command.arg("--version");
        crate::launch_path::apply_launch_path(&mut command);
        let output = command
            .output()
            .map_err(|err| format!("failed to read Pi version from '{binary}': {err}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(if stderr.is_empty() {
                format!(
                    "failed to read Pi version from '{binary}' ({})",
                    output.status
                )
            } else {
                format!(
                    "failed to read Pi version from '{binary}' ({}): {stderr}",
                    output.status
                )
            });
        }
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !pi_version_is_compatible(&version) {
            return Err(format!(
                "qmux requires Pi {MIN_PI_MAJOR}.{MIN_PI_MINOR}.{MIN_PI_PATCH} or newer; '{binary}' reported {version:?}"
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

    fn session_helper_entrypoint(&self) -> Result<PathBuf, String> {
        let entrypoint = self.extension_dir.join("session-helper.js");
        if !entrypoint.is_file() {
            return Err(format!(
                "Pi SessionManager helper was not found at {}. Reinstall qmux or set QMUX_PI_EXTENSION_DIR to the bundled qmux-pi-extension directory.",
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

    fn create_branched_session(
        &self,
        source_path: &Path,
        leaf_id: &str,
        target_cwd: &Path,
    ) -> Result<PiBranchedSession, String> {
        if leaf_id.trim().is_empty() {
            return Err("this Pi session has no entry to fork yet; send a turn first".to_string());
        }
        if !source_path.is_file() {
            return Err(format!(
                "Pi source session {} was not found",
                source_path.display()
            ));
        }
        if !target_cwd.is_dir() {
            return Err(format!(
                "Pi fork working directory {} does not exist",
                target_cwd.display()
            ));
        }
        let pi_binary = self.ensure_compatible_binary()?;
        let node = ensure_on_path("node").ok_or_else(|| {
            "Node.js was not found; Pi SessionManager forks require Node.js".to_string()
        })?;
        let mut command = Command::new(node);
        command
            .arg(self.session_helper_entrypoint()?)
            .arg(pi_binary)
            .arg(source_path)
            .arg(leaf_id)
            .arg(target_cwd);
        crate::launch_path::apply_launch_path(&mut command);
        let output = command
            .output()
            .map_err(|err| format!("failed to run Pi SessionManager fork helper: {err}"))?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(if detail.is_empty() {
                format!("Pi SessionManager fork helper exited {}", output.status)
            } else {
                format!("Pi SessionManager fork failed: {detail}")
            });
        }
        let branched: PiBranchedSession = serde_json::from_slice(&output.stdout)
            .map_err(|err| format!("Pi SessionManager fork returned invalid output: {err}"))?;
        if !hook_transcript_path_acceptable(None, &branched.session_file) {
            return Err(format!(
                "Pi SessionManager returned an invalid session path: {}",
                branched.session_file
            ));
        }
        Ok(branched)
    }

    fn fork_pane_inner(
        &self,
        state: &AppState,
        source: &AgentInfo,
        use_worktree: bool,
        prompt: Option<&str>,
    ) -> Result<(PaneInfo, AgentInfo), String> {
        let binary = self.ensure_compatible_binary()?;
        let mut args = self.integration_args()?;
        let source_path = source
            .transcript_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                "this Pi session has no transcript yet; send a turn first".to_string()
            })?;
        let leaf_id = source
            .native_leaf_id
            .as_deref()
            .map(str::trim)
            .filter(|leaf| !leaf.is_empty())
            .ok_or_else(|| {
                "this Pi session has no active leaf yet; send a turn first".to_string()
            })?;
        let mut agent = prepare_agent_workspace_with_parent(
            state,
            PrepareAgentWorkspaceRequest {
                group_id: Some(source.group_id.clone()),
                base_repo: if use_worktree {
                    None
                } else {
                    Some(source.worktree_dir.clone())
                },
                base_ref: Some("HEAD".to_string()),
                adapter: self.id().to_string(),
                model: source.model.clone(),
                effort: source.effort.clone(),
                use_worktree,
            },
            Some(&source.id),
        )?;
        let cwd = PathBuf::from(&agent.worktree_dir);
        let branched = match self.create_branched_session(Path::new(source_path), leaf_id, &cwd) {
            Ok(branched) => branched,
            Err(err) => {
                let _ = mark_agent_failed(state, &agent.id);
                return Err(err);
            }
        };
        agent.session_id = Some(branched.session_id.clone());
        agent.transcript_path = Some(branched.session_file.clone());
        agent.native_leaf_id = branched.leaf_id.clone();
        agent.fork_point = source.session_id.clone();
        agent.root_session_id = source
            .root_session_id
            .clone()
            .or_else(|| source.session_id.clone());
        agent.status = AgentStatus::Idle;
        state.update_agent(agent.clone())?;

        args.push("--session".to_string());
        args.push(branched.session_file);
        append_pi_initial_prompt(&mut args, prompt);
        let pane_id = state.next_id("pane");
        let mut envs = agent_pane_envs(state, &pane_id, &agent.id)?;
        envs.push(("QMUX_ADAPTER_ID".to_string(), self.id().to_string()));
        let agent = attach_pi_agent_pane(
            state,
            &agent.id,
            pane_id.clone(),
            prompt.is_some_and(|prompt| !prompt.trim().is_empty()),
        )?;
        let spawn_result = plan_to_spec(
            state,
            PaneMeta {
                pane_id: Some(pane_id.clone()),
                agent_id: Some(agent.id.clone()),
                group_id: agent.group_id.clone(),
                kind: PaneKind::Agent,
                title: self.display_name().to_string(),
                last_osc_title: None,
                initial_size: None,
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
            Ok(pane) => Ok((pane, agent)),
            Err(err) => {
                let _ = mark_agent_spawn_failed(state, &agent.id, &pane_id);
                Err(err)
            }
        }
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
        append_pi_initial_prompt(&mut args, Some(prompt));

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

        if let Some(current) = agent.as_ref() {
            let session_id = string_field(&notification.payload, "session_id");
            let transcript_path =
                string_field(&notification.payload, "session_file").filter(|candidate| {
                    hook_transcript_path_acceptable(current.transcript_path.as_deref(), candidate)
                });
            let leaf_id = pi_leaf_marker(&notification.payload);
            let model = pi_model_from_payload(&notification.payload);
            let effort = string_field(&notification.payload, "thinking_level");

            let mut needs_refresh = false;
            let updated = state.mutate_agent(&current.id, |agent| {
                if let Some(session_id) = session_id {
                    agent.session_id = Some(session_id);
                }
                if let Some(transcript_path) = transcript_path {
                    if agent.transcript_path.as_deref() != Some(transcript_path.as_str()) {
                        agent.transcript_path = Some(transcript_path);
                        needs_refresh = true;
                    }
                }
                if let Some(leaf_id) = leaf_id {
                    if agent.native_leaf_id.as_deref() != Some(leaf_id.as_str()) {
                        agent.native_leaf_id = Some(leaf_id);
                        needs_refresh = true;
                    }
                }
                if model.is_some() {
                    agent.model = model;
                }
                if effort.is_some() {
                    agent.effort = effort;
                }
                if hook_event == "PiSessionStart" && agent.status == AgentStatus::Starting {
                    agent.status = AgentStatus::Idle;
                }
            })?;

            if needs_refresh {
                if let Some(path) = updated.as_ref().and_then(|a| a.transcript_path.clone()) {
                    let _ = refresh_transcript_turns(state, &current.id, &path, self.id());
                }
            }
        }

        let event_type = match hook_event.as_str() {
            "PiExtensionReady" => "agent.integration_ready",
            "PiSessionStart" => {
                if let Some(current) = agent.as_ref() {
                    if let Some(path) = state
                        .agent(&current.id)
                        .ok()
                        .flatten()
                        .and_then(|a| a.transcript_path)
                    {
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
            "PiModelSelect" | "PiThinkingLevelSelect" => "agent.updated",
            "PiSessionTree" | "PiSessionCompact" | "PiSessionInfoChanged" => "agent.updated",
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

    fn resolve_transcript_turns_at_leaf(
        &self,
        agent_id: &str,
        source_index_offset: usize,
        lines: &[String],
        native_leaf_id: Option<&str>,
    ) -> Vec<Turn> {
        resolve_pi_transcript_turns(agent_id, source_index_offset, lines, native_leaf_id)
    }

    fn transcript_line_can_update_turn_status(&self, line: &str) -> bool {
        serde_json::from_str::<Value>(line).is_ok_and(|value| {
            value.get("id").and_then(Value::as_str).is_some() && value.get("parentId").is_some()
        })
    }

    fn transcript_line_model(&self, line: &str) -> Option<String> {
        let value = serde_json::from_str::<Value>(line).ok()?;
        pi_model_from_transcript_value(&value)
    }

    fn synthesize_truncated_session(
        &self,
        transcript_path: &Path,
        anchor: &MessageAnchor,
        target_cwd: &Path,
    ) -> Result<String, String> {
        let leaf_id = anchor
            .parent_native_id
            .as_deref()
            .map(str::trim)
            .filter(|leaf_id| !leaf_id.is_empty())
            .ok_or_else(|| FORK_AT_MESSAGE_EMPTY_ERROR.to_string())?;
        self.create_branched_session(transcript_path, leaf_id, target_cwd)
            .map(|session| session.session_file)
    }

    fn supports_fork(&self) -> bool {
        true
    }

    fn supports_fork_at_message(&self) -> bool {
        true
    }

    fn shell_fork_args(
        &self,
        source: &AgentInfo,
        cwd: &Path,
        prompt: Option<&str>,
    ) -> Result<Vec<String>, String> {
        let transcript_path = source
            .transcript_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                "this Pi session has no transcript yet; send a turn first".to_string()
            })?;
        let leaf_id = source
            .native_leaf_id
            .as_deref()
            .map(str::trim)
            .filter(|leaf_id| !leaf_id.is_empty())
            .ok_or_else(|| {
                "this Pi session has no active leaf yet; send a turn first".to_string()
            })?;
        let session = self.create_branched_session(Path::new(transcript_path), leaf_id, cwd)?;
        Ok(pi_resume_args(&session.session_file, prompt))
    }

    fn shell_fork_at_message_args(
        &self,
        _source: &AgentInfo,
        seed_session_id: &str,
        prompt: Option<&str>,
    ) -> Result<Vec<String>, String> {
        Ok(pi_resume_args(seed_session_id, prompt))
    }

    fn fork_pane(
        &self,
        state: &AppState,
        source: &AgentInfo,
        use_worktree: bool,
        prompt: Option<&str>,
    ) -> Result<(PaneInfo, AgentInfo), String> {
        self.fork_pane_inner(state, source, use_worktree, prompt)
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

#[derive(Clone, Debug, Deserialize)]
struct PiBranchedSession {
    session_file: String,
    session_id: String,
    leaf_id: Option<String>,
}

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
        "install" | "remove" | "uninstall" | "update" | "list" | "config" | "auth"
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
            | "--tui-mode"
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

fn pi_resume_args(session_file: &str, prompt: Option<&str>) -> Vec<String> {
    let mut args = vec!["--session".to_string(), session_file.to_string()];
    append_pi_initial_prompt(&mut args, prompt);
    args
}

/// Pi's CLI parser does not implement a bare `--` option terminator, so initial
/// prompts have to be positional arguments. Protect its two reserved positional
/// prefixes with leading whitespace: `-` would otherwise be parsed as a flag and
/// `@` as a file inclusion. Prompt correlation normalizes whitespace, and the
/// padding is semantically inert while keeping arbitrary user text out of Pi's
/// option/file parser.
fn append_pi_initial_prompt(args: &mut Vec<String>, prompt: Option<&str>) {
    if let Some(prompt) = prompt.map(str::trim).filter(|prompt| !prompt.is_empty()) {
        args.push(if prompt.starts_with(['-', '@']) {
            format!(" {prompt}")
        } else {
            prompt.to_string()
        });
    }
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

/// `Some("")` represents Pi's explicit null leaf (the tree root before any
/// entry). `None` means an older/unknown notification omitted leaf state.
fn pi_leaf_marker(payload: &Value) -> Option<String> {
    match payload.get("leaf_id")? {
        Value::Null => Some(String::new()),
        Value::String(value) => Some(value.clone()),
        _ => None,
    }
}

#[derive(Clone, Debug)]
struct PiTranscriptEntry {
    id: String,
    parent_id: Option<String>,
    source_index: usize,
    value: Value,
}

fn resolve_pi_transcript_turns(
    agent_id: &str,
    source_index_offset: usize,
    lines: &[String],
    native_leaf_id: Option<&str>,
) -> Vec<Turn> {
    let mut session_id = None;
    let mut entries = Vec::new();
    for (relative_index, line) in lines.iter().enumerate() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("session") {
            session_id = string_field(&value, "id");
            continue;
        }
        let Some(id) = string_field(&value, "id") else {
            continue;
        };
        entries.push(PiTranscriptEntry {
            id,
            parent_id: string_field(&value, "parentId"),
            source_index: source_index_offset + relative_index,
            value,
        });
    }

    let selected = pi_context_entries(&entries, native_leaf_id);
    selected
        .into_iter()
        .filter_map(|entry| pi_entry_turn(agent_id, session_id.as_deref(), entry))
        .collect()
}

/// Mirrors Pi 0.80's `SessionManager.buildContextEntries`: follow the selected
/// leaf to the root, then have the latest compaction replace summarized history
/// while retaining the explicitly kept suffix.
fn pi_context_entries<'a>(
    entries: &'a [PiTranscriptEntry],
    native_leaf_id: Option<&str>,
) -> Vec<&'a PiTranscriptEntry> {
    if native_leaf_id == Some("") {
        return Vec::new();
    }
    let by_id = entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect::<HashMap<_, _>>();
    let mut current = native_leaf_id
        .and_then(|leaf_id| by_id.get(leaf_id).copied())
        .or_else(|| entries.last());
    let mut reversed = Vec::new();
    let mut visited = HashSet::new();
    while let Some(entry) = current {
        if !visited.insert(entry.id.as_str()) {
            break;
        }
        reversed.push(entry);
        current = entry
            .parent_id
            .as_deref()
            .and_then(|parent_id| by_id.get(parent_id).copied());
    }
    reversed.reverse();

    let Some(compaction) = reversed
        .iter()
        .rev()
        .find(|entry| entry.value.get("type").and_then(Value::as_str) == Some("compaction"))
        .copied()
    else {
        return reversed;
    };
    let Some(compaction_index) = reversed.iter().position(|entry| entry.id == compaction.id) else {
        return reversed;
    };
    let first_kept_id = string_field(&compaction.value, "firstKeptEntryId");
    let mut selected = vec![compaction];
    let mut keeping = false;
    for entry in &reversed[..compaction_index] {
        if first_kept_id.as_deref() == Some(entry.id.as_str()) {
            keeping = true;
        }
        if keeping {
            selected.push(*entry);
        }
    }
    selected.extend_from_slice(&reversed[compaction_index + 1..]);
    selected
}

fn pi_entry_turn(
    agent_id: &str,
    session_id: Option<&str>,
    entry: &PiTranscriptEntry,
) -> Option<Turn> {
    let entry_type = entry
        .value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("");
    let (role, blocks) = match entry_type {
        "message" => pi_message_turn_parts(entry.value.get("message")?)?,
        "compaction" => (
            "system".to_string(),
            vec![TurnBlock::Text {
                text: format!(
                    "Conversation compacted\n\n{}",
                    string_field(&entry.value, "summary").unwrap_or_default()
                ),
            }],
        ),
        "branch_summary" => (
            "system".to_string(),
            vec![TurnBlock::Text {
                text: format!(
                    "Branch summary\n\n{}",
                    string_field(&entry.value, "summary").unwrap_or_default()
                ),
            }],
        ),
        "custom_message" => {
            if entry.value.get("display").and_then(Value::as_bool) == Some(false) {
                return None;
            }
            (
                "system".to_string(),
                vec![TurnBlock::Raw {
                    value: entry.value.clone(),
                }],
            )
        }
        // These are native session state, not visible conversation content.
        "model_change" | "thinking_level_change" | "session_info" | "label" | "custom" => {
            return None;
        }
        // Extensions may append future entry kinds. Keep them visible and
        // lossless without guessing at semantics.
        _ => (
            "system".to_string(),
            vec![TurnBlock::Raw {
                value: entry.value.clone(),
            }],
        ),
    };
    if blocks.is_empty() {
        return None;
    }
    Some(Turn {
        id: format!("{agent_id}-{}", entry.source_index),
        agent_id: agent_id.to_string(),
        session_id: session_id.map(str::to_string),
        role,
        blocks,
        source_index: entry.source_index,
        timestamp: super::native_timestamp_ms(&entry.value),
        status: None,
        status_reason: None,
        context_status: None,
        native_id: Some(entry.id.clone()),
        parent_native_id: entry.parent_id.clone(),
        native_message_id: entry
            .value
            .get("message")
            .and_then(|message| string_field(message, "id")),
    })
}

fn pi_message_turn_parts(message: &Value) -> Option<(String, Vec<TurnBlock>)> {
    let role = string_field(message, "role").unwrap_or_else(|| "system".to_string());
    match role.as_str() {
        "toolResult" => Some((
            "assistant".to_string(),
            vec![TurnBlock::ToolResult {
                tool_use_id: string_field(message, "toolCallId"),
                content: message.get("content").cloned().unwrap_or(Value::Null),
                is_error: message
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }],
        )),
        "bashExecution" => Some((
            "user".to_string(),
            vec![TurnBlock::Text {
                text: pi_bash_execution_text(message),
            }],
        )),
        "custom" => {
            if message.get("display").and_then(Value::as_bool) == Some(false) {
                None
            } else {
                Some((
                    "system".to_string(),
                    vec![TurnBlock::Raw {
                        value: message.clone(),
                    }],
                ))
            }
        }
        "branchSummary" | "compactionSummary" => Some((
            "system".to_string(),
            vec![TurnBlock::Text {
                text: string_field(message, "summary").unwrap_or_default(),
            }],
        )),
        "user" | "assistant" => Some((role, pi_content_blocks(message.get("content")))),
        // A future extension-defined message role may not use `content` at all.
        // Preserve the complete value instead of silently dropping it.
        _ => Some((
            role,
            vec![TurnBlock::Raw {
                value: message.clone(),
            }],
        )),
    }
}

fn pi_content_blocks(content: Option<&Value>) -> Vec<TurnBlock> {
    match content {
        Some(Value::String(text)) => vec![TurnBlock::Text { text: text.clone() }],
        Some(Value::Array(items)) => items.iter().filter_map(pi_content_block).collect(),
        Some(Value::Null) | None => Vec::new(),
        Some(value) => vec![TurnBlock::Raw {
            value: value.clone(),
        }],
    }
}

fn pi_content_block(value: &Value) -> Option<TurnBlock> {
    match value.get("type").and_then(Value::as_str) {
        Some("text") => string_field(value, "text").map(|text| TurnBlock::Text { text }),
        Some("toolCall") => Some(TurnBlock::ToolUse {
            id: string_field(value, "id"),
            name: string_field(value, "name").unwrap_or_else(|| "tool".to_string()),
            input: value.get("arguments").cloned().unwrap_or(Value::Null),
        }),
        // Thinking, images, and extension-defined blocks remain raw. The
        // frontend recognizes standard thinking shapes and renders their prose;
        // unfamiliar objects fall back to formatted JSON.
        _ => Some(TurnBlock::Raw {
            value: value.clone(),
        }),
    }
}

fn pi_bash_execution_text(message: &Value) -> String {
    let command = string_field(message, "command").unwrap_or_default();
    let output = string_field(message, "output").unwrap_or_default();
    let mut text = format!("Ran `{command}`");
    if !output.is_empty() {
        text.push_str("\n```\n");
        text.push_str(&output);
        if !output.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("```");
    }
    if message.get("cancelled").and_then(Value::as_bool) == Some(true) {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(exit_code) = message.get("exitCode").and_then(Value::as_i64)
        && exit_code != 0
    {
        text.push_str(&format!("\n\nCommand exited with code {exit_code}"));
    }
    text
}

fn pi_model_from_transcript_value(value: &Value) -> Option<String> {
    match value.get("type").and_then(Value::as_str) {
        Some("model_change") => {
            let provider = string_field(value, "provider")?;
            let model = string_field(value, "modelId")?;
            Some(format!("{provider}/{model}"))
        }
        Some("message") => {
            let message = value.get("message")?;
            if message.get("role").and_then(Value::as_str) != Some("assistant") {
                return None;
            }
            let model = string_field(message, "model")?;
            match string_field(message, "provider") {
                Some(provider) => Some(format!("{provider}/{model}")),
                None => Some(model),
            }
        }
        _ => None,
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
    let Some(patch) = components.next().and_then(|part| part.parse::<u64>().ok()) else {
        return false;
    };
    (major, minor, patch) >= (MIN_PI_MAJOR, MIN_PI_MINOR, MIN_PI_PATCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn compatibility_floor_is_pi_0_80_5() {
        assert!(!pi_version_is_compatible("0.79.9"));
        assert!(!pi_version_is_compatible("0.80.0"));
        assert!(!pi_version_is_compatible("0.80.3"));
        assert!(pi_version_is_compatible("0.80.5"));
        assert!(pi_version_is_compatible("0.80.6"));
        assert!(pi_version_is_compatible("1.0.0"));
        assert!(!pi_version_is_compatible("0.80"));
        assert!(!pi_version_is_compatible("unknown"));
    }

    #[test]
    fn generated_prompts_are_plain_positional_arguments() {
        assert_eq!(
            pi_resume_args("session.jsonl", Some("continue here")),
            args(&["--session", "session.jsonl", "continue here"])
        );

        let mut launch = args(&["--extension", "qmux-pi-extension/index.js"]);
        append_pi_initial_prompt(&mut launch, Some("start here"));
        assert_eq!(
            launch,
            args(&["--extension", "qmux-pi-extension/index.js", "start here"])
        );
        assert!(!launch.iter().any(|arg| arg == "--"));

        assert_eq!(
            pi_resume_args("session.jsonl", Some("--looks-like-a-flag")),
            args(&["--session", "session.jsonl", " --looks-like-a-flag"])
        );
        assert_eq!(
            pi_resume_args("session.jsonl", Some("@private-file")),
            args(&["--session", "session.jsonl", " @private-file"])
        );
    }

    #[test]
    fn shell_utilities_pass_through_without_agent_supervision() {
        for command in [
            args(&["install", "npm:pkg"]),
            args(&["--offline", "update"]),
            args(&["config", "-l"]),
            args(&["auth", "check", "--provider", "anthropic"]),
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
    fn tui_mode_value_is_not_mistaken_for_an_initial_prompt() {
        let command = args(&["--tui-mode", "fullscreen"]);
        assert_eq!(
            pi_shell_disposition(&command).unwrap(),
            PiShellDisposition::Supervised
        );
        assert!(!pi_args_contain_prompt(&command));
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

    #[test]
    fn transcript_follows_the_extension_reported_leaf() {
        let lines = vec![
            r#"{"type":"session","version":3,"id":"session-1","timestamp":"2026-01-01T00:00:00Z","cwd":"/work"}"#.to_string(),
            r#"{"type":"message","id":"user-1","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":[{"type":"text","text":"one"}]}}"#.to_string(),
            r#"{"type":"message","id":"answer-a","parentId":"user-1","timestamp":"2026-01-01T00:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"branch A"}]}}"#.to_string(),
            r#"{"type":"message","id":"answer-b","parentId":"user-1","timestamp":"2026-01-01T00:00:03Z","message":{"role":"assistant","content":[{"type":"text","text":"branch B"}]}}"#.to_string(),
        ];

        let turns = resolve_pi_transcript_turns("agent-1", 0, &lines, Some("answer-a"));
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].native_id.as_deref(), Some("user-1"));
        assert_eq!(turns[1].native_id.as_deref(), Some("answer-a"));
        assert_eq!(turns[1].session_id.as_deref(), Some("session-1"));

        assert!(resolve_pi_transcript_turns("agent-1", 0, &lines, Some("")).is_empty());
    }

    #[test]
    fn transcript_applies_pi_compaction_context_order() {
        let lines = vec![
            r#"{"type":"session","version":3,"id":"session-1"}"#.to_string(),
            r#"{"type":"message","id":"old","parentId":null,"message":{"role":"user","content":"old"}}"#.to_string(),
            r#"{"type":"message","id":"kept","parentId":"old","message":{"role":"user","content":"kept"}}"#.to_string(),
            r#"{"type":"compaction","id":"compact","parentId":"kept","summary":"summary","firstKeptEntryId":"kept","tokensBefore":10}"#.to_string(),
            r#"{"type":"message","id":"new","parentId":"compact","message":{"role":"assistant","content":[{"type":"text","text":"new"}]}}"#.to_string(),
        ];

        let turns = resolve_pi_transcript_turns("agent-1", 0, &lines, Some("new"));
        assert_eq!(
            turns
                .iter()
                .filter_map(|turn| turn.native_id.as_deref())
                .collect::<Vec<_>>(),
            vec!["compact", "kept", "new"]
        );
    }

    #[test]
    fn transcript_maps_tools_thinking_and_unknown_extension_content() {
        let lines = vec![
            r#"{"type":"session","version":3,"id":"session-1"}"#.to_string(),
            r#"{"type":"message","id":"assistant","parentId":null,"message":{"role":"assistant","provider":"p","model":"m","content":[{"type":"thinking","thinking":"hmm","signature":"opaque"},{"type":"toolCall","id":"call-1","name":"bash","arguments":{"command":"pwd"}}]}}"#.to_string(),
            r#"{"type":"message","id":"result","parentId":"assistant","message":{"role":"toolResult","toolCallId":"call-1","toolName":"bash","content":[{"type":"text","text":"/work"}],"isError":false}}"#.to_string(),
            r#"{"type":"message","id":"unknown-message","parentId":"result","message":{"role":"extensionFuture","payload":{"answer":41}}}"#.to_string(),
            r#"{"type":"extension_future","id":"future","parentId":"unknown-message","payload":{"answer":42}}"#.to_string(),
        ];

        let turns = resolve_pi_transcript_turns("agent-1", 0, &lines, Some("future"));
        assert!(matches!(turns[0].blocks[0], TurnBlock::Raw { .. }));
        assert!(matches!(turns[0].blocks[1], TurnBlock::ToolUse { .. }));
        assert!(matches!(turns[1].blocks[0], TurnBlock::ToolResult { .. }));
        assert_eq!(turns[2].role, "extensionFuture");
        assert!(matches!(turns[2].blocks[0], TurnBlock::Raw { .. }));
        assert_eq!(turns[3].role, "system");
        assert!(matches!(turns[3].blocks[0], TurnBlock::Raw { .. }));
        assert_eq!(
            pi_model_from_transcript_value(&serde_json::from_str(&lines[1]).unwrap()).as_deref(),
            Some("p/m")
        );
    }

    #[test]
    fn null_leaf_is_distinct_from_an_omitted_leaf() {
        assert_eq!(
            pi_leaf_marker(&json!({ "leaf_id": null })).as_deref(),
            Some("")
        );
        assert_eq!(
            pi_leaf_marker(&json!({ "leaf_id": "leaf-1" })).as_deref(),
            Some("leaf-1")
        );
        assert_eq!(pi_leaf_marker(&json!({})), None);
    }
}
