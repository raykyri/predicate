use super::{
    AdapterNotification, AdapterNotificationOutcome, AgentAdapter, ComposerPolicy, LaunchEnv,
    PrepareShellAgentLaunchRequest, PreparedShellAgentLaunch, ShellCommandIntegration,
    SpawnAgentRequest, apply_shell_cli_model, cli_flag_value, ensure_on_path, prepared_shell_agent,
    record_shell_session_lineage, reusable_session_agent, shell_cli_model, shell_quote_arg,
    shell_quote_path,
};
use crate::config::QmuxConfig;
use crate::events::QmuxEvent;
use crate::pty::{
    CommandPlan, InitialPaneSize, PaneMeta, SupportFile, agent_pane_envs, plan_to_spec,
    recoverable_dir, spawn_pty,
};
use crate::state::{AppState, PaneInfo, PaneKind};
use crate::transcript::{Turn, TurnBlock, rfc3339_to_epoch_ms, start_transcript_tail};
use crate::turn_queue::{IdleResolution, advance_after_idle, is_shell_escape_turn};
use crate::workspace::{
    AgentInfo, AgentStatus, PrepareAgentWorkspaceRequest, attach_agent_pane, mark_agent_failed,
    mark_agent_spawn_failed, prepare_agent_workspace, prepare_agent_workspace_with_parent,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Permission modes the Devin CLI accepts on `--permission-mode`.
/// `auto` is Devin's default; `normal` is documented as an alias of `auto`.
/// `autonomous` requires `--sandbox` and is not offered in the qmux launcher.
const DEVIN_PERMISSION_MODES: &[&str] = &[
    "auto",
    "normal",
    "accept-edits",
    "smart",
    "dangerous",
    "bypass",
    "yolo",
];

/// Lifecycle events Devin documents and fires. `--config` replaces the user
/// file rather than merging, so this list is the complete hook set qmux
/// installs. Unknown names are omitted: Devin may reject the whole config.
const DEVIN_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Stop",
    "PostCompaction",
    "SessionEnd",
];

/// Adapter for Cognition's Devin CLI (`devin`).
///
/// Devin is a local interactive TUI with Claude-shaped lifecycle hooks, session
/// resume (`--resume` / `-r`), and ATIF JSON transcripts. This adapter launches
/// the TUI in a qmux pane and wraps `devin` in qmux shells. Each spawn copies
/// the user's `~/.config/devin/config.json`, injects `qmux notify` hooks, and
/// passes that file as `--config` so the original user file is never written.
/// Conversation history is `--export`ed to a qmux-owned ATIF JSON file and
/// parsed as a whole document for the sidebar timeline.
///
/// There is no CLI fork flag (`/fork` is TUI-only). Remote launches stay off:
/// hook config and the data dir have not been checked on a remote host.
#[derive(Clone, Debug)]
pub struct DevinAdapter {
    binary: String,
}

impl DevinAdapter {
    pub fn new(config: &QmuxConfig) -> Self {
        Self {
            binary: config.devin_binary(),
        }
    }

    fn ensure_binary(&self) -> Result<String, String> {
        let binary = ensure_on_path(&self.binary).ok_or_else(|| {
            format!(
                "Devin adapter binary '{}' was not found on PATH or standard macOS tool paths. Install the Devin CLI or update adapters.devin.binary in qmux.config.json.",
                self.binary
            )
        })?;
        Ok(binary.display().to_string())
    }
}

