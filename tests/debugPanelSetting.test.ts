import assert from "node:assert/strict";
import test from "node:test";

const store = new Map<string, string>();
(globalThis as { localStorage?: unknown }).localStorage = {
  getItem: (key: string) => store.get(key) ?? null,
  setItem: (key: string, value: string) => store.set(key, value),
  removeItem: (key: string) => store.delete(key),
};

import { DEFAULT_SETTINGS, loadSettings, saveSettings } from "../src/lib/settings";

test("the agent debug panel is opt-in and persists through Display settings", () => {
  store.clear();
  assert.equal(loadSettings().showDebugPanel, false);

  saveSettings({ ...DEFAULT_SETTINGS, showDebugPanel: true });
  assert.equal(loadSettings().showDebugPanel, true);

  saveSettings({ ...DEFAULT_SETTINGS, showDebugPanel: false });
  assert.equal(loadSettings().showDebugPanel, false);
});

test("invalid stored debug visibility falls back to hidden", () => {
  store.clear();
  saveSettings({ ...DEFAULT_SETTINGS, showDebugPanel: "yes" as unknown as boolean });
  assert.equal(loadSettings().showDebugPanel, false);
});
