// Research navigation restoration state (selected node and per-node scroll
// offsets, keyed by tree). Held as a module-level singleton so the workspace
// (which reads/writes selections) and the app shell (which prunes entries for
// deleted trees) mutate the same object — pruning localStorage behind a
// separate in-memory copy would just get resurrected by the next save.

import type { ResearchHighlightAnchor } from "../types";

export interface SavedResearchScrollPosition {
  top: number;
  updatedAt: number;
}

/** An in-progress targeted follow-up (ask mode): the passage it is being
 * composed against and whatever the user has typed so far. Persisted so
 * leaving the research surface — which unmounts the document — does not
 * discard the ask; removed only by submit or an explicit dismiss. */
export interface SavedResearchAsk {
  anchor: ResearchHighlightAnchor;
  text: string;
  updatedAt: number;
}

export interface SavedResearchNavigation {
  selectedNodeId?: string;
  scrollByNode: Record<string, SavedResearchScrollPosition>;
  /** Nodes whose "Show earlier" window the user expanded. Restored together
   * with the scroll offset — an offset captured against the expanded list
   * would land in the wrong place in the collapsed one. */
  expandedByNode?: Record<string, boolean>;
  /** In-progress asks, keyed by the node they were started on. */
  askByNode?: Record<string, SavedResearchAsk>;
}

const RESEARCH_NAVIGATION_KEY = "qmux.research-navigation.v1";
export const RESEARCH_SCROLL_POSITION_TTL_MS = 15 * 60 * 1000;

let store: Record<string, SavedResearchNavigation> | null = null;

export function isResearchNodeSelectionChange(
  selectedNodeId: string | null,
  nextNodeId: string,
): boolean {
  return selectedNodeId !== nextNodeId;
}

export function isResearchTreeSelectionChange(
  selectedTreeId: string | null,
  documentVisible: boolean,
  nextTreeId: string,
): boolean {
  // Re-selecting the visible document would clear its detail while the same
  // tree is fetched again, producing a needless loading blink. The same tree
  // remains selectable when one of its terminal panes is currently visible.
  return !documentVisible || selectedTreeId !== nextTreeId;
}

function isSavedAnchor(value: unknown): value is ResearchHighlightAnchor {
  if (!value || typeof value !== "object") {
    return false;
  }
  const anchor = value as Partial<ResearchHighlightAnchor>;
  return (
    anchor.version === 1 &&
    anchor.projection === "answer-v1" &&
    typeof anchor.responseRevision === "string" &&
    typeof anchor.start === "number" &&
    typeof anchor.end === "number" &&
    typeof anchor.exact === "string" &&
    typeof anchor.prefix === "string" &&
    typeof anchor.suffix === "string"
  );
}

function load(): Record<string, SavedResearchNavigation> {
  try {
    const now = Date.now();
    const parsed = JSON.parse(localStorage.getItem(RESEARCH_NAVIGATION_KEY) ?? "{}") as unknown;
    if (!parsed || typeof parsed !== "object") {
      return {};
    }
    return Object.fromEntries(
      Object.entries(parsed).flatMap(([treeId, value]) => {
        if (!value || typeof value !== "object") {
          return [];
        }
        const candidate = value as Partial<SavedResearchNavigation>;
        const scrollByNode = Object.fromEntries(
          Object.entries(candidate.scrollByNode ?? {}).flatMap(([nodeId, value]) => {
            if (!value || typeof value !== "object") {
              // The previous schema stored a bare number, which has no age and
              // therefore cannot safely be carried into the expiring cache.
              return [];
            }
            const position = value as Partial<SavedResearchScrollPosition>;
            return typeof position.top === "number" &&
              Number.isFinite(position.top) &&
              position.top >= 0 &&
              typeof position.updatedAt === "number" &&
              Number.isFinite(position.updatedAt) &&
              now - position.updatedAt < RESEARCH_SCROLL_POSITION_TTL_MS
              ? [[nodeId, { top: position.top, updatedAt: position.updatedAt }]]
              : [];
          }),
        );
        const expandedByNode = Object.fromEntries(
          Object.entries(candidate.expandedByNode ?? {}).filter(
            (entry): entry is [string, boolean] => entry[1] === true,
          ),
        );
        const askByNode = Object.fromEntries(
          Object.entries(candidate.askByNode ?? {}).flatMap(([nodeId, value]) => {
            if (!value || typeof value !== "object") {
              return [];
            }
            const ask = value as Partial<SavedResearchAsk>;
            return isSavedAnchor(ask.anchor) &&
              typeof ask.text === "string" &&
              typeof ask.updatedAt === "number" &&
              Number.isFinite(ask.updatedAt)
              ? [[nodeId, { anchor: ask.anchor, text: ask.text, updatedAt: ask.updatedAt }]]
              : [];
          }),
        );
        return [[treeId, {
          selectedNodeId:
            typeof candidate.selectedNodeId === "string" ? candidate.selectedNodeId : undefined,
          scrollByNode,
          ...(Object.keys(expandedByNode).length > 0 ? { expandedByNode } : {}),
          ...(Object.keys(askByNode).length > 0 ? { askByNode } : {}),
        } satisfies SavedResearchNavigation]];
      }),
    );
  } catch {
    return {};
  }
}

