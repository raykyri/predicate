/**
 * Normalizes line endings in a Ghostty viewport dump for the floating
 * terminal PiP. Every row — including blank and trailing ones — is preserved
 * so the card renders the terminal's full grid shape instead of collapsing
 * to its non-empty prefix.
 */
export function formatTerminalPipText(raw: string): string {
  return raw.replace(/\r\n/g, "\n").replace(/\r/g, "\n");
}

export interface TerminalPipVisibility {
  transcriptExpanded: boolean;
  toggledOn: boolean;
  rightPaneCollapsed: boolean;
  nativeTerminalAvailable: boolean;
}

export type TerminalPipToggleVisibility = Omit<TerminalPipVisibility, "toggledOn">;

/** The PiP affordance only makes sense once the transcript owns the stage. */
export function shouldShowTerminalPipToggle({
  transcriptExpanded,
  rightPaneCollapsed,
  nativeTerminalAvailable,
}: TerminalPipToggleVisibility): boolean {
  return transcriptExpanded && !rightPaneCollapsed && nativeTerminalAvailable;
}

/** PiP is opt-in and can only occupy an open, expanded transcript stage. */
export function shouldShowTerminalPip({
  transcriptExpanded,
  toggledOn,
  rightPaneCollapsed,
  nativeTerminalAvailable,
}: TerminalPipVisibility): boolean {
  return (
    toggledOn &&
    shouldShowTerminalPipToggle({
      transcriptExpanded,
      rightPaneCollapsed,
      nativeTerminalAvailable,
    })
  );
}

/** Smallest mini-map font; below this the card clips rather than shrink further. */
export const TERMINAL_PIP_MIN_FONT_SIZE = 3.5;
/** Never let a narrow grid blow the preview up past a comfortable read. */
export const TERMINAL_PIP_MAX_FONT_SIZE = 12;

/**
 * Largest font size (px) at which a `columns × rows` monospace grid fits
 * inside `maxWidth × maxHeight` (content box, px). `charWidthPerPx` is one
 * glyph's advance at 1px font size (~0.6 for monospace, canvas-measured by
 * the caller); `lineHeight` is the unitless CSS line-height of the pre.
 */
export function fitTerminalPipFontSize(
  columns: number,
  rows: number,
  charWidthPerPx: number,
  maxWidth: number,
  maxHeight: number,
  lineHeight = 1.2,
): number {
  if (
    columns <= 0 ||
    rows <= 0 ||
    charWidthPerPx <= 0 ||
    maxWidth <= 0 ||
    maxHeight <= 0 ||
    lineHeight <= 0
  ) {
    return TERMINAL_PIP_MIN_FONT_SIZE;
  }
  const byWidth = maxWidth / (columns * charWidthPerPx);
  const byHeight = maxHeight / (rows * lineHeight);
  return Math.min(
    TERMINAL_PIP_MAX_FONT_SIZE,
    Math.max(TERMINAL_PIP_MIN_FONT_SIZE, Math.min(byWidth, byHeight)),
  );
}
