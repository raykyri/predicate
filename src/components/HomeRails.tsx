import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";
import { Check, Ellipsis } from "lucide-react";
import { placePanePopover, type AgentStatusTone } from "../lib/appHelpers";
import { growComposerTextarea } from "../lib/composerTextarea";
import { formatRelativeTime } from "../lib/transcriptSessions";
import { useConfirm } from "../hooks/useConfirm";
import type { GlobalDraft } from "../types";
import {
  QueuedTurnCard,
  renderQueuedTurnText,
  waitFooterTitle,
  type QueuedTurnCardTone,
} from "./QueuedTurnCard";

export interface HomeRailQueuedTurn {
  /** The turn's stable id — the authoritative identity for drags and every
   * mutation, since duplicate-text turns are otherwise indistinguishable. */
  id: string;
  /** Display text (instruction blocks stripped). */
  text: string;
  /** The turn's stored text, verbatim — what the queue commands' expectedData
   * checks compare against. Drags must send this, never the stripped text. */
  rawText: string;
  pauseAfter: boolean;
  waitForAgentId?: string | null;
  waitForLabel?: string | null;
  deliveryLabel?: string | null;
}

export interface HomeRailPastTurn {
  id: string;
  text: string;
  /** Millis when the exchange settled (its last record before the next
   * prompt); null when the transcript carries no time data. */
  settledAt: number | null;
}

export interface HomeRailWorkstream {
  agentId: string;
  paneId: string;
  /** The pane's root terminal-scope sidebar group — the home workspace tab
   * this workstream files under (nested child groups fold into their root). */
  rootGroupId: string;
  title: string;
  statusTone: AgentStatusTone;
  statusClass: string;
  waitingOnPane: boolean;
  /** The queue stopped after a pause-after turn; mirrors the composer's
   * "Queue Paused" state so Home doesn't show a silently stuck queue. */
  paused: boolean;
  latestUserTurn: string | null;
  /** When the latest prompt was sent — feeds the current card's elapsed time. */
  currentStartedAt: number | null;
  /** When the transcript last moved — the settled receipt for done agents. */
  currentSettledAt: number | null;
  pastTurns: HomeRailPastTurn[];
  hasEarlierPastTurns: boolean;
  loadingEarlierPastTurns: boolean;
  queuedTurns: HomeRailQueuedTurn[];
}

/** A rail's scroll state, kept by the app so it survives Home unmounting.
 * `stuck` means pinned to the bottom: new cards keep the rail at "now". */
export interface HomeRailScrollPosition {
  top: number;
  stuck: boolean;
}

interface HomeRailsProps {
  workstreams: HomeRailWorkstream[];
  /** Application-global drafts; when enabled, the rail shows on every workspace tab. */
  drafts: GlobalDraft[];
  draftsVisible: boolean;
  /** Reveal the Drafts rail when its app shortcut is used while hidden. */
  onShowDrafts: () => void;
  onActivatePane: (paneId: string) => void;
  onLoadEarlierPastTurns: (agentId: string) => void;
  onReorderQueuedTurn: (
    agentId: string,
    fromIndex: number,
    toIndex: number,
    text: string,
    expectedId?: string,
  ) => void;
  /** Cross-rail drop; the backend appends to the target agent's queue. */
  onMoveQueuedTurn: (
    fromAgentId: string,
    toAgentId: string,
    index: number,
    text: string,
    expectedId?: string,
  ) => void;
  onQueueTurn: (agentId: string, text: string) => Promise<boolean>;
  /** Remove a queued turn (the … menu's Remove and edit-recall). Resolves true
   * when the backend dropped it, so an edit only pulls text in on success. */
  onRemoveQueuedTurn: (
    agentId: string,
    index: number,
    rawText: string,
    expectedId?: string,
  ) => Promise<boolean>;
  /** Clear a paused queue (the right pane's Unpause button, as a card menu item). */
  onUnpauseAgent: (agentId: string) => Promise<void>;
  /** Toggle a queued turn's pause-after-send flag (the composer menu's
   * "Pause after top queued item", available per card here). */
  onSetQueuedTurnPause: (
    agentId: string,
    index: number,
    pauseAfter: boolean,
    rawText: string,
    expectedId?: string,
  ) => Promise<void>;
  /** Push the agent's top queued turn immediately (the composer menu's
   * "Send top queued item now!"); shown on the first queued card only. */
  onSendNextQueuedTurn: (agentId: string) => Promise<void>;
  onCreateDraft: (text: string) => Promise<boolean>;
  onDeleteDraft: (draftId: string) => Promise<void>;
  /** Drop a draft on an agent's rail: atomic claim + send-or-queue. */
  onAssignDraft: (draftId: string, agentId: string) => void;
  readRailScroll: (agentId: string) => HomeRailScrollPosition | null;
  saveRailScroll: (agentId: string, position: HomeRailScrollPosition) => void;
  /** In-progress composer text, keyed by rail id (agentId or DRAFTS_RAIL_ID).
   * Seeded from and mirrored to an app-held store so tab switches and
   * interface reloads preserve it — kept out of app state so keystrokes only
   * re-render this component. */
  readComposerDrafts: () => Record<string, string>;
  saveComposerDrafts: (drafts: Record<string, string>) => void;
  /** Late backend draft restore (sessionStorage came up empty after a reload),
   * merged under anything typed since; null until it lands. */
  restoredComposerDrafts: Record<string, string> | null;
}

interface LinkPath {
  key: string;
  d: string;
}

type RailPointerDrag = {
  pointerId: number;
  // "queued" drags a queued turn between/within agent rails; "draft" drags a
  // draft out of the Drafts rail onto an agent rail (assign).
  kind: "queued" | "draft";
  // Queued: the owning agent. Draft: the DRAFTS_RAIL_ID sentinel.
  agentId: string;
  from: number;
  // Queued: the turn's stored (raw) text, sent as the backend's expectedData.
  // Draft: the draft id.
  text: string;
  // Queued only: the grabbed turn's stable id, so the drop re-derives its index
  // by identity and never mistakes a duplicate-text turn for the grabbed one.
  id?: string;
  startX: number;
  startY: number;
  active: boolean;
};

