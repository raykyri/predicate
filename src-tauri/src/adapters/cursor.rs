use super::{
    AdapterNotification, AdapterNotificationOutcome, AgentAdapter, ComposerPolicy, LaunchEnv,
    PrepareShellAgentLaunchRequest, PreparedShellAgentLaunch, ShellCommandIntegration,
    SpawnAgentRequest, TranscriptLifecycleEvent, apply_shell_cli_model, cli_flag_value,
    ensure_on_path, hook_transcript_path_acceptable, maybe_record_agent_model,
    parse_claude_native_transcript_value, prepared_shell_agent, record_shell_session_lineage,
    reusable_session_agent, same_dir, shell_cli_model, shell_quote_arg, shell_quote_path,
    string_field, subagent_id,
};
use crate::config::QmuxConfig;
use crate::events::QmuxEvent;
use crate::pty::{
    CommandPlan, InitialPaneSize, PaneMeta, agent_pane_envs, plan_to_spec, recoverable_dir,
    spawn_pty,
};
use crate::state::{AppState, PaneInfo, PaneKind};
use crate::transcript::{Turn, TurnBlock, start_transcript_tail, unwrap_user_query_envelope};
use crate::turn_queue::{IdleResolution, advance_after_idle, is_shell_escape_turn};
use crate::workspace::{
    AgentInfo, AgentStatus, PrepareAgentWorkspaceRequest, attach_agent_pane, mark_agent_failed,
    mark_agent_spawn_failed, prepare_agent_workspace, prepare_agent_workspace_with_parent,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Adapter for the Cursor Agent CLI (`cursor-agent`).
///
/// Cursor's interactive TUI is launched in a qmux pane. Lifecycle comes from a
/// generated observer plugin loaded with `--plugin-dir` (not from user/project
/// `hooks.json`, which would be a shared writable target across panes). Native
/// transcripts live under `~/.cursor/projects/<slug>/agent-transcripts/<id>/`
/// and are Claude-shaped JSONL plus `turn_ended` records. cursor-agent only
/// invokes `stop` / `afterAgentResponse` when user or project `hooks.json`
/// registers them — not for `--plugin-dir` plugins — so a successful
/// `turn_ended` record is the idle signal.
///
/// cursor-agent runs plugin hooks with a constructed environment that does not
/// inherit `QMUX_*`, so the Claude-style env-gated shim cannot identify its
/// pane. qmux writes a binding file per live Cursor pane and the generated
/// plugin shim calls `qmux cursor-notify`, matching Muse. See
/// [`write_cursor_binding`].
///
/// There is no native fork command, and this adapter does not opt into remote
/// groups: `--plugin-dir` points at a locally-materialized plugin.
#[derive(Clone, Debug)]
pub struct CursorAdapter {
    binary: String,
    plugin_dir: PathBuf,
}

impl CursorAdapter {
    pub fn new(config: &QmuxConfig) -> Self {
        Self {
            binary: config.cursor_binary(),
            plugin_dir: config.cursor_plugin_dir.clone(),
        }
    }

    fn ensure_binary(&self) -> Result<String, String> {
        let binary = ensure_on_path(&self.binary).ok_or_else(|| {
            format!(
                "Cursor adapter binary '{}' was not found on PATH or standard macOS tool paths. Install Cursor Agent (`cursor-agent`) or update adapters.cursor.binary in qmux.config.json.",
                self.binary
            )
        })?;
        Ok(binary.display().to_string())
    }

    fn plugin_dir(&self) -> Result<PathBuf, String> {
        ensure_source_cursor_plugin(&self.plugin_dir)?;
        ensure_cursor_plugin_overlay(&self.plugin_dir)
    }

    fn integration_args(&self, cwd: &Path) -> Result<Vec<String>, String> {
        Ok(vec![
            "--plugin-dir".to_string(),
            self.plugin_dir()?.display().to_string(),
            "--workspace".to_string(),
            cwd.display().to_string(),
        ])
    }
}

impl AgentAdapter for CursorAdapter {
    fn id(&self) -> &'static str {
        "cursor"
    }

    fn display_name(&self) -> &'static str {
        "Cursor"
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
        match cursor_shell_disposition(&request.args) {
            CursorShellDisposition::Supervised => Ok(None),
            CursorShellDisposition::Passthrough => {
                let binary = self.ensure_binary()?;
                let cwd = PathBuf::from(&request.cwd);
                if !cwd.is_dir() {
                    return Err(format!(
                        "Cursor working directory {} does not exist",
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
            command_name: "cursor-agent",
            adapter_id: self.id(),
        }]
    }

    fn shell_resume_command(&self, session_id: &str) -> Option<String> {
        Some(format!(
            "cursor-agent --resume {}",
            shell_quote_arg(session_id)
        ))
    }

    fn ingest_notification(
        &self,
        state: &AppState,
        notification: AdapterNotification,
    ) -> Result<AdapterNotificationOutcome, String> {
        self.ingest_cursor_notification(state, notification)
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

    fn transcript_line_model(&self, line: &str) -> Option<String> {
        let value = serde_json::from_str::<Value>(line).ok()?;
        string_field(&value, "model")
            .or_else(|| {
                value
                    .get("message")
                    .and_then(|message| string_field(message, "model"))
            })
            .and_then(|model| super::normalize_agent_model(&model))
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

impl CursorAdapter {
    fn spawn_pane(&self, state: &AppState, request: SpawnAgentRequest) -> Result<PaneInfo, String> {
        let binary = self.ensure_binary()?;
        let plugin_dir = self.plugin_dir()?;
        let options = CursorLaunchOptions::from_value(request.options)?;
        let model = options
            .model
            .clone()
            .or_else(|| request.model.clone())
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty());
        let agent = prepare_agent_workspace_with_parent(
            state,
            PrepareAgentWorkspaceRequest {
                group_id: request.group_id,
                base_repo: request.base_repo,
                base_ref: request.base_ref,
                adapter: self.id().to_string(),
                model: model.clone(),
                effort: None,
                use_worktree: request.use_worktree.unwrap_or(false),
            },
            request.parent_id.as_deref(),
        )?;
        // The Cursor execution mode is adapter-specific launch state. Persist it
        // so recovery cannot silently turn a read-only plan/ask session back into
        // the default agent mode.
        let agent = match options.mode.as_deref() {
            Some(mode) => {
                let mode = mode.to_string();
                state
                    .mutate_agent(&agent.id, |agent| {
                        agent.approval_mode = Some(mode.clone());
                    })?
                    .unwrap_or(agent)
            }
            None => agent,
        };
        let cwd = request
            .cwd
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&agent.worktree_dir));
        if !cwd.is_dir() {
            let _ = mark_agent_failed(state, &agent.id);
            return Err(format!(
                "Cursor working directory {} does not exist",
                cwd.display()
            ));
        }

        let has_initial_prompt = !request.prompt.trim().is_empty();
        let args = build_cursor_args(
            plugin_dir,
            &cwd,
            model.as_deref(),
            options.mode.as_deref(),
            None,
            &request.prompt,
        )?;

        let pane_id = state.next_id("pane");
        let mut envs = agent_pane_envs(state, &pane_id, &agent.id)?;
        envs.push(("QMUX_ADAPTER_ID".to_string(), self.id().to_string()));
        attach_cursor_agent_pane(state, &agent.id, pane_id.clone(), has_initial_prompt)?;
        write_cursor_binding(state, &pane_id, &agent.id, &cwd, None)?;
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
                remove_cursor_binding(&pane_id);
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
        let cwd = recoverable_dir(&agent.worktree_dir).ok_or_else(|| {
            format!(
                "agent worktree {} no longer exists; relaunch manually",
                agent.worktree_dir
            )
        })?;
        let (args, resumed) = build_cursor_resume_args(
            self.plugin_dir()?,
            &cwd,
            agent.model.as_deref(),
            agent.approval_mode.as_deref(),
            agent.session_id.as_deref(),
        )?;
        let mut envs = agent_pane_envs(state, &pane.id, &agent.id)?;
        envs.push(("QMUX_ADAPTER_ID".to_string(), self.id().to_string()));
        write_cursor_binding(
            state,
            &pane.id,
            &agent.id,
            &cwd,
            agent.session_id.as_deref(),
        )?;
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
        let binary = self.ensure_binary()?;
        let cwd = PathBuf::from(&request.cwd);
        if !cwd.is_dir() {
            return Err(format!(
                "Cursor working directory {} does not exist",
                cwd.display()
            ));
        }
        let mut args = self.integration_args(&cwd)?;
        if !state.pane_exists(&request.pane_id)? {
            return Err(format!("pane {} was not found", request.pane_id));
        }
        let cwd_str = cwd.display().to_string();
        let pane_group_id = state
            .pane_group_id(&request.pane_id)?
            .ok_or_else(|| format!("pane {} was not found", request.pane_id))?;
        let resume_session_id = cursor_resume_session_id(&request.args);
        let resume_latest =
            resume_session_id.is_none() && cursor_resumes_latest_session(&request.args);
        let cursor_mode = cursor_cli_mode(&request.args)?;
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
                None if resume_latest => match reusable_latest_cursor_agent(state, &cwd_str)? {
                    Some(existing) => existing,
                    None => prepare_agent_workspace(
                        state,
                        PrepareAgentWorkspaceRequest {
                            group_id: Some(pane_group_id.clone()),
                            base_repo: Some(cwd_str.clone()),
                            base_ref: Some("HEAD".to_string()),
                            adapter: self.id().to_string(),
                            model: shell_cli_model(&request.args),
                            effort: None,
                            use_worktree: false,
                        },
                    )?,
                },
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
            },
        };
        let agent = record_shell_session_lineage(
            state,
            agent,
            self.id(),
            None,
            resume_session_id.as_deref(),
            &cwd_str,
        )?;
        let agent = apply_shell_cli_model(state, agent, &request.args)?;
        let agent = match cursor_mode {
            Some(mode) => state
                .mutate_agent(&agent.id, |agent| {
                    agent.approval_mode = Some(mode.clone());
                })?
                .unwrap_or(agent),
            None => agent,
        };
        let agent = attach_cursor_agent_pane(
            state,
            &agent.id,
            request.pane_id.clone(),
            cursor_args_contain_prompt(&request.args),
        )?;
        write_cursor_binding(
            state,
            &request.pane_id,
            &agent.id,
            &cwd,
            agent.session_id.as_deref().or(resume_session_id.as_deref()),
        )?;
        args.extend(request.args);
        let mut envs = agent_pane_envs(state, &request.pane_id, &agent.id)?;
        envs.push(("QMUX_ADAPTER_ID".to_string(), self.id().to_string()));
        if let Some(session_id) = resume_session_id {
            envs.push(("QMUX_ROOT_SESSION_ID".to_string(), session_id));
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

    fn ingest_cursor_notification(
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
        if matches!(
            hook_event.as_str(),
            "sessionStart"
                | "beforeSubmitPrompt"
                | "afterAgentResponse"
                | "stop"
                | "preToolUse"
                | "beforeShellExecution"
        ) && let Some(current) = agent.as_ref()
        {
            bind_cursor_session(state, current, &notification.payload)?;
            if let Some(refreshed) = state.agent(&current.id)? {
                agent = Some(refreshed);
            }
        }
        if let Some(model) = string_field(&notification.payload, "model")
            && let Some(current) = agent.as_ref()
        {
            let _ = maybe_record_agent_model(state, &current.id, &model);
        }
        let event_type = match hook_event.as_str() {
            "sessionStart" => {
                if let Some(current) = agent.as_ref()
                    && current.status == AgentStatus::Starting
                {
                    state.set_agent_status(&current.id, AgentStatus::Idle)?;
                }
                "agent.session_start"
            }
            "beforeSubmitPrompt" => {
                if let Some(current) = agent.as_ref() {
                    let prompt = string_field(&notification.payload, "prompt");
                    if !prompt.as_deref().is_some_and(is_shell_escape_turn) {
                        state.set_agent_status(&current.id, AgentStatus::Running)?;
                    }
                    send_tracking =
                        Some(state.match_agent_prompt_submit(&current.id, prompt.as_deref())?);
                }
                "agent.prompt_submitted"
            }
            "preToolUse" | "beforeShellExecution" | "afterAgentThought" => {
                if let Some(current) = agent.as_ref() {
                    state.set_agent_status(&current.id, AgentStatus::Running)?;
                }
                "agent.running"
            }
            "subagentStart" => {
                if let Some(current) = agent.as_ref() {
                    state
                        .agent_subagent_started(&current.id, subagent_id(&notification.payload))?;
                    state.set_agent_status(&current.id, AgentStatus::Running)?;
                }
                "agent.subagent_started"
            }
            "subagentStop" => {
                if let Some(current) = agent.as_ref() {
                    let tracked = state
                        .agent_subagent_stopped(&current.id, subagent_id(&notification.payload))?
                        .is_some();
                    if tracked {
                        state.set_agent_status(&current.id, AgentStatus::Running)?;
                    }
                }
                "agent.subagent_stopped"
            }
            // `afterAgentResponse` is the same turn boundary as `stop`. Both
            // are gated by cursor-agent to user/project hooks.json (plugin
            // hooks never run), so this path is a backstop for those files;
            // plugin-only sessions idle via `turn_ended` instead.
            "afterAgentResponse" | "stop" => {
                let waiting_on_subagents = agent
                    .as_ref()
                    .map(|current| state.agent_has_active_subagents(&current.id))
                    .transpose()?
                    .unwrap_or(false);
                let drained = if waiting_on_subagents {
                    false
                } else if let Some(current) = agent.as_ref() {
                    finish_agent_after_stop(state, current)?
                } else {
                    false
                };
                if waiting_on_subagents || drained {
                    "agent.running"
                } else {
                    "agent.done"
                }
            }
            "sessionEnd" => {
                if let Some(current) = agent.as_ref() {
                    state.clear_agent_subagents(&current.id);
                }
                "agent.session_end"
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
        let agent = match agent {
            Some(agent) => state.agent(&agent.id)?.or(Some(agent)),
            None => None,
        };
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
struct CursorLaunchOptions {
    model: Option<String>,
    mode: Option<String>,
}

impl CursorLaunchOptions {
    fn from_value(value: Value) -> Result<Self, String> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let mut options: Self = serde_json::from_value(value)
            .map_err(|err| format!("invalid Cursor adapter options: {err}"))?;
        options.mode = validate_cursor_mode(options.mode.as_deref())?;
        Ok(options)
    }
}

fn validate_cursor_mode(mode: Option<&str>) -> Result<Option<String>, String> {
    let Some(mode) = mode.map(str::trim).filter(|mode| !mode.is_empty()) else {
        return Ok(None);
    };
    if matches!(mode, "plan" | "ask") {
        return Ok(Some(mode.to_string()));
    }
    Err(format!(
        "Cursor adapter does not support --mode {mode}; use plan or ask"
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CursorShellDisposition {
    Supervised,
    Passthrough,
}

fn cursor_shell_disposition(args: &[String]) -> CursorShellDisposition {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return CursorShellDisposition::Supervised;
        }
        if matches!(
            arg.as_str(),
            "--help" | "-h" | "--version" | "-v" | "--print" | "-p" | "--list-models"
        ) || arg.starts_with("--list-models=")
        {
            return CursorShellDisposition::Passthrough;
        }
        if cursor_value_flag(arg) {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        return if cursor_management_command(arg) {
            CursorShellDisposition::Passthrough
        } else {
            CursorShellDisposition::Supervised
        };
    }
    CursorShellDisposition::Supervised
}

fn cursor_management_command(arg: &str) -> bool {
    matches!(
        arg,
        "login"
            | "logout"
            | "status"
            | "whoami"
            | "about"
            | "models"
            | "mcp"
            | "plugin"
            | "worker"
            | "update"
            | "ls"
            | "create-chat"
            | "generate-rule"
            | "rule"
            | "sandbox"
            | "acp"
            | "install-shell-integration"
            | "uninstall-shell-integration"
            | "bedrock"
            | "help"
    )
}

fn cursor_value_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--api-key"
            | "--header"
            | "-H"
            | "--endpoint"
            | "-e"
            | "--output-format"
            | "--mode"
            | "--model"
            | "--sandbox"
            | "--workspace"
            | "--add-dir"
            | "--plugin-dir"
            | "--worktree"
            | "-w"
            | "--worktree-base"
            | "--resume"
    )
}

fn cursor_resume_session_id(args: &[String]) -> Option<String> {
    cli_flag_value(args, "--resume")
        .or_else(|| cursor_resume_subcommand_id(args))
        .filter(|value| cursor_session_id_acceptable(value))
}

fn cursor_resume_subcommand_id(args: &[String]) -> Option<String> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return None;
        }
        if cursor_value_flag(arg) {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        if arg == "resume" {
            return args
                .get(index + 1)
                .filter(|value| !value.starts_with('-'))
                .cloned();
        }
        return None;
    }
    None
}

fn cursor_resumes_latest_session(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return false;
        }
        if arg == "--continue" {
            return true;
        }
        if cursor_value_flag(arg) {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        return arg == "resume"
            && args
                .get(index + 1)
                .is_none_or(|value| value.starts_with('-'));
    }
    false
}

fn cursor_session_subcommand(arg: &str) -> bool {
    matches!(arg, "resume" | "agent")
}

fn reusable_latest_cursor_agent(state: &AppState, cwd: &str) -> Result<Option<AgentInfo>, String> {
    Ok(state
        .list_agents()?
        .into_iter()
        .filter(|agent| {
            agent.adapter == "cursor"
                && agent.pane_id.is_none()
                && agent.session_id.is_some()
                && same_dir(&agent.worktree_dir, cwd)
        })
        .max_by_key(|agent| agent.created_at))
}

fn cursor_cli_mode(args: &[String]) -> Result<Option<String>, String> {
    let mut mode = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            break;
        }
        if arg == "--plan" {
            mode = Some("plan".to_string());
        } else if arg == "--mode" {
            let value = args
                .get(index + 1)
                .filter(|value| !value.starts_with('-'))
                .ok_or_else(|| "Cursor --mode requires plan or ask".to_string())?;
            mode = validate_cursor_mode(Some(value))?;
            index += 1;
        } else if let Some(value) = arg.strip_prefix("--mode=") {
            mode = validate_cursor_mode(Some(value))?;
        }
        index += 1;
    }
    Ok(mode)
}

