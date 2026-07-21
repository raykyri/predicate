use super::{
    AdapterNotification, AdapterNotificationOutcome, AgentAdapter, ComposerPolicy, LaunchEnv,
    PrepareShellAgentLaunchRequest, PreparedShellAgentLaunch, ShellCommandIntegration,
    SpawnAgentRequest, TranscriptLifecycleEvent, ensure_on_path, hook_transcript_path_acceptable,
    prepared_shell_agent, record_shell_session_lineage, reusable_session_agent, shell_quote_arg,
    shell_quote_path,
};
use crate::config::QmuxConfig;
use crate::events::QmuxEvent;
use crate::pty::{InitialPaneSize, PtySpawnSpec, qmux_pane_envs, recoverable_dir, spawn_pty};
use crate::state::{AppState, PaneInfo, PaneKind};
use crate::transcript::{Turn, TurnBlock, start_transcript_tail};
use crate::turn_queue::{IdleResolution, advance_after_idle, is_shell_escape_turn};
use crate::workspace::{
    AgentInfo, AgentStatus, PrepareAgentWorkspaceRequest, attach_agent_pane, mark_agent_failed,
    mark_agent_spawn_failed, prepare_agent_workspace,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The xAI Grok Build lifecycle hook events qMux installs. Grok discovers global
/// hooks from `~/.grok/hooks/*.json` (not `user-settings.json`); each entry runs a
/// command per event with the event JSON on stdin. qMux drives the agent timeline
/// from the same core events it uses for Claude. Grok has no pre-decision permission
/// event, but its passive `PermissionDenied` event still keeps status/activity honest.
const GROK_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionDenied",
    "Stop",
    "StopFailure",
    "Notification",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "PostCompact",
    "SessionEnd",
];

/// Adapter for the xAI Grok Build CLI. Grok ships a Claude-compatible hook system
/// (shell commands run at lifecycle events, event JSON on stdin), so qMux integrates
/// it like its Claude and Codex adapters rather than like OpenCode: a qMux-managed
/// hook file is installed at `~/.grok/hooks/qmux.json` and a shim forwards each
/// lifecycle event back to qMux via `qmux notify <event>`. The hook command no-ops
/// outside qMux (it checks for the `QMUX_*` env vars only qMux-launched panes set),
/// so standalone `grok` runs are unaffected. Agent status (running, idle, awaiting
/// permission) is driven by these hooks; the transcript timeline binds to the
/// transcript path the `SessionStart` hook reports when Grok provides one.
#[derive(Clone, Debug)]
pub struct GrokAdapter {
    binary: String,
}

impl GrokAdapter {
    pub fn new(config: &QmuxConfig) -> Self {
        Self {
            binary: config.grok_binary(),
        }
    }

    fn ensure_binary(&self) -> Result<String, String> {
        let binary = ensure_on_path(&self.binary).ok_or_else(|| {
            format!(
                "Grok adapter binary '{}' was not found on PATH or standard macOS tool paths. Install the Grok CLI or update adapters.grok.binary in qmux.config.json.",
                self.binary
            )
        })?;
        Ok(binary.display().to_string())
    }

    /// The qMux-managed JSONL transcript fallback path for an agent, used when Grok's
    /// `SessionStart` hook does not report a transcript path of its own. It is shaped
    /// for `parse_transcript_line` and tailed with the same pipeline used for Claude,
    /// Codex, and OpenCode.
    fn transcript_path_for(state: &AppState, agent_id: &str) -> PathBuf {
        state
            .config()
            .workspace_root
            .join(".qmux")
            .join("grok")
            .join(format!("{agent_id}.jsonl"))
    }
}

