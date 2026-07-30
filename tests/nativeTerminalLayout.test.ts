import assert from "node:assert/strict";
import test from "node:test";

import {
  nativeTerminalLayoutRect,
  type NativeTerminalRect,
} from "../src/lib/nativeTerminalLayout";

const rect = (
  left: number,
  top: number,
  width: number,
  height: number,
): NativeTerminalRect => ({ left, top, width, height });

test("visible terminals use and remember their measured layout", () => {
  const measured = rect(100, 50, 700, 600);

  assert.equal(nativeTerminalLayoutRect(measured, true, null), measured);
});

test("a hidden terminal keeps its last visible frame across right-pane changes", () => {
  const withRightPane = rect(100, 50, 700, 600);
  const withoutRightPane = rect(100, 50, 1100, 600);

  assert.equal(
    nativeTerminalLayoutRect(withoutRightPane, false, withRightPane),
    withRightPane,
  );
});

test("a full-width terminal keeps the same frame through a tab round trip", () => {
  const fullWidth = rect(100, 50, 1100, 600);
  const otherTab = rect(100, 50, 700, 600);
  const parked = nativeTerminalLayoutRect(otherTab, false, fullWidth);
  const restored = nativeTerminalLayoutRect(fullWidth, true, parked);

  assert.equal(parked, fullWidth);
  assert.equal(restored, fullWidth);
});

test("a never-visible terminal may adopt its initial hidden frame", () => {
  const initial = rect(100, 50, 1100, 600);

  assert.equal(nativeTerminalLayoutRect(initial, false, null), initial);
});
