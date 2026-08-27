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
  TweetSnapshot,
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

/** The timeline's age stamp: "29m" and "5h" inside a day, "Jul 27" inside the
 * year, "Mar 21, 2006" beyond it. */
function formatTweetAge(iso: string | undefined, now = Date.now()): string | null {
  if (!iso) {
    return null;
  }
  const parsed = new Date(iso);
  if (Number.isNaN(parsed.getTime())) {
    return null;
  }
  const seconds = Math.max(0, Math.round((now - parsed.getTime()) / 1000));
  if (seconds < 60) {
    return `${seconds}s`;
  }
  if (seconds < 3600) {
    return `${Math.floor(seconds / 60)}m`;
  }
  if (seconds < 86_400) {
    return `${Math.floor(seconds / 3600)}h`;
  }
  const sameYear = parsed.getFullYear() === new Date(now).getFullYear();
  return parsed.toLocaleDateString(undefined, {
    month: "short",
    day: "numeric",
    ...(sameYear ? {} : { year: "numeric" }),
  });
}

/** The full stamp behind the age, for the title tooltip. */
function formatTweetDate(iso: string | undefined): string | null {
  if (!iso) {
    return null;
  }
  const parsed = new Date(iso);
  return Number.isNaN(parsed.getTime()) ? null : parsed.toLocaleString();
}

/** Engagement counts the way a timeline abbreviates them: exact under ten
 * thousand, whole thousands past it, one decimal past a million. */
function formatTweetCount(value: number): string {
  if (value < 10_000) {
    return value.toLocaleString();
  }
  if (value < 1_000_000) {
    return `${Math.floor(value / 1000)}K`;
  }
  return `${(value / 1_000_000).toFixed(1).replace(/\.0$/, "")}M`;
}

function ReplyGlyph() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
      <path d="M21 11.5a8.38 8.38 0 0 1-.9 3.8 8.5 8.5 0 0 1-7.6 4.7 8.38 8.38 0 0 1-3.8-.9L3 21l1.9-5.7a8.38 8.38 0 0 1-.9-3.8 8.5 8.5 0 0 1 4.7-7.6 8.38 8.38 0 0 1 3.8-.9h.5a8.48 8.48 0 0 1 8 8v.5z" />
    </svg>
  );
}

function LikeGlyph() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
      <path d="M20.84 4.61a5.5 5.5 0 0 0-7.78 0L12 5.67l-1.06-1.06a5.5 5.5 0 0 0-7.78 7.78l1.06 1.06L12 21.23l7.78-7.78 1.06-1.06a5.5 5.5 0 0 0 0-7.78z" />
    </svg>
  );
}

function VerifiedBadge() {
  return (
    <svg
      className="journal-tweet-verified"
      viewBox="0 0 22 22"
      fill="currentColor"
      aria-label="Verified"
      role="img"
    >
      <path d="M20.396 11c-.018-.646-.215-1.275-.57-1.816-.354-.54-.852-.972-1.438-1.246.223-.607.27-1.264.14-1.897-.131-.634-.437-1.218-.882-1.687-.47-.445-1.053-.75-1.687-.882-.633-.13-1.29-.083-1.897.14-.273-.587-.704-1.086-1.245-1.44S11.647 1.62 11 1.604c-.646.017-1.273.213-1.813.568s-.969.854-1.24 1.44c-.608-.223-1.267-.272-1.902-.14-.635.13-1.22.436-1.69.882-.445.47-.749 1.055-.878 1.688-.13.633-.08 1.29.144 1.896-.587.274-1.087.705-1.443 1.245-.356.54-.555 1.17-.574 1.817.02.647.218 1.276.574 1.816.356.54.856.972 1.443 1.245-.224.606-.274 1.263-.144 1.896.13.634.433 1.218.877 1.688.47.443 1.054.747 1.687.878.633.132 1.29.084 1.897-.136.274.586.705 1.084 1.246 1.439.54.354 1.17.551 1.816.569.647-.016 1.276-.213 1.817-.567s.972-.854 1.245-1.44c.604.239 1.266.296 1.903.164.636-.132 1.22-.447 1.68-.907.46-.46.776-1.044.908-1.681s.075-1.299-.165-1.903c.586-.274 1.084-.705 1.439-1.246.354-.54.551-1.17.569-1.816zM9.662 14.85l-3.429-3.428 1.293-1.302 2.072 2.072 4.4-4.794 1.347 1.246z" />
    </svg>
  );
}

