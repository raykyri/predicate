import type { BrowserOverlayState } from "../appTypes";

export function browserOverlayUsesNativeHumanChild(
  state: BrowserOverlayState | undefined,
): boolean {
  return Boolean(
    state?.open && state.mode === "webkit" && !state.sandbox && state.url !== null,
  );
}

export function nativeHumanBrowserOwnerIds(
  overlays: Record<string, BrowserOverlayState>,
): Set<string> {
  return new Set(
    Object.entries(overlays)
      .filter(([, state]) => browserOverlayUsesNativeHumanChild(state))
      .map(([ownerId]) => ownerId),
  );
}
