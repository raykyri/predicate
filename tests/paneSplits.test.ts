import test from "node:test";
import assert from "node:assert/strict";
import { cycleTabId, selectPaneAfterClose } from "../src/lib/appHelpers";
import {
  movePaneAdjacentToPane,
  movePaneAfter,
  movePaneAcrossGroups,
  movePaneBy,
  paneCanMoveAcrossGroups,
} from "../src/lib/paneTree";
import {
  canToggleTurnSidebar,
  detachPaneFromSplitMemberships,
  joinPaneSplit,
  normalizePaneSplitsForPanes,
  paneSplitAxis,
  paneSplitFlagIsEnabled,
  paneSnapshotForPersistedPaneSplits,
  reservedTerminalStageWidth,
  setPaneSplitFlagEnabled,
  togglePaneSplitAxis,
  withPaneSplitAxis,
} from "../src/lib/paneSplits";
import type { PaneInfo, PaneSplitInfo } from "../src/types";

function pane(id: string, depth = 0, groupId = "group-1"): PaneInfo {
  return {
    id,
    title: id,
    kind: "shell",
    groupId,
    cwd: "/tmp",
    cols: 80,
    rows: 24,
    status: "running",
    depth,
  };
}

function panes(ids: string[]): PaneInfo[] {
  return ids.map((id) => pane(id));
}

function split(paneIds: string[]): PaneSplitInfo {
  return {
    id: "split-1",
    paneIds,
    sizes: Object.fromEntries(paneIds.map((paneId) => [paneId, 1 / paneIds.length])),
  };
}

function isPaneInCollapsedGroup(pane: PaneInfo) {
  return pane.groupId === "group-collapsed";
}

function splitWithSizes(sizes: Record<string, number>, id = "split-1"): PaneSplitInfo {
  return {
    id,
    paneIds: Object.keys(sizes),
    sizes,
  };
}

function insertedRelativeIntent(
  anchorPaneId: string,
  position: "above" | "below",
  source: "command" | "join" | "drag-half" | "drag-divider" = "join",
  createdAt = 123,
) {
  return {
    kind: "inserted-relative" as const,
    anchorPaneId,
    position,
    source,
    createdAt,
  };
}

function assertApprox(actual: number, expected: number) {
  assert.ok(
    Math.abs(actual - expected) < 0.000001,
    `expected ${actual} to be approximately ${expected}`,
  );
}

test("Option-Command-Arrow pane moves stay within flat group boundaries", () => {
  const flat = panes(["a", "b", "c"]);
  assert.deepEqual(movePaneBy(flat, "b", -1).map((item) => item.id), ["b", "a", "c"]);
  assert.deepEqual(movePaneBy(flat, "b", 1).map((item) => item.id), ["a", "c", "b"]);
  assert.equal(movePaneBy(flat, "a", -1), flat);
  assert.equal(movePaneBy(flat, "c", 1), flat);

  const legacyDepths = [
    pane("root"),
    pane("a", 1),
    pane("a-child", 2),
    pane("b", 1),
    pane("next-root"),
  ];
  assert.deepEqual(
    movePaneBy(legacyDepths, "a", 1).map((item) => item.id),
    ["root", "a-child", "a", "b", "next-root"],
  );
  assert.deepEqual(movePaneBy(legacyDepths, "b", 1).map((item) => item.id), [
    "root",
    "a",
    "a-child",
    "next-root",
    "b",
  ]);
});

test("normalizePaneSplitsForPanes preserves a split after its top pane closes", () => {
  const normalized = normalizePaneSplitsForPanes(
    [split(["pane-1", "pane-2", "pane-3"])],
    panes(["pane-2", "pane-3", "pane-4"]),
  );

  assert.deepEqual(normalized.map((candidate) => candidate.paneIds), [["pane-2", "pane-3"]]);
});

test("normalizePaneSplitsForPanes preserves a split after its middle pane closes", () => {
  const normalized = normalizePaneSplitsForPanes(
    [split(["pane-1", "pane-2", "pane-3"])],
    panes(["pane-1", "pane-3", "pane-4"]),
  );

  assert.deepEqual(normalized.map((candidate) => candidate.paneIds), [["pane-1", "pane-3"]]);
});

