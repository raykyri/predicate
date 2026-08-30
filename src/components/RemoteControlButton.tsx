import { Smartphone } from "lucide-react";
import type { MouseEvent, PointerEvent, RefObject } from "react";
import { remoteButtonIndicator } from "../lib/remoteControl";
import type { RemoteStatus } from "../lib/remoteControl";

interface RemoteControlButtonProps {
  className?: string;
  size?: number;
  pressed?: boolean;
  status: RemoteStatus;
  // The popover anchors to this button, which lives in App's grouped header row.
  buttonRef?: RefObject<HTMLButtonElement | null>;
  onClick: (event: MouseEvent<HTMLButtonElement>) => void;
  onPointerDown?: (event: PointerEvent<HTMLButtonElement>) => void;
}

/**
 * Opens remote control — the paired-device panel for driving qmux from a phone.
 * Remote control that is silently on is a footgun, so the button reads as active
 * whenever an endpoint is bound, popover open or not, and wears an accent dot
 * while a device is actually connected.
 */
export default function RemoteControlButton({
  className = "icon-button",
  size = 14,
  pressed = false,
  status,
  buttonRef,
  onClick,
  onPointerDown,
}: RemoteControlButtonProps) {
  const { active, sessionDot } = remoteButtonIndicator(status, pressed);
  return (
    <button
      ref={buttonRef}
      type="button"
      className={`${className} remote-control-button${active ? " is-active" : ""}`}
      title="Remote control"
      aria-label="Remote control"
      aria-pressed={pressed}
      onPointerDown={onPointerDown}
      onClick={onClick}
    >
      <Smartphone size={size} aria-hidden="true" />
      {sessionDot ? <span className="remote-control-button-dot" aria-hidden="true" /> : null}
    </button>
  );
}
