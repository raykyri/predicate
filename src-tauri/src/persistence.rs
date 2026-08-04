use crate::encyclopedia::EncyclopediaState;
use crate::research::{ResearchFolderState, ResearchNode, ResearchTree};
use crate::state::{GlobalDraft, PaneInfo, PaneSplitInfo, QueuedTurn, RecentSessionInfo};
use crate::thread_graph::ThreadRecord;
use crate::workspace::{AgentInfo, GroupInfo};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Distinguishes the scratch file of each in-flight `save` so concurrent writers
/// never share (and then race to rename) the same temp path.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
/// Preferences are one small document updated by several independent UI surfaces
/// (launcher selection, login-shell toggle, OpenRouter key, global shortcut). Keep
/// each read-modify-write transaction under one lock so concurrent setters cannot
/// both read the same snapshot and then overwrite one another's field.
static PREFERENCES_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
/// Last-loaded preferences per path, written through on every successful save.
/// Preference reads sit on hot paths — every shell spawn consults
/// `use_login_shell` — and each previously re-read and re-parsed the file;
/// serving repeats from memory keeps spawn paths free of disk I/O. Writes from
/// outside this process are only picked up at the next in-process save, which
/// matches the app's single-writer assumption for this file.
static PREFERENCES_CACHE: LazyLock<Mutex<HashMap<PathBuf, AppPreferences>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Bumped whenever the on-disk shape changes incompatibly. A file written by a
/// newer or unknown version is treated as empty rather than misinterpreted.
pub const STATE_VERSION: u32 = 5;
/// Written when the state contains no conversation nodes, so sessions that
/// never used terminal export stay loadable by pre-conversations builds
/// (which accept versions 2–4). Version 5 marks conversation nodes being
/// present: a pre-conversations build then refuses the file with its
/// newer-version guidance instead of silently dropping those nodes and
/// erasing them on its next save. Mirrors the detached-archive tiering.
pub const STATE_VERSION_PRE_CONVERSATIONS: u32 = 4;
const MIN_MIGRATABLE_STATE_VERSION: u32 = 2;
const STATE_FILE: &str = "state.json";
const PREFERENCES_FILE: &str = "preferences.json";
const V2_BACKUP_FILE: &str = "state.v2.bak";
pub(crate) const STATE_DIR: &str = ".qmux";

/// Snapshot of everything a qmux restart needs to recreate panes, agents,
/// groups and queued turns. Live PTY handles are intentionally absent: a
/// restarted process cannot adopt the old PTY, so only metadata is persisted.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedState {
    pub version: u32,
    #[serde(default)]
    pub next_id: u64,
    #[serde(default)]
    pub panes: Vec<PaneInfo>,
    #[serde(default)]
    pub groups: Vec<GroupInfo>,
    #[serde(default)]
    pub group_order: Vec<String>,
    #[serde(default)]
    pub agents: Vec<AgentInfo>,
    #[serde(default)]
    pub queues: HashMap<String, Vec<QueuedTurn>>,
    /// Durable, tab-independent history of resumable agent sessions. Unlike `agents`,
    /// entries remain after their tab closes so Home can show recent work.
    #[serde(default)]
    pub recent_sessions: Vec<RecentSessionInfo>,
    /// Per-agent composer drafts: the unsent text sitting in the right-pane input.
    /// Persisted so an in-progress draft survives a restart, recovered alongside
    /// queues and transcripts.
    #[serde(default)]
    pub drafts: HashMap<String, String>,
    /// Application-global prompt drafts (the home Drafts rail), oldest first.
    #[serde(default)]
    pub global_drafts: Vec<GlobalDraft>,
    /// Per-agent in-flight turn: a queued turn claimed for delivery but not confirmed
    /// on the PTY before shutdown. Recovered by re-queueing at the front so a crash
    /// mid-delivery re-sends rather than loses it. Absent in older state files.
    #[serde(default)]
    pub inflight: HashMap<String, QueuedTurn>,
    /// Vertical terminal split groups. Each group glues adjacent pane tabs into a
    /// shared top-to-bottom terminal viewport; older state files simply have none.
    #[serde(default)]
    pub pane_splits: Vec<PaneSplitInfo>,
    /// The selected tab in the frontend. This may be a pane id or the Home tab
    /// sentinel; the frontend validates it against the recovered panes on boot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tab_id: Option<String>,
    /// Per-thread routing and storage metadata. The thread snapshot itself lives in
    /// `<workspaceRoot>/.qmux/threads/<thread-id>.json`; this tells qmux where to find
    /// it and what branch a default view should focus.
    #[serde(default)]
    pub threads: HashMap<String, ThreadRecord>,
    /// Focused branch per qmux transcript thread. The full graph lives in the global
    /// thread store; state.json keeps only the routing metadata needed to recover
    /// pane/agent views.
    #[serde(default)]
    pub thread_focus: HashMap<String, String>,
    /// Durable research documents and their one-prompt native-session nodes.
    #[serde(default)]
    pub research_trees: HashMap<String, ResearchTree>,
    /// Stable sidebar order for research trees. The ids form one master order;
    /// folder and archived filters preserve their relative subsequences.
    #[serde(default)]
    pub research_tree_order: Vec<String>,
    #[serde(default)]
    pub research_nodes: HashMap<String, ResearchNode>,
    /// Client-authored organizational grouping of research trees (folders,
    /// membership, stars, collapsed). Optional and dropped-if-empty so state
    /// files from builds that predate folders round-trip byte-identically.
    #[serde(default, skip_serializing_if = "ResearchFolderState::is_empty")]
    pub research_folders: ResearchFolderState,
    /// Per-workspace encyclopedia settings and generation cursor. Optional
    /// and dropped-if-empty so state files from builds that predate the
    /// encyclopedia round-trip byte-identically.
    #[serde(default, skip_serializing_if = "EncyclopediaState::is_empty")]
    pub encyclopedia: EncyclopediaState,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            next_id: 0,
            panes: Vec::new(),
            groups: Vec::new(),
            group_order: Vec::new(),
            agents: Vec::new(),
            queues: HashMap::new(),
            recent_sessions: Vec::new(),
            drafts: HashMap::new(),
            global_drafts: Vec::new(),
            inflight: HashMap::new(),
            pane_splits: Vec::new(),
            active_tab_id: None,
            threads: HashMap::new(),
            thread_focus: HashMap::new(),
            research_trees: HashMap::new(),
            research_tree_order: Vec::new(),
            research_nodes: HashMap::new(),
            research_folders: ResearchFolderState::default(),
            encyclopedia: EncyclopediaState::default(),
        }
    }
}

