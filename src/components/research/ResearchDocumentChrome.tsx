import type { ReactNode } from "react";
import { ArrowLeft, ArrowRight, PanelLeftOpen } from "lucide-react";
import TerminalMapButton from "../TerminalMapButton";

interface ResearchHistoryNavProps {
  canGoBack?: boolean;
  canGoForward?: boolean;
  backTitle?: string;
  forwardTitle?: string;
  onBack?: () => void;
  onForward?: () => void;
}

/** Browser-style back/forward pair at the left edge of a research document
 * header. Renders disabled when no handlers/ability are supplied, which is
 * the whole state for placeholder and composer headers. */
export function ResearchHistoryNav({
  canGoBack = false,
  canGoForward = false,
  backTitle,
  forwardTitle,
  onBack,
  onForward,
}: ResearchHistoryNavProps) {
  return (
    <div className="research-history-nav" aria-label="Research history">
      <button
        type="button"
        className="control-button research-history-button"
        disabled={!canGoBack}
        title={backTitle}
        aria-label="Back"
        onClick={onBack}
      >
        <ArrowLeft size={16} aria-hidden="true" />
      </button>
      <button
        type="button"
        className="control-button research-history-button"
        disabled={!canGoForward}
        title={forwardTitle}
        aria-label="Forward"
        onClick={onForward}
      >
        <ArrowRight size={16} aria-hidden="true" />
      </button>
    </div>
  );
}

interface ResearchDocumentFrameProps {
  /** Single inert breadcrumb entry naming the page. */
  title: string;
  hidden?: boolean;
  headerActions?: ReactNode;
  children: ReactNode;
}

interface ResearchSidebarRestoreButtonProps {
  onClick: () => void;
  onOpenTerminalMap?: () => void;
  terminalMapOpen?: boolean;
}

/** Reopens the app sidebar from research chrome, alongside the document's
 * transcript and run controls rather than floating over the research body. */
export function ResearchSidebarRestoreButton({
  onClick,
  onOpenTerminalMap,
  terminalMapOpen = false,
}: ResearchSidebarRestoreButtonProps) {
  const restore = (
    <button
      type="button"
      className="control-button research-sidebar-restore"
      title="Show left sidebar (⇧⌘G)"
      aria-label="Show left sidebar"
      onClick={onClick}
    >
      <PanelLeftOpen size={15} aria-hidden="true" />
    </button>
  );
  if (!onOpenTerminalMap) {
    return restore;
  }
  return (
    <div className="research-sidebar-controls is-grouped">
      {restore}
      <TerminalMapButton
        className="control-button research-sidebar-restore"
        size={15}
        pressed={terminalMapOpen}
        onClick={onOpenTerminalMap}
      />
    </div>
  );
}

/** The research-surface page chrome shared by states that are not a live
 * document: the standard header with inert history controls and a one-entry
 * breadcrumb, wrapping whatever body the state renders. The live document
 * view keeps its own header (interactive breadcrumb and run controls) but
 * shares the nav component above. */
export function ResearchDocumentFrame({
  title,
  hidden = false,
  headerActions,
  children,
}: ResearchDocumentFrameProps) {
  return (
    <div className="research-workspace" hidden={hidden}>
      <main className="research-document">
        <header className="research-document-header">
          <ResearchHistoryNav />
          <div className="research-breadcrumb" aria-label="Research path">
            <span>
              <button className="control-button" type="button" disabled>
                {title}
              </button>
            </span>
          </div>
          {headerActions}
        </header>
        {children}
      </main>
    </div>
  );
}
