import assert from "node:assert/strict";
import test from "node:test";
import {
  isResearchNodeSelectionChange,
  isResearchTreeSelectionChange,
  recordResearchFollowupDraft,
  recordResearchScrollPosition,
  RESEARCH_SCROLL_POSITION_TTL_MS,
  restoreResearchScrollPosition,
  type SavedResearchNavigation,
} from "../src/lib/researchNavigation";
import { researchBranchInfo } from "../src/lib/researchBranches";
import {
  moveResearchTreeIdBy,
  moveResearchTreeIdToGap,
  replaceResearchTreeScopeOrder,
} from "../src/lib/researchOrder";
import {
  intersectingResearchHighlightIds,
  isResearchHighlightActionShortcut,
  resolveResearchHighlightOffset,
} from "../src/lib/researchHighlights";
import {
  clearResearchTreeAttention,
  reconcileResearchActivity,
  reconcileResearchTreeDetail,
  reconcileResearchTreeSummaries,
} from "../src/lib/researchSnapshots";
import type {
  ResearchHighlight,
  ResearchNode,
  ResearchNodeStatus,
  ResearchTreeDetail,
  ResearchTreeSummary,
} from "../src/types";

function node(
  id: string,
  parentNodeId: string | null,
  status: ResearchNodeStatus = "complete",
): ResearchNode {
  return {
    id,
    treeId: "tree",
    parentNodeId,
    prompt: id,
    adapter: "claude",
    groupId: "group",
    worktreeDir: "/tmp",
    status,
    createdAt: 1,
    highlights: [],
  };
}

function highlight(overrides: Partial<ResearchHighlight["anchor"]> = {}): ResearchHighlight {
  return {
    id: "highlight-1",
    createdAt: 1,
    anchor: {
      version: 1,
      projection: "answer-v1",
      responseRevision: "a".repeat(64),
      start: 7,
      end: 13,
      exact: "target",
      prefix: "before ",
      suffix: " after",
      ...overrides,
    },
  };
}

function tree(id: string, workspaceId: string): ResearchTreeSummary {
  return {
    id,
    title: id,
    rootNodeId: `${id}-root`,
    kind: "run",
    workspaceId,
    runningCount: 0,
    failedCount: 0,
    completedCount: 1,
    cancelledCount: 0,
    updatedAt: 1,
    hasUnseenUpdate: false,
    hasUnseenFailure: false,
  };
}

test("research pointer gaps reorder without off-by-one moves", () => {
  const ids = ["a", "b", "c", "d"];
  assert.deepEqual(moveResearchTreeIdToGap(ids, "b", 4), ["a", "c", "d", "b"]);
  assert.deepEqual(moveResearchTreeIdToGap(ids, "d", 1), ["a", "d", "b", "c"]);
  assert.equal(moveResearchTreeIdToGap(ids, "b", 1), ids);
  assert.equal(moveResearchTreeIdToGap(ids, "b", 2), ids);
});

test("research keyboard moves stop at section boundaries", () => {
  const ids = ["a", "b", "c"];
  assert.deepEqual(moveResearchTreeIdBy(ids, "b", -1), ["b", "a", "c"]);
  assert.deepEqual(moveResearchTreeIdBy(ids, "b", 1), ["a", "c", "b"]);
  assert.equal(moveResearchTreeIdBy(ids, "a", -1), ids);
  assert.equal(moveResearchTreeIdBy(ids, "c", 1), ids);
});

test("research reorder replaces only the selected folder subsequence", () => {
  const trees = [tree("a", "one"), tree("x", "two"), tree("b", "one")];
  assert.deepEqual(
    replaceResearchTreeScopeOrder(trees, "one", ["b", "a"]).map((item) => item.id),
    ["b", "x", "a"],
  );
  assert.equal(replaceResearchTreeScopeOrder(trees, "one", ["a"]), trees);
});

test("clicking the currently selected research breadcrumb is a no-op", () => {
  assert.equal(isResearchNodeSelectionChange("root-node", "root-node"), false);
});

test("clicking a different research breadcrumb changes the selection", () => {
  assert.equal(isResearchNodeSelectionChange("child-node", "root-node"), true);
  assert.equal(isResearchNodeSelectionChange(null, "root-node"), true);
});

test("clicking the research tree already displayed is a no-op", () => {
  assert.equal(isResearchTreeSelectionChange("tree", true, "tree"), false);
});

test("a research tree remains selectable when its document is not displayed", () => {
  assert.equal(isResearchTreeSelectionChange("tree", false, "tree"), true);
  assert.equal(isResearchTreeSelectionChange("tree", true, "other-tree"), true);
});

