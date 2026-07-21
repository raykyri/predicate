use crate::adapters::adapter_registry;
use crate::events::QmuxEvent;
use crate::pty::{InitialPaneSize, respawn_shell_pane};
use crate::scrollback::append_pane_scrollback;
use crate::state::{AppState, PaneInfo, PaneKind, PaneStatus};
use crate::workspace::{
    AgentInfo, AgentStatus, LaunchOrigin, mark_agent_failed, validate_launch_workspace,
};
use serde_json::json;

const DEFAULT_RECENT_SESSION_COLS: u16 = 100;
const DEFAULT_RECENT_SESSION_ROWS: u16 = 24;

/// Recreates recoverable panes from persisted metadata after a restart.
///
/// Panes that were already finished before the previous shutdown
/// (exited/killed/failed) are skipped — they should stay closed, not resurrect.
/// Each remaining pane is respawned in place (same id); a failure is isolated so
/// one bad pane never blocks the rest. Failed agent respawns mark the agent as
/// failed so the UI surfaces a "needs relaunch" state.
pub fn respawn_session(state: &AppState, panes: Vec<PaneInfo>) {
    let mut recovered = 0_usize;
    let mut failed = 0_usize;

    for pane in panes {
        if matches!(
            pane.status,
            PaneStatus::Exited | PaneStatus::Killed | PaneStatus::Failed
        ) {
            continue;
        }

        if let Err(err) =
            validate_launch_workspace(state, Some(&pane.group_id), LaunchOrigin::Recovery)
        {
            failed += 1;
            state.emit(QmuxEvent::new(
                "pane.recovery_failed",
                Some(pane.id.clone()),
                pane.agent_id.clone(),
                json!({ "error": err, "title": pane.title, "kind": pane.kind }),
            ));
            continue;
        }

        // Isolate a panic the same way an `Err` is isolated: a panic in one pane's
        // respawn (e.g. an index/unwrap on malformed persisted metadata) would
        // otherwise unwind `respawn_session` and silently skip every later pane,
        // which is exactly the "one bad pane blocks the rest" failure this loop
        // exists to prevent.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match pane.kind {
            PaneKind::Shell => respawn_shell_pane(state, &pane).map(|_| ()),
            PaneKind::Agent => respawn_agent_pane(state, &pane).map(|_| ()),
        }))
        .unwrap_or_else(|payload| {
            let detail = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            Err(format!("recovery panicked: {detail}"))
        });

        match result {
            Ok(()) => recovered += 1,
            Err(err) => {
                failed += 1;
                if let Some(agent_id) = pane.agent_id.as_deref() {
                    let _ = mark_agent_failed(state, agent_id);
                }
                state.emit(QmuxEvent::new(
                    "pane.recovery_failed",
                    Some(pane.id.clone()),
                    pane.agent_id.clone(),
                    json!({ "error": err, "title": pane.title, "kind": pane.kind }),
                ));
            }
        }
    }

    if recovered > 0 || failed > 0 {
        state.emit(QmuxEvent::new(
            "session.recovered",
            None,
            None,
            json!({ "recovered": recovered, "failed": failed }),
        ));
    }
}

pub fn restore_last_closed_pane(state: &AppState) -> Result<Option<PaneInfo>, String> {
    let Some(mut snapshot) = state.take_last_closed_pane()? else {
        return Ok(None);
    };
    snapshot.pane.id = state.next_id("pane");

    match restore_closed_pane_snapshot(state, &snapshot) {
        Ok(pane) => Ok(Some(pane)),
        Err(err) => {
            let _ = state.remember_last_closed_pane(snapshot);
            Err(err)
        }
    }
}

