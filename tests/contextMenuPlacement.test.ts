import test from "node:test";
import assert from "node:assert/strict";
import {
  CONTEXT_MENU_VIEWPORT_MARGIN as MARGIN,
  clampContextMenuToViewport,
} from "../src/lib/appHelpers";

test("context menu stays at the pointer when it already fits", () => {
  assert.deepEqual(
    clampContextMenuToViewport({
      x: 40,
      y: 80,
      width: 320,
      height: 240,
      viewportWidth: 1000,
      viewportHeight: 800,
    }),
    { x: 40, y: 80 },
  );
});

test("context menu shifts up when it would run off the bottom", () => {
  const placed = clampContextMenuToViewport({
    x: 40,
    y: 720,
    width: 320,
    height: 400,
    viewportWidth: 1000,
    viewportHeight: 800,
  });
  assert.equal(placed.x, 40);
  assert.equal(placed.y, 800 - MARGIN - 400);
  assert.ok(placed.y + 400 <= 800 - MARGIN);
});

test("context menu pins to the top margin when taller than the window", () => {
  const placed = clampContextMenuToViewport({
    x: 40,
    y: 500,
    width: 320,
    height: 900,
    viewportWidth: 1000,
    viewportHeight: 800,
  });
  assert.equal(placed.y, MARGIN);
});

test("context menu shifts left when it would run off the right edge", () => {
  const placed = clampContextMenuToViewport({
    x: 900,
    y: 40,
    width: 320,
    height: 200,
    viewportWidth: 1000,
    viewportHeight: 800,
  });
  assert.equal(placed.x, 1000 - MARGIN - 320);
  assert.equal(placed.y, 40);
});
