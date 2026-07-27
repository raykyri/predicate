import { useEffect, useMemo, useRef, useState } from "react";
import type { GroupInfo, ResearchTreeDetail } from "../../types";
import {
  discardConversationArchive,
  importResearchConversations,
  listClaudeCodeSessions,
  listCodexSessions,
  listOpencodeSessions,
  pickImportFile,
  readConversationImportFile,
  readOpencodeSession,
  readStagedConversations,
  stageConversationArchive,
  type ConversationArchiveSummary,
  type HarnessSessionSummary,
  type ImportedConversationPayload,
  type ImportedConversationSource,
  type SkippedImport,
} from "../../lib/api";
import type {
  ImportWorkerRequest,
  ImportWorkerResponse,
} from "../../lib/import/importWorker";
import type { ImportedConversationDraft } from "../../lib/import/types";

// Import external conversations into Research: pick a source (claude.ai or
// ChatGPT export archive, local Claude Code or Codex sessions, or another
// harness's transcript file), select the conversations, pick a destination
// folder, and import them as read-only conversation trees. Parsing runs in a
// module Worker so multi-megabyte exports never stall the UI thread; archive
// payloads stay staged in the backend until fetched in small chunks.

interface ImportConversationsDialogProps {
  folders: GroupInfo[];
  /** The research folder currently scoped in the sidebar, preferred as the
   * destination so imports land where the user is looking. */
  defaultFolderId: string | null;
  /** A dropped .zip / conversations.json — staged immediately on mount. */
  initialArchivePath?: string;
  /** A dropped .jsonl — treated as a single Claude Code session selection. */
  initialTranscriptPath?: string;
  /** Maps the picked folder (or null when none exist yet) to a concrete
   * research workspace id, creating the default workspace on first use — the
   * same resolution the research composer uses. */
  onResolveWorkspace: (workspaceId: string | null) => Promise<string>;
  onClose: () => void;
  onImported: (result: {
    trees: ResearchTreeDetail[];
    folderName: string | null;
    skipped: SkippedImport[];
    warnings: string[];
  }) => void;
}

type ImportStep = "source" | "list" | "destination" | "summary";

/** Fetch/parse chunk size: keeps IPC transfers and worker parses incremental
 * so progress stays visible on large selections. */
const FETCH_CHUNK = 10;
/** The backend caps import batches at 200 conversations per call. */
const IMPORT_CHUNK = 200;

const SOURCE_LABELS: Record<ImportedConversationSource, string> = {
  claudeAi: "Claude",
  chatgpt: "ChatGPT",
  claudeCode: "Claude Code",
  codex: "Codex",
  hermes: "Hermes",
  lettaCode: "Letta Code",
  openclaw: "OpenClaw",
  opencode: "OpenCode",
  openhands: "OpenHands",
  pi: "Pi",
};

/** Sources that arrive as JSONL transcript files rather than staged export
 * archives — they read through readConversationImportFile at import time. */
type TranscriptSource = Exclude<ImportedConversationSource, "claudeAi" | "chatgpt">;

function isTranscriptSource(
  source: ImportedConversationSource,
): source is TranscriptSource {
  return source !== "claudeAi" && source !== "chatgpt";
}

/** Transcript sources with a first-class session browser (a known local
 * sessions tree the backend can scan). The rest are file-pick only.
 * OpenCode differs from the JSONL pair at import time: its sessions are
 * assembled backend-side via readOpencodeSession rather than read as one
 * transcript file. */
type SessionBrowserSource = "claudeCode" | "codex" | "opencode";

/** The file-pick-only formats offered under "Other transcript…". */
const OTHER_TRANSCRIPT_FORMATS: Array<
  Exclude<TranscriptSource, SessionBrowserSource>
> = ["openhands", "lettaCode", "pi", "openclaw", "hermes"];

