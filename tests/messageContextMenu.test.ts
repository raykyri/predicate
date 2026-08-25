import assert from "node:assert/strict";
import test from "node:test";
import { transcriptMessageContextMenuPoint } from "../src/lib/messageContextMenu";

test("eligible transcript messages open their menu at the right-click point", () => {
  assert.deepEqual(
    transcriptMessageContextMenuPoint({
      hasMessageMenu: true,
      defaultPrevented: false,
      clientX: 320,
      clientY: 240,
    }),
    { clientX: 320, clientY: 240 },
  );
});

test("messages without actions retain the browser context menu", () => {
  assert.equal(
    transcriptMessageContextMenuPoint({
      hasMessageMenu: false,
      defaultPrevented: false,
      clientX: 320,
      clientY: 240,
    }),
    null,
  );
});

test("nested transcript controls keep their specialized context menus", () => {
  assert.equal(
    transcriptMessageContextMenuPoint({
      hasMessageMenu: true,
      defaultPrevented: true,
      clientX: 320,
      clientY: 240,
    }),
    null,
  );
});
