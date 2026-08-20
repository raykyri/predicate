import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import {
  COMPLETION_SOUND_OPTIONS,
  DEFAULT_COMPLETION_SOUND,
} from "../src/lib/completionSounds";
import { DEFAULT_SETTINGS, loadSettings, saveSettings } from "../src/lib/settings";

const store = new Map<string, string>();
const appSource = readFileSync(new URL("../src/App.tsx", import.meta.url), "utf8");
(globalThis as { localStorage?: unknown }).localStorage = {
  getItem: (key: string) => store.get(key) ?? null,
  setItem: (key: string, value: string) => store.set(key, value),
  removeItem: (key: string) => store.delete(key),
};

test("the shared completion sound catalog is curated and Default is the default", () => {
  assert.equal(DEFAULT_COMPLETION_SOUND, "default");
  assert.deepEqual(
    COMPLETION_SOUND_OPTIONS.map((option) => option.label),
    [
      "None",
      "Default",
      "Confirmation",
      "Chime",
      "Light",
      "Water",
      "Warp",
      "Switch",
      "Digital",
      "Power Up",
      "Event",
      "Drum",
      "Quest",
      "Impact",
      "Pots",
      "Bell",
    ],
  );
  assert.deepEqual(
    COMPLETION_SOUND_OPTIONS.map((option) => option.systemName),
    [
      null,
      null,
      null,
      null,
      null,
      null,
      null,
      null,
      null,
      null,
      null,
      null,
      null,
      null,
      null,
      null,
    ],
  );
  assert.deepEqual(
    COMPLETION_SOUND_OPTIONS.map((option) => option.bundledName ?? null),
    [
      null,
      "success",
      "confirmation",
      "chime",
      "light",
      "water",
      "warp",
      "switch",
      "digital",
      "power-up",
      "event",
      "drum",
      "quest",
      "impact",
      "pots",
      "bell",
    ],
  );
});

test("completion sound settings round-trip and reject unknown ids", () => {
  store.clear();
  assert.equal(loadSettings().completionSound, "default");

  saveSettings({ ...DEFAULT_SETTINGS, completionSound: "digital" });
  assert.equal(loadSettings().completionSound, "digital");

  saveSettings({ ...DEFAULT_SETTINGS, completionSound: "none" });
  assert.equal(loadSettings().completionSound, "none");

  saveSettings({
    ...DEFAULT_SETTINGS,
    completionSound: "arbitrary-path" as unknown as typeof DEFAULT_SETTINGS.completionSound,
  });
  assert.equal(loadSettings().completionSound, "default");
});

test("Basic settings preview completion sounds above Worktree location", () => {
  const basicSettingsStart = appSource.indexOf(
    "Code mode (enables worktrees, extra shell UI, etc.)",
  );
  const completionSoundSelect = appSource.indexOf('id="settings-completion-sound"');
  const worktreeLocationSelect = appSource.indexOf('id="settings-worktree-location"');

  assert.notEqual(basicSettingsStart, -1);
  assert.ok(completionSoundSelect > basicSettingsStart);
  assert.ok(worktreeLocationSelect > completionSoundSelect);
  assert.match(
    appSource,
    /setSettings\(\(current\) => \(\{ \.\.\.current, completionSound \}\)\);\s+void testCompletionSound\(completionSound\);/,
  );
});
