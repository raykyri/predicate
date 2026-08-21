import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { ArrowLeft, Check, Copy, ExternalLink, Highlighter, LoaderCircle, MoreHorizontal, Pencil, RefreshCw, ScrollText, Share2, Terminal, Trash2, Wrench, X } from "lucide-react";
import { IS_MAC, isEditableTarget } from "../../lib/appHelpers";
import {
  createResearchHighlight,
  getResearchNodeContent,
  getResearchTree,
  listPublicationProposals,
  removeResearchHighlights,
  resolvePublicationProposal,
} from "../../lib/api";
import { writeClipboardText } from "../../lib/clipboard";
import { growComposerTextarea } from "../../lib/composerTextarea";
import type {
  PublicationBinding,
  PublicationProposal,
} from "../../lib/publication";
import {
  EMPTY_RESEARCH_HISTORY,
  canGoBack as historyCanGoBack,
  canGoForward as historyCanGoForward,
  initResearchHistory,
  pushResearchHistory,
  pruneResearchHistory,
  researchHistoryBack,
  researchHistoryForward,
  researchSwipeDirection,
} from "../../lib/researchHistory";
import { researchBranchInfo } from "../../lib/researchBranches";
import { listenToResearchFollowupsFocus } from "../../lib/researchShortcuts";
import {
  conversationActivityToolCalls,
  conversationToolCallLabel,
} from "../../lib/researchConversations";
import {
  canContinueThread,
  canFollowUpFrom,
  canRetryResearchNode,
  inlineChainFor,
  isActiveResearchStatus,
} from "../../lib/researchThreads";
import { countResearchDocumentWords } from "../../lib/researchDocuments";
import {
  expandedResearchHighlightOffsets,
  intersectingResearchHighlightIds,
  isResearchAskActionShortcut,
  isResearchExpandActionShortcut,
  isResearchHighlightActionShortcut,
  overlappingResearchHighlightRegions,
  researchAnchorContextBounds,
  resolveResearchHighlightOffset,
} from "../../lib/researchHighlights";
import {
  isResearchNodeSelectionChange,
  pruneResearchNavigationNodes,
  recordResearchFollowupDraft,
  recordResearchScrollPosition,
  researchNavigationStore,
  restoreResearchScrollPosition,
  saveResearchNavigation,
} from "../../lib/researchNavigation";
import {
  createResearchSelectionSnapper,
  shouldDismissEmptyResearchAskOnClick,
  type ResearchSelectionSnapper,
} from "../../lib/researchSelection";
import {
  assistantTextFromTimelineItems,
  buildTimelineItems,
  formatPlainTextTranscript,
  timelineItemsAfterLastToolCall,
  timelineItemsContainTranscriptActivity,
} from "../../lib/turnTimeline";
import {
  createResearchPublicationDraft,
  researchNodeSnapshotMatches,
} from "../../lib/publicationDrafts";
import type { MessageBlock, MessageItem } from "../../lib/turnTimeline";
import type {
  ResearchBranchRemoval,
  ResearchHighlight,
  ResearchHighlightAnchor,
  ResearchNode,
  ResearchNodeContent,
  ResearchTreeDetail,
  UpdateResearchDocumentResult,
} from "../../types";
import { ComposerSubmitShortcutGlyph } from "../ComposerSubmitShortcut";
import DomSearchBar from "../DomSearchBar";
import type { PublishDialogTarget } from "../PublishDialog";
import {
  RawTranscriptDisclosure,
  TranscriptActivityItem,
  timelineContextStatusClass,
  timelineStatusClass,
} from "../TranscriptActivity";
import TranscriptMarkdown, {
  TranscriptLinkActionsProvider,
  type LinkActions,
} from "../TranscriptMarkdown";
import DocumentComposer from "./DocumentComposer";
import {
  ResearchDocumentFrame,
  ResearchHistoryNav,
  ResearchSidebarRestoreButton,
} from "./ResearchDocumentChrome";

interface ResearchDocumentProps {
  detail: ResearchTreeDetail | null;
  /** Durable sidebar title shown in the header while tree detail is loading. */
  treeTitle?: string;
  /** Archived trees remain browsable, but cannot be branched. */
  archived: boolean;
  /** Why `detail` is null, when the tree fetch itself failed. */
  detailError?: string | null;
  /** Refetches the active tree's detail after a failed load. */
  onRetryDetail?: () => void;
  onFork: (
    parentNodeId: string,
    prompt: string,
    publicationProposal?: {
      publicationId: string;
      commentId: number;
    } | null,
    queryAnchor?: ResearchHighlightAnchor | null,
    inline?: boolean,
  ) => Promise<ResearchNode>;
  onRemoveBranch: (nodeId: string) => Promise<ResearchBranchRemoval>;
  onRemoveTree: (treeId: string) => Promise<void>;
  onUpdateDocument: (input: {
    nodeId: string;
    markdown: string;
    title: string | null;
    expectedResponseRevision: string;
    expectedTitle: string;
    expectedHighlightIds: string[];
  }) => Promise<UpdateResearchDocumentResult>;
  onCancel: (nodeId: string) => Promise<void>;
  /** Relaunches a failed or cancelled run in place (same node id, same
   * inputs). The refreshed tree detail flows back through the caller's
   * reconciliation, so the segment re-renders as Queued. */
  onRetryNode: (nodeId: string) => Promise<void>;
  onOpenPane: (paneId: string) => void;
  linkActions: LinkActions;
  onError: (message: string) => void;
  onToast: (message: string, tone?: "normal" | "warning") => void;
  onPublish: (target: PublishDialogTarget) => void;
  publicationBinding?: PublicationBinding | null;
  onPublicationBindingChange: (binding: PublicationBinding) => void;
  /** Reopens the application sidebar when research is using the full width. */
  onShowSidebar?: () => void;
  onOpenTerminalMap?: () => void;
  terminalMapOpen?: boolean;
  /** Show held-⌘ shortcut badges (the ⌘J follow-ups hint). */
  shortcutHintsShown: boolean;
}

// The backend caps snapshots at 64MB, which is still far beyond what markdown
// parsing and eager React element creation can absorb without freezing the
// interface. Blocks past this size render as plain preformatted text — itself
// display-capped, since laying out a multi-megabyte text node freezes the interface
// too — and long transcripts render only their tail until expanded.
const MARKDOWN_CHAR_LIMIT = 100_000;
const PLAINTEXT_DISPLAY_CHAR_LIMIT = 1_000_000;
const ACTIVITY_PAYLOAD_CHAR_LIMIT = 200_000;
const TIMELINE_ITEM_RENDER_WINDOW = 100;
// Hoisted so the memoized markdown renderer sees a stable prop identity — an
// inline object literal would defeat its render cache on every poll.
const OVERSIZED_MARKDOWN_POLICY = {
  maxCharacters: MARKDOWN_CHAR_LIMIT,
  maxDisplayCharacters: PLAINTEXT_DISPLAY_CHAR_LIMIT,
  fallbackClassName: "research-plaintext",
} as const;

const RESEARCH_SWIPE_IDLE_MS = 180;
const FOLLOWUP_MENU_WIDTH = 230;
const FOLLOWUP_MENU_HEIGHT = 154;
const DOCUMENT_MENU_HEIGHT = 196;
const FOLLOWUP_MENU_MARGIN = 8;

interface FollowupMenu {
  nodeId: string;
  left: number;
  top: number;
}

interface HighlightAction {
  /** The thread segment (node) the selection landed in. */
  nodeId: string;
  anchor: ResearchHighlightAnchor;
  highlightIds: string[];
  /** The merged annotation an Expand action would save — the union of the
   * selection and every highlight it intersects. Null when there is nothing
   * to expand. */
  expandAnchor: ResearchHighlightAnchor | null;
  left: number;
  top: number;
  /** The selection has scrolled out of the viewport: keep the action alive
   * but stop drawing a bar over unrelated content. */
  offscreen: boolean;
}

interface ResolvedHighlight {
  highlight: ResearchHighlight;
  start: number;
  end: number;
}

interface DocumentEditSession {
  nodeId: string;
  markdown: string;
  title: string;
  responseRevision: string;
  highlightIds: string[];
  highlightCount: number;
}

interface ResearchHighlightRegistry {
  set(name: string, highlight: unknown): void;
  delete(name: string): void;
}

interface ResearchNativeHighlight {
  add(range: Range): void;
  clear(): void;
  priority: number;
}

interface ResearchHighlightApi {
  registry: ResearchHighlightRegistry;
  Highlight: new () => ResearchNativeHighlight;
}

const RESEARCH_HIGHLIGHT_NAME = "qmux-research-highlights";
const RESEARCH_QUERY_ANCHOR_NAME = "qmux-research-query-anchors";
const RESEARCH_OVERLAP_NAME = "qmux-research-highlight-overlaps";
const RESEARCH_SELECTED_NAME = "qmux-research-selected-highlights";
// Stacking order for the highlight layers that repaint over the shared base
// tone: overlap regions above the base paint and the selection tone above
// everything.
const RESEARCH_OVERLAP_PRIORITY = 1;
const RESEARCH_SELECTED_PRIORITY = 2;
const RESEARCH_HIGHLIGHT_CONTEXT_LENGTH = 128;
// Clearance between the anchored composer's bottom edge and the first pushed
// follow-up card.
const ASK_COMPOSER_CLEARANCE = 20;
// Minimum vertical gap between anchored cards after collision resolution.
// Tighter than the stacked rail's 20px so a cascaded cluster stays visually
// attached to the passage that produced it.
const ANCHORED_CARD_GAP = 12;
// When connector vertical runs would sit on top of one another, they fan out
// into the gutter by one lane's worth per collision. A run may be displaced up
// to this fraction of its own horizontal span from the centered midline.
const CONNECTOR_STAGGER_FRACTION = 0.25;
// Pixel separation between adjacent lanes of overlapping vertical runs. Capped
// per-connector by CONNECTOR_STAGGER_FRACTION.
const CONNECTOR_STAGGER_STEP = 14;
// Wait for window/split-pane resizing to settle before remeasuring passage,
// card, and connector geometry. A trailing debounce avoids forced layout on
// every drag frame while still repairing the final arrangement promptly.
const ANCHOR_LAYOUT_DEBOUNCE_MS = 140;
// Match the platform's small click-versus-drag tolerance: a click should keep
// link, annotation, and native double-click behavior, while a deliberate drag
// switches to live whole-word selection quickly.
const RESEARCH_SELECTION_DRAG_THRESHOLD = 3;

interface ResearchSelectionDrag {
  root: HTMLDivElement;
  anchorOffset: number | null;
  originX: number;
  originY: number;
  lastX: number;
  lastY: number;
  active: boolean;
  snapEligible: boolean;
  /** Undefined until the drag crosses the click threshold, null when word
   * segmentation is unavailable, otherwise cached for every live frame. */
  snapper: ResearchSelectionSnapper | null | undefined;
  frame: number | null;
}

function researchHighlightApi(): ResearchHighlightApi | null {
  const css = (globalThis as unknown as { CSS?: { highlights?: ResearchHighlightRegistry } }).CSS;
  const Highlight = (globalThis as unknown as { Highlight?: unknown }).Highlight;
  if (!css?.highlights || typeof Highlight !== "function") {
    return null;
  }
  return {
    registry: css.highlights,
    Highlight: Highlight as new () => ResearchNativeHighlight,
  };
}

function textNodesWithin(root: HTMLElement) {
  const nodes: Text[] = [];
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let node = walker.nextNode();
  while (node) {
    nodes.push(node as Text);
    node = walker.nextNode();
  }
  return nodes;
}

function rangeForTextOffsets(root: HTMLElement, start: number, end: number) {
  if (start < 0 || end <= start) {
    return null;
  }
  const nodes = textNodesWithin(root);
  let consumed = 0;
  let startPoint: { node: Text; offset: number } | null = null;
  let endPoint: { node: Text; offset: number } | null = null;
  for (const node of nodes) {
    const next = consumed + node.data.length;
    if (!startPoint && start <= next) {
      startPoint = { node, offset: start - consumed };
    }
    if (end <= next) {
      endPoint = { node, offset: end - consumed };
      break;
    }
    consumed = next;
  }
  if (!startPoint || !endPoint) {
    return null;
  }
  const range = document.createRange();
  range.setStart(startPoint.node, startPoint.offset);
  range.setEnd(endPoint.node, endPoint.offset);
  return range;
}

function applyDirectionalSelectionRange(
  range: Range,
  direction: "forward" | "backward",
) {
  const selection = window.getSelection();
  if (!selection) {
    return;
  }
  if (typeof selection.setBaseAndExtent === "function") {
    if (direction === "forward") {
      selection.setBaseAndExtent(
        range.startContainer,
        range.startOffset,
        range.endContainer,
        range.endOffset,
      );
    } else {
      selection.setBaseAndExtent(
        range.endContainer,
        range.endOffset,
        range.startContainer,
        range.startOffset,
      );
    }
    return;
  }
  selection.removeAllRanges();
  selection.addRange(range);
}

// Rows whose text is transcript machinery rather than the assistant's prose:
// tool calls and their payloads, the "Raw" disclosure (also a `.tool-block`,
// and it can nest inside a message), collapsed thinking, and grouped activity.
// Selections that touch any of these carry no meaning worth anchoring, so
// highlight/ask is not offered on them.
const NON_TEXT_ROW_SELECTOR =
  ".tool-block, .thinking-block, .activity-group-block, .research-conversation-activity";

/** True when the range starts, ends, or spans across any non-text row. Because
 * `.tool-block` disclosures can sit inside a `.research-response-message`, this
 * intersection test — not a positive "is inside a message" check — is what
 * keeps highlights confined to prose. */
function selectionTouchesNonTextRow(root: HTMLElement, range: Range) {
  for (const row of root.querySelectorAll(NON_TEXT_ROW_SELECTOR)) {
    if (range.intersectsNode(row)) {
      return true;
    }
  }
  return false;
}

function selectionOffsets(root: HTMLElement, range: Range) {
  if (!root.contains(range.startContainer) || !root.contains(range.endContainer)) {
    return null;
  }
  const prefixRange = document.createRange();
  prefixRange.selectNodeContents(root);
  prefixRange.setEnd(range.startContainer, range.startOffset);
  const throughSelectionRange = document.createRange();
  throughSelectionRange.selectNodeContents(root);
  throughSelectionRange.setEnd(range.endContainer, range.endOffset);
  const start = prefixRange.cloneContents().textContent?.length ?? 0;
  const end = throughSelectionRange.cloneContents().textContent?.length ?? 0;
  return end > start ? { start, end } : null;
}

/** The flat rendered-text offset of a caret position, or null when the
 * position sits outside `root`. Text-node positions take the cheap walking
 * path; element positions (rare — clicks on gaps between blocks) fall back to
 * the same clone-and-measure approach as `selectionOffsets`. */
function flatTextOffsetAt(root: HTMLElement, node: Node, offsetInNode: number) {
  if (!root.contains(node)) {
    return null;
  }
  if (node.nodeType === Node.TEXT_NODE) {
    let consumed = 0;
    for (const textNode of textNodesWithin(root)) {
      if (textNode === node) {
        return consumed + Math.min(offsetInNode, textNode.data.length);
      }
      consumed += textNode.data.length;
    }
    return null;
  }
  const probe = document.createRange();
  probe.selectNodeContents(root);
  try {
    probe.setEnd(node, offsetInNode);
  } catch {
    return null;
  }
  return probe.cloneContents().textContent?.length ?? 0;
}

/** Hit-tests a viewport point against the rendered text: the flat offset the
 * point falls on, or null off-text. caretRangeFromPoint is the WebKit spelling,
 * caretPositionFromPoint the standard one. */
function flatOffsetAtPoint(root: HTMLElement, clientX: number, clientY: number) {
  const doc = document as Document & {
    caretPositionFromPoint?: (
      x: number,
      y: number,
    ) => { offsetNode: Node; offset: number } | null;
  };
  if (typeof doc.caretRangeFromPoint === "function") {
    const range = doc.caretRangeFromPoint(clientX, clientY);
    return range ? flatTextOffsetAt(root, range.startContainer, range.startOffset) : null;
  }
  if (typeof doc.caretPositionFromPoint === "function") {
    const position = doc.caretPositionFromPoint(clientX, clientY);
    return position ? flatTextOffsetAt(root, position.offsetNode, position.offset) : null;
  }
  return null;
}

/** Flat offsets where the enclosing `.research-response-message` changes —
 * every seam between one message and the next, or between a message and the
 * tool/thinking rows around it. The projection concatenates these with no
 * separating whitespace, so word snapping needs them to avoid fusing a
 * message's last word to its neighbour's first (see
 * `createResearchSelectionSnapper`). */
function messageFlatBoundaries(root: HTMLElement) {
  const boundaries: number[] = [];
  let consumed = 0;
  let previous: HTMLElement | null = null;
  let first = true;
  for (const node of textNodesWithin(root)) {
    const message =
      node.parentElement?.closest<HTMLElement>(".research-response-message") ?? null;
    if (!first && message !== previous) {
      boundaries.push(consumed);
    }
    first = false;
    previous = message;
    consumed += node.data.length;
  }
  return boundaries;
}

/** Flat-offset span of the `.research-response-message` that encloses the
 * whole selection, or null when the selection is not confined to a single
 * message. A message renders the same text in every transcript view, but the
 * rows around it do not, so anchoring context taken near a message edge is
 * clamped to this span — otherwise a prefix/suffix that reached into an
 * adjacent tool or thinking row would only resolve in the view it was taken
 * in, and the highlight would vanish on toggle. */
function enclosingMessageFlatBounds(root: HTMLElement, range: Range) {
  const messageOf = (node: Node) =>
    (node instanceof Element ? node : node.parentElement)?.closest<HTMLElement>(
      ".research-response-message",
    ) ?? null;
  const message = messageOf(range.startContainer);
  if (!message || message !== messageOf(range.endContainer)) {
    return null;
  }
  const start = flatTextOffsetAt(root, message, 0);
  const end = flatTextOffsetAt(root, message, message.childNodes.length);
  return start !== null && end !== null ? { start, end } : null;
}

function textContextSlice(text: string, start: number, end: number) {
  let safeStart = Math.max(0, start);
  let safeEnd = Math.min(text.length, end);
  if (
    safeStart > 0 &&
    safeStart < text.length &&
    /[\uDC00-\uDFFF]/.test(text[safeStart]) &&
    /[\uD800-\uDBFF]/.test(text[safeStart - 1])
  ) {
    safeStart -= 1;
  }
  if (
    safeEnd > 0 &&
    safeEnd < text.length &&
    /[\uD800-\uDBFF]/.test(text[safeEnd - 1]) &&
    /[\uDC00-\uDFFF]/.test(text[safeEnd])
  ) {
    safeEnd += 1;
  }
  return text.slice(safeStart, safeEnd);
}

/** The quoted selection as a single line: block structure and inline markdown
 * are already absent from the rendered-text projection, so collapsing
 * whitespace is all that quoting requires. */
function quoteDisplayText(exact: string) {
  return exact.split(/\s+/).join(" ").trim();
}

/** Where the action bar sits for a selection rect: just under it, clamped to
 * the viewport with `reservedWidth` room for the buttons before the right
 * edge (wider when an Expand button joins Remove and Ask). */
function highlightActionPlacement(rect: DOMRect, reservedWidth = 260) {
  return {
    left: Math.max(8, Math.min(rect.left, window.innerWidth - reservedWidth)),
    top: Math.max(8, Math.min(rect.bottom + 4, window.innerHeight - 35)),
    offscreen: rect.bottom < 0 || rect.top > window.innerHeight,
  };
}

// Rounded-elbow connector path: out from the passage's line into the gutter,
// a vertical run at `midX` (defaults to the gutter's midline), then into the
// card at the card's own height. Degenerates to a straight segment when the
// pair is level or the gutter is too tight for the turns.
function connectorElbowPath(
  sx: number,
  sy: number,
  ex: number,
  ey: number,
  midX = Math.round((sx + ex) / 2),
): string {
  const dy = ey - sy;
  if (Math.abs(dy) < 2 || ex - sx < 8) {
    return `M ${sx} ${sy} L ${ex} ${ey}`;
  }
  const r = Math.min(10, Math.abs(dy) / 2, midX - sx, ex - midX);
  const dir = dy > 0 ? 1 : -1;
  return (
    `M ${sx} ${sy} L ${midX - r} ${sy}` +
    ` Q ${midX} ${sy} ${midX} ${sy + dir * r}` +
    ` L ${midX} ${ey - dir * r}` +
    ` Q ${midX} ${ey} ${midX + r} ${ey}` +
    ` L ${ex} ${ey}`
  );
}

function sameCardTops(a: Record<string, number>, b: Record<string, number>) {
  const aKeys = Object.keys(a);
  return (
    aKeys.length === Object.keys(b).length &&
    aKeys.every((key) => a[key] === b[key])
  );
}

/** Removes the given keys from a record, keeping the identity when none are
 * present — the shape every per-node cache invalidation here needs. */
function withoutKeys<T>(record: Record<string, T>, keys: Iterable<string>): Record<string, T> {
  const stale = [...keys].filter((key) => key in record);
  if (stale.length === 0) {
    return record;
  }
  const next = { ...record };
  for (const key of stale) {
    delete next[key];
  }
  return next;
}

/** Dotted elbow connectors, grouped by the thread segment whose grid hosts
 * them. Paths are in that segment's response-grid pixel coordinates. */
interface SegmentAnchorConnector {
  segmentId: string;
  id: string;
  d: string;
  x: number;
  y: number;
}

/** Content-derived render state for one thread segment, cached per node (see
 * segmentViews) so the whole view identity survives detail replacements —
 * which is what lets ThreadSegment's memo and the markdown renderer's cache
 * hold. `node` is the node as of the last content/toggle change and MAY BE
 * STALE on volatile fields (status, error, timestamps); consumers needing
 * fresh metadata take the live node separately. */
interface SegmentView {
  node: ResearchNode;
  content: ResearchNodeContent | null;
  isDocument: boolean;
  isConversation: boolean;
  showAllTurns: boolean;
  showFullTrace: boolean;
  timelineItems: MessageItem[];
  displayedTimelineItems: MessageItem[];
  visibleTimelineItems: MessageItem[];
  hiddenTimelineItemCount: number;
  hasTranscriptActivity: boolean;
  rawAnswer: string;
  conversationCopyText: string | null;
  answerWordCount: number;
  editableDocumentMarkdown: string | null;
}

function horizontalScrollerConsumesWheel(
  target: EventTarget | null,
  boundary: HTMLElement,
  deltaX: number,
): boolean {
  let element = target instanceof Element ? target : null;
  while (element && element !== boundary && boundary.contains(element)) {
    if (element instanceof HTMLElement && element.scrollWidth > element.clientWidth) {
      const overflowX = getComputedStyle(element).overflowX;
      if (overflowX === "auto" || overflowX === "scroll") {
        const canScrollLeft = deltaX < 0 && element.scrollLeft > 0;
        const canScrollRight =
          deltaX > 0 && element.scrollLeft < element.scrollWidth - element.clientWidth;
        if (canScrollLeft || canScrollRight) {
          return true;
        }
      }
    }
    element = element.parentElement;
  }
  return false;
}

function statusLabel(status: ResearchNode["status"]) {
  switch (status) {
    case "queued":
      return "Queued";
    case "starting":
      return "Starting";
    case "running":
      return "Working…";
    case "complete":
      return "Complete";
    case "failed":
      return "Failed";
    case "cancelled":
      return "Cancelled";
  }
}

function formatDuration(durationMs: number) {
  const totalSeconds = Math.max(0, Math.floor(durationMs / 1000));
  if (totalSeconds < 60) {
    return `${totalSeconds}s`;
  }
  const minutes = Math.floor(totalSeconds / 60);
  if (minutes < 60) {
    return `${minutes}m ${totalSeconds % 60}s`;
  }
  return `${Math.floor(minutes / 60)}h ${minutes % 60}m`;
}

function unexpectedRoleLabel(role: string) {
  if (role === "system") {
    return "System content";
  }
  if (role === "user") {
    return "Additional user content";
  }
  return `${role || "Unknown"} content`;
}

function ResearchMessageBlock({
  block,
  role,
  conversation = false,
}: {
  block: MessageBlock;
  role: string;
  conversation?: boolean;
}) {
  if (block.type === "text") {
    // In a conversation node every turn is first-class content: user
    // messages render as markdown prompts, not as unexpected-content
    // callouts (that framing exists for run responses, where a mid-response
    // user turn signals leakage).
    if (role === "assistant" || conversation) {
      return (
        <TranscriptMarkdown
          text={block.text}
          imageBehavior="open"
          oversizedContent={OVERSIZED_MARKDOWN_POLICY}
        />
      );
    }
    return <p className="research-unexpected-text">{block.text}</p>;
  }
  return (
    <RawTranscriptDisclosure
      value={block.value}
      maxPayloadCharacters={ACTIVITY_PAYLOAD_CHAR_LIMIT}
      deferPayload
    />
  );
}

