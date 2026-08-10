// Pure timeline-item construction for the transcript pane. Extracted from
// TurnOverlay so the fold (merging, tool-call pairing, activity grouping, key
// derivation) is testable without rendering — TurnOverlay's import chain
// reaches ESM-only markdown packages that the node test runner can't load.

import type {
  MessageAnchor,
  ThreadParticipant,
  TranscriptAnnotation,
  Turn,
  TurnBlock,
} from "../types";
import {
  stripTaggedInstructionBlocks,
  stripTaggedUserInstructionBlocks,
  taggedUserInstructionDetails,
} from "./taggedInstructions";

export type TextBlock = Extract<TurnBlock, { type: "text" }>;
export type ToolUseBlock = Extract<TurnBlock, { type: "toolUse" }>;
export type ToolResultBlock = Extract<TurnBlock, { type: "toolResult" }>;
export type RawBlock = Extract<TurnBlock, { type: "raw" }>;

export type MessageBlock = TextBlock | RawBlock;
export type TurnTimelineStatus = NonNullable<Turn["status"]>;
export type TurnTimelineContextStatus = NonNullable<Turn["contextStatus"]>;

export interface MessageItem {
  type: "message";
  key: string;
  role: string;
  participant?: ThreadParticipant | null;
  blocks: MessageBlock[];
  activities: ActivityItem[];
  status?: TurnTimelineStatus;
  contextStatus?: TurnTimelineContextStatus;
  /** Where a fork anchored at this message would branch from. Set from the
   * first turn folded into the card, so forking from a card that merged
   * back-to-back prompts branches before the first of them. */
  anchor?: MessageAnchor | null;
  /**
   * Latest native timestamp among the turns folded into this item. Used for
   * the optional end-of-assistant-group timestamp footer.
   */
  timestamp?: number | null;
  /** Native/qmux turns whose text or activity was folded into this visible
   * message. Saved excerpts use these ids to stay attached across card merges. */
  sourceTurnIds: string[];
  /** Source turn for each entry in `blocks`, kept separately because one
   * visible assistant card can fold text from several native turns. */
  blockSourceTurnIds: string[];
}

export interface ToolEntry {
  type: "tool";
  key: string;
  id?: string | null;
  name: string;
  input?: unknown;
  result?: unknown;
  isError: boolean;
  status?: TurnTimelineStatus;
}

export interface ThinkingItem {
  type: "thinking";
  key: string;
  values: unknown[];
  status?: TurnTimelineStatus;
}

// Assistant "raw" blocks become thinking values. A real provider reasoning
// block arrives as an object like
// `{ type: "thinking", thinking: "<prose>", signature: "<long base64 blob>" }`;
// the signature is an opaque verification token of no use to a reader and, at
// ~10 KB, drowns the actual reasoning when the whole object is JSON-dumped.
// This pulls out the human-readable reasoning so the renderer can show it as
// prose. Order matters: the first field that holds a non-empty string wins.
const THINKING_TEXT_KEYS = ["thinking", "text", "thought", "reasoning", "content"] as const;

/**
 * The human-readable reasoning carried by one thinking value, or null when the
 * value has no recognizable prose field (an export marker, a bare number, an
 * unfamiliar shape) and should fall back to serialized JSON.
 */
export function thinkingProseText(value: unknown): string | null {
  if (typeof value === "string") {
    const trimmed = value.trim();
    return trimmed.length > 0 ? trimmed : null;
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return null;
  }
  const record = value as Record<string, unknown>;
  for (const key of THINKING_TEXT_KEYS) {
    const candidate = record[key];
    if (typeof candidate === "string" && candidate.trim().length > 0) {
      return candidate;
    }
  }
  return null;
}

export interface ActivityGroupItem {
  type: "activityGroup";
  key: string;
  children: ActivityLeafItem[];
  toolCallCount: number;
  status?: TurnTimelineStatus;
}

export type ActivityLeafItem = ToolEntry | ThinkingItem;
export type ActivityItem = ActivityLeafItem | ActivityGroupItem;

