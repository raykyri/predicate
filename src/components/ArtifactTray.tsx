import {
  ExternalLink,
  FileSearchCorner,
  FileText,
  Folder,
  Globe,
  Image as ImageIcon,
  Minus,
  Paperclip,
  Undo2,
  X,
} from "lucide-react";
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { PointerEvent as ReactPointerEvent } from "react";
import { artifactFileUrl } from "../lib/api";
import {
  artifactCanPreview,
  artifactKind,
  artifactName,
} from "../lib/artifacts";
import { formatRelativeTime } from "../lib/transcriptSessions";
import type { ArtifactInfo } from "../types";

interface ArtifactTrayProps {
  /** The pane whose right-pane cell hosts this tray instance. */
  paneId: string;
  /** This workspace group's artifacts, oldest first (newest renders on top). */
  artifacts: ArtifactInfo[];
  paneExists: (paneId: string) => boolean;
  collapsed: boolean;
  /** Dragged position; null keeps the default top-right anchor. */
  position: { x: number; y: number } | null;
  onPositionChange: (position: { x: number; y: number }) => void;
  onSetCollapsed: (collapsed: boolean) => void;
  onClose: () => void;
  onOpen: (artifact: ArtifactInfo) => void;
  onOpenExternal: (artifact: ArtifactInfo) => void;
  onReveal: (artifact: ArtifactInfo) => void;
  onRemove: (artifact: ArtifactInfo) => void;
  /** The last removal in this workspace, if the undo window is still open. */
  undo: ArtifactInfo | null;
  onUndo: () => void;
  /** Row hover, for previewing a cross-pane row's source tab; null on leave. */
  onHoverArtifact: (artifact: ArtifactInfo | null) => void;
}

const FRAME_INSET = 8;
const DEFAULT_TOP = 48;
/** Hover dwell before the preview card shows. */
const PREVIEW_DELAY_MS = 400;

