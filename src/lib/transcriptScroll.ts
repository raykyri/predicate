export interface TranscriptScrollCaptureSlot {
  capture: () => void;
  register: (capture: () => void) => () => void;
}

export interface TranscriptScrollPosition {
  scrollTop: number;
  /** Whether newly arriving visible content should continue following the tail. */
  stuck: boolean;
  /**
   * Whether this snapshot should restore to the physical end. This is narrower
   * than `stuck`: the live-follow threshold intentionally starts 100px before
   * the end, but a tab round-trip must not discard an exact offset in that band.
   */
  atEnd: boolean;
  /**
   * History-scan restore: the compact list's pixel `scrollTop` is not comparable
   * to the full transcript, so tab restore places this card at `anchorOffset`.
   */
  anchorKey?: string;
  /** Card top minus scroller top, matching getBoundingClientRect. */
  anchorOffset?: number;
}

export interface TranscriptScrollMetrics {
  scrollTop: number;
  scrollHeight: number;
  clientHeight: number;
}

const TRANSCRIPT_END_EPSILON = 2;

export function captureTranscriptScrollPosition(
  metrics: TranscriptScrollMetrics,
  followingLatest: boolean,
  stickThreshold: number,
): TranscriptScrollPosition {
  const distanceFromBottom =
    metrics.scrollHeight - metrics.scrollTop - metrics.clientHeight;
  return {
    scrollTop: metrics.scrollTop,
    stuck: followingLatest || distanceFromBottom <= stickThreshold,
    // An in-progress explicit jump carries the same restore intent as having
    // physically reached the end, even if its smooth animation is interrupted
    // by the tab switch.
    atEnd: followingLatest || distanceFromBottom <= TRANSCRIPT_END_EPSILON,
  };
}

export function transcriptScrollRestoreTop(
  saved: TranscriptScrollPosition | null | undefined,
  scrollHeight: number,
): number {
  return !saved || saved.atEnd ? scrollHeight : saved.scrollTop;
}

export function withTranscriptScrollAnchor(
  position: TranscriptScrollPosition,
  anchor: { key: string; offset: number } | null,
): TranscriptScrollPosition {
  if (!anchor || position.atEnd) {
    return position;
  }
  return { ...position, anchorKey: anchor.key, anchorOffset: anchor.offset };
}

export function transcriptScrollAnchorOf(
  saved: TranscriptScrollPosition | null | undefined,
): { key: string; offset: number } | null {
  if (
    !saved ||
    saved.atEnd ||
    saved.anchorKey === undefined ||
    saved.anchorOffset === undefined
  ) {
    return null;
  }
  return { key: saved.anchorKey, offset: saved.anchorOffset };
}

/**
 * Holds the active transcript's synchronous scroll capture. Registration
 * cleanups are token-scoped so an outgoing pane cannot clear a newer pane's
 * capture when React switches their layout effects in the same commit.
 */
export function createTranscriptScrollCaptureSlot(): TranscriptScrollCaptureSlot {
  let current: (() => void) | null = null;

  return {
    capture: () => current?.(),
    register: (capture) => {
      current = capture;
      return () => {
        if (current === capture) {
          current = null;
        }
      };
    },
  };
}
