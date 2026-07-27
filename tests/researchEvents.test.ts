import assert from "node:assert/strict";
import test from "node:test";
import {
  addResearchNodeHighlight,
  parseResearchEvent,
  patchResearchDetailHighlightCreated,
  patchResearchDetailHighlightsRemoved,
  patchResearchDetailNode,
  patchResearchDetailTree,
  patchResearchSummaryForCreatedNode,
  patchResearchSummaryForNode,
  patchResearchSummaryForRemovedNodes,
  patchResearchSummaryTree,
  removeResearchDetailNodes,
  removeResearchNodeHighlights,
  researchNodeIsActivity,
  researchStatusContribution,
  researchSummaryFromDetail,
  removeResearchNodes,
  upsertResearchActivity,
  upsertResearchNode,
} from "../src/lib/researchEvents";
import type {
  QmuxEvent,
  ResearchHighlight,
  ResearchNode,
  ResearchTree,
  ResearchTreeDetail,
} from "../src/types";

const anchor = {
  version: 1 as const,
  projection: "answer-v1" as const,
  responseRevision: "revision-1",
  start: 2,
  end: 8,
  exact: "answer",
  prefix: "an ",
  suffix: " here",
};

const highlightA: ResearchHighlight = { id: "highlight-a", anchor, createdAt: 20 };
const highlightB: ResearchHighlight = {
  id: "highlight-b",
  anchor: { ...anchor, start: 12, end: 17, exact: "other" },
  createdAt: 21,
};

function tree(overrides: Partial<ResearchTree> = {}): ResearchTree {
  return {
    id: "tree-1",
    title: "Research",
    rootNodeId: "node-root",
    workspaceId: "workspace-1",
    createdAt: 10,
    updatedAt: 10,
    archivedAt: null,
    lastViewedAt: 10,
    ...overrides,
  };
}

function node(overrides: Partial<ResearchNode> = {}): ResearchNode {
  return {
    id: "node-root",
    treeId: "tree-1",
    parentNodeId: null,
    prompt: "Investigate",
    responsePreview: null,
    adapter: "codex",
    model: null,
    groupId: "workspace-1",
    worktreeDir: "/tmp/workspace",
    nativeSessionId: null,
    transcriptPath: null,
    promptNativeId: null,
    agentId: null,
    paneId: null,
    threadId: null,
    kind: "run",
    status: "queued",
    error: null,
    responseSnapshotAt: null,
    createdAt: 10,
    startedAt: null,
    completedAt: null,
    highlights: [],
    ...overrides,
  };
}

function detail(nodes: ResearchNode[], overrides: Partial<ResearchTree> = {}): ResearchTreeDetail {
  return { tree: tree(overrides), nodes };
}

function qmuxEvent(type: string, payload: Record<string, unknown>): QmuxEvent {
  return { type, payload, timestamp: 100 };
}

test("parseResearchEvent recognizes the complete backend research taxonomy", () => {
  const root = node();
  const researchTree = tree();
  const cases: Array<[string, Record<string, unknown>]> = [
    ["research.tree.created", { tree: researchTree, node: root }],
    [
      "research.document.updated",
      {
        tree: researchTree,
        node: root,
        responseRevision: "revision-2",
        markdownChanged: true,
        removedHighlightCount: 1,
      },
    ],
    ["research.node.created", { node: root }],
    ["research.node.updated", { node: root }],
    ["research.tree.updated", { tree: researchTree }],
    ["research.tree.archived", { tree: researchTree }],
    ["research.tree.restored", { tree: researchTree }],
    ["research.highlight.created", { nodeId: root.id, highlight: highlightA }],
    ["research.highlight.removed", { nodeId: root.id, highlightId: highlightA.id }],
    ["research.highlights.removed", { nodeId: root.id, highlightIds: [highlightA.id] }],
    ["research.tree.removed", { treeId: researchTree.id }],
    [
      "research.node.removed",
      { treeId: researchTree.id, parentNodeId: root.id, removedNodeIds: ["child-1"] },
    ],
  ];

  for (const [type, payload] of cases) {
    const parsed = parseResearchEvent(qmuxEvent(type, payload));
    assert.equal(parsed.kind, "event", type);
    if (parsed.kind === "event") {
      assert.equal(parsed.event.type, type);
      assert.equal(parsed.event.timestamp, 100);
    }
  }
});

test("parseResearchEvent separates unrelated, unsupported, and malformed events", () => {
  assert.deepEqual(parseResearchEvent(qmuxEvent("pane.created", {})), {
    kind: "notResearch",
  });
  assert.deepEqual(parseResearchEvent(qmuxEvent("research.future.changed", {})), {
    kind: "unsupported",
    type: "research.future.changed",
  });
  assert.deepEqual(parseResearchEvent(qmuxEvent("research.node.updated", { node: {} })), {
    kind: "malformed",
    type: "research.node.updated",
  });
  assert.deepEqual(
    parseResearchEvent(
      qmuxEvent("research.highlights.removed", {
        nodeId: "node-root",
        highlightIds: ["valid", 3],
      }),
    ),
    { kind: "malformed", type: "research.highlights.removed" },
  );
});

