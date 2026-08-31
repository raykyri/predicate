use crate::adapters::adapter_registry;
use crate::events::QmuxEvent;
use crate::pty::{reattach_remote_pane, respawn_shell_pane};
use crate::scrollback::append_pane_scrollback;
use crate::state::{AppState, PaneInfo, PaneKind, PaneStatus};
use crate::workspace::{LaunchOrigin, mark_agent_failed, validate_launch_workspace};
use serde_json::json;

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
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if pane.remote_session.is_some() {
                reattach_remote_pane(state, &pane)?;
                if matches!(pane.kind, PaneKind::Agent) {
                    rebind_reattached_remote_agent(state, &pane)?;
                }
                return Ok(());
            }
            match pane.kind {
                PaneKind::Shell => respawn_shell_pane(state, &pane).map(|_| ()),
                PaneKind::Agent => respawn_agent_pane(state, &pane).map(|_| ()),
            }
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

fn rebind_reattached_remote_agent(state: &AppState, pane: &PaneInfo) -> Result<(), String> {
    let agent_id = pane
        .agent_id
        .as_deref()
        .ok_or_else(|| format!("remote agent pane {} has no agent id", pane.id))?;
    let agent = state
        .mutate_agent(agent_id, |agent| {
            agent.pane_id = Some(pane.id.clone());
        })?
        .ok_or_else(|| format!("agent {agent_id} was not found during remote recovery"))?;
    // This is an attach to the still-running process, not adapter.resume(): do
    // not force Idle or launch a duplicate agent. Fresh lifecycle hooks resume
    // status tracking over the forwarded socket. Remote transcript paths are
    // intentionally not tailed through the local filesystem.
    state.emit(QmuxEvent::new(
        "agent.recovered",
        Some(pane.id.clone()),
        Some(agent.id.clone()),
        json!({ "resumed": true, "remoteAttached": true, "agent": agent }),
    ));
    Ok(())
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
    let panes = state.place_restored_pane(&snapshot.pane.id, snapshot.index)?;
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
