import assert from "node:assert/strict";
import test from "node:test";
import { formatShortRelativeTime } from "../src/components/UserNotificationStack";

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
