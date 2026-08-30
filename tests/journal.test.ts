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
import { insertJournalEntryAt } from "../src/lib/journal";
import {
  syndicationToken,
  tweetIdFromUrl,
  tweetSnapshotFromSyndication,
  type TweetSnapshot,
} from "../src/lib/journalTweets";
import {
  JournalTweetCard,
  journalEntryMenuItems,
  journalEntryUrl,
} from "../src/components/research/JournalPane";

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
  // The quoted tweet keeps its link inline (its own card is not this tweet's).
  const quoted = snapshot("1599367266448994304").quoted;
  assert.ok(quoted);
  const link = quoted.runs.find((run) => run.kind === "link");
  assert.ok(link);
  assert.ok(link.url?.startsWith("https://"));
  assert.ok(!link.url?.includes("t.co"));
  assert.ok(!link.text.includes("t.co"));
  assert.ok(quoted.runs.every((run) => !run.text.includes("https://t.co")));
});

test("a link card is lifted out of the text it stands for", () => {
  const tweet = snapshot("1628832338187636740");
  const card = tweet.card;
  assert.ok(card);
  assert.equal(card.domain, "nextjs.org");
  assert.equal(card.title, "Next.js 13.2");
  assert.ok(card.description);
  assert.ok(card.large);
  assert.ok(card.imageUrl?.startsWith("https://pbs.twimg.com/card_img/"));
  assert.ok(card.url.startsWith("https://nextjs.org"));
  // The trailing t.co that produced the card no longer doubles as body text.
  assert.ok(tweet.runs.every((run) => run.url !== card.url));
  assert.ok(!tweet.runs[tweet.runs.length - 1].text.endsWith(" "));
  // Tweets without a preview carry no card.
  assert.equal(snapshot("20").card, undefined);
});

test("engagement counts and verification come through", () => {
  const tweet = snapshot("20");
  assert.equal(tweet.likes, 309060);
  assert.equal(tweet.replies, 17999);
  assert.equal(tweet.author.verified, true);
  assert.equal(snapshot("1628832338187636740").author.verified, false);
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

test("insert-at restores a removed entry at its original position", () => {
  const at = (n: number) =>
    createJournalEntry({ kind: "note", text: `n${n}` }, `id${n}`, "2026-08-27");
  let state = emptyJournalState();
  for (const n of [0, 1, 2]) {
    state = appendJournalEntry(state, at(n));
  }
  const removed = removeJournalEntry(state, "id1");
  const restored = insertJournalEntryAt(removed, at(1), 1);
  assert.deepEqual(restored, state);
  // Double-undo can't duplicate.
  assert.equal(insertJournalEntryAt(restored, at(1), 1), restored);
  // Out-of-range indices clamp instead of throwing.
  assert.equal(insertJournalEntryAt(removed, at(1), 99).entries.length, 3);
  assert.equal(insertJournalEntryAt(removed, at(1), -5).entries[0].text, "n1");
});

test("context menu items and keycaps track entry kind and state", () => {
  const note = createJournalEntry({ kind: "note", text: "hi" }, "a", "2026-08-27");
  assert.deepEqual(
    journalEntryMenuItems(note).map((item) => [item.action, item.key]),
    [
      ["copy", "C"],
      ["delete", "D"],
    ],
  );
  assert.equal(journalEntryUrl(note), null);

  const link = createJournalEntry(
    { kind: "link", url: "https://example.com" },
    "b",
    "2026-08-27",
  );
  assert.deepEqual(
    journalEntryMenuItems(link).map((item) => item.action),
    ["open", "copy", "delete"],
  );
  assert.equal(journalEntryUrl(link), "https://example.com");

  const pendingTweet = createJournalEntry(
    { kind: "tweet", url: "https://x.com/jack/status/20", tweetId: "20" },
    "c",
    "2026-08-27",
  );
  // No retry while a fetch is already owed.
  assert.deepEqual(
    journalEntryMenuItems(pendingTweet).map((item) => item.action),
    ["open", "copy", "delete"],
  );

  const okTweet = { ...tweetEntry(snapshot("20")) };
  const okItems = journalEntryMenuItems(okTweet);
  assert.deepEqual(
    okItems.map((item) => [item.action, item.key]),
    [
      ["open", "O"],
      ["copy", "C"],
      ["retry", "R"],
      ["delete", "D"],
    ],
  );
  assert.equal(okItems.find((item) => item.action === "retry")?.label, "Refresh tweet");
  assert.ok(okItems.find((item) => item.action === "delete")?.danger);
  // The canonical hydrated permalink wins over what the user typed.
  assert.equal(journalEntryUrl({ ...okTweet, url: "https://x.com/JACK/status/20?x=1" }),
    "https://x.com/jack/status/20");

  const failedTweet = { ...pendingTweet, hydration: "failed" as const };
  assert.equal(
    journalEntryMenuItems(failedTweet).find((item) => item.action === "retry")?.label,
    "Retry tweet",
  );
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
  assert.match(html, /journal-tweet-age"[^>]*href="https:\/\/x\.com\/Interior\/status\/463440424141459456"/);
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

test("tweet card offers Show more on a long post", () => {
  const html = renderCard(tweetEntry(snapshot("1623411400545632256")));
  assert.match(html, /journal-tweet-more/);
  assert.match(html, /Show more/);
  assert.match(html, /…/);
});

test("tweet card lays out avatar, inline header, and stats like a timeline", () => {
  const html = renderCard(tweetEntry(snapshot("20")));
  // Avatar sits outside the content column, and the header is one line:
  // name, badge, handle, then the age linking to the post.
  assert.match(html, /journal-tweet-avatar-link/);
  assert.match(html, /journal-tweet-main/);
  assert.match(html, /journal-tweet-author">jack<\/span>/);
  assert.match(html, /journal-tweet-verified/);
  assert.match(html, /journal-tweet-handle">@jack<\/span>/);
  assert.match(html, /journal-tweet-age"[^>]*>[^<]+<\/a>/);
  // Counts read as metadata, never as controls.
  assert.match(html, /journal-tweet-stat/);
  assert.match(html, /309K/);
  assert.doesNotMatch(html, /<button/);
  // The embed chrome is gone.
  assert.doesNotMatch(html, /journal-tweet-x"/);
  assert.doesNotMatch(html, /journal-tweet-foot/);
});

test("tweet card renders a link preview card", () => {
  const html = renderCard(tweetEntry(snapshot("1628832338187636740")));
  assert.match(html, /journal-tweet-card is-large/);
  assert.match(html, /journal-tweet-card-domain">nextjs\.org</);
  assert.match(html, /journal-tweet-card-title">Next\.js 13\.2</);
  assert.match(html, /card_img/);
});

test("tweet card shows reply context", () => {
  const html = renderCard(tweetEntry(snapshot("1674865731136020505")));
  assert.match(html, /journal-tweet-reply/);
  assert.match(html, /Replying to <span>@xDaily<\/span>/);
});
