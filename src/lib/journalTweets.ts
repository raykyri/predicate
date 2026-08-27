// Tweet URL recognition and hydration for journal entries. The fetch itself
// happens in the backend (the webview CSP has no connect-src for X, and the
// syndication CDN's CORS only admits platform.twitter.com); this module owns
// everything around it: recognizing a tweet permalink, deriving the token the
// syndication endpoint expects, and normalizing the raw payload into the
// TweetSnapshot stored on a journal entry. The snapshot — not the raw
// payload — is the persisted format, so its shape changes deliberately.

/** The status id from a tweet permalink, if `input` is one. Accepts
 * twitter.com / x.com (plus www. / mobile.) and both /status/ and the legacy
 * /statuses/, tolerating query strings, fragments, and trailing segments like
 * /photo/1. */
export function tweetIdFromUrl(input: string): string | null {
  let url: URL;
  try {
    url = new URL(input);
  } catch {
    return null;
  }
  if (url.protocol !== "https:" && url.protocol !== "http:") {
    return null;
  }
  const host = url.hostname.toLowerCase();
  if (
    ![
      "twitter.com",
      "www.twitter.com",
      "mobile.twitter.com",
      "x.com",
      "www.x.com",
      "mobile.x.com",
    ].includes(host)
  ) {
    return null;
  }
  const segments = url.pathname.split("/").filter(Boolean);
  const statusIndex = segments.findIndex(
    (segment) => segment === "status" || segment === "statuses",
  );
  // "x.com/status/…" (statusIndex 0) is not a tweet permalink — the handle
  // segment must precede /status/.
  if (statusIndex <= 0) {
    return null;
  }
  const id = segments[statusIndex + 1];
  return id && /^[0-9]+$/.test(id) ? id : null;
}

/** The `token` query parameter the syndication CDN expects, derived the way
 * X's embedded-widget code derived it. The endpoint is loose about the value
 * today, but keeping it shaped like real widget output avoids depending on
 * that tolerance. */
export function syndicationToken(id: string): string {
  const token = ((Number(id) / 1e15) * Math.PI)
    .toString(36)
    .replace(/(0+|\.)/g, "");
  return token || "x";
}

export interface TweetMedia {
  kind: "photo" | "video" | "gif";
  /** Direct https image URL: the photo itself, or the video poster frame. */
  imageUrl: string;
  /** Permalink for watching the video on X (videos and gifs only). */
  watchUrl?: string;
  width?: number;
  height?: number;
}

/** One span of the tweet text: plain text, or a t.co entity expanded into a
 * link labeled with its display form. Structured runs rather than markdown so
 * tweet text that happens to contain markdown syntax renders literally. */
export interface TweetTextRun {
  kind: "text" | "link";
  text: string;
  /** Expanded destination, for link runs. */
  url?: string;
}

/** The hydrated form of a tweet stored on a journal entry: everything the
 * feed card renders, and nothing tied to the syndication payload's shape. */
export interface TweetSnapshot {
  id: string;
  url: string;
  author: { name: string; handle: string; avatarUrl?: string };
  /** ISO timestamp of the tweet itself (not the capture). */
  createdAt?: string;
  runs: TweetTextRun[];
  /** True when the payload is a preview of a longer post (full text is not
   * available from the syndication endpoint). */
  partial: boolean;
  media: TweetMedia[];
  replyTo?: { handle: string; id?: string };
  quoted?: QuotedTweetSnapshot;
}

export type QuotedTweetSnapshot = Omit<TweetSnapshot, "quoted" | "replyTo">;

type Payload = Record<string, unknown>;

const HTML_ENTITIES: Record<string, string> = {
  "&amp;": "&",
  "&lt;": "<",
  "&gt;": ">",
  "&quot;": '"',
  "&#39;": "'",
  "&apos;": "'",
};

