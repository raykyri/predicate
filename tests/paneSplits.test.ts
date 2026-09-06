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
  canSplitPaneInTree,
  canToggleTurnSidebar,
  detachPaneFromSplitMemberships,
  joinPaneSplit,
  normalizePaneSplitsForPanes,
  paneAtStagePoint,
  paneSplitAxis,
  paneSplitFlagIsEnabled,
  paneSplitIsNested,
  paneSplitLayout,
  paneSplitRootNode,
  paneSplitsEqual,
  paneSnapshotForPersistedPaneSplits,
  reservedTerminalStageWidth,
  resizeSplitNodeFractions,
  setPaneSplitFlagEnabled,
  splitAxisForPane,
  splitBranchChildCountForPane,
  splitCalc,
  splitFractions,
  splitNodeAtPath,
  splitNodePaneIds,
  splitRectOffsets,
  splitRectPixels,
  togglePaneSplitAxis,
  withPaneSplitAxis,
} from "../src/lib/paneSplits";
import type { PaneInfo, PaneSplitInfo, PaneSplitNode } from "../src/types";

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

test("joinPaneSplit can start a new split as columns", () => {
  const joined = joinPaneSplit([], panes(["pane-1", "pane-2"]), "pane-1", "pane-2", {
    axis: "horizontal",
  });

  assert.equal(joined[0].axis, "horizontal");
});