export interface PlainTextTranscriptMessage {
  id: string;
  role: "user" | "assistant";
  label: string;
  text: string;
}

/** Original assistant text, before Markdown rendering, in timeline order. */
export function assistantTextFromTimelineItems(items: MessageItem[]) {
  return items
    .filter((item) => item.role === "assistant")
    .flatMap((item) =>
      item.blocks.flatMap((block) => (block.type === "text" ? [block.text] : [])),
    )
    .join("\n\n")
    .trim();
}

export function formatPlainTextTranscript(turns: Turn[], assistantLabel: string) {
  return plainTextTranscriptMessages(turns, assistantLabel)
    .map((message) => `${message.label}:\n${message.text}`)
    .join("\n\n");
}

export function plainTextTranscriptMessages(
  turns: Turn[],
  assistantLabel: string,
): PlainTextTranscriptMessage[] {
  return buildTimelineItems(turns, false)
    .flatMap((item) => {
      if (item.role !== "user" && item.role !== "assistant") {
        return [];
      }
      const text = plainTextMessageItemText(item);
      if (!text) {
        return [];
      }
      return [
        {
          id: item.key,
          role: item.role,
          label: messageRoleLabel(item, assistantLabel),
          text,
        },
      ];
    });
}

function plainTextMessageItemText(item: MessageItem) {
  const text = item.blocks
    .flatMap((block) => (block.type === "text" ? [block.text] : []))
    .join("\n\n");
  const stripped =
    item.role === "user" ? stripTaggedUserInstructionBlocks(text) : text;
  const trimmed = stripped.trim();
  return trimmed ? trimmed : null;
}

function messageRoleLabel(item: MessageItem, assistantLabel: string) {
  if (item.participant?.label) {
    return item.participant.label;
  }
  return item.role === "assistant" ? assistantLabel : "User";
}

function containsToolActivity(item: ActivityItem) {
  return (
    item.type === "tool" ||
    (item.type === "activityGroup" && item.children.some((child) => child.type === "tool"))
  );
}

function containsTranscriptActivity(item: ActivityItem) {
  return (
    item.type === "tool" ||
    item.type === "thinking" ||
    (item.type === "activityGroup" && item.children.length > 0)
  );
}

export function timelineItemsContainTranscriptActivity(items: MessageItem[]) {
  return items.some((item) => item.activities.some(containsTranscriptActivity));
}

function withoutTranscriptActivities(items: MessageItem[]) {
  return items
    .map((item) =>
      item.activities.length > 0 ? { ...item, activities: [] } : item,
    )
    .filter((item) => item.blocks.length > 0);
}

/**
 * The collapsed user-facing answer begins after the final tool-bearing item
 * and never includes tool or thinking activity disclosures.
 */
export function timelineItemsAfterLastToolCall(items: MessageItem[]) {
  for (let index = items.length - 1; index >= 0; index -= 1) {
    if (items[index].activities.some(containsToolActivity)) {
      const after = items.slice(index + 1);
      // Trailing tool activity with no text after it (a TodoWrite wrap-up, a
      // post-answer verification) attaches to the same item as the final
      // answer text, making the answer item itself the boundary. Slicing it
      // away would present a completed response as unavailable, so carry the
      // boundary item's message content forward — its activities remain
      // visible in the full trace.
      const boundary = items[index];
      if (
        boundary.role === "assistant" &&
        messageItemText(boundary) !== null &&
        !assistantTextFromTimelineItems(after)
      ) {
        return withoutTranscriptActivities([boundary, ...after]);
      }
      return withoutTranscriptActivities(after);
    }
  }
  return withoutTranscriptActivities(items);
}

// A FIFO of pending tool calls that matches results in amortized O(1). Matched
// entries are tombstoned in a shared `resolved` set rather than spliced out of
// an array, and a head cursor skips past dead entries, so a transcript with N
// tool calls and N results is processed in O(N) instead of O(N²). The same
// entry can live in more than one queue (by id and by status); the shared
// tombstone keeps every queue consistent.
class PendingToolQueue {
  private readonly items: ToolEntry[] = [];
  private head = 0;

