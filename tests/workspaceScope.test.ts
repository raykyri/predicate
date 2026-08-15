import test from "node:test";
import assert from "node:assert/strict";
import type { GroupInfo, PaneInfo, ResearchTreeSummary } from "../src/types";
import { cycleTabId } from "../src/lib/appHelpers";
import {
  groupsForScope,
  panesForScope,
  researchAttention,
  replaceScopedGroupOrder,
} from "../src/lib/workspaceScope";
import {
  parseSidebarMode,
  researchCycleTabIds,
  researchTreeIdFromTabId,
  researchTreeTabId,
  terminalTabForMode,
} from "../src/lib/sidebarMode";
import {
  nextTreeInResearchScope,
  resolveResearchScope,
  treeForResearchScope,
  treesForResearchScope,
  workspaceIsInResearchScope,
} from "../src/lib/researchScope";

function group(id: string, scope: GroupInfo["scope"]): GroupInfo {
  return {
    id,
    name: id,
    dir: `/tmp/${id}`,
    managedDir: `/tmp/qmux/${id}`,
    createdAt: 1,
    collapsed: false,
    scope,
    agents: [],
  };
}

function pane(id: string, groupId: string): PaneInfo {
  return {
    id,
    title: id,
    kind: "shell",
    groupId,
    cwd: "/tmp",
    cols: 80,
    rows: 24,
    status: "running",
  };
}

test("scope selectors partition groups and panes without relying on research node bindings", () => {
  const terminalA = group("terminal-a", "terminal");
  const research = group("research-a", "research");
  const terminalB = group("terminal-b", "terminal");
  const groups = [terminalA, research, terminalB];
  const panes = [
    pane("research-pane", research.id),
    pane("terminal-b-pane", terminalB.id),
    pane("terminal-a-pane", terminalA.id),
  ];

  assert.deepEqual(groupsForScope(groups, "terminal").map(({ id }) => id), [
    terminalA.id,
    terminalB.id,
  ]);
  assert.deepEqual(panesForScope(panes, groups, "terminal").map(({ id }) => id), [
    "terminal-a-pane",
    "terminal-b-pane",
  ]);
  assert.deepEqual(panesForScope(panes, groups, "research").map(({ id }) => id), [
    "research-pane",
  ]);
});

test("a pane with missing group metadata remains recoverable in Terminal scope", () => {
  const orphan = pane("orphan", "missing-group");

  assert.deepEqual(panesForScope([orphan], [], "terminal"), [orphan]);
  assert.deepEqual(panesForScope([orphan], [], "research"), []);
});

test("research attention is derived from durable summaries", () => {
  assert.deepEqual(
    researchAttention([
      {
        id: "tree-a",
        title: "A",
        rootNodeId: "root-a",
        kind: "run",
        workspaceId: "folder-a",
        runningCount: 2,
        failedCount: 0,
        completedCount: 1,
        cancelledCount: 0,
        updatedAt: 1,
        hasUnseenUpdate: true,
        hasUnseenFailure: false,
      },
      {
        id: "tree-b",
        title: "B",
        rootNodeId: "root-b",
        kind: "run",
        workspaceId: "folder-b",
        runningCount: 0,
        failedCount: 1,
        completedCount: 0,
        cancelledCount: 1,
        updatedAt: 2,
        hasUnseenUpdate: false,
        hasUnseenFailure: true,
      },
      {
        // An old failure the user already viewed: counted in the lifetime
        // failedCount but no longer demanding attention.
        id: "tree-c",
        title: "C",
        rootNodeId: "root-c",
        kind: "run",
        workspaceId: "folder-a",
        runningCount: 0,
        failedCount: 3,
        completedCount: 2,
        cancelledCount: 0,
        updatedAt: 3,
        hasUnseenUpdate: false,
        hasUnseenFailure: false,
      },
    ]),
    { runningCount: 2, failedCount: 1, unseenCount: 1 },
  );
});

