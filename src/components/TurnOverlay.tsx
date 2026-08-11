import {
  Fragment,
  memo,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import type {
  CSSProperties,
  KeyboardEvent as ReactKeyboardEvent,
  PointerEvent as ReactPointerEvent,
  ReactNode,
} from "react";
import { createPortal } from "react-dom";
import { ArrowDown, ArrowUpRight, Ellipsis, X } from "lucide-react";
import type {
  MessageAnchor,
  ThreadParticipant,
  TranscriptAnnotation,
  Turn,
  TurnBlock,
  TranscriptOption,
} from "../types";
import {
  placePanePopover,
  turnPaneRectFrom,
} from "../lib/appHelpers";
import { claimNativeTerminalPointerForWebDrag } from "../lib/api";
import { writeClipboardText } from "../lib/clipboard";
import { buildHandoffDocument, type HandoffContext } from "../lib/handoff";
import { splitImageMarkers, type ImageMarkerSegment } from "../lib/imageMarkers";
import { requestSaveDraftAsPrompt } from "../lib/promptLibrary";
import { taggedUserInstructionDetails } from "../lib/taggedInstructions";
import { formatEstimatedTokenCount } from "../lib/tokenEstimate";
import {
  captureTranscriptScrollPosition,
  transcriptScrollRestoreTop,
  type TranscriptScrollPosition,
} from "../lib/transcriptScroll";
import {
  assistantGroupTimestamp,
  assistantRunForItemKey,
  annotationsForTimeline,
  assistantRunCopyTextByItemKey,
  buildTimelineItems,
  formatAbsoluteMessageTimestamp,
  formatMessageTimestamp,
  hasToolCall,
  messageItemCopyText,
  messageItemIsTaggedInstruction,
  messageItemText,
  sameMessageItem,
  shouldShowAssistantGroupTimestamp,
} from "../lib/turnTimeline";
import type { MessageBlock, MessageItem } from "../lib/turnTimeline";
import DomSearchBar from "./DomSearchBar";
import TranscriptImage from "./TranscriptImage";
import TranscriptPickerLink from "./TranscriptPickerLink";
import TranscriptMarkdown, {
  TranscriptLinkActionsProvider,
  type LinkActions,
  type OversizedMarkdownPolicy,
} from "./TranscriptMarkdown";
import {
  DisclosureChevron,
  RawTranscriptDisclosure,
  TranscriptActivityItem,
  timelineContextStatusClass,
  timelineStatusClass,
} from "./TranscriptActivity";

export type { LinkActions } from "./TranscriptMarkdown";

// A remembered transcript scroll position. `stuck` retains live-follow intent,
// while `atEnd` separately decides whether a tab restore targets the physical
// tail or the exact saved offset.
export type { TranscriptScrollPosition } from "../lib/transcriptScroll";

interface TurnOverlayProps {
  turns: Turn[];
  annotations?: TranscriptAnnotation[];
  onSaveAnnotation?: (sourceTurnId: string, text: string) => Promise<void>;
  onRemoveAnnotation?: (annotationId: string) => Promise<void>;
  assistantLabel: string;
  // Top bar pinned across the top of the pane (session and browser controls).
  header?: ReactNode;
  input?: ReactNode;
  // Identifies the agent whose transcript is shown; a change means a different
  // transcript loaded, which is when we restore that agent's remembered scroll
  // position (or, absent one, jump the view to the latest turn).
  agentId?: string;
  // Persist/restore the transcript scroll position across agent switches and
  // right-pane unmounts (Home/Research and back). Backed by an App-level store
  // so a remembered position survives the docked pane unmounting entirely.
  getTranscriptScroll?: (agentId: string) => TranscriptScrollPosition | undefined;
  saveTranscriptScroll?: (agentId: string, position: TranscriptScrollPosition) => void;
  // Registers a synchronous capture owned by the active pane. App calls it
  // before changing tabs, while this transcript's DOM geometry is still live.
  registerScrollCapture?: (capture: () => void) => () => void;
  // Identifies the prompt-library listener that can handle "save message as
  // prompt" requests. Unlike agentId, this is absent for detached transcripts.
  savePromptAgentId?: string | null;
  // Short diagnostic shown under the empty-state placeholder when the transcript
  // tail is in an unexpected state (stalled/unreadable file, adapter failure).
  notice?: string | null;
  // Sessions offered by the empty-state "No transcript loaded" picker (same set as
  // the header session menu), the currently-loaded transcript path, and
  // the handler that loads a chosen one (or null to detach).
  transcriptOptions?: TranscriptOption[];
  transcriptPath?: string | null;
  onSelectTranscript?: (path: string | null) => void;
  // When true, the queue/composer area is reserved below the transcript instead of
  // floating over it, with a draggable divider between the two regions.
  queueSplit?: boolean;
  queueSplitHeight?: number;
  onQueueSplitHeightChange?: (height: number) => void;
  // How rendered-markdown links behave (left-click opens internally; right-click
  // opens a chooser).
  linkActions: LinkActions;
  // When true, the agent is actively working, so a "Working…" indicator is pinned
  // to the bottom of the transcript. Driven by live status transitions upstream, so
  // an agent merely restored into a working status does not light it up.
  thinking?: boolean;
  thinkingLabel?: string;
  // Code-mode transcript detail: when false, hide historical tool/thinking
  // activity from the visible transcript while keeping normal messages.
  showActivityDetail?: boolean;
  // When false, the latest user message never pins to the top of the pane —
  // it scrolls with the rest of the transcript (Settings → sticky messages).
  stickyUserMessages?: boolean;
  // When true, show a wall-clock timestamp after each consecutive run of
  // assistant messages (before the next non-assistant message, or at the tail).
  showAssistantTimestamps?: boolean;
  // Reader state is owned by App so docked panes can expand into it and split
  // panes still share one focused response. The key may point anywhere inside
  // the assistant run.
  assistantTurnFocusEnabled?: boolean;
  focusedAssistantTurnKey?: string | null;
  onFocusAssistantTurn?: (itemKey: string | null) => void;
  // When supplied, user-message headers get a small action that asks App to
  // regenerate the tab title from that message.
  onRegenerateTitleFromUserMessage?: (message: string) => void;
  titleGenerationBusy?: boolean;
  // When supplied, user messages offer "Fork from here", branching a new agent
  // from the transcript as it stood just before that message. Absent for
  // adapters that cannot fork at a message. The branch opens with an empty
  // composer: its history ends just before the message, so the point is to take
  // that turn somewhere else rather than re-run it.
  onForkFromMessage?: (anchor: MessageAnchor) => void;
  // Where the work lives, for the "Copy handoff" briefing. Every field is
  // optional: a detached transcript has no agent, and a missing field just
  // drops its line from the copied document.
  handoffContext?: HandoffContext;
  // When true (the overlay showing the active pane), Cmd-F/Ctrl-F opens this
  // pane's find bar — unless focus is in the terminal, which owns its own find.
  searchHotkeyActive?: boolean;
  // App-level motion preference. The OS preference is checked at click time too.
  reduceMotion?: boolean;
}

// Gap kept between the last transcript message and the top of the composer.
const COMPOSER_CLEARANCE = 16;
// Reserve used before the floating composer has completed its first live
// measurement. Matches the transcript's historical CSS bottom padding so a
// fresh mount does not flash its tail underneath the composer for one frame.
const DEFAULT_COMPOSER_RESERVE = 132;

// How close to the bottom (in px) the user must be for new turns or a growing
// composer to keep the transcript pinned to the bottom.
const STICK_TO_BOTTOM_THRESHOLD = 100;
const MIN_TRANSCRIPT_SPLIT_HEIGHT = 96;
const MIN_QUEUE_SPLIT_HEIGHT = 120;
const MIN_QUEUE_AREA_HEIGHT = 56;
const QUEUE_SPLIT_RESIZER_HALF_HEIGHT = 5;
const SPLIT_KEYBOARD_STEP = 16;
const LONG_USER_MESSAGE_COLLAPSE_THRESHOLD = 12_000;
const LONG_USER_MESSAGE_PREVIEW_CHARS = 1_200;
// Assistant messages past this size render as plain preformatted text instead
// of markdown (same limit as research documents): react-markdown re-parses the
// whole message on every streamed append, so a pathological multi-hundred-KB
// message would otherwise stall the stream per event batch. Hoisted so the
// memoized renderer sees a stable prop identity.
// Past maxCharacters the message renders as plain preformatted text instead of
// markdown; maxDisplayCharacters then bounds what actually lands in the DOM, so
// a single multi-megabyte assistant message can't force the privileged webview
// to allocate and lay out the whole node (the research document view uses the
// same 1M display cap). Content beyond the cap is elided with a notice.
const OVERSIZED_ASSISTANT_MARKDOWN: OversizedMarkdownPolicy = {
  maxCharacters: 100_000,
  maxDisplayCharacters: 1_000_000,
  fallbackClassName: "turn-text",
};
// A pinned last-user-message taller than this share of the pane would blanket
// the reply scrolling beneath it, so sticking is disabled for tall bubbles.
const STICKY_USER_MAX_HEIGHT_RATIO = 0.4;
interface QueueSplitDrag {
  pointerId: number;
  startY: number;
  startHeight: number;
}

interface TranscriptSelectionAction {
  sourceTurnId: string;
  text: string;
  left: number;
  top: number;
  offscreen: boolean;
}

function transcriptSelectionPlacement(rect: DOMRect, source: Element) {
  const pane = turnPaneRectFrom(source);
  const leftBound = (pane?.left ?? 0) + 8;
  const rightBound = (pane?.right ?? window.innerWidth) - 72;
  const topBound = (pane?.top ?? 0) + 8;
  const bottomBound = (pane?.bottom ?? window.innerHeight) - 34;
  return {
    left: Math.max(leftBound, Math.min(rect.left, rightBound)),
    top: Math.max(topBound, Math.min(rect.bottom + 4, bottomBound)),
    offscreen:
      rect.bottom < (pane?.top ?? 0) || rect.top > (pane?.bottom ?? window.innerHeight),
  };
}

export function formatTurnsTranscript(turns: Turn[], assistantLabel: string) {
  return turns.map((turn) => formatTurnTranscript(turn, assistantLabel)).join("\n\n");
}

export default function TurnOverlay({
  turns,
  annotations = [],
  onSaveAnnotation,
  onRemoveAnnotation,
  assistantLabel,
  header,
  input,
  agentId,
  getTranscriptScroll,
  saveTranscriptScroll,
  registerScrollCapture,
  savePromptAgentId,
  notice,
  transcriptOptions = [],
  transcriptPath = null,
  onSelectTranscript,
  queueSplit = false,
  queueSplitHeight,
  onQueueSplitHeightChange,
  linkActions,
  thinking = false,
  thinkingLabel = "Working…",
  showActivityDetail = true,
  stickyUserMessages = true,
  showAssistantTimestamps = false,
  assistantTurnFocusEnabled = false,
  focusedAssistantTurnKey = null,
  onFocusAssistantTurn,
  onRegenerateTitleFromUserMessage,
  titleGenerationBusy = false,
  onForkFromMessage,
  handoffContext,
  searchHotkeyActive = false,
  reduceMotion = false,
}: TurnOverlayProps) {
  const readerModeRequested =
    assistantTurnFocusEnabled && focusedAssistantTurnKey !== null;
  const sidebarRef = useRef<HTMLElement | null>(null);
  const inputWrapRef = useRef<HTMLDivElement | null>(null);
  const timelineRef = useRef<HTMLDivElement | null>(null);
  const readerCloseRef = useRef<HTMLButtonElement | null>(null);
  const readerScrollRestoreRef = useRef<TranscriptScrollPosition | null>(null);
  const readerModeWasActiveRef = useRef(false);
  // Zero-height marker after the last timeline row; see the observer effect below.
  const bottomSentinelRef = useRef<HTMLDivElement | null>(null);
  const regenerateTitleFromUserMessageRef = useRef(onRegenerateTitleFromUserMessage);
  regenerateTitleFromUserMessageRef.current = onRegenerateTitleFromUserMessage;
  const titleGenerationEnabled = Boolean(onRegenerateTitleFromUserMessage);
  const handleRegenerateTitleFromUserMessage = useCallback((message: string) => {
    regenerateTitleFromUserMessageRef.current?.(message);
  }, []);

  const queueSplitDragRef = useRef<QueueSplitDrag | null>(null);
  const queueSplitPointerReleaseRef = useRef<(() => void) | null>(null);
  useEffect(() => {
    if (!queueSplit) {
      queueSplitDragRef.current = null;
      queueSplitPointerReleaseRef.current?.();
      queueSplitPointerReleaseRef.current = null;
    }
  }, [queueSplit]);
  useEffect(
    () => () => {
      queueSplitPointerReleaseRef.current?.();
      queueSplitPointerReleaseRef.current = null;
    },
    [],
  );
  // Whether the view is parked near the bottom, so incoming content can keep it
  // pinned there. Starts true (we load at the bottom) and tracks user scrolling.
  const stickToBottomRef = useRef(true);
  // Smooth scrolling emits intermediate scroll events far from the bottom. Keep
  // an explicit jump intent so streamed growth during that animation cannot
  // turn live following back off before the view reaches its moving target.
  const jumpingToLatestRef = useRef(false);
  const [jumpToLatestVisible, setJumpToLatestVisible] = useState(false);
  const [composerHeight, setComposerHeight] = useState(0);
  const [selectionAction, setSelectionAction] = useState<TranscriptSelectionAction | null>(null);
  const [savingAnnotation, setSavingAnnotation] = useState(false);
  const [composerBaseHeight, setComposerBaseHeight] = useState(0);
  // When the shared timeline swaps transcripts, the browser clamps the previous
  // scrollTop onto the new content and fires onScroll *before* layout effects
  // run. That intermediate event would overwrite the incoming agent's saved
  // position (often as stuck-at-bottom) and the restore effect would then read
  // the poisoned value. Snapshot the restore target during render — still
  // before any DOM mutation — and ignore scroll events until restore finishes.
  const prevTranscriptAgentIdRef = useRef<string | undefined>(undefined);
  const restoringScrollRef = useRef(false);
  const pendingRestoreRef = useRef<TranscriptScrollPosition | null | undefined>(undefined);
  if (agentId !== prevTranscriptAgentIdRef.current) {
    prevTranscriptAgentIdRef.current = agentId;
    restoringScrollRef.current = true;
    pendingRestoreRef.current = agentId ? getTranscriptScroll?.(agentId) : undefined;
  }

  const scrollToBottom = () => {
    const timeline = timelineRef.current;
    if (timeline) {
      timeline.scrollTop = timeline.scrollHeight;
    }
  };

  const currentTimelineScrollPosition = (
    timeline: HTMLDivElement,
  ): TranscriptScrollPosition =>
    captureTranscriptScrollPosition(
      timeline,
      jumpingToLatestRef.current,
      STICK_TO_BOTTOM_THRESHOLD,
    );

  const handleTimelineScroll = () => {
    const timeline = timelineRef.current;
    if (!timeline || readerModeRequested || readerModeWasActiveRef.current) {
      return;
    }
    // Clamp/restore churn while switching agents is not user intent — leave
    // stickiness and the store alone until the agentId layout effect settles.
    if (restoringScrollRef.current) {
      return;
    }
    // The stick-to-bottom ref must update synchronously (layout effects read it
    // on the very next commit); the sticky-message geometry can wait for the
    // next frame — see scheduleStickyUserStuckUpdate.
    const position = currentTimelineScrollPosition(timeline);
    const reachedLatest =
      timeline.scrollHeight - timeline.scrollTop - timeline.clientHeight <=
      STICK_TO_BOTTOM_THRESHOLD;
    if (reachedLatest) {
      jumpingToLatestRef.current = false;
    }
    stickToBottomRef.current = position.stuck;
    setJumpToLatestVisible(!position.stuck);
    // Remember where this transcript is parked so switching tabs (or away to
    // Home/Research) and back restores it. This fires for programmatic scrolls
    // too (auto-pin to bottom, restore), so the stored value stays current.
    if (agentId) {
      saveTranscriptScroll?.(agentId, position);
    }
    scheduleStickyUserStuckUpdate();
  };

  // Give App a last-chance synchronous capture for tab switches. The ordinary
  // onScroll path normally has the same value, but WebKit may clamp/reflow an
  // expanded pane during the tab commit without delivering a final usable
  // event. Registering before the switch avoids reading the shared timeline
  // after React has reconciled it with the incoming transcript.
  useLayoutEffect(() => {
    if (!agentId || !registerScrollCapture || readerModeRequested) {
      return;
    }
    return registerScrollCapture(() => {
      const timeline = timelineRef.current;
      if (timeline) {
        saveTranscriptScroll?.(agentId, currentTimelineScrollPosition(timeline));
      }
    });
  }, [agentId, readerModeRequested, registerScrollCapture, saveTranscriptScroll]);

  const jumpToLatest = () => {
    const timeline = timelineRef.current;
    if (!timeline) {
      return;
    }
    // Treat the click as an explicit return to live output immediately. The
    // intent stays armed through intermediate smooth-scroll events so a growing
    // transcript still lands at its current physical end.
    jumpingToLatestRef.current = true;
    stickToBottomRef.current = true;
    setJumpToLatestVisible(false);
    timeline.scrollTo({
      top: timeline.scrollHeight,
      behavior:
        reduceMotion || window.matchMedia?.("(prefers-reduced-motion: reduce)").matches
          ? "auto"
          : "smooth",
    });
  };

  const cancelJumpToLatest = () => {
    if (!jumpingToLatestRef.current) {
      return;
    }
    jumpingToLatestRef.current = false;
    handleTimelineScroll();
  };

  const splitBounds = () => {
    const sidebar = sidebarRef.current;
    const timeline = timelineRef.current;
    const totalHeight = Math.max(0, (sidebar?.clientHeight ?? 0) - (timeline?.offsetTop ?? 0));
    const composerMinHeight = Math.ceil(composerBaseHeight || 0);
    const min = Math.max(MIN_QUEUE_SPLIT_HEIGHT, composerMinHeight + MIN_QUEUE_AREA_HEIGHT);
    const max = Math.max(min, totalHeight - MIN_TRANSCRIPT_SPLIT_HEIGHT);
    return { min, max };
  };

  const clampQueueSplitHeight = (height: number) => {
    const { min, max } = splitBounds();
    return Math.max(min, Math.min(max, Math.round(height)));
  };

  const defaultQueueSplitHeight = () => {
    const sidebar = sidebarRef.current;
    const timeline = timelineRef.current;
    const totalHeight = Math.max(0, (sidebar?.clientHeight ?? 0) - (timeline?.offsetTop ?? 0));
    const preferred = totalHeight > 0 ? Math.round(totalHeight * 0.38) : 320;
    return clampQueueSplitHeight(Math.max(preferred, composerBaseHeight + 180));
  };

  const effectiveQueueSplitHeight = queueSplit
    ? clampQueueSplitHeight(queueSplitHeight ?? defaultQueueSplitHeight())
    : 0;

  const startQueueSplitResize = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (!queueSplit) {
      return;
    }
    event.preventDefault();
    queueSplitPointerReleaseRef.current?.();
    event.currentTarget.setPointerCapture(event.pointerId);
    queueSplitPointerReleaseRef.current = claimNativeTerminalPointerForWebDrag();
    queueSplitDragRef.current = {
      pointerId: event.pointerId,
      startY: event.clientY,
      startHeight: effectiveQueueSplitHeight,
    };
  };

  const moveQueueSplitResize = (event: ReactPointerEvent<HTMLDivElement>) => {
    const drag = queueSplitDragRef.current;
    if (!drag || drag.pointerId !== event.pointerId) {
      return;
    }
    const nextHeight = drag.startHeight + (drag.startY - event.clientY);
    onQueueSplitHeightChange?.(clampQueueSplitHeight(nextHeight));
  };

  const endQueueSplitResize = (event: ReactPointerEvent<HTMLDivElement>) => {
    const drag = queueSplitDragRef.current;
    if (!drag || drag.pointerId !== event.pointerId) {
      return;
    }
    queueSplitDragRef.current = null;
    queueSplitPointerReleaseRef.current?.();
    queueSplitPointerReleaseRef.current = null;
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
  };

  const resizeQueueSplitWithKeyboard = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    if (!queueSplit || (event.key !== "ArrowUp" && event.key !== "ArrowDown")) {
      return;
    }
    event.preventDefault();
    const delta = event.key === "ArrowUp" ? SPLIT_KEYBOARD_STEP : -SPLIT_KEYBOARD_STEP;
    onQueueSplitHeightChange?.(clampQueueSplitHeight(effectiveQueueSplitHeight + delta));
  };

  // When a different transcript loads (or this pane remounts after switching
  // away to Home/Research), restore that agent's remembered scroll position.
  // Absent one — or if the view was physically at the end when it was saved —
  // start at the latest turn. Every other offset is restored verbatim, even if
  // it was inside the wider live-follow threshold. That distinction matters
  // when the latest user card is sticky: it can be pinned while the viewport is
  // near the tail, but tabbing must not turn "near" into "at the end".
  // useLayoutEffect runs before paint, so the pane never flashes at the wrong
  // spot first; the rAF re-assert catches the composer's measured tail-spacer
  // reflow, most visibly on a fresh remount where the composer measures from
  // zero.
  //
  // The restore target is snapshotted during render (pendingRestoreRef), not
  // re-read from the store here: content-swap scroll events can poison the
  // store between commit and this effect (see restoringScrollRef above).
  // Keep the snapshot until the next agentId change so React Strict Mode's
  // effect re-run still sees the unpoisoned value.
  useLayoutEffect(() => {
    const saved = pendingRestoreRef.current;
    jumpingToLatestRef.current = false;

    const finishRestore = () => {
      // Only release the gate if we are still restoring this same agent —
      // a newer agentId change will have re-armed the ref during render.
      if (prevTranscriptAgentIdRef.current !== agentId) {
        return;
      }
      restoringScrollRef.current = false;
      // Publish the settled position now that clamp events are done, so the
      // store matches what the user actually sees (and stickiness is correct).
      handleTimelineScroll();
    };

    if (saved && !saved.atEnd) {
      stickToBottomRef.current = saved.stuck;
      const restore = () => {
        const timeline = timelineRef.current;
        if (timeline) {
          timeline.scrollTop = transcriptScrollRestoreTop(saved, timeline.scrollHeight);
          const distanceFromBottom =
            timeline.scrollHeight - timeline.scrollTop - timeline.clientHeight;
          setJumpToLatestVisible(
            !saved.stuck && distanceFromBottom > STICK_TO_BOTTOM_THRESHOLD,
          );
        }
      };
      restore();
      const frame = requestAnimationFrame(() => {
        restore();
        finishRestore();
      });
      return () => {
        cancelAnimationFrame(frame);
        // If agentId changes again before rAF, the next render already set
        // restoringScrollRef; don't clear it here and reopen the race.
      };
    }
    stickToBottomRef.current = true;
    setJumpToLatestVisible(false);
    scrollToBottom();
    const frame = requestAnimationFrame(() => {
      scrollToBottom();
      finishRestore();
    });
    return () => {
      cancelAnimationFrame(frame);
    };
    // handleTimelineScroll reads live refs/props; agentId is the real trigger.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [agentId, getTranscriptScroll]);

  // Keep pinned to the bottom when new turns arrive or the composer grows (e.g. a
  // queued message), but only while the user is already near the bottom — instant,
  // so reading older turns is never interrupted.
  useLayoutEffect(() => {
    if (!restoringScrollRef.current && stickToBottomRef.current) {
      scrollToBottom();
    }
  }, [turns, composerHeight, effectiveQueueSplitHeight, queueSplit, thinking]);

  // Re-pin outside React commits too. Content grows asynchronously — diagrams
  // render once their chunk loads, images decode late — and the scroller
  // itself resizes with the window and pane dividers; none of that passes
  // through the layout effects above, so a pinned view drifted off the
  // bottom. The sentinel observer fires whenever the timeline's bottom edge
  // crosses the viewport, but re-pins only when scrollHeight actually changed:
  // plain user scrolling flips the sentinel's visibility too and must never
  // be fought. Growth right after a pointer/key interaction inside the
  // timeline is also left alone — that shape is a disclosure being expanded
  // (a details toggle, a long-message reveal), and yanking to the bottom
  // would pull the view off the content the user just opened. Scroller
  // resizes re-pin on stickiness alone — a resize isn't a scroll gesture, so
  // being pinned beforehand is the only signal that matters. Timeline and
  // sentinel are rendered unconditionally, so the empty deps are safe.
  useEffect(() => {
    const timeline = timelineRef.current;
    const sentinel = bottomSentinelRef.current;
    if (!timeline) {
      return;
    }
    let lastScrollHeight = timeline.scrollHeight;
    let suppressAutoPinUntil = 0;
    const noteUserInteraction = () => {
      suppressAutoPinUntil = performance.now() + 250;
    };
    timeline.addEventListener("pointerdown", noteUserInteraction, true);
    timeline.addEventListener("keydown", noteUserInteraction, true);
    const contentObserver =
      sentinel && typeof IntersectionObserver !== "undefined"
        ? new IntersectionObserver(
            () => {
              const grew = timeline.scrollHeight !== lastScrollHeight;
              lastScrollHeight = timeline.scrollHeight;
              if (
                grew &&
                !restoringScrollRef.current &&
                stickToBottomRef.current &&
                performance.now() >= suppressAutoPinUntil
              ) {
                scrollToBottom();
              }
            },
            { root: timeline },
          )
        : null;
    if (sentinel && contentObserver) {
      contentObserver.observe(sentinel);
    }
    const resizeObserver = new ResizeObserver(() => {
      lastScrollHeight = timeline.scrollHeight;
      if (!restoringScrollRef.current && stickToBottomRef.current) {
        scrollToBottom();
      }
    });
    resizeObserver.observe(timeline);
    return () => {
      timeline.removeEventListener("pointerdown", noteUserInteraction, true);
      timeline.removeEventListener("keydown", noteUserInteraction, true);
      contentObserver?.disconnect();
      resizeObserver.disconnect();
    };
  }, []);

  // The composer normally floats over the transcript, so reserve scroll room
  // beneath the last message equal to the live input height. In split mode, also
  // measure the composer-only portion so the divider cannot be dragged down over it.
  useEffect(() => {
    const element = inputWrapRef.current;
    if (!element) {
      setComposerHeight(0);
      setComposerBaseHeight(0);
      return;
    }
    const measure = () => {
      setComposerHeight(element.offsetHeight);
      const composer = element.querySelector(".native-input-composer") as HTMLElement | null;
      const styles = window.getComputedStyle(element);
      const paddingY =
        Number.parseFloat(styles.paddingTop || "0") +
        Number.parseFloat(styles.paddingBottom || "0");
      setComposerBaseHeight((composer?.offsetHeight ?? 0) + paddingY);
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    const composer = element.querySelector(".native-input-composer");
    if (composer) {
      observer.observe(composer);
    }
    return () => observer.disconnect();
  }, [Boolean(input)]);

  const queueSplitHeightRef = useRef(queueSplitHeight);
  queueSplitHeightRef.current = queueSplitHeight;
  const onQueueSplitHeightChangeRef = useRef(onQueueSplitHeightChange);
  onQueueSplitHeightChangeRef.current = onQueueSplitHeightChange;
  useLayoutEffect(() => {
    if (!queueSplit || !onQueueSplitHeightChange) {
      return;
    }
    const clamped = clampQueueSplitHeight(queueSplitHeight ?? defaultQueueSplitHeight());
    if (queueSplitHeight !== clamped) {
      onQueueSplitHeightChange(clamped);
    }
    // The clamp intentionally depends on live measurements read from refs.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [queueSplit, queueSplitHeight, composerBaseHeight, agentId, Boolean(input)]);
  // Re-clamp when the pane itself resizes. Split off from the effect above and
  // reading the live height through a ref, so a divider drag (which changes
  // queueSplitHeight on every pointermove) doesn't tear down and recreate the
  // ResizeObserver once per frame.
  useLayoutEffect(() => {
    if (!queueSplit || !onQueueSplitHeightChange) {
      return;
    }
    const sidebar = sidebarRef.current;
    if (!sidebar) {
      return;
    }
    const observer = new ResizeObserver(() => {
      const current = queueSplitHeightRef.current;
      const next = clampQueueSplitHeight(current ?? defaultQueueSplitHeight());
      if (next !== current) {
        onQueueSplitHeightChangeRef.current?.(next);
      }
    });
    observer.observe(sidebar);
    return () => observer.disconnect();
    // The clamp intentionally depends on live measurements read from refs.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [queueSplit, composerBaseHeight, agentId, Boolean(input)]);

  const timelineItems = useMemo(
    () => buildTimelineItems(turns, showActivityDetail),
    [turns, showActivityDetail],
  );
  const focusedAssistantItems = useMemo(
    () =>
      focusedAssistantTurnKey
        ? assistantRunForItemKey(timelineItems, focusedAssistantTurnKey)
        : [],
    [focusedAssistantTurnKey, timelineItems],
  );
  const readerMode =
    readerModeRequested && focusedAssistantItems.length > 0;
  const displayedTimelineItems = readerMode ? focusedAssistantItems : timelineItems;
  const timelineAnnotations = useMemo(
    () => annotationsForTimeline(timelineItems, annotations),
    [annotations, timelineItems],
  );
  const hasTimelineContent = readerMode
    ? displayedTimelineItems.length > 0
    : timelineItems.length > 0 || annotations.length > 0 || thinking;

  const focusAssistantTurn = useCallback(
    (itemKey: string) => {
      const timeline = timelineRef.current;
      if (timeline) {
        readerScrollRestoreRef.current = currentTimelineScrollPosition(timeline);
      }
      onFocusAssistantTurn?.(itemKey);
    },
    [onFocusAssistantTurn],
  );

  useLayoutEffect(() => {
    if (readerMode) {
      readerModeWasActiveRef.current = true;
      jumpingToLatestRef.current = false;
      setJumpToLatestVisible(false);
      timelineRef.current?.scrollTo({ top: 0 });
      readerCloseRef.current?.focus();
      return;
    }
    if (readerModeWasActiveRef.current) {
      readerModeWasActiveRef.current = false;
      const saved = readerScrollRestoreRef.current;
      readerScrollRestoreRef.current = null;
      const timeline = timelineRef.current;
      if (timeline && saved) {
        stickToBottomRef.current = saved.stuck;
        timeline.scrollTop = transcriptScrollRestoreTop(saved, timeline.scrollHeight);
        handleTimelineScroll();
      }
    }
  }, [focusedAssistantTurnKey, readerMode]);

  useEffect(() => {
    if (readerModeRequested && focusedAssistantItems.length === 0) {
      onFocusAssistantTurn?.(null);
    }
  }, [focusedAssistantItems.length, onFocusAssistantTurn, readerModeRequested]);

  const captureAnnotationSelection = useCallback(() => {
    if (!onSaveAnnotation) {
      setSelectionAction(null);
      return;
    }
    const selection = window.getSelection();
    if (!selection || selection.isCollapsed || selection.rangeCount === 0) {
      setSelectionAction(null);
      return;
    }
    const range = selection.getRangeAt(0);
    const cardFor = (node: Node) =>
      (node instanceof Element ? node : node.parentElement)?.closest<HTMLElement>(
        ".turn-card.role-assistant",
      ) ?? null;
    const sourceFor = (node: Node) =>
      (node instanceof Element ? node : node.parentElement)?.closest<HTMLElement>(
        "[data-annotation-source-turn-id]",
      )?.dataset.annotationSourceTurnId ?? null;
    const startCard = cardFor(range.startContainer);
    const endCard = cardFor(range.endContainer);
    const timeline = timelineRef.current;
    if (!timeline || !startCard || startCard !== endCard || !timeline.contains(startCard)) {
      setSelectionAction(null);
      return;
    }
    const startBlocks = (range.startContainer instanceof Element
      ? range.startContainer
      : range.startContainer.parentElement
    )?.closest(".turn-blocks");
    const endBlocks = (range.endContainer instanceof Element
      ? range.endContainer
      : range.endContainer.parentElement
    )?.closest(".turn-blocks");
    if (!startBlocks || startBlocks !== endBlocks || !startCard.contains(startBlocks)) {
      setSelectionAction(null);
      return;
    }
    const startSourceTurnId = sourceFor(range.startContainer);
    const endSourceTurnId = sourceFor(range.endContainer);
    if (!startSourceTurnId || startSourceTurnId !== endSourceTurnId) {
      setSelectionAction(null);
      return;
    }
    const text = selection.toString().trim();
    if (!text) {
      setSelectionAction(null);
      return;
    }
    setSelectionAction({
      sourceTurnId: startSourceTurnId,
      text,
      ...transcriptSelectionPlacement(range.getBoundingClientRect(), startCard),
    });
  }, [onSaveAnnotation]);

  const applyAnnotationSelection = useCallback(async () => {
    if (!selectionAction || !onSaveAnnotation || savingAnnotation) {
      return;
    }
    setSavingAnnotation(true);
    try {
      await onSaveAnnotation(selectionAction.sourceTurnId, selectionAction.text);
      window.getSelection()?.removeAllRanges();
      setSelectionAction(null);
    } catch {
      // App owns the user-facing error toast; keep the selection/action so the
      // user can adjust or retry it.
    } finally {
      setSavingAnnotation(false);
    }
  }, [onSaveAnnotation, savingAnnotation, selectionAction]);

  useEffect(() => {
    setSelectionAction(null);
  }, [agentId]);

  useEffect(() => {
    if (!selectionAction) {
      return;
    }
    const dismiss = (event: Event) => {
      const target = event.target instanceof Element ? event.target : null;
      if (!target?.closest(".turn-annotation-save-action")) {
        setSelectionAction(null);
      }
    };
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setSelectionAction(null);
      }
    };
    document.addEventListener("mousedown", dismiss);
    document.addEventListener("keydown", handleKeyDown);
    window.addEventListener("resize", dismiss);
    window.addEventListener("scroll", dismiss, true);
    return () => {
      document.removeEventListener("mousedown", dismiss);
      document.removeEventListener("keydown", handleKeyDown);
      window.removeEventListener("resize", dismiss);
      window.removeEventListener("scroll", dismiss, true);
    };
  }, [selectionAction]);
  // WebKit can omit a scrollable flex container's block-end padding from its
  // effective scroll range. Represent the floating composer's clearance as a
  // real, non-shrinking tail item instead, so scrollHeight and the bottom
  // sentinel share the same physical end. Split mode already ends the
  // timeline above its queue and needs no tail reserve.
  const tailReserveHeight =
    !readerMode && !queueSplit && input && hasTimelineContent
      ? composerHeight > 0
        ? composerHeight + COMPOSER_CLEARANCE
        : DEFAULT_COMPOSER_RESERVE
      : 0;
  const timelineStyle: CSSProperties | undefined = !readerMode && queueSplit
    ? { bottom: effectiveQueueSplitHeight, paddingBottom: 10 }
    : tailReserveHeight > 0
      ? { paddingBottom: 0 }
      : undefined;
  const inputStyle: CSSProperties | undefined = !readerMode && queueSplit
    ? { height: effectiveQueueSplitHeight }
    : undefined;
  // Keep the floating affordance in the visible transcript strip: the composer
  // overlays the bottom in the normal layout, while split mode ends the
  // transcript above its separately-sized queue area.
  const jumpToLatestBottom = !readerMode && queueSplit
    ? effectiveQueueSplitHeight + 12
    : input
      ? (composerHeight || DEFAULT_COMPOSER_RESERVE) + 10
      : 12;
  const splitBoundsNow = queueSplit ? splitBounds() : null;
  const assistantRunCopyText = useMemo(
    () => assistantRunCopyTextByItemKey(timelineItems),
    [timelineItems],
  );

  // "Copy handoff" reads live transcript state through a ref so its callback
  // identity never changes: it is passed to every memoized message row, and a
  // fresh function per render would re-render the whole timeline on every
  // streamed event.
  const handoffSourceRef = useRef({ turns, assistantLabel, handoffContext, timelineItems });
  handoffSourceRef.current = { turns, assistantLabel, handoffContext, timelineItems };
  const copyHandoff = useCallback(async (anchorKey: string) => {
    const source = handoffSourceRef.current;
    // Rebuild with activity detail forced on: the file paths a handoff needs
    // are in the tool calls, which the pane itself may be configured to hide.
    // Message keys are stable across that flag, but the detail-on fold can
    // split a merged card, so fall back to the pane's own items if the anchor
    // is missing rather than copying the wrong slice.
    const handoffText =
      buildHandoffDocument({
        items: buildTimelineItems(source.turns, true),
        anchorKey,
        assistantLabel: source.assistantLabel,
        context: source.handoffContext,
      }) ??
      buildHandoffDocument({
        items: source.timelineItems,
        anchorKey,
        assistantLabel: source.assistantLabel,
        context: source.handoffContext,
      });
    if (!handoffText) {
      return false;
    }
    try {
      await writeClipboardText(handoffText);
      return true;
    } catch {
      return false;
    }
  }, []);

  // The last real user message (a bubble — not a tagged instruction, not a
  // dropped turn) is the sticky candidate: it pins to the top of the pane while
  // the turn beneath it scrolls, and releases as the view scrolls back up to
  // its original slot.
  const stickyUserKey = useMemo(() => {
    // A null key disables the whole pipeline downstream: no candidate class on
    // the card, no fit measurement, no stuck tracking.
    if (!stickyUserMessages) {
      return null;
    }
    for (let index = timelineItems.length - 1; index >= 0; index -= 1) {
      const item = timelineItems[index];
      if (
        item.role === "user" &&
        item.blocks.length > 0 &&
        !item.status &&
        !messageItemIsTaggedInstruction(item)
      ) {
        return item.key;
      }
    }
    return null;
  }, [stickyUserMessages, timelineItems]);
  const [stickyUserFits, setStickyUserFits] = useState(false);
  const [stickyUserStuck, setStickyUserStuck] = useState(false);
  const stickyUserEnabled = Boolean(stickyUserKey) && stickyUserFits;

  // Whether the candidate is pinned right now. Drives the stuck-only styling
  // (shadow + backfill mask) so the bubble renders exactly as before whenever
  // it sits in its flow position. Sticky offsets resolve against the scroller's
  // content edge, so the pinned card rests just below the timeline's top
  // padding — the threshold must include it.
  //
  // Scroll events fire this continuously while streaming output is also
  // mutating layout, so the check is coalesced to one run per animation frame
  // and reuses the cached card element (stable for a given stickyUserKey
  // thanks to keyed reconciliation; refreshed by the measurement effect below)
  // instead of re-querying the DOM every event. The top padding is re-read per
  // run — it varies with pane modes (split, headerless-expanded) and font
  // scale that no cache key here observes — but at most once per frame.
  const stickyUserCardRef = useRef<HTMLElement | null>(null);
  const stickyUserStuckFrameRef = useRef<number | null>(null);
  const updateStickyUserStuckRef = useRef(() => {});
  updateStickyUserStuckRef.current = () => {
    const timeline = timelineRef.current;
    const card = stickyUserCardRef.current;
    if (!timeline || !card || !stickyUserEnabled) {
      setStickyUserStuck(false);
      return;
    }
    const paddingTop = Number.parseFloat(window.getComputedStyle(timeline).paddingTop || "0");
    setStickyUserStuck(
      card.getBoundingClientRect().top <=
        timeline.getBoundingClientRect().top + paddingTop + 1,
    );
  };
  const scheduleStickyUserStuckUpdate = () => {
    if (stickyUserStuckFrameRef.current !== null) {
      return;
    }
    stickyUserStuckFrameRef.current = requestAnimationFrame(() => {
      stickyUserStuckFrameRef.current = null;
      updateStickyUserStuckRef.current();
    });
  };
  useEffect(
    () => () => {
      if (stickyUserStuckFrameRef.current !== null) {
        cancelAnimationFrame(stickyUserStuckFrameRef.current);
      }
    },
    [],
  );

  // Sticky is armed only while the candidate bubble is short relative to the
  // visible transcript strip; re-measured when the bubble, the pane, or the
  // floating queue/composer resizes (font scale, queue split, expand, a
  // collapsed long message being expanded, messages queueing up). Unsplit, the
  // queue/composer floats over the timeline's bottom, so subtract it — that
  // also keeps a pinned card from ever reaching the queue, which would paint
  // beneath it. In split mode the timeline already ends above the queue.
  useLayoutEffect(() => {
    const timeline = timelineRef.current;
    const card = timeline?.querySelector<HTMLElement>(".turn-card.is-sticky-user-message");
    stickyUserCardRef.current = card ?? null;
    if (!timeline || !card) {
      setStickyUserFits(false);
      return;
    }
    const inputWrap = queueSplit ? null : inputWrapRef.current;
    const measure = () => {
      const visibleHeight = timeline.clientHeight - (inputWrap?.offsetHeight ?? 0);
      setStickyUserFits(card.offsetHeight <= visibleHeight * STICKY_USER_MAX_HEIGHT_RATIO);
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(timeline);
    observer.observe(card);
    if (inputWrap) {
      observer.observe(inputWrap);
    }
    return () => observer.disconnect();
  }, [stickyUserKey, agentId, queueSplit]);

  // Re-evaluate pinning outside scroll events too: content growing below the
  // bubble or a transcript swap moves it without firing onScroll. Direct call
  // (not the rAF-coalesced schedule): this runs once per commit, and layout
  // effects should settle the state before paint.
  useLayoutEffect(() => {
    updateStickyUserStuckRef.current();
    // The update reads live geometry from refs; these deps are the renders
    // after which that geometry can have changed.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [stickyUserEnabled, timelineItems, composerHeight, effectiveQueueSplitHeight, agentId, thinking]);

  const visibleHeader = readerMode ? null : header;

  return (
    <section
      ref={sidebarRef}
      className={`turn-sidebar${visibleHeader ? " has-header" : ""}${
        !readerMode && queueSplit ? " has-queue-split" : ""
      }${readerMode ? " is-reader-mode" : ""}`}
      role={readerMode ? "dialog" : undefined}
      aria-modal={readerMode || undefined}
      aria-label={readerMode ? "Focused assistant turn" : "Agent turns"}
      onKeyDown={(event) => {
        if (readerMode && event.key === "Tab" && !event.defaultPrevented) {
          const focusable = Array.from(
            event.currentTarget.querySelectorAll<HTMLElement>(
              'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), summary, [tabindex]:not([tabindex="-1"]), [contenteditable="true"]',
            ),
          ).filter((element) => element.getClientRects().length > 0);
          const first = focusable[0];
          const last = focusable[focusable.length - 1];
          if (first && last) {
            const active = document.activeElement;
            if (
              (event.shiftKey && (active === first || !event.currentTarget.contains(active))) ||
              (!event.shiftKey && (active === last || !event.currentTarget.contains(active)))
            ) {
              event.preventDefault();
              (event.shiftKey ? last : first).focus();
            }
          }
          return;
        }
        if (readerMode && event.key === "Escape" && !event.defaultPrevented) {
          event.preventDefault();
          event.stopPropagation();
          onFocusAssistantTurn?.(null);
        }
      }}
    >
      {visibleHeader}
      {readerMode ? (
        <div className="turn-reader-header">
          <span className="turn-reader-title">Assistant response</span>
          <button
            ref={readerCloseRef}
            type="button"
            className="icon-button turn-reader-close-button"
            title="Close reader"
            aria-label="Close assistant response reader"
            onClick={() => onFocusAssistantTurn?.(null)}
          >
            <X size={16} aria-hidden="true" />
          </button>
        </div>
      ) : null}
      <DomSearchBar
        active={searchHotkeyActive}
        placeholder="Find in transcript"
        rootRef={timelineRef}
        hotkeyScopeRef={sidebarRef}
        resetKey={agentId}
      />
      {!readerMode && queueSplit ? (
        <div
          className="turn-queue-resizer"
          role="separator"
          aria-label="Resize queue split"
          aria-orientation="horizontal"
          aria-valuemin={splitBoundsNow?.min ?? 0}
          aria-valuemax={splitBoundsNow?.max ?? 0}
          aria-valuenow={effectiveQueueSplitHeight}
          tabIndex={0}
          style={{ bottom: effectiveQueueSplitHeight - QUEUE_SPLIT_RESIZER_HALF_HEIGHT }}
          onPointerDown={startQueueSplitResize}
          onPointerMove={moveQueueSplitResize}
          onPointerUp={endQueueSplitResize}
          onPointerCancel={endQueueSplitResize}
          onKeyDown={resizeQueueSplitWithKeyboard}
        />
      ) : null}
      <TranscriptLinkActionsProvider actions={linkActions}>
        <div
          ref={timelineRef}
          className={`turn-timeline${displayedTimelineItems.length === 0 && annotations.length === 0 && !thinking ? " is-empty" : ""}${
            stickyUserEnabled && !readerMode ? " has-sticky-user" : ""
          }`}
          style={timelineStyle}
          onScroll={handleTimelineScroll}
          onPointerDownCapture={cancelJumpToLatest}
          onWheelCapture={cancelJumpToLatest}
          onKeyDownCapture={cancelJumpToLatest}
          onMouseUp={captureAnnotationSelection}
          onKeyUp={captureAnnotationSelection}
        >
        {displayedTimelineItems.length === 0 && annotations.length === 0 && !thinking ? (
          <div className="empty-state turn-empty-state">
            <span>No activity yet</span>
            {/* Adapters emit both the bare "Transcript unavailable" and
                detailed variants ("Transcript unavailable: <reason>"), and
                transcript.error relays arbitrary failure text. Show the
                diagnostic whenever it says more than the picker already
                does, and offer the session picker for every
                transcript-unavailable flavor — an exact-string match here
                used to swallow the detailed ones and misleadingly render
                "Send a message to continue" over a broken transcript. */}
            {notice && notice !== "Transcript unavailable" ? (
              <span className="turn-empty-notice">{notice}</span>
            ) : null}
            {notice?.startsWith("Transcript unavailable") && onSelectTranscript ? (
              <TranscriptPickerLink
                options={transcriptOptions}
                activePath={transcriptPath}
                onSelect={onSelectTranscript}
              />
            ) : notice ? null : (
              <span className="turn-empty-notice">Send a message to continue</span>
            )}
          </div>
        ) : (
          displayedTimelineItems.map((item, index) => {
            // A continued agent turn — agent text, then a tool-call group, then
            // more agent text — is still the same speaker, so drop the repeated
            // name on the continuation. (Consecutive agent messages only stay
            // separate when activities sit between them; see buildTimelineItems.)
            const previous = displayedTimelineItems[index - 1];
            const next = displayedTimelineItems[index + 1];
            const showName = !(
              item.role === "assistant" &&
              previous?.role === "assistant" &&
              previous.blocks.length > 0 &&
              hasToolCall(previous)
            );
            // Only the last item of a consecutive assistant run gets a footer
            // timestamp — before the next user/system message, or at the tail.
            const groupTimestamp =
              showAssistantTimestamps &&
              item.role === "assistant" &&
              (next === undefined || next.role !== "assistant")
                ? assistantGroupTimestamp(displayedTimelineItems, index)
                : null;
            const showGroupTimestamp =
              groupTimestamp !== null &&
              shouldShowAssistantGroupTimestamp(groupTimestamp, {
                atTranscriptTail: next === undefined,
                // A focused historical response is its temporary visual tail,
                // but a newer live response elsewhere must not suppress its
                // timestamp as though this were the real working tail.
                working: readerMode ? false : thinking,
              });
            // The candidate keeps its marker class even while sticky is
            // disarmed (the height measurement finds it by this class); the
            // sticky styling only activates under the timeline's
            // has-sticky-user class.
            const stickyClassName =
              item.key === stickyUserKey
                ? ` is-sticky-user-message${stickyUserStuck ? " is-stuck" : ""}`
                : "";
            return (
              <Fragment key={item.key}>
                <MessageTimelineItemView
                  item={item}
                  agentId={agentId}
                  savePromptAgentId={savePromptAgentId}
                  assistantLabel={assistantLabel}
                  showName={showName}
                  assistantCopyText={assistantRunCopyText.get(item.key) ?? null}
                  stickyClassName={stickyClassName}
                  titleGenerationEnabled={titleGenerationEnabled}
                  onRegenerateTitleFromUserMessage={handleRegenerateTitleFromUserMessage}
                  titleGenerationBusy={titleGenerationBusy}
                  // Hidden on the very first rendered message: a fork from there
                  // keeps nothing, which is a new agent rather than a branch.
                  // When the per-agent cap has truncated older turns this is
                  // conservative by one item, which beats offering an action the
                  // backend would refuse.
                  onForkFromMessage={index === 0 ? undefined : onForkFromMessage}
                  onCopyHandoff={copyHandoff}
                />
                {showGroupTimestamp ? (
                  <div className="turn-assistant-timestamp">
                    <time
                      dateTime={new Date(groupTimestamp).toISOString()}
                      title={formatAbsoluteMessageTimestamp(groupTimestamp)}
                    >
                      {formatMessageTimestamp(groupTimestamp)}
                    </time>
                    {assistantTurnFocusEnabled && !readerMode && onFocusAssistantTurn ? (
                      <button
                        type="button"
                        className="turn-assistant-focus-button"
                        title="Read this assistant turn"
                        aria-label="Read this assistant turn in focus mode"
                        onPointerDown={(event) => event.stopPropagation()}
                        onClick={(event) => {
                          event.stopPropagation();
                          focusAssistantTurn(item.key);
                        }}
                      >
                        <ArrowUpRight size={13} aria-hidden="true" />
                      </button>
                    ) : null}
                  </div>
                ) : null}
                {!readerMode &&
                  (timelineAnnotations.byItemKey.get(item.key) ?? []).map((annotation) => (
                    <SavedAnnotationCard
                      key={annotation.id}
                      annotation={annotation}
                      onRemove={onRemoveAnnotation}
                    />
                  ))}
              </Fragment>
            );
          })
        )}
        {!readerMode &&
          timelineAnnotations.orphaned.map((annotation) => (
            <SavedAnnotationCard
              key={annotation.id}
              annotation={annotation}
              sourceUnavailable
              onRemove={onRemoveAnnotation}
            />
          ))}
        {!readerMode && thinking ? (
          <div className="turn-thinking" aria-live="polite">
            <span className="turn-thinking-dot" aria-hidden="true" />
            <span className="turn-thinking-label">{thinkingLabel}</span>
          </div>
        ) : null}
        <div
          className="turn-timeline-tail-space"
          style={tailReserveHeight > 0 ? { height: tailReserveHeight } : undefined}
          aria-hidden="true"
        >
          <div ref={bottomSentinelRef} className="turn-timeline-sentinel" />
        </div>
        </div>
      </TranscriptLinkActionsProvider>
      {!readerMode ? (
        <button
          type="button"
          className={`turn-jump-to-latest${jumpToLatestVisible ? " is-visible" : ""}`}
          style={{ bottom: jumpToLatestBottom }}
          aria-hidden={!jumpToLatestVisible}
          tabIndex={jumpToLatestVisible ? 0 : -1}
          onClick={(event) => {
            // Once the pill slips away at the bottom, leaving focus on it would
            // strand keyboard users on an invisible control.
            event.currentTarget.blur();
            jumpToLatest();
          }}
        >
          <ArrowDown size={12} aria-hidden="true" />
          Jump to latest
        </button>
      ) : null}
      {!readerMode && input ? (
        <div
          className={`turn-sidebar-input${queueSplit ? " is-split" : ""}`}
          ref={inputWrapRef}
          style={inputStyle}
        >
          <div className="turn-sidebar-input-rail">{input}</div>
        </div>
      ) : null}
      {selectionAction
        ? createPortal(
            <button
              type="button"
              className="control-button turn-annotation-save-action"
              style={{
                left: selectionAction.left,
                top: selectionAction.top,
                visibility: selectionAction.offscreen ? "hidden" : undefined,
              }}
              disabled={savingAnnotation}
              onMouseDown={(event) => event.preventDefault()}
              onClick={() => void applyAnnotationSelection()}
            >
              {savingAnnotation ? "Saving…" : "Save"}
            </button>,
            document.body,
          )
        : null}
    </section>
  );
}

const MessageTimelineItemView = memo(function MessageTimelineItemView({
  item,
  agentId,
  savePromptAgentId,
  assistantLabel,
  showName,
  assistantCopyText,
  stickyClassName = "",
  titleGenerationEnabled,
  onRegenerateTitleFromUserMessage,
  titleGenerationBusy,
  onForkFromMessage,
  onCopyHandoff,
}: {
  item: MessageItem;
  agentId?: string;
  savePromptAgentId?: string | null;
  assistantLabel: string;
  showName: boolean;
  assistantCopyText?: string | null;
  stickyClassName?: string;
  titleGenerationEnabled: boolean;
  onRegenerateTitleFromUserMessage: (message: string) => void;
  titleGenerationBusy: boolean;
  onForkFromMessage?: ((anchor: MessageAnchor) => void) | undefined;
  onCopyHandoff?: (anchorKey: string) => Promise<boolean>;
}) {
  return (
    <>
      {item.blocks.length > 0 ? (
        <MessageItemView
          item={item}
          agentId={agentId}
          savePromptAgentId={savePromptAgentId}
          assistantLabel={assistantLabel}
          showName={showName}
          assistantCopyText={assistantCopyText}
          stickyClassName={stickyClassName}
          titleGenerationEnabled={titleGenerationEnabled}
          onRegenerateTitleFromUserMessage={onRegenerateTitleFromUserMessage}
          titleGenerationBusy={titleGenerationBusy}
          onForkFromMessage={onForkFromMessage}
          onCopyHandoff={onCopyHandoff}
        />
      ) : null}
      {item.blocks.length === 0 && item.contextStatus === "rolledBack" ? (
        <div className="turn-context-status is-activity-context-status">
          Excluded from active context
        </div>
      ) : null}
      {item.activities.map((activity) => (
        // deferPayloads keeps collapsed thinking bodies out of the DOM until
        // opened (tool entries already defer their payloads internally). A
        // long transcript carries megabytes of thinking text, and mounting it
        // all inside closed <details> made tab switches pay for content
        // nobody had asked to see. Collapsed payloads were never findable by
        // Cmd-F anyway (unrendered ranges have no client rects), so search
        // behavior is unchanged.
        <TranscriptActivityItem key={activity.key} item={activity} isRootActivity deferPayloads />
      ))}
    </>
  );
},
(previous, next) =>
  previous.assistantLabel === next.assistantLabel &&
  previous.agentId === next.agentId &&
  previous.savePromptAgentId === next.savePromptAgentId &&
  previous.showName === next.showName &&
  previous.assistantCopyText === next.assistantCopyText &&
  previous.stickyClassName === next.stickyClassName &&
  previous.titleGenerationEnabled === next.titleGenerationEnabled &&
  previous.titleGenerationBusy === next.titleGenerationBusy &&
  previous.onRegenerateTitleFromUserMessage === next.onRegenerateTitleFromUserMessage &&
  previous.onCopyHandoff === next.onCopyHandoff &&
  previous.onForkFromMessage === next.onForkFromMessage &&
  (previous.item === next.item || sameMessageItem(previous.item, next.item)));

function MessageItemView({
  item,
  agentId,
  savePromptAgentId,
  assistantLabel,
  showName,
  assistantCopyText,
  stickyClassName = "",
  titleGenerationEnabled,
  onRegenerateTitleFromUserMessage,
  titleGenerationBusy,
  onForkFromMessage,
  onCopyHandoff,
}: {
  item: MessageItem;
  agentId?: string;
  savePromptAgentId?: string | null;
  assistantLabel: string;
  showName: boolean;
  assistantCopyText?: string | null;
  stickyClassName?: string;
  titleGenerationEnabled: boolean;
  onRegenerateTitleFromUserMessage: (message: string) => void;
  titleGenerationBusy: boolean;
  onForkFromMessage?: ((anchor: MessageAnchor) => void) | undefined;
  onCopyHandoff?: (anchorKey: string) => Promise<boolean>;
}) {
  const taggedInstructionMessage = messageItemIsTaggedInstruction(item);
  const messageText =
    item.role === "user" || item.role === "assistant" ? messageItemText(item) : null;
  const copyText =
    item.role === "assistant"
      ? assistantCopyText ?? null
      : messageItemCopyText(item);
  // A fork anchor is only meaningful on a user message, and only when the turn
  // that produced the card carried one (a detached transcript has none).
  const forkFromMessage =
    item.role === "user" && onForkFromMessage && item.anchor
      ? () => onForkFromMessage(item.anchor as MessageAnchor)
      : null;
  // Handing off exports the transcript up to and including this message: from a
  // user message that is an outstanding request, from an assistant message it is
  // where the previous agent stopped. Unlike forking it needs no adapter support
  // and no live agent, so a detached transcript can still be handed to another
  // tool.
  const copyHandoff =
    (item.role === "user" || item.role === "assistant") && onCopyHandoff
      ? () => onCopyHandoff(item.key)
      : null;
  const showUserMessageActions = Boolean(
    item.role === "user" &&
      showName &&
      (titleGenerationEnabled || savePromptAgentId || forkFromMessage || copyHandoff),
  );
  const showMessageActions = Boolean(
    !taggedInstructionMessage &&
      messageText &&
      copyText &&
      (item.role === "assistant" || showUserMessageActions),
  );
  const showHeader =
    !taggedInstructionMessage &&
    (showName || showMessageActions || item.contextStatus === "rolledBack");
  return (
    <article
      className={`turn-card role-${item.role}${
        taggedInstructionMessage ? " is-tagged-instruction-message" : ""
      }${timelineStatusClass(item.status)}${timelineContextStatusClass(
        item.contextStatus,
      )}${stickyClassName}`}
    >
      {showHeader ? (
        <header className={showName ? undefined : "is-actions-only"}>
          {showName ? (
            <span className="turn-card-role-label">
              {turnRoleLabel(item.role, assistantLabel, item.participant)}
            </span>
          ) : null}
          {item.contextStatus === "rolledBack" ? (
            <span className="turn-context-status">Excluded from active context</span>
          ) : null}
          {showMessageActions && messageText && copyText ? (
            <MessageActionsMenu
              savePromptAgentId={item.role === "user" ? savePromptAgentId : null}
              messageText={messageText}
              copyText={copyText}
              copyLabel={item.role === "assistant" ? "Copy response" : "Copy message"}
              titleGenerationEnabled={item.role === "user" && titleGenerationEnabled}
              titleGenerationBusy={titleGenerationBusy}
              onRegenerateTitleFromUserMessage={onRegenerateTitleFromUserMessage}
              onForkFromMessage={forkFromMessage}
              onCopyHandoff={copyHandoff}
            />
          ) : null}
        </header>
      ) : null}
      <div className="turn-blocks">
        {item.blocks.map((block, index) => (
          <div
            key={`${item.key}-${index}`}
            className="turn-message-block"
            data-annotation-source-turn-id={
              item.role === "assistant" ? item.blockSourceTurnIds[index] : undefined
            }
          >
            <MessageBlockView block={block} role={item.role} />
          </div>
        ))}
      </div>
    </article>
  );
}

const ignoreMessageAction = () => undefined;

function SavedAnnotationCard({
  annotation,
  sourceUnavailable = false,
  onRemove,
}: {
  annotation: TranscriptAnnotation;
  sourceUnavailable?: boolean;
  onRemove?: (annotationId: string) => Promise<void>;
}) {
  return (
    <article className="turn-card role-user turn-saved-annotation">
      <header>
        <span className="turn-card-role-label">
          Saved{sourceUnavailable ? " · source unavailable" : ""}
        </span>
        <MessageActionsMenu
          messageText={annotation.text}
          copyText={annotation.text}
          copyLabel="Copy saved highlight"
          titleGenerationEnabled={false}
          titleGenerationBusy={false}
          onRegenerateTitleFromUserMessage={ignoreMessageAction}
          onRemoveMessage={
            onRemove ? () => onRemove(annotation.id) : undefined
          }
          removeLabel="Remove saved highlight"
        />
      </header>
      <div className="turn-blocks">
        <p className="turn-text">{annotation.text}</p>
      </div>
    </article>
  );
}

// Preferred natural width for the message "..." menu; placement clamps to the pane.
const MESSAGE_MENU_PREFERRED_WIDTH = 200;

// How long the "Copied handoff" confirmation stays up before the menu closes.
const HANDOFF_FEEDBACK_MS = 900;

// The "..." menu shown at the right of a message header. Both roles offer Copy
// and a handoff briefing; user messages can also regenerate the tab title, fork
// from the message, or save to the prompt library.
// Portaled so the scrollable timeline cannot clip it; right-aligned so it grows
// toward the pane center.
function MessageActionsMenu({
  savePromptAgentId,
  messageText,
  copyText,
  copyLabel,
  titleGenerationEnabled,
  titleGenerationBusy,
  onRegenerateTitleFromUserMessage,
  onForkFromMessage,
  onCopyHandoff,
  onRemoveMessage,
  removeLabel = "Remove",
}: {
  savePromptAgentId?: string | null;
  messageText: string;
  copyText: string;
  copyLabel: string;
  titleGenerationEnabled: boolean;
  titleGenerationBusy: boolean;
  onRegenerateTitleFromUserMessage: (message: string) => void;
  onForkFromMessage?: (() => void) | null;
  onCopyHandoff?: (() => Promise<boolean>) | null;
  onRemoveMessage?: (() => Promise<void>) | null;
  removeLabel?: string;
}) {
  const [open, setOpen] = useState(false);
  // A handoff's payload is never on screen, so unlike the plain copy items this
  // one reports what happened instead of closing silently.
  const [handoffState, setHandoffState] = useState<
    "idle" | "copying" | "copied" | "failed"
  >("idle");
  const handoffTimerRef = useRef<number | null>(null);
  useEffect(
    () => () => {
      if (handoffTimerRef.current !== null) {
        window.clearTimeout(handoffTimerRef.current);
      }
    },
    [],
  );
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const popoverRef = useRef<HTMLDivElement | null>(null);
  const [pos, setPos] = useState<{
    left: number;
    top: number;
    maxHeight: number;
    maxWidth: number;
  } | null>(null);

  const positionMenu = useCallback(() => {
    const trigger = triggerRef.current;
    const popover = popoverRef.current;
    if (!trigger || !popover) {
      return;
    }
    const { height } = popover.getBoundingClientRect();
    setPos(
      placePanePopover({
        triggerRect: trigger.getBoundingClientRect(),
        popoverSize: { width: MESSAGE_MENU_PREFERRED_WIDTH, height },
        paneRect: turnPaneRectFrom(trigger),
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
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handlePointerDown);
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("mousedown", handlePointerDown);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [open]);

  useLayoutEffect(() => {
    if (!open) {
      setPos(null);
      setHandoffState("idle");
      return;
    }
    positionMenu();
    const onReflow = () => positionMenu();
    window.addEventListener("resize", onReflow);
    window.addEventListener("scroll", onReflow, true);
    return () => {
      window.removeEventListener("resize", onReflow);
      window.removeEventListener("scroll", onReflow, true);
    };
  }, [open, positionMenu]);

  return (
    <span className="turn-message-menu">
      <button
        ref={triggerRef}
        type="button"
        className="control-button turn-message-menu-trigger"
        title="Message options"
        aria-label="Message options"
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
              className="popover-surface popover-surface--context turn-message-menu-popover"
              role="menu"
              style={
                pos
                  ? {
                      left: pos.left,
                      top: pos.top,
                      maxHeight: pos.maxHeight,
                      width: Math.min(MESSAGE_MENU_PREFERRED_WIDTH, pos.maxWidth),
                      maxWidth: pos.maxWidth,
                    }
                  : { left: -9999, top: -9999 }
              }
            >
              {titleGenerationEnabled ? (
                <button
                  type="button"
                  role="menuitem"
                  className="menu-item turn-message-menu-item"
                  disabled={titleGenerationBusy}
                  onClick={(event) => {
                    event.stopPropagation();
                    setOpen(false);
                    onRegenerateTitleFromUserMessage?.(messageText);
                  }}
                >
                  {titleGenerationBusy ? "Regenerating title…" : "Regenerate title"}
                </button>
              ) : null}
              {onForkFromMessage ? (
                <button
                  type="button"
                  role="menuitem"
                  className="menu-item turn-message-menu-item"
                  onClick={(event) => {
                    event.stopPropagation();
                    setOpen(false);
                    onForkFromMessage();
                  }}
                >
                  <span className="turn-message-menu-label">Fork from here</span>
                  <span className="turn-message-menu-badge">Preview</span>
                </button>
              ) : null}
              <button
                type="button"
                role="menuitem"
                className="menu-item turn-message-menu-item"
                onClick={(event) => {
                  event.stopPropagation();
                  setOpen(false);
                  void writeClipboardText(copyText);
                }}
              >
                {copyLabel}
              </button>
              {onCopyHandoff ? (
                <button
                  type="button"
                  role="menuitem"
                  className="menu-item turn-message-menu-item"
                  title="Copy the conversation up to this message as a briefing for another coding agent"
                  disabled={handoffState === "copying"}
                  onClick={(event) => {
                    event.stopPropagation();
                    if (handoffState === "copying") {
                      return;
                    }
                    setHandoffState("copying");
                    void onCopyHandoff().then(
                      (copied) => {
                        setHandoffState(copied ? "copied" : "failed");
                        // A failure stays on screen until the menu is
                        // dismissed; only the confirmation self-closes.
                        if (!copied) {
                          return;
                        }
                        if (handoffTimerRef.current !== null) {
                          window.clearTimeout(handoffTimerRef.current);
                        }
                        handoffTimerRef.current = window.setTimeout(() => {
                          handoffTimerRef.current = null;
                          setOpen(false);
                        }, HANDOFF_FEEDBACK_MS);
                      },
                      () => setHandoffState("failed"),
                    );
                  }}
                >
                  {handoffState === "copying"
                    ? "Copying handoff…"
                    : handoffState === "copied"
                      ? "Copied handoff"
                      : handoffState === "failed"
                        ? "Handoff copy failed"
                        : "Copy handoff"}
                </button>
              ) : null}
              {savePromptAgentId ? (
                <button
                  type="button"
                  role="menuitem"
                  className="menu-item turn-message-menu-item"
                  onClick={(event) => {
                    event.stopPropagation();
                    setOpen(false);
                    requestSaveDraftAsPrompt(
                      savePromptAgentId,
                      copyText,
                      { lockToGlobal: false },
                    );
                  }}
                >
                  Save to prompt library
                </button>
              ) : null}
              {onRemoveMessage ? (
                <button
                  type="button"
                  role="menuitem"
                  className="menu-item turn-message-menu-item is-destructive"
                  onClick={(event) => {
                    event.stopPropagation();
                    setOpen(false);
                    void onRemoveMessage().catch(() => undefined);
                  }}
                >
                  {removeLabel}
                </button>
              ) : null}
            </div>,
            document.body,
          )
        : null}
    </span>
  );
}

function turnRoleLabel(
  role: string,
  assistantLabel: string,
  participant?: ThreadParticipant | null,
) {
  if (participant?.label) {
    return participant.label;
  }
  if (role === "assistant") {
    return assistantLabel;
  }
  if (role === "system") {
    return "System";
  }
  return role;
}

function MessageBlockView({ block, role }: { block: MessageBlock; role: string }) {
  if (block.type === "text") {
    if (role !== "assistant") {
      const taggedInstruction =
        role === "user" ? taggedUserInstructionDetails(block.text) : null;
      if (taggedInstruction) {
        return (
          <CollapsedTaggedUserInstruction label={taggedInstruction.label} text={block.text} />
        );
      }
      // Pasted-image markers take precedence over the long-message collapse:
      // the whole point of a pasted screenshot is to be seen, and marker-bearing
      // messages are overwhelmingly short prompts around the paste.
      if (role === "user") {
        const segments = splitImageMarkers(block.text);
        if (segments.some((segment) => segment.kind === "image")) {
          return <UserTextWithImages segments={segments} />;
        }
      }
      if (role === "user" && block.text.length > LONG_USER_MESSAGE_COLLAPSE_THRESHOLD) {
        return <CollapsedUserText text={block.text} />;
      }
      return <p className="turn-text">{block.text}</p>;
    }
    return (
      <TranscriptMarkdown
        text={block.text}
        oversizedContent={OVERSIZED_ASSISTANT_MARKDOWN}
        artifactLinks
      />
    );
  }
  return <RawTranscriptDisclosure value={block.value} deferPayload />;
}

/** A user message containing pasted-image markers keeps its text flowing
 *  exactly as typed (the paragraph stays pre-wrap) while each
 *  "[Image: source: <path>]" marker renders as the actual image at its marker
 *  position. Pathless "[Image #N]" references stay collapsed chips. */
function UserTextWithImages({ segments }: { segments: ImageMarkerSegment[] }) {
  return (
    <p className="turn-text">
      {segments.map((segment, index) =>
        segment.kind === "image" ? (
          <TranscriptImage key={index} marker={segment.text} />
        ) : (
          <span key={index}>{segment.text}</span>
        ),
      )}
    </p>
  );
}

function CollapsedTaggedUserInstruction({ label, text }: { label: string; text: string }) {
  const [expanded, setExpanded] = useState(false);
  const title = expanded ? `Collapse ${label}` : `Show ${label}`;

  return (
    <div className={`tagged-user-instruction${expanded ? " is-expanded" : " is-collapsed"}`}>
      <button
        type="button"
        className="control-button tagged-user-instruction-toggle"
        aria-expanded={expanded}
        title={title}
        onClick={() => setExpanded((current) => !current)}
      >
        {label}
      </button>
      {expanded ? <p className="turn-text is-tagged-instruction">{text}</p> : null}
    </div>
  );
}

function CollapsedUserText({ text }: { text: string }) {
  const [expanded, setExpanded] = useState(false);
  const preview = useMemo(() => collapsedUserTextPreview(text), [text]);
  const sizeLabel = useMemo(() => formatEstimatedTokenCount(text), [text]);

  return (
    <div className="long-user-message">
      <button
        type="button"
        className="control-button long-user-message-toggle"
        aria-expanded={expanded}
        onClick={() => setExpanded((current) => !current)}
      >
        <DisclosureChevron />
        <span>{expanded ? "Collapse long message" : "Expand long message"}</span>
        <span className="long-user-message-size">{sizeLabel}</span>
      </button>
      {expanded ? (
        <p className="turn-text">{text}</p>
      ) : (
        <p className="long-user-message-preview">{preview}</p>
      )}
    </div>
  );
}

function collapsedUserTextPreview(text: string) {
  const trimmedPreview = text.slice(0, LONG_USER_MESSAGE_PREVIEW_CHARS).trimEnd();
  return `${trimmedPreview}\n...`;
}

function formatTurnTranscript(turn: Turn, assistantLabel: string) {
  return [
    turnRoleLabel(turn.role, assistantLabel, turn.participant),
    ...turn.blocks.map(formatTurnBlockTranscript),
  ]
    .join("\n")
    .trimEnd();
}

function formatTurnBlockTranscript(block: TurnBlock) {
  switch (block.type) {
    case "text":
      return block.text;
    case "toolUse":
      return formatLabeledBlock(block.name, block.input);
    case "toolResult":
      return formatLabeledBlock(block.isError ? "Tool error" : "Tool result", block.content);
    case "raw":
      return formatLabeledBlock("Raw", block.value);
  }
}

function formatLabeledBlock(label: string, value: unknown) {
  const content = stringify(value);
  return content ? `${label}\n${content}` : label;
}

function stringify(value: unknown) {
  if (typeof value === "string") {
    return value;
  }
  return JSON.stringify(value, null, 2);
}