  push(entry: ToolEntry): void {
    this.items.push(entry);
  }

  /** Removes and returns the oldest live entry satisfying `matches`, or null. */
  take(resolved: Set<ToolEntry>, matches: (entry: ToolEntry) => boolean): ToolEntry | null {
    while (this.head < this.items.length && resolved.has(this.items[this.head])) {
      this.head += 1;
    }
    for (let index = this.head; index < this.items.length; index += 1) {
      const entry = this.items[index];
      if (!resolved.has(entry) && matches(entry)) {
        resolved.add(entry);
        return entry;
      }
    }
    return null;
  }
}

export function buildTimelineItems(turns: Turn[], showActivityDetail = true): MessageItem[] {
  const items: MessageItem[] = [];
  // Tool calls awaiting a result, indexed for O(1) matching. A result carrying
  // a tool-use id collapses into its own call (even across a status boundary);
  // an id-less result collapses into the oldest pending call; a keyed result
  // whose call isn't pending renders as its own row. Every pending call is in
  // the by-status queue; those with an id are also in the by-id queue. A match
  // through either tombstones the entry in `resolved` so the other queue skips
  // it.
  const resolved = new Set<ToolEntry>();
  const pendingByStatus = new Map<string, PendingToolQueue>();
  const pendingById = new Map<string, PendingToolQueue>();
  // Id-less results may only claim a call from the same presentation state.
  // Outcome status alone is insufficient now that rolled-back and active calls
  // can both have no status; keep context membership in the queue key too.
  const activityStateKey = (
    status?: TurnTimelineStatus,
    contextStatus?: TurnTimelineContextStatus,
  ) => `${status ?? ""}\0${contextStatus ?? ""}`;
  const queueFor = (map: Map<string, PendingToolQueue>, key: string) => {
    let queue = map.get(key);
    if (!queue) {
      queue = new PendingToolQueue();
      map.set(key, queue);
    }
    return queue;
  };
  // Keys derive from the originating turn id and block position, not a running
  // sequence number: when older turns are truncated (the per-agent cap) or a
  // graph refresh prepends history, sequence-numbered keys shift and React
  // transfers per-item DOM state — expanded <details> — onto unrelated content.
  // Turn ids are stable across re-parses, so these keys stick to their content.
  // Item creation always happens while some block is being processed, and each
  // block creates at most one item per prefix, so keys stay unique.
  let keyBase = "";

  const nextKey = (prefix: string) => `${prefix}-${keyBase}`;

  const applyTimestamp = (item: MessageItem, timestamp?: number | null) => {
    if (typeof timestamp === "number") {
      // Turns arrive in order; keep the latest so end-of-group footers reflect
      // when the assistant last wrote into this card.
      item.timestamp = timestamp;
    }
  };

  const createMessageItem = (
    role: string,
    sourceTurnId: string,
    block?: MessageBlock,
    status?: TurnTimelineStatus,
    contextStatus?: TurnTimelineContextStatus,
    participant?: ThreadParticipant | null,
    anchor?: MessageAnchor | null,
    timestamp?: number | null,
  ): MessageItem => ({
    type: "message",
    key: nextKey(`message-${role}`),
    role,
    participant,
    blocks: block ? [block] : [],
    activities: [],
    status,
    contextStatus,
    anchor,
    timestamp: typeof timestamp === "number" ? timestamp : null,
    sourceTurnIds: [sourceTurnId],
    blockSourceTurnIds: block ? [sourceTurnId] : [],
  });

  const addSourceTurn = (item: MessageItem, sourceTurnId: string) => {
    if (item.sourceTurnIds[item.sourceTurnIds.length - 1] !== sourceTurnId) {
      item.sourceTurnIds.push(sourceTurnId);
    }
  };

  const pushMessageBlock = (
    role: string,
    sourceTurnId: string,
    block: MessageBlock,
    status?: TurnTimelineStatus,
    contextStatus?: TurnTimelineContextStatus,
    participant?: ThreadParticipant | null,
    anchor?: MessageAnchor | null,
    timestamp?: number | null,
  ) => {
    const previous = items[items.length - 1];
    if (
      previous?.role === role &&
      previous.status === status &&
      previous.contextStatus === contextStatus &&
      participantKey(previous.participant) === participantKey(participant) &&
      previous.activities.length === 0
    ) {
      // Deliberately keeps the existing anchor: the card now spans several
      // turns, and a fork from it must branch before the earliest.
      previous.blocks.push(block);
      previous.blockSourceTurnIds.push(sourceTurnId);
      addSourceTurn(previous, sourceTurnId);
      applyTimestamp(previous, timestamp);
      return;
    }
    items.push(
      createMessageItem(
        role,
        sourceTurnId,
        block,
        status,
        contextStatus,
        participant,
        anchor,
        timestamp,
      ),
    );
  };

  const assistantActivityOwner = (
    sourceTurnId: string,
    status?: TurnTimelineStatus,
    contextStatus?: TurnTimelineContextStatus,
    participant?: ThreadParticipant | null,
    timestamp?: number | null,
  ) => {
    // Walk back only across assistant items: a user (or system) message is a
    // hard boundary. Without it, a reply that opens with thinking or a tool
    // call (the normal shape of an agent response) attaches those activities
    // to the assistant message from the PREVIOUS exchange, rendering them
    // above the user message that triggered them. Tool-result-only user turns
    // never create items, so they don't block the walk.
    for (let index = items.length - 1; index >= 0; index -= 1) {
      if (items[index].role !== "assistant") {
        break;
      }
      if (
        items[index].status === status &&
        items[index].contextStatus === contextStatus &&
        participantKey(items[index].participant) === participantKey(participant)
      ) {
        addSourceTurn(items[index], sourceTurnId);
        applyTimestamp(items[index], timestamp);
        return items[index];
      }
    }
    const fallback = createMessageItem(
      "assistant",
      sourceTurnId,
      undefined,
      status,
      contextStatus,
      participant,
      null,
      timestamp,
    );
    items.push(fallback);
    return fallback;
  };

  const pushThinkingValue = (
    sourceTurnId: string,
    value: unknown,
    status?: TurnTimelineStatus,
    contextStatus?: TurnTimelineContextStatus,
    participant?: ThreadParticipant | null,
    timestamp?: number | null,
  ) => {
    const owner = assistantActivityOwner(
      sourceTurnId,
      status,
      contextStatus,
      participant,
      timestamp,
    );
    const previousActivity = owner.activities[owner.activities.length - 1];
    if (previousActivity?.type === "thinking" && previousActivity.status === status) {
      previousActivity.values.push(value);
      return;
    }
    owner.activities.push({
      type: "thinking",
      key: nextKey("thinking"),
      values: [value],
      status,
    });
  };

  const pushToolEntry = (
    sourceTurnId: string,
    entry: ToolEntry,
    status?: TurnTimelineStatus,
    contextStatus?: TurnTimelineContextStatus,
    participant?: ThreadParticipant | null,
    timestamp?: number | null,
  ) => {
    assistantActivityOwner(
      sourceTurnId,
      status,
      contextStatus,
      participant,
      timestamp,
    ).activities.push(entry);
  };

  const registerToolUse = (
    sourceTurnId: string,
    block: ToolUseBlock,
    status?: TurnTimelineStatus,
    contextStatus?: TurnTimelineContextStatus,
    participant?: ThreadParticipant | null,
    timestamp?: number | null,
  ) => {
    const entry: ToolEntry = {
      type: "tool",
      key: nextKey("tool"),
      id: block.id ?? null,
      name: block.name,
      input: block.input,
      isError: false,
      status,
    };
    pushToolEntry(sourceTurnId, entry, status, contextStatus, participant, timestamp);
    queueFor(pendingByStatus, activityStateKey(status, contextStatus)).push(entry);
    if (entry.id) {
      queueFor(pendingById, entry.id).push(entry);
    }
  };

  const attachToolResult = (
    sourceTurnId: string,
    block: ToolResultBlock,
    status?: TurnTimelineStatus,
    contextStatus?: TurnTimelineContextStatus,
    participant?: ThreadParticipant | null,
    timestamp?: number | null,
  ) => {
    const toolUseId = block.toolUseId ?? null;

    // A keyed result belongs to its own call: try id+status first, then id
    // alone — a call/result pair can straddle a status boundary (a fork
    // supersedes the call's turn but not the result's, an interruption marks
    // only one side), and the id is authoritative across it. What a keyed
    // result must never do is take over a DIFFERENT call: with no id match at
    // all (its call fell out of the visible turn window), a status-only
    // fallback would show the stray payload as that call's result and strand
    // the call's real result as a duplicate orphan row below it. Only an
    // id-less result falls back to the oldest same-status pending call.
    let matched: ToolEntry | null = null;
    if (toolUseId) {
      const idQueue = pendingById.get(toolUseId);
      if (idQueue) {
        matched =
          idQueue.take(resolved, (entry) => entry.status === status) ??
          idQueue.take(resolved, () => true);
      }
    } else {
      matched =
        pendingByStatus
          .get(activityStateKey(status, contextStatus))
          ?.take(resolved, () => true) ?? null;
    }

    if (matched) {
      matched.result = block.content;
      matched.isError = block.isError;
      return;
    }

    // A result with no pending call at all — surface it on its own row.
    pushToolEntry(
      sourceTurnId,
      {
        type: "tool",
        key: nextKey("tool-result"),
        id: toolUseId,
        name: block.isError ? "Tool error" : "Tool result",
        result: block.content,
        isError: block.isError,
        status,
      },
      status,
      contextStatus,
      participant,
      timestamp,
    );
  };

  for (const turn of turns) {
    const status = timelineStatus(turn.status);
    const contextStatus = timelineContextStatus(turn.contextStatus);
    const participant = turn.participant ?? null;
    const timestamp = turn.timestamp ?? null;
    const anchor: MessageAnchor = {
      nativeId: turn.nativeId ?? null,
      parentNativeId: turn.parentNativeId ?? null,
      sourceIndex: turn.sourceIndex,
    };
    for (const [blockIndex, block] of turn.blocks.entries()) {
      keyBase = `${turn.id}:${blockIndex}`;
      switch (block.type) {
        case "text":
          pushMessageBlock(
            turn.role,
            turn.id,
            block,
            status,
            contextStatus,
            participant,
            anchor,
            timestamp,
          );
          break;
        case "toolUse":
          if (showActivityDetail) {
            registerToolUse(turn.id, block, status, contextStatus, participant, timestamp);
          }
          break;
        case "toolResult":
          if (showActivityDetail) {
            attachToolResult(turn.id, block, status, contextStatus, participant, timestamp);
          }
          break;
        case "raw":
          if (turn.role === "assistant") {
            if (showActivityDetail) {
              pushThinkingValue(
                turn.id,
                block.value,
                status,
                contextStatus,
                participant,
                timestamp,
              );
            }
          } else {
            pushMessageBlock(
              turn.role,
              turn.id,
              block,
              status,
              contextStatus,
              participant,
              anchor,
              timestamp,
            );
          }
          break;
      }
    }
  }

  return items.map((item) => ({
    ...item,
    activities: groupActivityItems(item.activities),
  }));
}

