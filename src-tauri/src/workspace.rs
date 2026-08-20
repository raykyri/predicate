use crate::events::QmuxEvent;
use crate::host::{self, Host};
use crate::persistence::{self, WorktreeLocation};
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

static RESEARCH_WORKSPACE_CREATION_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static AGENT_WORKSPACE_CREATION_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static MANIFEST_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Serializes research-workspace mutations against research launches. The
/// folder-replace and remove commands make a compound decision — "no live
/// panes, no active runs / no trees → commit" — that is not atomic under the
/// model lock alone: a tree or follow-up admitted between the check and the
/// commit would either launch into the old directory or silently survive a
/// removal. Launch admission (`create_research_tree` / `fork_research_node`)
/// takes this same guard, so the check-then-commit windows close.
pub fn lock_research_workspace_mutations() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    RESEARCH_WORKSPACE_CREATION_LOCK
        .lock()
        .map_err(|_| "research workspace mutation lock poisoned".to_string())
}

/// Which application mode owns a workspace and every pane launched into it.
/// Ownership is durable: callers must never infer it from a transient agent or
/// research-node binding.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkspaceScope {
    #[default]
    Terminal,
    Research,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchOrigin {
    Terminal,
    Research,
    Recovery,
}

pub fn validate_launch_workspace(
    state: &AppState,
    group_id: Option<&str>,
    origin: LaunchOrigin,
) -> Result<Option<GroupInfo>, String> {
    let Some(group_id) = group_id else {
        return match origin {
            LaunchOrigin::Terminal | LaunchOrigin::Recovery => Ok(None),
            LaunchOrigin::Research => Err("research launch requires a workspace".to_string()),
        };
    };
    let group = state
        .group(group_id)?
        .ok_or_else(|| format!("workspace {group_id} was not found"))?;
    match origin {
        LaunchOrigin::Terminal if group.scope != WorkspaceScope::Terminal => {
            Err("ordinary agents cannot be launched in a research workspace".to_string())
        }
        LaunchOrigin::Research if group.scope != WorkspaceScope::Research => {
            Err("research requires a Research-scoped workspace".to_string())
        }
        LaunchOrigin::Research if !Path::new(&group.dir).is_dir() => Err(format!(
            "research folder '{}' is unavailable; restore it at that path before launching another run",
            group.dir
        )),
        _ => Ok(Some(group)),
    }
}

/// Which multiplexer manages a remote group's panes on its host.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteMultiplexer {
    /// The default because it is the one qmux can currently drive.
    #[default]
    Tmux,
    Herdr,
}

/// The remote host a group is bound to.
///
/// Created from a `remotes` entry in `qmux.config.json` and then snapshotted
/// here, so editing that entry never moves a group whose worktrees already live
/// on the old machine. `plan_to_spec` wraps this group's panes in ssh plus the
/// named multiplexer; adapters opt in through `AgentAdapter::supports_remote`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteRef {
    /// Stable identity of the saved remote this group was created against.
    pub id: String,
    /// Display label, e.g. the ssh-config alias.
    pub label: String,
    /// Connection target: an ssh-config alias or `user@host` string. Auth and
    /// address resolution belong to the system `ssh` client, never to qmux.
    pub host: String,
    pub multiplexer: RemoteMultiplexer,
    /// How to invoke the qmux CLI on that host; defaults to `qmux-cli`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qmux_cli: Option<String>,
    /// Where agent worktrees live there. The group's `managed_dir` is always
    /// local, so a remote group needs somewhere on its own machine to put them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupInfo {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_override: Option<String>,
    /// User-facing working directory represented by this group. For a remote
    /// group this is a path on the remote host, so it must only be validated
    /// through the group-aware helpers below, never by a bare local stat.
    pub dir: String,
    /// Qmux-owned storage for the group's manifest and any generated worktrees.
    /// Always local, remote groups included: the manifest describes the group,
    /// it does not live with the group's processes.
    pub managed_dir: String,
    pub base_repo: Option<String>,
    pub base_ref: Option<String>,
    pub parent_id: Option<String>,
    pub created_at: u128,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default)]
    pub scope: WorkspaceScope,
    /// Identifies the exact portable archive whose import committed this
    /// workspace. Kept until that archive is removed so startup can safely
    /// finish cleanup after a crash without touching an unrelated archive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_research_archive_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteRef>,
    pub agents: Vec<String>,
}

