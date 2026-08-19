import assert from "node:assert/strict";
import test from "node:test";
import {
  collapseOlderNonUserItems,
  historyScanCollapsesItems,
  pickHistoryScanAnchor,
  remapSectionLabels,
  scrollTopDeltaToKeepOffset,
} from "../src/lib/transcriptHistoryScan";
import type { MessageItem } from "../src/lib/turnTimeline";

function item(key: string, role: string): MessageItem {
  return {
    type: "message",
    key,
    role,
    blocks: [],
    activities: [],
    sourceTurnIds: [key],
    blockSourceTurnIds: [],
  };
}

test("collapseOlderNonUserItems drops assistant cards above the pin and keeps the live turn", () => {
  const items = [
    item("a1", "assistant"),
    item("u1", "user"),
    item("a2", "assistant"),
    item("u2", "user"),
    item("a3", "assistant"),
  ];
  const collapsed = collapseOlderNonUserItems(items, "u2");
  assert.deepEqual(
    collapsed.map((entry) => entry.key),
    ["u1", "u2", "a3"],
  );
  assert.equal(historyScanCollapsesItems(items, "u2"), true);
  assert.equal(historyScanCollapsesItems(items, "u1"), true);
  assert.equal(historyScanCollapsesItems(collapsed, "u2"), false);
});

test("collapseOlderNonUserItems is a no-op when the pin is first or missing", () => {
  const items = [item("u1", "user"), item("a1", "assistant")];
  assert.deepEqual(
    collapseOlderNonUserItems(items, "u1").map((entry) => entry.key),
    ["u1", "a1"],
  );
  assert.equal(historyScanCollapsesItems(items, "u1"), false);
  assert.deepEqual(
    collapseOlderNonUserItems(items, "missing").map((entry) => entry.key),
    ["u1", "a1"],
  );
});

test("remapSectionLabels moves a divider onto the next visible card", () => {
  const original = [item("a1", "assistant"), item("u1", "user"), item("u2", "user")];
  const visible = [item("u1", "user"), item("u2", "user")];
  const labels = new Map([
    ["a1", "Previous conversation"],
    ["u2", "Current conversation"],
  ]);
  const remapped = remapSectionLabels(labels, original, visible);
  assert.equal(remapped.get("u1"), "Previous conversation");
  assert.equal(remapped.get("u2"), "Current conversation");
});

test("scroll compensation restores a card that jumped up after collapse", () => {
  assert.equal(scrollTopDeltaToKeepOffset(12, 52), -40);
  assert.equal(scrollTopDeltaToKeepOffset(52, 52), 0);
});

test("pickHistoryScanAnchor prefers the nearest user card above a visible assistant reply", () => {
  const cards = [
    { key: "u1", role: "user", top: -80, bottom: -20 },
    { key: "a1", role: "assistant", top: 8, bottom: 80 },
    { key: "u2", role: "user", top: 96, bottom: 140 },
  ];
  const anchor = pickHistoryScanAnchor(cards, 0, 10);
  assert.deepEqual(anchor, { key: "u1", offset: -80 });
});

test("pickHistoryScanAnchor keeps a user card that is already at the top", () => {
  const cards = [
    { key: "u1", role: "user", top: 10, bottom: 40 },
    { key: "u2", role: "user", top: 48, bottom: 80 },
  ];
  const anchor = pickHistoryScanAnchor(cards, 0, 10);
  assert.deepEqual(anchor, { key: "u1", offset: 10 });
});

test("pickHistoryScanAnchor walks forward when the opener is an assistant greeting", () => {
  const cards = [
    { key: "a0", role: "assistant", top: 8, bottom: 40 },
    { key: "u1", role: "user", top: 48, bottom: 80 },
  ];
  const anchor = pickHistoryScanAnchor(cards, 0, 10);
  assert.deepEqual(anchor, { key: "u1", offset: 48 });
});

test("pickHistoryScanAnchor returns null when no user card exists", () => {
  const cards = [{ key: "a0", role: "assistant", top: 8, bottom: 40 }];
  assert.equal(pickHistoryScanAnchor(cards, 0, 10), null);
});
