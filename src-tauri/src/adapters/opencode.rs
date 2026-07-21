use super::{
    AdapterNotification, AdapterNotificationOutcome, AgentAdapter, ComposerPolicy, LaunchEnv,
    PrepareShellAgentLaunchRequest, PreparedShellAgentLaunch, ShellCommandIntegration,
    SpawnAgentRequest, TranscriptLifecycleEvent, ensure_on_path, prepared_shell_agent,
    record_shell_session_lineage, reusable_session_agent, shell_quote_arg,
};
use crate::config::QmuxConfig;
use crate::events::QmuxEvent;
use crate::pty::{InitialPaneSize, PtySpawnSpec, qmux_pane_envs, recoverable_dir, spawn_pty};
use crate::state::{AppState, PaneInfo, PaneKind};
use crate::transcript::{Turn, TurnBlock, start_transcript_tail, string_field};
use crate::turn_queue::{IdleResolution, advance_after_idle, is_shell_escape_turn};
use crate::workspace::{
    AgentInfo, AgentStatus, PrepareAgentWorkspaceRequest, attach_agent_pane, mark_agent_failed,
    mark_agent_spawn_failed, prepare_agent_workspace,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct OpencodeAdapter {
    binary: String,
    plugin_dir: PathBuf,
}

impl OpencodeAdapter {
    pub fn new(config: &QmuxConfig) -> Self {
        Self {
            binary: config.opencode_binary(),
            plugin_dir: config.opencode_plugin_dir.clone(),
        }
    }

    fn ensure_binary(&self) -> Result<String, String> {
        let binary = ensure_on_path(&self.binary).ok_or_else(|| {
            format!(
                "OpenCode adapter binary '{}' was not found on PATH or standard macOS tool paths. Install OpenCode CLI or update adapters.opencode.binary in qmux.config.json.",
                self.binary
            )
        })?;
        Ok(binary.display().to_string())
    }

    /// The qmux-managed JSONL transcript path for an agent. The opencode plugin
    /// appends one JSON line per message part here; qmux tails it with the same
    /// transcript pipeline used for Claude and Codex.
    fn transcript_path_for(state: &AppState, agent_id: &str, session_id: &str) -> PathBuf {
        let session_id = if !session_id.is_empty()
            && session_id.len() <= 128
            && session_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            session_id
        } else {
            "pending"
        };
        state
            .config()
            .workspace_root
            .join(".qmux")
            .join("opencode")
            .join(agent_id)
            .join(format!("{session_id}.jsonl"))
    }

    /// `OPENCODE_CONFIG_DIR` pointing at the qmux-managed plugin. Without this
    /// entrypoint the process still opens, but every integration surface silently
    /// disappears, so fail clearly instead of presenting a permanently stale agent.
    fn config_dir_env(&self) -> Result<(String, String), String> {
        let entrypoint = self.plugin_dir.join("plugins").join("qmux-notify.js");
        if !self.plugin_dir.is_dir() || !entrypoint.is_file() {
            return Err(format!(
                "OpenCode integration plugin was not found at {}. Reinstall qmux or set QMUX_OPENCODE_PLUGIN_DIR to the bundled qmux-opencode-plugin directory.",
                entrypoint.display()
            ));
        }
        Ok((
            "OPENCODE_CONFIG_DIR".to_string(),
            self.plugin_dir.display().to_string(),
        ))
    }
}