test("normalizePaneSplitsForPanes preserves a split after its bottom pane closes", () => {
  const normalized = normalizePaneSplitsForPanes(
    [split(["pane-1", "pane-2", "pane-3"])],
    panes(["pane-1", "pane-2", "pane-4"]),
  );

  assert.deepEqual(normalized.map((candidate) => candidate.paneIds), [["pane-1", "pane-2"]]);
});

test("normalizePaneSplitsForPanes drops a split when fewer than two panes remain", () => {
  const normalized = normalizePaneSplitsForPanes(
    [split(["pane-1", "pane-2"])],
    panes(["pane-1", "pane-3"]),
  );

  assert.deepEqual(normalized, []);
});

test("normalizePaneSplitsForPanes preserves a horizontal axis and omits vertical", () => {
  const horizontal = normalizePaneSplitsForPanes(
    [{ ...split(["pane-1", "pane-2"]), axis: "horizontal" }],
    panes(["pane-1", "pane-2"]),
  );
  const vertical = normalizePaneSplitsForPanes(
    [{ ...split(["pane-1", "pane-2"]), axis: "vertical" }],
    panes(["pane-1", "pane-2"]),
  );
  const omitted = normalizePaneSplitsForPanes(
    [split(["pane-1", "pane-2"])],
    panes(["pane-1", "pane-2"]),
  );

  assert.equal(paneSplitAxis(horizontal[0]), "horizontal");
  assert.equal(horizontal[0].axis, "horizontal");
  assert.equal(paneSplitAxis(vertical[0]), "vertical");
  assert.equal(vertical[0].axis, undefined);
  assert.equal(omitted[0].axis, undefined);
});

test("togglePaneSplitAxis switches between stacked and columns and omits vertical", () => {
  const stacked = split(["pane-1", "pane-2"]);
  const columns = togglePaneSplitAxis(stacked);
  const restored = togglePaneSplitAxis(columns);

  assert.equal(paneSplitAxis(stacked), "vertical");
  assert.equal(columns.axis, "horizontal");
  assert.equal(restored.axis, undefined);
  assert.equal(withPaneSplitAxis(columns, "horizontal"), columns);
  assert.equal(withPaneSplitAxis(stacked, "vertical"), stacked);
});

test("reservedTerminalStageWidth keeps column splits at the per-pane floor", () => {
  assert.equal(
    reservedTerminalStageWidth({
      axis: "vertical",
      paneCount: 3,
      minWidth: 380,
      splitMinWidth: 200,
      gutter: 8,
    }),
    380,
  );
  assert.equal(
    reservedTerminalStageWidth({
      axis: "horizontal",
      paneCount: 1,
      minWidth: 380,
      splitMinWidth: 200,
      gutter: 8,
    }),
    380,
  );
  assert.equal(
    reservedTerminalStageWidth({
      axis: "horizontal",
      paneCount: 2,
      minWidth: 380,
      splitMinWidth: 200,
      gutter: 8,
    }),
    408,
  );
  assert.equal(
    reservedTerminalStageWidth({
      axis: "horizontal",
      paneCount: 3,
      minWidth: 380,
      splitMinWidth: 200,
      gutter: 8,
    }),
    616,
  );
});

test("joinPaneSplit inherits a horizontal axis from the existing split", () => {
  const joined = joinPaneSplit(
    [{ ...split(["pane-1", "pane-2"]), axis: "horizontal" }],
    panes(["pane-1", "pane-2", "pane-3"]),
    "pane-2",
    "pane-3",
    { insertedPaneId: "pane-3", source: "command", createdAt: 1 },
  );

  assert.equal(joined[0].axis, "horizontal");
  assert.deepEqual(joined[0].paneIds, ["pane-1", "pane-2", "pane-3"]);
});

test("joinPaneSplit starts a new split vertical when neither pane is already split", () => {
  const joined = joinPaneSplit([], panes(["pane-1", "pane-2"]), "pane-1", "pane-2");

  assert.equal(joined[0].axis, undefined);
  assert.equal(paneSplitAxis(joined[0]), "vertical");
});

