import { memo, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { FormEvent, KeyboardEvent, MouseEvent } from "react";
import { createPortal } from "react-dom";
import {
  Copy,
  ExternalLink,
  LoaderCircle,
  MoreHorizontal,
  Play,
  RotateCw,
  Trash2,
  Undo2,
  X,
} from "lucide-react";
import type { JournalEntry, JournalTweetEntry } from "../../lib/journal";
import type {
  QuotedTweetSnapshot,
  TweetTextRun,
} from "../../lib/journalTweets";
import { openExternalUrl } from "../../lib/api";
import { writeClipboardText } from "../../lib/clipboard";
import { ResearchDocumentFrame } from "./ResearchDocumentChrome";

interface JournalPaneProps {
  /** Oldest first (storage order); the feed renders newest first. */
  entries: JournalEntry[];
  /** The most recently removed entry, still restorable. */
  pendingUndo: { entry: JournalEntry } | null;
  onAddEntry: (input: string) => void;
  onRemoveEntry: (id: string) => void;
  onRetryTweet: (id: string) => void;
  onUndoRemove: () => void;
  onDismissUndo: () => void;
}

const JOURNAL_MENU_WIDTH = 180;
const JOURNAL_MENU_HEIGHT_ESTIMATE = 132;
const JOURNAL_VIEWPORT_MARGIN = 8;

export type JournalMenuAction = "open" | "copy" | "retry" | "delete";

export interface JournalMenuItem {
  action: JournalMenuAction;
  label: string;
  /** Single-letter keycap shown in the menu; pressing it fires the item. */
  key: string;
  danger?: boolean;
}

/** The URL an entry stands for, if any: the canonical tweet permalink once
 * hydrated, otherwise what the user entered. Notes have none. */
export function journalEntryUrl(entry: JournalEntry): string | null {
  if (entry.kind === "link") {
    return entry.url;
  }
  if (entry.kind === "tweet") {
    return entry.tweet?.url ?? entry.url;
  }
  return null;
}

/** Context-menu items for an entry. Pure, so tests can pin the layout and
 * keycaps per entry kind without driving the portal menu. */
export function journalEntryMenuItems(entry: JournalEntry): JournalMenuItem[] {
  const items: JournalMenuItem[] = [];
  if (journalEntryUrl(entry)) {
    items.push({
      action: "open",
      label: entry.kind === "tweet" ? "Open on X" : "Open link",
      key: "O",
    });
  }
  items.push({
    action: "copy",
    label: entry.kind === "note" ? "Copy text" : "Copy link",
    key: "C",
  });
  if (entry.kind === "tweet" && entry.hydration !== "pending") {
    items.push({
      action: "retry",
      label: entry.hydration === "failed" ? "Retry tweet" : "Refresh tweet",
      key: "R",
    });
  }
  items.push({ action: "delete", label: "Delete", key: "D", danger: true });
  return items;
}

function menuItemIcon(action: JournalMenuAction) {
  switch (action) {
    case "open":
      return <ExternalLink size={13} aria-hidden="true" />;
    case "copy":
      return <Copy size={13} aria-hidden="true" />;
    case "retry":
      return <RotateCw size={13} aria-hidden="true" />;
    case "delete":
      return <Trash2 size={13} aria-hidden="true" />;
  }
}

function externalLinkClick(url: string) {
  return (event: MouseEvent) => {
    event.preventDefault();
    event.stopPropagation();
    void openExternalUrl(url);
  };
}

// No avatar (older snapshots, fetch-blocked images fall back via onError is
// out of scope) renders an initial on a disc whose hue derives from the
// handle — stable across renders and distinct between authors.
function avatarFallback(name: string, handle: string) {
  const seed = handle || name;
  let hash = 0;
  for (let i = 0; i < seed.length; i++) {
    hash = (hash * 31 + seed.charCodeAt(i)) >>> 0;
  }
  return {
    initial: [...(name || seed)][0]?.toUpperCase() ?? "?",
    color: `hsl(${hash % 360} 55% 45%)`,
  };
}

function TweetAvatar({
  name,
  handle,
  avatarUrl,
  size,
}: {
  name: string;
  handle: string;
  avatarUrl?: string;
  size: number;
}) {
  // A snapshot's avatar URL can go stale (profile changed, image deleted);
  // a failed load falls back to the initial disc instead of a broken image.
  const [failed, setFailed] = useState(false);
  if (avatarUrl && !failed) {
    return (
      <img
        className="journal-tweet-avatar"
        src={avatarUrl}
        width={size}
        height={size}
        alt=""
        aria-hidden="true"
        draggable={false}
        onError={() => setFailed(true)}
      />
    );
  }
  const fallback = avatarFallback(name, handle);
  return (
    <span
      className="journal-tweet-avatar journal-tweet-avatar-fallback"
      style={{ width: size, height: size, background: fallback.color }}
      aria-hidden="true"
    >
      {fallback.initial}
    </span>
  );
}

function TweetText({ runs, className }: { runs: TweetTextRun[]; className: string }) {
  if (runs.length === 0) {
    return null;
  }
  return (
    <p className={className}>
      {runs.map((run, index) =>
        run.kind === "link" && run.url ? (
          <a key={index} href={run.url} onClick={externalLinkClick(run.url)}>
            {run.text}
          </a>
        ) : (
          <span key={index}>{run.text}</span>
        ),
      )}
    </p>
  );
}

function TweetMediaStrip({
  media,
  compact,
}: {
  media: QuotedTweetSnapshot["media"];
  compact?: boolean;
}) {
  if (media.length === 0) {
    return null;
  }
  return (
    <div
      className={`journal-tweet-media${media.length > 1 ? " is-multi" : ""}${
        compact ? " is-compact" : ""
      }`}
    >
      {media.map((item, index) => {
        // Known dimensions reserve the media box before (or without) the
        // image, so the feed doesn't reflow as media arrives.
        const aspect =
          item.width && item.height
            ? { aspectRatio: `${item.width} / ${item.height}` }
            : undefined;
        const image = (
          <img
            src={item.imageUrl}
            alt={item.kind === "photo" ? "Photo" : "Video thumbnail"}
            loading="lazy"
            draggable={false}
            style={aspect}
          />
        );
        if (item.kind === "photo") {
          return (
            <span key={index} className="journal-tweet-media-item">
              {image}
            </span>
          );
        }
        const watchUrl = item.watchUrl;
        return (
          <a
            key={index}
            className="journal-tweet-media-item journal-tweet-video"
            href={watchUrl}
            title={item.kind === "gif" ? "Watch GIF on X" : "Watch video on X"}
            onClick={watchUrl ? externalLinkClick(watchUrl) : (e) => e.preventDefault()}
          >
            {image}
            <span className="journal-tweet-play" aria-hidden="true">
              <Play size={compact ? 14 : 18} fill="currentColor" />
            </span>
          </a>
        );
      })}
    </div>
  );
}

/** X-style local-time stamp: "8:23 AM · May 31, 2018". */
function formatTweetDate(iso: string | undefined): string | null {
  if (!iso) {
    return null;
  }
  const parsed = new Date(iso);
  if (Number.isNaN(parsed.getTime())) {
    return null;
  }
  const time = parsed.toLocaleTimeString(undefined, {
    hour: "numeric",
    minute: "2-digit",
  });
  const day = parsed.toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
  return `${time} · ${day}`;
}

/** The hydrated tweet, rendered as the entry's whole content — an X-embed
 * look (header, text, media, quote, linked timestamp) with no wrapper
 * chrome of its own, so the feed reads as tweets rather than tweets inside
 * content items. Exported for the static-markup tests. */
export function JournalTweetCard({ entry }: { entry: JournalTweetEntry }) {
  const tweet = entry.tweet;
  if (!tweet) {
    return null;
  }
  const date = formatTweetDate(tweet.createdAt);
  const quoted = tweet.quoted;
  return (
    <article className="journal-tweet" aria-label={`Tweet by @${tweet.author.handle}`}>
      <header className="journal-tweet-head">
        <a
          className="journal-tweet-who"
          href={`https://x.com/${tweet.author.handle}`}
          onClick={externalLinkClick(`https://x.com/${tweet.author.handle}`)}
        >
          <TweetAvatar
            name={tweet.author.name}
            handle={tweet.author.handle}
            avatarUrl={tweet.author.avatarUrl}
            size={32}
          />
          <span className="journal-tweet-names">
            <span className="journal-tweet-author">{tweet.author.name}</span>
            <span className="journal-tweet-handle">@{tweet.author.handle}</span>
          </span>
        </a>
        <a
          className="journal-tweet-x"
          href={tweet.url}
          aria-label="View on X"
          onClick={externalLinkClick(tweet.url)}
        >
          <svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
            <path d="M18.244 2.25h3.308l-7.227 8.26 8.502 11.24h-6.657l-5.214-6.817L4.99 21.75H1.68l7.73-8.835L1.254 2.25H8.08l4.713 6.231zm-1.161 17.52h1.833L7.084 4.126H5.117z" />
          </svg>
        </a>
      </header>
      {tweet.replyTo ? (
        <p className="journal-tweet-reply">Replying to @{tweet.replyTo.handle}</p>
      ) : null}
      <TweetText runs={tweet.runs} className="journal-tweet-text" />
      {tweet.partial ? (
        <p className="journal-tweet-partial">
          Long post —{" "}
          <a href={tweet.url} onClick={externalLinkClick(tweet.url)}>
            full text on X
          </a>
        </p>
      ) : null}
      <TweetMediaStrip media={tweet.media} />
      {quoted ? (
        <div
          className="journal-tweet-quote"
          role="link"
          tabIndex={0}
          onClick={(event) => {
            // Links inside the quoted text keep their own targets.
            if ((event.target as HTMLElement).closest("a")) {
              return;
            }
            void openExternalUrl(quoted.url);
          }}
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              void openExternalUrl(quoted.url);
            }
          }}
        >
          <div className="journal-tweet-quote-head">
            <TweetAvatar
              name={quoted.author.name}
              handle={quoted.author.handle}
              avatarUrl={quoted.author.avatarUrl}
              size={16}
            />
            <span className="journal-tweet-author">{quoted.author.name}</span>
            <span className="journal-tweet-handle">@{quoted.author.handle}</span>
          </div>
          <TweetText runs={quoted.runs} className="journal-tweet-text is-quote" />
          {quoted.partial ? (
            <p className="journal-tweet-partial">
              Long post —{" "}
              <a href={quoted.url} onClick={externalLinkClick(quoted.url)}>
                full text on X
              </a>
            </p>
          ) : null}
          <TweetMediaStrip media={quoted.media} compact />
        </div>
      ) : null}
      {date ? (
        <footer className="journal-tweet-foot">
          <a href={tweet.url} onClick={externalLinkClick(tweet.url)}>
            {date}
          </a>
        </footer>
      ) : null}
    </article>
  );
}

