import { memo, useMemo, useState } from "react";
import type { FormEvent, KeyboardEvent, MouseEvent } from "react";
import { LoaderCircle, Play, RotateCw, X } from "lucide-react";
import type { JournalEntry, JournalTweetEntry } from "../../lib/journal";
import type {
  QuotedTweetSnapshot,
  TweetTextRun,
} from "../../lib/journalTweets";
import { openExternalUrl } from "../../lib/api";
import { ResearchDocumentFrame } from "./ResearchDocumentChrome";

interface JournalPaneProps {
  /** Oldest first (storage order); the feed renders newest first. */
  entries: JournalEntry[];
  onAddEntry: (input: string) => void;
  onRemoveEntry: (id: string) => void;
  onRetryTweet: (id: string) => void;
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
  onRemove,
  onRetryTweet,
}: {
  entry: JournalEntry;
  onRemove: (id: string) => void;
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
      className={`journal-entry ${variant}`}
      title={new Date(entry.createdAt).toLocaleString()}
    >
      {body}
      <button
        className="control-button journal-entry-remove"
        type="button"
        title="Remove from journal"
        aria-label="Remove from journal"
        onClick={() => onRemove(entry.id)}
      >
        <X size={12} aria-hidden="true" />
      </button>
    </article>
  );
}

function JournalPane({
  entries,
  onAddEntry,
  onRemoveEntry,
  onRetryTweet,
}: JournalPaneProps) {
  const [draft, setDraft] = useState("");
  const feed = useMemo(() => [...entries].reverse(), [entries]);

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
            <div className="journal-composer-hint">
              Enter to add — a tweet URL becomes an embedded tweet
            </div>
          </form>
          <div className="journal-feed" role="feed" aria-label="Journal entries">
            {feed.map((entry) => (
              <JournalEntryCard
                key={entry.id}
                entry={entry}
                onRemove={onRemoveEntry}
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
    </ResearchDocumentFrame>
  );
}

export default memo(JournalPane);
