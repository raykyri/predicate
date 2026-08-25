import { CheckCircle2, CircleAlert, Info, TriangleAlert, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { useNativeWebOverlayRegion } from "../hooks/useNativeWebOverlayRegion";

export type UserNotificationTone = "info" | "success" | "warning" | "error";

export interface UserNotificationItem {
  id: string;
  title: string;
  body: string;
  tone: UserNotificationTone;
  timeoutMs: number;
  paneId: string | null;
}

interface UserNotificationStackProps {
  notifications: UserNotificationItem[];
  onDismiss: (id: string) => void;
  onOpenPane: (paneId: string) => void;
}

const EXIT_MS = 180;
const MAX_VISIBLE = 3;

function ToneIcon({ tone }: { tone: UserNotificationTone }) {
  const props = { size: 18, "aria-hidden": true as const };
  switch (tone) {
    case "success":
      return <CheckCircle2 {...props} />;
    case "warning":
      return <TriangleAlert {...props} />;
    case "error":
      return <CircleAlert {...props} />;
    default:
      return <Info {...props} />;
  }
}

function NotificationCard({
  notification,
  active,
  onDismiss,
  onOpenPane,
}: {
  notification: UserNotificationItem;
  active: boolean;
  onDismiss: (id: string) => void;
  onOpenPane: (paneId: string) => void;
}) {
  const [phase, setPhase] = useState<"entering" | "visible" | "exiting">("entering");
  const [hovered, setHovered] = useState(false);
  const remainingRef = useRef(notification.timeoutMs);
  const dismissingRef = useRef(false);
  const exitTimerRef = useRef<number | null>(null);

  useEffect(() => {
    const frame = requestAnimationFrame(() => setPhase("visible"));
    return () => cancelAnimationFrame(frame);
  }, []);

  useEffect(
    () => () => {
      if (exitTimerRef.current !== null) window.clearTimeout(exitTimerRef.current);
    },
    [],
  );

  const beginDismiss = useCallback(() => {
    if (dismissingRef.current) return;
    dismissingRef.current = true;
    setPhase("exiting");
    exitTimerRef.current = window.setTimeout(() => onDismiss(notification.id), EXIT_MS);
  }, [notification.id, onDismiss]);

  useEffect(() => {
    if (!active || hovered || phase !== "visible") return;
    const startedAt = performance.now();
    const timer = window.setTimeout(beginDismiss, Math.max(0, remainingRef.current));
    return () => {
      window.clearTimeout(timer);
      remainingRef.current = Math.max(0, remainingRef.current - (performance.now() - startedAt));
    };
  }, [active, beginDismiss, hovered, phase]);

  const content = (
    <>
      <span className="user-notification-icon">
        <ToneIcon tone={notification.tone} />
      </span>
      <span className="user-notification-copy">
        <strong>{notification.title}</strong>
        <span>{notification.body}</span>
      </span>
    </>
  );

  return (
    <article
      className={`user-notification is-${notification.tone} is-${phase}`}
      role="status"
      aria-live={notification.tone === "error" ? "assertive" : "polite"}
      onMouseEnter={() => setHovered(true)}
      onMouseLeave={() => setHovered(false)}
    >
      {notification.paneId ? (
        <button
          type="button"
          className="user-notification-open"
          aria-label={`${notification.title}: ${notification.body}. Open source pane.`}
          onClick={() => {
            onOpenPane(notification.paneId!);
            beginDismiss();
          }}
        >
          {content}
        </button>
      ) : (
        <div className="user-notification-open">{content}</div>
      )}
      <button
        type="button"
        className="user-notification-close"
        aria-label="Dismiss notification"
        onClick={beginDismiss}
      >
        <X size={15} aria-hidden="true" />
      </button>
    </article>
  );
}

export function UserNotificationStack({
  notifications,
  onDismiss,
  onOpenPane,
}: UserNotificationStackProps) {
  const [windowActive, setWindowActive] = useState(
    () => document.visibilityState === "visible" && document.hasFocus(),
  );
  const visible = notifications.slice(0, MAX_VISIBLE);
  const regionRef = useNativeWebOverlayRegion<HTMLDivElement>(
    visible.length > 0,
    visible.map((notification) => notification.id).join("\0"),
  );

  useEffect(() => {
    const update = () =>
      setWindowActive(document.visibilityState === "visible" && document.hasFocus());
    window.addEventListener("focus", update);
    window.addEventListener("blur", update);
    document.addEventListener("visibilitychange", update);
    return () => {
      window.removeEventListener("focus", update);
      window.removeEventListener("blur", update);
      document.removeEventListener("visibilitychange", update);
    };
  }, []);

  if (visible.length === 0) return null;
  return (
    <div ref={regionRef} className="user-notification-stack" aria-label="Notifications">
      {visible.map((notification) => (
        <NotificationCard
          key={notification.id}
          notification={notification}
          active={windowActive}
          onDismiss={onDismiss}
          onOpenPane={onOpenPane}
        />
      ))}
    </div>
  );
}
