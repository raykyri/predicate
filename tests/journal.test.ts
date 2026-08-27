import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import {
  appendJournalEntry,
  classifyJournalInput,
  createJournalEntry,
  emptyJournalState,
  normalizeJournalState,
  removeJournalEntry,
  setJournalTweetHydration,
  type JournalEntry,
  type JournalTweetEntry,
} from "../src/lib/journal";
import {
  syndicationToken,
  tweetIdFromUrl,
  tweetSnapshotFromSyndication,
  type TweetSnapshot,
} from "../src/lib/journalTweets";
import { JournalTweetCard } from "../src/components/research/JournalPane";

// Real syndication payloads captured from cdn.syndication.twimg.com, one per
// content shape the feed must handle.
function fixture(id: string): unknown {
  return JSON.parse(
    readFileSync(join(import.meta.dirname, "fixtures", "journal", `${id}.json`), "utf8"),
  );
}

function snapshot(id: string): TweetSnapshot {
  const parsed = tweetSnapshotFromSyndication(id, fixture(id));
  assert.ok(parsed, `fixture ${id} should hydrate`);
  return parsed;
}

function tweetEntry(tweet: TweetSnapshot): JournalTweetEntry {
  return {
    kind: "tweet",
    id: `entry-${tweet.id}`,
    createdAt: "2026-08-27T00:00:00.000Z",
    url: tweet.url,
    tweetId: tweet.id,
    hydration: "ok",
    tweet,
  };
}

test("tweet permalinks parse across hosts and trailing segments", () => {
  assert.equal(tweetIdFromUrl("https://x.com/jack/status/20"), "20");
  assert.equal(tweetIdFromUrl("https://twitter.com/jack/status/20?s=61&t=abc"), "20");
  assert.equal(tweetIdFromUrl("https://mobile.x.com/jack/status/20#photo"), "20");
  assert.equal(tweetIdFromUrl("https://www.twitter.com/a/statuses/123/photo/1"), "123");
  assert.equal(tweetIdFromUrl("https://x.com/status/20"), null);
  assert.equal(tweetIdFromUrl("https://x.com/jack/status/20abc"), null);
  assert.equal(tweetIdFromUrl("https://example.com/jack/status/20"), null);
  assert.equal(tweetIdFromUrl("not a url"), null);
  assert.equal(tweetIdFromUrl("ftp://x.com/jack/status/20"), null);
});

test("syndication token matches the widget derivation", () => {
  // ((20 / 1e15) * Math.PI).toString(36) with zeros and the radix point
  // removed — the value X's own embed code sends for this id.
  assert.equal(syndicationToken("20"), "6dq1a2xwd93");
});

test("composer input classifies into note, link, and tweet", () => {
  assert.deepEqual(classifyJournalInput("  just a thought  "), {
    kind: "note",
    text: "just a thought",
  });
  assert.deepEqual(classifyJournalInput("https://example.com/a?b=c"), {
    kind: "link",
    url: "https://example.com/a?b=c",
  });
  assert.deepEqual(classifyJournalInput("https://x.com/jack/status/20"), {
    kind: "tweet",
    url: "https://x.com/jack/status/20",
    tweetId: "20",
  });
  // A URL inside prose stays a note.
  assert.equal(classifyJournalInput("see https://example.com for more")?.kind, "note");
  assert.equal(classifyJournalInput("   "), null);
});

test("plain tweet hydrates author, text, and date", () => {
  const tweet = snapshot("20");
  assert.equal(tweet.author.handle, "jack");
  assert.equal(tweet.url, "https://x.com/jack/status/20");
  assert.deepEqual(tweet.runs, [{ kind: "text", text: "just setting up my twttr" }]);
  assert.equal(tweet.partial, false);
  assert.deepEqual(tweet.media, []);
  assert.ok(tweet.createdAt?.startsWith("2006-03-21"));
  assert.ok(tweet.author.avatarUrl?.startsWith("https://pbs.twimg.com/"));
});