impl AgentAdapter for DevinAdapter {
    fn id(&self) -> &'static str {
        "devin"
    }

    fn display_name(&self) -> &'static str {
        "Devin"
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
        match devin_shell_disposition(&request.args)? {
            DevinShellDisposition::Supervised => Ok(None),
            DevinShellDisposition::Passthrough => {
                let binary = self.ensure_binary()?;
                let cwd = PathBuf::from(&request.cwd);
                if !cwd.is_dir() {
                    return Err(format!(
                        "Devin working directory {} does not exist",
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
            command_name: "devin",
            adapter_id: self.id(),
        }]
    }

    fn shell_resume_command(&self, session_id: &str) -> Option<String> {
        Some(format!("devin --resume {}", shell_quote_arg(session_id)))
    }

    fn ingest_notification(
        &self,
        state: &AppState,
        notification: AdapterNotification,
    ) -> Result<AdapterNotificationOutcome, String> {
        self.ingest_devin_notification(state, notification)
    }

    fn parse_transcript_line(
        &self,
        _agent_id: &str,
        _source_index: usize,
        _line: &str,
    ) -> Option<Turn> {
        None
    }

    fn resolve_transcript_turns(
        &self,
        agent_id: &str,
        source_index_offset: usize,
        lines: &[String],
    ) -> Vec<Turn> {
        resolve_devin_transcript_turns(agent_id, source_index_offset, lines)
    }

    fn transcript_line_can_update_turn_status(&self, _line: &str) -> bool {
        // ATIF is one JSON object. Any newly tailed lines belong to a rewrite
        // of that object, so the whole document must be reparsed.
        true
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

impl DevinAdapter {
    fn spawn_pane(&self, state: &AppState, request: SpawnAgentRequest) -> Result<PaneInfo, String> {
        let binary = self.ensure_binary()?;
        let options = DevinLaunchOptions::from_value(request.options)?;
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
        let agent = match trimmed(options.permission_mode.as_deref()) {
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
                "Devin working directory {} does not exist",
                cwd.display()
            ));
        }

        let has_initial_prompt = prompt_has_initial_text(&request.prompt);
        let pane_id = state.next_id("pane");
        let (config_path, hook_config) = match hook_config_support_file(state.config(), &pane_id) {
            Ok(planned) => planned,
            Err(err) => {
                let _ = mark_agent_failed(state, &agent.id);
                return Err(err);
            }
        };
        let export_path = match bind_devin_export(state, &agent.id) {
            Ok(path) => path,
            Err(err) => {
                let _ = mark_agent_failed(state, &agent.id);
                return Err(err);
            }
        };
        let args = prepend_devin_managed_flags(
            &config_path,
            &export_path,
            build_devin_args(
                model.as_deref(),
                options.permission_mode.as_deref(),
                &request.prompt,
            ),
        );
        let envs = devin_pane_envs(state, &pane_id, &agent.id)?;
        attach_devin_agent_pane(state, &agent.id, pane_id.clone(), has_initial_prompt)?;
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
                support_files: vec![hook_config],
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
        let cwd = recoverable_dir(&agent.worktree_dir).ok_or_else(|| {
            format!(
                "agent worktree {} no longer exists; relaunch manually",
                agent.worktree_dir
            )
        })?;
        let (config_path, hook_config) = hook_config_support_file(state.config(), &pane.id)?;
        let export_path = bind_devin_export(state, &agent.id)?;
        let (args, resumed) = build_devin_resume_args(
            agent.model.as_deref(),
            agent.approval_mode.as_deref(),
            agent.session_id.as_deref(),
        );
        let args = prepend_devin_managed_flags(&config_path, &export_path, args);
        let envs = devin_pane_envs(state, &pane.id, &agent.id)?;
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
                support_files: vec![hook_config],
                support_file_fallback: None,
            },
        )?;
        let info = spawn_pty(state, spec)?;

        let mut restored = agent.clone();
        restored.pane_id = Some(pane.id.clone());
        restored.status = AgentStatus::Idle;
        state.update_agent(restored.clone())?;
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
        validate_devin_supervised_args(&request.args)?;
        if !state.pane_exists(&request.pane_id)? {
            return Err(format!("pane {} was not found", request.pane_id));
        }
        let shell_cwd = PathBuf::from(&request.cwd);
        if !shell_cwd.is_dir() {
            return Err(format!(
                "Devin working directory {} does not exist",
                shell_cwd.display()
            ));
        }

        let cwd_str = shell_cwd.display().to_string();
        let pane_group_id = state
            .pane_group_id(&request.pane_id)?
            .ok_or_else(|| format!("pane {} was not found", request.pane_id))?;
        let resume_session_id = devin_resume_session_id(&request.args).map(str::to_string);
        let shell_model = shell_cli_model(&request.args);
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
                        model: shell_model,
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
        if let Some(mode) = cli_flag_value(&request.args, "--permission-mode") {
            let mode = canonicalize_permission_mode(&mode)?;
            let _ = state.mutate_agent(&agent.id, |agent| {
                agent.approval_mode = Some(mode);
            })?;
        }
        // The in-shell launch is exec'd by the CLI supervisor as soon as this
        // response returns — there is no PTY-spawn step in between to
        // materialize support files, so write the hook config eagerly here.
        let config_path = match hook_config_support_file(state.config(), &request.pane_id).and_then(
            |(config_path, hook_config)| {
                crate::pty::materialize_support_files(&[hook_config])?;
                Ok(config_path)
            },
        ) {
            Ok(config_path) => config_path,
            Err(err) => {
                let _ = mark_agent_failed(state, &agent.id);
                return Err(err);
            }
        };
        let export_path = match bind_devin_export(state, &agent.id) {
            Ok(path) => path,
            Err(err) => {
                let _ = mark_agent_failed(state, &agent.id);
                return Err(err);
            }
        };
        let agent = attach_devin_agent_pane(
            state,
            &agent.id,
            request.pane_id.clone(),
            args_contain_prompt(&request.args),
        )?;

        let envs = devin_pane_envs(state, &request.pane_id, &agent.id)?;
        let agent_id = agent.id.clone();
        state.emit(QmuxEvent::new(
            "agent.spawned",
            Some(request.pane_id),
            Some(agent_id),
            json!({ "agent": agent.clone(), "source": "shell" }),
        ));

        Ok(PreparedShellAgentLaunch {
            binary,
            cwd: cwd_str,
            args: prepend_devin_managed_flags(&config_path, &export_path, request.args),
            envs: envs
                .into_iter()
                .map(|(key, value)| LaunchEnv { key, value })
                .collect(),
            supervised: true,
        })
    }

    fn ingest_devin_notification(
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
                    let session_id = super::string_field(&notification.payload, "session_id")
                        .or_else(|| super::string_field(&notification.payload, "sessionId"));
                    // Field-scoped mutation: attach_agent_pane may still be writing
                    // pane_id on another thread.
                    state.mutate_agent(&current.id, |agent| {
                        if let Some(session_id) = session_id {
                            agent.session_id = Some(session_id);
                        }
                    })?;
                }
                "agent.session_start"
            }
            "UserPromptSubmit" => {
                if let Some(agent) = agent.as_mut() {
                    let prompt = super::string_field(&notification.payload, "prompt")
                        .or_else(|| super::string_field(&notification.payload, "input"));
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
            "PostCompaction" => {
                if let Some(agent) = agent.as_mut() {
                    agent.status = AgentStatus::Running;
                    state.set_agent_status(&agent.id, agent.status)?;
                }
                "agent.compacted"
            }
            "SessionEnd" => "agent.session_end",
            "Stop" => {
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
        // advance_after_idle writes status without touching this snapshot, so
        // re-read before the UI upsert.
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

fn devin_pane_envs(
    state: &AppState,
    pane_id: &str,
    agent_id: &str,
) -> Result<Vec<(String, String)>, String> {
    let mut envs = agent_pane_envs(state, pane_id, agent_id)?;
    envs.push(("QMUX_ADAPTER_ID".to_string(), "devin".to_string()));
    Ok(envs)
}

fn attach_devin_agent_pane(
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

fn prepend_devin_managed_flags(
    config_path: &Path,
    export_path: &Path,
    args: Vec<String>,
) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len() + 4);
    out.push("--config".to_string());
    out.push(config_path.display().to_string());
    out.push("--export".to_string());
    out.push(export_path.display().to_string());
    out.extend(args);
    out
}

fn devin_export_path(config: &QmuxConfig, agent_id: &str) -> Result<PathBuf, String> {
    if agent_id.is_empty()
        || !agent_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(format!("invalid agent id for Devin export: {agent_id:?}"));
    }
    Ok(config
        .workspace_root
        .join(".qmux")
        .join("devin")
        .join(format!("{agent_id}.json")))
}

fn bind_devin_export(state: &AppState, agent_id: &str) -> Result<PathBuf, String> {
    let path = devin_export_path(state.config(), agent_id)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create Devin export directory {}: {err}",
                parent.display()
            )
        })?;
    }
    let path_str = path.display().to_string();
    state
        .mutate_agent(agent_id, |agent| {
            agent.transcript_path = Some(path_str.clone());
        })?
        .ok_or_else(|| format!("agent {agent_id} disappeared while binding Devin export"))?;
    start_transcript_tail(
        state.clone(),
        agent_id.to_string(),
        path_str,
        "devin".to_string(),
    );
    Ok(path)
}

