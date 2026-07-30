import { useEffect, useRef } from "react";
import { ExternalLink } from "lucide-react";

// Right-click action for a link. Positioned at the pointer (viewport coords);
// closes on outside click, Escape, or after opening the link externally.
interface LinkContextMenuProps {
  x: number;
  y: number;
  onOpen: () => void;
  onClose: () => void;
}

export default function LinkContextMenu({
  x,
  y,
  onOpen,
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
      <button
        type="button"
        role="menuitem"
        className="menu-item link-context-menu-item"
        onClick={onOpen}
      >
        <ExternalLink size={14} aria-hidden="true" />
        <span>Open externally</span>
      </button>
    </div>
  );
}