test("joinPaneSplit prefers the source pane's axis when merging two splits", () => {
  const joined = joinPaneSplit(
    [
      { ...splitWithSizes({ "pane-1": 0.5, "pane-2": 0.5 }, "split-1"), axis: "horizontal" },
      splitWithSizes({ "pane-3": 0.5, "pane-4": 0.5 }, "split-2"),
    ],
    panes(["pane-1", "pane-2", "pane-3", "pane-4"]),
    "pane-2",
    "pane-3",
  );

  assert.equal(joined[0].axis, "horizontal");
});

test("detachPaneFromSplitMemberships keeps the remaining split's axis", () => {
  const detached = detachPaneFromSplitMemberships(
    [{ ...split(["pane-1", "pane-2", "pane-3"]), axis: "horizontal" }],
    "pane-3",
  );

  assert.equal(detached[0].axis, "horizontal");
  assert.deepEqual(detached[0].paneIds, ["pane-1", "pane-2"]);
});

test("split pane flags apply to the whole group and preserve unrelated panes", () => {
  assert.equal(
    paneSplitFlagIsEnabled({ "pane-2": true }, ["pane-1", "pane-2", "pane-3"]),
    true,
  );

  const flags = { "pane-outside": true };
  const expanded = setPaneSplitFlagEnabled(flags, ["pane-1", "pane-2", "pane-3"], true);

  assert.deepEqual(expanded, {
    "pane-outside": true,
    "pane-1": true,
    "pane-2": true,
    "pane-3": true,
  });
  assert.equal(paneSplitFlagIsEnabled(expanded, ["pane-1", "pane-2", "pane-3"]), true);

  const collapsed = setPaneSplitFlagEnabled(expanded, ["pane-1", "pane-2", "pane-3"], false);
  assert.deepEqual(collapsed, { "pane-outside": true });
  assert.equal(paneSplitFlagIsEnabled(collapsed, ["pane-1", "pane-2", "pane-3"]), false);
});

test("split pane flag updates preserve state identity when nothing changes", () => {
  const expanded = { "pane-1": true, "pane-2": true };
  assert.equal(setPaneSplitFlagEnabled(expanded, ["pane-1", "pane-2"], true), expanded);

  const collapsed = { "pane-outside": true };
  assert.equal(setPaneSplitFlagEnabled(collapsed, ["pane-1", "pane-2"], false), collapsed);
});

test("split transcript controls remain available from a shell sibling", () => {
  assert.equal(canToggleTurnSidebar(false, true, 1), true);
  assert.equal(canToggleTurnSidebar(false, true, 0), false);
  assert.equal(canToggleTurnSidebar(false, false, 1), false);
  assert.equal(canToggleTurnSidebar(true, false, 0), true);
});

test("normalizePaneSplitsForPanes prunes split intent for missing panes and anchors", () => {
  const normalized = normalizePaneSplitsForPanes(
    [
      {
        ...split(["pane-1", "pane-2", "pane-3"]),
        intent: {
          "pane-2": insertedRelativeIntent("pane-1", "below", "command", 1),
          "pane-3": insertedRelativeIntent("pane-missing", "below", "drag-half", 2),
          "pane-missing": insertedRelativeIntent("pane-1", "below", "join", 3),
        },
      },
    ],
    panes(["pane-1", "pane-2"]),
  );

  assert.deepEqual(normalized[0].intent, {
    "pane-2": insertedRelativeIntent("pane-1", "below", "command", 1),
  });
});

test("normalizePaneSplitsForPanes still rejects non-contiguous remaining panes", () => {
  const normalized = normalizePaneSplitsForPanes(
    [split(["pane-1", "pane-2", "pane-3"])],
    panes(["pane-1", "pane-4", "pane-3"]),
  );

  assert.deepEqual(normalized, []);
});

test("selectPaneAfterClose prefers the next split pane when closing the top split pane", () => {
  assert.equal(
    selectPaneAfterClose(panes(["pane-outside", "pane-1", "pane-2"]), "pane-1", [
      split(["pane-1", "pane-2"]),
    ]),
    "pane-2",
  );
});

test("selectPaneAfterClose prefers a previous split pane when closing middle or bottom panes", () => {
  const currentPanes = panes(["pane-1", "pane-2", "pane-3", "pane-outside"]);
  const currentSplits = [split(["pane-1", "pane-2", "pane-3"])];

  assert.equal(selectPaneAfterClose(currentPanes, "pane-2", currentSplits), "pane-1");
  assert.equal(selectPaneAfterClose(currentPanes, "pane-3", currentSplits), "pane-2");
});