function decodeEntities(text: string): string {
  return text.replace(/&(?:amp|lt|gt|quot|#39|apos);/g, (m) => HTML_ENTITIES[m] ?? m);
}

function str(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

function num(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function records(value: unknown): Payload[] {
  return Array.isArray(value)
    ? value.filter(
        (item): item is Payload => typeof item === "object" && item !== null,
      )
    : [];
}

/** Expand a tweet's raw text into display runs: t.co URL entities become link
 * runs labeled by their display_url, t.co media links vanish (the media
 * renders separately), everything else stays plain text. */
function textRuns(raw: string, entities: unknown): TweetTextRun[] {
  const source = (entities ?? {}) as Payload;
  const subs: { tco: string; run: TweetTextRun | null }[] = [];
  for (const u of records(source.urls)) {
    const tco = str(u.url);
    const expanded = str(u.expanded_url);
    if (!tco || !expanded) {
      continue;
    }
    subs.push({
      tco,
      run: { kind: "link", text: str(u.display_url) ?? expanded, url: expanded },
    });
  }
  for (const m of records(source.media)) {
    const tco = str(m.url);
    if (tco) {
      subs.push({ tco, run: null });
    }
  }
  // Longer t.co strings first so a link is never split by a prefix match.
  subs.sort((a, b) => b.tco.length - a.tco.length);

  let runs: TweetTextRun[] = [{ kind: "text", text: decodeEntities(raw) }];
  for (const { tco, run } of subs) {
    const next: TweetTextRun[] = [];
    for (const existing of runs) {
      if (existing.kind !== "text" || !existing.text.includes(tco)) {
        next.push(existing);
        continue;
      }
      const parts = existing.text.split(tco);
      parts.forEach((part, index) => {
        if (part) {
          next.push({ kind: "text", text: part });
        }
        if (index < parts.length - 1 && run) {
          next.push({ ...run });
        }
      });
    }
    runs = next;
  }
  // Trim whitespace left hanging at the edges by removed media links.
  while (runs.length) {
    const last = runs[runs.length - 1];
    if (last.kind !== "text") {
      break;
    }
    last.text = last.text.replace(/\s+$/, "");
    if (last.text) {
      break;
    }
    runs.pop();
  }
  while (runs.length) {
    const first = runs[0];
    if (first.kind !== "text") {
      break;
    }
    first.text = first.text.replace(/^\s+/, "");
    if (first.text) {
      break;
    }
    runs.shift();
  }
  return runs;
}

function mediaItems(v: Payload): TweetMedia[] {
  const items: TweetMedia[] = [];
  for (const m of records(v.mediaDetails)) {
    const imageUrl = str(m.media_url_https);
    if (!imageUrl) {
      continue;
    }
    const type = str(m.type);
    const size = (m.original_info ?? {}) as Payload;
    const base = { imageUrl, width: num(size.width), height: num(size.height) };
    if (type === "photo") {
      items.push({ kind: "photo", ...base });
    } else if (type === "video" || type === "animated_gif") {
      items.push({
        kind: type === "video" ? "video" : "gif",
        ...base,
        watchUrl: str(m.expanded_url),
      });
    }
  }
  return items;
}

function snapshotCore(fallbackId: string, v: Payload): QuotedTweetSnapshot | null {
  const user = (v.user ?? {}) as Payload;
  const handle = str(user.screen_name);
  if (!handle) {
    return null;
  }
  const id = str(v.id_str) ?? fallbackId;
  const partial = v.note_tweet !== undefined;
  const runs = textRuns(str(v.text) ?? "", v.entities);
  // A preview is by definition cut off, so the text trails an ellipsis.
  if (partial && runs.length > 0) {
    const last = runs[runs.length - 1];
    if (last.kind === "text" && !last.text.endsWith("…")) {
      last.text += "…";
    } else if (last.kind !== "text") {
      runs.push({ kind: "text", text: "…" });
    }
  }
  return {
    id,
    url: `https://x.com/${handle}/status/${id}`,
    author: {
      name: str(user.name) ?? "Unknown",
      handle,
      avatarUrl: str(user.profile_image_url_https),
    },
    createdAt: str(v.created_at),
    runs,
    partial,
    media: mediaItems(v),
  };
}

/** Normalize a raw syndication tweet-result payload into the snapshot stored
 * on a journal entry. Returns null for unavailable tweets (deleted, withheld,
 * protected come back as TweetTombstone) and unrecognized payloads. */
export function tweetSnapshotFromSyndication(
  id: string,
  payload: unknown,
): TweetSnapshot | null {
  if (typeof payload !== "object" || payload === null) {
    return null;
  }
  const v = payload as Payload;
  if (v.__typename !== "Tweet") {
    return null;
  }
  const core = snapshotCore(id, v);
  if (!core) {
    return null;
  }
  const snapshot: TweetSnapshot = { ...core };
  const replyHandle = str(v.in_reply_to_screen_name);
  if (replyHandle) {
    snapshot.replyTo = { handle: replyHandle, id: str(v.in_reply_to_status_id_str) };
  }
  if (typeof v.quoted_tweet === "object" && v.quoted_tweet !== null) {
    // A tombstoned quote (deleted/withheld) is a stub with no user; the card
    // renders the tweet quote-less rather than failing hydration.
    const quoted = snapshotCore("", v.quoted_tweet as Payload);
    if (quoted && quoted.id) {
      snapshot.quoted = quoted;
    }
  }
  return snapshot;
}
