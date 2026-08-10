//! The Agent Client Protocol adapter.
//!
//! Every other adapter in this module is written against one vendor's CLI. ACP
//! is a wire protocol — JSON-RPC over stdio, the LSP of coding agents — so this
//! adapter is written against the protocol instead, and a new agent is a
//! `adapters.acp.agents` entry in `qmux.config.json` rather than new Rust.
//!
//! The process qmux actually spawns in the pane is `qmux acp`, the bridge in
//! the `qmux-cli` crate. ACP agents have no TUI of their own: the client owns
//! rendering, the filesystem, permissions, and terminals. The bridge supplies
//! all four, writes the JSONL transcript parsed below, and posts the same
//! lifecycle hooks the OpenCode plugin does, so status tracking, the follow-up
//! composer, and session recovery all work unchanged.
//!
//! Scope note: this adapter deliberately has no shell-command integration.
//! `ShellCommandIntegration` and `AgentAdapter::id` are `&'static str`, so one
//! registry entry per configured agent would mean leaking a string per
//! `adapter_registry` call — and that runs on every transcript tail. A single
//! `acp` adapter that resolves the concrete agent at launch avoids the leak;
//! typing an ACP agent's name in a shell pane simply isn't intercepted.

use super::{
    AdapterNotification, AdapterNotificationOutcome, AgentAdapter, ComposerPolicy,
    PrepareShellAgentLaunchRequest, PreparedShellAgentLaunch, ShellCommandIntegration,
    SpawnAgentRequest, TranscriptLifecycleEvent, ensure_on_path,
};
use crate::config::{AcpAdapterConfig, AcpAgentConfig, QmuxConfig};
use crate::events::QmuxEvent;
use crate::host::Host;
use crate::pty::{
    CommandPlan, InitialPaneSize, PaneMeta, agent_pane_envs, plan_to_spec, recoverable_dir,
    spawn_pty,
};
use crate::state::{AppState, PaneInfo, PaneKind};
use crate::transcript::{Turn, TurnBlock, start_transcript_tail, string_field};
use crate::turn_queue::{IdleResolution, advance_after_idle};
use crate::workspace::{
    AcpConfigOption, AgentInfo, AgentStatus, PrepareAgentWorkspaceRequest, attach_agent_pane,
    mark_agent_failed, mark_agent_spawn_failed, prepare_agent_workspace_with_parent,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct AcpAdapter {
    config: AcpAdapterConfig,
}

impl AcpAdapter {
    pub fn new(config: &QmuxConfig) -> Self {
        Self {
            config: config.adapters.acp.clone(),
        }
    }

    /// The transcript is keyed by qmux agent id, not ACP session id: the bridge
    /// needs the path before `session/new` has answered, and keying it this way
    /// also lets a resumed pane keep appending to one file.
    fn transcript_path_for(state: &AppState, agent_id: &str) -> PathBuf {
        state
            .config()
            .workspace_root
            .join(".qmux")
            .join("acp")
            .join(agent_id)
            .join("session.jsonl")
    }

    /// Everything `qmux acp` reads out of its environment. Kept in one place so
    /// launch, resume, and any future fork path can't drift apart.
    fn bridge_envs(
        agent_key: &str,
        agent: &AcpAgentConfig,
        binary: &str,
        cwd: &str,
        transcript: Option<&Path>,
        prompt: Option<&str>,
        load_session: Option<&str>,
        auth_method: Option<&str>,
    ) -> Result<Vec<(String, String)>, String> {
        let mut envs = vec![
            ("QMUX_ACP_AGENT".to_string(), agent_key.to_string()),
            (
                "QMUX_ACP_NAME".to_string(),
                agent.name.clone().unwrap_or_else(|| agent_key.to_string()),
            ),
            ("QMUX_ACP_COMMAND".to_string(), binary.to_string()),
            (
                "QMUX_ACP_ARGS".to_string(),
                serde_json::to_string(&agent.args)
                    .map_err(|err| format!("failed to encode ACP agent args: {err}"))?,
            ),
            ("QMUX_ACP_CWD".to_string(), cwd.to_string()),
        ];
        match transcript {
            // Local: the bridge writes the file the sidebar tails.
            Some(transcript) => envs.push((
                "QMUX_ACP_TRANSCRIPT".to_string(),
                transcript.display().to_string(),
            )),
            // Remote: it cannot see that filesystem, so records are streamed
            // back over the control socket. Where the agent's stderr lands is
            // left to the bridge — it is the one running on that machine, so
            // it can put the log under the user's own home. Naming a path
            // here would have to be a guess, and the obvious guess (a fixed
            // name in /tmp) is both shared between concurrent panes and a
            // symlink anyone else on that host can plant.
            None => envs.push(("QMUX_ACP_TRANSCRIPT_STREAM".to_string(), "1".to_string())),
        }
        if !agent.env.is_empty() {
            envs.push((
                "QMUX_ACP_ENV".to_string(),
                serde_json::to_string(&agent.env)
                    .map_err(|err| format!("failed to encode ACP agent env: {err}"))?,
            ));
        }
        if let Some(prompt) = prompt.map(str::trim).filter(|prompt| !prompt.is_empty()) {
            envs.push(("QMUX_ACP_PROMPT".to_string(), prompt.to_string()));
        }
        if let Some(session_id) = load_session
            .map(str::trim)
            .filter(|session_id| !session_id.is_empty())
        {
            envs.push(("QMUX_ACP_LOAD_SESSION".to_string(), session_id.to_string()));
        }
        if let Some(method) = auth_method
            .map(str::trim)
            .filter(|method| !method.is_empty())
        {
            envs.push(("QMUX_ACP_AUTH_METHOD".to_string(), method.to_string()));
        }
        Ok(envs)
    }

    /// Last successful auth method for this agent key, if any — the bridge
    /// tries it silently before prompting so a signed-in agent is not re-asked.
    fn preferred_auth_method(state: &AppState, agent_key: &str) -> Option<String> {
        crate::persistence::load_preferences(&state.config().workspace_root)
            .ok()?
            .acp_auth_method_by_agent
            .get(agent_key)
            .map(|method| method.trim().to_string())
            .filter(|method| !method.is_empty())
    }

    fn remember_auth_method(state: &AppState, agent_key: &str, method_id: &str) {
        let method_id = method_id.trim();
        if agent_key.is_empty() || method_id.is_empty() {
            return;
        }
        let root = state.config().workspace_root.clone();
        let agent_key = agent_key.to_string();
        let method_id = method_id.to_string();
        let _ = crate::persistence::update_preferences(&root, |preferences| {
            preferences
                .acp_auth_method_by_agent
                .insert(agent_key, method_id);
        });
    }

    /// Where the bridge should record turns, given where it will run.
    ///
    /// A remote bridge must be told *nothing*: handing it this path would have
    /// it faithfully write a transcript on a filesystem the sidebar cannot see,
    /// and the pane would look like it had no history. `None` is what switches
    /// it to streaming records back over the control socket instead.
    fn transcript_sink<'a>(host: &Host, transcript: &'a Path) -> Option<&'a Path> {
        host.is_local().then_some(transcript)
    }

    /// The host a launch runs on, taken from its group's remote binding.
    ///
    /// A launch with no group is creating one, which is always local — a remote
    /// group has to be created deliberately.
    fn group_host(state: &AppState, group_id: Option<&str>) -> Result<Host, String> {
        let Some(group_id) = group_id else {
            return Ok(Host::Local);
        };
        let group = state
            .group(group_id)?
            .ok_or_else(|| format!("group {group_id} was not found"))?;
        Ok(crate::host::for_group(group.remote.as_ref()))
    }

    /// Resolves the agent to launch — from `qmux.config.json` or the registry
    /// store — and checks its binary exists before any workspace is created, so
    /// a typo fails as a clean error rather than an empty pane with a dead
    /// process in it.
    fn resolve(
        &self,
        state: &AppState,
        host: &Host,
        requested: Option<&str>,
    ) -> Result<(String, AcpAgentConfig, String), String> {
        let installed = crate::acp_registry::installed_configs(&state.config().workspace_root)?;
        // Prefer the explicit launch choice, then config's defaultAgent, then
        // the settings pin — never write either back into the other.
        let effective = {
            let mut config = self.config.clone();
            if config.default_agent.is_none() {
                config.default_agent = state.config().acp_preferred_default_agent();
            }
            config
        };
        let (key, agent) = effective.resolve_with(&installed, requested)?;
        // Only a local agent's binary can be checked here. A remote one is
        // resolved by the remote shell's PATH, and probing it over ssh would
        // cost a round trip to produce an error the launch reports anyway.
        let binary = match host {
            Host::Local => ensure_on_path(&agent.command)
                .ok_or_else(|| {
                    format!(
                        "ACP agent '{key}' runs '{}', which was not found on PATH or standard macOS tool paths.",
                        agent.command
                    )
                })?
                .display()
                .to_string(),
            Host::Remote { .. } => agent.command.clone(),
        };
        Ok((key, agent, binary))
    }

    fn spawn_pane(&self, state: &AppState, request: SpawnAgentRequest) -> Result<PaneInfo, String> {
        let options = AcpLaunchOptions::from_value(request.options)?;
        let host = Self::group_host(state, request.group_id.as_deref())?;
        let (agent_key, acp_agent, binary) =
            self.resolve(state, &host, options.agent.as_deref())?;
        let bridge = crate::launch_path::qmux_cli_path()?;

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
            .clone()
            .unwrap_or_else(|| agent.worktree_dir.clone());
        // Only meaningful for a local agent: a remote path says nothing about
        // this filesystem, and the launch reports a missing directory anyway.
        if host.is_local() && !Path::new(&cwd).is_dir() {
            let _ = mark_agent_failed(state, &agent.id);
            return Err(format!("ACP working directory {cwd} does not exist"));
        }

        // The transcript always lives here — a remote bridge streams its
        // records back rather than writing them where nothing would read them.
        let transcript = Self::transcript_path_for(state, &agent.id);
        let has_initial_prompt = !request.prompt.trim().is_empty();

        let pane_id = state.next_id("pane");
        let mut envs = agent_pane_envs(state, &pane_id, &agent.id)?;
        let preferred_auth = Self::preferred_auth_method(state, &agent_key);
        envs.extend(Self::bridge_envs(
            &agent_key,
            &acp_agent,
            &binary,
            &cwd,
            Self::transcript_sink(&host, &transcript),
            has_initial_prompt.then_some(request.prompt.as_str()),
            None,
            preferred_auth.as_deref(),
        )?);

        let agent = attach_acp_agent_pane(state, &agent.id, pane_id.clone(), has_initial_prompt)?;
        bind_transcript(state, &agent, &agent_key, &transcript, self.id())?;

        let spawn_result = plan_to_spec(
            state,
            PaneMeta {
                pane_id: Some(pane_id.clone()),
                agent_id: Some(agent.id.clone()),
                group_id: agent.group_id.clone(),
                kind: PaneKind::Agent,
                title: acp_agent.name.clone().unwrap_or(agent_key),
                last_osc_title: None,
                initial_size: request.initial_size,
                recovered: false,
            },
            CommandPlan {
                program: bridge.display().to_string(),
                args: vec!["acp".to_string()],
                cwd: PathBuf::from(&cwd),
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
        let host = Self::group_host(state, Some(&agent.group_id))?;
        let (agent_key, acp_agent, binary) =
            self.resolve(state, &host, agent.acp_agent.as_deref())?;
        let bridge = crate::launch_path::qmux_cli_path()?;
        // A worktree on another machine cannot be stat'd from here. Recovery
        // still has to check the local case, where a deleted directory is the
        // common reason a respawn would otherwise fail confusingly.
        let cwd = if host.is_local() {
            recoverable_dir(&agent.worktree_dir)
                .ok_or_else(|| {
                    format!(
                        "agent worktree {} no longer exists; relaunch manually",
                        agent.worktree_dir
                    )
                })?
                .display()
                .to_string()
        } else {
            agent.worktree_dir.clone()
        };

        let transcript = Self::transcript_path_for(state, &agent.id);
        let mut envs = agent_pane_envs(state, &pane.id, &agent.id)?;
        let preferred_auth = Self::preferred_auth_method(state, &agent_key);
        envs.extend(Self::bridge_envs(
            &agent_key,
            &acp_agent,
            &binary,
            &cwd,
            Self::transcript_sink(&host, &transcript),
            None,
            agent.session_id.as_deref(),
            preferred_auth.as_deref(),
        )?);

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
                program: bridge.display().to_string(),
                args: vec!["acp".to_string()],
                cwd: PathBuf::from(&cwd),
                envs,
                support_files: Vec::new(),
                support_file_fallback: None,
            },
        )?;
        let info = spawn_pty(state, spec)?;

        // A recovered bridge starts without a prompt, so it is idle as soon as
        // it is up; the first hook promotes it.
        let mut restored = agent.clone();
        restored.pane_id = Some(pane.id.clone());
        restored.status = AgentStatus::Idle;
        state.update_agent(restored.clone())?;
        bind_transcript(state, &restored, &agent_key, &transcript, self.id())?;

        // `session/load` is an optional ACP capability, so a resume is a request
        // rather than a promise; the bridge falls back to a fresh session and
        // says so in the pane.
        state.emit(QmuxEvent::new(
            "agent.recovered",
            Some(pane.id.clone()),
            Some(restored.id.clone()),
            json!({ "resumed": restored.session_id.is_some(), "agent": restored }),
        ));

        Ok(info)
    }

    fn ingest_acp_notification(
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
                        .or_else(|| string_field(&notification.payload, "sessionId"));
                    // Field-scoped mutation rather than a full-struct write: the
                    // pane binding is being set on another thread and a stale
                    // snapshot here would clobber it.
                    state.mutate_agent(&current.id, |agent| {
                        if let Some(session_id) = session_id.clone() {
                            agent.session_id = Some(session_id);
                        }
                    })?;
                }
                "agent.session_start"
            }
            "ConfigOptions" => {
                if let Some(current) = agent.as_ref() {
                    let options = parse_config_options(&notification.payload);
                    let model = category_label(&options, "model");
                    let effort = category_label(&options, "thought_level");
                    state.mutate_agent(&current.id, |agent| {
                        // The agent owns this state; a later push always
                        // replaces what came before rather than merging.
                        agent.acp_config_options = options.clone();
                        // `model` and `thought_level` have somewhere to go
                        // already, so an ACP pane gets the same header as every
                        // other adapter for free. Only overwrite when the agent
                        // actually exposes the category — a agent without a
                        // model selector shouldn't blank a model set at launch.
                        if let Some(model) = model.clone() {
                            agent.model = Some(model);
                        }
                        if let Some(effort) = effort.clone() {
                            agent.effort = Some(effort);
                        }
                    })?;
                }
                "agent.config_options"
            }
            "UserPromptSubmit" => {
                if let Some(agent) = agent.as_mut() {
                    let prompt = string_field(&notification.payload, "prompt");
                    agent.status = AgentStatus::Running;
                    state.set_agent_status(&agent.id, agent.status)?;
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
            // First-run / re-auth: the bridge is between initialize and
            // session/new. Stay on AwaitingPermission so the composer queues
            // rather than sending into the auth prompt; the UI card reads the
            // method list from the event payload.
            "AuthRequired" => {
                if let Some(agent) = agent.as_mut() {
                    agent.status = AgentStatus::AwaitingPermission;
                    state.set_agent_status(&agent.id, agent.status)?;
                }
                "agent.auth_required"
            }
            "AuthSucceeded" => {
                if let Some(current) = agent.as_ref() {
                    let method_id = string_field(&notification.payload, "methodId")
                        .or_else(|| string_field(&notification.payload, "method_id"));
                    let agent_key = current
                        .acp_agent
                        .clone()
                        .or_else(|| string_field(&notification.payload, "agent"));
                    if let (Some(agent_key), Some(method_id)) = (agent_key, method_id) {
                        Self::remember_auth_method(state, &agent_key, &method_id);
                    }
                    // Auth is pre-session; Starting is honest until SessionStart.
                    state.set_agent_status(&current.id, AgentStatus::Starting)?;
                }
                "agent.auth_succeeded"
            }
            "AuthFailed" => {
                if let Some(agent) = agent.as_mut() {
                    agent.status = AgentStatus::AwaitingPermission;
                    state.set_agent_status(&agent.id, agent.status)?;
                }
                "agent.auth_failed"
            }
            "Stop" | "StopFailure" => {
                let drained = if let Some(agent) = agent.as_mut() {
                    match advance_after_idle(state, &agent.id) {
                        Ok(IdleResolution::Drained) => true,
                        Ok(IdleResolution::Paused | IdleResolution::Idle) => false,
                        Err(err) => {
                            state.emit(QmuxEvent::new(
                                "agent.queue_error",
                                agent.pane_id.clone(),
                                Some(agent.id.clone()),
                                json!({ "error": err }),
                            ));
                            false
                        }
                    }
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
                    json!({ "hookEvent": hook_event, "payload": notification.payload }),
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
        // Re-read: the idle handler writes status straight to the store without
        // touching the local snapshot, so attaching `agent` as-is would ship a
        // stale copy and hide the change from the UI.
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

impl AgentAdapter for AcpAdapter {
    fn id(&self) -> &'static str {
        "acp"
    }

    fn display_name(&self) -> &'static str {
        "ACP"
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
        _state: &AppState,
        _request: PrepareShellAgentLaunchRequest,
    ) -> Result<PreparedShellAgentLaunch, String> {
        Err(
            "ACP agents are launched from qmux rather than by name in a shell pane; use the agent picker"
                .to_string(),
        )
    }

    fn shell_commands(&self) -> Vec<ShellCommandIntegration> {
        Vec::new()
    }

    /// The bridge resolves the agent binary on the remote's own `PATH`, carries
    /// no locally-materialized files, and chdirs itself from `QMUX_ACP_CWD`
    /// rather than inheriting the pane's directory.
    fn supports_remote(&self) -> bool {
        true
    }

    fn ingest_notification(
        &self,
        state: &AppState,
        notification: AdapterNotification,
    ) -> Result<AdapterNotificationOutcome, String> {
        self.ingest_acp_notification(state, notification)
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
            // ACP has no mid-turn steer: `session/prompt` is one request per
            // turn and the only in-flight control is `session/cancel`. Queueing
            // is honest here; pretending to steer would drop the text into the
            // bridge's stdin mid-turn and surface it as the *next* prompt.
            steer_statuses: Vec::new(),
            permission_actions: Vec::new(),
        }
    }
}

fn attach_acp_agent_pane(
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

/// Records where the bridge writes and which configured agent it is running,
/// then starts the tail.
///
/// The vendor adapters learn their transcript path from a session-start hook,
/// because the CLI chooses it. Here qmux chooses it and passes it in, so it can
/// be bound before the process is even spawned and the sidebar is live from the
/// first line. `acp_agent` rides along in the same field-scoped mutation: a
/// full-struct write would race the pane binding happening on another thread.
fn bind_transcript(
    state: &AppState,
    agent: &AgentInfo,
    agent_key: &str,
    transcript: &Path,
    adapter_id: &str,
) -> Result<(), String> {
    let transcript = transcript.display().to_string();
    let agent_key = agent_key.to_string();
    state.mutate_agent(&agent.id, |agent| {
        agent.transcript_path = Some(transcript.clone());
        agent.acp_agent = Some(agent_key.clone());
    })?;
    start_transcript_tail(
        state.clone(),
        agent.id.clone(),
        transcript,
        adapter_id.to_string(),
    );
    Ok(())
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AcpLaunchOptions {
    /// Key into `adapters.acp.agents`. Absent means "use the default".
    #[serde(default)]
    agent: Option<String>,
}

impl AcpLaunchOptions {
    fn from_value(value: Value) -> Result<Self, String> {
        if value.is_null() {
            return Ok(Self::default());
        }
        serde_json::from_value(value).map_err(|err| format!("invalid ACP adapter options: {err}"))
    }
}

/// Parses a line written by the `qmux acp` bridge.
///
/// The bridge owns this format — ACP itself has no on-disk transcript — so
/// `blocks` is already in `TurnBlock`'s serde shape and deserializes directly:
/// ```json
/// {"type":"turn","role":"assistant","sessionId":"…","blocks":[{"type":"text","text":"…"}]}
/// ```
fn parse_transcript_line(agent_id: &str, source_index: usize, line: &str) -> Option<Turn> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("turn") {
        return None;
    }
    let role = value.get("role").and_then(Value::as_str)?;
    let blocks: Vec<TurnBlock> = serde_json::from_value(value.get("blocks")?.clone()).ok()?;
    if blocks.is_empty() {
        return None;
    }
    let native_id = string_field(&value, "nativeId");

    Some(Turn {
        id: format!("{agent_id}-{source_index}"),
        agent_id: agent_id.to_string(),
        session_id: string_field(&value, "sessionId"),
        role: role.to_string(),
        blocks,
        source_index,
        timestamp: super::native_timestamp_ms(&value),
        status: None,
        status_reason: None,
        context_status: None,
        native_id: native_id.clone(),
        parent_native_id: None,
        native_message_id: native_id,
    })
}

/// Reads the `configOptions` array out of a `ConfigOptions` hook payload.
///
/// Malformed entries are skipped rather than failing the batch: ACP is
/// explicitly extensible, and one option shaped in a way qmux doesn't
/// understand must not cost the user the rest of their model picker.
fn parse_config_options(payload: &Value) -> Vec<AcpConfigOption> {
    payload
        .get("configOptions")
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|option| serde_json::from_value(option.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The display label of the first option in `category`, if the agent exposes
/// one. Categories are unique in practice but the spec does not require it, so
/// first-wins rather than asserting.
fn category_label(options: &[AcpConfigOption], category: &str) -> Option<String> {
    options
        .iter()
        .find(|option| option.category.as_deref() == Some(category))
        .and_then(AcpConfigOption::current_label)
        .map(|label| label.trim().to_string())
        .filter(|label| !label.is_empty())
}

fn parse_transcript_lifecycle_event(line: &str) -> Option<TranscriptLifecycleEvent> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("lifecycle") {
        return None;
    }
    match value.get("event").and_then(Value::as_str)? {
        "interrupted" => Some(TranscriptLifecycleEvent::Interrupted),
        "turnStarted" => Some(TranscriptLifecycleEvent::TurnStarted),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AcpAgentConfig;
    use std::collections::BTreeMap;

    fn agent(command: &str) -> AcpAgentConfig {
        AcpAgentConfig {
            name: None,
            command: command.to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    fn config(agents: &[(&str, &str)], default: Option<&str>) -> AcpAdapterConfig {
        AcpAdapterConfig {
            default_agent: default.map(str::to_string),
            agents: agents
                .iter()
                .map(|(key, command)| (key.to_string(), agent(command)))
                .collect(),
        }
    }

    #[test]
    fn a_lone_configured_agent_needs_no_default() {
        let (key, resolved) = config(&[("gemini", "gemini")], None)
            .resolve_with(&BTreeMap::new(), None)
            .expect("the only agent is unambiguous");
        assert_eq!(key, "gemini");
        assert_eq!(resolved.command, "gemini");
    }

    #[test]
    fn several_agents_without_a_default_require_an_explicit_choice() {
        let config = config(&[("gemini", "gemini"), ("goose", "goose")], None);
        let err = config
            .resolve_with(&BTreeMap::new(), None)
            .expect_err("ambiguous");
        assert!(err.contains("defaultAgent"), "{err}");
        assert!(err.contains("gemini") && err.contains("goose"), "{err}");

        assert_eq!(
            config
                .resolve_with(&BTreeMap::new(), Some("goose"))
                .unwrap()
                .0,
            "goose"
        );
        assert_eq!(
            config
                .resolve_with(&BTreeMap::new(), None.or(Some("gemini")))
                .unwrap()
                .0,
            "gemini"
        );
    }

    #[test]
    fn an_explicit_choice_outranks_the_default() {
        let config = config(&[("gemini", "gemini"), ("goose", "goose")], Some("gemini"));
        assert_eq!(
            config.resolve_with(&BTreeMap::new(), None).unwrap().0,
            "gemini"
        );
        assert_eq!(
            config
                .resolve_with(&BTreeMap::new(), Some("goose"))
                .unwrap()
                .0,
            "goose"
        );
    }

    #[test]
    fn unknown_and_unconfigured_agents_report_what_is_available() {
        let err = AcpAdapterConfig::default()
            .resolve_with(&BTreeMap::new(), None)
            .expect_err("nothing configured");
        assert!(err.contains("adapters.acp.agents"), "{err}");

        let err = config(&[("gemini", "gemini")], None)
            .resolve_with(&BTreeMap::new(), Some("cline"))
            .expect_err("unknown agent");
        assert!(err.contains("cline") && err.contains("gemini"), "{err}");
    }

    #[test]
    fn registry_agents_are_launchable_alongside_configured_ones() {
        let config = config(&[("gemini", "gemini")], Some("gemini"));
        let installed = BTreeMap::from([("cline".to_string(), agent("npx"))]);

        assert_eq!(
            config
                .resolve_with(&installed, Some("cline"))
                .unwrap()
                .1
                .command,
            "npx"
        );
        // The configured default still wins when nothing is asked for.
        assert_eq!(config.resolve_with(&installed, None).unwrap().0, "gemini");
        assert_eq!(config.merged_agents(&installed).len(), 2);
    }

    #[test]
    fn a_hand_written_entry_outranks_a_registry_one_with_the_same_id() {
        // Config is the thing someone edited on purpose; a registry refresh
        // must not quietly redirect it somewhere else.
        let config = config(&[("cline", "/my/patched/cline")], None);
        let installed = BTreeMap::from([("cline".to_string(), agent("npx"))]);
        assert_eq!(
            config
                .resolve_with(&installed, Some("cline"))
                .unwrap()
                .1
                .command,
            "/my/patched/cline"
        );
    }

    #[test]
    fn a_lone_registry_agent_needs_no_default_either() {
        let installed = BTreeMap::from([("cline".to_string(), agent("npx"))]);
        assert_eq!(
            AcpAdapterConfig::default()
                .resolve_with(&installed, None)
                .unwrap()
                .0,
            "cline"
        );
    }

    #[test]
    fn errors_list_registry_agents_too_so_the_message_is_actionable() {
        let installed = BTreeMap::from([("cline".to_string(), agent("npx"))]);
        let err = config(&[("gemini", "gemini")], None)
            .resolve_with(&installed, Some("nope"))
            .expect_err("unknown agent");
        assert!(err.contains("cline") && err.contains("gemini"), "{err}");
    }

    #[test]
    fn an_empty_command_is_rejected_rather_than_spawned() {
        let err = config(&[("broken", "")], None)
            .resolve_with(&BTreeMap::new(), None)
            .expect_err("empty command");
        assert!(err.contains("empty command"), "{err}");
    }

    #[test]
    fn transcript_lines_round_trip_through_the_bridge_format() {
        let line = json!({
            "type": "turn",
            "role": "assistant",
            "sessionId": "sess_1",
            "nativeId": "msg_7",
            "timestamp": 1_700_000_000_000i64,
            "blocks": [{ "type": "text", "text": "hello" }],
        })
        .to_string();

        let turn = parse_transcript_line("agent-1", 4, &line).expect("a turn");
        assert_eq!(turn.id, "agent-1-4");
        assert_eq!(turn.role, "assistant");
        assert_eq!(turn.session_id.as_deref(), Some("sess_1"));
        assert_eq!(turn.native_id.as_deref(), Some("msg_7"));
        assert_eq!(turn.timestamp, Some(1_700_000_000_000));
        assert_eq!(
            turn.blocks,
            vec![TurnBlock::Text {
                text: "hello".to_string()
            }]
        );
    }

    #[test]
    fn tool_use_and_tool_result_blocks_survive_the_round_trip() {
        let line = json!({
            "type": "turn",
            "role": "assistant",
            "blocks": [{
                "type": "toolUse",
                "id": "call_1",
                "name": "Read file",
                "input": { "path": "/tmp/x" },
            }],
        })
        .to_string();
        let turn = parse_transcript_line("agent-1", 0, &line).expect("a turn");
        assert_eq!(
            turn.blocks,
            vec![TurnBlock::ToolUse {
                id: Some("call_1".to_string()),
                name: "Read file".to_string(),
                input: json!({ "path": "/tmp/x" }),
            }]
        );

        // The bridge writes these lines from a crate that cannot import
        // `TurnBlock`, so this pins the exact spelling it has to produce.
        let line = json!({
            "type": "turn",
            "role": "assistant",
            "blocks": [{
                "type": "toolResult",
                "toolUseId": "call_1",
                "content": [{ "type": "content", "content": { "type": "text", "text": "ok" } }],
                "isError": true,
            }],
        })
        .to_string();
        let turn = parse_transcript_line("agent-1", 1, &line).expect("a turn");
        match &turn.blocks[0] {
            TurnBlock::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id.as_deref(), Some("call_1"));
                assert!(is_error);
            }
            other => panic!("expected a tool result, got {other:?}"),
        }
    }

    /// Captured verbatim from a real `qmux acp` run against an ACP agent. The
    /// bridge lives in another crate and hand-writes these lines, so nothing but
    /// a fixture proves the two halves still agree.
    const CAPTURED_SESSION: &str = concat!(
        r#"{"blocks":[{"text":"hello acp","type":"text"}],"nativeId":null,"role":"user","sessionId":"sess_fake_1","timestamp":1786084826810,"type":"turn"}"#,
        "\n",
        r#"{"blocks":[{"text":"You said: hello acp","type":"text"}],"nativeId":"m1","role":"assistant","sessionId":"sess_fake_1","timestamp":1786084826811,"type":"turn"}"#,
        "\n",
        r#"{"blocks":[{"id":"c1","input":null,"name":"Run a command","type":"toolUse"}],"nativeId":"c1","role":"assistant","sessionId":"sess_fake_1","timestamp":1786084826811,"type":"turn"}"#,
        "\n",
        r#"{"blocks":[{"content":[{"content":{"text":"done","type":"text"},"type":"content"}],"isError":false,"toolUseId":"c1","type":"toolResult"}],"nativeId":"c1","role":"assistant","sessionId":"sess_fake_1","timestamp":1786084826819,"type":"turn"}"#,
        "\n",
        r#"{"blocks":[{"type":"raw","value":{"plan":[{"content":"probe","priority":"high","status":"completed"}]}}],"nativeId":null,"role":"assistant","sessionId":"sess_fake_1","timestamp":1786084826820,"type":"turn"}"#,
    );

    #[test]
    fn a_real_bridge_session_parses_into_the_expected_turns() {
        let turns: Vec<Turn> = CAPTURED_SESSION
            .lines()
            .enumerate()
            .filter_map(|(index, line)| parse_transcript_line("agent-1", index, line))
            .collect();

        assert_eq!(turns.len(), 5, "every captured line should parse");
        assert_eq!(turns[0].role, "user");
        assert!(turns[1..].iter().all(|turn| turn.role == "assistant"));
        assert!(
            turns
                .iter()
                .all(|turn| turn.session_id.as_deref() == Some("sess_fake_1"))
        );

        assert_eq!(
            turns[1].blocks,
            vec![TurnBlock::Text {
                text: "You said: hello acp".to_string()
            }]
        );
        assert!(matches!(turns[2].blocks[0], TurnBlock::ToolUse { .. }));
        // The tool result must pair with its call, which is the whole point of
        // getting `tool_use_id`'s spelling right.
        match &turns[3].blocks[0] {
            TurnBlock::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id.as_deref(), Some("c1"));
                assert!(!is_error);
            }
            other => panic!("expected a tool result, got {other:?}"),
        }
        assert!(matches!(turns[4].blocks[0], TurnBlock::Raw { .. }));
    }

    /// Transcripts written by an older bridge sit on disk beside newer ones and
    /// are re-tailed on every resume, so the parser has to keep reading them.
    #[test]
    fn transcripts_written_before_the_field_rename_still_parse() {
        let line = json!({
            "type": "turn",
            "role": "assistant",
            "blocks": [{
                "type": "toolResult",
                "tool_use_id": "call_1",
                "content": "ok",
                "is_error": true,
            }],
        })
        .to_string();

        let turn = parse_transcript_line("agent-1", 0, &line).expect("a turn");
        assert_eq!(
            turn.blocks,
            vec![TurnBlock::ToolResult {
                tool_use_id: Some("call_1".to_string()),
                content: json!("ok"),
                is_error: true,
            }]
        );
    }

    fn config_payload() -> Value {
        json!({ "configOptions": [
            {
                "id": "mode", "name": "Session Mode", "category": "mode", "type": "select",
                "currentValue": "ask",
                "options": [
                    { "value": "ask", "name": "Ask" },
                    { "value": "code", "name": "Code" },
                ],
            },
            {
                "id": "model", "name": "Model", "category": "model", "type": "select",
                "currentValue": "model-2",
                "options": [
                    { "value": "model-1", "name": "Sonnet" },
                    { "value": "model-2", "name": "Opus" },
                ],
            },
            {
                "id": "thinking", "name": "Thinking", "category": "thought_level", "type": "select",
                "currentValue": "high",
                "options": [{ "value": "high", "name": "Extra" }],
            },
            { "id": "brave", "name": "Brave Mode", "type": "boolean", "currentValue": true },
        ]})
    }

    #[test]
    fn config_options_parse_including_boolean_and_uncategorized_entries() {
        let options = parse_config_options(&config_payload());
        assert_eq!(options.len(), 4);

        let model = &options[1];
        assert_eq!(model.id, "model");
        assert_eq!(model.kind, "select");
        assert_eq!(model.category.as_deref(), Some("model"));
        assert_eq!(model.options.len(), 2);

        // A boolean carries no choices and has no category.
        let brave = &options[3];
        assert_eq!(brave.kind, "boolean");
        assert_eq!(brave.current_value, json!(true));
        assert!(brave.options.is_empty());
        assert_eq!(brave.category, None);
    }

    #[test]
    fn the_current_value_label_prefers_the_choice_name_over_its_id() {
        let options = parse_config_options(&config_payload());
        // "model-2" is meaningless in a header; "Opus" is the point.
        assert_eq!(category_label(&options, "model").as_deref(), Some("Opus"));
        assert_eq!(
            category_label(&options, "thought_level").as_deref(),
            Some("Extra")
        );
        assert_eq!(category_label(&options, "mode").as_deref(), Some("Ask"));
        assert_eq!(category_label(&options, "model_config"), None);
    }

    #[test]
    fn a_value_with_no_matching_choice_falls_back_to_itself() {
        let options = parse_config_options(&json!({ "configOptions": [
            { "id": "m", "name": "Model", "category": "model", "type": "select",
              "currentValue": "gpt-9", "options": [{ "value": "other", "name": "Other" }] },
        ]}));
        assert_eq!(category_label(&options, "model").as_deref(), Some("gpt-9"));
    }

    #[test]
    fn booleans_and_empty_values_produce_sensible_labels() {
        let options = parse_config_options(&json!({ "configOptions": [
            { "id": "a", "name": "A", "category": "_x", "type": "boolean", "currentValue": true },
            { "id": "b", "name": "B", "category": "_y", "type": "boolean", "currentValue": false },
            { "id": "c", "name": "C", "category": "_z", "type": "select", "currentValue": null },
            { "id": "d", "name": "D", "category": "_w", "type": "select", "currentValue": "  " },
        ]}));
        assert_eq!(category_label(&options, "_x").as_deref(), Some("on"));
        assert_eq!(category_label(&options, "_y").as_deref(), Some("off"));
        // Nothing worth showing beats showing an empty string.
        assert_eq!(category_label(&options, "_z"), None);
        assert_eq!(category_label(&options, "_w"), None);
    }

    #[test]
    fn one_malformed_option_does_not_discard_the_rest() {
        // ACP is extensible; a shape qmux can't read must not cost the user
        // their whole model picker.
        let options = parse_config_options(&json!({ "configOptions": [
            { "nonsense": true },
            { "id": "model", "name": "Model", "category": "model", "type": "select",
              "currentValue": "m1", "options": [{ "value": "m1", "name": "One" }] },
        ]}));
        assert_eq!(options.len(), 1);
        assert_eq!(category_label(&options, "model").as_deref(), Some("One"));
    }

    #[test]
    fn a_payload_without_config_options_parses_as_empty() {
        for payload in [json!({}), json!({ "configOptions": [] }), json!(null)] {
            assert!(parse_config_options(&payload).is_empty(), "{payload}");
        }
    }

    #[test]
    fn config_options_round_trip_to_the_frontend_shape() {
        let options = parse_config_options(&config_payload());
        let encoded = serde_json::to_value(&options).expect("serializes");
        // `type` and `currentValue` are the names the protocol and the UI both
        // use; `kind` is only the Rust spelling.
        assert_eq!(encoded[1]["type"], "select");
        assert_eq!(encoded[1]["currentValue"], "model-2");
        assert_eq!(encoded[1]["options"][1]["name"], "Opus");
        assert!(
            encoded[3].get("options").is_none(),
            "empty choices are omitted"
        );

        let decoded: Vec<AcpConfigOption> = serde_json::from_value(encoded).expect("round-trips");
        assert_eq!(decoded, options);
    }

    #[test]
    fn non_turn_lines_are_ignored_by_the_turn_parser() {
        for line in [
            r#"{"type":"lifecycle","event":"interrupted"}"#,
            r#"{"type":"malformed","line":"garbage"}"#,
            r#"{"type":"turn","role":"assistant","blocks":[]}"#,
            "not json",
        ] {
            assert!(
                parse_transcript_line("agent-1", 0, line).is_none(),
                "should not parse: {line}"
            );
        }
    }

    #[test]
    fn lifecycle_lines_map_to_transcript_events() {
        assert_eq!(
            parse_transcript_lifecycle_event(r#"{"type":"lifecycle","event":"interrupted"}"#),
            Some(TranscriptLifecycleEvent::Interrupted)
        );
        assert_eq!(
            parse_transcript_lifecycle_event(r#"{"type":"lifecycle","event":"turnStarted"}"#),
            Some(TranscriptLifecycleEvent::TurnStarted)
        );
        assert_eq!(
            parse_transcript_lifecycle_event(r#"{"type":"lifecycle","event":"unknown"}"#),
            None
        );
        assert_eq!(
            parse_transcript_lifecycle_event(r#"{"type":"turn","role":"user","blocks":[]}"#),
            None
        );
    }

    #[test]
    fn launch_options_accept_an_agent_and_reject_typos() {
        assert_eq!(
            AcpLaunchOptions::from_value(Value::Null).unwrap().agent,
            None
        );
        assert_eq!(
            AcpLaunchOptions::from_value(json!({ "agent": "goose" }))
                .unwrap()
                .agent
                .as_deref(),
            Some("goose")
        );
        assert!(AcpLaunchOptions::from_value(json!({ "agnet": "goose" })).is_err());
    }

    fn remote_host(ssh: &str) -> Host {
        crate::host::for_group(Some(&crate::workspace::RemoteRef {
            id: "saved-1".to_string(),
            label: "devbox".to_string(),
            host: ssh.to_string(),
            multiplexer: crate::workspace::RemoteMultiplexer::Tmux,
            qmux_cli: Some("/opt/qmux-cli".to_string()),
            workspace_root: None,
        }))
    }

    #[test]
    fn only_a_local_bridge_is_given_a_transcript_path() {
        let transcript = Path::new("/data/session.jsonl");
        assert_eq!(
            AcpAdapter::transcript_sink(&Host::Local, transcript),
            Some(transcript)
        );

        assert_eq!(
            AcpAdapter::transcript_sink(&remote_host("devbox"), transcript),
            None,
            "a remote bridge told this path would write where nothing reads"
        );
    }

    #[test]
    fn a_remote_bridge_streams_its_transcript_instead_of_writing_one() {
        let envs = AcpAdapter::bridge_envs(
            "gemini",
            &agent("gemini"),
            "gemini",
            "/srv/work/repo",
            // `None` is what a remote launch passes: the sidebar's filesystem
            // is not reachable from where the bridge runs.
            None,
            None,
            None,
            None,
        )
        .expect("encodes");
        let envs: BTreeMap<String, String> = envs.into_iter().collect();

        assert_eq!(envs["QMUX_ACP_TRANSCRIPT_STREAM"], "1");
        assert!(
            !envs.contains_key("QMUX_ACP_TRANSCRIPT"),
            "a path here would be a file nobody reads"
        );
        // The agent's stderr needs somewhere writable on its own machine, and
        // only the bridge knows where that is. Naming one from here would be a
        // guess shared by every pane running this agent on that host.
        assert!(!envs.contains_key("QMUX_ACP_LOG"));
        assert_eq!(envs["QMUX_ACP_CWD"], "/srv/work/repo");
    }

    #[test]
    fn a_local_bridge_still_writes_the_file_the_sidebar_tails() {
        let envs = AcpAdapter::bridge_envs(
            "gemini",
            &agent("gemini"),
            "gemini",
            "/repo",
            Some(Path::new("/data/session.jsonl")),
            None,
            None,
            None,
        )
        .expect("encodes");
        let envs: BTreeMap<String, String> = envs.into_iter().collect();

        assert_eq!(envs["QMUX_ACP_TRANSCRIPT"], "/data/session.jsonl");
        assert!(!envs.contains_key("QMUX_ACP_TRANSCRIPT_STREAM"));
        assert!(!envs.contains_key("QMUX_ACP_LOG"), "derived from the path");
    }

    #[test]
    fn bridge_envs_carry_everything_the_bridge_requires() {
        let mut acp_agent = agent("gemini");
        acp_agent.args = vec!["--experimental-acp".to_string()];
        acp_agent.env = BTreeMap::from([("FOO".to_string(), "bar".to_string())]);

        let envs = AcpAdapter::bridge_envs(
            "gemini",
            &acp_agent,
            "/usr/local/bin/gemini",
            "/repo",
            Some(Path::new("/data/session.jsonl")),
            Some("  hi  "),
            Some("sess_9"),
            Some("oauth-personal"),
        )
        .expect("encodes");
        let envs: BTreeMap<String, String> = envs.into_iter().collect();

        assert_eq!(envs["QMUX_ACP_COMMAND"], "/usr/local/bin/gemini");
        assert_eq!(envs["QMUX_ACP_ARGS"], r#"["--experimental-acp"]"#);
        assert_eq!(envs["QMUX_ACP_ENV"], r#"{"FOO":"bar"}"#);
        assert_eq!(envs["QMUX_ACP_CWD"], "/repo");
        assert_eq!(envs["QMUX_ACP_TRANSCRIPT"], "/data/session.jsonl");
        assert_eq!(envs["QMUX_ACP_NAME"], "gemini");
        assert_eq!(envs["QMUX_ACP_PROMPT"], "hi");
        assert_eq!(envs["QMUX_ACP_LOAD_SESSION"], "sess_9");
        assert_eq!(envs["QMUX_ACP_AUTH_METHOD"], "oauth-personal");
    }

    #[test]
    fn bridge_envs_omit_optional_entries_when_they_are_blank() {
        let envs = AcpAdapter::bridge_envs(
            "gemini",
            &agent("gemini"),
            "gemini",
            "/repo",
            Some(Path::new("/data/session.jsonl")),
            Some("   "),
            Some(""),
            Some("  "),
        )
        .expect("encodes");
        let keys: Vec<&str> = envs.iter().map(|(key, _)| key.as_str()).collect();

        assert!(!keys.contains(&"QMUX_ACP_PROMPT"));
        assert!(!keys.contains(&"QMUX_ACP_AUTH_METHOD"));
        assert!(!keys.contains(&"QMUX_ACP_LOAD_SESSION"));
        assert!(!keys.contains(&"QMUX_ACP_ENV"));
        // An agent with no args still gets the variable, as an empty JSON array.
        assert_eq!(
            envs.iter()
                .find(|(key, _)| key == "QMUX_ACP_ARGS")
                .map(|(_, value)| value.as_str()),
            Some("[]")
        );
    }
}
