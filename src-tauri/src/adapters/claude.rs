use super::{
    AdapterNotification, AdapterNotificationOutcome, AgentAdapter, ComposerPolicy,
    FORK_AT_MESSAGE_EMPTY_ERROR, LaunchEnv, MessageAnchor, PermissionAction,
    PrepareShellAgentLaunchRequest, PreparedShellAgentLaunch, ShellCommandIntegration,
    SpawnAgentRequest, TranscriptLifecycleEvent, ensure_on_path, new_uuid_v4,
    parse_transcript_records, prepared_shell_agent, record_shell_session_lineage,
    reusable_session_agent, shell_quote_arg, shell_quote_path,
};
use crate::config::QmuxConfig;
use crate::events::QmuxEvent;
use crate::pty::{InitialPaneSize, PtySpawnSpec, qmux_pane_envs, recoverable_dir, spawn_pty};
use crate::state::{AppState, PaneInfo, PaneKind};
use crate::transcript::{
    Turn, TurnStatus, TurnStatusReason, session_id_from_transcript_path, start_transcript_tail,
};

#[cfg(test)]
use crate::transcript::TurnBlock;
use crate::turn_queue::{IdleResolution, advance_after_idle, is_shell_escape_turn};
use crate::workspace::{
    AgentInfo, AgentStatus, PrepareAgentWorkspaceRequest, attach_agent_pane, mark_agent_failed,
    mark_agent_spawn_failed, prepare_agent_workspace,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const CLAUDE_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "PermissionDenied",
    "Stop",
    "StopFailure",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "PostCompact",
    "Elicitation",
    "ElicitationResult",
    "SessionEnd",
];

const CLAUDE_NOTIFICATION_MATCHERS: &[(&str, &str)] = &[
    ("permission_prompt", "Notification.permission_prompt"),
    ("idle_prompt", "Notification.idle_prompt"),
    ("elicitation_dialog", "Notification.elicitation_dialog"),
];

const CLAUDE_PERMISSION_MODES: &[&str] = &[
    "acceptEdits",
    "auto",
    "bypassPermissions",
    "dontAsk",
    "manual",
    "plan",
];

/// Effort levels accepted by the Claude CLI's `--effort` flag. Every current
/// Claude model (Opus, Fable, Sonnet) supports the full range.
const CLAUDE_EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultracode"];

#[derive(Clone, Debug)]
pub struct ClaudeAdapter {
    binary: String,
    plugin_dir: PathBuf,
}

impl ClaudeAdapter {
    pub fn new(config: &QmuxConfig) -> Self {
        Self {
            binary: config.claude_binary(),
            plugin_dir: config.claude_plugin_dir.clone(),
        }
    }

    /// `--plugin-dir` args that inject the qmux-managed plugin (and its skills)
    /// into a launched Claude instance. Emitted only when the plugin directory
    /// actually exists, so a checkout without one launches cleanly. This is the
    /// sole skill-injection vector: it points at a qmux-owned directory and never
    /// touches the user's `~/.claude` or the project's `.claude`.
    fn plugin_dir_args(&self) -> Vec<String> {
        if self.plugin_dir.is_dir() {
            vec![
                "--plugin-dir".to_string(),
                self.plugin_dir.display().to_string(),
            ]
        } else {
            Vec::new()
        }
    }

    fn ensure_binary(&self) -> Result<String, String> {
        let binary = ensure_on_path(&self.binary).ok_or_else(|| {
            format!(
                "Claude adapter binary '{}' was not found on PATH or standard macOS tool paths. Install Claude Code or update adapters.claude.binary in qmux.config.json.",
                self.binary
            )
        })?;
        Ok(binary.display().to_string())
    }
}

impl AgentAdapter for ClaudeAdapter {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn display_name(&self) -> &'static str {
        "Claude"
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
            command_name: "claude",
            adapter_id: self.id(),
        }]
    }

    fn shell_resume_command(&self, session_id: &str) -> Option<String> {
        Some(format!("claude --resume {}", shell_quote_arg(session_id)))
    }

    fn ingest_notification(
        &self,
        state: &AppState,
        notification: AdapterNotification,
    ) -> Result<AdapterNotificationOutcome, String> {
        self.ingest_hook_notification(state, notification)
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

    fn resolve_transcript_turns(
        &self,
        agent_id: &str,
        source_index_offset: usize,
        lines: &[String],
    ) -> Vec<Turn> {
        resolve_transcript_turns_from(agent_id, source_index_offset, lines)
    }

    fn transcript_line_can_update_turn_status(&self, line: &str) -> bool {
        claude_line_can_update_turn_status(line)
    }

    fn synthesize_truncated_session(
        &self,
        transcript_path: &Path,
        anchor: &MessageAnchor,
    ) -> Result<String, String> {
        synthesize_truncated_claude_session(transcript_path, anchor)
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
            permission_actions: vec![
                PermissionAction {
                    id: "approve",
                    label: "Approve",
                    input: "y",
                },
                PermissionAction {
                    id: "deny",
                    label: "Deny",
                    input: "n",
                },
            ],
        }
    }
}