// Memoized on item identity: the tree detail is replaced by every research
// event (4×/s while any run in the tree streams), and without the memo each
// replacement re-rendered — and re-parsed the markdown of — every visible
// item. Item identities are stable across detail replacements because they
// derive from `content`, which only changes when this node's own fetch lands.
const ResearchTimelineItem = memo(function ResearchTimelineItem({
  item,
  conversation = false,
}: {
  item: MessageItem;
  conversation?: boolean;
}) {
  if (conversation) {
    return (
      <section
        className={`research-response-item role-${item.role} is-conversation${timelineContextStatusClass(
          item.contextStatus,
        )}`}
        data-timeline-key={item.key}
      >
        {item.contextStatus === "rolledBack" ? (
          <div className="turn-context-status">Excluded from active context</div>
        ) : null}
        {item.blocks.length > 0 ? (
          <div
            className={`research-response-message${
              item.role === "user" ? " research-conversation-prompt research-prompt" : ""
            }${timelineStatusClass(item.status)}`}
          >
            {item.blocks.map((block, index) => (
              <ResearchMessageBlock
                key={`${item.key}-${index}`}
                block={block}
                role={item.role}
                conversation
              />
            ))}
          </div>
        ) : null}
        {item.activities.map((activity) => {
          // Export activity markers (collapsed tool calls) render as quiet
          // chips; anything else — impossible in a well-formed export, but
          // archives are only shape-validated — falls back to the ordinary
          // activity disclosure.
          const toolCalls = conversationActivityToolCalls(activity);
          if (toolCalls === null) {
            return (
              <TranscriptActivityItem
                key={activity.key}
                item={activity}
                isRootActivity
                maxPayloadCharacters={ACTIVITY_PAYLOAD_CHAR_LIMIT}
                deferPayloads
                showResultTokenCount={false}
              />
            );
          }
          // A malformed marker count still renders a chip (labelled without
          // a number): the marker's whole job is preserving the fact that
          // tool activity happened.
          return (
            <div key={activity.key} className="research-conversation-activity">
              <Wrench size={12} aria-hidden="true" />
              <span>{conversationToolCallLabel(toolCalls)}</span>
            </div>
          );
        })}
      </section>
    );
  }
  const hasUnexpectedContent = item.role !== "assistant" && item.blocks.length > 0;
  return (
    <section
      className={`research-response-item role-${item.role}${timelineContextStatusClass(
        item.contextStatus,
      )}`}
      data-timeline-key={item.key}
    >
      {item.contextStatus === "rolledBack" ? (
        <div className="turn-context-status">Excluded from active context</div>
      ) : null}
      {item.blocks.length > 0 ? (
        <div
          className={`research-response-message${
            hasUnexpectedContent ? " research-unexpected-content" : ""
          }${timelineStatusClass(item.status)}`}
        >
          {hasUnexpectedContent ? <span>{unexpectedRoleLabel(item.role)}</span> : null}
          {item.blocks.map((block, index) => (
            <ResearchMessageBlock key={`${item.key}-${index}`} block={block} role={item.role} />
          ))}
        </div>
      ) : null}
      {item.activities.map((activity) => (
        <TranscriptActivityItem
          key={activity.key}
          item={activity}
          isRootActivity
          maxPayloadCharacters={ACTIVITY_PAYLOAD_CHAR_LIMIT}
          deferPayloads
          showResultTokenCount={false}
        />
      ))}
    </section>
  );
});

/** Keeps the previous identity of a derived value while an equality check
 * says nothing changed. Chain ids and anchored entries are recomputed on
 * every research event (4×/s while any run streams); without this, each
 * recomputation's fresh identity would churn every effect keyed on them. */
function useStableValue<T>(next: T, isEqual: (previous: T, candidate: T) => boolean): T {
  const ref = useRef(next);
  if (ref.current !== next && !isEqual(ref.current, next)) {
    ref.current = next;
  }
  return ref.current;
}

const EMPTY_SEGMENT_CHILDREN: ResearchNode[] = [];
const EMPTY_SEGMENT_CONNECTORS: SegmentAnchorConnector[] = [];

type SegmentDomKind = "anchor" | "grid" | "root" | "aside";

interface ThreadSegmentProps {
  view: SegmentView;
  /** Fresh node metadata (status, error, timestamps, pane). The comparator
   * field-compares this, so detail replacements that change nothing the
   * segment renders bail out of reconciliation. */
  node: ResearchNode;
  index: number;
  isSelected: boolean;
  contentError: string | null;
  segmentActive: boolean;
  /** Terminal/cancel controls for a run streaming (or a cancel stuck with a
   * lingering pane) on a non-selected segment; the header's pair follows the
   * selection. */
  showRunControls: boolean;
  cancelling: boolean;
  /** Pre-formatted duration line ("Generating for 1m 08s", "Ran for …", a
   * bare duration, "Waiting to start"), or null to omit. Computed by the
   * parent so the once-per-second clock tick only re-renders segments whose
   * text actually changes. */
  durationText: string | null;
  hiddenHighlightCount: number;
  /** Sorted, comma-joined ids of this segment's follow-up cards that finished
   * while open and remain unopened; each gets an unread dot. */
  unreadCardKey: string;
  pointerOverHighlight: boolean;
  /** The hover-linked anchored follow-up, narrowed to this segment's cards
   * so hovering one rail does not re-render every segment. */
  linkedAnchorId: string | null;
  connectors: SegmentAnchorConnector[];
  segmentChildren: ResearchNode[];
  anchoredCardTops: Record<string, number>;
  resolvedCardTops: Record<string, number>;
  menuOpen: boolean;
  /** The docked ask composer, when this segment hosts the in-progress ask.
   * Rendered by the parent (it owns the composer state); non-null values
   * intentionally defeat the memo so keystrokes reach the composer. */
  askComposer: React.ReactNode;
  /** The community-proposals section, present only on the selected segment. */
  proposalsSection: React.ReactNode;
  registerSegmentElement: (
    nodeId: string,
    kind: SegmentDomKind,
    element: HTMLElement | null,
  ) => void;
  onSelectNode: (nodeId: string) => void;
  onExpandTurns: (nodeId: string) => void;
  onRetryContentLoad: () => void;
  onShowFullTrace: (nodeId: string) => void;
  onCopyAnswer: (view: SegmentView) => void;
  onOpenAnswerMenu: (trigger: HTMLButtonElement, nodeId: string) => void;
  onOpenFollowupMenu: (nodeId: string, clientX: number, clientY: number) => void;
  onOpenPane: (paneId: string) => void;
  onCancelNode: (nodeId: string) => void;
  /** Whether this segment's settled node can be retried in place (predicate
   * plus archived gating, computed by the parent). */
  canRetryNode: boolean;
  /** True while this segment's retry request is in flight. */
  retryingNode: boolean;
  onRetryNode: (nodeId: string) => void;
  onCardHover: (childId: string, entering: boolean) => void;
  onRootMouseDown: (event: React.MouseEvent<HTMLDivElement>) => void;
  onRootMouseUp: (event: React.MouseEvent<HTMLDivElement>) => void;
  onRootKeyUp: () => void;
  onRootClick: (event: React.MouseEvent<HTMLDivElement>) => void;
  onRootMouseMove: (event: React.MouseEvent<HTMLDivElement>) => void;
  onRootMouseLeave: () => void;
}

function sameSegmentNode(a: ResearchNode, b: ResearchNode) {
  return (
    a.id === b.id &&
    a.status === b.status &&
    a.error === b.error &&
    a.paneId === b.paneId &&
    a.parentNodeId === b.parentNodeId &&
    a.prompt === b.prompt
  );
}

function sameSegmentChildren(a: ResearchNode[], b: ResearchNode[]) {
  return (
    a.length === b.length &&
    a.every((child, index) => {
      const other = b[index];
      return (
        child.id === other.id &&
        child.status === other.status &&
        child.prompt === other.prompt &&
        child.responsePreview === other.responsePreview
      );
    })
  );
}

function sameSegmentConnectors(
  previous: SegmentAnchorConnector[],
  next: SegmentAnchorConnector[],
) {
  return (
    previous.length === next.length &&
    previous.every((connector, index) => {
      const candidate = next[index];
      return (
        connector.id === candidate.id &&
        connector.segmentId === candidate.segmentId &&
        connector.d === candidate.d &&
        connector.x === candidate.x &&
        connector.y === candidate.y
      );
    })
  );
}

/** Everything compares by identity except the node, card, placement, and
 * connector props rebuilt on research/layout events; those compare only the
 * fields and child ids this segment actually renders.
 * The generic loop keeps future props safe by default: a new prop falls back
 * to identity comparison, which can only cost memo hits, never stale UI. */
function threadSegmentPropsEqual(prev: ThreadSegmentProps, next: ThreadSegmentProps) {
  for (const key of Object.keys(next) as (keyof ThreadSegmentProps)[]) {
    if (
      key === "node" ||
      key === "segmentChildren" ||
      key === "connectors" ||
      key === "anchoredCardTops" ||
      key === "resolvedCardTops"
    ) {
      continue;
    }
    if (!Object.is(prev[key], next[key])) {
      return false;
    }
  }
  return (
    sameSegmentNode(prev.node, next.node) &&
    sameSegmentChildren(prev.segmentChildren, next.segmentChildren) &&
    sameSegmentConnectors(prev.connectors, next.connectors) &&
    sameRailCardTops(
      next.segmentChildren,
      prev.anchoredCardTops,
      next.anchoredCardTops,
    ) &&
    sameRailCardTops(
      next.segmentChildren,
      prev.resolvedCardTops,
      next.resolvedCardTops,
    )
  );
}

interface ResearchAnswerPaneProps {
  view: SegmentView;
  node: ResearchNode;
  contentError: string | null;
  segmentActive: boolean;
  showRunControls: boolean;
  cancelling: boolean;
  durationText: string | null;
  hiddenHighlightCount: number;
  pointerOverHighlight: boolean;
  menuOpen: boolean;
  registerSegmentElement: ThreadSegmentProps["registerSegmentElement"];
  onExpandTurns: ThreadSegmentProps["onExpandTurns"];
  onRetryContentLoad: ThreadSegmentProps["onRetryContentLoad"];
  onShowFullTrace: ThreadSegmentProps["onShowFullTrace"];
  onCopyAnswer: ThreadSegmentProps["onCopyAnswer"];
  onOpenAnswerMenu: ThreadSegmentProps["onOpenAnswerMenu"];
  onOpenPane: ThreadSegmentProps["onOpenPane"];
  onCancelNode: ThreadSegmentProps["onCancelNode"];
  canRetryNode: ThreadSegmentProps["canRetryNode"];
  retryingNode: ThreadSegmentProps["retryingNode"];
  onRetryNode: ThreadSegmentProps["onRetryNode"];
  onRootMouseDown: ThreadSegmentProps["onRootMouseDown"];
  onRootMouseUp: ThreadSegmentProps["onRootMouseUp"];
  onRootKeyUp: ThreadSegmentProps["onRootKeyUp"];
  onRootClick: ThreadSegmentProps["onRootClick"];
  onRootMouseMove: ThreadSegmentProps["onRootMouseMove"];
  onRootMouseLeave: ThreadSegmentProps["onRootMouseLeave"];
}

function answerPanePropsEqual(prev: ResearchAnswerPaneProps, next: ResearchAnswerPaneProps) {
  for (const key of Object.keys(next) as (keyof ResearchAnswerPaneProps)[]) {
    if (key === "node") {
      continue;
    }
    if (!Object.is(prev[key], next[key])) {
      return false;
    }
  }
  return sameSegmentNode(prev.node, next.node);
}

/** The expensive answer subtree is isolated from its segment's live card
 * previews. A follow-up can now stream in the rail without rebuilding the
 * answer's timeline element tree or walking its Markdown renderers. */
const ResearchAnswerPane = memo(function ResearchAnswerPane({
  view,
  node,
  contentError,
  segmentActive,
  showRunControls,
  cancelling,
  durationText,
  hiddenHighlightCount,
  pointerOverHighlight,
  menuOpen,
  registerSegmentElement,
  onExpandTurns,
  onRetryContentLoad,
  onShowFullTrace,
  onCopyAnswer,
  onOpenAnswerMenu,
  onOpenPane,
  onCancelNode,
  canRetryNode,
  retryingNode,
  onRetryNode,
  onRootMouseDown,
  onRootMouseUp,
  onRootKeyUp,
  onRootClick,
  onRootMouseMove,
  onRootMouseLeave,
}: ResearchAnswerPaneProps) {
  // One-click relaunch beside a settled failure/cancellation. Errors surface
  // through the parent's handler (the shared global banner), matching the
  // neighboring cancel control.
  const retryButton = canRetryNode ? (
    <button
      className="control-button research-response-retry"
      type="button"
      disabled={retryingNode}
      onClick={() => onRetryNode(node.id)}
    >
      {retryingNode ? (
        <>
          <LoaderCircle className="research-spinner" size={12} aria-hidden="true" />
          <span>Retrying…</span>
        </>
      ) : (
        <>
          <RefreshCw size={12} aria-hidden="true" />
          <span>Retry</span>
        </>
      )}
    </button>
  ) : null;
  return (
    <section className="research-response" aria-label="Research response">
      {!view.content ? (
        <div className="research-response-loading">
          {contentError ? (
            <>
              <p role="alert">{contentError}</p>
              <button className="control-button" type="button" onClick={onRetryContentLoad}>
                Retry
              </button>
            </>
          ) : (
            <LoaderCircle className="research-spinner" size={18} aria-hidden="true" />
          )}
        </div>
      ) : (
        <>
          {contentError ? (
            <div className="research-response-stale" role="alert">
              <p>Refreshing this response failed: {contentError}</p>
              <button className="control-button" type="button" onClick={onRetryContentLoad}>
                Retry
              </button>
            </div>
          ) : null}
          {node.status === "failed" && view.content.turns.length > 0 ? (
            <div className="research-response-failure" role="alert">
              <p className="research-response-error">
                {node.error ?? "The research run failed."}
              </p>
              {retryButton}
            </div>
          ) : null}
          {view.displayedTimelineItems.length === 0 ? (
            <>
              <p className="research-response-empty">
                {node.status === "failed"
                  ? node.error ?? "The research run failed."
                  : node.status === "cancelled"
                    ? "Research was cancelled."
                    : view.content.sourceError
                      ? `The response is no longer available: ${view.content.sourceError}`
                      : node.status === "complete"
                        ? "Research completed, but its response is unavailable. Open the original session transcript if it still exists."
                        : segmentActive
                          ? view.timelineItems.length > 0
                            ? "Waiting for the final response…"
                            : "Working…"
                          : "No response is available."}
              </p>
              {node.status === "failed" || node.status === "cancelled" ? retryButton : null}
            </>
          ) : (
            <>
              {view.hiddenTimelineItemCount > 0 && !view.isConversation ? (
                <button
                  type="button"
                  className="control-button research-show-earlier"
                  onClick={() => onExpandTurns(node.id)}
                >
                  Show {view.hiddenTimelineItemCount} earlier response item
                  {view.hiddenTimelineItemCount === 1 ? "" : "s"}
                </button>
              ) : null}
              <div
                ref={(element) => registerSegmentElement(node.id, "root", element)}
                data-node-id={node.id}
                className={`research-response-content-root${
                  pointerOverHighlight ? " is-highlight-hovered" : ""
                }`}
                onMouseDown={onRootMouseDown}
                onMouseUp={onRootMouseUp}
                onKeyUp={onRootKeyUp}
                onClick={onRootClick}
                onMouseMove={onRootMouseMove}
                onMouseLeave={onRootMouseLeave}
              >
                {view.visibleTimelineItems.map((item) => (
                  <ResearchTimelineItem
                    key={item.key}
                    item={item}
                    conversation={view.isConversation}
                  />
                ))}
              </div>
              {view.hiddenTimelineItemCount > 0 && view.isConversation ? (
                <button
                  type="button"
                  className="control-button research-show-earlier"
                  onClick={() => onExpandTurns(node.id)}
                >
                  Show {view.hiddenTimelineItemCount} more turn
                  {view.hiddenTimelineItemCount === 1 ? "" : "s"}
                </button>
              ) : null}
            </>
          )}
          <footer className="research-answer-meta">
            <span>
              {view.answerWordCount.toLocaleString()} {view.answerWordCount === 1 ? "word" : "words"}
            </span>
            {durationText ? <span>{durationText}</span> : null}
            {hiddenHighlightCount > 0 ? (
              <span
                className="research-hidden-highlights"
                title="These saved highlights couldn't be located in the current view. Their passages may sit in content that isn't rendered right now."
              >
                {hiddenHighlightCount} {hiddenHighlightCount === 1 ? "highlight" : "highlights"} not
                visible in this view
                {/* Only a run needs a reveal here, and `hasTranscriptActivity`
                    is false on the other kinds. What can hide a run's passage
                    sits behind the answer fold, which nothing else on screen
                    opens. A conversation has no fold — its tool payloads never
                    survived the export — and the only content it hides is
                    windowed-off turns, whose expander already renders directly
                    above this footer under the same condition. */}
                {view.hasTranscriptActivity && !view.showFullTrace ? (
                  <>
                    {" · "}
                    <button
                      type="button"
                      className="control-button research-hidden-highlights-reveal"
                      onClick={() => onShowFullTrace(node.id)}
                    >
                      Show full transcript
                    </button>
                  </>
                ) : null}
              </span>
            ) : null}
            {node.status === "complete" && (view.conversationCopyText ?? view.rawAnswer) ? (
              <button
                type="button"
                className="control-button research-answer-copy"
                title={view.isConversation ? "Copy conversation as Markdown" : "Copy answer as Markdown"}
                aria-label={
                  view.isConversation ? "Copy conversation as Markdown" : "Copy answer as Markdown"
                }
                onClick={() => onCopyAnswer(view)}
              >
                <Copy size={14} aria-hidden="true" />
              </button>
            ) : null}
            <button
              type="button"
              className="control-button research-answer-menu-trigger"
              title="Answer actions"
              aria-label="Answer actions"
              aria-haspopup="menu"
              aria-expanded={menuOpen}
              data-research-answer-menu-trigger
              onClick={(event) => onOpenAnswerMenu(event.currentTarget, node.id)}
            >
              <MoreHorizontal size={15} aria-hidden="true" />
            </button>
            {showRunControls ? (
              <div className="research-segment-actions">
                {node.paneId ? (
                  <button
                    type="button"
                    className="control-button research-segment-action"
                    onClick={() => onOpenPane(node.paneId!)}
                  >
                    Open terminal
                  </button>
                ) : null}
                <button
                  type="button"
                  className="control-button research-segment-action"
                  disabled={cancelling}
                  onClick={() => onCancelNode(node.id)}
                >
                  {cancelling
                    ? "Cancelling…"
                    : node.status === "cancelled"
                      ? "Retry cancel"
                      : "Cancel"}
                </button>
              </div>
            ) : null}
          </footer>
        </>
      )}
    </section>
  );
}, answerPanePropsEqual);

interface ResearchFollowupRailProps {
  nodeId: string;
  unreadCardKey: string;
  linkedAnchorId: string | null;
  segmentChildren: ResearchNode[];
  anchoredCardTops: Record<string, number>;
  resolvedCardTops: Record<string, number>;
  askComposer: React.ReactNode;
  proposalsSection: React.ReactNode;
  registerSegmentElement: ThreadSegmentProps["registerSegmentElement"];
  onSelectNode: ThreadSegmentProps["onSelectNode"];
  onOpenFollowupMenu: ThreadSegmentProps["onOpenFollowupMenu"];
  onCardHover: ThreadSegmentProps["onCardHover"];
}

function sameRailCardTops(
  children: ResearchNode[],
  previous: Record<string, number>,
  next: Record<string, number>,
) {
  return children.every((child) => previous[child.id] === next[child.id]);
}

function followupRailPropsEqual(prev: ResearchFollowupRailProps, next: ResearchFollowupRailProps) {
  for (const key of Object.keys(next) as (keyof ResearchFollowupRailProps)[]) {
    if (key === "segmentChildren" || key === "anchoredCardTops" || key === "resolvedCardTops") {
      continue;
    }
    if (!Object.is(prev[key], next[key])) {
      return false;
    }
  }
  return (
    sameSegmentChildren(prev.segmentChildren, next.segmentChildren) &&
    sameRailCardTops(next.segmentChildren, prev.anchoredCardTops, next.anchoredCardTops) &&
    sameRailCardTops(next.segmentChildren, prev.resolvedCardTops, next.resolvedCardTops)
  );
}

/** The rail owns card previews and anchored placement. Its comparator ignores
 * placement changes belonging to other segments, so one moving card no longer
 * invalidates every rail in the rendered thread. */
const ResearchFollowupRail = memo(function ResearchFollowupRail({
  nodeId,
  unreadCardKey,
  linkedAnchorId,
  segmentChildren,
  anchoredCardTops,
  resolvedCardTops,
  askComposer,
  proposalsSection,
  registerSegmentElement,
  onSelectNode,
  onOpenFollowupMenu,
  onCardHover,
}: ResearchFollowupRailProps) {
  const unreadCardIds = unreadCardKey ? new Set(unreadCardKey.split(",")) : null;
  const stackedChildren = segmentChildren.filter(
    (child) => anchoredCardTops[child.id] === undefined,
  );
  const anchoredChildren = segmentChildren
    .filter((child) => anchoredCardTops[child.id] !== undefined)
    .map((child) => ({
      child,
      top: resolvedCardTops[child.id] ?? anchoredCardTops[child.id],
    }))
    .sort((a, b) => a.top - b.top);

  // Cards keep their queryAnchor for placement and hover-linking, but the
  // quoted passage appears only after the follow-up is opened, above its
  // question in the main content column.
  const renderFollowupCard = (child: ResearchNode, anchoredTop?: number) => {
    const isUnread = unreadCardIds?.has(child.id) ?? false;
    return (
      <button
        key={child.id}
        type="button"
        className={`control-button research-followup-card${
          anchoredTop !== undefined ? " is-anchored" : ""
        }${linkedAnchorId === child.id ? " is-anchor-linked" : ""}${
          isUnread ? " has-unread" : ""
        }`}
        style={anchoredTop !== undefined ? { top: anchoredTop } : undefined}
        data-node-id={child.id}
        onClick={() => onSelectNode(child.id)}
        onMouseEnter={child.queryAnchor ? () => onCardHover(child.id, true) : undefined}
        onMouseLeave={child.queryAnchor ? () => onCardHover(child.id, false) : undefined}
        onContextMenu={(event) => {
          event.preventDefault();
          event.stopPropagation();
          onOpenFollowupMenu(child.id, event.clientX, event.clientY);
        }}
      >
        {isUnread ? (
          <span className="research-followup-unread" aria-label="New answer, not opened yet" />
        ) : null}
        <strong>{child.prompt}</strong>
        {child.responsePreview ? (
          <TranscriptMarkdown
            text={child.responsePreview}
            className="research-followup-preview"
            imageBehavior="open"
            inline
          />
        ) : null}
        {child.status !== "complete" ? (
          <small className={`is-${child.status}`}>
            {child.status === "running" ? (
              <LoaderCircle
                className="research-spinner research-followup-status-spinner"
                size={11}
                aria-hidden="true"
              />
            ) : null}
            {statusLabel(child.status)}
          </small>
        ) : null}
      </button>
    );
  };

  return (
    <aside
      ref={(element) => registerSegmentElement(nodeId, "aside", element)}
      className="research-followups"
      data-node-id={nodeId}
      aria-label="Follow-ups"
    >
      {askComposer}
      <div className="research-followup-cards" data-node-id={nodeId}>
        {stackedChildren.map((child) => renderFollowupCard(child))}
      </div>
      {anchoredChildren.map(({ child, top }) => renderFollowupCard(child, top))}
      {proposalsSection}
    </aside>
  );
}, followupRailPropsEqual);

const ResearchConnectorOverlay = memo(function ResearchConnectorOverlay({
  connectors,
  linkedAnchorId,
}: {
  connectors: SegmentAnchorConnector[];
  linkedAnchorId: string | null;
}) {
  if (connectors.length === 0) {
    return null;
  }
  return (
    <svg className="research-anchor-connector" aria-hidden="true">
      {connectors.map((connector) => (
        <g
          key={connector.id}
          className={`research-anchor-connector-pair${
            linkedAnchorId === connector.id ? " is-anchor-linked" : ""
          }`}
        >
          <path d={connector.d} />
          <circle cx={connector.x} cy={connector.y} r={2} />
        </g>
      ))}
    </svg>
  );
}, (previous, next) =>
  previous.linkedAnchorId === next.linkedAnchorId &&
  sameSegmentConnectors(previous.connectors, next.connectors));

