import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import {
  RESEARCH_DOCUMENT_BYTE_LIMIT,
  RESEARCH_DOCUMENT_WORD_LIMIT,
  ResearchDocumentWordLimitExceeded,
  countResearchDocumentWords,
  deriveResearchDocumentTitle,
} from "../../lib/researchDocuments";
import {
  ComposerSubmitShortcutGlyph,
  isComposerSubmitShortcut,
} from "../ComposerSubmitShortcut";
import {
  clearSessionDraft,
  loadSessionDraftJson,
  readSessionDraftJson,
  saveSessionDraftJson,
} from "../../lib/sessionDrafts";

interface DocumentComposerProps {
  mode: "create" | "edit";
  /** "dialog" renders the modal card over a backdrop; "page" renders just the
   * composer form, for embedding in the main research pane. */
  variant?: "dialog" | "page";
  /** False while the composer is mounted but display:none (a parked page
   * draft behind another surface); measurement effects wait for it. */
  visible?: boolean;
  initialMarkdown?: string;
  initialTitle?: string;
  highlightCount?: number;
  resetKey?: string;
  onClose: () => void;
  onSubmit: (input: { markdown: string; title: string | null }) => Promise<void>;
  /** Reports edits so the app can tell a pristine composer (safe to dismiss on
   * navigation) from one holding a draft. */
  onDirtyChange?: (dirty: boolean) => void;
  sessionDraftKey?: string;
}

/** Shared Markdown composer for new and existing research documents. The edit
 * variant is a modal dialog over the document (its backdrop and Escape only
 * dismiss while the fields are pristine); the create variant renders as a
 * main-pane page. Visibility is mount-controlled: render it to show it. */
