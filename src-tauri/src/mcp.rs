//! Capability-scoped MCP orchestration operations.
//!
//! The stdio protocol lives in `qmux-cli`; this module is the authority
//! boundary. Every call begins with a pane id resolved from `QMUX_TOKEN`, then
//! derives the caller and permitted lineage from live qmux state.

use crate::adapters::{SpawnAgentRequest, agent_fork, agent_spawn};
use crate::events::QmuxEvent;
use crate::state::AppState;
use crate::turn_queue::{SubmitAgentTurnMode, SubmitAgentTurnRequest, submit_agent_turn};
use crate::workspace::{AgentInfo, AgentStatus};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const MAX_WAIT_SECONDS: u64 = 600;
const MAX_SUMMARY_LINES: usize = 200;
const MAX_CONCURRENT_WAITS: usize = 16;
static ACTIVE_WAITS: AtomicUsize = AtomicUsize::new(0);

pub fn handle_call(
    state: &AppState,
    authed_pane: &str,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    let caller = state
        .agent_by_pane(authed_pane)?
        .ok_or_else(|| "qmux MCP is available only inside an active agent pane".to_string())?;
    let agents = state
        .list_agents()?
        .into_iter()
        .filter(|agent| agent.group_id == caller.group_id)
        .collect::<Vec<_>>();
    let graph = Lineage::new(&agents);

    match name {
        "whoami" => {
            ensure_no_arguments(arguments, "whoami")?;
            whoami(&caller, &graph)
        }
        "spawn_agent" => spawn_child(state, &caller, arguments),
        "fork_self" => fork_self(state, authed_pane, arguments),
        "list_children" => list_children(&caller, &graph, arguments),
        "send_prompt" => send_prompt(state, &caller, &graph, arguments),
        "wait_for_children" => wait_for_children(state, &caller, arguments),
        "summarize_children" => summarize_children(state, &caller, arguments),
        "release_agent" => release_agent(state, &caller, &graph, arguments),
        "get_artifacts" => get_artifacts(state, &caller, &graph, arguments),
        "report_to_parent" => report_to_parent(state, &caller, arguments),
        other => Err(format!("unknown qmux MCP tool '{other}'")),
    }
}

