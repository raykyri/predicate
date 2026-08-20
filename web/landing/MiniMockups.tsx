// Four miniature replicas of single app surfaces, shown side by side above the
// full window replica in the hero. Each one freezes a moment the real app
// produces — a tab menu mid-open, the artifact tray, the composer's slash
// typeahead, a transcript scrolled far back — using the app's own class names
// and tokens (shared with .app-mockup in mockupCss.ts).
//
// They are illustrations, not enhancements: every control is a <span>, each
// stage is aria-hidden behind its caption, and no script touches them.
import React from "react";
import {
  Columns2Icon,
  FolderGit2Icon,
  GitBranchIcon,
  GitForkIcon,
  MessageSquareTextIcon,
  PanelBottomCloseIcon,
  PaperclipIcon,
  RotateCcwIcon,
  XIcon,
} from "./icons";
import { ARTIFACTS } from "./mockupData";

function MiniModule({
  label,
  sub,
  fade = false,
  children,
}: {
  label: string;
  sub: string;
  /** Fade the content into the stage at the fold instead of a hard crop. */
  fade?: boolean;
  children: React.ReactNode;
}) {
  return (
    <div className="mini-mockup">
      <div className={`mini-mockup-stage${fade ? " has-fade" : ""}`} aria-hidden="true">
        <div className="mini-mockup-stage-inner">{children}</div>
      </div>
      <strong className="mini-mockup-label">{label}</strong>
      <span className="mini-mockup-sub">{sub}</span>
    </div>
  );
}

// A tab's right-click menu with "Open worktree" under the pointer — the whole
// reason the command is never forgotten.
function WorktreesMini() {
  return (
    <div className="pane-context-menu">
      <div className="pane-context-actions">
        <span className="context-menu-item">
          <RotateCcwIcon size={13} />
          <span>Reset title</span>
        </span>
        <span className="context-menu-item context-menu-has-shortcut">
          <PanelBottomCloseIcon size={13} />
          <span>Split terminal</span>
          <kbd className="context-menu-shortcut">⌘D</kbd>
        </span>
        <span className="context-menu-item context-menu-has-shortcut">
          <Columns2Icon size={13} />
          <span>Split terminal to the right</span>
          <kbd className="context-menu-shortcut">⌘⇧D</kbd>
        </span>
        <span className="context-menu-item is-selected">
          <FolderGit2Icon size={13} />
          <span>Open worktree</span>
        </span>
        <div className="context-menu-divider" role="separator" />
        <span className="context-menu-item">
          <GitBranchIcon size={13} />
          <span>Fork session</span>
        </span>
        <span className="context-menu-item">
          <GitBranchIcon size={13} />
          <span>Fork session in split</span>
        </span>
        <span className="context-menu-item">
          <GitBranchIcon size={13} />
          <span>Fork session in worktree</span>
        </span>
        <span className="context-menu-item">
          <MessageSquareTextIcon size={13} />
          <span>Export to Research…</span>
        </span>
        <div className="context-menu-divider" role="separator" />
        <span className="context-menu-item context-menu-danger">
          <XIcon size={13} />
          <span>Close tab</span>
        </span>
      </div>
    </div>
  );
}

// The tray of files the agent opened with `qmux open`.
function ArtifactsMini() {
  return (
    <div className="artifact-tray">
      <div className="artifact-tray-titlebar">
        <PaperclipIcon className="lucide artifact-tray-clip" size={11} />
        <span className="artifact-tray-label">Artifacts</span>
        <span className="artifact-tray-chrome-button">
          <XIcon size={12} />
        </span>
      </div>
      <div className="artifact-tray-body">
        {ARTIFACTS.map((artifact) => (
          <span className="artifact-tray-row" key={artifact.name}>
            <span className="artifact-tray-name">{artifact.name}</span>
            <span className="artifact-tray-meta">{artifact.meta}</span>
          </span>
        ))}
      </div>
    </div>
  );
}

// Typing a slash in the composer raises the command typeahead over the
// transcript; "/fo" still matches both commands.
function SlashCommandsMini() {
  return (
    <div className="mini-composer">
      <div className="composer-slash-popover">
        <div className="composer-slash-list">
          <span className="composer-slash-option is-selected">
            <span className="composer-slash-icon">
              <GitForkIcon size={12} strokeWidth={1.75} />
            </span>
            <span className="composer-slash-token">/fork</span>
            <span className="composer-slash-summary">Fork this session</span>
          </span>
          <span className="composer-slash-option">
            <span className="composer-slash-icon">
              <FolderGit2Icon size={12} strokeWidth={1.75} />
            </span>
            <span className="composer-slash-token">/worktree</span>
            <span className="composer-slash-summary">Fork into a new worktree</span>
          </span>
        </div>
      </div>
      <div className="mini-composer-field">
        <span className="mini-composer-text">/fo</span>
        <span className="mini-composer-caret" />
      </div>
    </div>
  );
}

// A transcript scrolled back to its earliest turns; the scrollbar thumb sits
// near the top of a very long thread.
function KeepWorkingMini() {
  return (
    <div className="mini-scrollback">
      <div className="mini-scrollback-thread">
        <div className="turn-card role-user">
          <header>
            <span className="turn-card-role-label">You</span>
          </header>
          <div className="turn-blocks">
            <div className="turn-message-block">
              <p className="turn-text">
                streamed responses sometimes drop the final chunk when the connection
                closes mid-frame
              </p>
            </div>
          </div>
        </div>
        <div className="turn-card role-assistant">
          <header>
            <span className="turn-card-role-label">Codex</span>
          </header>
          <div className="turn-blocks">
            <div className="turn-message-block">
              <p className="turn-text">
                The reader finishes a frame as soon as its buffer runs dry, so a short
                trailing chunk never flushes. I'll hold the tail until the stream
                closes.
              </p>
            </div>
          </div>
        </div>
        <div className="activity-group-block is-root-activity">
          <div className="activity-summary">
            <span className="activity-group-label is-tool-group">Called 6 tools</span>
          </div>
        </div>
        <div className="turn-card role-user">
          <header>
            <span className="turn-card-role-label">You</span>
          </header>
          <div className="turn-blocks">
            <div className="turn-message-block">
              <p className="turn-text">
                add a regression that closes the stream after half a frame
              </p>
            </div>
          </div>
        </div>
        <div className="turn-card role-assistant">
          <header>
            <span className="turn-card-role-label">Codex</span>
          </header>
          <div className="turn-blocks">
            <div className="turn-message-block">
              <p className="turn-text">
                The new test fails on main and passes with the buffered flush; the rest
                of the suite stays green.
              </p>
            </div>
          </div>
        </div>
      </div>
      <span className="mini-scrollback-thumb" />
    </div>
  );
}

export default function FeatureMiniMockups() {
  return (
    <div className="mini-mockups">
      <MiniModule
        label="Worktrees"
        sub="Never forget the command for creating a worktree again"
        fade
      >
        <WorktreesMini />
      </MiniModule>
      <MiniModule label="Artifacts" sub="View mockups and documents at a glance" fade>
        <ArtifactsMini />
      </MiniModule>
      <MiniModule label="Slash commands" sub="Fork sessions right from the message input">
        <SlashCommandsMini />
      </MiniModule>
      <MiniModule
        label="History view"
        sub="Scroll back thousands of messages, even across auto-compactions"
        fade
      >
        <KeepWorkingMini />
      </MiniModule>
    </div>
  );
}