test("selectPaneAfterClose skips stale split members before leaving the split", () => {
  assert.equal(
    selectPaneAfterClose(panes(["pane-outside", "pane-1", "pane-3"]), "pane-1", [
      split(["pane-1", "pane-missing", "pane-3"]),
    ]),
    "pane-3",
  );
});

test("selectPaneAfterClose falls back to neighboring tabs outside a split", () => {
  assert.equal(selectPaneAfterClose(panes(["pane-1", "pane-2", "pane-3"]), "pane-2"), "pane-1");
});

test("selectPaneAfterClose prefers visible tabs over collapsed-group neighbors", () => {
  assert.equal(
    selectPaneAfterClose(
      [
        pane("pane-visible-before", 0, "group-visible"),
        pane("pane-collapsed-previous", 0, "group-collapsed"),
        pane("pane-closing", 0, "group-visible"),
        pane("pane-visible-next", 0, "group-visible"),
      ],
      "pane-closing",
      [],
      { isPaneInCollapsedGroup },
    ),
    "pane-visible-next",
  );
});

test("selectPaneAfterClose prefers visible tabs over collapsed split members", () => {
  assert.equal(
    selectPaneAfterClose(
      [
        pane("pane-visible", 0, "group-visible"),
        pane("pane-closing", 0, "group-collapsed"),
        pane("pane-split-peer", 0, "group-collapsed"),
      ],
      "pane-closing",
      [split(["pane-closing", "pane-split-peer"])],
      { isPaneInCollapsedGroup },
    ),
    "pane-visible",
  );
});

test("selectPaneAfterClose falls back to collapsed groups when no visible tabs remain", () => {
  assert.equal(
    selectPaneAfterClose(
      [
        pane("pane-collapsed-previous", 0, "group-collapsed"),
        pane("pane-closing", 0, "group-collapsed"),
        pane("pane-collapsed-next", 0, "group-collapsed"),
      ],
      "pane-closing",
      [],
      { isPaneInCollapsedGroup },
    ),
    "pane-collapsed-previous",
  );
});

test("cycleTabId skips other panes in the active split", () => {
  const tabIds = ["pane-1", "pane-2", "pane-3", "pane-4"];
  const paneSplits = [split(["pane-2", "pane-3"])];

  assert.equal(cycleTabId(tabIds, "pane-2", 1, paneSplits), "pane-4");
  assert.equal(cycleTabId(tabIds, "pane-3", -1, paneSplits), "pane-1");
});

test("cycleTabId enters split panes from the nearest edge", () => {
  const tabIds = ["pane-1", "pane-2", "pane-3", "pane-4"];
  const paneSplits = [split(["pane-2", "pane-3"])];

  assert.equal(cycleTabId(tabIds, "pane-1", 1, paneSplits), "pane-2");
  assert.equal(cycleTabId(tabIds, "pane-4", -1, paneSplits), "pane-3");
});

test("cycleTabId treats a split as one stop when a sentinel tab is included", () => {
  const tabIds = ["__home__", "pane-1", "pane-2"];
  const paneSplits = [split(["pane-1", "pane-2"])];

  assert.equal(cycleTabId(tabIds, "pane-1", 1, paneSplits), "__home__");
  assert.equal(cycleTabId(tabIds, "pane-2", -1, paneSplits), "__home__");
});

test("cycleTabId stays put when a split is the only cycle target", () => {
  const tabIds = ["pane-1", "pane-2"];
  const paneSplits = [split(["pane-1", "pane-2"])];

  assert.equal(cycleTabId(tabIds, "pane-1", 1, paneSplits), "pane-1");
  assert.equal(cycleTabId(tabIds, "pane-2", -1, paneSplits), "pane-2");
});

test("movePaneAdjacentToPane moves one pane below a target", () => {
  const moved = movePaneAdjacentToPane(
    [pane("pane-1"), pane("pane-2", 1), pane("pane-3"), pane("pane-4")],
    "pane-4",
    "pane-2",
    "below",
  );

  assert.deepEqual(
    moved.map((candidate) => [candidate.id, candidate.depth ?? 0]),
    [
      ["pane-1", 0],
      ["pane-2", 0],
      ["pane-4", 0],
      ["pane-3", 0],
    ],
  );
});

