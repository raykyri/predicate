import type {
  AgentInfo,
  ThreadBranch,
  ThreadGraph,
  ThreadNode,
  ThreadParticipant,
  TranscriptAnnotation,
  Turn,
  TurnNode,
} from "../types";

// The agent's thread id, mirroring the backend's fallback (agent_thread_id in
// thread_graph.rs): the stored value when present, else the synthetic
// `thread-{agentId}` the backend keys graph records by for agents that never
// got an explicit thread. Kept untrimmed so lookups match the backend's keys.
export function threadIdForAgent(agent: AgentInfo): string {
  return agent.threadId && agent.threadId.trim() ? agent.threadId : `thread-${agent.id}`;
}

export function addThreadGraphAnnotation(
  graph: ThreadGraph,
  annotation: TranscriptAnnotation,
): ThreadGraph {
  const current = graph.annotations ?? [];
  if (current.some(({ id }) => id === annotation.id)) {
    return graph;
  }
  return { ...graph, annotations: [...current, annotation] };
}

export function removeThreadGraphAnnotation(
  graph: ThreadGraph,
  annotationId: string,
): ThreadGraph {
  const current = graph.annotations ?? [];
  const next = current.filter(({ id }) => id !== annotationId);
  return next.length === current.length ? graph : { ...graph, annotations: next };
}

/** Highlights follow their source turn onto every branch that contains it.
 * Keep a missing-node highlight as an orphan, but do not mistake a source on
 * another branch for a deleted source. */
export function threadGraphAnnotationsForTurns(
  graph: ThreadGraph,
  turns: Turn[],
): TranscriptAnnotation[] {
  const visibleTurnIds = new Set(turns.map((turn) => turn.id));
  return (graph.annotations ?? []).filter(
    (annotation) =>
      visibleTurnIds.has(annotation.sourceTurnId) || !graph.nodes[annotation.sourceTurnId],
  );
}

// The branch whose turns the right pane renders for this agent — shared by
// focusedBranchTurns and pendingGraphOverlayTurns so the overlay's acceptance
// check can never validate a different branch than the one rendered.
export function resolveFocusedBranchId(graph: ThreadGraph, agent: AgentInfo): string {
  return agent.branchId?.trim() || graph.focusedBranchId;
}

export function buildSingleAgentThreadGraph(agent: AgentInfo, turns: Turn[]): ThreadGraph {
  const threadId = threadIdForAgent(agent);
  const branchId = agent.branchId?.trim() || `branch-${agent.id}`;
  const nodes: Record<string, ThreadNode> = {};
  const rootTurnIds: string[] = [];
  let previousTurnId: string | null = null;

  turns.forEach((turn, index) => {
    if (!rootTurnIds.length) {
      rootTurnIds.push(turn.id);
    }
    nodes[turn.id] = {
      kind: "turn",
      id: turn.id,
      threadId,
      branchId,
      parentTurnIds: previousTurnId ? [previousTurnId] : [],
      participant: participantForTurn(agent, turn),
      createdAt: agent.createdAt,
      createdOrder: index,
      status: turn.status ?? null,
      statusReason: turn.statusReason ?? null,
      contextStatus: turn.contextStatus ?? null,
      turn: {
        role: turn.role,
        blocks: turn.blocks,
        sourceIndex: turn.sourceIndex,
      },
      native: {
        adapter: agent.adapter,
        agentId: agent.id,
        sessionId: turn.sessionId ?? agent.sessionId ?? null,
        transcriptPath: agent.transcriptPath ?? null,
        nativeId: turn.nativeId ?? null,
        parentNativeId: turn.parentNativeId ?? null,
        nativeMessageId: turn.nativeMessageId ?? null,
        sourceIndex: turn.sourceIndex,
      },
    };
    previousTurnId = turn.id;
  });

  const branch: ThreadBranch = {
    id: branchId,
    threadId,
    parentBranchId: null,
    baseTurnId: null,
    createdFromTurnId: null,
    headTurnIds: previousTurnId ? [previousTurnId] : [],
    label: null,
    createdByAgentId: agent.id,
    createdByActorId: agent.id,
    createdAt: agent.createdAt,
    status: "active",
  };

  return {
    version: 1,
    threadId,
    focusedBranchId: branchId,
    nextCreatedOrder: turns.length,
    rootTurnIds,
    branches: { [branchId]: branch },
    nodes,
  };
}

