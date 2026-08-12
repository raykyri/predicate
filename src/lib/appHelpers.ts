import {
  isTerminalFontLoaded,
  TERMINAL_FONT_SIZE,
} from "./terminalFont";
import { FONT_OPTIONS } from "./settings";
import { findAgentUiAdapter } from "../adapters";
import type {
  AgentInfo,
  GlobalDraft,
  PaneInfo,
  PaneSplitInfo,
  QmuxEvent,
  QueuedTurn,
  RuntimeConfig,
  ThreadGraph,
  TranscriptCopyPayload,
  TranscriptHookEvent,
  Turn,
} from "../types";

const TRANSCRIPT_COPY_VERSION = 1;
const DEFAULT_FONT_STACK = FONT_OPTIONS[0].stack;

let measuredTerminalCellSize: { width: number; height: number } | null = null;

export function clamp(value: number, min: number, max: number) {
  return Math.min(Math.max(value, min), max);
}

export function firstUserTurnText(turn: Turn): string | null {
  if (
    turn.role !== "user" ||
    turn.status === "superseded" ||
    turn.contextStatus === "rolledBack"
  ) {
    return null;
  }

  for (const block of turn.blocks) {
    if (block.type !== "text") {
      continue;
    }
    const trimmed = block.text.trim();
    if (trimmed) {
      return trimmed;
    }
  }

  return null;
}

export function latestUserTurnText(turns: Turn[]): string | null {
  for (let index = turns.length - 1; index >= 0; index -= 1) {
    const text = firstUserTurnText(turns[index]);
    if (text) {
      return text;
    }
  }
  return null;
}

/** When active context last moved: the newest included, non-superseded turn
 * that carries a native timestamp. */
export function latestTurnTimestamp(turns: Turn[]): number | null {
  for (let index = turns.length - 1; index >= 0; index -= 1) {
    const turn = turns[index];
    if (turn.status === "superseded" || turn.contextStatus === "rolledBack") {
      continue;
    }
    if (typeof turn.timestamp === "number") {
      return turn.timestamp;
    }
  }
  return null;
}

/** The last words the agent said: the final non-empty text block of the most
 * recent assistant turn that has one. Tool-only turns (an agent mid-work) are
 * walked past so the latest spoken reply still surfaces. */
export function latestAssistantTurnText(turns: Turn[]): string | null {
  for (let index = turns.length - 1; index >= 0; index -= 1) {
    const turn = turns[index];
    if (
      turn.role !== "assistant" ||
      turn.status === "superseded" ||
      turn.contextStatus === "rolledBack"
    ) {
      continue;
    }
    for (let blockIndex = turn.blocks.length - 1; blockIndex >= 0; blockIndex -= 1) {
      const block = turn.blocks[blockIndex];
      if (block.type !== "text") {
        continue;
      }
      const trimmed = block.text.trim();
      if (trimmed) {
        return trimmed;
      }
    }
  }
  return null;
}

export const DEFAULT_SHELL_TITLE = "Shell";

/** The title a pane is created with ("Shell" or the adapter's label). A pane
 * whose stored title differs has been renamed or auto-titled; shared by the
 * sidebar and the quick launcher so both resolve tab names by the same rule. */
export function defaultPaneTitle(
  pane: PaneInfo,
  agent: AgentInfo | undefined,
  config: RuntimeConfig | null,
): string | null {
  if (pane.kind === "shell") {
    return DEFAULT_SHELL_TITLE;
  }
  if (!agent) {
    return null;
  }
  return (
    config?.adapters.find((adapter) => adapter.id === agent.adapter)?.label ??
    findAgentUiAdapter(agent.adapter)?.label ??
    null
  );
}

/** Fixed-position placement for a popover anchored to a control inside a pane. */
export type PanePopoverPlacement = {
  left: number;
  top: number;
  maxHeight: number;
  maxWidth: number;
};

/**
 * Place a fixed popover next to a trigger, clamped inside the right pane (or
 * the viewport). Horizontal `align: "start"` left-aligns to the trigger and
 * grows right (toward center for left-edge controls); `"end"` right-aligns and
 * grows left (toward center for right-edge controls). Vertical preference flips
 * when the other side has more room.
 */
