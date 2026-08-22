import { useEffect, useMemo, useRef, useState } from "react";
import {
  AlertTriangle,
  Bot,
  GitBranch,
  History,
  LoaderCircle,
  RefreshCw,
  Search,
  X,
} from "lucide-react";
import { listConversationHistory } from "../lib/api";
import { formatRelativeTime } from "../lib/transcriptSessions";
import type {
  ConversationHistoryEntry,
  ConversationHistoryLaunchMode,
} from "../types";

interface ConversationHistoryDialogProps {
  open: boolean;
  launching: boolean;
  onClose: () => void;
  onFocusPane: (paneId: string) => void;
  onLaunch: (
    entry: ConversationHistoryEntry,
    mode: ConversationHistoryLaunchMode,
    prompt: string,
  ) => Promise<void>;
}

type AdapterFilter = "all" | "claude" | "codex";

export default function ConversationHistoryDialog({
  open,
  launching,
  onClose,
  onFocusPane,
  onLaunch,
}: ConversationHistoryDialogProps) {
  const [entries, setEntries] = useState<ConversationHistoryEntry[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [adapter, setAdapter] = useState<AdapterFilter>("all");
  const [prompt, setPrompt] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const searchRef = useRef<HTMLInputElement | null>(null);
  const dialogRef = useRef<HTMLElement | null>(null);
  const rowRefs = useRef(new Map<string, HTMLButtonElement>());

  const refresh = async () => {
    setLoading(true);
    setError(null);
    try {
      const next = await listConversationHistory();
      setEntries(next);
      setSelectedId((current) =>
        current && next.some((entry) => entry.id === current) ? current : (next[0]?.id ?? null),
      );
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    if (!open) return;
    setPrompt("");
    void refresh();
    requestAnimationFrame(() => searchRef.current?.focus());
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !launching) {
        event.preventDefault();
        event.stopPropagation();
        onClose();
        return;
      }
      if (event.key === "Tab") {
        const focusable = Array.from(
          dialogRef.current?.querySelectorAll<HTMLElement>(
            'button:not(:disabled), input:not(:disabled), textarea:not(:disabled), [tabindex="0"]',
          ) ?? [],
        ).filter((element) => element.getClientRects().length > 0);
        const first = focusable[0];
        const last = focusable[focusable.length - 1];
        if (first && last && event.shiftKey && document.activeElement === first) {
          event.preventDefault();
          last.focus();
        } else if (first && last && !event.shiftKey && document.activeElement === last) {
          event.preventDefault();
          first.focus();
        }
      }
    };
    window.addEventListener("keydown", onKeyDown, true);
    return () => window.removeEventListener("keydown", onKeyDown, true);
  }, [launching, onClose, open]);

  const filtered = useMemo(() => {
    const needle = query.trim().toLocaleLowerCase();
    return entries.filter((entry) => {
      if (adapter !== "all" && entry.adapter !== adapter) return false;
      if (!needle) return true;
      return [entry.title, entry.preview, entry.cwd, entry.sessionId, entry.model]
        .filter(Boolean)
        .some((value) => value!.toLocaleLowerCase().includes(needle));
    });
  }, [adapter, entries, query]);

  useEffect(() => {
    if (selectedId && filtered.some((entry) => entry.id === selectedId)) return;
    setSelectedId(filtered[0]?.id ?? null);
  }, [filtered, selectedId]);

  if (!open) return null;
  const selected = entries.find((entry) => entry.id === selectedId) ?? null;

  const launchEntry = async (
    entry: ConversationHistoryEntry,
    mode: ConversationHistoryLaunchMode,
    initialPrompt: string,
  ) => {
    if (launching || !entry.cwdExists) return;
    setError(null);
    try {
      await onLaunch(entry, mode, initialPrompt.trim());
      setPrompt("");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };

  const launch = (mode: ConversationHistoryLaunchMode) => {
    if (!selected) return Promise.resolve();
    return launchEntry(selected, mode, prompt);
  };

  const activateEntry = (entry: ConversationHistoryEntry) => {
    if (entry.active && entry.paneId) {
      onFocusPane(entry.paneId);
      return Promise.resolve();
    }
    return launchEntry(entry, "resume", "");
  };

  const moveSelection = (edgeOrDelta: "first" | "last" | number) => {
    if (filtered.length === 0) return;
    const currentIndex = Math.max(0, filtered.findIndex((entry) => entry.id === selectedId));
    const nextIndex =
      edgeOrDelta === "first"
        ? 0
        : edgeOrDelta === "last"
          ? filtered.length - 1
          : Math.min(filtered.length - 1, Math.max(0, currentIndex + edgeOrDelta));
    const next = filtered[nextIndex];
    setSelectedId(next.id);
    requestAnimationFrame(() => rowRefs.current.get(next.id)?.focus());
  };

  return (
    <div
      className="history-backdrop"
      role="presentation"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget && !launching) onClose();
      }}
    >
      <section
        ref={dialogRef}
        className="history-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="conversation-history-title"
      >
        <header className="history-header">
          <div>
            <h2 id="conversation-history-title">
              <History size={18} aria-hidden="true" /> Conversation history
            </h2>
            <p>Resume or branch conversations discovered in Claude and Codex.</p>
          </div>
          <button
            type="button"
            className="icon-button history-close"
            aria-label="Close conversation history"
            disabled={launching}
            onClick={onClose}
          >
            <X size={17} aria-hidden="true" />
          </button>
        </header>

        <div className="history-toolbar">
          <label className="history-search">
            <Search size={14} aria-hidden="true" />
            <input
              ref={searchRef}
              value={query}
              onChange={(event) => setQuery(event.currentTarget.value)}
              placeholder="Search title, project, session, or model"
              aria-label="Search conversation history"
            />
          </label>
          <div className="history-filters" role="group" aria-label="Agent filter">
            {(["all", "claude", "codex"] as const).map((value) => (
              <button
                key={value}
                type="button"
                className={adapter === value ? "is-selected" : ""}
                aria-pressed={adapter === value}
                onClick={() => setAdapter(value)}
              >
                {value === "all" ? "All" : value === "claude" ? "Claude" : "Codex"}
              </button>
            ))}
          </div>
          <button
            type="button"
            className="icon-button history-refresh"
            title="Refresh conversation history"
            aria-label="Refresh conversation history"
            disabled={loading || launching}
            onClick={() => void refresh()}
          >
            <RefreshCw size={14} className={loading ? "is-spinning" : ""} aria-hidden="true" />
          </button>
        </div>

        <div className="history-body">
          <div
            className="history-list"
            role="listbox"
            aria-label="Past conversations"
            onKeyDown={(event) => {
              if (event.key === "ArrowDown" || event.key === "ArrowUp") {
                event.preventDefault();
                moveSelection(event.key === "ArrowDown" ? 1 : -1);
              } else if (event.key === "Home" || event.key === "End") {
                event.preventDefault();
                moveSelection(event.key === "Home" ? "first" : "last");
              } else if (event.key === "Enter" && selected) {
                event.preventDefault();
                void activateEntry(selected);
              }
            }}
          >
            {loading && entries.length === 0 ? (
              <div className="history-empty"><LoaderCircle className="is-spinning" /> Scanning…</div>
            ) : filtered.length === 0 ? (
              <div className="history-empty">
                <History size={22} aria-hidden="true" />
                <strong>No conversations found</strong>
                <span>{query ? "Try another search." : "Start a Claude or Codex session first."}</span>
              </div>
            ) : (
              filtered.map((entry) => (
                <button
                  key={entry.id}
                  type="button"
                  role="option"
                  ref={(node) => {
                    if (node) rowRefs.current.set(entry.id, node);
                    else rowRefs.current.delete(entry.id);
                  }}
                  tabIndex={entry.id === selectedId ? 0 : -1}
                  aria-selected={entry.id === selectedId}
                  className={`history-row${entry.id === selectedId ? " is-selected" : ""}`}
                  onClick={() => setSelectedId(entry.id)}
                  onDoubleClick={() => void activateEntry(entry)}
                >
                  <span className={`history-agent history-agent--${entry.adapter}`}>
                    <Bot size={14} aria-hidden="true" />
                  </span>
                  <span className="history-row-copy">
                    <span className="history-row-title">{entry.title}</span>
                    <span className="history-row-meta">
                      {entry.active ? <em>Open</em> : null}
                      <span>{formatRelativeTime(entry.lastActiveAt)}</span>
                      <span>{entry.cwd}</span>
                    </span>
                  </span>
                </button>
              ))
            )}
          </div>

          <aside className="history-detail">
            {selected ? (
              <>
                <div className="history-detail-heading">
                  <span className={`history-agent history-agent--${selected.adapter}`}>
                    <Bot size={16} aria-hidden="true" />
                  </span>
                  <div>
                    <strong>{selected.title}</strong>
                    <span>{selected.adapter === "claude" ? "Claude Code" : "Codex"}</span>
                  </div>
                </div>
                {selected.preview && selected.preview !== selected.title ? (
                  <p className="history-preview">{selected.preview}</p>
                ) : null}
                <dl className="history-facts">
                  <div><dt>Project</dt><dd title={selected.cwd}>{selected.cwd}</dd></div>
                  <div><dt>Session</dt><dd title={selected.sessionId}>{selected.sessionId}</dd></div>
                  {selected.model ? <div><dt>Model</dt><dd>{selected.model}</dd></div> : null}
                  <div><dt>Activity</dt><dd>{formatRelativeTime(selected.lastActiveAt)}</dd></div>
                </dl>
                {!selected.cwdExists ? (
                  <div className="history-warning" role="status">
                    <AlertTriangle size={14} aria-hidden="true" />
                    The original working directory is missing. Restore it before resuming.
                  </div>
                ) : null}
                <label className="history-prompt">
                  <span>Optional first message</span>
                  <textarea
                    value={prompt}
                    onChange={(event) => setPrompt(event.currentTarget.value)}
                    placeholder="Continue with a new instruction…"
                    disabled={launching}
                  />
                </label>
                <div className="history-actions">
                  {selected.active && selected.paneId ? (
                    <button type="button" onClick={() => onFocusPane(selected.paneId!)}>
                      Open tab
                    </button>
                  ) : (
                    <button
                      type="button"
                      disabled={!selected.cwdExists || launching}
                      onClick={() => void launch("resume")}
                    >
                      {launching ? <LoaderCircle className="is-spinning" size={14} /> : null}
                      Resume
                    </button>
                  )}
                  <button
                    type="button"
                    disabled={!selected.cwdExists || launching}
                    onClick={() => void launch("fork")}
                  >
                    <GitBranch size={14} aria-hidden="true" /> Fork
                  </button>
                  <button
                    type="button"
                    disabled={!selected.cwdExists || launching}
                    onClick={() => void launch("forkWorktree")}
                  >
                    <GitBranch size={14} aria-hidden="true" /> Fork in worktree
                  </button>
                </div>
              </>
            ) : (
              <div className="history-empty">Select a conversation.</div>
            )}
          </aside>
        </div>
        {error ? <div className="history-error" role="alert">{error}</div> : null}
      </section>
    </div>
  );
}