// Decides whether a stored thread graph can render the agent's timeline, and
// which of the agent's current turns the graph hasn't absorbed yet. The graph
// refresh is debounced behind the live turn stream, so during streaming the
// newest turn(s) are typically missing from the graph for a few hundred
// milliseconds. Falling back to the (200-capped) raw turn list in that window
// swaps a long history for a short one and back — a full DOM replacement per
// appended turn. Instead, keep rendering the stored graph and overlay the
// missing suffix on top.
//
// `branchTurns` must be focusedBranchTurns(graph, agent) — the prefix the pane
// will actually render. Membership is checked against that branch, not the
// whole graph, so a live history from one branch can never be stitched onto a
// different branch's turns.
//
// Returns the turns to append after `branchTurns`, or null when the stored
// graph cannot represent this history at all:
// - the agent's branch is missing from the graph, or
// - none of the current turns are on the rendered branch (a different
//   session/branch/transcript was loaded wholesale), or
// - a turn missing from the branch is already known to the graph (it belongs
//   to another branch — the live history diverges from the rendered one), or
// - the missing turns are interleaved with known ones rather than a trailing
//   suffix (the history was rewritten, not appended to).
// Turn ids are deterministic over transcript position (`{agentId}-{sourceIndex}`),
// so a turn already in the graph keeps its id across re-parses and a genuinely
// new turn can only appear after the known ones. That also means the check is
// content-blind: a rewrite that reuses line indexes (rewind-then-continue) can
// briefly render the graph's superseded copy of a turn — bounded by the reset
// event's graph refresh, which rewrites the branch and lands within the same
// debounce window.
export function pendingGraphOverlayTurns(
  graph: ThreadGraph,
  agent: AgentInfo,
  branchTurns: Turn[],
  turns: Turn[],
): Turn[] | null {
  const branchId = resolveFocusedBranchId(graph, agent);
  if (!branchId || !graph.branches[branchId]) {
    return null;
  }
  const branchTurnIds = new Set(branchTurns.map((turn) => turn.id));
  let firstMissing = turns.length;
  for (let index = 0; index < turns.length; index += 1) {
    if (!branchTurnIds.has(turns[index].id)) {
      firstMissing = index;
      break;
    }
  }
  if (firstMissing === turns.length) {
    // The rendered branch already contains every turn — nothing pending.
    return [];
  }
  if (firstMissing === 0) {
    // No overlap with the rendered branch: the graph describes another history.
    return null;
  }
  for (let index = firstMissing; index < turns.length; index += 1) {
    // A "missing" turn the graph does know (on any branch) means divergence,
    // not appending; interleaved branch members are caught here too.
    if (graph.nodes[turns[index].id]?.kind === "turn") {
      return null;
    }
  }
  return turns.slice(firstMissing);
}

/**
 * A stored graph refresh is debounced behind the live transcript stream. Keep
 * rendering its full branch, but let the live window override mutable turn
 * state immediately so an interruption or rollback does not briefly show the
 * graph's stale status/context flags.
 */
export function overlayLiveTurnState(branchTurns: Turn[], liveTurns: Turn[]): Turn[] {
  const liveById = new Map(liveTurns.map((turn) => [turn.id, turn]));
  let changed = false;
  const overlaid = branchTurns.map((turn) => {
    const live = liveById.get(turn.id);
    if (
      !live ||
      (turn.status ?? null) === (live.status ?? null) &&
        (turn.statusReason ?? null) === (live.statusReason ?? null) &&
        (turn.contextStatus ?? null) === (live.contextStatus ?? null)
    ) {
      return turn;
    }
    changed = true;
    return {
      ...turn,
      status: live.status ?? null,
      statusReason: live.statusReason ?? null,
      contextStatus: live.contextStatus ?? null,
    };
  });
  return changed ? overlaid : branchTurns;
}

