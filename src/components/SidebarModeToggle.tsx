import { BookOpen, Library, SquareTerminal } from "lucide-react";
import type { SidebarMode } from "../lib/sidebarMode";

interface SidebarModeToggleProps {
  mode: SidebarMode;
  shortcutHintsShown: boolean;
  runningResearchCount: number;
  unseenResearchCount: number;
  failedResearchCount: number;
  onChange: (mode: SidebarMode) => void;
}

export default function SidebarModeToggle({
  mode,
  shortcutHintsShown,
  runningResearchCount,
  unseenResearchCount,
  failedResearchCount,
  onChange,
}: SidebarModeToggleProps) {
  return (
    <div className="sidebar-mode-toggle" role="tablist" aria-label="Sidebar mode">
      <button
        type="button"
        role="tab"
        aria-selected={mode === "terminal"}
        className={`control-button${mode === "terminal" ? " is-selected" : ""}`}
        onClick={() => onChange("terminal")}
      >
        <SquareTerminal size={14} aria-hidden="true" />
        <span>Terminal</span>
        {shortcutHintsShown && mode !== "terminal" ? (
          <span
            className="pane-tab-shortcut-hint sidebar-mode-shortcut-hint"
            aria-hidden="true"
          >
            ⌘`
          </span>
        ) : null}
      </button>
      <button
        type="button"
        role="tab"
        aria-selected={mode === "research"}
        className={`control-button${mode === "research" ? " is-selected" : ""}`}
        onClick={() => onChange("research")}
      >
        <BookOpen size={14} aria-hidden="true" />
        <span>Research</span>
        {shortcutHintsShown && mode !== "research" ? (
          <span
            className="pane-tab-shortcut-hint sidebar-mode-shortcut-hint"
            aria-hidden="true"
          >
            ⌘`
          </span>
        ) : null}
        {runningResearchCount > 0 ? (
          <span
            className="sidebar-mode-attention is-running"
            title={`${runningResearchCount} research run${runningResearchCount === 1 ? "" : "s"} in progress`}
          >
            {runningResearchCount}
          </span>
        ) : null}
        {failedResearchCount > 0 ? (
          <span
            className="sidebar-mode-attention is-failed"
            title={`${failedResearchCount} research item${failedResearchCount === 1 ? "" : "s"} with new failures`}
            aria-label={`${failedResearchCount} research item${failedResearchCount === 1 ? "" : "s"} with new failures`}
          >
            !
          </span>
        ) : null}
        {unseenResearchCount > 0 ? (
          <span
            className="sidebar-mode-attention is-unseen"
            title={`${unseenResearchCount} research item${unseenResearchCount === 1 ? " has" : "s have"} unseen results`}
            aria-label={`${unseenResearchCount} research item${unseenResearchCount === 1 ? " has" : "s have"} unseen results`}
          >
            •
          </span>
        ) : null}
      </button>
      <button
        type="button"
        role="tab"
        aria-selected={mode === "encyclopedia"}
        className={`control-button${mode === "encyclopedia" ? " is-selected" : ""}`}
        onClick={() => onChange("encyclopedia")}
      >
        <Library size={14} aria-hidden="true" />
        <span>Encyclopedia</span>
        {shortcutHintsShown && mode !== "encyclopedia" ? (
          <span
            className="pane-tab-shortcut-hint sidebar-mode-shortcut-hint"
            aria-hidden="true"
          >
            ⌘`
          </span>
        ) : null}
      </button>
    </div>
  );
}
