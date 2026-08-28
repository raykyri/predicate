use super::{
    AdapterNotification, AdapterNotificationOutcome, AgentAdapter, ComposerPolicy, LaunchEnv,
    PrepareShellAgentLaunchRequest, PreparedShellAgentLaunch, ShellCommandIntegration,
    SpawnAgentRequest, TranscriptLifecycleEvent, apply_shell_cli_model, ensure_on_path,
    normalize_agent_model, prepared_shell_agent, record_shell_session_lineage,
    reusable_session_agent, shell_cli_model, shell_quote_arg, shell_quote_path,
};
use crate::config::QmuxConfig;
use crate::events::QmuxEvent;
use crate::pty::{
    CommandPlan, InitialPaneSize, PaneMeta, agent_pane_envs, plan_to_spec, recoverable_dir,
    spawn_pty,
};
use crate::state::{AppState, PaneInfo, PaneKind};
use crate::transcript::{Turn, TurnBlock, start_transcript_tail};
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
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The Muse Code lifecycle hook events qmux installs. Muse's hook payloads are
/// Claude-shaped (event JSON on stdin), but the delivery mechanism is not: hooks
/// are declared by a *plugin*, and the plugin's capabilities must be approved
/// before they run. See [`ensure_muse_integration`].
const MUSE_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Stop",
    "SubagentStart",
    "SubagentStop",
];

/// Id of the qmux-owned Muse plugin. Also the name `muse plugins approve` takes.
const MUSE_PLUGIN_ID: &str = "qmux-hooks";

/// Bumped whenever the generated plugin sources change shape. The installed
/// stamp records a fingerprint of the rendered files, so this is only a
/// human-readable marker in the manifest.
const MUSE_PLUGIN_VERSION: &str = "0.1.0";

/// Muse refuses to load plugins at all without this set.
const MUSE_EXPERIMENTAL_PLUGINS_ENV: &str = "MUSE_EXPERIMENTAL_PLUGINS";

/// Adapter for Meta's Muse Code CLI.
///
/// Muse ships Claude-shaped lifecycle hooks, so the agent timeline is driven the
/// same way as Claude, Codex, and Grok. Two things make the wiring different:
///
/// 1. **Hooks are plugin capabilities.** There is no settings file or hooks
///    directory qmux can drop a file into — every probed alternative
///    (`settings.json` hooks, `managed_hooks_path`, `TBH_MANAGED_HOOKS_PATH`, a
///    project `.musehooks.json`) is silently ignored. Only a native plugin
///    works, it needs `MUSE_EXPERIMENTAL_PLUGINS=1`, and its capabilities must
///    be approved once before they run.
/// 2. **Muse sanitizes the hook environment.** Hooks are exec'd with a
///    whitelist (`PATH`, `HOME`, `MUSE_PLUGIN_*`, …); every `QMUX_*` variable is
///    stripped. The Claude/Grok shim pattern — "no-op unless the qmux env is
///    set, otherwise `qmux notify`" — cannot work here, because the shim can
///    never see the pane it belongs to. Instead qmux writes a *binding file* per
///    pane before launch, and the shim resolves its pane from the `session_id`
///    and `cwd` that every hook payload carries. See [`write_muse_binding`] and
///    the CLI's `muse-notify` command.
#[derive(Clone, Debug)]
pub struct MuseAdapter {
    binary: String,
}

impl MuseAdapter {
    pub fn new(config: &QmuxConfig) -> Self {
        Self {
            binary: config.muse_binary(),
        }
    }

    fn ensure_binary(&self) -> Result<String, String> {
        let binary = ensure_on_path(&self.binary).ok_or_else(|| {
            format!(
                "Muse adapter binary '{}' was not found on PATH or standard macOS tool paths. Install the Muse CLI or update adapters.muse.binary in qmux.config.json.",
                self.binary
            )
        })?;
        Ok(binary.display().to_string())
    }
}