pub fn resume_recent_session(
    state: &AppState,
    recent_session_id: &str,
    initial_size: Option<InitialPaneSize>,
) -> Result<PaneInfo, String> {
    let session = state
        .recent_session(recent_session_id)?
        .ok_or_else(|| "recent session was not found".to_string())?;

    if let Some(pane_id) = session.pane_id.as_deref()
        && let Some(pane) = state
            .list_panes()?
            .into_iter()
            .find(|pane| pane.id == pane_id)
    {
        return Ok(pane);
    }

    if session.missing {
        return Err("recent session files are no longer available".to_string());
    }

    let group_id = session
        .group_id
        .clone()
        .ok_or_else(|| "recent session is missing its group metadata".to_string())?;
    if state.group(&group_id)?.is_none() {
        return Err("recent session group is no longer available".to_string());
    }

    let adapter_registry = adapter_registry(state.config());
    let adapter = adapter_registry.get(&session.adapter)?;
    let agent = reusable_recent_agent(state, &session.id)?.unwrap_or_else(|| AgentInfo {
        id: state.next_id("agent"),
        group_id,
        adapter: session.adapter.clone(),
        worktree_dir: session.worktree_dir.clone(),
        branch: session.branch.clone(),
        pane_id: None,
        orphaned_queue_pane_id: None,
        session_id: session.session_id.clone(),
        transcript_path: session.transcript_path.clone(),
        status: AgentStatus::Starting,
        model: session.model.clone(),
        effort: session.effort.clone(),
        parent_id: session.parent_id.clone(),
        fork_point: session.fork_point.clone(),
        root_session_id: session.root_session_id.clone(),
        thread_id: None,
        branch_id: None,
        paused: false,
        created_at: session.created_at,
    });
    let agent = AgentInfo {
        pane_id: None,
        orphaned_queue_pane_id: None,
        status: AgentStatus::Starting,
        ..agent
    };
    state.insert_agent(agent.clone())?;

    let size = initial_size.unwrap_or(InitialPaneSize {
        cols: DEFAULT_RECENT_SESSION_COLS,
        rows: DEFAULT_RECENT_SESSION_ROWS,
    });
    let pane = PaneInfo {
        id: state.next_id("pane"),
        title: adapter.display_name().to_string(),
        last_osc_title: None,
        kind: PaneKind::Agent,
        agent_id: Some(agent.id.clone()),
        group_id: agent.group_id.clone(),
        cwd: agent.worktree_dir.clone(),
        cols: size.cols,
        rows: size.rows,
        status: PaneStatus::Starting,
        last_active_at: crate::state::now_millis(),
        recovered: false,
        depth: 0,
    };

    match adapter.resume(state, &pane, &agent) {
        Ok(pane) => Ok(pane),
        Err(err) => {
            let _ = mark_agent_failed(state, &agent.id);
            Err(err)
        }
    }
}

fn reusable_recent_agent(
    state: &AppState,
    recent_session_id: &str,
) -> Result<Option<AgentInfo>, String> {
    Ok(state.list_agents()?.into_iter().find(|agent| {
        agent.pane_id.is_none()
            && recent_session_id
                == crate::state::recent_session_key(
                    &agent.adapter,
                    agent.session_id.as_deref(),
                    agent.transcript_path.as_deref(),
                )
                .as_deref()
                .unwrap_or("")
    }))
}

fn restore_closed_pane_snapshot(
    state: &AppState,
    snapshot: &crate::state::ClosedPaneSnapshot,
) -> Result<PaneInfo, String> {
    state.restore_closed_pane_metadata(snapshot)?;

    match snapshot.pane.kind {
        PaneKind::Shell => {
            respawn_shell_pane(state, &snapshot.pane)?;
        }
        PaneKind::Agent => {
            let agent = snapshot
                .agent
                .as_ref()
                .ok_or_else(|| {
                    format!(
                        "closed agent pane {} is missing its agent",
                        snapshot.pane.id
                    )
                })?
                .agent
                .clone();
            adapter_registry(state.config())
                .get(&agent.adapter)?
                .resume(state, &snapshot.pane, &agent)?;
        }
    }

    if !snapshot.scrollback.is_empty()
        && let Err(err) = append_pane_scrollback(
            &state.config().workspace_root,
            &snapshot.pane.id,
            &snapshot.scrollback,
        )
    {
        eprintln!(
            "qmux: failed to restore scrollback for pane {}: {err}",
            snapshot.pane.id
        );
    }

    state.set_pane_recovered(&snapshot.pane.id, false)?;
    let panes =
        state.place_restored_pane(&snapshot.pane.id, snapshot.index, snapshot.pane.depth)?;
    panes
        .into_iter()
        .find(|pane| pane.id == snapshot.pane.id)
        .ok_or_else(|| format!("restored pane {} was not found", snapshot.pane.id))
}

fn respawn_agent_pane(state: &AppState, pane: &PaneInfo) -> Result<PaneInfo, String> {
    let agent_id = pane
        .agent_id
        .as_deref()
        .ok_or_else(|| "recovered agent pane is missing an agent id".to_string())?;
    let agent = state
        .agent(agent_id)?
        .ok_or_else(|| format!("agent {agent_id} was not found in persisted state"))?;
    adapter_registry(state.config())
        .get(&agent.adapter)?
        .resume(state, pane, &agent)
}