const RAIL_DRAG_START_THRESHOLD = 4;

/** Sentinel rail id for the Drafts column (also its scroll-store key). */
const DRAFTS_RAIL_ID = "__drafts__";

/** Consumed drafts linger as grayed history this long, then prune from view. */
const CONSUMED_DRAFT_TTL_MS = 7 * 24 * 60 * 60 * 1000;

const STATUS_TONE_LABELS: Record<AgentStatusTone, string> = {
  active: "running",
  pending: "starting",
  attention: "awaiting input",
  done: "done",
  error: "failed",
  idle: "idle",
};

function statusDotClass(workstream: HomeRailWorkstream) {
  return [
    "pane-tab-dot",
    `status-${workstream.statusTone}`,
    workstream.statusClass,
    workstream.waitingOnPane ? "is-waiting-on-pane" : "",
  ]
    .filter(Boolean)
    .join(" ");
}

function statusLabel(workstream: HomeRailWorkstream) {
  return workstream.waitingOnPane
    ? "waiting on another pane"
    : STATUS_TONE_LABELS[workstream.statusTone];
}

function currentCardTone(tone: AgentStatusTone): QueuedTurnCardTone {
  switch (tone) {
    case "attention":
      return "attention";
    case "error":
      return "error";
    case "done":
    case "idle":
      return "done";
    default:
      return "active";
  }
}

function queuedCardKey(agentId: string, index: number) {
  return `${agentId}:${index}`;
}

function formatElapsedShort(ms: number): string | null {
  const minutes = Math.floor(ms / 60_000);
  if (minutes < 1) {
    return null;
  }
  if (minutes < 60) {
    return `${minutes}m`;
  }
  const hours = Math.floor(minutes / 60);
  const rest = minutes % 60;
  return rest > 0 && hours < 10 ? `${hours}h ${rest}m` : `${hours}h`;
}

// Links route orthogonally along the gutter between rails: a short horizontal
// exit, a rounded corner, a vertical run hugging the source column's edge, and
// a horizontal approach ending in the arrowhead at the target card's edge — so
// a link never crosses its own columns' card content. Coordinates are measured
// against the scrollable content box (.home-rails-inner), not the scroll
// viewport, so links stay glued to their cards while panning.
export function railLinkPath(
  fromRect: DOMRect,
  toRect: DOMRect,
  baseRect: DOMRect,
  fromSide: "left" | "right",
  toSide: "left" | "right",
) {
  const point = (rect: DOMRect, side: "left" | "right") => ({
    x: (side === "left" ? rect.left - 2 : rect.right + 2) - baseRect.left,
    y: rect.top + rect.height / 2 - baseRect.top,
  });
  const a = point(fromRect, fromSide);
  const b = point(toRect, toSide);
  if (Math.abs(b.y - a.y) < 1) {
    return `M ${a.x} ${a.y} L ${b.x} ${b.y}`;
  }
  const gx =
    fromSide === toSide ? Math.max(a.x, b.x) + 9 : a.x + (fromSide === "right" ? 5 : -5);
  const dirY = b.y >= a.y ? 1 : -1;
  const rMax = Math.abs(b.y - a.y) / 2;
  const r1 = Math.min(6, rMax, Math.abs(gx - a.x));
  // Keep the corner radius short of the target edge so the path always ends
  // with a real horizontal segment — a zero-length ending flips the marker.
  const r2 = Math.min(6, rMax, Math.abs(b.x - gx) - 2);
  const hA = gx >= a.x ? 1 : -1;
  const hB = b.x >= gx ? 1 : -1;
  return [
    `M ${a.x} ${a.y}`,
    `H ${gx - hA * r1}`,
    `Q ${gx} ${a.y} ${gx} ${a.y + dirY * r1}`,
    `V ${b.y - dirY * r2}`,
    `Q ${gx} ${b.y} ${gx + hB * r2} ${b.y}`,
    `H ${b.x}`,
  ].join(" ");
}

const RAIL_CARD_MENU_WIDTH = 132;

interface RailCardMenuItem {
  label: string;
  action: () => void;
  danger?: boolean;
  /** Grayed while a queue mutation is in flight (the composer's `submitting`). */
  disabled?: boolean;
}

/** The "…" menu on a draft or queued card — the home columns are too narrow for
 *  inline Edit/× buttons, so those actions live in a small portaled menu (the
 *  same shape as the prompt-library row menu). */