function groupActivityItems(activities: ActivityItem[]): ActivityItem[] {
  if (activities.length <= 1) {
    return activities;
  }

  const children = activities.filter(isActivityLeafItem);
  if (children.length <= 1) {
    return activities;
  }

  return [
    {
      type: "activityGroup",
      key: `activity-group-${children[0].key}`,
      children,
      toolCallCount: countUniqueToolCalls(children),
      status: commonActivityStatus(children),
    },
  ];
}

export function isActivityLeafItem(item: ActivityItem): item is ActivityLeafItem {
  return item.type === "tool" || item.type === "thinking";
}

function countUniqueToolCalls(items: ActivityLeafItem[]) {
  const counted = new Set<string>();
  for (const item of items) {
    if (item.type !== "tool") {
      continue;
    }
    counted.add(item.id ? `id:${item.id}` : `entry:${item.key}`);
  }
  return counted.size;
}

function commonActivityStatus(items: ActivityLeafItem[]): TurnTimelineStatus | undefined {
  const [first] = items;
  if (!first?.status) {
    return undefined;
  }
  return items.every((item) => item.status === first.status) ? first.status : undefined;
}

function timelineStatus(status: Turn["status"]): TurnTimelineStatus | undefined {
  return status === "superseded" || status === "interrupted" || status === "uncertain"
    ? status
    : undefined;
}

