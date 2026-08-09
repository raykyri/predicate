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
import { artifactKind, artifactName } from "../lib/artifacts";
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
  position: ArtifactTrayPosition | null;
  onPositionChange: (position: ArtifactTrayPosition) => void;
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

export interface ArtifactTrayPosition {
  top: number;
  right: number;
}

const FRAME_INSET = 8;

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

  useEffect(() => {
    const missing = artifacts.filter(
      (artifact) =>
        artifactKind(artifact) === "image" && !(artifact.id in thumbUrlById),
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
      const maxRight = Math.max(
        FRAME_INSET,
        parent.clientWidth - frame.offsetWidth - FRAME_INSET,
      );
      const maxTop = Math.max(
        FRAME_INSET,
        parent.clientHeight - frame.offsetHeight - FRAME_INSET,
      );
      const clamped = {
        top: Math.min(maxTop, Math.max(FRAME_INSET, position.top)),
        right: Math.min(maxRight, Math.max(FRAME_INSET, position.right)),
      };
      if (clamped.top !== position.top || clamped.right !== position.right) {
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
    const origin = {
      top: frame.offsetTop,
      right: parent.clientWidth - frame.offsetLeft - frame.offsetWidth,
    };

    const move = (moveEvent: PointerEvent) => {
      const maxRight = Math.max(
        FRAME_INSET,
        parent.clientWidth - frame.offsetWidth - FRAME_INSET,
      );
      const maxTop = Math.max(
        FRAME_INSET,
        parent.clientHeight - frame.offsetHeight - FRAME_INSET,
      );
      onPositionChange({
        top: Math.min(
          maxTop,
          Math.max(FRAME_INSET, origin.top + moveEvent.clientY - start.y),
        ),
        right: Math.min(
          maxRight,
          Math.max(FRAME_INSET, origin.right - (moveEvent.clientX - start.x)),
        ),
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

  // Newest first: the freshest artifact is the one the user is most likely
  // reaching for, and the tray grows downward from its titlebar.
  const rows = [...artifacts].reverse();

  return (
    <div
      ref={frameRef}
      className={`artifact-tray${collapsed ? " is-collapsed" : ""}`}
      style={position ? { top: position.top, right: position.right, left: "auto" } : undefined}
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
                onMouseEnter={() => onHoverArtifact(artifact)}
                onMouseLeave={() => onHoverArtifact(null)}
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
    </div>
  );
}