export const ResearchSegmentPrompt = memo(function ResearchSegmentPrompt({
  visible,
  index,
  parentNodeId,
  queryQuote,
  prompt,
  onSelectNode,
}: {
  visible: boolean;
  index: number;
  parentNodeId: string | null;
  queryQuote: string | null;
  prompt: string;
  onSelectNode: (nodeId: string) => void;
}) {
  if (!visible) {
    return null;
  }
  return (
    <div className="research-prompt">
      {index === 0 && parentNodeId ? (
        <button
          type="button"
          className="control-button research-parent-link"
          onClick={() => onSelectNode(parentNodeId)}
        >
          <ArrowLeft size={13} aria-hidden="true" />
          Back
        </button>
      ) : null}
      {queryQuote ? (
        <blockquote className="research-prompt-quote">{quoteDisplayText(queryQuote)}</blockquote>
      ) : null}
      <TranscriptMarkdown text={prompt} imageBehavior="open" />
    </div>
  );
});

/** Structural shell for one thread segment. It retains the shared grid and DOM
 * registration points needed by cross-column geometry, while the prompt,
 * answer, connector, and rail subtrees own independent memo boundaries. */
const ThreadSegment = memo(function ThreadSegment({
  view,
  node,
  index,
  isSelected,
  contentError,
  segmentActive,
  showRunControls,
  cancelling,
  durationText,
  hiddenHighlightCount,
  unreadCardKey,
  pointerOverHighlight,
  linkedAnchorId,
  connectors,
  segmentChildren,
  anchoredCardTops,
  resolvedCardTops,
  menuOpen,
  askComposer,
  proposalsSection,
  registerSegmentElement,
  onSelectNode,
  onExpandTurns,
  onRetryContentLoad,
  onShowFullTrace,
  onCopyAnswer,
  onOpenAnswerMenu,
  onOpenFollowupMenu,
  onOpenPane,
  onCancelNode,
  canRetryNode,
  retryingNode,
  onRetryNode,
  onCardHover,
  onRootMouseDown,
  onRootMouseUp,
  onRootKeyUp,
  onRootClick,
  onRootMouseMove,
  onRootMouseLeave,
}: ThreadSegmentProps) {
  return (
    <div
      ref={(element) => registerSegmentElement(node.id, "anchor", element)}
      className={`research-thread-segment${isSelected ? " is-selected" : ""}`}
      data-segment-anchor={node.id}
    >
      <ResearchSegmentPrompt
        visible={!view.isDocument && !view.isConversation}
        index={index}
        parentNodeId={node.parentNodeId ?? null}
        queryQuote={node.queryAnchor?.exact ?? null}
        prompt={node.prompt}
        onSelectNode={onSelectNode}
      />
      <div
        ref={(element) => registerSegmentElement(node.id, "grid", element)}
        className="research-response-grid"
        data-node-id={node.id}
      >
        <ResearchConnectorOverlay
          connectors={connectors}
          linkedAnchorId={linkedAnchorId}
        />
        <ResearchAnswerPane
          view={view}
          node={node}
          contentError={contentError}
          segmentActive={segmentActive}
          showRunControls={showRunControls}
          cancelling={cancelling}
          durationText={durationText}
          hiddenHighlightCount={hiddenHighlightCount}
          pointerOverHighlight={pointerOverHighlight}
          menuOpen={menuOpen}
          registerSegmentElement={registerSegmentElement}
          onExpandTurns={onExpandTurns}
          onRetryContentLoad={onRetryContentLoad}
          onShowFullTrace={onShowFullTrace}
          onCopyAnswer={onCopyAnswer}
          onOpenAnswerMenu={onOpenAnswerMenu}
          onOpenPane={onOpenPane}
          onCancelNode={onCancelNode}
          canRetryNode={canRetryNode}
          retryingNode={retryingNode}
          onRetryNode={onRetryNode}
          onRootMouseDown={onRootMouseDown}
          onRootMouseUp={onRootMouseUp}
          onRootKeyUp={onRootKeyUp}
          onRootClick={onRootClick}
          onRootMouseMove={onRootMouseMove}
          onRootMouseLeave={onRootMouseLeave}
        />
        <ResearchFollowupRail
          nodeId={node.id}
          unreadCardKey={unreadCardKey}
          linkedAnchorId={linkedAnchorId}
          segmentChildren={segmentChildren}
          anchoredCardTops={anchoredCardTops}
          resolvedCardTops={resolvedCardTops}
          askComposer={askComposer}
          proposalsSection={proposalsSection}
          registerSegmentElement={registerSegmentElement}
          onSelectNode={onSelectNode}
          onOpenFollowupMenu={onOpenFollowupMenu}
          onCardHover={onCardHover}
        />
      </div>
    </div>
  );
}, threadSegmentPropsEqual);