impl AgentAdapter for GrokAdapter {
    fn id(&self) -> &'static str {
        "grok"
    }

    fn display_name(&self) -> &'static str {
        "Grok"
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
            command_name: "grok",
            adapter_id: self.id(),
        }]
    }

    fn shell_resume_command(&self, session_id: &str) -> Option<String> {
        Some(format!("grok --resume {}", shell_quote_arg(session_id)))
    }

    fn ingest_notification(
        &self,
        state: &AppState,
        notification: AdapterNotification,
    ) -> Result<AdapterNotificationOutcome, String> {
        self.ingest_grok_notification(state, notification)
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

impl GrokAdapter {
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
                "this Grok session isn't ready to fork yet (no session id); send a turn first"
                    .to_string()
            })?;
        Ok(build_grok_fork_args(
            cwd,
            source.model.as_deref(),
            session_id,
            prompt,
        ))
    }

    fn spawn_pane(&self, state: &AppState, request: SpawnAgentRequest) -> Result<PaneInfo, String> {
        let binary = self.ensure_binary()?;
        let _options = GrokLaunchOptions::from_value(request.options)?;
        ensure_grok_integration()?;

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
                "Grok working directory {} does not exist",
                cwd.display()
            ));
        }

        let has_initial_prompt = prompt_has_initial_text(&request.prompt);
        let args = build_grok_args(&cwd, request.model.as_deref(), &request.prompt);

        let pane_id = state.next_id("pane");
        let mut envs = qmux_pane_envs(state, &pane_id)?;
        envs.push(("QMUX_AGENT_ID".to_string(), agent.id.clone()));
        envs.push(("QMUX_CLI".to_string(), qmux_cli_path()?));

        // SessionStart may fire immediately after exec. Reserve the pane binding
        // before spawn so the control socket's pane/agent scope check accepts that
        // first hook instead of losing the session and transcript identity.
        attach_grok_agent_pane(state, &agent.id, pane_id.clone(), has_initial_prompt)?;
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
        ensure_grok_integration()?;
        let cwd = recoverable_dir(&agent.worktree_dir).ok_or_else(|| {
            format!(
                "agent worktree {} no longer exists; relaunch manually",
                agent.worktree_dir
            )
        })?;

        // Resume the recorded session when there is one (`grok --resume <id>`;
        // sessions live under ~/.grok/sessions), so a recovered pane continues the
        // prior conversation instead of starting over.
        let (args, resumed) =
            build_grok_resume_args(&cwd, agent.model.as_deref(), agent.session_id.as_deref());

        let mut envs = qmux_pane_envs(state, &pane.id)?;
        envs.push(("QMUX_AGENT_ID".to_string(), agent.id.clone()));
        envs.push(("QMUX_CLI".to_string(), qmux_cli_path()?));

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

        // A recovered Grok process launches without an inline prompt, so it is ready
        // once the TUI appears. The first real prompt/tool hook promotes it to Running.
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

    /// Forks a Grok session into a new independent session using
    /// `--resume <source> --fork-session`, optionally in a qMux worktree.
    pub fn fork_pane(
        &self,
        state: &AppState,
        source: &AgentInfo,
        use_worktree: bool,
        prompt: Option<&str>,
    ) -> Result<(PaneInfo, AgentInfo), String> {
        let binary = self.ensure_binary()?;
        ensure_grok_integration()?;
        let session_id = source
            .session_id
            .clone()
            .map(|session| session.trim().to_string())
            .filter(|session| !session.is_empty())
            .ok_or_else(|| {
                "this Grok session isn't ready to fork yet (no session id); send a turn first"
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
        let args = build_grok_fork_args(
            &cwd,
            agent.model.as_deref(),
            &session_id,
            has_initial_prompt.then_some(prompt),
        );

        let pane_id = state.next_id("pane");
        let mut envs = qmux_pane_envs(state, &pane_id)?;
        envs.push(("QMUX_AGENT_ID".to_string(), agent.id.clone()));
        envs.push(("QMUX_CLI".to_string(), qmux_cli_path()?));

        // Reserve the binding before spawn so a fast SessionStart hook passes the
        // authenticated pane/agent scope check. Roll it back if process creation fails.
        attach_grok_agent_pane(state, &agent.id, pane_id.clone(), has_initial_prompt)?;
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
        ensure_grok_integration()?;

        if !state.pane_exists(&request.pane_id)? {
            return Err(format!("pane {} was not found", request.pane_id));
        }

        let shell_cwd = PathBuf::from(&request.cwd);
        if !shell_cwd.is_dir() {
            return Err(format!(
                "Grok working directory {} does not exist",
                shell_cwd.display()
            ));
        }
        let agent_cwd = grok_effective_cwd(&shell_cwd, &request.args)?;

        // A restart-driven resume (`grok --resume <id>`) rebinds the original agent
        // for that session instead of minting a duplicate; any other invocation starts
        // a fresh agent in the current directory.
        let cwd_str = agent_cwd.display().to_string();
        let pane_group_id = state
            .pane_group_id(&request.pane_id)?
            .ok_or_else(|| format!("pane {} was not found", request.pane_id))?;
        let resume_session_id = grok_resume_session_id(&request.args).map(str::to_string);
        let fork_point = grok_fork_source_session_id(&request.args).map(str::to_string);
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
                        model: None,
                        effort: None,
                        // Typing `grok` in a shell runs in the current directory; no worktree.
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
        let agent = attach_grok_agent_pane(
            state,
            &agent.id,
            request.pane_id.clone(),
            args_contain_prompt(&request.args),
        )?;

        let args = build_grok_args_from_shell(&shell_cwd, &request.args);
        let mut envs = qmux_pane_envs(state, &request.pane_id)?;
        envs.push(("QMUX_AGENT_ID".to_string(), agent.id.clone()));
        envs.push(("QMUX_CLI".to_string(), qmux_cli_path()?));
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

    fn ingest_grok_notification(
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
            "UserPromptSubmit"
                | "PreToolUse"
                | "PostToolUse"
                | "PostToolUseFailure"
                | "PermissionDenied"
                | "Stop"
                | "StopFailure"
                | "Notification"
                | "PreCompact"
                | "PostCompact"
        ) && let Some(current) = agent.as_ref()
        {
            adopt_forked_grok_session_identity(state, current, &notification.payload)?;
        }
        let event_type = match hook_event.as_str() {
            "SessionStart" => {
                if let Some(current) = agent.as_ref() {
                    let session_id = super::string_field(&notification.payload, "session_id")
                        .or_else(|| super::string_field(&notification.payload, "sessionId"));
                    // A fork must adopt Grok's newly-created child identity. Some
                    // versions can briefly report the resumed source identity at
                    // SessionStart; reject that entire payload, including its
                    // transcript path, rather than merely dropping the id and then
                    // tailing the source session from the forked pane.
                    let stale_fork_payload = current
                        .fork_point
                        .as_deref()
                        .is_some_and(|fork_point| session_id.as_deref() == Some(fork_point));
                    let session_cwd = super::string_field(&notification.payload, "cwd")
                        .or_else(|| super::string_field(&notification.payload, "workspaceRoot"))
                        .filter(|cwd| grok_session_cwd_acceptable(&current.worktree_dir, cwd));
                    // Grok's SessionStart hook reports the rollout transcript path when
                    // it has one (Claude-compatible). Prefer it so the timeline tails
                    // Grok's own transcript.
                    let hook_transcript_path =
                        super::string_field(&notification.payload, "transcript_path")
                            .or_else(|| {
                                super::string_field(&notification.payload, "transcriptPath")
                            })
                            // The hook arrives over the control socket under the pane's
                            // token, so a prompt-injected agent can forge this path.
                            // Reject a non-.jsonl or non-sibling path before tailing it;
                            // a rejected path falls back to the qMux-managed transcript
                            // path under workspace_root.
                            .filter(|candidate| {
                                hook_transcript_path_acceptable(
                                    current.transcript_path.as_deref(),
                                    candidate,
                                )
                            });
                    // Current Grok versions report sessionId + cwd, not a transcript
                    // path. Bind their native chat history directly so the right pane
                    // does not wait forever on the legacy qMux-managed fallback file.
                    let native_transcript_path = session_id
                        .as_deref()
                        .zip(session_cwd.as_deref())
                        .and_then(|(session_id, cwd)| {
                            grok_home().ok().and_then(|home| {
                                grok_session_transcript_path(&home, cwd, session_id)
                            })
                        })
                        .map(|path| path.display().to_string());
                    let fallback_transcript_path = Self::transcript_path_for(state, &current.id)
                        .display()
                        .to_string();
                    // Field-scoped mutation, not a full-struct `update_agent`: this
                    // freshly spawned process's pane is being bound by attach_agent_pane
                    // on another thread, and a stale-snapshot write here would race it —
                    // wiping either the pane_id it set or the session_id we set.
                    let updated = if stale_fork_payload {
                        None
                    } else {
                        state.mutate_agent(&current.id, |agent| {
                            if let Some(session_id) = session_id {
                                agent.session_id = Some(session_id);
                            }
                            // Only overwrite a recorded path when this event actually carries
                            // one. A late/duplicate SessionStart that omits the field must not
                            // rebind the tail out from under a running transcript, which would
                            // silently freeze the timeline. When nothing is recorded yet, bind
                            // the qMux-managed fallback so a tail still starts and picks up
                            // content once it appears.
                            if let Some(transcript_path) =
                                hook_transcript_path.or(native_transcript_path)
                            {
                                agent.transcript_path = Some(transcript_path);
                            } else if agent.transcript_path.is_none() {
                                agent.transcript_path = Some(fallback_transcript_path);
                            }
                            // A session starting doesn't mean a turn is running. Keep status
                            // unchanged here; the first real prompt/tool hook promotes the
                            // agent to Running.
                        })?
                    };

                    // Start tailing the bound transcript. The file may not exist yet, so
                    // the tail waits for it to appear rather than erroring.
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
                    let prompt = super::string_field(&notification.payload, "prompt")
                        .or_else(|| super::string_field(&notification.payload, "input"));
                    if !prompt.as_deref().is_some_and(is_shell_escape_turn) {
                        agent.status = AgentStatus::Running;
                        state.set_agent_status(&agent.id, agent.status)?;
                    }
                    // A new main-agent turn supersedes the previous one, so
                    // subagents tracked for it can no longer gate completion.
                    // Clearing here also self-heals a counter wedged by a lost
                    // SubagentStop, which would otherwise suppress every future
                    // Stop.
                    if super::subagent_id(&notification.payload).is_none() {
                        state.clear_agent_subagents(&agent.id);
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
            "PostToolUse" | "PostToolUseFailure" => {
                if let Some(agent) = agent.as_mut() {
                    agent.status = AgentStatus::Running;
                    state.set_agent_status(&agent.id, agent.status)?;
                }
                "agent.tool_result"
            }
            // Grok reports denial after the decision; it does not expose Claude's
            // pre-decision PermissionRequest event. The turn remains active so mark
            // it Running rather than stranding a stale AwaitingPermission state.
            "PermissionDenied" => {
                if let Some(agent) = agent.as_mut() {
                    agent.status = AgentStatus::Running;
                    state.set_agent_status(&agent.id, agent.status)?;
                }
                "agent.permission_denied"
            }
            "PreCompact" => {
                if let Some(agent) = agent.as_mut() {
                    agent.status = AgentStatus::Running;
                    state.set_agent_status(&agent.id, agent.status)?;
                }
                "agent.compacting"
            }
            "PostCompact" => {
                if let Some(agent) = agent.as_mut() {
                    agent.status = AgentStatus::Running;
                    state.set_agent_status(&agent.id, agent.status)?;
                }
                "agent.compacted"
            }
            "Notification" => "agent.notification",
            "SubagentStart" => {
                if let Some(agent) = agent.as_mut() {
                    state.agent_subagent_started(
                        &agent.id,
                        super::subagent_id(&notification.payload),
                    )?;
                    agent.status = AgentStatus::Running;
                    state.set_agent_status(&agent.id, agent.status)?;
                }
                "agent.subagent_started"
            }
            "SubagentStop" => {
                if let Some(agent) = agent.as_mut() {
                    let tracked = state
                        .agent_subagent_stopped(
                            &agent.id,
                            super::subagent_id(&notification.payload),
                        )?
                        .is_some();
                    // A late or duplicate stop with nothing tracked must not
                    // drag a settled agent back to Running.
                    if tracked {
                        agent.status = AgentStatus::Running;
                        state.set_agent_status(&agent.id, agent.status)?;
                    }
                }
                "agent.subagent_stopped"
            }
            "SessionEnd" => {
                if let Some(agent) = agent.as_ref() {
                    state.clear_agent_subagents(&agent.id);
                }
                "agent.session_end"
            }
            "Stop" | "StopFailure" => {
                let waiting_on_subagents = if let Some(agent) = agent.as_mut() {
                    if state.agent_has_active_subagents(&agent.id)? {
                        agent.status = AgentStatus::Running;
                        state.set_agent_status(&agent.id, agent.status)?;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                let drained = if waiting_on_subagents {
                    false
                } else if let Some(agent) = agent.as_mut() {
                    finish_agent_after_stop(state, agent)?
                } else {
                    false
                };
                if waiting_on_subagents || drained {
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
        // surgically instead of refetching the entire agent list on every hook event.
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
struct GrokLaunchOptions {}

impl GrokLaunchOptions {
    fn from_value(value: Value) -> Result<Self, String> {
        if value.is_null() {
            return Ok(Self::default());
        }
        serde_json::from_value(value).map_err(|err| format!("invalid Grok adapter options: {err}"))
    }
}

/// Builds the argument list for a qMux-launched Grok process. Uses the xAI Grok
/// Build CLI contract: `--cwd <dir>` for the working directory, `--model <model>`,
/// and the initial prompt as a trailing positional argument (which starts the
/// interactive TUI and submits the prompt immediately — unlike `-p/--prompt`, which
/// runs headless and exits, so it can't back an interactive pane).
fn build_grok_args(cwd: &Path, model: Option<&str>, prompt: &str) -> Vec<String> {
    let mut args = Vec::new();

    // The working directory is passed explicitly so Grok runs in the agent's cwd
    // even when the process cwd differs (e.g. worktree paths).
    args.push("--cwd".to_string());
    args.push(cwd.display().to_string());

    if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }

    // The interactive initial prompt is a trailing positional, so it must come after
    // all flags. Delimit it with `--` so a prompt that starts with `-` (e.g. queued
    // text delivered to a new session) is parsed as the positional prompt rather
    // than as a Grok flag; mirrors the Claude and Codex launch paths.
    let prompt = prompt.trim();
    if !prompt.is_empty() {
        args.push("--".to_string());
        args.push(prompt.to_string());
    }

    args
}

/// Builds args for recovering a Grok agent. With a recorded session id, resumes it
/// via `grok --cwd <dir> [--model <m>] --resume <id>` and returns `true`; without
/// one, falls back to a fresh interactive launch and returns `false`.
fn build_grok_resume_args(
    cwd: &Path,
    model: Option<&str>,
    session_id: Option<&str>,
) -> (Vec<String>, bool) {
    let Some(session_id) = session_id
        .map(str::trim)
        .filter(|session_id| !session_id.is_empty())
    else {
        return (build_grok_args(cwd, model, ""), false);
    };

    let mut args = build_grok_args(cwd, model, "");
    args.push("--resume".to_string());
    args.push(session_id.to_string());
    (args, true)
}

fn build_grok_fork_args(
    cwd: &Path,
    model: Option<&str>,
    session_id: &str,
    prompt: Option<&str>,
) -> Vec<String> {
    let mut args = build_grok_args(cwd, model, "");
    args.push("--resume".to_string());
    args.push(session_id.to_string());
    args.push("--fork-session".to_string());
    if let Some(prompt) = prompt.map(str::trim).filter(|prompt| !prompt.is_empty()) {
        args.push("--".to_string());
        args.push(prompt.to_string());
    }
    args
}

/// Builds args for a `grok ...` invocation typed in a shell pane. The user's own
/// args are forwarded verbatim; a `--cwd` is supplied only when the user did not
/// pass one so the agent still runs in the pane's cwd.
fn build_grok_args_from_shell(cwd: &Path, tail_args: &[String]) -> Vec<String> {
    let mut args = Vec::new();
    if !args_contain_directory(tail_args) {
        args.push("--cwd".to_string());
        args.push(cwd.display().to_string());
    }
    args.extend(tail_args.iter().cloned());
    args
}

/// Ensures the qMux Grok hook integration is present. Writes are skipped when
/// the shim script and `hooks/qmux.json` already match the expected content.
/// Best-effort cleanup also strips stale qMux entries from the legacy
/// `user-settings.json` location (Grok never loads hooks from there).
fn ensure_grok_integration() -> Result<(), String> {
    let grok_home = grok_home()?;
    write_grok_integration_files(&grok_home)
}

/// The Grok config home. Honors a `GROK_HOME` override (used by tests and unusual
/// installs); otherwise `~/.grok`, where Grok keeps its user settings and sessions.
fn grok_home() -> Result<PathBuf, String> {
    env::var_os("GROK_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".grok")))
        .ok_or_else(|| "GROK_HOME and HOME are not set; cannot configure Grok hooks".to_string())
}

fn qmux_cli_path() -> Result<String, String> {
    env::current_exe()
        .map(|path| path.display().to_string())
        .map_err(|err| format!("failed to resolve qmux executable for Grok hooks: {err}"))
}

/// Path of the qMux-owned global hook file under `$GROK_HOME/hooks/`. Grok merges
/// every `*.json` in that directory as always-trusted global hooks.
fn grok_hooks_file_path(grok_home: &Path) -> PathBuf {
    grok_home.join("hooks").join("qmux.json")
}

/// Native Grok conversations live at
/// `$GROK_HOME/sessions/<percent-encoded-cwd>/<session-id>/chat_history.jsonl`.
/// Very long cwd values use a hashed group directory, so scan the one-level group
/// list as a fallback when the conventional path is not present yet.
fn grok_session_transcript_path(grok_home: &Path, cwd: &str, session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty()
        || session_id.len() > 128
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return None;
    }

    let sessions_root = grok_home.join("sessions");
    let encoded_cwd = percent_encode_grok_cwd(cwd);
    let conventional = sessions_root
        .join(&encoded_cwd)
        .join(session_id)
        .join("chat_history.jsonl");
    if conventional.parent().is_some_and(Path::is_dir) {
        return Some(conventional);
    }

    if let Ok(groups) = fs::read_dir(&sessions_root) {
        for group in groups.flatten() {
            let Ok(file_type) = group.file_type() else {
                continue;
            };
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            // Hashed long-cwd group names carry the original directory in `.cwd`.
            // Require that proof before accepting a non-conventional group; hook
            // payloads are agent-controlled and must not bind another project's
            // transcript merely by naming its session id.
            let Ok(recorded_cwd) = fs::read_to_string(group.path().join(".cwd")) else {
                continue;
            };
            if recorded_cwd.trim_end() != cwd {
                continue;
            }
            let candidate = group.path().join(session_id).join("chat_history.jsonl");
            if candidate.parent().is_some_and(Path::is_dir) {
                return Some(candidate);
            }
        }
    }

    // SessionStart normally fires after the session directory is created. If the
    // filesystem is briefly behind, return the predictable path and let the tail's
    // normal warm-up polling wait for chat_history.jsonl to appear.
    (encoded_cwd.len() <= 255).then_some(conventional)
}

/// Grok reports the working directory used to group native sessions. Hook payloads
/// are process-controlled, so only accept the agent's recorded workspace (including
/// symlink/`..` spellings that canonicalize to it) before deriving a transcript path.
fn grok_session_cwd_acceptable(expected: &str, reported: &str) -> bool {
    if expected == reported {
        return true;
    }
    match (fs::canonicalize(expected), fs::canonicalize(reported)) {
        (Ok(expected), Ok(reported)) => expected == reported,
        _ => false,
    }
}

/// One-shot recovery when a native fork's SessionStart briefly reported the source
/// identity. Every regular Grok hook carries the active session id and cwd, so the
/// first child event can safely bind the fork after both values pass the same
/// confinement checks as a normal SessionStart.
fn adopt_forked_grok_session_identity(
    state: &AppState,
    current: &AgentInfo,
    payload: &Value,
) -> Result<(), String> {
    let Some(fork_point) = current.fork_point.as_deref() else {
        return Ok(());
    };
    let is_unbound_or_stale =
        |session_id: Option<&str>| session_id.is_none() || session_id == Some(fork_point);
    if !is_unbound_or_stale(current.session_id.as_deref()) {
        return Ok(());
    }
    let Some(session_id) = super::string_field(payload, "session_id")
        .or_else(|| super::string_field(payload, "sessionId"))
        .filter(|session_id| session_id != fork_point)
    else {
        return Ok(());
    };
    let Some(session_cwd) = super::string_field(payload, "cwd")
        .or_else(|| super::string_field(payload, "workspaceRoot"))
        .filter(|cwd| grok_session_cwd_acceptable(&current.worktree_dir, cwd))
    else {
        return Ok(());
    };
    let Some(transcript_path) = grok_home()
        .ok()
        .and_then(|home| grok_session_transcript_path(&home, &session_cwd, &session_id))
        .map(|path| path.display().to_string())
    else {
        return Ok(());
    };

    let updated = state.mutate_agent(&current.id, |agent| {
        if is_unbound_or_stale(agent.session_id.as_deref()) {
            agent.session_id = Some(session_id.clone());
            agent.transcript_path = Some(transcript_path.clone());
        }
    })?;
    if updated
        .as_ref()
        .and_then(|agent| agent.transcript_path.as_deref())
        == Some(transcript_path.as_str())
    {
        start_transcript_tail(
            state.clone(),
            current.id.clone(),
            transcript_path,
            "grok".to_string(),
        );
    }
    Ok(())
}

fn percent_encode_grok_cwd(cwd: &str) -> String {
    let mut encoded = String::with_capacity(cwd.len());
    for byte in cwd.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(*byte as char);
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            encoded.push('%');
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    encoded
}

fn write_grok_integration_files(grok_home: &Path) -> Result<(), String> {
    let qmux_dir = grok_home.join("qmux");
    fs::create_dir_all(&qmux_dir)
        .map_err(|err| format!("failed to create {}: {err}", qmux_dir.display()))?;

    let shim_path = qmux_dir.join("qmux-grok-hook");

    // Only rewrite the shim when missing, content differs, or permissions are wrong.
    if !shim_is_up_to_date(&shim_path) {
        fs::write(&shim_path, grok_hook_shim())
            .map_err(|err| format!("failed to write {}: {err}", shim_path.display()))?;
        fs::set_permissions(&shim_path, fs::Permissions::from_mode(0o755))
            .map_err(|err| format!("failed to chmod {}: {err}", shim_path.display()))?;
    }

    let hooks_dir = grok_home.join("hooks");
    fs::create_dir_all(&hooks_dir)
        .map_err(|err| format!("failed to create {}: {err}", hooks_dir.display()))?;

    let hooks_path = grok_hooks_file_path(grok_home);
    let desired = grok_hooks_file_contents(&shim_path);
    // qMux fully owns this file; rewrite only when content drifts (missing events,
    // stale shim path, leftover matchers from older installs).
    if !hooks_file_is_up_to_date(&hooks_path, &desired) {
        fs::write(&hooks_path, desired)
            .map_err(|err| format!("failed to write {}: {err}", hooks_path.display()))?;
    }

    // Older qMux versions installed hooks into `user-settings.json`, which Grok
    // does not load. Strip our entries so the dead config does not confuse users
    // or re-surface after a manual merge; failures here are non-fatal.
    let _ = strip_stale_user_settings_hooks(grok_home, &shim_path);

    Ok(())
}

/// POSIX shim that forwards a Grok lifecycle event to `qmux notify <event>`. No-ops
/// (exit 0) unless launched inside a qMux pane, so a standalone `grok` run that
/// inherits the globally-installed hook is unaffected. The event JSON Grok writes to
/// the shim's stdin is passed through to qmux, which reads it as the hook payload.
fn grok_hook_shim() -> &'static str {
    r#"#!/bin/sh
event="${1:-}"
if [ -z "$event" ]; then
  exit 0
fi
if [ -z "${QMUX_SOCK:-}" ] || [ -z "${QMUX_TOKEN:-}" ] || [ -z "${QMUX_PANE_ID:-}" ] || [ -z "${QMUX_AGENT_ID:-}" ] || [ -z "${QMUX_CLI:-}" ]; then
  exit 0
fi
exec "$QMUX_CLI" notify "$event"
"#
}

/// Builds the qMux-owned `hooks/qmux.json` document. Grok's lifecycle events
/// (`SessionStart`, `UserPromptSubmit`, `Stop`, …) reject a `matcher` field
/// (`LifecycleMatcherNotAllowed`), so entries omit it; an omitted matcher on tool
/// events matches every tool.
fn grok_hooks_document(shim_path: &Path) -> Value {
    let command_prefix = shell_quote_path(shim_path);
    let mut hooks = serde_json::Map::new();
    for event in GROK_HOOK_EVENTS {
        hooks.insert(
            event.to_string(),
            json!([
                {
                    "hooks": [
                        {
                            "type": "command",
                            "command": format!("{command_prefix} {event}"),
                        }
                    ]
                }
            ]),
        );
    }
    json!({ "hooks": hooks })
}

fn grok_hooks_file_contents(shim_path: &Path) -> String {
    // Pretty JSON with trailing newline so re-writes are stable and diffs are readable.
    let mut raw = serde_json::to_string_pretty(&grok_hooks_document(shim_path))
        .expect("hooks document is always serializable");
    raw.push('\n');
    raw
}

/// Whether a hook matcher entry is one qMux installed, i.e. one of its `command`s
/// runs the qMux Grok shim. Used when stripping stale entries from the legacy
/// `user-settings.json` path. Matches on filename so absolute paths from previous
/// installations are cleaned up too.
fn is_qmux_grok_hook_entry(entry: &Value, shim_path: &Path) -> bool {
    let needle = shim_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("qmux-grok-hook");
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| command.contains(needle))
            })
        })
}

/// Returns whether the shim script exists, has exactly the expected content, and
/// has all executable bits set.
fn shim_is_up_to_date(shim_path: &Path) -> bool {
    let Ok(content) = fs::read_to_string(shim_path) else {
        return false;
    };
    if content != grok_hook_shim() {
        return false;
    }
    match fs::metadata(shim_path) {
        Ok(meta) => (meta.permissions().mode() & 0o111) == 0o111,
        Err(_) => false,
    }
}

fn hooks_file_is_up_to_date(hooks_path: &Path, desired: &str) -> bool {
    fs::read_to_string(hooks_path).is_ok_and(|content| content == desired)
}

/// Removes qMux-owned hook entries from the legacy `user-settings.json` location.
/// Pre-fix qMux versions wrote hooks there, but Grok only discovers global hooks
/// from `~/.grok/hooks/*.json` (and Claude/Cursor compat paths). Leaving the stale
/// entries is harmless at runtime but confuses anyone inspecting settings; strip
/// them when present. Returns `Ok(true)` when the file was rewritten.
fn strip_stale_user_settings_hooks(grok_home: &Path, shim_path: &Path) -> Result<bool, String> {
    let settings_path = grok_home.join("user-settings.json");
    let Ok(raw) = fs::read_to_string(&settings_path) else {
        return Ok(false);
    };
    if raw.trim().is_empty() {
        return Ok(false);
    }
    let mut settings: Value = serde_json::from_str(&raw)
        .map_err(|err| format!("failed to parse {}: {err}", settings_path.display()))?;
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(false);
    };

    let mut changed = false;
    // Walk every event (including ones we no longer install, e.g. PermissionRequest)
    // so leftover qMux entries from older versions are fully removed.
    let event_keys: Vec<String> = hooks.keys().cloned().collect();
    for event in event_keys {
        let Some(list) = hooks.get_mut(&event).and_then(Value::as_array_mut) else {
            continue;
        };
        let before = list.len();
        list.retain(|item| !is_qmux_grok_hook_entry(item, shim_path));
        if list.len() != before {
            changed = true;
        }
        if list.is_empty() {
            hooks.remove(&event);
            changed = true;
        }
    }
    if hooks.is_empty()
        && let Some(obj) = settings.as_object_mut()
    {
        obj.remove("hooks");
        changed = true;
    }
    if !changed {
        return Ok(false);
    }

    let raw = serde_json::to_string_pretty(&settings)
        .map_err(|err| format!("failed to encode {}: {err}", settings_path.display()))?;
    fs::write(&settings_path, raw)
        .map_err(|err| format!("failed to write {}: {err}", settings_path.display()))?;
    Ok(true)
}

fn prompt_has_initial_text(prompt: &str) -> bool {
    !prompt.trim().is_empty()
}

fn attach_grok_agent_pane(
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

/// Whether a manual `grok ...` invocation carries a prompt — either a headless
/// prompt flag (`-p`/`--prompt`/`--single <text>`) or a trailing positional (the
/// interactive initial message). Value-taking flags and their values are skipped so
/// `grok --model grok-4` or `grok --resume <id>` is treated as interactive (no
/// prompt). Erring toward "no prompt" is safe: the agent starts idle and the first
/// real turn promotes it.
fn args_contain_prompt(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return args.get(index + 1).is_some_and(|value| !value.is_empty());
        }
        // Headless prompt flags carry the prompt as their value.
        if matches!(
            arg.as_str(),
            "-p" | "--prompt" | "--single" | "--prompt-file" | "--prompt-json"
        ) {
            return true;
        }
        if arg.starts_with("--prompt=")
            || arg.starts_with("--single=")
            || arg.starts_with("--prompt-file=")
            || arg.starts_with("--prompt-json=")
        {
            return arg
                .split_once('=')
                .is_some_and(|(_, value)| !value.is_empty());
        }
        if grok_optional_value_flag(arg) {
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
        // A value-taking flag consumes the next arg, so that value isn't a prompt.
        if grok_value_flag(arg) {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        if grok_subcommand(arg) {
            return false;
        }
        // A bare positional is the interactive initial prompt.
        return true;
    }
    false
}

/// Extracts the session id from a `grok --resume <id>` (or `-r <id>` /
/// `--resume=<id>`) shell argument list, so a resume launch can rebind the original
/// agent. `None` when the invocation doesn't resume a specific session.
fn grok_resume_session_id(args: &[String]) -> Option<&str> {
    if args
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--fork-session")
    {
        return None;
    }
    grok_resume_argument_id(args)
}

fn grok_fork_source_session_id(args: &[String]) -> Option<&str> {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--fork-session")
        .then(|| grok_resume_argument_id(args))
        .flatten()
}

fn grok_resume_argument_id(args: &[String]) -> Option<&str> {
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
                .filter(|next| !next.starts_with('-'));
        }
        if let Some(value) = arg.strip_prefix("--resume=") {
            return (!value.is_empty()).then_some(value);
        }
        if grok_value_flag(arg)
            || (grok_optional_value_flag(arg)
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

/// Grok flags that take a separate value argument, so the following token is the
/// flag's value rather than a positional prompt. Inline `--flag=value` forms start
/// with `-` and are skipped by the generic flag check, so they need no entry here.
fn grok_value_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--agent"
            | "--agents"
            | "--allow"
            | "--best-of-n"
            | "--cwd"
            | "--debug-file"
            | "--deny"
            | "--disallowed-tools"
            | "--json-schema"
            | "--leader-socket"
            | "--model"
            | "-m"
            | "--max-turns"
            | "--base-url"
            | "-u"
            | "--api-key"
            | "-k"
            | "--max-tool-rounds"
            | "--session-id"
            | "-s"
            | "--output-format"
            | "--permission-mode"
            | "--reasoning-effort"
            | "--effort"
            | "--rules"
            | "--sandbox"
            | "--system-prompt-override"
            | "--tools"
            | "--worktree-ref"
            | "--ref"
    )
}

fn grok_optional_value_flag(arg: &str) -> bool {
    matches!(arg, "--resume" | "-r" | "--worktree" | "-w")
}

fn grok_subcommand(arg: &str) -> bool {
    matches!(
        arg,
        "agent"
            | "completions"
            | "dashboard"
            | "export"
            | "help"
            | "import"
            | "inspect"
            | "leader"
            | "login"
            | "logout"
            | "mcp"
            | "memory"
            | "models"
            | "plugin"
            | "sessions"
            | "setup"
            | "trace"
            | "update"
            | "version"
            | "worktree"
            | "wrap"
    )
}

/// Whether the user already supplied a `--cwd` so qMux does not add a duplicate one
/// when forwarding shell args.
fn args_contain_directory(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--cwd" || arg.starts_with("--cwd="))
}

/// The project Grok will actually operate on for a shell invocation. Keep this
/// separate from the process cwd: relative CLI paths retain normal shell semantics,
/// while the qmux agent identity, resume matching, and native transcript grouping
/// follow Grok's explicit `--cwd` override.
fn grok_effective_cwd(shell_cwd: &Path, args: &[String]) -> Result<PathBuf, String> {
    let mut requested = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            break;
        }
        if arg == "--cwd" {
            let value = args
                .get(index + 1)
                .filter(|value| !value.is_empty() && !value.starts_with('-'))
                .ok_or_else(|| "Grok --cwd requires a directory".to_string())?;
            requested = Some(PathBuf::from(value));
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--cwd=") {
            if value.is_empty() {
                return Err("Grok --cwd requires a directory".to_string());
            }
            requested = Some(PathBuf::from(value));
        }
        index += 1;
    }

    let cwd = match requested {
        Some(path) if path.is_absolute() => path,
        Some(path) => shell_cwd.join(path),
        None => shell_cwd.to_path_buf(),
    };
    if !cwd.is_dir() {
        return Err(format!(
            "Grok working directory {} does not exist",
            cwd.display()
        ));
    }
    Ok(fs::canonicalize(&cwd).unwrap_or(cwd))
}

/// Resolves an idle Grok agent: drains the next queued turn, or enters/stays paused.
/// Returns whether a turn was drained. Status/paused are written by
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

/// Parses a transcript line for a Grok agent.
///
/// Grok Build uses Claude-compatible rollout transcripts (the path it reports in
/// the SessionStart hook under `transcript_path`). We therefore support the native
/// Claude-style JSONL format first (same as the Claude adapter). We also support
/// the synthetic "response_item" format (used by Codex/OpenCode and the qmux
/// opencode plugin) as a fallback for the qmux-managed transcript path or future
/// Grok plugin writers.
///
/// ```json
/// // Native (Claude/Grok rollout)
/// {"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}
///
/// // Synthetic (response_item)
/// {"type":"response_item","payload":{"type":"message","role":"user","content":[...]},"session_id":"..."}
/// ```
fn parse_transcript_line(agent_id: &str, source_index: usize, line: &str) -> Option<Turn> {
    // Parse once, then try each shape against the same value — the tail calls this per
    // line, so re-parsing per attempt doubled the JSON work on hot streaming output.
    let value = serde_json::from_str::<Value>(line).ok()?;

    // Current Grok chat_history.jsonl format. Assistant records may carry both
    // visible text and multiple tool calls; tool results are separate records.
    if value.get("content").is_some()
        || value.get("tool_calls").is_some()
        || value.get("tool_call_id").is_some()
        || matches!(
            value.get("type").and_then(Value::as_str),
            Some("reasoning" | "backend_tool_call")
        )
    {
        return parse_grok_chat_history_value(agent_id, source_index, &value);
    }

    // Primary: native Claude-compatible rollout format. This is what Grok reports
    // via its SessionStart hook for real sessions (under ~/.grok/sessions/...).
    if let Some(turn) = super::parse_claude_native_transcript_value(agent_id, source_index, &value)
    {
        return Some(turn);
    }

    // Fallback: synthetic response_item format (for the .qmux/grok fallback path
    // or if a future plugin emits it).
    if value.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    let payload = value.get("payload")?;
    let item_type = payload.get("type").and_then(Value::as_str)?;
    let session_id = super::string_field(&value, "session_id")
        .or_else(|| super::string_field(&value, "sessionId"));

    let (role, blocks) = match item_type {
        "message" => {
            let role = payload.get("role").and_then(Value::as_str)?;
            if role == "developer" || role == "system" {
                return None;
            }
            let blocks = parse_grok_synthetic_message_blocks(payload.get("content"))?;
            (role.to_string(), blocks)
        }
        "tool_use" | "function_call" | "custom_tool_call" => {
            let name = super::string_field(payload, "name").unwrap_or_else(|| "tool".to_string());
            (
                "assistant".to_string(),
                vec![TurnBlock::ToolUse {
                    id: super::string_field(payload, "id")
                        .or_else(|| super::string_field(payload, "call_id"))
                        .or_else(|| super::string_field(payload, "callId")),
                    name,
                    input: payload.get("input").cloned().unwrap_or(Value::Null),
                }],
            )
        }
        "tool_result" | "function_call_output" | "custom_tool_call_output" => (
            "assistant".to_string(),
            vec![TurnBlock::ToolResult {
                tool_use_id: super::string_field(payload, "tool_use_id")
                    .or_else(|| super::string_field(payload, "toolUseId"))
                    .or_else(|| super::string_field(payload, "call_id"))
                    .or_else(|| super::string_field(payload, "callId")),
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
        native_id: super::string_field(payload, "id"),
        parent_native_id: None,
        native_message_id: super::string_field(payload, "id"),
    })
}

fn parse_grok_chat_history_value(
    agent_id: &str,
    source_index: usize,
    value: &Value,
) -> Option<Turn> {
    let record_type = value.get("type").and_then(Value::as_str)?;
    let (role, blocks, native_message_id) = match record_type {
        "user" => {
            // Grok persists injected project/skill reminders as synthetic user
            // records. They are private harness context rather than conversation.
            if value.get("synthetic_reason").is_some() {
                return None;
            }
            (
                "user".to_string(),
                parse_grok_synthetic_message_blocks(value.get("content"))?,
                None,
            )
        }
        "assistant" => {
            let mut blocks =
                parse_grok_synthetic_message_blocks(value.get("content")).unwrap_or_default();
            if let Some(tool_calls) = value.get("tool_calls").and_then(Value::as_array) {
                for tool_call in tool_calls {
                    let arguments = match tool_call.get("arguments") {
                        Some(Value::String(raw)) => {
                            serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.clone()))
                        }
                        Some(arguments) => arguments.clone(),
                        None => Value::Null,
                    };
                    blocks.push(TurnBlock::ToolUse {
                        id: super::string_field(tool_call, "id"),
                        name: super::string_field(tool_call, "name")
                            .unwrap_or_else(|| "tool".to_string()),
                        input: arguments,
                    });
                }
            }
            ("assistant".to_string(), blocks, None)
        }
        "tool_result" => {
            let tool_use_id = super::string_field(value, "tool_call_id");
            (
                "assistant".to_string(),
                vec![TurnBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: value.get("content").cloned().unwrap_or(Value::Null),
                    is_error: value
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                }],
                tool_use_id,
            )
        }
        _ => return None,
    };

    if blocks.is_empty()
        || blocks
            .iter()
            .all(|block| matches!(block, TurnBlock::Text { text } if text.trim().is_empty()))
    {
        return None;
    }

    Some(Turn {
        id: format!("{agent_id}-{source_index}"),
        agent_id: agent_id.to_string(),
        session_id: None,
        role,
        blocks,
        source_index,
        timestamp: super::native_timestamp_ms(value),
        status: None,
        status_reason: None,
        native_id: native_message_id.clone(),
        parent_native_id: None,
        native_message_id,
    })
}