test("movePaneAdjacentToPane does not move legacy descendants with a pane", () => {
  const moved = movePaneAdjacentToPane(
    [
      pane("pane-1"),
      pane("pane-2"),
      pane("pane-2-child", 1),
      pane("pane-2-grandchild", 2),
      pane("pane-3"),
    ],
    "pane-2",
    "pane-3",
    "below",
  );

  assert.deepEqual(
    moved.map((candidate) => [candidate.id, candidate.depth ?? 0]),
    [
      ["pane-1", 0],
      ["pane-2-child", 0],
      ["pane-2-grandchild", 0],
      ["pane-3", 0],
      ["pane-2", 0],
    ],
  );
});

test("movePaneAdjacentToPane treats legacy descendants as ordinary rows", () => {
  const panes = [pane("pane-1"), pane("pane-2", 1), pane("pane-3")];

  assert.deepEqual(
    movePaneAdjacentToPane(panes, "pane-1", "pane-2", "below").map(
      (candidate) => candidate.id,
    ),
    ["pane-2", "pane-1", "pane-3"],
  );
});

test("joinPaneSplit inserts a dragged pane into an existing split after reordering", () => {
  const orderedPanes = movePaneAdjacentToPane(
    panes(["pane-1", "pane-2", "pane-3"]),
    "pane-3",
    "pane-1",
    "below",
  );
  const joined = joinPaneSplit(
    detachPaneFromSplitMemberships([split(["pane-1", "pane-2"])], "pane-3"),
    orderedPanes,
    "pane-1",
    "pane-3",
  );

  assert.deepEqual(joined.map((candidate) => candidate.paneIds), [
    ["pane-1", "pane-3", "pane-2"],
  ]);
});

test("joinPaneSplit records inserted pane intent", () => {
  const joined = joinPaneSplit([], panes(["pane-1", "pane-2"]), "pane-1", "pane-2", {
    insertedPaneId: "pane-2",
    source: "command",
    createdAt: 456,
  });

  assert.deepEqual(joined[0].intent, {
    "pane-2": insertedRelativeIntent("pane-1", "below", "command", 456),
  });
});

test("paneSnapshotForPersistedPaneSplits keeps a split when current panes lag a new pane", () => {
  const currentPanes = panes(["pane-1"]);
  const requestedPanes = panes(["pane-1", "pane-2"]);
  const persistedSplits = [split(["pane-1", "pane-2"])];
  const paneSnapshot = paneSnapshotForPersistedPaneSplits(
    persistedSplits,
    currentPanes,
    requestedPanes,
  );

  assert.strictEqual(paneSnapshot, requestedPanes);
  assert.deepEqual(
    normalizePaneSplitsForPanes(persistedSplits, paneSnapshot).map(
      (candidate) => candidate.paneIds,
    ),
    [["pane-1", "pane-2"]],
  );
});

test("paneSnapshotForPersistedPaneSplits uses current panes once they include the split", () => {
  const currentPanes = panes(["pane-1", "pane-2", "pane-3"]);
  const requestedPanes = panes(["pane-1", "pane-2"]);
  const persistedSplits = [split(["pane-1", "pane-2"])];

  assert.strictEqual(
    paneSnapshotForPersistedPaneSplits(persistedSplits, currentPanes, requestedPanes),
    currentPanes,
  );
});

test("joinPaneSplit preserves existing split intent when inserting another pane", () => {
  const joined = joinPaneSplit(
    [
      {
        ...splitWithSizes({ "pane-1": 0.5, "pane-2": 0.5 }),
        intent: {
          "pane-2": insertedRelativeIntent("pane-1", "below", "command", 1),
        },
      },
    ],
    panes(["pane-1", "pane-2", "pane-3"]),
    "pane-2",
    "pane-3",
    {
      insertedPaneId: "pane-3",
      source: "drag-half",
      createdAt: 2,
    },
  );

  assert.deepEqual(joined[0].intent, {
    "pane-2": insertedRelativeIntent("pane-1", "below", "command", 1),
    "pane-3": insertedRelativeIntent("pane-2", "below", "drag-half", 2),
  });
});

