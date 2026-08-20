import { useEffect, useRef } from "react";
import { ExternalLink, FolderOpen, Globe } from "lucide-react";

// Right-click chooser for a link. Web links choose between the internal and OS
// browsers; local links can preview, reveal, or deliberately use the default app.
// Positioned at the pointer (viewport coords); closes on outside click or Escape.
interface LinkContextMenuProps {
  x: number;
  y: number;
  canOpenInternal: boolean;
  onOpenInternal: () => void;
  externalLabel?: string;
  externalKind?: "browser" | "reveal";
  onOpenExternal: () => void;
  onOpenWithDefaultApp?: (() => void) | null;
  onClose: () => void;
}

export default function LinkContextMenu({
  x,
  y,
  canOpenInternal,
  onOpenInternal,
  externalLabel = "Open in browser",
  externalKind = "browser",
  onOpenExternal,
  onOpenWithDefaultApp = null,
  onClose,
}: LinkContextMenuProps) {
  const ref = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    const handlePointerDown = (event: MouseEvent) => {
      if (!ref.current?.contains(event.target as Node)) {
        onClose();
      }
    };
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        onClose();
      }
    };
    document.addEventListener("mousedown", handlePointerDown);
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("mousedown", handlePointerDown);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [onClose]);

  return (
    <div
      ref={ref}
      className="popover-surface popover-surface--context link-context-menu"
      style={{ left: x, top: y }}
      role="menu"
    >
      {canOpenInternal ? (
        <button
          type="button"
          role="menuitem"
          className="menu-item link-context-menu-item"
          onClick={onOpenInternal}
        >
          <Globe size={14} aria-hidden="true" />
          <span>Open</span>
        </button>
      ) : null}
      <button
        type="button"
        role="menuitem"
        className="menu-item link-context-menu-item"
        onClick={onOpenExternal}
      >
        {externalKind === "reveal" ? (
          <FolderOpen size={14} aria-hidden="true" />
        ) : (
          <ExternalLink size={14} aria-hidden="true" />
        )}
        <span>{externalLabel}</span>
      </button>
      {onOpenWithDefaultApp ? (
        <button
          type="button"
          role="menuitem"
          className="menu-item link-context-menu-item"
          onClick={onOpenWithDefaultApp}
        >
          <ExternalLink size={14} aria-hidden="true" />
          <span>Open with default app</span>
        </button>
      ) : null}
    </div>
  );
}