export default function DocumentComposer({
  mode,
  variant = "dialog",
  visible = true,
  initialMarkdown = "",
  initialTitle = "",
  highlightCount = 0,
  resetKey = "",
  onClose,
  onSubmit,
  onDirtyChange,
  sessionDraftKey,
}: DocumentComposerProps) {
  const [initialSessionDraft] = useState(() =>
    sessionDraftKey
      ? readSessionDraftJson<{ markdown: string; title: string }>(sessionDraftKey)
      : null,
  );
  const [markdown, setMarkdown] = useState(initialSessionDraft?.markdown ?? "");
  const [title, setTitle] = useState(initialSessionDraft?.title ?? "");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [sessionDraftReady, setSessionDraftReady] = useState(
    !sessionDraftKey || initialSessionDraft !== null,
  );
  const sessionDraftTouchedRef = useRef(false);
  const initialResetRef = useRef(true);
  const markdownRef = useRef<HTMLTextAreaElement | null>(null);
  // Dirty reports go through a ref so the effects below (including the
  // unmount cleanup, which captures its closure once) always reach the
  // caller's current handler, not the first render's.
  const onDirtyChangeRef = useRef(onDirtyChange);
  onDirtyChangeRef.current = onDirtyChange;

  useEffect(() => {
    if (initialResetRef.current) {
      initialResetRef.current = false;
      if (initialSessionDraft) {
        return;
      }
    }
    setMarkdown(initialMarkdown);
    setTitle(initialTitle);
    setSubmitting(false);
    setError(null);
  }, [resetKey]);

  useEffect(() => {
    if (!sessionDraftKey || initialSessionDraft) {
      return;
    }
    let disposed = false;
    void loadSessionDraftJson<{ markdown: string; title: string }>(sessionDraftKey)
      .then((restored) => {
        if (!disposed && restored && !sessionDraftTouchedRef.current) {
          setMarkdown(restored.markdown);
          setTitle(restored.title);
        }
      })
      .catch(() => undefined)
      .finally(() => {
        if (!disposed) {
          setSessionDraftReady(true);
        }
      });
    return () => {
      disposed = true;
    };
  }, [initialSessionDraft, sessionDraftKey]);

  useEffect(() => {
    if (!sessionDraftKey || !sessionDraftReady) {
      return;
    }
    if (!markdown && !title) {
      clearSessionDraft(sessionDraftKey);
    } else {
      saveSessionDraftJson(sessionDraftKey, { markdown, title });
    }
  }, [markdown, sessionDraftKey, sessionDraftReady, title]);

  // Autogrow: the textarea tracks its content height. In the dialog variant
  // the card's max-height caps it — the flex layout shrinks the textarea back
  // down and its own scrollbar takes over once the cap is hit; the page
  // variant has no cap and extends the document scroller instead. Skipped
  // while hidden (scrollHeight reads 0 under display:none) and re-run when
  // the composer becomes visible again or the window resizes, so wrapping
  // changes that land while a page draft is parked don't leave a stale
  // height.
  useLayoutEffect(() => {
    const textarea = markdownRef.current;
    if (!visible || !textarea) {
      return;
    }
    const measure = () => {
      textarea.style.height = "auto";
      textarea.style.height = `${textarea.scrollHeight + 2}px`;
    };
    measure();
    window.addEventListener("resize", measure);
    return () => window.removeEventListener("resize", measure);
  }, [markdown, visible]);

  // These scan up to the 10 MB document cap. Counting stops immediately after
  // the first word over the limit so a dense import cannot monopolize the UI.
  const { wordCount, byteCount, derivedTitle, overWordLimit } = useMemo(() => {
    let wordCount: number;
    let overWordLimit = false;
    try {
      wordCount = countResearchDocumentWords(markdown, RESEARCH_DOCUMENT_WORD_LIMIT);
    } catch (caught) {
      if (!(caught instanceof ResearchDocumentWordLimitExceeded)) {
        throw caught;
      }
      wordCount = caught.count;
      overWordLimit = true;
    }
    return {
      wordCount,
      overWordLimit,
      byteCount: new TextEncoder().encode(markdown).length,
      derivedTitle: markdown.trim() ? deriveResearchDocumentTitle(markdown) : "",
    };
  }, [markdown]);

  const editing = mode === "edit";
  const changed = markdown !== initialMarkdown || title !== initialTitle;
  const pristine = editing ? !changed : !markdown.trim() && !title.trim();

  useEffect(() => {
    onDirtyChangeRef.current?.(!pristine);
  }, [pristine]);
  // A closing composer is no longer holding a draft.
  useEffect(() => () => onDirtyChangeRef.current?.(false), []);

  const overByteLimit = byteCount > RESEARCH_DOCUMENT_BYTE_LIMIT;
  const canSubmit =
    Boolean(markdown.trim()) &&
    !overWordLimit &&
    !overByteLimit &&
    !submitting &&
    (!editing || changed);
  const warningId = editing && highlightCount > 0 ? "edit-document-highlight-warning" : undefined;

  async function submit() {
    if (!canSubmit) {
      return;
    }
    setSubmitting(true);
    setError(null);
    try {
      await onSubmit({ markdown, title: title.trim() || null });
      close();
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setSubmitting(false);
    }
  }

  function close() {
    if (sessionDraftKey) {
      clearSessionDraft(sessionDraftKey);
    }
    onClose();
  }

  const dialog = variant === "dialog";
  const form = (
      <form
        className={`new-document-composer${dialog ? "" : " is-page"}`}
        role={dialog ? "dialog" : undefined}
        aria-modal={dialog || undefined}
        aria-label={editing ? "Edit document" : "New document"}
        aria-describedby={warningId}
        onKeyDown={(event) => {
          if (event.key === "Escape" && pristine && !submitting) {
            close();
          }
        }}
        onSubmit={(event) => {
          event.preventDefault();
          void submit();
        }}
      >
        {dialog ? <h2>{editing ? "Edit document" : "New document"}</h2> : null}
        <input
          className="new-document-title"
          type="text"
          value={title}
          placeholder={derivedTitle || "Title (uses the first line if left blank)"}
          aria-label="Document title"
          onChange={(event) => {
            sessionDraftTouchedRef.current = true;
            setTitle(event.currentTarget.value);
          }}
          onKeyDown={(event) => {
            // Document submission is deliberate (Cmd+Enter or the button),
            // never an implicit side effect of Enter in the title field.
            if (event.key === "Enter" && !isComposerSubmitShortcut(event, true)) {
              event.preventDefault();
            }
          }}
        />
        <textarea
          autoFocus
          ref={markdownRef}
          className="new-document-markdown"
          value={markdown}
          placeholder="Paste or write Markdown…"
          aria-label="Document markdown"
          onChange={(event) => {
            sessionDraftTouchedRef.current = true;
            setMarkdown(event.currentTarget.value);
          }}
          onKeyDown={(event) => {
            if (isComposerSubmitShortcut(event, true)) {
              event.preventDefault();
              void submit();
            }
          }}
        />
        <footer className="new-document-footer">
          {warningId ? (
            <p id={warningId} className="edit-document-highlight-warning">
              This document has {highlightCount.toLocaleString()} highlight
              {highlightCount === 1 ? "" : "s"}. Changing its content will erase{" "}
              {highlightCount === 1 ? "it" : "them"}. Title-only changes keep highlights.
            </p>
          ) : null}
          <div className="new-document-footer-row">
            <span
              className={`new-document-wordcount${overWordLimit || overByteLimit ? " is-over" : ""}`}
              role={overWordLimit || overByteLimit ? "alert" : undefined}
              title={
                overWordLimit
                  ? `Documents are limited to ${RESEARCH_DOCUMENT_WORD_LIMIT.toLocaleString()} words for now`
                  : undefined
              }
            >
              {wordCount.toLocaleString()} / {RESEARCH_DOCUMENT_WORD_LIMIT.toLocaleString()} words
              {overByteLimit ? " · over the 10 MB size limit" : ""}
            </span>
            {error ? (
              <p className="new-document-error" role="alert">
                {error}
              </p>
            ) : null}
            <div className="confirm-dialog-actions">
              <button className="control-button" type="button" disabled={submitting} onClick={close}>
                Cancel
              </button>
              <button className="control-button" type="submit" disabled={!canSubmit}>
                <span>
                  {submitting
                    ? editing
                      ? "Saving…"
                      : "Adding…"
                    : editing
                      ? "Save changes"
                      : "Add document"}
                </span>
                {!submitting ? (
                  <ComposerSubmitShortcutGlyph requireCmdEnter className="shortcut-hint" />
                ) : null}
              </button>
            </div>
          </div>
        </footer>
      </form>
  );

  if (!dialog) {
    return form;
  }
  return (
    <div
      className="confirm-dialog-backdrop new-document-backdrop"
      role="presentation"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget && pristine && !submitting) {
          close();
        }
      }}
    >
      {form}
    </div>
  );
}
