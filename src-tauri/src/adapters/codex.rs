use super::{
    AdapterNotification, AdapterNotificationOutcome, AgentAdapter, ComposerPolicy,
    FORK_AT_MESSAGE_EMPTY_ERROR, LaunchEnv, MessageAnchor, PrepareShellAgentLaunchRequest,
    PreparedShellAgentLaunch, ShellCommandIntegration, SpawnAgentRequest, TranscriptLifecycleEvent,
    WorkspaceObservation, apply_shell_cli_model, ensure_on_path, hook_transcript_path_acceptable,
    model_from_codex_transcript_line, new_uuid_v4, parse_transcript_records, prepared_shell_agent,
    record_shell_fork_lineage, record_shell_session_lineage, reusable_session_agent,
    shell_cli_model, shell_quote_arg, shell_quote_path,
};
use crate::config::QmuxConfig;
use crate::events::QmuxEvent;
use crate::host::{self, Host};
use crate::pty::{
    CommandPlan, InitialPaneSize, PaneMeta, agent_pane_envs, plan_to_spec, recoverable_dir,
    spawn_pty,
};
use crate::state::{AppState, PaneInfo, PaneKind};
use crate::transcript::{
    Turn, TurnBlock, TurnContextStatus, TurnStatus, TurnStatusReason, codex_transcript_session_id,
    gather_transcript_candidates_recursive, read_codex_transcript_session_id,
    start_transcript_tail, string_field,
};
use crate::turn_queue::is_shell_escape_turn;
use crate::workspace::{
    ActiveWorkspaceSource, AgentInfo, AgentStatus, PrepareAgentWorkspaceRequest, attach_agent_pane,
    configured_worktree_root_for_cwd, mark_agent_failed, mark_agent_spawn_failed,
    prepare_agent_workspace, prepare_agent_workspace_with_parent,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

const CODEX_QMUX_PROFILE: &str = "qmux-codex";
const CODEX_CODE_MODE_HOST: &str = "codex-code-mode-host";
const CODEX_QMUX_WORKTREE_INSTRUCTIONS: &str = "Qmux worktree policy: When you directly create a Git worktree for the user, ensure the directory in the QMUX_WORKTREE_ROOT environment variable exists and use it as the worktree's parent directory. Do not use QMUX_WORKSPACE_ROOT as the worktree parent. Treat QMUX_WORKTREE_ROOT as an opaque filesystem path.";
const CODEX_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PermissionRequest",
    "PreToolUse",
    "PostToolUse",
    "PreCompact",
    "PostCompact",
    "SubagentStart",
    "SubagentStop",
    "Stop",
];

#[derive(Clone, Debug)]
pub struct CodexAdapter {
    binary: String,
}

impl CodexAdapter {
    pub fn new(config: &QmuxConfig) -> Self {
        Self {
            binary: config.codex_binary(),
        }
    }

    pub(crate) fn ensure_binary(&self) -> Result<String, String> {
        let binary = ensure_on_path(&self.binary).ok_or_else(|| {
            format!(
                "Codex adapter binary '{}' was not found on PATH or standard macOS tool paths. Install Codex CLI or update adapters.codex.binary in qmux.config.json.",
                self.binary
            )
        })?;
        let binary = codex_binary_with_code_mode_host(binary);
        Ok(binary.display().to_string())
    }

    fn host_for_group(&self, state: &AppState, group_id: &str) -> Result<Host, String> {
        let group = state.group(group_id)?;
        Ok(host::for_group(
            group.as_ref().and_then(|group| group.remote.as_ref()),
        ))
    }

    fn binary_for_host(&self, host: &Host) -> Result<String, String> {
        if host.is_local() {
            self.ensure_binary()
        } else if self.binary.trim().is_empty() {
            Err("Codex adapter binary cannot be empty for a remote launch".to_string())
        } else {
            // The configured program is resolved by tmux on the SSH host. A
            // local PATH check would reject a valid remote install or turn a
            // local macOS path into the remote command by accident.
            Ok(self.binary.clone())
        }
    }

    fn cwd_for_host(
        &self,
        host: &Host,
        path: &str,
        missing_message: impl FnOnce() -> String,
    ) -> Result<PathBuf, String> {
        if host.is_local() {
            recoverable_dir(path).ok_or_else(missing_message)
        } else {
            Ok(PathBuf::from(path))
        }
    }

    fn integration_for_host(
        &self,
        host: &Host,
    ) -> Result<(Option<PathBuf>, Option<String>), String> {
        if host.is_local() {
            Ok((Some(ensure_codex_integration()?), None))
        } else {
            let qmux_cli = host
                .remote()
                .map(|remote| remote.qmux_cli.clone())
                .ok_or_else(|| "remote Codex launch lost its host configuration".to_string())?;
            Ok((None, Some(qmux_cli)))
        }
    }
}

fn codex_binary_with_code_mode_host(binary: PathBuf) -> PathBuf {
    if codex_code_mode_host_is_sibling(&binary) {
        return binary;
    }

    let Ok(target) = fs::canonicalize(&binary) else {
        return binary;
    };

    if target != binary && codex_code_mode_host_is_sibling(&target) {
        return target;
    }

    binary
}

fn codex_code_mode_host_is_sibling(binary: &Path) -> bool {
    binary
        .parent()
        .is_some_and(|dir| dir.join(CODEX_CODE_MODE_HOST).is_file())
}