function RailCardMenu({ items }: { items: RailCardMenuItem[] }) {
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const popoverRef = useRef<HTMLDivElement | null>(null);
  const [pos, setPos] = useState<{
    left: number;
    top: number;
    maxHeight: number;
    maxWidth: number;
  } | null>(null);

  const position = useCallback(() => {
    const trigger = triggerRef.current;
    const popover = popoverRef.current;
    if (!trigger || !popover) {
      return;
    }
    const { height } = popover.getBoundingClientRect();
    // No paneRect: the home stage has no right pane, so clamp to the viewport.
    setPos(
      placePanePopover({
        triggerRect: trigger.getBoundingClientRect(),
        popoverSize: { width: RAIL_CARD_MENU_WIDTH, height },
        align: "end",
        prefer: "below",
      }),
    );
  }, []);

  useEffect(() => {
    if (!open) {
      return;
    }
    const handlePointerDown = (event: MouseEvent) => {
      const target = event.target as Node;
      if (!triggerRef.current?.contains(target) && !popoverRef.current?.contains(target)) {
        setOpen(false);
      }
    };
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handlePointerDown);
    window.addEventListener("keydown", handleKeyDown, true);
    return () => {
      document.removeEventListener("mousedown", handlePointerDown);
      window.removeEventListener("keydown", handleKeyDown, true);
    };
  }, [open]);

  useLayoutEffect(() => {
    if (!open) {
      setPos(null);
      return;
    }
    position();
    // The columns pan and scroll under the menu; follow the trigger or close.
    const onReflow = () => position();
    window.addEventListener("resize", onReflow);
    window.addEventListener("scroll", onReflow, true);
    return () => {
      window.removeEventListener("resize", onReflow);
      window.removeEventListener("scroll", onReflow, true);
    };
  }, [open, position]);

  return (
    <>
      <button
        ref={triggerRef}
        type="button"
        className={`control-button home-rail-card-menu-trigger${open ? " is-open" : ""}`}
        title="More actions"
        aria-label="More actions"
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={(event) => {
          event.stopPropagation();
          setOpen((current) => !current);
        }}
      >
        <Ellipsis size={14} aria-hidden="true" />
      </button>
      {open
        ? createPortal(
            <div
              ref={popoverRef}
              className="popover-surface popover-surface--context home-rail-card-menu-popover"
              role="menu"
              aria-label="Card actions"
              style={
                pos
                  ? {
                      left: pos.left,
                      top: pos.top,
                      maxHeight: pos.maxHeight,
                      width: Math.min(RAIL_CARD_MENU_WIDTH, pos.maxWidth),
                      maxWidth: pos.maxWidth,
                    }
                  : { left: -9999, top: -9999 }
              }
            >
              {items.map((entry) => (
                <button
                  key={entry.label}
                  type="button"
                  role="menuitem"
                  className={`menu-item${entry.danger ? " is-danger" : ""}`}
                  disabled={entry.disabled}
                  onClick={(event) => {
                    event.stopPropagation();
                    setOpen(false);
                    entry.action();
                  }}
                >
                  {entry.label}
                </button>
              ))}
            </div>,
            document.body,
          )
        : null}
    </>
  );
}

interface RailComposerProps {
  railId: string;
  value: string;
  placeholder: string;
  ariaLabel: string;
  onChange: (value: string) => void;
  onSubmit: () => void;
  /** Up-arrow on an empty composer recalls the last item to edit, shell-style. */
  onEditLast: () => void;
  registerRef: (railId: string, element: HTMLTextAreaElement | null) => void;
}

/** Per-rail ghost composer: an autogrowing textarea so a multi-line draft or
 *  follow-up expands in place (yielding the scroller above it) instead of
 *  scrolling sideways inside a one-line input. Enter sends; Shift+Enter adds a
 *  line. */
function RailComposer({
  railId,
  value,
  placeholder,
  ariaLabel,
  onChange,
  onSubmit,
  onEditLast,
  registerRef,
}: RailComposerProps) {
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);
  // Fit the box to its content on mount (restored text) and whenever the value
  // changes out from under the field (a send clears it, an edit refills it).
  useLayoutEffect(() => {
    if (textareaRef.current) {
      growComposerTextarea(textareaRef.current);
    }
  }, [value]);
  return (
    <form
      className="home-rail-composer"
      onSubmit={(event) => {
        event.preventDefault();
        onSubmit();
      }}
    >
      <textarea
        ref={(element) => {
          textareaRef.current = element;
          registerRef(railId, element);
        }}
        rows={1}
        value={value}
        placeholder={placeholder}
        aria-label={ariaLabel}
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={(event) => {
          if (
            event.key === "Enter" &&
            !event.repeat &&
            !event.shiftKey &&
            !event.metaKey &&
            !event.ctrlKey &&
            !event.altKey &&
            !event.nativeEvent.isComposing
          ) {
            event.preventDefault();
            onSubmit();
            return;
          }
          // Empty field + Up: pull the last item back in to edit (like recalling
          // the previous shell line), matching the right pane's queue.
          if (
            event.key === "ArrowUp" &&
            !event.repeat &&
            value.length === 0 &&
            !event.metaKey &&
            !event.ctrlKey &&
            !event.altKey
          ) {
            event.preventDefault();
            onEditLast();
          }
        }}
      />
    </form>
  );
}

