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
    COMPLETION_SOUND_OPTIONS.map((option) => [
      option.id,
      option.label,
      option.bundledName ?? null,
      option.systemName,
      option.systemPath ?? null,
    ]),
    [
      ["none", "None", null, null, null],
      ["default", "Default", "success", null, null],
      ["confirmation", "Confirmation", "confirmation", null, null],
      ["chime", "Chime", "chime", null, null],
      [
        "messages",
        "Messages",
        null,
        null,
        "/System/Library/Components/CoreAudio.component/Contents/SharedSupport/SystemSounds/system/SentMessage.caf",
      ],
      [
        "apple-pay",
        "Apple Pay",
        null,
        null,
        "/System/Library/Components/CoreAudio.component/Contents/SharedSupport/SystemSounds/system/payment_success.aif",
      ],
      ["nokia", "Nokia", "nokia", null, null],
      ["metal-gear", "Metal Gear", "metal-gear", null, null],
      ["minecraft", "Minecraft", "minecraft", null, null],
      ["door", "Door", "door", null, null],
      ["light", "Light", "light", null, null],
      ["water", "Water", "water", null, null],
      ["warp", "Warp", "warp", null, null],
      ["switch", "Switch", "switch", null, null],
      ["digital", "Digital", "digital", null, null],
      ["power-up", "Power Up", "power-up", null, null],
      ["event", "Event", "event", null, null],
      ["drum", "Drum", "drum", null, null],
      ["quest", "Quest", "quest", null, null],
      ["impact", "Impact", "impact", null, null],
      ["pots", "Pots", "pots", null, null],
      ["bell", "Bell", "bell", null, null],
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
