import { useEffect, useRef } from "react";
import { ExternalLink, Globe } from "lucide-react";

// Right-click chooser for a link: open it in the internal browser overlay or the OS
// browser. Positioned at the pointer (viewport coords); closes on outside click,
// Escape, or either choice.
interface LinkContextMenuProps {
  x: number;
  y: number;
  canOpenInternal: boolean;
  onOpenInternal: () => void;
  onOpenExternal: () => void;
  onClose: () => void;
}

export default function LinkContextMenu({
  x,
  y,
  canOpenInternal,
  onOpenInternal,
  onOpenExternal,
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
        <ExternalLink size={14} aria-hidden="true" />
        <span>Open in browser</span>
      </button>
    </div>
  );
}
