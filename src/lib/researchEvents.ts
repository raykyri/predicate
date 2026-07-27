import type {
  QmuxEvent,
  ResearchHighlight,
  ResearchNode,
  ResearchNodeStatus,
  ResearchTree,
  ResearchTreeDetail,
  ResearchTreeSummary,
} from "../types";

type TreeCreatedEvent = {
  type: "research.tree.created";
  tree: ResearchTree;
  node: ResearchNode;
  timestamp: number;
};

type DocumentUpdatedEvent = {
  type: "research.document.updated";
  tree: ResearchTree;
  node: ResearchNode;
  responseRevision: string;
  markdownChanged: boolean;
  removedHighlightCount: number;
  timestamp: number;
};

type NodeEvent = {
  type: "research.node.created" | "research.node.updated";
  node: ResearchNode;
  timestamp: number;
};

type TreeEvent = {
  type:
    | "research.tree.updated"
    | "research.tree.archived"
    | "research.tree.restored";
  tree: ResearchTree;
  timestamp: number;
};

type HighlightCreatedEvent = {
  type: "research.highlight.created";
  nodeId: string;
  highlight: ResearchHighlight;
  timestamp: number;
};

type HighlightRemovedEvent = {
  type: "research.highlight.removed";
  nodeId: string;
  highlightId: string;
  timestamp: number;
};

type HighlightsRemovedEvent = {
  type: "research.highlights.removed";
  nodeId: string;
  highlightIds: string[];
  timestamp: number;
};

type TreeRemovedEvent = {
  type: "research.tree.removed";
  treeId: string;
  timestamp: number;
};

type NodeRemovedEvent = {
  type: "research.node.removed";
  treeId: string;
  parentNodeId: string;
  removedNodeIds: string[];
  timestamp: number;
};

/** Every research event currently emitted by the backend. Keeping this a
 * closed union makes a new backend event take the explicit recovery path
 * until its frontend state effects are intentionally implemented. */
export type ParsedResearchEvent =
  | TreeCreatedEvent
  | DocumentUpdatedEvent
  | NodeEvent
  | TreeEvent
  | HighlightCreatedEvent
  | HighlightRemovedEvent
  | HighlightsRemovedEvent
  | TreeRemovedEvent
  | NodeRemovedEvent;

export type ResearchEventParseResult =
  | { kind: "event"; event: ParsedResearchEvent }
  | { kind: "notResearch" }
  | { kind: "unsupported"; type: string }
  | { kind: "malformed"; type: string };

const RESEARCH_STATUSES = new Set<ResearchNodeStatus>([
  "queued",
  "starting",
  "running",
  "complete",
  "failed",
  "cancelled",
]);

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isFiniteNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

function isOptionalFiniteNumber(value: unknown): boolean {
  return value === undefined || value === null || isFiniteNumber(value);
}

function isOptionalString(value: unknown): boolean {
  return value === undefined || value === null || typeof value === "string";
}

function isStringArray(value: unknown): value is string[] {
  return Array.isArray(value) && value.every((item) => typeof item === "string");
}

function isResearchTree(value: unknown): value is ResearchTree {
  if (!isRecord(value)) {
    return false;
  }
  return (
    typeof value.id === "string" &&
    typeof value.title === "string" &&
    typeof value.rootNodeId === "string" &&
    typeof value.workspaceId === "string" &&
    isFiniteNumber(value.createdAt) &&
    isFiniteNumber(value.updatedAt) &&
    isOptionalFiniteNumber(value.archivedAt) &&
    isOptionalFiniteNumber(value.lastViewedAt)
  );
}

function isResearchHighlight(value: unknown): value is ResearchHighlight {
  if (!isRecord(value) || !isRecord(value.anchor)) {
    return false;
  }
  const anchor = value.anchor;
  return (
    typeof value.id === "string" &&
    isFiniteNumber(value.createdAt) &&
    anchor.version === 1 &&
    anchor.projection === "answer-v1" &&
    typeof anchor.responseRevision === "string" &&
    isFiniteNumber(anchor.start) &&
    isFiniteNumber(anchor.end) &&
    typeof anchor.exact === "string" &&
    typeof anchor.prefix === "string" &&
    typeof anchor.suffix === "string"
  );
}