fn resolve_devin_transcript_turns(
    agent_id: &str,
    source_index_offset: usize,
    lines: &[String],
) -> Vec<Turn> {
    let raw = lines.join("\n");
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let session_id = super::string_field(&value, "session_id")
        .or_else(|| super::string_field(&value, "sessionId"));
    let Some(steps) = value.get("steps").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut turns = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let source_index = source_index_offset + index;
        let Some(turn) = turn_from_atif_step(agent_id, session_id.as_deref(), source_index, step)
        else {
            continue;
        };
        turns.push(turn);
    }
    turns
}

fn turn_from_atif_step(
    agent_id: &str,
    session_id: Option<&str>,
    source_index: usize,
    step: &Value,
) -> Option<Turn> {
    let source = step.get("source").and_then(Value::as_str)?;
    let timestamp = step
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(rfc3339_to_epoch_ms);
    let step_id = step
        .get("step_id")
        .and_then(|value| {
            value
                .as_i64()
                .map(|id| id.to_string())
                .or_else(|| value.as_u64().map(|id| id.to_string()))
                .or_else(|| value.as_str().map(ToString::to_string))
        })
        .filter(|id| !id.is_empty());
    let (role, blocks) = match source {
        "user" => {
            let message = step.get("message").and_then(Value::as_str).unwrap_or("");
            if message.is_empty() {
                return None;
            }
            (
                "user",
                vec![TurnBlock::Text {
                    text: message.to_string(),
                }],
            )
        }
        "agent" => {
            let mut blocks = Vec::new();
            if let Some(message) = step.get("message").and_then(Value::as_str)
                && !message.is_empty()
            {
                blocks.push(TurnBlock::Text {
                    text: message.to_string(),
                });
            }
            if let Some(calls) = step.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let name = super::string_field(call, "function_name")
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| "tool".to_string());
                    blocks.push(TurnBlock::ToolUse {
                        id: super::string_field(call, "tool_call_id"),
                        name,
                        input: call.get("arguments").cloned().unwrap_or(Value::Null),
                    });
                }
            }
            if let Some(results) = step
                .get("observation")
                .and_then(|observation| observation.get("results"))
                .and_then(Value::as_array)
            {
                for result in results {
                    let content = result.get("content").cloned().unwrap_or(Value::Null);
                    blocks.push(TurnBlock::ToolResult {
                        tool_use_id: super::string_field(result, "source_call_id"),
                        content,
                        is_error: false,
                    });
                }
            }
            if blocks.is_empty() {
                return None;
            }
            ("assistant", blocks)
        }
        _ => return None,
    };
    Some(Turn {
        id: format!(
            "{agent_id}-{}",
            step_id.clone().unwrap_or_else(|| source_index.to_string())
        ),
        agent_id: agent_id.to_string(),
        session_id: session_id.map(ToString::to_string),
        role: role.to_string(),
        blocks,
        source_index,
        timestamp,
        status: None,
        status_reason: None,
        context_status: None,
        native_id: step_id,
        parent_native_id: None,
        native_message_id: None,
    })
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

