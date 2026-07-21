use crate::transcript::{Turn, TurnBlock, TurnStatus, TurnStatusReason};
use crate::workspace::AgentInfo;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::Duration;

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
static THREAD_LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// How long the graph flusher lets a burst of mutations settle before writing.
/// A streaming turn appends transcript lines every tail tick; without this each
/// line re-wrote (and fsynced) the whole snapshot file.
const GRAPH_FLUSH_DEBOUNCE: Duration = Duration::from_millis(500);

/// In-memory authority for thread graphs, keyed by (storage root, thread id).
///
/// Every transcript line used to read+parse the whole snapshot off disk, mutate
/// it, and rewrite it with two fsyncs — O(conversation) I/O per appended line.
/// Mutations now run against this cache and only mark the entry dirty; a single
/// flusher thread writes dirty graphs on a debounce, and reads are served from
/// the cache so they always observe the newest mutation rather than a
/// yet-to-be-flushed disk file. Test builds bypass the cache entirely (reads
/// and writes stay synchronous on disk) so tests can assert snapshot files
/// immediately after a mutation.
///
/// A crash can lose at most the last debounce window of graph updates; graphs
/// derive from transcripts, which the next launch re-tails.
struct CachedGraph {
    graph: ThreadGraph,
    dirty: bool,
}

static GRAPH_CACHE: LazyLock<Mutex<HashMap<(String, String), CachedGraph>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static GRAPH_FLUSH_PENDING: LazyLock<(Mutex<bool>, Condvar)> =
    LazyLock::new(|| (Mutex::new(false), Condvar::new()));
static GRAPH_FLUSHER_SPAWNED: AtomicBool = AtomicBool::new(false);

fn graph_cache_enabled() -> bool {
    !cfg!(test)
}

fn cached_graph(storage_root: &str, thread_id: &str) -> Option<ThreadGraph> {
    let cache = GRAPH_CACHE.lock().unwrap_or_else(|err| err.into_inner());
    cache
        .get(&(storage_root.to_string(), thread_id.to_string()))
        .map(|entry| entry.graph.clone())
}

fn cache_graph(storage_root: &str, thread_id: &str, graph: ThreadGraph, dirty: bool) {
    let mut cache = GRAPH_CACHE.lock().unwrap_or_else(|err| err.into_inner());
    cache.insert(
        (storage_root.to_string(), thread_id.to_string()),
        CachedGraph { graph, dirty },
    );
}

fn schedule_graph_flush() {
    if GRAPH_FLUSHER_SPAWNED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        std::thread::spawn(graph_flusher_loop);
    }
    let (pending, wake) = &*GRAPH_FLUSH_PENDING;
    let mut pending = pending.lock().unwrap_or_else(|err| err.into_inner());
    *pending = true;
    wake.notify_one();
}

fn graph_flusher_loop() {
    loop {
        {
            let (pending, wake) = &*GRAPH_FLUSH_PENDING;
            let mut pending = pending.lock().unwrap_or_else(|err| err.into_inner());
            while !*pending {
                pending = wake.wait(pending).unwrap_or_else(|err| err.into_inner());
            }
            *pending = false;
        }
        // Let the rest of the burst land; marks made during the sleep are
        // covered by the flush below, so absorb them instead of re-looping.
        std::thread::sleep(GRAPH_FLUSH_DEBOUNCE);
        {
            let (pending, _) = &*GRAPH_FLUSH_PENDING;
            *pending.lock().unwrap_or_else(|err| err.into_inner()) = false;
        }
        flush_dirty_thread_graphs();
    }
}