export default function ArtifactTray({
  paneId,
  artifacts,
  paneExists,
  collapsed,
  position,
  onPositionChange,
  onSetCollapsed,
  onClose,
  onOpen,
  onOpenExternal,
  onReveal,
  onRemove,
  undo,
  onUndo,
  onHoverArtifact,
}: ArtifactTrayProps) {
  const frameRef = useRef<HTMLDivElement>(null);
  // Token-scoped file-server URLs for image and HTML previews, fetched once
  // per artifact id. null = fetched but unavailable (source pane gone, file
  // moved); the row falls back to a glyph tile.
  const [thumbUrlById, setThumbUrlById] = useState<Record<string, string | null>>({});
  const [previewArtifactId, setPreviewArtifactId] = useState<string | null>(null);
  /** Which side of the tray the preview card opens on, from free space. */
  const [previewSide, setPreviewSide] = useState<"left" | "right">("left");
  const previewTimerRef = useRef<number | null>(null);

  useEffect(() => {
    const missing = artifacts.filter(
      (artifact) =>
        artifactCanPreview(artifact) && !(artifact.id in thumbUrlById),
    );
    if (missing.length === 0) {
      return;
    }
    let disposed = false;
    for (const artifact of missing) {
      artifactFileUrl(artifact.id)
        .catch((): string | null => null)
        .then((url) => {
          if (!disposed) {
            setThumbUrlById((current) => ({ ...current, [artifact.id]: url }));
          }
        });
    }
    return () => {
      disposed = true;
    };
  }, [artifacts, thumbUrlById]);

  useEffect(
    () => () => {
      if (previewTimerRef.current !== null) {
        window.clearTimeout(previewTimerRef.current);
      }
    },
    [],
  );

  // A dragged position is clamped back inside the cell when the pane shrinks;
  // the default (null) position is a CSS right-anchor and clamps itself.
  useLayoutEffect(() => {
    if (!position) {
      return;
    }
    const frame = frameRef.current;
    const parent = frame?.parentElement;
    if (!frame || !parent) {
      return;
    }
    const constrain = () => {
      const maxX = Math.max(FRAME_INSET, parent.clientWidth - frame.offsetWidth - FRAME_INSET);
      const maxY = Math.max(FRAME_INSET, parent.clientHeight - frame.offsetHeight - FRAME_INSET);
      const clamped = {
        x: Math.min(maxX, Math.max(FRAME_INSET, position.x)),
        y: Math.min(maxY, Math.max(FRAME_INSET, position.y)),
      };
      if (clamped.x !== position.x || clamped.y !== position.y) {
        onPositionChange(clamped);
      }
    };
    const observer = new ResizeObserver(constrain);
    observer.observe(parent);
    constrain();
    return () => observer.disconnect();
  }, [position, onPositionChange]);

  function startDrag(event: ReactPointerEvent<HTMLDivElement>) {
    if (event.button !== 0) {
      return;
    }
    const frame = frameRef.current;
    const parent = frame?.parentElement;
    if (!frame || !parent) {
      return;
    }
    event.preventDefault();
    event.currentTarget.setPointerCapture(event.pointerId);
    const start = { x: event.clientX, y: event.clientY };
    // The default anchor has no stored position; the frame's current offsets
    // are the origin either way.
    const origin = { x: frame.offsetLeft, y: frame.offsetTop };

    const move = (moveEvent: PointerEvent) => {
      const maxX = Math.max(FRAME_INSET, parent.clientWidth - frame.offsetWidth - FRAME_INSET);
      const maxY = Math.max(FRAME_INSET, parent.clientHeight - frame.offsetHeight - FRAME_INSET);
      onPositionChange({
        x: Math.min(maxX, Math.max(FRAME_INSET, origin.x + moveEvent.clientX - start.x)),
        y: Math.min(maxY, Math.max(FRAME_INSET, origin.y + moveEvent.clientY - start.y)),
      });
    };
    const stop = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
      window.removeEventListener("pointercancel", stop);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
    window.addEventListener("pointercancel", stop);
  }

  function hoverRow(artifact: ArtifactInfo) {
    onHoverArtifact(artifact);
    if (previewTimerRef.current !== null) {
      window.clearTimeout(previewTimerRef.current);
      previewTimerRef.current = null;
    }
    setPreviewArtifactId(null);
    if (!artifactCanPreview(artifact) || !thumbUrlById[artifact.id]) {
      return;
    }
    previewTimerRef.current = window.setTimeout(() => {
      previewTimerRef.current = null;
      const frame = frameRef.current;
      const parent = frame?.parentElement;
      if (frame && parent) {
        const spaceLeft = frame.offsetLeft;
        const spaceRight = parent.clientWidth - frame.offsetLeft - frame.offsetWidth;
        setPreviewSide(spaceLeft >= spaceRight ? "left" : "right");
      }
      setPreviewArtifactId(artifact.id);
    }, PREVIEW_DELAY_MS);
  }

  function leaveRow() {
    onHoverArtifact(null);
    if (previewTimerRef.current !== null) {
      window.clearTimeout(previewTimerRef.current);
      previewTimerRef.current = null;
    }
    setPreviewArtifactId(null);
  }

  // Newest first: the freshest artifact is the one the user is most likely
  // reaching for, and the tray grows downward from its titlebar.
  const rows = [...artifacts].reverse();
  const previewArtifact = previewArtifactId
    ? rows.find((artifact) => artifact.id === previewArtifactId)
    : undefined;
  const previewUrl = previewArtifact ? thumbUrlById[previewArtifact.id] : null;

  return (
    <div
      ref={frameRef}
      className={`artifact-tray${collapsed ? " is-collapsed" : ""}`}
      style={position ? { left: position.x, top: position.y, right: "auto" } : undefined}
    >
      <div className="artifact-tray-titlebar" onPointerDown={startDrag}>
        <Paperclip size={11} aria-hidden="true" className="artifact-tray-clip" />
        <span className="artifact-tray-label">Artifacts</span>
        <button
          type="button"
          className="artifact-tray-chrome-button"
          title={collapsed ? "Expand artifacts" : "Collapse artifacts"}
          aria-label={collapsed ? "Expand artifacts" : "Collapse artifacts"}
          onPointerDown={(event) => event.stopPropagation()}
          onClick={() => onSetCollapsed(!collapsed)}
        >
          <Minus size={10} aria-hidden="true" />
        </button>
        <button
          type="button"
          className="artifact-tray-chrome-button"
          title="Hide artifact tray"
          aria-label="Hide artifact tray"
          onPointerDown={(event) => event.stopPropagation()}
          onClick={onClose}
        >
          <X size={10} aria-hidden="true" />
        </button>
      </div>
      {collapsed ? null : (
        <div className="artifact-tray-body">
          {rows.map((artifact) => {
            const kind = artifactKind(artifact);
            const other = artifact.paneId !== paneId && paneExists(artifact.paneId);
            const thumbUrl = kind === "image" ? thumbUrlById[artifact.id] : null;
            return (
              <div
                key={artifact.id}
                role="button"
                tabIndex={0}
                className={`artifact-tray-row${other ? " is-other" : ""}`}
                onMouseEnter={() => hoverRow(artifact)}
                onMouseLeave={leaveRow}
                onClick={() => onOpen(artifact)}
                onKeyDown={(event) => {
                  if (event.key === "Enter" || event.key === " ") {
                    event.preventDefault();
                    onOpen(artifact);
                  }
                }}
              >
                <span className="artifact-tray-thumb">
                  {thumbUrl ? (
                    <img src={thumbUrl} alt="" loading="lazy" />
                  ) : kind === "url" ? (
                    <Globe size={11} aria-hidden="true" />
                  ) : kind === "image" ? (
                    <ImageIcon size={11} aria-hidden="true" />
                  ) : (
                    <FileText size={11} aria-hidden="true" />
                  )}
                </span>
                <span
                  className="artifact-tray-name"
                  title={artifact.path ?? artifact.url ?? undefined}
                >
                  {artifactName(artifact)}
                </span>
                <span className="artifact-tray-meta">
                  {formatRelativeTime(artifact.createdAt)}
                </span>
                <span className="artifact-tray-actions">
                  <button
                    type="button"
                    title="Open in qmux browser"
                    aria-label="Open in qmux browser"
                    onClick={(event) => {
                      event.stopPropagation();
                      onOpen(artifact);
                    }}
                  >
                    <FileSearchCorner size={11} aria-hidden="true" />
                  </button>
                  {kind !== "image" ? (
                    <button
                      type="button"
                      title="Open in external browser"
                      aria-label="Open in external browser"
                      onClick={(event) => {
                        event.stopPropagation();
                        onOpenExternal(artifact);
                      }}
                    >
                      <ExternalLink size={11} aria-hidden="true" />
                    </button>
                  ) : null}
                  {kind !== "url" ? (
                    <button
                      type="button"
                      title="Open folder"
                      aria-label="Open folder"
                      onClick={(event) => {
                        event.stopPropagation();
                        onReveal(artifact);
                      }}
                    >
                      <Folder size={11} aria-hidden="true" />
                    </button>
                  ) : null}
                  <button
                    type="button"
                    title="Remove"
                    aria-label="Remove"
                    onClick={(event) => {
                      event.stopPropagation();
                      onRemove(artifact);
                    }}
                  >
                    <X size={11} aria-hidden="true" />
                  </button>
                </span>
              </div>
            );
          })}
          {undo ? (
            <div className="artifact-tray-undo">
              <Undo2 size={10} aria-hidden="true" />
              <span className="artifact-tray-undo-name">
                Removed {artifactName(undo)}
              </span>
              <button type="button" onClick={onUndo}>
                Undo
              </button>
            </div>
          ) : null}
        </div>
      )}
      {previewArtifact && previewUrl ? (
        <div className={`artifact-tray-preview is-${previewSide}`}>
          {artifactKind(previewArtifact) === "html" ? (
            <iframe
              src={previewUrl}
              title={`Preview of ${artifactName(previewArtifact)}`}
              // Match the full browser overlay: scripts may render, but the
              // opaque origin cannot read other token-gated file responses.
              sandbox="allow-scripts"
              referrerPolicy="no-referrer"
              tabIndex={-1}
            />
          ) : (
            <img src={previewUrl} alt="" />
          )}
          <span className="artifact-tray-preview-caption">
            {artifactName(previewArtifact)}
          </span>
        </div>
      ) : null}
    </div>
  );
}