/// XDG-style path Devin actually uses (`~/.config/devin/config.json`), not
/// macOS Application Support. `--config` replaces this file rather than merging.
fn user_devin_config_path() -> Option<PathBuf> {
    if let Some(xdg) = env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("devin").join("config.json"));
    }
    env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("devin")
            .join("config.json")
    })
}

fn default_devin_config_document() -> Value {
    json!({
        "version": 1,
        "shell": { "setup_complete": true }
    })
}

fn load_user_devin_config() -> Result<Value, String> {
    let Some(path) = user_devin_config_path() else {
        return Ok(default_devin_config_document());
    };
    load_devin_config_document(&path)
}

fn load_devin_config_document(path: &Path) -> Result<Value, String> {
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|err| format!("failed to parse Devin config {}: {err}", path.display())),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(default_devin_config_document()),
        Err(err) => Err(format!(
            "failed to read Devin config {}: {err}",
            path.display()
        )),
    }
}

fn ensure_shell_setup_complete(document: &mut Value) {
    let Some(obj) = document.as_object_mut() else {
        return;
    };
    let shell = obj.entry("shell").or_insert_with(|| json!({}));
    if let Some(shell) = shell.as_object_mut() {
        shell.insert("setup_complete".to_string(), json!(true));
    }
}

fn apply_devin_hooks(document: &mut Value, qmux_cli: &Path) {
    ensure_shell_setup_complete(document);
    let Some(obj) = document.as_object_mut() else {
        return;
    };
    let mut hooks = serde_json::Map::new();
    for event in DEVIN_HOOK_EVENTS {
        hooks.insert(
            event.to_string(),
            json!([
                {
                    "matcher": "",
                    "hooks": [
                        {
                            "type": "command",
                            "command": format!("{} notify {}", shell_quote_path(qmux_cli), event)
                        }
                    ]
                }
            ]),
        );
    }
    obj.insert("hooks".to_string(), Value::Object(hooks));
}

fn hook_settings_nonce() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|err| format!("failed to generate Devin hook config nonce: {err}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Plans the per-spawn Devin `--config` file: a copy of the user's config with
/// qmux hooks injected. Declarative — nothing is written here.
fn hook_config_support_file(
    config: &QmuxConfig,
    pane_id: &str,
) -> Result<(PathBuf, SupportFile), String> {
    hook_config_support_file_from(config, pane_id, load_user_devin_config()?)
}

fn hook_config_support_file_from(
    config: &QmuxConfig,
    pane_id: &str,
    mut document: Value,
) -> Result<(PathBuf, SupportFile), String> {
    if pane_id.is_empty()
        || !pane_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(format!(
            "invalid pane id for Devin hook config: {pane_id:?}"
        ));
    }
    if !document.is_object() {
        return Err("Devin config must be a JSON object".to_string());
    }

    let hooks_dir = config.workspace_root.join(".qmux").join("hooks");
    let qmux_cli = crate::launch_path::qmux_cli_path()
        .map_err(|err| format!("failed to resolve qmux executable for Devin hooks: {err}"))?;
    apply_devin_hooks(&mut document, &qmux_cli);
    let raw = serde_json::to_string_pretty(&document)
        .map_err(|err| format!("failed to encode Devin hook config: {err}"))?;

    let config_path = hooks_dir.join(format!("devin-{pane_id}-{}.json", hook_settings_nonce()?));
    let support_file = SupportFile {
        root: hooks_dir,
        path: config_path.clone(),
        contents: raw,
        mode: 0o600,
        create_new: true,
        prune_prefix: Some(format!("devin-{pane_id}-")),
    };
    Ok((config_path, support_file))
}

