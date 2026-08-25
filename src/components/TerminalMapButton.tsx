import { LayoutDashboard } from "lucide-react";
import type { MouseEvent, PointerEvent } from "react";

interface TerminalMapButtonProps {
  className?: string;
  size?: number;
  pressed?: boolean;
  onClick: (event: MouseEvent<HTMLButtonElement>) => void;
  onPointerDown?: (event: PointerEvent<HTMLButtonElement>) => void;
}

/** Opens the terminal map — the column/queue overview of every agent. */
export default function TerminalMapButton({
  className = "icon-button",
  size = 14,
  pressed = false,
  onClick,
  onPointerDown,
}: TerminalMapButtonProps) {
  return (
    <button
      type="button"
      className={`${className}${pressed ? " is-active" : ""}`}
      title="Terminal map"
      aria-label="Open terminal map"
      aria-pressed={pressed}
      onPointerDown={onPointerDown}
      onClick={onClick}
    >
      <LayoutDashboard size={size} aria-hidden="true" />
    </button>
  );
}
