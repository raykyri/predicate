/** Default line budget for the floating terminal PiP card. */
export const TERMINAL_PIP_MAX_LINES = 18;

/**
 * Trims trailing blank rows and keeps the last `maxLines` of a viewport dump
 * so the PiP shows the live bottom of the screen rather than empty padding.
 */
export function formatTerminalPipText(
  raw: string,
  maxLines = TERMINAL_PIP_MAX_LINES,
): string {
  const lines = raw.replace(/\r\n/g, "\n").replace(/\r/g, "\n").split("\n");
  while (lines.length > 0 && lines[lines.length - 1]?.trim() === "") {
    lines.pop();
  }
  if (lines.length <= maxLines) {
    return lines.join("\n");
  }
  return lines.slice(-maxLines).join("\n");
}