fn prompt_has_initial_text(prompt: &str) -> bool {
    !prompt.trim().is_empty()
}

fn trimmed(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn canonicalize_permission_mode(mode: &str) -> Result<String, String> {
    let mode = mode.trim();
    if mode.is_empty() {
        return Ok(String::new());
    }
    if !DEVIN_PERMISSION_MODES.contains(&mode) {
        return Err(format!(
            "unsupported Devin permission mode '{mode}'; expected one of {}",
            DEVIN_PERMISSION_MODES.join(", ")
        ));
    }
    Ok(mode.to_string())
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DevinLaunchOptions {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    permission_mode: Option<String>,
}

impl DevinLaunchOptions {
    fn from_value(value: Value) -> Result<Self, String> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let options = serde_json::from_value::<Self>(value)
            .map_err(|err| format!("invalid Devin adapter options: {err}"))?;
        if let Some(mode) = trimmed(options.permission_mode.as_deref()) {
            canonicalize_permission_mode(mode)?;
        }
        Ok(options)
    }
}

fn build_devin_args(
    model: Option<&str>,
    permission_mode: Option<&str>,
    prompt: &str,
) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(mode) = trimmed(permission_mode) {
        args.push("--permission-mode".to_string());
        args.push(mode.to_string());
    }
    if let Some(model) = trimmed(model) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    let prompt = prompt.trim();
    if !prompt.is_empty() {
        args.push("--".to_string());
        args.push(prompt.to_string());
    }
    args
}

/// Resume args. `--model` is omitted: Devin ignores it on resume and uses the
/// session's saved model.
fn build_devin_resume_args(
    _model: Option<&str>,
    permission_mode: Option<&str>,
    session_id: Option<&str>,
) -> (Vec<String>, bool) {
    let Some(session_id) = trimmed(session_id) else {
        return (build_devin_args(None, permission_mode, ""), false);
    };
    let mut args = build_devin_args(None, permission_mode, "");
    args.push("--resume".to_string());
    args.push(session_id.to_string());
    (args, true)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DevinShellDisposition {
    Supervised,
    Passthrough,
}

fn devin_shell_disposition(args: &[String]) -> Result<DevinShellDisposition, String> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return Ok(DevinShellDisposition::Supervised);
        }
        if matches!(
            arg.as_str(),
            "--help" | "-h" | "--version" | "-V" | "-v" | "--print" | "-p"
        ) || arg.starts_with("--print=")
        {
            return Ok(DevinShellDisposition::Passthrough);
        }
        if devin_optional_value_flag(arg) {
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
        if devin_value_flag(arg) {
            index += 2;
            continue;
        }
        if arg.starts_with("--") && arg.contains('=') {
            index += 1;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        return if devin_management_command(arg) {
            Ok(DevinShellDisposition::Passthrough)
        } else {
            Ok(DevinShellDisposition::Supervised)
        };
    }
    Ok(DevinShellDisposition::Supervised)
}

fn validate_devin_supervised_args(args: &[String]) -> Result<(), String> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            break;
        }
        if let Some(reason) = rejected_devin_flag(arg) {
            return Err(format!("Devin flag {arg} is not supported: {reason}"));
        }
        if devin_optional_value_flag(arg) {
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
        if devin_value_flag(arg) {
            index += 2;
            continue;
        }
        index += 1;
    }
    Ok(())
}

fn rejected_devin_flag(arg: &str) -> Option<&'static str> {
    if arg == "--config" || arg.starts_with("--config=") {
        return Some("it replaces the user config file; qmux injects per-pane hook config itself");
    }
    if arg == "--export" || arg.starts_with("--export=") {
        return Some("qmux binds Devin's conversation export itself");
    }
    None
}

fn devin_management_command(arg: &str) -> bool {
    matches!(
        arg,
        "auth"
            | "mcp"
            | "models"
            | "rules"
            | "skills"
            | "plugins"
            | "cloud"
            | "list"
            | "ls"
            | "update"
            | "version"
            | "migrate"
            | "sandbox"
            | "setup"
            | "uninstall"
            | "acp"
            | "help"
    )
}

fn devin_value_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--model" | "--permission-mode" | "--config" | "--prompt-file"
    )
}

fn devin_optional_value_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--resume" | "-r" | "--export" | "--print" | "-p" | "--respect-workspace-trust"
    )
}

fn devin_resume_session_id(args: &[String]) -> Option<&str> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            break;
        }
        if arg == "--resume" || arg == "-r" {
            return args
                .get(index + 1)
                .map(String::as_str)
                .filter(|next| !next.is_empty() && !next.starts_with('-'));
        }
        if let Some(value) = arg.strip_prefix("--resume=") {
            return (!value.is_empty()).then_some(value);
        }
        if devin_value_flag(arg)
            || (devin_optional_value_flag(arg)
                && args
                    .get(index + 1)
                    .is_some_and(|value| !value.starts_with('-')))
        {
            index += 2;
        } else {
            index += 1;
        }
    }
    None
}

