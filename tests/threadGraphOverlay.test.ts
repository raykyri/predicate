import assert from "node:assert/strict";
import test from "node:test";
import {
  buildSingleAgentThreadGraph,
  focusedBranchTurns,
  pendingGraphOverlayTurns,
  threadGraphAnnotationsForTurns,
} from "../src/lib/threadGraph";
import { upsertThreadGraphs } from "../src/lib/appHelpers";
import type { AgentInfo, ThreadGraph, Turn } from "../src/types";

const AGENT: AgentInfo = {
  id: "agent-1",
  groupId: "group-1",
  adapter: "claude",
  worktreeDir: "/tmp",
  threadId: "thread-1",
  branchId: null,
  status: "running",
  createdAt: 1,
};

function makeTurn(sourceIndex: number, role = sourceIndex % 2 === 0 ? "user" : "assistant"): Turn {
  return {
    id: `${AGENT.id}-${sourceIndex}`,
    agentId: AGENT.id,
    role,
    blocks: [{ type: "text", text: `turn ${sourceIndex}` }],
    sourceIndex,
  };
}

function makeTurns(count: number, startIndex = 0): Turn[] {
  return Array.from({ length: count }, (_, offset) => makeTurn(startIndex + offset));
}

// Builds a stored graph over `turns` that carries the agent's real thread id,
// the shape the backend graph store produces for this agent.
function storedGraphFor(turns: Turn[]): ThreadGraph {
  return buildSingleAgentThreadGraph(AGENT, turns);
}

// Mirrors App's call shape: the overlay validates against the branch prefix
// the pane will actually render.
function overlay(graph: ThreadGraph, agent: AgentInfo, turns: Turn[]) {
  return pendingGraphOverlayTurns(graph, agent, focusedBranchTurns(graph, agent), turns);
}

test("graph containing every current turn has an empty pending overlay", () => {
  const turns = makeTurns(5);
  const graph = storedGraphFor(turns);
  assert.deepEqual(overlay(graph, AGENT, turns), []);
});

test("turns appended after the graph snapshot come back as the pending suffix", () => {
  const graphTurns = makeTurns(5);
  const graph = storedGraphFor(graphTurns);
  const appended = [makeTurn(5), makeTurn(6)];
  const pending = overlay(graph, AGENT, [...graphTurns, ...appended]);
  assert.deepEqual(pending, appended);
});

test("a history with no overlap with the graph rejects the overlay", () => {
  const graph = storedGraphFor(makeTurns(5));
  const otherHistory = makeTurns(3, 100);
  assert.equal(overlay(graph, AGENT, otherHistory), null);
});

test("missing turns interleaved with known ones reject the overlay", () => {
  const graphTurns = makeTurns(5);
  const graph = storedGraphFor(graphTurns);
  const rewritten = [graphTurns[0], makeTurn(100), graphTurns[1]];
  assert.equal(overlay(graph, AGENT, rewritten), null);
});

test("a graph without the agent's branch rejects the overlay", () => {
  const graph = storedGraphFor(makeTurns(3));
  const otherBranchAgent = { ...AGENT, branchId: "branch-elsewhere" };
  assert.equal(overlay(graph, otherBranchAgent, makeTurns(3)), null);
});

test("an empty live turn list keeps rendering the stored graph", () => {
  const graph = storedGraphFor(makeTurns(3));
  assert.deepEqual(overlay(graph, AGENT, []), []);
});

// Membership is branch-scoped: live turns that exist in the graph but on a
// branch other than the rendered one must reject the overlay (else another
// branch's suffix would be stitched onto the focused branch's history).
test("live turns from a non-focused branch reject the overlay", () => {
  const turns = makeTurns(3);
  const graph = storedGraphFor(turns);
  // Re-home the graph's nodes onto a side branch while focus stays on the
  // agent's branch (which keeps an empty turn set).
  const sideBranchId = "branch-side";
  graph.branches[sideBranchId] = {
    ...graph.branches[`branch-${AGENT.id}`],
    id: sideBranchId,
  };
  for (const node of Object.values(graph.nodes)) {
    node.branchId = sideBranchId;
  }
  const pending = overlay(graph, AGENT, [...turns, makeTurn(3)]);
  assert.equal(pending, null);
});