impl AgentAdapter for CodexAdapter {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn display_name(&self) -> &'static str {
        "Codex"
    }

    fn configured_binary(&self) -> &str {
        &self.binary
    }

    fn supports_remote(&self) -> bool {
        true
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
            command_name: "codex",
            adapter_id: self.id(),
        }]
    }

    fn shell_resume_command(&self, session_id: &str) -> Option<String> {
        Some(format!("codex resume {}", shell_quote_arg(session_id)))
    }

    fn ingest_notification(
        &self,
        state: &AppState,
        notification: AdapterNotification,
    ) -> Result<AdapterNotificationOutcome, String> {
        self.ingest_codex_notification(state, notification)
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
        model_from_codex_transcript_line(line)
    }

    fn transcript_workspace_observation(&self, line: &str) -> Option<WorkspaceObservation> {
        codex_workspace_observation(line)
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
        is_codex_status_event(line)
    }

    fn synthesize_truncated_session(
        &self,
        transcript_path: &Path,
        anchor: &MessageAnchor,
        _target_cwd: &Path,
    ) -> Result<String, String> {
        synthesize_truncated_codex_session(transcript_path, anchor)
    }

    fn supports_fork(&self) -> bool {
        true
    }

    fn supports_research(&self) -> bool {
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
        CodexAdapter::shell_fork_args(self, source, cwd, prompt)
    }

    fn shell_fork_at_message_args(
        &self,
        source: &AgentInfo,
        seed_session_id: &str,
        prompt: Option<&str>,
    ) -> Result<Vec<String>, String> {
        Ok(CodexAdapter::shell_fork_at_message_args(
            self,
            source,
            seed_session_id,
            prompt,
        ))
    }

    fn fork_pane(
        &self,
        state: &AppState,
        source: &AgentInfo,
        use_worktree: bool,
        prompt: Option<&str>,
    ) -> Result<(PaneInfo, AgentInfo), String> {
        CodexAdapter::fork_pane(self, state, source, use_worktree, prompt)
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

impl CodexAdapter {
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
                "this Codex session isn't ready to fork yet (no session id); send a turn first"
                    .to_string()
            })?;
        Ok(shell_session_args(
            session_id,
            source.model.as_deref(),
            prompt,
            true,
        ))
    }

    /// Args for a fork anchored at a message. The session id is a rollout
    /// synthesized by `synthesize_truncated_session`, which already ends where
    /// the branch begins — so this resumes it directly rather than running
    /// `codex fork`, which would copy the seed into a second session and leave
    /// the seed behind as a duplicate in the picker.
    pub fn shell_fork_at_message_args(
        &self,
        source: &AgentInfo,
        session_id: &str,
        prompt: Option<&str>,
    ) -> Vec<String> {
        shell_session_args(session_id, source.model.as_deref(), prompt, false)
    }

    fn spawn_pane(&self, state: &AppState, request: SpawnAgentRequest) -> Result<PaneInfo, String> {
        let options = CodexLaunchOptions::from_value(request.options)?;
        let resume_session_id = request
            .resume_session_id
            .as_deref()
            .map(str::trim)
            .filter(|session_id| !session_id.is_empty())
            .map(ToString::to_string);
        if request.fork_session && resume_session_id.is_none() {
            return Err("a Codex history fork requires a session id".to_string());
        }
        let lineage_cwd = request
            .cwd
            .as_deref()
            .or(request.base_repo.as_deref())
            .map(str::to_string);

        let mut agent = prepare_agent_workspace_with_parent(
            state,
            PrepareAgentWorkspaceRequest {
                group_id: request.group_id,
                base_repo: request.base_repo,
                base_ref: request.base_ref,
                adapter: self.id().to_string(),
                model: request.model.clone(),
                effort: options.reasoning_effort.clone(),
                use_worktree: request.use_worktree.unwrap_or(false),
            },
            request.parent_id.as_deref(),
        )?;
        if let Some(session_id) = resume_session_id.as_ref() {
            if request.fork_session {
                let lineage_cwd = lineage_cwd
                    .as_deref()
                    .unwrap_or(&agent.worktree_dir)
                    .to_string();
                agent = record_shell_fork_lineage(
                    state,
                    agent,
                    self.id(),
                    Some(session_id),
                    &lineage_cwd,
                )?;
            } else {
                agent.session_id = Some(session_id.clone());
            }
            agent.status = AgentStatus::Idle;
            state.update_agent(agent.clone())?;
        }
        let cwd = request
            .cwd
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&agent.worktree_dir));
        let host = self.host_for_group(state, &agent.group_id)?;
        let binary = self.binary_for_host(&host)?;
        let (codex_home, remote_hook_cli) = self.integration_for_host(&host)?;
        if host.is_local() && !cwd.is_dir() {
            let _ = mark_agent_failed(state, &agent.id);
            return Err(format!(
                "Codex working directory {} does not exist",
                cwd.display()
            ));
        }

        let has_initial_prompt = prompt_has_initial_text(&request.prompt);
        let tail_args = if let Some(session_id) = resume_session_id {
            let mut args = vec![
                if request.fork_session {
                    "fork"
                } else {
                    "resume"
                }
                .to_string(),
                session_id,
            ];
            if has_initial_prompt {
                args.push("--".to_string());
                args.push(request.prompt.trim().to_string());
            }
            args
        } else {
            prompt_tail_args(&request.prompt)
        };
        let worktree_root = codex_worktree_root(state, &agent, &cwd)?;
        let args = build_codex_args(
            &cwd,
            host.is_local()
                .then_some(state.config().workspace_root.as_path()),
            request.model.as_deref(),
            &options,
            worktree_root
                .as_deref()
                .map(|_| CODEX_QMUX_WORKTREE_INSTRUCTIONS),
            remote_hook_cli.as_deref(),
            tail_args,
        );
        let pane_id = state.next_id("pane");
        let mut envs = agent_pane_envs(state, &pane_id, &agent.id)?;
        if let Some(codex_home) = codex_home {
            envs.push(("CODEX_HOME".to_string(), codex_home.display().to_string()));
        }
        add_codex_worktree_root_env(&mut envs, worktree_root.as_deref());

        // Bind before spawn so a fast SessionStart hook can authenticate against the
        // pane/agent scope and record the native session identity. The spawn-failure
        // path clears this reserved binding.
        attach_codex_agent_pane(state, &agent.id, pane_id.clone(), has_initial_prompt)?;

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
        let host = self.host_for_group(state, &agent.group_id)?;
        // Older qmux versions could persist a hook-reported side-conversation id
        // beside the original rollout path. The rollout is the durable authority,
        // so repair that hybrid before choosing which Codex session to resume. A
        // remote rollout path cannot be opened through the local filesystem.
        let agent = if host.is_local() {
            reconcile_codex_agent_identity(state, agent)?
        } else {
            agent.clone()
        };
        let binary = self.binary_for_host(&host)?;
        let (codex_home, remote_hook_cli) = self.integration_for_host(&host)?;
        let cwd = self.cwd_for_host(&host, &agent.worktree_dir, || {
            format!(
                "agent worktree {} no longer exists; relaunch manually",
                agent.worktree_dir
            )
        })?;
        let options = CodexLaunchOptions {
            reasoning_effort: agent.effort.clone(),
            ..CodexLaunchOptions::default()
        };
        let worktree_root = codex_worktree_root(state, &agent, &cwd)?;
        let (args, resumed) = build_codex_resume_args(
            &cwd,
            host.is_local()
                .then_some(state.config().workspace_root.as_path()),
            agent.model.as_deref(),
            &options,
            worktree_root
                .as_deref()
                .map(|_| CODEX_QMUX_WORKTREE_INSTRUCTIONS),
            remote_hook_cli.as_deref(),
            agent.session_id.as_deref(),
        );

        let mut envs = agent_pane_envs(state, &pane.id, &agent.id)?;
        if let Some(codex_home) = codex_home {
            envs.push(("CODEX_HOME".to_string(), codex_home.display().to_string()));
        }
        add_codex_worktree_root_env(&mut envs, worktree_root.as_deref());

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

        // A recovered Codex process is launched without an inline prompt, even when
        // resuming a session, so it is ready once the TUI appears. Mark it Idle (not
        // Running) so a recovered quiet session isn't shown as working; the first real
        // prompt/tool hook promotes it to Running.
        let restored = attach_codex_agent_pane(state, &agent.id, pane.id.clone(), false)?;
        if host.is_local()
            && let Some(transcript_path) = restored.transcript_path.clone()
        {
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

    /// Forks `source` into a new Codex agent pane using `codex fork <session> [prompt]`.
    /// Codex records a fresh session id for the fork, so the source session keeps
    /// running independently.
    pub fn fork_pane(
        &self,
        state: &AppState,
        source: &AgentInfo,
        use_worktree: bool,
        prompt: Option<&str>,
    ) -> Result<(PaneInfo, AgentInfo), String> {
        let session_id = source
            .session_id
            .clone()
            .map(|session| session.trim().to_string())
            .filter(|session| !session.is_empty())
            .ok_or_else(|| {
                "this Codex session isn't ready to fork yet (no session id); send a turn first"
                    .to_string()
            })?;

        let mut agent = prepare_agent_workspace_with_parent(
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
            Some(&source.id),
        )?;
        agent.fork_point = Some(session_id.clone());
        agent.root_session_id = source
            .root_session_id
            .clone()
            .or_else(|| Some(session_id.clone()));
        agent.status = AgentStatus::Idle;
        state.update_agent(agent.clone())?;

        let host = self.host_for_group(state, &agent.group_id)?;
        let binary = self.binary_for_host(&host)?;
        let (codex_home, remote_hook_cli) = self.integration_for_host(&host)?;
        let cwd = self.cwd_for_host(&host, &agent.worktree_dir, || {
            format!(
                "fork working directory {} does not exist",
                agent.worktree_dir
            )
        })?;
        let options = CodexLaunchOptions {
            reasoning_effort: agent.effort.clone(),
            ..CodexLaunchOptions::default()
        };
        let prompt = prompt.map(str::trim).unwrap_or_default();
        let has_initial_prompt = !prompt.is_empty();
        let worktree_root = codex_worktree_root(state, &agent, &cwd)?;
        let args = build_codex_fork_args(
            &cwd,
            host.is_local()
                .then_some(state.config().workspace_root.as_path()),
            agent.model.as_deref(),
            &options,
            worktree_root
                .as_deref()
                .map(|_| CODEX_QMUX_WORKTREE_INSTRUCTIONS),
            remote_hook_cli.as_deref(),
            &session_id,
            if has_initial_prompt {
                Some(prompt)
            } else {
                None
            },
        );

        let pane_id = state.next_id("pane");
        let mut envs = agent_pane_envs(state, &pane_id, &agent.id)?;
        if let Some(codex_home) = codex_home {
            envs.push(("CODEX_HOME".to_string(), codex_home.display().to_string()));
        }
        add_codex_worktree_root_env(&mut envs, worktree_root.as_deref());

        // Bind before spawn so a fast Codex SessionStart hook passes the control
        // socket's agent/pane scope check. mark_agent_spawn_failed clears this
        // reserved binding if the process fails to launch.
        attach_codex_agent_pane(state, &agent.id, pane_id.clone(), has_initial_prompt)?;

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
            Ok(pane) => {
                let forked = state.agent(&agent.id)?.unwrap_or_else(|| agent.clone());
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
        validate_shell_tail_args(&request.args)?;

        if !state.pane_exists(&request.pane_id)? {
            return Err(format!("pane {} was not found", request.pane_id));
        }

        let pane_group_id = state
            .pane_group_id(&request.pane_id)?
            .ok_or_else(|| format!("pane {} was not found", request.pane_id))?;
        let host = self.host_for_group(state, &pane_group_id)?;
        let binary = self.binary_for_host(&host)?;
        let (codex_home, remote_hook_cli) = self.integration_for_host(&host)?;

        let shell_cwd = PathBuf::from(&request.cwd);
        if host.is_local() && !shell_cwd.is_dir() {
            return Err(format!(
                "Codex working directory {} does not exist",
                shell_cwd.display()
            ));
        }
        let agent_cwd = codex_effective_cwd(&host, &shell_cwd, &request.args)?;

        // A restart-driven resume (`codex resume <id>`) rebinds the original agent for
        // that session instead of minting a duplicate; any other invocation starts a
        // fresh agent in the current directory.
        let cwd_str = agent_cwd.display().to_string();
        let resume_session_id = codex_resume_session_id(&request.args).map(str::to_string);
        let fork_point = codex_fork_source_session_id(&request.args).map(str::to_string);
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
                        // Typing `codex` in a shell runs in the current directory; no worktree.
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
        let agent = attach_codex_agent_pane(
            state,
            &agent.id,
            request.pane_id.clone(),
            args_contain_prompt(&request.args),
        )?;
        let agent = if host.is_local() {
            agent
        } else {
            state
                .mutate_agent(&agent.id, |agent| agent.transcript_path = None)?
                .ok_or_else(|| "prepared remote Codex agent disappeared".to_string())?
        };

        let options = CodexLaunchOptions::default();
        let worktree_root = codex_worktree_root(state, &agent, &agent_cwd)?;
        let args = build_codex_args(
            &shell_cwd,
            host.is_local()
                .then_some(state.config().workspace_root.as_path()),
            None,
            &options,
            worktree_root
                .as_deref()
                .map(|_| CODEX_QMUX_WORKTREE_INSTRUCTIONS),
            remote_hook_cli.as_deref(),
            request.args,
        );
        let mut envs = agent_pane_envs(state, &request.pane_id, &agent.id)?;
        if let Some(codex_home) = codex_home {
            envs.push(("CODEX_HOME".to_string(), codex_home.display().to_string()));
        }
        add_codex_worktree_root_env(&mut envs, worktree_root.as_deref());
        let launch_envs = if host.is_local() {
            envs
        } else {
            let identity = state
                .list_panes()?
                .into_iter()
                .find(|pane| pane.id == request.pane_id)
                .and_then(|pane| pane.remote_session)
                .ok_or_else(|| {
                    format!(
                        "remote pane {} is missing its tmux session identity",
                        request.pane_id
                    )
                })?;
            host.tmux_pane_envs(
                &identity,
                &state.pane_remote_token(&request.pane_id)?,
                &envs,
            )?
        };
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
            envs: launch_envs
                .into_iter()
                .map(|(key, value)| LaunchEnv { key, value })
                .collect(),
            supervised: true,
        })
    }

    fn ingest_codex_notification(
        &self,
        state: &AppState,
        notification: AdapterNotification,
    ) -> Result<AdapterNotificationOutcome, String> {
        let pane_id = notification.pane_id.clone();
        let remote_pane = pane_id.as_deref().is_some_and(|pane_id| {
            state.list_panes().ok().is_some_and(|panes| {
                panes
                    .iter()
                    .any(|pane| pane.id == pane_id && pane.remote_session.is_some())
            })
        });
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
        if !remote_pane && let Some(current) = agent.take() {
            agent = Some(reconcile_codex_agent_identity(state, &current)?);
        }
        let hook_event = notification.event.clone();
        if hook_event != "SessionStart"
            && let Some(current) = agent.as_ref()
        {
            if remote_pane {
                adopt_remote_forked_codex_session_identity(state, current, &notification.payload)?;
            } else {
                adopt_forked_codex_session_identity(state, current, &notification.payload)?;
            }
        }
        let event_type = match hook_event.as_str() {
            "SessionStart" => {
                if let Some(current) = agent.as_ref() {
                    let session_id = string_field(&notification.payload, "session_id")
                        .or_else(|| string_field(&notification.payload, "sessionId"))
                        .or_else(|| string_field(&notification.payload, "resource_id"))
                        .or_else(|| string_field(&notification.payload, "resourceId"));
                    if remote_pane {
                        if let Some(session_id) = session_id
                            .as_deref()
                            .and_then(valid_remote_codex_session_id)
                            && current.fork_point.as_deref() != Some(session_id.as_str())
                        {
                            record_remote_codex_session_id(state, current, session_id)?;
                        }
                    } else {
                        let transcript_path =
                            string_field(&notification.payload, "transcript_path")
                                .or_else(|| string_field(&notification.payload, "transcriptPath"))
                                // This payload arrives over the control socket under the pane's
                                // token, so a prompt-injected agent can forge a SessionStart.
                                // Reject a path that isn't a sibling of the already-bound
                                // transcript (or isn't a .jsonl) before tailing it; a rejected
                                // path falls back to session-id directory discovery, which is
                                // confined to $CODEX_HOME/sessions and matched on session_meta id.
                                .filter(|candidate| {
                                    hook_transcript_path_acceptable(
                                        current.transcript_path.as_deref(),
                                        candidate,
                                    )
                                });
                        let stale_fork_payload =
                            current.fork_point.as_deref().is_some_and(|fork_point| {
                                session_id.as_deref() == Some(fork_point)
                                    || transcript_path.as_deref().is_some_and(|path| {
                                        codex_transcript_session_id(Path::new(path)).as_deref()
                                            == Some(fork_point)
                                    })
                            });
                        if !stale_fork_payload {
                            // A SessionStart can describe an ephemeral TUI fork routed
                            // through the same pane token. Keep its identity provisional;
                            // the binding worker promotes both fields together only after
                            // a rollout's session_meta proves the reported id.
                            start_codex_transcript_binding(
                                state.clone(),
                                current.id.clone(),
                                session_id,
                                transcript_path,
                            );
                        }
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
                    // Codex accepts prompts while Running, so this may be a
                    // steer rather than a clean turn boundary. Preserve known
                    // children until their lifecycle stop arrives.
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
            "Stop" => {
                // Codex fires Stop between auto-review/guardian jobs and when
                // the TUI queues another prompt, then continues the same turn.
                // Waiters and this agent's own queue settle on transcript
                // `task_complete` instead (see parse_transcript_lifecycle_event).
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
                if waiting_on_subagents {
                    "agent.running"
                } else if remote_pane {
                    // Local panes settle from the authoritative task_complete
                    // transcript record. Remote transcript streaming is not in
                    // the first terminal increment, so Stop is the best
                    // available completion boundary there; any later tool or
                    // prompt hook promotes the agent back to Running.
                    if let Some(agent) = agent.as_mut() {
                        agent.status = AgentStatus::AwaitingInput;
                        state.set_agent_status(&agent.id, agent.status)?;
                    }
                    "agent.awaiting_input"
                } else {
                    "agent.stop_observed"
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
        // Status/paused writes go straight to the store without touching this
        // local snapshot, so re-read the agent before attaching it — otherwise
        // the event ships a stale copy and the surgical upsert below hides the
        // change from the UI.
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

/// Reasoning levels accepted by the Codex CLI's `model_reasoning_effort`
/// config across supported models.
const CODEX_REASONING_EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CodexLaunchOptions {
    #[serde(default)]
    sandbox: Option<String>,
    #[serde(default)]
    approval_policy: Option<String>,
    #[serde(default)]
    approvals_reviewer: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    // Kept only so saved launcher options that still carry `search: true` parse
    // cleanly under `deny_unknown_fields`; --search is now always emitted.
    #[serde(default)]
    #[allow(dead_code)]
    search: bool,
}

impl CodexLaunchOptions {
    fn from_value(value: Value) -> Result<Self, String> {
        if value.is_null() {
            return Ok(Self::default());
        }

        let mut options: CodexLaunchOptions = serde_json::from_value(value)
            .map_err(|err| format!("invalid Codex adapter options: {err}"))?;
        options.sandbox = normalize_option(
            "sandbox",
            options.sandbox.as_deref(),
            &["read-only", "workspace-write", "danger-full-access"],
        )?;
        options.approval_policy = normalize_option(
            "approvalPolicy",
            options.approval_policy.as_deref(),
            &["untrusted", "on-request", "never"],
        )?;
        options.approvals_reviewer = normalize_option(
            "approvalsReviewer",
            options.approvals_reviewer.as_deref(),
            &["auto_review"],
        )?;
        // Union across supported models: GPT-5.6 (Sol, Terra, Luna) accepts the
        // full range while GPT-5.4 tops out at xhigh; the CLI rejects a level
        // the selected model does not support.
        options.reasoning_effort = normalize_option(
            "reasoningEffort",
            options.reasoning_effort.as_deref(),
            CODEX_REASONING_EFFORT_LEVELS,
        )?;
        Ok(options)
    }
}

fn normalize_option(
    field: &str,
    value: Option<&str>,
    allowed: &[&str],
) -> Result<Option<String>, String> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if allowed.contains(&value) {
        Ok(Some(value.to_string()))
    } else {
        Err(format!(
            "invalid Codex adapter option {field}='{value}'; expected one of {}",
            allowed.join(", ")
        ))
    }
}

fn build_codex_args(
    cwd: &Path,
    additional_workspace_root: Option<&Path>,
    model: Option<&str>,
    options: &CodexLaunchOptions,
    developer_instructions: Option<&str>,
    remote_hook_cli: Option<&str>,
    tail_args: Vec<String>,
) -> Vec<String> {
    let mut args = vec!["--cd".to_string(), cwd.display().to_string()];
    if let Some(additional_workspace_root) = additional_workspace_root {
        args.push("--add-dir".to_string());
        args.push(additional_workspace_root.display().to_string());
    }

    if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    push_codex_hook_integration(&mut args, remote_hook_cli);
    if let Some(developer_instructions) = developer_instructions {
        args.push("--config".to_string());
        args.push(format!(
            "developer_instructions={}",
            toml_string(developer_instructions)
        ));
    }
    let sandbox = options.sandbox.as_deref().unwrap_or("workspace-write");
    args.push("--sandbox".to_string());
    args.push(sandbox.to_string());
    if let Some(approval_policy) = options.approval_policy.as_deref()
        && options.approvals_reviewer.as_deref() != Some("auto_review")
    {
        args.push("--ask-for-approval".to_string());
        args.push(approval_policy.to_string());
    }
    if let Some(approvals_reviewer) = options.approvals_reviewer.as_deref() {
        args.push("--config".to_string());
        args.push(format!(
            "approvals_reviewer={}",
            toml_string(approvals_reviewer)
        ));
    }
    if let Some(reasoning_effort) = options.reasoning_effort.as_deref() {
        args.push("--config".to_string());
        args.push(format!(
            "model_reasoning_effort={}",
            toml_string(reasoning_effort)
        ));
    }
    args.push("--search".to_string());

    args.extend(tail_args);
    args
}

fn build_codex_resume_args(
    cwd: &Path,
    additional_workspace_root: Option<&Path>,
    model: Option<&str>,
    options: &CodexLaunchOptions,
    developer_instructions: Option<&str>,
    remote_hook_cli: Option<&str>,
    session_id: Option<&str>,
) -> (Vec<String>, bool) {
    let Some(session_id) = session_id
        .map(str::trim)
        .filter(|session_id| !session_id.is_empty())
    else {
        return (
            build_codex_args(
                cwd,
                additional_workspace_root,
                model,
                options,
                developer_instructions,
                remote_hook_cli,
                Vec::new(),
            ),
            false,
        );
    };

    (
        build_codex_args(
            cwd,
            additional_workspace_root,
            model,
            options,
            developer_instructions,
            remote_hook_cli,
            vec!["resume".to_string(), session_id.to_string()],
        ),
        true,
    )
}

fn build_codex_fork_args(
    cwd: &Path,
    additional_workspace_root: Option<&Path>,
    model: Option<&str>,
    options: &CodexLaunchOptions,
    developer_instructions: Option<&str>,
    remote_hook_cli: Option<&str>,
    session_id: &str,
    prompt: Option<&str>,
) -> Vec<String> {
    let mut tail_args = vec!["fork".to_string(), session_id.trim().to_string()];
    if let Some(prompt) = prompt.map(str::trim).filter(|prompt| !prompt.is_empty()) {
        // Delimit the prompt with `--` so a fork prompt that happens to start with
        // `-` (e.g. a forged `agent.fork` payload of "--dangerously-bypass-...") is
        // parsed as the positional prompt, not as a Codex flag that could weaken the
        // sandbox/approval posture. Mirrors the initial-launch path (`prompt_tail_args`).
        tail_args.push("--".to_string());
        tail_args.push(prompt.to_string());
    }
    build_codex_args(
        cwd,
        additional_workspace_root,
        model,
        options,
        developer_instructions,
        remote_hook_cli,
        tail_args,
    )
}

/// Local Codex panes use qmux's generated profile in the user's CODEX_HOME.
/// A remote pane must not point CODEX_HOME at a pane support directory: doing
/// so would hide the remote user's auth, config, and session history. Instead,
/// inject only the qmux lifecycle hooks as process-local config overrides and
/// leave the remote Codex home untouched.
fn push_codex_hook_integration(args: &mut Vec<String>, remote_hook_cli: Option<&str>) {
    let Some(qmux_cli) = remote_hook_cli else {
        args.push("--profile".to_string());
        args.push(CODEX_QMUX_PROFILE.to_string());
        return;
    };

    args.push("--config".to_string());
    args.push("features.hooks=true".to_string());
    let command_prefix = shell_quote_arg(qmux_cli);
    for event in CODEX_HOOK_EVENTS {
        let command = toml_string(&format!("{command_prefix} notify {event}"));
        let entry = if *event == "SessionStart" {
            format!(
                "hooks.{event}=[{{matcher=\"startup|resume\",hooks=[{{type=\"command\",command={command},timeout=5}}]}}]"
            )
        } else {
            format!("hooks.{event}=[{{hooks=[{{type=\"command\",command={command},timeout=5}}]}}]")
        };
        args.push("--config".to_string());
        args.push(entry);
    }
}

fn codex_worktree_root(
    state: &AppState,
    agent: &AgentInfo,
    cwd: &Path,
) -> Result<Option<String>, String> {
    let group = state
        .group(&agent.group_id)?
        .ok_or_else(|| format!("group {} was not found", agent.group_id))?;
    let host = host::for_group(group.remote.as_ref());
    let cwd = cwd
        .to_str()
        .ok_or_else(|| "Codex working directory is not valid UTF-8".to_string())?;
    configured_worktree_root_for_cwd(state, &host, cwd, &group)
        .map(|root| root.map(|root| root.display().to_string()))
}

fn add_codex_worktree_root_env(envs: &mut Vec<(String, String)>, root: Option<&str>) {
    if let Some(root) = root {
        envs.push(("QMUX_WORKTREE_ROOT".to_string(), root.to_string()));
    }
}

fn prompt_tail_args(prompt: &str) -> Vec<String> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        Vec::new()
    } else {
        vec!["--".to_string(), prompt.to_string()]
    }
}

fn prompt_has_initial_text(prompt: &str) -> bool {
    !prompt.trim().is_empty()
}

fn attach_codex_agent_pane(
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

/// The project Codex will actually operate on for a shell invocation. Keep this
/// separate from the process cwd so the intercepted shell command retains normal
/// relative-path behavior while qMux identity and resume matching follow `--cd`.
fn codex_effective_cwd(host: &Host, shell_cwd: &Path, args: &[String]) -> Result<PathBuf, String> {
    let mut requested = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            break;
        }
        if arg == "--cd" || arg == "-C" {
            let value = args
                .get(index + 1)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("Codex {arg} requires a directory"))?;
            requested = Some(PathBuf::from(value));
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--cd=") {
            if value.is_empty() {
                return Err("Codex --cd requires a directory".to_string());
            }
            requested = Some(PathBuf::from(value));
        } else if let Some(value) = arg.strip_prefix("-C")
            && !value.is_empty()
        {
            requested = Some(PathBuf::from(value));
        }
        index += 1;
    }

    let cwd = match requested {
        Some(path) if path.is_absolute() => path,
        Some(path) => shell_cwd.join(path),
        None => shell_cwd.to_path_buf(),
    };
    if host.is_local() && !cwd.is_dir() {
        return Err(format!(
            "Codex working directory {} does not exist",
            cwd.display()
        ));
    }
    if host.is_local() {
        Ok(fs::canonicalize(&cwd).unwrap_or(cwd))
    } else {
        // This path belongs to the SSH host. It is already passed to Codex and
        // tmux as an opaque remote path, so never canonicalize or stat it on
        // the machine running the qmux UI.
        Ok(cwd)
    }
}

/// Whether a manual `codex ...` invocation carries an inline prompt. Value-taking
/// flags are skipped so `codex --model gpt-5` is treated as interactive.
fn args_contain_prompt(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return index + 1 < args.len();
        }
        if codex_variadic_value_flag(arg) {
            index += 1;
            while index < args.len() && !args[index].starts_with('-') {
                index += 1;
            }
            continue;
        }
        if codex_value_flag(arg) {
            index += 2;
            continue;
        }
        if codex_inline_value_flag(arg) || arg.starts_with('-') {
            index += 1;
            continue;
        }

        return match arg.as_str() {
            // These interactive subcommands take an optional session selector and
            // then an optional prompt. Parse both positions rather than treating the
            // command or session id itself as prompt text.
            "resume" | "fork" => codex_session_command_has_prompt(args, index + 1),
            // Non-interactive agent runs are working even when their instructions
            // come from stdin or review-selection flags instead of a prompt token.
            "exec" | "e" | "review" => true,
            command if codex_utility_command(command) => false,
            // The first positional token of the base interactive CLI is its prompt.
            _ => true,
        };
    }
    false
}

fn codex_session_command_has_prompt(args: &[String], mut index: usize) -> bool {
    let mut session_seen = false;
    let mut use_last = false;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--last" {
            use_last = true;
            index += 1;
            continue;
        }
        if arg == "--" {
            let remaining = args.len().saturating_sub(index + 1);
            return if use_last || session_seen {
                remaining >= 1
            } else {
                remaining >= 2
            };
        }
        if codex_variadic_value_flag(arg) {
            index += 1;
            while index < args.len() && !args[index].starts_with('-') {
                index += 1;
            }
            continue;
        }
        if codex_value_flag(arg) {
            index += 2;
            continue;
        }
        if codex_inline_value_flag(arg) || arg.starts_with('-') {
            index += 1;
            continue;
        }
        if use_last || session_seen {
            return true;
        }
        session_seen = true;
        index += 1;
    }
    false
}

fn codex_utility_command(command: &str) -> bool {
    matches!(
        command,
        "a" | "app"
            | "app-server"
            | "apply"
            | "archive"
            | "cloud"
            | "completion"
            | "debug"
            | "delete"
            | "doctor"
            | "exec-server"
            | "features"
            | "help"
            | "login"
            | "logout"
            | "mcp"
            | "mcp-server"
            | "plugin"
            | "remote-control"
            | "sandbox"
            | "unarchive"
            | "update"
    )
}

/// Extracts the session id from a `codex resume <id>` shell argument list, so a resume
/// launch can rebind the original agent. `None` when the invocation isn't a `resume` of
/// a specific session (e.g. `codex resume --last`).
fn codex_resume_session_id(args: &[String]) -> Option<&str> {
    codex_session_command_id(args, "resume")
}

fn codex_fork_source_session_id(args: &[String]) -> Option<&str> {
    codex_session_command_id(args, "fork")
}

fn codex_session_command_id<'a>(args: &'a [String], expected_command: &str) -> Option<&'a str> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            break;
        }
        if codex_variadic_value_flag(arg) {
            index += 1;
            while index < args.len() && !args[index].starts_with('-') {
                index += 1;
            }
            continue;
        }
        if codex_value_flag(arg) {
            index += 2;
            continue;
        }
        if codex_inline_value_flag(arg) || arg.starts_with('-') {
            index += 1;
            continue;
        }
        // The first positional token is either the interactive prompt or a
        // subcommand. Only the requested session command can identify its native
        // source; never scan through another command's arguments.
        return (arg == expected_command)
            .then(|| codex_resume_command_session_id(args, index + 1))
            .flatten();
    }
    None
}

fn codex_resume_command_session_id(args: &[String], mut index: usize) -> Option<&str> {
    while index < args.len() {
        let arg = &args[index];
        if arg == "--last" {
            return None;
        }
        if arg == "--" {
            return args.get(index + 1).map(String::as_str);
        }
        if codex_variadic_value_flag(arg) {
            index += 1;
            while index < args.len() && !args[index].starts_with('-') {
                index += 1;
            }
            continue;
        }
        if codex_value_flag(arg) {
            index += 2;
            continue;
        }
        if codex_inline_value_flag(arg) || arg.starts_with('-') {
            index += 1;
            continue;
        }
        return Some(arg);
    }
    None
}

