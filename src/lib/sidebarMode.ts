import type { GroupInfo, PaneInfo, ResearchTreeSummary } from "../types";
import { type ResearchFolderScope, workspaceIsInResearchScope } from "./researchScope";
import { panesForScope } from "./workspaceScope";

export type SidebarMode = "terminal" | "research" | "encyclopedia";

export const SIDEBAR_MODE_STORAGE_KEY = "qmux.sidebar-mode.v1";
const RESEARCH_TREE_TAB_PREFIX = "__research_tree__:";

export function researchTreeTabId(treeId: string): string {
  return `${RESEARCH_TREE_TAB_PREFIX}${treeId}`;
}

export function researchTreeIdFromTabId(tabId: string): string | null {
  if (!tabId.startsWith(RESEARCH_TREE_TAB_PREFIX)) {
    return null;
  }
  const treeId = tabId.slice(RESEARCH_TREE_TAB_PREFIX.length);
  return treeId || null;
}

export function parseSidebarMode(value: string | null): SidebarMode {
  return value === "research" || value === "encyclopedia" ? value : "terminal";
}

export function terminalTabForMode(
  panes: PaneInfo[],
  groups: GroupInfo[],
  preferredTabId: string | null,
  homeTabId: string,
): string {
  if (preferredTabId === homeTabId) {
    return homeTabId;
  }
  const terminalPanes = panesForScope(panes, groups, "terminal");
  if (preferredTabId && terminalPanes.some((pane) => pane.id === preferredTabId)) {
    return preferredTabId;
  }
  // Fall back the way the app's other automatic selections do: prefer a pane
  // whose group is expanded — a collapsed group renders no rows, so
  // activating one of its panes would highlight nothing in the sidebar. Only
  // when every group is collapsed (or there are no panes) fall through to
  // the first pane / home.
  const groupById = new Map(groups.map((group) => [group.id, group]));
  const visiblePane = terminalPanes.find(
    (pane) => groupById.get(pane.groupId)?.collapsed !== true,
  );
  return visiblePane?.id ?? terminalPanes[0]?.id ?? homeTabId;
}

export function researchCycleTabIds(
  panes: PaneInfo[],
  groups: GroupInfo[],
  trees: ResearchTreeSummary[],
  scope: ResearchFolderScope,
): string[] {
  const groupById = new Map(groups.map((group) => [group.id, group]));
  // Cycle exactly what the sidebar lists: the "Live terminals" section is
  // filtered by the research folder scope, and cycling into a pane with no
  // visible row would leave the stage and the sidebar contradicting each
  // other (an active terminal with nothing highlighted anywhere).
  const visibleResearchPaneIds = panesForScope(panes, groups, "research")
    .filter((pane) => workspaceIsInResearchScope(pane.groupId, scope))
    .filter((pane) => groupById.get(pane.groupId)?.collapsed !== true)
    .map((pane) => pane.id);
  return [
    ...trees
      .filter((tree) => workspaceIsInResearchScope(tree.workspaceId, scope))
      .map((tree) => researchTreeTabId(tree.id)),
    ...visibleResearchPaneIds,
  ];
}
