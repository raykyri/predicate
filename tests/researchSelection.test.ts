import assert from "node:assert/strict";
import test from "node:test";
import {
  researchSelectionActionPlacement,
  shouldDismissEmptyResearchAskOnClick,
  snapResearchDragSelection,
} from "../src/lib/researchSelection";

function rect(left: number, top: number, width: number, height: number) {
  return {
    left,
    right: left + width,
    top,
    bottom: top + height,
    width,
    height,
  };
}

test("places selection actions beside the final line of a multi-line selection", () => {
  assert.deepEqual(
    researchSelectionActionPlacement({
      fragments: [rect(80, 40, 900, 24), rect(80, 64, 230, 24)],
      boundingRect: rect(80, 40, 900, 48),
      viewportWidth: 1200,
      viewportHeight: 800,
      reservedWidth: 260,
    }),
    { left: 314, top: 58.5, offscreen: false },
  );
});

test("drops selection actions below the final line when they do not fit beside it", () => {
  assert.deepEqual(
    researchSelectionActionPlacement({
      fragments: [rect(80, 40, 900, 24), rect(900, 64, 230, 24)],
      boundingRect: rect(80, 40, 1050, 48),
      viewportWidth: 1200,
      viewportHeight: 800,
      reservedWidth: 260,
    }),
    { left: 870, top: 92, offscreen: false },
  );
});

test("dismisses an empty targeted ask only when a click clears the selection", () => {
  const base = {
    followup: "",
    selectionCollapsed: true,
    insideComposer: false,
    insideSelectionActions: false,
  };
  assert.equal(shouldDismissEmptyResearchAskOnClick(base), true);
  assert.equal(
    shouldDismissEmptyResearchAskOnClick({ ...base, selectionCollapsed: false }),
    false,
  );
  assert.equal(
    shouldDismissEmptyResearchAskOnClick({ ...base, insideComposer: true }),
    false,
  );
  assert.equal(
    shouldDismissEmptyResearchAskOnClick({ ...base, insideSelectionActions: true }),
    false,
  );
});

test("preserves targeted asks containing text", () => {
  const base = {
    selectionCollapsed: true,
    insideComposer: false,
    insideSelectionActions: false,
  };
  assert.equal(shouldDismissEmptyResearchAskOnClick({ ...base, followup: "question" }), false);
  assert.equal(shouldDismissEmptyResearchAskOnClick({ ...base, followup: "  \n" }), true);
});

test("snaps forward and backward drags to whole words", () => {
  const text = "The quick, brown fox.";
  assert.deepEqual(snapResearchDragSelection(text, 6, 13), {
    start: 4,
    end: 16,
    direction: "forward",
  });
  assert.deepEqual(snapResearchDragSelection(text, 13, 6), {
    start: 4,
    end: 16,
    direction: "backward",
  });
});

test("keeps the anchor word selected while a drag reverses inside it", () => {
  const text = "The quick brown fox";
  assert.deepEqual(snapResearchDragSelection(text, 7, 5), {
    start: 4,
    end: 9,
    direction: "backward",
  });
  assert.deepEqual(snapResearchDragSelection(text, 7, 7), {
    start: 4,
    end: 9,
    direction: "forward",
  });
});

test("excludes outer whitespace and punctuation but retains them internally", () => {
  const text = "The quick, brown fox.";
  assert.deepEqual(snapResearchDragSelection(text, 5, 10), {
    start: 4,
    end: 9,
    direction: "forward",
  });
  assert.deepEqual(snapResearchDragSelection(text, 5, 20), {
    start: 4,
    end: 20,
    direction: "forward",
  });
});

test("follows locale-aware boundaries for contractions, hyphens, and CJK", () => {
  assert.deepEqual(snapResearchDragSelection("don't stop", 2, 2), {
    start: 0,
    end: 5,
    direction: "forward",
  });
  assert.deepEqual(snapResearchDragSelection("state-of-the-art", 1, 10), {
    start: 0,
    end: 12,
    direction: "forward",
  });
  assert.deepEqual(snapResearchDragSelection("你好世界", 1, 3), {
    start: 0,
    end: 4,
    direction: "forward",
  });
});

test("treats composed emoji as indivisible selectable units", () => {
  const text = "go 👩‍💻 now";
  assert.deepEqual(snapResearchDragSelection(text, 4, 4), {
    start: 3,
    end: 8,
    direction: "forward",
  });
  assert.deepEqual(snapResearchDragSelection("e\u0301lan", 2, 2), {
    start: 0,
    end: 5,
    direction: "forward",
  });
});

test("never snaps across a message seam", () => {
  // The projection carries no separator between messages, so without the seam
  // segmentation fuses this turn's last word with the next turn's first.
  const turn = "Make it faster";
  const text = `${turn}I rewrote the loop`;
  assert.deepEqual(snapResearchDragSelection(text, 8, turn.length), {
    start: 8,
    end: turn.length + 1,
    direction: "forward",
  });
  assert.deepEqual(
    snapResearchDragSelection(text, 8, turn.length, undefined, [turn.length]),
    { start: 8, end: turn.length, direction: "forward" },
  );
  // A period between letters does not break a word either, so a turn ending in
  // one fuses just the same.
  const punctuated = "Make it faster.";
  const punctuatedText = `${punctuated}I rewrote the loop`;
  assert.deepEqual(snapResearchDragSelection(punctuatedText, 8, punctuated.length), {
    start: 8,
    end: punctuated.length + 1,
    direction: "forward",
  });
  assert.deepEqual(
    snapResearchDragSelection(punctuatedText, 8, punctuated.length, undefined, [
      punctuated.length,
    ]),
    { start: 8, end: punctuated.length, direction: "forward" },
  );
  // Dragging back from inside the next turn stays inside it: the seam ends the
  // leading unit at the turn's first character rather than the previous turn's
  // last word.
  assert.deepEqual(
    snapResearchDragSelection(text, turn.length + 3, turn.length, undefined, [
      turn.length,
    ]),
    { start: turn.length, end: turn.length + 9, direction: "backward" },
  );
  // A drag that genuinely spans two messages still snaps on both outer edges;
  // the seam only stops snapping from creating the crossing.
  assert.deepEqual(
    snapResearchDragSelection(text, 10, turn.length + 3, undefined, [turn.length]),
    { start: 8, end: turn.length + 9, direction: "forward" },
  );
});

test("rejects invalid offsets", () => {
  assert.equal(snapResearchDragSelection("answer", -1, 2), null);
  assert.equal(snapResearchDragSelection("answer", 1, 99), null);
});

test("falls back cleanly when the locale cannot be segmented", () => {
  assert.equal(snapResearchDragSelection("answer", 1, 2, "not_a_locale"), null);
});
