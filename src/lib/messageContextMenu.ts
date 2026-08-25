export interface TranscriptMessageContextMenuPoint {
  clientX: number;
  clientY: number;
}

/**
 * A transcript message replaces the browser context menu only when its own
 * ellipsis menu is available. A nested control may prevent the event first to
 * claim a more specific menu (for example, link actions).
 */
export function transcriptMessageContextMenuPoint(input: {
  hasMessageMenu: boolean;
  defaultPrevented: boolean;
  clientX: number;
  clientY: number;
}): TranscriptMessageContextMenuPoint | null {
  if (!input.hasMessageMenu || input.defaultPrevented) {
    return null;
  }
  return { clientX: input.clientX, clientY: input.clientY };
}
