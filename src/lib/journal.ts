// The journal data model. This module is the single canonical definition of
// the journal format: the backend stores the state as an opaque versioned
// blob inside .qmux/state.json (dedupe-by-id is its only structural
// knowledge), so every semantic rule about what an entry is lives here.
//
// Shape, and the room left for what comes next:
// - JournalState is a versioned envelope over a flat entry list, stored
//   oldest-first (append order); the feed renders it newest-first.
// - Every entry has a stable `id` and a capture `createdAt`, so future
//   layers — grouping related entries, attaching research questions and
//   auto-asked questions to an entry — can reference entries by id the way
//   research folders reference tree ids, without touching this format.
//   Grouping is expected to arrive as sibling fields on JournalState
//   (e.g. groups + membership), not as entry mutations.
// - Tweet entries separate what the user gave us (url, tweetId — permanent)
//   from what hydration fetched (tweet: TweetSnapshot — replaceable), so a
//   re-fetch or a failed fetch never loses the entry itself.

import type { TweetSnapshot } from "./journalTweets";
import { tweetIdFromUrl } from "./journalTweets";

export const JOURNAL_STATE_VERSION = 1;

interface JournalEntryBase {
  id: string;
  /** ISO timestamp of when the entry was added to the journal. */
  createdAt: string;
}

/** A free-form text note. */
export interface JournalNoteEntry extends JournalEntryBase {
  kind: "note";
  text: string;
}

/** A saved URL that is not a tweet permalink. */
export interface JournalLinkEntry extends JournalEntryBase {
  kind: "link";
  url: string;
}

export type JournalTweetHydration = "pending" | "ok" | "failed";

/** A tweet permalink, hydrated (or awaiting hydration) into a snapshot. */
export interface JournalTweetEntry extends JournalEntryBase {
  kind: "tweet";
  /** The permalink as entered (normalized snapshot.url may differ). */
  url: string;
  tweetId: string;
  hydration: JournalTweetHydration;
  tweet?: TweetSnapshot;
  /** Why the last hydration failed, when hydration is "failed". */
  error?: string;
}

export type JournalEntry = JournalNoteEntry | JournalLinkEntry | JournalTweetEntry;

export interface JournalState {
  version: number;
  /** Oldest first (append order). */
  entries: JournalEntry[];
}

export function emptyJournalState(): JournalState {
  return { version: JOURNAL_STATE_VERSION, entries: [] };
}

export function isEmptyJournalState(state: JournalState): boolean {
  return state.entries.length === 0;
}

function sanitizeEntry(value: unknown): JournalEntry | null {
  if (typeof value !== "object" || value === null) {
    return null;
  }
  const raw = value as Record<string, unknown>;
  const { id, createdAt } = raw;
  if (typeof id !== "string" || !id || typeof createdAt !== "string") {
    return null;
  }
  if (raw.kind === "note" && typeof raw.text === "string") {
    return { kind: "note", id, createdAt, text: raw.text };
  }
  if (raw.kind === "link" && typeof raw.url === "string") {
    return { kind: "link", id, createdAt, url: raw.url };
  }
  if (
    raw.kind === "tweet" &&
    typeof raw.url === "string" &&
    typeof raw.tweetId === "string"
  ) {
    const tweet =
      typeof raw.tweet === "object" && raw.tweet !== null
        ? (raw.tweet as TweetSnapshot)
        : undefined;
    // A stored "ok" without its snapshot (or any unknown status) re-enters
    // hydration rather than rendering an empty card.
    const hydration: JournalTweetHydration =
      raw.hydration === "ok" && tweet
        ? "ok"
        : raw.hydration === "failed"
          ? "failed"
          : "pending";
    return {
      kind: "tweet",
      id,
      createdAt,
      url: raw.url,
      tweetId: raw.tweetId,
      hydration,
      ...(tweet ? { tweet } : {}),
      ...(typeof raw.error === "string" ? { error: raw.error } : {}),
    };
  }
  return null;
}

/** Parse a stored (or backend-returned) journal state. Entry-by-entry: a
 * malformed entry is dropped, not the journal. Anything unrecognizable
 * altogether yields the empty state. */