pub fn state_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(STATE_DIR).join(STATE_FILE)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WorktreeLocation {
    #[default]
    Global,
    LocalQmux,
    LocalClaude,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppPreferences {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launcher_adapter_id: Option<String>,
    /// Whether new and recovered shells run as login shells (sourcing the user's
    /// login profile files in addition to the interactive rc). Absent means the
    /// default, on — matching how terminal emulators launch shells.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_login_shell: Option<bool>,
    /// Root used for newly-created isolated worktrees. Absent preserves the
    /// historical global qmux workspace location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_location: Option<WorktreeLocation>,
    /// Global shortcut used to show or hide the qmux app. Absent means no
    /// shortcut is registered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_hide_shortcut: Option<String>,
    /// Fixed-choice global shortcut that opens the task launcher. Absent means
    /// the global hotkey is disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_launcher_hotkey: Option<String>,
    /// OpenRouter API key used for tab-title generation. Kept here — in the
    /// owner-only (0600) preferences file — rather than in webview localStorage,
    /// so the secret isn't sitting in a store any injected script could read at
    /// rest. Absent means no key is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_router_key: Option<String>,
    /// Custom instruction text sent with every research launch (fresh runs,
    /// run follow-ups, and document/conversation follow-ups). The backend owns
    /// this preference because follow-up launches assemble their sent prompt
    /// here, not in the frontend. Absent or empty means launches send the
    /// user's prompt unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub research_launch_instruction: Option<String>,
}

pub fn preferences_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(STATE_DIR).join(PREFERENCES_FILE)
}

pub fn load_preferences(workspace_root: &Path) -> Result<AppPreferences, String> {
    let path = preferences_path(workspace_root);
    if let Some(cached) = cached_preferences(&path) {
        return Ok(cached);
    }
    // Fill the cache under the preferences lock so a cold read racing an
    // in-flight update can't stash the pre-update snapshot after the update
    // wrote through its fresh one.
    let _guard = PREFERENCES_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(cached) = cached_preferences(&path) {
        return Ok(cached);
    }
    let preferences = read_preferences_from_disk(&path)?;
    store_cached_preferences(&path, &preferences);
    Ok(preferences)
}

fn cached_preferences(path: &Path) -> Option<AppPreferences> {
    PREFERENCES_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(path)
        .cloned()
}

fn store_cached_preferences(path: &Path, preferences: &AppPreferences) {
    PREFERENCES_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(path.to_path_buf(), preferences.clone());
}

fn read_preferences_from_disk(path: &Path) -> Result<AppPreferences, String> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(AppPreferences::default()),
        Err(err) => {
            return Err(format!(
                "failed to read preferences {}: {err}",
                path.display()
            ));
        }
    };

    serde_json::from_str::<AppPreferences>(&raw)
        .map_err(|err| format!("invalid preferences {}: {err}", path.display()))
}

#[cfg(test)]
pub fn save_preferences(workspace_root: &Path, preferences: &AppPreferences) -> Result<(), String> {
    let _guard = PREFERENCES_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    save_preferences_unlocked(workspace_root, preferences)
}

/// Atomically updates selected preference fields while preserving every unrelated
/// field from the latest on-disk snapshot. A read/parse failure is returned without
/// writing anything, rather than treating a damaged preferences file as empty and
/// erasing recoverable settings (including the stored API key). Reads the disk —
/// not the cache — so a damaged file still refuses the write.
pub fn update_preferences(
    workspace_root: &Path,
    update: impl FnOnce(&mut AppPreferences),
) -> Result<(), String> {
    let _guard = PREFERENCES_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut preferences = read_preferences_from_disk(&preferences_path(workspace_root))?;
    update(&mut preferences);
    save_preferences_unlocked(workspace_root, &preferences)
}

fn save_preferences_unlocked(
    workspace_root: &Path,
    preferences: &AppPreferences,
) -> Result<(), String> {
    let path = preferences_path(workspace_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create preferences dir {}: {err}",
                parent.display()
            )
        })?;
        // Owner-only state dir, consistent with config.rs. Best-effort on an existing dir.
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }

    let raw = serde_json::to_string_pretty(preferences)
        .map_err(|err| format!("failed to encode preferences: {err}"))?;
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.{}.{seq}.tmp", std::process::id()));

    write_synced(&tmp, raw.as_bytes())
        .map_err(|err| format!("failed to write {}: {err}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|err| {
        let _ = fs::remove_file(&tmp);
        format!("failed to commit {}: {err}", path.display())
    })?;

    if let Some(parent) = path.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    store_cached_preferences(&path, preferences);
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadWarning {
    pub message: String,
    pub path: PathBuf,
    pub backup_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct LoadOutcome {
    pub state: PersistedState,
    pub warning: Option<LoadWarning>,
    pub source_version: Option<u32>,
}

/// True when the user asked to force startup past an *unreadable* state file,
/// discarding it. Set via the `QMUX_RESET_STATE` env var (documented in the
/// abort message from `preflight_state`) — the GUI equivalent of a "hold a
/// modifier to reset" escape hatch, chosen over live modifier detection because
/// a Finder launch has no reliable key state to read at startup.
fn reset_state_requested() -> bool {
    std::env::var_os("QMUX_RESET_STATE").is_some_and(|value| value == "1" || value == "true")
}

/// Checked once at startup, *before* recovery hydrates state and enables saving.
///
/// If the state file exists but cannot be read (permission denied, fd
/// exhaustion, an offline iCloud/network volume, EIO, …) then starting with an
/// empty session would let the first boot-time `save` overwrite the intact file
/// with nothing — and, unlike the corrupt-file path, there is no backup to
/// recover from. So this refuses to continue: it returns `Err` with a
/// user-facing message and the caller aborts startup, leaving the file untouched
/// so a relaunch (after fixing the transient cause) restores the session.
///
/// The same refusal applies to a state file written by a *newer* qmux: this
/// build cannot load it (there are no forward migrations), and moving it aside
/// to start empty would destroy the newer install's session every time a stale
/// copy of the app is launched. The fix is to run the newer app, so abort and
/// say so rather than eat the session.
///
/// A missing file (first run) or a readable, loadable file returns `Ok`. With
/// `QMUX_RESET_STATE` set, an unreadable or newer-versioned file is renamed
/// aside to a `.bak` and `Ok` is returned so the user can deliberately start
/// fresh without losing the original bytes.
///
/// On success the raw bytes that were read (and version-checked) are returned
/// so the subsequent hydration can reuse them instead of reading and parsing
/// the same file a second time; `None` means there was nothing to read (first
/// run, or the file was deliberately moved aside).
pub fn preflight_state(workspace_root: &Path) -> Result<Option<Vec<u8>>, String> {
    let path = state_path(workspace_root);
    // Read raw bytes, not a UTF-8 string: a state file with invalid UTF-8 is
    // *corrupt content*, not an I/O failure, and must fall through to the
    // version check below (where it fails to parse and is left for
    // `load_with_diagnostics` to preserve as a `.bak`). `read_to_string` would
    // instead surface it as `ErrorKind::InvalidData` and hit the fatal arm,
    // refusing startup forever with misleading "this is almost always temporary"
    // guidance.
    let raw = match fs::read(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) if reset_state_requested() => {
            return match preserve_rejected_state(&path, "unreadable") {
                Ok(backup_path) => {
                    eprintln!(
                        "qmux: QMUX_RESET_STATE set; moved unreadable state {} aside to {} and starting fresh",
                        path.display(),
                        backup_path.display()
                    );
                    Ok(None)
                }
                Err(rename_err) => Err(format!(
                    "could not read persisted state {} ({err}) and could not move it aside to reset: {rename_err}",
                    path.display()
                )),
            };
        }
        Err(err) => {
            return Err(format!(
                "Could not read your saved qmux session at {path}:\n  {err}\n\n\
                 Your panes and agents are still saved on disk, so qmux is refusing to start with an \
                 empty session and overwrite them. This is almost always temporary — fix the cause \
                 below and relaunch to get your session back:\n\
                 \x20 • Permission denied: check the file's ownership and permissions.\n\
                 \x20 • Too many open files: quit other apps (or raise the open-file limit), then relaunch.\n\
                 \x20 • File on iCloud/a network volume: wait for the volume to come back online.\n\n\
                 To start fresh on purpose instead — your current session file is moved aside to a \
                 .bak first, so nothing is lost — relaunch with:\n\
                 \x20 QMUX_RESET_STATE=1 open -a qmux",
                path = path.display()
            ));
        }
    };

    // Non-UTF-8, unparseable JSON, or a missing/older version is left for
    // `load_with_diagnostics`, which preserves the file as a `.bak` before starting
    // fresh. `from_slice` rejects non-UTF-8 as a parse error, so it lands here too.
    let newer_version = serde_json::from_slice::<Value>(&raw)
        .ok()
        .and_then(|value| value.get("version").and_then(Value::as_u64))
        .filter(|&version| version > u64::from(STATE_VERSION));
    let Some(version) = newer_version else {
        return Ok(Some(raw));
    };

    if reset_state_requested() {
        return match preserve_rejected_state(&path, "newer-version") {
            Ok(backup_path) => {
                eprintln!(
                    "qmux: QMUX_RESET_STATE set; moved newer-versioned state {} aside to {} and starting fresh",
                    path.display(),
                    backup_path.display()
                );
                Ok(None)
            }
            Err(rename_err) => Err(format!(
                "persisted state {} was written by a newer qmux (state version {version}) and could not be moved aside to reset: {rename_err}",
                path.display()
            )),
        };
    }

    Err(format!(
        "Your saved qmux session at {path} was written by a newer version of qmux \
         (state version {version}; this build supports up to {STATE_VERSION}).\n\n\
         Loading it here would discard that session, so this copy of qmux is refusing to \
         start. Launch the newer qmux instead, or update this copy.\n\n\
         To start fresh on purpose — your current session file is moved aside to a .bak \
         first, so nothing is lost — relaunch with:\n\
         \x20 QMUX_RESET_STATE=1 open -a qmux",
        path = path.display()
    ))
}