fn cursor_args_contain_prompt(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return args.get(index + 1).is_some_and(|value| !value.is_empty());
        }
        if cursor_value_flag(arg) {
            if args
                .get(index + 1)
                .is_some_and(|value| !value.starts_with('-'))
            {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        if cursor_management_command(arg) || cursor_session_subcommand(arg) {
            index += 1;
            continue;
        }
        return true;
    }
    false
}

fn build_cursor_args(
    plugin_dir: PathBuf,
    cwd: &Path,
    model: Option<&str>,
    mode: Option<&str>,
    resume: Option<&str>,
    prompt: &str,
) -> Result<Vec<String>, String> {
    let mut args = vec![
        "--plugin-dir".to_string(),
        plugin_dir.display().to_string(),
        "--workspace".to_string(),
        cwd.display().to_string(),
    ];
    if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(mode) = mode.map(str::trim).filter(|mode| !mode.is_empty()) {
        let mode = validate_cursor_mode(Some(mode))?.expect("non-empty Cursor mode");
        args.push("--mode".to_string());
        args.push(mode);
    }
    if let Some(session_id) = resume.map(str::trim).filter(|id| !id.is_empty()) {
        args.push("--resume".to_string());
        args.push(session_id.to_string());
    }
    let prompt = prompt.trim();
    if !prompt.is_empty() {
        args.push("--".to_string());
        args.push(prompt.to_string());
    }
    Ok(args)
}