impl AgentAdapter for OpencodeAdapter {
    fn id(&self) -> &'static str {
        "opencode"
    }

    fn display_name(&self) -> &'static str {
        "OpenCode"
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
        self.prepare_shell_launch(state, request)
    }

    fn shell_commands(&self) -> Vec<ShellCommandIntegration> {
        vec![ShellCommandIntegration {
            command_name: "opencode",
            adapter_id: self.id(),
        }]
    }

    fn shell_resume_command(&self, session_id: &str) -> Option<String> {
        Some(format!(
            "opencode --session {}",
            shell_quote_arg(session_id)
        ))
    }

    fn ingest_notification(
        &self,
        state: &AppState,
        notification: AdapterNotification,
    ) -> Result<AdapterNotificationOutcome, String> {
        self.ingest_opencode_notification(state, notification)
    }

    fn parse_transcript_line(
        &self,
        agent_id: &str,
        source_index: usize,
        line: &str,
    ) -> Option<Turn> {
        parse_transcript_line(agent_id, source_index, line)
    }

    fn parse_transcript_lifecycle_event(&self, line: &str) -> Option<TranscriptLifecycleEvent> {
        parse_transcript_lifecycle_event(line)
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

impl OpencodeAdapter {
    pub fn shell_fork_args(
        &self,
        source: &AgentInfo,
        cwd: &Path,
        prompt: Option<&str>,
    ) -> Result<Vec<String>, String> {
        let session_id = source
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|session_id| !session_id.is_empty())
            .ok_or_else(|| {
                "this OpenCode session isn't ready to fork yet (no session id); send a turn first"
                    .to_string()
            })?;
        Ok(build_opencode_fork_args(
            cwd,
            source.model.as_deref(),
            session_id,
            prompt,
        ))
    }

    fn spawn_pane(&self, state: &AppState, request: SpawnAgentRequest) -> Result<PaneInfo, String> {
        let binary = self.ensure_binary()?;
        let config_dir_env = self.config_dir_env()?;
        let _options = OpencodeLaunchOptions::from_value(request.options)?;

        let agent = prepare_agent_workspace(
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
        )?;
        let cwd = request
            .cwd
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&agent.worktree_dir));
        if !cwd.is_dir() {
            let _ = mark_agent_failed(state, &agent.id);
            return Err(format!(
                "OpenCode working directory {} does not exist",
                cwd.display()
            ));
        }

        let has_initial_prompt = prompt_has_initial_text(&request.prompt);
        let args = build_opencode_args(&cwd, request.model.as_deref(), &request.prompt);

        let pane_id = state.next_id("pane");
        let mut envs = qmux_pane_envs(state, &pane_id)?;
        envs.push(("QMUX_AGENT_ID".to_string(), agent.id.clone()));
        envs.push(config_dir_env);

        // The plugin can emit session.created immediately after exec. Reserve the
        // binding first so its authenticated hook passes pane/agent scope checks.
        attach_opencode_agent_pane(state, &agent.id, pane_id.clone(), has_initial_prompt)?;
        let spawn_result = spawn_pty(
            state,
            PtySpawnSpec {
                pane_id: Some(pane_id.clone()),
                agent_id: Some(agent.id.clone()),
                group_id: agent.group_id.clone(),
                kind: PaneKind::Agent,
                title: self.display_name().to_string(),
                last_osc_title: None,
                program: binary,
                args,
                cwd,
                envs,
                initial_size: request.initial_size,
                recovered: false,
            },
        );

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
        let config_dir_env = self.config_dir_env()?;
        let cwd = recoverable_dir(&agent.worktree_dir).ok_or_else(|| {
            format!(
                "agent worktree {} no longer exists; relaunch manually",
                agent.worktree_dir
            )
        })?;

        let (args, resumed) =
            build_opencode_resume_args(&cwd, agent.model.as_deref(), agent.session_id.as_deref());

        let mut envs = qmux_pane_envs(state, &pane.id)?;
        envs.push(("QMUX_AGENT_ID".to_string(), agent.id.clone()));
        if let Some(session_id) = agent
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|session_id| !session_id.is_empty())
        {
            envs.push(("QMUX_ROOT_SESSION_ID".to_string(), session_id.to_string()));
        }
        envs.push(config_dir_env);

        let info = spawn_pty(
            state,
            PtySpawnSpec {
                pane_id: Some(pane.id.clone()),
                agent_id: Some(agent.id.clone()),
                group_id: agent.group_id.clone(),
                kind: PaneKind::Agent,
                title: pane.title.clone(),
                last_osc_title: pane.last_osc_title.clone(),
                program: binary,
                args,
                cwd,
                envs,
                initial_size: Some(InitialPaneSize {
                    cols: pane.cols,
                    rows: pane.rows,
                }),
                recovered: true,
            },
        )?;

        // A recovered opencode process is launched without an inline prompt,
        // even when resuming a session, so it is ready once the TUI appears. The
        // first real prompt/tool hook will promote it to Running.
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

    pub fn fork_pane(
        &self,
        state: &AppState,
        source: &AgentInfo,
        use_worktree: bool,
        prompt: Option<&str>,
    ) -> Result<(PaneInfo, AgentInfo), String> {
        let binary = self.ensure_binary()?;
        let config_dir_env = self.config_dir_env()?;
        let session_id = source
            .session_id
            .clone()
            .map(|session| session.trim().to_string())
            .filter(|session| !session.is_empty())
            .ok_or_else(|| {
                "this OpenCode session isn't ready to fork yet (no session id); send a turn first"
                    .to_string()
            })?;

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
                adapter: self.id().to_string(),
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

        let cwd = recoverable_dir(&agent.worktree_dir).ok_or_else(|| {
            format!(
                "fork working directory {} does not exist",
                agent.worktree_dir
            )
        })?;
        let prompt = prompt.map(str::trim).unwrap_or_default();
        let has_initial_prompt = !prompt.is_empty();
        let args = build_opencode_fork_args(
            &cwd,
            agent.model.as_deref(),
            &session_id,
            has_initial_prompt.then_some(prompt),
        );

        let pane_id = state.next_id("pane");
        let mut envs = qmux_pane_envs(state, &pane_id)?;
        envs.push(("QMUX_AGENT_ID".to_string(), agent.id.clone()));
        envs.push(("QMUX_FORK_POINT".to_string(), session_id.clone()));
        envs.push(config_dir_env);
        attach_opencode_agent_pane(state, &agent.id, pane_id.clone(), has_initial_prompt)?;

        let spawn_result = spawn_pty(
            state,
            PtySpawnSpec {
                pane_id: Some(pane_id.clone()),
                agent_id: Some(agent.id.clone()),
                group_id: agent.group_id.clone(),
                kind: PaneKind::Agent,
                title: self.display_name().to_string(),
                last_osc_title: None,
                program: binary,
                args,
                cwd,
                envs,
                initial_size: None,
                recovered: false,
            },
        );
        match spawn_result {
            Ok(pane) => {
                let forked = state
                    .agent(&agent.id)?
                    .ok_or_else(|| format!("forked agent {} disappeared during spawn", agent.id))?;
                Ok((pane, forked))
            }
            Err(err) => {
                let _ = mark_agent_spawn_failed(state, &agent.id, &pane_id);
                Err(err)
            }
        }
    }

    fn prepare_shell_launch(
        &self,
        state: &AppState,
        request: PrepareShellAgentLaunchRequest,
    ) -> Result<PreparedShellAgentLaunch, String> {
        let binary = self.ensure_binary()?;
        let config_dir_env = self.config_dir_env()?;
        validate_opencode_shell_args(&request.args)?;

        if !state.pane_exists(&request.pane_id)? {
            return Err(format!("pane {} was not found", request.pane_id));
        }

        let shell_cwd = PathBuf::from(&request.cwd);
        if !shell_cwd.is_dir() {
            return Err(format!(
                "OpenCode working directory {} does not exist",
                shell_cwd.display()
            ));
        }
        let agent_cwd = opencode_effective_project(&shell_cwd, &request.args)?;

        let cwd_str = agent_cwd.display().to_string();
        let pane_group_id = state
            .pane_group_id(&request.pane_id)?
            .ok_or_else(|| format!("pane {} was not found", request.pane_id))?;
        let resume_session_id = opencode_resume_session_id(&request.args).map(str::to_string);
        let fork_point = opencode_fork_source_session_id(&request.args).map(str::to_string);
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
                            model: None,
                            effort: None,
                            // Typing `opencode` in a shell runs in the current directory.
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
        let agent = attach_opencode_agent_pane(
            state,
            &agent.id,
            request.pane_id.clone(),
            args_contain_prompt(&request.args),
        )?;

        let args = build_opencode_args_from_shell(None, &request.args);
        let mut envs = qmux_pane_envs(state, &request.pane_id)?;
        envs.push(("QMUX_AGENT_ID".to_string(), agent.id.clone()));
        if let Some(session_id) = resume_session_id {
            envs.push(("QMUX_ROOT_SESSION_ID".to_string(), session_id));
        }
        if let Some(fork_point) = fork_point {
            envs.push(("QMUX_FORK_POINT".to_string(), fork_point));
        }
        envs.push(config_dir_env);
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
            args,
            envs: envs
                .into_iter()
                .map(|(key, value)| LaunchEnv { key, value })
                .collect(),
        })
    }

    fn ingest_opencode_notification(
        &self,
        state: &AppState,
        notification: AdapterNotification,
    ) -> Result<AdapterNotificationOutcome, String> {
        let pane_id = notification.pane_id.clone();
        let mut send_tracking = None;
        let mut agent = notification
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
            "SessionStart" => {
                if let Some(current) = agent.as_ref() {
                    let session_id = string_field(&notification.payload, "session_id")
                        .or_else(|| string_field(&notification.payload, "sessionId"))
                        .filter(|session_id| {
                            current.fork_point.as_deref() != Some(session_id.as_str())
                        });
                    // Field-scoped mutation, not a full-struct `update_agent`: this
                    // freshly spawned process's pane is being bound by attach_agent_pane
                    // on another thread, and a stale-snapshot write here would race it —
                    // wiping either the pane_id it set or the session_id we set.
                    let transcript_path = session_id.as_deref().map(|session_id| {
                        Self::transcript_path_for(state, &current.id, session_id)
                            .display()
                            .to_string()
                    });
                    let updated = state.mutate_agent(&current.id, |agent| {
                        if let Some(session_id) = session_id.clone() {
                            agent.session_id = Some(session_id);
                        }
                        // Bind the qmux-managed transcript path. The opencode plugin
                        // writes JSONL here; qmux tails it with the same pipeline used
                        // for Claude and Codex.
                        if let Some(transcript_path) = transcript_path.clone() {
                            agent.transcript_path = Some(transcript_path);
                        }
                        // A session starting doesn't mean a turn is running. Keep
                        // status unchanged here; the first real prompt/tool hook
                        // promotes the agent to Running.
                    })?;

                    // Start tailing the qmux-managed transcript file. The plugin may
                    // not have written anything yet, so the tail waits for the file
                    // to appear rather than erroring.
                    if let Some(transcript_path) = updated.and_then(|agent| agent.transcript_path) {
                        start_transcript_tail(
                            state.clone(),
                            current.id.clone(),
                            transcript_path,
                            self.id().to_string(),
                        );
                    }
                }
                "agent.session_start"
            }
            "UserPromptSubmit" => {
                if let Some(agent) = agent.as_mut() {
                    let prompt = string_field(&notification.payload, "prompt")
                        .or_else(|| string_field(&notification.payload, "input"));
                    if !prompt.as_deref().is_some_and(is_shell_escape_turn) {
                        agent.status = AgentStatus::Running;
                        state.set_agent_status(&agent.id, agent.status)?;
                    }
                    send_tracking =
                        Some(state.match_agent_prompt_submit(&agent.id, prompt.as_deref())?);
                }
                "agent.prompt_submitted"
            }
            "PreToolUse" => {
                if let Some(agent) = agent.as_mut() {
                    agent.status = AgentStatus::Running;
                    state.set_agent_status(&agent.id, agent.status)?;
                }
                "agent.tool_use"
            }
            "PostToolUse" => {
                if let Some(agent) = agent.as_mut() {
                    agent.status = AgentStatus::Running;
                    state.set_agent_status(&agent.id, agent.status)?;
                }
                "agent.tool_result"
            }
            "PermissionRequest" => {
                if let Some(agent) = agent.as_mut() {
                    agent.status = AgentStatus::AwaitingPermission;
                    state.set_agent_status(&agent.id, agent.status)?;
                }
                "agent.awaiting_permission"
            }
            "PermissionResolved" => {
                if let Some(agent) = agent.as_mut() {
                    agent.status = AgentStatus::Running;
                    state.set_agent_status(&agent.id, agent.status)?;
                }
                "agent.running"
            }
            "InputRequest" => {
                if let Some(agent) = agent.as_mut() {
                    agent.status = AgentStatus::AwaitingInput;
                    state.set_agent_status(&agent.id, agent.status)?;
                }
                "agent.awaiting_input"
            }
            "InputResolved" => {
                if let Some(agent) = agent.as_mut() {
                    agent.status = AgentStatus::Running;
                    state.set_agent_status(&agent.id, agent.status)?;
                }
                "agent.running"
            }
            "Stop" | "StopFailure" => {
                let drained = if let Some(agent) = agent.as_mut() {
                    finish_agent_after_stop(state, agent)?
                } else {
                    false
                };
                if drained {
                    "agent.running"
                } else {
                    "agent.done"
                }
            }
            other => {
                return Ok(AdapterNotificationOutcome::Event(QmuxEvent::new(
                    format!("agent.hook.{other}"),
                    pane_id,
                    agent.map(|agent| agent.id),
                    json!({
                        "hookEvent": hook_event,
                        "payload": notification.payload,
                    }),
                )));
            }
        };
        let mut event_payload = json!({
            "hookEvent": hook_event,
            "payload": notification.payload,
        });
        if let Some(send_tracking) = send_tracking
            && let Value::Object(payload) = &mut event_payload
        {
            payload.insert(
                "sendTracking".to_string(),
                serde_json::to_value(send_tracking)
                    .map_err(|err| format!("failed to encode send tracking: {err}"))?,
            );
        }
        // The idle handler (advance_after_idle) writes status/paused straight to the
        // store without touching this local snapshot, so re-read the agent before
        // attaching it — otherwise the event ships a stale (e.g. not-yet-paused) copy
        // and the surgical upsert below hides the change from the UI.
        let agent = match agent {
            Some(agent) => state.agent(&agent.id)?.or(Some(agent)),
            None => None,
        };
        // Carry the updated agent so the frontend can apply this status change
        // surgically instead of refetching the entire agent list on every hook
        // event (which also avoids out-of-order refetches clobbering newer state).
        if let (Value::Object(payload), Some(agent)) = (&mut event_payload, agent.as_ref()) {
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
            event_payload,
        )))
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OpencodeLaunchOptions {}