function isResearchNode(value: unknown): value is ResearchNode {
  if (!isRecord(value)) {
    return false;
  }
  return (
    typeof value.id === "string" &&
    typeof value.treeId === "string" &&
    typeof value.prompt === "string" &&
    typeof value.adapter === "string" &&
    typeof value.groupId === "string" &&
    typeof value.worktreeDir === "string" &&
    typeof value.status === "string" &&
    RESEARCH_STATUSES.has(value.status as ResearchNodeStatus) &&
    isFiniteNumber(value.createdAt) &&
    isOptionalFiniteNumber(value.startedAt) &&
    isOptionalFiniteNumber(value.completedAt) &&
    isOptionalFiniteNumber(value.responseSnapshotAt) &&
    isOptionalString(value.parentNodeId) &&
    isOptionalString(value.title) &&
    isOptionalString(value.responsePreview) &&
    isOptionalString(value.paneId) &&
    isOptionalString(value.agentId) &&
    Array.isArray(value.highlights) &&
    value.highlights.every(isResearchHighlight)
  );
}

/** Parse and minimally validate a backend event before it is allowed to
 * mutate live research state. A malformed or newly-added research event is
 * distinguishable from an unrelated event so the caller can recover with an
 * authoritative refetch instead of silently ignoring it. */
export function parseResearchEvent(event: QmuxEvent): ResearchEventParseResult {
  if (!event.type.startsWith("research.")) {
    return { kind: "notResearch" };
  }
  const malformed = (): ResearchEventParseResult => ({
    kind: "malformed",
    type: event.type,
  });
  if (!isFiniteNumber(event.timestamp) || !isRecord(event.payload)) {
    return malformed();
  }
  const payload = event.payload;
  switch (event.type) {
    case "research.tree.created":
      return isResearchTree(payload.tree) && isResearchNode(payload.node)
        ? {
            kind: "event",
            event: {
              type: event.type,
              tree: payload.tree,
              node: payload.node,
              timestamp: event.timestamp,
            },
          }
        : malformed();
    case "research.document.updated":
      return isResearchTree(payload.tree) &&
        isResearchNode(payload.node) &&
        typeof payload.responseRevision === "string" &&
        typeof payload.markdownChanged === "boolean" &&
        isFiniteNumber(payload.removedHighlightCount)
        ? {
            kind: "event",
            event: {
              type: event.type,
              tree: payload.tree,
              node: payload.node,
              responseRevision: payload.responseRevision,
              markdownChanged: payload.markdownChanged,
              removedHighlightCount: payload.removedHighlightCount,
              timestamp: event.timestamp,
            },
          }
        : malformed();
    case "research.node.created":
    case "research.node.updated":
      return isResearchNode(payload.node)
        ? {
            kind: "event",
            event: { type: event.type, node: payload.node, timestamp: event.timestamp },
          }
        : malformed();
    case "research.tree.updated":
    case "research.tree.archived":
    case "research.tree.restored":
      return isResearchTree(payload.tree)
        ? {
            kind: "event",
            event: { type: event.type, tree: payload.tree, timestamp: event.timestamp },
          }
        : malformed();
    case "research.highlight.created":
      return typeof payload.nodeId === "string" && isResearchHighlight(payload.highlight)
        ? {
            kind: "event",
            event: {
              type: event.type,
              nodeId: payload.nodeId,
              highlight: payload.highlight,
              timestamp: event.timestamp,
            },
          }
        : malformed();
    case "research.highlight.removed":
      return typeof payload.nodeId === "string" && typeof payload.highlightId === "string"
        ? {
            kind: "event",
            event: {
              type: event.type,
              nodeId: payload.nodeId,
              highlightId: payload.highlightId,
              timestamp: event.timestamp,
            },
          }
        : malformed();
    case "research.highlights.removed":
      return typeof payload.nodeId === "string" && isStringArray(payload.highlightIds)
        ? {
            kind: "event",
            event: {
              type: event.type,
              nodeId: payload.nodeId,
              highlightIds: payload.highlightIds,
              timestamp: event.timestamp,
            },
          }
        : malformed();
    case "research.tree.removed":
      return typeof payload.treeId === "string"
        ? {
            kind: "event",
            event: { type: event.type, treeId: payload.treeId, timestamp: event.timestamp },
          }
        : malformed();
    case "research.node.removed":
      return typeof payload.treeId === "string" &&
        typeof payload.parentNodeId === "string" &&
        isStringArray(payload.removedNodeIds)
        ? {
            kind: "event",
            event: {
              type: event.type,
              treeId: payload.treeId,
              parentNodeId: payload.parentNodeId,
              removedNodeIds: payload.removedNodeIds,
              timestamp: event.timestamp,
            },
          }
        : malformed();
    default:
      return { kind: "unsupported", type: event.type };
  }
}

