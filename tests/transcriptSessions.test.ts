import assert from "node:assert/strict";
import test from "node:test";
import {
  sessionPickerTopologyChanged,
  TranscriptOptionsRequestTracker,
} from "../src/lib/transcriptSessions";

test("a newer session-list scan invalidates an older response", () => {
  const tracker = new TranscriptOptionsRequestTracker();
  const initial = tracker.begin("agent-1");
  const refresh = tracker.begin("agent-1");

  assert.equal(tracker.isLatest("agent-1", initial), false);
  assert.equal(tracker.isLatest("agent-1", refresh), true);
  assert.equal(tracker.isLatest("agent-2", refresh), false);

  tracker.retain(new Set(["agent-2"]));
  assert.equal(tracker.isLatest("agent-1", refresh), false);
});

test("session topology events invalidate visible picker snapshots", () => {
  for (const eventType of [
    "agent.session_start",
    "agent.transcript_bound",
    "agent.transcript_recovered",
    "pane.removed",
  ]) {
    assert.equal(sessionPickerTopologyChanged(eventType), true, eventType);
  }

  for (const eventType of ["agent.running", "turn.appended", "pane.renamed"]) {
    assert.equal(sessionPickerTopologyChanged(eventType), false, eventType);
  }
});