function formatRowDate(epochMs: number | undefined): string | null {
  if (typeof epochMs !== "number" || !Number.isFinite(epochMs) || epochMs <= 0) {
    return null;
  }
  return new Date(epochMs).toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}

function defaultFolderName(source: ImportedConversationSource): string {
  const today = new Date().toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
  return `${SOURCE_LABELS[source]} import — ${today}`;
}

function sessionLabel(session: HarnessSessionSummary): string {
  return (
    session.preview?.trim() ||
    session.sessionId ||
    session.path.split("/").pop() ||
    session.path
  );
}

export default function ImportConversationsDialog({
  folders,
  defaultFolderId,
  initialArchivePath,
  initialTranscriptPath,
  onResolveWorkspace,
  onClose,
  onImported,
}: ImportConversationsDialogProps) {
  const [step, setStep] = useState<ImportStep>("source");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [archive, setArchive] = useState<ConversationArchiveSummary | null>(null);
  const [sessions, setSessions] = useState<HarnessSessionSummary[] | null>(null);
  const [source, setSource] = useState<ImportedConversationSource | null>(null);
  // The format picked in the "Other transcript…" control on the source step.
  const [otherFormat, setOtherFormat] = useState<TranscriptSource>(
    OTHER_TRANSCRIPT_FORMATS[0],
  );
  const [search, setSearch] = useState("");
  // Selection keys: staged archive rows use String(index); Claude Code
  // sessions use their transcript path.
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [workspaceId, setWorkspaceId] = useState<string | null>(
    (defaultFolderId && folders.some((folder) => folder.id === defaultFolderId)
      ? defaultFolderId
      : null) ??
      folders[0]?.id ??
      null,
  );
  const [folderName, setFolderName] = useState("");
  // Grouping a multi-conversation import is a choice, not a default the user
  // has to clear a text field to escape.
  const [groupInFolder, setGroupInFolder] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [progress, setProgress] = useState<string | null>(null);
  const [summary, setSummary] = useState<{
    imported: number;
    skipped: SkippedImport[];
    warnings: string[];
  } | null>(null);

  const tokenRef = useRef<string | null>(null);
  const workerRef = useRef<Worker | null>(null);
  const pendingParsesRef = useRef(
    new Map<
      number,
      {
        resolve: (value: { drafts: ImportedConversationDraft[]; errors: string[] }) => void;
        reject: (reason: unknown) => void;
      }
    >(),
  );
  const parseRequestSeqRef = useRef(0);
  const dialogRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    setWorkspaceId((current) =>
      current && folders.some((folder) => folder.id === current)
        ? current
        : (folders[0]?.id ?? null),
    );
  }, [folders]);

  // Same focus-retry cadence as ExportToResearchDialog: the native terminal
  // only yields keyboard ownership once the dialog is registered as a blocking
  // overlay, which can land after mount.
  useEffect(() => {
    const focusDialog = (force: boolean) => {
      const dialog = dialogRef.current;
      if (!dialog) {
        return;
      }
      if (force || !dialog.contains(document.activeElement)) {
        const target = dialog.querySelector<HTMLElement>(
          "input, select, button:not([disabled])",
        );
        (target ?? dialog).focus();
      }
    };
    focusDialog(true);
    const frame = requestAnimationFrame(() => focusDialog(false));
    const settle = window.setTimeout(() => focusDialog(false), 100);
    return () => {
      cancelAnimationFrame(frame);
      window.clearTimeout(settle);
    };
  }, [step]);

  // Release the backend staging slot and the parse worker with the dialog.
  // discardConversationArchive is safe with a stale token, so unconditional
  // cleanup covers cancel, close-on-success, and unmount alike.
  useEffect(() => {
    return () => {
      workerRef.current?.terminate();
      workerRef.current = null;
      const failure = new Error("Import dialog closed");
      for (const pending of pendingParsesRef.current.values()) {
        pending.reject(failure);
      }
      pendingParsesRef.current.clear();
      const token = tokenRef.current;
      if (token) {
        tokenRef.current = null;
        void discardConversationArchive(token).catch(() => undefined);
      }
    };
  }, []);

  function parseInWorker(
    format: ImportedConversationSource,
    payloads: string[],
  ): Promise<{ drafts: ImportedConversationDraft[]; errors: string[] }> {
    if (!workerRef.current) {
      const worker = new Worker(
        new URL("../../lib/import/importWorker.ts", import.meta.url),
        { type: "module" },
      );
      worker.onmessage = (event: MessageEvent<ImportWorkerResponse>) => {
        const pending = pendingParsesRef.current.get(event.data.requestId);
        if (pending) {
          pendingParsesRef.current.delete(event.data.requestId);
          pending.resolve({ drafts: event.data.drafts, errors: event.data.errors });
        }
      };
      worker.onerror = (event) => {
        const failure = new Error(event.message || "Conversation parsing failed");
        for (const pending of pendingParsesRef.current.values()) {
          pending.reject(failure);
        }
        pendingParsesRef.current.clear();
      };
      workerRef.current = worker;
    }
    const requestId = ++parseRequestSeqRef.current;
    return new Promise((resolve, reject) => {
      pendingParsesRef.current.set(requestId, { resolve, reject });
      const request: ImportWorkerRequest = { requestId, format, payloads };
      workerRef.current?.postMessage(request);
    });
  }

  async function stageArchivePath(path: string) {
    const staged = await stageConversationArchive(path);
    // Staging a new archive server-side replaces the previous slot, but keep
    // the token bookkeeping exact so cleanup discards the live stage.
    tokenRef.current = staged.token;
    setArchive(staged);
    setSessions(null);
    // The backend sniffs the real format, so a "Claude" click on a ChatGPT
    // zip still parses correctly.
    setSource(staged.format);
    setSelected(new Set());
    setSearch("");
    setStep("list");
  }

  function adoptTranscriptPath(path: string, transcriptSource: TranscriptSource) {
    const fileName = path.split("/").pop() ?? path;
    setArchive(null);
    setSessions([
      {
        projectSlug: fileName,
        path,
        modifiedMs: Date.now(),
      },
    ]);
    setSource(transcriptSource);
    setSelected(new Set([path]));
    setSearch("");
    setStep("destination");
  }

  async function runSourceAction(action: () => Promise<void>) {
    if (busy) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await action();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  const chooseArchive = () =>
    runSourceAction(async () => {
      const path = await pickImportFile("archive");
      if (path) {
        await stageArchivePath(path);
      }
    });

  const chooseSessionBrowser = (browserSource: SessionBrowserSource) =>
    runSourceAction(async () => {
      const listed = await (browserSource === "codex"
        ? listCodexSessions()
        : browserSource === "opencode"
          ? listOpencodeSessions()
          : listClaudeCodeSessions());
      setArchive(null);
      setSessions(listed);
      setSource(browserSource);
      setSelected(new Set());
      setSearch("");
      setStep("list");
    });

  const chooseTranscriptFile = (transcriptSource: TranscriptSource) =>
    runSourceAction(async () => {
      const path = await pickImportFile("transcript");
      if (path) {
        adoptTranscriptPath(path, transcriptSource);
      }
    });

  // Pre-staged starting points from drag-drop.
  useEffect(() => {
    if (initialArchivePath) {
      void runSourceAction(() => stageArchivePath(initialArchivePath));
    } else if (initialTranscriptPath) {
      adoptTranscriptPath(initialTranscriptPath, "claudeCode");
    }
    // Mount-only: the props describe the drop that opened the dialog.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const normalizedSearch = search.trim().toLowerCase();

  const filteredConversations = useMemo(() => {
    const conversations = archive?.conversations ?? [];
    if (!normalizedSearch) {
      return conversations;
    }
    return conversations.filter((meta) =>
      meta.title.toLowerCase().includes(normalizedSearch),
    );
  }, [archive, normalizedSearch]);

  const filteredSessions = useMemo(() => {
    const all = sessions ?? [];
    if (!normalizedSearch) {
      return all;
    }
    return all.filter((session) =>
      `${session.projectDir ?? session.projectSlug} ${sessionLabel(session)}`
        .toLowerCase()
        .includes(normalizedSearch),
    );
  }, [sessions, normalizedSearch]);

  const sessionGroups = useMemo(() => {
    const groups = new Map<string, HarnessSessionSummary[]>();
    for (const session of filteredSessions) {
      const key = session.projectDir ?? session.projectSlug;
      const existing = groups.get(key);
      if (existing) {
        existing.push(session);
      } else {
        groups.set(key, [session]);
      }
    }
    return [...groups.entries()];
  }, [filteredSessions]);

  // Keys the select-all checkbox governs: the filtered, selectable rows.
  const filteredSelectableKeys = useMemo(() => {
    if (source && isTranscriptSource(source)) {
      return filteredSessions.map((session) => session.path);
    }
    return filteredConversations
      .filter((meta) => meta.messageCount > 0)
      .map((meta) => String(meta.index));
  }, [source, filteredSessions, filteredConversations]);

  const allFilteredSelected =
    filteredSelectableKeys.length > 0 &&
    filteredSelectableKeys.every((key) => selected.has(key));

  function toggleSelected(key: string) {
    setSelected((current) => {
      const next = new Set(current);
      if (next.has(key)) {
        next.delete(key);
      } else {
        next.add(key);
      }
      return next;
    });
  }

  function toggleSelectAll() {
    setSelected((current) => {
      const next = new Set(current);
      if (allFilteredSelected) {
        for (const key of filteredSelectableKeys) {
          next.delete(key);
        }
      } else {
        for (const key of filteredSelectableKeys) {
          next.add(key);
        }
      }
      return next;
    });
  }

  function continueToDestination() {
    if (selected.size === 0 || !source) {
      return;
    }
    setFolderName((current) => current || defaultFolderName(source));
    setError(null);
    setStep("destination");
  }

  async function runImport() {
    if (submitting || !source) {
      return;
    }
    setSubmitting(true);
    setError(null);
    try {
      const groupId = await onResolveWorkspace(workspaceId);
      const warnings: string[] = [];
      const skipped: SkippedImport[] = [];
      const payloads: ImportedConversationPayload[] = [];

      if (isTranscriptSource(source)) {
        // OpenCode sessions are assembled backend-side from the store's
        // metadata/message/part files; the JSONL harnesses read one
        // transcript file directly.
        const readSession =
          source === "opencode" ? readOpencodeSession : readConversationImportFile;
        const chosen = (sessions ?? []).filter((session) => selected.has(session.path));
        for (let start = 0; start < chosen.length; start += FETCH_CHUNK) {
          const chunk = chosen.slice(start, start + FETCH_CHUNK);
          setProgress(
            `Reading ${Math.min(start + chunk.length, chosen.length)}/${chosen.length}…`,
          );
          const texts = await Promise.all(
            chunk.map((session) => readSession(session.path)),
          );
          // One worker request per transcript so a parse failure attributes
          // to its session instead of collapsing the whole chunk.
          const parsed = await Promise.all(
            texts.map((text) => parseInWorker(source, [text])),
          );
          parsed.forEach(({ drafts, errors }, offset) => {
            const session = chunk[offset];
            for (const message of errors) {
              skipped.push({ title: sessionLabel(session), error: message });
            }
            for (const draft of drafts) {
              warnings.push(...draft.warnings);
              payloads.push({
                title: draft.title ?? session.preview ?? null,
                createdAt: draft.createdAt,
                turns: draft.turns,
              });
            }
          });
        }
      } else if (archive && tokenRef.current) {
        const token = tokenRef.current;
        const chosen = archive.conversations.filter((meta) =>
          selected.has(String(meta.index)),
        );
        for (let start = 0; start < chosen.length; start += FETCH_CHUNK) {
          const chunk = chosen.slice(start, start + FETCH_CHUNK);
          setProgress(
            `Reading ${Math.min(start + chunk.length, chosen.length)}/${chosen.length}…`,
          );
          const slices = await readStagedConversations(
            token,
            chunk.map((meta) => meta.index),
          );
          // One request per slice for exact skip attribution; the worker still
          // does all the parsing, so the chunk stays off the main thread.
          const parsed = await Promise.all(
            slices.map((slice) => parseInWorker(source, [slice])),
          );
          parsed.forEach(({ drafts, errors }, offset) => {
            const meta = chunk[offset];
            for (const message of errors) {
              skipped.push({ title: meta.title, error: message });
            }
            for (const draft of drafts) {
              warnings.push(...draft.warnings);
              payloads.push({
                title: draft.title ?? meta.title,
                createdAt: draft.createdAt ?? meta.createdAt ?? null,
                turns: draft.turns,
              });
            }
          });
        }
      }

      const trees: ResearchTreeDetail[] = [];
      for (let start = 0; start < payloads.length; start += IMPORT_CHUNK) {
        const chunk = payloads.slice(start, start + IMPORT_CHUNK);
        setProgress(
          `Importing ${Math.min(start + chunk.length, payloads.length)}/${payloads.length}…`,
        );
        const result = await importResearchConversations({
          groupId,
          source,
          conversations: chunk,
        });
        trees.push(...result.trees);
        skipped.push(...result.skipped);
      }
      setProgress(null);

      if (payloads.length === 0 && skipped.length === 0) {
        throw new Error("Nothing selected could be imported.");
      }

      // Grouping is opt-in; a checked box with a blanked field still groups,
      // under the prefilled default name.
      const trimmedFolderName = folderName.trim() || (source ? defaultFolderName(source) : "");
      onImported({
        trees,
        folderName: selected.size > 1 && groupInFolder ? trimmedFolderName : null,
        skipped,
        warnings,
      });
      if (skipped.length > 0 || warnings.length > 0) {
        // Keep the dialog up over the freshly adopted trees so the user sees
        // what did not make it across.
        setSummary({ imported: trees.length, skipped, warnings });
        setSubmitting(false);
        setStep("summary");
      } else {
        onClose();
      }
    } catch (err) {
      setProgress(null);
      setError(err instanceof Error ? err.message : String(err));
      setSubmitting(false);
    }
  }

  const selectedCount = selected.size;
  const sourceLabel = source ? SOURCE_LABELS[source] : null;

  return (
    <div
      className="confirm-dialog-backdrop"
      role="presentation"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget && !submitting) {
          onClose();
        }
      }}
    >
      <div
        ref={dialogRef}
        className={`confirm-dialog export-research-dialog import-conversations-dialog${
          step === "source" ? " is-source-step" : ""
        }`}
        role="dialog"
        aria-modal="true"
        aria-label="Import conversations"
        tabIndex={-1}
        onKeyDown={(event) => {
          if (event.key === "Escape" && !submitting) {
            event.preventDefault();
            event.stopPropagation();
            onClose();
          }
        }}
      >
        <p>
          <strong>Import conversations</strong>
          {sourceLabel && step !== "source" ? ` — ${sourceLabel}` : null}
        </p>

        {step === "source" ? (
          <>
            <p className="export-research-note">
              Bring past conversations into Research as read-only items. Pick a
              data export archive, or import sessions straight from a local
              coding agent.
            </p>
            <div className="import-conversations-sources">
              <button
                className="control-button"
                type="button"
                disabled={busy}
                onClick={() => void chooseArchive()}
              >
                Claude export (.zip)
              </button>
              <button
                className="control-button"
                type="button"
                disabled={busy}
                onClick={() => void chooseArchive()}
              >
                ChatGPT export (.zip)
              </button>
              <button
                className="control-button"
                type="button"
                disabled={busy}
                onClick={() => void chooseSessionBrowser("claudeCode")}
              >
                Claude Code sessions
              </button>
              <button
                className="control-button"
                type="button"
                disabled={busy}
                onClick={() => void chooseSessionBrowser("codex")}
              >
                Codex sessions
              </button>
              <button
                className="control-button"
                type="button"
                disabled={busy}
                onClick={() => void chooseSessionBrowser("opencode")}
              >
                OpenCode sessions
              </button>
            </div>
            <div className="import-conversations-other">
              <span className="import-conversations-other-label">
                Other transcript
              </span>
              <select
                value={otherFormat}
                aria-label="Transcript format"
                disabled={busy}
                onChange={(event) =>
                  setOtherFormat(event.currentTarget.value as TranscriptSource)
                }
              >
                {OTHER_TRANSCRIPT_FORMATS.map((format) => (
                  <option key={format} value={format}>
                    {SOURCE_LABELS[format]}
                  </option>
                ))}
              </select>
              <button
                className="control-button"
                type="button"
                disabled={busy}
                onClick={() => void chooseTranscriptFile(otherFormat)}
              >
                Choose file…
              </button>
            </div>
          </>
        ) : null}

        {step === "list" ? (
          <>
            <div className="import-conversations-toolbar">
              <input
                className="export-research-input"
                type="search"
                value={search}
                placeholder="Filter conversations"
                aria-label="Filter conversations"
                onChange={(event) => setSearch(event.currentTarget.value)}
              />
              <label className="import-conversations-select-all">
                <input
                  type="checkbox"
                  checked={allFilteredSelected}
                  disabled={filteredSelectableKeys.length === 0}
                  aria-label="Select all listed conversations"
                  onChange={toggleSelectAll}
                />
                <span>All</span>
              </label>
            </div>
            <div className="import-conversations-list" role="group" aria-label="Conversations">
              {source && isTranscriptSource(source) ? (
                sessionGroups.length === 0 ? (
                  <p className="import-conversations-empty">
                    {normalizedSearch
                      ? "No sessions match the filter."
                      : `No ${sourceLabel} sessions found.`}
                  </p>
                ) : (
                  sessionGroups.map(([project, groupSessions]) => (
                    <div key={project} className="import-conversations-group">
                      <div className="import-conversations-group-label" title={project}>
                        {project}
                      </div>
                      {groupSessions.map((session) => (
                        <label key={session.path} className="import-conversations-row">
                          <input
                            type="checkbox"
                            checked={selected.has(session.path)}
                            onChange={() => toggleSelected(session.path)}
                          />
                          <span className="import-conversations-row-title">
                            {sessionLabel(session)}
                          </span>
                          <span className="import-conversations-row-meta">
                            {formatRowDate(session.modifiedMs)}
                          </span>
                        </label>
                      ))}
                    </div>
                  ))
                )
              ) : filteredConversations.length === 0 ? (
                <p className="import-conversations-empty">
                  {normalizedSearch
                    ? "No conversations match the filter."
                    : "The archive contains no conversations."}
                </p>
              ) : (
                filteredConversations.map((meta) => {
                  const key = String(meta.index);
                  const disabled = meta.messageCount === 0;
                  const date = formatRowDate(meta.createdAt ?? meta.updatedAt);
                  return (
                    <label
                      key={key}
                      className={`import-conversations-row${disabled ? " is-disabled" : ""}`}
                    >
                      <input
                        type="checkbox"
                        checked={selected.has(key)}
                        disabled={disabled}
                        onChange={() => toggleSelected(key)}
                      />
                      <span className="import-conversations-row-title">{meta.title}</span>
                      <span className="import-conversations-row-meta">
                        {date ? `${date} · ` : ""}
                        {meta.messageCount} message{meta.messageCount === 1 ? "" : "s"}
                      </span>
                    </label>
                  );
                })
              )}
            </div>
            {source && isTranscriptSource(source) && source !== "opencode" ? (
              // Not offered for OpenCode: a hand-picked file cannot be
              // assembled — sessions import via the store browser above.
              <button
                className="control-button import-conversations-secondary"
                type="button"
                disabled={busy}
                onClick={() => void chooseTranscriptFile(source)}
              >
                Choose .jsonl file…
              </button>
            ) : null}
            <p className="import-conversations-count" aria-live="polite">
              {selectedCount} selected
            </p>
          </>
        ) : null}

        {step === "destination" ? (
          <>
            <p className="export-research-note">
              {selectedCount} conversation{selectedCount === 1 ? "" : "s"} will be
              imported as read-only research items.
            </p>
            {folders.length > 1 ? (
              <label className="export-research-field">
                <span>Research folder</span>
                <select
                  value={workspaceId ?? ""}
                  aria-label="Research folder"
                  onChange={(event) => setWorkspaceId(event.currentTarget.value || null)}
                >
                  {folders.map((folder) => (
                    <option key={folder.id} value={folder.id}>
                      {folder.name}
                    </option>
                  ))}
                </select>
              </label>
            ) : null}
            {selectedCount > 1 ? (
              <>
                <label className="import-conversations-group-toggle">
                  <input
                    type="checkbox"
                    checked={groupInFolder}
                    aria-label="Group the imported conversations into a folder"
                    onChange={(event) => setGroupInFolder(event.currentTarget.checked)}
                  />
                  <span>Group into a folder</span>
                </label>
                {groupInFolder ? (
                  <label className="export-research-field">
                    <span>Folder name</span>
                    <input
                      className="export-research-input"
                      type="text"
                      value={folderName}
                      aria-label="Folder name for the imported conversations"
                      onChange={(event) => setFolderName(event.currentTarget.value)}
                    />
                  </label>
                ) : null}
              </>
            ) : null}
            {progress ? (
              <p className="import-conversations-progress" aria-live="polite">
                {progress}
              </p>
            ) : null}
          </>
        ) : null}

        {step === "summary" && summary ? (
          <>
            <p className="export-research-note">
              Imported {summary.imported} conversation
              {summary.imported === 1 ? "" : "s"}.
              {summary.skipped.length > 0
                ? ` ${summary.skipped.length} could not be imported:`
                : null}
            </p>
            {summary.skipped.length > 0 ? (
              <ul className="import-conversations-summary-list">
                {summary.skipped.map((entry, index) => (
                  <li key={`${entry.title}-${index}`}>
                    <strong>{entry.title || "Untitled"}</strong> — {entry.error}
                  </li>
                ))}
              </ul>
            ) : null}
            {summary.warnings.length > 0 ? (
              <ul className="import-conversations-summary-list is-warnings">
                {summary.warnings.map((warning, index) => (
                  <li key={`${warning}-${index}`}>{warning}</li>
                ))}
              </ul>
            ) : null}
          </>
        ) : null}

        {error ? (
          <p className="export-research-error" role="alert">
            {error}
          </p>
        ) : null}

        <div className="confirm-dialog-actions">
          {step === "summary" ? (
            <button className="control-button" type="button" onClick={onClose}>
              Done
            </button>
          ) : (
            <>
              <button
                className="control-button"
                type="button"
                disabled={submitting}
                onClick={onClose}
              >
                Cancel
              </button>
              {step === "list" ? (
                <button
                  className="control-button"
                  type="button"
                  disabled={selectedCount === 0}
                  onClick={continueToDestination}
                >
                  Continue
                </button>
              ) : null}
              {step === "destination" ? (
                <button
                  className="control-button"
                  type="button"
                  disabled={submitting || selectedCount === 0}
                  onClick={() => void runImport()}
                >
                  {submitting ? (progress ?? "Importing…") : "Import"}
                </button>
              ) : null}
            </>
          )}
        </div>
      </div>
    </div>
  );
}
