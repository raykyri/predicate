import assert from "node:assert/strict";
import test from "node:test";
import { launcherTabAction } from "../src/lib/launcherKeyboard";

const key = (overrides: Partial<Parameters<typeof launcherTabAction>[0]> = {}) => ({
  key: "Tab",
  metaKey: false,
  ctrlKey: false,
  altKey: false,
  shiftKey: false,
  ...overrides,
});

test("Tab cycles research models and is captured by terminal launchers", () => {
  assert.equal(launcherTabAction(key(), true), "cycle-model");
  assert.equal(launcherTabAction(key(), false), "capture");
});

test("Shift-Tab cycles providers in either launcher", () => {
  assert.equal(launcherTabAction(key({ shiftKey: true }), true), "cycle-provider");
  assert.equal(launcherTabAction(key({ shiftKey: true }), false), "cycle-provider");
});

test("modified Tab chords remain available to app shortcuts", () => {
  assert.equal(launcherTabAction(key({ ctrlKey: true }), true), null);
  assert.equal(launcherTabAction(key({ metaKey: true }), true), null);
  assert.equal(launcherTabAction(key({ altKey: true }), true), null);
  assert.equal(launcherTabAction(key({ key: "Enter" }), true), null);
});