test("joinPaneSplit preserves existing split proportions when inserting a pane", () => {
  const joined = joinPaneSplit(
    [splitWithSizes({ "pane-1": 0.75, "pane-2": 0.25 })],
    panes(["pane-1", "pane-3", "pane-2"]),
    "pane-1",
    "pane-3",
  );
  const sizes = joined[0].sizes;

  assert.deepEqual(joined.map((candidate) => candidate.paneIds), [
    ["pane-1", "pane-3", "pane-2"],
  ]);
  assertApprox(sizes["pane-1"], 0.5);
  assertApprox(sizes["pane-3"], 1 / 3);
  assertApprox(sizes["pane-2"], 1 / 6);
});

test("joinPaneSplit preserves each split's proportions when merging split groups", () => {
  const joined = joinPaneSplit(
    [
      splitWithSizes({ "pane-1": 0.75, "pane-2": 0.25 }, "split-1"),
      splitWithSizes({ "pane-3": 0.6, "pane-4": 0.4 }, "split-2"),
    ],
    panes(["pane-1", "pane-2", "pane-3", "pane-4"]),
    "pane-2",
    "pane-3",
  );
  const sizes = joined[0].sizes;

  assert.deepEqual(joined.map((candidate) => candidate.paneIds), [
    ["pane-1", "pane-2", "pane-3", "pane-4"],
  ]);
  assertApprox(sizes["pane-1"], 0.375);
  assertApprox(sizes["pane-2"], 0.125);
  assertApprox(sizes["pane-3"], 0.3);
  assertApprox(sizes["pane-4"], 0.2);
});

test("joinPaneSplit joins one dragged pane and leaves legacy rows in place", () => {
  const orderedPanes = movePaneAdjacentToPane(
    [pane("pane-1"), pane("pane-2"), pane("pane-2-child", 1), pane("pane-3")],
    "pane-2",
    "pane-3",
    "below",
  );
  const joined = joinPaneSplit([], orderedPanes, "pane-3", "pane-2");

  assert.deepEqual(
    orderedPanes.map((candidate) => [candidate.id, candidate.depth ?? 0]),
    [
      ["pane-1", 0],
      ["pane-2-child", 0],
      ["pane-3", 0],
      ["pane-2", 0],
    ],
  );
  assert.deepEqual(joined.map((candidate) => candidate.paneIds), [["pane-3", "pane-2"]]);
});

test("detachPaneFromSplitMemberships lets a pane reorder within its existing split", () => {
  const orderedPanes = movePaneAdjacentToPane(
    panes(["pane-1", "pane-2", "pane-3"]),
    "pane-3",
    "pane-1",
    "below",
  );
  const joined = joinPaneSplit(
    detachPaneFromSplitMemberships([split(["pane-1", "pane-2", "pane-3"])], "pane-3"),
    orderedPanes,
    "pane-1",
    "pane-3",
  );

  assert.deepEqual(joined.map((candidate) => candidate.paneIds), [
    ["pane-1", "pane-3", "pane-2"],
  ]);
});

test("detachPaneFromSplitMemberships drops intent for detached panes and detached anchors", () => {
  const detached = detachPaneFromSplitMemberships(
    [
      {
        ...split(["pane-1", "pane-2", "pane-3"]),
        intent: {
          "pane-2": insertedRelativeIntent("pane-1", "below", "command", 1),
          "pane-3": insertedRelativeIntent("pane-2", "below", "drag-half", 2),
        },
      },
    ],
    "pane-1",
  );

  assert.deepEqual(detached[0].intent, {
    "pane-3": insertedRelativeIntent("pane-2", "below", "drag-half", 2),
  });
});

test("movePaneAfter lifts a middle tab to just below the target", () => {
  const moved = movePaneAfter(panes(["pane-1", "pane-2", "pane-3"]), "pane-2", "pane-3");

  assert.deepEqual(
    moved.map((candidate) => candidate.id),
    ["pane-1", "pane-3", "pane-2"],
  );
});

test("movePaneAfter keeps trailing tabs after the moved tab", () => {
  const moved = movePaneAfter(
    panes(["x", "pane-1", "pane-2", "pane-3", "y"]),
    "pane-2",
    "pane-3",
  );

  assert.deepEqual(
    moved.map((candidate) => candidate.id),
    ["x", "pane-1", "pane-3", "pane-2", "y"],
  );
});