function timelineContextStatus(
  status: Turn["contextStatus"],
): TurnTimelineContextStatus | undefined {
  return status === "rolledBack" ? status : undefined;
}

// Includes the label: the header renders participant.label when present, so
// two participants that differ only by label must neither merge into one
// message item nor compare equal in the memo below.
export function participantKey(participant: ThreadParticipant | null | undefined) {
  if (!participant) {
    return "";
  }
  return `${participant.kind}:${participant.actorId}:${participant.label ?? ""}`;
}

// Structural equality for rebuilt timeline items. buildTimelineItems re-runs on
// every turns change (every parsed line while an agent streams) and always
// allocates fresh item wrappers, so an identity-based memo would miss for
// every message on every turn event — re-running the markdown render over the
// entire transcript per event. The block/value objects *inside* the wrappers
// keep their identity for unchanged turns (turn reconciliation reuses turn
// objects), so wrappers are compared structurally with reference equality at
// the leaves: an append then re-renders only the items whose content actually
// changed. Keys derive from the originating turn id and block position, so key
// equality is a real content signal (and survives truncation/prepending).
function sameMessageBlockList(a: MessageBlock[], b: MessageBlock[]) {
  return a.length === b.length && a.every((block, index) => block === b[index]);
}

function sameActivityLeaf(a: ActivityLeafItem, b: ActivityLeafItem): boolean {
  if (a.key !== b.key || a.status !== b.status) {
    return false;
  }
  if (a.type === "tool" && b.type === "tool") {
    return (
      a.id === b.id &&
      a.name === b.name &&
      a.input === b.input &&
      a.result === b.result &&
      a.isError === b.isError
    );
  }
  if (a.type === "thinking" && b.type === "thinking") {
    return (
      a.values.length === b.values.length &&
      a.values.every((value, index) => value === b.values[index])
    );
  }
  return false;
}