test("joinPaneSplit keeps an existing stacked axis even if columns are requested", () => {
  const joined = joinPaneSplit(
    [split(["pane-1", "pane-2"])],
    panes(["pane-1", "pane-2", "pane-3"]),
    "pane-2",
    "pane-3",
    { axis: "horizontal" },
  );

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

test("selectPaneAfterClose selects the next tab when closing the first tab in a group", () => {
  assert.equal(
    selectPaneAfterClose(
      [
        pane("pane-previous-group", 0, "group-1"),
        pane("pane-closing", 0, "group-2"),
        pane("pane-next", 0, "group-2"),
      ],
      "pane-closing",
    ),
    "pane-next",
  );
});

test("selectPaneAfterClose leaves the group when its only tab closes", () => {
  assert.equal(
    selectPaneAfterClose(
      [
        pane("pane-previous-group", 0, "group-1"),
        pane("pane-closing", 0, "group-2"),
        pane("pane-next-group", 0, "group-3"),
      ],
      "pane-closing",
    ),
    "pane-previous-group",
  );
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

/* ------------------------------------------------------------------------- *
 * Nested splits (bracketing tree)
 * ------------------------------------------------------------------------- */

const GUTTER = 8;

function paneNode(paneId: string, size?: number): PaneSplitNode {
  return size === undefined ? { kind: "pane", paneId } : { kind: "pane", paneId, size };
}

function branchNode(
  axis: "vertical" | "horizontal",
  children: PaneSplitNode[],
  size?: number,
): PaneSplitNode {
  return size === undefined
    ? { kind: "split", axis, children }
    : { kind: "split", axis, size, children };
}

function nestedSplit(root: PaneSplitNode, id = "split-1"): PaneSplitInfo {
  const paneIds = splitNodePaneIds(root);
  return {
    id,
    paneIds,
    sizes: Object.fromEntries(paneIds.map((paneId) => [paneId, 1 / paneIds.length])),
    ...(root.kind === "split" && root.axis === "horizontal" ? { axis: "horizontal" as const } : {}),
    root,
  };
}

/** The pane geometry the flat split has always produced, from the pre-nesting
 * `splitTrackExtent` / `splitTrackPosition` formulas. */
function legacyFlatRect(split: PaneSplitInfo, index: number) {
  const fractions = splitFractions(split);
  const totalGutter = (split.paneIds.length - 1) * GUTTER;
  const start = fractions.slice(0, index).reduce((sum, value) => sum + value, 0);
  return {
    startFraction: start,
    startPx: -start * totalGutter + index * GUTTER,
    extentFraction: fractions[index],
    extentPx: -fractions[index] * totalGutter,
  };
}

test("paneSplitLayout reproduces the legacy flat geometry on both axes", () => {
  for (const axis of ["vertical", "horizontal"] as const) {
    const flat: PaneSplitInfo = {
      ...split(["pane-1", "pane-2", "pane-3"]),
      sizes: { "pane-1": 0.5, "pane-2": 0.25, "pane-3": 0.25 },
      ...(axis === "horizontal" ? { axis } : {}),
    };
    const layout = paneSplitLayout(flat, GUTTER);
    flat.paneIds.forEach((paneId, index) => {
      const rect = layout.panes.get(paneId);
      assert.ok(rect, `${axis} ${paneId}`);
      const expected = legacyFlatRect(flat, index);
      if (axis === "horizontal") {
        assert.equal(rect.leftFraction, expected.startFraction);
        assert.equal(rect.leftPx, expected.startPx);
        assert.equal(rect.widthFraction, expected.extentFraction);
        assert.equal(rect.widthPx, expected.extentPx);
        // The cross axis still spans the stage.
        assert.deepEqual(
          [rect.topFraction, rect.topPx, rect.heightFraction, rect.heightPx],
          [0, 0, 1, 0],
        );
      } else {
        assert.equal(rect.topFraction, expected.startFraction);
        assert.equal(rect.topPx, expected.startPx);
        assert.equal(rect.heightFraction, expected.extentFraction);
        assert.equal(rect.heightPx, expected.extentPx);
        assert.deepEqual(
          [rect.leftFraction, rect.leftPx, rect.widthFraction, rect.widthPx],
          [0, 0, 1, 0],
        );
      }
    });
    // Dividers land where the legacy cumulative-offset math put them.
    assert.equal(layout.dividers.length, 2);
    layout.dividers.forEach((divider, index) => {
      const fractions = splitFractions(flat);
      const offset = fractions.slice(0, index + 1).reduce((sum, value) => sum + value, 0);
      const totalGutter = (flat.paneIds.length - 1) * GUTTER;
      const startPx = -offset * totalGutter + index * GUTTER;
      if (axis === "horizontal") {
        assert.equal(divider.rect.leftFraction, offset);
        assert.equal(divider.rect.leftPx, startPx);
        assert.equal(divider.rect.widthPx, GUTTER);
      } else {
        assert.equal(divider.rect.topFraction, offset);
        assert.equal(divider.rect.topPx, startPx);
        assert.equal(divider.rect.heightPx, GUTTER);
      }
    });
  }
});

test("paneSplitLayout tiles a 2x2 with a gutter at each level", () => {
  const layout = paneSplitLayout(
    nestedSplit(
      branchNode("horizontal", [
        branchNode("vertical", [paneNode("a", 0.5), paneNode("b", 0.5)], 0.5),
        branchNode("vertical", [paneNode("c", 0.5), paneNode("d", 0.5)], 0.5),
      ]),
    ),
    GUTTER,
  );
  const stage = { width: 1000, height: 1000 };
  const box = (paneId: string) => {
    const rect = layout.panes.get(paneId);
    assert.ok(rect, paneId);
    const pixels = splitRectPixels(rect, stage);
    return [pixels.left, pixels.top, pixels.width, pixels.height];
  };

  assert.deepEqual(box("a"), [0, 0, 496, 496]);
  assert.deepEqual(box("b"), [0, 504, 496, 496]);
  assert.deepEqual(box("c"), [504, 0, 496, 496]);
  assert.deepEqual(box("d"), [504, 504, 496, 496]);

  // Three dividers: one full-height between the columns, one inside each column
  // spanning only that column's width.
  assert.equal(layout.dividers.length, 3);
  const pixelDividers = layout.dividers.map((divider) => ({
    path: divider.path,
    axis: divider.axis,
    ...splitRectPixels(divider.rect, stage),
  }));
  const outer = pixelDividers.find((divider) => divider.path === "");
  assert.deepEqual(
    [outer?.left, outer?.top, outer?.width, outer?.height],
    [496, 0, 8, 1000],
  );
  const innerLeft = pixelDividers.find((divider) => divider.path === "0");
  assert.deepEqual(
    [innerLeft?.left, innerLeft?.top, innerLeft?.width, innerLeft?.height],
    [0, 496, 496, 8],
  );
  const innerRight = pixelDividers.find((divider) => divider.path === "1");
  assert.deepEqual(
    [innerRight?.left, innerRight?.top, innerRight?.width, innerRight?.height],
    [504, 496, 496, 8],
  );
});

test("paneSplitLayout gives unequal nested fractions the leftover pixels", () => {
  const layout = paneSplitLayout(
    nestedSplit(
      branchNode("horizontal", [
        paneNode("a", 0.25),
        branchNode("vertical", [paneNode("b", 0.75), paneNode("c", 0.25)], 0.75),
      ]),
    ),
    GUTTER,
  );
  const stage = { width: 1000, height: 1000 };
  const a = splitRectPixels(layout.panes.get("a")!, stage);
  const b = splitRectPixels(layout.panes.get("b")!, stage);
  const c = splitRectPixels(layout.panes.get("c")!, stage);
  // 1000 - 8 = 992 of content; a takes a quarter of it.
  assert.equal(a.width, 248);
  assert.equal(b.left, 256);
  assert.equal(b.width, 744);
  // The column's own 992px of vertical content splits 3:1.
  assert.equal(b.height, 744);
  assert.equal(c.top, 752);
  assert.equal(c.height, 248);
});

test("paneAtStagePoint distinguishes nested panes a single axis cannot", () => {
  const layout = paneSplitLayout(
    nestedSplit(
      branchNode("horizontal", [
        branchNode("vertical", [paneNode("a", 0.5), paneNode("b", 0.5)], 0.5),
        paneNode("c", 0.5),
      ]),
    ),
    GUTTER,
  );
  const stage = { width: 1000, height: 1000 };
  // `a` and `b` share every column of x; only y tells them apart.
  assert.equal(paneAtStagePoint(layout, stage, 100, 100), "a");
  assert.equal(paneAtStagePoint(layout, stage, 100, 900), "b");
  assert.equal(paneAtStagePoint(layout, stage, 900, 100), "c");
  assert.equal(paneAtStagePoint(layout, stage, 900, 900), "c");
  // Inside the outer gutter, between panes.
  assert.equal(paneAtStagePoint(layout, stage, 500, 500), null);
});

test("normalizePaneSplitsForPanes keeps a nested tree and derives its sizes", () => {
  const nested = nestedSplit(
    branchNode("horizontal", [
      paneNode("pane-1", 0.5),
      branchNode("vertical", [paneNode("pane-2", 0.5), paneNode("pane-3", 0.5)], 0.5),
    ]),
  );
  const [normalized] = normalizePaneSplitsForPanes(
    [nested],
    panes(["pane-1", "pane-2", "pane-3"]),
  );

  assert.ok(normalized.root);
  assert.equal(paneSplitIsNested(normalized), true);
  assert.equal(normalized.axis, "horizontal");
  assert.equal(splitNodePaneIds(normalized.root).join(","), "pane-1,pane-2,pane-3");
  // Each leaf's share of its own parent, mirrored for older builds.
  assert.deepEqual(normalized.sizes, { "pane-1": 0.5, "pane-2": 0.5, "pane-3": 0.5 });
});

test("normalizePaneSplitsForPanes is idempotent for a nested split", () => {
  const once = normalizePaneSplitsForPanes(
    [
      nestedSplit(
        branchNode("horizontal", [
          paneNode("pane-1", 0.4),
          branchNode("vertical", [paneNode("pane-2", 0.3), paneNode("pane-3", 0.7)], 0.6),
        ]),
      ),
    ],
    panes(["pane-1", "pane-2", "pane-3"]),
  );
  const twice = normalizePaneSplitsForPanes(once, panes(["pane-1", "pane-2", "pane-3"]));

  // The pane-change effect persists whenever normalization is not a fixed point.
  assert.equal(paneSplitsEqual(once, twice), true);
});

test("normalizePaneSplitsForPanes keeps nesting when a pane in a stack closes", () => {
  const [normalized] = normalizePaneSplitsForPanes(
    [
      nestedSplit(
        branchNode("horizontal", [
          branchNode("vertical", [paneNode("pane-1", 0.5), paneNode("pane-2", 0.5)], 0.5),
          branchNode("vertical", [paneNode("pane-3", 0.5), paneNode("pane-4", 0.5)], 0.5),
        ]),
      ),
    ],
    panes(["pane-1", "pane-2", "pane-3"]),
  );

  assert.deepEqual(normalized.paneIds, ["pane-1", "pane-2", "pane-3"]);
  assert.ok(normalized.root);
  // The emptied column collapses to its survivor; the other stack is untouched.
  assert.equal(normalized.axis, "horizontal");
  assert.equal(splitNodePaneIds(normalized.root).join(","), "pane-1,pane-2,pane-3");
});

test("normalizePaneSplitsForPanes flips the root axis when a collapse leaves one branch", () => {
  const [normalized] = normalizePaneSplitsForPanes(
    [
      nestedSplit(
        branchNode("horizontal", [
          branchNode("vertical", [paneNode("pane-1", 0.5), paneNode("pane-2", 0.5)], 0.5),
          paneNode("pane-3", 0.5),
        ]),
      ),
    ],
    panes(["pane-1", "pane-2"]),
  );

  // Only the stacked pair is left, so the split is now a plain stack — and flat,
  // because a tree with no branch children is exactly a flat split.
  assert.equal(normalized.axis, undefined);
  assert.equal(normalized.root, undefined);
  assert.equal(paneSplitIsNested(normalized), false);
  assert.deepEqual(normalized.paneIds, ["pane-1", "pane-2"]);
});

test("normalizePaneSplitsForPanes merges same-axis nesting into one branch", () => {
  const [normalized] = normalizePaneSplitsForPanes(
    [
      nestedSplit(
        branchNode("horizontal", [
          paneNode("pane-1", 0.5),
          branchNode("horizontal", [paneNode("pane-2", 0.5), paneNode("pane-3", 0.5)], 0.5),
        ]),
      ),
    ],
    panes(["pane-1", "pane-2", "pane-3"]),
  );

  // Three columns, one representation: `root` is dropped because the merged tree
  // has no branch children left.
  assert.equal(normalized.root, undefined);
  assert.equal(normalized.axis, "horizontal");
  assert.deepEqual(normalized.sizes, {
    "pane-1": 0.5,
    "pane-2": 0.25,
    "pane-3": 0.25,
  });
});

test("normalizePaneSplitsForPanes drops a tree that disagrees with the tab order", () => {
  const mismatched = nestedSplit(
    branchNode("horizontal", [
      paneNode("pane-1", 0.5),
      branchNode("vertical", [paneNode("pane-3", 0.5), paneNode("pane-2", 0.5)], 0.5),
    ]),
  );
  mismatched.paneIds = ["pane-1", "pane-2", "pane-3"];
  const [normalized] = normalizePaneSplitsForPanes(
    [mismatched],
    panes(["pane-1", "pane-2", "pane-3"]),
  );

  // Out-of-order leaves would let geometry and the sidebar disagree. Degrade to
  // flat instead of dropping the split.
  assert.equal(normalized.root, undefined);
  assert.deepEqual(normalized.paneIds, ["pane-1", "pane-2", "pane-3"]);
});

test("normalizePaneSplitsForPanes rejects malformed and over-deep trees", () => {
  const cases: unknown[] = [
    { kind: "pane", paneId: "pane-1" },
    { kind: "split", axis: "horizontal", children: [] },
    { kind: "split", axis: "horizontal", children: [{ kind: "bogus" }] },
    "not-a-node",
    null,
  ];
  for (const root of cases) {
    const [normalized] = normalizePaneSplitsForPanes(
      [{ ...split(["pane-1", "pane-2"]), root: root as PaneSplitNode }],
      panes(["pane-1", "pane-2"]),
    );
    assert.equal(normalized?.root, undefined, JSON.stringify(root));
    assert.deepEqual(normalized?.paneIds, ["pane-1", "pane-2"]);
  }

  // 20 levels deep: past the ceiling, so it degrades to flat rather than
  // recursing on a corrupt file.
  let deep: PaneSplitNode = paneNode("pane-2", 0.5);
  for (let level = 0; level < 20; level += 1) {
    deep = branchNode(level % 2 === 0 ? "vertical" : "horizontal", [deep], 1);
  }
  const [deepNormalized] = normalizePaneSplitsForPanes(
    [
      {
        ...split(["pane-1", "pane-2"]),
        root: branchNode("horizontal", [paneNode("pane-1", 0.5), deep]),
      },
    ],
    panes(["pane-1", "pane-2"]),
  );
  assert.equal(deepNormalized?.root, undefined);
});

test("joinPaneSplit nests when the requested axis crosses the anchor's branch", () => {
  const existing = normalizePaneSplitsForPanes(
    [{ ...split(["pane-1", "pane-2"]), axis: "horizontal" }],
    panes(["pane-1", "pane-2"]),
  );
  // ⌘D on the left column of a two-column split: stack inside that column.
  const [joined] = normalizePaneSplitsForPanes(
    joinPaneSplit(existing, panes(["pane-1", "pane-3", "pane-2"]), "pane-1", "pane-3", {
      insertedPaneId: "pane-3",
      source: "command",
      axis: "vertical",
      nestAxis: "vertical",
    }),
    panes(["pane-1", "pane-3", "pane-2"]),
  );

  assert.equal(joined.axis, "horizontal");
  assert.ok(joined.root);
  assert.equal(joined.root.kind, "split");
  assert.deepEqual(joined.paneIds, ["pane-1", "pane-3", "pane-2"]);
  const [first, second] = (joined.root as { children: PaneSplitNode[] }).children;
  assert.equal(first.kind, "split");
  assert.deepEqual(splitNodePaneIds(first), ["pane-1", "pane-3"]);
  assert.equal(second.kind, "pane");
});

test("joinPaneSplit appends along the anchor's own branch instead of nesting", () => {
  const nested = normalizePaneSplitsForPanes(
    [
      nestedSplit(
        branchNode("horizontal", [
          branchNode("vertical", [paneNode("pane-1", 0.5), paneNode("pane-2", 0.5)], 0.5),
          paneNode("pane-3", 0.5),
        ]),
      ),
    ],
    panes(["pane-1", "pane-2", "pane-3"]),
  );
  // ⌘D on pane-2, already in a stack: a third row, not a nested pair.
  const [joined] = normalizePaneSplitsForPanes(
    joinPaneSplit(
      nested,
      panes(["pane-1", "pane-2", "pane-4", "pane-3"]),
      "pane-2",
      "pane-4",
      {
        insertedPaneId: "pane-4",
        source: "command",
        axis: "vertical",
        nestAxis: "vertical",
      },
    ),
    panes(["pane-1", "pane-2", "pane-4", "pane-3"]),
  );

  const children = (joined.root as { children: PaneSplitNode[] }).children;
  assert.equal(children.length, 2);
  assert.deepEqual(splitNodePaneIds(children[0]), ["pane-1", "pane-2", "pane-4"]);
  // Equal siblings each yield a share of the newcomer's slot: thirds, matching
  // what a flat ⌘D has always produced.
  const stack = children[0] as { children: PaneSplitNode[] };
  for (const child of stack.children) {
    assert.ok(Math.abs((child.size ?? 0) - 1 / 3) < 1e-12);
  }
});

test("joinPaneSplit falls back to flat when two separate splits merge", () => {
  const splits = normalizePaneSplitsForPanes(
    [
      nestedSplit(
        branchNode("horizontal", [
          branchNode("vertical", [paneNode("pane-1", 0.5), paneNode("pane-2", 0.5)], 0.5),
          paneNode("pane-3", 0.5),
        ]),
        "split-a",
      ),
      { ...split(["pane-4", "pane-5"]), id: "split-b" },
    ],
    panes(["pane-1", "pane-2", "pane-3", "pane-4", "pane-5"]),
  );
  const [joined] = normalizePaneSplitsForPanes(
    joinPaneSplit(
      splits,
      panes(["pane-1", "pane-2", "pane-3", "pane-4", "pane-5"]),
      "pane-3",
      "pane-4",
    ),
    panes(["pane-1", "pane-2", "pane-3", "pane-4", "pane-5"]),
  );

  assert.deepEqual(joined.paneIds, [
    "pane-1",
    "pane-2",
    "pane-3",
    "pane-4",
    "pane-5",
  ]);
  assert.equal(joined.root, undefined);
});

test("resizeSplitNodeFractions moves only the target branch's children", () => {
  const nested = nestedSplit(
    branchNode("horizontal", [
      branchNode("vertical", [paneNode("a", 0.5), paneNode("b", 0.5)], 0.5),
      paneNode("c", 0.5),
    ]),
  );
  const resized = resizeSplitNodeFractions(nested, "0", 0, 0.2);
  const children = (resized.root as { children: PaneSplitNode[] }).children;
  const stack = children[0] as { children: PaneSplitNode[]; size?: number };

  assert.ok(Math.abs((stack.children[0].size ?? 0) - 0.7) < 1e-12);
  assert.ok(Math.abs((stack.children[1].size ?? 0) - 0.3) < 1e-12);
  // The outer split is untouched.
  assert.equal(stack.size, 0.5);
  assert.equal(children[1].size, 0.5);
});

test("resizeSplitNodeFractions clamps at the per-pane floor and ignores bad paths", () => {
  const nested = nestedSplit(
    branchNode("horizontal", [
      branchNode("vertical", [paneNode("a", 0.5), paneNode("b", 0.5)], 0.5),
      paneNode("c", 0.5),
    ]),
  );
  const clamped = resizeSplitNodeFractions(nested, "0", 0, -5);
  const stack = (clamped.root as { children: PaneSplitNode[] }).children[0] as {
    children: PaneSplitNode[];
  };
  assert.ok((stack.children[0].size ?? 0) >= 0.12 - 1e-12);

  assert.equal(resizeSplitNodeFractions(nested, "9", 0, 0.1), nested);
  assert.equal(resizeSplitNodeFractions(nested, "0", 4, 0.1), nested);
});

test("resizeSplitNodeFractions still resizes a flat split through sizes", () => {
  const flat = { ...split(["pane-1", "pane-2"]), sizes: { "pane-1": 0.5, "pane-2": 0.5 } };
  const resized = resizeSplitNodeFractions(flat, "", 0, 0.25);

  assert.ok(Math.abs(resized.sizes["pane-1"] - 0.75) < 1e-12);
  assert.equal(resized.root, undefined);
});

test("withPaneSplitAxis transposes a nested tree instead of flattening it", () => {
  const nested = nestedSplit(
    branchNode("horizontal", [
      branchNode("vertical", [paneNode("a", 0.5), paneNode("b", 0.5)], 0.5),
      paneNode("c", 0.5),
    ]),
  );
  const [rotated] = normalizePaneSplitsForPanes(
    [togglePaneSplitAxis(nested)],
    panes(["a", "b", "c"]),
  );

  // Flipping only the root would make it share an axis with its child, which
  // normalization then merges away — losing the nesting entirely.
  assert.equal(rotated.axis, undefined);
  assert.ok(rotated.root);
  const children = (rotated.root as { axis: string; children: PaneSplitNode[] }).children;
  assert.equal((rotated.root as { axis: string }).axis, "vertical");
  assert.equal((children[0] as { axis: string }).axis, "horizontal");
  assert.deepEqual(splitNodePaneIds(rotated.root), ["a", "b", "c"]);
});

test("reservedTerminalStageWidth charges a nested layout only for its widest row", () => {
  const columnOfStacks = branchNode("horizontal", [
    branchNode("vertical", [paneNode("a"), paneNode("b")]),
    paneNode("c"),
  ]);
  const stackOfColumns = branchNode("vertical", [
    branchNode("horizontal", [paneNode("a"), paneNode("b"), paneNode("c")]),
    paneNode("d"),
  ]);
  const common = {
    axis: "horizontal" as const,
    paneCount: 3,
    minWidth: 380,
    splitMinWidth: 200,
    gutter: 8,
  };

  // Two columns wide, not three panes wide: the stack shares one column.
  assert.equal(reservedTerminalStageWidth({ ...common, root: columnOfStacks }), 408);
  // Three columns in the widest row, and the lone pane below does not add width.
  assert.equal(reservedTerminalStageWidth({ ...common, root: stackOfColumns }), 616);
  // Never below the single-pane floor.
  assert.equal(
    reservedTerminalStageWidth({
      ...common,
      root: branchNode("vertical", [paneNode("a"), paneNode("b")]),
    }),
    380,
  );
  // Flat splits keep the old arithmetic.
  assert.equal(reservedTerminalStageWidth(common), 616);
});

test("canSplitPaneInTree refuses a split that would breach the pane floor", () => {
  const stage = { width: 1000, height: 1000 };
  const guard = {
    stage,
    gutter: GUTTER,
    minWidth: 200,
    minHeight: 140,
  };
  const twoColumns = normalizePaneSplitsForPanes(
    [{ ...split(["a", "b"]), axis: "horizontal" }],
    panes(["a", "b"]),
  )[0];

  // A 496px column nests into two 244px halves: fine.
  assert.equal(
    canSplitPaneInTree({ ...guard, split: twoColumns, paneId: "a", axis: "horizontal" }),
    true,
  );
  // Same split on a narrow stage: appending a third column leaves 328px of
  // content per pane... but at 700px it does not.
  assert.equal(
    canSplitPaneInTree({
      ...guard,
      stage: { width: 700, height: 1000 },
      split: twoColumns,
      paneId: "a",
      axis: "horizontal",
    }),
    true,
  );
  assert.equal(
    canSplitPaneInTree({
      ...guard,
      stage: { width: 560, height: 1000 },
      split: twoColumns,
      paneId: "a",
      axis: "horizontal",
    }),
    false,
  );
  // Crossing the axis subdivides only the source pane, so its own height rules.
  assert.equal(
    canSplitPaneInTree({
      ...guard,
      stage: { width: 1000, height: 240 },
      split: twoColumns,
      paneId: "a",
      axis: "vertical",
    }),
    false,
  );
  assert.equal(
    canSplitPaneInTree({
      ...guard,
      stage: { width: 1000, height: 300 },
      split: twoColumns,
      paneId: "a",
      axis: "vertical",
    }),
    true,
  );
  // An unknown pane or a collapsed stage is never splittable.
  assert.equal(
    canSplitPaneInTree({ ...guard, split: twoColumns, paneId: "zz", axis: "vertical" }),
    false,
  );
  assert.equal(
    canSplitPaneInTree({
      ...guard,
      stage: { width: 0, height: 0 },
      split: twoColumns,
      paneId: "a",
      axis: "vertical",
    }),
    false,
  );
});

test("splitAxisForPane and splitBranchChildCountForPane report the pane's own branch", () => {
  const nested = nestedSplit(
    branchNode("horizontal", [
      branchNode("vertical", [paneNode("a", 0.5), paneNode("b", 0.5)], 0.5),
      paneNode("c", 0.5),
    ]),
  );

  assert.equal(splitAxisForPane(nested, "a"), "vertical");
  assert.equal(splitAxisForPane(nested, "c"), "horizontal");
  assert.equal(splitAxisForPane(nested, "missing"), null);
  assert.equal(splitBranchChildCountForPane(nested, "a"), 2);
  assert.equal(splitBranchChildCountForPane(nested, "c"), 2);
  assert.equal(splitBranchChildCountForPane(nested, "missing"), 1);

  // A flat split reports its single branch.
  const flat = { ...split(["pane-1", "pane-2", "pane-3"]), axis: "horizontal" as const };
  assert.equal(splitAxisForPane(flat, "pane-2"), "horizontal");
  assert.equal(splitBranchChildCountForPane(flat, "pane-2"), 3);
});

test("detachPaneFromSplitMemberships leaves a nested tree for normalization to prune", () => {
  const nested = nestedSplit(
    branchNode("horizontal", [
      branchNode("vertical", [paneNode("a", 0.5), paneNode("b", 0.5)], 0.5),
      paneNode("c", 0.5),
    ]),
  );
  const detached = detachPaneFromSplitMemberships([nested], "b");
  // A middle member cannot stay between the others without re-forming the split,
  // so the app lifts its tab past the remaining block first.
  const [normalized] = normalizePaneSplitsForPanes(detached, panes(["a", "c", "b"]));

  assert.deepEqual(normalized.paneIds, ["a", "c"]);
  // Two panes left in different columns: a plain two-column split.
  assert.equal(normalized.root, undefined);
  assert.equal(normalized.axis, "horizontal");
});

test("paneSplitRootNode ignores a tree that no longer matches its members", () => {
  const stale: PaneSplitInfo = {
    id: "split-1",
    paneIds: ["a", "c"],
    sizes: { a: 0.5, c: 0.5 },
    axis: "horizontal",
    root: branchNode("horizontal", [
      branchNode("vertical", [paneNode("a", 0.5), paneNode("b", 0.5)], 0.5),
      paneNode("c", 0.5),
    ]),
  };
  const root = paneSplitRootNode(stale);

  // An optimistic drag result can carry a detached pane. Rendering it would hand
  // stage space to a pane with no surface.
  assert.deepEqual(splitNodePaneIds(root), ["a", "c"]);
  assert.equal(root.children.every((child) => child.kind === "pane"), true);
});

test("joinPaneSplit inserts before the anchor when the newcomer leads the pair", () => {
  const nested = normalizePaneSplitsForPanes(
    [
      nestedSplit(
        branchNode("horizontal", [
          branchNode("vertical", [paneNode("a", 0.5), paneNode("b", 0.5)], 0.5),
          paneNode("c", 0.5),
        ]),
      ),
    ],
    panes(["a", "b", "c"]),
  );
  // A drag dropped on b's leading half: the tab lands before b, so the tree has
  // to insert before it too or the in-order check rejects the whole tree.
  const [joined] = normalizePaneSplitsForPanes(
    joinPaneSplit(nested, panes(["a", "d", "b", "c"]), "d", "b", {
      insertedPaneId: "d",
      source: "drag-half",
    }),
    panes(["a", "d", "b", "c"]),
  );

  assert.deepEqual(joined.paneIds, ["a", "d", "b", "c"]);
  assert.ok(joined.root, "nesting should survive a leading-half drop");
  assert.deepEqual(splitNodePaneIds(joined.root), ["a", "d", "b", "c"]);
  const children = (joined.root as { children: PaneSplitNode[] }).children;
  assert.deepEqual(splitNodePaneIds(children[0]), ["a", "d", "b"]);
});

test("a drag reordering inside its own nested split keeps the nesting", () => {
  const nested = normalizePaneSplitsForPanes(
    [
      nestedSplit(
        branchNode("horizontal", [
          branchNode("vertical", [paneNode("a", 0.5), paneNode("b", 0.5)], 0.5),
          branchNode("vertical", [paneNode("c", 0.5), paneNode("d", 0.5)], 0.5),
        ]),
      ),
    ],
    panes(["a", "b", "c", "d"]),
  );
  // Drag `a` onto `c`'s trailing half: detach from its own split, then rejoin.
  const detached = detachPaneFromSplitMemberships(nested, "a");
  const [joined] = normalizePaneSplitsForPanes(
    joinPaneSplit(detached, panes(["b", "c", "a", "d"]), "c", "a", {
      insertedPaneId: "a",
      source: "drag-half",
    }),
    panes(["b", "c", "a", "d"]),
  );

  assert.deepEqual(joined.paneIds, ["b", "c", "a", "d"]);
  // Detach has to prune the tree, or the rejoin sees a tree naming `a` twice and
  // falls back to flat.
  assert.ok(joined.root, "nesting should survive a within-split reorder");
  assert.deepEqual(splitNodePaneIds(joined.root), ["b", "c", "a", "d"]);
  const children = (joined.root as { children: PaneSplitNode[] }).children;
  assert.equal(children.length, 2);
  assert.deepEqual(splitNodePaneIds(children[0]), ["b"]);
  assert.deepEqual(splitNodePaneIds(children[1]), ["c", "a", "d"]);
});

test("detachPaneFromSplitMemberships prunes the tree it leaves behind", () => {
  const nested = nestedSplit(
    branchNode("horizontal", [
      branchNode("vertical", [paneNode("a", 0.5), paneNode("b", 0.5)], 0.5),
      branchNode("vertical", [paneNode("c", 0.5), paneNode("d", 0.5)], 0.5),
    ]),
  );
  const [detached] = detachPaneFromSplitMemberships([nested], "a");

  assert.deepEqual(detached.paneIds, ["b", "c", "d"]);
  assert.ok(detached.root);
  assert.deepEqual(splitNodePaneIds(detached.root), ["b", "c", "d"]);
});

test("split offsets never emit a signed calc operand", () => {
  assert.equal(splitCalc(0.5, -4), "calc(50% - 4px)");
  assert.equal(splitCalc(0.5, 4), "calc(50% + 4px)");
  assert.equal(splitCalc(0, 0), "calc(0% + 0px)");
  // Non-finite input must not produce an unparseable declaration; an invalid
  // string would drop the whole rule and leave the pane unpositioned.
  assert.equal(splitCalc(Number.NaN, Number.POSITIVE_INFINITY), "calc(0% + 0px)");

  const layout = paneSplitLayout(
    nestedSplit(
      branchNode("horizontal", [
        branchNode("vertical", [paneNode("a", 0.5), paneNode("b", 0.5)], 0.5),
        branchNode("vertical", [paneNode("c", 0.25), paneNode("d", 0.75)], 0.5),
      ]),
    ),
    GUTTER,
  );
  const declarations = [...layout.panes.values(), ...layout.dividers.map((d) => d.rect)]
    .flatMap((rect) => Object.values(splitRectOffsets(rect)));
  assert.ok(declarations.length > 0);
  for (const declaration of declarations) {
    // `calc(50% + -4px)` parses per spec, but this renders in a WKWebView and
    // the pre-nesting styles always used the subtraction form.
    assert.ok(!declaration.includes("+ -"), declaration);
    assert.ok(/^calc\(-?[\d.e+-]+% [+-] [\d.e+-]+px\)$/.test(declaration), declaration);
  }

  // A flat split still emits the same numbers the legacy formula produced.
  const flat = { ...split(["pane-1", "pane-2"]), axis: "horizontal" as const };
  const flatOffsets = splitRectOffsets(paneSplitLayout(flat, GUTTER).panes.get("pane-2")!);
  assert.equal(flatOffsets.left, "calc(50% + 4px)");
  assert.equal(flatOffsets.width, "calc(50% - 4px)");
  assert.equal(flatOffsets.top, "calc(0% + 0px)");
  assert.equal(flatOffsets.height, "calc(100% + 0px)");
});

test("joinPaneSplit nests a column inside a stacked split (⌘⇧D on a stack)", () => {
  // The headline new capability: ⌘⇧D used to no-op on a stacked split.
  const stacked = normalizePaneSplitsForPanes(
    [split(["pane-1", "pane-2"])],
    panes(["pane-1", "pane-2"]),
  );
  const [joined] = normalizePaneSplitsForPanes(
    joinPaneSplit(stacked, panes(["pane-1", "pane-3", "pane-2"]), "pane-1", "pane-3", {
      insertedPaneId: "pane-3",
      source: "command",
      axis: "horizontal",
      nestAxis: "horizontal",
    }),
    panes(["pane-1", "pane-3", "pane-2"]),
  );

  // Still a stack at the root; its first row is now two columns.
  assert.equal(joined.axis, undefined);
  assert.ok(joined.root);
  assert.equal((joined.root as { axis: string }).axis, "vertical");
  const children = (joined.root as { children: PaneSplitNode[] }).children;
  assert.equal(children[0].kind, "split");
  assert.equal((children[0] as { axis: string }).axis, "horizontal");
  assert.deepEqual(splitNodePaneIds(children[0]), ["pane-1", "pane-3"]);
  assert.equal(children[1].kind, "pane");
  assert.deepEqual(joined.paneIds, ["pane-1", "pane-3", "pane-2"]);

  // The nested pair splits the row it replaced, so the untouched pane keeps its
  // share of the stack.
  assert.equal(children[1].size, 0.5);
});

test("normalizePaneSplitsForPanes drops a tree naming a pane twice", () => {
  const [normalized] = normalizePaneSplitsForPanes(
    [
      {
        ...split(["pane-1", "pane-2"]),
        root: branchNode("horizontal", [
          paneNode("pane-1", 0.5),
          branchNode("vertical", [paneNode("pane-2", 0.5), paneNode("pane-2", 0.5)], 0.5),
        ]),
      },
    ],
    panes(["pane-1", "pane-2"]),
  );

  // A duplicated leaf would give one pane two rectangles.
  assert.equal(normalized.root, undefined);
  assert.deepEqual(normalized.paneIds, ["pane-1", "pane-2"]);
});

/* ------------------------------------------------------------------------- *
 * Property test: random gesture sequences must never break the invariants the
 * renderer and the sidebar both rely on.
 * ------------------------------------------------------------------------- */

/** Deterministic LCG so a failure is reproducible from its seed. */
function rng(seed: number) {
  let state = seed >>> 0;
  return () => {
    state = (state * 1664525 + 1013904223) >>> 0;
    return state / 0x100000000;
  };
}

const MIN_PANE_WIDTH = 200;
const MIN_PANE_HEIGHT = 140;

type Stage = { width: number; height: number };

function assertLayoutTiles(split: PaneSplitInfo, stage: Stage, label: string) {
  const layout = paneSplitLayout(split, GUTTER);
  const boxes = new Map(
    [...layout.panes].map(([id, rect]) => [id, splitRectPixels(rect, stage)] as const),
  );
  assert.equal(boxes.size, split.paneIds.length, `${label}: every pane placed`);

  for (const [paneId, box] of boxes) {
    // Every pane the app allows must stay usable, not merely non-overlapping.
    assert.ok(box.width > 0 && box.height > 0, `${label}: ${paneId} has area`);
    assert.ok(
      box.left >= -0.001 &&
        box.top >= -0.001 &&
        box.right <= stage.width + 0.001 &&
        box.bottom <= stage.height + 0.001,
      `${label}: ${paneId} inside the stage`,
    );
  }

  // No two panes may overlap, or one terminal would be drawn over another.
  const entries = [...boxes.entries()];
  for (let i = 0; i < entries.length; i += 1) {
    for (let j = i + 1; j < entries.length; j += 1) {
      const [aId, a] = entries[i];
      const [bId, b] = entries[j];
      const overlaps =
        a.left < b.right - 0.001 &&
        b.left < a.right - 0.001 &&
        a.top < b.bottom - 0.001 &&
        b.top < a.bottom - 0.001;
      assert.ok(!overlaps, `${label}: ${aId} overlaps ${bId}`);
    }
  }

  // Every branch's box is exactly the bounding box of the panes beneath it, so a
  // divider and its resize mask land on their own branch and nowhere else.
  const root = paneSplitRootNode(split);
  for (const [path, branch] of layout.branches) {
    const node = splitNodeAtPath(root, path);
    assert.ok(node, `${label}: branch ${path} resolves`);
    const members = splitNodePaneIds(node).map((paneId) => boxes.get(paneId));
    assert.ok(members.every(Boolean), `${label}: branch ${path} members placed`);
    const union = members.reduce(
      (acc, box) => ({
        left: Math.min(acc.left, box!.left),
        top: Math.min(acc.top, box!.top),
        right: Math.max(acc.right, box!.right),
        bottom: Math.max(acc.bottom, box!.bottom),
      }),
      {
        left: Number.POSITIVE_INFINITY,
        top: Number.POSITIVE_INFINITY,
        right: Number.NEGATIVE_INFINITY,
        bottom: Number.NEGATIVE_INFINITY,
      },
    );
    const own = splitRectPixels(branch.rect, stage);
    for (const side of ["left", "top", "right", "bottom"] as const) {
      assert.ok(
        Math.abs(union[side] - own[side]) < 0.01,
        `${label}: branch ${path} ${side} ${union[side]} vs ${own[side]}`,
      );
    }
  }
}

function assertSplitInvariants(
  splits: PaneSplitInfo[],
  paneIds: string[],
  stage: Stage,
  label: string,
) {
  const seen = new Set<string>();
  for (const split of splits) {
    assert.ok(split.paneIds.length >= 2, `${label}: split has two or more panes`);
    for (const paneId of split.paneIds) {
      assert.ok(!seen.has(paneId), `${label}: ${paneId} in one split only`);
      seen.add(paneId);
      assert.ok(paneIds.includes(paneId), `${label}: ${paneId} still exists`);
    }
    if (split.root) {
      // The invariant the whole design rests on: geometry order == tab order.
      assert.deepEqual(
        splitNodePaneIds(split.root),
        split.paneIds,
        `${label}: leaves match tabs`,
      );
      assert.ok(
        (split.root as { children: PaneSplitNode[] }).children.some(
          (child) => child.kind === "split",
        ),
        `${label}: a stored tree is really nested`,
      );
      assert.equal(
        (split.root as { axis: string }).axis,
        paneSplitAxis(split),
        `${label}: axis mirrors the root`,
      );
    }
    assertLayoutTiles(split, stage, label);
  }
  // Normalization has to be a fixed point or the pane-change effect persists on
  // every pass.
  assert.equal(
    paneSplitsEqual(splits, normalizePaneSplitsForPanes(splits, panes(paneIds))),
    true,
    `${label}: normalization is a fixed point`,
  );
}

/** Drives the gestures the app performs, refusing a split exactly where
 * `splitTerminal` would, and asserting the invariants after every step. The
 * property under test is that any layout the app *lets* you build still tiles
 * the stage with usable panes. */
function runGestureSequence(seed: number, steps: number, maxPanes: number, stage: Stage) {
  const next = rng(seed);
  let paneIds = ["p0"];
  let splits: PaneSplitInfo[] = [];
  let counter = 1;
  const trace: string[] = [];
  const pick = <T,>(items: T[]) => items[Math.floor(next() * items.length)];

  for (let step = 0; step < steps; step += 1) {
    const roll = next();

    if (roll < 0.55 && paneIds.length < maxPanes) {
      const anchor = pick(paneIds);
      const axis: "vertical" | "horizontal" = next() < 0.5 ? "vertical" : "horizontal";
      const anchorSplit = splits.find((split) => split.paneIds.includes(anchor));
      if (
        anchorSplit &&
        !canSplitPaneInTree({
          split: anchorSplit,
          paneId: anchor,
          axis,
          stage,
          gutter: GUTTER,
          minWidth: MIN_PANE_WIDTH,
          minHeight: MIN_PANE_HEIGHT,
        })
      ) {
        trace.push(`refused ${anchor} ${axis}`);
        continue;
      }
      const inserted = `p${counter}`;
      counter += 1;
      const at = paneIds.indexOf(anchor);
      paneIds = [...paneIds.slice(0, at + 1), inserted, ...paneIds.slice(at + 1)];
      splits = normalizePaneSplitsForPanes(
        joinPaneSplit(splits, panes(paneIds), anchor, inserted, {
          insertedPaneId: inserted,
          source: "command",
          axis,
          nestAxis: axis,
        }),
        panes(paneIds),
      );
      trace.push(`split ${anchor} ${axis} -> ${inserted}`);
    } else if (roll < 0.75 && paneIds.length > 1) {
      const closing = pick(paneIds);
      paneIds = paneIds.filter((paneId) => paneId !== closing);
      splits = normalizePaneSplitsForPanes(splits, panes(paneIds));
      trace.push(`close ${closing}`);
    } else if (roll < 0.88 && splits.length > 0) {
      const target = pick(splits);
      splits = normalizePaneSplitsForPanes(
        splits.map((split) => (split.id === target.id ? togglePaneSplitAxis(split) : split)),
        panes(paneIds),
      );
      trace.push(`rotate ${target.id}`);
    } else if (splits.length > 0) {
      const target = pick(splits);
      const branch = pick([...paneSplitLayout(target, GUTTER).branches.values()]);
      const index = Math.floor(next() * Math.max(1, branch.fractions.length - 1));
      const delta = (next() - 0.5) * 1.6;
      splits = normalizePaneSplitsForPanes(
        splits.map((split) =>
          split.id === target.id
            ? resizeSplitNodeFractions(split, branch.path, index, delta)
            : split,
        ),
        panes(paneIds),
      );
      trace.push(`resize ${target.id} ${branch.path}#${index} ${delta.toFixed(2)}`);
    }

    assertSplitInvariants(
      splits,
      paneIds,
      stage,
      `seed ${seed} step ${step}\n  ${trace.join("\n  ")}`,
    );
  }
}

test("random gesture sequences keep every layout invariant on a roomy stage", () => {
  for (let seed = 1; seed <= 400; seed += 1) {
    runGestureSequence(seed, 18, 10, { width: 1440, height: 900 });
  }
});

test("random gesture sequences keep every layout invariant on a cramped stage", () => {
  // Small enough that the split guard does most of the work: nesting depth is
  // bounded by screen area, so these sequences refuse far more often.
  for (let seed = 1; seed <= 400; seed += 1) {
    runGestureSequence(seed, 26, 12, { width: 620, height: 420 });
  }
});
