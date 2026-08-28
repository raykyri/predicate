import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import HomeGroupSelector from "../src/components/HomeGroupSelector";
import {
  centeredHorizontalScrollLeft,
  railLinkPath,
} from "../src/components/HomeRails";
import {
  latestUserTurnTimestamp,
  mergeRailPastTurns,
  railLatestUserTurn,
  railPastTurnSummaries,
  railPastTurns,
  railQueuedTurnText,
} from "../src/lib/homeRails";
import type { Turn } from "../src/types";

let nextTurnId = 0;

function makeTurn(options: {
  role: "user" | "assistant";
  text?: string;
  timestamp?: number | null;
  status?: Turn["status"];
  contextStatus?: Turn["contextStatus"];
}): Turn {
  nextTurnId += 1;
  return {
    id: `turn-${nextTurnId}`,
    agentId: "agent-1",
    role: options.role,
    blocks: [{ type: "text", text: options.text ?? `${options.role} ${nextTurnId}` }],
    sourceIndex: nextTurnId,
    timestamp: options.timestamp ?? null,
    status: options.status ?? null,
    contextStatus: options.contextStatus ?? null,
  };
}

function rect(left: number, top: number, width = 100, height = 40): DOMRect {
  return {
    x: left,
    y: top,
    left,
    top,
    width,
    height,
    right: left + width,
    bottom: top + height,
    toJSON: () => ({}),
  } as DOMRect;
}

function renderHomeSelector(draftsVisible: boolean) {
  return renderToStaticMarkup(
    createElement(HomeGroupSelector, {
      groups: [],
      draftsVisible,
      onDraftsVisibleChange: () => undefined,
      hiddenTerminalIds: new Set<string>(),
      onSetTerminalsHidden: () => undefined,
      onToggleTerminal: () => undefined,
    }),
  );
}

test("Home stream selector exposes the Drafts visibility checkbox without agent groups", () => {
  const shown = renderHomeSelector(true);
  assert.match(shown, /aria-label="Home streams"/);
  assert.match(shown, /role="checkbox" aria-checked="true"/);
  assert.match(shown, />Drafts<\/span>/);

  const hidden = renderHomeSelector(false);
  assert.match(hidden, /class="home-group-chip is-off"/);
  assert.match(hidden, /role="checkbox" aria-checked="false"/);
});

test("railPastTurns drops the dangling latest prompt (the current card)", () => {
  const turns = [
    makeTurn({ role: "user", text: "first", timestamp: 100 }),
    makeTurn({ role: "assistant", timestamp: 200 }),
    makeTurn({ role: "user", text: "second", timestamp: 300 }),
  ];
  const past = railPastTurns(turns);
  assert.equal(past.length, 1);
  assert.equal(past[0].text, "first");
});

test("railPastTurns settles each exchange at its last record before the next prompt", () => {
  const turns = [
    makeTurn({ role: "user", text: "first", timestamp: 100 }),
    makeTurn({ role: "assistant", timestamp: 250 }),
    makeTurn({ role: "user", text: "second", timestamp: 300 }),
    makeTurn({ role: "user", text: "third", timestamp: 400 }),
  ];
  const past = railPastTurns(turns);
  assert.equal(past.length, 2);
  assert.equal(past[0].settledAt, 250);
  // No records between "second" and "third": falls back to the prompt's own time.
  assert.equal(past[1].settledAt, 300);
});

test("railPastTurns skips superseded records and instruction-only prompts", () => {
  const turns = [
    makeTurn({ role: "user", text: "kept", timestamp: 100 }),
    makeTurn({ role: "assistant", timestamp: 150, status: "superseded" }),
    makeTurn({ role: "assistant", timestamp: 200 }),
    makeTurn({ role: "user", text: "<system-reminder>noise</system-reminder>", timestamp: 300 }),
    makeTurn({ role: "user", text: "latest", timestamp: 400 }),
  ];
  const past = railPastTurns(turns);
  // "kept" settles at the live assistant record; the instruction-only prompt
  // never becomes a card; "latest" is the current card.
  assert.equal(past.length, 1);
  assert.equal(past[0].text, "kept");
  assert.equal(past[0].settledAt, 200);
});

