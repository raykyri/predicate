import { useLayoutEffect, useRef } from "react";
import { setNativeTerminalWebOverlayRegion } from "../lib/api";

let nextRegionSequence = 0;

/** Publishes the element's current rect, grown by `padding` on every side. The
 * slop exists so the native hit region matches a control whose DOM hit area is
 * deliberately larger than its painted box (the split resizer). */
function publishRegion(regionId: string, element: HTMLElement, padding: number) {
  const rect = element.getBoundingClientRect();
  void setNativeTerminalWebOverlayRegion({
    regionId,
    x: rect.left - padding,
    y: rect.top - padding,
    width: rect.width + padding * 2,
    height: rect.height + padding * 2,
    visible: rect.width > 0 && rect.height > 0,
  }).catch(() => undefined);
}

/**
 * Keeps the referenced element's bounding rect registered as a web-owned
 * pointer region with the native terminal event router. Without this, any
 * pointer event over a terminal surface is forwarded straight to Ghostty, so
 * a DOM control floating over the terminal never receives its clicks.
 *
 * Attach the returned ref to the floating element; pass `enabled: false` (or
 * unmount) to release the region. `layoutKey` should change when the element
 * moves without resizing (for example, between terminal split tracks), and
 * `undefined` freezes position tracking without releasing the region — used
 * while a drag owns the whole pointer stream anyway. `padding` grows the
 * registered rect beyond the element's own box.
 */
export function useNativeWebOverlayRegion<T extends HTMLElement>(
  enabled: boolean,
  layoutKey?: unknown,
  padding = 0,
) {
  const elementRef = useRef<T | null>(null);
  const regionIdRef = useRef<string | null>(null);
  // Bumped on every registration; release retries from an earlier
  // registration stop the moment a newer one owns the region.
  const registrationRef = useRef(0);
  // Read through a ref so the ResizeObserver sync (registered once) always
  // uses the current slop without tearing the registration down.
  const paddingRef = useRef(padding);
  paddingRef.current = padding;
  if (regionIdRef.current === null) {
    nextRegionSequence += 1;
    regionIdRef.current = `web-overlay-${nextRegionSequence}`;
  }

  useLayoutEffect(() => {
    const regionId = regionIdRef.current;
    const element = elementRef.current;
    if (!enabled || !regionId || !element) {
      return;
    }
    const registration = ++registrationRef.current;
    let frame: number | null = null;
    const sync = () => {
      frame = null;
      publishRegion(regionId, element, paddingRef.current);
    };
    const schedule = () => {
      if (frame !== null) {
        cancelAnimationFrame(frame);
      }
      frame = requestAnimationFrame(sync);
    };
    const observer = new ResizeObserver(schedule);
    observer.observe(element);
    // A ResizeObserver only reports size changes; the rect also moves when the
    // surrounding layout resizes, so track the offset parent and the window.
    if (element.offsetParent instanceof HTMLElement) {
      observer.observe(element.offsetParent);
    }
    window.addEventListener("resize", schedule);
    sync();
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", schedule);
      if (frame !== null) {
        cancelAnimationFrame(frame);
      }
      // A dropped release leaves a phantom web-owned rectangle over the
      // terminal — pointer events inside it bypass Ghostty indefinitely — so
      // transient failures retry. A newer registration of this same region
      // (the control re-enabled) supersedes the release and stops the loop.
      const release = (attempt: number) => {
        if (registrationRef.current !== registration) {
          return;
        }
        void setNativeTerminalWebOverlayRegion({
          regionId,
          x: 0,
          y: 0,
          width: 0,
          height: 0,
          visible: false,
        }).catch(() => {
          if (attempt < 2) {
            window.setTimeout(() => release(attempt + 1), 50 * (attempt + 1));
          }
        });
      };
      release(0);
    };
  }, [enabled]);

  // Position-only changes do not notify ResizeObserver. Resync them without
  // tearing down the registration: a release/register pair could briefly
  // expose the moved control to native pointer routing.
  useLayoutEffect(() => {
    const regionId = regionIdRef.current;
    const element = elementRef.current;
    if (!enabled || layoutKey === undefined || !regionId || !element) {
      return;
    }
    publishRegion(regionId, element, padding);
  }, [enabled, layoutKey, padding]);

  return elementRef;
}