export function placePanePopover(args: {
  triggerRect: DOMRect;
  popoverSize: { width: number; height: number };
  paneRect?: DOMRect | null;
  align: "start" | "end";
  prefer: "above" | "below";
  margin?: number;
  gap?: number;
}): PanePopoverPlacement {
  const margin = args.margin ?? 8;
  const gap = args.gap ?? 6;
  const pane = args.paneRect ?? null;
  const boundLeft = (pane ? pane.left : 0) + margin;
  const boundRight = (pane ? pane.right : window.innerWidth) - margin;
  const boundTop = (pane ? pane.top : 0) + margin;
  const boundBottom = (pane ? pane.bottom : window.innerHeight) - margin;
  const maxWidth = Math.max(0, boundRight - boundLeft);
  const width = Math.min(args.popoverSize.width, maxWidth);

  let left =
    args.align === "end" ? args.triggerRect.right - width : args.triggerRect.left;
  // When the popover is wider than the bounds, pin to the inward edge so it
  // grows toward the center rather than spilling off the outer edge.
  if (width >= maxWidth) {
    left = boundLeft;
  } else {
    left = Math.max(boundLeft, Math.min(left, boundRight - width));
  }

  const availableAbove = Math.max(0, args.triggerRect.top - gap - boundTop);
  const availableBelow = Math.max(0, boundBottom - (args.triggerRect.bottom + gap));

  let prefer = args.prefer;
  if (
    prefer === "below" &&
    args.popoverSize.height > availableBelow &&
    availableAbove > availableBelow
  ) {
    prefer = "above";
  } else if (
    prefer === "above" &&
    args.popoverSize.height > availableAbove &&
    availableBelow > availableAbove
  ) {
    prefer = "below";
  }

  const maxHeight = prefer === "above" ? availableAbove : availableBelow;
  const height = Math.min(args.popoverSize.height, maxHeight);
  const top =
    prefer === "above"
      ? args.triggerRect.top - gap - height
      : args.triggerRect.bottom + gap;

  return {
    left,
    top: clamp(top, boundTop, Math.max(boundTop, boundBottom - height)),
    maxHeight,
    maxWidth,
  };
}

/** Resolve the nearest `.turn-pane` bounds for clamping a right-pane popover. */
export function turnPaneRectFrom(el: Element | null): DOMRect | null {
  const pane = el?.closest(".turn-pane");
  return pane instanceof HTMLElement ? pane.getBoundingClientRect() : null;
}

export function selectPaneAfterClose(
  panes: PaneInfo[],
  closedPaneId: string,
  paneSplits: PaneSplitInfo[] = [],
  options?: {
    isPaneInCollapsedGroup?: (pane: PaneInfo) => boolean;
  },
): string | null {
  const selectPreferredPane = (candidates: PaneInfo[]) => {
    if (candidates.length === 0) {
      return null;
    }
    const isPaneInCollapsedGroup = options?.isPaneInCollapsedGroup;
    if (!isPaneInCollapsedGroup) {
      return candidates[0].id;
    }
    return candidates.find((pane) => !isPaneInCollapsedGroup(pane))?.id ?? candidates[0].id;
  };

  const closedIndex = panes.findIndex((pane) => pane.id === closedPaneId);
  if (closedIndex === -1) {
    return selectPreferredPane(panes);
  }

  const availablePaneIds = new Set(panes.map((pane) => pane.id));
  availablePaneIds.delete(closedPaneId);
  const paneById = new Map(panes.map((pane) => [pane.id, pane]));
  const candidates: PaneInfo[] = [];
  const candidateIds = new Set<string>();
  const addCandidate = (paneId?: string) => {
    if (!paneId || paneId === closedPaneId || candidateIds.has(paneId)) {
      return;
    }
    if (!availablePaneIds.has(paneId)) {
      return;
    }
    const pane = paneById.get(paneId);
    if (!pane) {
      return;
    }
    candidateIds.add(paneId);
    candidates.push(pane);
  };

  const split = paneSplits.find((candidate) => candidate.paneIds.includes(closedPaneId));
  const splitIndex = split?.paneIds.indexOf(closedPaneId) ?? -1;
  if (split && splitIndex >= 0) {
    for (let index = splitIndex - 1; index >= 0; index -= 1) {
      addCandidate(split.paneIds[index]);
    }
    for (let index = splitIndex + 1; index < split.paneIds.length; index += 1) {
      addCandidate(split.paneIds[index]);
    }
  }

  for (let offset = 1; offset < panes.length; offset += 1) {
    addCandidate(panes[closedIndex - offset]?.id);
    addCandidate(panes[closedIndex + offset]?.id);
  }

  return selectPreferredPane(candidates);
}

