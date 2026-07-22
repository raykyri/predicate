import assert from "node:assert/strict";
import test from "node:test";

// settings.ts touches localStorage only inside load/saveSettings, but those
// are exactly what these tests exercise, so give the node process a stub.
const store = new Map<string, string>();
(globalThis as { localStorage?: unknown }).localStorage = {
  getItem: (key: string) => store.get(key) ?? null,
  setItem: (key: string, value: string) => {
    store.set(key, value);
  },
  removeItem: (key: string) => {
    store.delete(key);
  },
};

import {
  clampResearchLaunchInstruction,
  DEFAULT_SETTINGS,
  loadSettings,
  RESEARCH_LAUNCH_INSTRUCTION_MAX_BYTES,
  saveSettings,
} from "../src/lib/settings";

function utf8Bytes(value: string): number {
  return Buffer.byteLength(value, "utf8");
}

test("short instructions pass through the clamp unchanged", () => {
  assert.equal(clampResearchLaunchInstruction(""), "");
  assert.equal(
    clampResearchLaunchInstruction("Answer concisely, in a few short paragraphs."),
    "Answer concisely, in a few short paragraphs.",
  );
});

test("the clamp counts UTF-8 bytes and never splits a code point", () => {
  // Two bytes per char: half the cap in characters fits, not one more.
  const twoByte = "é".repeat(RESEARCH_LAUNCH_INSTRUCTION_MAX_BYTES);
  const clampedTwoByte = clampResearchLaunchInstruction(twoByte);
  assert.equal(utf8Bytes(clampedTwoByte), RESEARCH_LAUNCH_INSTRUCTION_MAX_BYTES);
  assert.equal(clampedTwoByte.length, RESEARCH_LAUNCH_INSTRUCTION_MAX_BYTES / 2);

  // Four-byte emoji (surrogate pairs in UTF-16): the cut must land between
  // code points, never inside one.
  const emoji = "🌊".repeat(RESEARCH_LAUNCH_INSTRUCTION_MAX_BYTES);
  const clampedEmoji = clampResearchLaunchInstruction(emoji);
  assert.ok(utf8Bytes(clampedEmoji) <= RESEARCH_LAUNCH_INSTRUCTION_MAX_BYTES);
  assert.equal(clampedEmoji.length % 2, 0, "no dangling surrogate half");
  assert.ok(!clampedEmoji.includes("\uFFFD"));
});

test("settings round-trip the research launch instruction", () => {
  store.clear();
  assert.equal(loadSettings().researchLaunchInstruction, "");

  saveSettings({ ...DEFAULT_SETTINGS, researchLaunchInstruction: "Keep answers short." });
  assert.equal(loadSettings().researchLaunchInstruction, "Keep answers short.");
});

test("corrupt or oversized stored instructions degrade safely", () => {
  store.clear();
  saveSettings({
    ...DEFAULT_SETTINGS,
    researchLaunchInstruction: 42 as unknown as string,
  });
  assert.equal(loadSettings().researchLaunchInstruction, "");

  store.clear();
  saveSettings({
    ...DEFAULT_SETTINGS,
    researchLaunchInstruction: "x".repeat(RESEARCH_LAUNCH_INSTRUCTION_MAX_BYTES * 2),
  });
  assert.equal(
    utf8Bytes(loadSettings().researchLaunchInstruction),
    RESEARCH_LAUNCH_INSTRUCTION_MAX_BYTES,
  );
});