test("rail helpers omit records excluded from active context", () => {
  const turns = [
    makeTurn({ role: "user", text: "rolled back", timestamp: 100, contextStatus: "rolledBack" }),
    makeTurn({ role: "assistant", text: "old answer", timestamp: 150, contextStatus: "rolledBack" }),
    makeTurn({ role: "user", text: "active", timestamp: 200 }),
  ];

  assert.equal(railLatestUserTurn(turns), "active");
  assert.equal(latestUserTurnTimestamp(turns), 200);
  assert.deepEqual(railPastTurns(turns), []);
});

test("railPastTurns caches by turns-array identity", () => {
  const turns = [
    makeTurn({ role: "user", text: "first", timestamp: 100 }),
    makeTurn({ role: "user", text: "second", timestamp: 200 }),
  ];
  assert.equal(railPastTurns(turns), railPastTurns(turns));
});

test("compact Home history merges with the live turn window without duplicates", () => {
  const historical = railPastTurnSummaries([
    { id: "old", text: "old prompt", settledAt: null },
    { id: "overlap", text: "stale prompt", settledAt: null },
    {
      id: "instruction",
      text: "<system-reminder>noise</system-reminder>",
      settledAt: null,
    },
  ]);
  const merged = mergeRailPastTurns(historical, [
    { id: "overlap", text: "fresh prompt", settledAt: 200 },
    { id: "recent", text: "recent prompt", settledAt: 300 },
  ]);
  assert.deepEqual(merged, [
    { id: "old", text: "old prompt", settledAt: null },
    { id: "overlap", text: "fresh prompt", settledAt: 200 },
    { id: "recent", text: "recent prompt", settledAt: 300 },
  ]);
});

test("rail text helpers strip instruction blocks with raw-text fallback", () => {
  assert.equal(railQueuedTurnText("<system-reminder>only</system-reminder>").includes("only"), true);
  assert.equal(railQueuedTurnText("do the thing"), "do the thing");
  const turns = [
    makeTurn({ role: "user", text: "real prompt", timestamp: 100 }),
    makeTurn({ role: "assistant", timestamp: 200 }),
  ];
  assert.equal(railLatestUserTurn(turns), "real prompt");
  assert.equal(latestUserTurnTimestamp(turns), 100);
});

test("terminal map centers its horizontal viewport on the active rail", () => {
  assert.equal(centeredHorizontalScrollLeft(600, 1_400, [{ start: 700, end: 980 }]), 540);
});

test("terminal map centers the full span of an active split", () => {
  assert.equal(
    centeredHorizontalScrollLeft(600, 1_400, [
      { start: 300, end: 580 },
      { start: 596, end: 876 },
    ]),
    288,
  );
});

test("terminal map initial horizontal position clamps at either edge", () => {
  assert.equal(centeredHorizontalScrollLeft(600, 1_400, [{ start: 14, end: 294 }]), 0);
  assert.equal(centeredHorizontalScrollLeft(600, 1_400, [{ start: 1_120, end: 1_400 }]), 800);
});

test("terminal map leaves horizontal position alone without a visible active rail", () => {
  assert.equal(centeredHorizontalScrollLeft(600, 1_400, []), null);
});

test("railLinkPath draws a straight line for level cards", () => {
  const base = rect(0, 0, 1000, 600);
  const d = railLinkPath(rect(0, 100), rect(296, 100), base, "right", "left");
  assert.match(d, /^M [\d.]+ [\d.]+ L [\d.]+ [\d.]+$/);
});

test("railLinkPath always ends with a real horizontal segment", () => {
  const base = rect(0, 0, 1000, 600);
  // The historical marker-flip bug: a same-side loop whose vertical run lands
  // exactly one corner radius from the target edge. The final H segment must
  // keep real length so the arrowhead keeps its direction.
  const cases: Array<[DOMRect, DOMRect, "left" | "right", "left" | "right"]> = [
    [rect(0, 200), rect(296, 100), "right", "left"],
    [rect(296, 100), rect(0, 260), "left", "right"],
    [rect(0, 100), rect(0, 160), "right", "right"],
    [rect(0, 100), rect(0, 111), "right", "right"],
  ];
  for (const [from, to, fromSide, toSide] of cases) {
    const d = railLinkPath(from, to, base, fromSide, toSide);
    const match = d.match(/Q [\d.-]+ [\d.-]+ ([\d.-]+) [\d.-]+ H ([\d.-]+)$/);
    assert.ok(match, `orthogonal path expected: ${d}`);
    const cornerEndX = Number(match[1]);
    const finalX = Number(match[2]);
    assert.ok(
      Math.abs(finalX - cornerEndX) >= 1,
      `final H segment must have real length: ${d}`,
    );
  }
});