fn whoami(caller: &AgentInfo, graph: &Lineage) -> Result<Value, String> {
    let parent = caller
        .parent_id
        .as_deref()
        .and_then(|id| graph.by_id.get(id))
        .cloned();
    let children = graph.direct_children(&caller.id);
    Ok(json!({
        "agent": caller,
        "parent": parent,
        "children": children,
        "capabilities": {
            "read": "self and live descendants in this workspace",
            "write": "direct parent and direct children only",
            "spawn": "direct children in this workspace only"
        }
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpawnChildArgs {
    #[serde(default)]
    adapter: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    use_worktree: bool,
}

fn spawn_child(state: &AppState, caller: &AgentInfo, arguments: Value) -> Result<Value, String> {
    let args: SpawnChildArgs = parse(arguments, "spawn_agent")?;
    let adapter = args
        .adapter
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| caller.adapter.clone());
    let options = if adapter == caller.adapter {
        match (adapter.as_str(), caller.effort.as_deref()) {
            ("claude", Some(effort)) => json!({ "effort": effort }),
            ("codex", Some(effort)) => json!({ "reasoningEffort": effort }),
            _ => Value::Null,
        }
    } else {
        Value::Null
    };
    let pane = agent_spawn(
        state,
        SpawnAgentRequest {
            adapter_id: adapter.clone(),
            // Establish lineage before delivering user-controlled content.
            prompt: String::new(),
            group_id: Some(caller.group_id.clone()),
            base_repo: Some(caller.worktree_dir.clone()),
            base_ref: Some("HEAD".to_string()),
            cwd: (!args.use_worktree).then(|| caller.worktree_dir.clone()),
            model: (adapter == caller.adapter)
                .then(|| caller.model.clone())
                .flatten(),
            initial_size: None,
            use_worktree: Some(args.use_worktree),
            options,
            parent_id: Some(caller.id.clone()),
            resume_session_id: None,
            fork_session: false,
        },
    )?;
    let child = state
        .agent_by_pane(&pane.id)?
        .ok_or_else(|| format!("spawned pane {} has no agent", pane.id))?;
    if let Some(parent_pane) = caller.pane_id.as_deref()
        && let Err(err) = state.place_pane_after(&pane.id, parent_pane)
    {
        eprintln!(
            "qmux: MCP child {} could not be placed after {parent_pane}: {err}",
            child.id
        );
    }
    state.emit(QmuxEvent::new(
        "agent.spawned",
        Some(pane.id.clone()),
        Some(child.id.clone()),
        json!({ "agent": child, "pane": pane, "source": "mcp", "parentAgentId": caller.id }),
    ));

    let delivery = args
        .prompt
        .map(|prompt| prompt.trim().to_string())
        .filter(|prompt| !prompt.is_empty())
        .map(|prompt| {
            submit_agent_turn(
                state,
                SubmitAgentTurnRequest {
                    agent_id: child.id.clone(),
                    data: prompt,
                    mode: Some(SubmitAgentTurnMode::Auto),
                },
            )
        })
        .transpose()?;
    Ok(json!({ "agent": child, "pane": pane, "delivery": delivery }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ForkArgs {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    use_worktree: bool,
}

fn fork_self(state: &AppState, pane_id: &str, arguments: Value) -> Result<Value, String> {
    let args: ForkArgs = parse(arguments, "fork_self")?;
    let pane = agent_fork(state, pane_id, args.use_worktree, args.prompt, None)?;
    let agent = state.agent_by_pane(&pane.id)?;
    Ok(json!({ "agent": agent, "pane": pane }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListChildrenArgs {
    #[serde(default)]
    recursive: bool,
}

fn list_children(caller: &AgentInfo, graph: &Lineage, arguments: Value) -> Result<Value, String> {
    let args: ListChildrenArgs = parse(arguments, "list_children")?;
    let children = if args.recursive {
        graph.descendants(&caller.id)
    } else {
        graph.direct_children(&caller.id)
    }
    .into_iter()
    .filter(|agent| agent.pane_id.is_some())
    .collect::<Vec<_>>();
    Ok(json!({
        "children": children,
        "count": children.len()
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SendPromptArgs {
    agent_id: String,
    text: String,
}

fn send_prompt(
    state: &AppState,
    caller: &AgentInfo,
    graph: &Lineage,
    arguments: Value,
) -> Result<Value, String> {
    let args: SendPromptArgs = parse(arguments, "send_prompt")?;
    ensure_direct_write(caller, graph, &args.agent_id)?;
    let result = submit_agent_turn(
        state,
        SubmitAgentTurnRequest {
            agent_id: args.agent_id,
            data: args.text,
            mode: Some(SubmitAgentTurnMode::Auto),
        },
    )?;
    serde_json::to_value(result).map_err(|err| format!("failed to encode delivery: {err}"))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WaitArgs {
    #[serde(default)]
    agent_ids: Vec<String>,
    #[serde(default = "default_until")]
    until: String,
    #[serde(default = "default_timeout")]
    timeout_seconds: u64,
}

fn default_until() -> String {
    "settled".to_string()
}

fn default_timeout() -> u64 {
    30
}

fn wait_for_children(
    state: &AppState,
    caller: &AgentInfo,
    arguments: Value,
) -> Result<Value, String> {
    let args: WaitArgs = parse(arguments, "wait_for_children")?;
    if !matches!(args.until.as_str(), "settled" | "done" | "exited") {
        return Err("until must be settled, done, or exited".to_string());
    }
    let ids = selected_direct_child_ids(state, caller, args.agent_ids)?;
    let _wait_slot = WaitSlot::acquire()?;
    let deadline = Instant::now() + Duration::from_secs(args.timeout_seconds.min(MAX_WAIT_SECONDS));
    loop {
        let current = ids
            .iter()
            .map(|id| state.agent(id).map(|agent| (id.clone(), agent)))
            .collect::<Result<Vec<_>, _>>()?;
        let complete = current.iter().all(|(_, agent)| match args.until.as_str() {
            "exited" => agent.as_ref().is_none_or(|agent| agent.pane_id.is_none()),
            "done" => agent.as_ref().is_none_or(|agent| {
                agent.pane_id.is_none()
                    || matches!(
                        agent.status,
                        AgentStatus::Done | AgentStatus::Idle | AgentStatus::Failed
                    )
            }),
            _ => agent.as_ref().is_none_or(|agent| {
                agent.pane_id.is_none()
                    || agent.status.is_at_rest()
                    || agent.status == AgentStatus::Failed
            }),
        });
        if complete || Instant::now() >= deadline {
            let agents = current
                .into_iter()
                .map(|(id, agent)| json!({ "agentId": id, "agent": agent }))
                .collect::<Vec<_>>();
            return Ok(
                json!({ "complete": complete, "timedOut": !complete, "until": args.until, "agents": agents }),
            );
        }
        thread::sleep(Duration::from_millis(150));
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SummarizeArgs {
    #[serde(default)]
    agent_ids: Vec<String>,
    #[serde(default = "default_lines")]
    lines: usize,
}

fn default_lines() -> usize {
    40
}

fn summarize_children(
    state: &AppState,
    caller: &AgentInfo,
    arguments: Value,
) -> Result<Value, String> {
    let args: SummarizeArgs = parse(arguments, "summarize_children")?;
    let ids = selected_direct_child_ids(state, caller, args.agent_ids)?;
    let line_count = args.lines.clamp(1, MAX_SUMMARY_LINES);
    let mut summaries = Vec::with_capacity(ids.len());
    for id in ids {
        let agent = state.agent(&id)?;
        let (output, output_error) = match agent.as_ref().and_then(|agent| agent.pane_id.as_deref())
        {
            Some(pane_id) => {
                match crate::scrollback::read_pane_scrollback(
                    &state.config().workspace_root,
                    pane_id,
                ) {
                    Ok(raw) => (terminal_text_tail(&raw, line_count), None),
                    Err(err) => (String::new(), Some(err)),
                }
            }
            None => (String::new(), None),
        };
        let artifacts = agent
            .as_ref()
            .map(|agent| owned_artifacts(state, agent))
            .transpose()?
            .unwrap_or_default();
        summaries.push(json!({
            "agent": agent,
            "outputTail": output,
            "outputError": output_error,
            "artifacts": artifacts
        }));
    }
    Ok(json!({ "summaries": summaries, "lines": line_count }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReleaseArgs {
    agent_id: String,
}

fn release_agent(
    state: &AppState,
    caller: &AgentInfo,
    graph: &Lineage,
    arguments: Value,
) -> Result<Value, String> {
    let args: ReleaseArgs = parse(arguments, "release_agent")?;
    let target = graph
        .by_id
        .get(&args.agent_id)
        .filter(|target| {
            target.group_id == caller.group_id
                && target.parent_id.as_deref() == Some(caller.id.as_str())
        })
        .cloned();
    let Some(target) = target else {
        return Err(format!("agent {} is not your direct child", args.agent_id));
    };
    let live_descendants = graph
        .descendants(&target.id)
        .into_iter()
        .filter(|agent| agent.pane_id.is_some())
        .map(|agent| agent.id)
        .collect::<Vec<_>>();
    if !live_descendants.is_empty() {
        return Ok(json!({
            "agentId": target.id,
            "released": false,
            "alreadyExited": false,
            "blockedByLiveDescendants": true,
            "liveDescendantAgentIds": live_descendants
        }));
    }
    let Some(pane_id) = target.pane_id.clone() else {
        return Ok(json!({
            "agentId": target.id,
            "released": false,
            "alreadyExited": true,
            "blockedByLiveDescendants": false,
            "liveDescendantAgentIds": live_descendants
        }));
    };
    state.close_pane_for_user(&pane_id)?;
    // Agent-driven cleanup should not replace the user's Cmd-W reopen target.
    state.clear_last_closed_pane_for_pane(&pane_id);
    Ok(json!({
        "agentId": target.id,
        "released": true,
        "alreadyExited": false,
        "blockedByLiveDescendants": false,
        "liveDescendantAgentIds": live_descendants
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ArtifactsArgs {
    #[serde(default)]
    agent_id: Option<String>,
}

fn get_artifacts(
    state: &AppState,
    caller: &AgentInfo,
    graph: &Lineage,
    arguments: Value,
) -> Result<Value, String> {
    let args: ArtifactsArgs = parse(arguments, "get_artifacts")?;
    let target_id = args.agent_id.as_deref().unwrap_or(caller.id.as_str());
    if let Some(target) = graph.by_id.get(target_id).cloned() {
        if target.id != caller.id && !graph.is_descendant(&caller.id, &target.id) {
            return Err("artifacts may be read only for yourself or your descendants".to_string());
        }
        let artifacts = owned_artifacts(state, &target)?;
        return Ok(json!({
            "agent": target,
            "artifacts": artifacts
        }));
    }
    Err("artifacts may be read only for yourself or your live descendants".to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReportArgs {
    #[serde(default = "default_report_status")]
    status: String,
    summary: String,
    #[serde(default)]
    details: Option<String>,
    #[serde(default)]
    blockers: Vec<String>,
    #[serde(default)]
    questions: Vec<String>,
    #[serde(default)]
    next_steps: Vec<String>,
    #[serde(default)]
    changed_paths: Vec<String>,
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default)]
    proof: Vec<String>,
}

fn default_report_status() -> String {
    "update".to_string()
}

fn report_to_parent(
    state: &AppState,
    caller: &AgentInfo,
    arguments: Value,
) -> Result<Value, String> {
    let args: ReportArgs = parse(arguments, "report_to_parent")?;
    if !matches!(
        args.status.as_str(),
        "update" | "done" | "blocked" | "failed"
    ) {
        return Err("status must be update, done, blocked, or failed".to_string());
    }
    let parent_id = caller
        .parent_id
        .as_deref()
        .ok_or_else(|| "this agent has no qmux parent".to_string())?;
    let mut text = format!(
        "[report from agent {} · status: {}]\n\nSummary: {}",
        caller.id,
        args.status,
        args.summary.trim()
    );
    if let Some(details) = args.details.filter(|value| !value.trim().is_empty()) {
        text.push_str("\n\n");
        text.push_str(details.trim());
    }
    append_report_section(&mut text, "Blockers", &args.blockers);
    append_report_section(&mut text, "Questions", &args.questions);
    append_report_section(&mut text, "Next steps", &args.next_steps);
    append_report_section(&mut text, "Changed", &args.changed_paths);
    append_report_section(&mut text, "Artifacts", &args.artifacts);
    append_report_section(&mut text, "Proof", &args.proof);
    let result = submit_agent_turn(
        state,
        SubmitAgentTurnRequest {
            agent_id: parent_id.to_string(),
            data: text,
            mode: Some(SubmitAgentTurnMode::Auto),
        },
    )?;
    serde_json::to_value(result).map_err(|err| format!("failed to encode report delivery: {err}"))
}

fn append_report_section(text: &mut String, title: &str, items: &[String]) {
    let items = items
        .iter()
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .collect::<Vec<_>>();
    if items.is_empty() {
        return;
    }
    text.push_str("\n\n");
    text.push_str(title);
    text.push(':');
    for item in items {
        text.push_str("\n- ");
        text.push_str(item);
    }
}

fn owned_artifacts(
    state: &AppState,
    target: &AgentInfo,
) -> Result<Vec<crate::state::ArtifactInfo>, String> {
    let pane_id = target
        .pane_id
        .as_deref()
        .ok_or_else(|| format!("agent {} has no active pane", target.id))?;
    Ok(state
        .list_artifacts()?
        .into_iter()
        .filter(|entry| entry.pane_id == pane_id)
        .collect())
}

pub(crate) fn terminal_text_tail(raw: &[u8], lines: usize) -> String {
    let replay = crate::scrollback::sanitize_scrollback_replay(raw);
    let plain = strip_terminal_sequences(&replay);
    String::from_utf8_lossy(&plain)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_terminal_sequences(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] != 0x1b {
            let byte = input[index];
            if byte >= 0x20 || matches!(byte, b'\n' | b'\r' | b'\t') {
                output.push(byte);
            }
            index += 1;
            continue;
        }
        index += 1;
        let Some(kind) = input.get(index).copied() else {
            break;
        };
        index += 1;
        match kind {
            b'[' => {
                while let Some(byte) = input.get(index).copied() {
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
            b']' | b'P' | b'X' | b'^' | b'_' => {
                while index < input.len() {
                    if input[index] == 0x07 {
                        index += 1;
                        break;
                    }
                    if input[index] == 0x1b && input.get(index + 1) == Some(&b'\\') {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            _ => {}
        }
    }
    output
}

fn selected_direct_child_ids(
    state: &AppState,
    caller: &AgentInfo,
    requested: Vec<String>,
) -> Result<Vec<String>, String> {
    let agents = state.list_agents()?;
    let mut direct = agents
        .iter()
        .filter(|agent| {
            agent.parent_id.as_deref() == Some(caller.id.as_str())
                && agent.group_id == caller.group_id
                && agent.pane_id.is_some()
        })
        .map(|agent| (agent.created_at, agent.id.clone()))
        .collect::<Vec<_>>();
    direct.sort();
    if requested.is_empty() {
        return Ok(direct.into_iter().map(|(_, id)| id).collect());
    }
    let direct = direct.into_iter().map(|(_, id)| id).collect::<HashSet<_>>();
    for id in &requested {
        if !direct.contains(id) {
            return Err(format!("agent {id} is not your direct child"));
        }
    }
    Ok(requested)
}

fn ensure_direct_write(caller: &AgentInfo, graph: &Lineage, target_id: &str) -> Result<(), String> {
    let target = graph
        .by_id
        .get(target_id)
        .ok_or_else(|| format!("agent {target_id} was not found"))?;
    if target.group_id == caller.group_id
        && (caller.parent_id.as_deref() == Some(target_id)
            || target.parent_id.as_deref() == Some(caller.id.as_str()))
    {
        Ok(())
    } else {
        Err("target writes are limited to your direct parent and direct children".to_string())
    }
}

struct WaitSlot;

impl WaitSlot {
    fn acquire() -> Result<Self, String> {
        ACTIVE_WAITS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_CONCURRENT_WAITS).then_some(active + 1)
            })
            .map_err(|_| {
                "too many agents are waiting concurrently; retry after another wait finishes"
                    .to_string()
            })?;
        Ok(Self)
    }
}

impl Drop for WaitSlot {
    fn drop(&mut self) {
        ACTIVE_WAITS.fetch_sub(1, Ordering::AcqRel);
    }
}

fn parse<T: for<'de> Deserialize<'de>>(value: Value, tool: &str) -> Result<T, String> {
    serde_json::from_value(value).map_err(|err| format!("invalid {tool} arguments: {err}"))
}

fn ensure_no_arguments(value: Value, tool: &str) -> Result<(), String> {
    if value.is_null() || value.as_object().is_some_and(serde_json::Map::is_empty) {
        Ok(())
    } else {
        Err(format!("{tool} does not accept arguments"))
    }
}

struct Lineage {
    by_id: HashMap<String, AgentInfo>,
    children: HashMap<String, Vec<String>>,
}

impl Lineage {
    fn new(agents: &[AgentInfo]) -> Self {
        let by_id: HashMap<String, AgentInfo> = agents
            .iter()
            .map(|agent| (agent.id.clone(), agent.clone()))
            .collect();
        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        for agent in agents {
            if let Some(parent) = agent.parent_id.as_deref() {
                children
                    .entry(parent.to_string())
                    .or_default()
                    .push(agent.id.clone());
            }
        }
        for ids in children.values_mut() {
            ids.sort_by(|left, right| {
                let left = by_id.get(left).map(|agent| (agent.created_at, &agent.id));
                let right = by_id.get(right).map(|agent| (agent.created_at, &agent.id));
                left.cmp(&right)
            });
        }
        Self { by_id, children }
    }

    fn direct_children(&self, id: &str) -> Vec<AgentInfo> {
        self.children
            .get(id)
            .into_iter()
            .flatten()
            .filter_map(|id| self.by_id.get(id).cloned())
            .collect()
    }

    fn descendants(&self, id: &str) -> Vec<AgentInfo> {
        let mut pending = VecDeque::from([id.to_string()]);
        let mut seen = HashSet::from([id.to_string()]);
        let mut result = Vec::new();
        while let Some(parent) = pending.pop_front() {
            for child in self.children.get(&parent).into_iter().flatten() {
                if seen.insert(child.clone()) {
                    if let Some(agent) = self.by_id.get(child) {
                        result.push(agent.clone());
                    }
                    pending.push_back(child.clone());
                }
            }
        }
        result
    }

    fn is_descendant(&self, ancestor: &str, candidate: &str) -> bool {
        self.descendants(ancestor)
            .iter()
            .any(|agent| agent.id == candidate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, parent: Option<&str>) -> AgentInfo {
        AgentInfo {
            id: id.to_string(),
            group_id: "group".to_string(),
            adapter: "claude".to_string(),
            worktree_dir: "/tmp".to_string(),
            branch: None,
            active_workspace: None,
            pane_id: Some(format!("pane-{id}")),
            orphaned_queue_pane_id: None,
            session_id: None,
            transcript_path: None,
            status: AgentStatus::Idle,
            model: None,
            effort: None,
            approval_mode: None,
            parent_id: parent.map(str::to_string),
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
    fn lineage_is_cycle_safe_and_scopes_writes() {
        let root = agent("root", None);
        let child = agent("child", Some("root"));
        let grandchild = agent("grandchild", Some("child"));
        let sibling = agent("sibling", Some("root"));
        let mut foreign = agent("foreign", Some("root"));
        foreign.group_id = "other-group".to_string();
        let graph = Lineage::new(&[root.clone(), child.clone(), grandchild, sibling, foreign]);
        assert_eq!(graph.descendants("root").len(), 4);
        assert!(ensure_direct_write(&root, &graph, "child").is_ok());
        assert!(ensure_direct_write(&child, &graph, "root").is_ok());
        assert!(ensure_direct_write(&child, &graph, "sibling").is_err());
        assert!(ensure_direct_write(&root, &graph, "grandchild").is_err());
        assert!(ensure_direct_write(&root, &graph, "foreign").is_err());
    }

    #[test]
    fn terminal_tails_are_plain_non_blank_and_bounded() {
        let raw = b"first\r\n\x1b[31msecond\x1b[0m\r\n   \r\nthird\r\n";
        assert_eq!(terminal_text_tail(raw, 2), "second\nthird");
    }

    #[test]
    fn structured_report_sections_drop_blank_items() {
        let mut text = "Summary: ready".to_string();
        append_report_section(
            &mut text,
            "Proof",
            &["cargo test".to_string(), "  ".to_string()],
        );
        assert_eq!(text, "Summary: ready\n\nProof:\n- cargo test");
    }

    #[test]
    fn mcp_arguments_reject_unknown_fields() {
        let error = parse::<ListChildrenArgs>(
            json!({ "recursive": true, "recusrive": false }),
            "list_children",
        )
        .err()
        .expect("unknown fields must fail closed");
        assert!(error.contains("unknown field `recusrive`"));
        assert!(ensure_no_arguments(json!({ "unexpected": true }), "whoami").is_err());
    }
}