impl OpencodeLaunchOptions {
    fn from_value(value: Value) -> Result<Self, String> {
        if value.is_null() {
            return Ok(Self::default());
        }
        serde_json::from_value(value)
            .map_err(|err| format!("invalid OpenCode adapter options: {err}"))
    }
}

fn build_opencode_args(cwd: &Path, model: Option<&str>, prompt: &str) -> Vec<String> {
    let mut args = Vec::new();

    if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }

    let prompt = prompt.trim();
    if !prompt.is_empty() {
        args.push("--prompt".to_string());
        args.push(prompt.to_string());
    }

    // The project directory is passed as a positional arg so opencode runs in
    // the agent's cwd even when the process cwd differs (e.g. worktree paths).
    args.push(cwd.display().to_string());

    args
}

fn build_opencode_args_from_shell(model: Option<&str>, tail_args: &[String]) -> Vec<String> {
    let mut args = Vec::new();

    if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }

    args.extend(tail_args.iter().cloned());
    args
}

fn opencode_effective_project(shell_cwd: &Path, args: &[String]) -> Result<PathBuf, String> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            index += 1;
            break;
        }
        if opencode_value_flag(arg) {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        if opencode_subcommand(arg) {
            return fs_canonical_dir(shell_cwd);
        }
        return resolve_opencode_project(shell_cwd, arg);
    }
    if let Some(project) = args.get(index) {
        return resolve_opencode_project(shell_cwd, project);
    }
    fs_canonical_dir(shell_cwd)
}

