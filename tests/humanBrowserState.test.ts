import assert from "node:assert/strict";
import test from "node:test";
import type { BrowserOverlayState } from "../src/appTypes";
import {
  browserOverlayUsesNativeHumanChild,
  nativeHumanBrowserOwnerIds,
} from "../src/lib/humanBrowserState";

function overlay(overrides: Partial<BrowserOverlayState> = {}): BrowserOverlayState {
  return {
    url: "https://example.com/",
    open: true,
    reloadNonce: 1,
    sandbox: false,
    mode: "webkit",
    size: null,
    ...overrides,
  };
}

test("only an open external WebKit overlay retains a native child", () => {
  assert.equal(browserOverlayUsesNativeHumanChild(overlay()), true);
  assert.equal(browserOverlayUsesNativeHumanChild(overlay({ open: false })), false);
  assert.equal(browserOverlayUsesNativeHumanChild(overlay({ mode: "agent" })), false);
  assert.equal(browserOverlayUsesNativeHumanChild(overlay({ sandbox: true })), false);
  assert.equal(browserOverlayUsesNativeHumanChild(overlay({ url: null })), false);
});

test("owner transitions identify children retired by close and mode changes", () => {
  const owners = nativeHumanBrowserOwnerIds({
    open: overlay(),
    closed: overlay({ open: false }),
    agent: overlay({ mode: "agent" }),
    preview: overlay({ sandbox: true }),
  });
  assert.deepEqual([...owners], ["open"]);
});
