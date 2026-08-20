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
}

export interface TranscriptScrollMetrics {
  scrollTop: number;
  scrollHeight: number;
  clientHeight: number;
}

const TRANSCRIPT_END_EPSILON = 2;

/**
 * Scroll events do not identify their cause: WebKit emits the same event for a
 * wheel gesture, a focus-driven clamp, and an assignment to `scrollTop`.
 * Persist only while the transcript has observed explicit user scroll intent;
 * synchronous tab capture remains the authoritative non-event save path.
 */
export function shouldPersistTranscriptScroll(
  restoring: boolean,
  userScrollIntent: boolean,
): boolean {
  return !restoring && userScrollIntent;
}

/** Keys whose default action can move a focused transcript scroll container. */
export function transcriptScrollKeySignalsIntent(key: string): boolean {
  return (
    key === "ArrowUp" ||
    key === "ArrowDown" ||
    key === "PageUp" ||
    key === "PageDown" ||
    key === "Home" ||
    key === "End" ||
    key === " "
  );
}

/**
 * A plain content click only gives WebKit first responder and must not authorize
 * its later focus clamp. Touch starts and the scrollbar gutter can initiate a
 * scroll before any pointer movement, so they establish pending intent.
 */
export function transcriptPointerDownSignalsScrollIntent(
  pointerType: string,
  clientX: number,
  rightEdge: number,
  scrollbarWidth: number,
): boolean {
  return (
    pointerType === "touch" || clientX >= rightEdge - Math.max(12, scrollbarWidth)
  );
}

/**
 * A restore is allowed to commit only after a minimum focus/layout settling
 * window and consecutive undisturbed frames. The maximum keeps a continuously
 * animating transcript from retaining the restore gate indefinitely.
 */
export function transcriptRestoreHasSettled(
  elapsedMs: number,
  stableFrames: number,
  minimumMs: number,
  maximumMs: number,
  requiredStableFrames: number,
): boolean {
  return (
    elapsedMs >= maximumMs ||
    (elapsedMs >= minimumMs && stableFrames >= requiredStableFrames)
  );
}

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
