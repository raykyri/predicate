import type { MessageItem } from "./turnTimeline";

export interface HistoryScanAnchor {
  key: string;
  /** Card top minus scroller top, from getBoundingClientRect. */
  offset: number;
}

export interface HistoryScanCardBox {
  key: string;
  role: string;
  top: number;
  bottom: number;
}

/** Drop non-user cards above the pinned last-user message; keep that card and everything after. */
export function collapseOlderNonUserItems(
  items: readonly MessageItem[],
  stickyUserKey: string,
): MessageItem[] {
  const stickyIndex = items.findIndex((item) => item.key === stickyUserKey);
  if (stickyIndex <= 0) {
    return items as MessageItem[];
  }
  if (!items.slice(0, stickyIndex).some((item) => item.role !== "user")) {
    return items as MessageItem[];
  }
  return items
    .slice(0, stickyIndex)
    .filter((item) => item.role === "user")
    .concat(items.slice(stickyIndex));
}

export function historyScanCollapsesItems(
  items: readonly MessageItem[],
  stickyUserKey: string | null,
): boolean {
  if (!stickyUserKey) {
    return false;
  }
  const stickyIndex = items.findIndex((item) => item.key === stickyUserKey);
  if (stickyIndex <= 0) {
    return false;
  }
  return items.slice(0, stickyIndex).some((item) => item.role !== "user");
}

/**
 * Move a section divider onto the first still-visible item at or after its
 * original card, so "Previous conversation" survives collapsing an assistant
 * opener.
 */
export function remapSectionLabels(
  labels: ReadonlyMap<string, string>,
  original: readonly { key: string }[],
  visible: readonly { key: string }[],
): Map<string, string> {
  if (labels.size === 0) {
    return new Map();
  }
  const visibleKeys = new Set(visible.map((item) => item.key));
  const remapped = new Map<string, string>();
  for (let index = 0; index < original.length; index += 1) {
    const label = labels.get(original[index].key);
    if (!label) {
      continue;
    }
    for (let lookAhead = index; lookAhead < original.length; lookAhead += 1) {
      const key = original[lookAhead].key;
      if (visibleKeys.has(key) && !remapped.has(key)) {
        remapped.set(key, label);
        break;
      }
    }
  }
  return remapped;
}

/** Add this to scrollTop so a card now at `currentOffset` returns to `targetOffset`. */
export function scrollTopDeltaToKeepOffset(
  currentOffset: number,
  targetOffset: number,
): number {
  return currentOffset - targetOffset;
}

/**
 * The card that should stay put across a collapse/expand: the first visible
 * card, or the nearest user card above it when that first card is an assistant
 * reply that is about to disappear.
 */
export function pickHistoryScanAnchor(
  cards: readonly HistoryScanCardBox[],
  scrollerTop: number,
  paddingTop: number,
): HistoryScanAnchor | null {
  if (cards.length === 0) {
    return null;
  }
  const topLine = scrollerTop + paddingTop;
  let candidate =
    cards.find((card) => card.bottom > topLine + 1) ?? cards[cards.length - 1];
  if (candidate.role !== "user") {
    const index = cards.indexOf(candidate);
    for (let lookBehind = index - 1; lookBehind >= 0; lookBehind -= 1) {
      if (cards[lookBehind].role === "user") {
        candidate = cards[lookBehind];
        break;
      }
    }
  }
  if (candidate.role !== "user") {
    const index = cards.indexOf(candidate);
    for (let lookAhead = index + 1; lookAhead < cards.length; lookAhead += 1) {
      if (cards[lookAhead].role === "user") {
        candidate = cards[lookAhead];
        break;
      }
    }
  }
  if (candidate.role !== "user") {
    return null;
  }
  return {
    key: candidate.key,
    offset: candidate.top - scrollerTop,
  };
}