export interface ResearchStatusContribution {
  runningCount: number;
  failedCount: number;
  completedCount: number;
  cancelledCount: number;
}

export function researchStatusContribution(
  status: ResearchNodeStatus,
): ResearchStatusContribution {
  return {
    runningCount: status === "queued" || status === "starting" || status === "running" ? 1 : 0,
    failedCount: status === "failed" ? 1 : 0,
    completedCount: status === "complete" ? 1 : 0,
    cancelledCount: status === "cancelled" ? 1 : 0,
  };
}

export function researchSummaryFromDetail(detail: ResearchTreeDetail): ResearchTreeSummary {
  let runningCount = 0;
  let failedCount = 0;
  let completedCount = 0;
  let cancelledCount = 0;
  let latestSettlement: number | null = null;
  let latestFailure: number | null = null;
  for (const node of detail.nodes) {
    const contribution = researchStatusContribution(node.status);
    runningCount += contribution.runningCount;
    failedCount += contribution.failedCount;
    completedCount += contribution.completedCount;
    cancelledCount += contribution.cancelledCount;
    if (node.completedAt != null) {
      latestSettlement = Math.max(latestSettlement ?? node.completedAt, node.completedAt);
      if (node.status === "failed") {
        latestFailure = Math.max(latestFailure ?? node.completedAt, node.completedAt);
      }
    }
  }
  const lastViewedAt = detail.tree.lastViewedAt ?? null;
  const unseen = (settledAt: number | null) =>
    settledAt !== null && (lastViewedAt === null || settledAt > lastViewedAt);
  const root = detail.nodes.find((node) => node.id === detail.tree.rootNodeId);
  return {
    id: detail.tree.id,
    title: detail.tree.title,
    rootNodeId: detail.tree.rootNodeId,
    kind: root?.kind ?? "run",
    ...(root?.origin != null ? { origin: root.origin } : {}),
    workspaceId: detail.tree.workspaceId,
    runningCount,
    failedCount,
    completedCount,
    cancelledCount,
    updatedAt: detail.tree.updatedAt,
    archivedAt: detail.tree.archivedAt ?? null,
    hasUnseenUpdate: unseen(latestSettlement),
    hasUnseenFailure: unseen(latestFailure),
  };
}

function compareResearchNodes(left: ResearchNode, right: ResearchNode): number {
  return left.createdAt - right.createdAt || left.id.localeCompare(right.id);
}

/** Upsert one full-node event while preserving collection identity for an
 * identical object and the backend's (createdAt, id) ordering. */
export function upsertResearchNode(
  nodes: ResearchNode[],
  node: ResearchNode,
): ResearchNode[] {
  const index = nodes.findIndex((candidate) => candidate.id === node.id);
  if (index >= 0) {
    if (nodes[index] === node) {
      return nodes;
    }
    const next = [...nodes];
    next[index] = node;
    if (compareResearchNodes(nodes[index], node) !== 0) {
      next.sort(compareResearchNodes);
    }
    return next;
  }
  const next = [...nodes, node];
  next.sort(compareResearchNodes);
  return next;
}

export function removeResearchNodes(nodes: ResearchNode[], removedIds: Iterable<string>): ResearchNode[] {
  const removed = removedIds instanceof Set ? removedIds : new Set(removedIds);
  const next = nodes.filter((node) => !removed.has(node.id));
  return next.length === nodes.length ? nodes : next;
}

