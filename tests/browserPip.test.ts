import assert from "node:assert/strict";
import test from "node:test";
import type { BrowserAutomationTarget } from "../src/lib/api";
import {
  BROWSER_PIP_EDGE_INSET,
  browserPipPageLabel,
  browserPipRightInset,
  selectBrowserPipTargets,
} from "../src/lib/browserPip";

function target(
  paneId: string,
  tabId: number,
  title: string | null = null,
  url: string | null = null,
): BrowserAutomationTarget {
  return { paneId, tabId, title, url };
}

test("browser PiPs follow pane order and omit the expanded agent browser", () => {
  const selection = selectBrowserPipTargets(
    [target("pane-c", 3), target("pane-a", 1), target("pane-b", 2)],
    ["pane-a", "pane-b", "pane-c"],
    "pane-b",
  );
  assert.deepEqual(
    selection.visible.map((entry) => entry.paneId),
    ["pane-a", "pane-c"],
  );
  assert.equal(selection.overflow, 0);
});

test("browser PiPs ignore missing panes, deduplicate, and report overflow", () => {
  const selection = selectBrowserPipTargets(
    [
      target("pane-a", 1),
      target("pane-a", 7),
      target("missing", 9),
      target("pane-b", 2),
      target("pane-c", 3),
      target("pane-d", 4),
    ],
    ["pane-a", "pane-b", "pane-c", "pane-d"],
    null,
    3,
  );
  assert.deepEqual(
    selection.visible.map((entry) => entry.tabId),
    [1, 2, 3],
  );
  assert.equal(selection.overflow, 1);
});

test("browser PiP labels prefer page title, then host", () => {
  assert.equal(browserPipPageLabel(target("pane", 1, "Dashboard", "https://example.com")), "Dashboard");
  assert.equal(browserPipPageLabel(target("pane", 1, null, "https://example.com/path")), "example.com");
  assert.equal(browserPipPageLabel(target("pane", 1, null, "about:blank")), "Agent browser");
});

test("browser PiP rail clears a docked right pane and otherwise stays at the app edge", () => {
  assert.equal(browserPipRightInset(null), BROWSER_PIP_EDGE_INSET);
  assert.equal(browserPipRightInset(420), 432);
  assert.equal(browserPipRightInset(0), BROWSER_PIP_EDGE_INSET);
  assert.equal(browserPipRightInset(Number.NaN), BROWSER_PIP_EDGE_INSET);
});