fn codex_value_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--cd"
            | "-C"
            | "--add-dir"
            | "--model"
            | "-m"
            | "--sandbox"
            | "-s"
            | "--ask-for-approval"
            | "-a"
            | "--config"
            | "-c"
            | "--enable"
            | "--disable"
            | "--remote"
            | "--remote-auth-token-env"
            | "--local-provider"
            | "--profile"
            | "-p"
    )
}

fn codex_variadic_value_flag(arg: &str) -> bool {
    matches!(arg, "--image" | "-i")
}

fn codex_inline_value_flag(arg: &str) -> bool {
    [
        "--cd=",
        "--add-dir=",
        "--model=",
        "--sandbox=",
        "--ask-for-approval=",
        "--config=",
        "--enable=",
        "--disable=",
        "--remote=",
        "--remote-auth-token-env=",
        "--local-provider=",
        "--profile=",
    ]
    .iter()
    .any(|prefix| arg.starts_with(prefix))
        || (arg.starts_with("-C") && arg.len() > 2)
        || (arg.starts_with("-m") && arg.len() > 2)
        || (arg.starts_with("-c") && arg.len() > 2)
        || (arg.starts_with("-s") && arg.len() > 2)
        || (arg.starts_with("-a") && arg.len() > 2)
        || (arg.starts_with("-p") && arg.len() > 2)
}

fn ensure_codex_integration() -> Result<PathBuf, String> {
    let codex_home = codex_home()?;
    let qmux_cli = crate::launch_path::qmux_cli_path()
        .map_err(|err| format!("{err} (needed for Codex hooks)"))?;
    write_codex_integration_files(&codex_home, &qmux_cli)?;
    Ok(codex_home)
}

fn codex_home() -> Result<PathBuf, String> {
    env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .ok_or_else(|| "CODEX_HOME and HOME are not set; cannot configure Codex hooks".to_string())
}

fn write_codex_integration_files(codex_home: &Path, qmux_cli: &Path) -> Result<(), String> {
    let qmux_dir = codex_home.join("qmux");
    fs::create_dir_all(&qmux_dir)
        .map_err(|err| format!("failed to create {}: {err}", qmux_dir.display()))?;

    let shim_path = qmux_dir.join("qmux-codex-hook");
    let shim = codex_hook_shim();
    fs::write(&shim_path, shim)
        .map_err(|err| format!("failed to write {}: {err}", shim_path.display()))?;
    fs::set_permissions(&shim_path, fs::Permissions::from_mode(0o755))
        .map_err(|err| format!("failed to chmod {}: {err}", shim_path.display()))?;

    let profile_path = codex_home.join(format!("{CODEX_QMUX_PROFILE}.config.toml"));
    let existing_profile = fs::read_to_string(&profile_path).ok();
    let profile = codex_profile_toml(&shim_path, qmux_cli, existing_profile.as_deref());
    fs::write(&profile_path, profile)
        .map_err(|err| format!("failed to write {}: {err}", profile_path.display()))?;

    Ok(())
}

fn codex_hook_shim() -> &'static str {
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

fn codex_profile_toml(shim_path: &Path, qmux_cli: &Path, existing_profile: Option<&str>) -> String {
    let command_prefix = shell_quote_path(shim_path);
    let mut raw = String::new();
    raw.push_str("# Generated by qMux. Do not edit.\n");
    raw.push_str(
        "# This profile enables qMux Codex lifecycle hooks only for qMux-launched panes.\n",
    );
    raw.push_str(&format!("# qMux executable: {}\n\n", qmux_cli.display()));
    raw.push_str("[features]\n");
    raw.push_str("hooks = true\n\n");

    for event in CODEX_HOOK_EVENTS {
        if *event == "SessionStart" {
            raw.push_str("[[hooks.SessionStart]]\n");
            raw.push_str("matcher = \"startup|resume\"\n");
        } else {
            raw.push_str(&format!("[[hooks.{event}]]\n"));
        }
        raw.push_str(&format!("[[hooks.{event}.hooks]]\n"));
        raw.push_str("type = \"command\"\n");
        raw.push_str(&format!(
            "command = {}\n",
            toml_string(&format!("{command_prefix} {event}"))
        ));
        raw.push_str("timeout = 5\n\n");
    }

    if let Some(state) = existing_profile.and_then(codex_hooks_state_toml) {
        raw.push('\n');
        raw.push_str(state.trim_start_matches('\n'));
        if !raw.ends_with('\n') {
            raw.push('\n');
        }
    }

    raw
}

fn codex_hooks_state_toml(raw: &str) -> Option<&str> {
    let mut offset = 0;
    for line in raw.split_inclusive('\n') {
        if line.trim() == "[hooks.state]" {
            return Some(&raw[offset..]);
        }
        offset += line.len();
    }
    None
}

fn toml_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            other => escaped.push(other),
        }
    }
    escaped.push('"');
    escaped
}

fn validate_shell_tail_args(args: &[String]) -> Result<(), String> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            break;
        }
        if arg == "--oss" {
            return Err("qMux Codex integration does not support --oss".to_string());
        }
        if arg == "--remote" || arg.starts_with("--remote=") {
            return Err(
                "qMux Codex integration does not support --remote because lifecycle hooks and transcripts must run locally"
                    .to_string(),
            );
        }
        if arg == "--profile" || arg == "-p" || arg.starts_with("--profile=") {
            return Err(
                "qMux Codex integration uses its own profile and does not support --profile"
                    .to_string(),
            );
        }
        if arg.starts_with("-p") && arg.len() > 2 {
            return Err(
                "qMux Codex integration uses its own profile and does not support -p".to_string(),
            );
        }
        if arg == "--disable" && args.get(index + 1).is_some_and(|value| value == "hooks")
            || arg == "--disable=hooks"
        {
            return Err(
                "qMux Codex integration does not support disabling hooks because lifecycle tracking requires them"
                    .to_string(),
            );
        }
        let config_override = if arg == "--config" || arg == "-c" {
            args.get(index + 1).map(String::as_str)
        } else {
            arg.strip_prefix("--config=")
                .or_else(|| arg.strip_prefix("-c").filter(|value| !value.is_empty()))
        };
        if config_override.is_some_and(codex_config_overrides_hooks) {
            return Err(
                "qMux Codex integration does not support overriding hook configuration because lifecycle tracking requires the qMux hooks"
                    .to_string(),
            );
        }
        index += 1;
    }
    Ok(())
}

fn codex_config_overrides_hooks(value: &str) -> bool {
    let key = value.split_once('=').map_or(value, |(key, _)| key).trim();
    key == "hooks" || key.starts_with("hooks.") || key == "features.hooks"
}

/// Makes the durable rollout authoritative over a previously persisted hook id.
/// This repairs hybrid identities written by qmux versions that committed
/// SessionStart before validating its transcript.
fn reconcile_codex_agent_identity(
    state: &AppState,
    current: &AgentInfo,
) -> Result<AgentInfo, String> {
    let Some(transcript_path) = current.transcript_path.as_deref() else {
        return Ok(current.clone());
    };
    let Some(transcript_session_id) = codex_transcript_session_id(Path::new(transcript_path))
    else {
        return Ok(current.clone());
    };
    if current.session_id.as_deref() == Some(transcript_session_id.as_str()) {
        return Ok(current.clone());
    }

    state
        .mutate_agent(&current.id, |agent| {
            if agent.transcript_path.as_deref() == Some(transcript_path) {
                agent.session_id = Some(transcript_session_id.clone());
            }
        })?
        .ok_or_else(|| {
            format!(
                "agent {} disappeared while reconciling Codex transcript identity",
                current.id
            )
        })
}

fn valid_remote_codex_session_id(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')))
    .then(|| value.to_string())
}

/// Remote hooks cannot hand their rollout to the local transcript validator.
/// Keep only the bounded opaque session id needed for tmux recovery/resume and
/// deliberately do not persist or tail the remote filesystem path. The hook is
/// still scoped by the pane's forwarded capability token.
fn record_remote_codex_session_id(
    state: &AppState,
    current: &AgentInfo,
    session_id: String,
) -> Result<(), String> {
    state
        .mutate_agent(&current.id, |agent| {
            if agent.pane_id == current.pane_id {
                agent.session_id = Some(session_id.clone());
            }
        })?
        .ok_or_else(|| {
            format!(
                "agent {} disappeared while recording its remote Codex session",
                current.id
            )
        })?;
    Ok(())
}

fn adopt_remote_forked_codex_session_identity(
    state: &AppState,
    current: &AgentInfo,
    payload: &Value,
) -> Result<(), String> {
    let Some(fork_point) = current.fork_point.as_deref() else {
        return Ok(());
    };
    if current
        .session_id
        .as_deref()
        .is_some_and(|session_id| session_id != fork_point)
    {
        return Ok(());
    }
    let child_session_id = string_field(payload, "session_id")
        .or_else(|| string_field(payload, "sessionId"))
        .or_else(|| string_field(payload, "resource_id"))
        .or_else(|| string_field(payload, "resourceId"))
        .as_deref()
        .and_then(valid_remote_codex_session_id)
        .filter(|session_id| session_id != fork_point);
    if let Some(session_id) = child_session_id {
        record_remote_codex_session_id(state, current, session_id)?;
    }
    Ok(())
}

/// Recovers a fork's child identity when its startup hook briefly reported the
/// source session. Later lifecycle hooks carry the child session metadata, so the
/// first child candidate can be validated against its rollout.
fn adopt_forked_codex_session_identity(
    state: &AppState,
    current: &AgentInfo,
    payload: &Value,
) -> Result<(), String> {
    let Some(fork_point) = current.fork_point.as_deref() else {
        return Ok(());
    };
    if current
        .session_id
        .as_deref()
        .is_some_and(|session_id| session_id != fork_point)
    {
        return Ok(());
    }

    let transcript_path = string_field(payload, "transcript_path")
        .or_else(|| string_field(payload, "transcriptPath"))
        .filter(|candidate| {
            hook_transcript_path_acceptable(current.transcript_path.as_deref(), candidate)
        });
    let child_session_id = string_field(payload, "session_id")
        .or_else(|| string_field(payload, "sessionId"))
        .or_else(|| string_field(payload, "resource_id"))
        .or_else(|| string_field(payload, "resourceId"))
        .or_else(|| {
            transcript_path
                .as_deref()
                .and_then(|path| codex_transcript_session_id(Path::new(path)))
        })
        .filter(|session_id| session_id != fork_point);
    let Some(child_session_id) = child_session_id else {
        return Ok(());
    };

    start_codex_transcript_binding(
        state.clone(),
        current.id.clone(),
        Some(child_session_id),
        transcript_path,
    );
    Ok(())
}

const CODEX_TRANSCRIPT_DISCOVERY_ATTEMPTS: usize = 40;
const CODEX_TRANSCRIPT_DISCOVERY_DELAY: Duration = Duration::from_millis(250);

fn start_codex_transcript_binding(
    state: AppState,
    agent_id: String,
    session_id: Option<String>,
    transcript_path: Option<String>,
) {
    if state.agent(&agent_id).ok().flatten().is_some_and(|agent| {
        agent.session_id == session_id
            && agent.transcript_path.is_some()
            && transcript_path
                .as_deref()
                .is_none_or(|path| agent.transcript_path.as_deref() == Some(path))
    }) {
        return;
    }

    if transcript_path.is_none()
        && !session_id
            .as_deref()
            .is_some_and(looks_like_codex_session_id)
    {
        // No usable session id and no explicit transcript path, so directory
        // discovery can't run. An already-bound pane may receive such a hook from
        // a transient side conversation; keep its canonical binding quietly.
        emit_codex_transcript_failure_if_unbound(
            &state,
            &agent_id,
            "Transcript unavailable: Codex did not report a usable session id",
            None,
        );
        return;
    }

    let generation = match state.begin_transcript_binding_candidate(
        &agent_id,
        session_id.as_deref(),
        transcript_path.as_deref(),
    ) {
        Ok(Some(generation)) => generation,
        Ok(None) => return,
        Err(err) => {
            emit_codex_transcript_failure_if_unbound(&state, &agent_id, &err, None);
            return;
        }
    };

    if let Some(transcript_path) = transcript_path {
        start_explicit_codex_transcript_binding(
            state,
            agent_id,
            generation,
            session_id,
            transcript_path,
        );
        return;
    }

    let session_id = session_id.expect("usable session id checked above");
    let Ok(codex_home) = codex_home() else {
        finish_codex_transcript_binding_failure(
            &state,
            &agent_id,
            generation,
            "Transcript unavailable: could not resolve CODEX_HOME",
            None,
        );
        return;
    };

    thread::spawn(move || {
        for attempt in 0..CODEX_TRANSCRIPT_DISCOVERY_ATTEMPTS {
            if !codex_binding_should_continue(&state, &agent_id, generation) {
                state.clear_transcript_binding_candidate(&agent_id, generation);
                return;
            }
            match find_codex_transcript_path(&codex_home, &session_id) {
                Ok(Some(path)) => {
                    let path_string = path.display().to_string();
                    if let Err(err) = bind_codex_transcript_path(
                        &state,
                        &agent_id,
                        generation,
                        Some(&session_id),
                        &path,
                    ) {
                        finish_codex_transcript_binding_failure(
                            &state,
                            &agent_id,
                            generation,
                            &err,
                            Some(&path_string),
                        );
                    }
                    return;
                }
                Ok(None) => {}
                Err(err) => {
                    finish_codex_transcript_binding_failure(
                        &state, &agent_id, generation, &err, None,
                    );
                    return;
                }
            }

            if attempt + 1 < CODEX_TRANSCRIPT_DISCOVERY_ATTEMPTS {
                thread::sleep(CODEX_TRANSCRIPT_DISCOVERY_DELAY);
            }
        }

        finish_codex_transcript_binding_failure(
            &state,
            &agent_id,
            generation,
            "Transcript unavailable",
            None,
        );
    });
}

fn start_explicit_codex_transcript_binding(
    state: AppState,
    agent_id: String,
    generation: u64,
    expected_session_id: Option<String>,
    transcript_path: String,
) {
    thread::spawn(move || {
        let path = PathBuf::from(&transcript_path);
        for attempt in 0..CODEX_TRANSCRIPT_DISCOVERY_ATTEMPTS {
            if !codex_binding_should_continue(&state, &agent_id, generation) {
                state.clear_transcript_binding_candidate(&agent_id, generation);
                return;
            }
            match codex_transcript_path_ready(&path, expected_session_id.as_deref()) {
                Ok(true) => {
                    if let Err(err) = bind_codex_transcript_path(
                        &state,
                        &agent_id,
                        generation,
                        expected_session_id.as_deref(),
                        &path,
                    ) {
                        finish_codex_transcript_binding_failure(
                            &state,
                            &agent_id,
                            generation,
                            &err,
                            Some(&transcript_path),
                        );
                    }
                    return;
                }
                Ok(false) => {}
                Err(err) => {
                    finish_codex_transcript_binding_failure(
                        &state,
                        &agent_id,
                        generation,
                        &err,
                        Some(&transcript_path),
                    );
                    return;
                }
            }

            if attempt + 1 < CODEX_TRANSCRIPT_DISCOVERY_ATTEMPTS {
                thread::sleep(CODEX_TRANSCRIPT_DISCOVERY_DELAY);
            }
        }

        finish_codex_transcript_binding_failure(
            &state,
            &agent_id,
            generation,
            "Transcript unavailable",
            Some(&transcript_path),
        );
    });
}

/// A validator remains live only while its agent exists and no newer hook has
/// superseded its candidate generation.
fn codex_binding_should_continue(state: &AppState, agent_id: &str, generation: u64) -> bool {
    state.agent(agent_id).ok().flatten().is_some()
        && state.transcript_binding_candidate_is_current(agent_id, generation)
}

fn codex_transcript_path_ready(
    path: &Path,
    expected_session_id: Option<&str>,
) -> Result<bool, String> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
        return Err("Codex transcript must be a .jsonl file".to_string());
    }

    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(format!(
                "failed to inspect Codex transcript {}: {err}",
                path.display()
            ));
        }
    };
    if !metadata.is_file() {
        return Err(format!("Codex transcript {} is not a file", path.display()));
    }

    let Some(actual_session_id) = read_codex_transcript_session_id(path)? else {
        return Ok(false);
    };
    if let Some(expected_session_id) = expected_session_id {
        if actual_session_id != expected_session_id {
            // The file at this path currently belongs to a different session — it
            // may be a stale/rotated rollout, or still mid-write so its first line
            // is an older session_meta. Treat it as "not ready yet" so the caller
            // keeps polling rather than permanently aborting the binding; if it
            // never matches, the discovery loop emits a notice once attempts run
            // out.
            return Ok(false);
        }
    }

    Ok(true)
}

fn bind_codex_transcript_path(
    state: &AppState,
    agent_id: &str,
    generation: u64,
    expected_session_id: Option<&str>,
    path: &Path,
) -> Result<(), String> {
    let actual_session_id = read_codex_transcript_session_id(path)?
        .ok_or_else(|| format!("Codex transcript {} has no session_meta id", path.display()))?;
    if let Some(expected_session_id) = expected_session_id {
        if expected_session_id != actual_session_id {
            return Err(format!(
                "Codex transcript {} belongs to session {}, not {}",
                path.display(),
                actual_session_id,
                expected_session_id
            ));
        }
    }
    let path_string = path.display().to_string();
    let updated = state.commit_transcript_binding_candidate(
        agent_id,
        generation,
        &actual_session_id,
        &path_string,
    )?;

    if let Some(agent) = updated {
        state.emit(QmuxEvent::new(
            "agent.transcript_bound",
            agent.pane_id.clone(),
            Some(agent.id.clone()),
            json!({ "agent": agent, "transcriptPath": path_string }),
        ));
        emit_codex_transcript_notice(state, agent_id, None, Some(&path_string));
        start_transcript_tail(
            state.clone(),
            agent_id.to_string(),
            path_string,
            "codex".to_string(),
        );
    }

    Ok(())
}

fn finish_codex_transcript_binding_failure(
    state: &AppState,
    agent_id: &str,
    generation: u64,
    message: &str,
    path: Option<&str>,
) {
    if !state.transcript_binding_candidate_is_current(agent_id, generation) {
        return;
    }
    state.clear_transcript_binding_candidate(agent_id, generation);
    emit_codex_transcript_failure_if_unbound(state, agent_id, message, path);
}

fn emit_codex_transcript_failure_if_unbound(
    state: &AppState,
    agent_id: &str,
    message: &str,
    path: Option<&str>,
) {
    if state
        .agent(agent_id)
        .ok()
        .flatten()
        .is_some_and(|agent| agent.transcript_path.is_some())
    {
        return;
    }
    emit_codex_transcript_notice(state, agent_id, Some(message), path);
}

fn emit_codex_transcript_notice(
    state: &AppState,
    agent_id: &str,
    message: Option<&str>,
    path: Option<&str>,
) {
    state.emit(QmuxEvent::new(
        "transcript.notice",
        None,
        Some(agent_id.to_string()),
        json!({ "message": message, "path": path }),
    ));
}

