import type { ArtifactInfo } from "../types";
import type { BrowserOverlayState } from "../appTypes";

const IMAGE_EXTENSIONS = new Set([
  "avif",
  "bmp",
  "gif",
  "ico",
  "jpeg",
  "jpg",
  "png",
  "svg",
  "webp",
]);

const HTML_EXTENSIONS = new Set(["htm", "html"]);

export type ArtifactKind = "url" | "image" | "html" | "file";

/**
 * A tray is normally open when its pane has artifacts. Split layouts can turn
 * that default off for lower cells without overriding an explicit user reopen
 * (`closed === false`).
 */
export function artifactTrayVisible(
  hasArtifacts: boolean,
  closed: boolean | undefined,
  defaultVisible: boolean,
): boolean {
  if (!hasArtifacts || closed === true) {
    return false;
  }
  return defaultVisible || closed === false;
}

export function isArtifactBrowserOpen(
  overlay: BrowserOverlayState | undefined,
  artifactId: string,
): boolean {
  return overlay?.open === true && overlay.artifactId === artifactId;
}

export function artifactKind(artifact: ArtifactInfo): ArtifactKind {
  if (!artifact.path) {
    return "url";
  }
  const extension = artifact.path.split(".").pop()?.toLowerCase() ?? "";
  if (IMAGE_EXTENSIONS.has(extension)) {
    return "image";
  }
  return HTML_EXTENSIONS.has(extension) ? "html" : "file";
}

export function artifactName(artifact: ArtifactInfo): string {
  if (artifact.path) {
    return artifact.path.split("/").pop() || artifact.path;
  }
  const url = artifact.url ?? "";
  return url.replace(/^https?:\/\//, "").replace(/\/$/, "") || url;
}
