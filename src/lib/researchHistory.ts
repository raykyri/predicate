// Browser-style visit history for the research document's back/forward
// controls. A pure, immutable reducer: the component holds one `ResearchHistory`
// value and swaps it for the result of these transitions, so the branch/cursor
// semantics live here (and are unit-tested) rather than inline in the view.

export interface ResearchHistory {
  /** Visited node ids, oldest first. */
  entries: string[];
  /** Cursor into `entries` for the currently displayed node, or -1 when empty. */
  index: number;
}

export interface ResearchHistoryStep {
  history: ResearchHistory;
  nodeId: string;
}

export const EMPTY_RESEARCH_HISTORY: ResearchHistory = { entries: [], index: -1 };

export const RESEARCH_SWIPE_THRESHOLD_PX = 80;

/**
 * Resolves an accumulated two-axis wheel gesture into browser-style history
 * navigation. Horizontal intent must be clear before the distance threshold
 * counts, so diagonal/vertical scrolling does not accidentally change nodes.
 */
export function researchSwipeDirection(
  deltaX: number,
  deltaY: number,
): -1 | 0 | 1 {
  if (
    Math.abs(deltaX) < RESEARCH_SWIPE_THRESHOLD_PX ||
    Math.abs(deltaX) <= Math.abs(deltaY) * 1.25
  ) {
    return 0;
  }
  return deltaX < 0 ? -1 : 1;
}

/** Starts a fresh history at the given entry node (e.g. on a tree switch). */
export function initResearchHistory(nodeId: string | null): ResearchHistory {
  return nodeId ? { entries: [nodeId], index: 0 } : EMPTY_RESEARCH_HISTORY;
}

/**
 * Records fresh navigation to `nodeId`: any forward entries beyond the cursor
 * are discarded and the node is appended, leaving the cursor at the end —
 * exactly how a browser drops the forward stack when you follow a new link.
 */
export function pushResearchHistory(history: ResearchHistory, nodeId: string): ResearchHistory {
  const entries = [...history.entries.slice(0, history.index + 1), nodeId];
  return { entries, index: entries.length - 1 };
}

export function canGoBack(history: ResearchHistory): boolean {
  return history.index > 0;
}

export function canGoForward(history: ResearchHistory): boolean {
  return history.index < history.entries.length - 1;
}

/** Moves the cursor back one entry, or null if already at the start. */
export function researchHistoryBack(history: ResearchHistory): ResearchHistoryStep | null {
  if (!canGoBack(history)) {
    return null;
  }
  const index = history.index - 1;
  return { history: { entries: history.entries, index }, nodeId: history.entries[index] };
}

/** Moves the cursor forward one entry, or null if already at the end. */
export function researchHistoryForward(history: ResearchHistory): ResearchHistoryStep | null {
  if (!canGoForward(history)) {
    return null;
  }
  const index = history.index + 1;
  return { history: { entries: history.entries, index }, nodeId: history.entries[index] };
}

/** Cross-page research visits. Recent Activity is a peer of a document, so
 * opening a query from the feed must push here — the document's own node
 * stack remounts empty and cannot remember the feed. */
export type ResearchWorkspaceVisit =
  | { kind: "journal" }
  | { kind: "document"; treeId: string };

export interface ResearchWorkspaceHistory {
  entries: ResearchWorkspaceVisit[];
  index: number;
}

export interface ResearchWorkspaceHistoryStep {
  history: ResearchWorkspaceHistory;
  visit: ResearchWorkspaceVisit;
}

export const EMPTY_RESEARCH_WORKSPACE_HISTORY: ResearchWorkspaceHistory = {
  entries: [],
  index: -1,
};

export function initResearchWorkspaceHistory(
  visit: ResearchWorkspaceVisit | null,
): ResearchWorkspaceHistory {
  return visit ? { entries: [visit], index: 0 } : EMPTY_RESEARCH_WORKSPACE_HISTORY;
}

export function sameResearchWorkspaceVisit(
  left: ResearchWorkspaceVisit,
  right: ResearchWorkspaceVisit,
): boolean {
  if (left.kind !== right.kind) return false;
  if (left.kind === "journal" && right.kind === "journal") return true;
  if (left.kind === "document" && right.kind === "document") return left.treeId === right.treeId;
  return false;
}

export function pushResearchWorkspaceHistory(
  history: ResearchWorkspaceHistory,
  visit: ResearchWorkspaceVisit,
): ResearchWorkspaceHistory {
  const current = history.index >= 0 ? history.entries[history.index] : undefined;
  if (current && sameResearchWorkspaceVisit(current, visit)) {
    return history;
  }
  const entries = [...history.entries.slice(0, history.index + 1), visit];
  return { entries, index: entries.length - 1 };
}

export function canGoWorkspaceBack(history: ResearchWorkspaceHistory): boolean {
  return history.index > 0;
}

export function canGoWorkspaceForward(history: ResearchWorkspaceHistory): boolean {
  return history.index >= 0 && history.index < history.entries.length - 1;
}

export function researchWorkspaceHistoryBack(
  history: ResearchWorkspaceHistory,
): ResearchWorkspaceHistoryStep | null {
  if (!canGoWorkspaceBack(history)) {
    return null;
  }
  const index = history.index - 1;
  return { history: { entries: history.entries, index }, visit: history.entries[index] };
}

export function researchWorkspaceHistoryForward(
  history: ResearchWorkspaceHistory,
): ResearchWorkspaceHistoryStep | null {
  if (!canGoWorkspaceForward(history)) {
    return null;
  }
  const index = history.index + 1;
  return { history: { entries: history.entries, index }, visit: history.entries[index] };
}

/** Removes visits to nodes that no longer exist while keeping the cursor on
 * the same surviving visit whenever possible. Visits that become adjacent
 * duplicates are collapsed: stepping between two entries for the same node
 * would re-apply the already-selected node, which readers treat as a real
 * navigation (e.g. clearing content for a load that never restarts). */
export function pruneResearchHistory(
  history: ResearchHistory,
  validNodeIds: ReadonlySet<string>,
  fallbackNodeId: string | null,
): ResearchHistory {
  const entries: string[] = [];
  let index = -1;
  for (let visit = 0; visit < history.entries.length; visit += 1) {
    const nodeId = history.entries[visit];
    if (!validNodeIds.has(nodeId)) {
      continue;
    }
    if (entries[entries.length - 1] !== nodeId) {
      entries.push(nodeId);
    }
    if (visit <= history.index) {
      index = entries.length - 1;
    }
  }
  if (entries.length === 0) {
    return initResearchHistory(fallbackNodeId);
  }
  return { entries, index: Math.max(0, index) };
}