function sameActivityItem(a: ActivityItem, b: ActivityItem): boolean {
  if (a.type === "activityGroup" || b.type === "activityGroup") {
    return (
      a.type === "activityGroup" &&
      b.type === "activityGroup" &&
      a.key === b.key &&
      a.status === b.status &&
      a.toolCallCount === b.toolCallCount &&
      a.children.length === b.children.length &&
      a.children.every((child, index) => sameActivityLeaf(child, b.children[index]))
    );
  }
  return sameActivityLeaf(a, b);
}

export function sameMessageItem(a: MessageItem, b: MessageItem): boolean {
  return (
    a.key === b.key &&
    a.role === b.role &&
    a.status === b.status &&
    a.contextStatus === b.contextStatus &&
    a.timestamp === b.timestamp &&
    a.sourceTurnIds.length === b.sourceTurnIds.length &&
    a.sourceTurnIds.every((turnId, index) => turnId === b.sourceTurnIds[index]) &&
    a.blockSourceTurnIds.length === b.blockSourceTurnIds.length &&
    a.blockSourceTurnIds.every(
      (turnId, index) => turnId === b.blockSourceTurnIds[index],
    ) &&
    participantKey(a.participant) === participantKey(b.participant) &&
    sameMessageBlockList(a.blocks, b.blocks) &&
    a.activities.length === b.activities.length &&
    a.activities.every((activity, index) => sameActivityItem(activity, b.activities[index]))
  );
}