/// Writes every dirty cached graph to disk. Called by the flusher after each
/// debounce window and once at exit (alongside the state.json final snapshot)
/// so a clean quit never loses graph updates.
pub fn flush_dirty_thread_graphs() {
    // Snapshot the dirty entries without holding the cache lock across disk
    // writes; clearing the flag in the same critical section means a mutation
    // racing the write simply re-marks the entry for the next cycle.
    let dirty = {
        let mut cache = GRAPH_CACHE.lock().unwrap_or_else(|err| err.into_inner());
        cache
            .iter_mut()
            .filter(|(_, entry)| entry.dirty)
            .map(|(key, entry)| {
                entry.dirty = false;
                (key.clone(), entry.graph.clone())
            })
            .collect::<Vec<_>>()
    };
    for ((storage_root, thread_id), graph) in dirty {
        if let Err(err) = write_snapshot_to_disk(&storage_root, &graph) {
            eprintln!("qmux: failed to flush thread graph {thread_id}: {err}");
            // Re-mark so the next flush (or the exit flush) retries instead of
            // silently dropping the update.
            let mut cache = GRAPH_CACHE.lock().unwrap_or_else(|err| err.into_inner());
            if let Some(entry) = cache.get_mut(&(storage_root, thread_id)) {
                entry.dirty = true;
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRecord {
    pub id: String,
    pub storage_root: String,
    pub snapshot_path: String,
    pub default_focused_branch_id: String,
    pub created_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadGraph {
    pub version: u32,
    pub thread_id: String,
    pub focused_branch_id: String,
    pub next_created_order: u64,
    pub root_turn_ids: Vec<String>,
    pub branches: HashMap<String, ThreadBranch>,
    pub nodes: HashMap<String, ThreadNode>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadBranch {
    pub id: String,
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_branch_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_from_turn_id: Option<String>,
    pub head_turn_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by_actor_id: Option<String>,
    pub created_at: u64,
    pub status: ThreadBranchStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ThreadBranchStatus {
    Active,
    Archived,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ThreadNode {
    #[serde(rename = "turn")]
    Turn(TurnNode),
    #[serde(rename = "handoff")]
    Handoff(HandoffNode),
    #[serde(rename = "branchStart")]
    BranchStart(BranchStartNode),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnNode {
    #[serde(flatten)]
    pub base: BaseThreadNode,
    pub turn: ThreadTurnContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<NativeTurnRef>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffNode {
    #[serde(flatten)]
    pub base: BaseThreadNode,
    pub handoff: HandoffPayload,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchStartNode {
    #[serde(flatten)]
    pub base: BaseThreadNode,
    pub branch_start: BranchStartPayload,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BaseThreadNode {
    pub id: String,
    pub thread_id: String,
    pub branch_id: String,
    pub parent_turn_ids: Vec<String>,
    pub participant: ThreadParticipant,
    pub created_at: u64,
    pub created_order: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TurnStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<TurnStatusReason>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTurnContent {
    pub role: String,
    pub blocks: Vec<TurnBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_index: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadParticipant {
    pub kind: ThreadParticipantKind,
    pub actor_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ThreadParticipantKind {
    User,
    Assistant,
    Qmux,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffPayload {
    pub source_agent_id: String,
    pub source_adapter: String,
    pub source_branch_id: String,
    pub source_turn_id: String,
    pub target_agent_id: String,
    pub target_adapter: String,
    pub target_branch_id: String,
    pub context_path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchStartPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_branch_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_turn_id: Option<String>,
    pub target_branch_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeTurnRef {
    pub adapter: String,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_native_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_message_id: Option<String>,
    pub source_index: usize,
}

impl ThreadGraph {
    pub fn empty_for_agent(agent: &AgentInfo) -> Self {
        let thread_id = agent_thread_id(agent);
        let branch_id = agent_branch_id(agent);
        let mut branches = HashMap::new();
        branches.insert(
            branch_id.clone(),
            ThreadBranch {
                id: branch_id.clone(),
                thread_id: thread_id.clone(),
                parent_branch_id: None,
                base_turn_id: None,
                created_from_turn_id: None,
                head_turn_ids: Vec::new(),
                label: None,
                created_by_agent_id: Some(agent.id.clone()),
                created_by_actor_id: Some(agent.id.clone()),
                created_at: agent_created_at(agent),
                status: ThreadBranchStatus::Active,
            },
        );
        Self {
            version: 1,
            thread_id,
            focused_branch_id: branch_id,
            next_created_order: 0,
            root_turn_ids: Vec::new(),
            branches,
            nodes: HashMap::new(),
        }
    }
}

pub fn agent_thread_id(agent: &AgentInfo) -> String {
    agent
        .thread_id
        .clone()
        .unwrap_or_else(|| format!("thread-{}", agent.id))
}

pub fn agent_branch_id(agent: &AgentInfo) -> String {
    agent
        .branch_id
        .clone()
        .unwrap_or_else(|| format!("branch-{}", agent.id))
}

pub fn snapshot_path(storage_root: &str, thread_id: &str) -> PathBuf {
    Path::new(storage_root)
        .join(".qmux")
        .join("threads")
        .join(format!("{thread_id}.json"))
}

pub fn thread_record_for_agent(
    agent: &AgentInfo,
    default_focused_branch_id: &str,
    storage_root: &Path,
) -> ThreadRecord {
    let thread_id = agent_thread_id(agent);
    let storage_root = storage_root.display().to_string();
    ThreadRecord {
        id: thread_id.clone(),
        snapshot_path: snapshot_path(&storage_root, &thread_id)
            .display()
            .to_string(),
        storage_root,
        default_focused_branch_id: default_focused_branch_id.to_string(),
        created_at: agent_created_at(agent),
    }
}

pub fn migrate_record_to_storage_root(
    record: &mut ThreadRecord,
    storage_root: &Path,
) -> Result<bool, String> {
    let destination_root = storage_root.display().to_string();
    let destination_path = snapshot_path(&destination_root, &record.id);
    let record_is_current = record.storage_root == destination_root
        && Path::new(&record.snapshot_path) == destination_path;

    if let Some(graph) = read_snapshot(&destination_root, &record.id)? {
        validate_snapshot_thread_id(&graph, &record.id, &destination_path)?;
        if record_is_current {
            return Ok(false);
        }
        update_record_storage(record, destination_root, destination_path);
        return Ok(true);
    }

    if record_is_current {
        return Ok(false);
    }

    if let Some(graph) = read_snapshot(&record.storage_root, &record.id)? {
        let source_path = snapshot_path(&record.storage_root, &record.id);
        validate_snapshot_thread_id(&graph, &record.id, &source_path)?;
        write_snapshot(&destination_root, &graph)?;
    }

    update_record_storage(record, destination_root, destination_path);
    Ok(true)
}

fn update_record_storage(record: &mut ThreadRecord, storage_root: String, snapshot_path: PathBuf) {
    record.storage_root = storage_root;
    record.snapshot_path = snapshot_path.display().to_string();
}

fn validate_snapshot_thread_id(
    graph: &ThreadGraph,
    expected_thread_id: &str,
    path: &Path,
) -> Result<(), String> {
    if graph.thread_id == expected_thread_id {
        return Ok(());
    }
    Err(format!(
        "thread graph {} contains thread id {}, expected {expected_thread_id}",
        path.display(),
        graph.thread_id
    ))
}

pub fn read_snapshot(storage_root: &str, thread_id: &str) -> Result<Option<ThreadGraph>, String> {
    if graph_cache_enabled() {
        if let Some(graph) = cached_graph(storage_root, thread_id) {
            return Ok(Some(graph));
        }
    }
    let loaded = read_snapshot_from_disk(storage_root, thread_id)?;
    if graph_cache_enabled()
        && let Some(graph) = &loaded
    {
        // Populate-if-vacant, never overwrite: this read path holds no
        // per-thread lock, so between the cache miss above and here a mutation
        // (which does hold the lock) can have cached a newer, dirty graph.
        // Overwriting it with the pre-mutation disk copy marked clean would
        // drop that update from memory and skip its flush to disk. When the
        // entry got filled in the meantime, that newer copy wins.
        let mut cache = GRAPH_CACHE.lock().unwrap_or_else(|err| err.into_inner());
        let entry = cache
            .entry((storage_root.to_string(), thread_id.to_string()))
            .or_insert_with(|| CachedGraph {
                graph: graph.clone(),
                dirty: false,
            });
        return Ok(Some(entry.graph.clone()));
    }
    Ok(loaded)
}

fn read_snapshot_from_disk(
    storage_root: &str,
    thread_id: &str,
) -> Result<Option<ThreadGraph>, String> {
    let path = snapshot_path(storage_root, thread_id);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(format!(
                "failed to read thread graph {}: {err}",
                path.display()
            ));
        }
    };
    serde_json::from_str::<ThreadGraph>(&raw)
        .map(Some)
        .map_err(|err| format!("invalid thread graph {}: {err}", path.display()))
}

pub fn write_snapshot(storage_root: &str, graph: &ThreadGraph) -> Result<(), String> {
    write_snapshot_to_disk(storage_root, graph)?;
    // Keep the cache coherent for direct writers (thread-record migration at
    // startup): the disk write succeeded, so the cached copy is clean. A dirty
    // entry is left alone — this writer holds no per-thread lock, and a dirty
    // entry means a locked mutation has produced something newer than what was
    // just written; the flusher will bring disk up to date with it.
    if graph_cache_enabled() {
        let mut cache = GRAPH_CACHE.lock().unwrap_or_else(|err| err.into_inner());
        let entry = cache
            .entry((storage_root.to_string(), graph.thread_id.clone()))
            .or_insert_with(|| CachedGraph {
                graph: graph.clone(),
                dirty: false,
            });
        if !entry.dirty {
            entry.graph = graph.clone();
        }
    }
    Ok(())
}

fn write_snapshot_to_disk(storage_root: &str, graph: &ThreadGraph) -> Result<(), String> {
    let path = snapshot_path(storage_root, &graph.thread_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create thread graph dir {}: {err}",
                parent.display()
            )
        })?;
        if let Some(state_dir) = parent.parent() {
            fs::set_permissions(state_dir, fs::Permissions::from_mode(0o700)).map_err(|err| {
                format!(
                    "failed to set permissions on thread state dir {}: {err}",
                    state_dir.display()
                )
            })?;
        }
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(|err| {
            format!(
                "failed to set permissions on thread graph dir {}: {err}",
                parent.display()
            )
        })?;
    }
    // Compact rather than pretty: snapshots are machine-read only and rewritten
    // in full on every flush, so pretty encoding was pure serialize+write cost.
    let raw = serde_json::to_vec(graph)
        .map_err(|err| format!("failed to encode thread graph {}: {err}", path.display()))?;
    atomic_write_owner_only(&path, &raw)
}

#[derive(Clone, Debug)]
pub struct ThreadStore {
    storage_root: PathBuf,
}

impl ThreadStore {
    pub fn new(storage_root: impl Into<PathBuf>) -> Self {
        Self {
            storage_root: storage_root.into(),
        }
    }

    pub fn read_thread(&self, thread_id: &str) -> Result<Option<ThreadGraph>, String> {
        read_snapshot(&self.storage_root_string(), thread_id)
    }

    pub fn append_turn_node(&self, agent: &AgentInfo, turn: &Turn) -> Result<ThreadGraph, String> {
        let thread_id = agent_thread_id(agent);
        self.with_agent_graph(agent, |graph| {
            ensure_next_created_order(graph);
            let branch_id = agent_branch_id(agent);
            ensure_branch(graph, agent, &branch_id);
            if let Some(existing) = graph.nodes.get(&turn.id) {
                let mut node = turn_node_from_turn(agent, turn, Vec::new(), 0);
                if let ThreadNode::Turn(existing) = existing {
                    node.base.parent_turn_ids = existing.base.parent_turn_ids.clone();
                    node.base.created_at = existing.base.created_at;
                    node.base.created_order = existing.base.created_order;
                }
                // An appended turn is the branch's newest. A same-id re-delivery of
                // the branch tail keeps its slot, but a positional id collision with
                // a mid-branch node (a rotated/rewritten transcript's numbering
                // overlapping a longer past one) must not inherit that slot — every
                // later turn would render above this one. Re-tail it instead.
                let lands_mid_branch = branch_max_turn_order(graph, &branch_id)
                    .is_some_and(|max_order| node.base.created_order < max_order);
                if lands_mid_branch {
                    node.base.parent_turn_ids = graph
                        .branches
                        .get(&branch_id)
                        .map(|branch| branch.head_turn_ids.clone())
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|head_id| head_id != &turn.id)
                        .collect();
                    node.base.created_order = allocate_created_order(graph);
                    graph.nodes.insert(turn.id.clone(), ThreadNode::Turn(node));
                    if let Some(branch) = graph.branches.get_mut(&branch_id) {
                        branch.head_turn_ids = vec![turn.id.clone()];
                    }
                    recompute_root_turn_ids(graph);
                    return;
                }
                graph.nodes.insert(turn.id.clone(), ThreadNode::Turn(node));
                recompute_root_turn_ids(graph);
                return;
            }

            let parent_turn_ids = graph
                .branches
                .get(&branch_id)
                .map(|branch| branch.head_turn_ids.clone())
                .unwrap_or_default();
            let created_order = allocate_created_order(graph);
            graph.nodes.insert(
                turn.id.clone(),
                ThreadNode::Turn(turn_node_from_turn(
                    agent,
                    turn,
                    parent_turn_ids,
                    created_order,
                )),
            );
            if let Some(branch) = graph.branches.get_mut(&branch_id) {
                branch.head_turn_ids = vec![turn.id.clone()];
            }
            recompute_root_turn_ids(graph);
        })
        .map_err(|err| format!("thread {thread_id}: {err}"))
    }

    pub fn replace_agent_branch_turns(
        &self,
        agent: &AgentInfo,
        turns: &[Turn],
    ) -> Result<ThreadGraph, String> {
        let thread_id = agent_thread_id(agent);
        self.with_agent_graph(agent, |graph| {
            ensure_next_created_order(graph);
            let branch_id = agent_branch_id(agent);
            ensure_branch(graph, agent, &branch_id);

            let scoped_nodes = graph
                .nodes
                .iter()
                .filter_map(|(node_id, node)| match node {
                    ThreadNode::Turn(turn_node)
                        if turn_node.base.branch_id == branch_id
                            && turn_node.native.as_ref().is_some_and(|native| {
                                native.agent_id == agent.id && native.adapter == agent.adapter
                            }) =>
                    {
                        Some((node_id.clone(), turn_node.clone()))
                    }
                    _ => None,
                })
                .collect::<HashMap<_, _>>();
            let scoped_ids = scoped_nodes.keys().cloned().collect::<HashSet<_>>();
            let base_parent_turn_ids = scoped_nodes
                .values()
                .min_by_key(|node| node.base.created_order)
                .map(|node| node.base.parent_turn_ids.clone())
                .unwrap_or_else(|| {
                    graph
                        .branches
                        .get(&branch_id)
                        .map(|branch| branch.head_turn_ids.clone())
                        .unwrap_or_default()
                })
                .into_iter()
                .filter(|id| !scoped_ids.contains(id))
                .collect::<Vec<_>>();

            for node_id in scoped_ids {
                graph.nodes.remove(&node_id);
            }

            // Turn ids are positional (`{agent}-{source line}`), so a rewritten or
            // rotated transcript (a codex rewind that reuses line indexes, a rebind
            // onto a replayed rollout) can reuse an id for different content at a
            // different position. Keeping an id-matched node's created_order is only
            // sound while the kept orders, read in list order, stay strictly
            // increasing and no unmatched turn precedes a matched one — a fresh
            // allocation always exceeds every existing order, so a kept order after
            // it would sort the branch out of transcript order (continued turns
            // rendering above the last message). On any conflict, reassign the whole
            // list in order instead of inheriting.
            let mut kept_orders_consistent = true;
            let mut last_kept_order: Option<u64> = None;
            let mut needs_fresh_order = false;
            for turn in turns {
                match scoped_nodes.get(&turn.id) {
                    Some(existing) => {
                        let order = existing.base.created_order;
                        if needs_fresh_order
                            || last_kept_order.is_some_and(|previous| previous >= order)
                        {
                            kept_orders_consistent = false;
                            break;
                        }
                        last_kept_order = Some(order);
                    }
                    None => needs_fresh_order = true,
                }
            }

            let mut previous_turn_id: Option<String> = None;
            for turn in turns {
                let parent_turn_ids = previous_turn_id
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .into_iter()
                    .chain(if previous_turn_id.is_none() {
                        base_parent_turn_ids.clone()
                    } else {
                        Vec::new()
                    })
                    .collect::<Vec<_>>();
                let mut node = turn_node_from_turn(agent, turn, parent_turn_ids, 0);
                match scoped_nodes.get(&turn.id) {
                    Some(existing) if kept_orders_consistent => {
                        node.base.created_at = existing.base.created_at;
                        node.base.created_order = existing.base.created_order;
                    }
                    Some(existing) => {
                        node.base.created_at = existing.base.created_at;
                        node.base.created_order = allocate_created_order(graph);
                    }
                    None => {
                        node.base.created_order = allocate_created_order(graph);
                    }
                }
                graph.nodes.insert(turn.id.clone(), ThreadNode::Turn(node));
                previous_turn_id = Some(turn.id.clone());
            }

            if let Some(branch) = graph.branches.get_mut(&branch_id) {
                branch.head_turn_ids = previous_turn_id
                    .into_iter()
                    .collect::<Vec<_>>()
                    .into_iter()
                    .chain(base_parent_turn_ids)
                    .filter(|id| graph.nodes.contains_key(id))
                    .collect();
            }
            recompute_root_turn_ids(graph);
        })
        .map_err(|err| format!("thread {thread_id}: {err}"))
    }

    fn with_agent_graph<F>(&self, agent: &AgentInfo, mutate: F) -> Result<ThreadGraph, String>
    where
        F: FnOnce(&mut ThreadGraph),
    {
        let thread_id = agent_thread_id(agent);
        self.with_existing_or_else(&thread_id, || ThreadGraph::empty_for_agent(agent), mutate)
    }

    fn with_existing_or_else<F, G>(
        &self,
        thread_id: &str,
        default_graph: G,
        mutate: F,
    ) -> Result<ThreadGraph, String>
    where
        F: FnOnce(&mut ThreadGraph),
        G: FnOnce() -> ThreadGraph,
    {
        let lock = thread_lock(thread_id)?;
        let _guard = lock
            .lock()
            .map_err(|_| format!("thread graph lock poisoned for {thread_id}"))?;
        let storage_root = self.storage_root_string();
        let mut graph = read_snapshot(&storage_root, thread_id)?.unwrap_or_else(default_graph);
        mutate(&mut graph);
        if graph_cache_enabled() {
            // Mutations are memory-first: update the cache (the read authority)
            // and let the debounced flusher batch the disk write. Tests keep the
            // synchronous write below so snapshot files can be asserted directly.
            cache_graph(&storage_root, thread_id, graph.clone(), true);
            schedule_graph_flush();
        } else {
            write_snapshot_to_disk(&storage_root, &graph)?;
        }
        Ok(graph)
    }

    fn storage_root_string(&self) -> String {
        self.storage_root.display().to_string()
    }
}

fn ensure_branch(graph: &mut ThreadGraph, agent: &AgentInfo, branch_id: &str) {
    if graph.branches.contains_key(branch_id) {
        return;
    }
    graph.branches.insert(
        branch_id.to_string(),
        ThreadBranch {
            id: branch_id.to_string(),
            thread_id: graph.thread_id.clone(),
            parent_branch_id: None,
            base_turn_id: None,
            created_from_turn_id: None,
            head_turn_ids: Vec::new(),
            label: None,
            created_by_agent_id: Some(agent.id.clone()),
            created_by_actor_id: Some(agent.id.clone()),
            created_at: agent_created_at(agent),
            status: ThreadBranchStatus::Active,
        },
    );
    if graph.focused_branch_id.trim().is_empty() {
        graph.focused_branch_id = branch_id.to_string();
    }
}

fn turn_node_from_turn(
    agent: &AgentInfo,
    turn: &Turn,
    parent_turn_ids: Vec<String>,
    created_order: u64,
) -> TurnNode {
    TurnNode {
        base: BaseThreadNode {
            id: turn.id.clone(),
            thread_id: agent_thread_id(agent),
            branch_id: agent_branch_id(agent),
            parent_turn_ids,
            participant: participant_for_turn(agent, turn),
            created_at: agent_created_at(agent),
            created_order,
            status: turn.status,
            status_reason: turn.status_reason,
        },
        turn: ThreadTurnContent {
            role: turn.role.clone(),
            blocks: turn.blocks.clone(),
            source_index: Some(turn.source_index),
        },
        native: Some(NativeTurnRef {
            adapter: agent.adapter.clone(),
            agent_id: agent.id.clone(),
            session_id: turn.session_id.clone().or_else(|| agent.session_id.clone()),
            transcript_path: agent.transcript_path.clone(),
            native_id: turn.native_id.clone(),
            parent_native_id: turn.parent_native_id.clone(),
            native_message_id: turn.native_message_id.clone(),
            source_index: turn.source_index,
        }),
    }
}

fn participant_for_turn(agent: &AgentInfo, turn: &Turn) -> ThreadParticipant {
    if turn.role == "user" {
        ThreadParticipant {
            kind: ThreadParticipantKind::User,
            actor_id: "local-user".to_string(),
            adapter: None,
            agent_id: None,
            label: Some("You".to_string()),
        }
    } else {
        ThreadParticipant {
            kind: ThreadParticipantKind::Assistant,
            actor_id: agent.id.clone(),
            adapter: Some(agent.adapter.clone()),
            agent_id: Some(agent.id.clone()),
            label: Some(adapter_label(&agent.adapter).to_string()),
        }
    }
}

fn adapter_label(adapter: &str) -> &str {
    match adapter {
        "claude" => "Claude",
        "codex" => "Codex",
        "opencode" => "OpenCode",
        "grok" => "Grok",
        _ => "Agent",
    }
}

fn agent_created_at(agent: &AgentInfo) -> u64 {
    agent.created_at.min(u64::MAX as u128) as u64
}

fn ensure_next_created_order(graph: &mut ThreadGraph) {
    let next = graph
        .nodes
        .values()
        .map(node_created_order)
        .max()
        .map(|order| order.saturating_add(1))
        .unwrap_or(0);
    if graph.next_created_order < next {
        graph.next_created_order = next;
    }
}

fn allocate_created_order(graph: &mut ThreadGraph) -> u64 {
    ensure_next_created_order(graph);
    let order = graph.next_created_order;
    graph.next_created_order = graph.next_created_order.saturating_add(1);
    order
}

fn recompute_root_turn_ids(graph: &mut ThreadGraph) {
    let node_ids = graph.nodes.keys().cloned().collect::<HashSet<_>>();
    let mut roots = graph
        .nodes
        .iter()
        .filter_map(|(node_id, node)| {
            let base = node_base(node);
            if base
                .parent_turn_ids
                .iter()
                .any(|parent_id| node_ids.contains(parent_id))
            {
                None
            } else {
                Some((base.created_order, node_id.clone()))
            }
        })
        .collect::<Vec<_>>();
    roots.sort_by_key(|(order, node_id)| (*order, node_id.clone()));
    graph.root_turn_ids = roots.into_iter().map(|(_, node_id)| node_id).collect();
}

fn node_base(node: &ThreadNode) -> &BaseThreadNode {
    match node {
        ThreadNode::Turn(node) => &node.base,
        ThreadNode::Handoff(node) => &node.base,
        ThreadNode::BranchStart(node) => &node.base,
    }
}

fn node_created_order(node: &ThreadNode) -> u64 {
    node_base(node).created_order
}

/// Highest created_order among a branch's turn nodes — the branch tail's slot.
fn branch_max_turn_order(graph: &ThreadGraph, branch_id: &str) -> Option<u64> {
    graph
        .nodes
        .values()
        .filter_map(|node| match node {
            ThreadNode::Turn(turn_node) if turn_node.base.branch_id == branch_id => {
                Some(turn_node.base.created_order)
            }
            _ => None,
        })
        .max()
}

fn thread_lock(thread_id: &str) -> Result<Arc<Mutex<()>>, String> {
    let mut locks = THREAD_LOCKS
        .lock()
        .map_err(|_| "thread graph lock map poisoned".to_string())?;
    Ok(locks
        .entry(thread_id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone())
}

fn atomic_write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("thread graph path {} has no parent", path.display()))?;
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let tmp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("thread"),
        pid,
        seq
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|err| format!("failed to create {}: {err}", tmp.display()))?;
    file.write_all(bytes)
        .map_err(|err| format!("failed to write {}: {err}", tmp.display()))?;
    file.write_all(b"\n")
        .map_err(|err| format!("failed to write {}: {err}", tmp.display()))?;
    file.sync_all()
        .map_err(|err| format!("failed to sync {}: {err}", tmp.display()))?;
    drop(file);
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("failed to set permissions on {}: {err}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|err| format!("failed to replace {}: {err}", path.display()))?;
    let _ = OpenOptions::new()
        .read(true)
        .open(parent)
        .and_then(|dir| dir.sync_all());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::TurnBlock;
    use crate::workspace::AgentStatus;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    fn sample_agent(worktree_dir: String) -> AgentInfo {
        sample_agent_with("agent-1", "codex", "thread-1", "branch-1", worktree_dir)
    }

    fn sample_agent_with(
        id: &str,
        adapter: &str,
        thread_id: &str,
        branch_id: &str,
        worktree_dir: String,
    ) -> AgentInfo {
        AgentInfo {
            id: id.to_string(),
            group_id: "group-1".to_string(),
            adapter: adapter.to_string(),
            worktree_dir,
            branch: None,
            pane_id: None,
            orphaned_queue_pane_id: None,
            session_id: Some(format!("{id}-session")),
            transcript_path: Some(format!("/tmp/{id}-session.jsonl")),
            status: AgentStatus::Done,
            model: None,
            effort: None,
            parent_id: None,
            fork_point: None,
            root_session_id: None,
            thread_id: Some(thread_id.to_string()),
            branch_id: Some(branch_id.to_string()),
            paused: false,
            created_at: 123,
        }
    }

    fn sample_turn(agent_id: &str, id: &str, role: &str, source_index: usize) -> Turn {
        Turn {
            id: id.to_string(),
            agent_id: agent_id.to_string(),
            session_id: None,
            role: role.to_string(),
            blocks: vec![TurnBlock::Text {
                text: format!("{role} text"),
            }],
            source_index,
            timestamp: None,
            status: None,
            status_reason: None,
            native_id: Some(format!("native-{id}")),
            parent_native_id: None,
            native_message_id: None,
        }
    }

    fn temp_worktree(prefix: &str) -> PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or_default();
        let dir =
            std::env::temp_dir().join(format!("{prefix}-{}-{millis}-{seq}", std::process::id(),));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn snapshot_round_trips_owner_only_graph() {
        let worktree = temp_worktree("qmux-thread-graph-roundtrip");
        let agent = sample_agent(worktree.display().to_string());
        let turns = vec![sample_turn("agent-1", "turn-1", "user", 0)];
        let store = ThreadStore::new(worktree.clone());

        store.replace_agent_branch_turns(&agent, &turns).unwrap();

        let path = snapshot_path(&agent.worktree_dir, "thread-1");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "snapshot must be owner-only");
        let dir_mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o077, 0, "thread directory must be owner-only");

        let graph = read_snapshot(&agent.worktree_dir, "thread-1")
            .unwrap()
            .expect("snapshot exists");
        assert_eq!(graph.nodes.len(), 1);
        assert!(graph.nodes.contains_key("turn-1"));

        fs::remove_dir_all(worktree).unwrap();
    }

    #[test]
    fn duplicate_append_preserves_graph_position() {
        let worktree = temp_worktree("qmux-thread-graph-append");
        let agent = sample_agent(worktree.display().to_string());
        let first = sample_turn("agent-1", "turn-1", "user", 0);
        let second = sample_turn("agent-1", "turn-2", "assistant", 1);
        let store = ThreadStore::new(worktree.clone());
        store.append_turn_node(&agent, &first).unwrap();
        store.append_turn_node(&agent, &second).unwrap();

        let mut updated = second.clone();
        updated.blocks = vec![TurnBlock::Text {
            text: "updated text".to_string(),
        }];
        store.append_turn_node(&agent, &updated).unwrap();

        let graph = read_snapshot(&agent.worktree_dir, "thread-1")
            .unwrap()
            .expect("snapshot exists");
        let Some(ThreadNode::Turn(node)) = graph.nodes.get("turn-2") else {
            panic!("missing updated turn");
        };
        assert_eq!(node.base.parent_turn_ids, vec!["turn-1"]);
        assert_eq!(node.base.created_order, 1);
        assert_eq!(graph.next_created_order, 2);
        match node.turn.blocks.as_slice() {
            [TurnBlock::Text { text }] => assert_eq!(text, "updated text"),
            blocks => panic!("unexpected blocks: {blocks:?}"),
        }

        fs::remove_dir_all(worktree).unwrap();
    }

    fn branch_turn_ids_in_created_order(graph: &ThreadGraph) -> Vec<String> {
        let mut nodes = graph
            .nodes
            .values()
            .filter_map(|node| match node {
                ThreadNode::Turn(turn_node) => {
                    Some((turn_node.base.created_order, turn_node.base.id.clone()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        nodes.sort();
        nodes.into_iter().map(|(_, id)| id).collect()
    }

    // A rebind onto a rotated/replayed rollout re-parses the transcript with
    // shifted line indexes, so some replacement ids collide with nodes that held
    // *different* content at other positions. Inheriting those nodes' created
    // orders while unmatched ids allocate fresh (max+1) orders interleaves old
    // and new orders — the branch then renders out of transcript order, with
    // continued turns appearing above the last message. The replace must fall
    // back to reassigning the whole list in order.
    #[test]
    fn replace_reassigns_orders_when_reused_ids_would_break_list_order() {
        let worktree = temp_worktree("qmux-thread-graph-reused-ids");
        let agent = sample_agent(worktree.display().to_string());
        let store = ThreadStore::new(worktree.clone());
        for (id, index) in [
            ("agent-1-3", 3),
            ("agent-1-4", 4),
            ("agent-1-6", 6),
            ("agent-1-7", 7),
        ] {
            store
                .append_turn_node(&agent, &sample_turn("agent-1", id, "user", index))
                .unwrap();
        }

        // The re-parsed file names its newest turn with a previously used id
        // ("agent-1-7") while earlier turns use previously unseen ids.
        let replacement = vec![
            sample_turn("agent-1", "agent-1-1", "user", 1),
            sample_turn("agent-1", "agent-1-2", "assistant", 2),
            sample_turn("agent-1", "agent-1-5", "user", 5),
            sample_turn("agent-1", "agent-1-7", "assistant", 7),
        ];
        store
            .replace_agent_branch_turns(&agent, &replacement)
            .unwrap();

        let graph = read_snapshot(&agent.worktree_dir, "thread-1")
            .unwrap()
            .expect("snapshot exists");
        assert_eq!(
            branch_turn_ids_in_created_order(&graph),
            vec!["agent-1-1", "agent-1-2", "agent-1-5", "agent-1-7"],
        );

        // Turns appended after the reassignment continue at the branch tail.
        store
            .append_turn_node(&agent, &sample_turn("agent-1", "agent-1-9", "user", 9))
            .unwrap();
        let graph = read_snapshot(&agent.worktree_dir, "thread-1")
            .unwrap()
            .expect("snapshot exists");
        assert_eq!(
            branch_turn_ids_in_created_order(&graph),
            vec![
                "agent-1-1",
                "agent-1-2",
                "agent-1-5",
                "agent-1-7",
                "agent-1-9"
            ],
        );

        fs::remove_dir_all(worktree).unwrap();
    }

    // The hot path — a same-content window re-parse (every Claude typed prompt,
    // every codex abort/rollback marker) — must keep created orders untouched so
    // graph identity and downstream memoization survive the refresh.
    #[test]
    fn consistent_replace_keeps_created_orders_without_reallocating() {
        let worktree = temp_worktree("qmux-thread-graph-consistent-replace");
        let agent = sample_agent(worktree.display().to_string());
        let store = ThreadStore::new(worktree.clone());
        let turns = vec![
            sample_turn("agent-1", "agent-1-0", "user", 0),
            sample_turn("agent-1", "agent-1-1", "assistant", 1),
            sample_turn("agent-1", "agent-1-2", "user", 2),
        ];
        for turn in &turns {
            store.append_turn_node(&agent, turn).unwrap();
        }

        store.replace_agent_branch_turns(&agent, &turns).unwrap();

        let graph = read_snapshot(&agent.worktree_dir, "thread-1")
            .unwrap()
            .expect("snapshot exists");
        let orders = ["agent-1-0", "agent-1-1", "agent-1-2"]
            .into_iter()
            .map(|id| graph.nodes.get(id).map(node_created_order).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(orders, vec![0, 1, 2]);
        assert_eq!(graph.next_created_order, 3);

        fs::remove_dir_all(worktree).unwrap();
    }

    // An appended turn is by definition the branch's newest. When its positional
    // id collides with a mid-branch node left from a longer past numbering (a
    // stale tail racing a rebind), inheriting that slot would pin the branch's
    // real tail below it and render every continued message above the last one.
    #[test]
    fn colliding_append_moves_to_the_branch_tail() {
        let worktree = temp_worktree("qmux-thread-graph-colliding-append");
        let agent = sample_agent(worktree.display().to_string());
        let store = ThreadStore::new(worktree.clone());
        for (id, index) in [("agent-1-5", 5), ("agent-1-9", 9), ("agent-1-12", 12)] {
            store
                .append_turn_node(&agent, &sample_turn("agent-1", id, "user", index))
                .unwrap();
        }

        let mut continued = sample_turn("agent-1", "agent-1-9", "assistant", 9);
        continued.blocks = vec![TurnBlock::Text {
            text: "continued".to_string(),
        }];
        store.append_turn_node(&agent, &continued).unwrap();

        let graph = read_snapshot(&agent.worktree_dir, "thread-1")
            .unwrap()
            .expect("snapshot exists");
        assert_eq!(
            branch_turn_ids_in_created_order(&graph),
            vec!["agent-1-5", "agent-1-12", "agent-1-9"],
        );
        let Some(ThreadNode::Turn(node)) = graph.nodes.get("agent-1-9") else {
            panic!("missing re-tailed turn");
        };
        match node.turn.blocks.as_slice() {
            [TurnBlock::Text { text }] => assert_eq!(text, "continued"),
            blocks => panic!("unexpected blocks: {blocks:?}"),
        }
        let branch = graph.branches.get("branch-1").expect("branch exists");
        assert_eq!(branch.head_turn_ids, vec!["agent-1-9"]);

        fs::remove_dir_all(worktree).unwrap();
    }

    // A truncating replace (a rewind whose shrink the tail observed) keeps the
    // retained prefix in place; the continuation that reuses the dropped line
    // indexes appends cleanly at the tail.
    #[test]
    fn append_after_truncating_replace_lands_at_the_branch_tail() {
        let worktree = temp_worktree("qmux-thread-graph-truncating-replace");
        let agent = sample_agent(worktree.display().to_string());
        let store = ThreadStore::new(worktree.clone());
        let full = (0..5)
            .map(|index| {
                sample_turn(
                    "agent-1",
                    &format!("agent-1-{index}"),
                    if index % 2 == 0 { "user" } else { "assistant" },
                    index,
                )
            })
            .collect::<Vec<_>>();
        for turn in &full {
            store.append_turn_node(&agent, turn).unwrap();
        }

        store
            .replace_agent_branch_turns(&agent, &full[..3])
            .unwrap();
        let mut rewound = sample_turn("agent-1", "agent-1-3", "user", 3);
        rewound.blocks = vec![TurnBlock::Text {
            text: "edited message".to_string(),
        }];
        store.append_turn_node(&agent, &rewound).unwrap();

        let graph = read_snapshot(&agent.worktree_dir, "thread-1")
            .unwrap()
            .expect("snapshot exists");
        assert_eq!(
            branch_turn_ids_in_created_order(&graph),
            vec!["agent-1-0", "agent-1-1", "agent-1-2", "agent-1-3"],
        );
        let Some(ThreadNode::Turn(node)) = graph.nodes.get("agent-1-3") else {
            panic!("missing rewound turn");
        };
        match node.turn.blocks.as_slice() {
            [TurnBlock::Text { text }] => assert_eq!(text, "edited message"),
            blocks => panic!("unexpected blocks: {blocks:?}"),
        }

        fs::remove_dir_all(worktree).unwrap();
    }

    #[test]
    fn legacy_record_migrates_without_removing_source_snapshot() {
        let legacy_root = temp_worktree("qmux-thread-graph-legacy");
        let global_root = temp_worktree("qmux-thread-graph-global");
        let agent = sample_agent(legacy_root.display().to_string());
        ThreadStore::new(legacy_root.clone())
            .replace_agent_branch_turns(&agent, &[sample_turn("agent-1", "legacy-turn", "user", 0)])
            .unwrap();
        let mut record = thread_record_for_agent(&agent, "branch-1", &legacy_root);

        assert!(migrate_record_to_storage_root(&mut record, &global_root).unwrap());

        let global_root_string = global_root.display().to_string();
        assert_eq!(record.storage_root, global_root_string);
        assert_eq!(
            record.snapshot_path,
            snapshot_path(&global_root.display().to_string(), "thread-1")
                .display()
                .to_string()
        );
        assert!(
            read_snapshot(&global_root.display().to_string(), "thread-1")
                .unwrap()
                .expect("global snapshot exists")
                .nodes
                .contains_key("legacy-turn")
        );
        assert!(
            read_snapshot(&legacy_root.display().to_string(), "thread-1")
                .unwrap()
                .is_some(),
            "legacy snapshot is retained as a recovery copy"
        );

        fs::remove_dir_all(legacy_root).unwrap();
        fs::remove_dir_all(global_root).unwrap();
    }

    #[test]
    fn existing_global_snapshot_wins_during_migration() {
        let legacy_root = temp_worktree("qmux-thread-graph-stale");
        let global_root = temp_worktree("qmux-thread-graph-current");
        let agent = sample_agent(legacy_root.display().to_string());
        ThreadStore::new(legacy_root.clone())
            .replace_agent_branch_turns(&agent, &[sample_turn("agent-1", "stale-turn", "user", 0)])
            .unwrap();
        ThreadStore::new(global_root.clone())
            .replace_agent_branch_turns(
                &agent,
                &[sample_turn("agent-1", "current-turn", "user", 0)],
            )
            .unwrap();
        let mut record = thread_record_for_agent(&agent, "branch-1", &legacy_root);

        assert!(migrate_record_to_storage_root(&mut record, &global_root).unwrap());

        let graph = read_snapshot(&global_root.display().to_string(), "thread-1")
            .unwrap()
            .expect("global snapshot exists");
        assert!(graph.nodes.contains_key("current-turn"));
        assert!(!graph.nodes.contains_key("stale-turn"));

        fs::remove_dir_all(legacy_root).unwrap();
        fs::remove_dir_all(global_root).unwrap();
    }

    #[test]
    fn scoped_refresh_preserves_graph_nodes_and_other_branches() {
        let worktree = temp_worktree("qmux-thread-graph-shared");
        let source = sample_agent_with(
            "agent-source",
            "claude",
            "thread-shared",
            "branch-source",
            worktree.display().to_string(),
        );
        let target = sample_agent_with(
            "agent-target",
            "codex",
            "thread-shared",
            "branch-target",
            worktree.display().to_string(),
        );
        let store = ThreadStore::new(worktree.clone());

        store
            .append_turn_node(
                &source,
                &sample_turn("agent-source", "source-turn-1", "user", 0),
            )
            .unwrap();
        store
            .append_turn_node(
                &target,
                &sample_turn("agent-target", "target-turn-1", "assistant", 0),
            )
            .unwrap();

        store
            .replace_agent_branch_turns(
                &target,
                &[sample_turn("agent-target", "target-turn-1", "assistant", 0)],
            )
            .unwrap();

        let graph = store
            .read_thread("thread-shared")
            .unwrap()
            .expect("shared graph exists");
        assert!(graph.nodes.contains_key("source-turn-1"));
        assert!(graph.nodes.contains_key("target-turn-1"));
        assert!(graph.branches.contains_key("branch-source"));
        assert!(graph.branches.contains_key("branch-target"));

        fs::remove_dir_all(worktree).unwrap();
    }

    #[test]
    fn created_order_is_monotonic_across_contributors() {
        let worktree = temp_worktree("qmux-thread-graph-order");
        let first = sample_agent_with(
            "agent-first",
            "claude",
            "thread-order",
            "branch-first",
            worktree.display().to_string(),
        );
        let second = sample_agent_with(
            "agent-second",
            "codex",
            "thread-order",
            "branch-second",
            worktree.display().to_string(),
        );
        let store = ThreadStore::new(worktree.clone());
        store
            .append_turn_node(&first, &sample_turn("agent-first", "first-turn", "user", 0))
            .unwrap();
        store
            .append_turn_node(
                &second,
                &sample_turn("agent-second", "second-turn", "assistant", 0),
            )
            .unwrap();
        store
            .append_turn_node(&first, &sample_turn("agent-first", "third-turn", "user", 1))
            .unwrap();

        let graph = store
            .read_thread("thread-order")
            .unwrap()
            .expect("graph exists");
        let orders = ["first-turn", "second-turn", "third-turn"]
            .into_iter()
            .map(|id| graph.nodes.get(id).map(node_created_order).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(orders, vec![0, 1, 2]);
        assert_eq!(graph.next_created_order, 3);

        fs::remove_dir_all(worktree).unwrap();
    }
}
