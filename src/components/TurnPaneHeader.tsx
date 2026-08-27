import {
  Expand,
  Globe,
  Minimize2,
  PanelLeftOpen,
  PanelRightClose,
  Paperclip,
  PictureInPicture2,
  Pin,
  SquareCenterlineDashedVertical,
} from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { placePanePopover, turnPaneRectFrom } from "../lib/appHelpers";
import { writeClipboardText } from "../lib/clipboard";
import PromptLibraryMenu from "./PromptLibraryMenu";
import NotificationJournalMenu from "./NotificationJournalMenu";
import TerminalMapButton from "./TerminalMapButton";
import { formatRelativeTime, sessionMenuTitle } from "../lib/transcriptSessions";
import type { NotificationLogEntry } from "../lib/notificationLog";
import type { TranscriptOption } from "../types";

// How long the "copied" toast stays up after copying the session id.
const COPIED_TOAST_MS = 1600;

// Preferred natural widths for the header menus; placement clamps them to the pane.
const SESSION_MENU_PREFERRED_WIDTH = 320;
const SESSION_MENU_MAX_HEIGHT = 400;

// The top bar across the right pane: the active session's id on the left, and
// session/browser/transcript controls on the right. Its height matches the
// browser overlay's address bar so the two read as a single chrome line when
// the browser is open.
interface TurnPaneHeaderProps {
  agentId?: string | null;
  // The active agent's session id, or null before SessionStart lands.
  sessionId: string | null;
  // Model the agent was launched with, when known (e.g. launcher pick or
  // inherited on fork). Shell-typed launches often omit it.
  model?: string | null;
  // Sessions in this agent's folder for the top-left session switcher; the
  // active one is whichever matches transcriptPath.
  transcriptOptions: TranscriptOption[];
  transcriptPath: string | null;
  onSelectTranscript: (path: string | null) => void;
  showQueueSplit: boolean;
  queueSplit: boolean;
  onToggleQueueSplit: () => void;
  browserOpen: boolean;
  onToggleBrowser: () => void;
  // Workspace artifact-tray entries for this pane's group. Zero hides the
  // paperclip entirely; a count increase pulses it once.
  artifactCount?: number;
  artifactTrayOpen?: boolean;
  onToggleArtifactTray?: () => void;
  transcriptExpanded: boolean;
  showTerminalPipToggle: boolean;
  terminalPipEnabled: boolean;
  onToggleTerminalPip: () => void;
  transcriptShortcutLabel: string;
  onToggleTranscriptExpanded: () => void;
  onCollapseRightBar: () => void;
  onRestoreLeftSidebar?: () => void;
  onOpenTerminalMap?: () => void;
  terminalMapOpen?: boolean;
  // Inserts saved-prompt text into this pane's composer; absent when the pane
  // has no agent composer, which disables the prompt-library trigger.
  onInsertPrompt?: (text: string) => void;
  // Pins the latest user message to the top of the transcript while its reply
  // scrolls. Active chrome matches the other header toggles.
  stickyUserMessages: boolean;
  onToggleStickyUserMessages: () => void;
  // The pane's project directory (keys the prompt library's Project scope) and
  // its home-relative display form (shown beside the Project heading).
  promptProjectDir?: string | null;
  promptProjectPath?: string | null;
  notificationLog: NotificationLogEntry[];
  showNotifications: boolean;
  onShowNotificationsChange: (show: boolean) => void;
  onMarkNotificationRead: (id: string) => void;
  onMarkAllNotificationsRead: () => void;
  onClearNotification: (id: string) => void;
  onOpenNotificationPane: (paneId: string) => void;
}

type MenuPos = {
  left: number;
  top: number;
  maxHeight: number;
  maxWidth: number;
};