test("researchSummaryFromDetail exactly derives counts, kind, and unseen attention", () => {
  const nodes = [
    node({ kind: "document", status: "complete", completedAt: 10 }),
    node({ id: "running", parentNodeId: "node-root", status: "running", createdAt: 11 }),
    node({ id: "failed-old", status: "failed", completedAt: 9, createdAt: 12 }),
    node({ id: "failed-new", status: "failed", completedAt: 14, createdAt: 13 }),
    node({ id: "cancelled", status: "cancelled", completedAt: 13, createdAt: 14 }),
  ];
  assert.deepEqual(researchSummaryFromDetail(detail(nodes)), {
    id: "tree-1",
    title: "Research",
    rootNodeId: "node-root",
    kind: "document",
    workspaceId: "workspace-1",
    runningCount: 1,
    failedCount: 2,
    completedCount: 1,
    cancelledCount: 1,
    updatedAt: 10,
    archivedAt: null,
    hasUnseenUpdate: true,
    hasUnseenFailure: true,
  });

  const viewed = researchSummaryFromDetail(detail(nodes, { lastViewedAt: 14 }));
  assert.equal(viewed.hasUnseenUpdate, false);
  assert.equal(viewed.hasUnseenFailure, false);

  // The root's origin is surfaced (and the key omitted entirely when absent,
  // mirroring the backend's skip-if-none serialization).
  const imported = researchSummaryFromDetail(
    detail([
      node({ kind: "conversation", origin: "conversationImport", status: "complete" }),
    ]),
  );
  assert.equal(imported.origin, "conversationImport");
  assert.ok(!("origin" in researchSummaryFromDetail(detail(nodes))));
});

test("research status contributions match backend active and terminal buckets", () => {
  assert.deepEqual(researchStatusContribution("queued"), {
    runningCount: 1,
    failedCount: 0,
    completedCount: 0,
    cancelledCount: 0,
  });
  assert.deepEqual(researchStatusContribution("starting"), researchStatusContribution("queued"));
  assert.deepEqual(researchStatusContribution("running"), researchStatusContribution("queued"));
  assert.equal(researchStatusContribution("complete").completedCount, 1);
  assert.equal(researchStatusContribution("failed").failedCount, 1);
  assert.equal(researchStatusContribution("cancelled").cancelledCount, 1);
});

test("node collection helpers upsert in backend order and remove without no-op churn", () => {
  const later = node({ id: "node-z", createdAt: 30 });
  const earlier = node({ id: "node-a", createdAt: 20 });
  const current = [later];
  const inserted = upsertResearchNode(current, earlier);
  assert.deepEqual(inserted.map((entry) => entry.id), ["node-a", "node-z"]);
  assert.equal(upsertResearchNode(inserted, earlier), inserted);

  const updated = { ...earlier, responsePreview: "Live preview" };
  const replaced = upsertResearchNode(inserted, updated);
  assert.equal(replaced[0], updated);
  assert.equal(replaced[1], later);
  assert.equal(removeResearchNodes(replaced, ["missing"]), replaced);
  assert.deepEqual(removeResearchNodes(replaced, new Set(["node-a"])), [later]);
});

test("activity helper retains terminal nodes while their pane is bound, then removes them", () => {
  const active = node({ paneId: "pane-1", status: "running" });
  assert.equal(researchNodeIsActivity(active), true);
  const activity = upsertResearchActivity([], active);

  const settledBound = { ...active, status: "complete" as const, completedAt: 30 };
  const settledActivity = upsertResearchActivity(activity, settledBound);
  assert.equal(settledActivity.length, 1);
  assert.equal(settledActivity[0], settledBound);

  const detached = { ...settledBound, paneId: null };
  assert.equal(researchNodeIsActivity(detached), false);
  assert.deepEqual(upsertResearchActivity(settledActivity, detached), []);
});

test("detail node/tree/remove helpers touch only their matching tree", () => {
  const root = node();
  const current = detail([root]);
  const updatedRoot = { ...root, status: "running" as const };
  const withNode = patchResearchDetailNode(current, updatedRoot);
  assert.notEqual(withNode, current);
  assert.equal(withNode?.nodes[0], updatedRoot);
  assert.equal(patchResearchDetailNode(current, node({ treeId: "other" })), current);

  const renamed = tree({ title: "Renamed", updatedAt: 20 });
  const withTree = patchResearchDetailTree(withNode, renamed);
  assert.equal(withTree?.tree, renamed);
  assert.equal(patchResearchDetailTree(current, tree({ id: "other" })), current);

  assert.equal(removeResearchDetailNodes(current, "other", [root.id]), current);
  assert.deepEqual(removeResearchDetailNodes(current, current.tree.id, [root.id])?.nodes, []);
  assert.equal(removeResearchDetailNodes(current, current.tree.id, ["missing"]), current);
});

