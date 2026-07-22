import { useEffect, useMemo, useState } from "react";
import { BookOpen, LoaderCircle, RefreshCw } from "lucide-react";
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
// lands new content for the already-open page.
export default function EncyclopediaPage({
  workspaceId,
  fileName,
  refreshToken,
  linkActions,
  onOpenPage,
  onOpenCitation,
}: {
  workspaceId: string;
  fileName: string;
  refreshToken: number;
  linkActions: LinkActions;
  onOpenPage: (fileName: string) => void;
  onOpenCitation: (treeId: string, nodeId: string) => void;
}) {
  const [page, setPage] = useState<EncyclopediaPageContent | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

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
          <header className="encyclopedia-page-header">
            <span className="encyclopedia-page-file" title={page.fileName}>
              <BookOpen size={13} aria-hidden="true" />
              {page.fileName}
            </span>
            {page.updatedAt > 0 ? (
              <span
                className="encyclopedia-page-updated"
                title="Last written"
              >
                <RefreshCw size={11} aria-hidden="true" />
                {formatRelativeTime(page.updatedAt)}
              </span>
            ) : null}
          </header>
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
  );
}