fn args_contain_prompt(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return args.get(index + 1).is_some_and(|value| !value.is_empty());
        }
        if arg == "--prompt-file" || arg.starts_with("--prompt-file=") {
            return true;
        }
        if devin_optional_value_flag(arg) {
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
        if devin_value_flag(arg) {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        if devin_management_command(arg) {
            return false;
        }
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AdapterConfigs, DevinAdapterConfig};
    use crate::state::AppState;
    use std::os::unix::fs::PermissionsExt;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    fn unique_test_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_config() -> QmuxConfig {
        QmuxConfig {
            remotes: Default::default(),
            workspace_root: PathBuf::from("/tmp/qmux-devin-tests"),
            socket_path: PathBuf::from("/tmp/qmux-devin-tests.sock"),
            adapters: AdapterConfigs {
                devin: DevinAdapterConfig {
                    binary: Some("devin".to_string()),
                },
                ..Default::default()
            },
            legacy_claude_binary: None,
            claude_plugin_dir: PathBuf::new(),
            opencode_plugin_dir: PathBuf::new(),
            pi_extension_dir: PathBuf::new(),
            cursor_plugin_dir: PathBuf::new(),
        }
    }

    fn test_state() -> AppState {
        AppState::new(test_config())
    }

    fn sample_agent() -> AgentInfo {
        AgentInfo {
            id: "agent-1".to_string(),
            group_id: "group-1".to_string(),
            adapter: "devin".to_string(),
            worktree_dir: "/tmp/qmux-devin-tests".to_string(),
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
        let outcome = DevinAdapter::new(state.config())
            .ingest_notification(state, notification)
            .unwrap();
        match outcome {
            AdapterNotificationOutcome::Event(event) => event,
        }
    }

    #[test]
    fn launch_args_delimit_the_prompt() {
        assert_eq!(
            build_devin_args(Some("swe-1-7"), Some("accept-edits"), " fix the tests "),
            args(&[
                "--permission-mode",
                "accept-edits",
                "--model",
                "swe-1-7",
                "--",
                "fix the tests",
            ])
        );
        assert_eq!(build_devin_args(None, None, ""), Vec::<String>::new());
        assert_eq!(
            build_devin_args(None, None, "--looks-like-a-flag"),
            args(&["--", "--looks-like-a-flag"])
        );
    }

    #[test]
    fn resume_args_omit_model_and_name_the_session() {
        let (resume_args, resumed) =
            build_devin_resume_args(Some("swe-1-7"), Some("smart"), Some("patch-bead"));
        assert!(resumed);
        assert_eq!(
            resume_args,
            args(&["--permission-mode", "smart", "--resume", "patch-bead"])
        );
        let (resume_args, resumed) = build_devin_resume_args(None, None, None);
        assert!(!resumed);
        assert!(resume_args.is_empty());
    }

    #[test]
    fn shell_resume_command_quotes_the_session() {
        let command = DevinAdapter::new(&test_config())
            .shell_resume_command("patch-bead")
            .expect("devin supports shell resume");
        assert_eq!(command, "devin --resume 'patch-bead'");
    }

    #[test]
    fn shell_utilities_pass_through() {
        for command in [
            args(&["auth", "login"]),
            args(&["mcp", "list"]),
            args(&["models", "list"]),
            args(&["plugins", "list"]),
            args(&["--help"]),
            args(&["-V"]),
            args(&["--print", "hi"]),
            args(&["-p", "hi"]),
            args(&["list"]),
            args(&["acp"]),
            args(&["--model", "swe-1-7", "--print", "hi"]),
        ] {
            assert_eq!(
                devin_shell_disposition(&command).unwrap(),
                DevinShellDisposition::Passthrough,
                "{command:?}"
            );
        }
    }

    #[test]
    fn interactive_invocations_are_supervised() {
        for command in [
            args(&[]),
            args(&["--"]),
            args(&["--", "hello"]),
            args(&["--model", "swe-1-7"]),
            args(&["--resume", "patch-bead"]),
            args(&["-c"]),
            args(&["--continue"]),
            args(&["--permission-mode", "smart", "--", "fix it"]),
        ] {
            assert_eq!(
                devin_shell_disposition(&command).unwrap(),
                DevinShellDisposition::Supervised,
                "{command:?}"
            );
        }
    }

    #[test]
    fn supervised_args_reject_config_and_export() {
        assert!(validate_devin_supervised_args(&args(&["--config", "/tmp/x.json"])).is_err());
        assert!(validate_devin_supervised_args(&args(&["--config=/tmp/x.json"])).is_err());
        assert!(validate_devin_supervised_args(&args(&["--export", "out.json"])).is_err());
        assert!(validate_devin_supervised_args(&args(&["--model", "swe-1-7"])).is_ok());
        assert!(validate_devin_supervised_args(&args(&["--", "--config", "x"])).is_ok());
    }

    #[test]
    fn resume_session_id_reads_flag_forms() {
        assert_eq!(
            devin_resume_session_id(&args(&["--resume", "patch-bead"])),
            Some("patch-bead")
        );
        assert_eq!(
            devin_resume_session_id(&args(&["-r", "patch-bead"])),
            Some("patch-bead")
        );
        assert_eq!(
            devin_resume_session_id(&args(&["--resume=patch-bead"])),
            Some("patch-bead")
        );
        assert_eq!(devin_resume_session_id(&args(&["-r"])), None);
        assert_eq!(
            devin_resume_session_id(&args(&["--model", "swe-1-7", "--", "resume"])),
            None
        );
    }

    #[test]
    fn prompt_detection_skips_flag_values() {
        assert!(!args_contain_prompt(&args(&["--resume", "patch-bead"])));
        assert!(!args_contain_prompt(&args(&["--model", "swe-1-7"])));
        assert!(args_contain_prompt(&args(&["--", "hello"])));
        assert!(args_contain_prompt(&args(&["hello"])));
        assert!(args_contain_prompt(&args(&["--prompt-file", "p.txt"])));
        assert!(!args_contain_prompt(&args(&["auth", "login"])));
    }

    #[test]
    fn permission_mode_is_validated() {
        DevinLaunchOptions::from_value(json!({ "permissionMode": "smart" })).unwrap();
        DevinLaunchOptions::from_value(json!({ "permissionMode": "accept-edits" })).unwrap();
        assert!(DevinLaunchOptions::from_value(json!({ "permissionMode": "yolo-now" })).is_err());
        assert!(DevinLaunchOptions::from_value(json!({ "unknown": true })).is_err());
    }

    #[test]
    fn prepend_managed_flags_put_config_and_export_first() {
        let args = prepend_devin_managed_flags(
            Path::new("/tmp/qmux-hooks/devin-pane.json"),
            Path::new("/tmp/qmux-devin/agent-1.json"),
            build_devin_args(Some("swe-1-7"), Some("smart"), "hello"),
        );
        assert_eq!(
            args,
            vec![
                "--config",
                "/tmp/qmux-hooks/devin-pane.json",
                "--export",
                "/tmp/qmux-devin/agent-1.json",
                "--permission-mode",
                "smart",
                "--model",
                "swe-1-7",
                "--",
                "hello",
            ]
        );
    }

    #[test]
    fn hook_config_preserves_user_settings_and_injects_notify() {
        let workspace_root = unique_test_dir("qmux-devin-hooks");
        let config = QmuxConfig {
            workspace_root: workspace_root.clone(),
            socket_path: workspace_root.join("qmux.sock"),
            ..test_config()
        };
        let user = json!({
            "version": 1,
            "devin": { "org_id": "org_test" },
            "theme_mode": "dark",
            "agent": { "model": "swe-1-7" },
            "permissions": { "allow": ["Exec(git)"] },
            "hooks": { "Stop": [] }
        });
        let (path, support_file) = hook_config_support_file_from(&config, "pane-1", user).unwrap();
        crate::pty::materialize_support_files(&[support_file]).unwrap();

        assert!(path.starts_with(workspace_root.join(".qmux/hooks")));
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("devin-pane-1-") && name.ends_with(".json"));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let document: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(document["devin"]["org_id"], "org_test");
        assert_eq!(document["theme_mode"], "dark");
        assert_eq!(document["agent"]["model"], "swe-1-7");
        assert_eq!(document["shell"]["setup_complete"], true);
        assert!(
            document["hooks"]["Stop"].as_array().unwrap()[0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("notify Stop")
        );
        for event in DEVIN_HOOK_EVENTS {
            let command = document["hooks"][event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap_or("");
            assert!(
                command.contains(&format!("notify {event}")),
                "{event} hook missing notify: {command}"
            );
        }
    }

    #[test]
    fn missing_user_config_still_marks_setup_complete() {
        let missing = unique_test_dir("qmux-devin-missing-config").join("config.json");
        let document = load_devin_config_document(&missing).unwrap();
        assert_eq!(document["shell"]["setup_complete"], true);
        assert_eq!(document["version"], 1);
    }

    #[test]
    fn invalid_pane_id_is_rejected() {
        let err = hook_config_support_file_from(&test_config(), "../pane", json!({})).unwrap_err();
        assert!(err.contains("invalid pane id"));
    }

    #[test]
    fn session_start_binds_session_id_without_running() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Starting;
        state.insert_agent(agent).unwrap();

        let event = ingest(
            &state,
            hook_for_agent(
                "SessionStart",
                "agent-1",
                json!({ "session_id": "patch-bead", "source": "startup" }),
            ),
        );

        assert_eq!(event.event_type, "agent.session_start");
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(agent.session_id.as_deref(), Some("patch-bead"));
        assert!(matches!(agent.status, AgentStatus::Starting));
    }

    #[test]
    fn sparse_session_start_keeps_existing_session_id() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.session_id = Some("patch-bead".to_string());
        state.insert_agent(agent).unwrap();

        ingest(
            &state,
            hook_for_agent("SessionStart", "agent-1", json!({ "source": "resume" })),
        );

        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(agent.session_id.as_deref(), Some("patch-bead"));
    }

    #[test]
    fn prompt_and_tool_hooks_mark_running() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Idle;
        state.insert_agent(agent).unwrap();

        let event = ingest(
            &state,
            hook_for_agent(
                "UserPromptSubmit",
                "agent-1",
                json!({ "prompt": "fix it", "session_id": "patch-bead" }),
            ),
        );
        assert_eq!(event.event_type, "agent.prompt_submitted");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        ));

        ingest(&state, hook_for_agent("PreToolUse", "agent-1", json!({})));
        ingest(&state, hook_for_agent("PostToolUse", "agent-1", json!({})));
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        ));
    }

    #[test]
    fn permission_request_marks_awaiting_permission() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Running;
        state.insert_agent(agent).unwrap();

        let event = ingest(
            &state,
            hook_for_agent(
                "PermissionRequest",
                "agent-1",
                json!({ "tool_name": "exec" }),
            ),
        );
        assert_eq!(event.event_type, "agent.awaiting_permission");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::AwaitingPermission
        ));
    }

    #[test]
    fn stop_marks_agent_done_without_queued_turns() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Running;
        state.insert_agent(agent).unwrap();

        let event = ingest(&state, hook_for_agent("Stop", "agent-1", json!({})));
        assert_eq!(event.event_type, "agent.done");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Done
        ));
    }

    #[test]
    fn unknown_hook_is_forwarded() {
        let state = test_state();
        state.insert_agent(sample_agent()).unwrap();
        let event = ingest(&state, hook_for_agent("MysteryEvent", "agent-1", json!({})));
        assert_eq!(event.event_type, "agent.hook.MysteryEvent");
    }

    #[test]
    fn export_path_is_scoped_to_the_agent() {
        let path = devin_export_path(&test_config(), "agent-1").unwrap();
        assert_eq!(
            path,
            PathBuf::from("/tmp/qmux-devin-tests/.qmux/devin/agent-1.json")
        );
        assert!(devin_export_path(&test_config(), "../agent").is_err());
    }

    #[test]
    fn atif_transcript_maps_user_and_agent_steps() {
        let document = json!({
            "schema_version": "ATIF-v1.7",
            "session_id": "patch-bead",
            "steps": [
                { "step_id": 0, "source": "system", "message": "sysprompt" },
                {
                    "step_id": 1,
                    "timestamp": "2026-08-15T08:00:00Z",
                    "source": "user",
                    "message": "hello"
                },
                {
                    "step_id": 2,
                    "source": "agent",
                    "message": "working",
                    "tool_calls": [{
                        "tool_call_id": "call-1",
                        "function_name": "exec",
                        "arguments": { "command": "ls" }
                    }],
                    "observation": {
                        "results": [{
                            "source_call_id": "call-1",
                            "content": "ok"
                        }]
                    }
                },
                { "step_id": 3, "source": "agent", "message": "" }
            ]
        });
        let pretty = serde_json::to_string_pretty(&document).unwrap();
        let lines: Vec<String> = pretty.lines().map(ToString::to_string).collect();
        let turns = resolve_devin_transcript_turns("agent-1", 0, &lines);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].session_id.as_deref(), Some("patch-bead"));
        assert_eq!(turns[0].id, "agent-1-1");
        assert_eq!(
            turns[0].blocks,
            vec![TurnBlock::Text {
                text: "hello".to_string()
            }]
        );
        assert_eq!(turns[1].role, "assistant");
        assert_eq!(turns[1].blocks.len(), 3);
        assert_eq!(
            turns[1].blocks[1],
            TurnBlock::ToolUse {
                id: Some("call-1".to_string()),
                name: "exec".to_string(),
                input: json!({ "command": "ls" }),
            }
        );
        assert_eq!(
            turns[1].blocks[2],
            TurnBlock::ToolResult {
                tool_use_id: Some("call-1".to_string()),
                content: json!("ok"),
                is_error: false,
            }
        );
        assert!(resolve_devin_transcript_turns("agent-1", 0, &["{".to_string()]).is_empty());

        let compact = serde_json::to_string(&document).unwrap();
        assert!(
            !compact.ends_with('\n'),
            "ATIF compact JSON should match Devin's missing trailing newline"
        );
        let from_blob = resolve_devin_transcript_turns("agent-1", 0, &[compact]);
        assert_eq!(from_blob.len(), 2);
        assert_eq!(from_blob[0].role, "user");
    }
}
