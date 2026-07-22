import { useEffect, useRef, type RefObject } from "react";
import { isEditableTarget } from "../../lib/appHelpers";
import { researchSwipeDirection } from "../../lib/researchHistory";

// One physical trackpad gesture stays locked (through momentum events) until
// it goes idle for this long; mirrors ResearchDocument's swipe handling.
const SWIPE_IDLE_MS = 180;

/** Whether a horizontally scrollable descendant between `target` and
 * `boundary` can still consume movement in the wheel's direction, in which
 * case history swiping must yield to its native scroll. Mirror of the
 * research document's scroller check. */
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

/**
 * Browser-style history navigation inputs for a scrollable reading surface,
 * mirroring the research document's wiring exactly: Cmd/Ctrl+[ / ] and
 * Alt+←/→ (ignored while typing), dedicated mouse back/forward buttons
 * (3/4) on the surface, and an accumulated horizontal wheel swipe that
 * yields to descendant horizontal scrollers and locks per gesture.
 *
 * The handlers are read through refs so the listeners bind once per mounted
 * surface instead of rebinding on every history change.
 */
export function useHistoryNavigationInput(
  scrollRef: RefObject<HTMLElement | null>,
  goBack: () => void,
  goForward: () => void,
): void {
  const goBackRef = useRef(goBack);
  const goForwardRef = useRef(goForward);
  goBackRef.current = goBack;
  goForwardRef.current = goForward;

  useEffect(() => {
    const target = scrollRef.current;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.defaultPrevented || isEditableTarget(event.target)) {
        return;
      }
      const primary = event.metaKey || event.ctrlKey;
      let handler: (() => void) | null = null;
      if (primary && !event.altKey && !event.shiftKey && event.code === "BracketLeft") {
        handler = goBackRef.current;
      } else if (primary && !event.altKey && !event.shiftKey && event.code === "BracketRight") {
        handler = goForwardRef.current;
      } else if (
        event.altKey &&
        !event.metaKey &&
        !event.ctrlKey &&
        !event.shiftKey &&
        event.key === "ArrowLeft"
      ) {
        handler = goBackRef.current;
      } else if (
        event.altKey &&
        !event.metaKey &&
        !event.ctrlKey &&
        !event.shiftKey &&
        event.key === "ArrowRight"
      ) {
        handler = goForwardRef.current;
      }
      if (handler) {
        event.preventDefault();
        handler();
      }
    };
    const onMouseUp = (event: MouseEvent) => {
      if (event.button === 3) {
        event.preventDefault();
        goBackRef.current();
      } else if (event.button === 4) {
        event.preventDefault();
        goForwardRef.current();
      }
    };

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
      resetTimer = window.setTimeout(resetGesture, SWIPE_IDLE_MS);
    };
    const onWheel = (event: WheelEvent) => {
      if (
        !target ||
        event.defaultPrevented ||
        event.metaKey ||
        event.ctrlKey ||
        event.altKey ||
        event.shiftKey
      ) {
        return;
      }
      const scale =
        event.deltaMode === WheelEvent.DOM_DELTA_LINE
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

    window.addEventListener("keydown", onKeyDown);
    target?.addEventListener("mouseup", onMouseUp);
    target?.addEventListener("wheel", onWheel, { passive: false });
    return () => {
      window.removeEventListener("keydown", onKeyDown);
      target?.removeEventListener("mouseup", onMouseUp);
      target?.removeEventListener("wheel", onWheel);
      if (resetTimer !== null) {
        window.clearTimeout(resetTimer);
      }
    };
  }, [scrollRef]);
}