impl GroupInfo {
    pub fn is_remote(&self) -> bool {
        self.remote.is_some()
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResearchWorkspaceInfo {
    #[serde(flatten)]
    pub group: GroupInfo,
    pub available: bool,
    pub tree_count: usize,
}

/// One setting an ACP agent exposes for a session (ACP "session config
/// options"). Deliberately lenient: the protocol lets agents add categories and
/// value kinds qmux has never heard of, so unknown shapes must survive the trip
/// to the UI rather than being rejected here.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpConfigOption {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `model`, `mode`, `model_config`, `thought_level`, or an agent-specific
    /// one (which the spec requires to be `_`-prefixed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// `select` or `boolean`. Named `kind` because `type` is a keyword.
    #[serde(rename = "type")]
    pub kind: String,
    pub current_value: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<AcpConfigChoice>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpConfigChoice {
    pub value: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl AcpConfigOption {
    /// The label for `current_value`: a select's matching choice name, or the
    /// bare value. Agents choose opaque ids (`"model-1"`), so the label is what
    /// makes a header worth reading.
    pub fn current_label(&self) -> Option<String> {
        match &self.current_value {
            Value::String(current) => Some(
                self.options
                    .iter()
                    .find(|choice| &choice.value == current)
                    .map(|choice| choice.name.clone())
                    .unwrap_or_else(|| current.clone()),
            ),
            Value::Bool(value) => Some(if *value { "on" } else { "off" }.to_string()),
            Value::Null => None,
            other => Some(other.to_string()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    pub id: String,
    pub group_id: String,
    pub adapter: String,
    pub worktree_dir: String,
    pub branch: Option<String>,
    /// Best-effort, display-only workspace most recently observed from this
    /// agent's command stream. This is deliberately separate from
    /// `worktree_dir` / `branch`: those fields describe the launch workspace
    /// qmux owns and are used by resume, fork, and cleanup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_workspace: Option<ActiveWorkspace>,
    pub pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orphaned_queue_pane_id: Option<String>,
    pub session_id: Option<String>,
    pub transcript_path: Option<String>,
    pub status: AgentStatus,
    pub model: Option<String>,
    /// Reasoning effort level the session was launched with. Persisted like
    /// `model` so respawns, resumes, and forks keep the session's configured
    /// effort. Absent when the adapter default applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Tool-approval policy the session was launched with, persisted like
    /// `effort` so a respawn does not silently fall back to the CLI's default —
    /// which would be a quieter, more permissive session than the user chose.
    /// Muse and Devin set this today. Absent when the adapter default applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<String>,
    /// Which `adapters.acp.agents` entry an `acp` agent was launched with.
    /// Only the ACP adapter sets this: unlike the vendor adapters, its
    /// registry id names a protocol rather than a program, so a respawn has to
    /// remember which agent behind that protocol to start again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_agent: Option<String>,
    /// Session configuration an ACP agent exposes for itself — its model list,
    /// mode, reasoning level. Read-only for now: qmux renders it, and the agent
    /// remains the source of truth. Persisted so a restored pane shows the
    /// session's settings before the agent has said anything.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acp_config_options: Vec<AcpConfigOption>,
    pub parent_id: Option<String>,
    pub fork_point: Option<String>,
    pub root_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
    /// Adapter-owned leaf within a native session tree. This is intentionally
    /// separate from `branch_id`, which identifies qmux's own thread-graph
    /// branch and must remain stable when an agent rewinds or switches leaves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_leaf_id: Option<String>,
    /// True when the queue has paused after a pause-after turn finished; the backend
    /// stops auto-draining until the user unpauses.
    #[serde(default)]
    pub paused: bool,
    pub created_at: u128,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ActiveWorkspaceKind {
    Directory,
    GitCheckout,
    MainCheckout,
    LinkedWorktree,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ActiveWorkspaceSource {
    Qmux,
    Claude,
    Codex,
}

/// Runtime location reported by an agent. It is informational only: no launch,
/// fork, status, or deletion path may use it in place of `worktree_dir`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveWorkspace {
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub kind: ActiveWorkspaceKind,
    pub source: ActiveWorkspaceSource,
    pub managed_by_qmux: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AgentStatus {
    Starting,
    Running,
    AwaitingInput,
    AwaitingPermission,
    Done,
    #[serde(alias = "stopped")]
    Idle,
    Failed,
}

impl AgentStatus {
    /// Whether the agent is between turns with its delivered content settled.
    /// `AwaitingInput` counts as at rest: adapters assign it to agents that
    /// are ready for a prompt (post-notification, post-interruption), and the
    /// interrupted tail of such a timeline carries its own turn status. It is
    /// distinct from `AwaitingPermission`, which always sits inside a pending
    /// tool call. `Failed` is deliberately not at rest — a process that died
    /// mid-answer leaves a half-streamed tail that must not read as delivered.
    /// The allowlist runs in this direction so an unhandled future status
    /// classifies as busy, the conservative side for content capture.
    pub fn is_at_rest(self) -> bool {
        matches!(self, Self::Done | Self::Idle | Self::AwaitingInput)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateGroupRequest {
    pub name: Option<String>,
    pub dir: Option<String>,
    pub after_group_id: Option<String>,
    pub base_repo: Option<String>,
    pub base_ref: Option<String>,
    #[serde(default)]
    pub remote: Option<RemoteRef>,
    /// Names a `remotes` entry in `qmux.config.json` to bind this group to.
    /// Resolved into `remote` before the group is created; ignored when
    /// `remote` is already set.
    #[serde(default)]
    pub remote_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PrepareAgentWorkspaceRequest {
    pub group_id: Option<String>,
    pub base_repo: Option<String>,
    pub base_ref: Option<String>,
    pub adapter: String,
    pub model: Option<String>,
    /// Reasoning effort level for the new session, recorded on the agent like
    /// `model` so later respawns and forks re-apply it.
    pub effort: Option<String>,
    /// When false, the agent runs directly in the base repository / cwd with no
    /// isolated git worktree (the default).
    pub use_worktree: bool,
}

pub fn create_group(
    state: &AppState,
    mut request: CreateGroupRequest,
) -> Result<GroupInfo, String> {
    // Resolve the saved remote before anything is created, so an id that is no
    // longer declared fails the request rather than silently producing a local
    // group where the caller asked for a remote one.
    if request.remote.is_none()
        && let Some(id) = request.remote_id.as_deref()
    {
        request.remote = Some(state.config().saved_remote(id)?);
    }

    create_scoped_group(state, request, WorkspaceScope::Terminal)
}

/// Creates a durable research workspace rooted at `dir`. Research workspaces
/// share the group/agent execution machinery, but their explicit scope gives
/// them independent navigation and retention policy.
pub fn create_research_workspace(
    state: &AppState,
    name: Option<String>,
    dir: String,
) -> Result<GroupInfo, String> {
    let _guard = lock_research_workspace_mutations()?;
    create_research_workspace_locked(state, name, dir)
}

/// Whether an existing workspace record refers to the requested directory.
/// Records are stored canonicalized, but a record whose folder is currently
/// missing cannot be re-canonicalized (and legacy-migrated records may hold
/// non-canonical paths), so fall back to comparing the stored string against
/// both the canonical and as-picked forms of the requested path. Without the
/// fallback, re-picking a folder while the record's copy was unreadable
/// created a second workspace for the same directory.
fn group_dir_matches(group: &GroupInfo, canonical: &Path, requested: &Path) -> bool {
    let stored = Path::new(&group.dir);
    match fs::canonicalize(stored) {
        Ok(stored_canonical) => stored_canonical == canonical,
        Err(_) => stored == canonical || stored == requested,
    }
}

/// Reads a user-picked folder's detached archive for open/repoint decisions.
/// An unreadable archive (corrupted manifest, a version from a newer qmux, a
/// damaged response file) must not strand the user with a bare decode error:
/// these call sites are the only in-app way to use the folder at all, so the
/// error has to say where the archive lives and how to proceed without it.
fn read_detached_research_for_folder(
    folder: &Path,
) -> Result<Option<crate::research::DetachedResearchBundle>, String> {
    crate::research::read_detached_research(folder).map_err(|err| {
        format!(
            "{} contains detached research that could not be read: {err}. To use the folder without restoring its history, move {} out of the folder and keep it somewhere safe.",
            folder.display(),
            crate::research::detached_research_archive_location(folder).display()
        )
    })
}

fn create_research_workspace_locked(
    state: &AppState,
    name: Option<String>,
    dir: String,
) -> Result<GroupInfo, String> {
    let requested = PathBuf::from(&dir);
    let canonical = fs::canonicalize(&requested).map_err(|err| {
        format!(
            "research folder {} is unavailable: {err}",
            requested.display()
        )
    })?;
    if !canonical.is_dir() {
        return Err(format!(
            "research folder {} is not a directory",
            canonical.display()
        ));
    }
    if let Some(existing) = state.list_groups()?.into_iter().find(|group| {
        group.scope == WorkspaceScope::Research && group_dir_matches(group, &canonical, &requested)
    }) {
        return Ok(existing);
    }
    if let Some(bundle) = read_detached_research_for_folder(&canonical)? {
        // A completed node whose response file is absent from the archive
        // (a partial copy between machines, a hand-deleted file) is already
        // damaged outside qmux — refusing the whole import here turned one
        // missing file into a folder that could never be opened again,
        // stranding every intact prompt and answer alongside it. Import
        // everything that survived instead; import_detached_research clears
        // the snapshot stamp on nodes that arrive without a response, so the
        // node restores with its prompt and preview and never claims a
        // durable answer it does not have.
        for node in &bundle.archive.nodes {
            if node.status == crate::research::ResearchNodeStatus::Complete
                && !bundle.responses.contains_key(&node.id)
            {
                eprintln!(
                    "qmux: detached research response {} is missing from {}; importing the node without it",
                    node.id,
                    canonical.display()
                );
            }
        }
        let archive_name = bundle.archive.workspace.name_override.clone();
        let mut group = create_group_record(
            state,
            CreateGroupRequest {
                remote_id: None,
                name: name.or(archive_name),
                dir: Some(canonical.display().to_string()),
                after_group_id: None,
                base_repo: None,
                base_ref: Some("HEAD".to_string()),
                remote: None,
            },
            WorkspaceScope::Research,
        )?;
        group.imported_research_archive_id = Some(bundle.archive.archive_id.clone());
        if let Err(err) = write_group_manifest(&group) {
            let _ = fs::remove_dir_all(&group.managed_dir);
            return Err(err);
        }
        let imported = match state.import_detached_research(
            group.clone(),
            bundle.archive.tree_order,
            bundle.archive.trees,
            bundle.archive.folders,
            bundle.archive.membership,
            bundle.archive.nodes,
            bundle.responses,
        ) {
            Ok(imported) => imported,
            Err(err) => {
                let _ = fs::remove_dir_all(&group.managed_dir);
                return Err(err);
            }
        };
        if let Err(err) = reconcile_imported_research_archive(state, &imported) {
            // The global import and its cleanup receipt are already durable.
            // Startup retries this exact archive instead of risking deletion
            // of some other archive later placed in the folder.
            eprintln!("qmux: {err}");
        }
        return state
            .group(&imported.id)?
            .ok_or_else(|| format!("imported research workspace {} disappeared", imported.id));
    }
    create_scoped_group(
        state,
        CreateGroupRequest {
            remote_id: None,
            name,
            dir: Some(canonical.display().to_string()),
            after_group_id: None,
            base_repo: None,
            base_ref: Some("HEAD".to_string()),
            remote: None,
        },
        WorkspaceScope::Research,
    )
}

pub fn reconcile_imported_research_archives(state: &AppState) {
    let groups = match state.list_groups() {
        Ok(groups) => groups,
        Err(err) => {
            eprintln!("qmux: failed to list imported research cleanup receipts: {err}");
            return;
        }
    };
    for group in groups.into_iter().filter(|group| {
        group.scope == WorkspaceScope::Research && group.imported_research_archive_id.is_some()
    }) {
        if let Err(err) = reconcile_imported_research_archive(state, &group) {
            eprintln!("qmux: {err}");
        }
    }
}

fn reconcile_imported_research_archive(state: &AppState, group: &GroupInfo) -> Result<(), String> {
    let Some(expected_archive_id) = group.imported_research_archive_id.as_deref() else {
        return Ok(());
    };
    let folder = Path::new(&group.dir);
    match crate::research::read_detached_research(folder)? {
        Some(bundle) if bundle.archive.archive_id == expected_archive_id => {
            crate::research::remove_detached_research(folder, bundle.pending)?;
        }
        Some(_) => {
            // The receipt belongs to an older import. Preserve the different
            // archive now in the folder and merely retire the stale receipt.
        }
        None => {}
    }
    let mut updated = group.clone();
    updated.imported_research_archive_id = None;
    write_group_manifest(&updated)?;
    state.update_group(updated)
}

pub fn ensure_default_research_workspace(state: &AppState) -> Result<GroupInfo, String> {
    let _guard = lock_research_workspace_mutations()?;
    let dir = state.default_research_dir();
    fs::create_dir_all(&dir).map_err(|err| {
        format!(
            "failed to create default research directory {}: {err}",
            dir.display()
        )
    })?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).map_err(|err| {
        format!(
            "failed to restrict default research directory {}: {err}",
            dir.display()
        )
    })?;
    let canonical = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
    if let Some(existing) = state.list_groups()?.into_iter().find(|group| {
        group.scope == WorkspaceScope::Research && group_dir_matches(group, &canonical, &dir)
    }) {
        return Ok(existing);
    }
    create_research_workspace_locked(
        state,
        Some("Default research".to_string()),
        canonical.display().to_string(),
    )
}

pub fn rename_research_workspace(
    state: &AppState,
    workspace_id: &str,
    name: Option<String>,
) -> Result<GroupInfo, String> {
    // Keep rename and removal serialized so their read-modify-write updates
    // cannot silently revert one another in the model or durable manifest.
    let _guard = lock_research_workspace_mutations()?;
    let workspace = require_research_workspace(state, workspace_id)?;
    rename_group_record(state, workspace, name)
}

/// Moves a research workspace to a different directory. While a folder is
/// open its research history (trees, nodes, responses) lives in qmux's global
/// state, so a move repoints the durable record and relocates only the
/// research-specific state a folder can carry — a detached `.qmux` research
/// archive belonging to this workspace. The old folder itself and the rest of
/// its contents are never modified or deleted.
pub fn move_research_workspace(
    state: &AppState,
    workspace_id: &str,
    dir: String,
) -> Result<GroupInfo, String> {
    let _guard = lock_research_workspace_mutations()?;
    let mut workspace = require_research_workspace(state, workspace_id)?;
    // Retire a fresh import's cleanup receipt first, exactly like removal:
    // the receipt's archive is an already-imported duplicate that must be
    // deleted where it stands, not carried along as if it were history.
    if workspace.imported_research_archive_id.is_some() {
        reconcile_imported_research_archive(state, &workspace)?;
        workspace = require_research_workspace(state, workspace_id)?;
    }
    let dependencies = state.research_workspace_dependencies(workspace_id)?;
    let display_name = workspace
        .name_override
        .as_deref()
        .unwrap_or(&workspace.name);
    if dependencies.has_live_panes {
        return Err(format!(
            "research folder '{display_name}' cannot be moved while it has live terminals"
        ));
    }
    if dependencies.has_active_runs {
        return Err(format!(
            "research folder '{display_name}' cannot be moved while it has active runs"
        ));
    }
    let requested = PathBuf::from(&dir);
    let canonical = fs::canonicalize(&requested).map_err(|err| {
        format!(
            "research folder {} is unavailable: {err}",
            requested.display()
        )
    })?;
    if !canonical.is_dir() {
        return Err(format!(
            "research folder {} is not a directory",
            canonical.display()
        ));
    }
    if group_dir_matches(&workspace, &canonical, &requested) {
        return Ok(workspace);
    }
    if let Some(existing) = state.list_groups()?.into_iter().find(|group| {
        group.id != workspace.id
            && group.scope == WorkspaceScope::Research
            && group_dir_matches(group, &canonical, &requested)
    }) {
        return Err(format!(
            "{} is already open as research folder '{}'",
            canonical.display(),
            existing.name_override.as_deref().unwrap_or(&existing.name)
        ));
    }
    // A destination carrying its own detached history belongs to some other
    // research folder; moving on top of it would hide that archive behind
    // this workspace's record until removal, which would then refuse to
    // overwrite it. Opening it as its own folder is the deliberate path.
    if read_detached_research_for_folder(&canonical)?.is_some() {
        return Err(format!(
            "{} already contains detached research history; open it as its own research folder instead",
            canonical.display()
        ));
    }
    // The one research-specific thing the old folder can hold for a live
    // workspace is its own pending archive (a removal interrupted before its
    // global commit). Rewrite it into the destination before repointing so a
    // crash between the two steps leaves both copies rather than neither,
    // then clear the source copy. Foreign archives — a different workspace's
    // detached history sitting in the folder — stay with the folder they
    // describe.
    match crate::research::read_detached_research(Path::new(&workspace.dir)) {
        Ok(Some(bundle)) if bundle.archive.workspace.id == workspace.id => {
            crate::research::write_detached_research_pending(
                &canonical,
                &bundle.archive,
                &bundle.responses,
            )?;
            if !bundle.pending {
                crate::research::commit_detached_research(&canonical)?;
            }
            crate::research::remove_detached_research(Path::new(&workspace.dir), bundle.pending)?;
        }
        Ok(_) => {}
        Err(err) => {
            // Unreadable, so its owner cannot be identified — and a live
            // workspace's own history is safe in global state either way.
            // Leave it with the old folder rather than failing the move.
            eprintln!(
                "qmux: leaving unreadable detached research behind in {}: {err}",
                workspace.dir
            );
        }
    }
    workspace.dir = canonical.display().to_string();
    workspace.name = group_name_for_dir(&canonical);
    write_group_manifest(&workspace)?;
    state.update_group(workspace.clone())?;
    state.emit(crate::events::QmuxEvent::new(
        "group.updated",
        None,
        None,
        serde_json::json!({ "group": workspace.clone() }),
    ));
    Ok(workspace)
}

pub fn remove_research_workspace(
    state: &AppState,
    workspace_id: &str,
) -> Result<Vec<String>, String> {
    let _guard = lock_research_workspace_mutations()?;
    let mut workspace = require_research_workspace(state, workspace_id)?;
    if workspace.imported_research_archive_id.is_some() {
        reconcile_imported_research_archive(state, &workspace)?;
        workspace = require_research_workspace(state, workspace_id)?;
    }
    let dependencies = state.research_workspace_dependencies(workspace_id)?;
    let display_name = workspace
        .name_override
        .as_deref()
        .unwrap_or(&workspace.name);
    if dependencies.has_live_panes {
        return Err(format!(
            "research folder '{display_name}' cannot be removed while it has live terminals"
        ));
    }
    if dependencies.has_active_runs {
        return Err(format!(
            "research folder '{display_name}' cannot be removed while it has active runs"
        ));
    }
    let archive = state.detached_research_archive(workspace_id)?;
    let detached_tree_ids = archive
        .trees
        .iter()
        .map(|tree| tree.id.clone())
        .collect::<Vec<_>>();
    if archive.trees.is_empty() {
        // A folder with no research history has nothing to preserve, so it
        // must not gain a `.qmux` archive as a side effect of being removed.
        // The archive write would also recreate a folder the user already
        // deleted from disk, and fail outright on a read-only folder —
        // leaving an *unused* folder impossible to remove.
        state.commit_research_workspace_detach(workspace_id, &archive)?;
        remove_research_workspace_manifest_dir(&workspace);
        return Ok(detached_tree_ids);
    }
    let mut responses = HashMap::new();
    let archived_nodes_by_id = archive
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<HashMap<_, _>>();
    for node in &archive.nodes {
        let turns =
            match crate::research::read_response_snapshot(&state.config().workspace_root, &node.id)
            {
                Ok(Some(turns)) => Some(turns),
                Ok(None) | Err(_) => {
                    let ancestor_prompts = crate::research::ancestor_prompts(node, |id| {
                        archived_nodes_by_id.get(id).copied()
                    });
                    crate::research::load_transcript_response(
                        state.config(),
                        node,
                        &ancestor_prompts,
                    )
                    .ok()
                }
            };
        if let Some(turns) = turns.filter(|turns| {
            node.status != crate::research::ResearchNodeStatus::Complete
                || crate::research::has_active_assistant_turn(turns)
        }) {
            responses.insert(node.id.clone(), turns);
        } else if node.status == crate::research::ResearchNodeStatus::Complete {
            // Only a completed answer justifies refusing the detach: it is
            // real data the archive would silently lose. A failed or
            // cancelled run with a transcript_path whose file is gone (the
            // adapter owns those files and prunes them on its own schedule)
            // has nothing durable left to lose — the node record itself,
            // with its prompt, error, and preview, still travels in the
            // archive, and import does not require responses for
            // non-complete nodes. Refusing here made the folder permanently
            // unremovable while protecting nothing.
            return Err(format!(
                "research item '{}' has no durable response to detach",
                node.prompt.chars().take(80).collect::<String>()
            ));
        }
    }
    let folder = Path::new(&workspace.dir);
    crate::research::write_detached_research_pending(folder, &archive, &responses)?;
    let node_ids = state.commit_research_workspace_detach(workspace_id, &archive)?;
    // The pending archive is already complete and importable. Promotion is the
    // second phase; if it fails, reopening the folder deliberately recovers it.
    if let Err(err) = crate::research::commit_detached_research(folder) {
        // The verified pending form is intentionally importable: after the
        // global commit there is no data-loss reason to report removal as a
        // failure merely because the final directory rename did not land.
        eprintln!("qmux: detached research remains in recoverable pending form: {err}");
    }
    for node_id in node_ids {
        if let Err(err) =
            crate::research::remove_response_snapshot(&state.config().workspace_root, &node_id)
        {
            eprintln!("qmux: failed to remove detached global response {node_id}: {err}");
        }
    }
    remove_research_workspace_manifest_dir(&workspace);
    Ok(detached_tree_ids)
}

/// The manifest directory is qmux-internal bookkeeping for this group and
/// nothing else: research runs never create worktrees under it, so once the
/// record is gone it holds only the stale group.json. Deleting it here (and
/// only here — Terminal groups may own worktrees) keeps removed folders
/// from accumulating on disk forever. Best-effort, with a guard against a
/// record that ever pointed managed_dir at the user's own folder.
fn remove_research_workspace_manifest_dir(workspace: &GroupInfo) {
    if workspace.managed_dir != workspace.dir
        && Path::new(&workspace.managed_dir).join(".qmux").is_dir()
        && let Err(err) = fs::remove_dir_all(&workspace.managed_dir)
        && !matches!(err.kind(), std::io::ErrorKind::NotFound)
    {
        eprintln!(
            "qmux: failed to remove research workspace manifest dir {}: {err}",
            workspace.managed_dir
        );
    }
}

fn require_research_workspace(state: &AppState, workspace_id: &str) -> Result<GroupInfo, String> {
    let workspace = state
        .group(workspace_id)?
        .ok_or_else(|| format!("research workspace {workspace_id} was not found"))?;
    if workspace.scope != WorkspaceScope::Research {
        return Err(format!("workspace {workspace_id} is not Research-scoped"));
    }
    Ok(workspace)
}

fn create_scoped_group(
    state: &AppState,
    request: CreateGroupRequest,
    scope: WorkspaceScope,
) -> Result<GroupInfo, String> {
    let after_group_id = request.after_group_id.clone();
    let group = create_group_record(state, request, scope)?;
    state.insert_group_after(group.clone(), after_group_id.as_deref())?;
    state.emit(crate::events::QmuxEvent::new(
        "group.created",
        None,
        None,
        serde_json::json!({ "group": group.clone() }),
    ));
    Ok(group)
}

/// Allocates and persists a group manifest without inserting it into AppState.
/// Startup migration uses this to construct scoped workspace records before the
/// persisted model is hydrated.
pub(crate) fn create_group_record(
    state: &AppState,
    request: CreateGroupRequest,
    scope: WorkspaceScope,
) -> Result<GroupInfo, String> {
    let id = state.next_id("group");
    let dir = match request.dir.as_deref() {
        Some(dir) => group_canonical_dir(request.remote.as_ref(), dir)?,
        None => state.default_open_dir(),
    };
    let generated_name = group_name_for_dir(&dir);
    let name_override = request
        .name
        .clone()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty());
    let display_name = name_override
        .clone()
        .unwrap_or_else(|| generated_name.clone());
    let managed_dir = unique_group_dir(&state.config().workspace_root, &display_name)?;
    fs::create_dir_all(managed_dir.join(".qmux"))
        .map_err(|err| format!("failed to create group dir {}: {err}", dir.display()))?;

    let group = GroupInfo {
        id,
        name: generated_name,
        name_override,
        dir: dir.display().to_string(),
        managed_dir: managed_dir.display().to_string(),
        base_repo: request.base_repo,
        base_ref: request.base_ref,
        parent_id: None,
        created_at: now_millis(),
        collapsed: false,
        scope,
        imported_research_archive_id: None,
        remote: request.remote,
        agents: Vec::new(),
    };

    if let Err(err) = write_group_manifest(&group) {
        let _ = fs::remove_dir_all(&managed_dir);
        return Err(err);
    }
    Ok(group)
}

/// Creates a distinct scoped record from a legacy group without requiring its
/// working directory to still exist. Historical research remains viewable even
/// when its original project was moved; a later launch can surface that missing
/// directory and let the user choose a replacement deliberately.
pub(crate) fn clone_group_record_for_scope(
    state: &AppState,
    source: &GroupInfo,
    scope: WorkspaceScope,
) -> Result<GroupInfo, String> {
    let display_name = format!(
        "{} Research",
        source.name_override.as_deref().unwrap_or(&source.name)
    );
    let managed_dir = unique_group_dir(&state.config().workspace_root, &display_name)?;
    fs::create_dir_all(managed_dir.join(".qmux")).map_err(|err| {
        format!(
            "failed to create migrated research workspace {}: {err}",
            managed_dir.display()
        )
    })?;
    let group = GroupInfo {
        id: state.next_id("group"),
        name: source.name.clone(),
        name_override: Some(display_name),
        dir: source.dir.clone(),
        managed_dir: managed_dir.display().to_string(),
        base_repo: source.base_repo.clone(),
        base_ref: source.base_ref.clone(),
        parent_id: None,
        created_at: now_millis(),
        collapsed: false,
        scope,
        imported_research_archive_id: None,
        // Research runs entirely on the local machine; a research clone of a
        // remote group deliberately drops the binding.
        remote: None,
        agents: Vec::new(),
    };
    if let Err(err) = write_group_manifest(&group) {
        let _ = fs::remove_dir_all(&managed_dir);
        return Err(err);
    }
    Ok(group)
}

pub fn set_group_dir(state: &AppState, group_id: &str, dir: String) -> Result<GroupInfo, String> {
    let group = state
        .group(group_id)?
        .ok_or_else(|| format!("group {group_id} was not found"))?;
    if group.scope != WorkspaceScope::Terminal {
        return Err(
            "use the research workspace folder command for Research workspaces".to_string(),
        );
    }
    let dir = group_canonical_dir(group.remote.as_ref(), &dir)?;
    set_group_dir_record(state, group_id, dir)
}

fn set_group_dir_record(
    state: &AppState,
    group_id: &str,
    dir: PathBuf,
) -> Result<GroupInfo, String> {
    let mut group = state
        .group(group_id)?
        .ok_or_else(|| format!("group {group_id} was not found"))?;
    group.dir = dir.display().to_string();
    group.name = group_name_for_dir(&dir);
    group.base_repo = None;
    group.base_ref = None;
    write_group_manifest(&group)?;
    state.update_group(group.clone())?;
    state.emit(crate::events::QmuxEvent::new(
        "group.updated",
        None,
        None,
        serde_json::json!({ "group": group.clone() }),
    ));
    Ok(group)
}

pub fn rename_group(
    state: &AppState,
    group_id: &str,
    name: Option<String>,
) -> Result<GroupInfo, String> {
    let group = state
        .group(group_id)?
        .ok_or_else(|| format!("group {group_id} was not found"))?;
    if group.scope != WorkspaceScope::Terminal {
        return Err(
            "use the research workspace rename command for Research workspaces".to_string(),
        );
    }
    rename_group_record(state, group, name)
}

fn rename_group_record(
    state: &AppState,
    mut group: GroupInfo,
    name: Option<String>,
) -> Result<GroupInfo, String> {
    group.name_override = name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty());
    write_group_manifest(&group)?;
    state.update_group(group.clone())?;
    state.emit(crate::events::QmuxEvent::new(
        "group.updated",
        None,
        None,
        serde_json::json!({ "group": group.clone() }),
    ));
    Ok(group)
}

pub fn set_group_collapsed(
    state: &AppState,
    group_id: &str,
    collapsed: bool,
) -> Result<GroupInfo, String> {
    let mut group = state
        .group(group_id)?
        .ok_or_else(|| format!("group {group_id} was not found"))?;
    group.collapsed = collapsed;
    write_group_manifest(&group)?;
    state.update_group(group.clone())?;
    state.emit(crate::events::QmuxEvent::new(
        "group.updated",
        None,
        None,
        serde_json::json!({ "group": group.clone() }),
    ));
    Ok(group)
}

pub fn prepare_agent_workspace(
    state: &AppState,
    request: PrepareAgentWorkspaceRequest,
) -> Result<AgentInfo, String> {
    let _guard = AGENT_WORKSPACE_CREATION_LOCK
        .lock()
        .map_err(|_| "agent workspace creation lock poisoned".to_string())?;
    prepare_agent_workspace_locked(state, request, None)
}

/// Prepares a new agent and records its lineage before returning to the adapter
/// that will start the process. Keeping both mutations under the workspace
/// creation lock prevents a newly-execed child from observing itself without
/// the parent capability boundary its launcher requested.
pub fn prepare_agent_workspace_with_parent(
    state: &AppState,
    mut request: PrepareAgentWorkspaceRequest,
    parent_id: Option<&str>,
) -> Result<AgentInfo, String> {
    let _guard = AGENT_WORKSPACE_CREATION_LOCK
        .lock()
        .map_err(|_| "agent workspace creation lock poisoned".to_string())?;
    let parent = match parent_id.map(str::trim).filter(|id| !id.is_empty()) {
        Some(parent_id) => {
            let parent = state
                .agent(parent_id)?
                .ok_or_else(|| format!("parent agent {parent_id} was not found"))?;
            if request
                .group_id
                .as_deref()
                .is_some_and(|group_id| group_id != parent.group_id)
            {
                return Err(format!(
                    "parent agent {parent_id} belongs to workspace {}, not {}",
                    parent.group_id,
                    request.group_id.as_deref().unwrap_or_default()
                ));
            }
            if request.group_id.is_none() {
                request.group_id = Some(parent.group_id.clone());
            }
            Some(parent)
        }
        None => None,
    };
    let agent = prepare_agent_workspace_locked(state, request, None)?;
    let Some(parent) = parent else {
        return Ok(agent);
    };
    let parent_id = parent.id;
    debug_assert_eq!(parent.group_id, agent.group_id);
    state
        .mutate_agent(&agent.id, |agent| {
            agent.parent_id = Some(parent_id.clone());
        })?
        .ok_or_else(|| format!("prepared agent {} disappeared", agent.id))
}

fn prepare_agent_workspace_locked(
    state: &AppState,
    request: PrepareAgentWorkspaceRequest,
    agent_id_override: Option<String>,
) -> Result<AgentInfo, String> {
    let mut group = match request.group_id.as_deref() {
        Some(group_id) => state
            .group(group_id)?
            .ok_or_else(|| format!("group {group_id} was not found"))?,
        None => create_group(
            state,
            CreateGroupRequest {
                remote_id: None,
                name: None,
                dir: request
                    .base_repo
                    .clone()
                    .or_else(|| Some(state.default_open_dir().display().to_string())),
                after_group_id: None,
                base_repo: request
                    .base_repo
                    .clone()
                    .or_else(|| default_base_repo(state)),
                base_ref: request
                    .base_ref
                    .clone()
                    .or_else(|| Some("HEAD".to_string())),
                remote: None,
            },
        )?,
    };

    let agent_id = agent_id_override.unwrap_or_else(|| state.next_id("agent"));
    let agent_name = format!("agent-{}", group.agents.len() + 1);
    // An agent roots at its explicit base_repo, else the group's base_repo hint, else
    // the group's advisory cwd (its most-recently-active shell pane), else the group's
    // creation-time seed dir, else the default dir. Groups are not directory-scoped,
    // so the seed is only a fallback for a group with no shell panes yet.
    let base_repo = request
        .base_repo
        .or_else(|| group.base_repo.clone())
        .or_else(|| {
            state
                .group_spawn_cwd(&group.id)
                .map(|dir| dir.display().to_string())
        })
        .or_else(|| Some(group.dir.clone()));
    let base_ref = request
        .base_ref
        .or_else(|| group.base_ref.clone())
        .unwrap_or_else(|| "HEAD".to_string());
    let mut branch = None;
    // The group is already resolved above, so the host comes for free — and
    // cannot disagree with the workspace the worktree is being cut from.
    let host = host::for_group(group.remote.as_ref());

    let use_worktree = request.use_worktree;
    let worktree_dir = if use_worktree {
        // Isolated git worktree in the configured global or project-local root
        // (or a plain directory when the base is not a git repo).
        let dir =
            allocate_agent_worktree_dir(state, &host, base_repo.as_deref(), &group, &agent_name)?;
        match base_repo.as_deref().filter(|repo| is_git_repo(&host, repo)) {
            Some(base_repo) => {
                let branch_name =
                    format!("qmux/{}/{}", sanitize_ref_segment(&group.id), agent_name);
                create_worktree(&host, base_repo, &dir, &branch_name, &base_ref)?;
                branch = Some(branch_name);
            }
            // Not a git repo, so there is no worktree to add — just a
            // directory to run in. It belongs on the group's host: creating it
            // locally would leave a stray tree here and the agent pointed at a
            // path that does not exist over there.
            None => host.create_dir_all(&dir)?,
        }
        dir.display().to_string()
    } else {
        // Default: no worktree — the agent runs directly in the base repo / cwd.
        // base_repo is always Some by construction above; the tail is belt-and-braces.
        base_repo.unwrap_or_else(|| state.default_open_dir().display().to_string())
    };

    let active_workspace = host
        .is_local()
        .then(|| {
            resolve_active_workspace(
                &worktree_dir,
                ActiveWorkspaceSource::Qmux,
                use_worktree && branch.is_some(),
            )
        })
        .flatten();

    let agent = AgentInfo {
        acp_config_options: Vec::new(),
        id: agent_id.clone(),
        group_id: group.id.clone(),
        adapter: request.adapter,
        worktree_dir,
        branch,
        active_workspace,
        pane_id: None,
        orphaned_queue_pane_id: None,
        session_id: None,
        transcript_path: None,
        status: AgentStatus::Starting,
        model: request.model,
        effort: request.effort,
        approval_mode: None,
        // Set by the ACP adapter immediately after this returns; the shared
        // workspace request has no reason to carry an adapter-specific field.
        acp_agent: None,
        parent_id: None,
        fork_point: None,
        root_session_id: None,
        thread_id: Some(state.next_id("thread")),
        branch_id: Some(state.next_id("branch")),
        native_leaf_id: None,
        paused: false,
        created_at: now_millis(),
    };

    group.agents.push(agent_id);
    write_group_manifest(&group)?;
    state.update_group(group)?;
    state.insert_agent(agent.clone())?;
    Ok(agent)
}

/// Restores the pane-to-agent binding when an authenticated SessionStart is the
/// first lifecycle signal qmux can observe for a shell-launched agent.
///
/// Normally qmux's injected `codex` / `claude` / `opencode` / `grok` shell
/// function calls `agent.prepare_shell_launch` before exec. That creates and
/// attaches the agent early enough for the right pane to exist before a fast
/// SessionStart hook. If that preparation record is later lost or detached while
/// the launched process still owns the pane token, its authenticated hook can
/// arrive without a binding. Recover it here so SessionStart can bind the native
/// session and load its transcript instead of silently becoming a no-op. This also
/// covers other adapter integration paths that can emit an authenticated
/// SessionStart without going through the injected shell function.
pub fn recover_shell_agent_from_session_start(
    state: &AppState,
    pane: &crate::state::PaneInfo,
    adapter_id: &str,
    preferred_agent_id: Option<&str>,
) -> Result<AgentInfo, String> {
    let _guard = AGENT_WORKSPACE_CREATION_LOCK
        .lock()
        .map_err(|_| "agent workspace creation lock poisoned".to_string())?;

    // Serialize with ordinary shell preparation and recheck under the shared
    // guard: a nearly simultaneous prepare must win rather than minting a second
    // agent for the same pane.
    if let Some(agent) = state.agent_by_pane(&pane.id)? {
        return Ok(agent);
    }
    if !matches!(pane.kind, crate::state::PaneKind::Shell) {
        return Err(format!(
            "cannot recover a shell agent in non-shell pane {}",
            pane.id
        ));
    }

    let preferred_agent_id = preferred_agent_id
        .map(str::trim)
        .filter(|agent_id| !agent_id.is_empty());
    let agent = if let Some(agent_id) = preferred_agent_id
        && let Some(existing) = state.agent(agent_id)?
    {
        if existing.pane_id.is_some()
            || existing.group_id != pane.group_id
            || existing.adapter != adapter_id
            || existing.worktree_dir != pane.cwd
        {
            return Err(format!(
                "agent {agent_id} cannot be recovered into pane {}",
                pane.id
            ));
        }
        existing
    } else {
        // Retain a qmux-minted id when the record itself disappeared. The running
        // shell supervisor and every later hook still carry that id; changing it
        // here would prevent the supervisor from detaching this recovered binding
        // when the native agent process exits. Never accept an arbitrary id as a
        // durable key—the hook payload is pane-authenticated but can still be
        // influenced by code running inside that pane.
        let recovered_agent_id = preferred_agent_id
            .filter(|agent_id| is_qmux_agent_id(agent_id))
            .map(ToString::to_string);
        prepare_agent_workspace_locked(
            state,
            PrepareAgentWorkspaceRequest {
                group_id: Some(pane.group_id.clone()),
                base_repo: Some(pane.cwd.clone()),
                base_ref: Some("HEAD".to_string()),
                adapter: adapter_id.to_string(),
                model: None,
                effort: None,
                // This is an agent running inside the shell's own directory, not a
                // qmux-managed isolated worktree.
                use_worktree: false,
            },
            recovered_agent_id,
        )?
    };
    let attached = attach_agent_pane(state, &agent.id, pane.id.clone())?;
    let recovered = state
        .set_agent_status(&attached.id, AgentStatus::Idle)?
        .ok_or_else(|| {
            format!(
                "agent {} disappeared during SessionStart recovery",
                attached.id
            )
        })?;
    // Emit the recovered binding before the caller ingests the notification and
    // starts a transcript-tail thread. Its first read may immediately emit the
    // full history, and the frontend must know which right pane owns those
    // turns first.
    state.emit(QmuxEvent::new(
        "agent.spawned",
        Some(pane.id.clone()),
        Some(recovered.id.clone()),
        json!({ "agent": &recovered, "source": "session_start_recovery" }),
    ));
    Ok(recovered)
}

fn is_qmux_agent_id(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("agent-") else {
        return false;
    };
    let mut parts = rest.split('-');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(millis), Some(sequence), None)
            if !millis.is_empty()
                && !sequence.is_empty()
                && millis.bytes().all(|byte| byte.is_ascii_digit())
                && sequence.bytes().all(|byte| byte.is_ascii_digit())
    )
}

pub fn attach_agent_pane(
    state: &AppState,
    agent_id: &str,
    pane_id: String,
) -> Result<AgentInfo, String> {
    for previous in state.list_agents()? {
        if previous.id != agent_id && previous.pane_id.as_deref() == Some(&pane_id) {
            let has_queue = !state.list_agent_turn_queue(&previous.id)?.is_empty();
            let orphaned_queue_pane_id = has_queue.then(|| pane_id.clone());
            let detached = state.mutate_agent(&previous.id, |agent| {
                agent.pane_id = None;
                agent.orphaned_queue_pane_id = orphaned_queue_pane_id;
                agent.status = AgentStatus::Idle;
            })?;
            if let Some(detached) = detached {
                let detached_id = detached.id.clone();
                // Clear the pane binding before cancelling the barrier. Otherwise a
                // concurrent fork-dispatch finalizer can observe the missing barrier
                // while the old source still owns this pane and drain post-fork input
                // into the terminal being replaced.
                state.cancel_agent_fork_barrier_for_source(&detached_id)?;
                state.emit(crate::events::QmuxEvent::new(
                    "agent.detached",
                    Some(pane_id.clone()),
                    Some(detached_id.clone()),
                    serde_json::json!({
                        "agent": detached,
                        "replacementAgentId": agent_id,
                    }),
                ));
                crate::turn_queue::release_waiters_for_agent(state, &detached_id)?;
            }
        }
    }

    // Field-scoped mutation, not a full-struct `update_agent`: a freshly spawned agent's
    // SessionStart hook may be recording its session_id / transcript_path on another
    // thread right now, and a stale-snapshot write here would race it and wipe them. Only
    // touch the pane-binding fields.
    state
        .mutate_agent(agent_id, |agent| {
            agent.pane_id = Some(pane_id);
            agent.orphaned_queue_pane_id = None;
            agent.status = AgentStatus::Running;
        })?
        .ok_or_else(|| format!("agent {agent_id} was not found"))
}

/// Detaches whatever agent is currently bound to `pane_id` (if any), reverting the
/// pane to a plain shell. Used when a shell-launched agent's *process* exits but its
/// host shell — and so the pane — lives on: there is no `pty.exit` to reap the agent
/// in that case, so without this the agent would linger bound to the pane and the tab
/// would keep showing its last status (e.g. a freshly launched, never-used agent's
/// synthetic "Awaiting input") indefinitely. Mirrors the rebind detach in
/// `attach_agent_pane`: the binding is cleared and the agent goes Idle, but an agent
/// with queued turns keeps them parked as an orphaned queue so they stay
/// restart-recoverable. Emits `agent.detached` with the updated agent and returns it;
/// a no-op (`Ok(None)`) when no agent owns the pane.
pub fn detach_pane_agent(state: &AppState, pane_id: &str) -> Result<Option<AgentInfo>, String> {
    let Some(current) = state.agent_by_pane(pane_id)? else {
        return Ok(None);
    };
    detach_known_pane_agent(state, pane_id, current)
}

fn detach_known_pane_agent(
    state: &AppState,
    pane_id: &str,
    current: AgentInfo,
) -> Result<Option<AgentInfo>, String> {
    let has_queue = !state.list_agent_turn_queue(&current.id)?.is_empty();
    let orphaned_queue_pane_id = has_queue.then(|| pane_id.to_string());
    let detached = state.mutate_agent(&current.id, |agent| {
        agent.pane_id = None;
        agent.orphaned_queue_pane_id = orphaned_queue_pane_id;
        agent.status = AgentStatus::Idle;
    })?;
    if let Some(detached) = &detached {
        // Make the source visibly detached before removing its barrier so neither the
        // dispatch owner nor a child-ready hook can resume into the old pane during
        // the cancellation window.
        state.cancel_agent_fork_barrier_for_source(&detached.id)?;
        // Every caller reaches this only after the supervised adapter process has
        // exited (the host shell may remain). A queued fork that never reached its
        // prompt hook can therefore release its source safely; no live child remains
        // to copy more context.
        if let Err(err) = crate::turn_queue::abort_fork_barrier_for_child(
            state,
            &detached.id,
            "Forked terminal exited before its initial prompt was accepted",
        ) {
            eprintln!(
                "qmux: failed to release fork barrier for detached agent {}: {err}",
                detached.id
            );
        }
        state.emit(crate::events::QmuxEvent::new(
            "agent.detached",
            Some(pane_id.to_string()),
            Some(detached.id.clone()),
            serde_json::json!({ "agent": detached }),
        ));
        crate::turn_queue::release_waiters_for_agent(state, &detached.id)?;
    }
    Ok(detached)
}

/// Detaches only the named agent when it still owns `pane_id`. Shell jobs can
/// outlive their foreground tenure: an older background job exiting must never
/// detach a newer agent that the user launched in the same shell meanwhile.
pub fn detach_pane_agent_if_matches(
    state: &AppState,
    pane_id: &str,
    expected_agent_id: &str,
) -> Result<Option<AgentInfo>, String> {
    let Some(current) = state.agent_by_pane(pane_id)? else {
        return Ok(None);
    };
    if current.id != expected_agent_id {
        return Ok(None);
    }
    // Use the agent resolved above rather than looking the pane binding up again:
    // a replacement can attach between these operations, and a second lookup could
    // otherwise detach that newer primary on behalf of this stale job.
    detach_known_pane_agent(state, pane_id, current)
}

pub fn mark_agent_spawn_failed(
    state: &AppState,
    agent_id: &str,
    reserved_pane_id: &str,
) -> Result<AgentInfo, String> {
    // No release of waiters here: a failed target intentionally keeps its waiters
    // blocked (see `queued_turn_wait_is_resolved_locked`), so a dependent turn doesn't
    // silently fire when the agent it was waiting on errored out instead of finishing.
    state
        .mutate_agent(agent_id, |agent| {
            if agent.pane_id.as_deref() == Some(reserved_pane_id) {
                agent.pane_id = None;
                agent.orphaned_queue_pane_id = None;
            }
            agent.status = AgentStatus::Failed;
        })?
        .ok_or_else(|| format!("agent {agent_id} was not found"))
}

pub fn mark_agent_failed(state: &AppState, agent_id: &str) -> Result<AgentInfo, String> {
    // No release of waiters here, by design: a failed target keeps its waiters blocked
    // (see `mark_agent_spawn_failed` and `queued_turn_wait_is_resolved_locked`).
    // Field-scoped write (only status): a full-struct `update_agent` drops the lock
    // between read and write and could clobber a concurrent SessionStart hook's
    // session_id/transcript_path, leaving the session unresumable.
    state
        .mutate_agent(agent_id, |agent| {
            agent.status = AgentStatus::Failed;
        })?
        .ok_or_else(|| format!("agent {agent_id} was not found"))
}

pub fn acknowledge_agent(
    state: &AppState,
    agent_id: &str,
    include_failed: bool,
) -> Result<AgentInfo, String> {
    // Only acknowledge a Done/Failed agent, so an unrelated snapshot doesn't trigger a
    // no-op persist. Re-check the status inside the field-scoped mutation so the write
    // is atomic and can't clobber a concurrent hook's session fields (cf.
    // `mark_agent_failed`).
    let snapshot = state
        .agent(agent_id)?
        .ok_or_else(|| format!("agent {agent_id} was not found"))?;
    let should_ack = matches!(snapshot.status, AgentStatus::Done)
        || (include_failed && matches!(snapshot.status, AgentStatus::Failed));
    if !should_ack {
        return Ok(snapshot);
    }

    let acked = std::cell::Cell::new(false);
    let agent = state
        .mutate_agent(agent_id, |agent| {
            if matches!(agent.status, AgentStatus::Done)
                || (include_failed && matches!(agent.status, AgentStatus::Failed))
            {
                agent.status = AgentStatus::Idle;
                acked.set(true);
            }
        })?
        .ok_or_else(|| format!("agent {agent_id} was not found"))?;
    if acked.get() {
        state.emit(crate::events::QmuxEvent::new(
            "agent.acknowledged",
            agent.pane_id.clone(),
            Some(agent.id.clone()),
            serde_json::json!({ "agent": agent.clone() }),
        ));
        crate::turn_queue::release_waiters_for_agent(state, &agent.id)?;
    }
    Ok(agent)
}

pub fn clear_agent_working_status(state: &AppState, agent_id: &str) -> Result<AgentInfo, String> {
    let agent = state
        .agent(agent_id)?
        .ok_or_else(|| format!("agent {agent_id} was not found"))?;
    if !matches!(agent.status, AgentStatus::Starting | AgentStatus::Running) {
        return Ok(agent);
    }
    // Treat a manual clear as the agent having gone idle and route it through the same
    // completion path the adapters use, instead of blindly forcing a fresh Idle. Forcing
    // Idle leaves a queued turn stranded (the next idle that would have drained it never
    // fires), drops pending-pause handling, and persists a "ready to send" status that
    // sends the user's next turn straight into the process rather than queuing it.
    // advance_after_idle drains a queued turn, honors a pending pause / active typing, and
    // lands on Done (or Running if a turn drained) with field-scoped writes.
    crate::turn_queue::advance_after_idle(state, agent_id)?;
    let agent = state
        .agent(agent_id)?
        .ok_or_else(|| format!("agent {agent_id} was not found"))?;
    state.emit(crate::events::QmuxEvent::new(
        "agent.working_status_cleared",
        agent.pane_id.clone(),
        Some(agent.id.clone()),
        serde_json::json!({ "agent": agent.clone() }),
    ));
    Ok(agent)
}

/// How long after a lone Esc keystroke into a working agent's pane qmux waits for
/// counter-evidence before concluding the user interrupted the turn.
const ESC_INTERRUPT_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Adapters whose TUI interrupts the running turn on a *lone* Esc, verified to emit
/// no Stop hook / transcript line / idle notification when interrupted mid-thinking.
/// Only these get the escape watch below. Codex, for instance, requires Esc twice —
/// running the watch there would demote a still-working agent on every single Esc.
/// Others can opt in once their Esc semantics are confirmed.
fn adapter_interrupts_on_lone_escape(adapter: &str) -> bool {
    adapter == "claude"
}

/// Handles a lone Esc typed into a working agent's pane. In the agent TUIs qmux
/// hosts (see `adapter_interrupts_on_lone_escape`), Esc while a turn is running means
/// "interrupt" — and a turn canceled during its thinking phase produces no Stop hook,
/// no transcript line, and no idle notification (verified against Claude Code 2.1.x),
/// so nothing would ever demote the agent from Running and the transcript pane would
/// show "Working…" forever.
///
/// Watch the agent for a short grace window; if no hook or transcript activity lands,
/// mark it AwaitingInput. The watch itself does not drain the queue (unlike the hard
/// idle signals routed through advance_after_idle) — but AwaitingInput is a ready
/// status, so a later drain trigger can still send a queued turn. That is correct
/// after a real interrupt; the residual risk is a *wrong* demotion (an Esc that
/// dismissed a menu mid-run), which the activity-counter guard makes rare and which
/// the next hook/transcript event self-corrects by re-promoting to Running.
pub fn watch_agent_after_escape(state: &AppState, pane_id: &str) {
    let Ok(Some(agent)) = state.agent_by_pane(pane_id) else {
        return;
    };
    if !adapter_interrupts_on_lone_escape(&agent.adapter) {
        return;
    }
    if !matches!(agent.status, AgentStatus::Starting | AgentStatus::Running) {
        return;
    }
    let Ok(baseline) = state.agent_activity_seq(&agent.id) else {
        return;
    };
    // Dedupe: a held Esc (key repeat) calls this per keystroke. Spawn one watcher per
    // burst rather than a thread each, all racing to demote the same agent.
    if !state.begin_agent_escape_watch(&agent.id) {
        return;
    }
    let state = state.clone();
    let agent_id = agent.id;
    std::thread::spawn(move || {
        std::thread::sleep(ESC_INTERRUPT_GRACE);
        let resolved = resolve_agent_escape_watch(&state, &agent_id, baseline);
        state.end_agent_escape_watch(&agent_id);
        if let Some(agent) = resolved {
            state.emit(crate::events::QmuxEvent::new(
                "agent.interrupted",
                agent.pane_id.clone(),
                Some(agent.id.clone()),
                serde_json::json!({ "agent": agent }),
            ));
        }
    });
}

/// The post-grace half of [`watch_agent_after_escape`]: demotes the agent to
/// AwaitingInput and returns it if it is still in a working status with no activity
/// recorded since `baseline`; returns `None` when the watch should stand down.
fn resolve_agent_escape_watch(
    state: &AppState,
    agent_id: &str,
    baseline: u64,
) -> Option<AgentInfo> {
    let agent = state.agent(agent_id).ok().flatten()?;
    if !matches!(agent.status, AgentStatus::Starting | AgentStatus::Running) {
        return None;
    }
    // Any agent mutation or transcript write during the grace window means the
    // turn is still alive (or something else already resolved the status).
    if state.agent_activity_seq(agent_id).ok() != Some(baseline) {
        return None;
    }
    state
        .set_agent_status(agent_id, AgentStatus::AwaitingInput)
        .ok()
        .flatten()
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeStatus {
    pub has_changes: bool,
    pub changed_files: usize,
}

#[derive(Clone, Debug)]
pub struct CapturedWorktreeRemoval {
    run_dir: String,
    worktree_dir: String,
    branch: String,
    /// Captured with the paths rather than re-resolved later: removal runs
    /// after the agent record is gone, so the host has to be carried along.
    host: Host,
}

/// The host an agent's work runs on, which is its group's.
///
/// Derived rather than stored: the workspace is what is bound to a machine, so
/// reading it from the group is what makes it impossible for an agent to drift
/// onto a different host from its own worktree.
fn agent_host(state: &AppState, agent: &AgentInfo) -> Result<Host, String> {
    let group = state
        .group(&agent.group_id)?
        .ok_or_else(|| format!("group {} was not found", agent.group_id))?;
    Ok(host::for_group(group.remote.as_ref()))
}

/// Reports whether an agent's git worktree has uncommitted changes — staged,
/// unstaged, or untracked — so closing a tab can warn before that work is gone.
pub fn agent_worktree_status(state: &AppState, agent_id: &str) -> Result<WorktreeStatus, String> {
    let agent = state
        .agent(agent_id)?
        .ok_or_else(|| format!("agent {agent_id} was not found"))?;
    let host = agent_host(state, &agent)?;
    let dir = agent.worktree_dir;
    let output = host
        .git(["-C", dir.as_str(), "status", "--porcelain"])
        .output()
        .map_err(|err| format!("failed to run git status in {dir}: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git status failed in {dir} on {}: {}",
            host.label(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let changed_files = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    Ok(WorktreeStatus {
        has_changes: changed_files > 0,
        changed_files,
    })
}

/// Removes an agent's git worktree with `--force`, discarding any uncommitted
/// changes, then soft-deletes its branch (`git branch -d`). Because `-d` only
/// removes a fully-merged branch, any committed-but-unmerged work is preserved —
/// git refuses and the branch is kept. Runs from the group's base repository so
/// git is never asked to remove the worktree it is standing in.
pub fn remove_agent_worktree(state: &AppState, agent_id: &str) -> Result<(), String> {
    let agent = state
        .agent(agent_id)?
        .ok_or_else(|| format!("agent {agent_id} was not found"))?;
    let removal = capture_agent_worktree_removal(state, &agent)?;
    remove_captured_worktree(removal)?;
    state.clear_last_closed_pane_for_agent(agent_id);
    Ok(())
}

pub fn capture_agent_worktree_removal(
    state: &AppState,
    agent: &AgentInfo,
) -> Result<CapturedWorktreeRemoval, String> {
    let Some(branch) = agent.branch.clone() else {
        return Err(format!("agent {} is not in a git worktree", agent.id));
    };
    let worktree_dir = agent.worktree_dir.clone();
    let removal_host = agent_host(state, agent)?;

    let run_dir = state
        .group(&agent.group_id)?
        .and_then(|group| group.base_repo)
        .filter(|repo| is_git_repo(&removal_host, repo))
        .unwrap_or_else(|| worktree_dir.clone());

    Ok(CapturedWorktreeRemoval {
        run_dir,
        worktree_dir,
        branch,
        host: removal_host,
    })
}

pub fn remove_captured_worktree(removal: CapturedWorktreeRemoval) -> Result<(), String> {
    let CapturedWorktreeRemoval {
        run_dir,
        worktree_dir,
        branch,
        host,
    } = removal;

    let output = host
        .git([
            "-C",
            run_dir.as_str(),
            "worktree",
            "remove",
            "--force",
            worktree_dir.as_str(),
        ])
        .output()
        .map_err(|err| format!("failed to run git worktree remove: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "git worktree remove failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    // Best-effort: the worktree is gone, so the branch can be soft-deleted. A
    // declined delete (unmerged commits) or a git error is logged, not fatal —
    // the worktree removal the user confirmed has already succeeded.
    match soft_delete_branch(&host, &run_dir, &branch) {
        Ok(true) => {}
        Ok(false) => eprintln!("qmux: kept branch {branch}: not fully merged"),
        Err(err) => eprintln!("qmux: {err}"),
    }

    Ok(())
}

/// Soft-deletes `branch` in the repository at `run_dir`. Returns `Ok(true)` if
/// the branch was removed, `Ok(false)` if git declined because it is not fully
/// merged, or `Err` if git could not be run.
fn soft_delete_branch(host: &Host, run_dir: &str, branch: &str) -> Result<bool, String> {
    let output = host
        .git(["-C", run_dir, "branch", "-d", branch])
        .output()
        .map_err(|err| format!("failed to run git branch -d {branch}: {err}"))?;
    Ok(output.status.success())
}

/// The directory a launched agent works in when the caller doesn't specify one:
/// the group's directory if it still exists, else the app default.
fn default_base_repo(state: &AppState) -> Option<String> {
    Some(state.default_open_dir().display().to_string())
}

fn canonical_dir(path: &str) -> Result<PathBuf, String> {
    let canonical =
        fs::canonicalize(path).map_err(|err| format!("failed to resolve {path}: {err}"))?;
    if !canonical.is_dir() {
        return Err(format!("{} is not a directory", canonical.display()));
    }
    Ok(canonical)
}

/// Group-aware directory canonicalization: the single place that decides how a
/// group's directory strings are resolved and validated. Every path a group
/// stores must pass through here (or `group_recoverable_dir`) rather than a
/// bare local canonicalize/stat, because a remote group's paths live on its
/// host — a local stat would spuriously fail and silently redirect the caller
/// to a local fallback directory.
pub fn group_canonical_dir(remote: Option<&RemoteRef>, path: &str) -> Result<PathBuf, String> {
    match remote {
        // Remote paths cannot be resolved locally. Until a remote transport
        // exists, accept the string as given; the spawn layer refuses to
        // launch into remote groups, so nothing acts on the unverified path.
        Some(_) => Ok(PathBuf::from(path)),
        None => canonical_dir(path),
    }
}

/// Group-aware companion to `pty::recoverable_dir`: returns the path only when
/// it still resolves for the host the group's processes run on. Local groups
/// stat the local filesystem; remote groups pass the string through untouched
/// for the same reason as `group_canonical_dir`.
pub fn group_recoverable_dir(remote: Option<&RemoteRef>, path: &str) -> Option<PathBuf> {
    match remote {
        Some(_) => Some(PathBuf::from(path)),
        None => crate::pty::recoverable_dir(path),
    }
}

fn group_name_for_dir(dir: &Path) -> String {
    dir.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| {
            if dir.parent().is_none() {
                "Root".to_string()
            } else {
                default_group_name()
            }
        })
}

/// A friendly, human-readable name for a new group / worktree, e.g.
/// "brave-otter" (two hyphenated words). Falls back to a timestamped name only
/// if generation somehow yields nothing, which the bundled word lists make
/// unreachable in practice.
fn default_group_name() -> String {
    names::Generator::default()
        .next()
        .unwrap_or_else(|| format!("group-{}", now_millis()))
}

fn unique_group_dir(root: &Path, requested_name: &str) -> Result<PathBuf, String> {
    unique_dir(root, requested_name, "group")
}

fn unique_worktree_dir(root: &Path, requested_name: &str) -> Result<PathBuf, String> {
    // Leave room for unique_dir's largest collision suffix ("-999") under
    // the common 255-byte filesystem component limit. Group display names are
    // user-controlled and may be much longer than a valid filename.
    unique_dir(root, &bounded_path_segment(requested_name, 240), "worktree")
}

fn validate_worktree_name(requested_name: &str) -> Result<&str, String> {
    let name = requested_name.trim();
    if name.is_empty() {
        return Err("worktree name cannot be empty".to_string());
    }
    if name.len() > 240 {
        return Err("worktree name must be at most 240 characters".to_string());
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(
            "worktree name may contain only letters, numbers, hyphens, and underscores".to_string(),
        );
    }
    Ok(name)
}

fn unique_dir(root: &Path, requested_name: &str, kind: &str) -> Result<PathBuf, String> {
    let base = sanitize_path_segment(requested_name);
    for index in 0..1000 {
        let name = if index == 0 {
            base.clone()
        } else {
            format!("{base}-{index}")
        };
        let candidate = root.join(name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "failed to allocate a unique {kind} directory under {}",
        root.display()
    ))
}

fn allocate_agent_worktree_dir(
    state: &AppState,
    host: &Host,
    base_repo: Option<&str>,
    group: &GroupInfo,
    agent_name: &str,
) -> Result<PathBuf, String> {
    // Preserve the existing compact agent paths for remote/global storage;
    // project-local storage includes the group label to avoid collisions across
    // groups sharing the same repository root.
    if let Some(root) = host.remote_workspace_root()? {
        return Ok(PathBuf::from(root).join(&group.id).join(agent_name));
    }
    let location = persistence::load_preferences(&state.config().workspace_root)?
        .worktree_location
        .unwrap_or_default();
    if location == WorktreeLocation::Global {
        return Ok(PathBuf::from(&group.managed_dir).join(agent_name));
    }
    let root = worktree_root(state, host, base_repo, group)?;
    let display_name = group.name_override.as_deref().unwrap_or(&group.name);
    unique_worktree_dir(&root, &format!("{display_name}-{agent_name}"))
}

fn worktree_root(
    state: &AppState,
    host: &Host,
    base_repo: Option<&str>,
    group: &GroupInfo,
) -> Result<PathBuf, String> {
    // A remote worktree has to live under the *host's* root. The group's
    // managed directory and the project-local placements below are all resolved
    // against this filesystem — canonicalizing a remote path here would silently
    // produce a local directory and the agent would edit the wrong tree.
    if let Some(root) = host.remote_workspace_root()? {
        return Ok(PathBuf::from(root).join(&group.id));
    }

    let location = persistence::load_preferences(&state.config().workspace_root)?
        .worktree_location
        .unwrap_or_default();
    if location == WorktreeLocation::Global {
        return Ok(PathBuf::from(&group.managed_dir));
    }

    let base_repo = base_repo.ok_or_else(|| {
        "cannot resolve a project-local worktree location without a project directory".to_string()
    })?;
    let is_git = is_git_repo(host, base_repo);
    let project_root = if is_git {
        git_project_root(host, base_repo)?
    } else {
        fs::canonicalize(base_repo)
            .map_err(|err| format!("failed to resolve project directory {base_repo}: {err}"))?
    };
    let (relative_root, exclude_pattern) = match location {
        WorktreeLocation::Global => unreachable!(),
        WorktreeLocation::LocalQmux => (Path::new(".qmux/worktrees"), "/.qmux/worktrees/"),
        WorktreeLocation::LocalClaude => (Path::new(".claude/worktrees"), "/.claude/worktrees/"),
    };

    if is_git {
        ensure_git_local_exclude(host, base_repo, exclude_pattern)?;
    }
    let root = project_root.join(relative_root);
    fs::create_dir_all(&root)
        .map_err(|err| format!("failed to create worktree root {}: {err}", root.display()))?;
    Ok(root)
}

fn allocate_named_worktree_dir(
    state: &AppState,
    host: &Host,
    base_repo: &str,
    group: &GroupInfo,
    requested_name: &str,
) -> Result<PathBuf, String> {
    let name = validate_worktree_name(requested_name)?;
    let dir = worktree_root(state, host, Some(base_repo), group)?.join(name);
    if host.is_local() && dir.exists() {
        return Err(format!("a file or directory named {name:?} already exists"));
    }
    Ok(dir)
}

fn git_project_root(host: &Host, base_repo: &str) -> Result<PathBuf, String> {
    let output = host
        .git(["-C", base_repo, "rev-parse", "--show-toplevel"])
        .output()
        .map_err(|err| format!("failed to resolve git project root: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "failed to resolve git project root: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        return Err("git returned an empty project root".to_string());
    }
    Ok(PathBuf::from(root))
}

fn ensure_git_local_exclude(host: &Host, base_repo: &str, pattern: &str) -> Result<(), String> {
    let output = host
        .git(["-C", base_repo, "rev-parse", "--git-path", "info/exclude"])
        .output()
        .map_err(|err| format!("failed to resolve git exclude path: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "failed to resolve git exclude path: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let raw_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if raw_path.is_empty() {
        return Err("git returned an empty exclude path".to_string());
    }
    let path = {
        let candidate = PathBuf::from(raw_path);
        if candidate.is_absolute() {
            candidate
        } else {
            PathBuf::from(base_repo).join(candidate)
        }
    };
    let existing = match fs::read_to_string(&path) {
        Ok(existing) => existing,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    if existing.lines().any(|line| line.trim() == pattern) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        file.write_all(b"\n")
            .map_err(|err| format!("failed to update {}: {err}", path.display()))?;
    }
    writeln!(file, "{pattern}").map_err(|err| format!("failed to update {}: {err}", path.display()))
}

fn sanitize_path_segment(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();

    if sanitized.is_empty() {
        default_group_name()
    } else {
        sanitized
    }
}

fn bounded_path_segment(value: &str, max_bytes: usize) -> String {
    let sanitized = sanitize_path_segment(value);
    if sanitized.len() <= max_bytes {
        return sanitized;
    }

    // FNV-1a is sufficient here: the hash only keeps two long names with the
    // same truncated prefix distinguishable; unique_dir still handles an
    // actual collision. sanitize_path_segment emits ASCII, so byte slicing is
    // safe and exactly matches filesystem component accounting.
    let hash = value
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    let suffix = format!("-{hash:016x}");
    let prefix_len = max_bytes.saturating_sub(suffix.len());
    format!("{}{}", &sanitized[..prefix_len], suffix)
}

fn sanitize_ref_segment(value: &str) -> String {
    sanitize_path_segment(value)
}

fn is_git_repo(host: &Host, path: &str) -> bool {
    host.git(["-C", path, "rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn git_rev_parse_field(host: &Host, cwd: &str, spec: &str) -> Result<String, String> {
    let output = host
        .git(["-C", cwd, "rev-parse", spec])
        .output()
        .map_err(|err| format!("failed to run git rev-parse {spec}: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse {spec} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        return Err(format!("git rev-parse {spec} returned an empty path"));
    }
    Ok(value)
}

fn git_rev_parse_path(host: &Host, cwd: &str, spec: &str) -> Result<PathBuf, String> {
    let raw = git_rev_parse_field(host, cwd, spec)?;
    let path = PathBuf::from(&raw);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(Path::new(cwd).join(path))
    }
}

/// The repository's primary checkout. A linked worktree's `--show-toplevel` is
/// the worktree itself; allocating under that would nest `.qmux/worktrees`
/// inside another worktree. Prefer the parent of `--git-common-dir` when that
/// is the main `.git`.
///
/// Git dir probes run from `--show-toplevel`, not the caller's possibly nested
/// cwd. `--git-common-dir` is often the relative `.git`; joining that with a
/// subdirectory would invent `<subdir>/.git` and mis-classify a main checkout.
pub(crate) fn git_main_checkout(host: &Host, cwd: &str) -> Result<PathBuf, String> {
    if !is_git_repo(host, cwd) {
        return Err("this tab is not inside a git repository".to_string());
    }
    let current = git_project_root(host, cwd)?;
    let current_str = current
        .to_str()
        .ok_or_else(|| "current checkout path is not valid UTF-8".to_string())?;
    let git_dir = git_rev_parse_path(host, current_str, "--absolute-git-dir")?;
    let common_dir = git_rev_parse_path(host, current_str, "--git-common-dir")?;
    if git_dir == common_dir {
        return Ok(current);
    }
    match common_dir.file_name().and_then(|name| name.to_str()) {
        Some(".git") => {
            let parent = common_dir
                .parent()
                .ok_or_else(|| "could not resolve the repository's main checkout".to_string())?;
            let parent_str = parent
                .to_str()
                .ok_or_else(|| "main checkout path is not valid UTF-8".to_string())?;
            git_project_root(host, parent_str)
        }
        _ => Err("could not resolve the repository's main checkout".to_string()),
    }
}

/// Creates a linked worktree from the current checkout's HEAD and returns its
/// path. Allocation uses the main checkout so project-local worktrees do not
/// nest inside an existing linked worktree.
pub fn suggested_shell_worktree_name(group: &GroupInfo) -> String {
    let display_name = group.name_override.as_deref().unwrap_or(&group.name);
    bounded_path_segment(&format!("{display_name}-{}", default_group_name()), 240)
}

pub fn create_shell_worktree(
    state: &AppState,
    host: &Host,
    group: &GroupInfo,
    seed_cwd: &str,
    requested_name: &str,
) -> Result<PathBuf, String> {
    let _guard = AGENT_WORKSPACE_CREATION_LOCK
        .lock()
        .map_err(|_| "agent workspace creation lock poisoned".to_string())?;
    if !is_git_repo(host, seed_cwd) {
        return Err("this tab is not inside a git repository".to_string());
    }
    let current_checkout = git_project_root(host, seed_cwd)?;
    let main_checkout = git_main_checkout(host, seed_cwd)?;
    let current = current_checkout
        .to_str()
        .ok_or_else(|| "current checkout path is not valid UTF-8".to_string())?;
    let main = main_checkout
        .to_str()
        .ok_or_else(|| "main checkout path is not valid UTF-8".to_string())?;
    let name = validate_worktree_name(requested_name)?;
    let dir = allocate_named_worktree_dir(state, host, main, group, name)?;
    let head = git_rev_parse_field(host, current, "HEAD")?;
    let branch = format!(
        "qmux/{}/{}",
        sanitize_ref_segment(&group.id),
        sanitize_ref_segment(&name)
    );
    create_worktree(host, current, &dir, &branch, &head)?;
    Ok(dir)
}

/// Resolves a local command cwd into display-only workspace metadata. A cwd
/// outside Git is still useful to show, while Git's own plumbing determines
/// whether a checkout is the repository's primary tree or a linked worktree.
pub fn resolve_active_workspace(
    cwd: &str,
    source: ActiveWorkspaceSource,
    managed_by_qmux: bool,
) -> Option<ActiveWorkspace> {
    if !Path::new(cwd).is_absolute() {
        return None;
    }
    let reported = cwd.to_string();
    let canonical = fs::canonicalize(cwd).ok()?;
    if !canonical.is_dir() {
        return None;
    }
    let Some(git_root) = local_git_field(&canonical, &["rev-parse", "--show-toplevel"]) else {
        return Some(ActiveWorkspace {
            cwd: reported,
            git_root: None,
            branch: None,
            kind: ActiveWorkspaceKind::Directory,
            source,
            managed_by_qmux,
        });
    };
    let git_root = fs::canonicalize(&git_root).unwrap_or_else(|_| PathBuf::from(&git_root));
    let branch = local_git_field(&canonical, &["symbolic-ref", "--quiet", "--short", "HEAD"]);
    let git_dir = local_git_path(&canonical, &["rev-parse", "--absolute-git-dir"]);
    // Older Git releases do not support `--path-format=absolute`. Resolve the
    // command's relative output against its cwd instead of turning a plumbing
    // compatibility failure into a false linked-worktree claim.
    let common_dir = local_git_path(&canonical, &["rev-parse", "--git-common-dir"]);
    let kind = classify_git_checkout(git_dir.as_deref(), common_dir.as_deref());

    Some(ActiveWorkspace {
        cwd: reported,
        git_root: Some(git_root.display().to_string()),
        branch,
        kind,
        source,
        managed_by_qmux,
    })
}

fn classify_git_checkout(git_dir: Option<&Path>, common_dir: Option<&Path>) -> ActiveWorkspaceKind {
    match (git_dir, common_dir) {
        (Some(git_dir), Some(common_dir)) if git_dir == common_dir => {
            ActiveWorkspaceKind::MainCheckout
        }
        (Some(_), Some(_)) => ActiveWorkspaceKind::LinkedWorktree,
        _ => ActiveWorkspaceKind::GitCheckout,
    }
}

fn local_git_field(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn local_git_path(cwd: &Path, args: &[&str]) -> Option<PathBuf> {
    let path = PathBuf::from(local_git_field(cwd, args)?);
    let path = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    fs::canonicalize(path).ok()
}

/// Applies an adapter observation only for local agents and emits a surgical
/// update when the normalized workspace actually changes. The launch workspace
/// remains authoritative for lifecycle operations.
pub fn record_agent_active_workspace(
    state: &AppState,
    agent_id: &str,
    transcript_path: &str,
    tail_generation: u64,
    cwd: &str,
    source: ActiveWorkspaceSource,
) -> Result<Option<AgentInfo>, String> {
    let Some(current) = state.agent(agent_id)? else {
        return Ok(None);
    };
    if !agent_host(state, &current)?.is_local() {
        return Ok(None);
    }

    let mut workspace = match resolve_active_workspace(cwd, source, false) {
        Some(workspace) => workspace,
        None => return Ok(None),
    };
    workspace.managed_by_qmux = current.branch.is_some()
        && workspace.git_root.as_deref().is_some_and(|root| {
            fs::canonicalize(root).ok() == fs::canonicalize(&current.worktree_dir).ok()
        });
    let updated = state.set_agent_active_workspace_for_transcript(
        agent_id,
        transcript_path,
        tail_generation,
        workspace,
    )?;
    if let Some(agent) = updated.as_ref() {
        state.emit(QmuxEvent::new(
            "agent.workspace_changed",
            agent.pane_id.clone(),
            Some(agent.id.clone()),
            json!({ "agent": agent }),
        ));
    }
    Ok(updated)
}

fn create_worktree(
    host: &Host,
    base_repo: &str,
    worktree_dir: &Path,
    branch: &str,
    base_ref: &str,
) -> Result<(), String> {
    // base_ref arrives from the frontend. Resolve it to a real commit first so an
    // option-looking value (e.g. "--detach") can't be interpreted by git as a flag
    // rather than a starting point. `--end-of-options` keeps a ref beginning with
    // "-" from being parsed as an option by rev-parse itself.
    verify_base_ref(host, base_repo, base_ref)?;

    let output = host
        .git([
            "-C",
            base_repo,
            "worktree",
            "add",
            "-b",
            branch,
            // Stop option parsing before the user-influenced positional
            // arguments so neither the worktree path nor the ref can be
            // mistaken for a flag. On a remote host `Host::git` additionally
            // quotes each one, so the far side's shell cannot re-split them
            // and undo this guard.
            "--",
            &worktree_dir.display().to_string(),
            base_ref,
        ])
        .output()
        .map_err(|err| format!("failed to run git worktree add: {err}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git worktree add failed on {}: {}{}",
            host.label(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

/// Verifies that `base_ref` resolves to a commit in `base_repo`, rejecting values
/// that don't (including option-looking input). `--end-of-options` ensures a ref
/// starting with "-" is treated as a revision rather than a flag.
fn verify_base_ref(host: &Host, base_repo: &str, base_ref: &str) -> Result<(), String> {
    let resolved = host
        .git([
            "-C",
            base_repo,
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{base_ref}^{{commit}}"),
        ])
        .output()
        .map_err(|err| format!("failed to verify base ref: {err}"))?;

    if resolved.status.success() {
        Ok(())
    } else {
        Err(format!("base ref '{base_ref}' did not resolve to a commit"))
    }
}

pub(crate) fn write_group_manifest(group: &GroupInfo) -> Result<(), String> {
    let manifest_path = PathBuf::from(&group.managed_dir).join(".qmux/group.json");
    let parent = manifest_path
        .parent()
        .ok_or_else(|| format!("group manifest {} has no parent", manifest_path.display()))?;
    fs::create_dir_all(parent).map_err(|err| {
        format!(
            "failed to create group manifest directory {}: {err}",
            parent.display()
        )
    })?;
    remove_stale_manifest_tmp_files(parent);
    let raw = serde_json::to_string_pretty(group)
        .map_err(|err| format!("failed to encode group manifest: {err}"))?;
    let seq = MANIFEST_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".group.json.tmp-{}-{seq}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|err| format!("failed to create {}: {err}", tmp.display()))?;
    if let Err(err) = file
        .write_all(raw.as_bytes())
        .and_then(|()| file.sync_all())
    {
        let _ = fs::remove_file(&tmp);
        return Err(format!("failed to write {}: {err}", tmp.display()));
    }
    fs::rename(&tmp, &manifest_path).map_err(|err| {
        let _ = fs::remove_file(&tmp);
        format!("failed to commit {}: {err}", manifest_path.display())
    })?;
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Whether `dir` holds nothing besides an entry named `allowed`. A missing
/// directory holds nothing and so cannot make a scaffold non-pristine; any
/// other read failure reads as "unknown contents" rather than "empty", so an
/// unreadable directory is never mistaken for a removable one.
fn dir_holds_nothing_but(dir: &Path, allowed: &str) -> bool {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => return err.kind() == std::io::ErrorKind::NotFound,
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        if entry.file_name() != allowed {
            return false;
        }
    }
    true
}

/// Removes the manifest-only directory allocated for a group whose creation is
/// being rolled back. All-or-nothing: the whole scaffold is confirmed pristine
/// before any part of it is removed, so if another task raced in a worktree or
/// any other content, the managed directory is left exactly as found — manifest
/// included, so it stays self-describing — rather than half-dismantled just to
/// complete best-effort cleanup.
pub(crate) fn remove_pristine_group_scaffold(group: &GroupInfo) {
    if group.managed_dir == group.dir {
        return;
    }
    let managed_dir = PathBuf::from(&group.managed_dir);
    let qmux_dir = managed_dir.join(".qmux");
    if !dir_holds_nothing_but(&managed_dir, ".qmux")
        || !dir_holds_nothing_but(&qmux_dir, "group.json")
    {
        return;
    }
    let manifest = qmux_dir.join("group.json");
    if let Err(err) = fs::remove_file(&manifest)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!(
            "qmux: failed to remove rolled-back group manifest {}: {err}",
            manifest.display()
        );
        return;
    }
    // Content racing in after the check above surfaces here as
    // DirectoryNotEmpty. The directory is preserved either way; say so rather
    // than swallowing it, since by this point the check claimed it was empty.
    if let Err(err) = fs::remove_dir(&qmux_dir)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!(
            "qmux: failed to remove rolled-back group metadata directory {}: {err}",
            qmux_dir.display()
        );
        return;
    }
    if let Err(err) = fs::remove_dir(&managed_dir)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!(
            "qmux: failed to remove rolled-back group directory {}: {err}",
            managed_dir.display()
        );
    }
}

/// Removes manifest scratch files (`.group.json.tmp-<pid>-<seq>`) stranded by
/// a writer that died between creating its temp and renaming it into place —
/// nothing else ever revisits them. Runs on the next manifest write for the
/// group, with the same best-effort pid-liveness contract as
/// `persistence::remove_stale_tmp_files`.
fn remove_stale_manifest_tmp_files(parent: &Path) {
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(rest) = name.strip_prefix(".group.json.tmp-") else {
            continue;
        };
        let Some((pid, seq)) = rest.split_once('-') else {
            continue;
        };
        if seq.parse::<u64>().is_err() {
            continue;
        }
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        if pid != std::process::id() && !crate::persistence::process_is_alive(pid) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AdapterConfigs, ClaudeAdapterConfig, CodexAdapterConfig, GrokAdapterConfig,
        MuseAdapterConfig, OpencodeAdapterConfig, QmuxConfig,
    };
    use std::process::Command;

    fn test_remote() -> RemoteRef {
        RemoteRef {
            id: "remote-1".to_string(),
            label: "workbox".to_string(),
            host: "workbox".to_string(),
            multiplexer: RemoteMultiplexer::Tmux,
            qmux_cli: None,
            workspace_root: None,
        }
    }

    // The group-aware path helpers are the chokepoint keeping local stats away
    // from remote paths: a remote group's directory must pass through
    // unstatted, while local groups keep the strict canonicalize/exists
    // behavior. A silent local fallback here is the failure mode the whole
    // consolidation exists to prevent.
    #[test]
    fn group_path_helpers_never_stat_remote_paths_locally() {
        let remote = test_remote();
        let missing = "/no/such/dir/on/this/machine";
        assert_eq!(
            group_canonical_dir(Some(&remote), missing).unwrap(),
            PathBuf::from(missing)
        );
        assert!(group_canonical_dir(None, missing).is_err());
        assert_eq!(
            group_recoverable_dir(Some(&remote), missing),
            Some(PathBuf::from(missing))
        );
        assert_eq!(group_recoverable_dir(None, missing), None);
    }

    fn remote_host(root: Option<&str>) -> Host {
        host::for_group(Some(&RemoteRef {
            id: "saved-1".to_string(),
            label: "devbox".to_string(),
            host: "user@devbox".to_string(),
            multiplexer: RemoteMultiplexer::Tmux,
            qmux_cli: None,
            workspace_root: root.map(str::to_string),
        }))
    }

    #[test]
    fn a_remote_id_binds_the_group_to_that_machine() {
        // Without this the whole feature can be a no-op: the request is
        // accepted, a group is created, and it is silently local.
        let root = std::env::temp_dir().join("qmux-remote-id");
        let state = test_state_with_remotes(
            root,
            std::collections::BTreeMap::from([(
                "devbox".to_string(),
                crate::config::SavedRemote {
                    host: "user@devbox".to_string(),
                    label: Some("Dev box".to_string()),
                    ..Default::default()
                },
            )]),
        );

        let group = create_group(
            &state,
            CreateGroupRequest {
                name: None,
                dir: Some("/srv/code/project".to_string()),
                after_group_id: None,
                base_repo: None,
                base_ref: None,
                remote: None,
                remote_id: Some("devbox".to_string()),
            },
        )
        .expect("creates");

        let remote = group.remote.as_ref().expect("bound to the remote");
        assert_eq!(remote.id, "devbox");
        assert_eq!(remote.host, "user@devbox");
        assert_eq!(remote.label, "Dev box");
        assert!(group.is_remote());
    }

    #[test]
    fn an_unknown_remote_id_fails_before_a_group_exists() {
        let root = std::env::temp_dir().join("qmux-remote-id-unknown");
        let state = test_state_with_workspace(root);
        let before = state.list_groups().expect("groups").len();

        let err = create_group(
            &state,
            CreateGroupRequest {
                name: None,
                dir: Some("/tmp".to_string()),
                after_group_id: None,
                base_repo: None,
                base_ref: None,
                remote: None,
                remote_id: Some("nope".to_string()),
            },
        )
        .expect_err("unknown remote");

        assert!(err.contains("nope"), "{err}");
        // A half-created local group would be worse than the error.
        assert_eq!(state.list_groups().expect("groups").len(), before);
    }

    #[test]
    fn a_remote_worktree_is_allocated_under_the_host_root() {
        let root = std::env::temp_dir().join("qmux-remote-alloc");
        let state = test_state_with_workspace(root);
        let mut group =
            allocation_group(Path::new("/srv/code/project"), Path::new("/local/managed"));
        group.id = "group-9".to_string();
        group.managed_dir = "/local/managed/group-9".to_string();

        let dir = allocate_agent_worktree_dir(
            &state,
            &remote_host(Some("/srv/qmux")),
            Some("/srv/code/project"),
            &group,
            "agent-1",
        )
        .expect("allocates");

        // The group's managed directory is a path on *this* machine. Using it
        // would put the worktree where the remote agent cannot reach it — which
        // is exactly what happened before the host was threaded through here.
        assert_eq!(dir, PathBuf::from("/srv/qmux/group-9/agent-1"));
        assert!(!dir.starts_with("/local"), "must not land locally: {dir:?}");
    }

    #[test]
    fn a_remote_host_without_a_root_still_lands_remotely() {
        let root = std::env::temp_dir().join("qmux-remote-alloc-default");
        let state = test_state_with_workspace(root);
        // The default root is `~/.qmux/workspaces`, and every argument sent
        // over ssh is quoted, so it has to be expanded here or the worktree
        // lands in a directory literally named `~`.
        host::seed_remote_home("user@devbox", "/home/dev");
        let dir = allocate_agent_worktree_dir(
            &state,
            &remote_host(None),
            Some("/srv/x"),
            &allocation_group(Path::new("/srv/x"), Path::new("/local/managed")),
            "a",
        )
        .expect("allocates");
        assert!(dir.starts_with("/home/dev/.qmux/workspaces"), "{dir:?}");
        assert!(dir.is_absolute(), "the bridge requires an absolute cwd");
    }

    fn test_state_with_workspace(workspace_root: PathBuf) -> AppState {
        test_state_with_remotes(workspace_root, Default::default())
    }

    fn test_state_with_remotes(
        workspace_root: PathBuf,
        remotes: std::collections::BTreeMap<String, crate::config::SavedRemote>,
    ) -> AppState {
        std::fs::create_dir_all(&workspace_root).unwrap();
        let socket_path = workspace_root.join("qmux.sock");
        AppState::new(QmuxConfig {
            remotes,
            workspace_root,
            socket_path,
            adapters: AdapterConfigs {
                acp: Default::default(),
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
            claude_plugin_dir: std::path::PathBuf::new(),
            opencode_plugin_dir: std::path::PathBuf::new(),
            pi_extension_dir: std::path::PathBuf::new(),
            cursor_plugin_dir: std::path::PathBuf::new(),
        })
    }

    fn test_state() -> AppState {
        test_state_with_workspace(PathBuf::from("/tmp/qmux-workspace-tests"))
    }

    fn temp_workspace(prefix: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("qmux-workspace-{prefix}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn active_workspace_resolves_main_checkout_linked_worktree_and_nested_cwd() {
        let workspace = temp_workspace("active-worktree");
        let repo = workspace.join("repo");
        let linked = workspace.join("linked");
        let nested = linked.join("packages/app");
        let nested_main = repo.join("packages/app");
        fs::create_dir_all(&repo).unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .output()
                .expect("git runs");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "qmux test"]);
        git(&repo, &["commit", "--allow-empty", "-m", "init"]);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature/test",
                linked.to_str().unwrap(),
            ],
        );
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(&nested_main).unwrap();

        let main =
            resolve_active_workspace(repo.to_str().unwrap(), ActiveWorkspaceSource::Qmux, false)
                .unwrap();
        let canonical_repo = fs::canonicalize(&repo).unwrap();
        let canonical_linked = fs::canonicalize(&linked).unwrap();
        assert_eq!(main.kind, ActiveWorkspaceKind::MainCheckout);
        assert_eq!(main.git_root.as_deref(), canonical_repo.to_str());
        assert_eq!(main.branch.as_deref(), Some("main"));

        let current =
            resolve_active_workspace(nested.to_str().unwrap(), ActiveWorkspaceSource::Codex, true)
                .unwrap();
        assert_eq!(current.kind, ActiveWorkspaceKind::LinkedWorktree);
        assert_eq!(current.cwd, nested.to_str().unwrap());
        assert_eq!(
            fs::canonicalize(&current.cwd).unwrap(),
            fs::canonicalize(&nested).unwrap()
        );
        assert_eq!(current.git_root.as_deref(), canonical_linked.to_str());
        assert_eq!(current.branch.as_deref(), Some("feature/test"));
        assert_eq!(current.source, ActiveWorkspaceSource::Codex);
        assert!(current.managed_by_qmux);

        let main_from_nested = git_main_checkout(&Host::Local, nested.to_str().unwrap()).unwrap();
        let main_from_nested_main =
            git_main_checkout(&Host::Local, nested_main.to_str().unwrap()).unwrap();
        let main_from_repo = git_main_checkout(&Host::Local, repo.to_str().unwrap()).unwrap();
        assert_eq!(fs::canonicalize(&main_from_nested).unwrap(), canonical_repo);
        assert_eq!(
            fs::canonicalize(&main_from_nested_main).unwrap(),
            canonical_repo
        );
        assert_eq!(fs::canonicalize(&main_from_repo).unwrap(), canonical_repo);

        fs::remove_dir_all(workspace).ok();
    }

    #[test]
    fn git_main_checkout_rejects_a_directory_outside_git() {
        let directory = temp_workspace("not-git");
        let err = git_main_checkout(&Host::Local, directory.to_str().unwrap()).unwrap_err();
        assert!(err.contains("not inside a git repository"), "{err}");
        fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn create_shell_worktree_uses_main_checkout_and_current_head() {
        let workspace = temp_workspace("open-worktree");
        let repo = workspace.join("repo");
        let linked = workspace.join("linked");
        let managed = workspace.join("managed/group");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&managed).unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .output()
                .expect("git runs");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "qmux test"]);
        git(&repo, &["commit", "--allow-empty", "-m", "init"]);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature/open-worktree",
                linked.to_str().unwrap(),
            ],
        );
        git(&linked, &["commit", "--allow-empty", "-m", "feature"]);
        let feature_head = String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(&linked)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        let main_head = String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        assert_ne!(feature_head, main_head);

        let state = test_state_with_workspace(workspace.join("state"));
        persistence::save_preferences(
            &state.config().workspace_root,
            &persistence::AppPreferences {
                worktree_location: Some(WorktreeLocation::LocalQmux),
                ..Default::default()
            },
        )
        .unwrap();
        let group = allocation_group(&repo, &managed);
        let created = create_shell_worktree(
            &state,
            &Host::Local,
            &group,
            linked.to_str().unwrap(),
            "qmux-feature-shell",
        )
        .unwrap();
        let canonical_repo = fs::canonicalize(&repo).unwrap();
        let canonical_created = fs::canonicalize(&created).unwrap();
        assert_eq!(
            created.file_name().and_then(|name| name.to_str()),
            Some("qmux-feature-shell")
        );
        assert!(
            canonical_created.starts_with(canonical_repo.join(".qmux/worktrees")),
            "{canonical_created:?}"
        );
        assert!(
            !canonical_created.starts_with(fs::canonicalize(&linked).unwrap()),
            "must not nest inside the linked worktree: {canonical_created:?}"
        );
        let created_head = String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(&created)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        assert_eq!(created_head, feature_head);

        let collision = create_shell_worktree(
            &state,
            &Host::Local,
            &group,
            linked.to_str().unwrap(),
            "qmux-feature-shell",
        )
        .unwrap_err();
        assert!(collision.contains("already exists"), "{collision}");
        assert!(!repo.join(".qmux/worktrees/qmux-feature-shell-1").exists());

        persistence::save_preferences(
            &state.config().workspace_root,
            &persistence::AppPreferences {
                worktree_location: Some(WorktreeLocation::Global),
                ..Default::default()
            },
        )
        .unwrap();
        let global_created = create_shell_worktree(
            &state,
            &Host::Local,
            &group,
            linked.to_str().unwrap(),
            "qmux-global-shell",
        )
        .unwrap();
        assert_eq!(global_created, managed.join("qmux-global-shell"));

        let err = create_shell_worktree(
            &state,
            &Host::Local,
            &group,
            workspace.to_str().unwrap(),
            "qmux-invalid-shell",
        )
        .unwrap_err();
        assert!(err.contains("not inside a git repository"), "{err}");

        let invalid_name = create_shell_worktree(
            &state,
            &Host::Local,
            &group,
            linked.to_str().unwrap(),
            "feature/with-slash",
        )
        .unwrap_err();
        assert!(invalid_name.contains("letters, numbers"), "{invalid_name}");

        fs::remove_dir_all(workspace).ok();
    }

    #[test]
    fn suggested_shell_worktree_name_includes_the_group_name() {
        let group = allocation_group(Path::new("/tmp/qmux"), Path::new("/tmp/managed"));
        let name = suggested_shell_worktree_name(&group);
        assert!(name.starts_with("brave-otter-"), "{name}");
        assert!(validate_worktree_name(&name).is_ok());
    }

    #[test]
    fn active_workspace_keeps_non_git_command_directories_visible() {
        let directory = temp_workspace("active-directory");
        let current = resolve_active_workspace(
            directory.to_str().unwrap(),
            ActiveWorkspaceSource::Claude,
            false,
        )
        .unwrap();

        assert_eq!(current.kind, ActiveWorkspaceKind::Directory);
        assert_eq!(current.cwd, directory.to_str().unwrap());
        assert_eq!(current.git_root, None);
        assert_eq!(current.branch, None);
        fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn active_workspace_rejects_relative_or_missing_command_directories() {
        assert_eq!(
            resolve_active_workspace("relative/path", ActiveWorkspaceSource::Codex, false),
            None
        );
        assert_eq!(
            resolve_active_workspace(
                "/definitely/missing/qmux-command-directory",
                ActiveWorkspaceSource::Claude,
                false,
            ),
            None
        );
    }

    #[test]
    fn incomplete_git_plumbing_never_claims_a_linked_worktree() {
        let git_dir = Path::new("/repo/.git");
        assert_eq!(
            classify_git_checkout(Some(git_dir), None),
            ActiveWorkspaceKind::GitCheckout
        );
        assert_eq!(
            classify_git_checkout(None, Some(git_dir)),
            ActiveWorkspaceKind::GitCheckout
        );
    }

    #[test]
    fn rolled_back_group_scaffold_is_removed_when_pristine() {
        let workspace = temp_workspace("rollback-pristine");
        let project = workspace.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let state = test_state_with_workspace(workspace);
        let group = create_group_record(
            &state,
            CreateGroupRequest {
                remote_id: None,
                name: None,
                dir: Some(project.display().to_string()),
                after_group_id: None,
                base_repo: None,
                base_ref: None,
                remote: None,
            },
            WorkspaceScope::Terminal,
        )
        .unwrap();
        let managed_dir = PathBuf::from(&group.managed_dir);
        assert!(managed_dir.join(".qmux/group.json").is_file());

        remove_pristine_group_scaffold(&group);

        assert!(!managed_dir.exists());
    }

    #[test]
    fn rolled_back_group_scaffold_preserves_raced_in_content() {
        let workspace = temp_workspace("rollback-race");
        let project = workspace.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let state = test_state_with_workspace(workspace);
        let group = create_group_record(
            &state,
            CreateGroupRequest {
                remote_id: None,
                name: None,
                dir: Some(project.display().to_string()),
                after_group_id: None,
                base_repo: None,
                base_ref: None,
                remote: None,
            },
            WorkspaceScope::Terminal,
        )
        .unwrap();
        let managed_dir = PathBuf::from(&group.managed_dir);
        let marker = managed_dir.join("raced-worktree/keep");
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, "keep").unwrap();

        remove_pristine_group_scaffold(&group);

        assert_eq!(std::fs::read_to_string(marker).unwrap(), "keep");
        assert!(managed_dir.is_dir());
        // All-or-nothing: a directory that survives keeps the manifest that
        // says what it is, rather than being left unidentifiable.
        assert!(managed_dir.join(".qmux/group.json").is_file());
    }

    // The same guarantee one level down: scratch left in `.qmux` by a manifest
    // writer that died mid-rename must not cost the group its manifest either.
    #[test]
    fn rolled_back_group_scaffold_preserves_stranded_manifest_scratch() {
        let workspace = temp_workspace("rollback-scratch");
        let project = workspace.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let state = test_state_with_workspace(workspace);
        let group = create_group_record(
            &state,
            CreateGroupRequest {
                remote_id: None,
                name: None,
                dir: Some(project.display().to_string()),
                after_group_id: None,
                base_repo: None,
                base_ref: None,
                remote: None,
            },
            WorkspaceScope::Terminal,
        )
        .unwrap();
        let managed_dir = PathBuf::from(&group.managed_dir);
        let scratch = managed_dir.join(".qmux/.group.json.tmp-1234-0");
        std::fs::write(&scratch, "partial").unwrap();

        remove_pristine_group_scaffold(&group);

        assert!(scratch.is_file());
        assert!(managed_dir.join(".qmux/group.json").is_file());
    }

    fn sample_agent(id: &str, pane_id: Option<&str>, status: AgentStatus) -> AgentInfo {
        AgentInfo {
            acp_config_options: Vec::new(),
            acp_agent: None,
            id: id.to_string(),
            group_id: "group-1".to_string(),
            adapter: "claude".to_string(),
            worktree_dir: "/tmp/qmux-workspace-tests".to_string(),
            branch: None,
            active_workspace: None,
            pane_id: pane_id.map(ToString::to_string),
            orphaned_queue_pane_id: None,
            session_id: None,
            transcript_path: None,
            status,
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

    #[test]
    fn agent_info_deserializes_without_active_workspace() {
        let mut value = serde_json::to_value(sample_agent("agent-1", None, AgentStatus::Idle))
            .expect("serializes");
        value
            .as_object_mut()
            .expect("agent object")
            .remove("activeWorkspace");

        let restored: AgentInfo = serde_json::from_value(value).expect("legacy payload loads");
        assert_eq!(restored.active_workspace, None);
    }

    #[test]
    fn latest_local_command_workspace_replaces_the_previous_observation() {
        let root = temp_workspace("latest-active-command");
        let first = root.join("first");
        let second = root.join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let state = test_state_with_workspace(root.join("state"));
        state
            .insert_group_after(allocation_group(&first, &root.join("managed")), None)
            .unwrap();
        let mut agent = sample_agent("agent-1", None, AgentStatus::Running);
        agent.worktree_dir = first.display().to_string();
        agent.transcript_path = Some("/tmp/live-transcript.jsonl".to_string());
        state.insert_agent(agent).unwrap();
        let (tail_generation, _) = state
            .mark_transcript_tail("agent-1", "/tmp/live-transcript.jsonl", true)
            .unwrap()
            .unwrap();

        record_agent_active_workspace(
            &state,
            "agent-1",
            "/tmp/live-transcript.jsonl",
            tail_generation,
            first.to_str().unwrap(),
            ActiveWorkspaceSource::Claude,
        )
        .unwrap();
        record_agent_active_workspace(
            &state,
            "agent-1",
            "/tmp/live-transcript.jsonl",
            tail_generation,
            second.to_str().unwrap(),
            ActiveWorkspaceSource::Codex,
        )
        .unwrap();

        let current = state
            .agent("agent-1")
            .unwrap()
            .unwrap()
            .active_workspace
            .unwrap();
        assert_eq!(
            fs::canonicalize(&current.cwd).unwrap(),
            fs::canonicalize(&second).unwrap()
        );
        assert_eq!(current.cwd, second.to_str().unwrap());
        assert_eq!(current.source, ActiveWorkspaceSource::Codex);

        let (replacement_generation, _) = state
            .mark_transcript_tail("agent-1", "/tmp/live-transcript.jsonl", false)
            .unwrap()
            .unwrap();
        let superseded_update = record_agent_active_workspace(
            &state,
            "agent-1",
            "/tmp/live-transcript.jsonl",
            tail_generation,
            first.to_str().unwrap(),
            ActiveWorkspaceSource::Claude,
        )
        .unwrap();
        assert!(superseded_update.is_none());

        state
            .mutate_agent("agent-1", |agent| {
                agent.transcript_path = Some("/tmp/new-transcript.jsonl".to_string());
            })
            .unwrap();
        let stale_update = record_agent_active_workspace(
            &state,
            "agent-1",
            "/tmp/live-transcript.jsonl",
            replacement_generation,
            first.to_str().unwrap(),
            ActiveWorkspaceSource::Claude,
        )
        .unwrap();
        assert!(stale_update.is_none());
        assert_eq!(
            state
                .agent("agent-1")
                .unwrap()
                .unwrap()
                .active_workspace
                .unwrap()
                .cwd,
            second.to_str().unwrap()
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn remote_agent_transcript_cwds_are_ignored() {
        let root = temp_workspace("remote-active-command");
        let directory = root.join("local-lookalike");
        fs::create_dir_all(&directory).unwrap();
        let state = test_state_with_workspace(root.join("state"));
        let mut group = allocation_group(&directory, &root.join("managed"));
        group.remote = Some(test_remote());
        state.insert_group_after(group, None).unwrap();
        let mut agent = sample_agent("agent-1", None, AgentStatus::Running);
        agent.worktree_dir = "/srv/code/project".to_string();
        agent.transcript_path = Some("/tmp/live-transcript.jsonl".to_string());
        state.insert_agent(agent).unwrap();
        let (tail_generation, _) = state
            .mark_transcript_tail("agent-1", "/tmp/live-transcript.jsonl", true)
            .unwrap()
            .unwrap();

        let update = record_agent_active_workspace(
            &state,
            "agent-1",
            "/tmp/live-transcript.jsonl",
            tail_generation,
            directory.to_str().unwrap(),
            ActiveWorkspaceSource::Claude,
        )
        .unwrap();

        assert!(update.is_none());
        assert_eq!(
            state.agent("agent-1").unwrap().unwrap().active_workspace,
            None
        );
        fs::remove_dir_all(root).ok();
    }

    fn sample_shell_pane(id: &str, group_id: &str, cwd: &Path) -> crate::state::PaneInfo {
        crate::state::PaneInfo {
            id: id.to_string(),
            title: "Shell".to_string(),
            last_osc_title: None,
            kind: crate::state::PaneKind::Shell,
            agent_id: None,
            group_id: group_id.to_string(),
            cwd: cwd.display().to_string(),
            cols: 80,
            rows: 24,
            status: crate::state::PaneStatus::Running,
            last_active_at: 1,
            recovered: false,
            depth: 0,
        }
    }

    #[test]
    fn session_start_recovery_creates_one_idle_shell_agent() {
        let workspace = temp_workspace("session-start-recovery");
        let project = workspace.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let state = test_state_with_workspace(workspace.join("managed"));
        let group = create_group(
            &state,
            CreateGroupRequest {
                remote_id: None,
                name: None,
                dir: Some(project.display().to_string()),
                after_group_id: None,
                base_repo: Some(project.display().to_string()),
                base_ref: Some("HEAD".to_string()),
                remote: None,
            },
        )
        .unwrap();
        let pane = sample_shell_pane("pane-shell", &group.id, &project);

        let first =
            recover_shell_agent_from_session_start(&state, &pane, "codex", Some("agent-123-456"))
                .unwrap();
        let second = recover_shell_agent_from_session_start(
            &state,
            &pane,
            "codex",
            Some("stale-prepared-id"),
        )
        .unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(first.id, "agent-123-456");
        assert_eq!(first.pane_id.as_deref(), Some("pane-shell"));
        assert_eq!(first.status, AgentStatus::Idle);
        assert_eq!(first.adapter, "codex");
        assert_eq!(first.worktree_dir, project.display().to_string());
        assert_eq!(state.list_agents().unwrap().len(), 1);
        assert_eq!(
            state.group(&group.id).unwrap().unwrap().agents,
            vec![first.id]
        );
    }

    #[test]
    fn session_start_recovery_refuses_non_shell_panes() {
        let workspace = temp_workspace("session-start-agent-pane");
        let project = workspace.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let state = test_state_with_workspace(workspace.join("managed"));
        let group = create_group(
            &state,
            CreateGroupRequest {
                remote_id: None,
                name: None,
                dir: Some(project.display().to_string()),
                after_group_id: None,
                base_repo: Some(project.display().to_string()),
                base_ref: Some("HEAD".to_string()),
                remote: None,
            },
        )
        .unwrap();
        let mut pane = sample_shell_pane("pane-agent", &group.id, &project);
        pane.kind = crate::state::PaneKind::Agent;

        let err = recover_shell_agent_from_session_start(&state, &pane, "codex", None).unwrap_err();

        assert!(err.contains("non-shell pane"), "unexpected error: {err}");
        assert!(state.list_agents().unwrap().is_empty());
    }

    #[test]
    fn session_start_recovery_only_preserves_qmux_agent_ids() {
        assert!(is_qmux_agent_id("agent-1784252381261-2616"));
        assert!(!is_qmux_agent_id("agent-stale"));
        assert!(!is_qmux_agent_id("../agent-1-2"));
        assert!(!is_qmux_agent_id("agent-1-2-extra"));
    }

    #[test]
    fn rename_group_sets_and_clears_name_override() {
        let workspace = temp_workspace("rename");
        let managed_root = workspace.join("managed");
        let source_dir = workspace.join("dirs/project");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::create_dir_all(&managed_root).unwrap();
        let state = test_state_with_workspace(managed_root);
        let group = create_group(
            &state,
            CreateGroupRequest {
                remote_id: None,
                name: None,
                dir: Some(source_dir.to_string_lossy().to_string()),
                after_group_id: None,
                base_repo: None,
                base_ref: None,
                remote: None,
            },
        )
        .unwrap();

        let renamed = rename_group(&state, &group.id, Some("  Research  ".to_string())).unwrap();

        assert_eq!(renamed.name_override.as_deref(), Some("Research"));
        let manifest_path = PathBuf::from(&renamed.managed_dir).join(".qmux/group.json");
        let manifest: GroupInfo =
            serde_json::from_str(&std::fs::read_to_string(manifest_path).unwrap()).unwrap();
        assert_eq!(manifest.name_override.as_deref(), Some("Research"));

        let cleared = rename_group(&state, &group.id, Some(" ".to_string())).unwrap();

        assert_eq!(cleared.name_override, None);
        std::fs::remove_dir_all(workspace).ok();
    }

    #[test]
    fn set_group_dir_preserves_name_override() {
        let workspace = temp_workspace("rename-dir");
        let managed_root = workspace.join("managed");
        let first_dir = workspace.join("dirs/first");
        let second_dir = workspace.join("dirs/second");
        std::fs::create_dir_all(&first_dir).unwrap();
        std::fs::create_dir_all(&second_dir).unwrap();
        std::fs::create_dir_all(&managed_root).unwrap();
        let state = test_state_with_workspace(managed_root);
        let group = create_group(
            &state,
            CreateGroupRequest {
                remote_id: None,
                name: None,
                dir: Some(first_dir.to_string_lossy().to_string()),
                after_group_id: None,
                base_repo: None,
                base_ref: None,
                remote: None,
            },
        )
        .unwrap();
        rename_group(&state, &group.id, Some("Research".to_string())).unwrap();

        let moved =
            set_group_dir(&state, &group.id, second_dir.to_string_lossy().to_string()).unwrap();

        assert_eq!(moved.name, "second");
        assert_eq!(moved.name_override.as_deref(), Some("Research"));
        std::fs::remove_dir_all(workspace).ok();
    }

    #[test]
    fn default_research_workspace_is_scoped_persistent_and_idempotent() {
        let root = std::env::temp_dir().join(format!("qmux-default-research-{}", now_millis()));
        let state = test_state_with_workspace(root.clone());

        let first = ensure_default_research_workspace(&state).unwrap();
        let second = ensure_default_research_workspace(&state).unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(first.scope, WorkspaceScope::Research);
        assert_eq!(
            std::fs::canonicalize(&first.dir).unwrap(),
            std::fs::canonicalize(state.default_research_dir()).unwrap()
        );
        assert!(Path::new(&first.dir).is_dir());
        assert_eq!(state.list_research_workspaces().unwrap().len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn chosen_research_workspace_reuses_the_same_directory() {
        let root = temp_workspace("chosen-research");
        let chosen = root.join("project");
        std::fs::create_dir_all(&chosen).unwrap();
        let state = test_state_with_workspace(root.join("managed"));

        let first =
            create_research_workspace(&state, None, chosen.to_string_lossy().into_owned()).unwrap();
        let second = create_research_workspace(
            &state,
            Some("Duplicate".to_string()),
            chosen.to_string_lossy().into_owned(),
        )
        .unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(state.list_research_workspaces().unwrap().len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_research_workspace_creation_converges_on_one_directory() {
        let root = temp_workspace("concurrent-research");
        let chosen = root.join("project");
        std::fs::create_dir_all(&chosen).unwrap();
        let state = std::sync::Arc::new(test_state_with_workspace(root.join("managed")));
        let mut workers = Vec::new();
        for _ in 0..4 {
            let state = state.clone();
            let chosen = chosen.clone();
            workers.push(std::thread::spawn(move || {
                create_research_workspace(&state, None, chosen.display().to_string())
                    .unwrap()
                    .id
            }));
        }
        let ids = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), 1);
        assert_eq!(state.list_research_workspaces().unwrap().len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removing_unused_research_folder_writes_nothing_into_it() {
        let root = temp_workspace("remove-empty-research");
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let state = test_state_with_workspace(root.join("managed"));
        state.restore_session();
        let workspace =
            create_research_workspace(&state, None, project.display().to_string()).unwrap();
        // Removal of a folder that never ran research must succeed even when
        // the folder itself cannot be written to.
        std::fs::set_permissions(&project, std::fs::Permissions::from_mode(0o555)).unwrap();

        let detached = remove_research_workspace(&state, &workspace.id).unwrap();

        std::fs::set_permissions(&project, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(detached.is_empty());
        assert!(state.group(&workspace.id).unwrap().is_none());
        assert!(!project.join(crate::persistence::STATE_DIR).exists());
        assert!(
            crate::research::read_detached_research(&project)
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removing_research_folder_survives_failed_run_with_pruned_transcript() {
        let root = temp_workspace("remove-research-pruned-transcript");
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let state = test_state_with_workspace(root.join("managed"));
        state.restore_session();
        let workspace =
            create_research_workspace(&state, None, project.display().to_string()).unwrap();
        let detail = state
            .create_research_tree(crate::research::CreateResearchTreeRequest {
                prompt: "Doomed question".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: workspace.id.clone(),
            })
            .unwrap();
        // A run that failed and whose adapter transcript has since been
        // pruned from disk: the node keeps a transcript_path that no longer
        // resolves to anything readable.
        let mut agent = sample_agent("research-agent", None, AgentStatus::Failed);
        agent.group_id = workspace.id.clone();
        agent.transcript_path = Some(root.join("pruned/session-gone.jsonl").display().to_string());
        state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-never-existed")
            .unwrap();
        let node = state.research_node(&detail.tree.root_node_id).unwrap();
        assert_eq!(node.status, crate::research::ResearchNodeStatus::Failed);
        assert!(node.transcript_path.is_some());

        remove_research_workspace(&state, &workspace.id).unwrap();

        assert!(state.group(&workspace.id).unwrap().is_none());
        let archive = crate::research::read_detached_research(&project)
            .unwrap()
            .expect("detached archive");
        assert_eq!(archive.archive.nodes.len(), 1);
        assert!(archive.responses.is_empty());

        // The node record itself (prompt, failure) still restores on reopen.
        let restored =
            create_research_workspace(&state, None, project.display().to_string()).unwrap();
        assert_ne!(restored.id, workspace.id);
        let restored_trees = state.list_research_trees_with_archived(true).unwrap();
        assert_eq!(restored_trees.len(), 1);
        let restored_detail = state.research_tree(&restored_trees[0].id).unwrap();
        assert_eq!(
            restored_detail.nodes[0].status,
            crate::research::ResearchNodeStatus::Failed
        );
        assert_eq!(restored_detail.nodes[0].prompt, "Doomed question");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reopening_folder_with_missing_archive_response_still_imports_history() {
        let root = temp_workspace("reopen-damaged-archive");
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let state = test_state_with_workspace(root.join("managed"));
        state.restore_session();
        let workspace =
            create_research_workspace(&state, None, project.display().to_string()).unwrap();
        let detail = state
            .create_research_tree(crate::research::CreateResearchTreeRequest {
                prompt: "Answered question".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: workspace.id.clone(),
            })
            .unwrap();
        let mut agent = sample_agent("research-agent", None, AgentStatus::Done);
        agent.group_id = workspace.id.clone();
        state
            .bind_research_node_run(&detail.tree.root_node_id, &agent, "pane-never-existed")
            .unwrap();
        let node_id = detail.tree.root_node_id.clone();
        assert_eq!(
            state.research_node(&node_id).unwrap().status,
            crate::research::ResearchNodeStatus::Complete
        );
        crate::research::write_response_snapshot(
            &state.config().workspace_root,
            &node_id,
            &[crate::transcript::Turn {
                id: "answer-1".to_string(),
                agent_id: "research-agent".to_string(),
                session_id: None,
                role: "assistant".to_string(),
                blocks: vec![crate::transcript::TurnBlock::Text {
                    text: "The answer".to_string(),
                }],
                source_index: 0,
                timestamp: None,
                status: None,
                status_reason: None,
                context_status: None,
                native_id: None,
                parent_native_id: None,
                native_message_id: None,
            }],
        )
        .unwrap();
        remove_research_workspace(&state, &workspace.id).unwrap();

        // A normally-detached Complete node carries its snapshot stamp; the
        // helper-written snapshot above bypassed the stamping path, so stamp
        // the archived record the way a real run would have.
        let manifest_path = project
            .join(crate::persistence::STATE_DIR)
            .join("research-v1/manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["nodes"][0]["responseSnapshotAt"] = serde_json::json!(2);
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

        // Simulate the archive losing the response file outside qmux — a
        // partial copy between machines, or a hand-deleted file.
        let response_file = project
            .join(crate::persistence::STATE_DIR)
            .join("research-v1/responses")
            .join(format!("{node_id}.json"));
        std::fs::remove_file(&response_file).unwrap();

        let restored =
            create_research_workspace(&state, None, project.display().to_string()).unwrap();

        let restored_trees = state.list_research_trees_with_archived(true).unwrap();
        assert_eq!(restored_trees.len(), 1);
        let restored_detail = state.research_tree(&restored_trees[0].id).unwrap();
        let restored_node = &restored_detail.nodes[0];
        assert_eq!(restored_node.group_id, restored.id);
        assert_eq!(
            restored_node.status,
            crate::research::ResearchNodeStatus::Complete
        );
        // The node must not claim a durable answer it no longer has.
        assert!(restored_node.response_snapshot_at.is_none());
        assert!(
            crate::research::read_response_snapshot(
                &state.config().workspace_root,
                &restored_node.id,
            )
            .unwrap()
            .is_none()
        );
        // The consumed archive is cleaned up like any successful import.
        assert!(
            crate::research::read_detached_research(&project)
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    fn sample_detached_manifest(folder: &Path) -> crate::research::DetachedResearchArchive {
        crate::research::DetachedResearchArchive {
            version: crate::research::DETACHED_RESEARCH_ARCHIVE_VERSION,
            archive_id: "archive-test".to_string(),
            workspace: GroupInfo {
                id: "group-detached".to_string(),
                name: "project".to_string(),
                name_override: None,
                dir: folder.display().to_string(),
                managed_dir: String::new(),
                base_repo: None,
                base_ref: Some("HEAD".to_string()),
                parent_id: None,
                created_at: 1,
                collapsed: false,
                scope: WorkspaceScope::Research,
                imported_research_archive_id: None,
                remote: None,
                agents: Vec::new(),
            },
            trees: Vec::new(),
            tree_order: Vec::new(),
            folders: Vec::new(),
            membership: std::collections::HashMap::new(),
            nodes: Vec::new(),
            exported_at: 1,
        }
    }

    #[test]
    fn opening_folder_with_unreadable_archive_names_the_archive_and_a_way_out() {
        let root = temp_workspace("open-unreadable-archive");
        let project = root.join("project");
        let archive_dir = project
            .join(crate::persistence::STATE_DIR)
            .join("research-v1");
        std::fs::create_dir_all(&archive_dir).unwrap();
        std::fs::write(archive_dir.join("manifest.json"), b"{ not json").unwrap();
        let state = test_state_with_workspace(root.join("managed"));
        state.restore_session();

        let error =
            create_research_workspace(&state, None, project.display().to_string()).unwrap_err();

        assert!(
            error.contains(&archive_dir.display().to_string()),
            "{error}"
        );
        assert!(error.contains("move"), "{error}");

        // An archive from a newer qmux gets the same actionable framing plus
        // the version hint.
        let mut manifest = serde_json::to_value(sample_detached_manifest(&project)).unwrap();
        manifest["version"] = serde_json::json!(999);
        std::fs::write(
            archive_dir.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let error =
            create_research_workspace(&state, None, project.display().to_string()).unwrap_err();
        assert!(error.contains("newer qmux"), "{error}");
        assert!(
            error.contains(&archive_dir.display().to_string()),
            "{error}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn moving_research_folder_repoints_record_and_leaves_both_folders_intact() {
        let root = temp_workspace("move-research");
        let source = root.join("source/project");
        let destination = root.join("destination/notes");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(source.join("draft.md"), "user content").unwrap();
        std::fs::write(destination.join("existing.md"), "already here").unwrap();
        let state = test_state_with_workspace(root.join("managed"));
        state.restore_session();
        let workspace =
            create_research_workspace(&state, None, source.display().to_string()).unwrap();
        rename_research_workspace(&state, &workspace.id, Some("My research".to_string())).unwrap();
        let detail = state
            .create_research_tree(crate::research::CreateResearchTreeRequest {
                prompt: "Question".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: workspace.id.clone(),
            })
            .unwrap();

        assert!(
            move_research_workspace(&state, &workspace.id, destination.display().to_string())
                .unwrap_err()
                .contains("active runs")
        );
        state
            .fail_research_node(&detail.tree.root_node_id, "settled".to_string())
            .unwrap();

        let moved =
            move_research_workspace(&state, &workspace.id, destination.display().to_string())
                .unwrap();

        let canonical_destination = std::fs::canonicalize(&destination).unwrap();
        assert_eq!(moved.dir, canonical_destination.display().to_string());
        assert_eq!(moved.name, "notes");
        assert_eq!(moved.name_override.as_deref(), Some("My research"));
        // The history never left global state; only the record moved.
        assert_eq!(
            state.list_research_trees_with_archived(true).unwrap().len(),
            1
        );
        let node = state.research_node(&detail.tree.root_node_id).unwrap();
        assert_eq!(
            node.worktree_dir,
            std::fs::canonicalize(&source)
                .unwrap()
                .display()
                .to_string()
        );
        // Neither folder gains or loses anything: no `.qmux` appears, the
        // source keeps its contents, and the destination keeps its own.
        assert!(source.join("draft.md").is_file());
        assert!(!source.join(crate::persistence::STATE_DIR).exists());
        assert!(destination.join("existing.md").is_file());
        assert!(!destination.join(crate::persistence::STATE_DIR).exists());
        // The repoint is durable.
        let persisted = persistence::load_with_diagnostics(&state.config().workspace_root).state;
        let persisted_group = persisted
            .groups
            .iter()
            .find(|group| group.id == workspace.id)
            .expect("moved workspace persisted");
        assert_eq!(persisted_group.dir, moved.dir);
        // Moving to the directory it already occupies is a no-op.
        let unchanged =
            move_research_workspace(&state, &workspace.id, destination.display().to_string())
                .unwrap();
        assert_eq!(unchanged.dir, moved.dir);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn moving_research_folder_refuses_occupied_or_history_bearing_destinations() {
        let root = temp_workspace("move-research-refusals");
        let source = root.join("project");
        let occupied = root.join("occupied");
        let with_history = root.join("with-history");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&occupied).unwrap();
        std::fs::create_dir_all(&with_history).unwrap();
        let state = test_state_with_workspace(root.join("managed"));
        state.restore_session();
        let workspace =
            create_research_workspace(&state, None, source.display().to_string()).unwrap();
        create_research_workspace(&state, None, occupied.display().to_string()).unwrap();
        // A folder that carries another workspace's detached history: run a
        // real detach into it.
        let removed_workspace =
            create_research_workspace(&state, None, with_history.display().to_string()).unwrap();
        let removed_detail = state
            .create_research_tree(crate::research::CreateResearchTreeRequest {
                prompt: "Detached question".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: removed_workspace.id.clone(),
            })
            .unwrap();
        state
            .fail_research_node(&removed_detail.tree.root_node_id, "settled".to_string())
            .unwrap();
        remove_research_workspace(&state, &removed_workspace.id).unwrap();
        assert!(
            crate::research::read_detached_research(&with_history)
                .unwrap()
                .is_some()
        );

        assert!(
            move_research_workspace(&state, &workspace.id, occupied.display().to_string())
                .unwrap_err()
                .contains("already open as research folder")
        );
        assert!(
            move_research_workspace(&state, &workspace.id, with_history.display().to_string())
                .unwrap_err()
                .contains("detached research history")
        );
        // The refused moves changed nothing: the record still points at the
        // source and the foreign archive is untouched.
        let unchanged = state.group(&workspace.id).unwrap().unwrap();
        assert_eq!(unchanged.dir, workspace.dir);
        assert!(
            crate::research::read_detached_research(&with_history)
                .unwrap()
                .is_some()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn moving_research_folder_carries_its_own_pending_archive() {
        let root = temp_workspace("move-research-pending");
        let source = root.join("project");
        let destination = root.join("elsewhere");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        let state = test_state_with_workspace(root.join("managed"));
        state.restore_session();
        let workspace =
            create_research_workspace(&state, None, source.display().to_string()).unwrap();
        let detail = state
            .create_research_tree(crate::research::CreateResearchTreeRequest {
                prompt: "Question".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: workspace.id.clone(),
            })
            .unwrap();
        state
            .fail_research_node(&detail.tree.root_node_id, "settled".to_string())
            .unwrap();
        // A removal interrupted before its global commit leaves the
        // workspace's own archive in pending form.
        let own_archive = state.detached_research_archive(&workspace.id).unwrap();
        crate::research::write_detached_research_pending(
            &source,
            &own_archive,
            &std::collections::HashMap::new(),
        )
        .unwrap();

        let moved =
            move_research_workspace(&state, &workspace.id, destination.display().to_string())
                .unwrap();

        // The pending archive traveled with the workspace, still pending —
        // never promoted, so a retried removal can still replace it.
        let carried = crate::research::read_detached_research(&destination)
            .unwrap()
            .expect("archive at destination");
        assert!(carried.pending);
        assert_eq!(carried.archive.workspace.id, workspace.id);
        assert!(
            crate::research::read_detached_research(&source)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            moved.dir,
            std::fs::canonicalize(&destination)
                .unwrap()
                .display()
                .to_string()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn moving_research_folder_leaves_foreign_archives_with_the_old_folder() {
        let root = temp_workspace("move-research-foreign");
        let source = root.join("project");
        let destination = root.join("elsewhere");
        let donor = root.join("donor");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::create_dir_all(&donor).unwrap();
        let state = test_state_with_workspace(root.join("managed"));
        state.restore_session();
        let workspace =
            create_research_workspace(&state, None, source.display().to_string()).unwrap();
        // Another workspace's detached history sitting in this folder does
        // not describe this workspace, so a move must not take it along.
        let donor_workspace =
            create_research_workspace(&state, None, donor.display().to_string()).unwrap();
        let donor_detail = state
            .create_research_tree(crate::research::CreateResearchTreeRequest {
                prompt: "Donor question".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: donor_workspace.id.clone(),
            })
            .unwrap();
        state
            .fail_research_node(&donor_detail.tree.root_node_id, "settled".to_string())
            .unwrap();
        let donor_archive = state
            .detached_research_archive(&donor_workspace.id)
            .unwrap();
        crate::research::write_detached_research_pending(
            &source,
            &donor_archive,
            &std::collections::HashMap::new(),
        )
        .unwrap();

        let moved =
            move_research_workspace(&state, &workspace.id, destination.display().to_string())
                .unwrap();

        assert_eq!(
            moved.dir,
            std::fs::canonicalize(&destination)
                .unwrap()
                .display()
                .to_string()
        );
        let left_behind = crate::research::read_detached_research(&source)
            .unwrap()
            .expect("foreign archive stays with the old folder");
        assert_eq!(left_behind.archive.workspace.id, donor_workspace.id);
        assert!(
            crate::research::read_detached_research(&destination)
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removing_research_folder_detaches_and_reopening_imports_its_history() {
        let root = temp_workspace("remove-research");
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let state = test_state_with_workspace(root.join("managed"));
        state.restore_session();
        let workspace =
            create_research_workspace(&state, None, project.display().to_string()).unwrap();
        let detail = state
            .create_research_tree(crate::research::CreateResearchTreeRequest {
                prompt: "Question".to_string(),
                title: None,
                adapter: "claude".to_string(),
                model: None,
                effort: None,
                group_id: workspace.id.clone(),
            })
            .unwrap();

        assert!(
            remove_research_workspace(&state, &workspace.id)
                .unwrap_err()
                .contains("active runs")
        );
        state
            .fail_research_node(&detail.tree.root_node_id, "settled".to_string())
            .unwrap();
        crate::research::write_response_snapshot(
            &state.config().workspace_root,
            &detail.tree.root_node_id,
            &[crate::transcript::Turn {
                id: "answer-1".to_string(),
                agent_id: "research-agent".to_string(),
                session_id: None,
                role: "assistant".to_string(),
                blocks: vec![crate::transcript::TurnBlock::Text {
                    text: "Portable answer".to_string(),
                }],
                source_index: 0,
                timestamp: None,
                status: None,
                status_reason: None,
                context_status: None,
                native_id: None,
                parent_native_id: None,
                native_message_id: None,
            }],
        )
        .unwrap();
        remove_research_workspace(&state, &workspace.id).unwrap();
        assert!(state.group(&workspace.id).unwrap().is_none());
        assert!(
            state
                .list_research_trees_with_archived(true)
                .unwrap()
                .is_empty()
        );
        let detached_state =
            persistence::load_with_diagnostics(&state.config().workspace_root).state;
        assert!(
            detached_state
                .groups
                .iter()
                .all(|group| group.id != workspace.id)
        );
        assert!(detached_state.research_trees.is_empty());
        assert!(detached_state.research_nodes.is_empty());
        assert!(
            crate::research::read_response_snapshot(
                &state.config().workspace_root,
                &detail.tree.root_node_id,
            )
            .unwrap()
            .is_none()
        );
        let archive = crate::research::read_detached_research(&project)
            .unwrap()
            .expect("detached archive");
        assert_eq!(archive.archive.trees.len(), 1);
        assert_eq!(archive.archive.nodes.len(), 1);
        assert_eq!(
            archive.responses[&detail.tree.root_node_id][0].id,
            "answer-1"
        );

        // Simulate a crash after the checked global commit but before the
        // pending archive was promoted. Opening the folder must recover this
        // form exactly like the final archive.
        let archive_parent = project.join(crate::persistence::STATE_DIR);
        std::fs::rename(
            archive_parent.join("research-v1"),
            archive_parent.join("research-v1.pending"),
        )
        .unwrap();

        let restored =
            create_research_workspace(&state, None, project.display().to_string()).unwrap();
        assert_ne!(restored.id, workspace.id);
        assert!(restored.imported_research_archive_id.is_none());
        let restored_trees = state.list_research_trees_with_archived(true).unwrap();
        assert_eq!(restored_trees.len(), 1);
        assert_eq!(restored_trees[0].title, detail.tree.title);
        let restored_detail = state.research_tree(&restored_trees[0].id).unwrap();
        let restored_response = crate::research::read_response_snapshot(
            &state.config().workspace_root,
            &restored_detail.tree.root_node_id,
        )
        .unwrap()
        .expect("restored response");
        assert_eq!(restored_response[0].id, "answer-1");
        let imported_state =
            persistence::load_with_diagnostics(&state.config().workspace_root).state;
        assert_eq!(imported_state.research_trees.len(), 1);
        assert_eq!(imported_state.research_nodes.len(), 1);
        assert!(
            crate::research::read_detached_research(&project)
                .unwrap()
                .is_none()
        );

        // Simulate a crash after the global import committed but before its
        // matching folder archive was deleted. The persisted receipt lets the
        // next startup remove exactly that duplicate and then retire itself.
        crate::research::write_detached_research_pending(
            &project,
            &archive.archive,
            &archive.responses,
        )
        .unwrap();
        crate::research::commit_detached_research(&project).unwrap();
        let mut cleanup_receipt = restored.clone();
        cleanup_receipt.imported_research_archive_id = Some(archive.archive.archive_id.clone());
        write_group_manifest(&cleanup_receipt).unwrap();
        state.update_group(cleanup_receipt).unwrap();
        reconcile_imported_research_archives(&state);
        assert!(
            crate::research::read_detached_research(&project)
                .unwrap()
                .is_none()
        );
        assert!(
            state
                .group(&restored.id)
                .unwrap()
                .unwrap()
                .imported_research_archive_id
                .is_none()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn launch_origin_enforces_workspace_scope_and_research_availability() {
        let root = temp_workspace("launch-origin");
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let state = test_state_with_workspace(root.join("managed"));
        let research =
            create_research_workspace(&state, None, project.display().to_string()).unwrap();
        assert!(
            validate_launch_workspace(&state, Some(&research.id), LaunchOrigin::Terminal)
                .unwrap_err()
                .contains("ordinary agents")
        );
        assert!(
            validate_launch_workspace(&state, Some(&research.id), LaunchOrigin::Research).is_ok()
        );
        std::fs::remove_dir_all(&project).unwrap();
        assert!(
            validate_launch_workspace(&state, Some(&research.id), LaunchOrigin::Research)
                .unwrap_err()
                .contains("unavailable")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn set_group_collapsed_persists_to_manifest() {
        let workspace = temp_workspace("collapse");
        let managed_root = workspace.join("managed");
        let source_dir = workspace.join("dirs/project");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::create_dir_all(&managed_root).unwrap();
        let state = test_state_with_workspace(managed_root);
        let group = create_group(
            &state,
            CreateGroupRequest {
                remote_id: None,
                name: None,
                dir: Some(source_dir.to_string_lossy().to_string()),
                after_group_id: None,
                base_repo: None,
                base_ref: None,
                remote: None,
            },
        )
        .unwrap();

        let collapsed = set_group_collapsed(&state, &group.id, true).unwrap();

        assert!(collapsed.collapsed);
        let manifest_path = PathBuf::from(&collapsed.managed_dir).join(".qmux/group.json");
        let manifest: GroupInfo =
            serde_json::from_str(&std::fs::read_to_string(manifest_path).unwrap()).unwrap();
        assert!(manifest.collapsed);
        std::fs::remove_dir_all(workspace).ok();
    }

    #[test]
    fn attach_agent_pane_detaches_previous_agent_for_same_pane() {
        let state = test_state();
        state
            .insert_agent(sample_agent(
                "agent-old",
                Some("pane-1"),
                AgentStatus::Running,
            ))
            .unwrap();
        state
            .insert_agent(sample_agent("agent-new", None, AgentStatus::Starting))
            .unwrap();
        state
            .enqueue_agent_turn("agent-old", "old queued turn".to_string())
            .unwrap();

        let attached = attach_agent_pane(&state, "agent-new", "pane-1".to_string()).unwrap();

        assert_eq!(attached.pane_id.as_deref(), Some("pane-1"));
        assert!(matches!(attached.status, AgentStatus::Running));
        let old = state.agent("agent-old").unwrap().expect("old agent exists");
        assert_eq!(old.pane_id, None);
        assert_eq!(old.orphaned_queue_pane_id.as_deref(), Some("pane-1"));
        assert!(matches!(old.status, AgentStatus::Idle));
        assert_eq!(
            state.list_agent_turn_queue("agent-old").unwrap(),
            vec!["old queued turn".to_string()]
        );
    }

    #[test]
    fn attach_agent_pane_preserves_session_id_and_transcript() {
        // attach binds the pane via a field-scoped write, so the session id/transcript a
        // freshly spawned agent's SessionStart hook records on another thread survive —
        // attach never touches those fields. (The clobber it guards against is a
        // concurrent interleaving, which a single-threaded test can't reproduce; this
        // asserts the structural invariant that attach only writes pane-binding fields.)
        let state = test_state();
        let mut spawned = sample_agent("agent-new", None, AgentStatus::Starting);
        spawned.session_id = Some("sess-xyz".to_string());
        spawned.transcript_path = Some("/tmp/new.jsonl".to_string());
        state.insert_agent(spawned).unwrap();

        let attached = attach_agent_pane(&state, "agent-new", "pane-1".to_string()).unwrap();

        assert_eq!(attached.pane_id.as_deref(), Some("pane-1"));
        assert!(matches!(attached.status, AgentStatus::Running));
        assert_eq!(attached.session_id.as_deref(), Some("sess-xyz"));
        assert_eq!(attached.transcript_path.as_deref(), Some("/tmp/new.jsonl"));
    }

    #[test]
    fn detach_pane_agent_reverts_pane_to_plain_shell() {
        // A shell-launched agent may still be carrying a prompt/notification status:
        // once the agent process exits, detaching clears the pane binding and drops it
        // to Idle so the tab stops advertising a stale status.
        let state = test_state();
        state
            .insert_agent(sample_agent(
                "agent-shell",
                Some("pane-1"),
                AgentStatus::AwaitingInput,
            ))
            .unwrap();

        let detached = detach_pane_agent(&state, "pane-1")
            .unwrap()
            .expect("an agent was bound to the pane");

        assert_eq!(detached.id, "agent-shell");
        assert_eq!(detached.pane_id, None);
        assert!(matches!(detached.status, AgentStatus::Idle));
        let stored = state
            .agent("agent-shell")
            .unwrap()
            .expect("agent still exists");
        assert_eq!(stored.pane_id, None);
        assert!(matches!(stored.status, AgentStatus::Idle));
    }

    #[test]
    fn detach_pane_agent_parks_queued_turns_as_orphaned() {
        // An agent with queued turns must stay restart-recoverable, so detaching parks its
        // queue as an orphaned queue on the pane rather than discarding it.
        let state = test_state();
        state
            .insert_agent(sample_agent(
                "agent-shell",
                Some("pane-1"),
                AgentStatus::Running,
            ))
            .unwrap();
        state
            .enqueue_agent_turn("agent-shell", "queued turn".to_string())
            .unwrap();

        let detached = detach_pane_agent(&state, "pane-1").unwrap().unwrap();

        assert_eq!(detached.pane_id, None);
        assert_eq!(detached.orphaned_queue_pane_id.as_deref(), Some("pane-1"));
        assert_eq!(
            state.list_agent_turn_queue("agent-shell").unwrap(),
            vec!["queued turn".to_string()]
        );
    }

    #[test]
    fn clear_agent_working_status_only_clears_running_states() {
        let state = test_state();
        state
            .insert_agent(sample_agent(
                "agent-running",
                Some("pane-1"),
                AgentStatus::Running,
            ))
            .unwrap();
        state
            .insert_agent(sample_agent(
                "agent-starting",
                Some("pane-2"),
                AgentStatus::Starting,
            ))
            .unwrap();
        state
            .insert_agent(sample_agent(
                "agent-waiting",
                Some("pane-3"),
                AgentStatus::AwaitingInput,
            ))
            .unwrap();

        let running = clear_agent_working_status(&state, "agent-running").unwrap();
        let starting = clear_agent_working_status(&state, "agent-starting").unwrap();
        let waiting = clear_agent_working_status(&state, "agent-waiting").unwrap();

        // Working states are routed through the idle completion path, so with no queued
        // turn they land on Done (a finished status), not a fresh Idle. A non-working
        // state (AwaitingInput) is left untouched.
        assert!(matches!(running.status, AgentStatus::Done));
        assert!(matches!(starting.status, AgentStatus::Done));
        assert!(matches!(waiting.status, AgentStatus::AwaitingInput));
        assert!(matches!(
            state.agent("agent-running").unwrap().unwrap().status,
            AgentStatus::Done
        ));
        assert!(matches!(
            state.agent("agent-starting").unwrap().unwrap().status,
            AgentStatus::Done
        ));
        assert!(matches!(
            state.agent("agent-waiting").unwrap().unwrap().status,
            AgentStatus::AwaitingInput
        ));
    }

    #[test]
    fn escape_watch_demotes_quiet_working_agent_without_draining_queue() {
        let state = test_state();
        state
            .insert_agent(sample_agent(
                "agent-1",
                Some("pane-1"),
                AgentStatus::Running,
            ))
            .unwrap();
        state
            .enqueue_agent_turn("agent-1", "queued turn".to_string())
            .unwrap();
        let baseline = state.agent_activity_seq("agent-1").unwrap();

        let demoted = resolve_agent_escape_watch(&state, "agent-1", baseline)
            .expect("quiet working agent is demoted");
        assert!(matches!(demoted.status, AgentStatus::AwaitingInput));
        // Unlike the hard idle signals, the demotion must not auto-drain the queue:
        // if the Esc didn't actually interrupt, a drained turn would steer a busy
        // agent.
        assert_eq!(
            state.list_agent_turn_queue("agent-1").unwrap(),
            vec!["queued turn".to_string()]
        );
    }

    #[test]
    fn escape_watch_stands_down_when_activity_arrives_during_grace() {
        let state = test_state();
        state
            .insert_agent(sample_agent(
                "agent-1",
                Some("pane-1"),
                AgentStatus::Running,
            ))
            .unwrap();
        let baseline = state.agent_activity_seq("agent-1").unwrap();

        // A hook event landing during the grace window (e.g. a PostToolUse
        // re-marking Running) bumps the activity counter: the watch stands down.
        state
            .set_agent_status("agent-1", AgentStatus::Running)
            .unwrap();

        assert!(resolve_agent_escape_watch(&state, "agent-1", baseline).is_none());
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        ));
    }

    #[test]
    fn escape_watch_ignores_non_working_agents() {
        let state = test_state();
        state
            .insert_agent(sample_agent("agent-1", Some("pane-1"), AgentStatus::Done))
            .unwrap();
        let baseline = state.agent_activity_seq("agent-1").unwrap();

        assert!(resolve_agent_escape_watch(&state, "agent-1", baseline).is_none());
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Done
        ));
    }

    #[test]
    fn escape_watch_only_runs_for_lone_escape_adapters() {
        // Claude interrupts on a lone Esc (no hook), so it is watched; Codex needs Esc
        // twice, so watching it would demote a still-working agent on every Esc.
        assert!(adapter_interrupts_on_lone_escape("claude"));
        assert!(!adapter_interrupts_on_lone_escape("codex"));
        assert!(!adapter_interrupts_on_lone_escape("grok"));
        assert!(!adapter_interrupts_on_lone_escape("opencode"));
    }

    #[test]
    fn escape_watch_dedupes_a_held_escape_burst() {
        let state = test_state();
        state
            .insert_agent(sample_agent(
                "agent-1",
                Some("pane-1"),
                AgentStatus::Running,
            ))
            .unwrap();

        // The first Esc reserves the watch; a repeat while it is in flight does not.
        assert!(state.begin_agent_escape_watch("agent-1"));
        assert!(!state.begin_agent_escape_watch("agent-1"));
        // Once the watcher resolves, the next burst may reserve again.
        state.end_agent_escape_watch("agent-1");
        assert!(state.begin_agent_escape_watch("agent-1"));
    }

    #[test]
    fn detach_pane_agent_is_a_noop_for_an_unowned_pane() {
        let state = test_state();
        assert!(detach_pane_agent(&state, "pane-empty").unwrap().is_none());
    }

    #[test]
    fn stale_shell_job_cannot_detach_replacement_agent() {
        let state = test_state();
        state
            .insert_agent(sample_agent(
                "agent-new",
                Some("pane-1"),
                AgentStatus::Running,
            ))
            .unwrap();

        assert!(
            detach_pane_agent_if_matches(&state, "pane-1", "agent-old")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state.agent_by_pane("pane-1").unwrap().unwrap().id,
            "agent-new"
        );
    }

    #[test]
    fn mark_agent_spawn_failed_clears_reserved_pane_binding() {
        let state = test_state();
        let mut agent = sample_agent("agent-new", Some("pane-1"), AgentStatus::Running);
        agent.orphaned_queue_pane_id = Some("pane-1".to_string());
        agent.session_id = Some("sess-xyz".to_string());
        state.insert_agent(agent).unwrap();

        let failed = mark_agent_spawn_failed(&state, "agent-new", "pane-1").unwrap();

        assert_eq!(failed.pane_id, None);
        assert_eq!(failed.orphaned_queue_pane_id, None);
        assert!(matches!(failed.status, AgentStatus::Failed));
        assert_eq!(failed.session_id.as_deref(), Some("sess-xyz"));
    }

    #[test]
    fn stopped_status_deserializes_as_idle() {
        let status: AgentStatus = serde_json::from_str("\"stopped\"").unwrap();
        assert!(matches!(status, AgentStatus::Idle));
    }

    #[test]
    fn default_group_name_is_a_human_readable_hyphenated_name() {
        // Sample several times since generation is random.
        for _ in 0..50 {
            let name = default_group_name();
            let words: Vec<&str> = name.split('-').collect();
            // Hyphenated words (typically two: adjective-noun), each a non-empty
            // run of lowercase letters — not the old "group-<millis>" form.
            assert!(words.len() >= 2, "expected a hyphenated name, got {name:?}");
            assert!(
                words
                    .iter()
                    .all(|word| !word.is_empty() && word.chars().all(|ch| ch.is_ascii_lowercase())),
                "unexpected friendly name {name:?}"
            );
            assert!(
                !name.starts_with("group-"),
                "name should not use the group- prefix: {name:?}"
            );
        }
    }

    fn allocation_group(project: &Path, managed: &Path) -> GroupInfo {
        GroupInfo {
            id: "group-1".to_string(),
            name: "brave otter".to_string(),
            name_override: None,
            dir: project.display().to_string(),
            managed_dir: managed.display().to_string(),
            base_repo: Some(project.display().to_string()),
            base_ref: Some("HEAD".to_string()),
            parent_id: None,
            created_at: 1,
            collapsed: false,
            scope: WorkspaceScope::Terminal,
            imported_research_archive_id: None,
            remote: None,
            agents: Vec::new(),
        }
    }

    #[test]
    fn allocates_non_git_worktrees_in_local_qmux_root() {
        let workspace = temp_workspace("local-qmux");
        let project = workspace.join("project");
        let managed = workspace.join("managed/group");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&managed).unwrap();
        let state = test_state_with_workspace(workspace.join("state"));
        persistence::save_preferences(
            &state.config().workspace_root,
            &persistence::AppPreferences {
                worktree_location: Some(WorktreeLocation::LocalQmux),
                ..Default::default()
            },
        )
        .unwrap();
        let group = allocation_group(&project, &managed);
        let canonical_project = fs::canonicalize(&project).unwrap();

        let first = allocate_agent_worktree_dir(
            &state,
            &Host::Local,
            Some(project.to_str().unwrap()),
            &group,
            "agent-1",
        )
        .unwrap();
        assert_eq!(
            first,
            canonical_project.join(".qmux/worktrees/brave-otter-agent-1")
        );
        fs::create_dir_all(&first).unwrap();
        let collision = allocate_agent_worktree_dir(
            &state,
            &Host::Local,
            Some(project.to_str().unwrap()),
            &group,
            "agent-1",
        )
        .unwrap();
        assert_eq!(
            collision,
            canonical_project.join(".qmux/worktrees/brave-otter-agent-1-1")
        );
        fs::remove_dir_all(workspace).ok();
    }

    #[test]
    fn local_worktree_names_bound_long_group_names() {
        let workspace = temp_workspace("local-long-name");
        let project = workspace.join("project");
        let managed = workspace.join("managed/group");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&managed).unwrap();
        let state = test_state_with_workspace(workspace.join("state"));
        persistence::save_preferences(
            &state.config().workspace_root,
            &persistence::AppPreferences {
                worktree_location: Some(WorktreeLocation::LocalQmux),
                ..Default::default()
            },
        )
        .unwrap();
        let mut group = allocation_group(&project, &managed);
        group.name_override = Some("a".repeat(400));

        let dir = allocate_agent_worktree_dir(
            &state,
            &Host::Local,
            Some(project.to_str().unwrap()),
            &group,
            "agent-1",
        )
        .unwrap();
        let name = dir.file_name().unwrap().to_str().unwrap();
        assert!(
            name.len() <= 240,
            "worktree component was {} bytes",
            name.len()
        );
        fs::create_dir_all(&dir).unwrap();
        assert!(dir.is_dir());

        let mut other = group;
        other.name_override = Some(format!("{}b", "a".repeat(399)));
        let other_dir = allocate_agent_worktree_dir(
            &state,
            &Host::Local,
            Some(project.to_str().unwrap()),
            &other,
            "agent-1",
        )
        .unwrap();
        assert_ne!(
            dir, other_dir,
            "the hash should distinguish truncated names"
        );
        fs::remove_dir_all(workspace).ok();
    }

    #[test]
    fn local_claude_uses_git_top_level_and_excludes_worktrees_once() {
        let workspace = temp_workspace("local-claude");
        let project = workspace.join("project");
        let nested = project.join("nested/path");
        let managed = workspace.join("managed/group");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(&managed).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&project)
                .args(args)
                .output()
                .expect("git runs");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        let canonical_project = fs::canonicalize(&project).unwrap();
        let state = test_state_with_workspace(workspace.join("state"));
        persistence::save_preferences(
            &state.config().workspace_root,
            &persistence::AppPreferences {
                worktree_location: Some(WorktreeLocation::LocalClaude),
                ..Default::default()
            },
        )
        .unwrap();
        let group = allocation_group(&nested, &managed);

        let first = allocate_agent_worktree_dir(
            &state,
            &Host::Local,
            Some(nested.to_str().unwrap()),
            &group,
            "agent-1",
        )
        .unwrap();
        let second = allocate_agent_worktree_dir(
            &state,
            &Host::Local,
            Some(nested.to_str().unwrap()),
            &group,
            "agent-2",
        )
        .unwrap();

        assert_eq!(
            first,
            canonical_project.join(".claude/worktrees/brave-otter-agent-1")
        );
        assert_eq!(
            second,
            canonical_project.join(".claude/worktrees/brave-otter-agent-2")
        );
        let exclude = fs::read_to_string(project.join(".git/info/exclude")).unwrap();
        assert_eq!(
            exclude
                .lines()
                .filter(|line| line.trim() == "/.claude/worktrees/")
                .count(),
            1
        );
        fs::remove_dir_all(workspace).ok();
    }

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["branch", "--list", branch])
            .output()
            .expect("git branch --list runs");
        !String::from_utf8_lossy(&output.stdout).trim().is_empty()
    }

    #[test]
    fn soft_delete_branch_removes_merged_keeps_unmerged() {
        let repo = std::env::temp_dir().join(format!("qmux-branch-{}", now_millis()));
        fs::create_dir_all(&repo).unwrap();
        let repo_str = repo.to_string_lossy().to_string();

        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .expect("git runs");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };

        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "qmux test"]);
        git(&["commit", "--allow-empty", "-m", "init"]);

        // A branch at HEAD is fully merged, so the soft delete removes it.
        git(&["branch", "merged"]);
        assert_eq!(
            soft_delete_branch(&Host::Local, &repo_str, "merged"),
            Ok(true)
        );
        assert!(!branch_exists(&repo, "merged"));

        // A branch with its own commit is not merged into main, so git declines
        // and the branch (and its committed work) is preserved.
        git(&["checkout", "-b", "feature"]);
        git(&["commit", "--allow-empty", "-m", "work"]);
        git(&["checkout", "main"]);
        assert_eq!(
            soft_delete_branch(&Host::Local, &repo_str, "feature"),
            Ok(false)
        );
        assert!(branch_exists(&repo, "feature"));

        fs::remove_dir_all(&repo).ok();
    }
}