/// Reads persisted state and reports why recovery had to fall back to an empty
/// snapshot. Missing state is expected on first run and does not produce a warning.
/// Corrupt or unsupported state files are renamed aside before future saves can
/// overwrite them.
///
/// Production startup goes through `load_with_diagnostics_from` with the bytes
/// preflight already read; this read-the-file-itself wrapper serves tests.
#[cfg(test)]
pub fn load_with_diagnostics(workspace_root: &Path) -> LoadOutcome {
    load_with_diagnostics_from(workspace_root, None)
}

/// Like [`load_with_diagnostics`], but reuses bytes already read by
/// [`preflight_state`] instead of reading and parsing the same file a second
/// time during startup.
pub fn load_with_diagnostics_from(workspace_root: &Path, preread: Option<Vec<u8>>) -> LoadOutcome {
    let path = state_path(workspace_root);
    let raw = match preread {
        Some(raw) => match String::from_utf8(raw) {
            Ok(raw) => raw,
            Err(err) => {
                return discard_state_file(
                    &path,
                    "corrupt",
                    format!(
                        "persisted state {} is not valid UTF-8: {err}",
                        path.display()
                    ),
                );
            }
        },
        None => match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                return load_ok(PersistedState::default());
            }
            Err(err) => {
                // `preflight_state` runs first and aborts startup on an unreadable
                // file, so this is only reached if the read starts failing between
                // preflight and here (e.g. transient EIO). Preserve the bytes aside
                // as a `.bak` rather than starting empty and letting the next save
                // overwrite an intact session with no way back.
                return discard_state_file(
                    &path,
                    "unreadable",
                    format!("failed to read persisted state {}: {err}", path.display()),
                );
            }
        },
    };

    let mut value = match serde_json::from_str::<Value>(&raw) {
        Ok(value) => value,
        Err(err) => {
            return discard_state_file(
                &path,
                "corrupt",
                format!("invalid JSON in persisted state {}: {err}", path.display()),
            );
        }
    };

    let Some(version) = value
        .get("version")
        .and_then(Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
    else {
        return discard_state_file(
            &path,
            "corrupt",
            format!(
                "persisted state {} is missing a valid version",
                path.display()
            ),
        );
    };

    if !(MIN_MIGRATABLE_STATE_VERSION..=STATE_VERSION).contains(&version) {
        return discard_state_file(
            &path,
            "unsupported-version",
            format!(
                "persisted state {} was written by unsupported version {version}; current version is {STATE_VERSION}",
                path.display()
            ),
        );
    }

    // Version 3 introduces durable workspace ownership. Perform the shape-only
    // portion here, before lenient deserialization, so no research tree is lost
    // merely because its authoritative workspace field did not exist in v2.
    // Filesystem-backed workspace isolation remains a state-reconciliation job.
    if version == 2 {
        migrate_v2_to_v3(&mut value);
    }

    // Deserialize collections element-by-element so a single malformed record
    // (partial corruption, a hand-edit, an unforeseen schema drift in one entry)
    // drops only that entry instead of discarding the entire session. The bad
    // entry is left behind on disk until the next save re-serializes the good
    // state over it; the dropped entries are surfaced as a warning.
    let (state, dropped) = deserialize_lenient(value);
    if dropped.is_empty() {
        load_ok_version(state, Some(version))
    } else {
        LoadOutcome {
            state,
            warning: Some(LoadWarning {
                message: format!(
                    "recovered persisted state {} but dropped {} unreadable entr{}: {}",
                    path.display(),
                    dropped.len(),
                    if dropped.len() == 1 { "y" } else { "ies" },
                    dropped.join("; ")
                ),
                path,
                backup_path: None,
            }),
            source_version: Some(version),
        }
    }
}