impl ClaudeAdapter {
    pub fn shell_fork_args(
        &self,
        source: &AgentInfo,
        _cwd: &Path,
        prompt: Option<&str>,
    ) -> Result<Vec<String>, String> {
        let session_id = source
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|session_id| !session_id.is_empty())
            .ok_or_else(|| {
                "this Claude session isn't ready to fork yet (no session id); send a turn first"
                    .to_string()
            })?;
        Ok(shell_session_args(
            session_id,
            source.model.as_deref(),
            source.effort.as_deref(),
            prompt,
            true,
        ))
    }

    /// Args for a fork anchored at a message. The session id is a transcript
    /// synthesized by `synthesize_truncated_session`, which already ends where
    /// the branch begins — so this resumes it directly rather than passing
    /// `--fork-session`. Forking again would copy the seed into a second
    /// session and leave the seed behind as a duplicate in the picker.
    pub fn shell_fork_at_message_args(
        &self,
        source: &AgentInfo,
        session_id: &str,
        prompt: Option<&str>,
    ) -> Vec<String> {
        shell_session_args(
            session_id,
            source.model.as_deref(),
            source.effort.as_deref(),
            prompt,
            false,
        )
    }

    fn spawn_pane(&self, state: &AppState, request: SpawnAgentRequest) -> Result<PaneInfo, String> {
        let binary = self.ensure_binary()?;
        let options = ClaudeLaunchOptions::from_value(request.options)?;

        let agent = prepare_agent_workspace(
            state,
            PrepareAgentWorkspaceRequest {
                group_id: request.group_id,
                base_repo: request.base_repo,
                base_ref: request.base_ref,
                adapter: self.id().to_string(),
                model: request.model.clone(),
                effort: options.effort.clone(),
                use_worktree: request.use_worktree.unwrap_or(false),
            },
        )?;
        let cwd = request
            .cwd
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&agent.worktree_dir));
        if !cwd.is_dir() {
            return Err(format!(
                "Claude working directory {} does not exist",
                cwd.display()
            ));
        }
        let pane_id = state.next_id("pane");
        let settings_path = match write_hook_settings(state.config(), &pane_id) {
            Ok(settings_path) => settings_path,
            Err(err) => {
                let _ = mark_agent_failed(state, &agent.id);
                return Err(err);
            }
        };
        let mut args = vec![
            "--settings".to_string(),
            settings_path.display().to_string(),
        ];
        args.extend(self.plugin_dir_args());

        if let Some(model) = request.model.filter(|model| !model.trim().is_empty()) {
            args.push("--model".to_string());
            args.push(model);
        }
        push_effort_args(&mut args, agent.effort.as_deref());

        let permission_mode = options.permission_mode.unwrap_or("auto".to_string());
        args.push("--permission-mode".to_string());
        args.push(permission_mode);

        let prompt = request.prompt.trim();
        let has_prompt = !prompt.is_empty();
        if has_prompt {
            // Delimit the prompt with `--` so a prompt that starts with `-` (e.g.
            // queued text delivered to a new session) is parsed as the positional
            // prompt rather than as a Claude flag. Mirrors `fork_pane`.
            args.push("--".to_string());
            args.push(prompt.to_string());
        }

        let mut envs = qmux_pane_envs(state, &pane_id)?;
        envs.push(("QMUX_AGENT_ID".to_string(), agent.id.clone()));

        attach_agent_pane(state, &agent.id, pane_id.clone())?;

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
            Ok(pane) => {
                if !has_prompt {
                    // Launched without a prompt: Claude opens interactively and is
                    // ready, so present the tab as idle rather than working.
                    // Field-scoped write — a full-struct update here would race the
                    // SessionStart hook recording session_id.
                    state.set_agent_status(&agent.id, AgentStatus::Idle)?;
                }
                Ok(pane)
            }
            Err(err) => {
                let _ = mark_agent_spawn_failed(state, &agent.id, &pane_id);
                Err(err)
            }
        }
    }

    /// Forks `source` into a new agent pane: a fresh Claude started with
    /// `--resume <source session> --fork-session [prompt]`, so it inherits the
    /// source's transcript but writes to a new session id (the source is unaffected).
    /// Runs in the source's directory, or a fresh worktree when `use_worktree` is set.
    pub fn fork_pane(
        &self,
        state: &AppState,
        source: &AgentInfo,
        use_worktree: bool,
        prompt: Option<&str>,
    ) -> Result<(PaneInfo, AgentInfo), String> {
        let binary = self.ensure_binary()?;
        let session_id = source
            .session_id
            .clone()
            .map(|session| session.trim().to_string())
            .filter(|session| !session.is_empty())
            .ok_or_else(|| {
                "this Claude session isn't ready to fork yet (no session id); send a turn first"
                    .to_string()
            })?;

        let mut agent = prepare_agent_workspace(
            state,
            PrepareAgentWorkspaceRequest {
                group_id: Some(source.group_id.clone()),
                // Worktree forks branch off the group's base repo; in-place forks run
                // in the source's own directory so they see the same files.
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

        // Record fork lineage and the no-prompt idle status before the process starts,
        // so the fork's own hooks (SessionStart, or the first turn's hooks via
        // adopt_forked_session_identity, which record the new session_id) can't race
        // ahead of the lineage write — the stale-payload guards key off fork_point.
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

        let pane_id = state.next_id("pane");
        let settings_path = match write_hook_settings(state.config(), &pane_id) {
            Ok(settings_path) => settings_path,
            Err(err) => {
                let _ = mark_agent_failed(state, &agent.id);
                return Err(err);
            }
        };

        let mut args = vec![
            "--settings".to_string(),
            settings_path.display().to_string(),
        ];
        args.extend(self.plugin_dir_args());
        if let Some(model) = agent.model.clone().filter(|model| !model.trim().is_empty()) {
            args.push("--model".to_string());
            args.push(model);
        }
        push_effort_args(&mut args, agent.effort.as_deref());
        args.push("--permission-mode".to_string());
        args.push("auto".to_string());
        args.push("--resume".to_string());
        args.push(session_id);
        args.push("--fork-session".to_string());
        let prompt = prompt.map(str::trim).unwrap_or_default();
        let has_prompt = !prompt.is_empty();
        if has_prompt {
            // Delimit the prompt with `--` so a fork prompt that starts with `-`
            // (e.g. a forged `agent.fork` payload of "--dangerously-skip-permissions")
            // is parsed as the positional prompt rather than as a Claude flag that
            // would disable the forked agent's permission prompts.
            args.push("--".to_string());
            args.push(prompt.to_string());
        }

        let mut envs = qmux_pane_envs(state, &pane_id)?;
        envs.push(("QMUX_AGENT_ID".to_string(), agent.id.clone()));

        attach_agent_pane(state, &agent.id, pane_id.clone())?;

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

        let pane = match spawn_result {
            Ok(pane) => pane,
            Err(err) => {
                let _ = mark_agent_spawn_failed(state, &agent.id, &pane_id);
                return Err(err);
            }
        };

        let forked = if has_prompt {
            state
                .agent(&agent.id)?
                .ok_or_else(|| format!("forked agent {} disappeared during spawn", agent.id))?
        } else {
            // Restore Idle after the early pane bind (attach promotes to Running, but a
            // resumed fork with no prompt is simply ready). Use a field-scoped status
            // write, not a full-struct update: the spawned fork's SessionStart hook may
            // already be recording its new session_id/transcript on another thread, and a
            // stale snapshot write here would wipe them.
            state
                .set_agent_status(&agent.id, AgentStatus::Idle)?
                .ok_or_else(|| format!("forked agent {} disappeared during spawn", agent.id))?
        };

        Ok((pane, forked))
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

        let settings_path = write_hook_settings(state.config(), &pane.id)?;
        let mut args = vec![
            "--settings".to_string(),
            settings_path.display().to_string(),
        ];
        args.extend(self.plugin_dir_args());

        if let Some(model) = agent.model.clone().filter(|model| !model.trim().is_empty()) {
            args.push("--model".to_string());
            args.push(model);
        }
        push_effort_args(&mut args, agent.effort.as_deref());

        let resumed = if let Some(session_id) = agent
            .session_id
            .clone()
            .filter(|session_id| !session_id.trim().is_empty())
        {
            args.push("--resume".to_string());
            args.push(session_id);
            true
        } else {
            false
        };

        let mut envs = qmux_pane_envs(state, &pane.id)?;
        envs.push(("QMUX_AGENT_ID".to_string(), agent.id.clone()));

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

        // Re-bind the agent to its restored pane. A recovered Claude process is
        // launched without an inline prompt, even when resuming a session, so it is
        // ready once the TUI appears. The first real prompt/tool hook will promote it
        // to Running.
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

    fn prepare_shell_launch(
        &self,
        state: &AppState,
        request: PrepareShellAgentLaunchRequest,
    ) -> Result<PreparedShellAgentLaunch, String> {
        let binary = self.ensure_binary()?;
        validate_claude_shell_args(&request.args)?;

        if !state.pane_exists(&request.pane_id)? {
            return Err(format!("pane {} was not found", request.pane_id));
        }

        let cwd = PathBuf::from(&request.cwd);
        if !cwd.is_dir() {
            return Err(format!(
                "Claude working directory {} does not exist",
                cwd.display()
            ));
        }

        // A restart-driven resume (`claude --resume <id>`) rebinds the original agent
        // for that session instead of minting a duplicate; any other invocation starts
        // a fresh agent in the current directory.
        let cwd_str = cwd.display().to_string();
        let pane_group_id = state
            .pane_group_id(&request.pane_id)?
            .ok_or_else(|| format!("pane {} was not found", request.pane_id))?;
        let resume_session_id = claude_resume_session_id(&request.args).map(str::to_string);
        let fork_point = claude_fork_source_session_id(&request.args).map(str::to_string);
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
                        // Typing `claude` in a shell runs in the current directory; no worktree.
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
        let settings_path = match write_hook_settings(state.config(), &request.pane_id) {
            Ok(settings_path) => settings_path,
            Err(err) => {
                let _ = mark_agent_failed(state, &agent.id);
                return Err(err);
            }
        };
        let agent = attach_agent_pane(state, &agent.id, request.pane_id.clone())?;
        let agent = if !args_contain_prompt(&request.args) {
            // A bare `claude` (no inline prompt) drops into interactive mode ready
            // for the user, so present the tab as idle rather than working. The
            // first real turn promotes it.
            // Field-scoped write — a full-struct update here would race the
            // SessionStart hook recording session_id; carry the post-write state so
            // the agent.spawned event below ships the right status.
            state
                .set_agent_status(&agent.id, AgentStatus::Idle)?
                .unwrap_or(agent)
        } else {
            agent
        };

        let mut envs = qmux_pane_envs(state, &request.pane_id)?;
        envs.push(("QMUX_AGENT_ID".to_string(), agent.id.clone()));
        let agent_id = agent.id.clone();
        let worktree_dir = agent.worktree_dir.clone();
        state.emit(QmuxEvent::new(
            "agent.spawned",
            Some(request.pane_id),
            Some(agent_id),
            json!({ "agent": agent.clone(), "source": "shell" }),
        ));

        let mut args = vec![
            "--settings".to_string(),
            settings_path.display().to_string(),
        ];
        args.extend(self.plugin_dir_args());
        args.extend(request.args);

        Ok(PreparedShellAgentLaunch {
            binary,
            cwd: worktree_dir,
            args,
            envs: envs
                .into_iter()
                .map(|(key, value)| LaunchEnv { key, value })
                .collect(),
        })
    }

    fn ingest_hook_notification(
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
        // Heal a forked agent whose SessionStart carried the source session's stale
        // metadata (rejected above in the SessionStart arm): every later hook carries
        // the current session's id/transcript, so the fork's first turn binds the real
        // identity. Runs before the event match so even this event's own side effects
        // (e.g. a Stop drain) act on the corrected binding.
        if notification.event != "SessionStart"
            && let Some(current) = agent.as_ref()
        {
            adopt_forked_session_identity(state, self.id(), current, &notification.payload)?;
        }
        let event_type = match notification.event.as_str() {
            "SessionStart" => {
                if let Some(current) = agent.as_ref() {
                    let session_id = super::string_field(&notification.payload, "session_id")
                        .or_else(|| super::string_field(&notification.payload, "sessionId"));
                    let transcript_path =
                        super::string_field(&notification.payload, "transcript_path")
                            .or_else(|| {
                                super::string_field(&notification.payload, "transcriptPath")
                            })
                            // This payload arrives over the control socket under the pane's
                            // token, so a prompt-injected agent can forge a SessionStart.
                            // Validate the path before binding and tailing it — otherwise it
                            // could point the reader at an unrelated file (forging the
                            // timeline the UI shows as an audit surface) or at a device/FIFO.
                            .filter(|candidate| {
                                hook_transcript_path_acceptable(
                                    current.transcript_path.as_deref(),
                                    candidate,
                                )
                            });
                    // A fork (`--resume <src> --fork-session`) can deliver a SessionStart
                    // still carrying the *source* session's id/transcript (stale hook
                    // metadata; the forked session's id can never legitimately equal
                    // fork_point). Adopting it would pin this pane to the source session
                    // — another live tab — and tail the source's transcript, duplicating
                    // its timeline here and letting its abort markers drain this pane's
                    // queue. Drop the payload instead; the fork's first turn binds the
                    // real identity via adopt_forked_session_identity.
                    let stale_fork_payload =
                        current.fork_point.as_deref().is_some_and(|fork_point| {
                            session_id.as_deref() == Some(fork_point)
                                || transcript_path.as_deref().is_some_and(|path| {
                                    session_id_from_transcript_path(Path::new(path)).as_deref()
                                        == Some(fork_point)
                                })
                        });
                    if !stale_fork_payload {
                        // Field-scoped mutation, not a full-struct `update_agent`: this
                        // freshly spawned process's pane is being bound by attach_agent_pane
                        // on another thread, and a stale-snapshot write here would race it —
                        // wiping either the pane_id it set or the session_id we set.
                        let updated = state.mutate_agent(&current.id, |agent| {
                            // Same guard as transcript_path below: only overwrite when this
                            // event carries a session id. A late/duplicate SessionStart that
                            // omits it must not blank a recorded one, which fork + recovery
                            // key off.
                            if let Some(session_id) = session_id {
                                agent.session_id = Some(session_id);
                            }
                            // Only overwrite a known-good transcript path when this event
                            // actually carries one. A SessionStart whose payload omits the
                            // field must not blank the path out from under a running tail,
                            // which would silently freeze the timeline while the agent runs.
                            if let Some(transcript_path) = transcript_path {
                                agent.transcript_path = Some(transcript_path);
                            }
                            // A session starting doesn't mean a turn is running. Keep
                            // status unchanged here; the first real prompt/tool hook
                            // promotes the agent to Running.
                        })?;
                        if let Some(transcript_path) =
                            updated.and_then(|agent| agent.transcript_path)
                        {
                            start_transcript_tail(
                                state.clone(),
                                current.id.clone(),
                                transcript_path,
                                self.id().to_string(),
                            );
                        }
                    }
                }
                "agent.session_start"
            }
            "UserPromptSubmit" => {
                if let Some(agent) = agent.as_mut() {
                    let is_subagent = is_subagent_payload(&notification.payload);
                    let prompt = (!is_subagent)
                        .then(|| super::string_field(&notification.payload, "prompt"))
                        .flatten();
                    if !prompt.as_deref().is_some_and(is_shell_escape_turn) {
                        agent.status = AgentStatus::Running;
                        state.set_agent_status(&agent.id, agent.status)?;
                    }
                    if !is_subagent {
                        // A new main-agent turn supersedes the previous one, so
                        // subagents tracked for it can no longer gate completion.
                        // Clearing here also self-heals a counter wedged by a lost
                        // SubagentStop (interrupt, hook timeout, dropped socket),
                        // which would otherwise suppress every future Stop.
                        state.clear_agent_subagents(&agent.id);
                        send_tracking =
                            Some(state.match_agent_prompt_submit(&agent.id, prompt.as_deref())?);
                    }
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
            "PermissionRequest" => {
                if let Some(agent) = agent.as_mut() {
                    agent.status = AgentStatus::AwaitingPermission;
                    state.set_agent_status(&agent.id, agent.status)?;
                }
                "agent.awaiting_permission"
            }
            "PermissionDenied" => {
                if let Some(agent) = agent.as_mut() {
                    // PermissionDenied is emitted after the decision. Claude may
                    // continue reasoning or recover with another tool, so clear the
                    // pre-decision AwaitingPermission state.
                    agent.status = AgentStatus::Running;
                    state.set_agent_status(&agent.id, agent.status)?;
                }
                "agent.permission_denied"
            }
            "Elicitation" => {
                if let Some(agent) = agent.as_mut() {
                    agent.status = AgentStatus::AwaitingInput;
                    state.set_agent_status(&agent.id, agent.status)?;
                }
                "agent.awaiting_input"
            }
            "ElicitationResult" => {
                if let Some(agent) = agent.as_mut() {
                    agent.status = AgentStatus::Running;
                    state.set_agent_status(&agent.id, agent.status)?;
                }
                "agent.input_resolved"
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
            event if event.starts_with("Notification") => {
                let notification_kind = notification_kind(&notification);
                if matches!(notification_kind, NotificationKind::IdlePrompt) {
                    let waiting_on_subagents = if let Some(agent) = agent.as_mut() {
                        // Idle prompts fire ~60s into any idle stretch,
                        // including one a Stop deliberately left open for
                        // reported background tasks. Honor that recorded wait
                        // here too, or the idle notification would settle Done
                        // and silently cancel it. The flag clears when a later
                        // Stop reports the tasks finished.
                        if state.agent_has_active_subagents(&agent.id)?
                            || state.agent_has_reported_background_tasks(&agent.id)?
                        {
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
                        finish_agent_after_idle(state, agent)?
                    } else {
                        false
                    };
                    if waiting_on_subagents || drained {
                        "agent.running"
                    } else {
                        "agent.done"
                    }
                } else {
                    if let Some(agent) = agent.as_mut() {
                        agent.status = notification_status(notification_kind);
                        state.set_agent_status(&agent.id, agent.status)?;
                    }
                    notification_event_type(notification_kind)
                }
            }
            "Stop" | "StopFailure" => {
                let waiting_on_subagents = if let Some(agent) = agent.as_mut() {
                    // Claude 2.1.145+ reports its live task registry directly on
                    // Stop. This closes the race where a background agent is
                    // launched before qmux observes SubagentStart (and also covers
                    // background work that is not a subagent). Older versions omit
                    // the field and continue to rely on the hook-backed tracker.
                    // Only tasks still marked running count: a registry that
                    // retains completed entries must not pin the agent Running
                    // forever with nothing left to wait for. The verdict is
                    // recorded so the idle-prompt handler honors the same wait
                    // (a snapshot-less Stop leaves the previous verdict alone).
                    if let Some(active) = reported_background_tasks_active(&notification.payload) {
                        state.set_agent_background_tasks_reported(&agent.id, active)?;
                    }
                    if state.agent_has_reported_background_tasks(&agent.id)?
                        || state.agent_has_active_subagents(&agent.id)?
                    {
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
                    finish_agent_after_idle(state, agent)?
                } else {
                    false
                };
                if waiting_on_subagents || drained {
                    "agent.running"
                } else {
                    "agent.done"
                }
            }
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
                    state.set_agent_background_tasks_reported(&agent.id, false)?;
                }
                "agent.session_end"
            }
            other => {
                return Ok(AdapterNotificationOutcome::Event(QmuxEvent::new(
                    format!("agent.hook.{other}"),
                    pane_id,
                    agent.map(|agent| agent.id),
                    json!({
                        "hookEvent": other,
                        "payload": notification.payload,
                    }),
                )));
            }
        };

        let mut event_payload = json!({
            "hookEvent": notification.event,
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

/// Whether a manual `claude …` invocation carries an inline prompt — a positional
/// argument — as opposed to a bare or flags-only launch that drops into Claude's
/// interactive mode and waits for input. Flags that consume a separate value are
/// skipped so e.g. `claude --model sonnet` is not mistaken for carrying a prompt.
/// Erring toward "no prompt" is safe: the agent just starts idle and the first real
/// turn (UserPromptSubmit/PreToolUse) promotes it to running.
fn args_contain_prompt(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            // Everything after the `--` separator is positional.
            return index + 1 < args.len();
        }
        if arg.starts_with('-') {
            // `--flag=value` is self-contained and never consumes the next token.
            if arg.contains('=') {
                index += 1;
                continue;
            }

            if claude_variadic_value_flag(arg) {
                // Commander-style variadic options own every following positional
                // token until the next option. None of those tokens can be a prompt.
                index += 1;
                while index < args.len() && !args[index].starts_with('-') {
                    index += 1;
                }
                continue;
            }

            if claude_value_flag(arg) || claude_optional_value_flag(arg) {
                index += 1;
                if index < args.len() && !args[index].starts_with('-') {
                    index += 1;
                }
                continue;
            }

            if claude_boolean_flag(arg) {
                index += 1;
                continue;
            }

            // A future Claude option is more likely to own its following token than
            // that token is to be an inline prompt. Classify conservatively; a real
            // prompt immediately promotes the agent via UserPromptSubmit anyway.
            index += 1;
            if index < args.len() && !args[index].starts_with('-') {
                index += 1;
            }
            continue;
        }

        // These are administrative subcommands, not interactive prompt text. The
        // wrapper still executes them, but the transient pane binding should not be
        // presented as an agent actively working.
        if claude_utility_command(arg) {
            return false;
        }
        // A bare, non-flag token is an inline prompt.
        return true;
    }
    false
}

fn claude_value_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--agent"
            | "--agents"
            | "--append-system-prompt"
            | "--append-system-prompt-file"
            | "--debug-file"
            | "--effort"
            | "--fallback-model"
            | "--input-format"
            | "--json-schema"
            | "--max-budget-usd"
            | "--max-turns"
            | "--model"
            | "-n"
            | "--name"
            | "--output-format"
            | "--permission-mode"
            | "--permission-prompt-tool"
            | "--plugin-dir"
            | "--plugin-url"
            | "--remote-control-session-name-prefix"
            | "--session-id"
            | "--setting-sources"
            | "--settings"
            | "--system-prompt"
            | "--system-prompt-file"
    )
}

fn claude_variadic_value_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--add-dir"
            | "--allowedTools"
            | "--allowed-tools"
            | "--betas"
            | "--disallowedTools"
            | "--disallowed-tools"
            | "--file"
            | "--mcp-config"
            | "--tools"
    )
}

fn claude_optional_value_flag(arg: &str) -> bool {
    matches!(
        arg,
        "-d" | "--debug"
            | "--from-pr"
            | "--prompt-suggestions"
            | "--remote-control"
            | "-r"
            | "--resume"
            | "-w"
            | "--worktree"
    )
}

fn claude_boolean_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--allow-dangerously-skip-permissions"
            | "--ax-screen-reader"
            | "--background"
            | "--bare"
            | "--bg"
            | "--brief"
            | "-c"
            | "--continue"
            | "--chrome"
            | "--dangerously-skip-permissions"
            | "--disable-slash-commands"
            | "--exclude-dynamic-system-prompt-sections"
            | "--fork-session"
            | "-h"
            | "--help"
            | "--ide"
            | "--include-hook-events"
            | "--include-partial-messages"
            | "--no-chrome"
            | "--no-session-persistence"
            | "-p"
            | "--print"
            | "--replay-user-messages"
            | "--safe-mode"
            | "--strict-mcp-config"
            | "--tmux"
            | "-v"
            | "--verbose"
            | "--version"
    )
}

fn validate_claude_shell_args(args: &[String]) -> Result<(), String> {
    for arg in args.iter().take_while(|arg| arg.as_str() != "--") {
        let reason = match arg.as_str() {
            "--bare" | "--safe-mode" => Some("it disables the lifecycle hooks qMux requires"),
            "--background" | "--bg" => {
                Some("it detaches Claude from the pane that owns the qMux agent integration")
            }
            "--worktree" | "-w" => Some(
                "Claude-created worktrees are not represented in qMux agent workspace state; use qMux's worktree fork instead",
            ),
            "--tmux" => {
                Some("it moves Claude out of the pane that owns the qMux agent integration")
            }
            "--settings" => Some(
                "it can replace the qMux settings file that installs lifecycle hooks; use normal user or project settings instead",
            ),
            _ if arg.starts_with("--worktree=") => Some(
                "Claude-created worktrees are not represented in qMux agent workspace state; use qMux's worktree fork instead",
            ),
            _ if arg.starts_with("--tmux=") => {
                Some("it moves Claude out of the pane that owns the qMux agent integration")
            }
            _ if arg.starts_with("--settings=") => Some(
                "it can replace the qMux settings file that installs lifecycle hooks; use normal user or project settings instead",
            ),
            _ => None,
        };
        if let Some(reason) = reason {
            return Err(format!(
                "qMux Claude integration does not support {arg} because {reason}"
            ));
        }
    }
    Ok(())
}

fn claude_utility_command(arg: &str) -> bool {
    matches!(
        arg,
        "agents"
            | "auth"
            | "auto-mode"
            | "doctor"
            | "gateway"
            | "install"
            | "mcp"
            | "plugin"
            | "plugins"
            | "project"
            | "setup-token"
            | "update"
            | "upgrade"
    )
}

/// Extracts the session id from a `--resume <id>` / `-r <id>` / `--resume=<id>` shell
/// argument list, so a resume launch can rebind the original agent. `None` when the
/// invocation isn't resuming a specific session.
fn claude_resume_session_id(args: &[String]) -> Option<&str> {
    // A native Claude fork resumes the source transcript but deliberately creates a
    // different session. Reusing the source qmux record would let the fork's hooks
    // overwrite the source tab's session/transcript identity.
    if args
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--fork-session")
    {
        return None;
    }

    claude_resume_argument_id(args)
}

fn claude_fork_source_session_id(args: &[String]) -> Option<&str> {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--fork-session")
        .then(|| claude_resume_argument_id(args))
        .flatten()
}

fn claude_resume_argument_id(args: &[String]) -> Option<&str> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if let Some(id) = arg.strip_prefix("--resume=") {
            return (!id.is_empty()).then_some(id);
        }
        if arg == "--resume" || arg == "-r" {
            return iter
                .next()
                .map(String::as_str)
                .filter(|id| !id.starts_with('-'));
        }
    }
    None
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClaudeLaunchOptions {
    #[serde(default)]
    permission_mode: Option<String>,
    #[serde(default)]
    effort: Option<String>,
}