export interface TimelineAnnotations {
  byItemKey: Map<string, TranscriptAnnotation[]>;
  orphaned: TranscriptAnnotation[];
}

/** Places each saved excerpt after the visible message containing its source
 * turn. The final matching item wins when presentation folding changes, and
 * missing sources remain available for an explicit orphaned section. */
export function annotationsForTimeline(
  items: MessageItem[],
  annotations: TranscriptAnnotation[],
): TimelineAnnotations {
  const itemKeyByTurnId = new Map<string, string>();
  for (const item of items) {
    if (item.role !== "assistant") {
      continue;
    }
    for (const turnId of item.sourceTurnIds) {
      itemKeyByTurnId.set(turnId, item.key);
    }
  }
  const byItemKey = new Map<string, TranscriptAnnotation[]>();
  const orphaned: TranscriptAnnotation[] = [];
  const ordered = [...annotations].sort(
    (left, right) => left.createdAt - right.createdAt || left.id.localeCompare(right.id),
  );
  for (const annotation of ordered) {
    const itemKey = itemKeyByTurnId.get(annotation.sourceTurnId);
    if (!itemKey) {
      orphaned.push(annotation);
      continue;
    }
    const existing = byItemKey.get(itemKey);
    if (existing) {
      existing.push(annotation);
    } else {
      byItemKey.set(itemKey, [annotation]);
    }
  }
  return { byItemKey, orphaned };
}

/**
 * Latest timestamp across a trailing run of assistant items ending at
 * `endIndex` (inclusive). Returns null when none of the items carry a native
 * timestamp, so the footer can stay hidden rather than invent a time.
 */
export function assistantGroupTimestamp(
  items: MessageItem[],
  endIndex: number,
): number | null {
  if (endIndex < 0 || endIndex >= items.length || items[endIndex].role !== "assistant") {
    return null;
  }
  let latest: number | null = null;
  for (let index = endIndex; index >= 0; index -= 1) {
    if (items[index].role !== "assistant") {
      break;
    }
    const timestamp = items[index].timestamp;
    if (typeof timestamp === "number" && (latest === null || timestamp > latest)) {
      latest = timestamp;
    }
  }
  return latest;
}

/**
 * The consecutive assistant run containing `itemKey`.
 *
 * Timestamp footers are attached to the last item in one of these runs. Focus
 * mode stores that item's key, then resolves the full run again on every
 * transcript update so a continued response can grow without dropping out of
 * the reader.
 */
export function assistantRunForItemKey(
  items: MessageItem[],
  itemKey: string,
): MessageItem[] {
  const itemIndex = items.findIndex((item) => item.key === itemKey);
  if (itemIndex < 0 || items[itemIndex].role !== "assistant") {
    return [];
  }
  let startIndex = itemIndex;
  while (startIndex > 0 && items[startIndex - 1].role === "assistant") {
    startIndex -= 1;
  }
  let endIndex = itemIndex;
  while (endIndex + 1 < items.length && items[endIndex + 1].role === "assistant") {
    endIndex += 1;
  }
  return items.slice(startIndex, endIndex + 1);
}

/** Absolute wall-clock label (also used as the hover title on relative labels). */
export function formatAbsoluteMessageTimestamp(
  ms: number,
  now = Date.now(),
  locale?: string,
): string {
  const date = new Date(ms);
  const nowDate = new Date(now);
  const time = date.toLocaleTimeString(locale, {
    hour: "numeric",
    minute: "2-digit",
  });
  const sameDay =
    date.getFullYear() === nowDate.getFullYear() &&
    date.getMonth() === nowDate.getMonth() &&
    date.getDate() === nowDate.getDate();
  if (sameDay) {
    return time;
  }
  const sameYear = date.getFullYear() === nowDate.getFullYear();
  const day = date.toLocaleDateString(
    locale,
    sameYear
      ? { month: "short", day: "numeric" }
      : { month: "short", day: "numeric", year: "numeric" },
  );
  return `${day}, ${time}`;
}

