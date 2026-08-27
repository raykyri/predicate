import { Check, Notebook, X } from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { placePanePopover, turnPaneRectFrom } from "../lib/appHelpers";
import type { NotificationLogEntry } from "../lib/notificationLog";
import { notificationLogHasUnread } from "../lib/notificationLog";
import { useConfirm } from "../hooks/useConfirm";
import { formatShortRelativeTime } from "./UserNotificationStack";

const MENU_PREFERRED_WIDTH = 340;

interface NotificationJournalMenuProps {
  entries: NotificationLogEntry[];
  showNotifications: boolean;
  onShowNotificationsChange: (show: boolean) => void;
  onMarkRead: (id: string) => void;
  onMarkAllRead: () => void;
  onClear: (id: string) => void;
  onOpenPane: (paneId: string) => void;
}

export default function NotificationJournalMenu({
  entries,
  showNotifications,
  onShowNotificationsChange,
  onMarkRead,
  onMarkAllRead,
  onClear,
  onOpenPane,
}: NotificationJournalMenuProps) {
  const [open, setOpen] = useState(false);
  const [now, setNow] = useState(() => Date.now());
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const popoverRef = useRef<HTMLDivElement | null>(null);
  const [pos, setPos] = useState<{
    left: number;
    top: number;
    maxHeight: number;
    maxWidth: number;
  } | null>(null);
  const { confirm, dialog: confirmDialog } = useConfirm();
  const hasUnread = notificationLogHasUnread(entries);
  const newestFirst = [...entries].reverse();

  useEffect(() => {
    if (!open) return;
    setNow(Date.now());
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [open]);

  const position = useCallback(() => {
    const trigger = triggerRef.current;
    const popover = popoverRef.current;
    if (!trigger || !popover) {
      return;
    }
    const { height } = popover.getBoundingClientRect();
    setPos(
      placePanePopover({
        triggerRect: trigger.getBoundingClientRect(),
        popoverSize: { width: MENU_PREFERRED_WIDTH, height },
        paneRect: turnPaneRectFrom(trigger),
        align: "end",
        prefer: "below",
      }),
    );
  }, []);

  useLayoutEffect(() => {
    if (!open) {
      return;
    }
    position();
    const observer = new ResizeObserver(position);
    if (popoverRef.current) {
      observer.observe(popoverRef.current);
    }
    window.addEventListener("resize", position);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", position);
    };
  }, [open, newestFirst.length, position]);

  useEffect(() => {
    if (!open) {
      return;
    }
    const handlePointerDown = (event: MouseEvent) => {
      const target = event.target as Node;
      if (target instanceof Element && target.closest(".confirm-dialog-backdrop")) {
        return;
      }
      if (!triggerRef.current?.contains(target) && !popoverRef.current?.contains(target)) {
        setOpen(false);
      }
    };
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") {
        return;
      }
      event.stopPropagation();
      setOpen(false);
    };
    document.addEventListener("mousedown", handlePointerDown);
    document.addEventListener("keydown", handleKeyDown, true);
    return () => {
      document.removeEventListener("mousedown", handlePointerDown);
      document.removeEventListener("keydown", handleKeyDown, true);
    };
  }, [open]);

  const clearEntry = async (entry: NotificationLogEntry) => {
    const confirmed = await confirm({
      message: `Clear “${entry.title}”? This cannot be undone.`,
      confirmLabel: "Clear",
    });
    if (confirmed) {
      onClear(entry.id);
    }
  };

  return (
    <div className="notification-journal">
      <button
        ref={triggerRef}
        type="button"
        className={`control-button turn-pane-header-button${open ? " is-active" : ""}`}
        title="Journal"
        aria-label="Journal"
        aria-haspopup="dialog"
        aria-expanded={open}
        onClick={() => setOpen((current) => !current)}
      >
        <Notebook size={14} aria-hidden="true" />
        {hasUnread ? (
          <span className="notification-journal-unread-dot" aria-hidden="true" />
        ) : null}
      </button>
      {open
        ? createPortal(
            <div
              ref={popoverRef}
              className="popover-surface notification-journal-menu"
              role="dialog"
              aria-label="Journal"
              style={
                pos
                  ? {
                      left: pos.left,
                      top: pos.top,
                      maxHeight: pos.maxHeight,
                      width: Math.min(MENU_PREFERRED_WIDTH, pos.maxWidth),
                      maxWidth: pos.maxWidth,
                    }
                  : { left: -9999, top: -9999 }
              }
            >
              <div className="notification-journal-toolbar">
                <button
                  type="button"
                  role="checkbox"
                  aria-checked={showNotifications}
                  className="notification-journal-toggle"
                  onClick={() => onShowNotificationsChange(!showNotifications)}
                >
                  <span className="home-group-checkbox" aria-hidden="true">
                    {showNotifications ? <Check size={10} strokeWidth={3} /> : null}
                  </span>
                  Show notifications
                </button>
                <button
                  type="button"
                  className="control-button notification-journal-mark-all"
                  disabled={!hasUnread}
                  onClick={onMarkAllRead}
                >
                  Mark all read
                </button>
              </div>
              <div className="notification-journal-feed" role="feed" aria-label="Notifications">
                {newestFirst.length === 0 ? (
                  <p className="notification-journal-empty">No notifications yet</p>
                ) : (
                  newestFirst.map((entry) => (
                    <article
                      key={entry.id}
                      className={`notification-journal-item${entry.read ? "" : " is-unread"}`}
                    >
                      <div className="notification-journal-item-heading">
                        {!entry.read ? (
                          <span className="notification-journal-item-dot" aria-hidden="true" />
                        ) : null}
                        <strong>{entry.title}</strong>
                        <time
                          className="notification-journal-item-time"
                          dateTime={new Date(entry.createdAt).toISOString()}
                        >
                          {formatShortRelativeTime(entry.createdAt, now)}
                        </time>
                      </div>
                      <p className="notification-journal-item-body">{entry.body}</p>
                      <div className="notification-journal-item-actions">
                        {entry.paneId ? (
                          <button
                            type="button"
                            className="control-button"
                            onClick={() => {
                              onOpenPane(entry.paneId!);
                              if (!entry.read) {
                                onMarkRead(entry.id);
                              }
                              setOpen(false);
                            }}
                          >
                            Open pane
                          </button>
                        ) : null}
                        {!entry.read ? (
                          <button
                            type="button"
                            className="control-button"
                            onClick={() => onMarkRead(entry.id)}
                          >
                            Mark read
                          </button>
                        ) : null}
                        <button
                          type="button"
                          className="control-button notification-journal-clear"
                          aria-label={`Clear ${entry.title}`}
                          onClick={() => void clearEntry(entry)}
                        >
                          <X size={13} aria-hidden="true" />
                          Clear
                        </button>
                      </div>
                    </article>
                  ))
                )}
              </div>
            </div>,
            document.body,
          )
        : null}
      {confirmDialog}
    </div>
  );
}