function ResearchDocument({
  detail,
  treeTitle,
  archived,
  detailError,
  onRetryDetail,
  onFork,
  onRemoveBranch,
  onRemoveTree,
  onUpdateDocument,
  onCancel,
  onRetryNode,
  onOpenPane,
  linkActions,
  onError,
  onToast,
  onPublish,
  publicationBinding,
  onPublicationBindingChange,
  onShowSidebar,
  onOpenTerminalMap,
  terminalMapOpen,
  shortcutHintsShown,
}: ResearchDocumentProps) {
  const [selectedNodeId, setSelectedNodeId] = useState<string | null>(null);
  // Browser-style visit history for the header's back/forward controls, reset
  // per tree. Transitions live in ../../lib/researchHistory.
  const [history, setHistory] = useState(EMPTY_RESEARCH_HISTORY);
  // Loaded contents for the rendered thread's segments, keyed by node id.
  // Pruned to the current chain on chain switches, so memory stays bounded to
  // one page's worth — as it was when the document rendered a single node.
  const [contentByNode, setContentByNode] = useState<Record<string, ResearchNodeContent>>({});
  const [contentErrorByNode, setContentErrorByNode] = useState<Record<string, string>>({});
  const [followup, setFollowup] = useState("");
  // The thread composer's mode: continue the inline thread (default) or
  // branch a card from the tail answer. Ask mode overrides both — a targeted
  // ask is always a branch.
  const [followupMode, setFollowupMode] = useState<"thread" | "branch">("thread");
  const [submitting, setSubmitting] = useState(false);
  // The node whose in-place retry request is in flight (id-stable relaunch of
  // a failed/cancelled run), or null. One at a time is plenty: the control
  // that started it shows the busy state, every other retry control disables.
  const [retryingNodeId, setRetryingNodeId] = useState<string | null>(null);
  const [cancelling, setCancelling] = useState(false);
  const [followupMenu, setFollowupMenu] = useState<FollowupMenu | null>(null);
  const [deletingBranchId, setDeletingBranchId] = useState<string | null>(null);
  const [removingBranch, setRemovingBranch] = useState(false);
  const [documentEditSession, setDocumentEditSession] = useState<DocumentEditSession | null>(null);
  const [branchRemovalError, setBranchRemovalError] = useState<string | null>(null);
  const [contentLoadNonce, setContentLoadNonce] = useState(0);
  // Per-node reading state for the rendered thread: which segments show their
  // full turn window (persisted per tree) and which show the full transcript
  // (a per-visit choice, reset when the page changes).
  const [expandedNodes, setExpandedNodes] = useState<Record<string, boolean>>({});
  const [fullTraceNodes, setFullTraceNodes] = useState<Record<string, boolean>>({});
  const [highlightAction, setHighlightAction] = useState<HighlightAction | null>(null);
  const [savingHighlight, setSavingHighlight] = useState(false);
  // Saved highlights whose anchors no longer locate a passage in a segment's
  // current rendered projection (collapsed transcript content, an edited
  // document). They still exist — surfaced in that segment's footer instead
  // of vanishing silently.
  const [hiddenHighlightsByNode, setHiddenHighlightsByNode] = useState<Record<string, number>>({});
  // Follow-up cards whose answer settled while this document was open and that
  // the reader has not opened yet carry an unread dot. `firstSeenComplete`
  // records each card's status the first time it is observed, so a card that
  // was already complete when the chain loaded is treated as read; only cards
  // seen finishing under the reader's eyes light up. Opening a card (selecting
  // it) clears its dot.
  const firstSeenCompleteRef = useRef<Map<string, boolean>>(new Map());
  const [openedFollowupIds, setOpenedFollowupIds] = useState<ReadonlySet<string>>(
    () => new Set(),
  );
  // The anchored follow-up currently hover-linked to its passage, from either
  // side. This raises the associated card and connector without repainting
  // the passage's stable highlight treatment.
  const [linkedAnchorNodeId, setLinkedAnchorNodeId] = useState<string | null>(null);
  // Dotted elbows between anchored passages and their follow-up cards, in
  // each segment's response-grid pixel coordinates.
  const [anchorConnectors, setAnchorConnectors] = useState<SegmentAnchorConnector[]>([]);
  // Ask mode: the segment and selection anchor a targeted follow-up is being
  // composed against. While set, the composer docks beside the quoted
  // passage in that segment's rail.
  const [ask, setAsk] = useState<{
    nodeId: string;
    anchor: ResearchHighlightAnchor;
  } | null>(null);
  // The docked ask composer's rail offset, resolved by the anchor paint
  // effect alongside the card tops.
  const [askComposerTop, setAskComposerTop] = useState<number | null>(null);
  // Desired vertical offsets (px, relative to the owning segment's rail) for
  // child nodes whose query anchor still locates a passage in that segment's
  // rendered response. Membership in this map is what marks a card as
  // anchored; child ids are unique across segments, so one map serves the
  // whole thread.
  const [anchoredCardTops, setAnchoredCardTops] = useState<Record<string, number>>({});
  // Collision-resolved placements derived from the desired tops and the
  // rendered card heights, resolved per segment rail: cards keep their
  // passage alignment when there is room and cascade downward when anchors
  // crowd together.
  const [resolvedCardTops, setResolvedCardTops] = useState<Record<string, number>>({});
  const [highlightDomNonce, setHighlightDomNonce] = useState(0);
  const [anchorLayoutNonce, setAnchorLayoutNonce] = useState(0);
  const [pointerHighlightNodeId, setPointerHighlightNodeId] = useState<string | null>(null);
  const [metadataNow, setMetadataNow] = useState(() => Date.now());
  const [publicationProposals, setPublicationProposals] = useState<PublicationProposal[]>([]);
  const [proposalError, setProposalError] = useState<string | null>(null);
  const [proposalActionId, setProposalActionId] = useState<number | null>(null);
  const [proposalRetryNodeIds, setProposalRetryNodeIds] = useState<Record<number, string>>({});
  const treeId = detail?.tree.id ?? null;
  const documentScrollRef = useRef<HTMLElement | null>(null);
  const contentContainerRef = useRef<HTMLDivElement | null>(null);
  const followupTextareaRef = useRef<HTMLTextAreaElement | null>(null);
  const followupComposerRef = useRef<HTMLDivElement | null>(null);
  const followupMenuRef = useRef<HTMLDivElement | null>(null);
  // Resolved highlight ranges per segment, refreshed by the paint effect.
  const resolvedHighlightsRef = useRef(new Map<string, ResolvedHighlight[]>());
  // Flat-offset ranges of the query-anchor passages that resolved, tagged
  // with their owning segment, refreshed by the anchor paint effect.
  // Consulted by passage-side pointer hit-testing and connector measurement.
  const anchoredRangeOffsetsRef = useRef<
    { segmentId: string; id: string; start: number; end: number }[]
  >([]);
  const activeAskRangeOffsetsRef = useRef<{
    segmentId: string;
    start: number;
    end: number;
  } | null>(null);
  const savedHighlightPaintRef = useRef<ResearchNativeHighlight | null>(null);
  const selectedHighlightPaintRef = useRef<ResearchNativeHighlight | null>(null);
  const selectionDragRef = useRef<ResearchSelectionDrag | null>(null);
  const anchorHoverFrameRef = useRef<number | null>(null);
  const anchorHoverPointerRef = useRef<{
    root: HTMLDivElement;
    nodeId: string | null;
    clientX: number;
    clientY: number;
  } | null>(null);
  const navigationRef = useRef(researchNavigationStore());
  const navigationPersistTimerRef = useRef<number | null>(null);
  const selectedNodeIdRef = useRef(selectedNodeId);
  const treeIdRef = useRef(treeId);
  // The content-loading effect reads the tree through this ref so a routine
  // detail replacement (every research event rebuilds the object) does not
  // restart the effect and refetch content that has not changed.
  const detailRef = useRef(detail);
  const previousDetailNodesRef = useRef<ResearchNode[]>([]);
  const contentByNodeRef = useRef(contentByNode);
  const contentErrorByNodeRef = useRef(contentErrorByNode);
  const askRef = useRef(ask);
  // Set while a saved ordinary follow-up is being copied into React state.
  // The persistence effect uses it to avoid treating the one transitional
  // render (tree id loaded, state update not committed yet) as a user clear.
  const restoringFollowupDraftRef = useRef<{
    treeId: string;
    text: string;
    mode: "thread" | "branch";
  } | null>(null);
  // The (status, snapshot) stamp each cached content was fetched under, so
  // the loader can tell a cache hit from a stale entry without refetching
  // unchanged segments every time the chain recomputes.
  const fetchStampByNodeRef = useRef(new Map<string, string>());
  // Guards scroll recording and restoration, as the old contentNodeIdRef did
  // for the single-node view: offsets are only meaningful once the WHOLE
  // chain has settled (content or a terminal error per segment). While any
  // segment is still a short loading placeholder the page is not at its real
  // height, and the browser's clamp scroll event would otherwise record —
  // and permanently overwrite — the saved offset with the clamped value.
  const chainContentSettledRef = useRef(false);
  // Set once the chain page's scroll offset has been restored; cleared on
  // tree switches and cross-chain navigation. In-chain selection changes
  // scroll to segments instead of re-restoring, and a chain growing a new
  // tail must not yank the viewport back to a saved offset.
  const restoredChainRef = useRef<string | null>(null);
  // Scroll to a segment once it appears in the chain — the just-submitted
  // inline follow-up, delivered by the next detail refresh.
  const pendingScrollNodeIdRef = useRef<string | null>(null);
  selectedNodeIdRef.current = selectedNodeId;
  treeIdRef.current = treeId;
  detailRef.current = detail;
  contentByNodeRef.current = contentByNode;
  contentErrorByNodeRef.current = contentErrorByNode;
  askRef.current = ask;

  // The inline chain this document renders, stabilized by id-list equality:
  // `detail` is replaced by every research event (4×/s while any run in the
  // tree streams), and a fresh array identity per event would churn every
  // chain-keyed effect below.
  const chainNodeIds = useStableValue(
    useMemo(
      () => (detail && selectedNodeId ? inlineChainFor(detail.nodes, selectedNodeId) : []),
      [detail, selectedNodeId],
    ),
    (previous, candidate) =>
      previous.length === candidate.length &&
      previous.every((id, index) => id === candidate[index]),
  );
  // Live mirror for callbacks that must read the chain without depending on
  // it (applySelection's same-chain check).
  const chainNodeIdsRef = useRef(chainNodeIds);
  chainNodeIdsRef.current = chainNodeIds;
  const chainKey = chainNodeIds.join("\n");

  // Reflow can come from the window itself or from an internal pane resize
  // that changes the research column without firing window.resize. Observe
  // the rendered content width as well, then issue one trailing invalidation
  // that all passage/card/connector layout effects share.
  useEffect(() => {
    const target = contentContainerRef.current;
    let debounceTimer: number | null = null;
    let observedWidth = target?.getBoundingClientRect().width ?? null;
    const scheduleRelayout = () => {
      if (debounceTimer !== null) {
        window.clearTimeout(debounceTimer);
      }
      debounceTimer = window.setTimeout(() => {
        debounceTimer = null;
        setAnchorLayoutNonce((value) => value + 1);
      }, ANCHOR_LAYOUT_DEBOUNCE_MS);
    };
    const observer =
      target && typeof ResizeObserver !== "undefined"
        ? new ResizeObserver((entries) => {
            const nextWidth = entries[0]?.contentRect.width;
            if (nextWidth === undefined || observedWidth === null) {
              return;
            }
            if (Math.abs(nextWidth - observedWidth) < 0.5) {
              return;
            }
            observedWidth = nextWidth;
            scheduleRelayout();
          })
        : null;
    if (target && observer) {
      observer.observe(target);
    }
    window.addEventListener("resize", scheduleRelayout);
    return () => {
      observer?.disconnect();
      window.removeEventListener("resize", scheduleRelayout);
      if (debounceTimer !== null) {
        window.clearTimeout(debounceTimer);
      }
    };
  }, [chainKey]);
  const chainNodes = useMemo(
    () =>
      detail
        ? chainNodeIds
            .map((id) => detail.nodes.find((node) => node.id === id))
            .filter((node): node is ResearchNode => Boolean(node))
        : [],
    [detail, chainNodeIds],
  );
  const tailNode = chainNodes.length > 0 ? chainNodes[chainNodes.length - 1] : null;
  chainContentSettledRef.current =
    chainNodeIds.length > 0 &&
    chainNodeIds.every((id) => contentByNode[id] || contentErrorByNode[id]);
  // A per-segment (status, snapshot) key: the loader and pollers restart on
  // lifecycle transitions rather than on every detail replacement.
  const chainStatusKey = chainNodes
    .map((node) => `${node.id}:${node.status}:${node.responseSnapshotAt ?? 0}`)
    .join("\n");

  // Segment-scoped DOM lookups. Each ThreadSegment registers its wrapper
  // (scroll anchor), grid, response root, and rail as it mounts; measurement
  // effects read the maps instead of scanning the whole scroll subtree with
  // attribute selectors per lookup.
  const segmentDomRef = useRef({
    anchor: new Map<string, HTMLElement>(),
    grid: new Map<string, HTMLElement>(),
    root: new Map<string, HTMLElement>(),
    aside: new Map<string, HTMLElement>(),
  });
  const registerSegmentElement = useCallback(
    (nodeId: string, kind: SegmentDomKind, element: HTMLElement | null) => {
      const map = segmentDomRef.current[kind];
      if (element) {
        map.set(nodeId, element);
      } else {
        map.delete(nodeId);
      }
    },
    [],
  );
  const segmentRoot = useCallback(
    (nodeId: string) =>
      (segmentDomRef.current.root.get(nodeId) as HTMLDivElement | undefined) ?? null,
    [],
  );
  const segmentAside = useCallback(
    (nodeId: string) => segmentDomRef.current.aside.get(nodeId) ?? null,
    [],
  );
  const segmentGrid = useCallback(
    (nodeId: string) => segmentDomRef.current.grid.get(nodeId) ?? null,
    [],
  );
  const scrollToSegment = useCallback(
    (nodeId: string, behavior: ScrollBehavior = "smooth") => {
      segmentDomRef.current.anchor
        .get(nodeId)
        ?.scrollIntoView({ behavior, block: "start" });
    },
    [],
  );

  const refreshPublicationProposals = useCallback(async () => {
    const publicationId = publicationBinding?.publicationId;
    if (!publicationId) {
      setPublicationProposals([]);
      setProposalError(null);
      return;
    }
    try {
      const proposals = await listPublicationProposals(publicationId);
      setPublicationProposals(proposals);
      setProposalError(null);
    } catch (error) {
      setProposalError(error instanceof Error ? error.message : String(error));
    }
  }, [publicationBinding?.publicationId]);

  useEffect(() => {
    void refreshPublicationProposals();
  }, [refreshPublicationProposals, publicationBinding?.updatedAt]);

  // Content-derived keys the annotation machinery re-runs on: a segment's
  // durable revision landing, or a segment's transcript view toggling, both
  // shift every flat-text offset in that segment.
  const revisionsKey = chainNodeIds
    .map((id) => contentByNode[id]?.responseRevision ?? "")
    .join("\n");
  const highlightsKey = chainNodeIds
    .map((id) =>
      (contentByNode[id]?.node.highlights ?? []).map((highlight) => highlight.id).join(","),
    )
    .join("\n");
  const expandedKey = chainNodeIds.map((id) => (expandedNodes[id] ? "1" : "0")).join("");
  const fullTraceKey = chainNodeIds.map((id) => (fullTraceNodes[id] ? "1" : "0")).join("");

  // The floating action bar caches pixel geometry and a live selection from
  // one rendered projection, so a transcript-visibility change (or
  // navigation, or another segment's content landing and reflowing the page)
  // leaves it pointing at content that has moved: drop it whenever the view
  // changes.
  useEffect(() => {
    setHighlightAction(null);
    window.getSelection()?.removeAllRanges();
  }, [treeId, chainKey, revisionsKey, expandedKey, fullTraceKey]);

  // The in-progress ask and the hover-linked pair, unlike the action bar,
  // hold no cached geometry: both re-resolve their passage against whatever
  // projection is on screen, so a mere transcript toggle must not drop them —
  // only a page or revision change invalidates the anchor. Declared before
  // the restore below so that when both fire in the same pass, the restore
  // wins.
  useEffect(() => {
    setAsk(null);
    setLinkedAnchorNodeId(null);
  }, [treeId, chainKey, revisionsKey]);

  // Restore a persisted in-progress ask once its segment's content is on
  // screen. An ask can be composed on ANY chain segment and persists under
  // that segment's id, so the whole chain is searched (selected segment
  // first). Never replaces a live ask — an in-chain selection change or an
  // unrelated segment's revision landing must not retarget what the user is
  // typing. Declared after the reset above so that when both fire in the
  // same pass (a revision landing), the restore wins and the ask survives.
  useEffect(() => {
    if (!treeId || chainNodeIds.length === 0 || ask) {
      return;
    }
    const askByNode = navigationRef.current[treeId]?.askByNode;
    if (!askByNode) {
      return;
    }
    const candidates = selectedNodeId
      ? [selectedNodeId, ...chainNodeIds.filter((id) => id !== selectedNodeId)]
      : chainNodeIds;
    for (const nodeId of candidates) {
      const saved = askByNode[nodeId];
      if (!saved || !contentByNodeRef.current[nodeId]?.responseRevision) {
        continue;
      }
      setAsk({ nodeId, anchor: saved.anchor });
      // Never clobber text the user managed to type before the content
      // loaded.
      if (saved.text) {
        setFollowup((current) => current || saved.text);
      }
      return;
    }
  }, [ask, treeId, chainKey, chainNodeIds, selectedNodeId, revisionsKey]);

  // Match the right-pane composer: fit the textarea to its contents up to the
  // shared cap, then let it scroll. The chain dependency also sizes a newly
  // mounted empty composer after a page switch.
  useEffect(() => {
    const textarea = followupTextareaRef.current;
    if (textarea) {
      growComposerTextarea(textarea);
    }
  }, [followup, chainKey, ask]);

  // Event-driven node metadata; fresher than the cached contents for anything
  // that does not require reparsing a transcript (status, checkpoint,
  // children).
  const selectedDetailNode = useMemo(
    () => detail?.nodes.find((node) => node.id === selectedNodeId) ?? null,
    [detail, selectedNodeId],
  );
  // Highlight mutations are announced as research events but do not replace
  // the response snapshot. Mirror their refreshed node metadata into each
  // loaded segment so another window's changes reach both the document and a
  // subsequently opened edit warning. Bail on unchanged id lists: every
  // research event rebuilds the highlights arrays (4×/s while any run in the
  // tree streams), and adopting each fresh identity re-ran the paint
  // effect's full text-node walk per event. Highlights are
  // append/remove-only with unique ids, so id equality means the anchors are
  // unchanged too.
  useEffect(() => {
    if (!detail) {
      return;
    }
    setContentByNode((current) => {
      let changed = false;
      const next: Record<string, ResearchNodeContent> = { ...current };
      for (const node of detail.nodes) {
        const entry = current[node.id];
        if (!entry) {
          continue;
        }
        const previous = entry.node.highlights ?? [];
        const fresh = node.highlights ?? [];
        if (
          previous.length === fresh.length &&
          previous.every((highlight, index) => highlight.id === fresh[index].id)
        ) {
          continue;
        }
        next[node.id] = {
          ...entry,
          node: { ...entry.node, highlights: node.highlights },
        };
        changed = true;
      }
      return changed ? next : current;
    });
  }, [detail]);

  // Branch children per rendered segment. The chain's own members render as
  // segments, so an inline child never doubles as a card — except a stray
  // inline child on a corrupted store (a duplicated slot), which falls back
  // to a card rather than vanishing from the interface.
  const childrenBySegment = useMemo(() => {
    const chainIds = new Set(chainNodeIds);
    const map = new Map<string, ResearchNode[]>();
    if (detail) {
      for (const node of detail.nodes) {
        if (!node.parentNodeId || !chainIds.has(node.parentNodeId)) {
          continue;
        }
        if (node.inline && chainIds.has(node.id)) {
          continue;
        }
        const list = map.get(node.parentNodeId);
        if (list) {
          list.push(node);
        } else {
          map.set(node.parentNodeId, [node]);
        }
      }
    }
    return map;
  }, [detail, chainNodeIds]);
  // Remember each follow-up's status the first time it is seen, so the unread
  // dot can distinguish a card that finished while the reader watched from one
  // that was already complete when the chain loaded. Recorded after commit so
  // an aborted render never marks a card seen.
  useEffect(() => {
    const seen = firstSeenCompleteRef.current;
    for (const children of childrenBySegment.values()) {
      for (const child of children) {
        if (!seen.has(child.id)) {
          seen.set(child.id, child.status === "complete");
        }
      }
    }
  }, [childrenBySegment]);
  // Consumed only by the anchor paint effects, which read each child's id
  // and immutable queryAnchor — so keep the previous identity while the
  // (segment, id) list is unchanged, for the same event-churn reason as the
  // chain ids above. Cards render from `childrenBySegment` directly and
  // still see fresh prompts/previews/statuses.
  const anchoredEntries = useStableValue(
    useMemo(() => {
      const next: { segmentId: string; id: string; anchor: ResearchHighlightAnchor }[] = [];
      for (const segmentId of chainNodeIds) {
        for (const child of childrenBySegment.get(segmentId) ?? []) {
          if (child.queryAnchor) {
            next.push({ segmentId, id: child.id, anchor: child.queryAnchor });
          }
        }
      }
      return next;
    }, [chainNodeIds, childrenBySegment]),
    (previous, candidate) =>
      previous.length === candidate.length &&
      previous.every(
        (entry, index) =>
          entry.id === candidate[index].id &&
          entry.segmentId === candidate[index].segmentId,
      ),
  );
  // Height-relevant card content, for the collision measurer: preview and
  // prompt growth changes a card's rendered height; other detail churn does
  // not.
  const cardsMeasureKey = chainNodeIds
    .map((id) =>
      (childrenBySegment.get(id) ?? [])
        .map(
          (child) =>
            `${child.id}:${child.prompt.length}:${child.responsePreview?.length ?? 0}:${child.status}`,
        )
        .join("|"),
    )
    .join("\n");
  const deletingBranch = useMemo(
    () =>
      deletingBranchId && detail
        ? {
            node: detail.nodes.find((node) => node.id === deletingBranchId) ?? null,
            info: researchBranchInfo(detail.nodes, deletingBranchId),
          }
        : null,
    [deletingBranchId, detail],
  );

  // Scroll offsets and selections for deleted nodes would otherwise sit in
  // localStorage forever (tree-level pruning happens in the app shell, which
  // does not know a tree's nodes).
  useEffect(() => {
    if (treeId && detail) {
      const validNodeIds = new Set(detail.nodes.map((node) => node.id));
      const previousNode = selectedNodeId
        ? previousDetailNodesRef.current.find((node) => node.id === selectedNodeId)
        : null;
      const fallbackNodeId =
        previousNode?.parentNodeId && validNodeIds.has(previousNode.parentNodeId)
          ? previousNode.parentNodeId
          : detail.tree.rootNodeId;
      pruneResearchNavigationNodes(
        treeId,
        [...validNodeIds],
      );
      setHistory((current) =>
        pruneResearchHistory(current, validNodeIds, fallbackNodeId),
      );
      if (selectedNodeId && !validNodeIds.has(selectedNodeId)) {
        const navigation = (navigationRef.current[treeId] ??= { scrollByNode: {} });
        navigation.selectedNodeId = fallbackNodeId;
        saveResearchNavigation();
        // The fallback is a page swap that bypasses applySelection: reset the
        // same per-page state, or the new page never restores its saved
        // scroll offset (restoredChainRef would still hold the deleted
        // page's latch).
        restoredChainRef.current = null;
        setFullTraceNodes({});
        setFollowupMode("thread");
        setSelectedNodeId(fallbackNodeId);
      }
      previousDetailNodesRef.current = detail.nodes;
    }
  }, [detail, selectedNodeId, treeId]);

  useEffect(() => {
    if (!followupMenu) {
      return;
    }
    const closeMenu = (event: MouseEvent) => {
      const target = event.target as Node;
      // Leave trigger clicks to the trigger's own handler: closing here on
      // mousedown would clear the state its click-time toggle checks, so the
      // ensuing click always reopens and the trigger can never dismiss.
      if (
        target instanceof Element &&
        target.closest("[data-research-answer-menu-trigger]")
      ) {
        return;
      }
      if (!followupMenuRef.current?.contains(target)) {
        setFollowupMenu(null);
      }
    };
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setFollowupMenu(null);
      }
    };
    const closeOnReflow = () => setFollowupMenu(null);
    document.addEventListener("mousedown", closeMenu);
    document.addEventListener("keydown", closeOnEscape);
    window.addEventListener("resize", closeOnReflow);
    window.addEventListener("scroll", closeOnReflow, true);
    return () => {
      document.removeEventListener("mousedown", closeMenu);
      document.removeEventListener("keydown", closeOnEscape);
      window.removeEventListener("resize", closeOnReflow);
      window.removeEventListener("scroll", closeOnReflow, true);
    };
  }, [followupMenu]);

  // Stable (ref-routed) so the memoized segments' comparator never sees a
  // fresh identity for them.
  const openFollowupMenu = useCallback((nodeId: string, clientX: number, clientY: number) => {
    const currentDetail = detailRef.current;
    const node = currentDetail?.nodes.find((candidate) => candidate.id === nodeId);
    const menuHeight =
      node?.kind === "document" && node.id === currentDetail?.tree.rootNodeId
        ? DOCUMENT_MENU_HEIGHT
        : FOLLOWUP_MENU_HEIGHT;
    setFollowupMenu({
      nodeId,
      left: Math.max(
        FOLLOWUP_MENU_MARGIN,
        Math.min(clientX, window.innerWidth - FOLLOWUP_MENU_WIDTH - FOLLOWUP_MENU_MARGIN),
      ),
      top: Math.max(
        FOLLOWUP_MENU_MARGIN,
        Math.min(clientY, window.innerHeight - menuHeight - FOLLOWUP_MENU_MARGIN),
      ),
    });
  }, []);

  const followupMenuStateRef = useRef(followupMenu);
  followupMenuStateRef.current = followupMenu;
  const openAnswerMenu = useCallback(
    (trigger: HTMLButtonElement, nodeId: string) => {
      if (followupMenuStateRef.current?.nodeId === nodeId) {
        setFollowupMenu(null);
        return;
      }
      const rect = trigger.getBoundingClientRect();
      openFollowupMenu(nodeId, rect.right - FOLLOWUP_MENU_WIDTH, rect.bottom + 4);
    },
    [openFollowupMenu],
  );

  function openResearchPublisher(mode: "answer" | "tree", node: ResearchNode) {
    const detailSnapshot = detailRef.current;
    if (!detailSnapshot) {
      return;
    }
    const terminalNodes = detailSnapshot.nodes.filter((candidate) =>
      isTerminalResearchStatus(candidate.status),
    );
    const nodes = mode === "answer" ? [node] : terminalNodes;
    const publishView = segmentViews.find((candidate) => candidate.node.id === node.id);
    const preview =
      mode === "answer"
        ? publishView?.content
          ? publishView.rawAnswer
          : node.responsePreview?.trim() || researchNodeDisplayTitle(node, detailSnapshot)
        : researchTreePreview(detailSnapshot, terminalNodes);
    const existingBinding =
      mode === "tree" && publicationBinding?.source.kind === "researchTree"
        ? publicationBinding
        : null;
    setFollowupMenu(null);
    onPublish({
      kindLabel: mode === "answer" ? "research answer" : "research tree",
      initialTitle:
        mode === "answer"
          ? researchNodeDisplayTitle(node, detailSnapshot)
          : detailSnapshot.tree.title,
      previewText: preview,
      binding: existingBinding,
      buildDraft: async (title) => {
        const contents = await mapWithConcurrency(nodes, 4, (candidate) =>
          getResearchNodeContent(candidate.id),
        );
        const latestDetail = await getResearchTree(detailSnapshot.tree.id);
        for (const expectedNode of nodes) {
          const latestNode = latestDetail.nodes.find(
            (candidate) => candidate.id === expectedNode.id,
          );
          if (!latestNode || !researchNodeSnapshotMatches(expectedNode, latestNode)) {
            throw new Error(
              "Research changed while preparing the publication. Review the latest results and publish again.",
            );
          }
        }
        return createResearchPublicationDraft({
          title,
          detail: detailSnapshot,
          selectedNodeId: node.id,
          mode,
          contents,
          publicationId: existingBinding?.publicationId,
          createdAt:
            existingBinding?.publicationCreatedAt ??
            (existingBinding ? new Date(existingBinding.createdAt).toISOString() : undefined),
          updatedAt: existingBinding ? new Date().toISOString() : undefined,
          publicNodeIds: existingBinding?.publicNodeIds,
          contributionsByNodeId: Object.fromEntries(
            Object.values(existingBinding?.proposalStates ?? {})
              .filter(
                (state) =>
                  state.status === "accepted" &&
                  Boolean(state.localNodeId) &&
                  Boolean(state.resolutionCommentId),
              )
              .map((state) => [
                state.localNodeId!,
                {
                  githubLogin: state.authorLogin,
                  proposalCommentId: state.proposalCommentId,
                },
              ]),
          ),
        });
      },
    });
  }

  async function confirmBranchRemoval() {
    if (!deletingBranch?.node || !deletingBranch.info || deletingBranch.info.hasActiveRuns) {
      return;
    }
    setBranchRemovalError(null);
    setRemovingBranch(true);
    try {
      if (deletingBranch.node.id === detail?.tree.rootNodeId) {
        await onRemoveTree(detail.tree.id);
        setDeletingBranchId(null);
        return;
      }
      const removal = await onRemoveBranch(deletingBranch.node.id);
      // The backend call and detail refresh can outlive this document's tree.
      // Never let a deletion started in the previous tree overwrite the new
      // tree's history or live node selection when it eventually resolves.
      if (treeIdRef.current !== removal.treeId) {
        setDeletingBranchId(null);
        return;
      }
      const removedNodeIds = new Set(removal.removedNodeIds);
      const validNodeIds = new Set(
        (detail?.nodes ?? [])
          .filter((node) => !removedNodeIds.has(node.id))
          .map((node) => node.id),
      );
      setHistory((current) => {
        const pruned = pruneResearchHistory(current, validNodeIds, removal.parentNodeId);
        return pruned.entries[pruned.index] === removal.parentNodeId
          ? pruned
          : pushResearchHistory(pruned, removal.parentNodeId);
      });
      // Deleting a child from a currently displayed segment's follow-up list
      // already leaves us on the right page — including deleting an inline
      // follow-up, where the surviving parent is a rendered segment.
      if (isResearchNodeSelectionChange(selectedNodeIdRef.current, removal.parentNodeId)) {
        applySelection(removal.parentNodeId);
      }
      setDeletingBranchId(null);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setBranchRemovalError(message);
      onError(message);
    } finally {
      setRemovingBranch(false);
    }
  }

  useEffect(() => {
    const rootNodeId = detail?.tree.rootNodeId ?? null;
    const savedNodeId = treeId ? navigationRef.current[treeId]?.selectedNodeId : undefined;
    const selected =
      savedNodeId && detail?.nodes.some((node) => node.id === savedNodeId)
        ? savedNodeId
        : rootNodeId;
    setSelectedNodeId(selected);
    // A tree switch starts a fresh visit history rooted at the restored node.
    setHistory(initResearchHistory(selected));
    setContentByNode({});
    setContentErrorByNode({});
    fetchStampByNodeRef.current.clear();
    restoredChainRef.current = null;
    // Restore every node's expanded window with the tree: saved scroll
    // offsets were captured against this state, and restoring one without
    // the other lands the viewport in the wrong place.
    setExpandedNodes({
      ...(treeId ? navigationRef.current[treeId]?.expandedByNode : undefined),
    });
    // Full-trace is a per-visit reading choice, not a sticky session mode:
    // each page opens on its answers so revealing one node's transcript
    // never flips the default view for the next. The composer mode is
    // likewise a per-answer choice.
    setFullTraceNodes({});
    // Clear follow-up text here so a tree switch cannot keep the previous
    // tree's composer contents for even one paint of the persist effect.
    setFollowup("");
    setFollowupMode("thread");
  }, [treeId, detail?.tree.rootNodeId]);

  // Restore the ordinary composer independently of the targeted-ask restore
  // below. The document unmounts when a terminal tab comes forward, so its
  // local text and mode need to be copied back from the shared navigation
  // store when this tree mounts again. Always apply the destination tree's
  // draft (including empty) so text from the previous tree cannot leak into
  // the store via the persist effect below.
  useEffect(() => {
    if (!treeId) {
      restoringFollowupDraftRef.current = null;
      setFollowup("");
      setFollowupMode("thread");
      return;
    }
    const saved = navigationRef.current[treeId]?.followupDraft;
    restoringFollowupDraftRef.current = {
      treeId,
      text: saved?.text ?? "",
      mode: saved?.mode ?? "thread",
    };
    setFollowup(saved?.text ?? "");
    setFollowupMode(saved?.mode ?? "thread");
  }, [treeId]);

  // Switches the displayed node without touching visit history: records the
  // outgoing scroll offset and applies the selection. A target inside the
  // rendered chain scrolls to its segment — the thread is one page — while
  // anything else swaps the page. Shared by user navigation (which then
  // extends history) and back/forward (which only moves the history cursor).
  const applySelection = useCallback(
    (nodeId: string) => {
      if (!treeId) {
        return;
      }
      const navigation = (navigationRef.current[treeId] ??= { scrollByNode: {} });
      if (
        selectedNodeId &&
        documentScrollRef.current &&
        chainContentSettledRef.current
      ) {
        recordResearchScrollPosition(
          navigation,
          selectedNodeId,
          documentScrollRef.current.scrollTop,
        );
      }
      navigation.selectedNodeId = nodeId;
      saveResearchNavigation();
      const sameChain = chainNodeIdsRef.current.includes(nodeId);
      if (!sameChain) {
        // A page swap: reset the per-visit trace choice and the composer
        // mode (a "New branch" choice belongs to the answer it was made for,
        // not to whatever tail comes next), and let the chain effects prune
        // and reload content. Expanded windows are tree-wide persisted
        // state, so they carry over.
        setFullTraceNodes({});
        setFollowupMode("thread");
        restoredChainRef.current = null;
      }
      setSelectedNodeId(nodeId);
      if (sameChain) {
        window.requestAnimationFrame(() => scrollToSegment(nodeId));
      }
    },
    [scrollToSegment, selectedNodeId, treeId],
  );

  const selectNode = useCallback(
    (nodeId: string) => {
      if (!treeId) {
        return;
      }
      // A current-node click is navigation-wise a no-op — except as a retry
      // affordance when that segment's last load failed.
      if (!isResearchNodeSelectionChange(selectedNodeId, nodeId)) {
        if (contentErrorByNode[nodeId]) {
          setContentLoadNonce((value) => value + 1);
        }
        return;
      }
      applySelection(nodeId);
      setHistory((prev) => pushResearchHistory(prev, nodeId));
    },
    [applySelection, contentErrorByNode, selectedNodeId, treeId],
  );
  // Live mirror so the segments' stable onSelectNode wrapper always calls the
  // current navigation closure.
  const selectNodeRef = useRef(selectNode);
  selectNodeRef.current = selectNode;

  const canGoBack = historyCanGoBack(history);
  const canGoForward = historyCanGoForward(history);

  const goBack = useCallback(() => {
    const step = researchHistoryBack(history);
    if (!step) {
      return;
    }
    applySelection(step.nodeId);
    setHistory(step.history);
  }, [applySelection, history]);

  const goForward = useCallback(() => {
    const step = researchHistoryForward(history);
    if (!step) {
      return;
    }
    applySelection(step.nodeId);
    setHistory(step.history);
  }, [applySelection, history]);
  const goBackRef = useRef(goBack);
  const goForwardRef = useRef(goForward);
  goBackRef.current = goBack;
  goForwardRef.current = goForward;

  // Browser-style navigation shortcuts, active only while the document is
  // mounted (research surface visible). Cmd/Ctrl+[ / ] and Alt+←/→ mirror the
  // header arrows; the dedicated mouse back/forward buttons (3/4) do too. Keys
  // are ignored while typing so the follow-up composer keeps word-wise motion.
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.defaultPrevented || isEditableTarget(event.target)) {
        return;
      }
      const primary = event.metaKey || event.ctrlKey;
      let handler: (() => void) | null = null;
      if (primary && !event.altKey && !event.shiftKey && event.code === "BracketLeft") {
        handler = goBack;
      } else if (primary && !event.altKey && !event.shiftKey && event.code === "BracketRight") {
        handler = goForward;
      } else if (
        event.altKey &&
        !event.metaKey &&
        !event.ctrlKey &&
        !event.shiftKey &&
        event.key === "ArrowLeft"
      ) {
        handler = goBack;
      } else if (
        event.altKey &&
        !event.metaKey &&
        !event.ctrlKey &&
        !event.shiftKey &&
        event.key === "ArrowRight"
      ) {
        handler = goForward;
      }
      if (handler) {
        event.preventDefault();
        handler();
      }
    };
    const onMouseUp = (event: MouseEvent) => {
      if (event.button === 3) {
        event.preventDefault();
        goBack();
      } else if (event.button === 4) {
        event.preventDefault();
        goForward();
      }
    };
    const mouseTarget = documentScrollRef.current;
    window.addEventListener("keydown", onKeyDown);
    mouseTarget?.addEventListener("mouseup", onMouseUp);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
      mouseTarget?.removeEventListener("mouseup", onMouseUp);
    };
  }, [goBack, goForward]);

  // ⌘J routed from the app-level shortcut dispatcher: bring the follow-up
  // composer into view and hand it the caret. Focus reads the live disabled
  // state off the textarea (archived trees, running tails) rather than
  // re-subscribing on every composer state change; scrolling still works for
  // a disabled composer so the chord always lands on the follow-ups area.
  useEffect(
    () =>
      listenToResearchFollowupsFocus(() => {
        const textarea = followupTextareaRef.current;
        if (textarea && !textarea.disabled) {
          textarea.focus({ preventScroll: true });
        }
        followupComposerRef.current?.scrollIntoView({
          behavior: "smooth",
          block: "center",
        });
      }),
    [],
  );

  // Trackpads and horizontal mouse wheels both arrive as WheelEvents. Accumulate
  // one physical gesture until its horizontal travel is decisive, navigate once,
  // then keep the gesture locked through momentum events until it goes idle.
  // Descendant code/preformatted scrollers retain their native horizontal scroll
  // whenever they can still consume movement in the requested direction.
  useEffect(() => {
    const target = documentScrollRef.current;
    if (!target) {
      return;
    }
    let accumulatedX = 0;
    let accumulatedY = 0;
    let navigated = false;
    let blockedByScroller = false;
    let resetTimer: number | null = null;
    const resetGesture = () => {
      if (resetTimer !== null) {
        window.clearTimeout(resetTimer);
      }
      accumulatedX = 0;
      accumulatedY = 0;
      navigated = false;
      blockedByScroller = false;
      resetTimer = null;
    };
    const scheduleReset = () => {
      if (resetTimer !== null) {
        window.clearTimeout(resetTimer);
      }
      resetTimer = window.setTimeout(resetGesture, RESEARCH_SWIPE_IDLE_MS);
    };
    const onWheel = (event: WheelEvent) => {
      if (
        event.defaultPrevented ||
        event.metaKey ||
        event.ctrlKey ||
        event.altKey ||
        event.shiftKey
      ) {
        return;
      }
      const scale = event.deltaMode === WheelEvent.DOM_DELTA_LINE
        ? 16
        : event.deltaMode === WheelEvent.DOM_DELTA_PAGE
          ? target.clientWidth
          : 1;
      const deltaX = event.deltaX * scale;
      const deltaY = event.deltaY * scale;
      if (horizontalScrollerConsumesWheel(event.target, target, deltaX)) {
        blockedByScroller = true;
        scheduleReset();
        return;
      }
      if (blockedByScroller) {
        scheduleReset();
        return;
      }
      scheduleReset();
      accumulatedX += deltaX;
      accumulatedY += deltaY;
      const horizontalIntent = Math.abs(accumulatedX) > Math.abs(accumulatedY) * 1.25;
      if (navigated) {
        if (horizontalIntent) {
          event.preventDefault();
        }
        return;
      }
      const direction = researchSwipeDirection(accumulatedX, accumulatedY);
      if (direction === 0) {
        return;
      }
      event.preventDefault();
      navigated = true;
      if (direction < 0) {
        goBackRef.current();
      } else {
        goForwardRef.current();
      }
    };
    target.addEventListener("wheel", onWheel, { passive: false });
    return () => {
      target.removeEventListener("wheel", onWheel);
      if (resetTimer !== null) {
        window.clearTimeout(resetTimer);
      }
    };
  }, [Boolean(detail && selectedNodeId)]);

  const expandAllTurns = useCallback(
    (nodeId: string) => {
      setExpandedNodes((current) =>
        current[nodeId] ? current : { ...current, [nodeId]: true },
      );
      if (treeId) {
        const navigation = (navigationRef.current[treeId] ??= { scrollByNode: {} });
        (navigation.expandedByNode ??= {})[nodeId] = true;
        saveResearchNavigation();
      }
    },
    [treeId],
  );

  // Bound the cache to the rendered chain: navigating to another chain drops
  // the old page's contents, matching the single-node view's memory profile.
  useEffect(() => {
    const keep = new Set(chainNodeIds);
    const prune = <T,>(current: Record<string, T>) =>
      withoutKeys(current, Object.keys(current).filter((key) => !keep.has(key)));
    setContentByNode(prune);
    setContentErrorByNode(prune);
    for (const key of [...fetchStampByNodeRef.current.keys()]) {
      if (!keep.has(key)) {
        fetchStampByNodeRef.current.delete(key);
      }
    }
  }, [chainKey]);

  useEffect(() => {
    if (chainNodeIds.length === 0) {
      return;
    }
    let cancelled = false;
    const timers = new Map<string, number>();
    const errorCounts = new Map<string, number>();
    const stampFor = (nodeId: string) => {
      const node = detailRef.current?.nodes.find((candidate) => candidate.id === nodeId);
      return node ? `${node.status}:${node.responseSnapshotAt ?? 0}` : "";
    };
    const clearError = (nodeId: string) =>
      setContentErrorByNode((current) => withoutKeys(current, [nodeId]));
    const load = async (nodeId: string) => {
      // Stamped before the fetch: a transition that lands mid-flight
      // restarts this effect and refetches under the new stamp.
      const stamp = stampFor(nodeId);
      try {
        const next = await getResearchNodeContent(nodeId);
        if (cancelled) {
          return;
        }
        errorCounts.delete(nodeId);
        fetchStampByNodeRef.current.set(nodeId, stamp);
        clearError(nodeId);
        setContentByNode((current) => ({ ...current, [nodeId]: next }));
        if (isActiveResearchStatus(next.node.status)) {
          timers.set(nodeId, window.setTimeout(() => void load(nodeId), 1000));
        }
      } catch (err) {
        if (cancelled) {
          return;
        }
        setContentErrorByNode((current) => ({
          ...current,
          [nodeId]: err instanceof Error ? err.message : String(err),
        }));
        const knownNode = detailRef.current?.nodes.find(
          (candidate) => candidate.id === nodeId,
        );
        const attempts = (errorCounts.get(nodeId) ?? 0) + 1;
        errorCounts.set(nodeId, attempts);
        const isActive = knownNode && isActiveResearchStatus(knownNode.status);
        if (isActive || attempts <= 5) {
          timers.set(
            nodeId,
            window.setTimeout(() => void load(nodeId), Math.min(5000, 1000 * attempts)),
          );
        }
      }
    };
    for (const nodeId of chainNodeIds) {
      const node = detailRef.current?.nodes.find((candidate) => candidate.id === nodeId);
      const hasContent = Boolean(contentByNodeRef.current[nodeId]);
      const hasError = Boolean(contentErrorByNodeRef.current[nodeId]);
      if (
        !hasContent ||
        hasError ||
        fetchStampByNodeRef.current.get(nodeId) !== stampFor(nodeId)
      ) {
        // A restart is a fresh load for this segment (status transition,
        // snapshot landing, retry): a failure reported by the previous run
        // must not sit on screen while this one is in flight. An error with
        // a matching stamp still reloads — that is what the Retry button's
        // nonce bump asks for.
        clearError(nodeId);
        void load(nodeId);
      } else if (node && isActiveResearchStatus(node.status)) {
        // The cleanup above cancelled this still-streaming segment's pending
        // poll timer (a sibling's transition, a retry, or a document edit
        // restarted the effect). Re-arm it, or its live transcript freezes
        // until its own status finally changes.
        void load(nodeId);
      }
    }
    return () => {
      cancelled = true;
      for (const timer of timers.values()) {
        window.clearTimeout(timer);
      }
    };
    // Keyed on the chain's statuses and snapshot stamps rather than the
    // detail object: streaming runs replace `detail` on every event, and
    // restarting this effect for each replacement refetched and reparsed
    // unchanged content. A status transition still restarts it, which is
    // what fetches the final content once a run completes — and the snapshot
    // stamp restarts it once more if that fetch beat the adapter's final
    // transcript flush.
  }, [chainKey, chainStatusKey, contentLoadNonce]);

  // Restore the chain page's scroll offset once the WHOLE chain has settled
  // (see chainContentSettledRef — restoring against a partially loaded page
  // clamps, and the clamp destroys the saved offset). Once per page visit,
  // so a growing chain (a new inline follow-up landing) or in-chain
  // selection cannot yank the viewport back to a stale offset. When the
  // selected segment sits mid-chain and no offset was saved, land on the
  // segment itself rather than the top of the thread.
  useLayoutEffect(() => {
    if (
      !treeId ||
      !selectedNodeId ||
      !chainContentSettledRef.current ||
      !documentScrollRef.current ||
      restoredChainRef.current !== null
    ) {
      return;
    }
    restoredChainRef.current = `${treeId}:${chainKey}`;
    const saved = restoreResearchScrollPosition(
      navigationRef.current[treeId],
      selectedNodeId,
    );
    if (saved === 0 && chainNodeIds[0] !== selectedNodeId) {
      // Arriving at a mid-chain segment with nothing to restore (e.g. Back
      // from a branch page to the answer it was asked about): show that
      // answer, not the root five screens above it.
      scrollToSegment(selectedNodeId, "auto");
      return;
    }
    documentScrollRef.current.scrollTop = saved;
  }, [contentByNode, contentErrorByNode, treeId, chainKey, chainNodeIds, scrollToSegment, selectedNodeId]);

  useEffect(() => {
    if (treeId && selectedNodeId && detail?.nodes.some((node) => node.id === selectedNodeId)) {
      const navigation = (navigationRef.current[treeId] ??= { scrollByNode: {} });
      // `detail.nodes` is a fresh array on every research event; without the
      // guard a streaming run rewrote localStorage several times a second.
      if (navigation.selectedNodeId !== selectedNodeId) {
        navigation.selectedNodeId = selectedNodeId;
        saveResearchNavigation();
      }
    }
  }, [detail?.nodes, selectedNodeId, treeId]);

  useEffect(
    () => () => {
      if (navigationPersistTimerRef.current !== null) {
        window.clearTimeout(navigationPersistTimerRef.current);
      }
      const currentTreeId = treeIdRef.current;
      const currentNodeId = selectedNodeIdRef.current;
      if (
        currentTreeId &&
        currentNodeId &&
        documentScrollRef.current &&
        chainContentSettledRef.current
      ) {
        const navigation = (navigationRef.current[currentTreeId] ??= { scrollByNode: {} });
        recordResearchScrollPosition(
          navigation,
          currentNodeId,
          documentScrollRef.current.scrollTop,
        );
      }
      saveResearchNavigation();
    },
    [],
  );

  const recordScroll = useCallback(() => {
    const scroller = documentScrollRef.current;
    if (!scroller) {
      return;
    }
    // The document never scrolls sideways legitimately (wide tables and code
    // blocks scroll inside their own containers), and overflow-x is hidden —
    // but programmatic scrolls (scrollIntoView, find-in-page) can still shift
    // a hidden axis, and a stray scrollLeft reads as collapsed left padding
    // on every page sharing this scroller. Pin it back to the left edge.
    if (scroller.scrollLeft !== 0) {
      scroller.scrollLeft = 0;
    }
    if (!treeId || !selectedNodeId) {
      return;
    }
    // Loading windows are not this page's scroll state (see the ref's
    // comment) — without this, navigating to a node wipes its saved offset,
    // and a partially loaded chain's clamp event overwrites it.
    if (!chainContentSettledRef.current) {
      return;
    }
    const navigation = (navigationRef.current[treeId] ??= { scrollByNode: {} });
    recordResearchScrollPosition(navigation, selectedNodeId, scroller.scrollTop);
    if (navigationPersistTimerRef.current !== null) {
      window.clearTimeout(navigationPersistTimerRef.current);
    }
    navigationPersistTimerRef.current = window.setTimeout(() => {
      navigationPersistTimerRef.current = null;
      saveResearchNavigation();
    }, 250);
  }, [selectedNodeId, treeId]);

  const breadcrumb = useMemo(() => {
    if (!detail || !selectedNodeId) {
      return [];
    }
    const byId = new Map(detail.nodes.map((node) => [node.id, node]));
    const path: ResearchNode[] = [];
    let node = byId.get(selectedNodeId);
    const seen = new Set<string>();
    while (node && !seen.has(node.id)) {
      seen.add(node.id);
      // Inline follow-ups collapse into their chain head: the thread is one
      // page, so its members share one crumb.
      if (!node.inline) {
        path.unshift(node);
      }
      node = node.parentNodeId ? byId.get(node.parentNodeId) : undefined;
    }
    return path;
  }, [detail, selectedNodeId]);

  // Deep paths collapse to "root / … / parent / current": the intermediate
  // crumbs add little wayfinding value at this depth, and rendering them all
  // squeezes every crumb into unreadable slivers.
  const breadcrumbDisplay = useMemo(() => {
    type BreadcrumbEntry =
      | { kind: "node"; node: ResearchNode; index: number }
      | { kind: "ellipsis"; count: number };
    if (breadcrumb.length <= 4) {
      return breadcrumb.map(
        (node, index): BreadcrumbEntry => ({ kind: "node", node, index }),
      );
    }
    return [
      { kind: "node", node: breadcrumb[0], index: 0 },
      { kind: "ellipsis", count: breadcrumb.length - 3 },
      {
        kind: "node",
        node: breadcrumb[breadcrumb.length - 2],
        index: breadcrumb.length - 2,
      },
      {
        kind: "node",
        node: breadcrumb[breadcrumb.length - 1],
        index: breadcrumb.length - 1,
      },
    ] satisfies BreadcrumbEntry[];
  }, [breadcrumb]);

  // Per-segment content-derived view state. Recomputed only for segments
  // whose content or view toggles changed — node metadata stays live (it
  // refreshes 4×/s while runs stream) while the parsed timeline keeps its
  // identity, which is what keeps the memoized markdown renderer's cache
  // effective.
  const segmentViewCacheRef = useRef(
    new Map<
      string,
      {
        content: ResearchNodeContent | null;
        showAllTurns: boolean;
        showFullTrace: boolean;
        view: SegmentView;
      }
    >(),
  );
  const segmentViews = useMemo(() => {
    const cache = segmentViewCacheRef.current;
    const seen = new Set<string>();
    const views = chainNodes.map((node) => {
      seen.add(node.id);
      const content = contentByNode[node.id] ?? null;
      const showAllTurns = Boolean(expandedNodes[node.id]);
      const showFullTrace = Boolean(fullTraceNodes[node.id]);
      const cached = cache.get(node.id);
      if (
        cached &&
        cached.content === content &&
        cached.showAllTurns === showAllTurns &&
        cached.showFullTrace === showFullTrace
      ) {
        // Pure hit: the cached view keeps its identity (its `node` may go
        // stale on volatile fields — see the SegmentView contract).
        return cached.view;
      }
      // A conversation node's whole timeline is the document: there is no
      // "answer" fold to collapse to and no fuller trace to reveal (tool
      // payloads never survived the export).
      const isConversation =
        node.kind === "conversation" || content?.node.kind === "conversation";
      const isDocument = node.kind === "document";
      // Normalize the complete response before windowing it. The resulting
      // item boundaries keep a call and its result together and preserve
      // text → tools → continued-text ordering across every adapter's
      // transcript role choices.
      const timelineItems = buildTimelineItems(content?.turns ?? []);
      const answerTimelineItems = timelineItemsAfterLastToolCall(timelineItems);
      const hasTranscriptActivity =
        !isConversation && timelineItemsContainTranscriptActivity(timelineItems);
      const displayedTimelineItems =
        isConversation || showFullTrace ? timelineItems : answerTimelineItems;
      // A run trace reads bottom-up (the answer is the tail), so its window
      // keeps the newest items; a conversation reads top-down from its
      // opening question, so its window keeps the head with the remainder
      // behind the expander.
      const visibleTimelineItems =
        showAllTurns || displayedTimelineItems.length <= TIMELINE_ITEM_RENDER_WINDOW
          ? displayedTimelineItems
          : isConversation
            ? displayedTimelineItems.slice(0, TIMELINE_ITEM_RENDER_WINDOW)
            : displayedTimelineItems.slice(-TIMELINE_ITEM_RENDER_WINDOW);
      const rawAnswer = assistantTextFromTimelineItems(answerTimelineItems);
      // The copyable form of a conversation node: the whole exchange with
      // role labels, not the assistant-only fold (which would mash unrelated
      // answers together with their questions dropped).
      const conversationCopyText =
        isConversation && content ? formatPlainTextTranscript(content.turns, "Assistant") : null;
      let editableDocumentMarkdown: string | null = null;
      if (content?.node.kind === "document") {
        for (const turn of content.turns) {
          for (const block of turn.blocks) {
            if (block.type === "text") {
              editableDocumentMarkdown = block.text;
              break;
            }
          }
          if (editableDocumentMarkdown !== null) {
            break;
          }
        }
      }
      const view: SegmentView = {
        node,
        content,
        isDocument,
        isConversation,
        showAllTurns,
        showFullTrace,
        timelineItems,
        displayedTimelineItems,
        visibleTimelineItems,
        hiddenTimelineItemCount: displayedTimelineItems.length - visibleTimelineItems.length,
        hasTranscriptActivity,
        rawAnswer,
        conversationCopyText,
        answerWordCount: countResearchDocumentWords(conversationCopyText ?? rawAnswer),
        editableDocumentMarkdown,
      };
      cache.set(node.id, { content, showAllTurns, showFullTrace, view });
      return view;
    });
    for (const key of [...cache.keys()]) {
      if (!seen.has(key)) {
        cache.delete(key);
      }
    }
    return views;
  }, [chainNodes, contentByNode, expandedNodes, fullTraceNodes]);
  const selectedView =
    segmentViews.find((view) => view.node.id === selectedNodeId) ?? null;
  const viewForNode = (nodeId: string) =>
    segmentViews.find((view) => view.node.id === nodeId) ?? null;

  // Diagram rendering and other child-owned Markdown controls can replace text
  // nodes without changing the transcript items. Observe those commits so saved
  // ranges are rebuilt against the current rendered projection. Query-anchor
  // passages (and an in-progress ask) resolve against the same projections, so
  // they need the observer even when a segment has no saved highlights —
  // without it their paints and card offsets go stale after a diagram lands.
  // Only annotated segments' response roots are observed — never the rails,
  // whose streaming card previews mutate 4×/s and would otherwise churn the
  // nonce (and every DOM-walking paint effect keyed on it) for content no
  // annotation can resolve against.
  const annotatedSegmentsKey = chainNodeIds
    .filter((id) => {
      if (!contentByNode[id]?.responseRevision) {
        return false;
      }
      return (
        Boolean(contentByNode[id]?.node.highlights?.length) ||
        ask?.nodeId === id ||
        anchoredEntries.some((entry) => entry.segmentId === id)
      );
    })
    .join("\n");
  useEffect(() => {
    if (
      !annotatedSegmentsKey ||
      !researchHighlightApi() ||
      typeof MutationObserver === "undefined"
    ) {
      return;
    }
    const roots = annotatedSegmentsKey
      .split("\n")
      .map((id) => segmentRoot(id))
      .filter((root): root is HTMLDivElement => Boolean(root));
    if (roots.length === 0) {
      return;
    }
    let frame: number | null = null;
    const observer = new MutationObserver(() => {
      if (frame !== null) {
        return;
      }
      frame = window.requestAnimationFrame(() => {
        frame = null;
        setHighlightDomNonce((value) => value + 1);
      });
    });
    for (const root of roots) {
      observer.observe(root, { childList: true, characterData: true, subtree: true });
    }
    return () => {
      observer.disconnect();
      if (frame !== null) {
        window.cancelAnimationFrame(frame);
      }
    };
    // Re-observed on view toggles as well: the roots' inner DOM is rebuilt
    // wholesale then, and an observer bound before a rebuild keeps working,
    // but a root remount (content refetch after an edit) needs the rebind.
  }, [annotatedSegmentsKey, revisionsKey, expandedKey, fullTraceKey, segmentRoot]);

  // Keep the two saved-highlight paint objects registered for this document's
  // lifetime. Mutating their range sets avoids WebKit leaving deleted registry
  // paint on screen until an unrelated selection or pointer event invalidates
  // the text layer.
  useLayoutEffect(() => {
    const api = researchHighlightApi();
    api?.registry.delete(RESEARCH_HIGHLIGHT_NAME);
    api?.registry.delete(RESEARCH_SELECTED_NAME);
    if (!api) {
      return;
    }
    const saved = new api.Highlight();
    const selected = new api.Highlight();
    selected.priority = RESEARCH_SELECTED_PRIORITY;
    savedHighlightPaintRef.current = saved;
    selectedHighlightPaintRef.current = selected;
    api.registry.set(RESEARCH_HIGHLIGHT_NAME, saved);
    api.registry.set(RESEARCH_SELECTED_NAME, selected);
    return () => {
      savedHighlightPaintRef.current = null;
      selectedHighlightPaintRef.current = null;
      api.registry.delete(RESEARCH_HIGHLIGHT_NAME);
      api.registry.delete(RESEARCH_SELECTED_NAME);
    };
  }, []);

  // Paint saved ranges without rewriting the markdown DOM, across every
  // rendered segment. The CSS Custom Highlight registry is name-global, so
  // all segments' ranges aggregate into one Highlight object per layer.
  // Anchors retain an exact quote and nearby context so they can be
  // relocated when transcript visibility changes shift the flat
  // rendered-text offsets.
  useLayoutEffect(() => {
    const painted = savedHighlightPaintRef.current;
    painted?.clear();
    resolvedHighlightsRef.current = new Map();
    const hidden: Record<string, number> = {};
    if (painted) {
      for (const nodeId of chainNodeIds) {
        const root = segmentRoot(nodeId);
        const segmentContent = contentByNode[nodeId];
        if (!root || !segmentContent?.responseRevision) {
          continue;
        }
        const projection = root.textContent ?? "";
        const resolved: ResolvedHighlight[] = [];
        for (const highlight of segmentContent.node.highlights ?? []) {
          const offsets = resolveResearchHighlightOffset(
            projection,
            segmentContent.responseRevision,
            highlight,
          );
          if (!offsets) {
            continue;
          }
          const range = rangeForTextOffsets(root, offsets.start, offsets.end);
          if (!range) {
            continue;
          }
          painted.add(range);
          resolved.push({ highlight, ...offsets });
        }
        resolvedHighlightsRef.current.set(nodeId, resolved);
        const missing = (segmentContent.node.highlights?.length ?? 0) - resolved.length;
        if (missing > 0) {
          hidden[nodeId] = missing;
        }
      }
    }
    // Nothing resolvable (no content yet, or no Highlight API at all) is not
    // "hidden highlights" — segments without entries keep a quiet footer.
    setHiddenHighlightsByNode((current) => (sameCardTops(current, hidden) ? current : hidden));
    // Keyed on the revisions and highlight-id lists rather than contentByNode
    // identity: a streaming segment's 1s poll replaces the map every second,
    // and repainting would re-walk every settled segment's text nodes each
    // time for content that cannot have moved. Anchors only resolve against
    // revision-bearing (snapshot) content, and DOM swaps inside observed
    // roots arrive via highlightDomNonce.
  }, [
    chainKey,
    chainNodeIds,
    highlightsKey,
    revisionsKey,
    expandedKey,
    fullTraceKey,
    highlightDomNonce,
    segmentRoot,
  ]);

  // Paint the passages that targeted follow-ups (and an in-progress ask) were
  // asked about, and resolve each follow-up's rail offset so its card can sit
  // beside its passage in its own segment. Anchors that no longer locate a
  // passage drop out of the map — their cards fall back to the regular stack.
  useLayoutEffect(() => {
    const api = researchHighlightApi();
    api?.registry.delete(RESEARCH_QUERY_ANCHOR_NAME);
    const tops: Record<string, number> = {};
    let askTop: number | null = null;
    anchoredRangeOffsetsRef.current = [];
    activeAskRangeOffsetsRef.current = null;
    if (api) {
      const painted = new api.Highlight();
      let paintedAny = false;
      for (const nodeId of chainNodeIds) {
        const root = segmentRoot(nodeId);
        const revision = contentByNode[nodeId]?.responseRevision;
        if (!root || !revision) {
          continue;
        }
        const projection = root.textContent ?? "";
        const aside = segmentAside(nodeId);
        const asideTop = aside?.getBoundingClientRect().top ?? 0;
        const entries = anchoredEntries
          .filter((entry) => entry.segmentId === nodeId)
          .map((entry) => ({ id: entry.id, anchor: entry.anchor }));
        if (ask && ask.nodeId === nodeId) {
          entries.push({ id: "__ask__", anchor: ask.anchor });
        }
        for (const { id, anchor } of entries) {
          const offsets = resolveResearchHighlightOffset(projection, revision, {
            id,
            anchor,
            createdAt: 0,
          });
          const range = offsets
            ? rangeForTextOffsets(root, offsets.start, offsets.end)
            : null;
          if (!range) {
            continue;
          }
          painted.add(range);
          paintedAny = true;
          const top = aside
            ? Math.max(0, Math.round(range.getBoundingClientRect().top - asideTop))
            : null;
          if (id === "__ask__") {
            askTop = top;
            activeAskRangeOffsetsRef.current = { segmentId: nodeId, ...offsets! };
          } else {
            anchoredRangeOffsetsRef.current.push({ segmentId: nodeId, id, ...offsets! });
            if (top !== null) {
              tops[id] = top;
            }
          }
        }
      }
      if (paintedAny) {
        api.registry.set(RESEARCH_QUERY_ANCHOR_NAME, painted);
      }
    }
    setAnchoredCardTops((current) => (sameCardTops(current, tops) ? current : tops));
    setAskComposerTop((current) => (current === askTop ? current : askTop));
    return () => {
      api?.registry.delete(RESEARCH_QUERY_ANCHOR_NAME);
    };
    // revisionsKey instead of contentByNode identity for the same reason as
    // the saved-highlight paint above: anchors are immutable and resolve
    // only against revision-bearing content, so a streaming sibling's polls
    // must not force whole-thread re-resolution every second.
  }, [
    anchorLayoutNonce,
    anchoredEntries,
    ask,
    chainKey,
    chainNodeIds,
    revisionsKey,
    expandedKey,
    fullTraceKey,
    highlightDomNonce,
    segmentAside,
    segmentRoot,
  ]);

  // Repaint regions where annotations stack — saved highlights over each
  // other, or a follow-up's query anchor over a saved highlight — in the
  // near-text overlap tone. All base layers share one wash, so without this
  // pass stacked coverage would be invisible. Runs
  // after both base-paint effects in the same commit and re-runs on the
  // union of their dependencies, so the offset refs it reads are current.
  useLayoutEffect(() => {
    const api = researchHighlightApi();
    api?.registry.delete(RESEARCH_OVERLAP_NAME);
    if (!api) {
      return;
    }
    const painted = new api.Highlight();
    painted.priority = RESEARCH_OVERLAP_PRIORITY;
    let paintedAny = false;
    for (const nodeId of chainNodeIds) {
      const root = segmentRoot(nodeId);
      if (!root) {
        continue;
      }
      const ranges = [
        ...(resolvedHighlightsRef.current.get(nodeId) ?? []),
        ...anchoredRangeOffsetsRef.current.filter(
          (entry) => entry.segmentId === nodeId,
        ),
      ].map(({ start, end }) => ({ start, end }));
      for (const region of overlappingResearchHighlightRegions(ranges)) {
        const range = rangeForTextOffsets(root, region.start, region.end);
        if (!range) {
          continue;
        }
        painted.add(range);
        paintedAny = true;
      }
    }
    if (paintedAny) {
      api.registry.set(RESEARCH_OVERLAP_NAME, painted);
    }
    return () => {
      api.registry.delete(RESEARCH_OVERLAP_NAME);
    };
  }, [
    anchoredEntries,
    ask,
    chainKey,
    chainNodeIds,
    highlightsKey,
    revisionsKey,
    expandedKey,
    fullTraceKey,
    highlightDomNonce,
    segmentAside,
    segmentRoot,
  ]);

  // Measure every resolved anchor's connector: from its segment's answer
  // column at the passage's first line to the left edge of its follow-up
  // card. Depends on resolvedCardTops so each elbow lands on the card's
  // settled (collision-resolved) placement.
  useLayoutEffect(() => {
    const next: SegmentAnchorConnector[] = [];
    for (const nodeId of chainNodeIds) {
      const root = segmentRoot(nodeId);
      const grid = segmentGrid(nodeId);
      const aside = segmentAside(nodeId);
      if (!root || !grid || !aside) {
        continue;
      }
      const gridRect = grid.getBoundingClientRect();
      const sx = Math.round(root.getBoundingClientRect().right - gridRect.left) + 8;
      // First pass: resolve each connector's raw endpoints.
      const geometry = anchoredRangeOffsetsRef.current
        .filter((entry) => entry.segmentId === nodeId)
        .flatMap((entry) => {
          const range = rangeForTextOffsets(root, entry.start, entry.end);
          const card = aside.querySelector<HTMLElement>(
            `.research-followup-card.is-anchored[data-node-id="${CSS.escape(entry.id)}"]`,
          );
          const firstLine = Array.from(range?.getClientRects() ?? []).find(
            (rect) => rect.width > 0,
          );
          if (!card || !firstLine) {
            return [];
          }
          const cardRect = card.getBoundingClientRect();
          const sy = Math.round(firstLine.top + firstLine.height / 2 - gridRect.top);
          const ex = Math.round(cardRect.left - gridRect.left) - 6;
          const ey = Math.round(cardRect.top - gridRect.top) + 17;
          return [{ id: entry.id, sx, sy, ex, ey }];
        });
      // Second pass: connectors share the same gutter, so their vertical runs
      // would stack on one x line. Assign each run the lowest lane not taken by
      // an overlapping neighbour (greedy interval colouring, top-to-bottom), then
      // fan lanes leftward into the gutter — capped per-run at a fraction of its
      // span so a run never crowds the passage edge.
      const lanes: number[] = [];
      const laneByIndex = new Map<number, number>();
      geometry
        .map((g, index) => ({ index, top: Math.min(g.sy, g.ey), bottom: Math.max(g.sy, g.ey) }))
        .sort((a, b) => a.top - b.top || a.index - b.index)
        .forEach(({ index, top, bottom }) => {
          let lane = lanes.findIndex((occupiedUntil) => occupiedUntil <= top);
          if (lane === -1) {
            lane = lanes.length;
          }
          lanes[lane] = bottom;
          laneByIndex.set(index, lane);
        });
      for (const [index, g] of geometry.entries()) {
        const lane = laneByIndex.get(index) ?? 0;
        const maxOffset = CONNECTOR_STAGGER_FRACTION * (g.ex - g.sx);
        const offset = Math.min(lane * CONNECTOR_STAGGER_STEP, maxOffset);
        const midX = Math.round((g.sx + g.ex) / 2 - offset);
        next.push({
          segmentId: nodeId,
          id: g.id,
          d: connectorElbowPath(g.sx, g.sy, g.ex, g.ey, midX),
          x: g.sx,
          y: g.sy,
        });
      }
    }
    setAnchorConnectors(next);
  }, [anchorLayoutNonce, anchoredCardTops, chainNodeIds, highlightDomNonce, resolvedCardTops, segmentAside, segmentGrid, segmentRoot]);

  // A selection that lands on saved highlights repaints those annotations in
  // the standard selection tone: painted above the saved-highlight layer (via
  // priority) so the gold annotation tone cannot win over the selection,
  // keeping a selected highlight the same color as any other selected text.
  const selectedHighlightKey = (highlightAction?.highlightIds ?? []).join("\n");
  const highlightActionNodeId = highlightAction?.nodeId ?? null;
  useLayoutEffect(() => {
    const painted = selectedHighlightPaintRef.current;
    painted?.clear();
    const root = highlightActionNodeId ? segmentRoot(highlightActionNodeId) : null;
    if (!painted || !root || !selectedHighlightKey || !highlightActionNodeId) {
      return;
    }
    const selectedIds = selectedHighlightKey.split("\n");
    for (const { highlight, start, end } of resolvedHighlightsRef.current.get(
      highlightActionNodeId,
    ) ?? []) {
      if (!selectedIds.includes(highlight.id)) {
        continue;
      }
      const range = rangeForTextOffsets(root, start, end);
      if (!range) {
        continue;
      }
      painted.add(range);
    }
  }, [highlightActionNodeId, highlightDomNonce, segmentRoot, selectedHighlightKey]);

  // One-pass collision resolution for anchored cards, per segment rail:
  // place them in desired-top order, each no higher than the previous card's
  // bottom plus a gap. Runs as a layout effect in the commit that rendered
  // the cards, so their heights are measurable and the corrected placements
  // land before paint. Heights depend only on card content — never on the
  // tops this effect assigns — so a single pass settles the layout without
  // feedback.
  useLayoutEffect(() => {
    const next: Record<string, number> = {};
    const segmentByChild = new Map<string, string>();
    for (const entry of anchoredEntries) {
      segmentByChild.set(entry.id, entry.segmentId);
    }
    const bySegment = new Map<string, [string, number][]>();
    for (const [childId, top] of Object.entries(anchoredCardTops)) {
      const segmentId = segmentByChild.get(childId);
      if (!segmentId) {
        continue;
      }
      const list = bySegment.get(segmentId);
      if (list) {
        list.push([childId, top]);
      } else {
        bySegment.set(segmentId, [[childId, top]]);
      }
    }
    for (const [segmentId, placements] of bySegment) {
      const aside = segmentAside(segmentId);
      if (!aside) {
        continue;
      }
      const heights = new Map<string, number>();
      for (const element of aside.querySelectorAll<HTMLElement>(
        ".research-followup-card.is-anchored",
      )) {
        if (element.dataset.nodeId) {
          heights.set(element.dataset.nodeId, element.offsetHeight);
        }
      }
      let cursor = Number.NEGATIVE_INFINITY;
      placements.sort(
        // Ties break on node id so equal desired tops keep a stable order.
        (a, b) => a[1] - b[1] || a[0].localeCompare(b[0]),
      );
      for (const [childId, desiredTop] of placements) {
        const top = Math.max(desiredTop, cursor);
        next[childId] = top;
        cursor = top + (heights.get(childId) ?? 0) + ANCHORED_CARD_GAP;
      }
    }
    setResolvedCardTops((current) => (sameCardTops(current, next) ? current : next));
    // cardsMeasureKey remeasures when streaming previews change card
    // heights, without re-running (and force-laying-out every rail) on the
    // 4×/s detail replacements that change nothing height-relevant.
  }, [anchorLayoutNonce, anchoredCardTops, anchoredEntries, cardsMeasureKey, segmentAside]);

  // Ask mode: push any cards the docked composer would cover out of the way.
  // The composer itself sits absolutely at askComposerTop inside the ask
  // segment's rail; transforms keep the rail's flow layout untouched, so
  // clearing them animates everything home.
  useLayoutEffect(() => {
    const clearTransforms = (scope: HTMLElement | null) => {
      if (!scope) {
        return;
      }
      for (const element of scope.querySelectorAll<HTMLElement>(".research-followup-card")) {
        element.style.transform = "";
      }
    };
    const composer = followupComposerRef.current;
    const aside = ask ? segmentAside(ask.nodeId) : null;
    if (!ask || askComposerTop === null || !composer || !aside) {
      clearTransforms(contentContainerRef.current);
      return;
    }
    clearTransforms(contentContainerRef.current);
    const targetTop = askComposerTop;
    const clearanceBottom = targetTop + composer.offsetHeight + ASK_COMPOSER_CLEARANCE;
    const cards = aside.querySelector<HTMLElement>(".research-followup-cards");
    if (cards) {
      // The stack keeps its internal spacing: the first card the composer
      // would cover sets one shared shift for itself and everything after it.
      let delta = 0;
      for (const element of Array.from(cards.children) as HTMLElement[]) {
        if (delta === 0 && element.offsetTop + element.offsetHeight > targetTop) {
          delta = Math.max(0, clearanceBottom - element.offsetTop);
        }
        element.style.transform = delta > 0 ? `translateY(${delta}px)` : "";
      }
    }
    for (const element of aside.querySelectorAll<HTMLElement>(
      ".research-followup-card.is-anchored",
    )) {
      const overlaps =
        element.offsetTop < clearanceBottom &&
        element.offsetTop + element.offsetHeight > targetTop;
      element.style.transform = overlaps
        ? `translateY(${clearanceBottom - element.offsetTop}px)`
        : "";
    }
    return () => clearTransforms(aside);
  }, [
    anchorLayoutNonce,
    anchoredCardTops,
    ask,
    askComposerTop,
    followup,
    highlightDomNonce,
    resolvedCardTops,
    segmentAside,
  ]);

  const captureHighlightSelection = useCallback(() => {
    const selection = window.getSelection();
    if (
      !researchHighlightApi() ||
      !selection ||
      selection.isCollapsed ||
      selection.rangeCount === 0
    ) {
      setHighlightAction(null);
      return;
    }
    const range = selection.getRangeAt(0);
    // Resolve which thread segment the selection landed in; a selection that
    // spans segments anchors nowhere.
    const container =
      range.commonAncestorContainer instanceof Element
        ? range.commonAncestorContainer
        : range.commonAncestorContainer.parentElement;
    const root = container?.closest<HTMLElement>(".research-response-content-root") ?? null;
    const nodeId = root?.dataset.nodeId ?? null;
    const segmentContent = nodeId ? contentByNodeRef.current[nodeId] : null;
    const revision = segmentContent?.responseRevision;
    if (!root || !nodeId || !revision) {
      setHighlightAction(null);
      return;
    }
    // Tool calls, thinking, and other non-text rows are transcript machinery,
    // not answer prose: never offer highlight/ask when the selection touches
    // one, whether it sits inside such a row or drags across it.
    if (selectionTouchesNonTextRow(root, range)) {
      setHighlightAction(null);
      return;
    }
    const offsets = selectionOffsets(root, range);
    if (!offsets) {
      setHighlightAction(null);
      return;
    }
    const projection = root.textContent ?? "";
    const exact = projection.slice(offsets.start, offsets.end);
    const resolvedRanges = (resolvedHighlightsRef.current.get(nodeId) ?? []).map(
      ({ highlight, start, end }) => ({
        id: highlight.id,
        start,
        end,
      }),
    );
    const highlightIds = intersectingResearchHighlightIds(offsets, resolvedRanges);
    // Whitespace cannot form a useful new highlight, but it can still be a
    // selected subset of an existing annotation that the user wants removed.
    if (!exact.trim() && highlightIds.length === 0) {
      setHighlightAction(null);
      return;
    }
    // Keep the quote's surrounding context inside the message it belongs to so
    // the anchor resolves the same in either transcript view, and refuse the
    // selections that leaves unanchorable (see researchAnchorContextBounds).
    const contextBounds = researchAnchorContextBounds({
      isConversation: segmentContent?.node.kind === "conversation",
      messageBounds: enclosingMessageFlatBounds(root, range),
      projectionLength: projection.length,
    });
    if (!contextBounds) {
      setHighlightAction(null);
      return;
    }
    const contextFloor = contextBounds.start;
    const contextCeil = contextBounds.end;
    const anchorForOffsets = (span: {
      start: number;
      end: number;
    }): ResearchHighlightAnchor => ({
      version: 1,
      projection: "answer-v1",
      responseRevision: revision,
      start: span.start,
      end: span.end,
      exact: projection.slice(span.start, span.end),
      prefix: textContextSlice(
        projection,
        Math.max(contextFloor, span.start - RESEARCH_HIGHLIGHT_CONTEXT_LENGTH),
        span.start,
      ),
      suffix: textContextSlice(
        projection,
        span.end,
        Math.min(contextCeil, span.end + RESEARCH_HIGHLIGHT_CONTEXT_LENGTH),
      ),
    });
    const expandOffsets = expandedResearchHighlightOffsets(offsets, resolvedRanges);
    const rect = range.getBoundingClientRect();
    setHighlightAction({
      nodeId,
      anchor: anchorForOffsets(offsets),
      highlightIds,
      expandAnchor: expandOffsets ? anchorForOffsets(expandOffsets) : null,
      ...highlightActionPlacement(rect, expandOffsets ? 340 : 260),
    });
  }, []);

  /** Replaces the native character-precise drag range with whole-word
   * endpoints. The raw range is checked first so snapping never makes an
   * otherwise ineligible selection (one crossing tool/thinking machinery)
   * eligible for Highlight/Ask. */
  const applySnappedResearchSelection = useCallback(
    (drag: ResearchSelectionDrag, clientX: number, clientY: number) => {
      if (!drag.snapEligible || drag.anchorOffset === null) {
        return;
      }
      const focusOffset = flatOffsetAtPoint(drag.root, clientX, clientY);
      if (focusOffset === null) {
        return;
      }
      if (focusOffset !== drag.anchorOffset) {
        const rawRange = rangeForTextOffsets(
          drag.root,
          Math.min(drag.anchorOffset, focusOffset),
          Math.max(drag.anchorOffset, focusOffset),
        );
        if (!rawRange || selectionTouchesNonTextRow(drag.root, rawRange)) {
          if (rawRange) {
            applyDirectionalSelectionRange(
              rawRange,
              focusOffset < drag.anchorOffset ? "backward" : "forward",
            );
          }
          return;
        }
      }

      if (drag.snapper === undefined) {
        drag.snapper = createResearchSelectionSnapper(
          drag.root.textContent ?? "",
          document.documentElement.lang || navigator.language,
          messageFlatBoundaries(drag.root),
        );
      }
      const snapped = drag.snapper?.(drag.anchorOffset, focusOffset) ?? null;
      if (!snapped) {
        return;
      }
      const range = rangeForTextOffsets(drag.root, snapped.start, snapped.end);
      if (!range) {
        return;
      }

      // Keep the live focus on the pointer side. Besides making reversals feel
      // native, this means a subsequent keyboard extension continues from the
      // end the reader last moved.
      applyDirectionalSelectionRange(range, snapped.direction);
    },
    [],
  );

  // A mouse selection can begin in the answer and end over the rail or other
  // document chrome, where the response root's mouseup never fires. Remember
  // answer-originated drags and finish them from a document-level fallback;
  // the root handler clears the ref first on ordinary in-column releases so
  // capture still runs exactly once.
  const beginHighlightSelectionDrag = useCallback(
    (event: React.MouseEvent<HTMLDivElement>) => {
      if (event.button !== 0) {
        return;
      }
      const root = event.currentTarget;
      const nodeId = root.dataset.nodeId ?? null;
      const content = nodeId ? contentByNodeRef.current[nodeId] : null;
      const target = event.target instanceof Element ? event.target : null;
      const anchorOffset = flatOffsetAtPoint(root, event.clientX, event.clientY);
      selectionDragRef.current = {
        root,
        anchorOffset,
        originX: event.clientX,
        originY: event.clientY,
        lastX: event.clientX,
        lastY: event.clientY,
        active: false,
        // Turn membership is deliberately not part of this test: the snapper
        // splits its word units at every message seam, so it moves each
        // endpoint by at most a partial word *within one turn* and can neither
        // carry a selection across a turn boundary nor pull a cross-turn one
        // back inside a single turn. The eligibility class is preserved either
        // way.
        snapEligible: Boolean(
          researchHighlightApi() &&
            nodeId &&
            content?.responseRevision &&
            anchorOffset !== null &&
            !target?.closest(NON_TEXT_ROW_SELECTOR)
        ),
        snapper: undefined,
        frame: null,
      };
    },
    [],
  );
  const finishHighlightSelectionDrag = useCallback(
    (event: { clientX: number; clientY: number }) => {
      const drag = selectionDragRef.current;
      if (!drag) {
        return;
      }
      selectionDragRef.current = null;
      if (drag.frame !== null) {
        window.cancelAnimationFrame(drag.frame);
        drag.frame = null;
      }
      if (drag.active) {
        applySnappedResearchSelection(drag, event.clientX, event.clientY);
      }
      captureHighlightSelection();
    },
    [applySnappedResearchSelection, captureHighlightSelection],
  );
  useEffect(() => {
    const cancelDrag = () => {
      const drag = selectionDragRef.current;
      selectionDragRef.current = null;
      if (drag?.frame !== null && drag?.frame !== undefined) {
        window.cancelAnimationFrame(drag.frame);
      }
    };
    const updateDrag = (event: MouseEvent) => {
      const drag = selectionDragRef.current;
      if (!drag) {
        return;
      }
      drag.lastX = event.clientX;
      drag.lastY = event.clientY;
      if (
        !drag.active &&
        Math.hypot(event.clientX - drag.originX, event.clientY - drag.originY) >=
          RESEARCH_SELECTION_DRAG_THRESHOLD
      ) {
        drag.active = true;
      }
      if (!drag.active || !drag.snapEligible || drag.frame !== null) {
        return;
      }
      // Run after WebKit's native selection update for this mousemove. We do
      // not prevent the default, so dragging beyond the viewport keeps native
      // selection autoscroll.
      drag.frame = window.requestAnimationFrame(() => {
        drag.frame = null;
        if (selectionDragRef.current === drag) {
          applySnappedResearchSelection(drag, drag.lastX, drag.lastY);
        }
      });
    };
    document.addEventListener("mousemove", updateDrag);
    document.addEventListener("mouseup", finishHighlightSelectionDrag);
    window.addEventListener("blur", cancelDrag);
    return () => {
      document.removeEventListener("mousemove", updateDrag);
      document.removeEventListener("mouseup", finishHighlightSelectionDrag);
      window.removeEventListener("blur", cancelDrag);
      cancelDrag();
    };
  }, [applySnappedResearchSelection, finishHighlightSelectionDrag]);

  // A plain click on a painted highlight or ask passage selects the whole
  // annotation, which opens the action bar through the ordinary selection flow.
  // Runs on click, after the mouseup capture has already dismissed the bar for
  // a collapsed selection; CSS highlights have no DOM nodes, so the click is
  // hit-tested against the owning segment's resolved flat-offset ranges.
  const selectAnnotationAtPoint = useCallback(
    (event: React.MouseEvent<HTMLDivElement>) => {
      const root = event.currentTarget;
      const nodeId = root.dataset.nodeId;
      const selection = window.getSelection();
      if (!root || !nodeId || !selection || !selection.isCollapsed) {
        return;
      }
      // Links keep their own click behavior; popping the bar under a
      // navigation would be noise.
      if (event.target instanceof Element && event.target.closest("a")) {
        return;
      }
      const offset = flatOffsetAtPoint(root, event.clientX, event.clientY);
      if (offset === null) {
        return;
      }
      const activeAsk = activeAskRangeOffsetsRef.current;
      const resolved = [
        ...anchoredRangeOffsetsRef.current.filter((entry) => entry.segmentId === nodeId),
        ...(activeAsk?.segmentId === nodeId ? [activeAsk] : []),
        ...(resolvedHighlightsRef.current.get(nodeId) ?? []),
      ].find(
        ({ start, end }) => offset >= start && offset < end,
      );
      if (!resolved) {
        return;
      }
      const range = rangeForTextOffsets(root, resolved.start, resolved.end);
      if (!range) {
        return;
      }
      selection.removeAllRanges();
      selection.addRange(range);
      captureHighlightSelection();
    },
    [captureHighlightSelection],
  );

  // Passage-side hover behavior: hit-test the pointer against the segment's
  // resolved saved-highlight and query-anchor ranges (rAF-throttled — the
  // walk is cheap but not free). Saved highlights get a pointer cursor to
  // advertise their click behavior; query anchors link to the follow-up card
  // they produced.
  const linkAnchorUnderPointer = useCallback((event: React.MouseEvent<HTMLDivElement>) => {
    // Keep replacing the queued sample while a frame is pending so the hit
    // test observes the latest pointer position instead of lagging one frame
    // behind at highlight boundaries.
    anchorHoverPointerRef.current = {
      root: event.currentTarget,
      nodeId: event.currentTarget.dataset.nodeId ?? null,
      clientX: event.clientX,
      clientY: event.clientY,
    };
    if (anchorHoverFrameRef.current !== null) {
      return;
    }
    anchorHoverFrameRef.current = window.requestAnimationFrame(() => {
      anchorHoverFrameRef.current = null;
      const sample = anchorHoverPointerRef.current;
      anchorHoverPointerRef.current = null;
      if (!sample) {
        return;
      }
      const { root, nodeId, clientX, clientY } = sample;
      if (!nodeId || !root.isConnected) {
        return;
      }
      const segmentAnchors = anchoredRangeOffsetsRef.current.filter(
        (entry) => entry.segmentId === nodeId,
      );
      const segmentHighlights = resolvedHighlightsRef.current.get(nodeId) ?? [];
      const offset =
        segmentAnchors.length > 0 || segmentHighlights.length > 0
          ? flatOffsetAtPoint(root, clientX, clientY)
          : null;
      const overHighlight =
        offset !== null &&
        segmentHighlights.some(({ start, end }) => offset >= start && offset < end);
      const id =
        offset === null
          ? null
          : segmentAnchors.find(({ start, end }) => offset >= start && offset < end)?.id ??
            null;
      setPointerHighlightNodeId((current) => {
        const next = overHighlight ? nodeId : null;
        return current === next ? current : next;
      });
      setLinkedAnchorNodeId((current) => (current === id ? current : id));
    });
  }, []);

  const unlinkAnchorPointer = useCallback(() => {
    if (anchorHoverFrameRef.current !== null) {
      window.cancelAnimationFrame(anchorHoverFrameRef.current);
      anchorHoverFrameRef.current = null;
    }
    anchorHoverPointerRef.current = null;
    setPointerHighlightNodeId(null);
    setLinkedAnchorNodeId(null);
  }, []);

  // One updater for every optimistic highlight mutation: replaces a cached
  // segment's highlight list, leaving unrelated segments' identities alone.
  const patchNodeHighlights = useCallback(
    (
      nodeId: string,
      transform: (highlights: ResearchHighlight[]) => ResearchHighlight[],
    ) => {
      setContentByNode((current) => {
        const entry = current[nodeId];
        if (!entry) {
          return current;
        }
        return {
          ...current,
          [nodeId]: {
            ...entry,
            node: {
              ...entry.node,
              highlights: transform(entry.node.highlights ?? []),
            },
          },
        };
      });
    },
    [],
  );

  const applyHighlightAction = useCallback(async () => {
    if (!highlightAction || savingHighlight) {
      return;
    }
    const targetNodeId = highlightAction.nodeId;
    if (!contentByNodeRef.current[targetNodeId]) {
      return;
    }
    setSavingHighlight(true);
    try {
      if (highlightAction.highlightIds.length > 0) {
        const removed = await removeResearchHighlights(
          targetNodeId,
          highlightAction.highlightIds,
        );
        const removedIds = new Set(removed.map(({ id }) => id));
        patchNodeHighlights(targetNodeId, (highlights) =>
          highlights.filter(({ id }) => !removedIds.has(id)),
        );
      } else {
        const created = await createResearchHighlight(targetNodeId, highlightAction.anchor);
        patchNodeHighlights(targetNodeId, (highlights) => [...highlights, created]);
      }
      selectedHighlightPaintRef.current?.clear();
      window.getSelection()?.removeAllRanges();
      setHighlightAction(null);
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    } finally {
      setSavingHighlight(false);
    }
  }, [highlightAction, onError, patchNodeHighlights, savingHighlight]);

  // Expand: save the merged annotation, then retire the highlights it
  // absorbed. Creation goes first — if removal then fails, the leftover is
  // overlapping highlights, not a lost annotation.
  const applyExpandHighlightAction = useCallback(async () => {
    if (!highlightAction?.expandAnchor || savingHighlight) {
      return;
    }
    const targetNodeId = highlightAction.nodeId;
    if (!contentByNodeRef.current[targetNodeId]) {
      return;
    }
    setSavingHighlight(true);
    try {
      const created = await createResearchHighlight(
        targetNodeId,
        highlightAction.expandAnchor,
      );
      const removed =
        highlightAction.highlightIds.length > 0
          ? await removeResearchHighlights(targetNodeId, highlightAction.highlightIds)
          : [];
      const removedIds = new Set(removed.map(({ id }) => id));
      patchNodeHighlights(targetNodeId, (highlights) => [
        ...highlights.filter(({ id }) => !removedIds.has(id)),
        created,
      ]);
      selectedHighlightPaintRef.current?.clear();
      window.getSelection()?.removeAllRanges();
      setHighlightAction(null);
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    } finally {
      setSavingHighlight(false);
    }
  }, [highlightAction, onError, patchNodeHighlights, savingHighlight]);

  // The selection can back a targeted follow-up only while its segment can
  // actually take one: that node finished, and the tree accepts branches. A
  // targeted ask is always a branch — never an inline continuation — so the
  // tail's inline slot is irrelevant here.
  const highlightActionNode = highlightAction
    ? detail?.nodes.find((node) => node.id === highlightAction.nodeId) ?? null
    : null;
  const canAskSelection = Boolean(
    highlightAction?.anchor.exact.trim() &&
      !archived &&
      highlightActionNode?.status === "complete",
  );

  const enterAskMode = useCallback(() => {
    if (!highlightAction?.anchor.exact.trim() || archived) {
      return;
    }
    const node = detailRef.current?.nodes.find(
      (candidate) => candidate.id === highlightAction.nodeId,
    );
    if (node?.status !== "complete") {
      return;
    }
    const currentTreeId = treeIdRef.current;
    const navigation = currentTreeId ? navigationRef.current[currentTreeId] : undefined;
    if (navigation && recordResearchFollowupDraft(navigation, "", followupMode)) {
      saveResearchNavigation();
    }
    setAsk({ nodeId: highlightAction.nodeId, anchor: highlightAction.anchor });
    setHighlightAction(null);
    window.getSelection()?.removeAllRanges();
    // Focus once the composer has mounted beside the quoted passage so typing
    // can start immediately. preventScroll matters: the passage — right where
    // the user just selected — is already in view, and a scrolling focus
    // would jump the page.
    window.requestAnimationFrame(() =>
      followupTextareaRef.current?.focus({ preventScroll: true }),
    );
  }, [archived, followupMode, highlightAction]);

  // Removes the persisted ask for a node. Called from the explicit exits
  // (submit, Escape, the quote row's X) — the persist effect below never
  // deletes, so a lifecycle reset of `ask` cannot wipe an ask the user still
  // wants back after a remount.
  const clearSavedAsk = useCallback((nodeId: string | null) => {
    const currentTreeId = treeIdRef.current;
    if (!currentTreeId || !nodeId) {
      return;
    }
    const askByNode = navigationRef.current[currentTreeId]?.askByNode;
    if (askByNode?.[nodeId]) {
      delete askByNode[nodeId];
      saveResearchNavigation();
    }
  }, []);

  const dismissAsk = useCallback(() => {
    clearSavedAsk(askRef.current?.nodeId ?? null);
    setAsk(null);
  }, [clearSavedAsk]);

  // Mirror the ordinary follow-up composer just like targeted asks. Store
  // mutation is immediate so the unmount cleanup can flush it even if the
  // 250ms localStorage debounce has not fired.
  useEffect(() => {
    if (!treeId || ask) {
      return;
    }
    const restoring = restoringFollowupDraftRef.current;
    if (restoring?.treeId === treeId) {
      if (followup !== restoring.text || followupMode !== restoring.mode) {
        return;
      }
      restoringFollowupDraftRef.current = null;
    }
    const navigation = (navigationRef.current[treeId] ??= { scrollByNode: {} });
    if (!recordResearchFollowupDraft(navigation, followup, followupMode)) {
      return;
    }
    if (navigationPersistTimerRef.current !== null) {
      window.clearTimeout(navigationPersistTimerRef.current);
    }
    navigationPersistTimerRef.current = window.setTimeout(() => {
      navigationPersistTimerRef.current = null;
      saveResearchNavigation();
    }, 250);
  }, [ask, followup, followupMode, treeId]);

  // Ask mode paints its quoted passage with the CSS Highlight API rather than
  // retaining the browser selection. Once an empty composer is open, a click
  // that leaves no live passage selection means the reader clicked away from
  // that transient ask and closes it. Run on `click` (after response-root
  // handlers) so clicking the painted passage can first select it and keep the
  // composer. Composer controls and the floating selection actions are exempt:
  // in particular, clicking Ask there may be retargeting this composer.
  const emptyAskOpen = Boolean(ask && !followup.trim());
  useEffect(() => {
    if (!emptyAskOpen) {
      return;
    }
    const closeEmptyAskOnClickAway = (event: MouseEvent) => {
      const target = event.target instanceof Element ? event.target : null;
      const selection = window.getSelection();
      if (
        shouldDismissEmptyResearchAskOnClick({
          followup,
          selectionCollapsed: !selection || selection.isCollapsed,
          insideComposer: Boolean(target?.closest(".research-followup-composer")),
          insideSelectionActions: Boolean(target?.closest(".research-highlight-actions")),
        })
      ) {
        dismissAsk();
      }
    };
    document.addEventListener("click", closeEmptyAskOnClickAway);
    return () => document.removeEventListener("click", closeEmptyAskOnClickAway);
  }, [dismissAsk, emptyAskOpen, followup]);

  // Mirror the in-progress ask into the navigation store so tabbing away from
  // the research surface (which unmounts this document) keeps it. The store
  // mutation is immediate; the localStorage write shares the scroll debounce,
  // and the unmount flush picks up anything still pending. The content guard
  // skips the transient render after a page switch, where an old anchor could
  // still be committed alongside a not-yet-loaded segment.
  const askContentLoaded = ask ? Boolean(contentByNode[ask.nodeId]) : false;
  useEffect(() => {
    if (!ask || !treeId || !askContentLoaded) {
      return;
    }
    const navigation = (navigationRef.current[treeId] ??= { scrollByNode: {} });
    (navigation.askByNode ??= {})[ask.nodeId] = {
      anchor: ask.anchor,
      text: followup,
      updatedAt: Date.now(),
    };
    if (navigationPersistTimerRef.current !== null) {
      window.clearTimeout(navigationPersistTimerRef.current);
    }
    navigationPersistTimerRef.current = window.setTimeout(() => {
      navigationPersistTimerRef.current = null;
      saveResearchNavigation();
    }, 250);
  }, [ask, askContentLoaded, followup, treeId]);

  // Keyed on the bar's existence, not the action object: repositioning below
  // replaces the object on every scroll frame, and rebinding all of these
  // listeners each frame would be pure churn.
  const hasHighlightAction = Boolean(highlightAction);
  useEffect(() => {
    if (!hasHighlightAction) {
      return;
    }
    const dismiss = (event: MouseEvent) => {
      if (!(event.target instanceof Element) || !event.target.closest(".research-highlight-actions")) {
        setHighlightAction(null);
      }
    };
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setHighlightAction(null);
        return;
      }
      if (savingHighlight || isEditableTarget(event.target)) {
        return;
      }
      if (isResearchAskActionShortcut(event)) {
        event.preventDefault();
        event.stopPropagation();
        event.stopImmediatePropagation();
        enterAskMode();
        return;
      }
      if (isResearchExpandActionShortcut(event)) {
        event.preventDefault();
        event.stopPropagation();
        event.stopImmediatePropagation();
        void applyExpandHighlightAction();
        return;
      }
      if (!isResearchHighlightActionShortcut(event)) {
        return;
      }
      event.preventDefault();
      event.stopPropagation();
      event.stopImmediatePropagation();
      void applyHighlightAction();
    };
    // The selection stays valid across scrolls and resizes, so follow it
    // instead of dismissing: recompute the bar's placement from the live
    // selection rect (rAF-throttled), and drop the bar only if the selection
    // itself has gone away.
    let repositionFrame: number | null = null;
    const reposition = () => {
      if (repositionFrame !== null) {
        return;
      }
      repositionFrame = window.requestAnimationFrame(() => {
        repositionFrame = null;
        const selection = window.getSelection();
        if (!selection || selection.rangeCount === 0 || selection.isCollapsed) {
          setHighlightAction(null);
          return;
        }
        const rect = selection.getRangeAt(0).getBoundingClientRect();
        setHighlightAction((current) => {
          if (!current) {
            return current;
          }
          const placement = highlightActionPlacement(
            rect,
            current.expandAnchor ? 340 : 260,
          );
          return current.left !== placement.left ||
            current.top !== placement.top ||
            current.offscreen !== placement.offscreen
            ? { ...current, ...placement }
            : current;
        });
      });
    };
    document.addEventListener("mousedown", dismiss);
    document.addEventListener("keydown", handleKeyDown);
    window.addEventListener("resize", reposition);
    window.addEventListener("scroll", reposition, true);
    return () => {
      document.removeEventListener("mousedown", dismiss);
      document.removeEventListener("keydown", handleKeyDown);
      window.removeEventListener("resize", reposition);
      window.removeEventListener("scroll", reposition, true);
      if (repositionFrame !== null) {
        window.cancelAnimationFrame(repositionFrame);
      }
    };
  }, [
    applyExpandHighlightAction,
    applyHighlightAction,
    enterAskMode,
    hasHighlightAction,
    savingHighlight,
    segmentRoot,
  ]);

  // Escape leaves ask mode from anywhere, including inside the composer's
  // textarea, returning the composer to the thread's tail.
  useEffect(() => {
    if (!ask) {
      return;
    }
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !event.defaultPrevented) {
        event.preventDefault();
        dismissAsk();
      }
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [ask, dismissAsk]);

  // Scroll to a just-submitted inline follow-up once the refreshed detail
  // delivers it into the chain.
  useEffect(() => {
    const target = pendingScrollNodeIdRef.current;
    if (!target || !chainNodeIds.includes(target)) {
      return;
    }
    pendingScrollNodeIdRef.current = null;
    window.requestAnimationFrame(() => scrollToSegment(target));
  }, [chainKey, chainNodeIds, scrollToSegment]);

  async function submitFollowup() {
    const prompt = followup.trim();
    if (!prompt || submitting || archived || !detail) {
      return;
    }
    // Ask mode targets the segment the passage was selected in and is always
    // a branch; thread mode continues from the tail; branch mode branches
    // from the last completed answer — the same targeting the composer's
    // enabled state was derived from.
    const askState = ask;
    const target = askState
      ? detail.nodes.find((node) => node.id === askState.nodeId) ?? null
      : followupMode === "branch"
        ? ([...chainNodes].reverse().find((node) => node.status === "complete") ?? null)
        : tailNode;
    // Mirrors the submit button's disabled conditions (same canFollowUpFrom
    // / canContinueThread predicates): Cmd+Enter must not reach the backend
    // (and bounce with an error) from a state the button presents as
    // unavailable.
    if (!target || !canFollowUpFrom(target)) {
      return;
    }
    const inline = !askState && followupMode === "thread";
    if (inline && !canContinueThread(detail.nodes, target)) {
      return;
    }
    setSubmitting(true);
    try {
      const child = await onFork(target.id, prompt, null, askState?.anchor ?? null, inline);
      const navigation = navigationRef.current[detail.tree.id];
      if (navigation) {
        recordResearchFollowupDraft(navigation, "", followupMode);
      }
      setFollowup("");
      if (askState) {
        clearSavedAsk(askState.nodeId);
        setAsk(null);
      }
      if (inline) {
        // The reader stays put — the thread is one page. The new segment
        // appears at the tail when the refreshed detail lands; bring it into
        // view then.
        pendingScrollNodeIdRef.current = child.id;
      }
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    } finally {
      setSubmitting(false);
    }
  }

  // One-click recovery for a failed (or cancelled) inline tail: relaunch the
  // node in place — same id, same question, same slot — through the retry
  // command. The reader stays put; the segment re-renders as Queued when the
  // refreshed detail lands.
  function retryInlineTail() {
    const tail = tailNode;
    if (
      !detail ||
      !tail ||
      !tail.inline ||
      !tail.parentNodeId ||
      tail.paneId ||
      retryingNodeId !== null ||
      submitting ||
      archived ||
      (tail.status !== "failed" && tail.status !== "cancelled")
    ) {
      return;
    }
    handleRetryNode(tail.id);
  }

  async function resolveProposal(
    proposal: PublicationProposal,
    status: "accepted" | "declined",
  ) {
    const binding = publicationBinding;
    if (!binding || proposalActionId !== null) {
      return;
    }
    if (status === "accepted" && (!proposal.parentNodeId || archived)) {
      setProposalError(
        archived
          ? "Restore this research before accepting contributed follow-ups."
          : "This proposal targets a result that is no longer available locally.",
      );
      return;
    }
    setProposalActionId(proposal.commentId);
    setProposalError(null);
    try {
      let localNodeId: string | null = null;
      if (status === "accepted") {
        localNodeId =
          proposal.localNodeId ??
          proposalRetryNodeIds[proposal.commentId] ??
          null;
        if (!localNodeId) {
          // An anchored proposal carries the passage it was asked about;
          // rebind it to the parent's current response revision so the new
          // node's card sits beside that passage. Offsets came from the
          // public page's rendered text, so the exact/prefix/suffix
          // relocation does the real work here.
          const parentRevision = proposal.parentNodeId
            ? contentByNode[proposal.parentNodeId]?.responseRevision ?? null
            : null;
          const child = await onFork(
            proposal.parentNodeId!,
            proposal.prompt,
            {
              publicationId: binding.publicationId,
              commentId: proposal.commentId,
            },
            proposal.anchor && parentRevision
              ? {
                  version: 1,
                  projection: "answer-v1",
                  responseRevision: parentRevision,
                  start: proposal.anchor.start,
                  end: proposal.anchor.end,
                  exact: proposal.anchor.exact,
                  prefix: proposal.anchor.prefix,
                  suffix: proposal.anchor.suffix,
                }
              : null,
          );
          localNodeId = child.id;
          setProposalRetryNodeIds((current) => ({
            ...current,
            [proposal.commentId]: child.id,
          }));
        }
      }
      const nextBinding = await resolvePublicationProposal({
        publicationId: binding.publicationId,
        proposalCommentId: proposal.commentId,
        status,
        localNodeId,
      });
      onPublicationBindingChange(nextBinding);
      setProposalRetryNodeIds((current) => {
        const next = { ...current };
        delete next[proposal.commentId];
        return next;
      });
      await refreshPublicationProposals();
      onToast(status === "accepted" ? "Proposal accepted" : "Proposal declined");
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      setProposalError(message);
      onError(message);
    } finally {
      setProposalActionId(null);
    }
  }

  // Prefer the event-driven node over the last content fetch for metadata:
  // detail updates arrive without reparsing the transcript. The chrome
  // (breadcrumb, header actions, proposals) renders from this alone, so
  // switching pages no longer blanks the whole document while content loads —
  // only each segment's response section waits for its fetch.
  const displayNode = selectedDetailNode ?? null;
  const displayPublicNodeId = displayNode
    ? publicationBinding?.publicNodeIds[displayNode.id] ?? null
    : null;
  const visiblePublicationProposals = publicationProposals.filter(
    (proposal) =>
      proposal.parentNodeId === displayNode?.id ||
      proposal.parentPublicNodeId === displayPublicNodeId,
  );

  const anyChainRunActive = chainNodes.some((node) => isActiveResearchStatus(node.status));
  useEffect(() => {
    setMetadataNow(Date.now());
    if (!anyChainRunActive) {
      return;
    }
    const timer = window.setInterval(() => setMetadataNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [anyChainRunActive, chainKey]);

  // Stable handler surface for the memoized segments: App-provided props and
  // navigation-dependent callbacks route through refs so their changing
  // identities cannot defeat the segment comparator.
  const onCancelRef = useRef(onCancel);
  onCancelRef.current = onCancel;
  const onRetryNodeRef = useRef(onRetryNode);
  onRetryNodeRef.current = onRetryNode;
  const onOpenPaneRef = useRef(onOpenPane);
  onOpenPaneRef.current = onOpenPane;
  const onToastRef = useRef(onToast);
  onToastRef.current = onToast;
  const onErrorRef = useRef(onError);
  onErrorRef.current = onError;
  const handleSelectNode = useCallback((nodeId: string) => {
    setOpenedFollowupIds((prev) =>
      prev.has(nodeId) ? prev : new Set(prev).add(nodeId),
    );
    selectNodeRef.current(nodeId);
  }, []);
  const handleOpenPane = useCallback((paneId: string) => onOpenPaneRef.current(paneId), []);
  const handleCancelNode = useCallback((nodeId: string) => {
    setCancelling(true);
    onCancelRef.current(nodeId)
      .catch((err) => onErrorRef.current(err instanceof Error ? err.message : String(err)))
      .finally(() => setCancelling(false));
  }, []);
  // Relaunches a settled node in place. Ref-guarded against double entry: the
  // clicked control disables itself, but a second control for the same node
  // (the follow-up menu beside the inline button) fires before the state
  // lands. The backend refuses a duplicate anyway — the reset finds the node
  // already Queued — so this only spares the user a bounced error.
  const retryRequestInFlightRef = useRef(false);
  const handleRetryNode = useCallback((nodeId: string) => {
    if (retryRequestInFlightRef.current) {
      return;
    }
    retryRequestInFlightRef.current = true;
    setRetryingNodeId(nodeId);
    onRetryNodeRef.current(nodeId)
      .catch((err) => onErrorRef.current(err instanceof Error ? err.message : String(err)))
      .finally(() => {
        retryRequestInFlightRef.current = false;
        setRetryingNodeId((current) => (current === nodeId ? null : current));
      });
  }, []);
  const retryContentLoad = useCallback(() => setContentLoadNonce((value) => value + 1), []);
  const showFullTraceFor = useCallback(
    (nodeId: string) =>
      setFullTraceNodes((current) =>
        current[nodeId] ? current : { ...current, [nodeId]: true },
      ),
    [],
  );
  const handleCardHover = useCallback((childId: string, entering: boolean) => {
    setLinkedAnchorNodeId(
      entering ? childId : (current) => (current === childId ? null : current),
    );
  }, []);

  const copyAnswer = useCallback(async (view: SegmentView) => {
    const text = view.conversationCopyText ?? view.rawAnswer;
    if (!text) {
      return;
    }
    try {
      await writeClipboardText(text);
      onToastRef.current(
        view.conversationCopyText ? "Copied conversation" : "Copied research answer",
      );
    } catch {
      onToastRef.current("Couldn’t copy the research answer", "warning");
    }
  }, []);
  const handleCopyAnswer = useCallback(
    (view: SegmentView) => void copyAnswer(view),
    [copyAnswer],
  );

  // The whole thread as one Markdown document: each turn's question and
  // answer in order, separated by rules. Document and conversation heads
  // have no question line — their content opens the document.
  async function copyThread() {
    const parts: string[] = [];
    for (const [index, view] of segmentViews.entries()) {
      const chainNode = chainNodes[index];
      const body = (view.conversationCopyText ?? view.rawAnswer).trim();
      const prompt =
        view.isDocument || view.isConversation ? null : chainNode?.prompt.trim() || null;
      if (!prompt && !body) {
        continue;
      }
      parts.push(
        [prompt ? `**Question:** ${prompt}` : null, body || "_No response available._"]
          .filter(Boolean)
          .join("\n\n"),
      );
    }
    if (parts.length === 0) {
      return;
    }
    try {
      await writeClipboardText(parts.join("\n\n---\n\n"));
      onToastRef.current("Copied thread");
    } catch {
      onToastRef.current("Couldn’t copy the thread", "warning");
    }
  }

  async function saveDocumentEdit(input: { markdown: string; title: string | null }) {
    if (!documentEditSession) {
      throw new Error("The document is not available for editing.");
    }
    const result = await onUpdateDocument({
      nodeId: documentEditSession.nodeId,
      markdown: input.markdown,
      title: input.title,
      expectedResponseRevision: documentEditSession.responseRevision,
      expectedTitle: documentEditSession.title,
      expectedHighlightIds: documentEditSession.highlightIds,
    });
    if (treeIdRef.current === result.tree.id && result.markdownChanged) {
      // Do not leave the old revision visible after the modal closes. The
      // existing loader refetches the atomically replaced snapshot.
      const editedNodeId = documentEditSession.nodeId;
      setContentByNode((current) => withoutKeys(current, [editedNodeId]));
      fetchStampByNodeRef.current.delete(editedNodeId);
      setContentLoadNonce((value) => value + 1);
    }
    if (result.removedHighlightCount > 0) {
      onToast(
        `Document updated · ${result.removedHighlightCount.toLocaleString()} highlight${
          result.removedHighlightCount === 1 ? "" : "s"
        } removed`,
      );
    } else {
      onToast("Document updated");
    }
  }

  // Keep every hook above the loading return below. The initial detail render
  // has no selected display node; selection is restored by an effect, so a
  // hook below that return would appear only on the next render and violate
  // React's hook ordering. Grouping here also preserves connector-list
  // identities across unrelated parent renders for the memoized segments.
  const connectorsBySegment = useMemo(() => {
    const map = new Map<string, SegmentAnchorConnector[]>();
    for (const connector of anchorConnectors) {
      const list = map.get(connector.segmentId);
      if (list) {
        list.push(connector);
      } else {
        map.set(connector.segmentId, [connector]);
      }
    }
    return map;
  }, [anchorConnectors]);

  if (!detail || !displayNode) {
    // A failed *tree* fetch retries through the app shell — without detail
    // there is no node to load, so no in-document retry can recover.
    const placeholderError = detail
      ? (selectedNodeId ? contentErrorByNode[selectedNodeId] : null) ?? null
      : detailError ?? null;
    const retry = detail
      ? () => setContentLoadNonce((value) => value + 1)
      : onRetryDetail;
    const headerTitle = detail?.tree.title ?? treeTitle ?? "Loading research…";
    return (
      <ResearchDocumentFrame
        title={headerTitle}
        headerActions={
          onShowSidebar ? (
            <ResearchSidebarRestoreButton
              onClick={onShowSidebar}
              onOpenTerminalMap={onOpenTerminalMap}
              terminalMapOpen={terminalMapOpen}
            />
          ) : undefined
        }
      >
        <div className="research-placeholder">
          {placeholderError ? null : (
            <LoaderCircle className="research-spinner" size={24} aria-hidden="true" />
          )}
          <h1>{placeholderError ? "Research unavailable" : "Loading research…"}</h1>
          {placeholderError ? (
            <>
              <p role="alert">{placeholderError}</p>
              {retry ? (
                <button className="control-button" type="button" onClick={retry}>
                  Retry
                </button>
              ) : null}
            </>
          ) : null}
        </div>
      </ResearchDocumentFrame>
    );
  }

  const activeRun = isActiveResearchStatus(displayNode.status);
  // Pane runs keep `paneId` bound until the process is reaped; SDK runs do
  // not, so a cancelled SDK node is immediately retryable. Treating
  // `runtime === "sdk"` as "still stopping" would hide Retry forever.
  const cancellationNeedsRetry =
    displayNode.status === "cancelled" && Boolean(displayNode.paneId);
  const threadLength = chainNodes.length;
  // The chip counts what this page shows: the thread's turns and the branch
  // cards hanging off them — not every descendant in the whole tree. The
  // single-node label keeps the legacy whole-tree follow-up total.
  const branchCount = chainNodeIds.reduce(
    (total, id) => total + (childrenBySegment.get(id)?.length ?? 0),
    0,
  );
  const legacyFollowupCount = Math.max(0, detail.nodes.length - 1);

  // The thread composer acts on the ask segment (always branching), the
  // thread's tail (Continue thread), or — in branch mode — the last
  // completed answer in the thread, so branching stays available while the
  // tail streams or after it failed, as every complete node's own composer
  // allowed before threads. A tail that already has an inline child cannot
  // exist by definition — the chain would extend through it — so "slot
  // taken" never disables the tail composer; only an unfinished or unusable
  // tail does.
  const lastCompleteChainNode =
    [...chainNodes].reverse().find((node) => node.status === "complete") ?? null;
  const composerTarget = ask
    ? detail.nodes.find((node) => node.id === ask.nodeId) ?? null
    : followupMode === "branch"
      ? lastCompleteChainNode
      : tailNode;
  const composerAwaitingCheckpoint = Boolean(
    composerTarget &&
      composerTarget.status === "complete" &&
      !canFollowUpFrom(composerTarget),
  );
  const composerDisabled =
    archived || !composerTarget || composerTarget.status !== "complete";
  const canSubmitFollowup = Boolean(
    composerTarget &&
      !archived &&
      followup.trim() &&
      !submitting &&
      canFollowUpFrom(composerTarget) &&
      (ask ||
        followupMode === "branch" ||
        canContinueThread(detail.nodes, composerTarget)),
  );
  const tailActive = Boolean(tailNode && isActiveResearchStatus(tailNode.status));
  const tailUnusable = Boolean(
    tailNode && (tailNode.status === "failed" || tailNode.status === "cancelled"),
  );
  // A settled inline tail can be retried in place: it is a deletable leaf
  // (no pane lingering — an unfinished cancel keeps its own Retry-cancel
  // controls) whose parent takes the relaunched question.
  const canRetryTail = Boolean(
    tailNode &&
      tailNode.inline &&
      tailNode.parentNodeId &&
      !archived &&
      canRetryResearchNode(tailNode),
  );
  const retryingTail = Boolean(tailNode && retryingNodeId === tailNode.id);
  const composerHint = archived
    ? null
    : composerAwaitingCheckpoint
      ? "Waiting for the native session checkpoint before continuing."
      : !ask && followupMode === "thread" && tailActive
        ? "Waiting for the answer above to finish."
        : !ask && followupMode === "thread" && tailUnusable
          ? `The last follow-up ${tailNode?.status === "failed" ? "failed" : "was cancelled"} — ${
              canRetryTail
                ? "retry it, delete it, or branch instead."
                : "delete it to continue the thread, or branch instead."
            }`
          : !ask &&
              followupMode === "branch" &&
              composerTarget &&
              composerTarget.id !== tailNode?.id
            ? "Branches from the last completed answer in this thread."
            : null;
  const composerPlaceholder = ask
    ? "Ask about the highlighted text…"
    : followupMode === "branch"
      ? "Start a new branch from the answer above…"
      : composerTarget?.kind === "document"
        ? "Ask about this document…"
        : composerTarget?.kind === "conversation"
          ? "Ask about this conversation…"
          : threadLength > 1
            ? "Continue this thread…"
            : "Type your query…";

  // The composer, parameterized on the ask it is docked to (null for the
  // thread-tail placement) so an anchored render cannot exist without its
  // ask context. A docked composer whose passage currently resolves sits
  // absolutely beside it; while the anchor cannot be located in the rendered
  // projection (a transcript-view toggle hid the passage, an edit moved it)
  // the composer stays in the rail's flow instead of pinning to top 0 over
  // the card stack.
  const renderComposer = (
    dockedAsk: { nodeId: string; anchor: ResearchHighlightAnchor } | null,
  ) => (
    <div
      ref={followupComposerRef}
      className={`research-followup-composer${composerDisabled ? " is-disabled" : ""}${
        dockedAsk ? " is-anchored" : " is-thread"
      }${dockedAsk && askComposerTop !== null ? " is-docked" : ""}`}
      style={dockedAsk && askComposerTop !== null ? { top: askComposerTop } : undefined}
    >
      {dockedAsk ? (
        <div className="research-followup-quote-row">
          <span className="research-followup-quote">
            {quoteDisplayText(dockedAsk.anchor.exact)}
          </span>
          <button
            type="button"
            className="control-button research-followup-quote-dismiss"
            aria-label="Cancel the targeted question"
            title="Cancel (Esc)"
            onClick={dismissAsk}
          >
            <X size={12} aria-hidden="true" />
          </button>
        </div>
      ) : null}
      {!dockedAsk && shortcutHintsShown ? (
        <span
          className="pane-tab-shortcut-hint research-followup-shortcut-hint"
          aria-hidden="true"
        >
          ⌘J
        </span>
      ) : null}
      {!dockedAsk ? (
        <div
          className="sidebar-mode-toggle research-followup-mode-toggle"
          role="tablist"
          aria-label="Follow-up mode"
        >
          <button
            type="button"
            role="tab"
            aria-selected={followupMode === "thread"}
            className={`control-button${followupMode === "thread" ? " is-selected" : ""}`}
            disabled={archived}
            onClick={() => setFollowupMode("thread")}
          >
            <span>Continue thread</span>
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={followupMode === "branch"}
            className={`control-button${followupMode === "branch" ? " is-selected" : ""}`}
            disabled={archived}
            onClick={() => setFollowupMode("branch")}
          >
            <span>New branch</span>
          </button>
        </div>
      ) : null}
      <textarea
        ref={followupTextareaRef}
        value={followup}
        placeholder={composerPlaceholder}
        aria-label="Follow-up question"
        disabled={composerDisabled}
        onChange={(event) => setFollowup(event.currentTarget.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
            event.preventDefault();
            void submitFollowup();
          }
        }}
        rows={2}
      />
      <div className="research-followup-footer">
        {composerHint ? (
          <div className="research-followup-hint-row">
            <small>{composerHint}</small>
            {!dockedAsk && followupMode === "thread" && canRetryTail ? (
              <button
                type="button"
                className="control-button research-followup-retry"
                disabled={retryingNodeId !== null || submitting}
                onClick={() => retryInlineTail()}
              >
                {retryingTail ? (
                  <>
                    <LoaderCircle className="research-spinner" size={12} aria-hidden="true" />
                    <span>Retrying…</span>
                  </>
                ) : (
                  <span>Retry follow-up</span>
                )}
              </button>
            ) : null}
          </div>
        ) : null}
        <div className="native-input-submit-actions">
          <button
            className="control-button"
            type="button"
            disabled={!canSubmitFollowup}
            onClick={() => void submitFollowup()}
          >
            <span>{submitting ? "Sending…" : "Send"}</span>
            {!submitting ? (
              <ComposerSubmitShortcutGlyph
                requireCmdEnter
                className="shortcut-hint"
              />
            ) : null}
          </button>
        </div>
      </div>
    </div>
  );

  const proposalsSection =
    publicationBinding && (visiblePublicationProposals.length > 0 || proposalError) ? (
      <section
        className="research-publication-proposals"
        aria-labelledby="research-publication-proposals-title"
      >
        <div className="research-publication-proposals-heading">
          <h3 id="research-publication-proposals-title">
            Community proposals
          </h3>
          <button
            type="button"
            className="control-button"
            aria-label="Refresh community proposals"
            title="Refresh"
            disabled={proposalActionId !== null}
            onClick={() => void refreshPublicationProposals()}
          >
            <RefreshCw size={13} aria-hidden="true" />
          </button>
        </div>
        {proposalError ? (
          <p className="research-publication-proposal-error" role="alert">
            {proposalError}
          </p>
        ) : null}
        {visiblePublicationProposals.map((proposal) => (
          <article
            className="research-publication-proposal"
            key={proposal.commentId}
          >
            <header>
              <strong>@{proposal.authorLogin}</strong>
              <span className={`is-${proposal.status}`}>
                {proposal.status}
              </span>
            </header>
            {proposal.anchor ? (
              <span className="research-followup-quote">
                {quoteDisplayText(proposal.anchor.exact)}
              </span>
            ) : null}
            <TranscriptMarkdown
              text={proposal.prompt}
              imageBehavior="open"
              inline
            />
            {proposal.answerMarkdown ? (
              <details>
                <summary>Proposed answer</summary>
                <TranscriptMarkdown
                  text={proposal.answerMarkdown}
                  imageBehavior="open"
                />
              </details>
            ) : null}
            {proposal.status === "pending" ? (
              <div className="research-publication-proposal-actions">
                <button
                  type="button"
                  className="control-button"
                  disabled={
                    proposalActionId !== null ||
                    archived ||
                    !proposal.parentNodeId
                  }
                  onClick={() => void resolveProposal(proposal, "accepted")}
                >
                  {proposalActionId === proposal.commentId ? (
                    <LoaderCircle
                      className="research-spinner"
                      size={13}
                      aria-hidden="true"
                    />
                  ) : (
                    <Check size={13} aria-hidden="true" />
                  )}
                  {proposal.localNodeId ? "Finish acceptance" : "Accept"}
                </button>
                <button
                  type="button"
                  className="control-button"
                  disabled={
                    proposalActionId !== null ||
                    Boolean(proposal.localNodeId)
                  }
                  onClick={() => void resolveProposal(proposal, "declined")}
                >
                  <X size={13} aria-hidden="true" />
                  Decline
                </button>
              </div>
            ) : proposal.localNodeId &&
              detail.nodes.some((node) => node.id === proposal.localNodeId) ? (
              <button
                type="button"
                className="control-button research-publication-proposal-result"
                onClick={() => selectNode(proposal.localNodeId!)}
              >
                Open local result
              </button>
            ) : null}
          </article>
        ))}
      </section>
    ) : null;

  // Sorted, comma-joined unread card ids per segment. A stable string keeps
  // the segment memo intact while the unread set is unchanged, unlike a fresh
  // Set identity every render.
  const unreadKeyBySegment = new Map<string, string>();
  for (const [segmentId, children] of childrenBySegment) {
    const ids = children
      .filter(
        (child) =>
          child.status === "complete" &&
          firstSeenCompleteRef.current.get(child.id) === false &&
          !openedFollowupIds.has(child.id),
      )
      .map((child) => child.id);
    if (ids.length > 0) {
      unreadKeyBySegment.set(segmentId, ids.sort().join(","));
    }
  }

  const renderedSegments = chainNodes.map((node, index) => {
    const view = segmentViews[index];
    if (!view) {
      return null;
    }
    const segmentChildren = childrenBySegment.get(node.id) ?? EMPTY_SEGMENT_CHILDREN;
    const segmentActive = isActiveResearchStatus(node.status);
    const linkedForSegment =
      linkedAnchorNodeId && segmentChildren.some((child) => child.id === linkedAnchorNodeId)
        ? linkedAnchorNodeId
        : null;
    const durationText = view.isConversation
      ? // A conversation's timestamps span the source terminal session's
        // lifetime, not a generation — a bare duration here would read as
        // run time.
        null
      : node.startedAt
        ? `${
            segmentActive
              ? "Generating for "
              : node.status !== "complete"
                ? "Ran for "
                : ""
          }${formatDuration((node.completedAt ?? metadataNow) - node.startedAt)}`
        : segmentActive
          ? "Waiting to start"
          : null;
    return (
      <ThreadSegment
        key={node.id}
        view={view}
        node={node}
        index={index}
        isSelected={node.id === selectedNodeId}
        contentError={contentErrorByNode[node.id] ?? null}
        segmentActive={segmentActive}
        showRunControls={
          (segmentActive || (node.status === "cancelled" && Boolean(node.paneId))) &&
          node.id !== displayNode.id
        }
        cancelling={cancelling}
        durationText={durationText}
        hiddenHighlightCount={hiddenHighlightsByNode[node.id] ?? 0}
        unreadCardKey={unreadKeyBySegment.get(node.id) ?? ""}
        pointerOverHighlight={pointerHighlightNodeId === node.id}
        linkedAnchorId={linkedForSegment}
        connectors={connectorsBySegment.get(node.id) ?? EMPTY_SEGMENT_CONNECTORS}
        segmentChildren={segmentChildren}
        anchoredCardTops={anchoredCardTops}
        resolvedCardTops={resolvedCardTops}
        menuOpen={followupMenu?.nodeId === node.id}
        askComposer={ask && ask.nodeId === node.id ? renderComposer(ask) : null}
        proposalsSection={node.id === selectedNodeId ? proposalsSection : null}
        registerSegmentElement={registerSegmentElement}
        onSelectNode={handleSelectNode}
        onExpandTurns={expandAllTurns}
        onRetryContentLoad={retryContentLoad}
        onShowFullTrace={showFullTraceFor}
        onCopyAnswer={handleCopyAnswer}
        onOpenAnswerMenu={openAnswerMenu}
        onOpenFollowupMenu={openFollowupMenu}
        onOpenPane={handleOpenPane}
        onCancelNode={handleCancelNode}
        canRetryNode={!archived && canRetryResearchNode(node)}
        retryingNode={retryingNodeId === node.id}
        onRetryNode={handleRetryNode}
        onCardHover={handleCardHover}
        onRootMouseDown={beginHighlightSelectionDrag}
        onRootMouseUp={finishHighlightSelectionDrag}
        onRootKeyUp={captureHighlightSelection}
        onRootClick={selectAnnotationAtPoint}
        onRootMouseMove={linkAnchorUnderPointer}
        onRootMouseLeave={unlinkAnchorPointer}
      />
    );
  });

  return (
    <TranscriptLinkActionsProvider actions={linkActions}>
      <>
        <div className="research-workspace">
        <main className="research-document">
          <header className="research-document-header">
            <ResearchHistoryNav
              canGoBack={canGoBack}
              canGoForward={canGoForward}
              backTitle={`Back (${IS_MAC ? "⌘[" : "Ctrl+["})`}
              forwardTitle={`Forward (${IS_MAC ? "⌘]" : "Ctrl+]"})`}
              onBack={goBack}
              onForward={goForward}
            />
            <div className="research-breadcrumb" aria-label="Research path">
              {breadcrumbDisplay.map((entry, displayIndex) =>
                entry.kind === "ellipsis" ? (
                  <span key="ellipsis">
                    <span className="research-breadcrumb-separator">/</span>
                    <span
                      className="research-breadcrumb-ellipsis"
                      title={`${entry.count} earlier ${entry.count === 1 ? "step" : "steps"}`}
                    >
                      …
                    </span>
                  </span>
                ) : (
                  <span key={entry.node.id}>
                    {displayIndex > 0 ? (
                      <span className="research-breadcrumb-separator">/</span>
                    ) : null}
                    <button
                      className="control-button"
                      type="button"
                      onClick={() => selectNode(entry.node.id)}
                    >
                      {entry.index === 0
                        ? detail.tree.title
                        : entry.node.title ?? entry.node.prompt}
                    </button>
                  </span>
                ),
              )}
            </div>
            {displayNode.origin === "terminalExport" ? (
              <span
                className="research-provenance-badge"
                title="This conversation was exported from a terminal session: a point-in-time copy whose content ran with the terminal's full permissions. Review it before publishing."
              >
                <Terminal size={12} aria-hidden="true" />
                Exported from terminal
              </span>
            ) : null}
            {threadLength > 1 || legacyFollowupCount > 0 ? (
              <span className="research-document-followup-count">
                {threadLength > 1
                  ? `${threadLength} in thread${
                      branchCount > 0
                        ? ` · ${branchCount} ${branchCount === 1 ? "branch" : "branches"}`
                        : ""
                    }`
                  : `${legacyFollowupCount} ${legacyFollowupCount === 1 ? "follow-up" : "follow-ups"}`}
              </span>
            ) : null}
            {onShowSidebar ? (
              <ResearchSidebarRestoreButton
                onClick={onShowSidebar}
                onOpenTerminalMap={onOpenTerminalMap}
                terminalMapOpen={terminalMapOpen}
              />
            ) : null}
            {selectedView?.hasTranscriptActivity && selectedNodeId ? (
              <button
                type="button"
                className={`control-button research-trace-toggle${
                  fullTraceNodes[selectedNodeId] ? " is-active" : ""
                }`}
                aria-pressed={Boolean(fullTraceNodes[selectedNodeId])}
                title={
                  fullTraceNodes[selectedNodeId]
                    ? "Hide full transcript"
                    : "Show full transcript"
                }
                aria-label={
                  fullTraceNodes[selectedNodeId]
                    ? "Hide full transcript"
                    : "Show full transcript"
                }
                onClick={() =>
                  setFullTraceNodes((current) => ({
                    ...current,
                    [selectedNodeId]: !current[selectedNodeId],
                  }))
                }
              >
                <ScrollText size={15} aria-hidden="true" />
              </button>
            ) : null}
            {displayNode.paneId && (activeRun || cancellationNeedsRetry) ? (
              <button
                type="button"
                className="control-button research-open-terminal"
                onClick={() => onOpenPane(displayNode.paneId!)}
              >
                <ExternalLink size={14} aria-hidden="true" />
                Open terminal
              </button>
            ) : null}
            {activeRun || cancellationNeedsRetry ? (
              <button
                type="button"
                className="control-button research-cancel-run"
                disabled={cancelling}
                onClick={() => {
                  const nodeId = displayNode.id;
                  setCancelling(true);
                  onCancel(nodeId)
                    .catch((err) => onError(err instanceof Error ? err.message : String(err)))
                    .finally(() => setCancelling(false));
                }}
              >
                <X size={14} aria-hidden="true" />
                {cancelling
                  ? "Cancelling…"
                  : cancellationNeedsRetry
                    ? "Retry cancel"
                    : "Cancel"}
              </button>
            ) : null}
          </header>

          <DomSearchBar
            active
            placeholder="Find in research"
            rootRef={documentScrollRef}
          />

          <article
            ref={documentScrollRef}
            className="research-document-scroll"
            onScroll={recordScroll}
          >
            <div ref={contentContainerRef} className="research-document-content">
              {renderedSegments}
              {!ask ? (
                <div className="research-response-grid research-thread-composer-row">
                  <div className="research-thread-composer-cell">{renderComposer(null)}</div>
                  <div aria-hidden="true" />
                </div>
              ) : null}
            </div>
          </article>
        </main>
        </div>
        {followupMenu && detail
          ? (() => {
              const node = detail.nodes.find((candidate) => candidate.id === followupMenu.nodeId);
              const info = researchBranchInfo(detail.nodes, followupMenu.nodeId);
              if (!node || !info) {
                return null;
              }
              const label = node.inline
                ? info.descendantCount > 0
                  ? "Delete from here"
                  : "Delete follow-up"
                : info.descendantCount > 0
                  ? "Delete branch"
                  : "Delete follow-up";
              const rootNode = node.id === detail.tree.rootNodeId;
              // A document root has no prompt; its identity is the tree title.
              const nodeName = (node.title ?? node.prompt) || detail.tree.title;
              const menuView = viewForNode(node.id);
              const menuContent = contentByNode[node.id] ?? null;
              return createPortal(
                <div
                  ref={followupMenuRef}
                  className="popover-surface popover-surface--context pane-context-menu research-followup-menu"
                  role="menu"
                  aria-label={`Actions for ${nodeName}`}
                  style={{ left: followupMenu.left, top: followupMenu.top }}
                  onMouseDown={(event) => event.stopPropagation()}
                  onContextMenu={(event) => event.preventDefault()}
                >
                  <div className="group-context-actions">
                    {(() => {
                      const rootReady = detail.nodes.some(
                        (candidate) =>
                          candidate.id === detail.tree.rootNodeId &&
                          isTerminalResearchStatus(candidate.status),
                      );
                      return (
                        <>
                          <button
                            type="button"
                            role="menuitem"
                            className="control-button"
                            disabled={!isTerminalResearchStatus(node.status)}
                            title={
                              isTerminalResearchStatus(node.status)
                                ? undefined
                                : "This result must finish before it can be published"
                            }
                            onClick={() => openResearchPublisher("answer", node)}
                          >
                            <Share2 size={13} aria-hidden="true" />
                            <span>Publish answer</span>
                          </button>
                          <button
                            type="button"
                            role="menuitem"
                            className="control-button"
                            disabled={!rootReady}
                            title={
                              rootReady
                                ? undefined
                                : "The root result must finish before publishing the tree"
                            }
                            onClick={() => openResearchPublisher("tree", node)}
                          >
                            <Share2 size={13} aria-hidden="true" />
                            <span>Publish research</span>
                          </button>
                        </>
                      );
                    })()}
                    {chainNodeIds.includes(node.id) && chainNodes.length > 1
                      ? (() => {
                          const threadReady = segmentViews.every((view) => view.content);
                          return (
                            <button
                              type="button"
                              role="menuitem"
                              className="control-button"
                              disabled={!threadReady}
                              title={
                                threadReady
                                  ? undefined
                                  : "The thread is still loading"
                              }
                              onClick={() => {
                                setFollowupMenu(null);
                                void copyThread();
                              }}
                            >
                              <Copy size={13} aria-hidden="true" />
                              <span>Copy thread as Markdown</span>
                            </button>
                          );
                        })()
                      : null}
                    {!archived && canRetryResearchNode(node) ? (
                      <button
                        type="button"
                        role="menuitem"
                        className="control-button"
                        disabled={retryingNodeId !== null}
                        onClick={() => {
                          setFollowupMenu(null);
                          handleRetryNode(node.id);
                        }}
                      >
                        <RefreshCw size={13} aria-hidden="true" />
                        <span>Retry run</span>
                      </button>
                    ) : null}
                    <div className="context-menu-divider" role="separator" />
                    {rootNode && node.kind === "document" ? (
                      <>
                        <button className="control-button"
                          type="button"
                          role="menuitem"
                          disabled={
                            archived ||
                            !menuContent?.responseRevision ||
                            menuView?.editableDocumentMarkdown == null
                          }
                          title={
                            archived
                              ? "Unarchive this research before editing its document"
                              : !menuContent?.responseRevision ||
                                  menuView?.editableDocumentMarkdown == null
                                ? "The document content is unavailable"
                                : undefined
                          }
                          onClick={() => {
                            setFollowupMenu(null);
                            if (
                              menuContent?.responseRevision &&
                              menuView?.editableDocumentMarkdown != null
                            ) {
                              setDocumentEditSession({
                                nodeId: node.id,
                                markdown: menuView.editableDocumentMarkdown,
                                title: detail.tree.title,
                                responseRevision: menuContent.responseRevision,
                                highlightIds:
                                  menuContent.node.highlights?.map((highlight) => highlight.id) ?? [],
                                highlightCount: menuContent.node.highlights?.length ?? 0,
                              });
                            }
                          }}
                        >
                          <Pencil size={13} aria-hidden="true" />
                          <span>Edit document</span>
                        </button>
                        <div className="context-menu-divider" role="separator" />
                      </>
                    ) : null}
                    <button
                      type="button"
                      role="menuitem"
                      className="control-button context-menu-danger"
                      disabled={info.hasActiveRuns}
                      title={
                        info.hasActiveRuns
                          ? "This branch must finish or be cancelled before deletion"
                          : undefined
                      }
                      onClick={() => {
                        setFollowupMenu(null);
                        setBranchRemovalError(null);
                        setDeletingBranchId(node.id);
                      }}
                    >
                      <Trash2 size={13} aria-hidden="true" />
                      <span>{rootNode ? "Delete research" : label}</span>
                    </button>
                  </div>
                </div>,
                document.body,
              );
            })()
          : null}
        {documentEditSession
          ? createPortal(
              <DocumentComposer
                mode="edit"
                initialMarkdown={documentEditSession.markdown}
                initialTitle={documentEditSession.title}
                highlightCount={documentEditSession.highlightCount}
                resetKey={`${documentEditSession.nodeId}:${documentEditSession.responseRevision}`}
                onClose={() => setDocumentEditSession(null)}
                onSubmit={saveDocumentEdit}
              />,
              document.body,
            )
          : null}
        {highlightAction
          ? createPortal(
              <div
                className="research-highlight-actions"
                style={{
                  left: highlightAction.left,
                  top: highlightAction.top,
                  visibility: highlightAction.offscreen ? "hidden" : undefined,
                }}
              >
                  <button
                    type="button"
                    className="control-button research-highlight-action"
                    disabled={savingHighlight}
                    aria-keyshortcuts="H"
                    aria-label={
                      !savingHighlight && highlightAction.highlightIds.length === 0
                        ? "Highlight"
                        : undefined
                    }
                    onMouseDown={(event) => event.preventDefault()}
                    onClick={() => void applyHighlightAction()}
                  >
                    {savingHighlight ? (
                      <span>Saving…</span>
                    ) : highlightAction.highlightIds.length > 0 ? (
                      <span>
                        {highlightAction.highlightIds.length > 1
                          ? "Remove highlights"
                          : "Remove highlight"}
                      </span>
                    ) : (
                      <Highlighter size={13} aria-hidden="true" />
                    )}
                    <kbd className="context-menu-shortcut is-keycap" aria-hidden="true">
                      H
                    </kbd>
                  </button>
                  {highlightAction.expandAnchor ? (
                    <button
                      type="button"
                      className="control-button research-highlight-action"
                      disabled={savingHighlight}
                      aria-keyshortcuts="E"
                      onMouseDown={(event) => event.preventDefault()}
                      onClick={() => void applyExpandHighlightAction()}
                    >
                      <span>
                        {highlightAction.highlightIds.length > 1
                          ? "Merge highlights"
                          : "Expand highlight"}
                      </span>
                      <kbd className="context-menu-shortcut is-keycap" aria-hidden="true">
                        E
                      </kbd>
                    </button>
                  ) : null}
                  {canAskSelection ? (
                    <button
                      type="button"
                      className="control-button research-highlight-action"
                      disabled={savingHighlight}
                      aria-keyshortcuts="A"
                      onMouseDown={(event) => event.preventDefault()}
                      onClick={enterAskMode}
                    >
                      <span>Ask</span>
                      <kbd className="context-menu-shortcut is-keycap" aria-hidden="true">
                        A
                      </kbd>
                    </button>
                  ) : null}
              </div>,
              document.body,
            )
          : null}
        {deletingBranch?.node && deletingBranch.info
          ? createPortal(
              <div
                className="confirm-dialog-backdrop"
                role="presentation"
                onMouseDown={(event) => {
                  if (event.target === event.currentTarget && !removingBranch) {
                    setDeletingBranchId(null);
                  }
                }}
              >
                <div
                  className="confirm-dialog"
                  role="dialog"
                  aria-modal="true"
                  aria-labelledby="delete-research-branch-dialog-title"
                  aria-busy={removingBranch}
                  onKeyDown={(event) => {
                    if (event.key === "Escape" && !removingBranch) {
                      event.preventDefault();
                      setDeletingBranchId(null);
                    }
                  }}
                >
                  <h2 id="delete-research-branch-dialog-title">
                    {deletingBranch.node.id === detail.tree.rootNodeId
                      ? "Delete research"
                      : deletingBranch.node.inline && deletingBranch.info.descendantCount > 0
                      ? "Delete the rest of this thread?"
                      : deletingBranch.info.descendantCount > 0
                      ? "Delete this research branch?"
                      : "Delete this follow-up?"}
                  </h2>
                  <p>
                    Delete "
                    {(deletingBranch.node.title ?? deletingBranch.node.prompt) ||
                      detail.tree.title}
                    "?
                  </p>
                  <p>
                    {deletingBranch.node.id === detail.tree.rootNodeId
                      ? deletingBranch.info.descendantCount > 0
                        ? `This permanently deletes the root answer and all ${deletingBranch.info.descendantCount} follow-up${deletingBranch.info.descendantCount === 1 ? "" : "s"}.`
                        : "This permanently deletes the root answer and its research history."
                      : deletingBranch.node.inline && deletingBranch.info.descendantCount > 0
                      ? `This permanently deletes this follow-up and everything after it in the thread — ${deletingBranch.info.descendantCount} descendant node${deletingBranch.info.descendantCount === 1 ? "" : "s"} in total, including any branches. Its parent answer keeps the freed inline slot.`
                      : deletingBranch.info.descendantCount > 0
                      ? `This also permanently deletes ${deletingBranch.info.descendantCount} descendant follow-up${deletingBranch.info.descendantCount === 1 ? "" : "s"}.`
                      : "This permanently deletes the follow-up and its response."} {" "}
                    This can’t be undone.
                  </p>
                  {branchRemovalError ? (
                    <p className="confirm-dialog-error" role="alert">
                      {branchRemovalError}
                    </p>
                  ) : null}
                  <div className="confirm-dialog-actions">
                    <button className="control-button"
                      type="button"
                      disabled={removingBranch}
                      onClick={() => setDeletingBranchId(null)}
                    >
                      Cancel
                    </button>
                    <button
                      type="button"
                      className="control-button danger"
                      autoFocus
                      disabled={removingBranch || deletingBranch.info.hasActiveRuns}
                      onClick={() => void confirmBranchRemoval()}
                    >
                      {removingBranch
                        ? "Deleting…"
                        : deletingBranch.node.id === detail.tree.rootNodeId
                          ? "Delete research"
                          : deletingBranch.node.inline && deletingBranch.info.descendantCount > 0
                          ? "Delete from here"
                          : deletingBranch.info.descendantCount > 0
                          ? "Delete branch"
                          : "Delete follow-up"}
                    </button>
                  </div>
                </div>
              </div>,
              document.body,
            )
          : null}
      </>
    </TranscriptLinkActionsProvider>
  );
}