test("research scroll positions remain available for 15 minutes", () => {
  const navigation: SavedResearchNavigation = { scrollByNode: {} };
  recordResearchScrollPosition(navigation, "root-node", 480, 1_000);

  assert.equal(
    restoreResearchScrollPosition(
      navigation,
      "root-node",
      1_000 + RESEARCH_SCROLL_POSITION_TTL_MS - 1,
    ),
    480,
  );
});

test("research scroll positions expire at 15 minutes", () => {
  const navigation: SavedResearchNavigation = { scrollByNode: {} };
  recordResearchScrollPosition(navigation, "root-node", 480, 1_000);

  assert.equal(
    restoreResearchScrollPosition(
      navigation,
      "root-node",
      1_000 + RESEARCH_SCROLL_POSITION_TTL_MS,
    ),
    0,
  );
});

test("research follow-up drafts retain text and composer mode until cleared", () => {
  const navigation: SavedResearchNavigation = { scrollByNode: {} };

  assert.equal(
    recordResearchFollowupDraft(navigation, "compare the two approaches", "branch", 1_000),
    true,
  );
  assert.deepEqual(navigation.followupDraft, {
    text: "compare the two approaches",
    mode: "branch",
    updatedAt: 1_000,
  });
  assert.equal(recordResearchFollowupDraft(navigation, "", "thread", 2_000), true);
  assert.equal(navigation.followupDraft, undefined);
  assert.equal(recordResearchFollowupDraft(navigation, "", "thread", 3_000), false);
});

test("branch info includes every descendant but not siblings", () => {
  const nodes = [
    node("root", null),
    node("branch", "root"),
    node("child", "branch"),
    node("leaf", "child"),
    node("sibling", "root"),
  ];
  assert.deepEqual(researchBranchInfo(nodes, "branch"), {
    nodeIds: ["branch", "child", "leaf"],
    descendantCount: 2,
    hasActiveRuns: false,
  });
});

test("branch info detects active descendants and live panes", () => {
  const running = [node("branch", "root"), node("child", "branch", "running")];
  assert.equal(researchBranchInfo(running, "branch")?.hasActiveRuns, true);
  const pane = node("child", "branch");
  pane.paneId = "pane";
  assert.equal(researchBranchInfo([node("branch", "root"), pane], "branch")?.hasActiveRuns, true);
  assert.equal(researchBranchInfo(running, "missing"), null);
});

test("research highlights relocate only with matching quote context", () => {
  const saved = highlight();
  assert.deepEqual(
    resolveResearchHighlightOffset("inserted before target after", "a".repeat(64), saved),
    { start: 16, end: 22 },
  );
  assert.equal(
    resolveResearchHighlightOffset("an unrelated target elsewhere", "a".repeat(64), saved),
    null,
  );
});

test("research highlights survive edits by relocating across revisions", () => {
  // A document edit bumps the revision; the highlight follows its quote as
  // long as the surrounding context still agrees.
  assert.deepEqual(
    resolveResearchHighlightOffset(
      "edited opening. before target after",
      "b".repeat(64),
      highlight(),
    ),
    { start: 23, end: 29 },
  );
  // The quote is gone from the new revision: orphan it rather than guess.
  assert.equal(
    resolveResearchHighlightOffset("nothing to match here", "b".repeat(64), highlight()),
    null,
  );
});

test("research highlights relocate to a single one-sided match when context is edited", () => {
  // An edit rewrote the suffix, so both sides no longer agree; a lone
  // occurrence keeping the prefix is still safe to follow.
  assert.deepEqual(
    resolveResearchHighlightOffset("before target rewritten", "b".repeat(64), highlight()),
    { start: 7, end: 13 },
  );
  // Two occurrences each keep only one side: too ambiguous, so orphan it.
  assert.equal(
    resolveResearchHighlightOffset(
      "before target here and target after",
      "b".repeat(64),
      highlight(),
    ),
    null,
  );
});

test("research highlight removal targets every whole highlight touched by a selection", () => {
  const highlights = [
    { id: "first", start: 2, end: 8 },
    { id: "second", start: 10, end: 20 },
    { id: "third", start: 24, end: 30 },
  ];

  assert.deepEqual(
    intersectingResearchHighlightIds({ start: 5, end: 12 }, highlights),
    ["first", "second"],
  );
  assert.deepEqual(
    intersectingResearchHighlightIds({ start: 12, end: 14 }, highlights),
    ["second"],
  );
});

test("research highlight removal ignores ranges that only touch an edge", () => {
  const highlights = [{ id: "highlight", start: 10, end: 20 }];

  assert.deepEqual(
    intersectingResearchHighlightIds({ start: 5, end: 10 }, highlights),
    [],
  );
  assert.deepEqual(
    intersectingResearchHighlightIds({ start: 20, end: 25 }, highlights),
    [],
  );
});