fn find_codex_transcript_path(
    codex_home: &Path,
    session_id: &str,
) -> Result<Option<PathBuf>, String> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return Ok(None);
    }
    let root = codex_home.join("sessions");
    if !root.exists() {
        return Ok(None);
    }

    let mut candidates = gather_transcript_candidates_recursive(&root)?
        .into_iter()
        .filter(|candidate| {
            codex_transcript_session_id(&candidate.path).as_deref() == Some(session_id)
        })
        .map(|candidate| (candidate.modified, candidate.path))
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    Ok(candidates.into_iter().map(|(_, path)| path).next())
}

fn looks_like_codex_session_id(value: &str) -> bool {
    let value = value.trim();
    // Only a sanity gate to avoid scanning the sessions tree for an obviously
    // unusable id. Accept any non-empty id free of path separators and control
    // characters rather than requiring a canonical 36-char UUID, so a non-UUID id
    // scheme still drives directory discovery instead of silently binding nothing.
    !value.is_empty()
        && !value.contains('/')
        && !value.contains('\\')
        && !value.chars().any(|ch| ch.is_control())
}

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
            let blocks = parse_codex_message_blocks(payload.get("content"))?;
            (role.to_string(), blocks)
        }
        "function_call" | "custom_tool_call" => {
            let name = string_field(payload, "name").unwrap_or_else(|| "tool".to_string());
            (
                "assistant".to_string(),
                vec![TurnBlock::ToolUse {
                    id: string_field(payload, "call_id")
                        .or_else(|| string_field(payload, "callId"))
                        .or_else(|| string_field(payload, "id")),
                    name,
                    input: codex_tool_input(payload),
                }],
            )
        }
        "function_call_output" | "custom_tool_call_output" => (
            "assistant".to_string(),
            vec![TurnBlock::ToolResult {
                tool_use_id: string_field(payload, "call_id")
                    .or_else(|| string_field(payload, "callId")),
                content: payload.get("output").cloned().unwrap_or(Value::Null),
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
        context_status: None,
        native_id: codex_payload_turn_id(payload),
        parent_native_id: None,
        native_message_id: string_field(payload, "id"),
    })
}

fn codex_workspace_observation(line: &str) -> Option<WorkspaceObservation> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    let raw_cwd = match value.get("type").and_then(Value::as_str)? {
        "turn_context" => value
            .get("payload")?
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::trim),
        "event_msg" => {
            let payload = value.get("payload")?;
            if payload.get("type").and_then(Value::as_str) != Some("item_completed") {
                return None;
            }
            let item = payload.get("item")?;
            if item.get("type").and_then(Value::as_str) != Some("CommandExecution") {
                return None;
            }
            item.get("cwd").and_then(Value::as_str).map(str::trim)
        }
        _ => None,
    }?;
    if raw_cwd.is_empty() {
        return None;
    }
    Some(WorkspaceObservation {
        cwd: decode_codex_cwd(raw_cwd)?,
        source: ActiveWorkspaceSource::Codex,
        session_id: codex_observation_session_id(&value),
        observed_at_millis: super::native_timestamp_ms(&value)
            .and_then(|millis| u128::try_from(millis).ok()),
    })
}

fn decode_codex_cwd(raw_cwd: &str) -> Option<String> {
    if raw_cwd.starts_with("file:") {
        url::Url::parse(raw_cwd)
            .ok()?
            .to_file_path()
            .ok()
            .map(|path| path.display().to_string())
    } else {
        Some(raw_cwd.to_string())
    }
}

fn codex_observation_session_id(value: &Value) -> Option<String> {
    string_field(value, "session_id")
        .or_else(|| string_field(value, "sessionId"))
        .or_else(|| {
            value.get("payload").and_then(|payload| {
                string_field(payload, "session_id")
                    .or_else(|| string_field(payload, "sessionId"))
                    .or_else(|| string_field(payload, "thread_id"))
            })
        })
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
    // Codex rollback removes complete replay segments newest-first until it has
    // crossed the requested number of genuine user boundaries. A newer
    // tool/context-only segment is removed too, but does not decrement the
    // count. Model those segments explicitly rather than treating every
    // task_started or role=user record as an independent user turn.
    let mut active_segments: Vec<CodexReplaySegment> = Vec::new();
    let mut interrupted_turn_ids = HashSet::new();
    let mut rolled_back_source_indices = HashSet::new();
    let mut seen_native_message_ids = HashSet::new();

    for (relative_index, line) in lines.iter().enumerate() {
        let source_index = source_index_offset + relative_index;
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(_) => continue,
        };

        if value.get("type").and_then(Value::as_str) == Some("response_item") {
            let segment_index = value.get("payload").and_then(|payload| {
                record_codex_response_segment(
                    &mut active_segments,
                    codex_payload_turn_id(payload),
                    codex_payload_is_user_turn_boundary(payload),
                )
            });
            if let Some(turn) = parse_transcript_line(agent_id, source_index, line) {
                if let Some(segment_index) = segment_index {
                    active_segments[segment_index]
                        .turn_source_indices
                        .push(source_index);
                }
                if should_add_codex_turn(&turn, &mut seen_native_message_ids) {
                    turns.push(turn);
                }
            }
            continue;
        }

        if value.get("type").and_then(Value::as_str) == Some("compacted") {
            for turn in codex_compacted_record_turns(agent_id, source_index, &value) {
                if should_add_codex_turn(&turn, &mut seen_native_message_ids) {
                    turns.push(turn);
                }
            }
            continue;
        }

        let Some(payload) = value
            .get("payload")
            .filter(|_| value.get("type").and_then(Value::as_str) == Some("event_msg"))
        else {
            continue;
        };
        match payload.get("type").and_then(Value::as_str) {
            Some("task_started") => {
                if let Some(turn_id) = string_field(payload, "turn_id") {
                    start_codex_replay_segment(&mut active_segments, turn_id);
                }
            }
            Some("user_message") => {
                // This event is Codex's non-contextual user boundary. Context
                // reinjections appear only as response items and are filtered
                // by codex_payload_is_user_turn_boundary below.
                if let Some(segment) = active_segments.last_mut() {
                    segment.counts_as_user_turn = true;
                }
            }
            Some("turn_aborted") => {
                if let Some(turn_id) = string_field(payload, "turn_id") {
                    interrupted_turn_ids.insert(turn_id);
                }
            }
            Some("thread_rolled_back") => {
                let num_turns = payload
                    .get("num_turns")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let mut remaining = num_turns;
                while remaining > 0 {
                    let Some(segment) = active_segments.pop() else {
                        break;
                    };
                    rolled_back_source_indices.extend(segment.turn_source_indices);
                    if segment.counts_as_user_turn {
                        remaining -= 1;
                    }
                }
            }
            _ => {}
        }
    }

    for turn in &mut turns {
        if turn
            .native_id
            .as_deref()
            .is_some_and(|turn_id| interrupted_turn_ids.contains(turn_id))
        {
            turn.status = Some(TurnStatus::Interrupted);
            turn.status_reason = Some(TurnStatusReason::Interrupted);
        }
        if rolled_back_source_indices.contains(&turn.source_index) {
            turn.context_status = Some(TurnContextStatus::RolledBack);
        }
    }

    turns
}

const COMPACTED_SOURCE_INDEX_STRIDE: usize = 1_000_000;

/// Returns true the first time a native message id is seen. Turns without a
/// native message id are always admitted. This prevents a message from
/// appearing twice when it is recorded both as a `response_item` and inside a
/// later `compacted.replacement_history`.
fn should_add_codex_turn(turn: &Turn, seen_native_message_ids: &mut HashSet<String>) -> bool {
    turn.native_message_id
        .as_ref()
        .map_or(true, |id| seen_native_message_ids.insert(id.clone()))
}

/// Expands a Codex `compacted` record into the user/assistant `message` entries
/// it summarizes. The top-level `compacted` line itself does not appear as a
/// turn; instead, each entry in `payload.replacement_history` becomes a turn
/// with a synthetic source index derived from the parent line so it is unique
/// and stable across re-reads. This makes the right pane transcript keep
/// working after Codex replaces earlier `response_item` records with a compacted
/// summary.
fn codex_compacted_record_turns(agent_id: &str, source_index: usize, value: &Value) -> Vec<Turn> {
    let Some(payload) = value.get("payload") else {
        return Vec::new();
    };
    let Some(history) = payload.get("replacement_history").and_then(Value::as_array) else {
        return Vec::new();
    };
    let session_id = string_field(value, "session_id").or_else(|| string_field(value, "sessionId"));
    let parent_timestamp = super::native_timestamp_ms(value);
    let mut turns = Vec::new();

    for (index, item) in history.iter().enumerate() {
        let item_type = item.get("type").and_then(Value::as_str);
        if item_type == Some("compaction") {
            // A nested compaction is an encrypted summary of an earlier
            // compaction; it has no human-readable content to display.
            continue;
        }
        if item_type != Some("message") {
            continue;
        }
        let Some(role) = item.get("role").and_then(Value::as_str) else {
            continue;
        };
        if role == "developer" || role == "system" {
            continue;
        }
        let Some(blocks) = parse_codex_message_blocks(item.get("content")) else {
            continue;
        };
        if blocks.is_empty() {
            continue;
        }
        let compacted_source_index = (source_index + 1)
            .saturating_mul(COMPACTED_SOURCE_INDEX_STRIDE)
            .saturating_add(index);
        turns.push(Turn {
            id: format!("{agent_id}-{compacted_source_index}"),
            agent_id: agent_id.to_string(),
            session_id: session_id.clone(),
            role: role.to_string(),
            blocks,
            source_index: compacted_source_index,
            timestamp: super::native_timestamp_ms(item).or(parent_timestamp),
            status: None,
            status_reason: None,
            context_status: None,
            native_id: codex_payload_turn_id(item),
            parent_native_id: None,
            native_message_id: string_field(item, "id"),
        });
    }

    turns
}

struct CodexReplaySegment {
    turn_id: String,
    counts_as_user_turn: bool,
    turn_source_indices: Vec<usize>,
}

fn start_codex_replay_segment(segments: &mut Vec<CodexReplaySegment>, turn_id: String) {
    if segments
        .last()
        .is_some_and(|segment| segment.turn_id == turn_id)
    {
        return;
    }
    segments.push(CodexReplaySegment {
        turn_id,
        counts_as_user_turn: false,
        turn_source_indices: Vec::new(),
    });
}

fn record_codex_response_segment(
    segments: &mut Vec<CodexReplaySegment>,
    turn_id: Option<String>,
    counts_as_user_turn: bool,
) -> Option<usize> {
    let segment_index = match turn_id {
        Some(turn_id) => {
            if let Some(index) = segments
                .iter()
                .rposition(|segment| segment.turn_id == turn_id)
            {
                index
            } else {
                start_codex_replay_segment(segments, turn_id);
                segments.len() - 1
            }
        }
        None => segments.len().checked_sub(1)?,
    };
    if let Some(segment) = segments.get_mut(segment_index) {
        segment.counts_as_user_turn |= counts_as_user_turn;
    }
    Some(segment_index)
}

fn codex_payload_is_user_turn_boundary(payload: &Value) -> bool {
    if payload.get("type").and_then(Value::as_str) != Some("message")
        || payload.get("role").and_then(Value::as_str) != Some("user")
    {
        return false;
    }
    match payload.get("content") {
        Some(Value::String(text)) => codex_user_text_is_turn_boundary(text),
        Some(Value::Array(items)) => items.iter().any(|item| {
            if item.get("type").and_then(Value::as_str) == Some("input_image") {
                return true;
            }
            matches!(
                item.get("type").and_then(Value::as_str),
                Some("input_text" | "text")
            ) && item
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(codex_user_text_is_turn_boundary)
        }),
        _ => false,
    }
}

fn codex_user_text_is_turn_boundary(text: &str) -> bool {
    let text = text.trim();
    !text.is_empty() && !codex_user_text_is_contextual(text)
}

/// Mirrors the stable wrappers Codex uses for user-role context closely enough
/// for old rollouts that lack `EventMsg::UserMessage`. Do not classify arbitrary
/// XML-looking text as context: a user's whole prompt can legitimately be
/// wrapped in `<request>` (or any other application-specific tag), and treating
/// it as plumbing makes a later rollback consume one extra real turn.
fn codex_user_text_is_contextual(text: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "# agents.md instructions",
        "<environment_context",
        "<user_shell_command>",
        "<turn_aborted>",
        "<subagent_notification>",
        "<codex_internal_context",
        "<goal_context>",
        "<recommended_plugins>",
        "<additional_context",
        "<hook_prompt",
    ];
    let text = text.trim_start().to_ascii_lowercase();
    PREFIXES.iter().any(|prefix| text.starts_with(prefix))
}

fn codex_payload_turn_id(payload: &Value) -> Option<String> {
    payload
        .get("internal_chat_message_metadata_passthrough")
        .and_then(|metadata| string_field(metadata, "turn_id"))
        .or_else(|| string_field(payload, "turn_id"))
}

/// Writes a rollout truncated to exclude the anchored turn, returning the new
/// session's id and path. Codex rollouts are linear — no uuid graph, and
/// `codex resume` appends in place rather than branching a file — so the cut is
/// positional.
///
/// The cut lands on the `task_started` event that opens the anchored turn, not
/// on the anchor's own line. A Codex turn spans several records (`task_started`,
/// `turn_context`, the `response_item` the anchor points at, and the
/// `event_msg` mirror of it); cutting at the anchor itself would leave the
/// turn's opening records behind and replay a duplicate prompt.
/// The shared shape of a session-continuing launch. `fork` picks the
/// subcommand: `codex fork <id>` branches at the head, `codex resume <id>`
/// continues the session as-is. `--` delimits the prompt so a leading `-` in
/// attacker-influenced text cannot be read as a flag.
fn shell_session_args(
    session_id: &str,
    model: Option<&str>,
    prompt: Option<&str>,
    fork: bool,
) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    args.push(if fork { "fork" } else { "resume" }.to_string());
    args.push(session_id.to_string());
    if let Some(prompt) = prompt.map(str::trim).filter(|prompt| !prompt.is_empty()) {
        args.push("--".to_string());
        args.push(prompt.to_string());
    }
    args
}

fn synthesize_truncated_codex_session(
    transcript_path: &Path,
    anchor: &MessageAnchor,
) -> Result<String, String> {
    let contents = fs::read_to_string(transcript_path).map_err(|err| {
        format!(
            "cannot read transcript {}: {err}",
            transcript_path.display()
        )
    })?;
    let records = parse_transcript_records(&contents);
    if anchor.source_index >= records.len() {
        return Err(format!(
            "fork anchor {} is past the end of {}",
            anchor.source_index,
            transcript_path.display()
        ));
    }

    let cut = records[..anchor.source_index]
        .iter()
        .rposition(|(_, value)| is_codex_turn_start(value))
        .ok_or_else(|| FORK_AT_MESSAGE_EMPTY_ERROR.to_string())?;

    // A prefix with no conversational message is preamble only (session meta,
    // sandbox policy, AGENTS.md) — a new session rather than a fork.
    let keeps_history = records[..cut]
        .iter()
        .any(|(_, value)| is_codex_conversational_message(value));
    if !keeps_history {
        return Err(FORK_AT_MESSAGE_EMPTY_ERROR.to_string());
    }

    let session_id = new_uuid_v4()?;
    let out_path = codex_fork_rollout_path(transcript_path, &session_id)?;

    let mut body = String::new();
    for (line, value) in &records[..cut] {
        if value.get("type").and_then(Value::as_str) == Some("session_meta") {
            let mut rewritten = value.clone();
            if let Some(payload) = rewritten.get_mut("payload") {
                payload["session_id"] = json!(session_id);
                if payload.get("id").is_some() {
                    payload["id"] = json!(session_id);
                }
            }
            body.push_str(&serde_json::to_string(&rewritten).map_err(|err| err.to_string())?);
        } else {
            body.push_str(line);
        }
        body.push('\n');
    }

    fs::write(&out_path, body)
        .map_err(|err| format!("cannot write fork transcript {}: {err}", out_path.display()))?;

    Ok(session_id)
}

fn is_codex_turn_start(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("event_msg")
        && value
            .get("payload")
            .and_then(|payload| payload.get("type"))
            .and_then(Value::as_str)
            == Some("task_started")
}

fn is_codex_conversational_message(value: &Value) -> bool {
    if value.get("type").and_then(Value::as_str) != Some("response_item") {
        return false;
    }
    let Some(payload) = value.get("payload") else {
        return false;
    };
    if payload.get("type").and_then(Value::as_str) != Some("message") {
        return false;
    }
    matches!(
        payload.get("role").and_then(Value::as_str),
        Some("user") | Some("assistant")
    )
}

/// Names the fork after the source rollout's timestamp rather than the current
/// time: Codex resolves sessions by the id in the filename (and in
/// `session_meta`), so the timestamp only affects directory ordering, where
/// sorting beside the session it branched from is the more useful result.
fn codex_fork_rollout_path(transcript_path: &Path, session_id: &str) -> Result<PathBuf, String> {
    let parent = transcript_path
        .parent()
        .ok_or_else(|| format!("transcript {} has no parent", transcript_path.display()))?;
    let stem = transcript_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| format!("transcript {} has no file name", transcript_path.display()))?;
    let timestamp = stem
        .strip_prefix("rollout-")
        .and_then(|rest| {
            // `rollout-<YYYY-MM-DDTHH-MM-SS>-<id>`: the timestamp is a fixed
            // five hyphen-separated groups. Splitting from the left bounds it
            // without assuming anything about the id, whose own hyphen count
            // varies (a bare id, or a uuid's five groups).
            let mut parts = rest.splitn(6, '-');
            let stamp: Vec<&str> = parts.by_ref().take(5).collect();
            let id = parts.next();
            (stamp.len() == 5 && id.is_some_and(|id| !id.is_empty())).then(|| stamp.join("-"))
        })
        .ok_or_else(|| {
            format!(
                "transcript {} is not a rollout-<timestamp>-<id>.jsonl file",
                transcript_path.display()
            )
        })?;
    Ok(parent.join(format!("rollout-{timestamp}-{session_id}.jsonl")))
}

fn is_codex_status_event(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    // A `compacted` record replaces earlier history with its own
    // `replacement_history`, so the whole visible transcript has to be re-resolved
    // rather than appended one line at a time.
    if value.get("type").and_then(Value::as_str) == Some("compacted") {
        return true;
    }
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return false;
    }
    let Some(event_type) = value
        .get("payload")
        .and_then(|payload| payload.get("type"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    matches!(event_type, "turn_aborted" | "thread_rolled_back")
}

fn parse_transcript_lifecycle_event(line: &str) -> Option<TranscriptLifecycleEvent> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    let payload = value.get("payload")?;
    match payload.get("type").and_then(Value::as_str) {
        Some("turn_aborted") => Some(TranscriptLifecycleEvent::Interrupted),
        Some("task_started") => Some(TranscriptLifecycleEvent::TurnStarted),
        // TurnComplete. Codex also emits Stop mid-turn (review jobs, internal
        // TUI queue); only this record means the parent turn actually finished.
        Some("task_complete") => Some(TranscriptLifecycleEvent::TurnCompleted),
        _ => None,
    }
}