test("movePaneAfter places the moved tab directly after the target", () => {
  const tree = [pane("a"), pane("b"), pane("c"), pane("c-child", 1), pane("d")];
  const moved = movePaneAfter(tree, "b", "c");

  assert.deepEqual(
    moved.map((candidate) => ({ id: candidate.id, depth: candidate.depth })),
    [
      { id: "a", depth: 0 },
      { id: "c", depth: 0 },
      { id: "b", depth: 0 },
      { id: "c-child", depth: 0 },
      { id: "d", depth: 0 },
    ],
  );
});

test("movePaneAfter treats legacy descendants as ordinary rows", () => {
  const tree = [pane("a"), pane("b"), pane("b-child", 1)];
  const moved = movePaneAfter(tree, "b", "b-child");

  assert.deepEqual(
    moved.map((candidate) => candidate.id),
    ["a", "b-child", "b"],
  );
});

test("detaching a middle member keeps the remaining tabs as a contiguous split", () => {
  const before = panes(["pane-1", "pane-2", "pane-3"]);
  const splits = [split(["pane-1", "pane-2", "pane-3"])];

  // Mirror removePaneFromSplit: drop the tab from the split, then relocate it.
  const detached = detachPaneFromSplitMemberships(splits, "pane-2");
  const reordered = movePaneAfter(before, "pane-2", "pane-3");
  const normalized = normalizePaneSplitsForPanes(detached, reordered);

  assert.deepEqual(
    reordered.map((candidate) => candidate.id),
    ["pane-1", "pane-3", "pane-2"],
  );
  assert.deepEqual(
    normalized.map((candidate) => candidate.paneIds),
    [["pane-1", "pane-3"]],
  );
});

test("detaching an edge member leaves the remaining split contiguous without reordering", () => {
  const before = panes(["pane-1", "pane-2", "pane-3"]);
  const splits = [split(["pane-1", "pane-2", "pane-3"])];

  // Edge members don't move; the membership change alone keeps the rest grouped.
  const detached = detachPaneFromSplitMemberships(splits, "pane-1");
  const normalized = normalizePaneSplitsForPanes(detached, before);

  assert.deepEqual(
    normalized.map((candidate) => candidate.paneIds),
    [["pane-2", "pane-3"]],
  );
});

function agentPane(id: string, depth = 0, groupId = "group-1"): PaneInfo {
  return { ...pane(id, depth, groupId), kind: "agent", agentId: `agent-${id}` };
}

test("paneCanMoveAcrossGroups checks only the selected pane", () => {
  const tree = [pane("a"), pane("a-child", 1), pane("b"), agentPane("b-child", 1)];

  assert.equal(paneCanMoveAcrossGroups(tree, "a"), true);
  assert.equal(paneCanMoveAcrossGroups(tree, "b"), true);
  assert.equal(paneCanMoveAcrossGroups(tree, "b-child"), false);
  assert.equal(paneCanMoveAcrossGroups(tree, "missing"), false);
});

test("movePaneAcrossGroups drops one pane at a gap in another group", () => {
  const source = [pane("a"), pane("a-child", 1), pane("b")];
  const target = [pane("x", 0, "group-2"), pane("y", 0, "group-2")];

  const moved = movePaneAcrossGroups(source, target, "a", "group-2", 1);

  assert.ok(moved);
  assert.deepEqual(
    moved.source.map((candidate) => [candidate.id, candidate.depth]),
    [
      ["a-child", 0],
      ["b", 0],
    ],
  );
  assert.deepEqual(
    moved.target.map((candidate) => [candidate.id, candidate.groupId, candidate.depth]),
    [
      ["x", "group-2", 0],
      ["a", "group-2", 0],
      ["y", "group-2", 0],
    ],
  );
});

test("movePaneAcrossGroups clamps an empty-target gap and moves only one pane", () => {
  const intoEmpty = movePaneAcrossGroups(
    [pane("a", 0), pane("a-child", 1)],
    [],
    "a",
    "group-2",
    5,
  );
  assert.ok(intoEmpty);
  assert.deepEqual(
    intoEmpty.target.map((candidate) => [candidate.id, candidate.groupId, candidate.depth]),
    [
      ["a", "group-2", 0],
    ],
  );
  assert.deepEqual(intoEmpty.source.map((candidate) => candidate.id), ["a-child"]);
});