export function researchNavigationStore(): Record<string, SavedResearchNavigation> {
  return (store ??= load());
}

export function saveResearchNavigation(): void {
  try {
    localStorage.setItem(RESEARCH_NAVIGATION_KEY, JSON.stringify(researchNavigationStore()));
  } catch {
    // Navigation restoration is a convenience; storage denial must not break research.
  }
}

/** Points a tree's saved selection at a specific node before the tree is
 * opened, so navigation that targets a node (an encyclopedia citation) lands
 * on it instead of the tree's last-viewed node. */
export function setSavedResearchSelection(treeId: string, nodeId: string): void {
  const current = researchNavigationStore();
  const navigation = (current[treeId] ??= { scrollByNode: {} });
  navigation.selectedNodeId = nodeId;
  saveResearchNavigation();
}

export function recordResearchScrollPosition(
  navigation: SavedResearchNavigation,
  nodeId: string,
  top: number,
  now = Date.now(),
): void {
  navigation.scrollByNode[nodeId] = { top, updatedAt: now };
}

export function restoreResearchScrollPosition(
  navigation: SavedResearchNavigation | undefined,
  nodeId: string,
  now = Date.now(),
): number {
  const position = navigation?.scrollByNode[nodeId];
  if (!position || now - position.updatedAt >= RESEARCH_SCROLL_POSITION_TTL_MS) {
    return 0;
  }
  return position.top;
}

/** Drops navigation state for trees that no longer exist. */
export function pruneResearchNavigation(validTreeIds: Iterable<string>): void {
  const valid = new Set(validTreeIds);
  const current = researchNavigationStore();
  let changed = false;
  for (const treeId of Object.keys(current)) {
    if (!valid.has(treeId)) {
      delete current[treeId];
      changed = true;
    }
  }
  if (changed) {
    saveResearchNavigation();
  }
}

/** Drops per-node state (scroll offsets, selection) for deleted nodes of a tree. */
export function pruneResearchNavigationNodes(treeId: string, validNodeIds: Iterable<string>): void {
  const navigation = researchNavigationStore()[treeId];
  if (!navigation) {
    return;
  }
  const valid = new Set(validNodeIds);
  let changed = false;
  for (const nodeId of Object.keys(navigation.scrollByNode)) {
    if (!valid.has(nodeId)) {
      delete navigation.scrollByNode[nodeId];
      changed = true;
    }
  }
  for (const nodeId of Object.keys(navigation.expandedByNode ?? {})) {
    if (!valid.has(nodeId)) {
      delete navigation.expandedByNode?.[nodeId];
      changed = true;
    }
  }
  for (const nodeId of Object.keys(navigation.askByNode ?? {})) {
    if (!valid.has(nodeId)) {
      delete navigation.askByNode?.[nodeId];
      changed = true;
    }
  }
  if (navigation.selectedNodeId && !valid.has(navigation.selectedNodeId)) {
    delete navigation.selectedNodeId;
    changed = true;
  }
  if (changed) {
    saveResearchNavigation();
  }
}
