import assert from "node:assert/strict";
import test from "node:test";
import {
  COMPLETION_SOUND_OPTIONS,
  DEFAULT_COMPLETION_SOUND,
} from "../src/lib/completionSounds";
import { DEFAULT_SETTINGS, loadSettings, saveSettings } from "../src/lib/settings";

const store = new Map<string, string>();
(globalThis as { localStorage?: unknown }).localStorage = {
  getItem: (key: string) => store.get(key) ?? null,
  setItem: (key: string, value: string) => store.set(key, value),
  removeItem: (key: string) => store.delete(key),
};

test("the shared completion sound catalog is curated and Chime is the default", () => {
  assert.equal(DEFAULT_COMPLETION_SOUND, "chime");
  assert.deepEqual(
    COMPLETION_SOUND_OPTIONS.map((option) => option.label),
    ["None", "Chime", "Ping", "Pop", "Tink", "Purr"],
  );
  assert.deepEqual(
    COMPLETION_SOUND_OPTIONS.map((option) => option.systemName),
    [null, "Glass", "Ping", "Pop", "Tink", "Purr"],
  );
});

test("completion sound settings round-trip and reject unknown ids", () => {
  store.clear();
  assert.equal(loadSettings().completionSound, "chime");

  saveSettings({ ...DEFAULT_SETTINGS, completionSound: "purr" });
  assert.equal(loadSettings().completionSound, "purr");

  saveSettings({
    ...DEFAULT_SETTINGS,
    completionSound: "arbitrary-path" as unknown as typeof DEFAULT_SETTINGS.completionSound,
  });
  assert.equal(loadSettings().completionSound, "chime");
});
