use crate::adapters::{
    FORK_UNSUPPORTED_ERROR, adapter_supports_fork, agent_composer_policy, fork_agent_source,
    spawn_sibling_agent_session,
};
use crate::events::QmuxEvent;
use crate::pty::{PaneWriteOptions, write_pane, write_pane_detailed};
use crate::state::{
    AgentSendSource, AgentTurnClaim, AppState, GlobalDraft, IdleAdvance, QueuedTurn,
    QueuedTurnDelivery, SubmitWatchStatus,
};
use crate::workspace::{AgentInfo, AgentStatus};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SubmitAgentTurnMode {
    Auto,
    Send,
    Queue,
    /// Force the turn through to the agent right now, even while it is busy, so
    /// the user can steer a running agent instead of waiting for it to go idle.
    Steer,
}

/// qMux sends leading-`!` text through the agent TUI, but the TUI handles it as a
/// shell escape rather than an agent turn. Those commands may not emit a normal
/// Stop/idle hook after they finish, so they must not enter the running lifecycle.
pub(crate) fn is_shell_escape_turn(text: &str) -> bool {
    text.trim_start().starts_with('!')
}

/// Turns the agent TUI intercepts as commands rather than plain prompts: `!` shell
/// escapes and `/` slash commands. Built-in slash commands (e.g. Claude's `/model`)
/// fire no hooks at all — no UserPromptSubmit and no Stop/idle — so a send must not
/// optimistically mark the agent Running on their behalf; nothing would ever mark it
/// idle again. Slash commands that do start a real turn (skills) still promote the
/// agent to Running through their own UserPromptSubmit/PreToolUse hooks moments
/// later.
pub(crate) fn is_tui_command_turn(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with('!') || trimmed.starts_with('/')
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitAgentTurnRequest {
    pub agent_id: String,
    pub data: String,
    pub mode: Option<SubmitAgentTurnMode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueWaitAgentTurnRequest {
    pub agent_id: String,
    pub data: String,
    pub wait_for_agent_id: String,
    #[serde(default)]
    pub wait_for_pane_id: Option<String>,
    #[serde(default)]
    pub wait_for_label: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueDeliveryAgentTurnRequest {
    pub agent_id: String,
    pub data: String,
    pub delivery: QueuedTurnDelivery,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitAgentTurnResult {
    pub queued: bool,
    pub pending_turns: usize,
    pub queued_turns: Vec<QueuedTurn>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveQueuedAgentTurnRequest {
    pub agent_id: String,
    pub index: usize,
    pub expected_data: Option<String>,
    #[serde(default)]
    pub expected_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveQueuedAgentTurnResult {
    pub removed_turn: String,
    pub pending_turns: usize,
    pub queued_turns: Vec<QueuedTurn>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReorderQueuedAgentTurnRequest {
    pub agent_id: String,
    pub from_index: usize,
    pub to_index: usize,
    pub expected_data: Option<String>,
    #[serde(default)]
    pub expected_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReorderQueuedAgentTurnResult {
    pub pending_turns: usize,
    pub queued_turns: Vec<QueuedTurn>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendNextQueuedAgentTurnResult {
    pub sent: bool,
    pub pending_turns: usize,
    pub queued_turns: Vec<QueuedTurn>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveQueuedAgentTurnRequest {
    pub from_agent_id: String,
    pub to_agent_id: String,
    pub index: usize,
    pub expected_data: Option<String>,
    #[serde(default)]
    pub expected_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveQueuedAgentTurnResult {
    /// Whether the moved turn was sent to the target immediately (vs. queued).
    pub sent: bool,
    pub source_queued_turns: Vec<QueuedTurn>,
    pub target_queued_turns: Vec<QueuedTurn>,
}

/// Atomically moves a queued turn from one agent to another. The turn is removed
/// from the source first, then handed to the target with normal send-or-queue
/// behavior; if the handoff fails it is rolled back to its original position in the
/// source queue. Because the removal and the add happen in one backend call, a
/// failure can never leave the turn in both queues (the duplication the previous
/// two-call frontend dance risked) nor silently lose it.
pub fn move_queued_agent_turn(
    state: &AppState,
    request: MoveQueuedAgentTurnRequest,
) -> Result<MoveQueuedAgentTurnResult, String> {
    if request.from_agent_id == request.to_agent_id {
        return Err("Cannot move a queued turn onto the same agent".to_string());
    }

    let source = state
        .agent(&request.from_agent_id)?
        .ok_or_else(|| format!("Agent {} was not found", request.from_agent_id))?;
    let source_id = source.id.clone();
    let source_pane_id = source.pane_id.clone();

    let (removed_turn, source_queued_turns) = state.remove_agent_turn_queue_item(
        &request.from_agent_id,
        request.index,
        request.expected_data.as_deref(),
        request.expected_id.as_deref(),
    )?;
    state.emit(QmuxEvent::new(
        "agent.queued_turn_removed",
        source_pane_id.clone(),
        Some(source_id.clone()),
        json!({
            "pendingTurns": source_queued_turns.len(),
            "queuedTurns": source_queued_turns.clone(),
        }),
    ));

    let submit = submit_agent_turn(
        state,
        SubmitAgentTurnRequest {
            agent_id: request.to_agent_id.clone(),
            // Moving to another agent intentionally resets queue directives
            // (pause-after, wait targets, and fork/new-session delivery are
            // contextual to the source queue), so hand over just the text.
            data: removed_turn.text.clone(),
            mode: Some(SubmitAgentTurnMode::Auto),
        },
    );
    let target_result = match submit {
        Ok(result) => result,
        Err(err) => {
            // Roll the turn back so the move can't lose it (preserving queue
            // directives). Re-emit the source queue so the UI reflects the restored
            // item.
            let pending =
                state.insert_agent_turn_at(&request.from_agent_id, request.index, removed_turn)?;
            let restored = state.agent_queued_turns(&request.from_agent_id)?;
            state.emit(QmuxEvent::new(
                "agent.turn_queued",
                source_pane_id,
                Some(source_id),
                json!({ "pendingTurns": pending, "queuedTurns": restored }),
            ));
            return Err(err);
        }
    };
    release_waiters_for_agent(state, &request.from_agent_id)?;

    Ok(MoveQueuedAgentTurnResult {
        sent: !target_result.queued,
        source_queued_turns,
        target_queued_turns: target_result.queued_turns,
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssignGlobalDraftRequest {
    pub draft_id: String,
    pub agent_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssignGlobalDraftResult {
    /// Whether the draft's text was sent to the agent immediately (vs. queued).
    pub sent: bool,
    pub drafts: Vec<GlobalDraft>,
    pub queued_turns: Vec<QueuedTurn>,
}

/// Hands a global draft to an agent in one atomic backend call, the same shape
/// as `move_queued_agent_turn`: the draft is claimed (marked consumed) first so
/// two concurrent assigns can't both deliver it, then the text goes through the
/// normal send-or-queue path; a failed submit unclaims the draft so the text is
/// never lost.
pub fn assign_global_draft(
    state: &AppState,
    request: AssignGlobalDraftRequest,
) -> Result<AssignGlobalDraftResult, String> {
    let draft = state.claim_global_draft(&request.draft_id, &request.agent_id)?;
    let submit = submit_agent_turn(
        state,
        SubmitAgentTurnRequest {
            agent_id: request.agent_id.clone(),
            data: draft.text.clone(),
            mode: Some(SubmitAgentTurnMode::Auto),
        },
    );
    let result = match submit {
        Ok(result) => result,
        Err(err) => {
            state.unclaim_global_draft(&request.draft_id)?;
            return Err(err);
        }
    };
    Ok(AssignGlobalDraftResult {
        sent: !result.queued,
        drafts: state.global_drafts()?,
        queued_turns: result.queued_turns,
    })
}

pub fn submit_agent_turn(
    state: &AppState,
    request: SubmitAgentTurnRequest,
) -> Result<SubmitAgentTurnResult, String> {
    let data = request.data.trim().to_string();
    if data.is_empty() {
        return Err("Turn text cannot be empty".to_string());
    }

    let agent = state
        .agent(&request.agent_id)?
        .ok_or_else(|| format!("Agent {} was not found", request.agent_id))?;

    if matches!(agent.status, AgentStatus::Failed) {
        return Err(format!("Agent {} has failed", agent.id));
    }
    let policy = agent_composer_policy(state, &agent)?;
    let has_pending_queue = !state.agent_queued_turns(&agent.id)?.is_empty();

    match request.mode.unwrap_or(SubmitAgentTurnMode::Auto) {
        SubmitAgentTurnMode::Auto => {
            // A paused agent holds its queue, so a fresh submit must queue rather than
            // send straight through — otherwise the pause is silently bypassed (and any
            // already-queued turns are jumped). It drains when the user unpauses.
            if agent.paused || policy.should_queue(agent.status) || has_pending_queue {
                let turn = QueuedTurn::new(data);
                let result = queue_agent_turn(state, &agent, turn.clone())?;
                // Rescue only a *newly* stranded turn. When the queue was already
                // non-empty we preserve strict append order and let the normal drain
                // triggers (idle hook, typing-clear, unpause) send it. But when the
                // queue was empty, this turn was queued solely on `agent.status`, which
                // was read without the model lock and separately from `has_pending_queue`
                // — so the agent may have gone idle in between (its Stop hook marking it
                // Done with the queue momentarily empty), leaving this lone turn with
                // nothing to drain it. Attempt the same guarded drain the other enqueue
                // sites use; it is a no-op unless the agent is genuinely idle-and-ready.
                if has_pending_queue {
                    return Ok(result);
                }
                return drain_after_enqueue(state, &request.agent_id, result, &turn);
            }
            if !policy.can_send(agent.status) {
                return Err(format!(
                    "Agent {} is not accepting turns in its current state",
                    agent.id
                ));
            }
            send_direct_or_queue(state, &agent, data)
        }
        SubmitAgentTurnMode::Send => {
            // Honor the pause even on an explicit send: queue the turn instead of writing
            // it straight through, so a paused agent never receives an out-of-band turn
            // ahead of its held queue. Unpausing drains it in order.
            if agent.paused {
                return queue_agent_turn(state, &agent, QueuedTurn::new(data));
            }
            if !policy.can_send(agent.status) {
                return Err("Agent is not ready for input; queue the turn instead".to_string());
            }
            send_direct_or_queue(state, &agent, data)
        }
        SubmitAgentTurnMode::Queue => {
            // A paused agent may be idle with an empty queue; still allow queueing (the
            // turn is held behind the pause) instead of rejecting it as "ready to send".
            if !agent.paused && !policy.should_queue(agent.status) && !has_pending_queue {
                return Err("Agent is ready for input; send the turn instead".to_string());
            }
            queue_agent_turn(state, &agent, QueuedTurn::new(data))
        }
        SubmitAgentTurnMode::Steer => {
            if !policy.can_steer(agent.status) {
                return Err("Agent does not support steering in its current state".to_string());
            }
            send_agent_turn(
                state,
                &agent,
                &QueuedTurn::new(data),
                AgentSendSource::Steer,
            )
            .map_err(|err| err.message)?;
            let queued_turns = state.agent_queued_turns(&agent.id)?;
            Ok(SubmitAgentTurnResult {
                queued: false,
                pending_turns: queued_turns.len(),
                queued_turns,
            })
        }
    }
}

pub fn queue_wait_agent_turn(
    state: &AppState,
    request: QueueWaitAgentTurnRequest,
) -> Result<SubmitAgentTurnResult, String> {
    let data = request.data.trim().to_string();
    if data.is_empty() {
        return Err("Turn text cannot be empty".to_string());
    }

    let agent = state
        .agent(&request.agent_id)?
        .ok_or_else(|| format!("Agent {} was not found", request.agent_id))?;
    if matches!(agent.status, AgentStatus::Failed) {
        return Err(format!("Agent {} has failed", agent.id));
    }
    // Same invariant as queue_agent_turn below: a research run never drains a
    // queue, so a wait-for turn accepted here would sit forever and its
    // non-empty queue would park the agent as an orphaned-queue zombie
    // instead of letting retirement reclaim it.
    if state.agent_is_research_run(&agent.id)? {
        return Err("research runs are read-only; create a follow-up branch instead".to_string());
    }

    let pending_turns = state.enqueue_agent_wait_turn_with_target_label(
        &agent.id,
        data,
        &request.wait_for_agent_id,
        request.wait_for_pane_id.as_deref(),
        request.wait_for_label.as_deref(),
    )?;
    let queued_turns = state.agent_queued_turns(&agent.id)?;
    state.emit(QmuxEvent::new(
        "agent.turn_queued",
        agent.pane_id.clone(),
        Some(agent.id.clone()),
        json!({ "pendingTurns": pending_turns, "queuedTurns": queued_turns }),
    ));
    let enqueued_turn = queued_turns.last().cloned();

    let agent = state.agent(&agent.id)?.unwrap_or(agent);
    let policy = agent_composer_policy(state, &agent)?;
    let _source_ran = !agent.paused
        && !state.agent_is_typing(&agent.id)?
        && policy.can_send(agent.status)
        && drain_agent_turn_queue(state, &agent.id)?;
    let queued_turns = state.agent_queued_turns(&agent.id)?;
    let queued = enqueued_turn
        .as_ref()
        .is_some_and(|enqueued| queued_turns.iter().any(|turn| turn == enqueued));
    Ok(SubmitAgentTurnResult {
        queued,
        pending_turns: queued_turns.len(),
        queued_turns,
    })
}

/// Queues a turn that, when reached, is delivered to a new pane instead of this
/// agent's own composer: a fork of the session (optionally in a fresh worktree) or
/// a brand-new session of the same adapter in the same directory. Fork preconditions
/// that can already be checked (adapter support) fail fast here; ones that can only
/// be known at dispatch time (a recorded session id) surface as `agent.queue_error`.
pub fn queue_delivery_agent_turn(
    state: &AppState,
    request: QueueDeliveryAgentTurnRequest,
) -> Result<SubmitAgentTurnResult, String> {
    let data = request.data.trim().to_string();
    if data.is_empty() {
        return Err("Turn text cannot be empty".to_string());
    }

    let agent = state
        .agent(&request.agent_id)?
        .ok_or_else(|| format!("Agent {} was not found", request.agent_id))?;
    if matches!(agent.status, AgentStatus::Failed) {
        return Err(format!("Agent {} has failed", agent.id));
    }
    if matches!(request.delivery, QueuedTurnDelivery::Fork { .. })
        && !adapter_supports_fork(&agent.adapter)
    {
        return Err(FORK_UNSUPPORTED_ERROR.to_string());
    }

    let turn = QueuedTurn::delivering(data, request.delivery);
    let queued_result = queue_agent_turn(state, &agent, turn.clone())?;
    // If the agent is already idle, dispatch now. A dispatch failure (e.g. the
    // session has no recorded id to fork yet) re-queues the turn at the front, so
    // report it as a queue error rather than failing the enqueue the user already
    // sees in the queue list.
    match drain_after_enqueue(state, &agent.id, queued_result, &turn) {
        Ok(result) => Ok(result),
        Err(err) => {
            let queued_turns = state.agent_queued_turns(&agent.id)?;
            state.emit(QmuxEvent::new(
                "agent.queue_error",
                agent.pane_id.clone(),
                Some(agent.id.clone()),
                json!({ "error": err, "queuedTurns": queued_turns.clone() }),
            ));
            Ok(SubmitAgentTurnResult {
                queued: true,
                pending_turns: queued_turns.len(),
                queued_turns,
            })
        }
    }
}

pub fn remove_queued_agent_turn(
    state: &AppState,
    request: RemoveQueuedAgentTurnRequest,
) -> Result<RemoveQueuedAgentTurnResult, String> {
    let agent = state
        .agent(&request.agent_id)?
        .ok_or_else(|| format!("Agent {} was not found", request.agent_id))?;
    let agent_id = agent.id.clone();
    let (removed_turn, queued_turns) = state.remove_agent_turn_queue_item(
        &agent_id,
        request.index,
        request.expected_data.as_deref(),
        request.expected_id.as_deref(),
    )?;
    let pending_turns = queued_turns.len();
    state.emit(QmuxEvent::new(
        "agent.queued_turn_removed",
        agent.pane_id.clone(),
        Some(agent_id.clone()),
        json!({ "pendingTurns": pending_turns, "queuedTurns": queued_turns.clone() }),
    ));
    release_waiters_for_agent(state, &agent_id)?;
    Ok(RemoveQueuedAgentTurnResult {
        removed_turn: removed_turn.text,
        pending_turns,
        queued_turns,
    })
}

pub fn reorder_queued_agent_turn(
    state: &AppState,
    request: ReorderQueuedAgentTurnRequest,
) -> Result<ReorderQueuedAgentTurnResult, String> {
    let agent = state
        .agent(&request.agent_id)?
        .ok_or_else(|| format!("Agent {} was not found", request.agent_id))?;
    let agent_id = agent.id.clone();
    let pane_id = agent.pane_id.clone();
    let mut queued_turns = state.reorder_agent_turn_queue_item(
        &agent.id,
        request.from_index,
        request.to_index,
        request.expected_data.as_deref(),
        request.expected_id.as_deref(),
    )?;
    state.emit(QmuxEvent::new(
        "agent.queued_turn_reordered",
        pane_id.clone(),
        Some(agent_id.clone()),
        json!({ "pendingTurns": queued_turns.len(), "queuedTurns": queued_turns.clone() }),
    ));

    if !agent.paused
        && !state.agent_is_typing(&agent_id)?
        && agent_composer_policy(state, &agent)?.can_send(agent.status)
    {
        match drain_agent_turn_queue(state, &agent_id) {
            Ok(true) => {
                queued_turns = state.agent_queued_turns(&agent_id)?;
                if let Some(updated) = state.agent(&agent_id)? {
                    state.emit(QmuxEvent::new(
                        "agent.running",
                        updated.pane_id.clone(),
                        Some(updated.id.clone()),
                        json!({ "agent": updated }),
                    ));
                }
            }
            Ok(false) => {}
            Err(err) => {
                queued_turns = state.agent_queued_turns(&agent_id)?;
                state.emit(QmuxEvent::new(
                    "agent.queue_error",
                    pane_id,
                    Some(agent_id.clone()),
                    json!({ "error": err, "queuedTurns": queued_turns.clone() }),
                ));
            }
        }
    }

    let pending_turns = queued_turns.len();
    Ok(ReorderQueuedAgentTurnResult {
        pending_turns,
        queued_turns,
    })
}

/// Drains the agent's queue: dispatches ready turns until one runs on the agent
/// itself, the queue empties/blocks, or a pause lands. Returns whether the agent is
/// now running a turn of its own — delivery turns dispatched along the way go to
/// new panes and leave it idle, so they don't count, keeping callers'
/// `agent.running` emissions and `sent` flags truthful.
pub fn drain_agent_turn_queue(state: &AppState, agent_id: &str) -> Result<bool, String> {
    loop {
        // Claim under the model lock so two concurrent triggers can't each pop a ready
        // turn and double-send (the agent isn't marked Running until the send below).
        match state.claim_ready_agent_turn(agent_id)? {
            AgentTurnClaim::Ready { turn, pending } => {
                match send_claimed_turn(state, agent_id, turn, pending)? {
                    DispatchOutcome::Ran => return Ok(true),
                    DispatchOutcome::StayedIdle => {
                        // The turn went to a new pane, not into this agent — it is
                        // still idle, so keep draining (a pause-after delivery turn
                        // pauses it instead; see send_claimed_turn).
                        if state.agent_is_paused(agent_id)? {
                            return Ok(false);
                        }
                    }
                }
            }
            AgentTurnClaim::Draining | AgentTurnClaim::Idle => return Ok(false),
        }
    }
}

/// How a claimed turn was dispatched — the single source of truth for whether the
/// owning agent is now busy, derived from the turn's delivery directive in exactly
/// one place (`send_claimed_turn`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DispatchOutcome {
    /// The turn was pasted into the agent's own pane; the agent is running it.
    Ran,
    /// The turn was delivered to a new pane (fork / new session); the agent
    /// stays idle.
    StayedIdle,
}

/// Sends a turn already claimed via [`AppState::claim_ready_agent_turn`] /
/// [`AppState::claim_next_turn_or_settle`], then clears the draining guard. On any
/// failure the turn is requeued and the guard cleared before returning the error, so a
/// failed send neither loses the turn nor wedges the queue (a still-set guard would
/// block every future drain). A turn carrying a `delivery` directive goes to a new
/// pane (fork / new session) instead of this agent's PTY and leaves its status
/// untouched — the caller is expected to keep draining.
fn send_claimed_turn(
    state: &AppState,
    agent_id: &str,
    turn: QueuedTurn,
    pending_turns: usize,
) -> Result<DispatchOutcome, String> {
    let agent = match state.agent(agent_id)? {
        Some(agent) => agent,
        None => {
            state.requeue_inflight_after_failed_drain(agent_id, turn);
            state.finish_agent_drain(agent_id);
            return Err(format!("Agent {agent_id} was not found"));
        }
    };
    let send_result = match turn.delivery.as_ref() {
        Some(delivery) => {
            // Delivery turns dispatch at-most-once across a crash: drop the durable
            // in-flight copy before spawning, so a crash mid-spawn loses the turn
            // instead of re-running it on restart and minting a duplicate
            // pane/agent/worktree. (Plain turns keep at-least-once semantics —
            // re-pasting text is benign; re-forking is not.)
            state.clear_agent_inflight(agent_id);
            deliver_queued_turn_to_new_pane(state, &agent, &turn.text, delivery)
                .map(|_| DispatchOutcome::StayedIdle)
                .map_err(|message| TurnSendError {
                    message,
                    text_delivered: false,
                })
        }
        None => send_agent_turn(state, &agent, &turn, AgentSendSource::QueuedTurn)
            .map(|_| DispatchOutcome::Ran),
    };
    let outcome = match send_result {
        Ok(outcome) => outcome,
        Err(err) => {
            // When the turn's text already reached the pane (the paste landed and
            // only the submit leg failed), tag the requeued turn so its retry
            // submits a bare Return instead of pasting a second copy that would
            // concatenate onto the one sitting in the composer.
            let mut turn = turn;
            turn.possibly_pasted = turn.possibly_pasted || err.text_delivered;
            state.requeue_inflight_after_failed_drain(agent_id, turn);
            state.finish_agent_drain(agent_id);
            return Err(err.message);
        }
    };
    // Delivered: clear the in-flight record so it isn't re-queued on the next restart
    // (a no-op for delivery turns, which cleared it before dispatch).
    state.clear_agent_inflight(agent_id);
    if turn.pause_after {
        if turn.delivery.is_some() {
            // A delivery turn never runs on this agent, so its "pause after" takes
            // effect now rather than arming a pending pause for the next idle.
            state.set_agent_paused(agent_id, true)?;
        } else {
            // A pause-after turn arms the pause; it takes effect when this turn
            // finishes (see `advance_after_idle`), not now.
            state.mark_agent_pending_pause(agent_id)?;
        }
    }
    state.finish_agent_drain(agent_id);
    let queued_turns = state.agent_queued_turns(agent_id)?;
    state.emit(QmuxEvent::new(
        "agent.queued_turn_sent",
        agent.pane_id.clone(),
        Some(agent.id),
        json!({ "pendingTurns": pending_turns, "queuedTurns": queued_turns }),
    ));
    Ok(outcome)
}

/// What an agent going idle should do next.
#[derive(Debug)]
pub enum IdleResolution {
    /// A queued turn was sent; the agent is running again.
    Drained,
    /// The just-finished turn requested a pause; the agent is now paused.
    Paused,
    /// Nothing to send (empty queue or already paused); the agent is idle.
    Idle,
}

/// Decides what happens when an agent goes idle: enter paused mode if the turn that
/// just finished requested it, stay idle while paused, otherwise drain the next
/// queued turn. Writes status/paused with field-scoped setters so a concurrent hook
/// update can't clobber them. Shared by the Claude and Codex idle handlers.
pub fn advance_after_idle(state: &AppState, agent_id: &str) -> Result<IdleResolution, String> {
    advance_after_settlement(state, agent_id, AgentStatus::Done, true)
}

/// Handles an interrupted turn without presenting it as completed work. Queue draining
/// remains identical to a normal idle boundary, but an agent with nothing to send waits
/// for input and does not release dependents that asked to wait for actual completion.
pub fn advance_after_interruption(
    state: &AppState,
    agent_id: &str,
) -> Result<IdleResolution, String> {
    advance_after_settlement(state, agent_id, AgentStatus::AwaitingInput, false)
}

fn advance_after_settlement(
    state: &AppState,
    agent_id: &str,
    settled_status: AgentStatus,
    release_waiters: bool,
) -> Result<IdleResolution, String> {
    // A different completion signal (for example a transcript abort marker racing a
    // Stop hook) may already have drained a queued turn. Until that send receives its
    // prompt-submit echo, another idle signal belongs to the previous turn and must
    // not consume the next queue entry. Report `Drained` because the agent is already
    // running the newly sent turn. Expired advisory records are pruned by the query,
    // so a genuinely missing echo cannot block the queue indefinitely.
    if state.agent_has_outstanding_send_source(agent_id, AgentSendSource::QueuedTurn)? {
        return Ok(IdleResolution::Drained);
    }
    // Outstanding-send tracking is advisory; clear it best-effort on every idle.
    let _ = state.clear_agent_outstanding_sends(agent_id);

    // Looped so a delivery turn — which goes to a new pane and leaves this agent
    // idle — falls through to the next queued turn (or settles the agent to Done)
    // in the same idle event, instead of stranding the rest of the queue in a
    // status that never fires another idle hook. Each iteration re-checks the
    // pause state, so a pause-after delivery turn still halts the queue.
    loop {
        // The pause branches settle with the caller's status and waiter policy
        // too: an *interrupted* turn that lands on a pausing agent was still
        // aborted, so presenting it as Done — and releasing dependents that
        // asked to wait for actual completion — would defeat the interruption
        // settlement path.
        if state.take_agent_pending_pause(agent_id)? {
            state.set_agent_paused(agent_id, true)?;
            state.set_agent_status(agent_id, settled_status)?;
            if release_waiters {
                release_waiters_for_agent(state, agent_id)?;
            }
            return Ok(IdleResolution::Paused);
        }
        if state.agent_is_paused(agent_id)? {
            // Paused: leave the queue intact and don't auto-send.
            state.set_agent_status(agent_id, settled_status)?;
            if release_waiters {
                release_waiters_for_agent(state, agent_id)?;
            }
            return Ok(IdleResolution::Idle);
        }
        // Atomically claim a ready turn, observe that another drain owns the agent, or
        // settle to the requested ready status — all under one model lock. This both
        // serializes draining (so a racing trigger can't double-send) and folds the typing
        // check into the same lock as the status write, closing the typing/idle lost-wakeup.
        // The user-is-typing case is handled inside as an idle settle (the queue is held;
        // it resumes when the frontend clears the typing flag).
        match state.claim_next_turn_or_settle(agent_id, settled_status)? {
            IdleAdvance::Sent { turn, pending } => {
                let is_delivery = turn.delivery.is_some();
                match send_claimed_turn(state, agent_id, turn, pending) {
                    Ok(DispatchOutcome::Ran) => return Ok(IdleResolution::Drained),
                    Ok(DispatchOutcome::StayedIdle) => continue,
                    Err(err) => {
                        if is_delivery {
                            // A failed delivery leaves this agent genuinely idle;
                            // settle it and release its waiters so the error
                            // (surfaced as a queue error by the caller) doesn't
                            // strand the tab in a stale Running status that no
                            // future idle hook will ever clear.
                            state.set_agent_status(agent_id, settled_status)?;
                            if release_waiters {
                                release_waiters_for_agent(state, agent_id)?;
                            }
                        }
                        return Err(err);
                    }
                }
            }
            IdleAdvance::Busy => {
                // Another drain is mid-send and owns the status transition; leave it be.
                return Ok(IdleResolution::Idle);
            }
            IdleAdvance::Idle => {
                if release_waiters {
                    release_waiters_for_agent(state, agent_id)?;
                }
                return Ok(IdleResolution::Idle);
            }
        }
    }
}

pub fn release_waiters_for_agent(state: &AppState, target_agent_id: &str) -> Result<usize, String> {
    let waiting_agent_ids = state.agents_with_front_wait_for(target_agent_id)?;
    let mut drained_count = 0;

    for source_agent_id in waiting_agent_ids {
        if source_agent_id == target_agent_id {
            continue;
        }
        let Some(source) = state.agent(&source_agent_id)? else {
            continue;
        };
        if source.paused || state.agent_is_typing(&source.id)? {
            continue;
        }
        let policy = agent_composer_policy(state, &source)?;
        if !policy.can_send(source.status) {
            continue;
        }
        match drain_agent_turn_queue(state, &source.id) {
            Ok(true) => {
                drained_count += 1;
                if let Some(updated) = state.agent(&source.id)? {
                    state.emit(QmuxEvent::new(
                        "agent.running",
                        updated.pane_id.clone(),
                        Some(updated.id.clone()),
                        json!({ "agent": updated }),
                    ));
                }
            }
            Ok(false) => {}
            Err(err) => {
                state.emit(QmuxEvent::new(
                    "agent.queue_error",
                    source.pane_id.clone(),
                    Some(source.id.clone()),
                    json!({ "error": err }),
                ));
            }
        }
    }

    Ok(drained_count)
}

/// Clears an agent's paused state. If the agent is in a ready (idle) state, the next
/// queued turn is sent immediately; otherwise normal draining resumes once its
/// current work finishes. Emits so the UI reflects the cleared pause and any send.
pub fn unpause_agent(
    state: &AppState,
    agent_id: &str,
) -> Result<SendNextQueuedAgentTurnResult, String> {
    let agent = state
        .agent(agent_id)?
        .ok_or_else(|| format!("Agent {agent_id} was not found"))?;
    let agent = state.set_agent_paused(agent_id, false)?.unwrap_or(agent);

    let policy = agent_composer_policy(state, &agent)?;
    let sent = if policy.can_send(agent.status) {
        drain_agent_turn_queue(state, agent_id)?
    } else {
        false
    };

    let queued_turns = state.agent_queued_turns(agent_id)?;
    state.emit(QmuxEvent::new(
        "agent.unpaused",
        agent.pane_id.clone(),
        Some(agent.id.clone()),
        json!({
            "agent": state.agent(agent_id)?,
            "sent": sent,
            "pendingTurns": queued_turns.len(),
            "queuedTurns": queued_turns.clone(),
        }),
    ));
    Ok(SendNextQueuedAgentTurnResult {
        sent,
        pending_turns: queued_turns.len(),
        queued_turns,
    })
}

/// Marks/clears whether the user is actively typing for an agent. While typing, the
/// idle handler holds the queue. On clear, if the agent is idle (and not paused), the
/// held turn is drained now; otherwise it resumes on the next idle. The frontend sets
/// this on keystrokes and clears it 1500ms after typing stops.
pub fn set_agent_typing(
    state: &AppState,
    agent_id: &str,
    typing: bool,
) -> Result<SendNextQueuedAgentTurnResult, String> {
    state.set_agent_typing(agent_id, typing)?;

    let mut sent = false;
    if !typing {
        // Typing stopped: drain a held turn if the agent is idle and not paused.
        if let Some(agent) = state.agent(agent_id)?
            && !agent.paused
            && agent_composer_policy(state, &agent)?.can_send(agent.status)
        {
            sent = drain_agent_turn_queue(state, agent_id)?;
        }
    }

    let queued_turns = state.agent_queued_turns(agent_id)?;
    Ok(SendNextQueuedAgentTurnResult {
        sent,
        pending_turns: queued_turns.len(),
        queued_turns,
    })
}

pub fn send_next_queued_agent_turn(
    state: &AppState,
    agent_id: &str,
) -> Result<SendNextQueuedAgentTurnResult, String> {
    // The explicit "send top item now" action dispatches exactly one turn — a
    // front delivery turn spawns its pane without cascading into the rest of the
    // queue, unlike the automatic drains, which keep going while the agent stays
    // idle. `sent` reports whether the top item was dispatched, whatever its kind.
    let sent = match state.claim_ready_agent_turn(agent_id)? {
        AgentTurnClaim::Ready { turn, pending } => {
            send_claimed_turn(state, agent_id, turn, pending)?;
            true
        }
        AgentTurnClaim::Draining | AgentTurnClaim::Idle => false,
    };
    let queued_turns = state.agent_queued_turns(agent_id)?;
    Ok(SendNextQueuedAgentTurnResult {
        sent,
        pending_turns: queued_turns.len(),
        queued_turns,
    })
}

fn queue_agent_turn(
    state: &AppState,
    agent: &AgentInfo,
    turn: QueuedTurn,
) -> Result<SubmitAgentTurnResult, String> {
    // Direct sends are gated by write_pane's research check, but queueing skips
    // the pane entirely — and a research run never drains a queue (it takes one
    // prompt at launch), so an accepted turn would sit forever and its non-empty
    // queue would park the agent instead of letting retirement reclaim it.
    if state.agent_is_research_run(&agent.id)? {
        return Err("research runs are read-only; create a follow-up branch instead".to_string());
    }
    let pending_turns = state.enqueue_agent_queued_turn(&agent.id, turn)?;
    let queued_turns = state.agent_queued_turns(&agent.id)?;
    state.emit(QmuxEvent::new(
        "agent.turn_queued",
        agent.pane_id.clone(),
        Some(agent.id.clone()),
        json!({ "pendingTurns": pending_turns, "queuedTurns": queued_turns }),
    ));
    Ok(SubmitAgentTurnResult {
        queued: true,
        pending_turns,
        queued_turns,
    })
}

/// After a turn was enqueued, attempt the guarded drain the enqueue may have raced
/// against: if the agent has since gone idle-and-ready (not paused, not mid-typing),
/// send the head of the queue now rather than leaving it stranded until the next
/// trigger. Mirrors the post-enqueue drain in `queue_wait_agent_turn`.
///
/// `drain_agent_turn_queue` claims under the model lock and only sends a genuinely
/// ready turn, so this is a safe no-op when the agent is really still busy.
fn drain_after_enqueue(
    state: &AppState,
    agent_id: &str,
    queued_result: SubmitAgentTurnResult,
    enqueued_turn: &QueuedTurn,
) -> Result<SubmitAgentTurnResult, String> {
    let Some(agent) = state.agent(agent_id)? else {
        return Ok(queued_result);
    };
    let policy = agent_composer_policy(state, &agent)?;
    // Send only if the agent is genuinely idle-and-ready now (not paused, not
    // mid-typing, in a can-send status). drain_agent_turn_queue claims under the
    // model lock, so this is a safe no-op if it is really still busy.
    let _source_ran = !agent.paused
        && !state.agent_is_typing(agent_id)?
        && policy.can_send(agent.status)
        && drain_agent_turn_queue(state, agent_id)?;
    // Rebuild the snapshot after the drain either way: the drain result says only
    // whether the source agent is running now, while the command result should say
    // whether this submitted turn is still queued. Delivery turns can dispatch to a
    // new pane without making the source run, and an existing front item can drain
    // while the newly appended item remains queued.
    let queued_turns = state.agent_queued_turns(agent_id)?;
    Ok(SubmitAgentTurnResult {
        queued: queued_turns.iter().any(|turn| turn == enqueued_turn),
        pending_turns: queued_turns.len(),
        queued_turns,
    })
}

/// Delivers a queued turn that targets a new pane instead of the source agent's own
/// PTY: fork the source session (optionally into a fresh worktree) or start a fresh
/// sibling session, launched with the turn text as its first message. The source
/// agent's status is deliberately left untouched — it stays idle and the caller
/// continues draining its queue.
fn deliver_queued_turn_to_new_pane(
    state: &AppState,
    source: &AgentInfo,
    text: &str,
    delivery: &QueuedTurnDelivery,
) -> Result<(), String> {
    match delivery {
        QueuedTurnDelivery::Fork { use_worktree } => {
            fork_agent_source(state, source, *use_worktree, true, Some(text))?;
        }
        QueuedTurnDelivery::NewSession => {
            spawn_sibling_agent_session(state, source, text)?;
        }
    }
    Ok(())
}

/// A failed turn send. `text_delivered` reports whether the turn's text had
/// already reached the pane when the failure occurred (the paste landed; only the
/// submit leg failed), so the requeue can tag the turn `possibly_pasted` and its
/// retry can avoid pasting a duplicate copy.
struct TurnSendError {
    message: String,
    text_delivered: bool,
}

/// The pane write for one turn: a `possibly_pasted` turn's text is (very likely)
/// already sitting in the composer from a failed prior attempt, so it submits a
/// bare Return instead of pasting a second copy — the concatenation that tag
/// exists to prevent. The outstanding-send record still carries the full text, so
/// the prompt-submit echo of the already-pasted content confirms it normally.
fn turn_write_options(pane_id: String, turn: &QueuedTurn) -> PaneWriteOptions {
    if turn.possibly_pasted {
        PaneWriteOptions {
            pane_id,
            data: String::new(),
            paste: false,
            submit: true,
        }
    } else {
        PaneWriteOptions {
            pane_id,
            data: turn.text.clone(),
            paste: true,
            submit: true,
        }
    }
}

/// Writes one turn into the agent's own pane: reserve prompt-correlation, paste
/// and submit, promote the status, then arm the submit-confirmation watch. Does
/// not requeue on failure — the caller owns the turn and decides (a queue drain
/// rolls it back to the front, tagged via `TurnSendError::text_delivered`).
fn send_agent_turn(
    state: &AppState,
    agent: &AgentInfo,
    turn: &QueuedTurn,
    source: AgentSendSource,
) -> Result<(), TurnSendError> {
    let pane_id = agent.pane_id.clone().ok_or_else(|| TurnSendError {
        message: format!("agent {} does not have an attached pane", agent.id),
        text_delivered: false,
    })?;
    let is_command = is_tui_command_turn(&turn.text);
    // Reserve prompt-correlation before Return reaches the PTY. The acknowledged
    // native path does not return until Return has been flushed, so Claude can emit
    // UserPromptSubmit on another thread before this function resumes. Recording
    // afterward would miss that echo and leave a false outstanding send for the
    // watchdog. Roll the exact record back if delivery fails.
    let send_id = match state.record_agent_send(&agent.id, turn.text.clone(), source) {
        Ok(send_id) => Some(send_id),
        Err(err) => {
            eprintln!("qmux: failed to record send for agent {}: {err}", agent.id);
            None
        }
    };
    if let Err(failure) = write_pane_detailed(state, turn_write_options(pane_id, turn)) {
        if let Some(send_id) = send_id {
            let _ = state.remove_agent_outstanding_send_id(&agent.id, send_id);
        }
        // A possibly-pasted turn's text was delivered by the *previous* attempt,
        // whatever this write's payload leg (a bare Return) reports.
        return Err(TurnSendError {
            message: failure.error,
            text_delivered: failure.data_delivered || turn.possibly_pasted,
        });
    }
    // Everything from here on happened after the turn's bytes reached the pane, so
    // any failure reports the text as delivered.
    let delivered_err = |message: String| TurnSendError {
        message,
        text_delivered: true,
    };
    // Field-scoped status write: a full-struct update_agent here would drop the lock
    // between read and write and could clobber a concurrent SessionStart hook's
    // session_id/transcript_path (leaving the session unresumable/unforkable). Only
    // the status changes, so write only the status.
    if is_command {
        // A built-in TUI command (a `/` slash command or a `!` shell escape) can run
        // hooklessly — no UserPromptSubmit, no Stop/idle — so it never represents a
        // running turn. Marking it Running would stick forever (nothing demotes it).
        // A direct send only reaches here from a can-send (already-ready) status, so
        // there is nothing to clear; but when the same command is *drained from the
        // queue*, the agent is still Running from the just-finished turn, and the
        // drain path (claim_next_turn_or_settle's Sent branch) leaves that Running
        // in place — wedging the pane at "Working…" and freezing the queue behind it.
        // Demote that stale working status so the queue can't deadlock. Skills that do
        // start a real turn re-promote via their own UserPromptSubmit/PreToolUse hooks.
        if let Some(current) = state.agent(&agent.id).map_err(delivered_err)?
            && matches!(current.status, AgentStatus::Running | AgentStatus::Starting)
        {
            state
                .set_agent_status(&agent.id, AgentStatus::AwaitingInput)
                .map_err(delivered_err)?;
        }
    } else {
        state
            .set_agent_status(&agent.id, AgentStatus::Running)
            .map_err(delivered_err)?;
    }
    // A plain-text turn is pasted then submitted with a trailing Return. The paste is
    // durable (it lands in the composer) but the lone Return can be dropped, leaving
    // the turn typed-but-unsubmitted. Arm a watchdog to detect and nudge that. Skip
    // TUI commands: they can run hooklessly, so they never echo the UserPromptSubmit
    // the watchdog keys on, and a spurious Return after one could misfire.
    if !is_command && let Some(send_id) = send_id {
        watch_agent_after_queued_send(state, agent, source, send_id, turn);
    }
    Ok(())
}

/// Grace before the first check of whether a just-sent turn actually submitted. A
/// real submit echoes a `UserPromptSubmit` hook (which pops the outstanding send)
/// and writes the prompt to the transcript within a few hundred ms, so a window
/// this size reliably separates "submitted and running" from "the Return was
/// dropped and the text is sitting unsubmitted".
const SUBMIT_CONFIRM_GRACE: std::time::Duration = std::time::Duration::from_millis(1200);

/// Additional waits before the second and final checks (cumulative ~3s and ~8s
/// after the send). Each non-final check that finds the send still unmatched — and
/// no prompt of any kind submitted since — nudges another bare Return; the final
/// check reclaims the turn instead (see [`reclaim_unconfirmed_send`]).
const SUBMIT_CONFIRM_RECHECKS: [std::time::Duration; 2] = [
    std::time::Duration::from_millis(1800),
    std::time::Duration::from_millis(5000),
];

/// What one submit-watch check should do, derived from [`SubmitWatchStatus`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubmitWatchDecision {
    /// Confirmed — or ambiguous (a prompt was submitted after our send but failed
    /// the exact text match, so the turn very likely started). Both stop recovery:
    /// nudging or requeueing a turn that actually started risks a duplicate, and a
    /// visible stall is the safer failure.
    StandDown,
    /// Still pending with checks remaining: re-send a bare Return (no paste — the
    /// text is already in the composer) and keep watching.
    NudgeReturn,
    /// Still pending on the final check: the turn never started despite the nudges;
    /// return it to the queue.
    Reclaim,
}

fn decide_submit_watch(status: SubmitWatchStatus, final_check: bool) -> SubmitWatchDecision {
    match status {
        SubmitWatchStatus::Confirmed | SubmitWatchStatus::StillPendingWithPromptActivity => {
            SubmitWatchDecision::StandDown
        }
        SubmitWatchStatus::StillPending if final_check => SubmitWatchDecision::Reclaim,
        SubmitWatchStatus::StillPending => SubmitWatchDecision::NudgeReturn,
    }
}

/// After a queued/direct turn is pasted-and-submitted into an agent's own pane,
/// verify the submit landed and recover if it did not. The failure this recovers:
/// the bracketed paste reaches the TUI (visible in the composer) but the trailing
/// Return is dropped — because it arrives before the composer is re-armed to
/// submit, or, on the native surface, races the still-in-flight paste — leaving the
/// turn stranded with the tab stuck at a false "Working…". Recovery escalates: a
/// bare Return is re-sent at each non-final check, and a turn still showing no sign
/// of having started by the final check is returned to the queue (tagged
/// possibly-pasted so its retry cannot duplicate the text). Only turns that go into
/// the agent's own pane and echo their prompt promptly are watched: `Steer` targets
/// a busy agent whose echo legitimately waits for the current turn boundary, so it
/// is excluded to avoid a spurious mid-turn Return.
fn watch_agent_after_queued_send(
    state: &AppState,
    agent: &AgentInfo,
    source: AgentSendSource,
    send_id: u64,
    turn: &QueuedTurn,
) {
    if !matches!(
        source,
        AgentSendSource::QueuedTurn | AgentSendSource::DirectSend
    ) {
        return;
    }
    let Some(pane_id) = agent.pane_id.clone() else {
        return;
    };
    // One watcher per exact send: overlapping sends (a direct send shortly after a
    // queued drain) each get their own confirmation instead of the second going
    // unwatched, while a double arm of the same send stays deduped.
    if !state.begin_agent_submit_watch(&agent.id, send_id) {
        return;
    }
    let state = state.clone();
    let agent_id = agent.id.clone();
    let turn = turn.clone();
    std::thread::spawn(move || {
        run_agent_submit_watch(&state, &agent_id, &pane_id, send_id, turn);
        state.end_agent_submit_watch(&agent_id, send_id);
    });
}

/// The watcher-thread body of [`watch_agent_after_queued_send`]: sleep, check,
/// act, repeat through the escalation schedule.
fn run_agent_submit_watch(
    state: &AppState,
    agent_id: &str,
    pane_id: &str,
    send_id: u64,
    turn: QueuedTurn,
) {
    let checks = 1 + SUBMIT_CONFIRM_RECHECKS.len();
    let delays = std::iter::once(SUBMIT_CONFIRM_GRACE).chain(SUBMIT_CONFIRM_RECHECKS);
    for (index, delay) in delays.enumerate() {
        std::thread::sleep(delay);
        let Ok(status) = state.check_agent_submit_watch(agent_id, send_id) else {
            return;
        };
        match decide_submit_watch(status, index + 1 == checks) {
            SubmitWatchDecision::StandDown => {
                if status == SubmitWatchStatus::StillPendingWithPromptActivity {
                    eprintln!(
                        "qmux: a turn sent to agent {agent_id} never matched its prompt echo, \
                         but a prompt was submitted after it; standing down to avoid a duplicate"
                    );
                }
                return;
            }
            SubmitWatchDecision::NudgeReturn => {
                // Best-effort: a failure here leaves the turn where it was, no
                // worse off; the next check escalates.
                let _ = write_pane(
                    state,
                    PaneWriteOptions {
                        pane_id: pane_id.to_string(),
                        data: String::new(),
                        paste: false,
                        submit: true,
                    },
                );
                eprintln!(
                    "qmux: re-sent Return for agent {agent_id}; a sent turn appeared unsubmitted"
                );
            }
            SubmitWatchDecision::Reclaim => {
                reclaim_unconfirmed_send(state, agent_id, pane_id, send_id, turn);
                return;
            }
        }
    }
}

/// The final escalation of a submit watch: the exact send is still unmatched and no
/// prompt at all was submitted since it was written, so after the unanswered Return
/// nudges the turn is treated as never-started and returned to the front of its
/// queue — tagged possibly-pasted, so the eventual retry submits a bare Return
/// rather than pasting a duplicate copy onto the text already in the composer. The
/// agent's optimistic Running from the failed send is demoted so the tab doesn't
/// sit at a false "Working…" and future drain triggers aren't gated off. Removing
/// the outstanding-send record and requeueing are atomic; if the prompt echo lands
/// in the race window the whole reclaim is a no-op.
fn reclaim_unconfirmed_send(
    state: &AppState,
    agent_id: &str,
    pane_id: &str,
    send_id: u64,
    mut turn: QueuedTurn,
) {
    turn.possibly_pasted = true;
    let queued_turns = match state.requeue_unconfirmed_send(agent_id, send_id, turn) {
        Ok(Some(queued_turns)) => queued_turns,
        Ok(None) => return,
        Err(err) => {
            eprintln!("qmux: failed to reclaim an unconfirmed turn for agent {agent_id}: {err}");
            return;
        }
    };
    eprintln!(
        "qmux: a turn sent to agent {agent_id} never started; returned it to the queue"
    );
    if let Ok(Some(agent)) = state.agent(agent_id)
        && matches!(agent.status, AgentStatus::Running | AgentStatus::Starting)
    {
        let _ = state.set_agent_status(agent_id, AgentStatus::AwaitingInput);
    }
    state.emit(QmuxEvent::new(
        "agent.turn_queued",
        Some(pane_id.to_string()),
        Some(agent_id.to_string()),
        json!({ "pendingTurns": queued_turns.len(), "queuedTurns": queued_turns.clone() }),
    ));
    state.emit(QmuxEvent::new(
        "agent.queue_error",
        Some(pane_id.to_string()),
        Some(agent_id.to_string()),
        json!({
            "error": "A sent turn never started; it was returned to the queue",
            "queuedTurns": queued_turns,
        }),
    ));
}

/// Sends a user turn straight to the agent, but only after reserving the drain guard
/// so it can't race an in-flight queue drain into the same pane. If a drain — or
/// another direct send — already owns the agent, the turn is queued behind it instead
/// of writing a second turn concurrently. The guard is always released afterward.
fn send_direct_or_queue(
    state: &AppState,
    agent: &AgentInfo,
    data: String,
) -> Result<SubmitAgentTurnResult, String> {
    if !state.begin_direct_send(&agent.id)? {
        return queue_agent_turn(state, agent, QueuedTurn::new(data));
    }
    let result = send_agent_turn(
        state,
        agent,
        &QueuedTurn::new(data),
        AgentSendSource::DirectSend,
    );
    state.finish_agent_drain(&agent.id);
    result.map_err(|err| err.message)?;
    let queued_turns = state.agent_queued_turns(&agent.id)?;
    Ok(SubmitAgentTurnResult {
        queued: false,
        pending_turns: queued_turns.len(),
        queued_turns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{ComposerPolicy, PermissionAction};
    use crate::config::{
        AdapterConfigs, ClaudeAdapterConfig, CodexAdapterConfig, GrokAdapterConfig,
        OpencodeAdapterConfig, QmuxConfig,
    };
    use crate::state::{PaneBacklog, PaneInfo, PaneKind, PaneRuntime, PaneStatus};
    use crate::workspace::{detach_pane_agent, mark_agent_failed};
    use portable_pty::{Child, ChildKiller, ExitStatus, PtySize, native_pty_system};
    use std::io;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_workspace() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("qmux-turn-queue-{nanos}-{seq}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_state() -> AppState {
        AppState::new(QmuxConfig {
            workspace_root: temp_workspace(),
            socket_path: PathBuf::from("/tmp/qmux-test.sock"),
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
            claude_plugin_dir: std::path::PathBuf::new(),
            opencode_plugin_dir: std::path::PathBuf::new(),
        })
    }

    fn sample_agent(status: AgentStatus) -> AgentInfo {
        AgentInfo {
            id: "agent-1".to_string(),
            group_id: "group-1".to_string(),
            adapter: "claude".to_string(),
            worktree_dir: "/tmp/work/agent-1".to_string(),
            branch: None,
            pane_id: Some("missing-pane".to_string()),
            orphaned_queue_pane_id: None,
            session_id: None,
            transcript_path: None,
            status,
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

    fn sample_agent_with_id(id: &str, status: AgentStatus, pane_id: Option<&str>) -> AgentInfo {
        AgentInfo {
            id: id.to_string(),
            group_id: "group-1".to_string(),
            adapter: "claude".to_string(),
            worktree_dir: format!("/tmp/work/{id}"),
            branch: None,
            pane_id: pane_id.map(ToString::to_string),
            orphaned_queue_pane_id: None,
            session_id: None,
            transcript_path: None,
            status,
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

    fn sample_pane_runtime(id: &str, agent_id: Option<&str>) -> PaneRuntime {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        drop(pair.slave);

        PaneRuntime {
            info: PaneInfo {
                id: id.to_string(),
                title: id.to_string(),
                last_osc_title: None,
                kind: if agent_id.is_some() {
                    PaneKind::Agent
                } else {
                    PaneKind::Shell
                },
                agent_id: agent_id.map(ToString::to_string),
                group_id: "group-1".to_string(),
                cwd: "/tmp/work".to_string(),
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
                writer: Arc::new(Mutex::new(Box::new(io::sink()))),
                backlog: Arc::new(Mutex::new(PaneBacklog::default())),
                native_surface: false,
            },
        }
    }

    fn claude_policy() -> ComposerPolicy {
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

    #[test]
    fn ready_agent_statuses_send_immediately() {
        let policy = claude_policy();
        assert!(!policy.should_queue(AgentStatus::AwaitingInput));
        assert!(!policy.should_queue(AgentStatus::Done));
        assert!(!policy.should_queue(AgentStatus::Idle));
    }

    #[test]
    fn busy_agent_statuses_queue_turns() {
        let policy = claude_policy();
        assert!(policy.should_queue(AgentStatus::Starting));
        assert!(policy.should_queue(AgentStatus::Running));
        assert!(policy.should_queue(AgentStatus::AwaitingPermission));
    }

    #[test]
    fn steer_statuses_are_policy_owned() {
        let policy = claude_policy();
        assert!(policy.can_steer(AgentStatus::Starting));
        assert!(policy.can_steer(AgentStatus::Running));
        assert!(!policy.can_steer(AgentStatus::AwaitingInput));
    }

    #[test]
    fn shell_escape_turns_are_detected_after_leading_space() {
        assert!(is_shell_escape_turn("!git status"));
        assert!(is_shell_escape_turn("  \n\t!git status"));
        assert!(!is_shell_escape_turn("please run !git status"));
        assert!(!is_shell_escape_turn(""));
    }

    #[test]
    fn tui_command_turns_cover_shell_escapes_and_slash_commands() {
        assert!(is_tui_command_turn("!git status"));
        assert!(is_tui_command_turn("/model"));
        assert!(is_tui_command_turn("  /compact keep the plan"));
        assert!(!is_tui_command_turn("fix the bug in /src/main.rs"));
        assert!(!is_tui_command_turn(""));
    }

    #[test]
    fn slash_command_send_does_not_mark_agent_running() {
        let state = test_state();
        state
            .insert_agent(sample_agent_with_id(
                "agent-1",
                AgentStatus::AwaitingInput,
                Some("pane-1"),
            ))
            .unwrap();
        state
            .insert_pane(sample_pane_runtime("pane-1", Some("agent-1")))
            .unwrap();

        // A slash command may run a hookless TUI built-in (e.g. /model), so the send
        // must not promote the agent to Running — nothing would ever demote it.
        let result = submit_agent_turn(
            &state,
            SubmitAgentTurnRequest {
                agent_id: "agent-1".to_string(),
                data: "/model".to_string(),
                mode: Some(SubmitAgentTurnMode::Send),
            },
        )
        .unwrap();
        assert!(!result.queued);
        let agent = state.agent("agent-1").unwrap().unwrap();
        assert!(matches!(agent.status, AgentStatus::AwaitingInput));

        // A plain prompt still lights up immediately.
        let result = submit_agent_turn(
            &state,
            SubmitAgentTurnRequest {
                agent_id: "agent-1".to_string(),
                data: "hello".to_string(),
                mode: Some(SubmitAgentTurnMode::Send),
            },
        )
        .unwrap();
        assert!(!result.queued);
        let agent = state.agent("agent-1").unwrap().unwrap();
        assert!(matches!(agent.status, AgentStatus::Running));
    }

    #[test]
    fn draining_a_queued_slash_command_clears_a_stale_running() {
        let state = test_state();
        state
            .insert_agent(sample_agent_with_id(
                "agent-1",
                AgentStatus::Running,
                Some("pane-1"),
            ))
            .unwrap();
        state
            .insert_pane(sample_pane_runtime("pane-1", Some("agent-1")))
            .unwrap();

        // A slash command queued behind a running turn. When it drains, the agent is
        // still Running from the just-finished turn and the drain path leaves that in
        // place; because /model runs hooklessly, nothing would ever demote it and the
        // pane would wedge at "Working…" with the rest of the queue stuck behind it.
        state
            .enqueue_agent_turn("agent-1", "/model".to_string())
            .unwrap();
        assert!(drain_agent_turn_queue(&state, "agent-1").unwrap());
        let agent = state.agent("agent-1").unwrap().unwrap();
        assert!(
            matches!(agent.status, AgentStatus::AwaitingInput),
            "a drained hookless command must not leave the agent stuck at Running: {:?}",
            agent.status
        );
    }

    #[test]
    fn submit_watch_nudges_then_reclaims_a_turn_that_never_submitted() {
        let state = test_state();

        // A queued send that never echoed a UserPromptSubmit — and saw no prompt
        // of any kind after it — looks dropped: non-final checks nudge a Return,
        // the final check reclaims the turn.
        let send_id = state
            .record_agent_send("agent-1", "commit".to_string(), AgentSendSource::QueuedTurn)
            .unwrap();
        let status = state.check_agent_submit_watch("agent-1", send_id).unwrap();
        assert_eq!(status, SubmitWatchStatus::StillPending);
        assert_eq!(
            decide_submit_watch(status, false),
            SubmitWatchDecision::NudgeReturn
        );
        assert_eq!(
            decide_submit_watch(status, true),
            SubmitWatchDecision::Reclaim
        );
    }

    #[test]
    fn submit_watch_ignores_unrelated_late_activity() {
        let state = test_state();
        state
            .insert_agent(sample_agent_with_id(
                "agent-1",
                AgentStatus::Running,
                Some("pane-1"),
            ))
            .unwrap();

        let send_id = state
            .record_agent_send("agent-1", "commit".to_string(), AgentSendSource::QueuedTurn)
            .unwrap();
        // A late status/transcript mutation from the previous turn is not proof
        // that this exact prompt submitted; its outstanding record still requires
        // recovery.
        state
            .set_agent_status("agent-1", AgentStatus::Running)
            .unwrap();
        assert_eq!(
            state.check_agent_submit_watch("agent-1", send_id).unwrap(),
            SubmitWatchStatus::StillPending
        );
    }

    #[test]
    fn submit_watch_stands_down_when_send_already_echoed() {
        let state = test_state();

        // No outstanding send (the UserPromptSubmit echo already popped it):
        // nothing to nudge even though the agent is otherwise quiet.
        let status = state.check_agent_submit_watch("agent-1", 1).unwrap();
        assert_eq!(status, SubmitWatchStatus::Confirmed);
        assert_eq!(
            decide_submit_watch(status, false),
            SubmitWatchDecision::StandDown
        );
        assert_eq!(
            decide_submit_watch(status, true),
            SubmitWatchDecision::StandDown
        );
    }

    #[test]
    fn submit_watch_stands_down_after_any_prompt_submit() {
        let state = test_state();

        let send_id = state
            .record_agent_send("agent-1", "commit".to_string(), AgentSendSource::QueuedTurn)
            .unwrap();
        // A prompt submitted after our send that fails the exact text match (e.g.
        // the paste was mangled or concatenated) leaves the record outstanding —
        // but the turn very likely started, so recovery must stand down on every
        // check rather than nudge or requeue a duplicate.
        state
            .match_agent_prompt_submit("agent-1", Some("commit commit"))
            .unwrap();
        let status = state.check_agent_submit_watch("agent-1", send_id).unwrap();
        assert_eq!(status, SubmitWatchStatus::StillPendingWithPromptActivity);
        assert_eq!(
            decide_submit_watch(status, false),
            SubmitWatchDecision::StandDown
        );
        assert_eq!(
            decide_submit_watch(status, true),
            SubmitWatchDecision::StandDown
        );
    }

    #[test]
    fn submit_watch_reservation_is_per_send() {
        let state = test_state();
        assert!(state.begin_agent_submit_watch("agent-1", 1));
        // Same send: deduped. A different overlapping send: watched independently.
        assert!(!state.begin_agent_submit_watch("agent-1", 1));
        assert!(state.begin_agent_submit_watch("agent-1", 2));
        state.end_agent_submit_watch("agent-1", 1);
        assert!(state.begin_agent_submit_watch("agent-1", 1));
    }

    #[test]
    fn reclaim_returns_the_turn_to_the_queue_tagged_and_demotes_running() {
        let state = test_state();
        state
            .insert_agent(sample_agent_with_id(
                "agent-1",
                AgentStatus::Running,
                Some("pane-1"),
            ))
            .unwrap();

        let send_id = state
            .record_agent_send("agent-1", "commit".to_string(), AgentSendSource::QueuedTurn)
            .unwrap();
        reclaim_unconfirmed_send(
            &state,
            "agent-1",
            "pane-1",
            send_id,
            QueuedTurn::new("commit".to_string()),
        );

        // The turn is back at the front, tagged so its retry submits without
        // re-pasting; the record is consumed; the false "Working…" is demoted.
        let queued = state.agent_queued_turns("agent-1").unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].text, "commit");
        assert!(queued[0].possibly_pasted);
        assert!(state.outstanding_agent_sends("agent-1").unwrap().is_empty());
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::AwaitingInput
        ));
    }

    #[test]
    fn reclaim_is_a_noop_once_the_echo_landed() {
        let state = test_state();
        state
            .insert_agent(sample_agent_with_id(
                "agent-1",
                AgentStatus::Running,
                Some("pane-1"),
            ))
            .unwrap();

        let send_id = state
            .record_agent_send("agent-1", "commit".to_string(), AgentSendSource::QueuedTurn)
            .unwrap();
        // The echo wins the race just before the reclaim runs: the turn started,
        // so nothing may be requeued and the running status stays.
        state
            .match_agent_prompt_submit("agent-1", Some("commit"))
            .unwrap();
        reclaim_unconfirmed_send(
            &state,
            "agent-1",
            "pane-1",
            send_id,
            QueuedTurn::new("commit".to_string()),
        );

        assert!(state.agent_queued_turns("agent-1").unwrap().is_empty());
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        ));
    }

    #[test]
    fn possibly_pasted_turns_submit_without_repasting() {
        let mut turn = QueuedTurn::new("commit".to_string());
        let options = turn_write_options("pane-1".to_string(), &turn);
        assert_eq!(options.data, "commit");
        assert!(options.paste);
        assert!(options.submit);

        // A retry of a turn whose text already reached the composer sends only the
        // Return, so it can never concatenate a second copy of the text.
        turn.possibly_pasted = true;
        let options = turn_write_options("pane-1".to_string(), &turn);
        assert_eq!(options.data, "");
        assert!(!options.paste);
        assert!(options.submit);
    }

    #[test]
    fn failed_direct_send_preserves_agent_status_and_tracking() {
        let state = test_state();
        state
            .insert_agent(sample_agent(AgentStatus::AwaitingInput))
            .unwrap();

        let err = submit_agent_turn(
            &state,
            SubmitAgentTurnRequest {
                agent_id: "agent-1".to_string(),
                data: "hello".to_string(),
                mode: Some(SubmitAgentTurnMode::Send),
            },
        )
        .unwrap_err();

        assert!(err.contains("pane missing-pane was not found"));
        let agent = state.agent("agent-1").unwrap().unwrap();
        assert!(matches!(agent.status, AgentStatus::AwaitingInput));
        assert!(state.outstanding_agent_sends("agent-1").unwrap().is_empty());
    }

    #[test]
    fn failed_queue_drain_reprepends_turn_without_running_status() {
        let state = test_state();
        state.insert_agent(sample_agent(AgentStatus::Done)).unwrap();
        state
            .enqueue_agent_turn("agent-1", "queued turn".to_string())
            .unwrap();

        let err = drain_agent_turn_queue(&state, "agent-1").unwrap_err();

        assert!(err.contains("pane missing-pane was not found"));
        assert_eq!(
            state.list_agent_turn_queue("agent-1").unwrap(),
            vec!["queued turn".to_string()]
        );
        let agent = state.agent("agent-1").unwrap().unwrap();
        assert!(matches!(agent.status, AgentStatus::Done));
        assert!(state.outstanding_agent_sends("agent-1").unwrap().is_empty());
    }

    #[test]
    fn failed_drain_preserves_the_pause_after_flag() {
        let state = test_state();
        state.insert_agent(sample_agent(AgentStatus::Done)).unwrap();
        state
            .enqueue_agent_turn("agent-1", "queued".to_string())
            .unwrap();
        state
            .set_queued_turn_pause("agent-1", 0, true, Some("queued"), None)
            .unwrap();

        // The pane is missing, so the send fails and the turn is requeued.
        let err = drain_agent_turn_queue(&state, "agent-1").unwrap_err();
        assert!(err.contains("missing-pane"));

        // The requeued turn keeps its pause-after flag (not reset to false).
        let items = state.agent_queued_turns("agent-1").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "queued");
        assert!(items[0].pause_after);
    }

    fn agent_with_id(id: &str, status: AgentStatus) -> AgentInfo {
        let mut agent = sample_agent(status);
        agent.id = id.to_string();
        agent
    }

    #[test]
    fn move_onto_a_busy_target_relocates_the_turn_without_duplicating() {
        let state = test_state();
        let mut source = agent_with_id("source", AgentStatus::Done);
        source.pane_id = None;
        state.insert_agent(source).unwrap();
        // A busy target queues rather than sends, so no pane write is needed.
        state
            .insert_agent(agent_with_id("target", AgentStatus::Running))
            .unwrap();
        state
            .enqueue_agent_turn("source", "move me".to_string())
            .unwrap();

        let result = move_queued_agent_turn(
            &state,
            MoveQueuedAgentTurnRequest {
                from_agent_id: "source".to_string(),
                to_agent_id: "target".to_string(),
                index: 0,
                expected_data: Some("move me".to_string()),
                expected_id: None,
            },
        )
        .unwrap();

        assert!(!result.sent);
        assert!(state.list_agent_turn_queue("source").unwrap().is_empty());
        assert_eq!(
            state.list_agent_turn_queue("target").unwrap(),
            vec!["move me".to_string()]
        );
    }

    #[test]
    fn failed_move_rolls_the_turn_back_to_the_source() {
        let state = test_state();
        let mut source = agent_with_id("source", AgentStatus::Done);
        source.pane_id = None;
        state.insert_agent(source).unwrap();
        // The target is ready to send but its pane is missing, so the handoff fails.
        state
            .insert_agent(agent_with_id("target", AgentStatus::AwaitingInput))
            .unwrap();
        state
            .enqueue_agent_turn("source", "keep me".to_string())
            .unwrap();

        let err = move_queued_agent_turn(
            &state,
            MoveQueuedAgentTurnRequest {
                from_agent_id: "source".to_string(),
                to_agent_id: "target".to_string(),
                index: 0,
                expected_data: Some("keep me".to_string()),
                expected_id: None,
            },
        )
        .unwrap_err();

        assert!(err.contains("missing-pane"));
        // Restored to the source rather than duplicated into both or lost.
        assert_eq!(
            state.list_agent_turn_queue("source").unwrap(),
            vec!["keep me".to_string()]
        );
        assert!(state.list_agent_turn_queue("target").unwrap().is_empty());
    }

    #[test]
    fn failed_wait_target_keeps_waiting_front_turn_blocked() {
        let state = test_state();
        state
            .insert_agent(sample_agent_with_id(
                "source",
                AgentStatus::Done,
                Some("source-pane"),
            ))
            .unwrap();
        state
            .insert_agent(sample_agent_with_id(
                "target",
                AgentStatus::Running,
                Some("target-pane"),
            ))
            .unwrap();
        state
            .insert_pane(sample_pane_runtime("source-pane", Some("source")))
            .unwrap();
        state
            .enqueue_agent_wait_turn_with_target_label(
                "source",
                "after failure".to_string(),
                "target",
                None,
                None,
            )
            .unwrap();

        mark_agent_failed(&state, "target").unwrap();

        // A failed target intentionally keeps its waiters blocked: marking it failed must
        // not release the waiter, and an explicit drain attempt must not send the turn.
        assert!(!drain_agent_turn_queue(&state, "source").unwrap());
        assert_eq!(
            state.list_agent_turn_queue("source").unwrap(),
            vec!["after failure".to_string()]
        );
        assert!(matches!(
            state.agent("source").unwrap().unwrap().status,
            AgentStatus::Done
        ));
    }

    #[test]
    fn parked_wait_target_with_queue_keeps_waiting_front_turn_blocked() {
        let state = test_state();
        state
            .insert_agent(sample_agent_with_id(
                "source",
                AgentStatus::Done,
                Some("source-pane"),
            ))
            .unwrap();
        // Target is parked (pane closed, no pane binding) but still owns queued work —
        // exactly the state remove_pane leaves an agent in when its pane is closed with a
        // live queue.
        state
            .insert_agent(sample_agent_with_id("target", AgentStatus::Idle, None))
            .unwrap();
        state
            .insert_pane(sample_pane_runtime("source-pane", Some("source")))
            .unwrap();
        state
            .enqueue_agent_turn("target", "unfinished".to_string())
            .unwrap();
        state
            .enqueue_agent_wait_turn_with_target_label(
                "source",
                "after target's queue".to_string(),
                "target",
                None,
                None,
            )
            .unwrap();

        // The parked target still has unfinished queued work, so "run after X finishes its
        // queue" must stay blocked rather than firing the moment the pane closed.
        assert!(!drain_agent_turn_queue(&state, "source").unwrap());
        assert_eq!(
            state.list_agent_turn_queue("source").unwrap(),
            vec!["after target's queue".to_string()]
        );
    }

    #[test]
    fn parked_wait_target_with_empty_queue_resolves_waiter() {
        let state = test_state();
        state
            .insert_agent(sample_agent_with_id(
                "source",
                AgentStatus::Done,
                Some("source-pane"),
            ))
            .unwrap();
        // Parked target with nothing left in its queue: there is no more work to finish,
        // so the waiter is released and its turn drains.
        state
            .insert_agent(sample_agent_with_id("target", AgentStatus::Idle, None))
            .unwrap();
        state
            .insert_pane(sample_pane_runtime("source-pane", Some("source")))
            .unwrap();
        state
            .enqueue_agent_wait_turn_with_target_label(
                "source",
                "after target's queue".to_string(),
                "target",
                None,
                None,
            )
            .unwrap();

        assert!(drain_agent_turn_queue(&state, "source").unwrap());
        assert!(state.list_agent_turn_queue("source").unwrap().is_empty());
    }

    #[test]
    fn queue_wait_result_reports_new_turn_still_queued_when_front_turn_drains() {
        let state = test_state();
        state
            .insert_agent(sample_agent_with_id(
                "source",
                AgentStatus::Done,
                Some("source-pane"),
            ))
            .unwrap();
        state
            .insert_agent(sample_agent_with_id(
                "target",
                AgentStatus::Running,
                Some("target-pane"),
            ))
            .unwrap();
        state
            .insert_pane(sample_pane_runtime("source-pane", Some("source")))
            .unwrap();
        state
            .enqueue_agent_turn("source", "send first".to_string())
            .unwrap();

        let result = queue_wait_agent_turn(
            &state,
            QueueWaitAgentTurnRequest {
                agent_id: "source".to_string(),
                data: "after target".to_string(),
                wait_for_agent_id: "target".to_string(),
                wait_for_pane_id: None,
                wait_for_label: None,
            },
        )
        .unwrap();

        assert!(result.queued);
        assert_eq!(result.pending_turns, 1);
        assert_eq!(result.queued_turns[0].text, "after target");
        assert!(result.queued_turns[0].wait_for.is_some());
        assert_eq!(
            state.list_agent_turn_queue("source").unwrap(),
            vec!["after target".to_string()]
        );
        assert!(matches!(
            state.agent("source").unwrap().unwrap().status,
            AgentStatus::Running
        ));
    }

    #[test]
    fn detached_wait_target_releases_waiting_front_turn() {
        let state = test_state();
        state
            .insert_agent(sample_agent_with_id(
                "source",
                AgentStatus::Done,
                Some("source-pane"),
            ))
            .unwrap();
        state
            .insert_agent(sample_agent_with_id(
                "target",
                AgentStatus::Running,
                Some("target-pane"),
            ))
            .unwrap();
        state
            .insert_pane(sample_pane_runtime("source-pane", Some("source")))
            .unwrap();
        state
            .enqueue_agent_wait_turn_with_target_label(
                "source",
                "after detach".to_string(),
                "target",
                None,
                None,
            )
            .unwrap();

        detach_pane_agent(&state, "target-pane").unwrap().unwrap();

        assert!(state.list_agent_turn_queue("source").unwrap().is_empty());
        assert!(matches!(
            state.agent("source").unwrap().unwrap().status,
            AgentStatus::Running
        ));
    }

    #[test]
    fn reorder_to_ready_front_turn_drains_idle_queue() {
        let state = test_state();
        state
            .insert_agent(sample_agent_with_id(
                "source",
                AgentStatus::Done,
                Some("source-pane"),
            ))
            .unwrap();
        state
            .insert_agent(sample_agent_with_id(
                "target",
                AgentStatus::Running,
                Some("target-pane"),
            ))
            .unwrap();
        state
            .insert_pane(sample_pane_runtime("source-pane", Some("source")))
            .unwrap();
        state
            .enqueue_agent_wait_turn_with_target_label(
                "source",
                "after target".to_string(),
                "target",
                None,
                None,
            )
            .unwrap();
        state
            .enqueue_agent_turn("source", "send now".to_string())
            .unwrap();

        let result = reorder_queued_agent_turn(
            &state,
            ReorderQueuedAgentTurnRequest {
                agent_id: "source".to_string(),
                from_index: 1,
                to_index: 0,
                expected_data: Some("send now".to_string()),
                expected_id: None,
            },
        )
        .unwrap();

        assert_eq!(result.pending_turns, 1);
        assert_eq!(result.queued_turns[0].text, "after target");
        assert_eq!(
            state.list_agent_turn_queue("source").unwrap(),
            vec!["after target".to_string()]
        );
        assert!(matches!(
            state.agent("source").unwrap().unwrap().status,
            AgentStatus::Running
        ));
    }

    #[test]
    fn auto_submit_appends_behind_existing_queue_for_ready_agent() {
        let state = test_state();
        state
            .insert_agent(sample_agent(AgentStatus::AwaitingInput))
            .unwrap();
        state
            .enqueue_agent_turn("agent-1", "first queued".to_string())
            .unwrap();

        let result = submit_agent_turn(
            &state,
            SubmitAgentTurnRequest {
                agent_id: "agent-1".to_string(),
                data: "second queued".to_string(),
                mode: Some(SubmitAgentTurnMode::Auto),
            },
        )
        .unwrap();

        assert!(result.queued);
        assert_eq!(result.pending_turns, 2);
        assert_eq!(
            state.list_agent_turn_queue("agent-1").unwrap(),
            vec!["first queued".to_string(), "second queued".to_string()]
        );
    }

    #[test]
    fn explicit_queue_appends_behind_existing_queue_for_ready_agent() {
        let state = test_state();
        state.insert_agent(sample_agent(AgentStatus::Done)).unwrap();
        state
            .enqueue_agent_turn("agent-1", "first queued".to_string())
            .unwrap();

        let result = submit_agent_turn(
            &state,
            SubmitAgentTurnRequest {
                agent_id: "agent-1".to_string(),
                data: "second queued".to_string(),
                mode: Some(SubmitAgentTurnMode::Queue),
            },
        )
        .unwrap();

        assert!(result.queued);
        assert_eq!(result.pending_turns, 2);
        assert_eq!(
            state.list_agent_turn_queue("agent-1").unwrap(),
            vec!["first queued".to_string(), "second queued".to_string()]
        );
    }

    #[test]
    fn paused_agent_queues_auto_submit_instead_of_sending() {
        let state = test_state();
        state.insert_agent(sample_agent(AgentStatus::Done)).unwrap();
        state.set_agent_paused("agent-1", true).unwrap();

        let result = submit_agent_turn(
            &state,
            SubmitAgentTurnRequest {
                agent_id: "agent-1".to_string(),
                data: "while paused".to_string(),
                mode: Some(SubmitAgentTurnMode::Auto),
            },
        )
        .unwrap();

        // A paused + idle agent would otherwise send straight through; honor the pause
        // by holding the turn in the queue instead.
        assert!(result.queued);
        assert_eq!(
            state.list_agent_turn_queue("agent-1").unwrap(),
            vec!["while paused".to_string()]
        );
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Done
        ));
    }

    #[test]
    fn queued_turn_delivery_serde_round_trips_and_accepts_legacy_shapes() {
        let turn = QueuedTurn::delivering(
            "do it".to_string(),
            QueuedTurnDelivery::Fork { use_worktree: true },
        );
        let json = serde_json::to_string(&turn).unwrap();
        assert!(json.contains(r#""kind":"fork""#), "unexpected json: {json}");
        assert!(
            json.contains(r#""useWorktree":true"#),
            "unexpected json: {json}"
        );
        let back: QueuedTurn = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.delivery,
            Some(QueuedTurnDelivery::Fork { use_worktree: true })
        );
        assert_eq!(back.text, "do it");

        let new_session: QueuedTurn =
            serde_json::from_str(r#"{"text":"t","delivery":{"kind":"newSession"}}"#).unwrap();
        assert_eq!(new_session.delivery, Some(QueuedTurnDelivery::NewSession));

        // The legacy persisted shapes (a bare string; an object without the new
        // field) still load, with no delivery directive.
        let legacy: QueuedTurn = serde_json::from_str(r#""plain text""#).unwrap();
        assert!(legacy.delivery.is_none());
        let object: QueuedTurn = serde_json::from_str(r#"{"text":"t","pauseAfter":true}"#).unwrap();
        assert!(object.delivery.is_none());
        assert!(object.pause_after);
    }

    #[test]
    fn queue_delivery_turn_rejects_fork_for_unsupported_adapter() {
        let state = test_state();
        let mut agent = sample_agent(AgentStatus::Running);
        agent.adapter = "custom".to_string();
        state.insert_agent(agent).unwrap();

        let err = queue_delivery_agent_turn(
            &state,
            QueueDeliveryAgentTurnRequest {
                agent_id: "agent-1".to_string(),
                data: "fork me".to_string(),
                delivery: QueuedTurnDelivery::Fork {
                    use_worktree: false,
                },
            },
        )
        .unwrap_err();

        assert!(
            err.contains("not supported for this agent adapter"),
            "unexpected error: {err}"
        );
        assert!(state.list_agent_turn_queue("agent-1").unwrap().is_empty());
    }

    #[test]
    fn queue_delivery_new_session_queues_behind_a_busy_agent_for_any_adapter() {
        let state = test_state();
        let mut agent = sample_agent(AgentStatus::Running);
        agent.adapter = "opencode".to_string();
        state.insert_agent(agent).unwrap();

        let result = queue_delivery_agent_turn(
            &state,
            QueueDeliveryAgentTurnRequest {
                agent_id: "agent-1".to_string(),
                data: "fresh start".to_string(),
                delivery: QueuedTurnDelivery::NewSession,
            },
        )
        .unwrap();

        assert!(result.queued);
        let queued = state.agent_queued_turns("agent-1").unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].text, "fresh start");
        assert_eq!(queued[0].delivery, Some(QueuedTurnDelivery::NewSession));
    }

    #[test]
    fn failed_delivery_dispatch_requeues_the_turn_with_its_directive() {
        let state = test_state();
        // A ready Claude agent with a live pane but no recorded session id: the
        // immediate dispatch attempts the fork, which fails before any spawn.
        state
            .insert_agent(sample_agent_with_id(
                "agent-1",
                AgentStatus::Done,
                Some("pane-1"),
            ))
            .unwrap();
        state
            .insert_pane(sample_pane_runtime("pane-1", Some("agent-1")))
            .unwrap();

        // The enqueue itself succeeds; the dispatch failure is reported as a queue
        // error event rather than failing the command.
        let result = queue_delivery_agent_turn(
            &state,
            QueueDeliveryAgentTurnRequest {
                agent_id: "agent-1".to_string(),
                data: "fork me".to_string(),
                delivery: QueuedTurnDelivery::Fork {
                    use_worktree: false,
                },
            },
        )
        .unwrap();
        assert!(result.queued);

        // The turn is back at the front with its delivery directive intact, the
        // agent is still idle (never marked Running), and the drain guard is clear
        // (a later explicit drain reaches the fork attempt again).
        let queued = state.agent_queued_turns("agent-1").unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].text, "fork me");
        assert_eq!(
            queued[0].delivery,
            Some(QueuedTurnDelivery::Fork {
                use_worktree: false
            })
        );
        let agent = state.agent("agent-1").unwrap().unwrap();
        assert!(matches!(agent.status, AgentStatus::Done));

        let err = drain_agent_turn_queue(&state, "agent-1").unwrap_err();
        assert!(!err.is_empty());
        assert_eq!(state.agent_queued_turns("agent-1").unwrap().len(), 1);
    }

    #[test]
    fn duplicate_idle_does_not_drain_past_an_outstanding_queued_send() {
        let state = test_state();
        state
            .insert_agent(sample_agent(AgentStatus::Running))
            .unwrap();
        state
            .record_agent_send(
                "agent-1",
                "already drained".to_string(),
                AgentSendSource::QueuedTurn,
            )
            .unwrap();
        state
            .enqueue_agent_turn("agent-1", "still queued".to_string())
            .unwrap();

        let resolution = advance_after_idle(&state, "agent-1").unwrap();

        assert!(matches!(resolution, IdleResolution::Drained));
        assert_eq!(
            state.list_agent_turn_queue("agent-1").unwrap(),
            vec!["still queued".to_string()]
        );
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::Running
        ));
    }

    #[test]
    fn interruption_settles_agent_to_awaiting_input() {
        let state = test_state();
        state
            .insert_agent(sample_agent(AgentStatus::Running))
            .unwrap();

        let resolution = advance_after_interruption(&state, "agent-1").unwrap();

        assert!(matches!(resolution, IdleResolution::Idle));
        assert!(matches!(
            state.agent("agent-1").unwrap().unwrap().status,
            AgentStatus::AwaitingInput
        ));
    }

    // An interrupted turn that lands on a pausing agent is still aborted work:
    // the pause branch must not present it as Done, and a dependent that asked
    // to wait for the target's actual completion must stay queued.
    #[test]
    fn interruption_on_pending_pause_keeps_waiters_and_awaiting_input() {
        let state = test_state();
        state
            .insert_agent(sample_agent_with_id(
                "source",
                AgentStatus::Done,
                Some("source-pane"),
            ))
            .unwrap();
        state
            .insert_agent(sample_agent_with_id(
                "target",
                AgentStatus::Running,
                Some("target-pane"),
            ))
            .unwrap();
        state
            .insert_pane(sample_pane_runtime("source-pane", Some("source")))
            .unwrap();
        state
            .enqueue_agent_wait_turn_with_target_label(
                "source",
                "after target".to_string(),
                "target",
                None,
                None,
            )
            .unwrap();
        state.mark_agent_pending_pause("target").unwrap();

        let resolution = advance_after_interruption(&state, "target").unwrap();

        assert!(matches!(resolution, IdleResolution::Paused));
        let target = state.agent("target").unwrap().unwrap();
        assert!(target.paused);
        assert!(matches!(target.status, AgentStatus::AwaitingInput));
        // The waiter was not released: its wait turn is still queued and it
        // was not started.
        assert_eq!(
            state.list_agent_turn_queue("source").unwrap(),
            vec!["after target".to_string()]
        );
        assert!(matches!(
            state.agent("source").unwrap().unwrap().status,
            AgentStatus::Done
        ));
    }

    #[test]
    fn failed_delivery_at_idle_settles_the_agent_instead_of_stranding_it() {
        let state = test_state();
        // The agent just finished a turn (still Running when its Stop hook fires)
        // with a fork-delivery turn queued; the fork fails (no recorded session id).
        state
            .insert_agent(sample_agent_with_id(
                "agent-1",
                AgentStatus::Running,
                Some("pane-1"),
            ))
            .unwrap();
        state
            .insert_pane(sample_pane_runtime("pane-1", Some("agent-1")))
            .unwrap();
        state
            .enqueue_agent_queued_turn(
                "agent-1",
                QueuedTurn::delivering(
                    "fork me".to_string(),
                    QueuedTurnDelivery::Fork {
                        use_worktree: false,
                    },
                ),
            )
            .unwrap();

        let err = advance_after_idle(&state, "agent-1").unwrap_err();
        assert!(!err.is_empty());

        // The failure must not strand the tab in a stale Running status (no future
        // idle hook would ever clear it); the turn stays requeued for the user to
        // retry or remove.
        let agent = state.agent("agent-1").unwrap().unwrap();
        assert!(matches!(agent.status, AgentStatus::Done));
        assert_eq!(state.agent_queued_turns("agent-1").unwrap().len(), 1);
    }

    #[test]
    fn paused_agent_queues_explicit_send() {
        let state = test_state();
        state
            .insert_agent(sample_agent(AgentStatus::AwaitingInput))
            .unwrap();
        state.set_agent_paused("agent-1", true).unwrap();

        let result = submit_agent_turn(
            &state,
            SubmitAgentTurnRequest {
                agent_id: "agent-1".to_string(),
                data: "explicit while paused".to_string(),
                mode: Some(SubmitAgentTurnMode::Send),
            },
        )
        .unwrap();

        // Even an explicit Send must not bypass the pause and jump the held queue.
        assert!(result.queued);
        assert_eq!(
            state.list_agent_turn_queue("agent-1").unwrap(),
            vec!["explicit while paused".to_string()]
        );
    }
}
