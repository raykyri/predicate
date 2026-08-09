import type { SidebarMode } from "./sidebarMode";

export type SidebarScrollRegion = "terminal" | "research" | "researchTerminals";

export function sidebarScrollRegionsForMode(mode: SidebarMode): SidebarScrollRegion[] {
  return mode === "research" ? ["research", "researchTerminals"] : ["terminal"];
}

export function activeSidebarScrollRegion(
  mode: SidebarMode,
  activeSurface: "pane" | "research",
): SidebarScrollRegion {
  if (mode === "terminal") {
    return "terminal";
  }
  return activeSurface === "research" ? "research" : "researchTerminals";
}

export type LeftSidebarRestorePlacement =
  | { kind: "hidden" }
  | { kind: "research-header" }
  | { kind: "floating" }
  | { kind: "turn-pane-header" }
  | { kind: "split-turn-pane"; paneId: string };

interface LeftSidebarRestorePlacementInput {
  leftSidebarCollapsed: boolean;
  researchHeaderOwnsRestore: boolean;
  splitRightPaneMode: boolean;
  activePaneId?: string | null;
  visibleRightBarPaneIds: readonly string[];
}

// Keep the restore control attached to existing pane chrome whenever possible.
// A split has several floating close controls, so the active pane owns the
// restore button when it has a right bar; otherwise the first visible right bar
// is the stable fallback.
export function leftSidebarRestorePlacement({
  leftSidebarCollapsed,
  researchHeaderOwnsRestore,
  splitRightPaneMode,
  activePaneId,
  visibleRightBarPaneIds,
}: LeftSidebarRestorePlacementInput): LeftSidebarRestorePlacement {
  if (!leftSidebarCollapsed) {
    return { kind: "hidden" };
  }
  if (researchHeaderOwnsRestore) {
    return { kind: "research-header" };
  }
  if (visibleRightBarPaneIds.length === 0) {
    return { kind: "floating" };
  }
  if (!splitRightPaneMode) {
    return { kind: "turn-pane-header" };
  }
  const paneId =
    activePaneId && visibleRightBarPaneIds.includes(activePaneId)
      ? activePaneId
      : visibleRightBarPaneIds[0];
  return paneId ? { kind: "split-turn-pane", paneId } : { kind: "floating" };
}