/// Pure JSON-shape migration for version-2 state. This deliberately performs no
/// filesystem I/O: loading a valid session must not have external side effects.
fn migrate_v2_to_v3(value: &mut Value) {
    let Value::Object(map) = value else {
        return;
    };

    let research_nodes = map
        .get("researchNodes")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    if let Some(Value::Object(trees)) = map.get_mut("researchTrees") {
        for tree in trees.values_mut() {
            let Some(tree) = tree.as_object_mut() else {
                continue;
            };
            if tree
                .get("workspaceId")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.trim().is_empty())
            {
                continue;
            }
            let Some(root_node_id) = tree.get("rootNodeId").and_then(Value::as_str) else {
                continue;
            };
            let Some(group_id) = research_nodes
                .get(root_node_id)
                .and_then(Value::as_object)
                .and_then(|node| node.get("groupId"))
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
            else {
                continue;
            };
            tree.insert(
                "workspaceId".to_string(),
                Value::String(group_id.to_string()),
            );
        }
    }

    // Research trees are their own durable history. Remove their native agent
    // sessions from Home's recent-session pool instead of tagging/filtering them;
    // otherwise they consume the global pruning cap and can evict terminal work.
    let mut agent_ids = HashSet::new();
    let mut pane_ids = HashSet::new();
    let mut session_ids = HashSet::new();
    let mut transcript_paths = HashSet::new();
    for node in research_nodes.values().filter_map(Value::as_object) {
        collect_nonempty_string(node, "agentId", &mut agent_ids);
        collect_nonempty_string(node, "paneId", &mut pane_ids);
        collect_nonempty_string(node, "nativeSessionId", &mut session_ids);
        collect_nonempty_string(node, "transcriptPath", &mut transcript_paths);
    }
    if let Some(Value::Array(sessions)) = map.get_mut("recentSessions") {
        sessions.retain(|session| {
            let Some(session) = session.as_object() else {
                return true;
            };
            !value_matches_set(session, "agentId", &agent_ids)
                && !value_matches_set(session, "paneId", &pane_ids)
                && !value_matches_set(session, "sessionId", &session_ids)
                && !value_matches_set(session, "transcriptPath", &transcript_paths)
        });
    }

    map.insert("version".to_string(), Value::from(STATE_VERSION));
}

fn collect_nonempty_string(
    map: &serde_json::Map<String, Value>,
    key: &str,
    values: &mut HashSet<String>,
) {
    if let Some(value) = map
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        values.insert(value.to_string());
    }
}

fn value_matches_set(
    map: &serde_json::Map<String, Value>,
    key: &str,
    values: &HashSet<String>,
) -> bool {
    map.get(key)
        .and_then(Value::as_str)
        .is_some_and(|value| values.contains(value))
}

/// Rebuilds a `PersistedState` from an already-validated JSON object, converting
/// each collection element independently. Returns the recovered state plus a
/// human-readable list of the entries that had to be dropped (empty on a clean
/// load). Scalar top-level fields fall back to their defaults when malformed.
fn deserialize_lenient(value: Value) -> (PersistedState, Vec<String>) {
    let mut dropped = Vec::new();
    let mut state = PersistedState::default();
    let Value::Object(mut map) = value else {
        // The version check upstream already parsed this as an object; anything
        // else is degenerate corruption, so recover an empty session.
        dropped.push("persisted state root is not a JSON object".to_string());
        return (state, dropped);
    };

    state.version = STATE_VERSION;
    if let Some(next_id) = map.get("nextId").and_then(Value::as_u64) {
        state.next_id = next_id;
    }
    state.panes = take_vec(&mut map, "panes", "pane", &mut dropped);
    state.groups = take_vec(&mut map, "groups", "group", &mut dropped);
    state.agents = take_vec(&mut map, "agents", "agent", &mut dropped);
    state.recent_sessions = take_vec(&mut map, "recentSessions", "recent session", &mut dropped);
    state.pane_splits = take_vec(&mut map, "paneSplits", "pane split", &mut dropped);
    state.group_order = take_string_vec(&mut map, "groupOrder");
    state.queues = take_map_of_vecs(&mut map, "queues", "queued turn", &mut dropped);
    state.inflight = take_typed_map(&mut map, "inflight", "in-flight turn", &mut dropped);
    state.drafts = take_string_map(&mut map, "drafts");
    state.global_drafts = take_vec(&mut map, "globalDrafts", "global draft", &mut dropped);
    state.threads = take_typed_map(&mut map, "threads", "thread", &mut dropped);
    state.thread_focus = take_string_map(&mut map, "threadFocus");
    state.research_trees = take_typed_map(&mut map, "researchTrees", "research tree", &mut dropped);
    state.research_tree_order = take_string_vec(&mut map, "researchTreeOrder");
    state.research_nodes = take_typed_map(&mut map, "researchNodes", "research node", &mut dropped);
    // The grouping is one small self-consistent blob, not a collection of
    // independently-recoverable records, so a malformed one drops the whole
    // grouping (the trees it references are recovered above regardless) rather
    // than the session — matching the frontend's parse-or-empty behavior.
    state.research_folders = match map.remove("researchFolders") {
        Some(value) => match serde_json::from_value(value) {
            Ok(folders) => folders,
            Err(err) => {
                dropped.push(format!("research folders: {err}"));
                ResearchFolderState::default()
            }
        },
        None => ResearchFolderState::default(),
    };
    // Same one-blob discipline as the folder grouping: a malformed
    // encyclopedia layer drops whole rather than dropping the session.
    state.encyclopedia = match map.remove("encyclopedia") {
        Some(value) => match serde_json::from_value(value) {
            Ok(encyclopedia) => encyclopedia,
            Err(err) => {
                dropped.push(format!("encyclopedia: {err}"));
                EncyclopediaState::default()
            }
        },
        None => EncyclopediaState::default(),
    };
    state.active_tab_id = map
        .get("activeTabId")
        .and_then(Value::as_str)
        .map(str::to_string);
    (state, dropped)
}

/// Removes an array field and deserializes each element on its own, collecting a
/// dropped-entry note for any that fail. A present-but-non-array field is itself
/// reported and ignored.
fn take_vec<T: DeserializeOwned>(
    map: &mut serde_json::Map<String, Value>,
    key: &str,
    label: &str,
    dropped: &mut Vec<String>,
) -> Vec<T> {
    match map.remove(key) {
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for (index, item) in items.into_iter().enumerate() {
                match serde_json::from_value::<T>(item) {
                    Ok(parsed) => out.push(parsed),
                    Err(err) => dropped.push(format!("{label} #{index}: {err}")),
                }
            }
            out
        }
        Some(_) => {
            dropped.push(format!("{label}s field was not an array; ignored it"));
            Vec::new()
        }
        None => Vec::new(),
    }
}