fn validate_opencode_shell_args(args: &[String]) -> Result<(), String> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            break;
        }
        if arg == "--pure" || arg == "--pure=true" {
            return Err(
                "qMux OpenCode integration does not support --pure because it disables the required lifecycle plugin"
                    .to_string(),
            );
        }
        if opencode_value_flag(arg) {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        if arg == "attach" {
            return Err(
                "qMux OpenCode integration does not support attach because the existing server does not inherit qMux lifecycle and transcript configuration"
                    .to_string(),
            );
        }
        break;
    }
    Ok(())
}

fn resolve_opencode_project(shell_cwd: &Path, value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    let path = if path.is_absolute() {
        path
    } else {
        shell_cwd.join(path)
    };
    fs_canonical_dir(&path)
}

fn fs_canonical_dir(path: &Path) -> Result<PathBuf, String> {
    if !path.is_dir() {
        return Err(format!(
            "OpenCode working directory {} does not exist",
            path.display()
        ));
    }
    Ok(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}

fn opencode_subcommand(arg: &str) -> bool {
    matches!(
        arg,
        "completion"
            | "acp"
            | "mcp"
            | "attach"
            | "run"
            | "debug"
            | "providers"
            | "auth"
            | "agent"
            | "upgrade"
            | "uninstall"
            | "serve"
            | "web"
            | "models"
            | "stats"
            | "export"
            | "import"
            | "github"
            | "pr"
            | "session"
            | "plugin"
            | "plug"
            | "db"
    )
}

fn build_opencode_resume_args(
    cwd: &Path,
    model: Option<&str>,
    session_id: Option<&str>,
) -> (Vec<String>, bool) {
    let Some(session_id) = session_id
        .map(str::trim)
        .filter(|session_id| !session_id.is_empty())
    else {
        return (build_opencode_args(cwd, model, ""), false);
    };

    let mut args = Vec::new();
    if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    args.push("--session".to_string());
    args.push(session_id.to_string());
    args.push(cwd.display().to_string());

    (args, true)
}

fn build_opencode_fork_args(
    cwd: &Path,
    model: Option<&str>,
    session_id: &str,
    prompt: Option<&str>,
) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    args.push("--session".to_string());
    args.push(session_id.to_string());
    args.push("--fork".to_string());
    if let Some(prompt) = prompt.map(str::trim).filter(|prompt| !prompt.is_empty()) {
        args.push("--prompt".to_string());
        args.push(prompt.to_string());
    }
    args.push(cwd.display().to_string());
    args
}

fn prompt_has_initial_text(prompt: &str) -> bool {
    !prompt.trim().is_empty()
}

fn attach_opencode_agent_pane(
    state: &AppState,
    agent_id: &str,
    pane_id: String,
    has_initial_prompt: bool,
) -> Result<AgentInfo, String> {
    let agent = attach_agent_pane(state, agent_id, pane_id)?;
    if !has_initial_prompt {
        // Field-scoped write — a full-struct update here would race the SessionStart
        // hook recording session_id on another thread. Return the post-write state so
        // callers see the final Idle status.
        if let Some(updated) = state.set_agent_status(agent_id, AgentStatus::Idle)? {
            return Ok(updated);
        }
    }
    Ok(agent)
}

/// Whether a manual `opencode ...` invocation carries an inline prompt via
/// `--prompt <text>`. Erring toward "no prompt" is safe: the agent just starts
/// idle and the first real turn (UserPromptSubmit/PreToolUse) promotes it.
fn args_contain_prompt(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return false;
        }
        if arg == "--prompt" {
            return args
                .get(index + 1)
                .is_some_and(|value| !value.is_empty() && !value.starts_with('-'));
        }
        if arg.starts_with("--prompt=") {
            return arg.len() > "--prompt=".len();
        }
        if opencode_value_flag(arg) {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        // `run` carries its message positionally (or via stdin), while `pr` starts
        // agent work after fetching the requested branch. Other first positionals
        // are a TUI project path or administrative subcommand, not prompt text.
        return matches!(arg.as_str(), "run" | "pr");
    }
    false
}

