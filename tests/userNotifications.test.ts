import assert from "node:assert/strict";
import test from "node:test";
import { formatShortRelativeTime } from "../src/components/UserNotificationStack";
import {
  normalizeNotificationLog,
  notificationLogHasUnread,
} from "../src/lib/notificationLog";

const now = Date.parse("2026-08-27T09:00:00.000Z");

test("formatShortRelativeTime uses compact units that tick at one-second resolution", () => {
  assert.equal(formatShortRelativeTime(now, now), "now");
  assert.equal(formatShortRelativeTime(now - 999, now), "now");
  assert.equal(formatShortRelativeTime(now - 1_000, now), "1s");
  assert.equal(formatShortRelativeTime(now - 5_000, now), "5s");
  assert.equal(formatShortRelativeTime(now - 59_999, now), "59s");
  assert.equal(formatShortRelativeTime(now - 60_000, now), "1m");
  assert.equal(formatShortRelativeTime(now - 3 * 60_000, now), "3m");
  assert.equal(formatShortRelativeTime(now - 60 * 60_000, now), "1h");
  assert.equal(formatShortRelativeTime(now - 5 * 60 * 60_000, now), "5h");
  assert.equal(formatShortRelativeTime(now - 24 * 60 * 60_000, now), "1d");
  assert.equal(formatShortRelativeTime(now + 2_000, now), "now");
});

test("normalizeNotificationLog keeps oldest-first unique rows and drops junk", () => {
  const entries = normalizeNotificationLog({
    entries: [
      {
        id: "a",
        title: "qmux",
        body: "CI finished on main.",
        tone: "success",
        paneId: "pane-1",
        createdAt: 1,
        read: false,
      },
      { id: "bad" },
      {
        id: "a",
        title: "duplicate",
        body: "ignored",
        createdAt: 2,
        read: true,
      },
      {
        id: "b",
        title: "research-agent",
        body: "Published the draft.",
        createdAt: 3,
        read: true,
      },
    ],
  });
  assert.equal(entries.length, 2);
  assert.equal(entries[0].id, "a");
  assert.equal(entries[0].title, "qmux");
  assert.equal(entries[0].paneId, "pane-1");
  assert.equal(entries[0].read, false);
  assert.equal(entries[1].id, "b");
  assert.equal(entries[1].read, true);
  assert.equal(notificationLogHasUnread(entries), true);
  assert.equal(notificationLogHasUnread(entries.map((entry) => ({ ...entry, read: true }))), false);
  assert.deepEqual(normalizeNotificationLog(null), []);
  assert.deepEqual(normalizeNotificationLog([]), []);
});