function JournalEntryCard({
  entry,
  menuOpen,
  onOpenMenu,
  onOpenContextMenu,
  onRetryTweet,
}: {
  entry: JournalEntry;
  menuOpen: boolean;
  onOpenMenu: (entryId: string, trigger: HTMLButtonElement) => void;
  onOpenContextMenu: (entryId: string, clientX: number, clientY: number) => void;
  onRetryTweet: (id: string) => void;
}) {
  let body;
  let variant;
  if (entry.kind === "note") {
    variant = "is-note";
    body = <p className="journal-note-text">{entry.text}</p>;
  } else if (entry.kind === "link") {
    variant = "is-link";
    body = (
      <a
        className="journal-link-url"
        href={entry.url}
        onClick={externalLinkClick(entry.url)}
      >
        {entry.url}
      </a>
    );
  } else if (entry.hydration === "ok" && entry.tweet) {
    variant = "is-tweet";
    body = <JournalTweetCard entry={entry} />;
  } else if (entry.hydration === "failed") {
    variant = "is-tweet-failed";
    body = (
      <div className="journal-tweet-placeholder">
        <a
          className="journal-link-url"
          href={entry.url}
          onClick={externalLinkClick(entry.url)}
        >
          {entry.url}
        </a>
        <p className="journal-tweet-error">
          Couldn’t load this tweet{entry.error ? ` — ${entry.error}` : ""}.
        </p>
        <button
          className="control-button journal-tweet-retry"
          type="button"
          onClick={() => onRetryTweet(entry.id)}
        >
          <RotateCw size={12} aria-hidden="true" />
          <span>Retry</span>
        </button>
      </div>
    );
  } else {
    variant = "is-tweet-pending";
    body = (
      <div className="journal-tweet-placeholder">
        <a
          className="journal-link-url"
          href={entry.url}
          onClick={externalLinkClick(entry.url)}
        >
          {entry.url}
        </a>
        <p className="journal-tweet-loading">
          <LoaderCircle size={12} aria-hidden="true" />
          <span>Loading tweet…</span>
        </p>
      </div>
    );
  }
  return (
    <article
      className={`journal-entry ${variant}${menuOpen ? " has-open-menu" : ""}`}
      title={new Date(entry.createdAt).toLocaleString()}
      onContextMenu={(event) => {
        // Right-clicking a link or the quote card keeps the entry menu too —
        // the browser menu has nothing useful to offer inside the shell.
        event.preventDefault();
        event.stopPropagation();
        onOpenContextMenu(entry.id, event.clientX, event.clientY);
      }}
    >
      {body}
      <button
        className="control-button journal-entry-menu-trigger"
        type="button"
        title="Entry actions"
        aria-label="Entry actions"
        aria-haspopup="menu"
        aria-expanded={menuOpen}
        data-journal-menu-trigger
        onClick={(event) => onOpenMenu(entry.id, event.currentTarget)}
      >
        <MoreHorizontal size={13} aria-hidden="true" />
      </button>
    </article>
  );
}