/// Removes a `{ key: [values] }` field, deserializing each value list leniently
/// and dropping any bad elements. A key whose value list ends up empty is
/// omitted, matching how `restore_session` treats empty queues.
fn take_map_of_vecs<T: DeserializeOwned>(
    map: &mut serde_json::Map<String, Value>,
    key: &str,
    label: &str,
    dropped: &mut Vec<String>,
) -> HashMap<String, Vec<T>> {
    let Some(Value::Object(entries)) = map.remove(key) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for (entry_key, value) in entries {
        let Value::Array(items) = value else {
            dropped.push(format!(
                "{label}s for '{entry_key}' were not an array; ignored"
            ));
            continue;
        };
        let mut parsed_items = Vec::with_capacity(items.len());
        for (index, item) in items.into_iter().enumerate() {
            match serde_json::from_value::<T>(item) {
                Ok(parsed) => parsed_items.push(parsed),
                Err(err) => dropped.push(format!("{label} '{entry_key}' #{index}: {err}")),
            }
        }
        if !parsed_items.is_empty() {
            out.insert(entry_key, parsed_items);
        }
    }
    out
}

/// Removes a `{ key: value }` field, deserializing each value on its own and dropping
/// only the bad entries (rather than the whole map).
fn take_typed_map<T: DeserializeOwned>(
    map: &mut serde_json::Map<String, Value>,
    key: &str,
    label: &str,
    dropped: &mut Vec<String>,
) -> HashMap<String, T> {
    let Some(Value::Object(entries)) = map.remove(key) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for (entry_key, value) in entries {
        match serde_json::from_value::<T>(value) {
            Ok(parsed) => {
                out.insert(entry_key, parsed);
            }
            Err(err) => dropped.push(format!("{label} '{entry_key}': {err}")),
        }
    }
    out
}

