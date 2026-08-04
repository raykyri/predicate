import assert from "node:assert/strict";
import test from "node:test";
import {
  ENCYCLOPEDIA_AUTO_UPDATE_DEBOUNCE_MS,
  ENCYCLOPEDIA_AUTO_UPDATE_MIN_INTERVAL_MS,
  isEncyclopediaPageFileName,
  nextEncyclopediaAutoUpdateDelay,
  parseEncyclopediaHref,
  shouldScheduleEncyclopediaAutoUpdate,
  type EncyclopediaStatus,
} from "../src/lib/encyclopedia";
import { safeHref } from "../src/lib/links";
import {
  EMPTY_RESEARCH_HISTORY,
  pruneResearchHistory,
  pushResearchHistory,
  researchHistoryBack,
  researchHistoryForward,
} from "../src/lib/researchHistory";

function status(overrides: Partial<EncyclopediaStatus> = {}): EncyclopediaStatus {
  return {
    workspaceId: "ws-1",
    enabled: true,
    autoUpdate: true,
    updating: false,
    lastGeneratedAt: 0,
    pendingSourceCount: 3,
    pages: [],
    ...overrides,
  };
}

// ---- page file names (mirror of encyclopedia.rs is_valid_encyclopedia_file_name) ----

test("page file names accept plain markdown names", () => {
  assert.equal(isEncyclopediaPageFileName("index.md"), true);
  assert.equal(isEncyclopediaPageFileName("build-pipeline.md"), true);
  assert.equal(isEncyclopediaPageFileName("Notes 2.md"), true);
  assert.equal(isEncyclopediaPageFileName("advice..md"), true);
});

test("page file names reject traversal, hidden, and foreign names", () => {
  assert.equal(isEncyclopediaPageFileName("nested/page.md"), false);
  assert.equal(isEncyclopediaPageFileName("..\\page.md"), false);
  assert.equal(isEncyclopediaPageFileName(".hidden.md"), false);
  assert.equal(isEncyclopediaPageFileName("..md"), false);
  assert.equal(isEncyclopediaPageFileName(".md"), false);
  assert.equal(isEncyclopediaPageFileName("page.txt"), false);
  assert.equal(isEncyclopediaPageFileName("página.md"), false);
  assert.equal(isEncyclopediaPageFileName(`${"a".repeat(200)}.md`), false);
});

// ---- link routing ----
// Links inside a rendered page arrive through safeHref, which resolves
// relative hrefs against the inert qmux.invalid base; route through the real
// pipeline so these tests break if that contract moves.

function resolved(href: string): string {
  const url = safeHref(href);
  assert.ok(url, `safeHref rejected ${href}`);
  return url;
}

test("citation links route to the cited chat node", () => {
  const link = parseEncyclopediaHref(resolved("/research/rtree-1/rnode-2"));
  assert.deepEqual(link, { kind: "citation", treeId: "rtree-1", nodeId: "rnode-2" });
});

test("relative page links route to the sibling page", () => {
  assert.deepEqual(parseEncyclopediaHref(resolved("build-pipeline.md")), {
    kind: "page",
    fileName: "build-pipeline.md",
  });
  assert.deepEqual(parseEncyclopediaHref(resolved("./cache design.md")), {
    kind: "page",
    fileName: "cache design.md",
  });
});

test("absolute links on real hosts stay external", () => {
  const link = parseEncyclopediaHref(resolved("https://example.com/research/a/b"));
  assert.deepEqual(link, { kind: "external", url: "https://example.com/research/a/b" });
});

test("unrecognized internal shapes fall out as external", () => {
  assert.equal(parseEncyclopediaHref(resolved("/research/only-tree")).kind, "external");
  assert.equal(parseEncyclopediaHref(resolved("nested/deep/page.md")).kind, "external");
  assert.equal(parseEncyclopediaHref(resolved("/settings")).kind, "external");
  assert.equal(parseEncyclopediaHref("not a url").kind, "external");
});

// ---- page history ----
// The encyclopedia reuses the research document's history reducer with page
// file names as entries; pin that contract (App.tsx holds the state).

test("page history walks back and forward over visited pages", () => {
  let history = pushResearchHistory(EMPTY_RESEARCH_HISTORY, "index.md");
  history = pushResearchHistory(history, "caching.md");
  history = pushResearchHistory(history, "build-pipeline.md");
  const back = researchHistoryBack(history);
  assert.ok(back);
  assert.equal(back.nodeId, "caching.md");
  const forward = researchHistoryForward(back.history);
  assert.ok(forward);
  assert.equal(forward.nodeId, "build-pipeline.md");
  // Following a new link from the middle drops the forward stack.
  const branched = pushResearchHistory(back.history, "index.md");
  assert.equal(researchHistoryForward(branched), null);
  assert.deepEqual(branched.entries, ["index.md", "caching.md", "index.md"]);
});

test("regeneration prunes deleted pages and keeps the cursor sensible", () => {
  let history = pushResearchHistory(EMPTY_RESEARCH_HISTORY, "index.md");
  history = pushResearchHistory(history, "stale.md");
  history = pushResearchHistory(history, "caching.md");
  const pruned = pruneResearchHistory(
    history,
    new Set(["index.md", "caching.md"]),
    "caching.md",
  );
  assert.deepEqual(pruned.entries, ["index.md", "caching.md"]);
  assert.equal(pruned.entries[pruned.index], "caching.md");
  // Everything deleted: the history restarts at the fallback (or empties).
  const emptied = pruneResearchHistory(history, new Set(), null);
  assert.deepEqual(emptied.entries, []);
});

// ---- auto-update scheduling ----

test("auto-update arms only for the scoped, enabled, idle, dirty workspace", () => {
  assert.equal(shouldScheduleEncyclopediaAutoUpdate(status(), "ws-1"), true);
  assert.equal(shouldScheduleEncyclopediaAutoUpdate(null, "ws-1"), false);
  assert.equal(shouldScheduleEncyclopediaAutoUpdate(status(), null), false);
  assert.equal(shouldScheduleEncyclopediaAutoUpdate(status(), "ws-2"), false);
  assert.equal(
    shouldScheduleEncyclopediaAutoUpdate(status({ enabled: false }), "ws-1"),
    false,
  );
  assert.equal(
    shouldScheduleEncyclopediaAutoUpdate(status({ autoUpdate: false }), "ws-1"),
    false,
  );
  assert.equal(
    shouldScheduleEncyclopediaAutoUpdate(status({ updating: true }), "ws-1"),
    false,
  );
  assert.equal(
    shouldScheduleEncyclopediaAutoUpdate(status({ pendingSourceCount: 0 }), "ws-1"),
    false,
  );
});

test("first auto-update waits only the debounce", () => {
  assert.equal(
    nextEncyclopediaAutoUpdateDelay(1_000_000, null),
    ENCYCLOPEDIA_AUTO_UPDATE_DEBOUNCE_MS,
  );
});

test("repeat attempts are stretched to the minimum interval", () => {
  const now = 1_000_000;
  // An attempt moments ago: wait out the rest of the interval.
  assert.equal(
    nextEncyclopediaAutoUpdateDelay(now, now - 10_000),
    ENCYCLOPEDIA_AUTO_UPDATE_MIN_INTERVAL_MS - 10_000,
  );
  // A long-past attempt no longer stretches the debounce.
  assert.equal(
    nextEncyclopediaAutoUpdateDelay(now, now - ENCYCLOPEDIA_AUTO_UPDATE_MIN_INTERVAL_MS * 2),
    ENCYCLOPEDIA_AUTO_UPDATE_DEBOUNCE_MS,
  );
});
