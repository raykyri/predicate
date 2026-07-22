import { useEffect, useMemo, useRef, useState } from "react";
import { BookOpen, LoaderCircle, RefreshCw } from "lucide-react";
import { IS_MAC } from "../../lib/appHelpers";
import { ResearchHistoryNav } from "./ResearchDocumentChrome";
import { useHistoryNavigationInput } from "./useHistoryNavigationInput";
import TranscriptMarkdown, {
  TranscriptLinkActionsProvider,
  type LinkActions,
} from "../TranscriptMarkdown";
import { encyclopediaReadPage } from "../../lib/api";
import {
  parseEncyclopediaHref,
  type EncyclopediaPageContent,
} from "../../lib/encyclopedia";
import { formatRelativeTime } from "../../lib/transcriptSessions";

// Same ceiling as research documents: pages are agent-written files, so an
// oversized one degrades to bounded plain text instead of freezing the parse.
const OVERSIZED_MARKDOWN_POLICY = {
  maxCharacters: 300_000,
  maxDisplayCharacters: 300_000,
  fallbackClassName: "research-plaintext",
} as const;

// Renders one encyclopedia page from the workspace's `encyclopedia/` folder.
// Link routing: citations (`/research/<treeId>/<nodeId>`) jump to the cited
// chat, sibling `.md` links open that page here, and everything else takes
// the app's ordinary link path. `refreshToken` re-fetches after an update run
// lands new content for the already-open page. History lives in App (the
// component remounts per page); this surface mirrors the research document's
// navigation affordances — header arrows, Cmd/Ctrl+[ and ], Alt+arrows,
// mouse back/forward buttons, and the horizontal wheel swipe.
export default function EncyclopediaPage({
  workspaceId,
  fileName,
  refreshToken,
  linkActions,
  canGoBack,
  canGoForward,
  onBack,
  onForward,
  onOpenPage,
  onOpenCitation,
}: {
  workspaceId: string;
  fileName: string;
  refreshToken: number;
  linkActions: LinkActions;
  canGoBack: boolean;
  canGoForward: boolean;
  onBack: () => void;
  onForward: () => void;
  onOpenPage: (fileName: string) => void;
  onOpenCitation: (treeId: string, nodeId: string) => void;
}) {
  const [page, setPage] = useState<EncyclopediaPageContent | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const scrollRef = useRef<HTMLDivElement | null>(null);
  useHistoryNavigationInput(scrollRef, onBack, onForward);

  useEffect(() => {
    let disposed = false;
    setLoading(true);
    setError(null);
    encyclopediaReadPage(workspaceId, fileName)
      .then((content) => {
        if (!disposed) {
          setPage(content);
        }
      })
      .catch((err: unknown) => {
        if (!disposed) {
          setPage(null);
          setError(String(err));
        }
      })
      .finally(() => {
        if (!disposed) {
          setLoading(false);
        }
      });
    return () => {
      disposed = true;
    };
  }, [workspaceId, fileName, refreshToken]);

  const actions = useMemo<LinkActions>(
    () => ({
      openLink: (url) => {
        const parsed = parseEncyclopediaHref(url);
        if (parsed.kind === "citation") {
          onOpenCitation(parsed.treeId, parsed.nodeId);
        } else if (parsed.kind === "page") {
          onOpenPage(parsed.fileName);
        } else {
          linkActions.openLink(parsed.url);
        }
      },
      openLinkMenu: (url, x, y) => {
        // Internal navigation offers no meaningful "open in browser" menu;
        // only real external links get the app's link context menu.
        if (parseEncyclopediaHref(url).kind === "external") {
          linkActions.openLinkMenu(url, x, y);
        }
      },
    }),
    [linkActions, onOpenCitation, onOpenPage],
  );

  return (
    <div className="encyclopedia-page" aria-label="Encyclopedia page">
      <header className="encyclopedia-page-header">
        <span className="encyclopedia-page-lead">
          <ResearchHistoryNav
            canGoBack={canGoBack}
            canGoForward={canGoForward}
            backTitle={`Back (${IS_MAC ? "⌘[" : "Ctrl+["})`}
            forwardTitle={`Forward (${IS_MAC ? "⌘]" : "Ctrl+]"})`}
            onBack={onBack}
            onForward={onForward}
          />
          <span className="encyclopedia-page-file" title={fileName}>
            <BookOpen size={13} aria-hidden="true" />
            {fileName}
          </span>
        </span>
        {page && page.updatedAt > 0 ? (
          <span className="encyclopedia-page-updated" title="Last written">
            <RefreshCw size={11} aria-hidden="true" />
            {formatRelativeTime(page.updatedAt)}
          </span>
        ) : null}
      </header>
      <div className="encyclopedia-page-scroll" ref={scrollRef}>
      {loading && !page ? (
        <div className="encyclopedia-page-placeholder">
          <LoaderCircle className="research-spinner" size={16} aria-hidden="true" />
          <span>Loading page…</span>
        </div>
      ) : null}
      {error ? (
        <div className="encyclopedia-page-placeholder" role="alert">
          <span>{error}</span>
        </div>
      ) : null}
      {page ? (
        <article className="encyclopedia-page-body">
          <TranscriptLinkActionsProvider actions={actions}>
            <TranscriptMarkdown
              text={page.markdown}
              imageBehavior="open"
              oversizedContent={OVERSIZED_MARKDOWN_POLICY}
            />
          </TranscriptLinkActionsProvider>
        </article>
      ) : null}
      </div>
    </div>
  );
}
