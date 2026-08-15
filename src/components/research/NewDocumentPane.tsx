import DocumentComposer from "./DocumentComposer";
import {
  ResearchDocumentFrame,
  ResearchSidebarRestoreButton,
} from "./ResearchDocumentChrome";

interface NewDocumentPaneProps {
  /** Kept mounted while hidden so a draft survives surface switches. */
  hidden?: boolean;
  initialMarkdown?: string;
  workspaceId: string | null;
  onClose: () => void;
  onCreate: (input: {
    markdown: string;
    title: string | null;
    workspaceId: string | null;
  }) => Promise<void>;
  onDirtyChange?: (dirty: boolean) => void;
  onShowSidebar?: () => void;
  onOpenTerminalMap?: () => void;
  terminalMapOpen?: boolean;
  sessionDraftKey?: string;
}

/** Full-pane composer for adding a pasted or imported Markdown document as a
 * root-level research item. Renders in the research surface with the standard
 * document chrome and the composer form in the answer column of the response
 * grid, leaving the follow-up column empty. Workspace resolution remains this
 * wrapper's job; field behavior and validation are shared with document
 * editing. */
export default function NewDocumentPane({
  hidden = false,
  initialMarkdown = "",
  workspaceId,
  onClose,
  onCreate,
  onDirtyChange,
  onShowSidebar,
  onOpenTerminalMap,
  terminalMapOpen,
  sessionDraftKey,
}: NewDocumentPaneProps) {
  return (
    <ResearchDocumentFrame
      title="New document"
      hidden={hidden}
      headerActions={
        onShowSidebar ? (
          <ResearchSidebarRestoreButton
            onClick={onShowSidebar}
            onOpenTerminalMap={onOpenTerminalMap}
            terminalMapOpen={terminalMapOpen}
          />
        ) : undefined
      }
    >
      <article className="research-document-scroll">
        <div className="research-document-content">
          <div className="research-response-grid">
            <section className="research-response" aria-label="New document">
              <DocumentComposer
                mode="create"
                variant="page"
                visible={!hidden}
                initialMarkdown={initialMarkdown}
                resetKey={initialMarkdown}
                sessionDraftKey={sessionDraftKey}
                onClose={onClose}
                onDirtyChange={onDirtyChange}
                onSubmit={({ markdown, title }) =>
                  onCreate({ markdown, title, workspaceId })
                }
              />
            </section>
          </div>
        </div>
      </article>
    </ResearchDocumentFrame>
  );
}
