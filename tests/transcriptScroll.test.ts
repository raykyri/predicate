import assert from "node:assert/strict";
import test from "node:test";
import {
  captureTranscriptScrollPosition,
  createTranscriptScrollCaptureSlot,
  transcriptScrollRestoreTop,
} from "../src/lib/transcriptScroll";

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

test("a near-tail position keeps its exact offset across a tab round-trip", () => {
  const saved = captureTranscriptScrollPosition(
    { scrollTop: 850, scrollHeight: 1_000, clientHeight: 100 },
    false,
    100,
  );

  assert.equal(saved.stuck, true, "near-tail content should still live-follow");
  assert.equal(saved.atEnd, false, "near-tail is not the physical end");
  assert.equal(transcriptScrollRestoreTop(saved, 1_000), 850);
});

test("a pinned sticky-user viewport does not restore to the transcript end", () => {
  // Sticky positioning changes how the user card paints, not these scroll
  // metrics. Model a pinned card with a viewport still 60px above the end.
  const saved = captureTranscriptScrollPosition(
    { scrollTop: 1_340, scrollHeight: 1_500, clientHeight: 100 },
    false,
    100,
  );

  assert.equal(saved.stuck, true);
  assert.equal(saved.atEnd, false);
  // Streaming while this tab is hidden must not reinterpret the pinned
  // viewport as a request to jump to the new tail.
  assert.equal(transcriptScrollRestoreTop(saved, 1_800), 1_340);
});

test("true tail and explicit jump intent still follow content that grows while hidden", () => {
  const atTail = captureTranscriptScrollPosition(
    { scrollTop: 900, scrollHeight: 1_000, clientHeight: 100 },
    false,
    100,
  );
  const jumping = captureTranscriptScrollPosition(
    { scrollTop: 500, scrollHeight: 1_000, clientHeight: 100 },
    true,
    100,
  );

  assert.equal(transcriptScrollRestoreTop(atTail, 1_250), 1_250);
  assert.equal(transcriptScrollRestoreTop(jumping, 1_250), 1_250);
});

test("an older scrolled position remains anchored when content grows while hidden", () => {
  const saved = captureTranscriptScrollPosition(
    { scrollTop: 300, scrollHeight: 1_000, clientHeight: 100 },
    false,
    100,
  );

  assert.equal(saved.stuck, false);
  assert.equal(saved.atEnd, false);
  assert.equal(transcriptScrollRestoreTop(saved, 1_400), 300);
});