export function normalizeJournalState(value: unknown): JournalState {
  if (typeof value !== "object" || value === null) {
    return emptyJournalState();
  }
  const raw = value as Record<string, unknown>;
  const entries: JournalEntry[] = [];
  const seen = new Set<string>();
  if (Array.isArray(raw.entries)) {
    for (const candidate of raw.entries) {
      const entry = sanitizeEntry(candidate);
      if (entry && !seen.has(entry.id)) {
        seen.add(entry.id);
        entries.push(entry);
      }
    }
  }
  return { version: JOURNAL_STATE_VERSION, entries };
}

/** What a submitted composer input becomes: a lone URL becomes a link (or a
 * tweet when it is a tweet permalink), anything else a note. */
export type JournalInput =
  | { kind: "note"; text: string }
  | { kind: "link"; url: string }
  | { kind: "tweet"; url: string; tweetId: string };

export function classifyJournalInput(input: string): JournalInput | null {
  const text = input.trim();
  if (!text) {
    return null;
  }
  // A URL pasted on its own line is an intent to save the URL; a URL inside
  // prose is part of the note.
  if (/^https?:\/\/\S+$/i.test(text)) {
    const tweetId = tweetIdFromUrl(text);
    if (tweetId) {
      return { kind: "tweet", url: text, tweetId };
    }
    try {
      new URL(text);
      return { kind: "link", url: text };
    } catch {
      // Fall through to a note.
    }
  }
  return { kind: "note", text };
}

export function newJournalEntryId(): string {
  return typeof crypto !== "undefined" && "randomUUID" in crypto
    ? crypto.randomUUID()
    : `journal-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

export function createJournalEntry(
  input: JournalInput,
  id: string,
  createdAt: string,
): JournalEntry {
  switch (input.kind) {
    case "note":
      return { kind: "note", id, createdAt, text: input.text };
    case "link":
      return { kind: "link", id, createdAt, url: input.url };
    case "tweet":
      return {
        kind: "tweet",
        id,
        createdAt,
        url: input.url,
        tweetId: input.tweetId,
        hydration: "pending",
      };
  }
}

export function appendJournalEntry(
  state: JournalState,
  entry: JournalEntry,
): JournalState {
  if (state.entries.some((existing) => existing.id === entry.id)) {
    return state;
  }
  return { ...state, entries: [...state.entries, entry] };
}

export function removeJournalEntry(state: JournalState, id: string): JournalState {
  const entries = state.entries.filter((entry) => entry.id !== id);
  return entries.length === state.entries.length ? state : { ...state, entries };
}

/** Insert an entry at a position (clamped), for undoing a removal. A no-op
 * when the id already exists, so a double-undo can't duplicate. */
export function insertJournalEntryAt(
  state: JournalState,
  entry: JournalEntry,
  index: number,
): JournalState {
  if (state.entries.some((existing) => existing.id === entry.id)) {
    return state;
  }
  const entries = [...state.entries];
  entries.splice(Math.max(0, Math.min(index, entries.length)), 0, entry);
  return { ...state, entries };
}

/** Record a hydration outcome on a tweet entry. No-op for other kinds or
 * unknown ids (the entry may have been deleted while the fetch was out). */
export function setJournalTweetHydration(
  state: JournalState,
  id: string,
  result:
    | { hydration: "pending" }
    | { hydration: "ok"; tweet: TweetSnapshot }
    | { hydration: "failed"; error: string },
): JournalState {
  let changed = false;
  const entries = state.entries.map((entry) => {
    if (entry.id !== id || entry.kind !== "tweet") {
      return entry;
    }
    changed = true;
    if (result.hydration === "ok") {
      const { error: _dropped, ...rest } = entry;
      return { ...rest, hydration: "ok" as const, tweet: result.tweet };
    }
    if (result.hydration === "failed") {
      return { ...entry, hydration: "failed" as const, error: result.error };
    }
    return { ...entry, hydration: "pending" as const };
  });
  return changed ? { ...state, entries } : state;
}
