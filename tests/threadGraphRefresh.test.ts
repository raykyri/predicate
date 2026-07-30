import assert from "node:assert/strict";
import test from "node:test";
import {
  ThreadGraphRequestTracker,
  uniqueResolvedThreadIds,
} from "../src/lib/threadGraphRefresh";

test("a newer thread-graph request invalidates an older response", () => {
  const tracker = new ThreadGraphRequestTracker();
  const initial = tracker.begin("thread-1");
  const refresh = tracker.begin("thread-1");

  assert.equal(tracker.isLatest("thread-1", initial), false);
  assert.equal(tracker.isLatest("thread-1", refresh), true);
});

test("thread-graph batches keep valid agents when a peer disappeared", () => {
  const threadIds = uniqueResolvedThreadIds(
    ["removed-agent", "visible-agent", "same-thread-agent"],
    (agentId) => {
      if (agentId === "removed-agent") {
        return null;
      }
      return "thread-visible";
    },
  );

  assert.deepEqual(threadIds, ["thread-visible"]);
});