impl ClaudeLaunchOptions {
    fn from_value(value: Value) -> Result<Self, String> {
        if value.is_null() {
            return Ok(Self {
                permission_mode: None,
                effort: None,
            });
        }
        let mut options: ClaudeLaunchOptions = serde_json::from_value(value)
            .map_err(|err| format!("invalid Claude adapter options: {err}"))?;
        options.permission_mode = match options.permission_mode.map(|mode| mode.trim().to_string())
        {
            Some(permission_mode) if permission_mode.is_empty() => None,
            Some(permission_mode)
                if CLAUDE_PERMISSION_MODES.contains(&permission_mode.as_str()) =>
            {
                Some(permission_mode)
            }
            Some(permission_mode) => {
                return Err(format!(
                    "invalid Claude adapter option permissionMode='{permission_mode}'; expected one of {}",
                    CLAUDE_PERMISSION_MODES.join(", ")
                ));
            }
            None => None,
        };
        options.effort = match options.effort.map(|effort| effort.trim().to_string()) {
            Some(effort) if effort.is_empty() => None,
            Some(effort) if CLAUDE_EFFORT_LEVELS.contains(&effort.as_str()) => Some(effort),
            Some(effort) => {
                return Err(format!(
                    "invalid Claude adapter option effort='{effort}'; expected one of {}",
                    CLAUDE_EFFORT_LEVELS.join(", ")
                ));
            }
            None => None,
        };
        Ok(options)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnClaudeRequest {
    pub prompt: String,
    pub group_id: Option<String>,
    pub base_repo: Option<String>,
    pub base_ref: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    pub initial_size: Option<InitialPaneSize>,
    /// Opt in to an isolated git worktree; defaults to false (run in place).
    pub use_worktree: Option<bool>,
}

impl SpawnClaudeRequest {
    pub fn into_agent_request(self) -> SpawnAgentRequest {
        let mut option_fields = serde_json::Map::new();
        if let Some(permission_mode) = self.permission_mode {
            option_fields.insert("permissionMode".to_string(), json!(permission_mode));
        }
        if let Some(effort) = self.effort {
            option_fields.insert("effort".to_string(), json!(effort));
        }
        let options = if option_fields.is_empty() {
            Value::Null
        } else {
            Value::Object(option_fields)
        };

        SpawnAgentRequest {
            adapter_id: "claude".to_string(),
            prompt: self.prompt,
            group_id: self.group_id,
            base_repo: self.base_repo,
            base_ref: self.base_ref,
            cwd: self.cwd,
            model: self.model,
            initial_size: self.initial_size,
            use_worktree: self.use_worktree,
            options,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepareShellClaudeLaunchRequest {
    pub pane_id: String,
    pub cwd: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// Generates a random hex nonce for a per-spawn hook-settings filename, so the path
/// is not guessable by another same-user process trying to pre-plant or locate it.
fn hook_settings_nonce() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|err| format!("failed to generate hook settings nonce: {err}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Writes a per-pane, per-spawn Claude hook-settings file and returns its path for
/// `--settings`.
///
/// This used to write one fixed, shared `.qmux/qmux-hooks.json` that every Claude
/// pane loaded. Because that path was stable and writable by any process running as
/// the desktop user, a prompt-injected agent in one pane could overwrite it to inject
/// a lifecycle-hook command into *another* pane's Claude — which runs with that pane's
/// `QMUX_TOKEN` — crossing qmux's per-pane authority boundary. Writing a fresh,
/// unpredictably-named file per spawn under a `0700` `.qmux/hooks/` dir, created with
/// `O_EXCL` at `0600`, removes the shared target and the pre-planted-file/symlink
/// race: there is no stable path to overwrite, and a file planted at our random path
/// makes the exclusive create fail (we error out rather than write through it). A
/// same-user process can still overwrite our specific file in the window before Claude
/// reads it — same-uid file tampering is not preventable — but it can no longer target
/// a shared file or a guessable per-pane path.
pub fn write_hook_settings(config: &QmuxConfig, pane_id: &str) -> Result<PathBuf, String> {
    // qmux mints pane ids itself, but validate before using one in a filename so a
    // malformed caller can never traverse out of the hooks dir or spoof another pane's
    // prefix during the prune below.
    if pane_id.is_empty()
        || !pane_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(format!("invalid pane id for hook settings: {pane_id:?}"));
    }

    let hooks_dir = config.workspace_root.join(".qmux").join("hooks");
    fs::create_dir_all(&hooks_dir)
        .map_err(|err| format!("failed to create {}: {err}", hooks_dir.display()))?;
    // Keep the hooks dir owner-only (the enclosing .qmux is already 0700; enforce it
    // here too so the per-spawn files are never group/other-readable).
    fs::set_permissions(&hooks_dir, fs::Permissions::from_mode(0o700))
        .map_err(|err| format!("failed to restrict {}: {err}", hooks_dir.display()))?;

    // Drop this pane's previous hook file(s) so the directory stays bounded to one file
    // per live pane instead of growing on every spawn/resume.
    if let Ok(entries) = fs::read_dir(&hooks_dir) {
        let prefix = format!("{pane_id}-");
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(&prefix))
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    let qmux_cli = env::current_exe()
        .map_err(|err| format!("failed to resolve qmux executable for hooks: {err}"))?;
    let mut hooks = serde_json::Map::new();
    for event in CLAUDE_HOOK_EVENTS {
        hooks.insert(
            event.to_string(),
            json!([
                {
                    "matcher": "",
                    "hooks": [
                        {
                            "type": "command",
                            "command": format!("{} notify {}", shell_quote_path(&qmux_cli), event)
                        }
                    ]
                }
            ]),
        );
    }
    hooks.insert(
        "Notification".to_string(),
        json!(
            CLAUDE_NOTIFICATION_MATCHERS
                .iter()
                .map(|(matcher, event)| json!({
                    "matcher": matcher,
                    "hooks": [
                        {
                            "type": "command",
                            "command": format!("{} notify {}", shell_quote_path(&qmux_cli), event)
                        }
                    ]
                }))
                .collect::<Vec<_>>()
        ),
    );

    let settings = json!({ "hooks": hooks });
    let raw = serde_json::to_string_pretty(&settings)
        .map_err(|err| format!("failed to encode hook settings: {err}"))?;

    let settings_path = hooks_dir.join(format!("{pane_id}-{}.json", hook_settings_nonce()?));
    // create_new (O_CREAT|O_EXCL) at 0600: never follow/truncate a pre-existing path,
    // so a planted file or symlink at our random target fails the create instead of
    // being written through.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&settings_path)
        .map_err(|err| format!("failed to create {}: {err}", settings_path.display()))?;
    file.write_all(raw.as_bytes())
        .map_err(|err| format!("failed to write {}: {err}", settings_path.display()))?;
    Ok(settings_path)
}

/// A skill the qmux-managed plugin makes available to launched Claude agents.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeSkill {
    /// Stable unique identifier — the skill's directory name. Used as the launcher
    /// key and selection id, so it must be unique even if two skills declare the
    /// same frontmatter `name:`.
    pub id: String,
    /// Human label for the launcher checkbox (e.g. `Deep Research`).
    pub name: String,
    /// Slash command that invokes the skill, namespaced by the plugin
    /// (e.g. `/qmux:deep-research`).
    pub command: String,
}

/// Enumerates the skills inside the qmux-managed Claude plugin (`<plugin>/skills/*`).
/// Returns an empty list when the plugin directory is absent so the launcher simply
/// shows no skill checkboxes rather than erroring.
pub fn list_skills(config: &QmuxConfig) -> Vec<ClaudeSkill> {
    let plugin_dir = &config.claude_plugin_dir;
    let skills_dir = plugin_dir.join("skills");
    let Ok(entries) = fs::read_dir(&skills_dir) else {
        return Vec::new();
    };
    let namespace = plugin_namespace(plugin_dir);

    let mut skills: Vec<ClaudeSkill> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .filter_map(|entry| {
            let skill_md = entry.path().join("SKILL.md");
            if !skill_md.is_file() {
                return None;
            }
            // Skills are inline-only by default — invoked mid-conversation as a slash
            // command (fork, open-in-browser, …), which makes no sense as a "New agent"
            // launch. Only skills that explicitly opt in with `qmux-launcher: true`
            // appear in the new-agent launcher. Filtering here does not affect Claude's own
            // ability to run any skill inline; it loads the plugin dir independently.
            if !skill_shows_in_launcher(&skill_md) {
                return None;
            }
            // The slug that names the skill to Claude comes from frontmatter `name:`
            // (falling back to the directory name); the command is namespaced by the
            // plugin. The `id` is always the directory name so it stays unique even
            // when two skills share a frontmatter name.
            let dir_name = entry.file_name().to_string_lossy().into_owned();
            let slug = skill_frontmatter_name(&skill_md).unwrap_or_else(|| dir_name.clone());
            Some(ClaudeSkill {
                command: format!("/{namespace}:{slug}"),
                name: humanize_skill_slug(&slug),
                id: dir_name,
            })
        })
        .collect();
    skills.sort_by_key(|a| a.name.to_lowercase());
    skills
}

/// The plugin's namespace, taken from `.claude-plugin/plugin.json`'s `name`, which
/// is how Claude prefixes the skill's slash command. When the manifest is missing or
/// nameless, fall back to the plugin directory name (Claude's own default) rather
/// than a hardcoded `qmux`, so the displayed command matches what Claude registers.
fn plugin_namespace(plugin_dir: &Path) -> String {
    let manifest = plugin_dir.join(".claude-plugin").join("plugin.json");
    fs::read_to_string(&manifest)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| super::string_field(&value, "name"))
        .filter(|name| !name.trim().is_empty())
        .or_else(|| {
            plugin_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "qmux".to_string())
}

/// Reads the `name:` value from a SKILL.md YAML frontmatter block. Cheap and safe:
/// it only scans the leading `---` fenced block and never parses the body. Matches
/// the top-level (column-0) `name:` key only — a nested `metadata:\n  name: ...` is
/// skipped — and strips inline `#` comments from unquoted values.
fn skill_frontmatter_name(skill_md: &Path) -> Option<String> {
    let raw = fs::read_to_string(skill_md).ok()?;
    let mut lines = raw.lines();
    if lines.next().map(str::trim) != Some("---") {
        return None;
    }
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        // Use the raw line (not trimmed) so indented keys nested under another
        // mapping are not mistaken for the top-level skill name.
        let Some(rest) = line.strip_prefix("name:") else {
            continue;
        };
        let value = rest.trim();
        let name = if value.starts_with('"') || value.starts_with('\'') {
            value.trim_matches(['"', '\'']).trim()
        } else {
            // An unescaped ` #` (or a leading `#`) starts a YAML comment.
            let value = value.split(" #").next().unwrap_or(value).trim();
            if value.starts_with('#') { "" } else { value }
        };
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

/// Whether a skill opts into the new-agent launcher via a top-level `qmux-launcher: true`
/// frontmatter key. Skills are inline-only (invoked mid-conversation) by default, so a
/// skill is hidden from the "New agent" launcher unless it explicitly opts in. Scans
/// the same leading `---` block as `skill_frontmatter_name` and, like it, matches only
/// the column-0 key — an indented `metadata:\n  qmux-launcher: ...` does not count.
fn skill_shows_in_launcher(skill_md: &Path) -> bool {
    let Ok(raw) = fs::read_to_string(skill_md) else {
        return false;
    };
    let mut lines = raw.lines();
    if lines.next().map(str::trim) != Some("---") {
        return false;
    }
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        // Use the raw line so an indented key nested under another mapping is ignored.
        let Some(rest) = line.strip_prefix("qmux-launcher:") else {
            continue;
        };
        // Strip an inline `#` comment and optional quotes, then require an explicit
        // `true` (case-insensitive) — anything else, including absent, reads as opt-out.
        let value = rest
            .split(" #")
            .next()
            .unwrap_or(rest)
            .trim()
            .trim_matches(['"', '\'']);
        return value.eq_ignore_ascii_case("true");
    }
    false
}

/// Turns a skill slug into a launcher label by splitting on `-`/`_` and sentence-casing
/// the result (only the first word is capitalized): `deep-research` -> `Deep research`.
fn humanize_skill_slug(slug: &str) -> String {
    let joined = slug
        .split(['-', '_'])
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let mut chars = joined.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
        None => String::new(),
    }
}

/// Resolves an idle agent: drains the next queued turn, or enters/stays paused.
/// Returns whether a turn was drained (→ `agent.running`), else not (→ `agent.done`).
/// Status/paused are written by `advance_after_idle`; nothing is set on the passed
/// agent (its only later use is its id for the emitted event).
fn finish_agent_after_idle(state: &AppState, agent: &AgentInfo) -> Result<bool, String> {
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

fn notification_event_type(notification_kind: NotificationKind) -> &'static str {
    match notification_kind {
        NotificationKind::PermissionPrompt => "agent.awaiting_permission",
        NotificationKind::IdlePrompt => "agent.done",
        NotificationKind::ElicitationDialog => "agent.awaiting_input",
        NotificationKind::Other => "agent.notification",
    }
}

fn notification_status(notification_kind: NotificationKind) -> AgentStatus {
    match notification_kind {
        NotificationKind::PermissionPrompt => AgentStatus::AwaitingPermission,
        NotificationKind::IdlePrompt => AgentStatus::Done,
        NotificationKind::ElicitationDialog => AgentStatus::AwaitingInput,
        NotificationKind::Other => AgentStatus::AwaitingInput,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NotificationKind {
    PermissionPrompt,
    IdlePrompt,
    ElicitationDialog,
    Other,
}

fn notification_kind(notification: &AdapterNotification) -> NotificationKind {
    match notification.event.as_str() {
        "Notification.permission_prompt" => return NotificationKind::PermissionPrompt,
        "Notification.idle_prompt" => return NotificationKind::IdlePrompt,
        "Notification.elicitation_dialog" => return NotificationKind::ElicitationDialog,
        _ => {}
    }

    if payload_contains(&notification.payload, "permission_prompt") {
        NotificationKind::PermissionPrompt
    } else if payload_contains(&notification.payload, "idle_prompt") {
        NotificationKind::IdlePrompt
    } else if payload_contains(&notification.payload, "elicitation_dialog") {
        NotificationKind::ElicitationDialog
    } else {
        NotificationKind::Other
    }
}

fn payload_contains(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(value) => value.contains(needle),
        Value::Array(values) => values.iter().any(|value| payload_contains(value, needle)),
        Value::Object(values) => values.iter().any(|(key, value)| {
            key.contains(needle)
                || value.as_str().is_some_and(|value| value.contains(needle))
                || payload_contains(value, needle)
        }),
        _ => false,
    }
}

/// Reads Claude's `background_tasks` registry snapshot from a Stop payload.
/// `None` when the field is absent (older Claude versions — callers keep any
/// previously recorded verdict and the hook-backed subagent tracker applies);
/// otherwise whether any reported task is still running. Entries without a
/// status count as running so a schema drift degrades toward waiting one Stop
/// too long rather than settling Done under live background work; entries with
/// a terminal status must not pin the agent, or a registry that retains
/// completed tasks would wedge the session Running forever.
fn reported_background_tasks_active(payload: &Value) -> Option<bool> {
    let tasks = payload.get("background_tasks")?.as_array()?;
    Some(tasks.iter().any(|task| {
        task.get("status")
            .and_then(Value::as_str)
            .is_none_or(|status| status.eq_ignore_ascii_case("running"))
    }))
}

fn is_subagent_payload(value: &Value) -> bool {
    value.get("agent_id").is_some() || value.get("agentId").is_some()
}

/// One-shot correction for a forked agent whose SessionStart delivered the source
/// session's metadata (or was rejected as stale, leaving the fork unbound): adopt the
/// current session's id and transcript from a later hook payload.
///
/// Deliberately narrow, so inconsistent hook metadata can never flap the binding:
/// - Applies only while the recorded session id is missing or still equals
///   `fork_point` — after the first adoption of a distinct id the condition is false
///   forever, and no later hook (subagent or otherwise) can move the binding again.
/// - All-or-nothing: adopts only a consistent (session id, transcript path) pair —
///   the path must pass the same forgery guard as SessionStart and must encode the
///   adopted session id, so the header can never point at one session while the tail
///   follows another.
/// - Never adopts the source's own id (`fork_point`): with `--fork-session` the new
///   session can't legitimately equal it, so such a payload is stale by definition.
/// - Subagent payloads are skipped; their metadata describes a sidechain, not the pane.
fn adopt_forked_session_identity(
    state: &AppState,
    adapter_id: &str,
    current: &AgentInfo,
    payload: &Value,
) -> Result<(), String> {
    let Some(fork_point) = current.fork_point.clone() else {
        return Ok(());
    };
    let unbound_or_stale =
        |session_id: Option<&str>| session_id.is_none() || session_id == Some(fork_point.as_str());
    if !unbound_or_stale(current.session_id.as_deref()) || is_subagent_payload(payload) {
        return Ok(());
    }
    let Some(session_id) = super::string_field(payload, "session_id")
        .or_else(|| super::string_field(payload, "sessionId"))
        .map(|session_id| session_id.trim().to_string())
        .filter(|session_id| !session_id.is_empty() && *session_id != fork_point)
    else {
        return Ok(());
    };
    let Some(transcript_path) = super::string_field(payload, "transcript_path")
        .or_else(|| super::string_field(payload, "transcriptPath"))
        .filter(|candidate| {
            hook_transcript_path_acceptable(current.transcript_path.as_deref(), candidate)
                && session_id_from_transcript_path(Path::new(candidate)).as_deref()
                    == Some(session_id.as_str())
        })
    else {
        return Ok(());
    };
    // Field-scoped mutation with the staleness re-checked under the model lock, so a
    // concurrent adopter (another hook mid-flight) that already bound a distinct id
    // is never overwritten.
    let updated = state.mutate_agent(&current.id, |agent| {
        if unbound_or_stale(agent.session_id.as_deref()) {
            agent.session_id = Some(session_id.clone());
            agent.transcript_path = Some(transcript_path.clone());
        }
    })?;
    // Tail only if our pair actually landed (ours, or an identical concurrent
    // adoption — start_transcript_tail dedupes per path either way).
    if updated
        .as_ref()
        .and_then(|agent| agent.transcript_path.as_deref())
        == Some(transcript_path.as_str())
    {
        start_transcript_tail(
            state.clone(),
            current.id.clone(),
            transcript_path,
            adapter_id.to_string(),
        );
    }
    Ok(())
}

/// Whether a transcript path reported by a Claude hook notification may be bound.
///
/// A hook arrives over the control socket carrying the pane's token, so a
/// prompt-injected agent can forge one. We can't fully validate the *first* path —
/// SessionStart is how qmux discovers it, and Claude may not have written the file
/// to disk yet — but we require a `.jsonl` extension, and once the agent is bound
/// we require any later path to be a sibling in the same session directory. Claude
/// keeps a project's sessions in one flat directory, so a legitimate rotation
/// (compact, resume) stays a sibling, while a forged mid-session hook can no longer
/// relocate the tail to an unrelated file.
fn hook_transcript_path_acceptable(current: Option<&str>, candidate: &str) -> bool {
    // Single-sourced in the adapters module so the Claude inline callsite and the shared
    // hook handling can never drift apart.
    super::hook_transcript_path_acceptable(current, candidate)
}

fn parse_transcript_line(agent_id: &str, source_index: usize, line: &str) -> Option<Turn> {
    super::parse_claude_native_transcript_line(agent_id, source_index, line)
}

#[derive(Clone, Debug)]
struct ClaudeGraphNode {
    uuid: String,
    parent_uuid: Option<String>,
    source_index: usize,
    is_typed_user_prompt: bool,
}

#[cfg(test)]
fn resolve_transcript_turns(agent_id: &str, lines: &[String]) -> Vec<Turn> {
    resolve_transcript_turns_from(agent_id, 0, lines)
}

fn resolve_transcript_turns_from(
    agent_id: &str,
    source_index_offset: usize,
    lines: &[String],
) -> Vec<Turn> {
    let mut turns = Vec::new();
    let mut nodes_by_uuid: HashMap<String, ClaudeGraphNode> = HashMap::new();
    let mut children_by_parent: HashMap<String, Vec<String>> = HashMap::new();
    let mut leaf_uuids = Vec::new();
    let mut interrupted_uuids = HashSet::new();
    let mut interrupted_message_ids = HashSet::new();

    for (relative_index, line) in lines.iter().enumerate() {
        let source_index = source_index_offset + relative_index;
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(_) => continue,
        };

        if value.get("type").and_then(Value::as_str) == Some("last-prompt")
            && let Some(leaf_uuid) = super::string_field(&value, "leafUuid")
                .or_else(|| super::string_field(&value, "leaf_uuid"))
        {
            leaf_uuids.push(leaf_uuid);
        }

        if super::parse_claude_native_lifecycle_value(&value).is_some()
            && let Some(uuid) = super::string_field(&value, "uuid")
        {
            interrupted_uuids.insert(uuid);
        }
        if let Some(interrupted_message_id) = super::string_field(&value, "interruptedMessageId")
            .or_else(|| super::string_field(&value, "interrupted_message_id"))
        {
            interrupted_message_ids.insert(interrupted_message_id);
        }

        if let Some(uuid) = super::string_field(&value, "uuid") {
            let parent_uuid = super::string_field(&value, "parentUuid")
                .or_else(|| super::string_field(&value, "parent_uuid"));
            if let Some(parent_uuid) = parent_uuid.as_ref() {
                children_by_parent
                    .entry(parent_uuid.clone())
                    .or_default()
                    .push(uuid.clone());
            }
            nodes_by_uuid.insert(
                uuid.clone(),
                ClaudeGraphNode {
                    uuid,
                    parent_uuid,
                    source_index,
                    is_typed_user_prompt: is_claude_typed_user_prompt(&value),
                },
            );
        }

        if let Some(turn) =
            super::parse_claude_native_transcript_value(agent_id, source_index, &value)
        {
            turns.push(turn);
        }
    }

    let active_uuids = selected_claude_leaf(&nodes_by_uuid, &leaf_uuids)
        .map(|leaf_uuid| claude_ancestor_set(leaf_uuid, &nodes_by_uuid))
        .unwrap_or_default();
    let (superseded_uuids, uncertain_uuids) =
        resolve_claude_prompt_branch_statuses(&nodes_by_uuid, &children_by_parent, &active_uuids);

    for turn in &mut turns {
        let native_id = turn.native_id.as_deref();
        if native_id.is_some_and(|uuid| superseded_uuids.contains(uuid)) {
            turn.status = Some(TurnStatus::Superseded);
            turn.status_reason = Some(TurnStatusReason::ClaudePromptBranch);
        } else if native_id.is_some_and(|uuid| interrupted_uuids.contains(uuid))
            || turn
                .native_message_id
                .as_deref()
                .is_some_and(|message_id| interrupted_message_ids.contains(message_id))
        {
            turn.status = Some(TurnStatus::Interrupted);
            turn.status_reason = Some(TurnStatusReason::Interrupted);
        } else if native_id.is_some_and(|uuid| uncertain_uuids.contains(uuid)) {
            turn.status = Some(TurnStatus::Uncertain);
            turn.status_reason = Some(TurnStatusReason::UnknownBranch);
        }
    }

    turns
}

fn selected_claude_leaf<'a>(
    nodes_by_uuid: &'a HashMap<String, ClaudeGraphNode>,
    leaf_uuids: &'a [String],
) -> Option<&'a str> {
    let latest_typed_user = nodes_by_uuid
        .values()
        .filter(|node| node.is_typed_user_prompt)
        .max_by_key(|node| node.source_index);

    if let Some(latest_typed_user) = latest_typed_user {
        return leaf_uuids
            .iter()
            .rev()
            .find(|leaf_uuid| {
                claude_ancestor_set(leaf_uuid, nodes_by_uuid)
                    .contains(latest_typed_user.uuid.as_str())
            })
            .map(String::as_str);
    }

    leaf_uuids
        .iter()
        .rev()
        .find(|leaf_uuid| nodes_by_uuid.contains_key(leaf_uuid.as_str()))
        .map(String::as_str)
}

fn resolve_claude_prompt_branch_statuses(
    nodes_by_uuid: &HashMap<String, ClaudeGraphNode>,
    children_by_parent: &HashMap<String, Vec<String>>,
    active_uuids: &HashSet<String>,
) -> (HashSet<String>, HashSet<String>) {
    let mut superseded_uuids = HashSet::new();
    let mut uncertain_uuids = HashSet::new();

    for child_uuids in children_by_parent.values() {
        let typed_children = child_uuids
            .iter()
            .filter_map(|uuid| nodes_by_uuid.get(uuid))
            .filter(|node| node.is_typed_user_prompt)
            .collect::<Vec<_>>();
        if typed_children.len() < 2 {
            continue;
        }

        let active_count = typed_children
            .iter()
            .filter(|node| active_uuids.contains(node.uuid.as_str()))
            .count();
        match active_count {
            1 => {
                for node in typed_children {
                    if !active_uuids.contains(node.uuid.as_str()) {
                        mark_claude_subtree(
                            node.uuid.as_str(),
                            children_by_parent,
                            &mut superseded_uuids,
                        );
                    }
                }
            }
            0 => {
                for node in typed_children {
                    mark_claude_subtree(
                        node.uuid.as_str(),
                        children_by_parent,
                        &mut uncertain_uuids,
                    );
                }
            }
            _ => {}
        }
    }

    (superseded_uuids, uncertain_uuids)
}

fn mark_claude_subtree(
    root_uuid: &str,
    children_by_parent: &HashMap<String, Vec<String>>,
    output: &mut HashSet<String>,
) {
    let mut stack = vec![root_uuid.to_string()];
    while let Some(uuid) = stack.pop() {
        if !output.insert(uuid.clone()) {
            continue;
        }
        if let Some(children) = children_by_parent.get(uuid.as_str()) {
            stack.extend(children.iter().cloned());
        }
    }
}

fn claude_ancestor_set(
    leaf_uuid: &str,
    nodes_by_uuid: &HashMap<String, ClaudeGraphNode>,
) -> HashSet<String> {
    let mut ancestors = HashSet::new();
    let mut current = Some(leaf_uuid);
    let mut guard = 0usize;
    while let Some(uuid) = current {
        if guard >= nodes_by_uuid.len().saturating_add(1) || !ancestors.insert(uuid.to_string()) {
            break;
        }
        guard += 1;
        current = nodes_by_uuid
            .get(uuid)
            .and_then(|node| node.parent_uuid.as_deref());
    }
    ancestors
}

/// Writes the ancestor chain of the message *before* `anchor` to a new session
/// file beside the source, producing a transcript that ends where the fork
/// begins. Claude discovers it by scanning the project directory, and derives
/// the session id from the filename, so the copy needs no registration beyond
/// existing at `<project>/<uuid>.jsonl`.
///
/// Records are copied opaquely apart from `sessionId`: the format carries
/// fields we neither model nor need to understand, and rewriting only the one
/// field that must change keeps this tolerant of upstream additions.
///
/// Note there is no `last-prompt`/`leafUuid` marker written here. Appending one
/// to an untruncated copy looks like it should steer the resume to an earlier
/// leaf, but Claude ignores it and replays the full history — truncation is the
/// only mechanism that actually works.
/// Appends `--effort <level>` when the session has a recorded reasoning effort,
/// mirroring how `--model` is re-applied on every launch shape.
fn push_effort_args(args: &mut Vec<String>, effort: Option<&str>) {
    if let Some(effort) = effort.map(str::trim).filter(|effort| !effort.is_empty()) {
        args.push("--effort".to_string());
        args.push(effort.to_string());
    }
}

/// The shared shape of a resume-based launch: optional model and effort, the
/// permission mode forks run under, the session to resume, and the prompt. `--`
/// delimits the prompt so a leading `-` in attacker-influenced text cannot be
/// read as a flag (e.g. `--dangerously-skip-permissions`).
fn shell_session_args(
    session_id: &str,
    model: Option<&str>,
    effort: Option<&str>,
    prompt: Option<&str>,
    fork_session: bool,
) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    push_effort_args(&mut args, effort);
    args.push("--permission-mode".to_string());
    args.push("auto".to_string());
    args.push("--resume".to_string());
    args.push(session_id.to_string());
    if fork_session {
        args.push("--fork-session".to_string());
    }
    if let Some(prompt) = prompt.map(str::trim).filter(|prompt| !prompt.is_empty()) {
        args.push("--".to_string());
        args.push(prompt.to_string());
    }
    args
}

fn synthesize_truncated_claude_session(
    transcript_path: &Path,
    anchor: &MessageAnchor,
) -> Result<String, String> {
    // The fork keeps everything up to and including the anchor's parent; the
    // anchored message itself is replaced by the caller's prompt.
    let keep_leaf = anchor
        .parent_native_id
        .as_deref()
        .filter(|uuid| !uuid.is_empty())
        .ok_or_else(|| FORK_AT_MESSAGE_EMPTY_ERROR.to_string())?;

    let contents = fs::read_to_string(transcript_path).map_err(|err| {
        format!(
            "cannot read transcript {}: {err}",
            transcript_path.display()
        )
    })?;
    let records = parse_transcript_records(&contents);

    let mut nodes_by_uuid: HashMap<String, ClaudeGraphNode> = HashMap::new();
    for (index, (_, value)) in records.iter().enumerate() {
        if let Some(uuid) = super::string_field(value, "uuid") {
            nodes_by_uuid.insert(
                uuid.clone(),
                ClaudeGraphNode {
                    uuid,
                    parent_uuid: super::string_field(value, "parentUuid")
                        .or_else(|| super::string_field(value, "parent_uuid")),
                    source_index: index,
                    is_typed_user_prompt: false,
                },
            );
        }
    }

    if !nodes_by_uuid.contains_key(keep_leaf) {
        return Err(format!(
            "fork anchor {keep_leaf} is not present in {}",
            transcript_path.display()
        ));
    }

    // The anchor is built from a rendered turn, which can lag the file — a
    // compaction or a rewind may have relinked the record since. Re-deriving the
    // parent from the transcript and requiring it to match means a stale anchor
    // fails loudly instead of silently forking from the wrong point.
    if let Some(native_id) = anchor.native_id.as_deref().filter(|id| !id.is_empty()) {
        let actual_parent = nodes_by_uuid
            .get(native_id)
            .ok_or_else(|| {
                format!(
                    "fork anchor {native_id} is not present in {}",
                    transcript_path.display()
                )
            })?
            .parent_uuid
            .as_deref();
        if actual_parent != Some(keep_leaf) {
            return Err(format!(
                "fork anchor {native_id} no longer follows {keep_leaf} in {}",
                transcript_path.display()
            ));
        }
    }

    let keep = claude_ancestor_set(keep_leaf, &nodes_by_uuid);
    let session_id = new_uuid_v4()?;
    let parent_dir = transcript_path
        .parent()
        .ok_or_else(|| format!("transcript {} has no parent", transcript_path.display()))?;
    let out_path = parent_dir.join(format!("{session_id}.jsonl"));

    let mut body = String::new();
    let mut kept = 0usize;
    for (line, value) in &records {
        let Some(uuid) = super::string_field(value, "uuid") else {
            continue;
        };
        if !keep.contains(&uuid) {
            continue;
        }
        match value.get("sessionId") {
            Some(_) => {
                let mut rewritten = value.clone();
                rewritten["sessionId"] = json!(session_id);
                body.push_str(&serde_json::to_string(&rewritten).map_err(|err| err.to_string())?);
            }
            None => body.push_str(line),
        }
        body.push('\n');
        kept += 1;
    }

    if kept == 0 {
        return Err(FORK_AT_MESSAGE_EMPTY_ERROR.to_string());
    }

    fs::write(&out_path, body)
        .map_err(|err| format!("cannot write fork transcript {}: {err}", out_path.display()))?;

    Ok(session_id)
}

fn claude_line_can_update_turn_status(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    value.get("type").and_then(Value::as_str) == Some("last-prompt")
        || super::parse_claude_native_lifecycle_value(&value).is_some()
        || is_claude_typed_user_prompt(&value)
}

fn is_claude_typed_user_prompt(value: &Value) -> bool {
    if value.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let message = value.get("message").unwrap_or(value);
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    if value
        .get("isMeta")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || value
            .get("isSidechain")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return false;
    }
    if is_claude_tool_result(value) || super::parse_claude_native_lifecycle_value(value).is_some() {
        return false;
    }
    let Some(content) = message.get("content").or_else(|| value.get("content")) else {
        return false;
    };
    let text = claude_content_text(content);
    let trimmed = text.trim_start();
    !trimmed.is_empty() && !is_claude_automated_user_text(trimmed)
}