impl AgentAdapter for MuseAdapter {
    fn id(&self) -> &'static str {
        "muse"
    }

    fn display_name(&self) -> &'static str {
        "Muse"
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
        self.prepare_shell_launch(state, request)
    }

    fn shell_commands(&self) -> Vec<ShellCommandIntegration> {
        vec![ShellCommandIntegration {
            command_name: "muse",
            adapter_id: self.id(),
        }]
    }

    fn shell_resume_command(&self, session_id: &str) -> Option<String> {
        Some(format!("muse resume {}", shell_quote_arg(session_id)))
    }

    fn ingest_notification(
        &self,
        state: &AppState,
        notification: AdapterNotification,
    ) -> Result<AdapterNotificationOutcome, String> {
        self.ingest_muse_notification(state, notification)
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
        transcript_line_model(line)
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

impl MuseAdapter {
    fn spawn_pane(&self, state: &AppState, request: SpawnAgentRequest) -> Result<PaneInfo, String> {
        let binary = self.ensure_binary()?;
        let options = MuseLaunchOptions::from_value(request.options)?;
        ensure_muse_integration_for(state, &binary)?;

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
                effort: options.reasoning_effort.clone(),
                use_worktree: request.use_worktree.unwrap_or(false),
            },
            request.parent_id.as_deref(),
        )?;
        // `prepare_agent_workspace` carries model and effort but knows nothing
        // adapter-specific, so the approval policy is recorded here — otherwise
        // a respawn would quietly revert to Muse's default.
        let agent = match trimmed(options.approval_mode.as_deref()) {
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
                "Muse working directory {} does not exist",
                cwd.display()
            ));
        }

        let has_initial_prompt = !request.prompt.trim().is_empty();
        let args = options.build_args(model.as_deref(), &request.prompt);

        let pane_id = state.next_id("pane");
        let mut envs = agent_pane_envs(state, &pane_id, &agent.id)?;
        envs.push((MUSE_EXPERIMENTAL_PLUGINS_ENV.to_string(), "1".to_string()));

        // The binding must exist before the process starts: SessionStart fires
        // within milliseconds of exec, and a hook that cannot resolve its pane is
        // simply dropped (there is no env fallback to recover it later).
        write_muse_binding(state, &pane_id, &agent.id, &cwd, None)?;
        if let Err(err) =
            attach_muse_agent_pane(state, &agent.id, pane_id.clone(), has_initial_prompt)
        {
            // The binding is already on disk and no pane will ever claim it.
            remove_muse_binding(&pane_id);
            return Err(err);
        }
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
                remove_muse_binding(&pane_id);
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
        ensure_muse_integration_for(state, &binary)?;
        let cwd = recoverable_dir(&agent.worktree_dir).ok_or_else(|| {
            format!(
                "agent worktree {} no longer exists; relaunch manually",
                agent.worktree_dir
            )
        })?;

        let session_id = agent
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|session_id| !session_id.is_empty());
        // Restore the launch configuration rather than falling back to Muse's
        // defaults: coming back from a crash with a more permissive approval
        // policy than the user chose would be a silent downgrade.
        let options = MuseLaunchOptions {
            model: agent.model.clone(),
            reasoning_effort: agent.effort.clone(),
            approval_mode: agent.approval_mode.clone(),
        };
        let args = options.build_resume_args(agent.model.as_deref(), session_id);
        let resumed = session_id.is_some();

        let mut envs = agent_pane_envs(state, &pane.id, &agent.id)?;
        envs.push((MUSE_EXPERIMENTAL_PLUGINS_ENV.to_string(), "1".to_string()));

        // A resumed Muse session never fires SessionStart, so the session id can
        // only ever reach the binding from here. Seed it up front — without it
        // the resumed pane's hooks resolve by cwd alone.
        write_muse_binding(state, &pane.id, &agent.id, &cwd, session_id)?;

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

        // A recovered Muse process launches without an inline prompt, so it is
        // idle once the TUI appears; the first prompt/tool hook promotes it.
        let mut restored = agent.clone();
        restored.pane_id = Some(pane.id.clone());
        restored.status = AgentStatus::Idle;
        state.update_agent(restored.clone())?;

        match restored.transcript_path.clone() {
            Some(transcript_path) => start_transcript_tail(
                state.clone(),
                restored.id.clone(),
                transcript_path,
                self.id().to_string(),
            ),
            // Recovered from a persisted agent that never bound a transcript
            // (e.g. it was killed before its first turn). Rediscover it from the
            // session id rather than leaving the right pane permanently blank.
            None => {
                if let Some(session_id) = session_id {
                    bind_muse_transcript(state, &restored.id, session_id);
                }
            }
        }

        state.emit(QmuxEvent::new(
            "agent.recovered",
            Some(pane.id.clone()),
            Some(restored.id.clone()),
            json!({ "resumed": resumed, "agent": restored }),
        ));

        Ok(info)
    }

    fn prepare_shell_launch(
        &self,
        state: &AppState,
        request: PrepareShellAgentLaunchRequest,
    ) -> Result<PreparedShellAgentLaunch, String> {
        let binary = self.ensure_binary()?;
        ensure_muse_integration_for(state, &binary)?;

        if !state.pane_exists(&request.pane_id)? {
            return Err(format!("pane {} was not found", request.pane_id));
        }

        let cwd = PathBuf::from(&request.cwd);
        if !cwd.is_dir() {
            return Err(format!(
                "Muse working directory {} does not exist",
                cwd.display()
            ));
        }
        let cwd_str = fs::canonicalize(&cwd)
            .unwrap_or_else(|_| cwd.clone())
            .display()
            .to_string();

        let pane_group_id = state
            .pane_group_id(&request.pane_id)?
            .ok_or_else(|| format!("pane {} was not found", request.pane_id))?;
        let resume_session_id = muse_resume_session_id(&request.args).map(str::to_string);
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
                        // Typing `muse` in a shell runs in the current directory.
                        use_worktree: false,
                    },
                )?,
            },
        };
        // Muse has no fork command, so there is never a fork point to record.
        let agent = record_shell_session_lineage(
            state,
            agent,
            self.id(),
            None,
            resume_session_id.as_deref(),
            &cwd_str,
        )?;
        let agent = apply_shell_cli_model(state, agent, &request.args)?;
        let agent = attach_muse_agent_pane(
            state,
            &agent.id,
            request.pane_id.clone(),
            muse_args_contain_prompt(&request.args),
        )?;

        // A shell `muse resume <id>` gets the same up-front session binding a
        // recovered pane does; a fresh `muse` binds by cwd until SessionStart.
        write_muse_binding(
            state,
            &request.pane_id,
            &agent.id,
            &cwd,
            resume_session_id.as_deref(),
        )?;
        if let Some(session_id) = resume_session_id.as_deref() {
            bind_muse_transcript(state, &agent.id, session_id);
        }

        let mut envs = agent_pane_envs(state, &request.pane_id, &agent.id)?;
        envs.push((MUSE_EXPERIMENTAL_PLUGINS_ENV.to_string(), "1".to_string()));
        let agent_id = agent.id.clone();

        state.emit(QmuxEvent::new(
            "agent.spawned",
            Some(request.pane_id.clone()),
            Some(agent_id),
            json!({ "agent": agent.clone(), "source": "shell" }),
        ));

        Ok(PreparedShellAgentLaunch {
            binary,
            cwd: request.cwd,
            args: request.args,
            envs: envs
                .into_iter()
                .map(|(key, value)| LaunchEnv { key, value })
                .collect(),
            supervised: true,
        })
    }

    fn ingest_muse_notification(
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
        let payload_session_id = super::string_field(&notification.payload, "session_id")
            .or_else(|| super::string_field(&notification.payload, "sessionId"));

        // Every hook Muse fires inside a subagent — including its built-in
        // `tbh-reminders` agents, which run on *every* turn and keep running
        // after the main `Stop` — reports the child's `session_id` and carries no
        // pointer back to the parent. They still route to the right pane (cwd is
        // inherited), but they must never drive the pane's status or the main
        // turn would never settle.
        //
        // Comparing against the recorded main session is the general test, but it
        // needs a main session to compare with. Muse's subagent lifecycle events
        // name themselves, so use that too: it is the only classification that
        // works before the pane has an identity.
        let is_subagent_event = payload_names_a_subagent(&notification.payload)
            || agent.as_ref().is_some_and(|agent| {
                matches!(
                    (agent.session_id.as_deref(), payload_session_id.as_deref()),
                    (Some(main), Some(reported)) if main != reported
                )
            });

        if is_subagent_event {
            return Ok(AdapterNotificationOutcome::Event(QmuxEvent::new(
                "agent.subagent_activity",
                pane_id,
                agent.map(|agent| agent.id),
                json!({
                    "hookEvent": hook_event,
                    "payload": notification.payload,
                }),
            )));
        }

        // A resumed session never fires SessionStart, so the first main-session
        // hook is where a pane that lost its identity gets it back. SessionStart
        // does its own binding below; running both would start two discovery
        // threads for one session.
        if hook_can_define_the_main_session(&hook_event)
            && let (Some(current), Some(session_id)) =
                (agent.as_ref(), payload_session_id.as_deref())
            && current.session_id.is_none()
        {
            adopt_muse_session_identity(state, current, session_id)?;
            agent = state.agent(&current.id)?.or(agent);
        }

        let event_type = match hook_event.as_str() {
            "SessionStart" => {
                if let Some(current) = agent.as_ref()
                    && let Some(session_id) = payload_session_id.as_deref()
                {
                    // First SessionStart wins. A Muse session id never changes
                    // (there is no fork, and a resume keeps its id), so a second
                    // one for an already-bound pane is not this pane's session —
                    // it is a `muse` the user started outside qmux in the same
                    // directory, which matched this pane's binding by cwd.
                    // Re-pointing the agent at it would silently strand the real
                    // session, whose every later hook would then look foreign.
                    let mut claimed = false;
                    let updated = state.mutate_agent(&current.id, |agent| {
                        if agent.session_id.is_none() {
                            agent.session_id = Some(session_id.to_string());
                            claimed = true;
                        }
                    })?;
                    if updated.is_some() && claimed {
                        // Record the session on the binding so this pane's later
                        // hooks resolve by session id rather than by directory —
                        // the only thing that tells two Muse processes in one
                        // directory apart.
                        claim_muse_binding(current, session_id);
                        bind_muse_transcript(state, &current.id, session_id);
                    }
                }
                // A session starting is not a turn running; the first prompt or
                // tool hook promotes the agent to Running.
                "agent.session_start"
            }
            "UserPromptSubmit" => {
                if let Some(agent) = agent.as_mut() {
                    let prompt = super::string_field(&notification.payload, "prompt");
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
            // Muse fires PermissionRequest for any prompt-bound call, but it has
            // no matching resolution event — nothing ever says the user answered.
            // Parking the pane in AwaitingPermission would therefore strand it,
            // and in practice Muse's policy layer and approval judge allow most
            // calls without ever showing a dialog. Treat it as ordinary activity
            // and surface the request as an event instead.
            "PermissionRequest" => {
                if let Some(agent) = agent.as_mut() {
                    agent.status = AgentStatus::Running;
                    state.set_agent_status(&agent.id, agent.status)?;
                }
                "agent.permission_request"
            }
            // Reported for the main session's own subagent bookkeeping. Kept
            // passive: Muse's built-in reminder agents fire these constantly, and
            // gating idle on them would wedge the queue behind a live tab.
            "SubagentStart" => "agent.subagent_started",
            "SubagentStop" => "agent.subagent_stopped",
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
        // `advance_after_idle` writes status/paused straight to the store, so
        // re-read the agent before attaching it or the event ships a stale copy.
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

/// Launcher options for a qmux-started Muse agent. Deliberately conservative:
/// no `--yolo`, and no worktree flag (qmux owns worktrees, so Muse's own `-w`
/// would nest a second one inside the first).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MuseLaunchOptions {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    approval_mode: Option<String>,
}

/// Values `--reasoning-effort` accepts, per `muse --help`. Validated here so a
/// stale UI choice fails in qmux with a readable error instead of making Muse
/// exit with a usage message the pane immediately discards.
const MUSE_REASONING_EFFORTS: &[&str] =
    &["none", "minimal", "low", "medium", "high", "xhigh", "ultra"];

/// Values `--approval-mode` accepts, per `muse --help`.
const MUSE_APPROVAL_MODES: &[&str] = &["untrusted", "on-request", "never"];

impl MuseLaunchOptions {
    fn from_value(value: Value) -> Result<Self, String> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let options = serde_json::from_value::<Self>(value)
            .map_err(|err| format!("invalid Muse adapter options: {err}"))?;
        options.validate()?;
        Ok(options)
    }

    fn validate(&self) -> Result<(), String> {
        if let Some(effort) = trimmed(self.reasoning_effort.as_deref())
            && !MUSE_REASONING_EFFORTS.contains(&effort)
        {
            return Err(format!(
                "unsupported Muse reasoning effort '{effort}'; expected one of {}",
                MUSE_REASONING_EFFORTS.join(", ")
            ));
        }
        if let Some(mode) = trimmed(self.approval_mode.as_deref())
            && !MUSE_APPROVAL_MODES.contains(&mode)
        {
            return Err(format!(
                "unsupported Muse approval mode '{mode}'; expected one of {}",
                MUSE_APPROVAL_MODES.join(", ")
            ));
        }
        Ok(())
    }

    /// Global flags shared by a fresh launch and a resume. Muse takes the
    /// working directory from the process cwd (there is no `--cwd`), so the
    /// pane's `CommandPlan.cwd` is what selects the workspace.
    fn global_args(&self, model: Option<&str>) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(model) = trimmed(model) {
            args.push("--model".to_string());
            args.push(model.to_string());
        }
        if let Some(effort) = trimmed(self.reasoning_effort.as_deref()) {
            args.push("--reasoning-effort".to_string());
            args.push(effort.to_string());
        }
        if let Some(mode) = trimmed(self.approval_mode.as_deref()) {
            args.push("--approval-mode".to_string());
            args.push(mode.to_string());
        }
        args
    }

    fn build_args(&self, model: Option<&str>, prompt: &str) -> Vec<String> {
        let mut args = self.global_args(model);
        // The initial prompt is a trailing positional, delimited with `--` so a
        // prompt starting with `-` is not parsed as a flag (Muse rejects the
        // undelimited form outright).
        if let Some(prompt) = trimmed(Some(prompt)) {
            args.push("--".to_string());
            args.push(prompt.to_string());
        }
        args
    }

    /// `muse resume <id>` when a session is recorded, else a fresh interactive
    /// launch. Root options may appear on either side of the subcommand; qmux
    /// puts them first to match the documented usage.
    fn build_resume_args(&self, model: Option<&str>, session_id: Option<&str>) -> Vec<String> {
        let mut args = self.global_args(model);
        if let Some(session_id) = trimmed(session_id) {
            args.push("resume".to_string());
            args.push(session_id.to_string());
        }
        args
    }
}

fn trimmed(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// Whether a payload identifies itself as coming from a subagent, without
/// reference to any recorded main session.
///
/// Muse reports `SubagentStart` / `SubagentStop` from the *child's* point of
/// view: `session_id` equals `child_session_id`, and `subagent_id` names the
/// agent. Those two markers are the only self-description available, and they
/// are what lets a subagent event be recognized on a pane whose own session id
/// is not yet known.
fn payload_names_a_subagent(payload: &Value) -> bool {
    if super::subagent_id(payload).is_some() {
        return true;
    }
    matches!(
        (
            super::string_field(payload, "child_session_id"),
            super::string_field(payload, "session_id"),
        ),
        (Some(child), Some(session)) if child == session
    )
}

/// Whether a hook may name the pane's main session when none is recorded yet.
///
/// Restricted to the events only a main session produces. A subagent's tool
/// hooks carry the child's `session_id` and nothing that distinguishes them from
/// the parent's, so adopting an identity from one would bind the pane to a
/// subagent — after which every real hook would look foreign and the pane would
/// stop updating. `SessionStart` opens a session, `UserPromptSubmit` is the
/// first hook of a resumed one, and subagents end with `SubagentStop` rather
/// than `Stop`.
fn hook_can_define_the_main_session(hook_event: &str) -> bool {
    // SessionStart is excluded here even though it qualifies: it records the
    // session in its own arm, and doing both would start two discovery threads.
    matches!(hook_event, "UserPromptSubmit" | "Stop")
}

fn attach_muse_agent_pane(
    state: &AppState,
    agent_id: &str,
    pane_id: String,
    has_initial_prompt: bool,
) -> Result<AgentInfo, String> {
    let agent = attach_agent_pane(state, agent_id, pane_id)?;
    if !has_initial_prompt {
        // Field-scoped write: a full-struct update would race the SessionStart
        // hook recording session_id on the control-socket thread.
        if let Some(updated) = state.set_agent_status(agent_id, AgentStatus::Idle)? {
            return Ok(updated);
        }
    }
    Ok(agent)
}

/// Resolves an idle Muse agent: drains the next queued turn, or enters/stays
/// paused. Returns whether a turn was drained.
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

/// Records a session id learned from a hook on an agent that has none, and
/// binds its transcript. The resume path: `muse resume <id>` fires no
/// SessionStart, so `UserPromptSubmit` is the first chance to recover identity
/// for a pane whose stored session id was lost.
fn adopt_muse_session_identity(
    state: &AppState,
    current: &AgentInfo,
    session_id: &str,
) -> Result<(), String> {
    let updated = state.mutate_agent(&current.id, |agent| {
        if agent.session_id.is_none() {
            agent.session_id = Some(session_id.to_string());
        }
    })?;
    if updated
        .as_ref()
        .and_then(|agent| agent.session_id.as_deref())
        == Some(session_id)
    {
        claim_muse_binding(current, session_id);
        bind_muse_transcript(state, &current.id, session_id);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pane bindings
//
// Muse strips `QMUX_*` from the hook environment, so a hook cannot be told which
// pane it belongs to. qmux instead writes one binding file per live Muse pane
// and the `qmux muse-notify` shim matches the hook payload's `session_id` (or
// failing that, its `cwd`) against them.
// ---------------------------------------------------------------------------

/// Directory holding one JSON binding per live Muse pane. `QMUX_MUSE_HOME`
/// overrides the location (used by tests and by the CLI, which must agree with
/// the app on where to look).
pub(crate) fn muse_integration_home() -> Result<PathBuf, String> {
    if let Some(explicit) = env::var_os("QMUX_MUSE_HOME") {
        return Ok(PathBuf::from(explicit));
    }
    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or_else(|| {
            "XDG_DATA_HOME and HOME are not set; cannot configure the Muse integration".to_string()
        })?;
    Ok(data_home.join("qmux").join("muse"))
}

/// Where Muse itself keeps session logs: `$XDG_DATA_HOME/muse/sessions`.
fn muse_sessions_root() -> Result<PathBuf, String> {
    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or_else(|| {
            "XDG_DATA_HOME and HOME are not set; cannot locate Muse session logs".to_string()
        })?;
    Ok(data_home.join("muse").join("sessions"))
}

fn muse_bindings_dir() -> Result<PathBuf, String> {
    Ok(muse_integration_home()?.join("bindings"))
}

/// Writes (or refreshes) the binding that lets this pane's hooks find their way
/// home. The file carries the pane's control-socket token, so the directory and
/// the file are owner-only — the same posture qmux uses for its scrollback
/// cache, which holds the same secret.
fn write_muse_binding(
    state: &AppState,
    pane_id: &str,
    agent_id: &str,
    cwd: &Path,
    session_id: Option<&str>,
) -> Result<(), String> {
    let dir = muse_bindings_dir()?;
    fs::create_dir_all(&dir).map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .map_err(|err| format!("failed to chmod {}: {err}", dir.display()))?;

    // Muse reports the symlink-resolved cwd (`/private/tmp/...` on macOS) while
    // qmux may hold the unresolved spelling. Record both so a cwd match works
    // whichever one the payload carries.
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
    let path = muse_binding_path(&dir, pane_id);
    let raw = serde_json::to_string(&document)
        .map_err(|err| format!("failed to encode Muse pane binding: {err}"))?;
    fs::write(&path, raw).map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("failed to chmod {}: {err}", path.display()))?;
    Ok(())
}

/// Stamps the session id onto an existing binding once SessionStart reports it,
/// so later hooks from this pane resolve by session rather than by directory.
///
/// This edits the file in place rather than rewriting it from the agent record,
/// because the agent does not know where the pane was launched: `spawn_pane`
/// honors an explicit `cwd` that can differ from `worktree_dir`, and rebuilding
/// the binding from the latter would overwrite the directory Muse actually
/// reports. Best-effort — a failure only costs precision when two Muse
/// processes share a directory.
fn claim_muse_binding(agent: &AgentInfo, session_id: &str) {
    let Some(pane_id) = agent.pane_id.as_deref() else {
        return;
    };
    if let Err(err) = stamp_muse_binding_session(pane_id, session_id) {
        eprintln!("qmux: failed to record Muse session binding for pane {pane_id}: {err}");
    }
}

fn stamp_muse_binding_session(pane_id: &str, session_id: &str) -> Result<(), String> {
    let dir = muse_bindings_dir()?;
    let path = muse_binding_path(&dir, pane_id);
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
        .map_err(|err| format!("failed to encode Muse pane binding: {err}"))?;
    fs::write(&path, raw).map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("failed to chmod {}: {err}", path.display()))
}

fn muse_binding_path(dir: &Path, pane_id: &str) -> PathBuf {
    dir.join(format!("{}.json", sanitize_binding_name(pane_id)))
}

/// Pane ids are qmux-minted (`pane-12`), but they name a file, so refuse to let
/// anything but an id-shaped value through rather than trusting the format.
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

fn remove_muse_binding(pane_id: &str) {
    let Ok(dir) = muse_bindings_dir() else {
        return;
    };
    let _ = fs::remove_file(muse_binding_path(&dir, pane_id));
}

/// A binding younger than this is never pruned, even when its pane is not in
/// the store yet. `spawn_pane` writes the binding before `spawn_pty` creates the
/// pane — SessionStart can arrive that early — so without a grace period a
/// second Muse launch racing through `prune_muse_bindings` in that window would
/// delete the first launch's binding and silently kill its hooks.
const MUSE_BINDING_PRUNE_GRACE: u64 = 60_000;

/// Drops bindings whose pane is gone.
///
/// Runs on every launch rather than on pane close because a binding outlives the
/// app (it is a file); [`clear_muse_bindings`] handles the restart case, where
/// every binding is stale by definition.
fn prune_muse_bindings(state: &AppState) {
    let Ok(dir) = muse_bindings_dir() else {
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
            // Unreadable or not JSON: nothing can ever resolve through it.
            let _ = fs::remove_file(&path);
            continue;
        };
        let written = document
            .get("updatedAt")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        if now.saturating_sub(written) < MUSE_BINDING_PRUNE_GRACE {
            continue;
        }
        let stale = match super::string_field(&document, "paneId") {
            // Keep the binding when the store cannot answer. A transient error
            // must not delete a live pane's only route home; a genuinely dead
            // binding is merely pruned by a later launch instead.
            Some(pane_id) => !state.pane_exists(&pane_id).unwrap_or(true),
            None => true,
        };
        if stale {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Removes every pane binding. Called once at startup: bindings carry a pane's
/// control-socket token, tokens are minted per process and never persisted, so
/// after a restart every binding on disk is both useless and a stale secret.
pub fn clear_muse_bindings() {
    let Ok(dir) = muse_bindings_dir() else {
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

// ---------------------------------------------------------------------------
// Transcript binding
// ---------------------------------------------------------------------------

/// How long to keep looking for a session's log directory before giving up.
const MUSE_TRANSCRIPT_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
const MUSE_TRANSCRIPT_DISCOVERY_INTERVAL: Duration = Duration::from_millis(250);

/// Finds the Muse session log for `session_id` and binds the agent's transcript
/// to it, on a background thread.
///
/// Muse always reports `transcript_path: null` in its hooks, and its logs are
/// filed under a *date* directory qmux cannot compute (no timezone-aware date
/// crate in the tree, and the date is Muse's local one). So the path is
/// discovered by scanning `sessions/<year>/<month>/<day>/<session-id>/` — and
/// the scan retries, because SessionStart can beat the directory into existence.
fn bind_muse_transcript(state: &AppState, agent_id: &str, session_id: &str) {
    if !is_muse_session_id(session_id) {
        return;
    }
    let state = state.clone();
    let agent_id = agent_id.to_string();
    let session_id = session_id.to_string();
    std::thread::spawn(move || {
        let deadline = Instant::now() + MUSE_TRANSCRIPT_DISCOVERY_TIMEOUT;
        loop {
            if let Some(path) = muse_session_transcript_path(&session_id) {
                let path = path.display().to_string();
                let updated = state.mutate_agent(&agent_id, |agent| {
                    agent.transcript_path = Some(path.clone());
                });
                match updated {
                    // The agent is gone (pane closed while we searched).
                    Ok(None) => return,
                    Ok(Some(_)) => {
                        start_transcript_tail(
                            state.clone(),
                            agent_id.clone(),
                            path,
                            "muse".to_string(),
                        );
                        return;
                    }
                    Err(err) => {
                        eprintln!(
                            "qmux: failed to bind Muse transcript for agent {agent_id}: {err}"
                        );
                        return;
                    }
                }
            }
            if Instant::now() >= deadline {
                eprintln!(
                    "qmux: no Muse session log appeared for session {session_id}; the transcript pane will stay empty"
                );
                return;
            }
            std::thread::sleep(MUSE_TRANSCRIPT_DISCOVERY_INTERVAL);
        }
    });
}

/// Muse session ids are UUIDs. Enforce that before letting one name a directory.
fn is_muse_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 64
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// `$XDG_DATA_HOME/muse/sessions/<year>/<month>/<day>/<session-id>/session.jsonl`.
///
/// Scans newest date directories first and only a few of them. A session qmux is
/// binding was started seconds ago, so it lands in the newest day directory —
/// the small margin covers a midnight rollover and the window before Muse has
/// created today's directory at all. Walking the whole tree instead would cost
/// one `read_dir` per month of history on *every* retry tick, which is precisely
/// the case where discovery is already polling hardest.
const MUSE_DISCOVERY_YEARS: usize = 2;
const MUSE_DISCOVERY_MONTHS: usize = 2;
const MUSE_DISCOVERY_DAYS: usize = 3;

fn muse_session_transcript_path(session_id: &str) -> Option<PathBuf> {
    let root = muse_sessions_root().ok()?;
    for year in descending_subdirectories(&root)
        .into_iter()
        .take(MUSE_DISCOVERY_YEARS)
    {
        for month in descending_subdirectories(&year)
            .into_iter()
            .take(MUSE_DISCOVERY_MONTHS)
        {
            for day in descending_subdirectories(&month)
                .into_iter()
                .take(MUSE_DISCOVERY_DAYS)
            {
                let candidate = day.join(session_id).join("session.jsonl");
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// Immediate subdirectories of `dir`, newest name first. Muse's date directories
/// are zero-padded, so a reverse lexicographic sort is a reverse date sort.
fn descending_subdirectories(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_type()
                .is_ok_and(|file_type| file_type.is_dir() && !file_type.is_symlink())
        })
        .map(|entry| entry.path())
        .collect();
    paths.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    paths
}

// ---------------------------------------------------------------------------
// Plugin + shim installation
// ---------------------------------------------------------------------------

/// Ensures the qmux Muse plugin is installed and approved, and that the shim it
/// executes points at the current qmux CLI.
///
/// Installation runs `muse plugins install` + `muse plugins approve`, which are
/// two subprocesses — so it is gated on a fingerprint stamp and skipped entirely
/// once the generated sources match what is already installed.
fn ensure_muse_integration(binary: &str, cli_path: &Path) -> Result<(), String> {
    let home = muse_integration_home()?;
    fs::create_dir_all(&home)
        .map_err(|err| format!("failed to create {}: {err}", home.display()))?;

    let shim_path = home.join("qmux-muse-hook");
    let shim = muse_hook_shim(cli_path, &muse_bindings_dir()?);
    if !file_matches(&shim_path, &shim) || !is_executable(&shim_path) {
        fs::write(&shim_path, &shim)
            .map_err(|err| format!("failed to write {}: {err}", shim_path.display()))?;
        fs::set_permissions(&shim_path, fs::Permissions::from_mode(0o755))
            .map_err(|err| format!("failed to chmod {}: {err}", shim_path.display()))?;
    }

    let plugin_dir = home.join("plugin");
    let fingerprint = write_muse_plugin_sources(&plugin_dir, &shim_path)?;

    // The stamp records what was last installed *and approved*. Muse freezes the
    // package into its own cache on install, so a source change is only live
    // after a reinstall — the stamp is what makes that a one-time cost.
    let stamp_path = home.join("installed.stamp");
    if file_matches(&stamp_path, &fingerprint) {
        return Ok(());
    }
    install_muse_plugin(binary, &plugin_dir)?;
    fs::write(&stamp_path, &fingerprint)
        .map_err(|err| format!("failed to write {}: {err}", stamp_path.display()))?;
    Ok(())
}

/// `ensure_muse_integration` plus binding cleanup. Split so the installation
/// half stays exercisable without an `AppState`, and so the qmux CLI path is
/// injected at one seam rather than read from a global.
fn ensure_muse_integration_for(state: &AppState, binary: &str) -> Result<(), String> {
    ensure_muse_integration(binary, &crate::launch_path::qmux_cli_path()?)?;
    prune_muse_bindings(state);
    Ok(())
}

/// POSIX shim the plugin's hook scripts exec. It exists so the *plugin* — which
/// Muse freezes into its cache at install time, and whose changes require
/// re-approval — never has to name the qmux binary directly. Updating qmux
/// rewrites this file; the frozen plugin keeps calling the same stable path.
///
/// Unlike the Claude and Grok shims there is no `QMUX_*` env guard: Muse strips
/// those variables, so there is nothing to test. A standalone `muse` run does
/// still reach this shim, and `muse-notify` is what no-ops there — it exits
/// quietly when the payload matches no live pane binding.
///
/// The bindings directory is baked in as an argument for the same reason. Muse's
/// env whitelist strips `QMUX_MUSE_HOME` and `XDG_DATA_HOME` along with
/// everything else, so a hook cannot *derive* the directory either — verified
/// the hard way, by watching every hook run, exit 0, and find nothing. Passing
/// it explicitly is what makes the shim independent of the environment.
fn muse_hook_shim(cli_path: &Path, bindings_dir: &Path) -> String {
    format!(
        r#"#!/bin/sh
event="${{1:-}}"
if [ -z "$event" ]; then
  exit 0
fi
exec {} muse-notify "$event" {}
"#,
        shell_quote_path(cli_path),
        shell_quote_path(bindings_dir)
    )
}

/// Writes the plugin bundle and returns a fingerprint of its contents.
fn write_muse_plugin_sources(plugin_dir: &Path, shim_path: &Path) -> Result<String, String> {
    let manifest_dir = plugin_dir.join(".muse-plugin");
    let hooks_dir = plugin_dir.join("hooks");
    for dir in [&manifest_dir, &hooks_dir] {
        fs::create_dir_all(dir)
            .map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
    }

    let mut fingerprint = Vec::new();
    let manifest = muse_plugin_manifest_contents();
    write_if_changed(&manifest_dir.join("plugin.json"), &manifest)?;
    fingerprint.push(manifest);

    for event in MUSE_HOOK_EVENTS {
        // Muse rejects a plugin whose hooks share one script path, so every event
        // gets its own file even though they all forward to the same shim.
        let script = muse_plugin_hook_script(shim_path, event);
        let path = hooks_dir.join(format!("{event}.sh"));
        write_if_changed(&path, &script)?;
        fingerprint.push(script);
    }

    Ok(fingerprint_of(&fingerprint))
}

fn muse_plugin_hook_script(shim_path: &Path, event: &str) -> String {
    format!(
        "#!/bin/sh\nexec {} {}\n",
        shell_quote_path(shim_path),
        shell_quote_arg(event)
    )
}

fn muse_plugin_manifest() -> Value {
    let hooks: Vec<Value> = MUSE_HOOK_EVENTS
        .iter()
        .map(|event| {
            json!({
                "id": event.to_ascii_lowercase(),
                "event": event,
                // Muse takes an argv array here, not a shell string.
                "command": ["sh", format!("hooks/{event}.sh")],
            })
        })
        .collect();
    json!({
        "schemaVersion": 1,
        "name": MUSE_PLUGIN_ID,
        "displayName": "qmux",
        "version": MUSE_PLUGIN_VERSION,
        "description": "Forwards Muse lifecycle events to qmux.",
        "compat": { "source": "native", "manifestDir": ".muse-plugin" },
        "capabilities": {
            "skills": [],
            "commands": [],
            "mcpServers": [],
            "reminders": [],
            "hooks": hooks,
        },
    })
}

fn muse_plugin_manifest_contents() -> String {
    let mut raw = serde_json::to_string_pretty(&muse_plugin_manifest())
        .expect("plugin manifest is always serializable");
    raw.push('\n');
    raw
}

/// Installs the bundle and approves its hook capabilities. Both steps are
/// idempotent, and both need the experimental-plugins flag.
fn install_muse_plugin(binary: &str, plugin_dir: &Path) -> Result<(), String> {
    run_muse_plugin_command(
        binary,
        &[
            "plugins",
            "install",
            &plugin_dir.display().to_string(),
            "--scope",
            "user",
        ],
    )?;
    run_muse_plugin_command(binary, &["plugins", "approve", MUSE_PLUGIN_ID])?;
    Ok(())
}

fn run_muse_plugin_command(binary: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new(binary)
        .args(args)
        .env(MUSE_EXPERIMENTAL_PLUGINS_ENV, "1")
        .output()
        .map_err(|err| format!("failed to run `muse {}`: {err}", args.join(" ")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = [stderr.trim(), stdout.trim()]
        .into_iter()
        .find(|text| !text.is_empty())
        .unwrap_or("no output");
    Err(format!(
        "qmux could not install its Muse hook plugin (`muse {}` failed): {detail}",
        args.join(" ")
    ))
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

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| (meta.permissions().mode() & 0o111) == 0o111)
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

// ---------------------------------------------------------------------------
// Shell argument parsing
// ---------------------------------------------------------------------------

/// Extracts the session id from a `muse resume <uuid>` invocation typed in a
/// shell pane, so the restart rebinds the original agent. `--last` and a bare
/// `muse resume` (the picker) deliberately return `None`: neither names a
/// concrete session, and the identity arrives later on the first hook.
fn muse_resume_session_id(args: &[String]) -> Option<&str> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return None;
        }
        if arg == "resume" {
            return args
                .get(index + 1)
                .map(String::as_str)
                .filter(|value| !value.starts_with('-'))
                .filter(|value| is_muse_session_id(value));
        }
        if muse_value_flag(arg) {
            index += 2;
            continue;
        }
        index += 1;
    }
    None
}

/// Whether a manual `muse ...` invocation carries an initial prompt, i.e. a
/// trailing positional. Erring toward "no prompt" is safe: the agent starts idle
/// and the first real turn promotes it.
fn muse_args_contain_prompt(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return args.get(index + 1).is_some_and(|value| !value.is_empty());
        }
        if muse_value_flag(arg) {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        // A subcommand is not a prompt, and everything after it belongs to the
        // subcommand rather than to an interactive session.
        if muse_subcommand(arg) {
            return false;
        }
        return true;
    }
    false
}

/// Muse flags that take a separate value argument. Inline `--flag=value` forms
/// start with `-` and are handled by the generic flag check.
fn muse_value_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--agents"
            | "--approval-judge"
            | "--approval-mode"
            | "--base-url"
            | "--image"
            | "--model"
            | "--preset"
            | "--provider"
            | "--reasoning-effort"
            | "--sandbox-network"
            | "--workspace"
            | "--worktree-base"
            | "--worktree-existing"
            | "--echo-delay-ms"
    )
}

fn muse_subcommand(arg: &str) -> bool {
    matches!(
        arg,
        "auth"
            | "exec"
            | "export"
            | "init"
            | "login"
            | "logout"
            | "plugins"
            | "resume"
            | "sandbox"
            | "session-message"
            | "skills"
            | "trace"
    )
}

// ---------------------------------------------------------------------------
// Transcript parsing
//
// Muse writes an event-sourced JSONL log per session. Every record shares an
// envelope (`stream.id` is the session, `recorded_at` is microseconds since the
// epoch) and carries a `payload` whose `kind` and nested `event.kind` name the
// event. Subagents get their own nested logs under `<session>/subagent/<id>/`,
// so the main log needs no filtering.
// ---------------------------------------------------------------------------

fn parse_transcript_line(agent_id: &str, source_index: usize, line: &str) -> Option<Turn> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    let payload = value.get("payload")?;
    let event = payload.get("event")?;
    let session_id = value
        .get("stream")
        .and_then(|stream| super::string_field(stream, "id"));
    let timestamp = muse_timestamp_ms(&value);

    let (role, blocks) = match event.get("kind").and_then(Value::as_str)? {
        // The user's own prompt for this run.
        "started" => {
            let prompt = super::string_field(event, "prompt")?;
            ("user", vec![TurnBlock::Text { text: prompt }])
        }
        "assistant_message_committed" => {
            let text = super::string_field(event, "text")?;
            ("assistant", vec![TurnBlock::Text { text }])
        }
        "assistant_tool_calls_committed" => {
            let blocks = muse_tool_use_blocks(event);
            if blocks.is_empty() {
                return None;
            }
            ("assistant", blocks)
        }
        "tool_result_batch_committed" => {
            let blocks = muse_tool_result_blocks(event);
            if blocks.is_empty() {
                return None;
            }
            ("tool", blocks)
        }
        _ => return None,
    };

    Some(Turn {
        id: format!("{agent_id}-{source_index}"),
        agent_id: agent_id.to_string(),
        session_id,
        role: role.to_string(),
        blocks,
        source_index,
        timestamp,
        status: None,
        status_reason: None,
        context_status: None,
        native_id: super::string_field(&value, "id"),
        parent_native_id: None,
        native_message_id: super::string_field(event, "message_id"),
    })
}

fn muse_tool_use_blocks(event: &Value) -> Vec<TurnBlock> {
    event
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .map(|call| TurnBlock::ToolUse {
                    id: super::string_field(call, "call_id")
                        .or_else(|| super::string_field(call, "id")),
                    name: call
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_string(),
                    // Muse serializes tool arguments as a JSON *string*. Decode it
                    // so the UI renders structured input rather than an escaped
                    // blob; keep the raw text when it isn't valid JSON.
                    input: muse_tool_call_input(call),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn muse_tool_call_input(call: &Value) -> Value {
    match call.get("args") {
        Some(Value::String(raw)) => {
            serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.clone()))
        }
        Some(other) => other.clone(),
        None => Value::Null,
    }
}

fn muse_tool_result_blocks(event: &Value) -> Vec<TurnBlock> {
    event
        .get("results")
        .and_then(Value::as_array)
        .map(|results| {
            results
                .iter()
                .map(|result| TurnBlock::ToolResult {
                    tool_use_id: super::string_field(result, "tool_call_id"),
                    content: result
                        .get("text")
                        .cloned()
                        .unwrap_or_else(|| result.clone()),
                    is_error: result
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Muse stamps `recorded_at` in **microseconds** since the epoch, which the
/// shared `native_timestamp_ms` helper would read as milliseconds and place in
/// 1970 — hence the local conversion.
fn muse_timestamp_ms(value: &Value) -> Option<i64> {
    let recorded_at = value.get("recorded_at")?.as_i64()?;
    Some(recorded_at / 1_000)
}

/// Muse names the model in its session metadata record and again on every
/// completed model call.
fn transcript_line_model(line: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    let payload = value.get("payload")?;
    let raw = payload
        .get("record")
        .and_then(|record| super::string_field(record, "model_id"))
        .or_else(|| {
            payload
                .get("event")
                .and_then(|event| super::string_field(event, "model"))
        })?;
    normalize_agent_model(&raw)
}

/// Muse closes every run with a `terminal` event naming how it ended. Anything
/// other than a clean completion is reported as an interruption so the timeline
/// marks the turn rather than leaving it looking finished.
fn parse_transcript_lifecycle_event(line: &str) -> Option<TranscriptLifecycleEvent> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    let event = value.get("payload")?.get("event")?;
    match event.get("kind").and_then(Value::as_str)? {
        "started" if event.get("prompt").is_some() => Some(TranscriptLifecycleEvent::TurnStarted),
        "terminal" => matches!(
            event.get("terminal").and_then(Value::as_str),
            Some("cancelled" | "canceled" | "interrupted" | "aborted")
        )
        .then_some(TranscriptLifecycleEvent::Interrupted),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AdapterConfigs, ClaudeAdapterConfig, CodexAdapterConfig, GrokAdapterConfig,
        MuseAdapterConfig, OpencodeAdapterConfig,
    };

    fn test_config() -> QmuxConfig {
        QmuxConfig {
            remotes: Default::default(),
            workspace_root: PathBuf::from("/tmp/qmux-muse-tests"),
            socket_path: PathBuf::from("/tmp/qmux-muse-tests.sock"),
            adapters: AdapterConfigs {
                pi: Default::default(),
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

    fn options(json: Value) -> MuseLaunchOptions {
        MuseLaunchOptions::from_value(json).expect("options parse")
    }

    #[test]
    fn adapter_reports_its_identity() {
        let adapter = MuseAdapter::new(&test_config());
        assert_eq!(adapter.id(), "muse");
        assert_eq!(adapter.display_name(), "Muse");
        assert_eq!(
            adapter.shell_resume_command("abc-123").as_deref(),
            Some("muse resume 'abc-123'")
        );
    }

    #[test]
    fn launch_options_reject_unknown_fields_and_bad_values() {
        assert!(MuseLaunchOptions::from_value(json!({ "nope": true })).is_err());
        let err = MuseLaunchOptions::from_value(json!({ "reasoningEffort": "turbo" })).unwrap_err();
        assert!(err.contains("reasoning effort"), "unexpected error: {err}");
        let err = MuseLaunchOptions::from_value(json!({ "approvalMode": "yolo" })).unwrap_err();
        assert!(err.contains("approval mode"), "unexpected error: {err}");
    }

    #[test]
    fn build_args_place_the_prompt_after_a_separator() {
        let args = options(json!({ "reasoningEffort": "low", "approvalMode": "on-request" }))
            .build_args(Some("muse-spark-1.2"), "  ship it  ");
        assert_eq!(
            args,
            vec![
                "--model",
                "muse-spark-1.2",
                "--reasoning-effort",
                "low",
                "--approval-mode",
                "on-request",
                "--",
                "ship it",
            ]
        );
    }

    #[test]
    fn build_args_omit_empty_prompt_and_model() {
        assert!(options(Value::Null).build_args(None, "   ").is_empty());
    }

    #[test]
    fn resume_args_use_the_resume_subcommand_after_global_flags() {
        let args = options(json!({ "reasoningEffort": "high" }))
            .build_resume_args(Some("m"), Some("11111111-2222-3333-4444-555555555555"));
        assert_eq!(
            args,
            vec![
                "--model",
                "m",
                "--reasoning-effort",
                "high",
                "resume",
                "11111111-2222-3333-4444-555555555555",
            ]
        );
    }

    #[test]
    fn resume_args_fall_back_to_a_fresh_launch_without_a_session() {
        let args = options(Value::Null).build_resume_args(None, Some("   "));
        assert!(args.is_empty());
    }

    #[test]
    fn resume_session_id_reads_the_subcommand_argument() {
        let args = |values: &[&str]| values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        assert_eq!(
            muse_resume_session_id(&args(&["resume", "abc-123"])),
            Some("abc-123")
        );
        assert_eq!(
            muse_resume_session_id(&args(&["--reasoning-effort", "low", "resume", "abc-123"])),
            Some("abc-123")
        );
        // `--last` and the bare picker name no concrete session.
        assert_eq!(muse_resume_session_id(&args(&["resume", "--last"])), None);
        assert_eq!(muse_resume_session_id(&args(&["resume"])), None);
        // A value flag must not have its value mistaken for the subcommand.
        assert_eq!(muse_resume_session_id(&args(&["--model", "resume"])), None);
        // Non-id-shaped values are refused before they can name a directory.
        assert_eq!(
            muse_resume_session_id(&args(&["resume", "../../etc/passwd"])),
            None
        );
    }

    #[test]
    fn args_contain_prompt_detects_a_trailing_positional() {
        let args = |values: &[&str]| values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        assert!(muse_args_contain_prompt(&args(&["fix the build"])));
        assert!(muse_args_contain_prompt(&args(&[
            "--",
            "-starts-with-dash"
        ])));
        assert!(muse_args_contain_prompt(&args(&[
            "--reasoning-effort",
            "low",
            "do it"
        ])));
        assert!(!muse_args_contain_prompt(&args(&[])));
        assert!(!muse_args_contain_prompt(&args(&[
            "--reasoning-effort",
            "low"
        ])));
        // A value flag's value is not a prompt.
        assert!(!muse_args_contain_prompt(&args(&["--model", "muse-spark"])));
        // Subcommands run their own thing rather than starting a prompted session.
        assert!(!muse_args_contain_prompt(&args(&["resume", "abc-123"])));
        assert!(!muse_args_contain_prompt(&args(&["plugins", "list"])));
    }

    #[test]
    fn hook_shim_forwards_to_the_cli_without_needing_the_environment() {
        let shim = muse_hook_shim(
            Path::new("/Applications/qmux.app/qmux"),
            Path::new("/data/qmux/muse/bindings"),
        );
        assert!(shim.contains("'/Applications/qmux.app/qmux'"), "{shim}");
        // Muse's env whitelist strips both the QMUX_* variables the other shims
        // guard on and the XDG paths this one would otherwise derive, so the
        // bindings directory has to travel as an argument.
        assert!(
            shim.contains("muse-notify \"$event\" '/data/qmux/muse/bindings'"),
            "{shim}"
        );
        assert!(!shim.contains("QMUX_"), "{shim}");
        assert!(!shim.contains("XDG_"), "{shim}");
    }

    #[test]
    fn plugin_manifest_declares_every_hook_with_its_own_script() {
        let manifest = muse_plugin_manifest();
        let hooks = manifest["capabilities"]["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), MUSE_HOOK_EVENTS.len());
        let mut scripts: Vec<String> = hooks
            .iter()
            .map(|hook| hook["command"][1].as_str().unwrap().to_string())
            .collect();
        scripts.sort();
        scripts.dedup();
        assert_eq!(
            scripts.len(),
            MUSE_HOOK_EVENTS.len(),
            "Muse rejects hooks that share one source file"
        );
        assert_eq!(hooks[0]["command"][0], "sh");
    }

    #[test]
    fn plugin_sources_are_written_and_fingerprinted_stably() {
        let dir = std::env::temp_dir().join(format!("qmux-muse-plugin-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let shim = Path::new("/tmp/qmux-muse-hook");

        let first = write_muse_plugin_sources(&dir, shim).unwrap();
        let second = write_muse_plugin_sources(&dir, shim).unwrap();
        assert_eq!(first, second, "identical sources must fingerprint the same");
        assert!(dir.join(".muse-plugin/plugin.json").is_file());
        for event in MUSE_HOOK_EVENTS {
            assert!(dir.join(format!("hooks/{event}.sh")).is_file());
        }

        let moved = write_muse_plugin_sources(&dir, Path::new("/tmp/other-shim")).unwrap();
        assert_ne!(first, moved, "a new shim path must force a reinstall");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_ids_are_confined_before_naming_a_directory() {
        assert!(is_muse_session_id("11111111-2222-3333-4444-555555555555"));
        assert!(!is_muse_session_id(""));
        assert!(!is_muse_session_id("../escape"));
        assert!(!is_muse_session_id("has/slash"));
        assert!(!is_muse_session_id(&"x".repeat(65)));
    }

    #[test]
    fn binding_file_names_are_confined_to_the_bindings_directory() {
        let dir = Path::new("/tmp/bindings");
        assert_eq!(
            muse_binding_path(dir, "pane-12"),
            PathBuf::from("/tmp/bindings/pane-12.json")
        );
        assert_eq!(
            muse_binding_path(dir, "../../etc/passwd"),
            PathBuf::from("/tmp/bindings/______etc_passwd.json")
        );
    }

    #[test]
    fn parses_a_user_prompt_record() {
        let line = json!({
            "stream": { "kind": "session", "id": "sess-1" },
            "id": "rec-1",
            "recorded_at": 1_786_177_441_441_982i64,
            "payload_type": "runtime.session",
            "payload": { "kind": "run", "event": { "kind": "started", "prompt": "hello" } },
        })
        .to_string();

        let turn = parse_transcript_line("agent-1", 3, &line).expect("user turn");
        assert_eq!(turn.role, "user");
        assert_eq!(turn.session_id.as_deref(), Some("sess-1"));
        assert_eq!(turn.source_index, 3);
        // recorded_at is microseconds; the timeline wants milliseconds.
        assert_eq!(turn.timestamp, Some(1_786_177_441_441));
        assert!(matches!(&turn.blocks[0], TurnBlock::Text { text } if text == "hello"));
    }

    #[test]
    fn parses_assistant_text_and_tool_calls() {
        let text = json!({
            "stream": { "id": "sess-1" },
            "payload": { "kind": "run", "event": {
                "kind": "assistant_message_committed",
                "message_id": "msg-9",
                "text": "done",
            } },
        })
        .to_string();
        let turn = parse_transcript_line("agent-1", 0, &text).expect("assistant turn");
        assert_eq!(turn.role, "assistant");
        assert_eq!(turn.native_message_id.as_deref(), Some("msg-9"));

        let calls = json!({
            "stream": { "id": "sess-1" },
            "payload": { "kind": "run", "event": {
                "kind": "assistant_tool_calls_committed",
                "tool_calls": [{
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": "write_file",
                    "args": "{\"path\":\"probe.txt\",\"content\":\"ok\"}",
                }],
            } },
        })
        .to_string();
        let turn = parse_transcript_line("agent-1", 1, &calls).expect("tool call turn");
        match &turn.blocks[0] {
            TurnBlock::ToolUse { id, name, input } => {
                // The call id is what tool results reference.
                assert_eq!(id.as_deref(), Some("call_1"));
                assert_eq!(name, "write_file");
                assert_eq!(input["path"], "probe.txt");
            }
            other => panic!("expected a tool use block, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_arguments_survive_unparseable_json() {
        let call = json!({ "args": "not json" });
        assert_eq!(muse_tool_call_input(&call), json!("not json"));
    }

    #[test]
    fn parses_tool_results_keyed_by_call_id() {
        let line = json!({
            "stream": { "id": "sess-1" },
            "payload": { "kind": "run", "event": {
                "kind": "tool_result_batch_committed",
                "results": [{
                    "tool_call_index": 0,
                    "tool_call_id": "call_1",
                    "text": "wrote 2 bytes",
                }],
            } },
        })
        .to_string();

        let turn = parse_transcript_line("agent-1", 2, &line).expect("tool result turn");
        assert_eq!(turn.role, "tool");
        match &turn.blocks[0] {
            TurnBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id.as_deref(), Some("call_1"));
                assert_eq!(content, "wrote 2 bytes");
                assert!(!is_error);
            }
            other => panic!("expected a tool result block, got {other:?}"),
        }
    }

    #[test]
    fn skips_bookkeeping_records() {
        for kind in [
            "hook_run_started",
            "hook_run_terminal",
            "reasoning_committed",
            "model_completed",
            "side_effect_intent",
            "goal_usage_attribution",
        ] {
            let line = json!({
                "stream": { "id": "s" },
                "payload": { "kind": "run", "event": { "kind": kind } },
            })
            .to_string();
            assert!(
                parse_transcript_line("agent-1", 0, &line).is_none(),
                "{kind} should not become a turn"
            );
        }
        assert!(parse_transcript_line("agent-1", 0, "not json").is_none());
    }

    #[test]
    fn reads_the_model_from_metadata_and_model_completions() {
        let metadata = json!({
            "payload_type": "runtime.session.metadata",
            "payload": { "kind": "metadata", "record": { "model_id": "muse-spark-1.2-contributor" } },
        })
        .to_string();
        assert_eq!(
            transcript_line_model(&metadata).as_deref(),
            Some("muse-spark-1.2-contributor")
        );

        let completed = json!({
            "payload": { "kind": "run", "event": { "kind": "model_completed", "model": "muse-spark-1.2" } },
        })
        .to_string();
        assert_eq!(
            transcript_line_model(&completed).as_deref(),
            Some("muse-spark-1.2")
        );

        let unrelated = json!({ "payload": { "kind": "run", "event": { "kind": "started" } } });
        assert_eq!(transcript_line_model(&unrelated.to_string()), None);
    }

    #[test]
    fn lifecycle_events_cover_turn_start_and_interruption() {
        let started = json!({
            "payload": { "kind": "run", "event": { "kind": "started", "prompt": "go" } },
        })
        .to_string();
        assert_eq!(
            parse_transcript_lifecycle_event(&started),
            Some(TranscriptLifecycleEvent::TurnStarted)
        );

        let cancelled = json!({
            "payload": { "kind": "run", "event": { "kind": "terminal", "terminal": "cancelled" } },
        })
        .to_string();
        assert_eq!(
            parse_transcript_lifecycle_event(&cancelled),
            Some(TranscriptLifecycleEvent::Interrupted)
        );

        let completed = json!({
            "payload": { "kind": "run", "event": { "kind": "terminal", "terminal": "completed" } },
        })
        .to_string();
        assert_eq!(parse_transcript_lifecycle_event(&completed), None);
    }

    /// Installs the real integration against the real `muse` binary, so the
    /// live end-to-end harness drives the shipped code rather than a copy of it.
    /// Ignored by default: it shells out to `muse plugins install/approve`,
    /// which mutates the developer's Muse settings unless `XDG_CONFIG_HOME` and
    /// `XDG_DATA_HOME` are pointed elsewhere.
    ///
    /// ```sh
    /// QMUX_MUSE_HOME=/tmp/probe/home QMUX_MUSE_TEST_CLI=/path/to/qmux-cli \
    ///   cargo test muse_integration_installs_against_the_real_cli -- --ignored
    /// ```
    #[test]
    #[ignore = "requires the muse CLI and mutates its plugin registry"]
    fn muse_integration_installs_against_the_real_cli() {
        let cli = env::var("QMUX_MUSE_TEST_CLI")
            .expect("set QMUX_MUSE_TEST_CLI to the qmux CLI the shim should call");
        let binary = env::var("QMUX_MUSE_TEST_BINARY").unwrap_or_else(|_| "muse".to_string());
        ensure_muse_integration(&binary, Path::new(&cli)).expect("integration installs");

        let home = muse_integration_home().expect("integration home");
        assert!(home.join("qmux-muse-hook").is_file());
        assert!(home.join("installed.stamp").is_file());
        // A second call must be a no-op — the stamp is what keeps two
        // subprocesses off every launch.
        ensure_muse_integration(&binary, Path::new(&cli)).expect("second call is idempotent");
    }

    /// Parses a session log Muse actually wrote, rather than the hand-built
    /// records the unit tests above use. Ignored by default because it needs a
    /// real run to point at.
    ///
    /// ```sh
    /// XDG_DATA_HOME=/tmp/probe/data QMUX_MUSE_TEST_SESSION=<uuid> \
    ///   cargo test parses_a_real_muse_session_log -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires a session log from a real muse run"]
    fn parses_a_real_muse_session_log() {
        let session_id =
            env::var("QMUX_MUSE_TEST_SESSION").expect("set QMUX_MUSE_TEST_SESSION to a session id");
        let path = muse_session_transcript_path(&session_id)
            .expect("the session log is discoverable from its id alone");
        let contents = fs::read_to_string(&path).expect("session log is readable");

        let turns: Vec<Turn> = contents
            .lines()
            .enumerate()
            .filter_map(|(index, line)| parse_transcript_line("agent-1", index, line))
            .collect();
        let roles: Vec<&str> = turns.iter().map(|turn| turn.role.as_str()).collect();
        println!("{} turns from {}: {roles:?}", turns.len(), path.display());

        assert!(roles.contains(&"user"), "no user turn: {roles:?}");
        assert!(roles.contains(&"assistant"), "no assistant turn: {roles:?}");
        assert!(roles.contains(&"tool"), "no tool result turn: {roles:?}");
        assert!(
            turns.iter().all(|turn| turn.timestamp.is_some_and(|ms| {
                // Microsecond stamps read as milliseconds would land in 1970.
                ms > 1_600_000_000_000
            })),
            "a turn carried an implausible timestamp"
        );
        // Tool results must reference a call id an earlier tool use emitted, or
        // the timeline cannot pair them.
        let used: Vec<&str> = turns
            .iter()
            .flat_map(|turn| &turn.blocks)
            .filter_map(|block| match block {
                TurnBlock::ToolUse { id, .. } => id.as_deref(),
                _ => None,
            })
            .collect();
        for block in turns.iter().flat_map(|turn| &turn.blocks) {
            if let TurnBlock::ToolResult {
                tool_use_id: Some(id),
                ..
            } = block
            {
                assert!(used.contains(&id.as_str()), "unpaired tool result {id}");
            }
        }
    }

    #[test]
    fn session_discovery_finds_the_newest_day_without_walking_history() {
        let root = env::temp_dir().join(format!("qmux-muse-sessions-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let sessions = root.join("muse").join("sessions");
        // Two years of history, plus today's session in the newest directory.
        for (year, month, day) in [
            ("2024", "01", "01"),
            ("2025", "06", "15"),
            ("2026", "08", "07"),
            ("2026", "08", "08"),
        ] {
            fs::create_dir_all(sessions.join(year).join(month).join(day)).unwrap();
        }
        let live = sessions.join("2026/08/08").join("live-session");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("session.jsonl"), "{}\n").unwrap();
        // A session filed under an old date is deliberately out of reach: the
        // bounded walk is what keeps the retry loop cheap.
        let ancient = sessions.join("2024/01/01").join("ancient-session");
        fs::create_dir_all(&ancient).unwrap();
        fs::write(ancient.join("session.jsonl"), "{}\n").unwrap();

        // SAFETY: single-threaded test process; the variable is restored below.
        let previous = env::var_os("XDG_DATA_HOME");
        unsafe { env::set_var("XDG_DATA_HOME", &root) };
        let found = muse_session_transcript_path("live-session");
        let skipped = muse_session_transcript_path("ancient-session");
        match previous {
            Some(value) => unsafe { env::set_var("XDG_DATA_HOME", value) },
            None => unsafe { env::remove_var("XDG_DATA_HOME") },
        }

        assert_eq!(found, Some(live.join("session.jsonl")));
        assert_eq!(skipped, None, "the walk must stay bounded");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn claiming_a_binding_keeps_the_directory_it_was_launched_in() {
        let home = env::temp_dir().join(format!("qmux-muse-claim-{}", std::process::id()));
        let dir = home.join("bindings");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pane-1.json");
        fs::write(
            &path,
            json!({
                "paneId": "pane-1",
                "agentId": "agent-1",
                // The launch cwd, which can differ from the agent's worktree.
                "cwd": "/explicit/launch/dir",
                "canonicalCwd": "/private/explicit/launch/dir",
                "sessionId": Value::Null,
                "sock": "/tmp/qmux.sock",
                "token": "tok",
                "updatedAt": 1,
            })
            .to_string(),
        )
        .unwrap();

        let previous = env::var_os("QMUX_MUSE_HOME");
        // SAFETY: single-threaded test process; the variable is restored below.
        unsafe { env::set_var("QMUX_MUSE_HOME", &home) };
        let result = stamp_muse_binding_session("pane-1", "session-a");
        match previous {
            Some(value) => unsafe { env::set_var("QMUX_MUSE_HOME", value) },
            None => unsafe { env::remove_var("QMUX_MUSE_HOME") },
        }
        result.expect("binding is claimed");

        let document: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(document["sessionId"], "session-a");
        // Rebuilding from the agent record would have replaced these with the
        // worktree directory, which Muse never reports.
        assert_eq!(document["cwd"], "/explicit/launch/dir");
        assert_eq!(document["canonicalCwd"], "/private/explicit/launch/dir");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "a rewritten binding still holds a pane token"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn subagent_payloads_are_recognized_without_a_recorded_main_session() {
        // Muse reports subagent lifecycle from the child's point of view.
        assert!(payload_names_a_subagent(&json!({
            "hook_event_name": "SubagentStart",
            "subagent_id": "plugin:tbh-reminders:goal-reminder",
            "child_session_id": "child-1",
            "session_id": "child-1",
        })));
        assert!(payload_names_a_subagent(&json!({
            "child_session_id": "child-1",
            "session_id": "child-1",
        })));
        // A main-session payload names no subagent.
        assert!(!payload_names_a_subagent(&json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "main-1",
            "prompt": "hi",
        })));
        assert!(!payload_names_a_subagent(&json!({})));
    }

    #[test]
    fn only_main_session_hooks_may_name_an_unbound_pane() {
        // The resume path: `muse resume <id>` fires no SessionStart, so the
        // first prompt is where identity comes back.
        assert!(hook_can_define_the_main_session("UserPromptSubmit"));
        assert!(hook_can_define_the_main_session("Stop"));
        // A subagent's tool hooks are indistinguishable from the parent's, so
        // they must never bind the pane to a child session.
        for event in ["PreToolUse", "PostToolUse", "PermissionRequest"] {
            assert!(
                !hook_can_define_the_main_session(event),
                "{event} must not define the main session"
            );
        }
        for event in ["SubagentStart", "SubagentStop"] {
            assert!(!hook_can_define_the_main_session(event));
        }
        // SessionStart records the session in its own arm instead.
        assert!(!hook_can_define_the_main_session("SessionStart"));
    }

    #[test]
    fn composer_policy_queues_running_panes() {
        let policy = MuseAdapter::new(&test_config()).composer_policy();
        assert!(policy.can_send(AgentStatus::Idle));
        assert!(policy.should_queue(AgentStatus::Running));
        assert!(policy.can_steer(AgentStatus::Running));
        assert!(!policy.can_send(AgentStatus::Running));
    }
}