test("highlight helpers are idempotent and preserve unrelated highlights", () => {
  const root = node({ highlights: [highlightA] });
  assert.equal(addResearchNodeHighlight(root, highlightA), root);
  const added = addResearchNodeHighlight(root, highlightB);
  assert.deepEqual(added.highlights, [highlightA, highlightB]);
  assert.equal(removeResearchNodeHighlights(added, ["missing"]), added);
  assert.deepEqual(removeResearchNodeHighlights(added, [highlightA.id]).highlights, [highlightB]);

  const current = detail([root]);
  const withHighlight = patchResearchDetailHighlightCreated(current, root.id, highlightB);
  assert.deepEqual(withHighlight?.nodes[0].highlights, [highlightA, highlightB]);
  assert.equal(
    patchResearchDetailHighlightCreated(withHighlight, root.id, highlightB),
    withHighlight,
  );
  const withoutHighlights = patchResearchDetailHighlightsRemoved(withHighlight, root.id, [
    highlightA.id,
    highlightB.id,
  ]);
  assert.deepEqual(withoutHighlights?.nodes[0].highlights, []);
  assert.equal(
    patchResearchDetailHighlightsRemoved(withoutHighlights, "unknown", [highlightA.id]),
    withoutHighlights,
  );
});

test("summary node patch handles active transitions without changing bucket totals", () => {
  const queued = node({ status: "queued" });
  const summary = researchSummaryFromDetail(detail([queued]));
  const running = { ...queued, status: "running" as const, startedAt: 50 };
  const patched = patchResearchSummaryForNode(summary, queued, running, 51);
  assert.equal(patched.runningCount, 1);
  assert.equal(patched.completedCount, 0);
  assert.equal(patched.updatedAt, 51);
  assert.equal(patched.hasUnseenUpdate, false);
});

test("summary node patch moves counts and lights attention only on a new settlement", () => {
  const running = node({ status: "running", startedAt: 20 });
  const summary = researchSummaryFromDetail(detail([running]));
  const failed = {
    ...running,
    status: "failed" as const,
    error: "boom",
    completedAt: 60,
  };
  const settled = patchResearchSummaryForNode(summary, running, failed, 61);
  assert.equal(settled.runningCount, 0);
  assert.equal(settled.failedCount, 1);
  assert.equal(settled.hasUnseenUpdate, true);
  assert.equal(settled.hasUnseenFailure, true);

  const metadataOnly = { ...failed, responseSnapshotAt: 70 };
  assert.equal(patchResearchSummaryForNode(settled, failed, metadataOnly, 70), settled);
});

test("summary node patch clamps inconsistent counts and ignores mismatched records", () => {
  const running = node({ status: "running" });
  const summary = { ...researchSummaryFromDetail(detail([running])), runningCount: 0 };
  const complete = { ...running, status: "complete" as const, completedAt: 40 };
  const patched = patchResearchSummaryForNode(summary, running, complete, 40);
  assert.equal(patched.runningCount, 0);
  assert.equal(patched.completedCount, 1);
  assert.equal(
    patchResearchSummaryForNode(summary, node({ id: "different" }), complete, 40),
    summary,
  );
});

test("summary structural patches add and remove cached status contributions", () => {
  const root = node({ status: "complete", completedAt: 20 });
  const summary = researchSummaryFromDetail(detail([root]));
  const child = node({
    id: "child",
    parentNodeId: root.id,
    status: "queued",
    createdAt: 30,
  });
  const added = patchResearchSummaryForCreatedNode(summary, child, 31);
  assert.equal(added.runningCount, 1);
  assert.equal(added.completedCount, 1);
  assert.equal(added.updatedAt, 31);
  assert.equal(
    patchResearchSummaryForCreatedNode(summary, node({ treeId: "other" }), 31),
    summary,
  );

  const failed = node({
    id: "failed-child",
    parentNodeId: root.id,
    status: "failed",
    completedAt: 35,
    createdAt: 35,
  });
  const withFailed = patchResearchSummaryForCreatedNode(added, failed, 36);
  const removed = patchResearchSummaryForRemovedNodes(
    withFailed,
    "tree-1",
    [child, failed],
    40,
  );
  assert.equal(removed.runningCount, 0);
  assert.equal(removed.failedCount, 0);
  assert.equal(removed.completedCount, 1);
  assert.equal(removed.updatedAt, 40);
  assert.equal(
    patchResearchSummaryForRemovedNodes(summary, "other", [root], 40),
    summary,
  );
});

test("tree summary patch adopts authoritative metadata and preserves derived fields", () => {
  const summary = researchSummaryFromDetail(
    detail([node({ status: "failed", completedAt: 30 })], { lastViewedAt: 10 }),
  );
  const archived = tree({ title: "Renamed", archivedAt: 50, updatedAt: 45 });
  const patched = patchResearchSummaryTree(summary, archived);
  assert.equal(patched.title, "Renamed");
  assert.equal(patched.archivedAt, 50);
  assert.equal(patched.updatedAt, 45);
  assert.equal(patched.failedCount, summary.failedCount);
  assert.equal(patched.hasUnseenFailure, summary.hasUnseenFailure);
  assert.equal(patchResearchSummaryTree(patched, archived), patched);
});