export function cycleTabId(
  tabIds: string[],
  activeTabId: string | null | undefined,
  direction: -1 | 1,
  paneSplits: PaneSplitInfo[] = [],
  fallbackIndex?: number,
): string | null {
  if (tabIds.length === 0) {
    return null;
  }

  const listedIndex = tabIds.indexOf(activeTabId ?? "");
  const currentIndex =
    listedIndex !== -1 ? listedIndex : (fallbackIndex ?? (direction === 1 ? -1 : 0));
  const activeSplitPaneIds =
    listedIndex !== -1 && activeTabId
      ? new Set(
          paneSplits.find((candidate) => candidate.paneIds.includes(activeTabId))?.paneIds ?? [],
        )
      : null;

  let nextIndex = currentIndex;
  for (let visited = 0; visited < tabIds.length; visited += 1) {
    nextIndex = (nextIndex + direction + tabIds.length) % tabIds.length;
    const nextTabId = tabIds[nextIndex];
    if (!activeSplitPaneIds?.has(nextTabId)) {
      return nextTabId;
    }
  }

  return listedIndex !== -1 ? tabIds[listedIndex] : tabIds[0];
}

// Validates a turn payload arriving over the backend event stream before it is
// trusted as a Turn. The data is structured by Rust, but guarding at the boundary
// keeps a malformed/renamed field from silently producing an invalid turn the UI
// then renders.
export function isTurn(value: unknown): value is Turn {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const turn = value as Record<string, unknown>;
  return (
    typeof turn.id === "string" &&
    typeof turn.agentId === "string" &&
    typeof turn.role === "string" &&
    Array.isArray(turn.blocks) &&
    optionalTurnStatus(turn.status) &&
    optionalTurnStatusReason(turn.statusReason) &&
    optionalTurnContextStatus(turn.contextStatus)
  );
}

function optionalTurnStatus(value: unknown) {
  return (
    value === undefined ||
    value === null ||
    value === "superseded" ||
    value === "interrupted" ||
    value === "uncertain"
  );
}

function optionalTurnStatusReason(value: unknown) {
  return (
    value === undefined ||
    value === null ||
    value === "codexRollback" ||
    value === "interrupted" ||
    value === "claudePromptBranch" ||
    value === "unknownBranch"
  );
}

function optionalTurnContextStatus(value: unknown) {
  return value === undefined || value === null || value === "rolledBack";
}

// Validates an agent payload arriving on an event before it is applied to local
// state, mirroring isTurn. Status events now carry the updated agent so the UI can
// apply changes surgically instead of refetching the whole list every time.
export function isAgentInfo(value: unknown): value is AgentInfo {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const agent = value as Record<string, unknown>;
  return (
    typeof agent.id === "string" &&
    typeof agent.adapter === "string" &&
    typeof agent.status === "string"
  );
}

export function isGlobalDraft(value: unknown): value is GlobalDraft {
  return (
    typeof value === "object" &&
    value !== null &&
    typeof (value as Record<string, unknown>).id === "string" &&
    typeof (value as Record<string, unknown>).text === "string"
  );
}

export function isQueuedTurn(value: unknown): value is QueuedTurn {
  return (
    typeof value === "object" &&
    value !== null &&
    typeof (value as Record<string, unknown>).text === "string"
  );
}

// Forking needs an adapter with a native fork command and a recorded session id to
// resume. Single owner of the gate used by context/launcher actions, the
// selection "Ask in new thread" button, and the composer's queue-and-fork options.
export function agentCanFork(agent: AgentInfo | null | undefined): boolean {
  if (!agent?.sessionId) {
    return false;
  }
  const adapter = findAgentUiAdapter(agent.adapter);
  return Boolean(
    adapter?.supportsFork && (adapter.canFork ? adapter.canFork(agent) : true),
  );
}

// Forking from a chosen message additionally needs an adapter that can preserve
// its native history at that anchor (by safe transcript synthesis or a native
// session-tree API), plus a transcript on disk. Mirrors the backend capability;
// the action is hidden rather than disabled where it does not apply.
export function agentSupportsForkAtMessage(agent: AgentInfo | null | undefined): boolean {
  return Boolean(
    agentCanFork(agent) &&
      agent?.transcriptPath &&
      findAgentUiAdapter(agent?.adapter)?.supportsForkAtMessage,
  );
}

