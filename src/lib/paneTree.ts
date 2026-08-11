import type { PaneInfo } from "../types";

/**
 * The backend layout command still accepts `depth` while older app versions may
 * be running during an upgrade. New layouts are deliberately flat and always
 * send zero so persisted nested tabs are cut over without a separate schema.
 */
export interface PaneLayoutItem {
  paneId: string;
  depth: 0;
}

export function toLayout(panes: PaneInfo[]): PaneLayoutItem[] {
  return panes.map((pane) => ({ paneId: pane.id, depth: 0 }));
}

const flatten = (panes: PaneInfo[]): PaneInfo[] =>
  panes.map((pane) => ((pane.depth ?? 0) === 0 ? pane : { ...pane, depth: 0 }));

export function movePaneBy(
  panes: PaneInfo[],
  paneId: string,
  direction: -1 | 1,
): PaneInfo[] {
  const from = panes.findIndex((pane) => pane.id === paneId);
  const to = from + direction;
  if (from < 0 || to < 0 || to >= panes.length) {
    return panes;
  }
  const next = [...panes];
  [next[from], next[to]] = [next[to], next[from]];
  return flatten(next);
}

/** Moves one pane to an insert-before gap in the current flat list. */
export function movePaneToGap(panes: PaneInfo[], dragId: string, gap: number): PaneInfo[] {
  const from = panes.findIndex((pane) => pane.id === dragId);
  if (from < 0 || gap === from || gap === from + 1) {
    return panes;
  }

  const pane = panes[from];
  const rest = panes.filter((candidate) => candidate.id !== dragId);
  const insertAt = Math.max(0, Math.min(gap > from ? gap - 1 : gap, rest.length));
  return flatten([...rest.slice(0, insertAt), pane, ...rest.slice(insertAt)]);
}

/** Agent tabs are group-bound; only an ordinary shell tab may cross groups. */
export function paneCanMoveAcrossGroups(panes: PaneInfo[], paneId: string): boolean {
  const pane = panes.find((candidate) => candidate.id === paneId);
  return Boolean(pane && pane.kind === "shell" && !pane.agentId);
}

/** Moves one pane into another group at an insert-before gap. */
export function movePaneAcrossGroups(
  source: PaneInfo[],
  target: PaneInfo[],
  dragId: string,
  targetGroupId: string,
  gap: number,
): { source: PaneInfo[]; target: PaneInfo[] } | null {
  const from = source.findIndex((pane) => pane.id === dragId);
  if (from < 0) {
    return null;
  }
  const pane = { ...source[from], groupId: targetGroupId, depth: 0 };
  const insertAt = Math.max(0, Math.min(gap, target.length));
  return {
    source: flatten([...source.slice(0, from), ...source.slice(from + 1)]),
    target: flatten([...target.slice(0, insertAt), pane, ...target.slice(insertAt)]),
  };
}

/** Moves a pane immediately above or below another pane. */
export function movePaneAdjacentToPane(
  panes: PaneInfo[],
  dragId: string,
  targetId: string,
  position: "above" | "below",
): PaneInfo[] {
  const from = panes.findIndex((pane) => pane.id === dragId);
  const targetIndex = panes.findIndex((pane) => pane.id === targetId);
  if (from < 0 || targetIndex < 0 || from === targetIndex) {
    return panes;
  }

  const pane = panes[from];
  const rest = panes.filter((candidate) => candidate.id !== dragId);
  const targetInRest = rest.findIndex((candidate) => candidate.id === targetId);
  const insertAt = targetInRest + (position === "below" ? 1 : 0);
  return flatten([...rest.slice(0, insertAt), pane, ...rest.slice(insertAt)]);
}

/** Moves a pane directly after another pane. */
export function movePaneAfter(
  panes: PaneInfo[],
  dragId: string,
  afterId: string,
): PaneInfo[] {
  return movePaneAdjacentToPane(panes, dragId, afterId, "below");
}