export function researchNodeIsActivity(node: ResearchNode): boolean {
  return (
    node.paneId != null ||
    node.status === "queued" ||
    node.status === "starting" ||
    node.status === "running"
  );
}

export function upsertResearchActivity(
  activity: ResearchNode[],
  node: ResearchNode,
): ResearchNode[] {
  return researchNodeIsActivity(node)
    ? upsertResearchNode(activity, node)
    : removeResearchNodes(activity, [node.id]);
}

export function patchResearchDetailNode(
  detail: ResearchTreeDetail | null,
  node: ResearchNode,
): ResearchTreeDetail | null {
  if (!detail || detail.tree.id !== node.treeId) {
    return detail;
  }
  const nodes = upsertResearchNode(detail.nodes, node);
  return nodes === detail.nodes ? detail : { ...detail, nodes };
}

export function patchResearchDetailTree(
  detail: ResearchTreeDetail | null,
  tree: ResearchTree,
): ResearchTreeDetail | null {
  if (!detail || detail.tree.id !== tree.id || detail.tree === tree) {
    return detail;
  }
  return { ...detail, tree };
}

export function removeResearchDetailNodes(
  detail: ResearchTreeDetail | null,
  treeId: string,
  removedIds: Iterable<string>,
): ResearchTreeDetail | null {
  if (!detail || detail.tree.id !== treeId) {
    return detail;
  }
  const nodes = removeResearchNodes(detail.nodes, removedIds);
  return nodes === detail.nodes ? detail : { ...detail, nodes };
}

export function addResearchNodeHighlight(
  node: ResearchNode,
  highlight: ResearchHighlight,
): ResearchNode {
  if (node.highlights.some((candidate) => candidate.id === highlight.id)) {
    return node;
  }
  return { ...node, highlights: [...node.highlights, highlight] };
}

export function removeResearchNodeHighlights(
  node: ResearchNode,
  highlightIds: Iterable<string>,
): ResearchNode {
  const removed = highlightIds instanceof Set ? highlightIds : new Set(highlightIds);
  const highlights = node.highlights.filter((highlight) => !removed.has(highlight.id));
  return highlights.length === node.highlights.length ? node : { ...node, highlights };
}

export function patchResearchDetailHighlightCreated(
  detail: ResearchTreeDetail | null,
  nodeId: string,
  highlight: ResearchHighlight,
): ResearchTreeDetail | null {
  if (!detail) {
    return detail;
  }
  const node = detail.nodes.find((candidate) => candidate.id === nodeId);
  return node ? patchResearchDetailNode(detail, addResearchNodeHighlight(node, highlight)) : detail;
}

export function patchResearchDetailHighlightsRemoved(
  detail: ResearchTreeDetail | null,
  nodeId: string,
  highlightIds: Iterable<string>,
): ResearchTreeDetail | null {
  if (!detail) {
    return detail;
  }
  const node = detail.nodes.find((candidate) => candidate.id === nodeId);
  return node
    ? patchResearchDetailNode(detail, removeResearchNodeHighlights(node, highlightIds))
    : detail;
}

/** Patch the summary fields carried authoritatively by a tree event without
 * disturbing node-derived counts or attention flags. */
export function patchResearchSummaryTree(
  summary: ResearchTreeSummary,
  tree: ResearchTree,
): ResearchTreeSummary {
  if (summary.id !== tree.id) {
    return summary;
  }
  const archivedAt = tree.archivedAt ?? null;
  if (
    summary.title === tree.title &&
    summary.rootNodeId === tree.rootNodeId &&
    summary.workspaceId === tree.workspaceId &&
    summary.updatedAt === tree.updatedAt &&
    (summary.archivedAt ?? null) === archivedAt
  ) {
    return summary;
  }
  return {
    ...summary,
    title: tree.title,
    rootNodeId: tree.rootNodeId,
    workspaceId: tree.workspaceId,
    updatedAt: tree.updatedAt,
    archivedAt,
  };
}