test("reordering Terminal groups preserves Research slots", () => {
  const terminalA = group("terminal-a", "terminal");
  const researchA = group("research-a", "research");
  const terminalB = group("terminal-b", "terminal");
  const researchB = group("research-b", "research");

  assert.deepEqual(
    replaceScopedGroupOrder(
      [terminalA, researchA, terminalB, researchB],
      "terminal",
      [terminalB, terminalA],
    ).map(({ id }) => id),
    [terminalB.id, researchA.id, terminalA.id, researchB.id],
  );
});

test("a stale, duplicate, or cross-scope reorder is ignored", () => {
  const terminalA = group("terminal-a", "terminal");
  const research = group("research-a", "research");
  const terminalB = group("terminal-b", "terminal");
  const groups = [terminalA, research, terminalB];

  assert.equal(
    replaceScopedGroupOrder(groups, "terminal", [terminalA, terminalA]),
    groups,
  );
  assert.equal(replaceScopedGroupOrder(groups, "terminal", [terminalA]), groups);
  assert.equal(
    replaceScopedGroupOrder(groups, "terminal", [terminalA, research]),
    groups,
  );
});

test("sidebar mode parsing safely defaults old or corrupt preferences to Terminal", () => {
  assert.equal(parseSidebarMode("research"), "research");
  assert.equal(parseSidebarMode("terminal"), "terminal");
  assert.equal(parseSidebarMode("other"), "terminal");
  assert.equal(parseSidebarMode(null), "terminal");
});

test("switching to Terminal restores a valid prior tab without crossing scope", () => {
  const terminalA = group("terminal-a", "terminal");
  const terminalB = group("terminal-b", "terminal");
  const research = group("research-a", "research");
  const groups = [terminalA, research, terminalB];
  const panes = [
    pane("research-pane", research.id),
    pane("terminal-a-pane", terminalA.id),
    pane("terminal-b-pane", terminalB.id),
  ];

  assert.equal(
    terminalTabForMode(panes, groups, "terminal-b-pane"),
    "terminal-b-pane",
  );
  assert.equal(
    terminalTabForMode(panes, groups, "research-pane"),
    "terminal-a-pane",
  );
  assert.equal(terminalTabForMode(panes, groups, "home"), "terminal-a-pane");
  assert.equal(terminalTabForMode([], groups, "missing"), null);
});

test("switching to Terminal prefers a pane whose group is expanded", () => {
  const terminalA = group("terminal-a", "terminal");
  const terminalB = group("terminal-b", "terminal");
  terminalA.collapsed = true;
  const groups = [terminalA, terminalB];
  const panes = [
    pane("terminal-a-pane", terminalA.id),
    pane("terminal-b-pane", terminalB.id),
  ];

  // The stale-preference fallback must not activate a tab the sidebar hides.
  assert.equal(terminalTabForMode(panes, groups, "missing"), "terminal-b-pane");

  // Every group collapsed: any pane beats an empty Terminal stage.
  terminalB.collapsed = true;
  assert.equal(terminalTabForMode(panes, groups, "missing"), "terminal-a-pane");
});

test("research cycling stays on the document when no research terminals are visible", () => {
  const research = group("research", "research");
  const groups = [research];
  const treeTabId = researchTreeTabId("tree");
  const ids = researchCycleTabIds(
    [],
    groups,
    [treeSummary("tree", research.id)],
    research.id,
  );

  assert.deepEqual(ids, [treeTabId]);
  assert.equal(cycleTabId(ids, treeTabId, 1), treeTabId);
  assert.equal(cycleTabId(ids, treeTabId, -1), treeTabId);
});

test("research cycling wraps between documents and visible research terminals", () => {
  const terminal = group("terminal", "terminal");
  const researchA = group("research-a", "research");
  const researchB = group("research-b", "research");
  researchB.collapsed = true;
  const ids = researchCycleTabIds(
    [
      pane("terminal-pane", terminal.id),
      pane("research-one", researchA.id),
      pane("research-collapsed", researchB.id),
      pane("research-two", researchA.id),
    ],
    [terminal, researchA, researchB],
    [treeSummary("tree-one", researchA.id), treeSummary("tree-two", researchA.id)],
    researchA.id,
  );

  const treeOneTabId = researchTreeTabId("tree-one");
  const treeTwoTabId = researchTreeTabId("tree-two");
  assert.deepEqual(ids, [treeOneTabId, treeTwoTabId, "research-one", "research-two"]);
  assert.equal(cycleTabId(ids, treeOneTabId, 1), treeTwoTabId);
  assert.equal(cycleTabId(ids, treeTwoTabId, 1), "research-one");
  assert.equal(cycleTabId(ids, "research-one", 1), "research-two");
  assert.equal(cycleTabId(ids, "research-two", 1), treeOneTabId);
  assert.equal(cycleTabId(ids, treeOneTabId, -1), "research-two");
  assert.equal(cycleTabId(ids, "research-one", -1), treeTwoTabId);
  assert.equal(researchTreeIdFromTabId(treeTwoTabId), "tree-two");
  assert.equal(researchTreeIdFromTabId("research-one"), null);
});

