import assert from "node:assert/strict";
import test from "node:test";
import type { BrowserOverlayState } from "../src/appTypes";
import {
  anyBrowserOverlayOpen,
  browserOverlayIsOpen,
  closeAllBrowserOverlaysState,
  resolveTranscriptOrBrowserToggle,
} from "../src/lib/browserOverlay";

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

test("anyBrowserOverlayOpen looks at every owner, not just the active tab", () => {
  assert.equal(anyBrowserOverlayOpen({}), false);
  assert.equal(anyBrowserOverlayOpen({ a: overlay({ open: false }) }), false);
  assert.equal(
    anyBrowserOverlayOpen({
      a: overlay({ open: false }),
      b: overlay(),
    }),
    true,
  );
  assert.equal(browserOverlayIsOpen(overlay()), true);
  assert.equal(browserOverlayIsOpen(overlay({ open: false })), false);
});

test("closeAllBrowserOverlaysState closes every owner and preserves identity when already closed", () => {
  const alreadyClosed = { a: overlay({ open: false }) };
  assert.equal(closeAllBrowserOverlaysState(alreadyClosed), alreadyClosed);

  const closed = closeAllBrowserOverlaysState({
    a: overlay(),
    b: overlay({ open: false, url: "https://kept.example/" }),
  });
  assert.equal(closed.a.open, false);
  assert.equal(closed.b.open, false);
  assert.equal(closed.b.url, "https://kept.example/");
});

test("⌘⇧E closes a live browser instead of expanding the transcript", () => {
  assert.deepEqual(
    resolveTranscriptOrBrowserToggle({
      anyBrowserOpen: true,
      canToggleTranscript: true,
    }),
    { type: "close-browser" },
  );
  assert.deepEqual(
    resolveTranscriptOrBrowserToggle({
      anyBrowserOpen: false,
      canToggleTranscript: true,
    }),
    { type: "toggle-transcript" },
  );
  assert.deepEqual(
    resolveTranscriptOrBrowserToggle({
      anyBrowserOpen: false,
      canToggleTranscript: false,
    }),
    { type: "toggle-browser" },
  );
});
