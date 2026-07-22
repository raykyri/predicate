import { BookOpen, LoaderCircle, RefreshCw, Square } from "lucide-react";
import type { EncyclopediaStatus } from "../../lib/encyclopedia";

// Sidebar block for the workspace encyclopedia: the generated pages, the
// update state, and the enable/auto-update controls. Pages live on disk in
// the workspace's `encyclopedia/` folder; this only renders what the status
// snapshot reports.
export default function EncyclopediaSection({
  status,
  activePageFileName,
  onSelectPage,
  onEnable,
  onSetAutoUpdate,
  onUpdateNow,
  onCancelUpdate,
}: {
  status: EncyclopediaStatus | null;
  activePageFileName: string | null;
  onSelectPage: (fileName: string) => void;
  onEnable: () => void;
  onSetAutoUpdate: (autoUpdate: boolean) => void;
  onUpdateNow: () => void;
  onCancelUpdate: () => void;
}) {
  if (!status) {
    return null;
  }
  if (!status.enabled) {
    return (
      <section className="encyclopedia-section" aria-label="Encyclopedia">
        <div className="research-sidebar-heading encyclopedia-heading">
          <span className="encyclopedia-heading-title">
            <BookOpen size={13} aria-hidden="true" />
            Encyclopedia
          </span>
        </div>
        <p className="encyclopedia-hint">
          Build a wiki of interlinked pages from this folder's chats and
          documents, updated as new material lands.
        </p>
        <button
          type="button"
          className="control-button encyclopedia-enable-button"
          onClick={onEnable}
        >
          Enable encyclopedia
        </button>
      </section>
    );
  }
  return (
    <section className="encyclopedia-section" aria-label="Encyclopedia">
      <div className="research-sidebar-heading encyclopedia-heading">
        <span className="encyclopedia-heading-title">
          <BookOpen size={13} aria-hidden="true" />
          Encyclopedia
        </span>
        {status.updating ? (
          <button
            type="button"
            className="control-button encyclopedia-heading-action"
            title="Cancel the running encyclopedia update"
            onClick={onCancelUpdate}
          >
            <LoaderCircle className="research-spinner" size={12} aria-hidden="true" />
            <Square size={9} aria-hidden="true" />
          </button>
        ) : (
          <button
            type="button"
            className="control-button encyclopedia-heading-action"
            title={
              status.pendingSourceCount > 0
                ? `Update now (${status.pendingSourceCount} new ${
                    status.pendingSourceCount === 1 ? "source" : "sources"
                  })`
                : "Nothing new since the last update"
            }
            disabled={status.pendingSourceCount === 0}
            onClick={onUpdateNow}
          >
            <RefreshCw size={12} aria-hidden="true" />
            {status.pendingSourceCount > 0 ? (
              <span className="encyclopedia-pending-count">
                {status.pendingSourceCount}
              </span>
            ) : null}
          </button>
        )}
      </div>
      {status.pages.length === 0 ? (
        <p className="encyclopedia-hint">
          {status.updating
            ? "Writing the first pages…"
            : status.pendingSourceCount > 0
              ? "No pages yet — update to write the first ones."
              : "No pages yet. Pages appear as chats complete."}
        </p>
      ) : (
        <div className="encyclopedia-pages" role="list">
          {status.pages.map((page) => (
            <div
              key={page.fileName}
              role="listitem"
              className={`research-sidebar-row encyclopedia-page-row${
                page.fileName === activePageFileName ? " is-selected" : ""
              }`}
            >
              <button
                type="button"
                className="control-button research-sidebar-select"
                aria-current={page.fileName === activePageFileName ? "page" : undefined}
                title={page.fileName}
                onClick={() => onSelectPage(page.fileName)}
              >
                <span className="research-sidebar-copy">
                  <span className="research-sidebar-title">
                    <BookOpen
                      className="research-sidebar-doc-icon"
                      size={12}
                      aria-hidden="true"
                    />
                    <span className="research-sidebar-title-text">{page.title}</span>
                  </span>
                </span>
              </button>
            </div>
          ))}
        </div>
      )}
      {status.lastError ? (
        <p className="encyclopedia-error" role="alert">
          {status.lastError}
        </p>
      ) : null}
      <label className="encyclopedia-auto-toggle">
        <input
          type="checkbox"
          checked={status.autoUpdate}
          onChange={(event) => onSetAutoUpdate(event.target.checked)}
        />
        Update automatically
      </label>
    </section>
  );
}
