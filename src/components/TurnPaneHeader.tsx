import {
  Expand,
  GitBranch,
  Globe,
  Minimize2,
  PanelLeftOpen,
  PanelRightClose,
  SquareCenterlineDashedVertical,
} from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { placePanePopover, turnPaneRectFrom } from "../lib/appHelpers";
import { writeClipboardText } from "../lib/clipboard";
import PromptLibraryMenu from "./PromptLibraryMenu";
import { formatRelativeTime, sessionMenuTitle } from "../lib/transcriptSessions";
import type { TranscriptJumpTarget } from "../lib/turnTimeline";
import type { BranchInfo, TranscriptOption } from "../types";

// How long the "copied" toast stays up after copying the session id.
const COPIED_TOAST_MS = 1600;

// Preferred natural widths for the header menus; placement clamps them to the pane.
const SESSION_MENU_PREFERRED_WIDTH = 320;
// Wider than the fork actions alone need: the menu also lists sibling branches,
// whose rows carry a prompt preview.
const FORK_MENU_PREFERRED_WIDTH = 300;

// The top bar across the right pane: the active session's id on the left, and
// session/browser/transcript controls on the right. Forking is only enabled for
// supported sessions with a live id. Its height matches the browser overlay's
// address bar so the two read as a single chrome line when the browser is open.
interface TurnPaneHeaderProps {
  agentId?: string | null;
  // The active agent's session id, or null before SessionStart lands.
  sessionId: string | null;
  // Sessions in this agent's folder for the top-left session switcher; the
  // active one is whichever matches transcriptPath.
  transcriptOptions: TranscriptOption[];
  transcriptPath: string | null;
  onSelectTranscript: (path: string | null) => void;
  // Only supported sessions with a valid session id can be forked.
  canFork: boolean;
  // Fork the session into a child tab of the current one, optionally in a fresh
  // git worktree.
  onFork: (options: { nest: boolean; useWorktree: boolean }) => void;
  // Sibling sessions in this agent's fork lineage, root first. Empty until the
  // agent has a session id, or when it has never been forked.
  branches: BranchInfo[];
  // Focuses another branch, reopening its pane if it was closed.
  onSelectBranch: (branch: BranchInfo) => void;
  // The recent user prompts offered by the menu's "Go to…" section, oldest
  // first. Empty when the pane has no transcript to jump around in.
  jumpTargets: TranscriptJumpTarget[];
  // Scrolls the transcript to a prompt by its message key.
  onJumpToMessage: (messageKey: string) => void;
  showQueueSplit: boolean;
  queueSplit: boolean;
  onToggleQueueSplit: () => void;
  browserOpen: boolean;
  onToggleBrowser: () => void;
  transcriptExpanded: boolean;
  transcriptShortcutLabel: string;
  onToggleTranscriptExpanded: () => void;
  onCollapseRightBar: () => void;
  // Reopens the collapsed app sidebar from this header. Present only while the
  // sidebar is collapsed and this header occupies the stage's top right — the
  // floating stage control would otherwise render on top of these buttons.
  onShowSidebar?: () => void;
  // Inserts saved-prompt text into this pane's composer; absent when the pane
  // has no agent composer, which disables the prompt-library trigger.
  onInsertPrompt?: (text: string) => void;
  // The pane's project directory (keys the prompt library's Project scope) and
  // its home-relative display form (shown beside the Project heading).
  promptProjectDir?: string | null;
  promptProjectPath?: string | null;
}

// A branch is named by its first prompt when one has been recorded. Before that
// (a fork spawned but not yet talked to) fall back to a short session id, then to
// a placeholder for a fork whose SessionStart hook hasn't landed.
function branchTitle(branch: BranchInfo): string {
  const preview = branch.preview?.trim();
  if (preview) {
    return preview;
  }
  const sessionId = branch.sessionId?.trim();
  return sessionId ? `Session ${sessionId.slice(0, 8)}` : "Starting…";
}