test("H confirms a research highlight action without modifiers or repeat", () => {
  const input = {
    key: "h",
    defaultPrevented: false,
    repeat: false,
    metaKey: false,
    ctrlKey: false,
    altKey: false,
  };

  assert.equal(isResearchHighlightActionShortcut(input), true);
  assert.equal(isResearchHighlightActionShortcut({ ...input, key: "H" }), true);
  assert.equal(isResearchHighlightActionShortcut({ ...input, key: "a" }), false);
  assert.equal(isResearchHighlightActionShortcut({ ...input, repeat: true }), false);
  assert.equal(isResearchHighlightActionShortcut({ ...input, metaKey: true }), false);
  assert.equal(isResearchHighlightActionShortcut({ ...input, ctrlKey: true }), false);
  assert.equal(isResearchHighlightActionShortcut({ ...input, altKey: true }), false);
  assert.equal(isResearchHighlightActionShortcut({ ...input, defaultPrevented: true }), false);
});

test("research snapshot reconciliation retains identical collection identities", () => {
  const summaries = [tree("first", "workspace"), tree("second", "workspace")];
  const nextSummaries = summaries.map((summary) => ({ ...summary }));
  assert.equal(reconcileResearchTreeSummaries(summaries, nextSummaries), summaries);

  const activity = [node("running", null, "running")];
  const nextActivity = activity.map((entry) => ({ ...entry, highlights: [] }));
  assert.equal(reconcileResearchActivity(activity, nextActivity), activity);
});

test("research snapshot reconciliation retains empty collection identities", () => {
  const summaries: ResearchTreeSummary[] = [];
  const activity: ResearchNode[] = [];
  assert.equal(reconcileResearchTreeSummaries(summaries, []), summaries);
  assert.equal(reconcileResearchActivity(activity, []), activity);

  const detail: ResearchTreeDetail = {
    tree: {
      id: "tree",
      title: "Tree",
      rootNodeId: "root",
      workspaceId: "workspace",
      createdAt: 1,
      updatedAt: 2,
    },
    nodes: [],
  };
  assert.equal(
    reconcileResearchTreeDetail(detail, { tree: { ...detail.tree }, nodes: [] }),
    detail,
  );
});

test("research snapshot reconciliation replaces only changed collection records", () => {
  const first = tree("first", "workspace");
  const second = tree("second", "workspace");
  const current = [first, second];
  const reconciled = reconcileResearchTreeSummaries(current, [
    { ...first },
    { ...second, runningCount: 1 },
  ]);

  assert.notEqual(reconciled, current);
  assert.equal(reconciled[0], first);
  assert.notEqual(reconciled[1], second);
  assert.equal(reconciled[1].runningCount, 1);
});

test("research detail reconciliation retains identical nested snapshots", () => {
  const detail: ResearchTreeDetail = {
    tree: {
      id: "tree",
      title: "Tree",
      rootNodeId: "root",
      workspaceId: "workspace",
      createdAt: 1,
      updatedAt: 2,
    },
    nodes: [node("root", null), node("child", "root")],
  };
  const incoming: ResearchTreeDetail = {
    tree: { ...detail.tree, archivedAt: undefined },
    nodes: detail.nodes.map((entry) => ({ ...entry, highlights: [...entry.highlights] })),
  };

  assert.equal(reconcileResearchTreeDetail(detail, incoming), detail);
});

test("research detail reconciliation preserves unchanged node identities", () => {
  const root = node("root", null);
  const child = node("child", "root");
  const detail: ResearchTreeDetail = {
    tree: {
      id: "tree",
      title: "Tree",
      rootNodeId: "root",
      workspaceId: "workspace",
      createdAt: 1,
      updatedAt: 2,
    },
    nodes: [root, child],
  };
  const reconciled = reconcileResearchTreeDetail(detail, {
    tree: { ...detail.tree },
    nodes: [{ ...root }, { ...child, responsePreview: "New preview" }],
  });

  assert.notEqual(reconciled, detail);
  assert.equal(reconciled.tree, detail.tree);
  assert.equal(reconciled.nodes[0], root);
  assert.notEqual(reconciled.nodes[1], child);
});

test("clearing research attention is an identity-preserving no-op when already viewed", () => {
  const viewed = [tree("viewed", "workspace")];
  assert.equal(clearResearchTreeAttention(viewed, "viewed"), viewed);

  const unseen = [{ ...viewed[0], hasUnseenUpdate: true, hasUnseenFailure: true }];
  const cleared = clearResearchTreeAttention(unseen, "viewed");
  assert.notEqual(cleared, unseen);
  assert.deepEqual(cleared[0], {
    ...unseen[0],
    hasUnseenUpdate: false,
    hasUnseenFailure: false,
  });
});
