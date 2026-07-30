import assert from "node:assert/strict";
import test from "node:test";
import { leftSidebarRestorePlacement } from "../src/lib/sidebarControls";

const placement = (
  overrides: Partial<Parameters<typeof leftSidebarRestorePlacement>[0]> = {},
) =>
  leftSidebarRestorePlacement({
    leftSidebarCollapsed: true,
    researchHeaderOwnsRestore: false,
    splitRightPaneMode: false,
    activePaneId: "pane-1",
    visibleRightBarPaneIds: ["pane-1"],
    ...overrides,
  });

test("groups the left restore control into the open right-pane header", () => {
  assert.deepEqual(placement(), { kind: "turn-pane-header" });
});

test("groups the left restore control with the active split pane controls", () => {
  assert.deepEqual(
    placement({
      splitRightPaneMode: true,
      activePaneId: "pane-2",
      visibleRightBarPaneIds: ["pane-1", "pane-2"],
    }),
    { kind: "split-turn-pane", paneId: "pane-2" },
  );
});

test("uses the first visible split right bar when the active pane has none", () => {
  assert.deepEqual(
    placement({
      splitRightPaneMode: true,
      activePaneId: "shell-only",
      visibleRightBarPaneIds: ["pane-1", "pane-2"],
    }),
    { kind: "split-turn-pane", paneId: "pane-1" },
  );
});

test("keeps the standalone and research placements for layouts without a right bar", () => {
  assert.deepEqual(placement({ visibleRightBarPaneIds: [] }), { kind: "floating" });
  assert.deepEqual(placement({ researchHeaderOwnsRestore: true }), {
    kind: "research-header",
  });
  assert.deepEqual(placement({ leftSidebarCollapsed: false }), { kind: "hidden" });
});