export function focusedBranchTurns(graph: ThreadGraph, agent: AgentInfo): Turn[] {
  const branchId = resolveFocusedBranchId(graph, agent);
  return turnsForBranch(graph, branchId, agent.id, agent.sessionId ?? null);
}

function turnsForBranch(
  graph: ThreadGraph,
  branchId: string,
  fallbackAgentId: string,
  fallbackSessionId: string | null,
): Turn[] {
  const branchSelection = focusedBranchSelection(graph, branchId);
  return Object.values(graph.nodes)
    .filter(
      (node): node is TurnNode =>
        node.kind === "turn" &&
        nodeMatchesBranchSelection(node, branchSelection),
    )
    .sort(compareThreadNodes)
    .map((node) => ({
      id: node.id,
      agentId: node.native?.agentId ?? node.participant.agentId ?? fallbackAgentId,
      sessionId: node.native?.sessionId ?? fallbackSessionId,
      role: node.turn.role,
      blocks: node.turn.blocks,
      sourceIndex: node.turn.sourceIndex ?? node.native?.sourceIndex ?? 0,
      timestamp: node.turn.timestamp ?? null,
      participant: node.participant,
      status: node.status === "active" ? null : node.status,
      statusReason: node.statusReason ?? null,
      contextStatus: node.contextStatus ?? null,
      nativeId: node.native?.nativeId ?? null,
      parentNativeId: node.native?.parentNativeId ?? null,
      nativeMessageId: node.native?.nativeMessageId ?? null,
    }));
}

function focusedBranchSelection(graph: ThreadGraph, branchId: string) {
  const selection = new Map<string, number | null>();
  let currentBranchId: string | null = branchId;
  let maxCreatedOrder: number | null = null;
  const visited = new Set<string>();

  while (currentBranchId && !visited.has(currentBranchId)) {
    visited.add(currentBranchId);
    selection.set(currentBranchId, maxCreatedOrder);
    const branch: ThreadBranch | undefined = graph.branches[currentBranchId];
    if (!branch) {
      break;
    }
    const baseTurnId = branch.baseTurnId ?? branch.createdFromTurnId ?? null;
    maxCreatedOrder =
      baseTurnId && graph.nodes[baseTurnId] ? graph.nodes[baseTurnId].createdOrder : null;
    currentBranchId = branch.parentBranchId ?? null;
  }

  return selection;
}

function nodeMatchesBranchSelection(node: ThreadNode, selection: Map<string, number | null>) {
  if (!selection.has(node.branchId)) {
    return false;
  }
  const maxCreatedOrder = selection.get(node.branchId);
  return maxCreatedOrder == null || node.createdOrder <= maxCreatedOrder;
}

function compareThreadNodes(left: ThreadNode, right: ThreadNode) {
  return left.createdOrder - right.createdOrder || left.id.localeCompare(right.id);
}

function participantForTurn(agent: AgentInfo, turn: Turn): ThreadParticipant {
  if (turn.role === "user") {
    return {
      kind: "user",
      actorId: "local-user",
      label: "You",
    };
  }
  return {
    kind: "assistant",
    actorId: agent.id,
    adapter: agent.adapter,
    agentId: agent.id,
    label: adapterLabel(agent.adapter),
  };
}

function adapterLabel(adapter: string) {
  switch (adapter) {
    case "claude":
      return "Claude";
    case "codex":
      return "Codex";
    case "opencode":
      return "OpenCode";
    case "grok":
      return "Grok";
    case "muse":
      return "Muse";
    case "pi":
      return "Pi";
    default:
      return "Agent";
  }
}
