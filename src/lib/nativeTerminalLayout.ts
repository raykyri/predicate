export interface NativeTerminalRect {
  left: number;
  top: number;
  width: number;
  height: number;
}

/**
 * Hidden terminal DOM nodes still follow the active tab's layout. Keep a
 * parked native surface at its last visible frame so another tab adding or
 * removing a right pane cannot resize its PTY behind the scenes.
 */
export function nativeTerminalLayoutRect(
  measuredRect: NativeTerminalRect,
  visible: boolean,
  lastVisibleRect: NativeTerminalRect | null,
): NativeTerminalRect {
  return visible || !lastVisibleRect ? measuredRect : lastVisibleRect;
}
