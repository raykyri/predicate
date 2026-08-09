import assert from "node:assert/strict";
import test from "node:test";
import { createTranscriptScrollCaptureSlot } from "../src/lib/transcriptScroll";

test("tab capture saves the outgoing expanded transcript before replacement", () => {
  const slot = createTranscriptScrollCaptureSlot();
  const captures: string[] = [];
  const unregisterExpanded = slot.register(() => captures.push("expanded"));

  slot.capture();
  assert.deepEqual(captures, ["expanded"]);

  const unregisterIncoming = slot.register(() => captures.push("incoming"));
  unregisterExpanded();
  slot.capture();
  assert.deepEqual(captures, ["expanded", "incoming"]);

  unregisterIncoming();
  slot.capture();
  assert.deepEqual(captures, ["expanded", "incoming"]);
});