fn opencode_resume_session_id(args: &[String]) -> Option<&str> {
    if args
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--fork")
    {
        return None;
    }
    opencode_session_argument_id(args)
}

fn opencode_fork_source_session_id(args: &[String]) -> Option<&str> {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--fork")
        .then(|| opencode_session_argument_id(args))
        .flatten()
}

fn opencode_session_argument_id(args: &[String]) -> Option<&str> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            break;
        }
        if arg == "--session" || arg == "-s" {
            return args
                .get(index + 1)
                .map(String::as_str)
                .filter(|value| !value.starts_with('-'));
        }
        if let Some(value) = arg.strip_prefix("--session=") {
            return (!value.is_empty()).then_some(value);
        }
        if opencode_value_flag(arg) {
            index += 2;
        } else {
            index += 1;
        }
    }
    None
}

fn opencode_value_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--log-level"
            | "--port"
            | "--hostname"
            | "--mdns-domain"
            | "--cors"
            | "--model"
            | "-m"
            | "--session"
            | "-s"
            | "--prompt"
            | "--agent"
            | "--replay-limit"
    )
}

/// Resolves an idle opencode agent: drains the next queued turn, or enters/stays
/// paused. Returns whether a turn was drained. Status/paused are written by
/// `advance_after_idle`; the passed agent is only used for its id afterward.
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

/// Parses a line written by the qmux opencode plugin into a `Turn`.
///
/// The plugin writes one JSON line per message part, shaped as:
/// ```json
/// {"type":"response_item","payload":{"type":"message","role":"user","content":[...]},"session_id":"..."}
/// ```
///
/// This mirrors the Codex transcript shape so the same `TurnBlock` variants apply.
fn parse_transcript_line(agent_id: &str, source_index: usize, line: &str) -> Option<Turn> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    let payload = value.get("payload")?;
    let item_type = payload.get("type").and_then(Value::as_str)?;
    let session_id =
        string_field(&value, "session_id").or_else(|| string_field(&value, "sessionId"));

    let (role, blocks) = match item_type {
        "message" => {
            let role = payload.get("role").and_then(Value::as_str)?;
            if role == "developer" || role == "system" {
                return None;
            }
            let blocks = parse_opencode_message_blocks(payload.get("content"))?;
            (role.to_string(), blocks)
        }
        "tool_use" | "function_call" | "custom_tool_call" => {
            let name = string_field(payload, "name").unwrap_or_else(|| "tool".to_string());
            (
                "assistant".to_string(),
                vec![TurnBlock::ToolUse {
                    id: string_field(payload, "id")
                        .or_else(|| string_field(payload, "call_id"))
                        .or_else(|| string_field(payload, "callId")),
                    name,
                    input: payload.get("input").cloned().unwrap_or(Value::Null),
                }],
            )
        }
        "tool_result" | "function_call_output" | "custom_tool_call_output" => (
            "assistant".to_string(),
            vec![TurnBlock::ToolResult {
                tool_use_id: string_field(payload, "tool_use_id")
                    .or_else(|| string_field(payload, "toolUseId"))
                    .or_else(|| string_field(payload, "call_id"))
                    .or_else(|| string_field(payload, "callId")),
                content: payload.get("content").cloned().unwrap_or(Value::Null),
                is_error: payload
                    .get("is_error")
                    .or_else(|| payload.get("isError"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }],
        ),
        _ => return None,
    };

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
        timestamp: super::native_timestamp_ms(&value),
        status: None,
        status_reason: None,
        native_id: string_field(payload, "id"),
        parent_native_id: None,
        native_message_id: string_field(payload, "id"),
    })
}

fn parse_transcript_lifecycle_event(line: &str) -> Option<TranscriptLifecycleEvent> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    let payload = value.get("payload")?;
    (payload.get("type").and_then(Value::as_str) == Some("turn_aborted"))
        .then_some(TranscriptLifecycleEvent::Interrupted)
}