test("t.co link entities expand into labeled link runs", () => {
  const tweet = snapshot("1628832338187636740");
  const link = tweet.runs.find((run) => run.kind === "link");
  assert.ok(link);
  assert.ok(link.url?.startsWith("https://"));
  assert.ok(!link.url?.includes("t.co"));
  assert.ok(!link.text.includes("t.co"));
  assert.ok(tweet.runs.every((run) => !run.text.includes("https://t.co")));
});

test("reply context is captured", () => {
  const tweet = snapshot("1674865731136020505");
  assert.equal(tweet.replyTo?.handle, "xDaily");
  assert.ok(tweet.replyTo?.id);
});

test("photo media hydrates with dimensions and no dangling t.co", () => {
  const tweet = snapshot("463440424141459456");
  assert.equal(tweet.media.length, 1);
  assert.equal(tweet.media[0].kind, "photo");
  assert.ok(tweet.media[0].imageUrl.startsWith("https://pbs.twimg.com/media/"));
  assert.ok((tweet.media[0].width ?? 0) > 0);
  // The media's own t.co link is stripped from the text runs.
  assert.ok(tweet.runs.every((run) => !run.text.includes("t.co")));
});

test("video media hydrates as a poster with a watch link", () => {
  const tweet = snapshot("1585341984679469056");
  assert.equal(tweet.media.length, 1);
  assert.equal(tweet.media[0].kind, "video");
  assert.ok(tweet.media[0].imageUrl.startsWith("https://pbs.twimg.com/"));
  assert.ok(tweet.media[0].watchUrl?.includes("/status/1585341984679469056"));
});

test("quote tweets nest with their own text, links, and media", () => {
  const tweet = snapshot("1599367266448994304");
  assert.equal(tweet.media[0]?.kind, "video");
  const quoted = tweet.quoted;
  assert.ok(quoted);
  assert.equal(quoted.author.handle, "CantBeFaraz");
  assert.equal(quoted.media[0]?.kind, "video");
  const quotedLinks = quoted.runs.filter((run) => run.kind === "link");
  assert.ok(quotedLinks.length >= 1);
  assert.ok(quotedLinks.every((run) => !run.url?.includes("t.co")));
  // Quote snapshots never recurse further.
  assert.ok(!("quoted" in quoted && (quoted as TweetSnapshot).quoted));
});

test("long posts are marked partial and get a trailing ellipsis", () => {
  const tweet = snapshot("1623411400545632256");
  assert.equal(tweet.partial, true);
  const last = tweet.runs[tweet.runs.length - 1];
  assert.ok(last.text.endsWith("…"));
});

test("tombstoned and malformed payloads do not hydrate", () => {
  assert.equal(tweetSnapshotFromSyndication("1", { __typename: "TweetTombstone" }), null);
  assert.equal(tweetSnapshotFromSyndication("1", "nope"), null);
  assert.equal(tweetSnapshotFromSyndication("1", null), null);
  assert.equal(tweetSnapshotFromSyndication("1", { __typename: "Tweet" }), null);
});

test("journal state normalizes entry-by-entry and round-trips", () => {
  const tweet = snapshot("20");
  const note = createJournalEntry(
    { kind: "note", text: "hello" },
    "a",
    "2026-08-27T00:00:00.000Z",
  );
  const link = createJournalEntry(
    { kind: "link", url: "https://example.com" },
    "b",
    "2026-08-27T00:00:01.000Z",
  );
  let state = appendJournalEntry(emptyJournalState(), note);
  state = appendJournalEntry(state, link);
  state = appendJournalEntry(state, tweetEntry(tweet));
  const roundTripped = normalizeJournalState(JSON.parse(JSON.stringify(state)));
  assert.deepEqual(roundTripped, state);

  const scrubbed = normalizeJournalState({
    version: 99,
    entries: [
      ...state.entries,
      { kind: "note" }, // no id
      { kind: "mystery", id: "z", createdAt: "2026-01-01" }, // unknown kind
      { ...note, id: "a" }, // duplicate id
      "garbage",
    ],
  });
  assert.deepEqual(scrubbed, state);
  assert.deepEqual(normalizeJournalState(null), emptyJournalState());
  assert.deepEqual(normalizeJournalState("junk"), emptyJournalState());
});

