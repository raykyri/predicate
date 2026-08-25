import assert from "node:assert/strict";
import test from "node:test";
import {
  formatLauncherModelLabel,
  nextModelPreset,
  selectedModelPreset,
} from "../src/lib/launcherModels";

test("preserves exact Codex model ids in launcher labels", () => {
  assert.equal(formatLauncherModelLabel("codex", "gpt-5.6-sol"), "gpt-5.6-sol");
  assert.equal(formatLauncherModelLabel("codex", "custom"), "Custom");
  assert.equal(formatLauncherModelLabel("claude", "opus"), "Opus");
});

test("cycles model presets within the selected provider", () => {
  assert.equal(selectedModelPreset("codex", null), "gpt-5.6-sol");
  assert.equal(nextModelPreset("codex", "gpt-5.6-sol"), "gpt-5.6-terra");
  assert.equal(nextModelPreset("codex", "custom"), "gpt-5.6-sol");
  assert.equal(nextModelPreset("claude", "opus"), "fable");
});