/// Removes a string-array field, keeping only the entries that are strings.
fn take_string_vec(map: &mut serde_json::Map<String, Value>, key: &str) -> Vec<String> {
    match map.remove(key) {
        Some(Value::Array(items)) => items
            .into_iter()
            .filter_map(|item| match item {
                Value::String(value) => Some(value),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Removes a `{ key: "value" }` field, keeping only the string-valued entries.
fn take_string_map(map: &mut serde_json::Map<String, Value>, key: &str) -> HashMap<String, String> {
    match map.remove(key) {
        Some(Value::Object(entries)) => entries
            .into_iter()
            .filter_map(|(entry_key, value)| match value {
                Value::String(value) => Some((entry_key, value)),
                _ => None,
            })
            .collect(),
        _ => HashMap::new(),
    }
}

fn load_ok(state: PersistedState) -> LoadOutcome {
    load_ok_version(state, None)
}

fn load_ok_version(state: PersistedState, source_version: Option<u32>) -> LoadOutcome {
    LoadOutcome {
        state,
        warning: None,
        source_version,
    }
}

fn load_warning(path: PathBuf, message: String, backup_path: Option<PathBuf>) -> LoadOutcome {
    LoadOutcome {
        state: PersistedState::default(),
        warning: Some(LoadWarning {
            message,
            path,
            backup_path,
        }),
        source_version: None,
    }
}

/// Preserves the last v2 bytes before the first successful v3 snapshot replaces
/// state.json. Unlike rejected-state handling this copies rather than renames:
/// recovery continues from the valid source in the same boot.
pub fn backup_v2_state_for_migration(workspace_root: &Path) -> Result<Option<PathBuf>, String> {
    let source = state_path(workspace_root);
    if !source.is_file() {
        return Ok(None);
    }
    let backup = source.with_file_name(V2_BACKUP_FILE);
    if backup.exists() {
        return Ok(Some(backup));
    }
    let bytes = fs::read(&source).map_err(|err| {
        format!(
            "failed to read version-2 state {} for backup: {err}",
            source.display()
        )
    })?;
    // Same temp+fsync+rename discipline as `save`: the exists() short-circuit
    // above trusts whatever bytes are at the backup path forever, so a crash
    // mid-copy must not be able to leave a truncated state.v2.bak as the only
    // preserved v2 snapshot. write_synced also keeps the backup owner-only,
    // matching the file it copies.
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = source.with_file_name(format!("{V2_BACKUP_FILE}.{}.{seq}.tmp", std::process::id()));
    write_synced(&tmp, &bytes).map_err(|err| {
        format!(
            "failed to preserve version-2 state {} at {}: {err}",
            source.display(),
            tmp.display()
        )
    })?;
    fs::rename(&tmp, &backup).map_err(|err| {
        let _ = fs::remove_file(&tmp);
        format!(
            "failed to commit version-2 state backup {}: {err}",
            backup.display()
        )
    })?;
    if let Some(parent) = backup.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(Some(backup))
}

fn discard_state_file(path: &Path, label: &str, reason: String) -> LoadOutcome {
    let (message, backup_path) = match preserve_rejected_state(path, label) {
        Ok(backup_path) => (
            format!(
                "{reason}; preserved rejected state at {}",
                backup_path.display()
            ),
            Some(backup_path),
        ),
        Err(err) => (
            format!("{reason}; failed to preserve rejected state: {err}"),
            None,
        ),
    };
    load_warning(path.to_path_buf(), message, backup_path)
}

fn preserve_rejected_state(path: &Path, label: &str) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("state path {} has no parent directory", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(STATE_FILE);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let backup_path = parent.join(format!(
        "{file_name}.{label}.{millis}.{}.{seq}.bak",
        std::process::id()
    ));
    fs::rename(path, &backup_path).map_err(|err| {
        format!(
            "failed to rename {} to {}: {err}",
            path.display(),
            backup_path.display()
        )
    })?;
    Ok(backup_path)
}

/// Writes state atomically: serialize to a sibling `.tmp` file, flush it to disk,
/// then rename over the target so a crash mid-write can never leave a half-written
/// state.json. The temp file is fsync'd before the rename and the directory is
/// fsync'd after, so the swap is durable across power loss too — without the fsync
/// the filesystem may order the rename ahead of the data write and surface a
/// zero-length or stale file on recovery.
///
/// The scratch file name is unique per call (pid + sequence). Two saves can run
/// concurrently — `persist` releases the model lock before writing — and a shared
/// temp name would let one writer's rename consume the file out from under the
/// other, which then fails with ENOENT.
pub fn save(workspace_root: &Path, state: &PersistedState) -> Result<(), String> {
    let path = state_path(workspace_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create state dir {}: {err}", parent.display()))?;
        // Owner-only state dir (drafts, queued-turn prompts), consistent with the
        // startup hardening in config.rs. Best-effort on an existing directory.
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }

    // Compact rather than pretty: this file is machine-read only, and it is
    // rewritten in full on every debounced snapshot, so the pretty encoding's
    // extra bytes were pure serialize+write overhead on a recurring path.
    let raw =
        serde_json::to_string(state).map_err(|err| format!("failed to encode state: {err}"))?;
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.{}.{seq}.tmp", std::process::id()));

    write_synced(&tmp, raw.as_bytes())
        .map_err(|err| format!("failed to write {}: {err}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|err| {
        // Don't strand the scratch file if the commit itself fails.
        let _ = fs::remove_file(&tmp);
        format!("failed to commit {}: {err}", path.display())
    })?;

    // Persist the directory entry so the rename itself survives a crash. Best
    // effort: some platforms don't allow fsync on a directory handle.
    if let Some(parent) = path.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Removes scratch files stranded by earlier processes. A `save` interrupted
/// between writing its temp file and renaming it into place (process killed
/// mid-quit, crash, …) leaves `state.json.<pid>.<seq>.tmp` behind forever. A
/// scratch file whose writer pid is no longer alive can never be renamed into
/// place, so it is deleted. Best-effort: the files are only clutter, and a pid
/// recycled onto an unrelated live process just postpones that file's cleanup.
pub fn remove_stale_tmp_files(workspace_root: &Path) {
    let dir = workspace_root.join(STATE_DIR);
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(pid) = scratch_writer_pid(name) else {
            continue;
        };
        if pid != std::process::id() && !process_is_alive(pid) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Parses the writer pid out of a `<state, preferences, or v2-backup
/// file>.<pid>.<seq>.tmp` scratch name. Returns `None` for anything else (the
/// live files themselves, `.bak` preserves, foreign files), which the cleanup
/// then leaves alone.
fn scratch_writer_pid(name: &str) -> Option<u32> {
    let rest = name.strip_suffix(".tmp")?;
    let (rest, seq) = rest.rsplit_once('.')?;
    seq.parse::<u64>().ok()?;
    let (base, pid) = rest.rsplit_once('.')?;
    if base != STATE_FILE && base != PREFERENCES_FILE && base != V2_BACKUP_FILE {
        return None;
    }
    pid.parse().ok()
}

/// Probes pid liveness with `kill(pid, 0)`, which signals nothing. EPERM still
/// means the pid exists (it belongs to another user), so only ESRCH counts as dead.
pub(crate) fn process_is_alive(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Writes `bytes` to `path` and fsyncs the file before returning, so the contents
/// are on disk before the caller renames it into place.
pub(crate) fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    // Owner-only: this temp is renamed over state.json / preferences.json, which hold
    // composer drafts and queued-turn prompt text, so keep it 0600 rather than the umask
    // default — matching the socket / scrollback hardening.
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{PaneKind, PaneStatus};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_root() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("qmux-persist-{nanos}-{seq}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn global_drafts_round_trip() {
        use crate::state::{GlobalDraft, GlobalDraftConsumed};
        let root = temp_root();
        let drafts = vec![
            GlobalDraft {
                id: "draft-1".to_string(),
                text: "open draft".to_string(),
                created_at: 100,
                consumed: None,
            },
            GlobalDraft {
                id: "draft-2".to_string(),
                text: "assigned draft".to_string(),
                created_at: 200,
                consumed: Some(GlobalDraftConsumed {
                    agent_id: "agent-1".to_string(),
                    at: 300,
                }),
            },
        ];
        let state = PersistedState {
            global_drafts: drafts.clone(),
            ..Default::default()
        };
        save(&root, &state).unwrap();
        let outcome = load_with_diagnostics(&root);
        assert!(outcome.warning.is_none());
        assert_eq!(outcome.state.global_drafts, drafts);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preferences_round_trip_launcher_adapter() {
        let root = temp_root();
        assert_eq!(load_preferences(&root).unwrap().launcher_adapter_id, None);

        save_preferences(
            &root,
            &AppPreferences {
                launcher_adapter_id: Some("codex".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            load_preferences(&root).unwrap().launcher_adapter_id,
            Some("codex".to_string())
        );
        // Also read the disk directly: load_preferences serves repeats from the
        // write-through cache, and a cache hit alone would not prove the saved
        // file deserializes.
        assert_eq!(
            read_preferences_from_disk(&preferences_path(&root))
                .unwrap()
                .launcher_adapter_id,
            Some("codex".to_string())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preferences_round_trip_open_router_key() {
        let root = temp_root();
        // Absent in a fresh preferences file (no key configured).
        assert_eq!(load_preferences(&root).unwrap().open_router_key, None);

        save_preferences(
            &root,
            &AppPreferences {
                open_router_key: Some("sk-or-secret".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            load_preferences(&root).unwrap().open_router_key,
            Some("sk-or-secret".to_string())
        );
        assert_eq!(
            read_preferences_from_disk(&preferences_path(&root))
                .unwrap()
                .open_router_key,
            Some("sk-or-secret".to_string())
        );

        // The preferences file that holds the key is written owner-only (0600), so the
        // secret at rest isn't world/group readable.
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(preferences_path(&root))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "preferences file must not be group/other readable"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preferences_round_trip_use_login_shell() {
        let root = temp_root();
        // Absent in a fresh preferences file; the spawn path treats that as the
        // default (on).
        assert_eq!(load_preferences(&root).unwrap().use_login_shell, None);

        save_preferences(
            &root,
            &AppPreferences {
                use_login_shell: Some(false),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            load_preferences(&root).unwrap().use_login_shell,
            Some(false)
        );
        assert_eq!(
            read_preferences_from_disk(&preferences_path(&root))
                .unwrap()
                .use_login_shell,
            Some(false)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preferences_round_trip_worktree_location() {
        let root = temp_root();
        assert_eq!(load_preferences(&root).unwrap().worktree_location, None);

        save_preferences(
            &root,
            &AppPreferences {
                worktree_location: Some(WorktreeLocation::LocalClaude),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            read_preferences_from_disk(&preferences_path(&root))
                .unwrap()
                .worktree_location,
            Some(WorktreeLocation::LocalClaude)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_preference_updates_preserve_unrelated_fields() {
        let root = std::sync::Arc::new(temp_root());
        let launcher_root = root.clone();
        let key_root = root.clone();

        let launcher = std::thread::spawn(move || {
            for _ in 0..32 {
                update_preferences(&launcher_root, |preferences| {
                    preferences.launcher_adapter_id = Some("codex".to_string());
                })
                .unwrap();
            }
        });
        let key = std::thread::spawn(move || {
            for _ in 0..32 {
                update_preferences(&key_root, |preferences| {
                    preferences.open_router_key = Some("sk-or-secret".to_string());
                })
                .unwrap();
            }
        });

        launcher.join().unwrap();
        key.join().unwrap();
        let preferences = load_preferences(&root).unwrap();
        assert_eq!(preferences.launcher_adapter_id.as_deref(), Some("codex"));
        assert_eq!(preferences.open_router_key.as_deref(), Some("sk-or-secret"));
        fs::remove_dir_all(root.as_ref()).unwrap();
    }

    #[test]
    fn preference_update_does_not_replace_corrupt_file_with_defaults() {
        let root = temp_root();
        let path = preferences_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{not valid json").unwrap();

        let error = update_preferences(&root, |preferences| {
            preferences.use_login_shell = Some(false);
        })
        .unwrap_err();

        assert!(error.contains("invalid preferences"));
        assert_eq!(fs::read(&path).unwrap(), b"{not valid json");
        fs::remove_dir_all(root).unwrap();
    }

    fn sample_pane() -> PaneInfo {
        PaneInfo {
            id: "pane-1".to_string(),
            title: "Shell".to_string(),
            last_osc_title: None,
            kind: PaneKind::Shell,
            agent_id: None,
            group_id: "group-1".to_string(),
            cwd: "/tmp/work".to_string(),
            cols: 120,
            rows: 40,
            status: PaneStatus::Running,
            last_active_at: 0,
            recovered: false,
            depth: 0,
        }
    }

    #[test]
    fn load_missing_file_returns_empty_current_version() {
        let root = temp_root();
        let state = load_with_diagnostics(&root).state;
        assert_eq!(state.version, STATE_VERSION);
        assert!(state.panes.is_empty());
        assert!(state.agents.is_empty());
        assert!(state.queues.is_empty());
    }

    #[test]
    fn version_two_state_migrates_workspace_scope_without_losing_groups() {
        let root = temp_root();
        let path = state_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{
                "version": 2,
                "groups": [{
                    "id": "group-1",
                    "name": "work",
                    "dir": "/tmp/work",
                    "managedDir": "/tmp/qmux/group-1",
                    "baseRepo": null,
                    "baseRef": null,
                    "parentId": null,
                    "createdAt": 1,
                    "collapsed": false,
                    "agents": []
                }],
                "groupOrder": ["group-1"]
            }"#,
        )
        .unwrap();

        let outcome = load_with_diagnostics(&root);
        assert!(outcome.warning.is_none());
        assert_eq!(outcome.state.version, STATE_VERSION);
        assert_eq!(outcome.state.groups.len(), 1);
        assert_eq!(
            outcome.state.groups[0].scope,
            crate::workspace::WorkspaceScope::Terminal
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn version_two_shape_migration_backfills_workspace_and_drops_research_recents() {
        let mut value = serde_json::json!({
            "version": 2,
            "researchTrees": {
                "tree-1": { "rootNodeId": "node-1" }
            },
            "researchNodes": {
                "node-1": {
                    "groupId": "group-1",
                    "agentId": "research-agent",
                    "paneId": "research-pane",
                    "nativeSessionId": "research-session",
                    "transcriptPath": "/tmp/research.jsonl"
                }
            },
            "recentSessions": [
                { "id": "by-agent", "agentId": "research-agent" },
                { "id": "by-pane", "paneId": "research-pane" },
                { "id": "by-session", "sessionId": "research-session" },
                { "id": "by-transcript", "transcriptPath": "/tmp/research.jsonl" },
                { "id": "terminal", "agentId": "terminal-agent" }
            ]
        });

        migrate_v2_to_v3(&mut value);

        assert_eq!(value["version"], serde_json::json!(STATE_VERSION));
        assert_eq!(value["researchTrees"]["tree-1"]["workspaceId"], "group-1");
        assert_eq!(
            value["recentSessions"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|session| session["id"].as_str())
                .collect::<Vec<_>>(),
            vec!["terminal"]
        );
    }

    #[test]
    fn version_two_migration_backup_preserves_original_bytes_once() {
        let root = temp_root();
        let path = state_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = br#"{"version":2,"nextId":7}"#;
        fs::write(&path, original).unwrap();

        let backup = backup_v2_state_for_migration(&root)
            .unwrap()
            .expect("existing state is backed up");
        assert_eq!(fs::read(&backup).unwrap(), original);
        fs::write(&path, br#"{"version":3}"#).unwrap();
        assert_eq!(
            backup_v2_state_for_migration(&root).unwrap(),
            Some(backup.clone())
        );
        assert_eq!(fs::read(&backup).unwrap(), original);
        fs::remove_dir_all(root).unwrap();
    }

    // The path production startup actually takes: preflight reads and
    // version-checks the file, hands the bytes to the loader, and the loader
    // parses them without touching the disk again.
    #[test]
    fn preflight_bytes_feed_the_loader_without_a_second_read() {
        let root = temp_root();
        let mut persisted = PersistedState::default();
        persisted.panes.push(sample_pane());
        save(&root, &persisted).unwrap();

        let bytes = preflight_state(&root)
            .unwrap()
            .expect("existing state yields bytes");
        // Prove the loader consumed the handed-off bytes, not the file: replace
        // the on-disk state with garbage first.
        fs::write(state_path(&root), b"{ not json").unwrap();

        let outcome = load_with_diagnostics_from(&root, Some(bytes));
        assert!(outcome.warning.is_none());
        assert_eq!(outcome.state.panes.len(), 1);
        assert_eq!(outcome.state.panes[0].id, sample_pane().id);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preread_non_utf8_state_is_discarded_as_corrupt() {
        let root = temp_root();
        let path = state_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bad_state: &[u8] = b"\xff\xfe{\"version\":2}";
        fs::write(&path, bad_state).unwrap();

        let outcome = load_with_diagnostics_from(&root, Some(bad_state.to_vec()));
        assert_eq!(outcome.state.version, STATE_VERSION);
        assert!(outcome.state.panes.is_empty());
        let warning = outcome.warning.expect("non-UTF-8 state should warn");
        assert!(warning.message.contains("not valid UTF-8"));
        let backup_path = warning
            .backup_path
            .expect("non-UTF-8 state should be preserved");
        assert!(!path.exists());
        assert_eq!(fs::read(backup_path).unwrap(), bad_state);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preflight_reset_leaves_nothing_for_the_loader_to_resurrect() {
        // QMUX_RESET_STATE moves the file aside and preflight returns None; the
        // loader's disk fallback must then find nothing (fresh session), not
        // re-read the moved-aside state.
        let root = temp_root();
        let path = state_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"version":99999}"#).unwrap();

        let backup = preserve_rejected_state(&path, "newer-version").unwrap();
        assert!(!path.exists());
        assert!(backup.exists());
        let outcome = load_with_diagnostics_from(&root, None);
        assert!(outcome.warning.is_none());
        assert!(outcome.state.panes.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn load_corrupt_file_returns_empty_and_preserves_rejected_file() {
        let root = temp_root();
        let path = state_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bad_state = "{ this is not json";
        fs::write(&path, bad_state).unwrap();

        let outcome = load_with_diagnostics(&root);
        assert_eq!(outcome.state.version, STATE_VERSION);
        assert!(outcome.state.panes.is_empty());
        let warning = outcome.warning.expect("corrupt state should warn");
        assert!(warning.message.contains("invalid JSON"));
        let backup_path = warning
            .backup_path
            .expect("corrupt state should be preserved");
        assert!(!path.exists());
        assert_eq!(fs::read_to_string(backup_path).unwrap(), bad_state);
    }

    #[test]
    fn load_unknown_version_returns_empty_and_preserves_rejected_file() {
        let root = temp_root();
        let path = state_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bad_state = r#"{"version":99999,"panes":[{"id":"x"}]}"#;
        fs::write(&path, bad_state).unwrap();

        let outcome = load_with_diagnostics(&root);
        assert!(outcome.state.panes.is_empty());
        let warning = outcome
            .warning
            .expect("unsupported state version should warn");
        assert!(warning.message.contains("unsupported version 99999"));
        let backup_path = warning
            .backup_path
            .expect("unsupported state should be preserved");
        assert!(!path.exists());
        assert_eq!(fs::read_to_string(backup_path).unwrap(), bad_state);
    }

    #[test]
    fn load_drops_only_malformed_entries_and_keeps_the_rest() {
        let root = temp_root();
        // Start from a real, schema-valid snapshot with one good pane so the
        // surviving entry is produced by the actual serializer.
        let state = PersistedState {
            next_id: 5,
            panes: vec![sample_pane()],
            ..PersistedState::default()
        };
        save(&root, &state).unwrap();

        // Splice a malformed pane (missing required fields) beside the good one.
        let path = state_path(&root);
        let mut value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        value["panes"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({ "id": "broken" }));
        fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();

        let outcome = load_with_diagnostics(&root);
        // The good pane survives; only the malformed one is dropped, and other
        // top-level fields still load.
        assert_eq!(outcome.state.panes.len(), 1);
        assert_eq!(outcome.state.panes[0].id, "pane-1");
        assert_eq!(outcome.state.next_id, 5);
        let warning = outcome.warning.expect("dropped entries should warn");
        assert!(warning.message.contains("pane #1"));
        // A partial recovery rewrites the file on the next save rather than
        // discarding it, so nothing is renamed aside.
        assert!(warning.backup_path.is_none());
        assert!(path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pane_metadata_survives_round_trip() {
        let root = temp_root();
        let state = PersistedState {
            next_id: 17,
            panes: vec![sample_pane()],
            active_tab_id: Some("pane-1".to_string()),
            ..PersistedState::default()
        };
        save(&root, &state).unwrap();

        let loaded = load_with_diagnostics(&root).state;
        assert_eq!(loaded.next_id, 17);
        assert_eq!(loaded.active_tab_id.as_deref(), Some("pane-1"));
        assert_eq!(loaded.panes.len(), 1);
        let pane = &loaded.panes[0];
        assert_eq!(pane.id, "pane-1");
        assert_eq!(pane.cwd, "/tmp/work");
        assert_eq!(pane.cols, 120);
        assert_eq!(pane.rows, 40);
        assert!(matches!(pane.kind, PaneKind::Shell));
    }

    #[test]
    fn preflight_refuses_state_written_by_a_newer_version() {
        let root = temp_root();
        let path = state_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let newer = format!(r#"{{"version":{},"panes":[]}}"#, STATE_VERSION + 1);
        fs::write(&path, &newer).unwrap();

        let err = preflight_state(&root).expect_err("newer-version state must refuse startup");
        assert!(err.contains("newer version"));
        // The file is left untouched so the newer install keeps its session.
        assert_eq!(fs::read_to_string(&path).unwrap(), newer);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preflight_accepts_missing_current_older_and_corrupt_state() {
        let root = temp_root();
        assert!(
            preflight_state(&root).is_ok(),
            "missing state is a first run"
        );

        let path = state_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!(r#"{{"version":{STATE_VERSION}}}"#)).unwrap();
        assert!(preflight_state(&root).is_ok());

        // Older and corrupt state are preflight-clean: load preserves them as a
        // .bak before starting fresh, so nothing is silently overwritten.
        fs::write(&path, r#"{"version":1}"#).unwrap();
        assert!(preflight_state(&root).is_ok());
        fs::write(&path, "{ not json").unwrap();
        assert!(preflight_state(&root).is_ok());
        // Invalid UTF-8 is corrupt content, not an I/O failure: it must be
        // preflight-clean too (read as bytes), not a permanent startup refusal.
        fs::write(&path, [0x7b, 0xff, 0xfe, 0x00, 0x80]).unwrap();
        assert!(
            preflight_state(&root).is_ok(),
            "non-UTF-8 state is corrupt content, recoverable via load's .bak path"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_tmp_cleanup_removes_dead_writers_and_keeps_everything_else() {
        let root = temp_root();
        let dir = root.join(STATE_DIR);
        fs::create_dir_all(&dir).unwrap();

        // A pid that is certainly dead: a reaped child's pid is gone until recycled.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();

        let dead = dir.join(format!("state.json.{dead_pid}.7.tmp"));
        let dead_prefs = dir.join(format!("preferences.json.{dead_pid}.9.tmp"));
        let live = dir.join(format!("state.json.{}.8.tmp", std::process::id()));
        let state_file = dir.join(STATE_FILE);
        let preserved = dir.join(format!("state.json.corrupt.123.{dead_pid}.0.bak"));
        for path in [&dead, &dead_prefs, &live, &state_file, &preserved] {
            fs::write(path, b"x").unwrap();
        }

        remove_stale_tmp_files(&root);

        assert!(!dead.exists());
        assert!(!dead_prefs.exists());
        assert!(live.exists(), "a live writer's scratch file must survive");
        assert!(state_file.exists());
        assert!(preserved.exists(), ".bak preserves must survive");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file() {
        let root = temp_root();
        let state = PersistedState::default();
        save(&root, &state).unwrap();

        let path = state_path(&root);
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());
    }

    // Regression: persist() releases the model lock before writing, so saves can
    // overlap. A shared temp name let one writer's rename pull the file out from
    // under another, which then failed to commit with ENOENT.
    #[test]
    fn concurrent_saves_do_not_race_and_leave_no_temp_files() {
        use std::sync::Arc;
        use std::thread;

        let root = Arc::new(temp_root());
        let parent = state_path(&root).parent().unwrap().to_path_buf();
        fs::create_dir_all(&parent).unwrap();

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let root = Arc::clone(&root);
                thread::spawn(move || {
                    for _ in 0..50 {
                        save(&root, &PersistedState::default()).expect("save must not fail");
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        // A complete, parseable snapshot survives and nothing is stranded.
        assert_eq!(load_with_diagnostics(&root).state.version, STATE_VERSION);
        let leftover_temps: Vec<_> = fs::read_dir(&parent)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(
            leftover_temps.is_empty(),
            "stranded temp files: {leftover_temps:?}"
        );
    }
}