fn is_claude_tool_result(value: &Value) -> bool {
    value.get("toolUseResult").is_some()
        || value.get("sourceToolAssistantUUID").is_some()
        || value
            .get("message")
            .and_then(|message| message.get("content"))
            .or_else(|| value.get("content"))
            .is_some_and(|content| claude_content_has_block_type(content, "tool_result"))
}

fn claude_content_has_block_type(content: &Value, block_type: &str) -> bool {
    matches!(
        content,
        Value::Array(items) if items
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some(block_type))
    )
}

fn claude_content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn is_claude_automated_user_text(text: &str) -> bool {
    const AUTOMATED_PREFIXES: &[&str] = &[
        "<task-notification>",
        "<local-command",
        "<command-name>",
        "<bash-",
        "<system-reminder>",
        "<local-command-caveat>",
    ];
    AUTOMATED_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

fn parse_transcript_lifecycle_event(line: &str) -> Option<TranscriptLifecycleEvent> {
    super::parse_claude_native_lifecycle_event(line)
}

// Thin wrappers so existing tests inside this module continue to call by the original names.
// These are test-only because the main Claude transcript parsing logic now delegates directly.
#[cfg(test)]
#[allow(dead_code)]
fn claude_content_has_interruption_marker(content: &Value) -> bool {
    super::claude_native_content_has_interruption_marker(content)
}

#[cfg(test)]
#[allow(dead_code)]
fn is_claude_interruption_marker(text: &str) -> bool {
    super::is_claude_interruption_marker(text)
}

#[cfg(test)]
#[allow(dead_code)]
fn parse_blocks(content: &Value) -> Vec<TurnBlock> {
    super::parse_claude_native_blocks(content)
}

#[cfg(test)]
#[allow(dead_code)]
fn parse_block(value: &Value) -> Option<TurnBlock> {
    super::parse_claude_native_block(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AdapterConfigs, ClaudeAdapterConfig, CodexAdapterConfig, GrokAdapterConfig,
        OpencodeAdapterConfig,
    };
    use crate::state::{AgentSendSource, PaneInfo, PaneRuntime, PaneStatus};
    use crate::transcript::TurnBlock;
    use portable_pty::{Child, ChildKiller, ExitStatus, PtySize, native_pty_system};
    use std::io::{self, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    fn svec(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn hook_transcript_path_confines_forged_session_start_paths() {
        let dir = "/home/u/.claude/projects/proj";
        let bound = format!("{dir}/sess-a.jsonl");

        // First discovery (no current path): accept any .jsonl, reject non-.jsonl.
        assert!(hook_transcript_path_acceptable(
            None,
            &format!("{dir}/sess-a.jsonl")
        ));
        assert!(!hook_transcript_path_acceptable(
            None,
            "/home/u/.ssh/id_rsa"
        ));
        assert!(!hook_transcript_path_acceptable(None, "/tmp/evil"));

        // Once bound, a later hook may only rotate to a sibling (compact/resume),
        // never relocate the tail to another directory or an unrelated file.
        assert!(hook_transcript_path_acceptable(
            Some(&bound),
            &format!("{dir}/sess-b.jsonl")
        ));
        assert!(!hook_transcript_path_acceptable(
            Some(&bound),
            "/home/u/.claude/projects/other/sess-x.jsonl"
        ));
        assert!(!hook_transcript_path_acceptable(
            Some(&bound),
            "/tmp/evil.jsonl"
        ));
        assert!(!hook_transcript_path_acceptable(
            Some(&bound),
            &format!("{dir}/id_rsa")
        ));
    }

    #[test]
    fn args_contain_prompt_detects_inline_prompts() {
        // Bare or flags-only launches drop into interactive mode (no prompt).
        assert!(!args_contain_prompt(&[]));
        assert!(!args_contain_prompt(&svec(&["--model", "sonnet"])));
        assert!(!args_contain_prompt(&svec(&["--permission-mode", "plan"])));
        assert!(!args_contain_prompt(&svec(&["--continue"])));
        assert!(!args_contain_prompt(&svec(&["--resume", "abc123"])));
        assert!(!args_contain_prompt(&svec(&["-r"])));
        assert!(!args_contain_prompt(&svec(&["--model=sonnet"])));
        assert!(!args_contain_prompt(&svec(&["--agent", "reviewer"])));
        assert!(!args_contain_prompt(&svec(&["--debug-file", "/tmp/debug"])));
        assert!(!args_contain_prompt(&svec(&["--from-pr", "123"])));
        assert!(!args_contain_prompt(&svec(&["--worktree", "feature"])));
        assert!(!args_contain_prompt(&svec(&[
            "--add-dir",
            "/tmp/a",
            "/tmp/b"
        ])));
        assert!(!args_contain_prompt(&svec(&["doctor"])));
        assert!(!args_contain_prompt(&svec(&["update"])));
        assert!(!args_contain_prompt(&svec(&["upgrade"])));
        assert!(!args_contain_prompt(&svec(&["--verbose", "doctor"])));
        // Plugin flags take a value, so their argument is not a prompt.
        assert!(!args_contain_prompt(&svec(&["--plugin-dir", "/tmp/p"])));
        assert!(!args_contain_prompt(&svec(&[
            "--plugin-url",
            "https://x/p.zip"
        ])));

        // A positional token is an inline prompt, even after value-taking flags.
        assert!(args_contain_prompt(&svec(&["fix the bug"])));
        assert!(args_contain_prompt(&svec(&[
            "--plugin-dir",
            "/tmp/p",
            "fix the bug"
        ])));
        assert!(args_contain_prompt(&svec(&[
            "--model",
            "sonnet",
            "fix the bug"
        ])));
        assert!(args_contain_prompt(&svec(&["--continue", "keep going"])));
        assert!(args_contain_prompt(&svec(&["--print", "summarize this"])));
        assert!(args_contain_prompt(&svec(&["ultrareview", "main"])));
        assert!(args_contain_prompt(&svec(&[
            "--resume",
            "sess-1",
            "keep going"
        ])));
        assert!(args_contain_prompt(&svec(&["--", "after separator"])));
    }

    #[test]
    fn shell_args_reject_modes_that_bypass_qmux_lifecycle_tracking() {
        for args in [
            svec(&["--bare"]),
            svec(&["--safe-mode"]),
            svec(&["--background"]),
            svec(&["--bg"]),
            svec(&["--worktree"]),
            svec(&["-w", "feature"]),
            svec(&["--worktree=feature"]),
            svec(&["--tmux"]),
            svec(&["--tmux=classic"]),
            svec(&["--settings", "/tmp/custom-settings.json"]),
            svec(&["--settings={}"]),
        ] {
            assert!(
                validate_claude_shell_args(&args).is_err(),
                "accepted {args:?}"
            );
        }

        assert!(validate_claude_shell_args(&svec(&["--model", "sonnet"])).is_ok());
        assert!(validate_claude_shell_args(&svec(&["--", "--safe-mode"])).is_ok());
    }

    #[test]
    fn claude_resume_session_id_reads_the_resumed_session() {
        assert_eq!(
            claude_resume_session_id(&svec(&["--resume", "sess-1"])),
            Some("sess-1")
        );
        assert_eq!(
            claude_resume_session_id(&svec(&["-r", "sess-2"])),
            Some("sess-2")
        );
        assert_eq!(
            claude_resume_session_id(&svec(&["--resume=sess-3"])),
            Some("sess-3")
        );
        assert_eq!(
            claude_resume_session_id(&svec(&["--model", "sonnet", "--resume", "sess-4"])),
            Some("sess-4")
        );
        // Not a resume invocation, or no id supplied.
        assert_eq!(claude_resume_session_id(&svec(&[])), None);
        assert_eq!(claude_resume_session_id(&svec(&["--continue"])), None);
        assert_eq!(claude_resume_session_id(&svec(&["--resume"])), None);
        assert_eq!(claude_resume_session_id(&svec(&["--resume="])), None);
        assert_eq!(
            claude_resume_session_id(&svec(&["--resume", "--model", "sonnet"])),
            None
        );
        assert_eq!(
            claude_resume_session_id(&svec(&["--resume", "sess-source", "--fork-session"])),
            None
        );
        assert_eq!(
            claude_resume_session_id(&svec(&["--fork-session", "--resume=sess-source"])),
            None
        );
        assert_eq!(
            claude_resume_session_id(&svec(&["--", "--resume", "prompt-session"])),
            None
        );
        assert_eq!(
            claude_fork_source_session_id(&svec(&["--resume", "sess-source", "--fork-session"])),
            Some("sess-source")
        );
    }

    #[derive(Debug)]
    struct FakeChild;

    impl ChildKiller for FakeChild {
        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(FakeChild)
        }
    }

    impl Child for FakeChild {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            Ok(None)
        }

        fn wait(&mut self) -> io::Result<ExitStatus> {
            Ok(ExitStatus::with_exit_code(0))
        }

        fn process_id(&self) -> Option<u32> {
            None
        }
    }

    struct RecordingWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.bytes.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn test_state() -> AppState {
        AppState::new(QmuxConfig {
            workspace_root: PathBuf::from("/tmp/qmux-hooks-test"),
            socket_path: PathBuf::from("/tmp/qmux-hooks-test.sock"),
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
        })
    }

    fn test_state_with_claude_binary(binary: &Path) -> AppState {
        AppState::new(QmuxConfig {
            workspace_root: unique_test_dir("qmux-claude-workspace"),
            socket_path: unique_test_dir("qmux-claude-socket").join("qmux.sock"),
            adapters: AdapterConfigs {
                claude: ClaudeAdapterConfig {
                    binary: Some(binary.display().to_string()),
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
        })
    }

    #[test]
    fn hook_settings_are_written_under_qmux_workspace_root() {
        let workspace_root = unique_test_dir("qmux-claude-global-hooks");
        let project_dir = unique_test_dir("qmux-claude-project");
        let config = QmuxConfig {
            workspace_root: workspace_root.clone(),
            socket_path: unique_test_dir("qmux-claude-hooks-socket").join("qmux.sock"),
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
        };

        let settings_path = write_hook_settings(&config, "pane-1").unwrap();

        // Per-pane, per-spawn file under a 0700 `.qmux/hooks/` dir, created 0600.
        let hooks_dir = workspace_root.join(".qmux/hooks");
        assert!(
            settings_path.starts_with(&hooks_dir),
            "unexpected hook path: {settings_path:?}"
        );
        let name = settings_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            name.starts_with("pane-1-") && name.ends_with(".json"),
            "unexpected hook filename: {name}"
        );
        assert!(settings_path.is_file());
        assert_eq!(
            fs::metadata(&settings_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!project_dir.join(".qmux/qmux-hooks.json").exists());

        // A second spawn for the same pane prunes the first file so the dir stays
        // bounded to one file per pane, and mints a fresh unguessable name.
        let second_path = write_hook_settings(&config, "pane-1").unwrap();
        assert_ne!(second_path, settings_path);
        assert!(!settings_path.exists());
        assert!(second_path.is_file());

        let raw = fs::read_to_string(&second_path).unwrap();
        assert!(raw.contains("\"hooks\""));
        for event in CLAUDE_HOOK_EVENTS {
            assert!(
                raw.contains(&format!(" notify {event}")),
                "missing hook for {event}"
            );
        }

        let _ = fs::remove_dir_all(workspace_root);
        let _ = fs::remove_dir_all(project_dir);
    }

    fn unique_test_dir(prefix: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }

    fn fake_claude_binary(dir: &Path) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let binary = dir.join("fake-claude");
        fs::write(
            &binary,
            "#!/bin/sh\nprintf 'fake claude ready\\n'\nsleep 1\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&binary).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&binary, permissions).unwrap();
        binary
    }

    fn sample_agent() -> AgentInfo {
        AgentInfo {
            id: "agent-1".to_string(),
            group_id: "group-1".to_string(),
            adapter: "claude".to_string(),
            worktree_dir: "/tmp/qmux-hooks-test".to_string(),
            branch: None,
            pane_id: Some("pane-1".to_string()),
            orphaned_queue_pane_id: None,
            session_id: None,
            transcript_path: None,
            status: AgentStatus::Running,
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

    #[test]
    fn anchored_fork_resumes_the_seed_instead_of_forking_again() {
        let mut source = sample_agent();
        source.session_id = Some("live-session".to_string());
        source.model = Some("opus".to_string());
        let state = test_state();
        let adapter = ClaudeAdapter::new(state.config());

        let anchored = adapter.shell_fork_at_message_args(&source, "seed-session", Some("retry"));
        let head = adapter
            .shell_fork_args(&source, Path::new("/tmp"), Some("retry"))
            .unwrap();

        // The seed transcript already ends at the fork point, so resume it as-is.
        // Forking again would copy it into a second session and strand the seed.
        assert!(anchored.contains(&"--resume".to_string()));
        assert!(anchored.contains(&"seed-session".to_string()));
        assert!(!anchored.contains(&"--fork-session".to_string()));
        assert!(!anchored.contains(&"live-session".to_string()));

        // The head fork keeps its existing shape.
        assert!(head.contains(&"--fork-session".to_string()));
        assert!(head.contains(&"live-session".to_string()));

        // Both delimit the prompt so a leading `-` cannot be read as a flag.
        for args in [&anchored, &head] {
            let delimiter = args.iter().position(|arg| arg == "--").unwrap();
            assert_eq!(args[delimiter + 1], "retry");
        }
        // Model still propagates to the branch.
        assert!(anchored.windows(2).any(|pair| pair == ["--model", "opus"]));
    }

    fn install_agent_pane(state: &AppState) -> Arc<Mutex<Vec<u8>>> {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        drop(pair.slave);

        state.insert_agent(sample_agent()).unwrap();
        state
            .insert_pane(PaneRuntime {
                info: PaneInfo {
                    id: "pane-1".to_string(),
                    title: "Claude".to_string(),
                    last_osc_title: None,
                    kind: PaneKind::Agent,
                    agent_id: Some("agent-1".to_string()),
                    group_id: "group-1".to_string(),
                    cwd: "/tmp/qmux-hooks-test".to_string(),
                    cols: 80,
                    rows: 24,
                    status: PaneStatus::Running,
                    last_active_at: 0,
                    recovered: false,
                    depth: 0,
                },
                backend: crate::state::PaneBackend::HostPty {
                    child: Arc::new(Mutex::new(Box::new(FakeChild))),
                    master: Arc::new(Mutex::new(pair.master)),
                    writer: Arc::new(Mutex::new(Box::new(RecordingWriter {
                        bytes: bytes.clone(),
                    }))),
                    backlog: Default::default(),
                    native_surface: false,
                },
            })
            .unwrap();
        bytes
    }

    fn hook(event: &str, payload: serde_json::Value) -> AdapterNotification {
        AdapterNotification {
            adapter_id: None,
            event: event.to_string(),
            pane_id: Some("pane-1".to_string()),
            agent_id: None,
            payload,
        }
    }

    fn hook_for_agent(
        event: &str,
        agent_id: &str,
        payload: serde_json::Value,
    ) -> AdapterNotification {
        AdapterNotification {
            adapter_id: None,
            event: event.to_string(),
            pane_id: Some("pane-1".to_string()),
            agent_id: Some(agent_id.to_string()),
            payload,
        }
    }

    fn ingest(state: &AppState, notification: AdapterNotification) -> QmuxEvent {
        match ClaudeAdapter::new(state.config()).ingest_notification(state, notification) {
            Ok(AdapterNotificationOutcome::Event(event)) => event,
            Err(err) => panic!("{err}"),
        }
    }

    fn written_text(bytes: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(bytes.lock().unwrap().clone()).unwrap()
    }

    fn claude_system_line(uuid: &str, parent_uuid: Option<&str>) -> String {
        claude_line(
            "system",
            uuid,
            parent_uuid,
            json!({ "role": "system", "content": "" }),
            json!({ "content": "" }),
        )
    }

    fn claude_user_line(uuid: &str, parent_uuid: Option<&str>, text: &str) -> String {
        claude_line(
            "user",
            uuid,
            parent_uuid,
            json!({ "role": "user", "content": text }),
            json!({}),
        )
    }

    fn claude_assistant_line(
        uuid: &str,
        parent_uuid: Option<&str>,
        message_id: &str,
        text: &str,
    ) -> String {
        claude_line(
            "assistant",
            uuid,
            parent_uuid,
            json!({ "id": message_id, "role": "assistant", "content": [{ "type": "text", "text": text }] }),
            json!({}),
        )
    }

    fn claude_tool_use_line(
        uuid: &str,
        parent_uuid: Option<&str>,
        message_id: &str,
        tool_use_id: &str,
    ) -> String {
        claude_line(
            "assistant",
            uuid,
            parent_uuid,
            json!({
                "id": message_id,
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": tool_use_id,
                    "name": "Read",
                    "input": { "file_path": "src/main.rs" }
                }]
            }),
            json!({}),
        )
    }

    fn claude_tool_result_line(
        uuid: &str,
        parent_uuid: Option<&str>,
        source_tool_assistant_uuid: &str,
        tool_use_id: &str,
    ) -> String {
        claude_line(
            "user",
            uuid,
            parent_uuid,
            json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": "ok"
                }]
            }),
            json!({
                "sourceToolAssistantUUID": source_tool_assistant_uuid,
                "toolUseResult": { "type": "text", "content": "ok" }
            }),
        )
    }

    fn claude_last_prompt_line(leaf_uuid: &str) -> String {
        json!({
            "type": "last-prompt",
            "lastPrompt": "prompt",
            "leafUuid": leaf_uuid,
            "sessionId": "session-1"
        })
        .to_string()
    }

    fn claude_line(
        entry_type: &str,
        uuid: &str,
        parent_uuid: Option<&str>,
        message: serde_json::Value,
        extra: serde_json::Value,
    ) -> String {
        let mut value = json!({
            "type": entry_type,
            "uuid": uuid,
            "message": message,
            "sessionId": "session-1"
        });
        if let Some(parent_uuid) = parent_uuid {
            value["parentUuid"] = json!(parent_uuid);
        }
        if let Some(extra) = extra.as_object() {
            for (key, value_to_insert) in extra {
                value[key.as_str()] = value_to_insert.clone();
            }
        }
        value.to_string()
    }

    fn turn_by_native_id<'a>(turns: &'a [Turn], native_id: &str) -> &'a Turn {
        turns
            .iter()
            .find(|turn| turn.native_id.as_deref() == Some(native_id))
            .unwrap_or_else(|| panic!("turn with native id {native_id} not found"))
    }

    #[test]
    fn stop_drains_one_queued_turn() {
        let state = test_state();
        let bytes = install_agent_pane(&state);
        state
            .enqueue_agent_turn("agent-1", "first".to_string())
            .unwrap();
        state
            .enqueue_agent_turn("agent-1", "second".to_string())
            .unwrap();

        let event = ingest(&state, hook("Stop", json!({})));

        assert_eq!(event.event_type, "agent.running");
        assert_eq!(
            state.list_agent_turn_queue("agent-1").unwrap(),
            vec!["second".to_string()]
        );
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(matches!(agent.status, AgentStatus::Running));
        let written = written_text(&bytes);
        assert!(written.contains("first"));
        assert!(!written.contains("second"));
    }

    #[test]
    fn session_start_without_session_id_keeps_a_recorded_one() {
        let state = test_state();
        install_agent_pane(&state);

        // The first SessionStart records the session id.
        ingest(
            &state,
            hook("SessionStart", json!({ "session_id": "sess-abc" })),
        );
        assert_eq!(
            state
                .agent("agent-1")
                .unwrap()
                .unwrap()
                .session_id
                .as_deref(),
            Some("sess-abc")
        );

        // A late/duplicate SessionStart that omits session_id must not blank it
        // (fork + recovery key off the recorded id).
        ingest(&state, hook("SessionStart", json!({})));
        assert_eq!(
            state
                .agent("agent-1")
                .unwrap()
                .unwrap()
                .session_id
                .as_deref(),
            Some("sess-abc")
        );
    }

    #[test]
    fn forked_agent_rejects_session_start_carrying_the_source_session() {
        let state = test_state();
        install_agent_pane(&state);
        state
            .mutate_agent("agent-1", |agent| {
                agent.fork_point = Some("sess-src".to_string());
            })
            .unwrap();

        // Stale fork payload: the source's id and transcript. Neither may bind —
        // adopting them would tail the source's live transcript from this pane.
        ingest(
            &state,
            hook(
                "SessionStart",
                json!({
                    "session_id": "sess-src",
                    "transcript_path": "/home/u/.claude/projects/proj/sess-src.jsonl",
                }),
            ),
        );
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(agent.session_id, None);
        assert_eq!(agent.transcript_path, None);

        // Same rejection when only the transcript betrays the source session.
        ingest(
            &state,
            hook(
                "SessionStart",
                json!({ "transcript_path": "/home/u/.claude/projects/proj/sess-src.jsonl" }),
            ),
        );
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(agent.transcript_path, None);

        // A SessionStart that does carry the fork's own id binds normally.
        ingest(
            &state,
            hook(
                "SessionStart",
                json!({
                    "session_id": "sess-fork",
                    "transcript_path": "/home/u/.claude/projects/proj/sess-fork.jsonl",
                }),
            ),
        );
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(agent.session_id.as_deref(), Some("sess-fork"));
        assert_eq!(
            agent.transcript_path.as_deref(),
            Some("/home/u/.claude/projects/proj/sess-fork.jsonl")
        );
    }

    #[test]
    fn forked_agent_adopts_session_identity_from_first_turn_hook() {
        let state = test_state();
        install_agent_pane(&state);
        state
            .mutate_agent("agent-1", |agent| {
                agent.fork_point = Some("sess-src".to_string());
            })
            .unwrap();

        // The source's own id is never adopted, even from a later hook.
        ingest(
            &state,
            hook(
                "PreToolUse",
                json!({
                    "session_id": "sess-src",
                    "transcript_path": "/home/u/.claude/projects/proj/sess-src.jsonl",
                }),
            ),
        );
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(agent.session_id, None);

        // Adoption is all-or-nothing: an id whose transcript doesn't encode it
        // (mismatched pair) must not bind either half.
        ingest(
            &state,
            hook(
                "PreToolUse",
                json!({
                    "session_id": "sess-fork",
                    "transcript_path": "/home/u/.claude/projects/proj/sess-other.jsonl",
                }),
            ),
        );
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(agent.session_id, None);
        assert_eq!(agent.transcript_path, None);

        // Subagent payloads describe a sidechain, not the pane; skipped.
        ingest(
            &state,
            hook(
                "PreToolUse",
                json!({
                    "agent_id": "task-subagent",
                    "session_id": "sess-side",
                    "transcript_path": "/home/u/.claude/projects/proj/sess-side.jsonl",
                }),
            ),
        );
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(agent.session_id, None);

        // The first consistent (id, transcript) pair from a real hook binds both.
        ingest(
            &state,
            hook(
                "PreToolUse",
                json!({
                    "session_id": "sess-fork",
                    "transcript_path": "/home/u/.claude/projects/proj/sess-fork.jsonl",
                }),
            ),
        );
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(agent.session_id.as_deref(), Some("sess-fork"));
        assert_eq!(
            agent.transcript_path.as_deref(),
            Some("/home/u/.claude/projects/proj/sess-fork.jsonl")
        );

        // One-shot: once a distinct id is bound, later hooks can't move it.
        ingest(
            &state,
            hook(
                "PostToolUse",
                json!({
                    "session_id": "sess-late",
                    "transcript_path": "/home/u/.claude/projects/proj/sess-late.jsonl",
                }),
            ),
        );
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(agent.session_id.as_deref(), Some("sess-fork"));
    }

    #[test]
    fn non_forked_agent_never_adopts_session_identity_from_turn_hooks() {
        let state = test_state();
        install_agent_pane(&state);

        // No fork lineage: only SessionStart may bind, exactly as before.
        ingest(
            &state,
            hook(
                "PreToolUse",
                json!({
                    "session_id": "sess-abc",
                    "transcript_path": "/home/u/.claude/projects/proj/sess-abc.jsonl",
                }),
            ),
        );
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(agent.session_id, None);
        assert_eq!(agent.transcript_path, None);
    }

    #[test]
    fn session_start_preserves_awaiting_input_status() {
        let state = test_state();
        install_agent_pane(&state);
        state
            .set_agent_status("agent-1", AgentStatus::AwaitingInput)
            .unwrap();

        let event = ingest(
            &state,
            hook("SessionStart", json!({ "session_id": "sess-abc" })),
        );

        assert_eq!(event.event_type, "agent.session_start");
        assert_eq!(event.payload["agent"]["status"], json!("awaitingInput"));
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(matches!(agent.status, AgentStatus::AwaitingInput));
        assert_eq!(agent.session_id.as_deref(), Some("sess-abc"));
    }

    #[test]
    fn recovered_claude_resume_starts_idle() {
        let dir = unique_test_dir("qmux-claude-recover");
        let fake_claude = fake_claude_binary(&dir);
        let state = test_state_with_claude_binary(&fake_claude);
        let mut agent = sample_agent();
        agent.worktree_dir = dir.display().to_string();
        agent.session_id = Some("sess-abc".to_string());
        agent.status = AgentStatus::Running;
        state.insert_agent(agent.clone()).unwrap();

        let pane = PaneInfo {
            id: "pane-recovered".to_string(),
            title: "Claude".to_string(),
            last_osc_title: None,
            kind: PaneKind::Agent,
            agent_id: Some(agent.id.clone()),
            group_id: agent.group_id.clone(),
            cwd: dir.display().to_string(),
            cols: 80,
            rows: 24,
            status: PaneStatus::Running,
            last_active_at: 0,
            recovered: true,
            depth: 0,
        };

        ClaudeAdapter::new(state.config())
            .respawn_pane(&state, &pane, &agent)
            .unwrap();

        let restored = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(restored.pane_id.as_deref(), Some("pane-recovered"));
        assert!(matches!(restored.status, AgentStatus::Idle));
    }

    #[test]
    fn stop_marks_agent_done_without_queued_turns() {
        let state = test_state();
        install_agent_pane(&state);

        let event = ingest(&state, hook("Stop", json!({})));

        assert_eq!(event.event_type, "agent.done");
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(matches!(agent.status, AgentStatus::Done));
    }

    #[test]
    fn stop_waits_for_reported_background_tasks_without_subagent_hooks() {
        let state = test_state();
        install_agent_pane(&state);

        let event = ingest(
            &state,
            hook(
                "Stop",
                json!({
                    "background_tasks": [{
                        "id": "task-1",
                        "type": "subagent",
                        "status": "running"
                    }]
                }),
            ),
        );
        assert_eq!(event.event_type, "agent.running");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        ));

        let event = ingest(&state, hook("Stop", json!({ "background_tasks": [] })));
        assert_eq!(event.event_type, "agent.done");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Done
        ));
    }

    #[test]
    fn failure_and_input_hooks_keep_status_current() {
        let state = test_state();
        install_agent_pane(&state);

        state
            .set_agent_status("agent-1", AgentStatus::AwaitingPermission)
            .unwrap();
        let event = ingest(&state, hook("PermissionDenied", json!({})));
        assert_eq!(event.event_type, "agent.permission_denied");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        ));

        let event = ingest(&state, hook("PostToolUseFailure", json!({})));
        assert_eq!(event.event_type, "agent.tool_result");

        let event = ingest(&state, hook("Elicitation", json!({})));
        assert_eq!(event.event_type, "agent.awaiting_input");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::AwaitingInput
        ));

        let event = ingest(&state, hook("ElicitationResult", json!({})));
        assert_eq!(event.event_type, "agent.input_resolved");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        ));

        let event = ingest(&state, hook("StopFailure", json!({})));
        assert_eq!(event.event_type, "agent.done");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Done
        ));
    }

    #[test]
    fn parent_stop_waits_for_subagents_and_a_later_synthesis_stop() {
        let state = test_state();
        install_agent_pane(&state);

        let event = ingest(
            &state,
            hook("SubagentStart", json!({ "agent_id": "child-1" })),
        );
        assert_eq!(event.event_type, "agent.subagent_started");
        assert!(state.agent_has_active_subagents("agent-1").unwrap());

        let event = ingest(
            &state,
            hook(
                "Notification.idle_prompt",
                json!({ "hook_event_name": "Notification" }),
            ),
        );
        assert_eq!(event.event_type, "agent.running");

        let event = ingest(&state, hook("Stop", json!({})));
        assert_eq!(event.event_type, "agent.running");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        ));

        let event = ingest(
            &state,
            hook("SubagentStop", json!({ "agent_id": "child-1" })),
        );
        assert_eq!(event.event_type, "agent.subagent_stopped");
        assert!(!state.agent_has_active_subagents("agent-1").unwrap());
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        ));

        let event = ingest(&state, hook("Stop", json!({})));
        assert_eq!(event.event_type, "agent.done");

        let event = ingest(&state, hook("SessionEnd", json!({})));
        assert_eq!(event.event_type, "agent.session_end");
    }

    #[test]
    fn pause_after_turn_pauses_the_queue_then_unpause_resumes() {
        let state = test_state();
        let bytes = install_agent_pane(&state);
        state
            .enqueue_agent_turn("agent-1", "first".to_string())
            .unwrap();
        state
            .enqueue_agent_turn("agent-1", "second".to_string())
            .unwrap();
        // Pause after the first queued turn.
        state
            .set_queued_turn_pause("agent-1", 0, true, Some("first"), None)
            .unwrap();

        // First idle drains the pause-after turn and the agent runs it.
        let event = ingest(&state, hook("Stop", json!({})));
        assert_eq!(event.event_type, "agent.running");
        assert!(written_text(&bytes).contains("first"));

        // Claude echoes the submitted prompt before this newly sent turn can finish.
        // A completion arriving before that echo is intentionally deduplicated as a
        // second completion for the previous turn.
        ingest(
            &state,
            hook("UserPromptSubmit", json!({ "prompt": "first" })),
        );

        // When that turn finishes, the queue pauses instead of sending "second".
        let event = ingest(&state, hook("Stop", json!({})));
        assert_eq!(event.event_type, "agent.done");
        // The emitted payload must reflect the paused state (not the stale pre-idle
        // snapshot), so the UI surfaces it without a separate refetch.
        assert_eq!(event.payload["agent"]["paused"], json!(true));
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(agent.paused);
        assert_eq!(
            state.list_agent_turn_queue("agent-1").unwrap(),
            vec!["second".to_string()]
        );
        assert!(!written_text(&bytes).contains("second"));

        // Unpausing (agent idle) clears the pause and sends the next turn now.
        let result = crate::turn_queue::unpause_agent(&state, "agent-1").unwrap();
        assert!(result.sent);
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(!agent.paused);
        assert!(state.list_agent_turn_queue("agent-1").unwrap().is_empty());
        assert!(written_text(&bytes).contains("second"));
    }

    #[test]
    fn typing_holds_the_queue_until_typing_stops() {
        let state = test_state();
        let bytes = install_agent_pane(&state);
        state
            .enqueue_agent_turn("agent-1", "queued".to_string())
            .unwrap();

        // While the user is typing, a finishing turn must not drain the queue.
        crate::turn_queue::set_agent_typing(&state, "agent-1", true).unwrap();
        let event = ingest(&state, hook("Stop", json!({})));
        assert_eq!(event.event_type, "agent.done");
        assert_eq!(
            state.list_agent_turn_queue("agent-1").unwrap(),
            vec!["queued".to_string()]
        );
        assert!(!written_text(&bytes).contains("queued"));

        // Once typing stops, releasing the hold drains the held turn (agent is idle).
        let result = crate::turn_queue::set_agent_typing(&state, "agent-1", false).unwrap();
        assert!(result.sent);
        assert!(state.list_agent_turn_queue("agent-1").unwrap().is_empty());
        assert!(written_text(&bytes).contains("queued"));
    }

    #[test]
    fn idle_prompt_drains_one_queued_turn() {
        let state = test_state();
        let bytes = install_agent_pane(&state);
        state
            .enqueue_agent_turn("agent-1", "queued".to_string())
            .unwrap();

        let event = ingest(
            &state,
            hook(
                "Notification.idle_prompt",
                json!({ "hook_event_name": "Notification" }),
            ),
        );

        assert_eq!(event.event_type, "agent.running");
        assert!(state.list_agent_turn_queue("agent-1").unwrap().is_empty());
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(matches!(agent.status, AgentStatus::Running));
        assert!(written_text(&bytes).contains("queued"));
    }

    #[test]
    fn idle_prompt_marks_agent_done_without_queued_turns() {
        let state = test_state();
        install_agent_pane(&state);

        let event = ingest(
            &state,
            hook(
                "Notification.idle_prompt",
                json!({ "hook_event_name": "Notification" }),
            ),
        );

        assert_eq!(event.event_type, "agent.done");
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(matches!(agent.status, AgentStatus::Done));
    }

    // The ~60s idle notification must honor the wait a Stop established for
    // reported background tasks, not settle Done and silently cancel it; a
    // later Stop reporting the registry empty is the terminating signal.
    #[test]
    fn idle_prompt_keeps_waiting_for_reported_background_tasks() {
        let state = test_state();
        install_agent_pane(&state);

        ingest(
            &state,
            hook(
                "Stop",
                json!({
                    "background_tasks": [{
                        "id": "task-1",
                        "type": "local_shell",
                        "status": "running"
                    }]
                }),
            ),
        );
        let event = ingest(
            &state,
            hook(
                "Notification.idle_prompt",
                json!({ "hook_event_name": "Notification" }),
            ),
        );
        assert_eq!(event.event_type, "agent.running");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        ));

        ingest(&state, hook("Stop", json!({ "background_tasks": [] })));
        let event = ingest(
            &state,
            hook(
                "Notification.idle_prompt",
                json!({ "hook_event_name": "Notification" }),
            ),
        );
        assert_eq!(event.event_type, "agent.done");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Done
        ));
    }

    // A registry that retains finished entries must not pin the agent Running
    // with nothing left to wait for.
    #[test]
    fn stop_ignores_background_tasks_that_already_finished() {
        let state = test_state();
        install_agent_pane(&state);

        let event = ingest(
            &state,
            hook(
                "Stop",
                json!({
                    "background_tasks": [
                        { "id": "task-1", "status": "completed" },
                        { "id": "task-2", "status": "failed" }
                    ]
                }),
            ),
        );

        assert_eq!(event.event_type, "agent.done");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Done
        ));
    }

    #[test]
    fn explicit_agent_id_routes_hooks_for_shared_shell_pane() {
        let state = test_state();
        let bytes = install_agent_pane(&state);
        let mut second_agent = sample_agent();
        second_agent.id = "agent-2".to_string();
        second_agent.created_at = 2;
        state.insert_agent(second_agent).unwrap();
        state
            .enqueue_agent_turn("agent-1", "wrong queue".to_string())
            .unwrap();
        state
            .enqueue_agent_turn("agent-2", "right queue".to_string())
            .unwrap();

        let event = ingest(
            &state,
            hook_for_agent(
                "Notification.idle_prompt",
                "agent-2",
                json!({ "hook_event_name": "Notification" }),
            ),
        );

        assert_eq!(event.agent_id.as_deref(), Some("agent-2"));
        assert_eq!(
            state.list_agent_turn_queue("agent-1").unwrap(),
            vec!["wrong queue".to_string()]
        );
        assert!(state.list_agent_turn_queue("agent-2").unwrap().is_empty());
        let written = written_text(&bytes);
        assert!(written.contains("right queue"));
        assert!(!written.contains("wrong queue"));
    }

    #[test]
    fn user_prompt_submit_matches_outstanding_send() {
        let state = test_state();
        install_agent_pane(&state);
        state
            .record_agent_send(
                "agent-1",
                "hello\n\nworld".to_string(),
                AgentSendSource::DirectSend,
            )
            .unwrap();

        let event = ingest(
            &state,
            hook("UserPromptSubmit", json!({ "prompt": "hello world" })),
        );

        assert!(state.outstanding_agent_sends("agent-1").unwrap().is_empty());
        assert_eq!(
            event.payload["sendTracking"],
            json!({
                "status": "matched",
                "source": "directSend",
                "outstandingSends": 0
            })
        );
    }

    #[test]
    fn shell_escape_prompt_submit_preserves_ready_status() {
        let state = test_state();
        install_agent_pane(&state);
        state
            .set_agent_status("agent-1", AgentStatus::Done)
            .unwrap();
        state
            .record_agent_send(
                "agent-1",
                "!git status".to_string(),
                AgentSendSource::DirectSend,
            )
            .unwrap();

        let event = ingest(
            &state,
            hook("UserPromptSubmit", json!({ "prompt": "!git status" })),
        );

        assert_eq!(event.event_type, "agent.prompt_submitted");
        assert_eq!(event.payload["sendTracking"]["status"], "matched");
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(matches!(agent.status, AgentStatus::Done));
    }

    #[test]
    fn user_prompt_submit_mismatch_does_not_block_stop_drain() {
        let state = test_state();
        let bytes = install_agent_pane(&state);
        state
            .record_agent_send(
                "agent-1",
                "expected".to_string(),
                AgentSendSource::DirectSend,
            )
            .unwrap();

        let event = ingest(
            &state,
            hook("UserPromptSubmit", json!({ "prompt": "foreign" })),
        );
        assert_eq!(event.payload["sendTracking"]["status"], "mismatched");

        state
            .enqueue_agent_turn("agent-1", "queued after mismatch".to_string())
            .unwrap();
        ingest(&state, hook("Stop", json!({})));

        assert!(written_text(&bytes).contains("queued after mismatch"));
        let outstanding = state.outstanding_agent_sends("agent-1").unwrap();
        assert_eq!(outstanding.len(), 1);
        assert_eq!(outstanding[0].text, "queued after mismatch");
        assert_eq!(outstanding[0].source, AgentSendSource::QueuedTurn);
    }

    #[test]
    fn subagent_prompt_submit_does_not_touch_parent_tracking() {
        let state = test_state();
        install_agent_pane(&state);
        state
            .record_agent_send("agent-1", "expected".to_string(), AgentSendSource::Steer)
            .unwrap();

        let event = ingest(
            &state,
            hook(
                "UserPromptSubmit",
                json!({ "agent_id": "subagent-1", "prompt": "expected" }),
            ),
        );

        assert!(event.payload.get("sendTracking").is_none());
        assert_eq!(state.outstanding_agent_sends("agent-1").unwrap().len(), 1);
    }

    #[test]
    fn parse_claude_interrupted_lifecycle_events() {
        let plain_interrupt = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{ "type": "text", "text": "[Request interrupted by user]" }]
            }
        })
        .to_string();
        let tool_interrupt = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{ "type": "text", "text": "[Request interrupted by user for tool use]" }]
            },
            "interruptedMessageId": "msg_123"
        })
        .to_string();
        let ordinary_user_message = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{ "type": "text", "text": "please keep going" }]
            }
        })
        .to_string();

        assert_eq!(
            parse_transcript_lifecycle_event(&plain_interrupt),
            Some(TranscriptLifecycleEvent::Interrupted)
        );
        assert_eq!(
            parse_transcript_lifecycle_event(&tool_interrupt),
            Some(TranscriptLifecycleEvent::Interrupted)
        );
        assert_eq!(
            parse_transcript_lifecycle_event(&ordinary_user_message),
            None
        );
    }

    #[test]
    fn resolve_claude_transcript_marks_inactive_prompt_branch_superseded() {
        let lines = vec![
            claude_system_line("root", None),
            claude_user_line("old-user", Some("root"), "typo"),
            claude_assistant_line("old-assistant", Some("old-user"), "msg-old", "old answer"),
            claude_user_line("new-user", Some("root"), "corrected"),
            claude_assistant_line("new-assistant", Some("new-user"), "msg-new", "new answer"),
            claude_last_prompt_line("new-assistant"),
        ];

        let turns = resolve_transcript_turns("agent-1", &lines);

        let old_user = turn_by_native_id(&turns, "old-user");
        let old_assistant = turn_by_native_id(&turns, "old-assistant");
        let new_user = turn_by_native_id(&turns, "new-user");
        assert_eq!(old_user.status, Some(TurnStatus::Superseded));
        assert_eq!(
            old_user.status_reason,
            Some(TurnStatusReason::ClaudePromptBranch)
        );
        assert_eq!(old_assistant.status, Some(TurnStatus::Superseded));
        assert_eq!(new_user.status, None);
    }

    #[test]
    fn bounded_claude_resolution_preserves_absolute_source_indices() {
        let lines = vec![
            claude_system_line("root", None),
            claude_user_line("user-1", Some("root"), "prompt"),
        ];

        let turns = resolve_transcript_turns_from("agent-1", 500, &lines);

        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].source_index, 500);
        assert_eq!(turns[1].source_index, 501);
        assert_eq!(turns[1].id, "agent-1-501");
    }

    #[test]
    fn resolve_claude_transcript_does_not_supersede_parallel_tool_branches() {
        let lines = vec![
            claude_system_line("root", None),
            claude_user_line("user-1", Some("root"), "inspect files"),
            claude_tool_use_line("tool-a", Some("user-1"), "msg-tools", "toolu_a"),
            claude_tool_use_line("tool-b", Some("tool-a"), "msg-tools", "toolu_b"),
            claude_tool_result_line("result-a", Some("tool-a"), "tool-a", "toolu_a"),
            claude_tool_result_line("result-b", Some("tool-b"), "tool-b", "toolu_b"),
            claude_last_prompt_line("result-b"),
        ];

        let turns = resolve_transcript_turns("agent-1", &lines);

        assert!(turns.iter().all(|turn| turn.status.is_none()));
    }

    #[test]
    fn resolve_claude_transcript_marks_ambiguous_prompt_branch_uncertain() {
        let lines = vec![
            claude_system_line("root", None),
            claude_user_line("first-user", Some("root"), "first"),
            claude_user_line("second-user", Some("root"), "second"),
            claude_system_line("unrelated-system", None),
            claude_last_prompt_line("unrelated-system"),
        ];

        let turns = resolve_transcript_turns("agent-1", &lines);

        assert_eq!(
            turn_by_native_id(&turns, "first-user").status,
            Some(TurnStatus::Uncertain)
        );
        assert_eq!(
            turn_by_native_id(&turns, "second-user").status,
            Some(TurnStatus::Uncertain)
        );
    }

    #[test]
    fn resolve_claude_transcript_treats_stale_leaf_branch_as_uncertain() {
        let lines = vec![
            claude_system_line("root", None),
            claude_user_line("old-user", Some("root"), "old"),
            claude_assistant_line("old-assistant", Some("old-user"), "msg-old", "old answer"),
            claude_last_prompt_line("old-assistant"),
            claude_user_line("new-user", Some("root"), "new"),
        ];

        let turns = resolve_transcript_turns("agent-1", &lines);

        assert_eq!(
            turn_by_native_id(&turns, "new-user").status,
            Some(TurnStatus::Uncertain)
        );
        assert_ne!(
            turn_by_native_id(&turns, "new-user").status,
            Some(TurnStatus::Superseded)
        );
    }

    #[test]
    fn resolve_claude_transcript_marks_interrupted_message_id() {
        let lines = vec![
            claude_system_line("root", None),
            claude_user_line("user-1", Some("root"), "go"),
            claude_assistant_line("assistant-1", Some("user-1"), "msg_123", "partial"),
            json!({
                "type": "user",
                "uuid": "interrupt-1",
                "parentUuid": "assistant-1",
                "message": {
                    "role": "user",
                    "content": [{ "type": "text", "text": "[Request interrupted by user]" }]
                },
                "interruptedMessageId": "msg_123"
            })
            .to_string(),
            claude_last_prompt_line("interrupt-1"),
        ];

        let turns = resolve_transcript_turns("agent-1", &lines);

        assert_eq!(
            turn_by_native_id(&turns, "assistant-1").status,
            Some(TurnStatus::Interrupted)
        );
        assert_eq!(
            turn_by_native_id(&turns, "interrupt-1").status,
            Some(TurnStatus::Interrupted)
        );
    }

    #[test]
    fn parse_tool_blocks_preserves_correlation_ids() {
        let tool_use = json!({
            "type": "tool_use",
            "id": "toolu_1",
            "name": "Bash",
            "input": { "cmd": "pwd" }
        });

        match parse_block(&tool_use).expect("tool_use should parse") {
            TurnBlock::ToolUse { id, name, input } => {
                assert_eq!(id.as_deref(), Some("toolu_1"));
                assert_eq!(name, "Bash");
                assert_eq!(input["cmd"], "pwd");
            }
            other => panic!("expected tool use, got {other:?}"),
        }

        let tool_result = json!({
            "type": "tool_result",
            "tool_use_id": "toolu_1",
            "content": "ok",
            "is_error": true
        });

        match parse_block(&tool_result).expect("tool_result should parse") {
            TurnBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id.as_deref(), Some("toolu_1"));
                assert_eq!(content, "ok");
                assert!(is_error);
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn parse_tool_result_accepts_camel_case_id() {
        let tool_result = json!({
            "type": "tool_result",
            "toolUseId": "toolu_2",
            "content": "ok"
        });

        match parse_block(&tool_result).expect("tool_result should parse") {
            TurnBlock::ToolResult { tool_use_id, .. } => {
                assert_eq!(tool_use_id.as_deref(), Some("toolu_2"));
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn launch_options_reject_unknown_fields() {
        let err = ClaudeLaunchOptions::from_value(json!({ "bogus": true })).unwrap_err();

        assert!(err.contains("invalid Claude adapter options"));
    }

    #[test]
    fn launch_options_validate_permission_mode() {
        for mode in CLAUDE_PERMISSION_MODES {
            let options = ClaudeLaunchOptions::from_value(json!({ "permissionMode": mode }))
                .expect("current Claude permission mode should be accepted");
            assert_eq!(options.permission_mode.as_deref(), Some(*mode));
        }

        let err =
            ClaudeLaunchOptions::from_value(json!({ "permissionMode": "always" })).unwrap_err();

        assert!(err.contains("invalid Claude adapter option permissionMode"));
        assert!(ClaudeLaunchOptions::from_value(json!({ "permissionMode": "default" })).is_err());
    }

    #[test]
    fn launch_options_validate_effort() {
        for level in CLAUDE_EFFORT_LEVELS {
            let options = ClaudeLaunchOptions::from_value(json!({ "effort": level }))
                .expect("current Claude effort level should be accepted");
            assert_eq!(options.effort.as_deref(), Some(*level));
        }

        let blank = ClaudeLaunchOptions::from_value(json!({ "effort": "  " })).unwrap();
        assert_eq!(blank.effort, None);

        let err = ClaudeLaunchOptions::from_value(json!({ "effort": "extreme" })).unwrap_err();
        assert!(err.contains("invalid Claude adapter option effort"));
    }

    #[test]
    fn shell_session_args_reapply_recorded_effort() {
        assert_eq!(
            shell_session_args("sess-1", Some("opus"), Some("xhigh"), None, false),
            svec(&[
                "--model",
                "opus",
                "--effort",
                "xhigh",
                "--permission-mode",
                "auto",
                "--resume",
                "sess-1",
            ])
        );

        // No recorded effort: the flag is omitted so the CLI default applies.
        assert_eq!(
            shell_session_args("sess-1", None, None, Some("go on"), true),
            svec(&[
                "--permission-mode",
                "auto",
                "--resume",
                "sess-1",
                "--fork-session",
                "--",
                "go on",
            ])
        );
    }

    #[test]
    fn humanize_skill_slug_sentence_cases_the_label() {
        assert_eq!(humanize_skill_slug("deep-research"), "Deep research");
        assert_eq!(humanize_skill_slug("hello_stub"), "Hello stub");
        assert_eq!(humanize_skill_slug("single"), "Single");
        assert_eq!(humanize_skill_slug("a--b"), "A b");
    }

    #[test]
    fn skill_frontmatter_name_reads_declared_name_or_none() {
        let dir = env::temp_dir().join(format!("qmux-skill-fm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");

        fs::write(
            &path,
            "---\nname: deep-research\ndescription: x\n---\n# Body\n",
        )
        .unwrap();
        assert_eq!(
            skill_frontmatter_name(&path).as_deref(),
            Some("deep-research")
        );

        // Quoted values are unwrapped.
        fs::write(&path, "---\nname: \"Quoted Name\"\n---\n").unwrap();
        assert_eq!(
            skill_frontmatter_name(&path).as_deref(),
            Some("Quoted Name")
        );

        // No frontmatter fence -> no name.
        fs::write(&path, "# No frontmatter\nname: ignored\n").unwrap();
        assert_eq!(skill_frontmatter_name(&path), None);

        // A nested `name:` under another mapping is not the skill name; the
        // top-level key wins.
        fs::write(
            &path,
            "---\nmetadata:\n  name: nested\nname: top-level\n---\n",
        )
        .unwrap();
        assert_eq!(skill_frontmatter_name(&path).as_deref(), Some("top-level"));

        // Inline `#` comments on an unquoted value are stripped.
        fs::write(&path, "---\nname: deep-research # rename later\n---\n").unwrap();
        assert_eq!(
            skill_frontmatter_name(&path).as_deref(),
            Some("deep-research")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn skill_shows_in_launcher_requires_explicit_opt_in() {
        let dir = env::temp_dir().join(format!("qmux-skill-launch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");

        // Opted in.
        fs::write(&path, "---\nname: x\nqmux-launcher: true\n---\n").unwrap();
        assert!(skill_shows_in_launcher(&path));

        // Case-insensitive, with an inline comment stripped.
        fs::write(&path, "---\nqmux-launcher: TRUE # opt in\n---\n").unwrap();
        assert!(skill_shows_in_launcher(&path));

        // Absent key -> inline-only by default.
        fs::write(&path, "---\nname: x\ndescription: d\n---\n").unwrap();
        assert!(!skill_shows_in_launcher(&path));

        // Explicit false stays hidden.
        fs::write(&path, "---\nqmux-launcher: false\n---\n").unwrap();
        assert!(!skill_shows_in_launcher(&path));

        // A nested key under another mapping does not opt the skill in.
        fs::write(&path, "---\nmetadata:\n  qmux-launcher: true\n---\n").unwrap();
        assert!(!skill_shows_in_launcher(&path));

        // No frontmatter fence -> hidden.
        fs::write(&path, "# No frontmatter\nqmux-launcher: true\n").unwrap();
        assert!(!skill_shows_in_launcher(&path));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn plugin_namespace_falls_back_to_dir_name_without_manifest() {
        let plugin_dir = env::temp_dir().join(format!("qmux-ns-{}", std::process::id()));
        let _ = fs::remove_dir_all(&plugin_dir);
        fs::create_dir_all(&plugin_dir).unwrap();

        // No manifest -> directory name (what Claude itself would use), not "qmux".
        assert_eq!(
            plugin_namespace(&plugin_dir),
            plugin_dir.file_name().unwrap().to_string_lossy()
        );

        // A manifest name takes precedence.
        let manifest_dir = plugin_dir.join(".claude-plugin");
        fs::create_dir_all(&manifest_dir).unwrap();
        fs::write(manifest_dir.join("plugin.json"), r#"{"name":"qmux"}"#).unwrap();
        assert_eq!(plugin_namespace(&plugin_dir), "qmux");

        let _ = fs::remove_dir_all(&plugin_dir);
    }

    #[test]
    fn list_skills_enumerates_named_namespaced_skills() {
        use crate::config::{
            AdapterConfigs, ClaudeAdapterConfig, CodexAdapterConfig, GrokAdapterConfig,
            OpencodeAdapterConfig,
        };

        let plugin_dir =
            env::temp_dir().join(format!("qmux-claude-plugin-list-{}", std::process::id()));
        let _ = fs::remove_dir_all(&plugin_dir);
        let manifest_dir = plugin_dir.join(".claude-plugin");
        fs::create_dir_all(&manifest_dir).unwrap();
        fs::write(manifest_dir.join("plugin.json"), r#"{"name":"qmux"}"#).unwrap();

        let skill_dir = plugin_dir.join("skills").join("deep-research");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: deep-research\ndescription: d\nqmux-launcher: true\n---\n",
        )
        .unwrap();
        // A subdirectory without SKILL.md is not a skill.
        fs::create_dir_all(plugin_dir.join("skills").join("scratch")).unwrap();
        // An inline-only skill (no `qmux-launcher: true`) is excluded from the launcher.
        let inline_dir = plugin_dir.join("skills").join("fork");
        fs::create_dir_all(&inline_dir).unwrap();
        fs::write(
            inline_dir.join("SKILL.md"),
            "---\nname: fork\ndescription: d\n---\n",
        )
        .unwrap();

        let config = QmuxConfig {
            workspace_root: env::temp_dir(),
            socket_path: env::temp_dir().join("qmux-list.sock"),
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
            claude_plugin_dir: plugin_dir.clone(),
            opencode_plugin_dir: PathBuf::new(),
        };

        let skills = list_skills(&config);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].id, "deep-research");
        assert_eq!(skills[0].name, "Deep research");
        assert_eq!(skills[0].command, "/qmux:deep-research");

        let _ = fs::remove_dir_all(&plugin_dir);
    }

    #[test]
    fn list_skills_is_empty_without_a_plugin_dir() {
        use crate::config::{
            AdapterConfigs, ClaudeAdapterConfig, CodexAdapterConfig, GrokAdapterConfig,
            OpencodeAdapterConfig,
        };

        let config = QmuxConfig {
            workspace_root: env::temp_dir(),
            socket_path: env::temp_dir().join("qmux-empty.sock"),
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
            claude_plugin_dir: env::temp_dir().join("qmux-nonexistent-claude-plugin-dir"),
            opencode_plugin_dir: PathBuf::new(),
        };

        assert!(list_skills(&config).is_empty());
    }

    #[test]
    fn list_skills_keeps_ids_unique_when_frontmatter_names_collide() {
        use crate::config::{
            AdapterConfigs, ClaudeAdapterConfig, CodexAdapterConfig, GrokAdapterConfig,
            OpencodeAdapterConfig,
        };

        let plugin_dir =
            env::temp_dir().join(format!("qmux-claude-plugin-dup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&plugin_dir);
        let manifest_dir = plugin_dir.join(".claude-plugin");
        fs::create_dir_all(&manifest_dir).unwrap();
        fs::write(manifest_dir.join("plugin.json"), r#"{"name":"qmux"}"#).unwrap();

        // Two distinct skill directories that declare the same frontmatter name.
        for dir in ["alpha", "beta"] {
            let skill_dir = plugin_dir.join("skills").join(dir);
            fs::create_dir_all(&skill_dir).unwrap();
            fs::write(
                skill_dir.join("SKILL.md"),
                "---\nname: shared\ndescription: d\nqmux-launcher: true\n---\n",
            )
            .unwrap();
        }

        let config = QmuxConfig {
            workspace_root: env::temp_dir(),
            socket_path: env::temp_dir().join("qmux-dup.sock"),
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
            claude_plugin_dir: plugin_dir.clone(),
            opencode_plugin_dir: PathBuf::new(),
        };

        let skills = list_skills(&config);
        // Ids are the (unique) directory names, even though both share a command.
        // Sort for an order-independent assertion (read_dir order is OS-dependent).
        let mut ids: Vec<&str> = skills.iter().map(|skill| skill.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["alpha", "beta"]);
        assert!(skills.iter().all(|skill| skill.command == "/qmux:shared"));

        let _ = fs::remove_dir_all(&plugin_dir);
    }

    /// A linear two-exchange transcript: user A -> assistant A -> user B ->
    /// assistant B, matching the record shape Claude writes.
    fn linear_fork_transcript(session: &str) -> String {
        [
            json!({"type": "user", "uuid": "u1", "parentUuid": null, "sessionId": session,
                   "message": {"role": "user", "content": "first"}}),
            json!({"type": "assistant", "uuid": "a1", "parentUuid": "u1", "sessionId": session,
                   "message": {"role": "assistant", "content": "ok one"}}),
            json!({"type": "user", "uuid": "u2", "parentUuid": "a1", "sessionId": session,
                   "message": {"role": "user", "content": "second"}}),
            json!({"type": "assistant", "uuid": "a2", "parentUuid": "u2", "sessionId": session,
                   "message": {"role": "assistant", "content": "ok two"}}),
        ]
        .iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n"
    }

    fn anchor_at(native_id: &str, parent: Option<&str>, source_index: usize) -> MessageAnchor {
        MessageAnchor {
            native_id: Some(native_id.to_string()),
            parent_native_id: parent.map(str::to_string),
            source_index,
        }
    }

    fn read_uuids(path: &Path) -> Vec<String> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["uuid"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn synthesize_claude_keeps_ancestors_and_rewrites_session_id() {
        let dir = unique_test_dir("qmux-claude-fork");
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("11111111-1111-4111-8111-111111111111.jsonl");
        fs::write(&source, linear_fork_transcript("original-session")).unwrap();

        // Fork from user message B: keep everything through its parent (a1).
        let result =
            synthesize_truncated_claude_session(&source, &anchor_at("u2", Some("a1"), 2)).unwrap();

        // Claude derives the session id from the filename, so the seed must
        // land at <project>/<id>.jsonl for `--resume <id>` to find it.
        let seed = dir.join(format!("{result}.jsonl"));
        assert_eq!(read_uuids(&seed), vec!["u1", "a1"]);
        // Every copied record is re-stamped with the new session id, or the
        // resumed session would report the one it branched from.
        for line in fs::read_to_string(&seed).unwrap().lines() {
            let value: Value = serde_json::from_str(line).unwrap();
            assert_eq!(value["sessionId"].as_str(), Some(result.as_str()));
        }
        // The source is never modified.
        assert!(
            fs::read_to_string(&source)
                .unwrap()
                .contains("original-session")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn synthesize_claude_drops_sibling_branches() {
        let dir = unique_test_dir("qmux-claude-fork-branch");
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("22222222-2222-4222-8222-222222222222.jsonl");
        // a1 has two children: the abandoned rewind branch `x1` and `u2`.
        let mut body = linear_fork_transcript("original-session");
        body.push_str(
            &json!({"type": "user", "uuid": "x1", "parentUuid": "a1", "sessionId": "original-session",
                    "message": {"role": "user", "content": "abandoned"}})
            .to_string(),
        );
        body.push('\n');
        fs::write(&source, body).unwrap();

        let result =
            synthesize_truncated_claude_session(&source, &anchor_at("u2", Some("a1"), 2)).unwrap();

        // Only the anchor's own ancestry survives; the sibling branch does not.
        assert_eq!(
            read_uuids(&dir.join(format!("{result}.jsonl"))),
            vec!["u1", "a1"]
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn synthesize_claude_tolerates_a_torn_trailing_record() {
        let dir = unique_test_dir("qmux-claude-fork-torn");
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("33333333-3333-4333-8333-333333333333.jsonl");
        // Forking from a live session races the CLI's own append.
        let mut body = linear_fork_transcript("original-session");
        body.push_str("{\"type\":\"assistant\",\"uuid\":\"a3\",\"parentUu");
        fs::write(&source, body).unwrap();

        let result =
            synthesize_truncated_claude_session(&source, &anchor_at("u2", Some("a1"), 2)).unwrap();

        assert_eq!(
            read_uuids(&dir.join(format!("{result}.jsonl"))),
            vec!["u1", "a1"]
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn synthesize_claude_refuses_the_first_message_and_unknown_anchors() {
        let dir = unique_test_dir("qmux-claude-fork-reject");
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("44444444-4444-4444-8444-444444444444.jsonl");
        fs::write(&source, linear_fork_transcript("original-session")).unwrap();

        // The first user message has no parent: truncating leaves nothing.
        let empty = synthesize_truncated_claude_session(&source, &anchor_at("u1", None, 0));
        assert_eq!(empty.unwrap_err(), FORK_AT_MESSAGE_EMPTY_ERROR);

        // An anchor from some other transcript must not silently produce an
        // empty fork — it means the caller and the file disagree.
        let unknown =
            synthesize_truncated_claude_session(&source, &anchor_at("u2", Some("nope"), 2));
        assert!(unknown.unwrap_err().contains("not present in"));

        // A stale anchor — the message still exists but has been relinked to a
        // different parent — must fail rather than fork from the wrong point.
        let stale = synthesize_truncated_claude_session(&source, &anchor_at("u2", Some("u1"), 2));
        assert!(stale.unwrap_err().contains("no longer follows"));

        // Nothing was written for either rejection.
        let strays = fs::read_dir(&dir)
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().path() != source)
            .count();
        assert_eq!(strays, 0);

        let _ = fs::remove_dir_all(&dir);
    }
}