function JournalPane({
  entries,
  pendingUndo,
  onAddEntry,
  onRemoveEntry,
  onRetryTweet,
  onUndoRemove,
  onDismissUndo,
}: JournalPaneProps) {
  const [draft, setDraft] = useState("");
  const [menu, setMenu] = useState<{ entryId: string; left: number; top: number } | null>(
    null,
  );
  const menuRef = useRef<HTMLDivElement | null>(null);
  const feed = useMemo(() => [...entries].reverse(), [entries]);
  const menuEntry = menu
    ? entries.find((entry) => entry.id === menu.entryId) ?? null
    : null;
  const menuItems = menuEntry ? journalEntryMenuItems(menuEntry) : [];

  function runMenuAction(entry: JournalEntry, action: JournalMenuAction) {
    setMenu(null);
    if (action === "open") {
      const url = journalEntryUrl(entry);
      if (url) {
        void openExternalUrl(url);
      }
      return;
    }
    if (action === "copy") {
      void writeClipboardText(
        entry.kind === "note" ? entry.text : journalEntryUrl(entry) ?? "",
      );
      return;
    }
    if (action === "retry") {
      onRetryTweet(entry.id);
      return;
    }
    onRemoveEntry(entry.id);
  }

  function clampedMenuPosition(clientX: number, clientY: number) {
    return {
      left: Math.max(
        JOURNAL_VIEWPORT_MARGIN,
        Math.min(clientX, window.innerWidth - JOURNAL_MENU_WIDTH - JOURNAL_VIEWPORT_MARGIN),
      ),
      top: Math.max(
        JOURNAL_VIEWPORT_MARGIN,
        Math.min(
          clientY,
          window.innerHeight - JOURNAL_MENU_HEIGHT_ESTIMATE - JOURNAL_VIEWPORT_MARGIN,
        ),
      ),
    };
  }

  function openMenuFromTrigger(entryId: string, trigger: HTMLButtonElement) {
    if (menu?.entryId === entryId) {
      setMenu(null);
      return;
    }
    const rect = trigger.getBoundingClientRect();
    setMenu({
      entryId,
      ...clampedMenuPosition(rect.right - JOURNAL_MENU_WIDTH, rect.bottom + 4),
    });
  }

  function openContextMenu(entryId: string, clientX: number, clientY: number) {
    setMenu({ entryId, ...clampedMenuPosition(clientX, clientY) });
  }

  // Menu dismissal and its keycap shortcuts, mirroring the research sidebar
  // menus: outside mousedown, Escape, viewport reflow all close; a bare
  // keycap letter fires its item.
  useEffect(() => {
    if (!menu) {
      return;
    }
    const closeMenu = (event: globalThis.MouseEvent) => {
      const target = event.target as Node;
      if (
        !menuRef.current?.contains(target) &&
        !(target instanceof Element && target.closest("[data-journal-menu-trigger]"))
      ) {
        setMenu(null);
      }
    };
    const handleKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape") {
        setMenu(null);
        return;
      }
      if (event.metaKey || event.ctrlKey || event.altKey) {
        return;
      }
      const entry = entries.find((candidate) => candidate.id === menu.entryId);
      if (!entry) {
        return;
      }
      const item = journalEntryMenuItems(entry).find(
        (candidate) => candidate.key.toLowerCase() === event.key.toLowerCase(),
      );
      if (!item) {
        return;
      }
      event.preventDefault();
      event.stopPropagation();
      runMenuAction(entry, item.action);
    };
    const closeOnReflow = () => setMenu(null);
    document.addEventListener("mousedown", closeMenu);
    document.addEventListener("keydown", handleKeyDown);
    window.addEventListener("resize", closeOnReflow);
    window.addEventListener("scroll", closeOnReflow, true);
    return () => {
      document.removeEventListener("mousedown", closeMenu);
      document.removeEventListener("keydown", handleKeyDown);
      window.removeEventListener("resize", closeOnReflow);
      window.removeEventListener("scroll", closeOnReflow, true);
    };
    // runMenuAction and entries are stable enough per menu lifetime; the menu
    // closes on any entry mutation the actions cause.
  });

  // The height estimate that positioned the menu is a guess (items vary per
  // entry kind); clamp the real menu back inside the viewport once rendered.
  useLayoutEffect(() => {
    const element = menuRef.current;
    if (!menu || !element) {
      return;
    }
    const height = element.getBoundingClientRect().height;
    const top = Math.max(
      JOURNAL_VIEWPORT_MARGIN,
      Math.min(menu.top, window.innerHeight - JOURNAL_VIEWPORT_MARGIN - height),
    );
    if (top !== menu.top) {
      element.style.top = `${top}px`;
    }
  }, [menu]);

  // ⌘Z / Ctrl-Z restores the last removed entry while the undo bar shows.
  useEffect(() => {
    if (!pendingUndo) {
      return;
    }
    const handleKeyDown = (event: globalThis.KeyboardEvent) => {
      if (
        (event.metaKey || event.ctrlKey) &&
        !event.shiftKey &&
        !event.altKey &&
        event.key.toLowerCase() === "z"
      ) {
        event.preventDefault();
        onUndoRemove();
      }
    };
    document.addEventListener("keydown", handleKeyDown);
    return () => document.removeEventListener("keydown", handleKeyDown);
  }, [onUndoRemove, pendingUndo]);

  function submit() {
    const text = draft.trim();
    if (!text) {
      return;
    }
    onAddEntry(text);
    setDraft("");
  }

  function handleSubmit(event: FormEvent) {
    event.preventDefault();
    submit();
  }

  function handleKeyDown(event: KeyboardEvent<HTMLTextAreaElement>) {
    if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
      event.preventDefault();
      submit();
    }
  }

  return (
    <ResearchDocumentFrame title="Journal">
      <div className="research-document-scroll journal-scroll">
        <div className="journal-column">
          <form className="journal-composer" onSubmit={handleSubmit}>
            <textarea
              className="journal-composer-input"
              value={draft}
              rows={2}
              placeholder="Add a note or paste a URL…"
              aria-label="New journal entry"
              onChange={(event) => setDraft(event.currentTarget.value)}
              onKeyDown={handleKeyDown}
            />
          </form>
          {pendingUndo ? (
            <div className="journal-undo" role="status">
              <span className="journal-undo-label">
                {pendingUndo.entry.kind === "note" ? "Note" : "Entry"} removed
              </span>
              <button
                className="control-button journal-undo-restore"
                type="button"
                onClick={onUndoRemove}
              >
                <Undo2 size={12} aria-hidden="true" />
                <span>Undo</span>
                <kbd className="context-menu-shortcut is-keycap">⌘Z</kbd>
              </button>
              <button
                className="control-button journal-undo-dismiss"
                type="button"
                title="Dismiss"
                aria-label="Dismiss undo"
                onClick={onDismissUndo}
              >
                <X size={12} aria-hidden="true" />
              </button>
            </div>
          ) : null}
          <div className="journal-feed" role="feed" aria-label="Journal entries">
            {feed.map((entry) => (
              <JournalEntryCard
                key={entry.id}
                entry={entry}
                menuOpen={menu?.entryId === entry.id}
                onOpenMenu={openMenuFromTrigger}
                onOpenContextMenu={openContextMenu}
                onRetryTweet={onRetryTweet}
              />
            ))}
            {feed.length === 0 ? (
              <p className="journal-empty">
                Notes and links you add appear here, newest first.
              </p>
            ) : null}
          </div>
        </div>
      </div>
      {menu && menuEntry
        ? createPortal(
            <div
              ref={menuRef}
              className="popover-surface popover-surface--context pane-context-menu journal-entry-menu"
              role="menu"
              aria-label="Journal entry actions"
              style={{ left: menu.left, top: menu.top }}
              onMouseDown={(event) => event.stopPropagation()}
              onContextMenu={(event) => event.preventDefault()}
            >
              <div className="group-context-actions">
                {menuItems.map((item, index) => (
                  <span key={item.action} style={{ display: "contents" }}>
                    {item.danger && index > 0 ? (
                      <div className="context-menu-divider" role="separator" />
                    ) : null}
                    <button
                      className={`control-button context-menu-has-shortcut${
                        item.danger ? " context-menu-danger" : ""
                      }`}
                      type="button"
                      role="menuitem"
                      onClick={() => runMenuAction(menuEntry, item.action)}
                    >
                      {menuItemIcon(item.action)}
                      <span>{item.label}</span>
                      <kbd className="context-menu-shortcut is-keycap">{item.key}</kbd>
                    </button>
                  </span>
                ))}
              </div>
            </div>,
            document.body,
          )
        : null}
    </ResearchDocumentFrame>
  );
}

export default memo(JournalPane);
