// Pasted images reach transcripts as literal text markers: Claude Code stores the
// bytes in ~/.claude/image-cache and writes "[Image: source: <path>]" as its own
// text block, images referenced inline in a typed prompt appear as "[Image #N]",
// and Codex serializes clipboard attachments as an empty <image ...></image>
// block whose name may be "[Image]" or "[Image #N]". Images pasted into a qmux
// composer/queue are stored as "[Image: <path>]"
// with an absolute path (the form delivered to the agent as text). All shapes
// collapse to a muted "[Image]" chip in compact views, or a thumbnail where the
// marker carries a resolvable path.
export interface ImageMarkerSegment {
  kind: "text" | "image";
  text: string;
}

export const COLLAPSED_IMAGE_LABEL = "[Image]";

// The bare-path form requires a leading "/" so it only matches an absolute path,
// never prose like "[Image: figure 2]" — and, since "source:" has no leading
// slash, never overlaps the Claude Code "[Image: source: …]" marker.
const IMAGE_MARKER_SOURCE =
  /\[Image: source: [^\]\n]*\]|\[Image: \/[^\]\n]*\]|\[Image #\d+\]|<image\b(?=[^>\r\n]*\bname=(?:"\[Image(?: #\d+)?\]"|'\[Image(?: #\d+)?\]'|\[Image(?: #\d+)?\]))(?=[^>\r\n]*\bpath=(?:"[^"\r\n]+"|'[^'\r\n]+'))[^>\r\n]*>[\t\r\n ]*<\/image>/;

function imageMarkerPattern() {
  return new RegExp(IMAGE_MARKER_SOURCE.source, "g");
}

export function splitImageMarkers(text: string): ImageMarkerSegment[] {
  const segments: ImageMarkerSegment[] = [];
  const pattern = imageMarkerPattern();
  let index = 0;

  for (const match of text.matchAll(pattern)) {
    const start = match.index;
    if (start > index) {
      segments.push({ kind: "text", text: text.slice(index, start) });
    }
    segments.push({ kind: "image", text: match[0] });
    index = start + match[0].length;
  }

  if (index < text.length || segments.length === 0) {
    segments.push({ kind: "text", text: text.slice(index) });
  }

  return segments;
}

export function collapseImageMarkers(text: string): string {
  return text.replace(imageMarkerPattern(), COLLAPSED_IMAGE_LABEL);
}

// Both the Claude Code "[Image: source: <path>]" marker and the qmux
// "[Image: <path>]" paste marker carry an on-disk path; the "source: " prefix is
// optional so one extractor handles both.
const IMAGE_MARKER_PATH = /^\[Image: (?:source: )?([^\]\n]*)\]$/;
const CODEX_IMAGE_MARKER_PATH = /\bpath=(?:"([^"\r\n]+)"|'([^'\r\n]+)')/;

/** Extracts the on-disk path from a path-bearing bracket or Codex XML marker.
 *  Standalone numbered "[Image #N]" references carry no path and return null —
 *  they can only render as the collapsed chip. */
export function imageMarkerSourcePath(marker: string): string | null {
  const bracketPath = IMAGE_MARKER_PATH.exec(marker)?.[1];
  const codexMatch = marker.match(new RegExp(`^(?:${IMAGE_MARKER_SOURCE.source})$`));
  const codexPath = codexMatch ? CODEX_IMAGE_MARKER_PATH.exec(marker) : null;
  const path = (bracketPath ?? codexPath?.[1] ?? codexPath?.[2])?.trim();
  return path ? path : null;
}
