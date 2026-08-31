import type { JournalEntry } from "./journal";
import type {
  RecentResearchQuery,
  ResearchNodeStatus,
  ResearchTreeSummary,
} from "../types";

/**
 * Shared activity grammar. Surfaces render these semantic slots rather than
 * hand-building slightly different metadata sentences for each source.
 */
export interface ActivityEvent<TSource = unknown> {
  id: string;
  actor: { kind: "user" | "agent" | "system"; label: string };
  action: { kind: "saved" | "asked" | "created" | "completed"; label: string };
  object: {
    kind: "note" | "link" | "post" | "research-query" | "artifact" | "task";
    id: string;
    label: string;
  };
  context?: { kind: "research" | "source" | "workspace"; label: string };
  relationship?: { kind: "top-level" | "follow-up"; label: string };
  execution?: { adapter: string; model?: string | null };
  state?: { kind: ResearchNodeStatus | "ready"; label: string };
  occurredAt: number;
  source: TSource;
}

export type RecentActivitySource =
  | { kind: "journal"; entry: JournalEntry }
  | { kind: "research-query"; query: RecentResearchQuery };

export type RecentActivityEvent = ActivityEvent<RecentActivitySource>;

export function recentResearchQueryFromNode(
  node: import("../types").ResearchNode,
): RecentResearchQuery | null {
  if (node.kind && node.kind !== "run") return null;
  return {
    nodeId: node.id,
    treeId: node.treeId,
    parentNodeId: node.parentNodeId,
    inline: Boolean(node.inline),
    prompt: node.prompt,
    title: node.title,
    adapter: node.adapter,
    model: node.model,
    status: node.status,
    createdAt: node.createdAt,
  };
}

export function upsertRecentResearchQuery(
  queries: RecentResearchQuery[],
  query: RecentResearchQuery,
): RecentResearchQuery[] {
  return [...queries.filter((candidate) => candidate.nodeId !== query.nodeId), query].sort(
    (left, right) => right.createdAt - left.createdAt || right.nodeId.localeCompare(left.nodeId),
  );
}

function journalTimestamp(entry: JournalEntry): number {
  const timestamp = Date.parse(entry.createdAt);
  return Number.isFinite(timestamp) ? timestamp : Number.NEGATIVE_INFINITY;
}

function journalObject(entry: JournalEntry): ActivityEvent["object"] {
  if (entry.kind === "note") {
    return { kind: "note", id: entry.id, label: "Note" };
  }
  if (entry.kind === "link") {
    return { kind: "link", id: entry.id, label: "Link" };
  }
  return { kind: "post", id: entry.id, label: "Post" };
}

export function activityEventFromJournalEntry(entry: JournalEntry): RecentActivityEvent {
  const sourceLabel =
    entry.kind === "tweet"
      ? entry.tweet?.author.handle
        ? `@${entry.tweet.author.handle}`
        : "X"
      : entry.kind === "link"
        ? (() => {
            try {
              return new URL(entry.url).hostname;
            } catch {
              return "Saved link";
            }
          })()
        : undefined;
  return {
    id: `journal:${entry.id}`,
    actor: { kind: "user", label: "You" },
    action: { kind: "saved", label: "saved" },
    object: journalObject(entry),
    ...(sourceLabel ? { context: { kind: "source" as const, label: sourceLabel } } : {}),
    occurredAt: journalTimestamp(entry),
    source: { kind: "journal", entry },
  };
}

function visibleResearchState(status: ResearchNodeStatus): ActivityEvent["state"] {
  switch (status) {
    case "queued":
      return { kind: status, label: "Queued" };
    case "starting":
      return { kind: status, label: "Starting" };
    case "running":
      return { kind: status, label: "Running" };
    case "failed":
      return { kind: status, label: "Failed" };
    case "cancelled":
      return { kind: status, label: "Cancelled" };
    case "complete":
      return undefined;
  }
}

export function activityEventFromResearchQuery(
  query: RecentResearchQuery,
  tree?: ResearchTreeSummary,
): RecentActivityEvent {
  const followUp = Boolean(query.parentNodeId);
  return {
    id: `research:${query.nodeId}`,
    actor: { kind: "user", label: "You" },
    action: { kind: "asked", label: "asked" },
    object: { kind: "research-query", id: query.nodeId, label: "Research" },
    context: { kind: "research", label: tree?.title ?? "Research" },
    relationship: {
      kind: followUp ? "follow-up" : "top-level",
      label: followUp ? "Follow-up" : "Top-level",
    },
    execution: { adapter: query.adapter, model: query.model },
    state: visibleResearchState(query.status),
    occurredAt: query.createdAt,
    source: { kind: "research-query", query },
  };
}

export function buildRecentActivity(
  entries: JournalEntry[],
  queries: RecentResearchQuery[],
  trees: ResearchTreeSummary[],
): RecentActivityEvent[] {
  const treeById = new Map(trees.map((tree) => [tree.id, tree]));
  return [
    ...entries.map(activityEventFromJournalEntry),
    ...queries.map((query) => activityEventFromResearchQuery(query, treeById.get(query.treeId))),
  ].sort((left, right) => right.occurredAt - left.occurredAt || right.id.localeCompare(left.id));
}

export function activityDayLabel(timestamp: number, now = Date.now()): string {
  if (!Number.isFinite(timestamp)) {
    return "Earlier";
  }
  const date = new Date(timestamp);
  const today = new Date(now);
  // UTC calendar ordinals avoid 23/25-hour daylight-saving days shifting a
  // local date into the wrong bucket.
  const startToday = Date.UTC(today.getFullYear(), today.getMonth(), today.getDate());
  const startDate = Date.UTC(date.getFullYear(), date.getMonth(), date.getDate());
  const days = Math.round((startToday - startDate) / 86_400_000);
  if (days === 0) return "Today";
  if (days === 1) return "Yesterday";
  return date.toLocaleDateString(undefined, { month: "short", day: "numeric", year: date.getFullYear() === today.getFullYear() ? undefined : "numeric" });
}