// Applies a single updated agent to the list: replaces it in place when present,
// otherwise appends it (e.g. a freshly spawned agent), preserving order.
//
// Returns the previous array itself when the incoming agent is content-equal to
// the stored one. A busy agent's hook stream (PreToolUse/PostToolUse per tool
// call) mostly re-delivers a byte-identical agent, and handing out a fresh
// array for every event committed a full-app render and invalidated every
// agents-keyed memo (turn-info cache, tray snapshot, home cascades) each time.
// Agents are small flat payloads, so one JSON comparison per event is far
// cheaper than the render it avoids.
export function upsertAgent(agents: AgentInfo[], updated: AgentInfo): AgentInfo[] {
  let unchanged = false;
  let replaced = false;
  const next = agents.map((agent) => {
    if (agent.id === updated.id) {
      replaced = true;
      if (JSON.stringify(agent) === JSON.stringify(updated)) {
        unchanged = true;
        return agent;
      }
      return updated;
    }
    return agent;
  });
  if (!replaced) {
    return [...next, updated];
  }
  return unchanged ? agents : next;
}

// Preserves turn object identity across a `turn.updated` reset. A reset ships
// the agent's whole turn list re-parsed into fresh objects — fired for every
// typed user prompt and lifecycle marker — even though almost every turn is
// content-identical to what the app already holds. Downstream memoization
// (the per-agent turn-info cache, the per-message timeline memo) keys on turn
// identity, so handing out fresh objects re-rendered and re-parsed the whole
// visible transcript per reset. Reuse the prior object when a replacement is
// content-equal, gated behind cheap discriminators so genuinely changed turns
// skip the JSON comparison; returns `current` itself when nothing about the
// agent's slice (content, order, or placement) changed.
export function reconcileReplacedTurns(
  current: Turn[],
  agentId: string | null | undefined,
  replacement: Turn[],
): Turn[] {
  const priorById = new Map<string, Turn>();
  for (const turn of current) {
    if (turn.agentId === agentId) {
      priorById.set(turn.id, turn);
    }
  }
  const reconciled = replacement.map((turn) => {
    const prior = priorById.get(turn.id);
    if (
      prior &&
      prior.sourceIndex === turn.sourceIndex &&
      prior.status === turn.status &&
      prior.statusReason === turn.statusReason &&
      prior.contextStatus === turn.contextStatus &&
      prior.blocks.length === turn.blocks.length &&
      JSON.stringify(prior) === JSON.stringify(turn)
    ) {
      return prior;
    }
    return turn;
  });
  const next = [
    ...current.filter((turn) => turn.agentId !== agentId),
    ...reconciled,
  ];
  if (next.length === current.length && next.every((turn, index) => turn === current[index])) {
    return current;
  }
  return next;
}

// Preserves object identity across thread-graph refetches. A refetch
// re-deserializes every graph into fresh objects even when nothing changed,
// but downstream memoization (the per-agent turn-info cache in App) keys on
// graph identity to avoid rebuilding branch turn lists — and, transitively,
// re-parsing the visible transcript's markdown. Content equality falls back to
// a JSON comparison, gated behind cheap discriminators so clearly-changed
// graphs never pay for it. Returns the previous array itself when every graph
// (and their order) is unchanged, so the state update is a no-op.
export function reconcileThreadGraphs(
  previous: ThreadGraph[],
  next: ThreadGraph[],
): ThreadGraph[] {
  const previousById = new Map(previous.map((graph) => [graph.threadId, graph]));
  const reconciled = next.map((graph) => reconcileThreadGraph(previousById.get(graph.threadId), graph));
  if (
    reconciled.length === previous.length &&
    reconciled.every((graph, index) => graph === previous[index])
  ) {
    return previous;
  }
  return reconciled;
}

function reconcileThreadGraph(prior: ThreadGraph | undefined, graph: ThreadGraph): ThreadGraph {
  if (
    prior &&
    prior.focusedBranchId === graph.focusedBranchId &&
    prior.nextCreatedOrder === graph.nextCreatedOrder &&
    JSON.stringify(prior) === JSON.stringify(graph)
  ) {
    return prior;
  }
  return graph;
}

// Merges targeted single-thread refetches into the graph list without touching
// the other graphs — the streaming path refreshes only the thread whose agent
// produced turn events, so unrelated graphs keep their identity (and their
// downstream memoized timelines) untouched by construction. Content-identical
// updates keep the prior object; a brand-new thread is appended. Returns
// `previous` itself when nothing changed.
export function upsertThreadGraphs(
  previous: ThreadGraph[],
  updates: ThreadGraph[],
): ThreadGraph[] {
  let changed = false;
  let next = previous;
  for (const update of updates) {
    const index = next.findIndex((graph) => graph.threadId === update.threadId);
    if (index === -1) {
      next = [...next, update];
      changed = true;
      continue;
    }
    const reconciled = reconcileThreadGraph(next[index], update);
    if (reconciled !== next[index]) {
      const copy = next.slice();
      copy[index] = reconciled;
      next = copy;
      changed = true;
    }
  }
  return changed ? next : previous;
}