export default memo(ResearchDocument);

function isTerminalResearchStatus(status: ResearchNode["status"]) {
  return status === "complete" || status === "failed" || status === "cancelled";
}

function researchNodeDisplayTitle(node: ResearchNode, detail: ResearchTreeDetail) {
  if (node.id === detail.tree.rootNodeId) {
    return node.title?.trim() || detail.tree.title;
  }
  return (
    node.title?.trim() ||
    node.prompt.split(/\r?\n/, 1)[0]?.replace(/\s+/g, " ").trim() ||
    "Research follow-up"
  );
}

function researchTreePreview(detail: ResearchTreeDetail, nodes: ResearchNode[]) {
  const lines = [
    detail.tree.title,
    "",
    `${nodes.length} published result${nodes.length === 1 ? "" : "s"}`,
  ];
  for (const node of nodes) {
    lines.push(
      `- ${researchNodeDisplayTitle(node, detail)}${
        node.status === "complete" ? "" : ` (${node.status})`
      }`,
    );
  }
  return lines.join("\n");
}

async function mapWithConcurrency<T, R>(
  values: T[],
  concurrency: number,
  mapper: (value: T) => Promise<R>,
) {
  const results = new Array<R>(values.length);
  let nextIndex = 0;
  const workers = Array.from(
    { length: Math.min(Math.max(1, concurrency), values.length) },
    async () => {
      while (nextIndex < values.length) {
        const index = nextIndex;
        nextIndex += 1;
        results[index] = await mapper(values[index]);
      }
    },
  );
  await Promise.all(workers);
  return results;
}