export default function HomeRails({
  workstreams,
  drafts,
  draftsVisible,
  onShowDrafts,
  onActivatePane,
  onLoadEarlierPastTurns,
  onReorderQueuedTurn,
  onMoveQueuedTurn,
  onQueueTurn,
  onRemoveQueuedTurn,
  onUnpauseAgent,
  onSetQueuedTurnPause,
  onSendNextQueuedTurn,
  onCreateDraft,
  onDeleteDraft,
  onAssignDraft,
  readRailScroll,
  saveRailScroll,
  readComposerDrafts,
  saveComposerDrafts,
  restoredComposerDrafts,
}: HomeRailsProps) {
  const wrapRef = useRef<HTMLDivElement | null>(null);
  const innerRef = useRef<HTMLDivElement | null>(null);
  const scrollerRefs = useRef(new Map<string, HTMLDivElement>());
  const currentCardRefs = useRef(new Map<string, HTMLElement>());
  const queuedCardRefs = useRef(new Map<string, HTMLElement>());
  // Rails whose scroll position has been placed (restored or pinned to the
  // bottom) — done once per rail per mount, before first paint.
  const placedScrollAgentIds = useRef(new Set<string>());
  const dragRef = useRef<RailPointerDrag | null>(null);
  // One queue mutation at a time, like the composer's `submitting`: menu items
  // gray out and repeat requests are dropped instead of racing the backend.
  const mutatingRef = useRef(false);
  const [mutating, setMutating] = useState(false);
  async function runMutation<T>(action: () => Promise<T>): Promise<T | undefined> {
    if (mutatingRef.current) {
      return undefined;
    }
    mutatingRef.current = true;
    setMutating(true);
    try {
      return await action();
    } finally {
      mutatingRef.current = false;
      setMutating(false);
    }
  }
  const [dragging, setDragging] = useState<{ agentId: string; index: number } | null>(null);
  // Same-rail target gap; cross-rail drops highlight the whole rail instead
  // (the backend appends to the target queue, so there is no gap to point at).
  // Both are mirrored in refs: the pointerup handler must read the drop target
  // set by the very last pointermove, which may not have committed yet.
  const [dropGap, setDropGapState] = useState<{ agentId: string; index: number } | null>(null);
  const [dropRailAgentId, setDropRailAgentIdState] = useState<string | null>(null);
  const dropGapRef = useRef<{ agentId: string; index: number } | null>(null);
  const dropRailAgentIdRef = useRef<string | null>(null);
  const setDropGap = (gap: { agentId: string; index: number } | null) => {
    dropGapRef.current = gap;
    setDropGapState(gap);
  };
  const setDropRailAgentId = (agentId: string | null) => {
    dropRailAgentIdRef.current = agentId;
    setDropRailAgentIdState(agentId);
  };
  // Composer textareas by rail id — used to focus the field (⌘D, edit recall).
  const composerRefs = useRef(new Map<string, HTMLTextAreaElement>());
  // Rails with an in-flight submission. onSubmit fires per Enter keydown, and
  // submitRailComposer awaits the backend, so without a synchronous guard rapid
  // Enters (or key-repeat) dispatch the same task more than once.
  const submittingRailsRef = useRef(new Set<string>());
  const registerComposerRef = useCallback(
    (railId: string, element: HTMLTextAreaElement | null) => {
      if (element) {
        composerRefs.current.set(railId, element);
      } else {
        composerRefs.current.delete(railId);
      }
    },
    [],
  );
  // Live composer text lives here so a keystroke re-renders only HomeRails;
  // every change mirrors into the app-held store, which also persists it.
  const [composerDrafts, setComposerDrafts] = useState<Record<string, string>>(() => ({
    ...readComposerDrafts(),
  }));
  useEffect(() => {
    saveComposerDrafts(composerDrafts);
  }, [composerDrafts, saveComposerDrafts]);
  useEffect(() => {
    if (restoredComposerDrafts) {
      setComposerDrafts((current) => ({ ...restoredComposerDrafts, ...current }));
    }
  }, [restoredComposerDrafts]);
  const setComposerDraft = useCallback(
    (railId: string, text: string) => {
      setComposerDrafts((current) => ({ ...current, [railId]: text }));
    },
    [],
  );
  // Recalling an item into a composer that already holds text would clobber it,
  // so guard the swap the same way the right pane's edit does.
  const { confirm, dialog: confirmDialog } = useConfirm();
  const [links, setLinks] = useState<LinkPath[]>([]);
  const [svgSize, setSvgSize] = useState({ width: 0, height: 0 });
  // Receipts ("2 hr ago") and the working card's elapsed time drift; a coarse
  // tick keeps them honest without re-rendering on every event.
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 30_000);
    return () => window.clearInterval(timer);
  }, []);

  const setScrollerRef = useCallback((agentId: string, element: HTMLDivElement | null) => {
    if (element) {
      scrollerRefs.current.set(agentId, element);
    } else {
      scrollerRefs.current.delete(agentId);
      placedScrollAgentIds.current.delete(agentId);
    }
  }, []);

  const setCurrentCardRef = useCallback((agentId: string, element: HTMLDivElement | null) => {
    if (element) {
      currentCardRefs.current.set(agentId, element);
    } else {
      currentCardRefs.current.delete(agentId);
    }
  }, []);

  const setQueuedCardRef = useCallback((key: string, element: HTMLDivElement | null) => {
    if (element) {
      queuedCardRefs.current.set(key, element);
    } else {
      queuedCardRefs.current.delete(key);
    }
  }, []);

  // Place each rail's scroll before paint: restore a saved offset, else pin to
  // the bottom (the fold is "now"). On later data changes, stuck rails re-pin
  // so a new card doesn't push the fold out of view.
  useLayoutEffect(() => {
    for (const [agentId, scroller] of scrollerRefs.current) {
      if (!placedScrollAgentIds.current.has(agentId)) {
        placedScrollAgentIds.current.add(agentId);
        const saved = readRailScroll(agentId);
        scroller.scrollTop = saved && !saved.stuck ? saved.top : scroller.scrollHeight;
        continue;
      }
      const saved = readRailScroll(agentId);
      if (!saved || saved.stuck) {
        scroller.scrollTop = scroller.scrollHeight;
      }
    }
    // `drafts` is a dependency so the Drafts rail re-pins when a card is added.
  }, [drafts, readRailScroll, workstreams]);

  const handleRailScroll = (agentId: string, scroller: HTMLDivElement) => {
    const stuck = scroller.scrollTop + scroller.clientHeight >= scroller.scrollHeight - 2;
    saveRailScroll(agentId, { top: scroller.scrollTop, stuck });
  };

  // Wait-link measurement: rAF-batched, re-run on any scroll/resize that can
  // move an anchor (the pattern HomeCascades used).
  useLayoutEffect(() => {
    const inner = innerRef.current;
    const wrap = wrapRef.current;
    if (!inner || !wrap) {
      setLinks([]);
      setSvgSize({ width: 0, height: 0 });
      return;
    }

    let frame = 0;
    const measure = () => {
      const baseRect = inner.getBoundingClientRect();
      const railIndexByAgentId = new Map(
        workstreams.map((workstream, index) => [workstream.agentId, index]),
      );
      const nextLinks: LinkPath[] = [];
      for (const workstream of workstreams) {
        const targetRailIndex = railIndexByAgentId.get(workstream.agentId) ?? 0;
        workstream.queuedTurns.forEach((turn, index) => {
          if (!turn.waitForAgentId) {
            return;
          }
          const sourceRailIndex = railIndexByAgentId.get(turn.waitForAgentId);
          if (sourceRailIndex === undefined) {
            return;
          }
          const from = currentCardRefs.current.get(turn.waitForAgentId);
          const to = queuedCardRefs.current.get(queuedCardKey(workstream.agentId, index));
          if (!from || !to) {
            return;
          }
          const rightward = targetRailIndex >= sourceRailIndex;
          nextLinks.push({
            key: `${turn.waitForAgentId}:${workstream.agentId}:${index}:wait`,
            d: railLinkPath(
              from.getBoundingClientRect(),
              to.getBoundingClientRect(),
              baseRect,
              rightward ? "right" : "left",
              rightward ? "left" : "right",
            ),
          });
        });
      }
      setSvgSize({ width: baseRect.width, height: baseRect.height });
      setLinks(nextLinks);
    };
    const scheduleMeasure = () => {
      window.cancelAnimationFrame(frame);
      frame = window.requestAnimationFrame(measure);
    };

    scheduleMeasure();
    window.addEventListener("resize", scheduleMeasure);
    wrap.addEventListener("scroll", scheduleMeasure, { passive: true });
    const observer = new ResizeObserver(scheduleMeasure);
    observer.observe(wrap);
    observer.observe(inner);
    const scrollers = Array.from(scrollerRefs.current.values());
    for (const scroller of scrollers) {
      scroller.addEventListener("scroll", scheduleMeasure, { passive: true });
      observer.observe(scroller);
    }
    return () => {
      window.cancelAnimationFrame(frame);
      window.removeEventListener("resize", scheduleMeasure);
      wrap.removeEventListener("scroll", scheduleMeasure);
      for (const scroller of scrollers) {
        scroller.removeEventListener("scroll", scheduleMeasure);
      }
      observer.disconnect();
    };
  }, [workstreams]);

  function clearDrag() {
    dragRef.current = null;
    setDragging(null);
    setDropGap(null);
    setDropRailAgentId(null);
  }

  function queuedGapFromPoint(railAgentId: string, clientY: number) {
    const workstream = workstreams.find((candidate) => candidate.agentId === railAgentId);
    if (!workstream) {
      return 0;
    }
    for (let index = 0; index < workstream.queuedTurns.length; index += 1) {
      const card = queuedCardRefs.current.get(queuedCardKey(railAgentId, index));
      if (!card) {
        continue;
      }
      const rect = card.getBoundingClientRect();
      if (clientY < rect.top + rect.height / 2) {
        return index;
      }
    }
    return workstream.queuedTurns.length;
  }

  function handleCardPointerDown(
    event: ReactPointerEvent<HTMLDivElement>,
    kind: "queued" | "draft",
    agentId: string,
    index: number,
    text: string,
    id?: string,
  ) {
    if (event.button !== 0) {
      return;
    }
    if (event.target instanceof Element && event.target.closest(".queued-turn-actions")) {
      return;
    }
    dragRef.current = {
      pointerId: event.pointerId,
      kind,
      agentId,
      from: index,
      text,
      id,
      startX: event.clientX,
      startY: event.clientY,
      active: false,
    };
    // Capture deferred to the first real move so plain clicks still activate
    // the pane (and text selection isn't hijacked) — same as the composer.
  }

  function handleCardPointerMove(event: ReactPointerEvent<HTMLDivElement>) {
    const drag = dragRef.current;
    if (!drag || drag.pointerId !== event.pointerId) {
      return;
    }
    if (!drag.active) {
      if (
        Math.abs(event.clientX - drag.startX) < RAIL_DRAG_START_THRESHOLD &&
        Math.abs(event.clientY - drag.startY) < RAIL_DRAG_START_THRESHOLD
      ) {
        return;
      }
      drag.active = true;
      try {
        event.currentTarget.setPointerCapture(event.pointerId);
      } catch {
        // The pointer may already have been released.
      }
      setDragging({ agentId: drag.agentId, index: drag.from });
    }
    event.preventDefault();
    // The pointer is captured by the source card; hit-test the point to find
    // the rail under it.
    const under = document.elementFromPoint(event.clientX, event.clientY);
    const railAgentId = under
      ?.closest("[data-rail-agent-id]")
      ?.getAttribute("data-rail-agent-id");
    if (!railAgentId || railAgentId === DRAFTS_RAIL_ID) {
      // Queued turns can't return to Drafts; a draft over its own rail is a no-op.
      setDropGap(null);
      setDropRailAgentId(null);
      return;
    }
    if (railAgentId !== drag.agentId) {
      setDropGap(null);
      setDropRailAgentId(railAgentId);
      return;
    }
    setDropRailAgentId(null);
    setDropGap({ agentId: railAgentId, index: queuedGapFromPoint(railAgentId, event.clientY) });
  }

  function handleCardPointerUp(event: ReactPointerEvent<HTMLDivElement>) {
    const drag = dragRef.current;
    if (!drag || drag.pointerId !== event.pointerId) {
      return;
    }
    try {
      event.currentTarget.releasePointerCapture(event.pointerId);
    } catch {
      // The pointer may already have been released by the platform.
    }
    if (!drag.active) {
      dragRef.current = null;
      return;
    }
    event.preventDefault();
    const crossRailAgentId = dropRailAgentIdRef.current;
    const gap = dropGapRef.current;
    clearDrag();
    if (drag.kind === "draft") {
      if (crossRailAgentId) {
        onAssignDraft(drag.text, crossRailAgentId);
      }
      return;
    }
    // The queue can shift under a drag (the agent draining its top turn), so
    // resolve the grabbed card's index at drop time by its stored text.
    const from = currentDragIndex(drag);
    if (from === null) {
      return;
    }
    if (crossRailAgentId && crossRailAgentId !== drag.agentId) {
      onMoveQueuedTurn(drag.agentId, crossRailAgentId, from, drag.text, drag.id);
      return;
    }
    if (!gap || gap.agentId !== drag.agentId) {
      return;
    }
    const to = from < gap.index ? gap.index - 1 : gap.index;
    if (to !== from) {
      onReorderQueuedTurn(drag.agentId, from, to, drag.text, drag.id);
    }
  }

  function currentDragIndex(drag: RailPointerDrag): number | null {
    const workstream = workstreams.find((candidate) => candidate.agentId === drag.agentId);
    if (!workstream) {
      return null;
    }
    // Resolve by the grabbed turn's stable id — the queue can shift mid-drag and
    // a duplicate-text turn must not be mistaken for the one that was grabbed.
    // Null means the grabbed turn is gone, so the drop is dropped.
    if (workstream.queuedTurns[drag.from]?.id === drag.id) {
      return drag.from;
    }
    const index = workstream.queuedTurns.findIndex((turn) => turn.id === drag.id);
    return index === -1 ? null : index;
  }

  function handleCardPointerCancel(event: ReactPointerEvent<HTMLDivElement>) {
    const drag = dragRef.current;
    if (!drag || drag.pointerId !== event.pointerId) {
      return;
    }
    clearDrag();
  }

  // Double-click selects the card's text (the composer queue's behavior); the
  // card body no longer navigates on click, so selection isn't hijacked.
  function handleCardTextDoubleClick(event: ReactMouseEvent<HTMLDivElement>) {
    event.preventDefault();
    event.stopPropagation();
    const range = document.createRange();
    range.selectNodeContents(event.currentTarget);
    const selection = window.getSelection();
    selection?.removeAllRanges();
    selection?.addRange(range);
  }

  async function submitRailComposer(railId: string) {
    if (submittingRailsRef.current.has(railId)) {
      return;
    }
    const snapshot = composerDrafts[railId] ?? "";
    const text = snapshot.trim();
    if (!text) {
      return;
    }
    submittingRailsRef.current.add(railId);
    try {
      const accepted =
        railId === DRAFTS_RAIL_ID ? await onCreateDraft(text) : await onQueueTurn(railId, text);
      if (accepted) {
        // Clear only if the field still holds exactly what was submitted; if the
        // user has started typing the next follow-up, keep their newer text.
        setComposerDrafts((current) =>
          current[railId] === snapshot ? { ...current, [railId]: "" } : current,
        );
      }
    } finally {
      submittingRailsRef.current.delete(railId);
    }
  }

  // Move focus back to a composer and drop the caret at the end, after the
  // controlled value has flushed — so an edit recall lands ready to type.
  function focusComposerAtEnd(railId: string) {
    requestAnimationFrame(() => {
      const textarea = composerRefs.current.get(railId);
      if (!textarea) {
        return;
      }
      textarea.focus();
      const end = textarea.value.length;
      textarea.setSelectionRange(end, end);
    });
  }

  // A recall replaces whatever is half-typed in the target composer; keep it
  // only after an explicit confirm (skipped when the field is empty).
  async function confirmComposerReplace(railId: string) {
    if ((composerDrafts[railId] ?? "").trim().length === 0) {
      return true;
    }
    return confirm({
      message: "Replace what you're typing with this item?",
      confirmLabel: "Replace",
    });
  }

  // Pull a draft into the Drafts composer to edit, removing the card — the same
  // dequeue-to-edit the right pane uses. Re-submitting stores it afresh.
  async function editDraft(draft: GlobalDraft) {
    if (!(await confirmComposerReplace(DRAFTS_RAIL_ID))) {
      return;
    }
    if (mutatingRef.current) {
      return;
    }
    await runMutation(() => onDeleteDraft(draft.id));
    setComposerDraft(DRAFTS_RAIL_ID, draft.text);
    focusComposerAtEnd(DRAFTS_RAIL_ID);
  }

  // Pull a queued turn into its agent's composer to edit; only drop it once the
  // backend confirms the removal, so a failed call can't lose the text.
  async function editQueuedTurn(
    agentId: string,
    index: number,
    rawText: string,
    expectedId: string,
  ) {
    if (!(await confirmComposerReplace(agentId))) {
      return;
    }
    if ((await runMutation(() => onRemoveQueuedTurn(agentId, index, rawText, expectedId))) !== true) {
      return;
    }
    setComposerDraft(agentId, rawText);
    focusComposerAtEnd(agentId);
  }

  // Up-arrow recall targets the most recent open draft / last queued turn.
  function editLastDraft() {
    const open = visibleDrafts.filter((draft) => !draft.consumed);
    const last = open[open.length - 1];
    if (last) {
      void editDraft(last);
    }
  }

  function editLastQueuedTurn(workstream: HomeRailWorkstream) {
    const index = workstream.queuedTurns.length - 1;
    if (index >= 0) {
      const turn = workstream.queuedTurns[index];
      void editQueuedTurn(workstream.agentId, index, turn.rawText, turn.id);
    }
  }

  // ⌘D jumps to the drafts composer while Home is on screen (the component
  // only mounts there). Reveal a hidden Drafts rail first so the shortcut
  // remains useful after adding the visibility filter.
  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key.toLowerCase() === "d" && event.metaKey && !event.ctrlKey && !event.altKey) {
        event.preventDefault();
        if (!draftsVisible) {
          onShowDrafts();
        }
        focusComposerAtEnd(DRAFTS_RAIL_ID);
      }
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [draftsVisible, onShowDrafts]);

  // Consumed drafts linger briefly as history, then fall out of the rail.
  const visibleDrafts = drafts.filter(
    (draft) => !draft.consumed || now - draft.consumed.at < CONSUMED_DRAFT_TTL_MS,
  );
  const openDraftCount = visibleDrafts.filter((draft) => !draft.consumed).length;

  const renderLinkSvg = () => (
    <svg
      className="home-rail-links"
      width={svgSize.width}
      height={svgSize.height}
      viewBox={`0 0 ${svgSize.width} ${svgSize.height}`}
      aria-hidden="true"
    >
      <defs>
        <marker
          id="home-rail-wait-arrow"
          viewBox="0 0 8 8"
          refX="7"
          refY="4"
          markerWidth="4.5"
          markerHeight="4.5"
          orient="auto"
        >
          <path d="M0 0 L8 4 L0 8 z" />
        </marker>
      </defs>
      {links.map((link) => (
        <path
          key={link.key}
          className="home-rail-link home-rail-link--wait"
          d={link.d}
          markerEnd="url(#home-rail-wait-arrow)"
        />
      ))}
    </svg>
  );

  const renderPastReceipt = (turn: HomeRailPastTurn) => (
    <>
      <Check
        size={11}
        strokeWidth={2.5}
        className="queued-turn-receipt-ok"
        aria-hidden="true"
      />
      {turn.settledAt !== null ? ` ${formatRelativeTime(turn.settledAt)}` : null}
    </>
  );

  const renderCurrentReceipt = (workstream: HomeRailWorkstream): ReactNode => {
    switch (workstream.statusTone) {
      case "active":
      case "pending": {
        const elapsed =
          workstream.currentStartedAt !== null
            ? formatElapsedShort(now - workstream.currentStartedAt)
            : null;
        return (
          <>
            <span className="queued-turn-receipt-live">●</span> working
            {elapsed ? ` · ${elapsed}` : ""}
          </>
        );
      }
      case "attention":
        return (
          <>
            <span className="queued-turn-receipt-live">●</span> awaiting input
          </>
        );
      case "error":
        return (
          <>
            <span className="queued-turn-receipt-error">●</span> failed
          </>
        );
      default:
        return (
          <>
            <Check
              size={11}
              strokeWidth={2.5}
              className="queued-turn-receipt-ok"
              aria-hidden="true"
            />
            {" Done"}
            {workstream.currentSettledAt !== null
              ? ` · ${formatRelativeTime(workstream.currentSettledAt)}`
              : null}
          </>
        );
    }
  };

  // Every card on a workstream rail opens its pane from the … menu; the card
  // body itself stays inert so double-click text selection works.
  const goToPaneItem = (workstream: HomeRailWorkstream): RailCardMenuItem => ({
    label: "Go to pane",
    action: () => onActivatePane(workstream.paneId),
  });

  const renderCurrentCard = (workstream: HomeRailWorkstream) => {
    if (workstream.latestUserTurn === null) {
      return (
        <QueuedTurnCard
          ref={(element) => setCurrentCardRef(workstream.agentId, element)}
          variant="current"
          className="is-empty"
          text="Inactive"
          actions={<RailCardMenu items={[goToPaneItem(workstream)]} />}
        />
      );
    }
    return (
      <QueuedTurnCard
        ref={(element) => setCurrentCardRef(workstream.agentId, element)}
        variant="current"
        tone={currentCardTone(workstream.statusTone)}
        text={renderQueuedTurnText(workstream.latestUserTurn, { imageThumbnails: true })}
        receipt={renderCurrentReceipt(workstream)}
        onTextDoubleClick={handleCardTextDoubleClick}
        actions={<RailCardMenu items={[goToPaneItem(workstream)]} />}
      />
    );
  };

  const renderQueuedCard = (
    workstream: HomeRailWorkstream,
    turn: HomeRailQueuedTurn,
    index: number,
  ) => {
    // Anchor refs (wait links) key by index, matching the measure loop; the
    // React key is text-based so card state survives the queue draining above
    // it (duplicated text disambiguates by occurrence — visually identical
    // cards, so an occurrence shift is harmless).
    const refKey = queuedCardKey(workstream.agentId, index);
    // Key by the turn's stable id: an occurrence+text key let a duplicate reuse
    // a drained card's DOM node, which could carry a captured drag onto the
    // wrong turn.
    const cardKey = `${workstream.agentId}:${turn.id}`;
    const isDraggingCard =
      dragging?.agentId === workstream.agentId && dragging.index === index;
    // Suppress the drop rule at the dragged card's own position.
    const activeGap =
      dropGap && dropGap.agentId === workstream.agentId
        ? dropGap.index === (dragging?.index ?? -1) || dropGap.index === (dragging?.index ?? -1) + 1
          ? null
          : dropGap.index
        : null;
    const waitLabel = turn.waitForAgentId ? (
      <>
        {index === 0 ? "Waiting on" : "Wait on"}{" "}
        {`"${waitFooterTitle(turn.waitForLabel ?? "selected terminal")}"`}
      </>
    ) : null;
    return (
      <QueuedTurnCard
        key={cardKey}
        ref={(element) => setQueuedCardRef(refKey, element)}
        text={renderQueuedTurnText(turn.text, { imageThumbnails: true })}
        pauseAfter={turn.pauseAfter}
        deliveryLabel={turn.deliveryLabel ?? null}
        waitLabel={waitLabel}
        className={[
          isDraggingCard ? "is-dragging" : "",
          activeGap === index ? "is-drop-before" : "",
          activeGap === workstream.queuedTurns.length &&
          index === workstream.queuedTurns.length - 1
            ? "is-drop-after"
            : "",
        ]
          .filter(Boolean)
          .join(" ")}
        onPointerDown={(event) =>
          handleCardPointerDown(event, "queued", workstream.agentId, index, turn.rawText, turn.id)
        }
        onPointerMove={handleCardPointerMove}
        onPointerUp={handleCardPointerUp}
        onPointerCancel={handleCardPointerCancel}
        onTextDoubleClick={handleCardTextDoubleClick}
        actions={
          <RailCardMenu
            items={[
              goToPaneItem(workstream),
              ...(workstream.paused
                ? [
                    {
                      label: "Unpause queue",
                      action: () => void runMutation(() => onUnpauseAgent(workstream.agentId)),
                      disabled: mutating,
                    },
                  ]
                : []),
              ...(index === 0
                ? [
                    {
                      label: "Send now",
                      action: () =>
                        void runMutation(() => onSendNextQueuedTurn(workstream.agentId)),
                      disabled: mutating,
                    },
                  ]
                : []),
              {
                label: "Edit",
                action: () =>
                  void editQueuedTurn(workstream.agentId, index, turn.rawText, turn.id),
                disabled: mutating,
              },
              {
                label: turn.pauseAfter ? "Remove pause" : "Pause after send",
                action: () =>
                  void runMutation(() =>
                    onSetQueuedTurnPause(
                      workstream.agentId,
                      index,
                      !turn.pauseAfter,
                      turn.rawText,
                      turn.id,
                    ),
                  ),
                disabled: mutating,
              },
              {
                label: "Remove",
                action: () =>
                  void runMutation(() =>
                    onRemoveQueuedTurn(workstream.agentId, index, turn.rawText, turn.id),
                  ),
                danger: true,
                disabled: mutating,
              },
            ]}
          />
        }
      />
    );
  };

  const renderDraftCard = (draft: GlobalDraft) => {
    if (draft.consumed) {
      return (
        <QueuedTurnCard
          key={draft.id}
          variant="past"
          text={renderQueuedTurnText(draft.text, { imageThumbnails: true })}
          onTextDoubleClick={handleCardTextDoubleClick}
          receipt={
            <>
              <Check
                size={11}
                strokeWidth={2.5}
                className="queued-turn-receipt-ok"
                aria-hidden="true"
              />
              {` ${formatRelativeTime(draft.consumed.at)}`}
            </>
          }
        />
      );
    }
    const isDraggingCard =
      dragging?.agentId === DRAFTS_RAIL_ID &&
      drafts.findIndex((candidate) => candidate.id === draft.id) === dragging.index;
    return (
      <QueuedTurnCard
        key={draft.id}
        text={renderQueuedTurnText(draft.text, { imageThumbnails: true })}
        className={isDraggingCard ? "is-dragging" : ""}
        onPointerDown={(event) =>
          handleCardPointerDown(
            event,
            "draft",
            DRAFTS_RAIL_ID,
            drafts.findIndex((candidate) => candidate.id === draft.id),
            draft.id,
          )
        }
        onPointerMove={handleCardPointerMove}
        onPointerUp={handleCardPointerUp}
        onPointerCancel={handleCardPointerCancel}
        onTextDoubleClick={handleCardTextDoubleClick}
        actions={
          <RailCardMenu
            items={[
              { label: "Edit", action: () => void editDraft(draft), disabled: mutating },
              {
                label: "Delete",
                action: () => void runMutation(() => onDeleteDraft(draft.id)),
                danger: true,
                disabled: mutating,
              },
            ]}
          />
        }
      />
    );
  };

  return (
    <section className="home-rails-section" aria-label="Agent workstreams">
      <div className="home-rails" ref={wrapRef}>
        <div className="home-rails-inner" ref={innerRef}>
          {renderLinkSvg()}
          <div className={`home-rails-columns${dragging ? " is-dragging" : ""}`}>
            {draftsVisible ? (
              <div className="home-rail" data-rail-agent-id={DRAFTS_RAIL_ID}>
                <div className="home-rail-head is-static">
                  <span className="home-rail-title">Drafts</span>
                  {openDraftCount > 0 ? (
                    <span className="home-rail-count">{openDraftCount}</span>
                  ) : null}
                </div>
                <div
                  className="home-rail-scroll"
                  ref={(element) => setScrollerRef(DRAFTS_RAIL_ID, element)}
                  onScroll={(event) => handleRailScroll(DRAFTS_RAIL_ID, event.currentTarget)}
                >
                  {visibleDrafts.map(renderDraftCard)}
                </div>
                <RailComposer
                  railId={DRAFTS_RAIL_ID}
                  value={composerDrafts[DRAFTS_RAIL_ID] ?? ""}
                  placeholder="New draft…"
                  ariaLabel="New draft"
                  onChange={(text) => setComposerDraft(DRAFTS_RAIL_ID, text)}
                  onSubmit={() => void submitRailComposer(DRAFTS_RAIL_ID)}
                  onEditLast={editLastDraft}
                  registerRef={registerComposerRef}
                />
              </div>
            ) : null}
            {workstreams.map((workstream) => (
              <div
                key={workstream.agentId}
                className={`home-rail${
                  dropRailAgentId === workstream.agentId ? " is-drop-target" : ""
                }`}
                data-rail-agent-id={workstream.agentId}
              >
                <button
                  type="button"
                  className="home-rail-head"
                  aria-label={`Open ${workstream.title} — ${statusLabel(workstream)}${
                    workstream.paused ? ", queue paused" : ""
                  }`}
                  onClick={() => onActivatePane(workstream.paneId)}
                >
                  <span className={statusDotClass(workstream)} aria-hidden="true" />
                  <span className="home-rail-title">{workstream.title}</span>
                  {workstream.queuedTurns.length > 0 ? (
                    <span className="home-rail-count">
                      {workstream.queuedTurns.length} queued
                    </span>
                  ) : null}
                  {workstream.paused ? (
                    <span className="home-rail-paused">paused</span>
                  ) : null}
                </button>
                <div
                  className="home-rail-scroll"
                  ref={(element) => setScrollerRef(workstream.agentId, element)}
                  onScroll={(event) =>
                    handleRailScroll(workstream.agentId, event.currentTarget)
                  }
                >
                  {workstream.hasEarlierPastTurns ? (
                    <button
                      type="button"
                      className="home-rail-load-earlier"
                      disabled={workstream.loadingEarlierPastTurns}
                      onClick={() => onLoadEarlierPastTurns(workstream.agentId)}
                    >
                      {workstream.loadingEarlierPastTurns ? "Loading…" : "Load earlier"}
                    </button>
                  ) : null}
                  {workstream.pastTurns.map((turn) => (
                    <QueuedTurnCard
                      key={turn.id}
                      variant="past"
                      text={renderQueuedTurnText(turn.text, { imageThumbnails: true })}
                      receipt={renderPastReceipt(turn)}
                      onTextDoubleClick={handleCardTextDoubleClick}
                      actions={<RailCardMenu items={[goToPaneItem(workstream)]} />}
                    />
                  ))}
                  {renderCurrentCard(workstream)}
                  {workstream.queuedTurns.map((turn, index) =>
                    renderQueuedCard(workstream, turn, index),
                  )}
                </div>
                <RailComposer
                  railId={workstream.agentId}
                  value={composerDrafts[workstream.agentId] ?? ""}
                  placeholder="Queue a follow-up…"
                  ariaLabel={`Queue a follow-up for ${workstream.title}`}
                  onChange={(text) => setComposerDraft(workstream.agentId, text)}
                  onSubmit={() => void submitRailComposer(workstream.agentId)}
                  onEditLast={() => editLastQueuedTurn(workstream)}
                  registerRef={registerComposerRef}
                />
              </div>
            ))}
          </div>
        </div>
      </div>
      {confirmDialog}
    </section>
  );
}