test("a stored ok-without-snapshot tweet re-enters hydration", () => {
  const state = normalizeJournalState({
    version: 1,
    entries: [
      {
        kind: "tweet",
        id: "a",
        createdAt: "2026-08-27T00:00:00.000Z",
        url: "https://x.com/jack/status/20",
        tweetId: "20",
        hydration: "ok",
      },
    ],
  });
  assert.equal((state.entries[0] as JournalTweetEntry).hydration, "pending");
});

test("hydration reducer records outcomes and tolerates deleted entries", () => {
  const pending = createJournalEntry(
    { kind: "tweet", url: "https://x.com/jack/status/20", tweetId: "20" },
    "a",
    "2026-08-27T00:00:00.000Z",
  );
  const state = appendJournalEntry(emptyJournalState(), pending);
  const failed = setJournalTweetHydration(state, "a", {
    hydration: "failed",
    error: "boom",
  });
  assert.equal((failed.entries[0] as JournalTweetEntry).hydration, "failed");
  assert.equal((failed.entries[0] as JournalTweetEntry).error, "boom");
  const ok = setJournalTweetHydration(failed, "a", {
    hydration: "ok",
    tweet: snapshot("20"),
  });
  const okEntry = ok.entries[0] as JournalTweetEntry;
  assert.equal(okEntry.hydration, "ok");
  assert.equal(okEntry.error, undefined);
  assert.equal(okEntry.tweet?.author.handle, "jack");
  // Unknown ids (entry deleted while the fetch was out) are a no-op.
  assert.equal(setJournalTweetHydration(ok, "gone", { hydration: "pending" }), ok);
  assert.deepEqual(removeJournalEntry(ok, "a").entries, []);
});

test("append dedupes by id", () => {
  const note = createJournalEntry(
    { kind: "note", text: "hello" },
    "a",
    "2026-08-27T00:00:00.000Z",
  );
  const state = appendJournalEntry(emptyJournalState(), note);
  assert.equal(appendJournalEntry(state, note), state);
});

function renderCard(entry: JournalEntry) {
  return renderToStaticMarkup(
    createElement(JournalTweetCard, { entry: entry as JournalTweetEntry }),
  );
}

test("tweet card renders header, text, media, and linked timestamp", () => {
  const html = renderCard(tweetEntry(snapshot("463440424141459456")));
  assert.match(html, /journal-tweet-head/);
  assert.match(html, /@Interior/);
  assert.match(html, /Sunsets don(&#x27;|')t get much better/);
  assert.match(html, /journal-tweet-media/);
  assert.match(html, /pbs\.twimg\.com\/media/);
  assert.match(html, /https:\/\/x\.com\/Interior\/status\/463440424141459456/);
});

test("tweet card renders quote tweets as a nested mini-card", () => {
  const html = renderCard(tweetEntry(snapshot("1599367266448994304")));
  assert.match(html, /journal-tweet-quote/);
  assert.match(html, /@CantBeFaraz/);
  // Outer video poster and the quoted tweet's own media both render.
  assert.match(html, /journal-tweet-video/);
  // Expanded link labels replace t.co.
  assert.doesNotMatch(html, /https:\/\/t\.co\//);
  assert.match(html, /codesandbox\.io/);
});

test("tweet card marks long posts with a full-text link", () => {
  const html = renderCard(tweetEntry(snapshot("1623411400545632256")));
  assert.match(html, /journal-tweet-partial/);
  assert.match(html, /full text on X/);
  assert.match(html, /…/);
});

test("tweet card shows reply context", () => {
  const html = renderCard(tweetEntry(snapshot("1674865731136020505")));
  assert.match(html, /Replying to @xDaily/);
});