fn parse_codex_message_blocks(content: Option<&Value>) -> Option<Vec<TurnBlock>> {
    match content? {
        Value::String(text) => Some(vec![TurnBlock::Text { text: text.clone() }]),
        Value::Array(items) => {
            let mut blocks = Vec::new();
            let mut index = 0;
            while index < items.len() {
                // Current Codex transcripts split a pasted image across three
                // content items: an opening <image ...> text item, the base64
                // input_image payload, and a closing </image> text item. Keep
                // the safe on-disk marker while dropping the huge data URL so
                // the frontend can load the source path as a thumbnail.
                if let Some(marker) = codex_image_marker_at(items, index) {
                    blocks.push(TurnBlock::Text { text: marker });
                    index += 3;
                    continue;
                }

                let item = &items[index];
                let block_type = item.get("type").and_then(Value::as_str);
                if matches!(block_type, Some("input_text" | "output_text" | "text"))
                    && let Some(text) = item.get("text").and_then(Value::as_str)
                {
                    blocks.push(TurnBlock::Text {
                        text: text.to_string(),
                    });
                }
                index += 1;
            }
            Some(blocks)
        }
        _ => None,
    }
}

fn codex_image_marker_at(items: &[Value], index: usize) -> Option<String> {
    let opening = items.get(index)?;
    let image = items.get(index + 1)?;
    let closing = items.get(index + 2)?;
    let opening_text = opening
        .get("type")
        .and_then(Value::as_str)
        .filter(|kind| matches!(*kind, "input_text" | "text"))
        .and_then(|_| opening.get("text"))
        .and_then(Value::as_str)?
        .trim();
    let is_image_payload = image.get("type").and_then(Value::as_str) == Some("input_image");
    let closing_text = closing
        .get("type")
        .and_then(Value::as_str)
        .filter(|kind| matches!(*kind, "input_text" | "text"))
        .and_then(|_| closing.get("text"))
        .and_then(Value::as_str)?
        .trim();

    (is_image_payload
        && opening_text.starts_with("<image ")
        && opening_text.ends_with('>')
        && closing_text == "</image>")
        .then(|| format!("{opening_text}\n</image>"))
}