test("research cycling honours the folder scope the sidebar is filtered to", () => {
  const researchA = group("research-a", "research");
  const researchB = group("research-b", "research");
  const panes = [
    pane("pane-a", researchA.id),
    pane("pane-b", researchB.id),
  ];

  // Scoped to A: B's live terminal has no sidebar row, so it must not be
  // reachable by cycling either.
  assert.deepEqual(
    researchCycleTabIds(
      panes,
      [researchA, researchB],
      [treeSummary("tree-a", researchA.id), treeSummary("tree-b", researchB.id)],
      researchA.id,
    ),
    [researchTreeTabId("tree-a"), "pane-a"],
  );
  assert.deepEqual(
    researchCycleTabIds(
      panes,
      [researchA, researchB],
      [treeSummary("tree-a", researchA.id), treeSummary("tree-b", researchB.id)],
      researchB.id,
    ),
    [researchTreeTabId("tree-b"), "pane-b"],
  );
});

test("a stored folder scope resolves to itself only while the workspace is live", () => {
  const researchA = group("research-a", "research");
  const researchB = group("research-b", "research");

  assert.equal(resolveResearchScope("research-a", [researchA, researchB]), "research-a");
  assert.equal(resolveResearchScope("research-a", [researchB]), "research-b");
  assert.equal(resolveResearchScope("research-a", []), null);
  assert.equal(resolveResearchScope(null, [researchA]), "research-a");
  assert.equal(resolveResearchScope("all", [researchA]), "research-a");
});

function treeSummary(id: string, workspaceId: string): ResearchTreeSummary {
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

test("folder scoping filters trees and returns none without a folder", () => {
  const trees = [
    treeSummary("tree-1", "research-a"),
    treeSummary("tree-2", "research-b"),
    treeSummary("tree-3", "research-a"),
  ];

  assert.deepEqual(treesForResearchScope(trees, null), []);
  assert.deepEqual(
    treesForResearchScope(trees, "research-a").map(({ id }) => id),
    ["tree-1", "tree-3"],
  );
  assert.deepEqual(treesForResearchScope(trees, "research-c"), []);
});

test("research restoration never crosses the selected folder scope", () => {
  const trees = [
    treeSummary("tree-1", "research-a"),
    treeSummary("tree-2", "research-b"),
  ];

  assert.equal(treeForResearchScope(trees, "research-b", "tree-1")?.id, "tree-2");
  assert.equal(treeForResearchScope(trees, "research-c", "tree-1"), null);
  assert.equal(treeForResearchScope(trees, null, "tree-1"), null);
  assert.equal(workspaceIsInResearchScope("research-a", "research-a"), true);
  assert.equal(workspaceIsInResearchScope("research-a", "research-b"), false);
  assert.equal(workspaceIsInResearchScope("research-a", null), false);
});

test("the next-tree fallback stays inside the scoped folder", () => {
  const trees = [
    treeSummary("tree-1", "research-a"),
    treeSummary("tree-2", "research-b"),
    treeSummary("tree-3", "research-a"),
  ];

  assert.equal(nextTreeInResearchScope(trees, "research-a", "tree-1")?.id, "tree-3");
  // The only tree in scope going away leaves the folder empty rather than
  // jumping to another folder's tree.
  assert.equal(nextTreeInResearchScope(trees, "research-b", "tree-2"), null);
  assert.equal(nextTreeInResearchScope(trees, null, "tree-1"), null);
});