test("annotations follow visible sources, hide other branches, and retain deleted sources", () => {
  const turns = makeTurns(3);
  const graph = storedGraphFor(turns);
  const sideBranchSource = makeTurn(100, "assistant");
  const sideGraph = buildSingleAgentThreadGraph(
    { ...AGENT, branchId: "branch-side" },
    [sideBranchSource],
  );
  graph.branches["branch-side"] = sideGraph.branches["branch-side"];
  Object.assign(graph.nodes, sideGraph.nodes);
  graph.annotations = [
    { id: "visible", sourceTurnId: turns[1].id, text: "visible", createdAt: 1 },
    { id: "other-branch", sourceTurnId: sideBranchSource.id, text: "hidden", createdAt: 2 },
    { id: "deleted", sourceTurnId: "deleted-source", text: "orphan", createdAt: 3 },
  ];

  assert.deepEqual(
    threadGraphAnnotationsForTurns(graph, turns).map(({ id }) => id),
    ["visible", "deleted"],
  );
});

// The regression the overlay exists for: with a >200-turn history, an appended
// turn (not yet in the debounced graph snapshot) must extend the visible
// history, not collapse it to the capped fallback window.
test("append to a >200-turn history extends the visible window instead of collapsing it", () => {
  const CAP = 200;
  const fullHistory = makeTurns(250);
  const graph = storedGraphFor(fullHistory);
  const newTurn = makeTurn(250);
  // The frontend keeps only the newest CAP turns; the newest one is missing
  // from the (debounced) graph.
  const cappedTurns = [...fullHistory.slice(-(CAP - 1)), newTurn];

  const pending = overlay(graph, AGENT, cappedTurns);
  assert.ok(pending, "capped live window over a graph prefix must overlay, not fall back");
  assert.deepEqual(pending, [newTurn]);

  const visible = [...focusedBranchTurns(graph, AGENT), ...pending];
  assert.equal(visible.length, 251);
  assert.equal(visible[0].id, fullHistory[0].id);
  assert.equal(visible[visible.length - 1].id, newTurn.id);
});

test("upsertThreadGraphs keeps array identity when nothing changed", () => {
  const graphs = [storedGraphFor(makeTurns(2))];
  const refetched = JSON.parse(JSON.stringify(graphs[0])) as ThreadGraph;
  assert.equal(upsertThreadGraphs(graphs, [refetched]), graphs);
});

test("upsertThreadGraphs replaces only the changed graph and keeps the rest by identity", () => {
  const stale = storedGraphFor(makeTurns(2));
  const otherAgent = { ...AGENT, id: "agent-2", threadId: "thread-2" };
  const other = buildSingleAgentThreadGraph(otherAgent, [
    { ...makeTurn(0), id: "agent-2-0", agentId: "agent-2" },
  ]);
  const updated = storedGraphFor(makeTurns(3));
  const next = upsertThreadGraphs([stale, other], [updated]);
  assert.notEqual(next, undefined);
  assert.equal(next.length, 2);
  assert.equal(next[0], updated);
  assert.equal(next[1], other);
});

test("upsertThreadGraphs appends graphs for threads it has not seen", () => {
  const existing = storedGraphFor(makeTurns(1));
  const newAgent = { ...AGENT, id: "agent-3", threadId: "thread-3" };
  const fresh = buildSingleAgentThreadGraph(newAgent, []);
  const next = upsertThreadGraphs([existing], [fresh]);
  assert.equal(next.length, 2);
  assert.equal(next[0], existing);
  assert.equal(next[1], fresh);
});
