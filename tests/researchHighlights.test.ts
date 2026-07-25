import assert from "node:assert/strict";
import test from "node:test";
import {
  expandedResearchHighlightOffsets,
  intersectingResearchHighlightIds,
  overlappingResearchHighlightRegions,
  researchAnchorContextBounds,
  resolveResearchHighlightOffset,
} from "../src/lib/researchHighlights";
import type { ResearchHighlight } from "../src/types";

function highlight(
  exact: string,
  start: number,
  options: { prefix?: string; suffix?: string; revision?: string } = {},
): ResearchHighlight {
  return {
    id: "h1",
    createdAt: 0,
    anchor: {
      version: 1,
      projection: "answer-v1",
      responseRevision: options.revision ?? "rev",
      start,
      end: start + exact.length,
      exact,
      prefix: options.prefix ?? "",
      suffix: options.suffix ?? "",
    },
  };
}

test("intersecting ids: overlap counts, edge contact does not", () => {
  const highlights = [
    { id: "a", start: 10, end: 20 },
    { id: "b", start: 30, end: 40 },
  ];
  assert.deepEqual(
    intersectingResearchHighlightIds({ start: 15, end: 35 }, highlights),
    ["a", "b"],
  );
  assert.deepEqual(
    intersectingResearchHighlightIds({ start: 20, end: 30 }, highlights),
    [],
  );
});

test("expand: no overlap yields nothing to expand", () => {
  assert.equal(
    expandedResearchHighlightOffsets({ start: 0, end: 5 }, [
      { id: "a", start: 10, end: 20 },
    ]),
    null,
  );
});

test("expand: a selection inside one highlight would only recreate it", () => {
  assert.equal(
    expandedResearchHighlightOffsets({ start: 12, end: 18 }, [
      { id: "a", start: 10, end: 20 },
    ]),
    null,
  );
  // Selecting the entire highlight is equally a no-op.
  assert.equal(
    expandedResearchHighlightOffsets({ start: 10, end: 20 }, [
      { id: "a", start: 10, end: 20 },
    ]),
    null,
  );
});

test("expand: a selection extending past a highlight grows it", () => {
  assert.deepEqual(
    expandedResearchHighlightOffsets({ start: 15, end: 25 }, [
      { id: "a", start: 10, end: 20 },
    ]),
    { start: 10, end: 25 },
  );
  assert.deepEqual(
    expandedResearchHighlightOffsets({ start: 5, end: 12 }, [
      { id: "a", start: 10, end: 20 },
    ]),
    { start: 5, end: 20 },
  );
});

test("expand: a selection bridging several highlights merges them", () => {
  assert.deepEqual(
    expandedResearchHighlightOffsets({ start: 15, end: 35 }, [
      { id: "a", start: 10, end: 20 },
      { id: "b", start: 30, end: 40 },
    ]),
    { start: 10, end: 40 },
  );
});

test("overlap regions: partial overlap yields the shared span only", () => {
  assert.deepEqual(
    overlappingResearchHighlightRegions([
      { start: 10, end: 30 },
      { start: 20, end: 40 },
    ]),
    [{ start: 20, end: 30 }],
  );
});

test("overlap regions: edge contact and disjoint ranges yield nothing", () => {
  assert.deepEqual(
    overlappingResearchHighlightRegions([
      { start: 10, end: 20 },
      { start: 20, end: 30 },
      { start: 40, end: 50 },
    ]),
    [],
  );
});

test("overlap regions: containment and triple stacks merge into one span", () => {
  assert.deepEqual(
    overlappingResearchHighlightRegions([
      { start: 0, end: 50 },
      { start: 10, end: 20 },
      { start: 15, end: 35 },
    ]),
    [{ start: 10, end: 35 }],
  );
});

test("overlap regions: chained pairwise overlaps stay contiguous", () => {
  // a∩b ends exactly where b∩c begins; the paint should not split there.
  assert.deepEqual(
    overlappingResearchHighlightRegions([
      { start: 0, end: 10 },
      { start: 5, end: 15 },
      { start: 10, end: 20 },
    ]),
    [{ start: 5, end: 15 }],
  );
});

test("overlap regions: empty ranges never count toward depth", () => {
  assert.deepEqual(
    overlappingResearchHighlightRegions([
      { start: 10, end: 10 },
      { start: 5, end: 15 },
    ]),
    [],
  );
});

test("context bounds: the enclosing message clamps every kind", () => {
  const messageBounds = { start: 14, end: 32 };
  assert.deepEqual(
    researchAnchorContextBounds({
      isConversation: true,
      messageBounds,
      projectionLength: 50,
    }),
    messageBounds,
  );
  assert.deepEqual(
    researchAnchorContextBounds({
      isConversation: false,
      messageBounds,
      projectionLength: 50,
    }),
    messageBounds,
  );
});

test("context bounds: only a conversation refuses a selection spanning messages", () => {
  // No enclosing message means the selection crossed one. A run's messages are
  // one speaker, so the whole projection is fair context; a conversation's are
  // separate turns, which is not anchorable at all.
  assert.equal(
    researchAnchorContextBounds({
      isConversation: true,
      messageBounds: null,
      projectionLength: 50,
    }),
    null,
  );
  assert.deepEqual(
    researchAnchorContextBounds({
      isConversation: false,
      messageBounds: null,
      projectionLength: 50,
    }),
    { start: 0, end: 50 },
  );
});

test("resolve: a whole-message quote keeps its place without either context", () => {
  // Context is clamped to the enclosing message, so selecting a whole
  // conversation turn saves no prefix and no suffix. The turn is neither at the
  // start nor at the end of the projection, and the anchor must still resolve —
  // treating an empty side as "must sit at the projection edge" orphaned it the
  // moment it was created.
  const turns = ["Make it faster", "I rewrote the loop", "Why is that faster"];
  const projection = turns.join("");
  const start = turns[0].length;
  assert.deepEqual(
    resolveResearchHighlightOffset(projection, "rev", highlight(turns[1], start)),
    { start, end: start + turns[1].length },
  );
  // Still located after the view shifted the offsets, since the quote itself
  // carries the anchor.
  assert.deepEqual(
    resolveResearchHighlightOffset(
      `Earlier turn${projection}`,
      "other",
      highlight(turns[1], start),
    ),
    { start: start + 12, end: start + 12 + turns[1].length },
  );
});

test("resolve: one clamped side still discriminates between repeats", () => {
  // A turn-leading quote saves a suffix but no prefix; the suffix alone has to
  // pick the right occurrence of a phrase that repeats.
  const projection = "Yes, for the reasons above.Yes, but only on macOS.";
  assert.deepEqual(
    resolveResearchHighlightOffset(
      projection,
      "rev",
      highlight("Yes", 27, { suffix: ", but only" }),
    ),
    { start: 27, end: 30 },
  );
  // No occurrence keeps the context: the highlight is orphaned rather than
  // painted on a guess.
  assert.equal(
    resolveResearchHighlightOffset(
      projection,
      "rev",
      highlight("Yes", 27, { suffix: ", and also" }),
    ),
    null,
  );
});

test("expand: only intersected highlights join the union", () => {
  assert.deepEqual(
    expandedResearchHighlightOffsets({ start: 15, end: 25 }, [
      { id: "a", start: 10, end: 20 },
      { id: "far", start: 100, end: 110 },
    ]),
    { start: 10, end: 25 },
  );
});