function TweetLinkCardView({ card }: { card: NonNullable<TweetSnapshot["card"]> }) {
  return (
    <a
      className={`journal-tweet-card${card.large ? " is-large" : ""}`}
      href={card.url}
      onClick={externalLinkClick(card.url)}
    >
      {card.imageUrl ? (
        <span className="journal-tweet-card-media">
          <img src={card.imageUrl} alt="" loading="lazy" draggable={false} />
        </span>
      ) : null}
      <span className="journal-tweet-card-copy">
        <span className="journal-tweet-card-domain">{card.domain}</span>
        <span className="journal-tweet-card-title">{card.title}</span>
        {card.description ? (
          <span className="journal-tweet-card-desc">{card.description}</span>
        ) : null}
      </span>
    </a>
  );
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
  const quoted = tweet.quoted;
  const authorUrl = `https://x.com/${tweet.author.handle}`;
  const age = formatTweetAge(tweet.createdAt);
  return (
    <article className="journal-tweet" aria-label={`Tweet by @${tweet.author.handle}`}>
      <a
        className="journal-tweet-avatar-link"
        href={authorUrl}
        aria-hidden="true"
        tabIndex={-1}
        onClick={externalLinkClick(authorUrl)}
      >
        <TweetAvatar
          name={tweet.author.name}
          handle={tweet.author.handle}
          avatarUrl={tweet.author.avatarUrl}
          size={40}
        />
      </a>
      <div className="journal-tweet-main">
        <div className="journal-tweet-head">
          <a
            className="journal-tweet-who"
            href={authorUrl}
            onClick={externalLinkClick(authorUrl)}
          >
            <span className="journal-tweet-author">{tweet.author.name}</span>
            {tweet.author.verified ? <VerifiedBadge /> : null}
            <span className="journal-tweet-handle">@{tweet.author.handle}</span>
          </a>
          {age ? (
            <>
              <span className="journal-tweet-dot" aria-hidden="true">
                ·
              </span>
              <a
                className="journal-tweet-age"
                href={tweet.url}
                title={formatTweetDate(tweet.createdAt) ?? undefined}
                onClick={externalLinkClick(tweet.url)}
              >
                {age}
              </a>
            </>
          ) : null}
        </div>
        {tweet.replyTo ? (
          <p className="journal-tweet-reply">
            Replying to <span>@{tweet.replyTo.handle}</span>
          </p>
        ) : null}
        <TweetText runs={tweet.runs} className="journal-tweet-text" />
        {tweet.partial ? (
          <a
            className="journal-tweet-more"
            href={tweet.url}
            onClick={externalLinkClick(tweet.url)}
          >
            Show more
          </a>
        ) : null}
        <TweetMediaStrip media={tweet.media} />
        {tweet.card ? <TweetLinkCardView card={tweet.card} /> : null}
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
                size={18}
              />
              <span className="journal-tweet-author">{quoted.author.name}</span>
              {quoted.author.verified ? <VerifiedBadge /> : null}
              <span className="journal-tweet-handle">@{quoted.author.handle}</span>
            </div>
            <TweetText runs={quoted.runs} className="journal-tweet-text is-quote" />
            {quoted.partial ? (
              <span className="journal-tweet-more">Show more</span>
            ) : null}
            <TweetMediaStrip media={quoted.media} compact />
          </div>
        ) : null}
        {tweet.replies !== undefined || tweet.likes !== undefined ? (
          // Counts as captured, not controls: this is a journal entry, so the
          // engagement reads as metadata and nothing here acts on X.
          <div className="journal-tweet-stats">
            {tweet.replies !== undefined ? (
              <span className="journal-tweet-stat" title={`${tweet.replies} replies`}>
                <ReplyGlyph />
                {formatTweetCount(tweet.replies)}
              </span>
            ) : null}
            {tweet.likes !== undefined ? (
              <span className="journal-tweet-stat" title={`${tweet.likes} likes`}>
                <LikeGlyph />
                {formatTweetCount(tweet.likes)}
              </span>
            ) : null}
          </div>
        ) : null}
      </div>
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