function branchMeta(branch: BranchInfo): string {
  const parts = [branch.isRoot ? "Root" : "Fork"];
  if (branch.missing) {
    // Explains why the row is disabled.
    parts.push("Unavailable");
  } else if (!branch.live) {
    parts.push("Closed");
  }
  parts.push(formatRelativeTime(branch.lastActiveAt));
  return parts.join(" · ");
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
  transcriptOptions,
  transcriptPath,
  onSelectTranscript,
  canFork,
  onFork,
  branches,
  onSelectBranch,
  jumpTargets,
  onJumpToMessage,
  showQueueSplit,
  queueSplit,
  onToggleQueueSplit,
  browserOpen,
  onToggleBrowser,
  transcriptExpanded,
  transcriptShortcutLabel,
  onToggleTranscriptExpanded,
  onCollapseRightBar,
  onShowSidebar,
  onInsertPrompt,
  promptProjectDir,
  promptProjectPath,
}: TurnPaneHeaderProps) {
  const [menuOpen, setMenuOpen] = useState(false);
  const menuTriggerRef = useRef<HTMLButtonElement | null>(null);
  const menuPopoverRef = useRef<HTMLDivElement | null>(null);
  const [menuPos, setMenuPos] = useState<MenuPos | null>(null);
  const [sessionMenuOpen, setSessionMenuOpen] = useState(false);
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
  // The branch menu carries both the lineage list and the fork actions, so it
  // opens when either has something to offer. A lineage of one is just this
  // session, which the header already names — not worth a list.
  const siblingBranches = branches.length > 1 ? branches : [];
  // Jumping counts as a reason to open the menu on its own: a session that
  // cannot fork (no recorded session id yet) can still have a transcript worth
  // navigating.
  const canOpenBranchMenu = canFork || siblingBranches.length > 0 || jumpTargets.length > 0;

  // Clear any pending toast timer on unmount so it can't fire into a gone component.
  useEffect(() => {
    return () => {
      if (toastTimer.current !== null) {
        window.clearTimeout(toastTimer.current);
      }
    };
  }, []);

  // Close the fork menu on an outside click or Escape while it is open.
  useEffect(() => {
    if (!menuOpen) {
      return;
    }
    const handlePointerDown = (event: MouseEvent) => {
      const target = event.target as Node;
      if (
        !menuTriggerRef.current?.contains(target) &&
        !menuPopoverRef.current?.contains(target)
      ) {
        setMenuOpen(false);
      }
    };
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setMenuOpen(false);
      }
    };
    document.addEventListener("mousedown", handlePointerDown);
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("mousedown", handlePointerDown);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [menuOpen]);

  useEffect(() => {
    if (!canOpenBranchMenu) {
      setMenuOpen(false);
    }
  }, [canOpenBranchMenu]);

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

  // Portaled menus escape the header/sidebar overflow:hidden. Session opens from
  // the left control (grow right / toward center); fork from the right (grow left).
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
        popoverSize: { width: SESSION_MENU_PREFERRED_WIDTH, height },
        paneRect: turnPaneRectFrom(trigger),
        align: "start",
        prefer: "below",
      }),
    );
  }, []);

  const positionForkMenu = useCallback(() => {
    const trigger = menuTriggerRef.current;
    const popover = menuPopoverRef.current;
    if (!trigger || !popover) {
      return;
    }
    const { height } = popover.getBoundingClientRect();
    setMenuPos(
      placePanePopover({
        triggerRect: trigger.getBoundingClientRect(),
        popoverSize: { width: FORK_MENU_PREFERRED_WIDTH, height },
        paneRect: turnPaneRectFrom(trigger),
        align: "end",
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

  useLayoutEffect(() => {
    if (!menuOpen) {
      setMenuPos(null);
      return;
    }
    positionForkMenu();
    const onReflow = () => positionForkMenu();
    window.addEventListener("resize", onReflow);
    window.addEventListener("scroll", onReflow, true);
    return () => {
      window.removeEventListener("resize", onReflow);
      window.removeEventListener("scroll", onReflow, true);
    };
  }, [menuOpen, positionForkMenu]);

  const fork = (options: { nest: boolean; useWorktree: boolean }) => {
    setMenuOpen(false);
    onFork(options);
  };

  const selectBranch = (branch: BranchInfo) => {
    setMenuOpen(false);
    onSelectBranch(branch);
  };

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
            onClick={() => {
              setMenuOpen(false);
              setSessionMenuOpen((open) => !open);
            }}
          >
            {sessionId ? `Session: ${sessionId}` : "New session"}
          </button>
        ) : (
          <span className="turn-pane-session">New session</span>
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
                        maxHeight: sessionPos.maxHeight,
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
        <div className="turn-pane-fork">
          <button
            ref={menuTriggerRef}
            type="button"
            className="icon-button turn-pane-header-button"
            disabled={!canOpenBranchMenu}
            title={
              canOpenBranchMenu
                ? "Branches"
                : "Forking is available after a supported session id is recorded"
            }
            aria-label="Branches"
            aria-haspopup="menu"
            aria-expanded={canOpenBranchMenu ? menuOpen : false}
            onClick={() => {
              if (canOpenBranchMenu) {
                setSessionMenuOpen(false);
                setMenuOpen((open) => !open);
              }
            }}
          >
            <GitBranch size={14} aria-hidden="true" />
          </button>
          {canOpenBranchMenu && menuOpen
            ? createPortal(
                <div
                  ref={menuPopoverRef}
                  className="popover-surface turn-pane-fork-menu"
                  role="menu"
                  style={
                    menuPos
                      ? {
                          left: menuPos.left,
                          top: menuPos.top,
                          maxHeight: menuPos.maxHeight,
                          width: Math.min(FORK_MENU_PREFERRED_WIDTH, menuPos.maxWidth),
                          maxWidth: menuPos.maxWidth,
                        }
                      : { left: -9999, top: -9999 }
                  }
                >
                  {siblingBranches.length > 0 ? (
                    <>
                      <div className="turn-pane-branch-label">Branches</div>
                      <div
                        className="turn-pane-branch-list"
                        role="group"
                        aria-label="Switch branch"
                      >
                        {siblingBranches.map((branch) => {
                          const current = Boolean(agentId) && branch.agentId === agentId;
                          return (
                            <button
                              key={branch.recentSessionId ?? branch.agentId ?? branch.sessionId}
                              type="button"
                              role="menuitemcheckbox"
                              aria-checked={current}
                              // A missing branch has no transcript left to reopen;
                              // the current one is already on screen.
                              disabled={current || branch.missing}
                              className={`menu-item menu-item--compact turn-pane-branch-item${
                                current ? " is-active" : ""
                              }`}
                              title={branch.sessionId ?? undefined}
                              onClick={() => selectBranch(branch)}
                            >
                              <span className="turn-pane-branch-title">
                                {branchTitle(branch)}
                              </span>
                              <span className="turn-pane-branch-meta">
                                {branchMeta(branch)}
                              </span>
                            </button>
                          );
                        })}
                      </div>
                      <div className="menu-divider" role="separator" />
                    </>
                  ) : null}
                  <button
                    type="button"
                    role="menuitem"
                    className="menu-item menu-item--compact turn-pane-fork-item"
                    disabled={!canFork}
                    onClick={() => fork({ nest: true, useWorktree: false })}
                  >
                    Fork session
                  </button>
                  <button
                    type="button"
                    role="menuitem"
                    className="menu-item menu-item--compact turn-pane-fork-item"
                    disabled={!canFork}
                    onClick={() => fork({ nest: true, useWorktree: true })}
                  >
                    Fork session in worktree
                  </button>
                  {jumpTargets.length > 0 ? (
                    <>
                      <div className="menu-divider" role="separator" />
                      <div className="turn-pane-branch-label">Go to…</div>
                      <div
                        className="turn-pane-jump-list"
                        role="group"
                        aria-label="Go to message"
                      >
                        {jumpTargets.map((target) => (
                          <button
                            key={target.key}
                            type="button"
                            role="menuitem"
                            className="menu-item menu-item--compact turn-pane-jump-item"
                            title={target.text}
                            onClick={() => {
                              setMenuOpen(false);
                              onJumpToMessage(target.key);
                            }}
                          >
                            <span className="turn-pane-jump-text">{target.text}</span>
                          </button>
                        ))}
                      </div>
                    </>
                  ) : null}
                </div>,
                document.body,
              )
            : null}
        </div>
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
        <button
          type="button"
          className="icon-button turn-pane-header-button"
          title="Collapse right bar"
          aria-label="Collapse right bar"
          onClick={onCollapseRightBar}
        >
          <PanelRightClose size={14} aria-hidden="true" />
        </button>
        {onShowSidebar ? (
          <button
            type="button"
            className="icon-button turn-pane-header-button"
            title="Show left sidebar"
            aria-label="Show left sidebar"
            onClick={onShowSidebar}
          >
            <PanelLeftOpen size={14} aria-hidden="true" />
          </button>
        ) : null}
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