fn build_cursor_resume_args(
    plugin_dir: PathBuf,
    cwd: &Path,
    model: Option<&str>,
    mode: Option<&str>,
    session_id: Option<&str>,
) -> Result<(Vec<String>, bool), String> {
    let Some(session_id) = session_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return Ok((
            build_cursor_args(plugin_dir, cwd, model, mode, None, "")?,
            false,
        ));
    };
    Ok((
        build_cursor_args(plugin_dir, cwd, model, mode, Some(session_id), "")?,
        true,
    ))
}

fn attach_cursor_agent_pane(
    state: &AppState,
    agent_id: &str,
    pane_id: String,
    has_initial_prompt: bool,
) -> Result<AgentInfo, String> {
    let agent = attach_agent_pane(state, agent_id, pane_id)?;
    if !has_initial_prompt {
        if let Some(updated) = state.set_agent_status(agent_id, AgentStatus::Idle)? {
            return Ok(updated);
        }
    }
    Ok(agent)
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

fn bind_cursor_session(
    state: &AppState,
    current: &AgentInfo,
    payload: &Value,
) -> Result<(), String> {
    let session_id = string_field(payload, "conversation_id")
        .or_else(|| string_field(payload, "session_id"))
        .or_else(|| string_field(payload, "sessionId"))
        .filter(|id| cursor_session_id_acceptable(id));
    let reported_path = string_field(payload, "transcript_path")
        .or_else(|| string_field(payload, "transcriptPath"))
        .filter(|candidate| {
            hook_transcript_path_acceptable(current.transcript_path.as_deref(), candidate)
        });
    let workspace = payload
        .get("workspace_roots")
        .and_then(Value::as_array)
        .and_then(|roots| roots.first())
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .unwrap_or(current.worktree_dir.as_str());
    let synthesized = session_id.as_deref().and_then(|session_id| {
        cursor_home().ok().and_then(|home| {
            let path = cursor_transcript_path(&home, workspace, session_id);
            let rendered = path.display().to_string();
            hook_transcript_path_acceptable(current.transcript_path.as_deref(), &rendered)
                .then_some(rendered)
        })
    });
    let transcript_path = reported_path.or(synthesized);
    let updated = state.mutate_agent(&current.id, |agent| {
        if let Some(session_id) = session_id.clone() {
            agent.session_id = Some(session_id);
        }
        if let Some(transcript_path) = transcript_path.clone() {
            agent.transcript_path = Some(transcript_path);
        }
    })?;
    if let Some(session_id) = session_id {
        claim_cursor_binding(current, &session_id);
    }
    if let Some(path) = updated.and_then(|agent| agent.transcript_path) {
        start_transcript_tail(
            state.clone(),
            current.id.clone(),
            path,
            "cursor".to_string(),
        );
    }
    Ok(())
}

fn cursor_home() -> Result<PathBuf, String> {
    dirs::home_dir().ok_or_else(|| "could not determine home directory".to_string())
}

fn cursor_project_slug(cwd: &str) -> String {
    let path = cwd.trim_end_matches('/');
    let path = path.strip_prefix('/').unwrap_or(path);
    path.replace('/', "-")
}

fn cursor_transcript_path(home: &Path, cwd: &str, session_id: &str) -> PathBuf {
    home.join(".cursor")
        .join("projects")
        .join(cursor_project_slug(cwd))
        .join("agent-transcripts")
        .join(session_id)
        .join(format!("{session_id}.jsonl"))
}

fn cursor_session_id_acceptable(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

// ---------------------------------------------------------------------------
// Pane bindings and generated plugin
//
// cursor-agent runs plugin hooks with a constructed env that does not inherit
// QMUX_*. The bundled env-gated shim therefore never notifies, so a restored
// pane has no session id to `--resume`. qmux materializes a plugin overlay
// whose shim calls `cursor-notify` with a baked CLI path and bindings dir,
// and writes one binding file per live pane (the Muse pattern).
// ---------------------------------------------------------------------------

fn ensure_source_cursor_plugin(source: &Path) -> Result<(), String> {
    let manifest = source.join(".cursor-plugin").join("plugin.json");
    let hooks = source.join("hooks").join("hooks.json");
    if !source.is_dir() || !manifest.is_file() || !hooks.is_file() {
        return Err(format!(
            "Cursor integration plugin was not found at {}. Reinstall qmux or set QMUX_CURSOR_PLUGIN_DIR to the bundled qmux-cursor-plugin directory.",
            source.display()
        ));
    }
    Ok(())
}

pub(crate) fn cursor_integration_home() -> Result<PathBuf, String> {
    if let Some(explicit) = env::var_os("QMUX_CURSOR_HOME") {
        return Ok(PathBuf::from(explicit));
    }
    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or_else(|| {
            "XDG_DATA_HOME and HOME are not set; cannot configure the Cursor integration"
                .to_string()
        })?;
    Ok(data_home.join("qmux").join("cursor"))
}

fn cursor_bindings_dir() -> Result<PathBuf, String> {
    Ok(cursor_integration_home()?.join("bindings"))
}

fn ensure_cursor_plugin_overlay(source: &Path) -> Result<PathBuf, String> {
    let home = cursor_integration_home()?;
    let plugin_dir = home.join("plugin");
    let cli = crate::launch_path::qmux_cli_path()?;
    let bindings = cursor_bindings_dir()?;
    let shim = cursor_hook_shim(&cli, &bindings);
    let manifest = fs::read_to_string(source.join(".cursor-plugin").join("plugin.json"))
        .map_err(|err| format!("failed to read Cursor plugin manifest: {err}"))?;
    let hooks = fs::read_to_string(source.join("hooks").join("hooks.json"))
        .map_err(|err| format!("failed to read Cursor plugin hooks: {err}"))?;
    let fingerprint = fingerprint_of(&[
        shim.clone(),
        manifest.clone(),
        hooks.clone(),
        cli.display().to_string(),
        bindings.display().to_string(),
    ]);
    let stamp_path = home.join("installed.stamp");
    if file_matches(&stamp_path, &fingerprint)
        && plugin_dir.join("scripts").join("qmux-notify.sh").is_file()
    {
        return Ok(plugin_dir);
    }

    for dir in [
        plugin_dir.join(".cursor-plugin"),
        plugin_dir.join("hooks"),
        plugin_dir.join("scripts"),
    ] {
        fs::create_dir_all(&dir)
            .map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
    }
    write_if_changed(
        &plugin_dir.join(".cursor-plugin").join("plugin.json"),
        &manifest,
    )?;
    write_if_changed(&plugin_dir.join("hooks").join("hooks.json"), &hooks)?;
    let shim_path = plugin_dir.join("scripts").join("qmux-notify.sh");
    write_if_changed(&shim_path, &shim)?;
    fs::set_permissions(&shim_path, fs::Permissions::from_mode(0o755))
        .map_err(|err| format!("failed to chmod {}: {err}", shim_path.display()))?;
    fs::write(&stamp_path, &fingerprint)
        .map_err(|err| format!("failed to write {}: {err}", stamp_path.display()))?;
    Ok(plugin_dir)
}

fn cursor_hook_shim(cli_path: &Path, bindings_dir: &Path) -> String {
    format!(
        r#"#!/bin/sh
# Generated by qmux. Do not edit.
event="${{1:-}}"
payload=$(cat || true)
if [ -n "$event" ]; then
  printf '%s' "$payload" | {} cursor-notify "$event" {} >/dev/null 2>&1 || true
fi
printf '%s\n' '{{}}'
"#,
        shell_quote_path(cli_path),
        shell_quote_path(bindings_dir)
    )
}

fn write_cursor_binding(
    state: &AppState,
    pane_id: &str,
    agent_id: &str,
    cwd: &Path,
    session_id: Option<&str>,
) -> Result<(), String> {
    prune_cursor_bindings(state);
    let dir = cursor_bindings_dir()?;
    fs::create_dir_all(&dir).map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .map_err(|err| format!("failed to chmod {}: {err}", dir.display()))?;

    let canonical_cwd = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let document = json!({
        "paneId": pane_id,
        "agentId": agent_id,
        "cwd": cwd.display().to_string(),
        "canonicalCwd": canonical_cwd.display().to_string(),
        "sessionId": session_id,
        "sock": state.config().socket_path.display().to_string(),
        "token": state.pane_token(pane_id)?,
        "updatedAt": unix_millis(),
    });
    let path = cursor_binding_path(&dir, pane_id);
    let raw = serde_json::to_string(&document)
        .map_err(|err| format!("failed to encode Cursor pane binding: {err}"))?;
    fs::write(&path, raw).map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("failed to chmod {}: {err}", path.display()))?;
    Ok(())
}

fn claim_cursor_binding(agent: &AgentInfo, session_id: &str) {
    let Some(pane_id) = agent.pane_id.as_deref() else {
        return;
    };
    if let Err(err) = stamp_cursor_binding_session(pane_id, session_id) {
        eprintln!("qmux: failed to record Cursor session binding for pane {pane_id}: {err}");
    }
}

fn stamp_cursor_binding_session(pane_id: &str, session_id: &str) -> Result<(), String> {
    let dir = cursor_bindings_dir()?;
    let path = cursor_binding_path(&dir, pane_id);
    let raw = fs::read_to_string(&path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let mut document = serde_json::from_str::<Value>(&raw)
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
    let Some(object) = document.as_object_mut() else {
        return Err(format!("{} is not a binding object", path.display()));
    };
    object.insert("sessionId".to_string(), json!(session_id));
    object.insert("updatedAt".to_string(), json!(unix_millis()));
    let raw = serde_json::to_string(&document)
        .map_err(|err| format!("failed to encode Cursor pane binding: {err}"))?;
    fs::write(&path, raw).map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("failed to chmod {}: {err}", path.display()))
}

fn cursor_binding_path(dir: &Path, pane_id: &str) -> PathBuf {
    dir.join(format!("{}.json", sanitize_binding_name(pane_id)))
}

fn sanitize_binding_name(pane_id: &str) -> String {
    pane_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn remove_cursor_binding(pane_id: &str) {
    let Ok(dir) = cursor_bindings_dir() else {
        return;
    };
    let _ = fs::remove_file(cursor_binding_path(&dir, pane_id));
}

const CURSOR_BINDING_PRUNE_GRACE: u64 = 60_000;

fn prune_cursor_bindings(state: &AppState) {
    let Ok(dir) = cursor_bindings_dir() else {
        return;
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let now = unix_millis();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let document = fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
        let Some(document) = document else {
            let _ = fs::remove_file(&path);
            continue;
        };
        let written = document
            .get("updatedAt")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        if now.saturating_sub(written) < CURSOR_BINDING_PRUNE_GRACE {
            continue;
        }
        let stale = match string_field(&document, "paneId") {
            Some(pane_id) => !state.pane_exists(&pane_id).unwrap_or(true),
            None => true,
        };
        if stale {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Tokens are minted per process, so every binding left by a previous run is
/// already useless. Called once at startup before recovery writes fresh ones.
pub fn clear_cursor_bindings() {
    let Ok(dir) = cursor_bindings_dir() else {
        return;
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            let _ = fs::remove_file(&path);
        }
    }
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

fn write_if_changed(path: &Path, contents: &str) -> Result<(), String> {
    if file_matches(path, contents) {
        return Ok(());
    }
    fs::write(path, contents).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn file_matches(path: &Path, contents: &str) -> bool {
    fs::read_to_string(path).is_ok_and(|existing| existing == contents)
}

fn fingerprint_of(parts: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    format!("{:x}\n", hasher.finalize())
}

fn parse_transcript_line(agent_id: &str, source_index: usize, line: &str) -> Option<Turn> {
    let mut value = serde_json::from_str::<Value>(line).ok()?;
    if value.get("type").and_then(Value::as_str) == Some("turn_ended") {
        return None;
    }
    // Cursor JSONL puts `role` on the record (`{"role":"user","message":{...}}`).
    // Claude's parser reads `message.role` or `type`, so copy the outer role
    // onto the message when it is missing.
    if let Some(role) = value.get("role").cloned()
        && let Some(message) = value.get_mut("message").and_then(Value::as_object_mut)
    {
        message.entry("role").or_insert(role);
    }
    let mut turn = parse_claude_native_transcript_value(agent_id, source_index, &value)?;
    if turn.role == "user" {
        normalize_cursor_user_turn(&mut turn);
    }
    Some(turn)
}

/// Cursor Agent persists prompts as `<timestamp>` + `<user_query>` harness.
/// Leaving those tags in the turn makes qmux's injected-instruction detector
/// collapse the user's words into a chip, so unwrap before the timeline sees them.
fn normalize_cursor_user_turn(turn: &mut Turn) {
    for block in &mut turn.blocks {
        if let TurnBlock::Text { text } = block {
            let unwrapped = unwrap_user_query_envelope(text);
            if unwrapped != *text {
                *text = unwrapped;
            }
        }
    }
}

fn parse_transcript_lifecycle_event(line: &str) -> Option<TranscriptLifecycleEvent> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    match value.get("type").and_then(Value::as_str)? {
        "turn_ended" => {
            let status = value.get("status").and_then(Value::as_str).unwrap_or("");
            if matches!(status, "error" | "aborted" | "cancelled" | "canceled") {
                Some(TranscriptLifecycleEvent::Interrupted)
            } else {
                // cursor-agent writes this unconditionally at end of turn. Its
                // `stop` hook is not invoked for `--plugin-dir` plugins.
                Some(TranscriptLifecycleEvent::TurnCompleted)
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn project_slug_strips_leading_slash_and_replaces_separators() {
        assert_eq!(
            cursor_project_slug("/tmp/qmux-cursor-spike/workspace"),
            "tmp-qmux-cursor-spike-workspace"
        );
        assert_eq!(
            cursor_project_slug("/Users/raymond/Code/qmux/"),
            "Users-raymond-Code-qmux"
        );
    }

    #[test]
    fn transcript_path_uses_home_slug_and_session_id() {
        let path = cursor_transcript_path(
            Path::new("/Users/raymond"),
            "/tmp/qmux-cursor-spike/workspace",
            "d68b3651-d521-4263-aad5-5cabcb413035",
        );
        assert_eq!(
            path,
            PathBuf::from(
                "/Users/raymond/.cursor/projects/tmp-qmux-cursor-spike-workspace/agent-transcripts/d68b3651-d521-4263-aad5-5cabcb413035/d68b3651-d521-4263-aad5-5cabcb413035.jsonl"
            )
        );
    }

    #[test]
    fn session_ids_reject_paths() {
        assert!(cursor_session_id_acceptable(
            "d68b3651-d521-4263-aad5-5cabcb413035"
        ));
        assert!(!cursor_session_id_acceptable("../etc/passwd"));
        assert!(!cursor_session_id_acceptable("/tmp/foo"));
        assert!(!cursor_session_id_acceptable(""));
    }

    #[test]
    fn shell_passthrough_covers_utilities_and_headless_print() {
        assert_eq!(
            cursor_shell_disposition(&args(&["login"])),
            CursorShellDisposition::Passthrough
        );
        assert_eq!(
            cursor_shell_disposition(&args(&["mcp", "list"])),
            CursorShellDisposition::Passthrough
        );
        assert_eq!(
            cursor_shell_disposition(&args(&["acp"])),
            CursorShellDisposition::Passthrough
        );
        assert_eq!(
            cursor_shell_disposition(&args(&["--print", "hello"])),
            CursorShellDisposition::Passthrough
        );
        assert_eq!(
            cursor_shell_disposition(&args(&["--help"])),
            CursorShellDisposition::Passthrough
        );
        assert_eq!(
            cursor_shell_disposition(&args(&["create-chat"])),
            CursorShellDisposition::Passthrough
        );
        assert_eq!(
            cursor_shell_disposition(&args(&["ls"])),
            CursorShellDisposition::Passthrough
        );
        assert_eq!(
            cursor_shell_disposition(&args(&["resume"])),
            CursorShellDisposition::Supervised
        );
        assert_eq!(
            cursor_shell_disposition(&[]),
            CursorShellDisposition::Supervised
        );
        assert_eq!(
            cursor_shell_disposition(&args(&["--resume", "abc"])),
            CursorShellDisposition::Supervised
        );
        assert_eq!(
            cursor_shell_disposition(&args(&["--continue"])),
            CursorShellDisposition::Supervised
        );
        assert_eq!(
            cursor_shell_disposition(&args(&["fix the tests"])),
            CursorShellDisposition::Supervised
        );
    }

    #[test]
    fn resume_id_requires_a_concrete_value() {
        assert_eq!(
            cursor_resume_session_id(&args(&["--resume", "d68b3651-d521-4263-aad5-5cabcb413035"]))
                .as_deref(),
            Some("d68b3651-d521-4263-aad5-5cabcb413035")
        );
        assert_eq!(
            cursor_resume_session_id(&args(&["resume", "d68b3651-d521-4263-aad5-5cabcb413035"]))
                .as_deref(),
            Some("d68b3651-d521-4263-aad5-5cabcb413035")
        );
        assert_eq!(cursor_resume_session_id(&args(&["--resume"])), None);
        assert_eq!(cursor_resume_session_id(&args(&["--continue"])), None);
        assert_eq!(
            cursor_resume_session_id(&args(&["--resume", "../etc/passwd"])),
            None
        );
        assert!(cursor_resumes_latest_session(&args(&["--continue"])));
        assert!(cursor_resumes_latest_session(&args(&["resume"])));
        assert!(!cursor_resumes_latest_session(&args(&[
            "--resume",
            "d68b3651-d521-4263-aad5-5cabcb413035"
        ])));
        assert!(!cursor_resumes_latest_session(&args(&["login"])));
    }

    #[test]
    fn shell_modes_are_validated_and_normalized() {
        assert_eq!(
            cursor_cli_mode(&args(&["--mode=ask"])).unwrap().as_deref(),
            Some("ask")
        );
        assert_eq!(
            cursor_cli_mode(&args(&["--plan"])).unwrap().as_deref(),
            Some("plan")
        );
        assert!(cursor_cli_mode(&args(&["--mode", "agent"])).is_err());
        assert!(cursor_cli_mode(&args(&["--mode"])).is_err());
    }

    #[test]
    fn launch_args_inject_plugin_workspace_and_delimited_prompt() {
        let args = build_cursor_args(
            PathBuf::from("/opt/qmux-cursor-plugin"),
            Path::new("/tmp/work"),
            Some("composer-2.5"),
            Some("plan"),
            None,
            "fix it",
        )
        .unwrap();
        assert_eq!(
            args,
            vec![
                "--plugin-dir",
                "/opt/qmux-cursor-plugin",
                "--workspace",
                "/tmp/work",
                "--model",
                "composer-2.5",
                "--mode",
                "plan",
                "--",
                "fix it",
            ]
        );
    }

    #[test]
    fn resume_args_pass_session_id_without_a_prompt() {
        let (args, resumed) = build_cursor_resume_args(
            PathBuf::from("/opt/qmux-cursor-plugin"),
            Path::new("/tmp/work"),
            None,
            Some("plan"),
            Some("d68b3651-d521-4263-aad5-5cabcb413035"),
        )
        .unwrap();
        assert!(resumed);
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--mode".to_string(), "plan".to_string()])
        );
        assert!(args.windows(2).any(|pair| pair
            == [
                "--resume".to_string(),
                "d68b3651-d521-4263-aad5-5cabcb413035".to_string()
            ]));
        assert!(!args.iter().any(|arg| arg == "--"));
    }

    #[test]
    fn generated_hook_shim_forwards_without_needing_the_environment() {
        let shim = cursor_hook_shim(
            Path::new("/Applications/qmux.app/qmux"),
            Path::new("/data/qmux/cursor/bindings"),
        );
        assert!(shim.contains("cursor-notify"));
        assert!(shim.contains("'/Applications/qmux.app/qmux'"));
        assert!(shim.contains("'/data/qmux/cursor/bindings'"));
        assert!(!shim.contains("QMUX_SOCK"));
        assert!(shim.contains("printf '%s\\n' '{}'"));
    }

    #[test]
    fn parser_reads_claude_shaped_jsonl_and_turn_ended_lifecycle() {
        let user = r#"{"role":"user","message":{"content":[{"type":"text","text":"PONG?"}]}}"#;
        let assistant =
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"PONG"}]}}"#;
        let ended = r#"{"type":"turn_ended","status":"success"}"#;
        let user_turn = parse_transcript_line("agent-1", 0, user).unwrap();
        assert_eq!(user_turn.role, "user");
        let assistant_turn = parse_transcript_line("agent-1", 1, assistant).unwrap();
        assert_eq!(assistant_turn.role, "assistant");
        assert_eq!(parse_transcript_line("agent-1", 2, ended), None);
        assert_eq!(
            parse_transcript_lifecycle_event(ended),
            Some(TranscriptLifecycleEvent::TurnCompleted)
        );
        assert_eq!(
            parse_transcript_lifecycle_event(r#"{"type":"turn_ended"}"#),
            Some(TranscriptLifecycleEvent::TurnCompleted)
        );
        assert_eq!(
            parse_transcript_lifecycle_event(r#"{"type":"turn_ended","status":"cancelled"}"#),
            Some(TranscriptLifecycleEvent::Interrupted)
        );
    }

    #[test]
    fn unwraps_cursor_user_query_envelope() {
        let user = r#"{"role":"user","message":{"content":[{"type":"text","text":"<timestamp>Wednesday, Aug 19, 2026, 3:52 PM (UTC-4)</timestamp>\n<user_query>\ncan you cherry pick those 7 commits onto HEAD, except 3b22fc07\n</user_query>"}]}}"#;
        let turn = parse_transcript_line("agent-1", 0, user).unwrap();
        assert_eq!(turn.role, "user");
        assert!(matches!(
            &turn.blocks[0],
            TurnBlock::Text { text }
                if text == "can you cherry pick those 7 commits onto HEAD, except 3b22fc07"
        ));
    }

    #[test]
    fn unwraps_cursor_user_query_preserving_image_markers() {
        let user = r#"{"role":"user","message":{"content":[{"type":"text","text":"[Image]\n<timestamp>Wednesday, Aug 19, 2026, 4:25 PM (UTC-4)</timestamp>\n<user_query>\ncursor-agent transcripts seem to not have user messages, they're mistakenly collapsed: [Image #1] \n</user_query>"}]}}"#;
        let turn = parse_transcript_line("agent-1", 0, user).unwrap();
        let TurnBlock::Text { text } = &turn.blocks[0] else {
            panic!("expected a text block");
        };
        assert!(
            text.contains("cursor-agent transcripts seem to not have user messages"),
            "user query must survive: {text:?}"
        );
        assert!(
            text.contains("[Image]") && text.contains("[Image #1]"),
            "image markers must survive: {text:?}"
        );
        assert!(
            !text.contains("<user_query>") && !text.contains("<timestamp>"),
            "harness tags must be stripped: {text:?}"
        );
    }

    #[test]
    fn launch_args_reject_unknown_modes() {
        let err = CursorLaunchOptions::from_value(json!({ "mode": "yolo" })).unwrap_err();
        assert!(err.contains("plan or ask"), "{err}");

        let err = build_cursor_args(
            PathBuf::from("/opt/qmux-cursor-plugin"),
            Path::new("/tmp/work"),
            None,
            Some("yolo"),
            None,
            "",
        )
        .unwrap_err();
        assert!(err.contains("plan or ask"), "{err}");
    }

    #[test]
    fn positional_prompts_are_detected_after_flags() {
        assert!(cursor_args_contain_prompt(&args(&["fix the tests"])));
        assert!(!cursor_args_contain_prompt(&args(&[
            "--resume",
            "d68b3651-d521-4263-aad5-5cabcb413035"
        ])));
        assert!(!cursor_args_contain_prompt(&args(&["--continue"])));
        assert!(!cursor_args_contain_prompt(&args(&["resume"])));
        assert!(cursor_args_contain_prompt(&args(&["--", "hello"])));
        assert!(!cursor_args_contain_prompt(&args(&[
            "--model",
            "composer-2.5"
        ])));
    }

    fn test_state() -> AppState {
        AppState::new(QmuxConfig {
            remotes: Default::default(),
            workspace_root: PathBuf::from("/tmp/qmux-cursor-test"),
            socket_path: PathBuf::from("/tmp/qmux-cursor-test.sock"),
            adapters: Default::default(),
            legacy_claude_binary: None,
            claude_plugin_dir: PathBuf::new(),
            opencode_plugin_dir: PathBuf::new(),
            pi_extension_dir: PathBuf::new(),
            cursor_plugin_dir: PathBuf::new(),
        })
    }

    fn sample_agent() -> AgentInfo {
        AgentInfo {
            id: "agent-1".to_string(),
            group_id: "group-1".to_string(),
            adapter: "cursor".to_string(),
            worktree_dir: "/tmp/qmux-cursor-test".to_string(),
            branch: None,
            active_workspace: None,
            pane_id: Some("pane-1".to_string()),
            orphaned_queue_pane_id: None,
            session_id: None,
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
        }
    }

    fn ingest(state: &AppState, event: &str) -> QmuxEvent {
        match CursorAdapter::new(state.config()).ingest_notification(
            state,
            AdapterNotification {
                adapter_id: Some("cursor".to_string()),
                event: event.to_string(),
                pane_id: Some("pane-1".to_string()),
                agent_id: Some("agent-1".to_string()),
                payload: json!({}),
            },
        ) {
            Ok(AdapterNotificationOutcome::Event(event)) => event,
            Err(err) => panic!("{err}"),
        }
    }

    #[test]
    fn stop_and_after_agent_response_mark_a_running_agent_done() {
        for event in ["stop", "afterAgentResponse"] {
            let state = test_state();
            state.insert_agent(sample_agent()).unwrap();
            let emitted = ingest(&state, event);
            assert_eq!(emitted.event_type, "agent.done", "{event}");
            let agent = state.agent("agent-1").unwrap().expect("agent exists");
            assert!(matches!(agent.status, AgentStatus::Done), "{event}");
        }
    }
}