fn parse_opencode_message_blocks(content: Option<&Value>) -> Option<Vec<TurnBlock>> {
    match content? {
        Value::String(text) => Some(vec![TurnBlock::Text { text: text.clone() }]),
        Value::Array(items) => {
            let blocks =
                items
                    .iter()
                    .filter_map(|item| {
                        let block_type = item.get("type").and_then(Value::as_str);
                        match block_type {
                            Some("text") => item.get("text").and_then(Value::as_str).map(|text| {
                                TurnBlock::Text {
                                    text: text.to_string(),
                                }
                            }),
                            Some("tool_use") => Some(TurnBlock::ToolUse {
                                id: item
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .map(ToString::to_string),
                                name: item
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("tool")
                                    .to_string(),
                                input: item.get("input").cloned().unwrap_or(Value::Null),
                            }),
                            Some("tool_result") => Some(TurnBlock::ToolResult {
                                tool_use_id: item
                                    .get("tool_use_id")
                                    .and_then(Value::as_str)
                                    .map(ToString::to_string),
                                content: item.get("content").cloned().unwrap_or(Value::Null),
                                is_error: item
                                    .get("is_error")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false),
                            }),
                            _ => item.get("text").and_then(Value::as_str).map(|text| {
                                TurnBlock::Text {
                                    text: text.to_string(),
                                }
                            }),
                        }
                    })
                    .collect::<Vec<_>>();
            Some(blocks)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AdapterConfigs, ClaudeAdapterConfig, CodexAdapterConfig, GrokAdapterConfig,
        OpencodeAdapterConfig,
    };
    use crate::state::AppState;
    use std::fs;
    use std::path::PathBuf;

    fn test_config() -> QmuxConfig {
        QmuxConfig {
            workspace_root: PathBuf::from("/tmp/qmux-opencode-tests"),
            socket_path: PathBuf::from("/tmp/qmux-opencode-tests.sock"),
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

    fn test_state() -> AppState {
        AppState::new(test_config())
    }

    fn sample_agent() -> AgentInfo {
        AgentInfo {
            id: "agent-1".to_string(),
            group_id: "group-1".to_string(),
            adapter: "opencode".to_string(),
            worktree_dir: "/tmp/qmux-opencode-tests".to_string(),
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
        }
    }

    fn hook_for_agent(event: &str, agent_id: &str, payload: Value) -> AdapterNotification {
        AdapterNotification {
            adapter_id: None,
            event: event.to_string(),
            pane_id: None,
            agent_id: Some(agent_id.to_string()),
            payload,
        }
    }

    fn ingest(state: &AppState, notification: AdapterNotification) -> QmuxEvent {
        let outcome = OpencodeAdapter::new(state.config())
            .ingest_notification(state, notification)
            .unwrap();
        match outcome {
            AdapterNotificationOutcome::Event(event) => event,
        }
    }

    #[test]
    fn launch_options_reject_unknown_fields() {
        let err = OpencodeLaunchOptions::from_value(json!({ "bogus": true })).unwrap_err();
        assert!(err.contains("invalid OpenCode adapter options"));
    }

    #[test]
    fn config_dir_requires_the_qmux_plugin_entrypoint() {
        let missing = OpencodeAdapter {
            binary: "opencode".to_string(),
            plugin_dir: PathBuf::from("/definitely/missing/qmux-opencode-plugin"),
        };
        assert!(
            missing
                .config_dir_env()
                .unwrap_err()
                .contains("OpenCode integration plugin was not found")
        );

        let dir = std::env::temp_dir().join(format!(
            "qmux-opencode-plugin-entrypoint-{}",
            std::process::id()
        ));
        fs::create_dir_all(dir.join("plugins")).unwrap();
        fs::write(dir.join("plugins").join("qmux-notify.js"), "export {};\n").unwrap();
        let adapter = OpencodeAdapter {
            binary: "opencode".to_string(),
            plugin_dir: dir.clone(),
        };
        assert_eq!(
            adapter.config_dir_env().unwrap(),
            ("OPENCODE_CONFIG_DIR".to_string(), dir.display().to_string())
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn build_args_adds_cwd_model_and_prompt() {
        let args = build_opencode_args(
            Path::new("/tmp/qmux"),
            Some("anthropic/claude-sonnet-4-5"),
            "fix the bug",
        );

        assert_eq!(
            args,
            vec![
                "--model",
                "anthropic/claude-sonnet-4-5",
                "--prompt",
                "fix the bug",
                "/tmp/qmux"
            ]
        );
    }

    #[test]
    fn build_args_omit_empty_prompt_and_model() {
        let args = build_opencode_args(Path::new("/tmp/qmux"), None, "  ");

        assert_eq!(args, vec!["/tmp/qmux"]);
    }

    #[test]
    fn shell_project_override_drives_agent_workspace_without_rewriting_args() {
        let values = |items: &[&str]| items.iter().map(ToString::to_string).collect::<Vec<_>>();
        let root = std::env::temp_dir().join(format!(
            "qmux-opencode-shell-project-{}",
            std::process::id()
        ));
        let shell = root.join("shell");
        let project = root.join("project");
        fs::create_dir_all(&shell).unwrap();
        fs::create_dir_all(&project).unwrap();

        let project_args = values(&["--model", "provider/model", "../project"]);
        assert_eq!(
            opencode_effective_project(&shell, &project_args).unwrap(),
            fs::canonicalize(&project).unwrap()
        );
        assert_eq!(
            build_opencode_args_from_shell(None, &project_args),
            project_args
        );

        assert_eq!(
            opencode_effective_project(&shell, &values(&["models", "provider"])).unwrap(),
            fs::canonicalize(&shell).unwrap()
        );
        assert_eq!(
            opencode_effective_project(&shell, &values(&["--", "../project"])).unwrap(),
            fs::canonicalize(&project).unwrap()
        );
        assert_eq!(
            opencode_effective_project(
                &shell,
                &values(&["--session", "source-session", "--fork", "../project"]),
            )
            .unwrap(),
            fs::canonicalize(&project).unwrap()
        );
        assert!(opencode_effective_project(&shell, &values(&["missing"])).is_err());

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn shell_args_reject_modes_without_the_qmux_plugin() {
        let values = |items: &[&str]| items.iter().map(ToString::to_string).collect::<Vec<_>>();

        assert!(validate_opencode_shell_args(&values(&["--pure"])).is_err());
        assert!(validate_opencode_shell_args(&values(&["--pure=true"])).is_err());
        assert!(
            validate_opencode_shell_args(&values(&[
                "--log-level",
                "INFO",
                "attach",
                "http://localhost:4096"
            ]))
            .is_err()
        );
        assert!(validate_opencode_shell_args(&values(&["--", "attach"])).is_ok());
        assert!(validate_opencode_shell_args(&values(&["run", "attach"])).is_ok());
    }

    #[test]
    fn resume_args_include_session_id_when_present() {
        let (args, resumed) = build_opencode_resume_args(
            Path::new("/tmp/qmux"),
            Some("anthropic/claude-sonnet-4-5"),
            Some(" session-123 "),
        );

        assert!(resumed);
        assert_eq!(
            args,
            vec![
                "--model",
                "anthropic/claude-sonnet-4-5",
                "--session",
                "session-123",
                "/tmp/qmux"
            ]
        );
    }

    #[test]
    fn resume_args_fall_back_to_fresh_launch_without_session_id() {
        let (args, resumed) = build_opencode_resume_args(Path::new("/tmp/qmux"), None, Some("   "));

        assert!(!resumed);
        assert_eq!(args, vec!["/tmp/qmux"]);
    }

    #[test]
    fn fork_args_resume_into_new_session_and_append_prompt() {
        let args = build_opencode_fork_args(
            Path::new("/tmp/qmux"),
            Some("anthropic/claude-sonnet-4-5"),
            "source-session",
            Some(" continue here "),
        );

        assert_eq!(
            args,
            vec![
                "--model",
                "anthropic/claude-sonnet-4-5",
                "--session",
                "source-session",
                "--fork",
                "--prompt",
                "continue here",
                "/tmp/qmux",
            ]
        );
    }

    #[test]
    fn shell_resume_command_and_parser_reuse_specific_session() {
        let command = OpencodeAdapter::new(&test_config())
            .shell_resume_command("session-123")
            .expect("OpenCode supports shell resume");
        assert_eq!(command, "opencode --session 'session-123'");

        let args = |values: &[&str]| values.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(
            opencode_resume_session_id(&args(&["--session", "session-123"])),
            Some("session-123")
        );
        assert_eq!(
            opencode_resume_session_id(&args(&["-s", "session-123"])),
            Some("session-123")
        );
        assert_eq!(
            opencode_resume_session_id(&args(&["--session=session-123"])),
            Some("session-123")
        );
        assert_eq!(
            opencode_resume_session_id(&args(&[
                "--agent",
                "--session",
                "--",
                "--session",
                "prompt-value"
            ])),
            None
        );
    }

    #[test]
    fn forked_shell_session_does_not_reuse_source_agent() {
        let args = [
            "--session".to_string(),
            "session-123".to_string(),
            "--fork".to_string(),
        ];
        assert_eq!(opencode_resume_session_id(&args), None);
        assert_eq!(opencode_fork_source_session_id(&args), Some("session-123"));
    }

    #[test]
    fn args_contain_prompt_detects_prompt_flag() {
        assert!(!args_contain_prompt(&[]));
        assert!(!args_contain_prompt(&[
            "--model".to_string(),
            "anthropic/claude-sonnet-4-5".to_string()
        ]));
        assert!(!args_contain_prompt(&["/tmp/qmux".to_string()]));

        assert!(args_contain_prompt(&[
            "--prompt".to_string(),
            "fix the bug".to_string()
        ]));
        assert!(args_contain_prompt(&["--prompt=fix the bug".to_string()]));
        assert!(!args_contain_prompt(&[
            "--prompt".to_string(),
            "--model".to_string(),
            "provider/model".to_string()
        ]));
        assert!(args_contain_prompt(&[
            "run".to_string(),
            "fix".to_string(),
            "the bug".to_string()
        ]));
        assert!(args_contain_prompt(&["pr".to_string(), "123".to_string()]));
        assert!(!args_contain_prompt(&[
            "models".to_string(),
            "anthropic".to_string()
        ]));
    }

    #[test]
    fn composer_policy_queues_running_panes() {
        let policy = OpencodeAdapter {
            binary: "opencode".to_string(),
            plugin_dir: PathBuf::new(),
        }
        .composer_policy();

        assert!(!policy.can_send(AgentStatus::Running));
        assert!(policy.should_queue(AgentStatus::Running));
        assert!(policy.can_steer(AgentStatus::Running));
    }

    #[test]
    fn interactive_attach_marks_agent_idle() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Starting;
        agent.pane_id = None;
        state.insert_agent(agent).unwrap();

        let attached =
            attach_opencode_agent_pane(&state, "agent-1", "pane-1".to_string(), false).unwrap();

        assert!(matches!(attached.status, AgentStatus::Idle));
        let stored = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(matches!(stored.status, AgentStatus::Idle));
    }

    #[test]
    fn prompted_attach_keeps_agent_running() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Starting;
        agent.pane_id = None;
        state.insert_agent(agent).unwrap();

        let attached =
            attach_opencode_agent_pane(&state, "agent-1", "pane-1".to_string(), true).unwrap();

        assert!(matches!(attached.status, AgentStatus::Running));
    }

    #[test]
    fn session_start_captures_session_id_and_binds_transcript() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Starting;
        state.insert_agent(agent).unwrap();

        let event = ingest(
            &state,
            hook_for_agent(
                "SessionStart",
                "agent-1",
                json!({ "session_id": "opencode-session-1" }),
            ),
        );

        assert_eq!(event.event_type, "agent.session_start");
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(agent.session_id.as_deref(), Some("opencode-session-1"));
        // SessionStart does not promote to Running (matches Claude/Codex idle convention).
        assert!(matches!(agent.status, AgentStatus::Starting));
        // Transcript path is bound to the qmux-managed JSONL file.
        assert!(
            agent
                .transcript_path
                .as_deref()
                .unwrap()
                .ends_with("/.qmux/opencode/agent-1/opencode-session-1.jsonl")
        );
    }

    #[test]
    fn session_start_preserves_awaiting_input_status() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::AwaitingInput;
        state.insert_agent(agent).unwrap();

        let event = ingest(
            &state,
            hook_for_agent(
                "SessionStart",
                "agent-1",
                json!({ "session_id": "opencode-session-1" }),
            ),
        );

        assert_eq!(event.event_type, "agent.session_start");
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(agent.session_id.as_deref(), Some("opencode-session-1"));
        assert!(matches!(agent.status, AgentStatus::AwaitingInput));
    }

    #[test]
    fn forked_agent_rejects_source_session_identity() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.fork_point = Some("source-session".to_string());
        state.insert_agent(agent).unwrap();

        ingest(
            &state,
            hook_for_agent(
                "SessionStart",
                "agent-1",
                json!({ "sessionId": "source-session" }),
            ),
        );
        assert_eq!(state.agent("agent-1").unwrap().unwrap().session_id, None);

        ingest(
            &state,
            hook_for_agent(
                "SessionStart",
                "agent-1",
                json!({ "sessionId": "fork-session" }),
            ),
        );
        assert_eq!(
            state
                .agent("agent-1")
                .unwrap()
                .unwrap()
                .session_id
                .as_deref(),
            Some("fork-session")
        );
    }

    #[test]
    fn shell_escape_prompt_submit_preserves_ready_opencode_status() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Done;
        state.insert_agent(agent).unwrap();
        state
            .record_agent_send(
                "agent-1",
                "!git status".to_string(),
                crate::state::AgentSendSource::DirectSend,
            )
            .unwrap();

        let event = ingest(
            &state,
            hook_for_agent(
                "UserPromptSubmit",
                "agent-1",
                json!({ "prompt": "!git status" }),
            ),
        );

        assert_eq!(event.event_type, "agent.prompt_submitted");
        assert_eq!(event.payload["sendTracking"]["status"], "matched");
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(matches!(agent.status, AgentStatus::Done));
    }

    #[test]
    fn stop_marks_agent_done_without_queued_turns() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Running;
        state.insert_agent(agent).unwrap();

        let event = ingest(&state, hook_for_agent("Stop", "agent-1", json!({})));

        assert_eq!(event.event_type, "agent.done");
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(matches!(agent.status, AgentStatus::Done));
    }

    #[test]
    fn permission_request_marks_agent_awaiting_permission() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Running;
        state.insert_agent(agent).unwrap();

        let event = ingest(
            &state,
            hook_for_agent("PermissionRequest", "agent-1", json!({})),
        );

        assert_eq!(event.event_type, "agent.awaiting_permission");
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(matches!(agent.status, AgentStatus::AwaitingPermission));
    }

    #[test]
    fn permission_and_input_resolution_restore_running_status() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::AwaitingPermission;
        state.insert_agent(agent).unwrap();

        let event = ingest(
            &state,
            hook_for_agent("PermissionResolved", "agent-1", json!({})),
        );
        assert_eq!(event.event_type, "agent.running");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        ));

        let event = ingest(&state, hook_for_agent("InputRequest", "agent-1", json!({})));
        assert_eq!(event.event_type, "agent.awaiting_input");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::AwaitingInput
        ));

        let event = ingest(
            &state,
            hook_for_agent("InputResolved", "agent-1", json!({})),
        );
        assert_eq!(event.event_type, "agent.running");
    }

    #[test]
    fn stop_failure_settles_running_agent() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Running;
        state.insert_agent(agent).unwrap();

        let event = ingest(&state, hook_for_agent("StopFailure", "agent-1", json!({})));

        assert_eq!(event.event_type, "agent.done");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Done
        ));
    }

    #[test]
    fn parse_transcript_line_extracts_user_message() {
        let line = r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"text","text":"hello world"}]},"session_id":"sess-1"}"#;

        let turn = parse_transcript_line("agent-1", 0, line).unwrap();

        assert_eq!(turn.role, "user");
        assert_eq!(turn.session_id.as_deref(), Some("sess-1"));
        assert_eq!(turn.blocks.len(), 1);
        match &turn.blocks[0] {
            TurnBlock::Text { text } => assert_eq!(text, "hello world"),
            other => panic!("expected text block, got {other:?}"),
        }
    }

    #[test]
    fn parse_transcript_line_extracts_assistant_tool_use() {
        let line = r#"{"type":"response_item","payload":{"type":"tool_use","name":"bash","id":"call-1","input":{"command":"ls"}},"session_id":"sess-1"}"#;

        let turn = parse_transcript_line("agent-1", 1, line).unwrap();

        assert_eq!(turn.role, "assistant");
        assert_eq!(turn.blocks.len(), 1);
        match &turn.blocks[0] {
            TurnBlock::ToolUse { id, name, input } => {
                assert_eq!(id.as_deref(), Some("call-1"));
                assert_eq!(name, "bash");
                assert_eq!(input["command"], "ls");
            }
            other => panic!("expected tool use block, got {other:?}"),
        }
    }

    #[test]
    fn parse_transcript_line_extracts_tool_result() {
        let line = r#"{"type":"response_item","payload":{"type":"tool_result","tool_use_id":"call-1","content":"output","is_error":false},"session_id":"sess-1"}"#;

        let turn = parse_transcript_line("agent-1", 2, line).unwrap();

        assert_eq!(turn.role, "assistant");
        assert_eq!(turn.blocks.len(), 1);
        match &turn.blocks[0] {
            TurnBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id.as_deref(), Some("call-1"));
                assert_eq!(content, "output");
                assert!(!is_error);
            }
            other => panic!("expected tool result block, got {other:?}"),
        }
    }

    #[test]
    fn parse_transcript_line_ignores_developer_messages() {
        let line = r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"text","text":"system"}]},"session_id":"sess-1"}"#;

        assert!(parse_transcript_line("agent-1", 0, line).is_none());
    }

    #[test]
    fn parse_transcript_line_ignores_non_response_item() {
        let line = r#"{"type":"other","payload":{}}"#;
        assert!(parse_transcript_line("agent-1", 0, line).is_none());
    }

    #[test]
    fn parse_opencode_turn_aborted_lifecycle_event() {
        let abort_line = json!({
            "type": "event_msg",
            "payload": {
                "type": "turn_aborted",
                "reason": "session.next.interrupt.requested"
            },
            "session_id": "sess-1"
        })
        .to_string();
        let ordinary_event_line = json!({
            "type": "event_msg",
            "payload": { "type": "status", "message": "ok" },
            "session_id": "sess-1"
        })
        .to_string();

        assert_eq!(
            parse_transcript_lifecycle_event(&abort_line),
            Some(TranscriptLifecycleEvent::Interrupted)
        );
        assert_eq!(parse_transcript_lifecycle_event(&ordinary_event_line), None);
    }

    #[test]
    fn transcript_path_is_under_workspace_root() {
        let state = test_state();
        let path = OpencodeAdapter::transcript_path_for(&state, "agent-42", "session-123");

        assert!(path.ends_with(".qmux/opencode/agent-42/session-123.jsonl"));
        assert!(path.starts_with("/tmp/qmux-opencode-tests"));
    }

    #[test]
    fn transcript_path_rejects_unsafe_session_components() {
        let state = test_state();
        let path = OpencodeAdapter::transcript_path_for(&state, "agent-42", "../outside");
        assert!(path.ends_with(".qmux/opencode/agent-42/pending.jsonl"));
    }
}
