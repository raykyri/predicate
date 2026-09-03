import assert from "node:assert/strict";
import test from "node:test";
import {
  EMPTY_RESEARCH_HISTORY,
  canGoBack,
  canGoForward,
  canGoWorkspaceBack,
  canGoWorkspaceForward,
  initResearchHistory,
  initResearchWorkspaceHistory,
  pushResearchHistory,
  pushResearchWorkspaceHistory,
  pruneResearchHistory,
  researchHistoryBack,
  researchHistoryForward,
  researchSwipeDirection,
  researchWorkspaceHistoryBack,
  researchWorkspaceHistoryForward,
} from "../src/lib/researchHistory";

test("empty history can go neither way", () => {
  assert.equal(canGoBack(EMPTY_RESEARCH_HISTORY), false);
  assert.equal(canGoForward(EMPTY_RESEARCH_HISTORY), false);
  assert.equal(researchHistoryBack(EMPTY_RESEARCH_HISTORY), null);
  assert.equal(researchHistoryForward(EMPTY_RESEARCH_HISTORY), null);
});

test("init roots the history at the entry node", () => {
  assert.deepEqual(initResearchHistory("root"), { entries: ["root"], index: 0 });
  assert.equal(initResearchHistory(null), EMPTY_RESEARCH_HISTORY);
});

test("a single-entry history cannot navigate", () => {
  const history = initResearchHistory("root");
  assert.equal(canGoBack(history), false);
  assert.equal(canGoForward(history), false);
});

test("push appends and advances the cursor to the end", () => {
  let history = initResearchHistory("a");
  history = pushResearchHistory(history, "b");
  history = pushResearchHistory(history, "c");
  assert.deepEqual(history, { entries: ["a", "b", "c"], index: 2 });
  assert.equal(canGoBack(history), true);
  assert.equal(canGoForward(history), false);
});

test("back then forward returns to the same node", () => {
  let history = pushResearchHistory(initResearchHistory("a"), "b");
  const back = researchHistoryBack(history);
  assert.ok(back);
  assert.equal(back.nodeId, "a");
  assert.deepEqual(back.history, { entries: ["a", "b"], index: 0 });

  const forward = researchHistoryForward(back.history);
  assert.ok(forward);
  assert.equal(forward.nodeId, "b");
  assert.deepEqual(forward.history, { entries: ["a", "b"], index: 1 });
});

test("pushing after going back truncates the forward entries", () => {
  // a -> b -> c, step back to b, then branch to d: c must be discarded.
  let history = pushResearchHistory(pushResearchHistory(initResearchHistory("a"), "b"), "c");
  const back = researchHistoryBack(history);
  assert.ok(back);
  assert.equal(back.nodeId, "b");
  const branched = pushResearchHistory(back.history, "d");
  assert.deepEqual(branched, { entries: ["a", "b", "d"], index: 2 });
  assert.equal(canGoForward(branched), false);
});

test("push does not mutate the prior history value", () => {
  const before = initResearchHistory("a");
  const after = pushResearchHistory(before, "b");
  assert.deepEqual(before, { entries: ["a"], index: 0 });
  assert.notEqual(before.entries, after.entries);
});

test("horizontal wheel gestures resolve only after clear dominant travel", () => {
  assert.equal(researchSwipeDirection(-79, 0), 0);
  assert.equal(researchSwipeDirection(79, 0), 0);
  assert.equal(researchSwipeDirection(-100, 90), 0);
  assert.equal(researchSwipeDirection(100, 90), 0);
  assert.equal(researchSwipeDirection(-100, 20), -1);
  assert.equal(researchSwipeDirection(100, 20), 1);
});

test("pruning deleted visits preserves the surviving cursor position", () => {
  const history = { entries: ["root", "branch", "leaf", "root"], index: 2 };
  assert.deepEqual(pruneResearchHistory(history, new Set(["root"]), "root"), {
    entries: ["root"],
    index: 0,
  });
});

test("pruning keeps the cursor on a later surviving visit", () => {
  const history = { entries: ["root", "branch", "leaf"], index: 1 };
  assert.deepEqual(pruneResearchHistory(history, new Set(["root", "leaf"]), "root"), {
    entries: ["root", "leaf"],
    index: 0,
  });
});

test("pruning collapses visits that become adjacent duplicates", () => {
  // root -> branch -> back to root, then branch is deleted: without the
  // collapse the history reads [root, root] and Back re-applies the already
  // selected node — readers treat that as real navigation (clearing content
  // for a load that never restarts).
  const history = { entries: ["root", "branch", "root"], index: 2 };
  const pruned = pruneResearchHistory(history, new Set(["root", "other"]), "root");
  assert.deepEqual(pruned, { entries: ["root"], index: 0 });
  assert.equal(canGoBack(pruned), false);
  assert.equal(canGoForward(pruned), false);
});

test("pruning falls back when every visited node was deleted", () => {
  assert.deepEqual(
    pruneResearchHistory({ entries: ["branch", "leaf"], index: 1 }, new Set(["root"]), "root"),
    { entries: ["root"], index: 0 },
  );
});

test("opening a document from Recent Activity pushes a return visit", () => {
  let history = initResearchWorkspaceHistory({ kind: "journal" });
  history = pushResearchWorkspaceHistory(history, { kind: "document", treeId: "tree-a" });
  assert.deepEqual(history, {
    entries: [{ kind: "journal" }, { kind: "document", treeId: "tree-a" }],
    index: 1,
  });
  assert.equal(canGoWorkspaceBack(history), true);
  assert.equal(canGoWorkspaceForward(history), false);

  const back = researchWorkspaceHistoryBack(history);
  assert.ok(back);
  assert.deepEqual(back.visit, { kind: "journal" });
  const forward = researchWorkspaceHistoryForward(back.history);
  assert.ok(forward);
  assert.deepEqual(forward.visit, { kind: "document", treeId: "tree-a" });
});

test("re-opening the current workspace page does not grow the stack", () => {
  const history = initResearchWorkspaceHistory({ kind: "journal" });
  assert.equal(pushResearchWorkspaceHistory(history, { kind: "journal" }), history);
});