// On macOS the find shortcut is Cmd-F; on other platforms it is Ctrl-F. (Ctrl-F
// is readline's forward-char, so on the Mac we leave it for the terminal.)
export const IS_MAC =
  typeof navigator !== "undefined" && /Mac/i.test(navigator.platform || navigator.userAgent);

export function isEditableTarget(target: EventTarget | null) {
  if (!(target instanceof HTMLElement)) {
    return false;
  }
  if (target.isContentEditable) {
    return true;
  }
  const tag = target.tagName;
  return tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT";
}

// Container class rendered by TerminalPane and matched by isTerminalTarget.
// Shared so the renderer and the key-routing guards cannot drift apart the
// way the old ".terminal-mount" selector did after the web terminal's DOM
// was replaced by native panes.
export const TERMINAL_PANE_CLASS = "terminal-pane";

// True when a DOM event originated inside a terminal pane's container —
// which for native panes means its web chrome (find bar, confirm dialog),
// since the Ghostty surface itself is an NSView and never dispatches DOM
// keydowns. Chords like ctrl-W and ⌘K stay with the terminal there.
export function isTerminalTarget(target: EventTarget | null) {
  return target instanceof HTMLElement && target.closest(`.${TERMINAL_PANE_CLASS}`) !== null;
}

export function measureTerminalCellSize(fontFamily: string, fontSize: number) {
  // Only the default font + size is cached (the common case); other choices
  // measure fresh so a pane created with them gets a close initial grid pre-fit.
  const isDefault = fontFamily === DEFAULT_FONT_STACK && fontSize === TERMINAL_FONT_SIZE;
  if (isDefault && measuredTerminalCellSize && isTerminalFontLoaded()) {
    return measuredTerminalCellSize;
  }

  const probe = document.createElement("span");
  probe.textContent = "mmmmmmmmmm";
  probe.style.position = "absolute";
  probe.style.visibility = "hidden";
  probe.style.whiteSpace = "pre";
  probe.style.fontFamily = fontFamily;
  probe.style.fontSize = `${fontSize}px`;
  document.body.appendChild(probe);

  const rect = probe.getBoundingClientRect();
  probe.remove();

  const cellSize = {
    width: rect.width > 0 ? rect.width / 10 : 8,
    height: rect.height > 0 ? rect.height : 16,
  };
  if (isDefault && isTerminalFontLoaded()) {
    measuredTerminalCellSize = cellSize;
  }
  return cellSize;
}

export function statusLabel(status: PaneInfo["status"]) {
  switch (status) {
    case "running":
      return "Running";
    case "starting":
      return "Starting";
    case "exited":
      return "Exited";
    case "killed":
      return "Killed";
    case "failed":
      return "Failed";
  }
}

export function agentStatusLabel(status: AgentInfo["status"]) {
  switch (status) {
    case "starting":
      return "Starting";
    case "running":
      return "Running";
    case "awaitingInput":
      return "Awaiting input";
    case "awaitingPermission":
      return "Requesting approval";
    case "done":
      return "Done";
    case "idle":
      return null;
    case "failed":
      return "Failed";
  }
}

export type AgentStatusTone = "active" | "pending" | "attention" | "done" | "error" | "idle";

// Maps an agent status onto the status-dot tones used by the pane detail popover.
export function agentStatusTone(status: AgentInfo["status"]): AgentStatusTone {
  switch (status) {
    case "running":
      return "active";
    case "starting":
      return "pending";
    case "awaitingInput":
    case "awaitingPermission":
      return "attention";
    case "done":
      return "done";
    case "failed":
      return "error";
    case "idle":
    default:
      return "idle";
  }
}

export function transcriptHookEvent(event: QmuxEvent): TranscriptHookEvent | null {
  const hookEvent = event.payload.hookEvent;
  if (!event.agentId || typeof hookEvent !== "string") {
    return null;
  }

  return {
    type: event.type,
    paneId: event.paneId ?? null,
    agentId: event.agentId,
    hookEvent,
    payload: event.payload.payload ?? null,
    timestamp: event.timestamp,
  };
}

export function formatTranscriptCopyJson(
  input: Omit<TranscriptCopyPayload, "version" | "exportedAt">,
) {
  const payload: TranscriptCopyPayload = {
    version: TRANSCRIPT_COPY_VERSION,
    exportedAt: new Date().toISOString(),
    ...input,
  };
  return JSON.stringify(payload, null, 2);
}
