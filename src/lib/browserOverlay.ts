import type { BrowserOverlayState } from "../appTypes";

export function browserOverlayIsOpen(
  state: BrowserOverlayState | undefined,
): boolean {
  return state?.open === true;
}

export function anyBrowserOverlayOpen(
  overlays: Record<string, BrowserOverlayState>,
): boolean {
  return Object.values(overlays).some((state) => state.open);
}

/** Marks every overlay closed. Returns the previous record when nothing changed. */
export function closeAllBrowserOverlaysState(
  overlays: Record<string, BrowserOverlayState>,
): Record<string, BrowserOverlayState> {
  let changed = false;
  const next: Record<string, BrowserOverlayState> = {};
  for (const [ownerId, state] of Object.entries(overlays)) {
    if (state.open) {
      next[ownerId] = { ...state, open: false };
      changed = true;
    } else {
      next[ownerId] = state;
    }
  }
  return changed ? next : overlays;
}

export type TranscriptOrBrowserToggle =
  | { type: "close-browser" }
  | { type: "toggle-transcript" }
  | { type: "toggle-browser" };

/** ⌘⇧E: a live browser always wins so the same chord can dismiss a leftover square. */
export function resolveTranscriptOrBrowserToggle(input: {
  anyBrowserOpen: boolean;
  canToggleTranscript: boolean;
}): TranscriptOrBrowserToggle {
  if (input.anyBrowserOpen) {
    return { type: "close-browser" };
  }
  if (input.canToggleTranscript) {
    return { type: "toggle-transcript" };
  }
  return { type: "toggle-browser" };
}