fn codex_tool_input(payload: &Value) -> Value {
    if let Some(arguments) = payload.get("arguments") {
        if let Some(arguments) = arguments.as_str() {
            return serde_json::from_str(arguments)
                .unwrap_or_else(|_| Value::String(arguments.to_string()));
        }
        return arguments.clone();
    }
    payload.get("input").cloned().unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AdapterConfigs, ClaudeAdapterConfig, CodexAdapterConfig, GrokAdapterConfig,
        MuseAdapterConfig, OpencodeAdapterConfig,
    };
    use crate::state::{AppState, PaneInfo, PaneRuntime, PaneStatus};
    use portable_pty::{Child, ChildKiller, ExitStatus, PtySize, native_pty_system};
    use std::io::{self, Write};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn svec(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn workspace_observation_uses_turn_context_cwd_and_record_session_id() {
        let line = json!({
            "type": "turn_context",
            "session_id": "child-session",
            "payload": { "cwd": "/tmp/feature worktree" }
        })
        .to_string();

        assert_eq!(
            codex_workspace_observation(&line),
            Some(WorkspaceObservation {
                cwd: "/tmp/feature worktree".to_string(),
                source: ActiveWorkspaceSource::Codex,
                session_id: Some("child-session".to_string()),
                observed_at_millis: None,
            })
        );
    }

    #[test]
    fn workspace_observation_decodes_completed_command_file_urls() {
        let line = json!({
            "type": "event_msg",
            "payload": {
                "type": "item_completed",
                "thread_id": "child-session",
                "item": {
                    "type": "CommandExecution",
                    "cwd": "file:///tmp/feature%20worktree",
                    "command": "pwd",
                    "status": "completed"
                }
            }
        })
        .to_string();

        assert_eq!(
            codex_workspace_observation(&line),
            Some(WorkspaceObservation {
                cwd: "/tmp/feature worktree".to_string(),
                source: ActiveWorkspaceSource::Codex,
                session_id: Some("child-session".to_string()),
                observed_at_millis: None,
            })
        );
    }

    #[test]
    fn workspace_observation_ignores_session_and_incomplete_command_records() {
        let session = json!({ "type": "session_meta", "cwd": "/wrong" }).to_string();
        let started = json!({
            "type": "event_msg",
            "payload": {
                "type": "item_started",
                "item": { "type": "CommandExecution", "cwd": "/wrong" }
            }
        })
        .to_string();

        assert_eq!(codex_workspace_observation(&session), None);
        assert_eq!(codex_workspace_observation(&started), None);
    }

    #[test]
    fn launch_options_reject_unknown_fields() {
        let err = CodexLaunchOptions::from_value(json!({ "bogus": true })).unwrap_err();

        assert!(err.contains("invalid Codex adapter options"));
    }

    #[test]
    fn launch_options_reject_removed_profile_and_oss_options() {
        let profile_err = CodexLaunchOptions::from_value(json!({ "profile": "work" })).unwrap_err();
        let oss_err = CodexLaunchOptions::from_value(json!({ "oss": true })).unwrap_err();

        assert!(profile_err.contains("invalid Codex adapter options"));
        assert!(oss_err.contains("invalid Codex adapter options"));
    }

    #[test]
    fn launch_options_validate_known_enums() {
        let err = CodexLaunchOptions::from_value(json!({ "sandbox": "full-send" })).unwrap_err();

        assert!(err.contains("invalid Codex adapter option sandbox"));
    }

    #[test]
    fn launch_options_reject_deprecated_on_failure_approval_policy() {
        let err =
            CodexLaunchOptions::from_value(json!({ "approvalPolicy": "on-failure" })).unwrap_err();

        assert!(err.contains("invalid Codex adapter option approvalPolicy"));
    }

    #[test]
    fn launch_options_validate_approvals_reviewer() {
        let err =
            CodexLaunchOptions::from_value(json!({ "approvalsReviewer": "robot" })).unwrap_err();

        assert!(err.contains("invalid Codex adapter option approvalsReviewer"));
    }

    #[test]
    fn codex_binary_keeps_path_when_code_mode_host_is_sibling() {
        let dir = temp_dir();
        let binary = dir.join("codex");
        let host = dir.join(CODEX_CODE_MODE_HOST);
        fs::write(&binary, "").unwrap();
        fs::write(&host, "").unwrap();

        assert_eq!(codex_binary_with_code_mode_host(binary.clone()), binary);
    }

    #[test]
    fn codex_binary_uses_symlink_target_when_host_alias_is_missing() {
        let root = temp_dir();
        let shim_dir = root.join("shim-bin");
        let real_dir = root.join("real-bin");
        fs::create_dir_all(&shim_dir).unwrap();
        fs::create_dir_all(&real_dir).unwrap();
        let real_binary = real_dir.join("codex");
        let shim_binary = shim_dir.join("codex");
        fs::write(&real_binary, "").unwrap();
        fs::write(real_dir.join(CODEX_CODE_MODE_HOST), "").unwrap();
        std::os::unix::fs::symlink(&real_binary, &shim_binary).unwrap();

        let resolved = codex_binary_with_code_mode_host(shim_binary.clone());

        assert_eq!(resolved, fs::canonicalize(&real_binary).unwrap());
        assert!(!shim_dir.join(CODEX_CODE_MODE_HOST).exists());
    }

    #[test]
    fn build_args_adds_cwd_model_options_and_tail_args() {
        let options = CodexLaunchOptions::from_value(json!({
            "sandbox": "workspace-write",
            "approvalPolicy": "on-request",
            "search": true
        }))
        .unwrap();

        let args = build_codex_args(
            Path::new("/tmp/qmux"),
            Some(Path::new("/tmp/qmux/.qmux/workspaces")),
            Some("gpt-5"),
            &options,
            Some("Use QMUX_WORKTREE_ROOT.\nTreat it as an opaque path."),
            None,
            vec!["--".to_string(), "start here".to_string()],
        );

        assert_eq!(
            args,
            vec![
                "--cd",
                "/tmp/qmux",
                "--add-dir",
                "/tmp/qmux/.qmux/workspaces",
                "--model",
                "gpt-5",
                "--profile",
                "qmux-codex",
                "--config",
                "developer_instructions=\"Use QMUX_WORKTREE_ROOT.\\nTreat it as an opaque path.\"",
                "--sandbox",
                "workspace-write",
                "--ask-for-approval",
                "on-request",
                "--search",
                "--",
                "start here"
            ]
        );
    }

    #[test]
    fn build_args_adds_auto_review_without_approval_policy_override() {
        let options = CodexLaunchOptions::from_value(json!({
            "sandbox": "workspace-write",
            "approvalPolicy": "on-request",
            "approvalsReviewer": "auto_review"
        }))
        .unwrap();

        let args = build_codex_args(
            Path::new("/tmp/qmux"),
            None,
            None,
            &options,
            None,
            None,
            Vec::new(),
        );

        assert_eq!(
            args,
            vec![
                "--cd",
                "/tmp/qmux",
                "--profile",
                "qmux-codex",
                "--sandbox",
                "workspace-write",
                "--config",
                "approvals_reviewer=\"auto_review\"",
                "--search"
            ]
        );
    }

    #[test]
    fn worktree_root_env_is_only_added_when_resolved() {
        let mut envs = vec![("EXISTING".to_string(), "value".to_string())];
        add_codex_worktree_root_env(&mut envs, None);
        assert_eq!(envs.len(), 1);

        add_codex_worktree_root_env(&mut envs, Some("/repo/.claude/worktrees"));
        assert_eq!(
            envs.last(),
            Some(&(
                "QMUX_WORKTREE_ROOT".to_string(),
                "/repo/.claude/worktrees".to_string()
            ))
        );
    }

    #[test]
    fn launch_options_validate_reasoning_effort() {
        for level in CODEX_REASONING_EFFORT_LEVELS {
            let options = CodexLaunchOptions::from_value(json!({ "reasoningEffort": level }))
                .expect("current Codex reasoning level should be accepted");
            assert_eq!(options.reasoning_effort.as_deref(), Some(*level));
        }

        let err =
            CodexLaunchOptions::from_value(json!({ "reasoningEffort": "extreme" })).unwrap_err();
        assert!(err.contains("invalid Codex adapter option reasoningEffort"));
    }

    #[test]
    fn build_args_adds_reasoning_effort_config() {
        let options =
            CodexLaunchOptions::from_value(json!({ "reasoningEffort": "ultra" })).unwrap();

        let args = build_codex_args(
            Path::new("/tmp/qmux"),
            None,
            Some("gpt-5.6-luna"),
            &options,
            None,
            None,
            Vec::new(),
        );

        assert_eq!(
            args,
            vec![
                "--cd",
                "/tmp/qmux",
                "--model",
                "gpt-5.6-luna",
                "--profile",
                "qmux-codex",
                "--sandbox",
                "workspace-write",
                "--config",
                "model_reasoning_effort=\"ultra\"",
                "--search"
            ]
        );
    }

    #[test]
    fn remote_args_inline_hooks_without_replacing_codex_home() {
        let args = build_codex_args(
            Path::new("/srv/project"),
            None,
            None,
            &CodexLaunchOptions::default(),
            None,
            Some("/opt/qmux tools/qmux-cli"),
            Vec::new(),
        );

        assert!(!args.iter().any(|arg| arg == "--profile"));
        assert!(
            args.windows(2)
                .any(|pair| { pair == ["--config", "features.hooks=true"] })
        );
        assert!(args.iter().any(|arg| {
            arg.starts_with("hooks.SessionStart=")
                && arg.contains("matcher=\"startup|resume\"")
                && arg.contains("'/opt/qmux tools/qmux-cli' notify SessionStart")
        }));
        assert!(args.iter().any(|arg| {
            arg.starts_with("hooks.Stop=") && arg.contains("'/opt/qmux tools/qmux-cli' notify Stop")
        }));
    }

    #[test]
    fn prompt_tail_args_trim_and_delimit_initial_prompt() {
        assert_eq!(prompt_tail_args("   "), Vec::<String>::new());
        assert_eq!(
            prompt_tail_args("  start here  "),
            vec!["--".to_string(), "start here".to_string()]
        );
    }

    #[test]
    fn args_contain_prompt_detects_interactive_codex_launches() {
        assert!(!args_contain_prompt(&[]));
        assert!(!args_contain_prompt(&svec(&["--model", "gpt-5"])));
        assert!(!args_contain_prompt(&svec(&[
            "--add-dir",
            "/tmp/workspaces"
        ])));
        assert!(!args_contain_prompt(&svec(&["--sandbox=workspace-write"])));
        assert!(!args_contain_prompt(&svec(&["--add-dir=/tmp/workspaces"])));
        assert!(!args_contain_prompt(&svec(&["--search"])));
        assert!(!args_contain_prompt(&svec(&[
            "--image", "one.png", "two.png"
        ])));
        assert!(!args_contain_prompt(&svec(&["doctor"])));

        assert!(args_contain_prompt(&svec(&["fix the bug"])));
        assert!(args_contain_prompt(&svec(&[
            "--model",
            "gpt-5",
            "fix the bug"
        ])));
        assert!(args_contain_prompt(&svec(&["--", "after separator"])));

        // `codex resume ...` is an interactive subcommand, not an inline prompt, so the
        // rebound agent is marked Idle instead of being pinned as working.
        assert!(!args_contain_prompt(&svec(&["resume"])));
        assert!(!args_contain_prompt(&svec(&["resume", "sess-1"])));
        assert!(!args_contain_prompt(&svec(&["resume", "--last"])));
        assert!(args_contain_prompt(&svec(&[
            "resume",
            "sess-1",
            "continue here"
        ])));
        assert!(args_contain_prompt(&svec(&[
            "resume",
            "--last",
            "continue here"
        ])));
        assert!(!args_contain_prompt(&svec(&[
            "--model", "gpt-5", "resume", "sess-1"
        ])));
        assert!(!args_contain_prompt(&svec(&["fork", "sess-1"])));
        assert!(args_contain_prompt(&svec(&[
            "fork",
            "sess-1",
            "try another path"
        ])));
        assert!(args_contain_prompt(&svec(&[
            "fork", "sess-1", "--", "-prompt"
        ])));
        assert!(args_contain_prompt(&svec(&["exec"])));
        assert!(args_contain_prompt(&svec(&["review", "--uncommitted"])));
    }

    #[test]
    fn shell_cd_override_drives_agent_workspace_identity() {
        let root = temp_dir();
        let shell = root.join("shell");
        let project = root.join("project");
        fs::create_dir_all(&shell).unwrap();
        fs::create_dir_all(&project).unwrap();

        assert_eq!(
            codex_effective_cwd(&Host::Local, &shell, &[]).unwrap(),
            fs::canonicalize(&shell).unwrap()
        );
        assert_eq!(
            codex_effective_cwd(&Host::Local, &shell, &svec(&["--cd", "../project"])).unwrap(),
            fs::canonicalize(&project).unwrap()
        );
        assert_eq!(
            codex_effective_cwd(
                &Host::Local,
                &shell,
                &svec(&[&format!("--cd={}", project.display())]),
            )
            .unwrap(),
            fs::canonicalize(&project).unwrap()
        );
        assert_eq!(
            codex_effective_cwd(
                &Host::Local,
                &shell,
                &svec(&[&format!("-C{}", project.display())]),
            )
            .unwrap(),
            fs::canonicalize(&project).unwrap()
        );
        assert_eq!(
            codex_effective_cwd(&Host::Local, &shell, &svec(&["--", "--cd", "../project"]),)
                .unwrap(),
            fs::canonicalize(&shell).unwrap()
        );
        assert!(codex_effective_cwd(&Host::Local, &shell, &svec(&["--cd"])).is_err());

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn remote_shell_cd_stays_opaque_to_the_local_filesystem() {
        let host = Host::Remote(crate::host::RemoteTarget {
            id: "remote-1".to_string(),
            label: "builder".to_string(),
            ssh: "builder.example".to_string(),
            qmux_cli: "qmux-cli".to_string(),
            workspace_root: Some("/srv/qmux".to_string()),
            multiplexer: crate::workspace::RemoteMultiplexer::Tmux,
        });
        assert_eq!(
            codex_effective_cwd(
                &host,
                Path::new("/srv/project"),
                &svec(&["--cd", "../other"]),
            )
            .unwrap(),
            PathBuf::from("/srv/project/../other")
        );
        let adapter = CodexAdapter {
            binary: "/opt/remote/bin/codex".to_string(),
        };
        assert!(AgentAdapter::supports_remote(&adapter));
        assert_eq!(
            adapter.binary_for_host(&host).unwrap(),
            "/opt/remote/bin/codex"
        );
        let (codex_home, hook_cli) = adapter.integration_for_host(&host).unwrap();
        assert_eq!(codex_home, None);
        assert_eq!(hook_cli.as_deref(), Some("qmux-cli"));
    }

    #[test]
    fn codex_resume_session_id_reads_the_resumed_session() {
        assert_eq!(
            codex_resume_session_id(&svec(&["resume", "sess-1"])),
            Some("sess-1")
        );
        assert_eq!(
            codex_resume_session_id(&svec(&[
                "--remote",
                "unix:///tmp/codex.sock",
                "resume",
                "--model",
                "gpt-5",
                "sess-2"
            ])),
            Some("sess-2")
        );
        assert_eq!(
            codex_resume_session_id(&svec(&["resume", "--model=gpt-5", "--", "sess-3"])),
            Some("sess-3")
        );
        // Not a resume invocation, or no concrete session id (e.g. `resume --last`).
        assert_eq!(codex_resume_session_id(&svec(&[])), None);
        assert_eq!(codex_resume_session_id(&svec(&["fix the bug"])), None);
        assert_eq!(codex_resume_session_id(&svec(&["resume"])), None);
        assert_eq!(codex_resume_session_id(&svec(&["resume", "--last"])), None);
        assert_eq!(
            codex_resume_session_id(&svec(&["--config", "resume", "actual-prompt"])),
            None
        );
        assert_eq!(
            codex_resume_session_id(&svec(&["--image", "one.png", "resume", "sess-4"])),
            None
        );
        assert_eq!(
            codex_resume_session_id(&svec(&["--", "resume", "prompt-session"])),
            None
        );
        assert_eq!(
            codex_fork_source_session_id(&svec(&["fork", "source-session"])),
            Some("source-session")
        );
        assert_eq!(
            codex_fork_source_session_id(&svec(&["fork", "--last"])),
            None
        );
        assert_eq!(
            codex_fork_source_session_id(&svec(&["resume", "source-session"])),
            None
        );
    }

    #[test]
    fn resume_args_include_session_id_when_present() {
        let options = CodexLaunchOptions::from_value(json!({
            "sandbox": "workspace-write",
            "approvalPolicy": "on-request"
        }))
        .unwrap();

        let (args, resumed) = build_codex_resume_args(
            Path::new("/tmp/qmux"),
            Some(Path::new("/tmp/qmux/.qmux/workspaces")),
            Some("gpt-5"),
            &options,
            None,
            None,
            Some(" session-123 "),
        );

        assert!(resumed);
        assert_eq!(
            args,
            vec![
                "--cd",
                "/tmp/qmux",
                "--add-dir",
                "/tmp/qmux/.qmux/workspaces",
                "--model",
                "gpt-5",
                "--profile",
                "qmux-codex",
                "--sandbox",
                "workspace-write",
                "--ask-for-approval",
                "on-request",
                "--search",
                "resume",
                "session-123"
            ]
        );
    }

    #[test]
    fn fork_args_include_session_id_and_prompt_when_present() {
        let options = CodexLaunchOptions::from_value(json!({
            "sandbox": "workspace-write",
            "approvalPolicy": "on-request"
        }))
        .unwrap();

        let args = build_codex_fork_args(
            Path::new("/tmp/qmux"),
            Some(Path::new("/tmp/qmux/.qmux/workspaces")),
            Some("gpt-5"),
            &options,
            None,
            None,
            " session-123 ",
            Some("  continue here  "),
        );

        assert_eq!(
            args,
            vec![
                "--cd",
                "/tmp/qmux",
                "--add-dir",
                "/tmp/qmux/.qmux/workspaces",
                "--model",
                "gpt-5",
                "--profile",
                "qmux-codex",
                "--sandbox",
                "workspace-write",
                "--ask-for-approval",
                "on-request",
                "--search",
                "fork",
                "session-123",
                "--",
                "continue here"
            ]
        );
    }

    #[test]
    fn fork_args_omit_empty_prompt() {
        let options = CodexLaunchOptions::default();

        let args = build_codex_fork_args(
            Path::new("/tmp/qmux"),
            None,
            None,
            &options,
            None,
            None,
            "session-123",
            Some("   "),
        );

        assert_eq!(
            args,
            vec![
                "--cd",
                "/tmp/qmux",
                "--profile",
                "qmux-codex",
                "--sandbox",
                "workspace-write",
                "--search",
                "fork",
                "session-123"
            ]
        );
    }

    #[test]
    fn resume_args_fall_back_to_fresh_launch_without_session_id() {
        let options = CodexLaunchOptions::default();

        let (args, resumed) = build_codex_resume_args(
            Path::new("/tmp/qmux"),
            None,
            None,
            &options,
            None,
            None,
            Some("   "),
        );

        assert!(!resumed);
        assert_eq!(
            args,
            vec![
                "--cd",
                "/tmp/qmux",
                "--profile",
                "qmux-codex",
                "--sandbox",
                "workspace-write",
                "--search"
            ]
        );
    }

    #[test]
    fn shell_tail_args_reject_incompatible_modes_before_delimiter() {
        let profile_args = vec!["--profile".to_string(), "work".to_string()];
        let inline_profile_args = vec!["--profile=work".to_string()];
        let short_profile_args = vec!["-pwork".to_string()];
        let oss_args = vec!["--oss".to_string()];
        let remote_args = vec!["--remote".to_string(), "unix:///tmp/codex.sock".to_string()];
        let inline_remote_args = vec!["--remote=unix:///tmp/codex.sock".to_string()];
        let disable_hooks_args = vec!["--disable".to_string(), "hooks".to_string()];
        let inline_disable_hooks_args = vec!["--disable=hooks".to_string()];
        let config_hooks_args = vec!["--config".to_string(), "features.hooks=false".to_string()];
        let short_config_hooks_args = vec!["-chooks.SessionStart=[]".to_string()];
        let prompt_args = vec![
            "--".to_string(),
            "--profile".to_string(),
            "work".to_string(),
        ];

        assert!(validate_shell_tail_args(&profile_args).is_err());
        assert!(validate_shell_tail_args(&inline_profile_args).is_err());
        assert!(validate_shell_tail_args(&short_profile_args).is_err());
        assert!(validate_shell_tail_args(&oss_args).is_err());
        assert!(validate_shell_tail_args(&remote_args).is_err());
        assert!(validate_shell_tail_args(&inline_remote_args).is_err());
        assert!(validate_shell_tail_args(&disable_hooks_args).is_err());
        assert!(validate_shell_tail_args(&inline_disable_hooks_args).is_err());
        assert!(validate_shell_tail_args(&config_hooks_args).is_err());
        assert!(validate_shell_tail_args(&short_config_hooks_args).is_err());
        assert!(
            validate_shell_tail_args(&svec(&["--config", "model_reasoning_effort=high"])).is_ok()
        );
        assert!(validate_shell_tail_args(&prompt_args).is_ok());
    }

    #[test]
    fn generated_profile_uses_stable_qmux_shim_and_inline_hooks() {
        let codex_home = temp_dir();
        let qmux_cli = Path::new("/Applications/qmux app/qmux");

        write_codex_integration_files(&codex_home, qmux_cli).unwrap();

        let profile_path = codex_home.join("qmux-codex.config.toml");
        let shim_path = codex_home.join("qmux").join("qmux-codex-hook");
        let profile = fs::read_to_string(profile_path).unwrap();
        let shim = fs::read_to_string(shim_path).unwrap();

        assert!(profile.contains("[features]"));
        assert!(profile.contains("hooks = true"));
        assert!(profile.contains("[[hooks.SessionStart]]"));
        assert!(profile.contains("matcher = \"startup|resume\""));
        for event in CODEX_HOOK_EVENTS {
            assert!(
                profile.contains(&format!("[[hooks.{event}]]")),
                "missing hook group for {event}"
            );
            assert!(
                profile.contains(&format!("qmux-codex-hook' {event}")),
                "missing hook command for {event}"
            );
        }
        assert!(profile.contains("qMux executable: /Applications/qmux app/qmux"));
        assert!(shim.contains("QMUX_SOCK"));
        assert!(shim.contains("exec \"$QMUX_CLI\" notify \"$event\""));
    }

    #[test]
    fn generated_profile_preserves_codex_hook_trust_state() {
        let codex_home = temp_dir();
        let qmux_cli = Path::new("/Applications/qmux app/qmux");
        let profile_path = codex_home.join("qmux-codex.config.toml");

        fs::write(
            &profile_path,
            r#"[features]
hooks = true

[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "command"
command = "'/old/qmux-codex-hook' Stop"
timeout = 5

[hooks.state]

[hooks.state."/Users/raymond/.codex/qmux-codex.config.toml:stop:0:0"]
trusted_hash = "sha256:trusted"
"#,
        )
        .unwrap();

        write_codex_integration_files(&codex_home, qmux_cli).unwrap();

        let profile = fs::read_to_string(profile_path).unwrap();
        assert!(profile.contains("command = \"'/"));
        assert!(profile.contains("qmux-codex-hook' Stop"));
        assert!(profile.contains("[hooks.state]"));
        assert!(profile.contains("trusted_hash = \"sha256:trusted\""));
        assert!(!profile.contains("command = \"'/old/qmux-codex-hook' Stop\""));
    }

    #[test]
    fn composer_policy_queues_running_codex_panes() {
        let policy = CodexAdapter {
            binary: "codex".to_string(),
        }
        .composer_policy();

        assert!(!policy.can_send(AgentStatus::Running));
        assert!(policy.should_queue(AgentStatus::Running));
        assert!(policy.can_steer(AgentStatus::Running));
    }

    #[test]
    fn interactive_codex_attach_marks_agent_idle() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Starting;
        agent.pane_id = None;
        state.insert_agent(agent).unwrap();

        let attached =
            attach_codex_agent_pane(&state, "agent-1", "pane-1".to_string(), false).unwrap();

        assert!(matches!(attached.status, AgentStatus::Idle));
        let stored = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(matches!(stored.status, AgentStatus::Idle));
    }

    #[test]
    fn prompted_codex_attach_marks_agent_running() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Starting;
        agent.pane_id = None;
        state.insert_agent(agent).unwrap();

        let attached =
            attach_codex_agent_pane(&state, "agent-1", "pane-1".to_string(), true).unwrap();

        assert!(matches!(attached.status, AgentStatus::Running));
        let stored = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(matches!(stored.status, AgentStatus::Running));
    }

    #[test]
    fn pre_spawn_fork_attach_promotes_a_validated_fork_session() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Idle;
        agent.pane_id = None;
        agent.parent_id = Some("agent-source".to_string());
        agent.fork_point = Some("source-session".to_string());
        agent.root_session_id = Some("root-session".to_string());
        state.insert_agent(agent).unwrap();

        let attached =
            attach_codex_agent_pane(&state, "agent-1", "pane-1".to_string(), false).unwrap();
        assert_eq!(attached.pane_id.as_deref(), Some("pane-1"));
        assert!(matches!(attached.status, AgentStatus::Idle));
        let transcript_path = temp_dir().join("fork-session.jsonl");
        fs::write(
            &transcript_path,
            r#"{"type":"session_meta","payload":{"id":"fork-session"}}"#,
        )
        .unwrap();

        let event = ingest(
            &state,
            hook_for_agent(
                "SessionStart",
                "agent-1",
                json!({
                    "session_id": "fork-session",
                    "transcript_path": transcript_path.display().to_string()
                }),
            ),
        );

        assert_eq!(event.event_type, "agent.session_start");
        let stored = wait_for_agent_transcript_path(&state, "agent-1", &transcript_path);
        assert_eq!(stored.pane_id.as_deref(), Some("pane-1"));
        assert_eq!(stored.session_id.as_deref(), Some("fork-session"));
        assert_eq!(stored.parent_id.as_deref(), Some("agent-source"));
        assert_eq!(stored.fork_point.as_deref(), Some("source-session"));
        assert_eq!(stored.root_session_id.as_deref(), Some("root-session"));
    }

    #[test]
    fn fork_rejects_stale_startup_identity_then_adopts_child_identity() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Idle;
        agent.fork_point = Some("source-session".to_string());
        agent.root_session_id = Some("source-session".to_string());
        state.insert_agent(agent).unwrap();

        ingest(
            &state,
            hook_for_agent(
                "SessionStart",
                "agent-1",
                json!({ "session_id": "source-session" }),
            ),
        );
        assert_eq!(state.agent("agent-1").unwrap().unwrap().session_id, None);
        let transcript_path = temp_dir().join("child-session.jsonl");
        fs::write(
            &transcript_path,
            r#"{"type":"session_meta","payload":{"id":"child-session"}}"#,
        )
        .unwrap();

        ingest(
            &state,
            hook_for_agent(
                "UserPromptSubmit",
                "agent-1",
                json!({
                    "session_id": "child-session",
                    "transcript_path": transcript_path.display().to_string(),
                    "prompt": "continue from the fork"
                }),
            ),
        );
        let stored = wait_for_agent_transcript_path(&state, "agent-1", &transcript_path);
        assert_eq!(stored.session_id.as_deref(), Some("child-session"));
        assert_eq!(stored.fork_point.as_deref(), Some("source-session"));
    }

    #[test]
    fn session_start_keeps_codex_resource_id_provisional_without_a_rollout() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Starting;
        state.insert_agent(agent).unwrap();

        let event = ingest(
            &state,
            hook_for_agent(
                "SessionStart",
                "agent-1",
                json!({ "resource_id": "codex-session-1" }),
            ),
        );

        assert_eq!(event.event_type, "agent.session_start");
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(agent.session_id, None);
        // SessionStart neither commits an unverified identity nor promotes status:
        // the agent remains Starting until a real prompt/tool hook lands.
        assert!(matches!(agent.status, AgentStatus::Starting));
    }

    #[test]
    fn session_start_preserves_interactive_codex_status() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::AwaitingInput;
        state.insert_agent(agent).unwrap();

        let event = ingest(
            &state,
            hook_for_agent(
                "SessionStart",
                "agent-1",
                json!({ "resource_id": "codex-session-1" }),
            ),
        );

        assert_eq!(event.event_type, "agent.session_start");
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(agent.session_id, None);
        assert!(matches!(agent.status, AgentStatus::AwaitingInput));
    }

    #[test]
    fn session_start_without_resource_id_keeps_a_recorded_one() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Starting;
        let transcript_path = temp_dir().join("codex-session.jsonl");
        fs::write(
            &transcript_path,
            r#"{"type":"session_meta","payload":{"id":"codex-session-1"}}"#,
        )
        .unwrap();
        agent.session_id = Some("codex-session-1".to_string());
        agent.transcript_path = Some(transcript_path.display().to_string());
        state.insert_agent(agent).unwrap();

        // A late/duplicate SessionStart that omits the id must not blank it.
        ingest(&state, hook_for_agent("SessionStart", "agent-1", json!({})));
        assert_eq!(
            state
                .agent("agent-1")
                .unwrap()
                .unwrap()
                .session_id
                .as_deref(),
            Some("codex-session-1")
        );
    }

    #[test]
    fn session_start_binds_explicit_codex_transcript_path() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Starting;
        state.insert_agent(agent).unwrap();
        let transcript_path = temp_dir().join("codex-session.jsonl");
        fs::write(
            &transcript_path,
            r#"{"type":"session_meta","payload":{"id":"019eeca7-d820-7b91-b1e8-9c954fb1a105"}}"#,
        )
        .unwrap();

        let event = ingest(
            &state,
            hook_for_agent(
                "SessionStart",
                "agent-1",
                json!({
                    "resource_id": "019eeca7-d820-7b91-b1e8-9c954fb1a105",
                    "transcript_path": transcript_path.display().to_string()
                }),
            ),
        );

        assert_eq!(event.event_type, "agent.session_start");
        let agent = wait_for_agent_transcript_path(&state, "agent-1", &transcript_path);
        assert_eq!(
            agent.session_id.as_deref(),
            Some("019eeca7-d820-7b91-b1e8-9c954fb1a105")
        );
        assert_eq!(
            agent.transcript_path.as_deref(),
            Some(transcript_path.to_str().unwrap())
        );
    }

    #[test]
    fn explicit_codex_transcript_path_treats_session_mismatch_as_not_ready() {
        let transcript_path = temp_dir().join("codex-session.jsonl");
        fs::write(
            &transcript_path,
            r#"{"type":"session_meta","payload":{"id":"019eeca7-d820-7b91-b1e8-9c954fb1a105"}}"#,
        )
        .unwrap();

        // A path whose first line currently names a different session is treated as
        // "not ready yet" (it may be a stale/rotated rollout or still mid-write), so
        // the binding loop keeps polling rather than aborting permanently.
        let ready = codex_transcript_path_ready(
            &transcript_path,
            Some("029eeca7-d820-7b91-b1e8-9c954fb1a105"),
        )
        .unwrap();

        assert!(!ready);
    }

    #[test]
    fn codex_binding_continues_only_for_the_current_candidate() {
        let state = test_state();
        state.insert_agent(sample_agent()).unwrap();
        let first = state
            .begin_transcript_binding_candidate("agent-1", Some("first"), None)
            .unwrap()
            .unwrap();

        assert!(codex_binding_should_continue(&state, "agent-1", first));

        let second = state
            .begin_transcript_binding_candidate("agent-1", Some("second"), None)
            .unwrap()
            .unwrap();
        assert!(!codex_binding_should_continue(&state, "agent-1", first));
        assert!(codex_binding_should_continue(&state, "agent-1", second));
    }

    #[test]
    fn repeated_codex_candidate_does_not_discard_its_explicit_path() {
        let state = test_state();
        state.insert_agent(sample_agent()).unwrap();
        let generation = state
            .begin_transcript_binding_candidate(
                "agent-1",
                Some("child-session"),
                Some("/tmp/child-session.jsonl"),
            )
            .unwrap()
            .unwrap();

        let repeated = state
            .begin_transcript_binding_candidate("agent-1", Some("child-session"), None)
            .unwrap();

        assert_eq!(repeated, None);
        assert!(codex_binding_should_continue(&state, "agent-1", generation));
    }

    #[test]
    fn codex_binding_stops_when_agent_is_gone() {
        let state = test_state();
        let generation = state
            .begin_transcript_binding_candidate("missing", Some("session"), None)
            .unwrap()
            .unwrap();

        assert!(!codex_binding_should_continue(
            &state, "missing", generation
        ));
    }

    #[test]
    fn codex_candidate_can_be_validated_while_an_old_transcript_stays_bound() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.transcript_path = Some("/tmp/session.jsonl".to_string());
        state.insert_agent(agent).unwrap();
        let generation = state
            .begin_transcript_binding_candidate("agent-1", Some("new-session"), None)
            .unwrap()
            .unwrap();

        assert!(
            codex_binding_should_continue(&state, "agent-1", generation),
            "the old canonical binding must remain while the candidate is validated"
        );
    }

    #[test]
    fn side_conversation_candidate_cannot_split_canonical_identity() {
        let state = test_state();
        let mut agent = sample_agent();
        let transcript_path = temp_dir().join("canonical.jsonl");
        fs::write(
            &transcript_path,
            r#"{"type":"session_meta","payload":{"id":"canonical-session"}}"#,
        )
        .unwrap();
        agent.session_id = Some("canonical-session".to_string());
        agent.transcript_path = Some(transcript_path.display().to_string());
        state.insert_agent(agent).unwrap();
        let generation = state
            .begin_transcript_binding_candidate(
                "agent-1",
                Some("side-conversation"),
                Some(transcript_path.to_str().unwrap()),
            )
            .unwrap()
            .unwrap();

        let error = bind_codex_transcript_path(
            &state,
            "agent-1",
            generation,
            Some("side-conversation"),
            &transcript_path,
        )
        .unwrap_err();

        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(error.contains("canonical-session"));
        assert_eq!(
            agent.transcript_path.as_deref(),
            Some(transcript_path.to_str().unwrap())
        );
        assert_eq!(
            agent.session_id.as_deref(),
            Some("canonical-session"),
            "a candidate without a matching rollout must not overwrite the root identity"
        );
    }

    #[test]
    fn validated_codex_candidate_replaces_both_canonical_fields() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.session_id = Some("old-session".to_string());
        agent.transcript_path = Some("/tmp/old-session.jsonl".to_string());
        state.insert_agent(agent).unwrap();
        let transcript_path = temp_dir().join("new-session.jsonl");
        fs::write(
            &transcript_path,
            r#"{"type":"session_meta","payload":{"id":"new-session"}}"#,
        )
        .unwrap();
        let generation = state
            .begin_transcript_binding_candidate(
                "agent-1",
                Some("new-session"),
                Some(transcript_path.to_str().unwrap()),
            )
            .unwrap()
            .unwrap();

        bind_codex_transcript_path(
            &state,
            "agent-1",
            generation,
            Some("new-session"),
            &transcript_path,
        )
        .unwrap();

        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(agent.session_id.as_deref(), Some("new-session"));
        assert_eq!(
            agent.transcript_path.as_deref(),
            Some(transcript_path.to_str().unwrap())
        );
    }

    #[test]
    fn reconcile_codex_identity_repairs_a_legacy_hybrid() {
        let state = test_state();
        let transcript_path = temp_dir().join("canonical.jsonl");
        fs::write(
            &transcript_path,
            r#"{"type":"session_meta","payload":{"id":"canonical-session"}}"#,
        )
        .unwrap();
        let mut agent = sample_agent();
        agent.session_id = Some("side-conversation".to_string());
        agent.transcript_path = Some(transcript_path.display().to_string());
        state.insert_agent(agent.clone()).unwrap();

        let repaired = reconcile_codex_agent_identity(&state, &agent).unwrap();

        assert_eq!(repaired.session_id.as_deref(), Some("canonical-session"));
        assert_eq!(
            state
                .agent("agent-1")
                .unwrap()
                .unwrap()
                .session_id
                .as_deref(),
            Some("canonical-session")
        );
    }

    #[test]
    fn superseded_codex_candidate_cannot_commit_late() {
        let state = test_state();
        state.insert_agent(sample_agent()).unwrap();
        let old_path = temp_dir().join("old-candidate.jsonl");
        fs::write(
            &old_path,
            r#"{"type":"session_meta","payload":{"id":"old-candidate"}}"#,
        )
        .unwrap();
        let old_generation = state
            .begin_transcript_binding_candidate(
                "agent-1",
                Some("old-candidate"),
                Some(old_path.to_str().unwrap()),
            )
            .unwrap()
            .unwrap();
        state
            .begin_transcript_binding_candidate("agent-1", Some("new-candidate"), None)
            .unwrap()
            .unwrap();

        bind_codex_transcript_path(
            &state,
            "agent-1",
            old_generation,
            Some("old-candidate"),
            &old_path,
        )
        .unwrap();

        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(agent.session_id, None);
        assert_eq!(agent.transcript_path, None);
    }

    #[test]
    fn explicit_codex_transcript_binding_retries_until_file_appears() {
        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Starting;
        state.insert_agent(agent).unwrap();
        let transcript_path = temp_dir().join("codex-late.jsonl");
        let session_id = "019eeca7-d820-7b91-b1e8-9c954fb1a105";
        let path_for_writer = transcript_path.clone();
        let sid_for_writer = session_id.to_string();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(400));
            fs::write(
                &path_for_writer,
                format!(r#"{{"type":"session_meta","payload":{{"id":"{sid_for_writer}"}}}}"#),
            )
            .unwrap();
        });

        start_codex_transcript_binding(
            state.clone(),
            "agent-1".to_string(),
            Some(session_id.to_string()),
            Some(transcript_path.display().to_string()),
        );

        let agent = wait_for_agent_transcript_path(&state, "agent-1", &transcript_path);
        assert_eq!(agent.session_id.as_deref(), Some(session_id));
    }

    #[test]
    fn codex_discovery_binds_when_file_appears_late() {
        let codex_home = temp_dir();
        let session_id = "019eeca7-d820-7b91-b1e8-9c954fb1a105";
        let session_dir = codex_home
            .join("sessions")
            .join("2026")
            .join("06")
            .join("21");
        fs::create_dir_all(&session_dir).unwrap();
        let transcript_path =
            session_dir.join(format!("rollout-2026-06-21T20-08-03-{session_id}.jsonl"));
        let path_for_writer = transcript_path.clone();
        let sid_for_writer = session_id.to_string();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(400));
            fs::write(
                &path_for_writer,
                format!(r#"{{"type":"session_meta","payload":{{"id":"{sid_for_writer}"}}}}"#),
            )
            .unwrap();
        });

        let state = test_state();
        let mut agent = sample_agent();
        agent.status = AgentStatus::Starting;
        agent.session_id = Some(session_id.to_string());
        state.insert_agent(agent).unwrap();

        let prev = env::var_os("CODEX_HOME");
        unsafe {
            env::set_var("CODEX_HOME", &codex_home);
        }

        start_codex_transcript_binding(
            state.clone(),
            "agent-1".to_string(),
            Some(session_id.to_string()),
            None,
        );

        let agent = wait_for_agent_transcript_path(&state, "agent-1", &transcript_path);
        assert_eq!(agent.session_id.as_deref(), Some(session_id));

        unsafe {
            match prev {
                Some(val) => env::set_var("CODEX_HOME", val),
                None => env::remove_var("CODEX_HOME"),
            }
        }
    }

    #[test]
    fn codex_discovery_skips_when_transcript_already_bound() {
        let state = test_state();
        let existing_path = temp_dir().join("existing.jsonl");
        let mut agent = sample_agent();
        agent.transcript_path = Some(existing_path.display().to_string());
        state.insert_agent(agent).unwrap();

        start_codex_transcript_binding(
            state.clone(),
            "agent-1".to_string(),
            Some("019eeca7-d820-7b91-b1e8-9c954fb1a105".to_string()),
            None,
        );

        thread::sleep(Duration::from_millis(300));

        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert_eq!(
            agent.transcript_path.as_deref(),
            Some(existing_path.to_str().unwrap()),
            "transcript_path should not be overridden by discovery when already bound"
        );
    }

    #[test]
    fn codex_transcript_discovery_matches_session_meta_id() {
        let codex_home = temp_dir();
        let session_id = "019eeca7-d820-7b91-b1e8-9c954fb1a105";
        let session_dir = codex_home
            .join("sessions")
            .join("2026")
            .join("06")
            .join("21");
        fs::create_dir_all(&session_dir).unwrap();
        let matching = session_dir.join("rollout-2026-06-21T20-08-03-short-id.jsonl");
        let wrong = session_dir.join(format!("rollout-2026-06-21T20-08-04-{session_id}.jsonl"));
        fs::write(
            &matching,
            format!(r#"{{"type":"session_meta","payload":{{"id":"{session_id}"}}}}"#),
        )
        .unwrap();
        fs::write(
            &wrong,
            r#"{"type":"session_meta","payload":{"id":"not-the-session"}}"#,
        )
        .unwrap();

        let found = find_codex_transcript_path(&codex_home, session_id)
            .unwrap()
            .expect("matching transcript found");

        assert_eq!(found, matching);
    }

    #[test]
    fn parse_codex_message_response_items() {
        let user_line = json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "fix the bug" }]
            }
        })
        .to_string();
        let assistant_line = json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "Done." }]
            }
        })
        .to_string();

        let user = parse_transcript_line("agent-1", 3, &user_line).expect("user turn");
        let assistant =
            parse_transcript_line("agent-1", 4, &assistant_line).expect("assistant turn");

        assert_eq!(user.role, "user");
        assert_eq!(assistant.role, "assistant");
        assert_text_block(&user.blocks[0], "fix the bug");
        assert_text_block(&assistant.blocks[0], "Done.");
    }

    #[test]
    fn parse_codex_message_reassembles_clipboard_image_markers() {
        let line = json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [
                    {
                        "type": "input_text",
                        "text": "<image name=[Image #1] path=\"/var/folders/example/T/codex-clipboard.png\">"
                    },
                    { "type": "input_image", "image_url": "data:image/png;base64,AAAA" },
                    { "type": "input_text", "text": "</image>" },
                    { "type": "input_text", "text": "Please inspect this image [Image #1]" }
                ]
            }
        })
        .to_string();

        let turn = parse_transcript_line("agent-1", 3, &line).expect("user turn");

        assert_eq!(turn.blocks.len(), 2);
        assert_text_block(
            &turn.blocks[0],
            "<image name=[Image #1] path=\"/var/folders/example/T/codex-clipboard.png\">\n</image>",
        );
        assert_text_block(&turn.blocks[1], "Please inspect this image [Image #1]");
    }

    #[test]
    fn parse_codex_tool_call_and_result_response_items() {
        let call_line = json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "exec_command",
                "call_id": "call-1",
                "arguments": "{\"cmd\":\"npm test\"}"
            }
        })
        .to_string();
        let result_line = json!({
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "call-1",
                "output": "ok"
            }
        })
        .to_string();

        let call = parse_transcript_line("agent-1", 5, &call_line).expect("tool call");
        let result = parse_transcript_line("agent-1", 6, &result_line).expect("tool result");

        assert_eq!(call.role, "assistant");
        match &call.blocks[0] {
            TurnBlock::ToolUse { id, name, input } => {
                assert_eq!(id.as_deref(), Some("call-1"));
                assert_eq!(name, "exec_command");
                assert_eq!(input["cmd"], "npm test");
            }
            other => panic!("unexpected block: {other:?}"),
        }
        match &result.blocks[0] {
            TurnBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id.as_deref(), Some("call-1"));
                assert_eq!(content, "ok");
                assert!(!is_error);
            }
            other => panic!("unexpected block: {other:?}"),
        }
    }

    #[test]
    fn resolve_codex_transcript_keeps_rolled_back_interrupted_turn_visible() {
        let lines = vec![
            codex_task_started_line("turn-1"),
            codex_user_message_line("turn-1", "typo"),
            codex_assistant_message_line("turn-1", "partial"),
            codex_turn_aborted_line("turn-1"),
            json!({
                "type": "event_msg",
                "payload": { "type": "thread_rolled_back", "num_turns": 1 }
            })
            .to_string(),
            codex_task_started_line("turn-2"),
            codex_user_message_line("turn-2", "corrected"),
            codex_assistant_message_line("turn-2", "final"),
        ];

        let turns = resolve_transcript_turns("agent-1", &lines);

        assert_eq!(turns.len(), 4);
        assert!(turns[0..2].iter().all(|turn| {
            turn.status == Some(TurnStatus::Interrupted)
                && turn.status_reason == Some(TurnStatusReason::Interrupted)
                && turn.context_status == Some(TurnContextStatus::RolledBack)
        }));
        assert!(
            turns[2..]
                .iter()
                .all(|turn| turn.status.is_none() && turn.context_status.is_none())
        );
    }

    #[test]
    fn resolve_codex_transcript_marks_abort_without_rollback_interrupted() {
        let lines = vec![
            codex_task_started_line("turn-1"),
            codex_user_message_line("turn-1", "prompt"),
            codex_assistant_message_line("turn-1", "partial"),
            codex_turn_aborted_line("turn-1"),
        ];

        let turns = resolve_transcript_turns("agent-1", &lines);

        assert_eq!(turns.len(), 2);
        assert!(turns.iter().all(|turn| {
            turn.status == Some(TurnStatus::Interrupted)
                && turn.status_reason == Some(TurnStatusReason::Interrupted)
                && turn.context_status.is_none()
        }));
    }

    #[test]
    fn resolve_codex_transcript_marks_rollback_without_changing_outcome() {
        let lines = vec![
            codex_task_started_line("turn-1"),
            codex_user_message_line("turn-1", "prompt"),
            codex_assistant_message_line("turn-1", "answer"),
            json!({
                "type": "event_msg",
                "payload": { "type": "thread_rolled_back", "num_turns": 1 }
            })
            .to_string(),
        ];

        let turns = resolve_transcript_turns("agent-1", &lines);

        assert_eq!(turns.len(), 2);
        assert!(turns.iter().all(|turn| {
            turn.status.is_none()
                && turn.status_reason.is_none()
                && turn.context_status == Some(TurnContextStatus::RolledBack)
        }));
    }

    #[test]
    fn resolve_codex_transcript_uses_task_boundary_when_items_lack_turn_metadata() {
        let lines = vec![
            codex_task_started_line("turn-1"),
            json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "prompt" }]
                }
            })
            .to_string(),
            json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "answer" }]
                }
            })
            .to_string(),
            json!({
                "type": "event_msg",
                "payload": { "type": "thread_rolled_back", "num_turns": 1 }
            })
            .to_string(),
        ];

        let turns = resolve_transcript_turns("agent-1", &lines);

        assert_eq!(turns.len(), 2);
        assert!(turns.iter().all(|turn| {
            turn.native_id.is_none() && turn.context_status == Some(TurnContextStatus::RolledBack)
        }));
    }

    #[test]
    fn resolve_codex_transcript_does_not_treat_same_turn_steer_as_rollback() {
        let lines = vec![
            codex_task_started_line("turn-1"),
            codex_user_message_line("turn-1", "original"),
            codex_assistant_message_line("turn-1", "first part"),
            codex_user_message_line("turn-1", "steer"),
            codex_assistant_message_line("turn-1", "continued"),
        ];

        let turns = resolve_transcript_turns("agent-1", &lines);

        assert_eq!(turns.len(), 4);
        assert!(turns.iter().all(|turn| {
            turn.status.is_none() && turn.status_reason.is_none() && turn.context_status.is_none()
        }));
    }

    #[test]
    fn resolve_codex_transcript_rolls_back_newer_non_user_segments_too() {
        let lines = vec![
            codex_task_started_line("turn-0"),
            codex_user_message_line("turn-0", "real prompt"),
            codex_assistant_message_line("turn-0", "real answer"),
            codex_task_started_line("turn-1"),
            json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{
                        "type": "input_text",
                        "text": "<environment_context>\ninternal\n</environment_context>"
                    }],
                    "internal_chat_message_metadata_passthrough": { "turn_id": "turn-1" }
                }
            })
            .to_string(),
            json!({
                "type": "event_msg",
                "payload": { "type": "thread_rolled_back", "num_turns": 1 }
            })
            .to_string(),
        ];

        let turns = resolve_transcript_turns("agent-1", &lines);

        assert_eq!(turns.len(), 3);
        assert!(
            turns
                .iter()
                .all(|turn| { turn.context_status == Some(TurnContextStatus::RolledBack) })
        );
    }

    #[test]
    fn resolve_codex_transcript_counts_arbitrary_tagged_prompts_as_user_turns() {
        let lines = vec![
            codex_task_started_line("turn-0"),
            codex_user_message_line("turn-0", "older prompt"),
            codex_assistant_message_line("turn-0", "older answer"),
            codex_task_started_line("turn-1"),
            codex_user_message_line("turn-1", "<request>\nnew prompt\n</request>"),
            codex_assistant_message_line("turn-1", "new answer"),
            json!({
                "type": "event_msg",
                "payload": { "type": "thread_rolled_back", "num_turns": 1 }
            })
            .to_string(),
        ];

        let turns = resolve_transcript_turns("agent-1", &lines);

        assert_eq!(turns.len(), 4);
        assert!(turns[0..2].iter().all(|turn| turn.context_status.is_none()));
        assert!(
            turns[2..]
                .iter()
                .all(|turn| { turn.context_status == Some(TurnContextStatus::RolledBack) })
        );
    }

    #[test]
    fn resolve_codex_transcript_does_not_guess_for_unscoped_records() {
        let lines = vec![
            json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "visible tail" }]
                }
            })
            .to_string(),
            json!({
                "type": "event_msg",
                "payload": { "type": "thread_rolled_back", "num_turns": 1 }
            })
            .to_string(),
        ];

        let turns = resolve_transcript_turns("agent-1", &lines);

        assert_eq!(turns.len(), 1);
        assert!(turns[0].context_status.is_none());
    }

    #[test]
    fn bounded_codex_resolution_preserves_absolute_source_indices() {
        let lines = vec![
            codex_task_started_line("turn-1"),
            codex_user_message_line("turn-1", "prompt"),
        ];

        let turns = resolve_transcript_turns_from("agent-1", 500, &lines);

        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].source_index, 501);
        assert_eq!(turns[0].id, "agent-1-501");
    }

    #[test]
    fn resolve_codex_transcript_expands_compacted_record() {
        let lines = vec![
            json!({
                "type": "compacted",
                "timestamp": "2026-08-18T21:17:36.289Z",
                "payload": {
                    "replacement_history": [
                        {
                            "type": "message",
                            "id": "msg-turn-1",
                            "role": "user",
                            "content": [{ "type": "input_text", "text": "hello" }],
                            "internal_chat_message_metadata_passthrough": {
                                "turn_id": "turn-1"
                            }
                        },
                        {
                            "type": "message",
                            "id": "msg-turn-2",
                            "role": "assistant",
                            "content": [{ "type": "output_text", "text": "hi there" }],
                            "internal_chat_message_metadata_passthrough": {
                                "turn_id": "turn-2"
                            }
                        },
                        {
                            "type": "compaction",
                            "id": "cmp-1"
                        }
                    ]
                }
            })
            .to_string(),
        ];

        let turns = resolve_transcript_turns("agent-1", &lines);

        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].source_index, 1_000_000);
        assert_eq!(turns[0].native_id.as_deref(), Some("turn-1"));
        assert_text_block(&turns[0].blocks[0], "hello");
        assert_eq!(turns[1].role, "assistant");
        assert_eq!(turns[1].source_index, 1_000_001);
        assert_eq!(turns[1].native_id.as_deref(), Some("turn-2"));
        assert_text_block(&turns[1].blocks[0], "hi there");
    }

    #[test]
    fn resolve_codex_transcript_deduplicates_compacted_messages() {
        let user_message = json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "id": "msg-turn-1",
                "role": "user",
                "content": [{ "type": "input_text", "text": "hello" }],
                "internal_chat_message_metadata_passthrough": {
                    "turn_id": "turn-1"
                }
            }
        })
        .to_string();
        let compacted = json!({
            "type": "compacted",
            "payload": {
                "replacement_history": [
                    {
                        "type": "message",
                        "id": "msg-turn-1",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "hello again" }],
                        "internal_chat_message_metadata_passthrough": {
                            "turn_id": "turn-1"
                        }
                    },
                    {
                        "type": "message",
                        "id": "msg-turn-2",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "hi" }],
                        "internal_chat_message_metadata_passthrough": {
                            "turn_id": "turn-2"
                        }
                    }
                ]
            }
        })
        .to_string();

        let turns = resolve_transcript_turns("agent-1", &[user_message, compacted]);

        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].source_index, 0);
        assert_text_block(&turns[0].blocks[0], "hello");
        assert_eq!(turns[1].source_index, 2_000_001);
        assert_text_block(&turns[1].blocks[0], "hi");
    }

    #[test]
    fn parse_codex_transcript_skips_duplicates_and_private_context() {
        let event_line = json!({
            "type": "event_msg",
            "payload": { "type": "user_message", "message": "fix the bug" }
        })
        .to_string();
        let developer_line = json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "developer",
                "content": [{ "type": "input_text", "text": "hidden" }]
            }
        })
        .to_string();
        let reasoning_line = json!({
            "type": "response_item",
            "payload": { "type": "reasoning", "summary": [] }
        })
        .to_string();

        assert!(parse_transcript_line("agent-1", 1, &event_line).is_none());
        assert!(parse_transcript_line("agent-1", 2, &developer_line).is_none());
        assert!(parse_transcript_line("agent-1", 3, &reasoning_line).is_none());
    }

    #[test]
    fn parse_codex_turn_aborted_lifecycle_event() {
        let abort_line = json!({
            "type": "event_msg",
            "payload": {
                "type": "turn_aborted",
                "turn_id": "turn-1",
                "reason": "interrupted"
            }
        })
        .to_string();
        let user_message_line = json!({
            "type": "event_msg",
            "payload": { "type": "user_message", "message": "fix the bug" }
        })
        .to_string();
        let task_started_line = json!({
            "type": "event_msg",
            "payload": { "type": "task_started", "turn_id": "turn-2" }
        })
        .to_string();
        let task_complete_line = json!({
            "type": "event_msg",
            "payload": { "type": "task_complete", "turn_id": "turn-2" }
        })
        .to_string();

        assert_eq!(
            parse_transcript_lifecycle_event(&abort_line),
            Some(TranscriptLifecycleEvent::Interrupted)
        );
        assert_eq!(
            parse_transcript_lifecycle_event(&task_started_line),
            Some(TranscriptLifecycleEvent::TurnStarted)
        );
        assert_eq!(
            parse_transcript_lifecycle_event(&task_complete_line),
            Some(TranscriptLifecycleEvent::TurnCompleted)
        );
        assert_eq!(parse_transcript_lifecycle_event(&user_message_line), None);
    }

    #[test]
    fn permission_request_marks_codex_awaiting_permission() {
        let state = test_state();
        state.insert_agent(sample_agent()).unwrap();

        let event = ingest(
            &state,
            hook_for_agent(
                "PermissionRequest",
                "agent-1",
                json!({ "tool_name": "Bash" }),
            ),
        );

        assert_eq!(event.event_type, "agent.awaiting_permission");
        let agent = state.agent("agent-1").unwrap().expect("agent exists");
        assert!(matches!(agent.status, AgentStatus::AwaitingPermission));
    }

    #[test]
    fn compaction_and_subagent_hooks_preserve_parent_activity() {
        let state = test_state();
        install_agent_pane(&state);

        state
            .set_agent_status("agent-1", AgentStatus::AwaitingInput)
            .unwrap();
        let event = ingest(&state, hook_for_agent("PreCompact", "agent-1", json!({})));
        assert_eq!(event.event_type, "agent.compacting");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        ));

        let event = ingest(&state, hook_for_agent("PostCompact", "agent-1", json!({})));
        assert_eq!(event.event_type, "agent.compacted");

        let event = ingest(
            &state,
            hook_for_agent("SubagentStart", "agent-1", json!({ "agent_id": "child-1" })),
        );
        assert_eq!(event.event_type, "agent.subagent_started");
        assert!(state.agent_has_active_subagents("agent-1").unwrap());
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        ));

        let event = ingest(&state, hook_for_agent("Stop", "agent-1", json!({})));
        assert_eq!(event.event_type, "agent.running");

        let event = ingest(
            &state,
            hook_for_agent("SubagentStop", "agent-1", json!({ "agent_id": "child-1" })),
        );
        assert_eq!(event.event_type, "agent.subagent_stopped");
        assert!(!state.agent_has_active_subagents("agent-1").unwrap());
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        ));

        let event = ingest(&state, hook_for_agent("Stop", "agent-1", json!({})));
        assert_eq!(event.event_type, "agent.stop_observed");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        ));
    }

    #[test]
    fn stop_does_not_settle_or_drain_a_codex_queue() {
        let state = test_state();
        let bytes = install_agent_pane(&state);
        state
            .enqueue_agent_turn("agent-1", "after synthesis".to_string())
            .unwrap();

        let event = ingest(&state, hook_for_agent("Stop", "agent-1", json!({})));
        assert_eq!(event.event_type, "agent.stop_observed");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        ));
        assert_eq!(
            state.list_agent_turn_queue("agent-1").unwrap(),
            vec!["after synthesis".to_string()]
        );
        assert!(bytes.lock().unwrap().is_empty());

        ingest(
            &state,
            hook_for_agent("SubagentStart", "agent-1", json!({ "agent_id": "child-1" })),
        );
        let event = ingest(&state, hook_for_agent("Stop", "agent-1", json!({})));
        assert_eq!(event.event_type, "agent.running");
        ingest(
            &state,
            hook_for_agent("SubagentStop", "agent-1", json!({ "agent_id": "child-1" })),
        );
        let event = ingest(&state, hook_for_agent("Stop", "agent-1", json!({})));
        assert_eq!(event.event_type, "agent.stop_observed");
        assert_eq!(
            state.list_agent_turn_queue("agent-1").unwrap(),
            vec!["after synthesis".to_string()]
        );
        assert!(bytes.lock().unwrap().is_empty());
    }

    #[test]
    fn stop_does_not_release_codex_waiters() {
        let state = test_state();
        install_agent_pane(&state);
        let mut source = sample_agent();
        source.id = "agent-2".to_string();
        source.pane_id = Some("pane-2".to_string());
        source.status = AgentStatus::Done;
        state.insert_agent(source).unwrap();
        state
            .enqueue_agent_wait_turn_with_target_label(
                "agent-2",
                "after target".to_string(),
                "agent-1",
                Some("pane-1"),
                Some("Codex"),
            )
            .unwrap();

        let event = ingest(&state, hook_for_agent("Stop", "agent-1", json!({})));
        assert_eq!(event.event_type, "agent.stop_observed");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        ));
        assert!(state.pop_ready_agent_turn("agent-2").unwrap().is_none());
    }

    #[test]
    fn shell_escape_prompt_submit_preserves_ready_codex_status() {
        let state = test_state();
        state.insert_agent(sample_agent()).unwrap();
        state
            .set_agent_status("agent-1", AgentStatus::Done)
            .unwrap();
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
    fn remote_hooks_record_session_identity_and_settle_without_local_transcript_io() {
        let state = test_state();
        install_remote_agent_pane(&state);

        let started = ingest(
            &state,
            hook_for_agent(
                "SessionStart",
                "agent-1",
                json!({
                    "session_id": "remote-session-1",
                    "transcript_path": "/home/remote/.codex/sessions/rollout.jsonl"
                }),
            ),
        );
        assert_eq!(started.event_type, "agent.session_start");
        let agent = state.agent("agent-1").unwrap().unwrap();
        assert_eq!(agent.session_id.as_deref(), Some("remote-session-1"));
        assert_eq!(agent.transcript_path, None);

        let stopped = ingest(&state, hook_for_agent("Stop", "agent-1", json!({})));
        assert_eq!(stopped.event_type, "agent.awaiting_input");
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::AwaitingInput
        ));
    }

    fn test_state() -> AppState {
        AppState::new(QmuxConfig {
            remotes: Default::default(),
            workspace_root: temp_dir(),
            socket_path: PathBuf::from("/tmp/qmux-codex-test.sock"),
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
                antigravity: Default::default(),
            },
            legacy_claude_binary: None,
            claude_plugin_dir: PathBuf::new(),
            opencode_plugin_dir: PathBuf::new(),
            pi_extension_dir: PathBuf::new(),
            cursor_plugin_dir: PathBuf::new(),
        })
    }

    #[test]
    fn anchored_fork_resumes_the_seed_instead_of_forking_again() {
        let mut source = sample_agent();
        source.session_id = Some("live-session".to_string());
        source.model = Some("gpt-5".to_string());
        let state = test_state();
        let adapter = CodexAdapter::new(state.config());

        let anchored = adapter.shell_fork_at_message_args(&source, "seed-session", Some("retry"));
        let head = adapter
            .shell_fork_args(&source, Path::new("/tmp"), Some("retry"))
            .unwrap();

        // The seed rollout already ends at the fork point, so continue it with
        // `resume`; `fork` would branch it again and strand the seed.
        assert!(anchored.contains(&"resume".to_string()));
        assert!(!anchored.contains(&"fork".to_string()));
        assert!(anchored.contains(&"seed-session".to_string()));
        assert!(!anchored.contains(&"live-session".to_string()));

        assert!(head.contains(&"fork".to_string()));
        assert!(head.contains(&"live-session".to_string()));

        for args in [&anchored, &head] {
            let delimiter = args.iter().position(|arg| arg == "--").unwrap();
            assert_eq!(args[delimiter + 1], "retry");
        }
        assert!(anchored.windows(2).any(|pair| pair == ["--model", "gpt-5"]));
    }

    fn sample_agent() -> AgentInfo {
        AgentInfo {
            id: "agent-1".to_string(),
            group_id: "group-1".to_string(),
            adapter: "codex".to_string(),
            worktree_dir: "/tmp/qmux-codex-test".to_string(),
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
                    title: "Codex".to_string(),
                    last_osc_title: None,
                    kind: PaneKind::Agent,
                    agent_id: Some("agent-1".to_string()),
                    group_id: "group-1".to_string(),
                    cwd: "/tmp/qmux-codex-test".to_string(),
                    active_workspace: None,
                    remote_session: None,
                    remote_connection: None,
                    cols: 80,
                    rows: 24,
                    status: PaneStatus::Running,
                    last_active_at: 0,
                    recovered: false,
                    ssh_target: None,
                    depth: 0,
                },
                backend: crate::state::PaneBackend::HostPty(crate::state::HostPtyBackend {
                    child: Arc::new(Mutex::new(Box::new(FakeChild))),
                    master: Arc::new(Mutex::new(pair.master)),
                    writer: Arc::new(Mutex::new(Box::new(RecordingWriter {
                        bytes: bytes.clone(),
                    }))),
                    backlog: Default::default(),
                    native_surface: false,
                }),
                cwd_observation_seq: 0,
            })
            .unwrap();
        bytes
    }

    fn install_remote_agent_pane(state: &AppState) {
        state.insert_agent(sample_agent()).unwrap();
        state
            .insert_pane(PaneRuntime {
                info: PaneInfo {
                    id: "pane-1".to_string(),
                    title: "Codex".to_string(),
                    last_osc_title: None,
                    kind: PaneKind::Agent,
                    agent_id: Some("agent-1".to_string()),
                    group_id: "group-1".to_string(),
                    cwd: "/srv/qmux/project".to_string(),
                    active_workspace: None,
                    remote_session: Some(
                        crate::state::RemoteSessionIdentity::new("remote-1", "pane-1").unwrap(),
                    ),
                    remote_connection: Some(crate::state::RemoteConnectionInfo::default()),
                    cols: 80,
                    rows: 24,
                    status: PaneStatus::Running,
                    last_active_at: 0,
                    recovered: false,
                    ssh_target: None,
                    depth: 0,
                },
                backend: crate::state::PaneBackend::RemoteTmux(
                    crate::state::RemoteTmuxBackend::new(
                        crate::remote_terminal::RemoteAttachmentController::new(),
                        crate::remote_terminal::RemoteHistoryCheckpoint::new(Vec::new()),
                        Arc::new(Mutex::new(Default::default())),
                        crate::host::RemoteTmuxCommands {
                            version_argv: Vec::new(),
                            create_argv: Vec::new(),
                            configure_argv: Vec::new(),
                            attach_argv: Vec::new(),
                            probe_argv: Vec::new(),
                            clients_argv: Vec::new(),
                            capture_argv: Vec::new(),
                            capture_full_argv: Vec::new(),
                            activity_argv: Vec::new(),
                            kill_argv: Vec::new(),
                            forward_cleanup_argv: Vec::new(),
                            support_cleanup_argv: Vec::new(),
                            remote_socket_path: "/tmp/qmux-pane-1.sock".to_string(),
                        },
                        false,
                    ),
                ),
                cwd_observation_seq: 0,
            })
            .unwrap();
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
        match CodexAdapter::new(state.config()).ingest_notification(state, notification) {
            Ok(AdapterNotificationOutcome::Event(event)) => event,
            Err(err) => panic!("{err}"),
        }
    }

    fn codex_task_started_line(turn_id: &str) -> String {
        json!({
            "type": "event_msg",
            "payload": { "type": "task_started", "turn_id": turn_id }
        })
        .to_string()
    }

    fn codex_user_message_line(turn_id: &str, text: &str) -> String {
        codex_message_line(turn_id, "user", "input_text", text)
    }

    fn codex_assistant_message_line(turn_id: &str, text: &str) -> String {
        codex_message_line(turn_id, "assistant", "output_text", text)
    }

    fn codex_message_line(turn_id: &str, role: &str, block_type: &str, text: &str) -> String {
        json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": role,
                "content": [{ "type": block_type, "text": text }],
                "internal_chat_message_metadata_passthrough": { "turn_id": turn_id }
            }
        })
        .to_string()
    }

    fn codex_turn_aborted_line(turn_id: &str) -> String {
        json!({
            "type": "event_msg",
            "payload": { "type": "turn_aborted", "turn_id": turn_id }
        })
        .to_string()
    }

    fn assert_text_block(block: &TurnBlock, expected: &str) {
        match block {
            TurnBlock::Text { text } => assert_eq!(text, expected),
            other => panic!("unexpected block: {other:?}"),
        }
    }

    fn wait_for_agent_transcript_path(
        state: &AppState,
        agent_id: &str,
        expected_path: &Path,
    ) -> AgentInfo {
        let expected = expected_path.to_str().expect("test path is utf-8");
        for _ in 0..20 {
            let agent = state.agent(agent_id).unwrap().expect("agent exists");
            if agent.transcript_path.as_deref() == Some(expected) {
                return agent;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("agent transcript path was not bound to {expected}");
    }

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("qmux-codex-{nanos}-{seq}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Preamble plus two turns, in the record order Codex writes: each turn
    /// opens with `task_started`/`turn_context`, then the user `response_item`,
    /// then its `event_msg` mirror, then the reply.
    fn codex_fork_rollout(session: &str) -> String {
        let mut records = vec![
            json!({"type": "session_meta", "payload": {"session_id": session, "id": session,
                   "cwd": "/repo"}}),
            json!({"type": "response_item", "payload": {"type": "message", "role": "developer",
                   "content": [{"type": "input_text", "text": "<permissions>"}]}}),
        ];
        for (index, (prompt, reply)) in [("first", "ok one"), ("second", "ok two")]
            .into_iter()
            .enumerate()
        {
            records.extend([
                json!({"type": "event_msg", "payload": {"type": "task_started",
                       "turn_id": format!("turn-{index}")}}),
                json!({"type": "turn_context", "payload": {"cwd": "/repo"}}),
                json!({"type": "response_item", "payload": {"type": "message", "role": "user",
                       "content": [{"type": "input_text", "text": prompt}]}}),
                json!({"type": "event_msg", "payload": {"type": "user_message",
                       "message": prompt}}),
                json!({"type": "response_item", "payload": {"type": "message", "role": "assistant",
                       "content": [{"type": "output_text", "text": reply}]}}),
                json!({"type": "event_msg", "payload": {"type": "task_complete"}}),
            ]);
        }
        records
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    fn codex_anchor(source_index: usize) -> MessageAnchor {
        MessageAnchor {
            native_id: None,
            parent_native_id: None,
            source_index,
        }
    }

    fn rollout_record_kinds(path: &Path) -> Vec<String> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let value: Value = serde_json::from_str(line).unwrap();
                let outer = value["type"].as_str().unwrap_or_default();
                let inner = value["payload"]["type"].as_str().unwrap_or_default();
                let role = value["payload"]["role"].as_str().unwrap_or_default();
                format!("{outer}/{inner}{}", if role.is_empty() { "" } else { role })
            })
            .collect()
    }

    #[test]
    fn synthesize_codex_cuts_at_the_turn_boundary_not_the_anchor() {
        let dir = temp_dir();
        let source = dir.join("rollout-2026-06-21T20-08-03-abc.jsonl");
        fs::write(&source, codex_fork_rollout("original-session")).unwrap();

        // Index 10 is the second turn's user `response_item` — the line a qmux
        // Turn anchors to, since only `response_item` records become Turns.
        let result = synthesize_truncated_codex_session(&source, &codex_anchor(10)).unwrap();

        // The cut lands on the second turn's `task_started` (index 8), so none
        // of that turn's opening records survive to replay a duplicate prompt.
        let seed = dir.join(format!("rollout-2026-06-21T20-08-03-{result}.jsonl"));
        assert_eq!(
            rollout_record_kinds(&seed),
            vec![
                "session_meta/",
                "response_item/messagedeveloper",
                "event_msg/task_started",
                "turn_context/",
                "response_item/messageuser",
                "event_msg/user_message",
                "response_item/messageassistant",
                "event_msg/task_complete",
            ]
        );
        let kept = fs::read_to_string(&seed).unwrap();
        assert!(kept.contains("first") && !kept.contains("second"));

        // Both id fields in session_meta are rewritten; Codex reads either.
        let meta: Value = serde_json::from_str(kept.lines().next().unwrap()).unwrap();
        assert_eq!(
            meta["payload"]["session_id"].as_str(),
            Some(result.as_str())
        );
        assert_eq!(meta["payload"]["id"].as_str(), Some(result.as_str()));

        // The fork is named for the source's timestamp so it sorts beside it,
        // which `seed` above already assumes by locating the file at all.
        assert!(seed.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn synthesize_codex_refuses_a_preamble_only_prefix() {
        let dir = temp_dir();
        let source = dir.join("rollout-2026-06-21T20-08-03-abc.jsonl");
        fs::write(&source, codex_fork_rollout("original-session")).unwrap();

        // Index 4 is the *first* turn's user message: cutting there leaves only
        // session meta and the developer preamble, which is a new session.
        let err = synthesize_truncated_codex_session(&source, &codex_anchor(4)).unwrap_err();
        assert_eq!(err, FORK_AT_MESSAGE_EMPTY_ERROR);

        let err = synthesize_truncated_codex_session(&source, &codex_anchor(999)).unwrap_err();
        assert!(err.contains("past the end"));

        let strays = fs::read_dir(&dir)
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().path() != source)
            .count();
        assert_eq!(strays, 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_fork_path_requires_the_rollout_naming_convention() {
        let dir = temp_dir();
        let session = "99999999-9999-4999-8999-999999999999";
        assert_eq!(
            codex_fork_rollout_path(&dir.join("rollout-2026-06-21T20-08-03-abc.jsonl"), session)
                .unwrap(),
            dir.join(format!("rollout-2026-06-21T20-08-03-{session}.jsonl"))
        );
        // A uuid source id keeps only the timestamp, not the id's own hyphens.
        assert_eq!(
            codex_fork_rollout_path(
                &dir.join("rollout-2026-06-21T20-08-03-019f8225-c9af-7072-af98-68445de31730.jsonl"),
                session,
            )
            .unwrap(),
            dir.join(format!("rollout-2026-06-21T20-08-03-{session}.jsonl"))
        );
        assert!(codex_fork_rollout_path(&dir.join("not-a-rollout.jsonl"), session).is_err());

        let _ = fs::remove_dir_all(&dir);
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
}