export default function TurnPaneHeader({
  agentId,
  sessionId,
  model,
  transcriptOptions,
  transcriptPath,
  onSelectTranscript,
  showQueueSplit,
  queueSplit,
  onToggleQueueSplit,
  browserOpen,
  onToggleBrowser,
  artifactCount = 0,
  artifactTrayOpen = false,
  onToggleArtifactTray,
  transcriptExpanded,
  showTerminalPipToggle,
  terminalPipEnabled,
  onToggleTerminalPip,
  transcriptShortcutLabel,
  onToggleTranscriptExpanded,
  onCollapseRightBar,
  onRestoreLeftSidebar,
  onOpenTerminalMap,
  terminalMapOpen = false,
  onInsertPrompt,
  promptProjectDir,
  promptProjectPath,
  stickyUserMessages,
  onToggleStickyUserMessages,
  notificationLog,
  showNotifications,
  onShowNotificationsChange,
  onMarkNotificationRead,
  onMarkAllNotificationsRead,
  onClearNotification,
  onOpenNotificationPane,
}: TurnPaneHeaderProps) {
  const [sessionMenuOpen, setSessionMenuOpen] = useState(false);
  // One pulse when a new artifact lands (the count ticks up), so an agent
  // opening a file is noticeable without stealing focus.
  const [artifactPulse, setArtifactPulse] = useState(false);
  const prevArtifactCountRef = useRef(artifactCount);
  useEffect(() => {
    const previous = prevArtifactCountRef.current;
    prevArtifactCountRef.current = artifactCount;
    if (artifactCount > previous) {
      setArtifactPulse(true);
      const timer = window.setTimeout(() => setArtifactPulse(false), 1900);
      return () => window.clearTimeout(timer);
    }
  }, [artifactCount]);
  const sessionTriggerRef = useRef<HTMLButtonElement | null>(null);
  const sessionPopoverRef = useRef<HTMLDivElement | null>(null);
  const [sessionPos, setSessionPos] = useState<MenuPos | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const toastTimer = useRef<number | null>(null);
  // Sorted newest first so recent sessions appear at the top of the menu.
  // Memoized: the header re-renders with every app render, and re-sorting the
  // (up to 30-entry) list each time was avoidable churn.
  const sessionOptions = useMemo(
    () => [...transcriptOptions].sort((a, b) => b.modifiedMs - a.modifiedMs),
    [transcriptOptions],
  );
  const canOpenSessionMenu = Boolean(sessionId || sessionOptions.length > 0);
  const modelLabel = model?.trim() || null;
  const sessionLabel = sessionId
    ? modelLabel
      ? `(${modelLabel}) Session: ${sessionId}`
      : `Session: ${sessionId}`
    : "New session";

  // Clear any pending toast timer on unmount so it can't fire into a gone component.
  useEffect(() => {
    return () => {
      if (toastTimer.current !== null) {
        window.clearTimeout(toastTimer.current);
      }
    };
  }, []);

  // Close the session menu on an outside click or Escape while it is open.
  useEffect(() => {
    if (!sessionMenuOpen) {
      return;
    }
    const handlePointerDown = (event: MouseEvent) => {
      const target = event.target as Node;
      if (
        !sessionTriggerRef.current?.contains(target) &&
        !sessionPopoverRef.current?.contains(target)
      ) {
        setSessionMenuOpen(false);
      }
    };
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setSessionMenuOpen(false);
      }
    };
    document.addEventListener("mousedown", handlePointerDown);
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("mousedown", handlePointerDown);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [sessionMenuOpen]);

  useEffect(() => {
    if (!canOpenSessionMenu) {
      setSessionMenuOpen(false);
    }
  }, [canOpenSessionMenu]);

  // The portaled menu escapes the header/sidebar overflow:hidden and opens from
  // the left control toward the pane center.
  const positionSessionMenu = useCallback(() => {
    const trigger = sessionTriggerRef.current;
    const popover = sessionPopoverRef.current;
    if (!trigger || !popover) {
      return;
    }
    const { height } = popover.getBoundingClientRect();
    setSessionPos(
      placePanePopover({
        triggerRect: trigger.getBoundingClientRect(),
        popoverSize: {
          width: SESSION_MENU_PREFERRED_WIDTH,
          height: Math.min(height, SESSION_MENU_MAX_HEIGHT),
        },
        paneRect: turnPaneRectFrom(trigger),
        align: "start",
        prefer: "below",
      }),
    );
  }, []);

  useLayoutEffect(() => {
    if (!sessionMenuOpen) {
      setSessionPos(null);
      return;
    }
    positionSessionMenu();
    const onReflow = () => positionSessionMenu();
    window.addEventListener("resize", onReflow);
    window.addEventListener("scroll", onReflow, true);
    return () => {
      window.removeEventListener("resize", onReflow);
      window.removeEventListener("scroll", onReflow, true);
    };
  }, [sessionMenuOpen, positionSessionMenu, sessionOptions.length]);

  const selectTranscript = (path: string | null) => {
    setSessionMenuOpen(false);
    onSelectTranscript(path);
  };

  const copySessionId = async () => {
    if (!sessionId) {
      return;
    }
    try {
      await writeClipboardText(sessionId);
      setToast("Copied session id");
    } catch {
      setToast("Couldn’t copy session id");
    }
    if (toastTimer.current !== null) {
      window.clearTimeout(toastTimer.current);
    }
    toastTimer.current = window.setTimeout(() => {
      setToast(null);
      toastTimer.current = null;
    }, COPIED_TOAST_MS);
  };

  return (
    <div className="turn-pane-header">
      <div className="turn-pane-session-control">
        {canOpenSessionMenu ? (
          <button
            ref={sessionTriggerRef}
            type="button"
            className="link-button turn-pane-session turn-pane-session-trigger"
            title="Session actions"
            aria-haspopup="menu"
            aria-expanded={sessionMenuOpen}
            onClick={() => setSessionMenuOpen((open) => !open)}
          >
            {sessionLabel}
          </button>
        ) : (
          <span className="turn-pane-session">{sessionLabel}</span>
        )}
        {sessionMenuOpen
          ? createPortal(
              <div
                ref={sessionPopoverRef}
                className="popover-surface turn-pane-session-menu"
                role="menu"
                style={
                  sessionPos
                    ? {
                        left: sessionPos.left,
                        top: sessionPos.top,
                        maxHeight: Math.min(SESSION_MENU_MAX_HEIGHT, sessionPos.maxHeight),
                        width: Math.min(SESSION_MENU_PREFERRED_WIDTH, sessionPos.maxWidth),
                        maxWidth: sessionPos.maxWidth,
                      }
                    : { left: -9999, top: -9999 }
                }
              >
                <button
                  type="button"
                  role="menuitem"
                  className="menu-item turn-pane-session-menu-item"
                  disabled={!sessionId}
                  onClick={() => {
                    setSessionMenuOpen(false);
                    void copySessionId();
                  }}
                >
                  Copy Session ID
                </button>
                {sessionOptions.length > 0 ? (
                  <>
                    <div className="menu-divider turn-pane-session-menu-divider" role="separator" />
                    <div className="turn-pane-session-menu-label">Select Session</div>
                    <div
                      className="turn-pane-session-list"
                      role="group"
                      aria-label="Select Session"
                    >
                      {sessionOptions.map((option) => {
                        const active = option.path === transcriptPath;
                        return (
                          <button
                            key={option.path}
                            type="button"
                            role="menuitemcheckbox"
                            aria-checked={active}
                            className={`menu-item turn-pane-session-menu-item session-menu-item${
                              active ? " is-active" : ""
                            }`}
                            onClick={() => selectTranscript(active ? null : option.path)}
                          >
                            <span className="session-menu-title">{sessionMenuTitle(option)}</span>
                            <span className="session-menu-meta">
                              {formatRelativeTime(option.modifiedMs)}
                              {option.boundToOtherAgent ? " · In use" : ""}
                            </span>
                          </button>
                        );
                      })}
                    </div>
                  </>
                ) : null}
              </div>,
              document.body,
            )
          : null}
      </div>
      <div className="turn-pane-header-controls">
        <PromptLibraryMenu
          agentId={agentId}
          onInsert={onInsertPrompt}
          projectDir={promptProjectDir}
          projectPath={promptProjectPath}
        />
        <NotificationJournalMenu
          entries={notificationLog}
          showNotifications={showNotifications}
          onShowNotificationsChange={onShowNotificationsChange}
          onMarkRead={onMarkNotificationRead}
          onMarkAllRead={onMarkAllNotificationsRead}
          onClear={onClearNotification}
          onOpenPane={onOpenNotificationPane}
        />
        <button
          type="button"
          className={`control-button turn-pane-header-button${
            stickyUserMessages ? " is-active" : ""
          }`}
          title={
            stickyUserMessages
              ? "Unpin user message from top of transcripts"
              : "Pin user message at top of transcripts"
          }
          aria-label={
            stickyUserMessages
              ? "Unpin user message from top of transcripts"
              : "Pin user message at top of transcripts"
          }
          aria-pressed={stickyUserMessages}
          onClick={onToggleStickyUserMessages}
        >
          <Pin size={14} aria-hidden="true" />
        </button>
        {showQueueSplit ? (
          <button
            type="button"
            className={`control-button turn-pane-header-button${queueSplit ? " is-active" : ""}`}
            title={queueSplit ? "Use floating queue" : "Split transcript and queue"}
            aria-label={queueSplit ? "Use floating queue" : "Split transcript and queue"}
            aria-pressed={queueSplit}
            onClick={onToggleQueueSplit}
          >
            <SquareCenterlineDashedVertical size={14} aria-hidden="true" />
          </button>
        ) : null}
        <button
          type="button"
          className={`control-button turn-pane-header-button${browserOpen ? " is-active" : ""}`}
          title={browserOpen ? "Hide browser" : "Show browser"}
          aria-label={browserOpen ? "Hide browser" : "Show browser"}
          aria-pressed={browserOpen}
          onClick={onToggleBrowser}
        >
          <Globe size={14} aria-hidden="true" />
        </button>
        {artifactCount > 0 && onToggleArtifactTray ? (
          <button
            type="button"
            className={`control-button turn-pane-header-button artifact-tray-toggle${
              artifactTrayOpen ? " is-active" : ""
            }${artifactPulse ? " is-pulsing" : ""}`}
            title={artifactTrayOpen ? "Hide artifact tray" : "Show artifact tray"}
            aria-label={artifactTrayOpen ? "Hide artifact tray" : "Show artifact tray"}
            aria-pressed={artifactTrayOpen}
            onClick={onToggleArtifactTray}
          >
            <Paperclip size={14} aria-hidden="true" />
            <span className="artifact-tray-badge">{artifactCount}</span>
          </button>
        ) : null}
        {showTerminalPipToggle ? (
          <button
            type="button"
            className={`control-button turn-pane-header-button${
              terminalPipEnabled ? " is-active" : ""
            }`}
            title={terminalPipEnabled ? "Hide terminal preview" : "Show terminal preview"}
            aria-label={
              terminalPipEnabled
                ? "Hide terminal picture in picture"
                : "Show terminal picture in picture"
            }
            aria-pressed={terminalPipEnabled}
            onClick={onToggleTerminalPip}
          >
            <PictureInPicture2 size={14} aria-hidden="true" />
          </button>
        ) : null}
        <button
          type="button"
          className={`control-button turn-pane-header-button${transcriptExpanded ? " is-active" : ""}`}
          title={
            `${transcriptExpanded ? "Restore transcript" : "Expand transcript"} (${transcriptShortcutLabel})`
          }
          aria-label={transcriptExpanded ? "Restore transcript" : "Expand transcript"}
          aria-pressed={transcriptExpanded}
          onClick={onToggleTranscriptExpanded}
        >
          {transcriptExpanded ? (
            <Minimize2 size={14} aria-hidden="true" />
          ) : (
            <Expand size={14} aria-hidden="true" />
          )}
        </button>
        <div
          className={`turn-pane-sidebar-controls${
            onRestoreLeftSidebar || onOpenTerminalMap ? " is-grouped" : ""
          }`}
        >
          {onRestoreLeftSidebar ? (
            <button
              type="button"
              className="icon-button turn-pane-header-button"
              title="Show left sidebar (⇧⌘G)"
              aria-label="Show left sidebar"
              onClick={onRestoreLeftSidebar}
            >
              <PanelLeftOpen size={14} aria-hidden="true" />
            </button>
          ) : null}
          {onOpenTerminalMap ? (
            <TerminalMapButton
              className="icon-button turn-pane-header-button"
              pressed={terminalMapOpen}
              onClick={onOpenTerminalMap}
            />
          ) : null}
          <button
            type="button"
            className="icon-button turn-pane-header-button"
            title="Collapse right bar (⇧⌘L)"
            aria-label="Collapse right bar"
            onClick={onCollapseRightBar}
          >
            <PanelRightClose size={14} aria-hidden="true" />
          </button>
        </div>
      </div>
      {/* Portaled to <body> so the fixed-position toast escapes the header's
          stacking context (position:absolute + z-index), which would otherwise
          trap it and keep it from showing — unlike the composer toast, whose
          wrapper sets no z-index. */}
      {toast
        ? createPortal(
            <div className="composer-toast" role="status" aria-live="polite">
              {toast}
            </div>,
            document.body,
          )
        : null}
    </div>
  );
}