/**
 * End-of-assistant-group footer label: relative when under 24 hours old
 * ("just now", "5 min ago", "6 hours ago"), otherwise absolute wall clock.
 */
export function formatMessageTimestamp(
  ms: number,
  now = Date.now(),
  locale?: string,
): string {
  const ageMs = now - ms;
  if (ageMs < 60_000) {
    return "just now";
  }
  if (ageMs < 60 * 60_000) {
    const minutes = Math.floor(ageMs / 60_000);
    return `${minutes} min ago`;
  }
  if (ageMs < 24 * 60 * 60_000) {
    const hours = Math.floor(ageMs / (60 * 60_000));
    return hours === 1 ? "1 hour ago" : `${hours} hours ago`;
  }
  return formatAbsoluteMessageTimestamp(ms, now, locale);
}

/**
 * A live transcript tail already communicates recency with its working
 * indicator, so omit only the redundant "just now" assistant footer there.
 * Older tail timestamps and timestamps within the transcript remain useful.
 */
export function shouldShowAssistantGroupTimestamp(
  timestamp: number,
  options: {
    atTranscriptTail: boolean;
    working: boolean;
    now?: number;
  },
): boolean {
  return !(
    options.atTranscriptTail &&
    options.working &&
    formatMessageTimestamp(timestamp, options.now) === "just now"
  );
}

export function messageItemText(item: MessageItem): string | null {
  const text = item.blocks
    .flatMap((block) => (block.type === "text" ? [block.text] : []))
    .join("\n\n");
  return text.trim() ? text : null;
}

/** Copyable user/assistant source text, without injected tagged instructions. */
export function messageItemCopyText(item: MessageItem): string | null {
  if (item.role !== "user" && item.role !== "assistant") {
    return null;
  }
  const text = messageItemText(item);
  if (text === null) {
    return null;
  }
  const stripped =
    item.role === "user"
      ? stripTaggedUserInstructionBlocks(text)
      : stripTaggedInstructionBlocks(text);
  return stripped.trim() ? stripped : null;
}

/**
 * One aggregate copy payload per user-delimited assistant run.
 *
 * System messages and fully tagged user-role instructions are omitted without
 * ending the run. Activity-only assistant items and assistant text that
 * sanitizes to empty are likewise skipped, so the menu lands on the first
 * visible copyable message.
 */
export function assistantRunCopyTextByItemKey(items: MessageItem[]) {
  const copyTextByKey = new Map<string, string>();
  let firstAssistantKey: string | null = null;
  let parts: string[] = [];

  const flush = () => {
    if (firstAssistantKey && parts.length > 0) {
      copyTextByKey.set(firstAssistantKey, parts.join("\n\n").trim());
    }
    firstAssistantKey = null;
    parts = [];
  };

  for (const item of items) {
    if (item.role === "user") {
      if (messageItemIsTaggedInstruction(item)) {
        continue;
      }
      flush();
      continue;
    }
    if (item.role !== "assistant") {
      continue;
    }
    const copyText = messageItemCopyText(item);
    if (!copyText) {
      continue;
    }
    firstAssistantKey ??= item.key;
    parts.push(copyText);
  }
  flush();

  return copyTextByKey;
}

export function messageItemIsTaggedInstruction(item: MessageItem) {
  if (item.role !== "user" || item.blocks.length !== 1) {
    return false;
  }
  const [block] = item.blocks;
  return block.type === "text" && taggedUserInstructionDetails(block.text) !== null;
}

// True when the item carries a tool call (a tool row, or a tool inside a grouped
// activity) — used to spot a continued agent turn whose name should be dropped.
export function hasToolCall(item: MessageItem): boolean {
  return item.activities.some(
    (activity) =>
      activity.type === "tool" ||
      (activity.type === "activityGroup" &&
        activity.children.some((child) => child.type === "tool")),
  );
}