function terminalStatus(status: ResearchNodeStatus): boolean {
  return status === "complete" || status === "failed" || status === "cancelled";
}

/** Apply the node-derived portion of a summary update. The caller must supply
 * the previously observed full node; without it, count deltas are ambiguous
 * and the safe path is a targeted authoritative tree refresh. */
export function patchResearchSummaryForNode(
  summary: ResearchTreeSummary,
  previous: ResearchNode,
  node: ResearchNode,
  eventTimestamp: number,
): ResearchTreeSummary {
  if (
    summary.id !== node.treeId ||
    previous.id !== node.id ||
    previous.treeId !== node.treeId
  ) {
    return summary;
  }
  const before = researchStatusContribution(previous.status);
  const after = researchStatusContribution(node.status);
  const lifecycleChanged =
    previous.status !== node.status || previous.completedAt !== node.completedAt;
  if (!lifecycleChanged) {
    return summary;
  }
  const newlySettled =
    terminalStatus(node.status) &&
    node.completedAt != null &&
    (previous.completedAt == null || previous.completedAt !== node.completedAt);
  return {
    ...summary,
    runningCount: Math.max(0, summary.runningCount - before.runningCount + after.runningCount),
    failedCount: Math.max(0, summary.failedCount - before.failedCount + after.failedCount),
    completedCount: Math.max(
      0,
      summary.completedCount - before.completedCount + after.completedCount,
    ),
    cancelledCount: Math.max(
      0,
      summary.cancelledCount - before.cancelledCount + after.cancelledCount,
    ),
    updatedAt: Math.max(
      summary.updatedAt,
      eventTimestamp,
      node.startedAt ?? 0,
      node.completedAt ?? 0,
    ),
    hasUnseenUpdate: summary.hasUnseenUpdate || newlySettled,
    hasUnseenFailure:
      summary.hasUnseenFailure || (newlySettled && node.status === "failed"),
  };
}

/** Add the status contribution of a newly-created node. Node-created events
 * do not include the tree, but they do carry everything needed for the live
 * count badges while the trailing authoritative tree read is debounced. */
export function patchResearchSummaryForCreatedNode(
  summary: ResearchTreeSummary,
  node: ResearchNode,
  eventTimestamp: number,
): ResearchTreeSummary {
  if (summary.id !== node.treeId) {
    return summary;
  }
  const contribution = researchStatusContribution(node.status);
  return {
    ...summary,
    runningCount: summary.runningCount + contribution.runningCount,
    failedCount: summary.failedCount + contribution.failedCount,
    completedCount: summary.completedCount + contribution.completedCount,
    cancelledCount: summary.cancelledCount + contribution.cancelledCount,
    updatedAt: Math.max(summary.updatedAt, eventTimestamp),
  };
}

/** Remove every cached node contribution available for a branch deletion.
 * Attention stays conservative because another node may still own the flag;
 * the debounced tree read resolves counts when some removed nodes were not in
 * the local cache. */
export function patchResearchSummaryForRemovedNodes(
  summary: ResearchTreeSummary,
  treeId: string,
  removedNodes: ResearchNode[],
  eventTimestamp: number,
): ResearchTreeSummary {
  if (summary.id !== treeId || removedNodes.length === 0) {
    return summary;
  }
  const removed = removedNodes.reduce(
    (total, node) => {
      const contribution = researchStatusContribution(node.status);
      total.runningCount += contribution.runningCount;
      total.failedCount += contribution.failedCount;
      total.completedCount += contribution.completedCount;
      total.cancelledCount += contribution.cancelledCount;
      return total;
    },
    { runningCount: 0, failedCount: 0, completedCount: 0, cancelledCount: 0 },
  );
  return {
    ...summary,
    runningCount: Math.max(0, summary.runningCount - removed.runningCount),
    failedCount: Math.max(0, summary.failedCount - removed.failedCount),
    completedCount: Math.max(0, summary.completedCount - removed.completedCount),
    cancelledCount: Math.max(0, summary.cancelledCount - removed.cancelledCount),
    updatedAt: Math.max(summary.updatedAt, eventTimestamp),
  };
}