fn parse_transcript_lifecycle_event(line: &str) -> Option<TranscriptLifecycleEvent> {
    // Parse once and check both shapes against the same value (see parse_transcript_line).
    let value = serde_json::from_str::<Value>(line).ok()?;

    // Support Claude-style interruption markers (native rollouts).
    if let Some(ev) = super::parse_claude_native_lifecycle_value(&value) {
        return Some(ev);
    }

    // Support synthetic event_msg for the qmux fallback path.
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    let payload = value.get("payload")?;
    (payload.get("type").and_then(Value::as_str) == Some("turn_aborted"))
        .then_some(TranscriptLifecycleEvent::Interrupted)
}

/// Parses blocks from a synthetic response_item payload (mirrors Codex/OpenCode shape).
fn parse_grok_synthetic_message_blocks(content: Option<&Value>) -> Option<Vec<TurnBlock>> {
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
    use std::path::PathBuf;

    fn test_config() -> QmuxConfig {
        QmuxConfig {
            workspace_root: PathBuf::from("/tmp/qmux-grok-tests"),
            socket_path: PathBuf::from("/tmp/qmux-grok-tests.sock"),
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
            adapter: "grok".to_string(),
            worktree_dir: "/tmp/qmux-grok-tests".to_string(),
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
        let outcome = GrokAdapter::new(state.config())
            .ingest_notification(state, notification)
            .unwrap();
        match outcome {
            AdapterNotificationOutcome::Event(event) => event,
        }
    }

    #[test]
    fn launch_options_reject_unknown_fields() {
        let err = GrokLaunchOptions::from_value(json!({ "bogus": true })).unwrap_err();
        assert!(err.contains("invalid Grok adapter options"));
    }

    #[test]
    fn build_args_adds_cwd_model_and_positional_prompt() {
        let args = build_grok_args(Path::new("/tmp/qmux"), Some("grok-code"), "fix the bug");

        // The prompt is a trailing positional, after --cwd and --model, delimited
        // with `--` so leading-dash text can't be parsed as a flag.
        assert_eq!(
            args,
            vec![
                "--cwd",
                "/tmp/qmux",
                "--model",
                "grok-code",
                "--",
                "fix the bug"
            ]
        );
    }

    #[test]
    fn build_args_omit_empty_prompt_and_model() {
        let args = build_grok_args(Path::new("/tmp/qmux"), None, "  ");

        assert_eq!(args, vec!["--cwd", "/tmp/qmux"]);
    }

    #[test]
    fn resume_args_include_session_id_when_present() {
        let (args, resumed) =
            build_grok_resume_args(Path::new("/tmp/qmux"), Some("grok-code"), Some(" sess-1 "));

        assert!(resumed);
        assert_eq!(
            args,
            vec![
                "--cwd",
                "/tmp/qmux",
                "--model",
                "grok-code",
                "--resume",
                "sess-1"
            ]
        );
    }

    #[test]
    fn resume_args_fall_back_to_fresh_launch_without_session_id() {
        let (args, resumed) = build_grok_resume_args(Path::new("/tmp/qmux"), None, Some("   "));

        assert!(!resumed);
        assert_eq!(args, vec!["--cwd", "/tmp/qmux"]);
    }

    #[test]
    fn fork_args_resume_into_new_session_and_append_prompt() {
        let args = build_grok_fork_args(
            Path::new("/tmp/qmux"),
            Some("grok-build"),
            "source-session",
            Some(" continue here "),
        );

        assert_eq!(
            args,
            vec![
                "--cwd",
                "/tmp/qmux",
                "--model",
                "grok-build",
                "--resume",
                "source-session",
                "--fork-session",
                "--",
                "continue here",
            ]
        );
    }

    #[test]
    fn shell_resume_command_resumes_the_session() {
        let command = GrokAdapter::new(&test_config())
            .shell_resume_command("sess-1")
            .expect("grok supports shell resume");
        assert_eq!(command, "grok --resume 'sess-1'");
    }

    #[test]
    fn shell_args_supply_cwd_only_when_absent() {
        // No cwd in the user's args: qMux adds the pane cwd.
        let args = build_grok_args_from_shell(
            Path::new("/tmp/qmux"),
            &["--model".to_string(), "grok-code".to_string()],
        );
        assert_eq!(args, vec!["--cwd", "/tmp/qmux", "--model", "grok-code"]);

        // User already passed a cwd: forward verbatim, no duplicate.
        let args = build_grok_args_from_shell(
            Path::new("/tmp/qmux"),
            &["--cwd".to_string(), "/elsewhere".to_string()],
        );
        assert_eq!(args, vec!["--cwd", "/elsewhere"]);
    }

    #[test]
    fn shell_cwd_override_drives_agent_workspace_identity() {
        let args = |values: &[&str]| values.iter().map(ToString::to_string).collect::<Vec<_>>();
        let root = std::env::temp_dir().join(format!("qmux-grok-shell-cwd-{}", std::process::id()));
        let shell = root.join("shell");
        let project = root.join("project");
        fs::create_dir_all(&shell).unwrap();
        fs::create_dir_all(&project).unwrap();

        assert_eq!(
            grok_effective_cwd(&shell, &args(&["--cwd", "../project"])).unwrap(),
            fs::canonicalize(&project).unwrap()
        );
        assert_eq!(
            grok_effective_cwd(&shell, &args(&[&format!("--cwd={}", project.display())])).unwrap(),
            fs::canonicalize(&project).unwrap()
        );
        assert!(grok_effective_cwd(&shell, &args(&["--cwd"])).is_err());
        assert!(grok_effective_cwd(&shell, &args(&["--cwd", "missing"])).is_err());

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn args_contain_prompt_detects_prompt_and_positional() {
        // Interactive, no prompt: bare flags or resume/model only.
        assert!(!args_contain_prompt(&[]));
        assert!(!args_contain_prompt(&[
            "--model".to_string(),
            "grok-code".to_string()
        ]));
        assert!(!args_contain_prompt(&[
            "--cwd".to_string(),
            "/tmp".to_string()
        ]));
        // `--resume <id>` consumes the id as a value, not a positional prompt.
        assert!(!args_contain_prompt(&[
            "--resume".to_string(),
            "sess-1".to_string()
        ]));
        assert!(!args_contain_prompt(&[
            "--rules".to_string(),
            "Use the repository instructions".to_string(),
            "--agent".to_string(),
            "reviewer".to_string(),
            "--json-schema".to_string(),
            "{\"type\":\"object\"}".to_string(),
        ]));
        assert!(!args_contain_prompt(&["models".to_string()]));
        assert!(!args_contain_prompt(&[
            "--resume".to_string(),
            "--model".to_string(),
            "grok-build".to_string(),
        ]));

        // Headless prompt flags.
        assert!(args_contain_prompt(&[
            "--prompt".to_string(),
            "fix the bug".to_string()
        ]));
        assert!(args_contain_prompt(&[
            "-p".to_string(),
            "fix the bug".to_string()
        ]));
        assert!(args_contain_prompt(&["--prompt=fix the bug".to_string()]));
        assert!(args_contain_prompt(&[
            "--prompt-file".to_string(),
            "prompt.md".to_string()
        ]));
        // A trailing positional is the interactive initial prompt.
        assert!(args_contain_prompt(&["fix the bug".to_string()]));
        assert!(args_contain_prompt(&[
            "--model".to_string(),
            "grok-code".to_string(),
            "fix the bug".to_string()
        ]));
    }

    #[test]
    fn composer_policy_queues_running_panes() {
        let policy = GrokAdapter {
            binary: "grok".to_string(),
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
            attach_grok_agent_pane(&state, "agent-1", "pane-1".to_string(), false).unwrap();

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
            attach_grok_agent_pane(&state, "agent-1", "pane-1".to_string(), true).unwrap();

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
                json!({ "session_id": "grok-session-1" }),
            ),
        );

        assert_eq!(event.event_type, "agent.session_start");
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(agent.session_id.as_deref(), Some("grok-session-1"));
        // SessionStart does not promote to Running (matches Claude/Codex/OpenCode).
        assert!(matches!(agent.status, AgentStatus::Starting));
        assert!(
            agent
                .transcript_path
                .as_deref()
                .unwrap()
                .ends_with("/.qmux/grok/agent-1.jsonl")
        );
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
    fn permission_denied_keeps_active_agent_running() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::AwaitingPermission;
        state.insert_agent(agent).unwrap();

        let event = ingest(
            &state,
            hook_for_agent("PermissionDenied", "agent-1", json!({})),
        );

        assert_eq!(event.event_type, "agent.permission_denied");
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(matches!(agent.status, AgentStatus::Running));
    }

    #[test]
    fn stop_failure_settles_agent_without_queued_turns() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Running;
        state.insert_agent(agent).unwrap();

        let event = ingest(&state, hook_for_agent("StopFailure", "agent-1", json!({})));

        assert_eq!(event.event_type, "agent.done");
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(matches!(agent.status, AgentStatus::Done));
    }

    #[test]
    fn compaction_and_passive_hooks_preserve_parent_activity() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::AwaitingInput;
        state.insert_agent(agent).unwrap();

        let event = ingest(&state, hook_for_agent("PreCompact", "agent-1", json!({})));
        assert_eq!(event.event_type, "agent.compacting");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        ));

        for (hook_event, expected_event) in [
            ("PostCompact", "agent.compacted"),
            ("Notification", "agent.notification"),
        ] {
            let event = ingest(&state, hook_for_agent(hook_event, "agent-1", json!({})));
            assert_eq!(event.event_type, expected_event);
            assert!(matches!(
                state.agent("agent-1").unwrap().unwrap().status,
                AgentStatus::Running
            ));
        }

        let event = ingest(
            &state,
            hook_for_agent(
                "SubagentStart",
                "agent-1",
                json!({ "subagentId": "child-1" }),
            ),
        );
        assert_eq!(event.event_type, "agent.subagent_started");
        assert!(state.agent_has_active_subagents("agent-1").unwrap());

        let event = ingest(&state, hook_for_agent("Stop", "agent-1", json!({})));
        assert_eq!(event.event_type, "agent.running");

        let event = ingest(
            &state,
            hook_for_agent(
                "SubagentStop",
                "agent-1",
                json!({ "subagentId": "child-1" }),
            ),
        );
        assert_eq!(event.event_type, "agent.subagent_stopped");
        assert!(!state.agent_has_active_subagents("agent-1").unwrap());
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        ));

        let event = ingest(&state, hook_for_agent("Stop", "agent-1", json!({})));
        assert_eq!(event.event_type, "agent.done");

        let event = ingest(&state, hook_for_agent("SessionEnd", "agent-1", json!({})));
        assert_eq!(event.event_type, "agent.session_end");
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
    fn parse_transcript_line_ignores_developer_messages() {
        let line = r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"text","text":"system"}]},"session_id":"sess-1"}"#;

        assert!(parse_transcript_line("agent-1", 0, line).is_none());
    }

    #[test]
    fn parse_grok_turn_aborted_lifecycle_event() {
        let abort_line = json!({
            "type": "event_msg",
            "payload": { "type": "turn_aborted", "reason": "interrupt" },
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
        let path = GrokAdapter::transcript_path_for(&state, "agent-42");

        assert!(path.ends_with(".qmux/grok/agent-42.jsonl"));
        assert!(path.starts_with("/tmp/qmux-grok-tests"));
    }

    #[test]
    fn native_session_transcript_path_encodes_cwd_and_confines_session_id() {
        let home =
            std::env::temp_dir().join(format!("qmux-grok-session-path-{}", std::process::id()));
        fs::create_dir_all(home.join("sessions")).unwrap();

        let path = grok_session_transcript_path(
            &home,
            "/Users/example/My Project",
            "019f4b23-836e-7260-909a-b975d090c6f8",
        )
        .unwrap();
        assert_eq!(
            path,
            home.join("sessions")
                .join("%2FUsers%2Fexample%2FMy%20Project")
                .join("019f4b23-836e-7260-909a-b975d090c6f8")
                .join("chat_history.jsonl")
        );
        assert!(grok_session_transcript_path(&home, "/tmp", "../other").is_none());

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn native_session_transcript_path_resolves_verified_hashed_cwd_group() {
        let home = std::env::temp_dir().join(format!(
            "qmux-grok-hashed-session-path-{}",
            std::process::id()
        ));
        let group = home.join("sessions").join("long-path-deadbeef");
        let session_id = "019f4b23-836e-7260-909a-b975d090c6f8";
        fs::create_dir_all(group.join(session_id)).unwrap();
        fs::write(group.join(".cwd"), "/very/long/project\n").unwrap();

        let path = grok_session_transcript_path(&home, "/very/long/project", session_id).unwrap();
        assert_eq!(path, group.join(session_id).join("chat_history.jsonl"));
        assert!(grok_session_transcript_path(&home, "/another/project", session_id).is_some());
        assert_ne!(
            grok_session_transcript_path(&home, "/another/project", session_id).unwrap(),
            path
        );

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn native_session_cwd_is_confined_to_the_agent_workspace() {
        let root =
            std::env::temp_dir().join(format!("qmux-grok-session-cwd-{}", std::process::id()));
        let expected = root.join("project");
        let other = root.join("other");
        fs::create_dir_all(&expected).unwrap();
        fs::create_dir_all(&other).unwrap();

        assert!(grok_session_cwd_acceptable(
            &expected.display().to_string(),
            &expected.join(".").display().to_string()
        ));
        assert!(!grok_session_cwd_acceptable(
            &expected.display().to_string(),
            &other.display().to_string()
        ));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn parses_native_grok_chat_history_messages_and_tools() {
        let user = json!({
            "type": "user",
            "content": [{ "type": "text", "text": "inspect the repo" }]
        })
        .to_string();
        let assistant = json!({
            "type": "assistant",
            "content": "I’ll inspect it.",
            "tool_calls": [{
                "id": "call-1",
                "name": "grep",
                "arguments": "{\"pattern\":\"TODO\"}"
            }]
        })
        .to_string();
        let result = json!({
            "type": "tool_result",
            "tool_call_id": "call-1",
            "content": "no matches"
        })
        .to_string();

        let user_turn = parse_transcript_line("agent-1", 10, &user).unwrap();
        assert_eq!(user_turn.role, "user");
        assert!(matches!(
            &user_turn.blocks[0],
            TurnBlock::Text { text } if text == "inspect the repo"
        ));

        let assistant_turn = parse_transcript_line("agent-1", 11, &assistant).unwrap();
        assert_eq!(assistant_turn.blocks.len(), 2);
        assert!(matches!(
            &assistant_turn.blocks[1],
            TurnBlock::ToolUse { id, name, input }
                if id.as_deref() == Some("call-1")
                    && name == "grep"
                    && input["pattern"] == "TODO"
        ));

        let result_turn = parse_transcript_line("agent-1", 12, &result).unwrap();
        assert!(matches!(
            &result_turn.blocks[0],
            TurnBlock::ToolResult { tool_use_id, content, .. }
                if tool_use_id.as_deref() == Some("call-1") && content == "no matches"
        ));
    }

    #[test]
    fn skips_native_grok_private_context_records() {
        let synthetic = json!({
            "type": "user",
            "content": [{ "type": "text", "text": "private harness context" }],
            "synthetic_reason": "system_reminder"
        })
        .to_string();
        let reasoning = json!({ "type": "reasoning", "summary": [] }).to_string();

        assert!(parse_transcript_line("agent-1", 0, &synthetic).is_none());
        assert!(parse_transcript_line("agent-1", 1, &reasoning).is_none());
    }

    #[test]
    fn session_start_prefers_hook_provided_transcript_path() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Starting;
        state.insert_agent(agent).unwrap();

        let event = ingest(
            &state,
            hook_for_agent(
                "SessionStart",
                "agent-1",
                json!({
                    "session_id": "grok-session-1",
                    "transcript_path": "/home/user/.grok/sessions/grok-session-1/rollout.jsonl"
                }),
            ),
        );

        assert_eq!(event.event_type, "agent.session_start");
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        // The hook-provided path wins over the qMux-managed fallback.
        assert_eq!(
            agent.transcript_path.as_deref(),
            Some("/home/user/.grok/sessions/grok-session-1/rollout.jsonl")
        );
    }

    #[test]
    fn session_start_without_transcript_path_keeps_recorded_path() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Idle;
        agent.session_id = Some("grok-session-1".to_string());
        agent.transcript_path =
            Some("/home/user/.grok/sessions/grok-session-1/rollout.jsonl".to_string());
        state.insert_agent(agent).unwrap();

        // A late/duplicate SessionStart (e.g. after a resume) that omits the field
        // must not rebind the tail to the qMux fallback path.
        let event = ingest(&state, hook_for_agent("SessionStart", "agent-1", json!({})));

        assert_eq!(event.event_type, "agent.session_start");
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(
            agent.transcript_path.as_deref(),
            Some("/home/user/.grok/sessions/grok-session-1/rollout.jsonl")
        );
        // Nor blank a recorded session id.
        assert_eq!(agent.session_id.as_deref(), Some("grok-session-1"));
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
        let agent = state.agent("agent-1").unwrap().unwrap();
        assert_eq!(agent.session_id, None);
        assert_eq!(agent.transcript_path, None);

        ingest(
            &state,
            hook_for_agent(
                "SessionStart",
                "agent-1",
                json!({
                    "sessionId": "source-session",
                    "transcript_path": "/home/user/.grok/sessions/source-session/rollout.jsonl"
                }),
            ),
        );
        let agent = state.agent("agent-1").unwrap().unwrap();
        assert_eq!(agent.session_id, None);
        assert_eq!(agent.transcript_path, None);

        ingest(
            &state,
            hook_for_agent(
                "SessionStart",
                "agent-1",
                json!({ "sessionId": "forked-session" }),
            ),
        );
        assert_eq!(
            state
                .agent("agent-1")
                .unwrap()
                .unwrap()
                .session_id
                .as_deref(),
            Some("forked-session")
        );
    }

    #[test]
    fn forked_agent_adopts_child_identity_from_first_turn_hook() {
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
        ingest(
            &state,
            hook_for_agent(
                "UserPromptSubmit",
                "agent-1",
                json!({
                    "sessionId": "forked-session",
                    "cwd": "/tmp/qmux-grok-tests",
                    "prompt": "continue"
                }),
            ),
        );

        let agent = state.agent("agent-1").unwrap().unwrap();
        assert_eq!(agent.session_id.as_deref(), Some("forked-session"));
        assert!(
            agent
                .transcript_path
                .as_deref()
                .is_some_and(|path| path.ends_with("/forked-session/chat_history.jsonl"))
        );

        // Adoption is one-shot: later inconsistent metadata cannot move the binding.
        ingest(
            &state,
            hook_for_agent(
                "PostToolUse",
                "agent-1",
                json!({
                    "sessionId": "other-session",
                    "cwd": "/tmp/qmux-grok-tests"
                }),
            ),
        );
        assert_eq!(
            state
                .agent("agent-1")
                .unwrap()
                .unwrap()
                .session_id
                .as_deref(),
            Some("forked-session")
        );
    }

    #[test]
    fn resume_session_id_parses_resume_forms() {
        let args = |values: &[&str]| values.iter().map(ToString::to_string).collect::<Vec<_>>();

        assert_eq!(
            grok_resume_session_id(&args(&["--resume", "sess-1"])),
            Some("sess-1")
        );
        assert_eq!(
            grok_resume_session_id(&args(&["-r", "sess-1"])),
            Some("sess-1")
        );
        assert_eq!(
            grok_resume_session_id(&args(&["--resume=sess-1"])),
            Some("sess-1")
        );
        assert_eq!(
            grok_resume_session_id(&args(&["--model", "grok-code", "--resume", "sess-1"])),
            Some("sess-1")
        );

        // Not a resume of a specific session.
        assert_eq!(grok_resume_session_id(&args(&[])), None);
        assert_eq!(grok_resume_session_id(&args(&["fix the bug"])), None);
        assert_eq!(grok_resume_session_id(&args(&["--resume"])), None);
        assert_eq!(grok_resume_session_id(&args(&["--resume="])), None);
        assert_eq!(
            grok_resume_session_id(&args(&["--resume", "--model"])),
            None
        );
        assert_eq!(
            grok_resume_session_id(&args(&[
                "--rules",
                "--resume",
                "--",
                "--resume",
                "prompt-session"
            ])),
            None
        );
        assert_eq!(
            grok_resume_session_id(&args(&["--", "--resume", "prompt-session"])),
            None
        );
        assert_eq!(
            grok_resume_session_id(&args(&["--resume", "source-session", "--fork-session"])),
            None
        );
        assert_eq!(
            grok_resume_session_id(&args(&["--fork-session", "--resume=source-session"])),
            None
        );
        assert_eq!(
            grok_fork_source_session_id(&args(&["--resume", "source-session", "--fork-session"])),
            Some("source-session")
        );
    }

    #[test]
    fn grok_hook_shim_is_env_gated_and_forwards_notify() {
        let shim = grok_hook_shim();
        // No-ops outside qMux: every required env var is checked.
        assert!(shim.contains("QMUX_SOCK"));
        assert!(shim.contains("QMUX_TOKEN"));
        assert!(shim.contains("QMUX_PANE_ID"));
        assert!(shim.contains("QMUX_AGENT_ID"));
        assert!(shim.contains("QMUX_CLI"));
        // Inside qMux it forwards the event to `qmux notify`.
        assert!(shim.contains(r#"exec "$QMUX_CLI" notify "$event""#));
    }

    #[test]
    fn grok_hooks_document_installs_events_without_matchers() {
        let shim = Path::new("/home/user/.grok/qmux/qmux-grok-hook");
        let doc = grok_hooks_document(shim);

        // Every qMux event gets exactly one entry whose command runs the shim,
        // and lifecycle events must not carry a matcher (Grok rejects them).
        for event in GROK_HOOK_EVENTS {
            let entries = doc["hooks"][event].as_array().expect("event array");
            assert_eq!(entries.len(), 1, "expected one entry for {event}");
            assert!(
                entries[0].get("matcher").is_none(),
                "{event} must omit matcher"
            );
            assert!(
                is_qmux_grok_hook_entry(&entries[0], shim),
                "missing qMux hook entry for {event}"
            );
            let command = entries[0]["hooks"][0]["command"].as_str().unwrap();
            assert_eq!(command, format!("'{}' {event}", shim.display()));
        }
        // PermissionRequest is Claude-only and must not be installed for Grok.
        assert!(doc["hooks"].get("PermissionRequest").is_none());
    }

    #[test]
    fn write_grok_integration_files_creates_shim_and_hooks_file() {
        let home =
            std::env::temp_dir().join(format!("qmux-grok-home-create-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);

        write_grok_integration_files(&home).unwrap();

        let shim_path = home.join("qmux").join("qmux-grok-hook");
        let shim_meta = fs::metadata(&shim_path).expect("shim written");
        // Executable bit set so Grok can run it.
        assert_eq!(shim_meta.permissions().mode() & 0o111, 0o111);

        // Grok discovers global hooks from hooks/*.json — not user-settings.json.
        let hooks_path = home.join("hooks").join("qmux.json");
        let hooks_raw = fs::read_to_string(&hooks_path).unwrap();
        assert_eq!(hooks_raw, grok_hooks_file_contents(&shim_path));
        let hooks: Value = serde_json::from_str(&hooks_raw).unwrap();
        for event in GROK_HOOK_EVENTS {
            assert!(
                hooks["hooks"][event].is_array(),
                "missing hooks for {event}"
            );
            assert!(
                hooks["hooks"][event][0].get("matcher").is_none(),
                "{event} must omit matcher"
            );
        }
        assert!(!home.join("user-settings.json").exists());

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn write_grok_integration_files_is_noop_when_already_correct() {
        let home = std::env::temp_dir().join(format!("qmux-grok-home-noop-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);

        write_grok_integration_files(&home).unwrap();

        let shim_path = home.join("qmux").join("qmux-grok-hook");
        let hooks_path = home.join("hooks").join("qmux.json");

        // Make both files unwritable (shim keeps its exec bits) so any rewrite
        // attempt fails rather than silently producing identical bytes.
        fs::set_permissions(&shim_path, fs::Permissions::from_mode(0o555)).unwrap();
        fs::set_permissions(&hooks_path, fs::Permissions::from_mode(0o444)).unwrap();

        let result = write_grok_integration_files(&home);

        fs::set_permissions(&shim_path, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&hooks_path, fs::Permissions::from_mode(0o644)).unwrap();

        result.expect("second call should not write anything");

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn write_grok_integration_files_repairs_missing_or_stale_hooks() {
        let home =
            std::env::temp_dir().join(format!("qmux-grok-home-repair-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);

        write_grok_integration_files(&home).unwrap();

        let shim_path = home.join("qmux").join("qmux-grok-hook");
        let hooks_path = home.join("hooks").join("qmux.json");

        // Corrupt the hooks file: drop Stop and leave a stale SessionStart path.
        let corrupted = json!({
            "hooks": {
                "SessionStart": [{
                    "matcher": "",
                    "hooks": [{
                        "type": "command",
                        "command": "'/old/home/.grok/qmux/qmux-grok-hook' SessionStart"
                    }]
                }],
                "UserPromptSubmit": [{
                    "hooks": [{
                        "type": "command",
                        "command": format!("'{}' UserPromptSubmit", shim_path.display())
                    }]
                }]
            }
        });
        fs::write(
            &hooks_path,
            serde_json::to_string_pretty(&corrupted).unwrap(),
        )
        .unwrap();

        // Re-run should fully rewrite to the expected document.
        write_grok_integration_files(&home).unwrap();

        let repaired = fs::read_to_string(&hooks_path).unwrap();
        assert_eq!(repaired, grok_hooks_file_contents(&shim_path));

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn write_grok_integration_files_strips_legacy_user_settings_hooks() {
        let home =
            std::env::temp_dir().join(format!("qmux-grok-home-legacy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();

        let shim = home.join("qmux").join("qmux-grok-hook");
        // Simulate the pre-fix install: hooks only lived in user-settings.json.
        // Keep a user-owned SessionStart entry that must survive cleanup.
        let legacy = json!({
            "model": "grok-code-fast",
            "hooks": {
                "SessionStart": [
                    {
                        "matcher": "",
                        "hooks": [{
                            "type": "command",
                            "command": format!("'{}' SessionStart", shim.display())
                        }]
                    },
                    {
                        "hooks": [{ "type": "command", "command": "echo user-hook" }]
                    }
                ],
                "Stop": [{
                    "matcher": "",
                    "hooks": [{
                        "type": "command",
                        "command": format!("'{}' Stop", shim.display())
                    }]
                }],
                "PermissionRequest": [{
                    "matcher": "",
                    "hooks": [{
                        "type": "command",
                        "command": format!("'{}' PermissionRequest", shim.display())
                    }]
                }]
            }
        });
        fs::write(
            home.join("user-settings.json"),
            serde_json::to_string_pretty(&legacy).unwrap(),
        )
        .unwrap();

        write_grok_integration_files(&home).unwrap();

        // New install path exists.
        assert!(home.join("hooks").join("qmux.json").exists());

        let cleaned: Value =
            serde_json::from_str(&fs::read_to_string(home.join("user-settings.json")).unwrap())
                .unwrap();
        // Unrelated settings preserved; user SessionStart hook kept; qMux entries gone.
        assert_eq!(cleaned["model"], "grok-code-fast");
        let session_start = cleaned["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(session_start.len(), 1);
        assert_eq!(session_start[0]["hooks"][0]["command"], "echo user-hook");
        assert!(cleaned["hooks"].get("Stop").is_none());
        assert!(cleaned["hooks"].get("PermissionRequest").is_none());

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn parse_transcript_line_supports_claude_native_format_for_grok() {
        // Typical lines from a Grok/Claude-style rollout.jsonl (what SessionStart binds).
        let user_line = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{ "type": "text", "text": "hello from grok" }]
            },
            "session_id": "grok-sess-1"
        })
        .to_string();

        let turn = parse_transcript_line("agent-g1", 0, &user_line).expect("native user line");
        assert_eq!(turn.role, "user");
        assert_eq!(turn.session_id.as_deref(), Some("grok-sess-1"));
        match &turn.blocks[0] {
            TurnBlock::Text { text } => assert_eq!(text, "hello from grok"),
            other => panic!("expected text, got {other:?}"),
        }

        let tool_use_line = json!({
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "tool-99",
                    "name": "read_file",
                    "input": { "path": "foo.txt" }
                }]
            }
        })
        .to_string();

        let turn = parse_transcript_line("agent-g1", 1, &tool_use_line).unwrap();
        assert_eq!(turn.role, "assistant");
        match &turn.blocks[0] {
            TurnBlock::ToolUse { id, name, input } => {
                assert_eq!(id.as_deref(), Some("tool-99"));
                assert_eq!(name, "read_file");
                assert_eq!(input["path"], "foo.txt");
            }
            other => panic!("expected tool use {other:?}"),
        }
    }

    #[test]
    fn parse_transcript_lifecycle_supports_native_and_synthetic_for_grok() {
        let native_interrupt = json!({
            "type": "user",
            "message": {
                "content": [{ "type": "text", "text": "[Request interrupted by user]" }]
            }
        })
        .to_string();

        assert_eq!(
            parse_transcript_lifecycle_event(&native_interrupt),
            Some(TranscriptLifecycleEvent::Interrupted)
        );

        let synthetic = json!({
            "type": "event_msg",
            "payload": { "type": "turn_aborted" }
        })
        .to_string();

        assert_eq!(
            parse_transcript_lifecycle_event(&synthetic),
            Some(TranscriptLifecycleEvent::Interrupted)
        );
    }
}
