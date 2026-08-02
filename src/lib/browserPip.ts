import type { BrowserAutomationTarget } from "./api";

export const BROWSER_PIP_MAX_VISIBLE = 3;
export const BROWSER_PIP_EDGE_INSET = 12;

export type BrowserPipSelection = {
  visible: BrowserAutomationTarget[];
  overflow: number;
};

/**
 * Keeps the global rail inside the terminal/workspace column when a docked
 * right pane is visible. A fully expanded transcript is not docked: it owns
 * the stage, so the rail retains its normal app-edge placement above it.
 */
export function browserPipRightInset(dockedRightPaneWidth: number | null): number {
  if (
    dockedRightPaneWidth === null ||
    !Number.isFinite(dockedRightPaneWidth) ||
    dockedRightPaneWidth <= 0
  ) {
    return BROWSER_PIP_EDGE_INSET;
  }
  return BROWSER_PIP_EDGE_INSET + dockedRightPaneWidth;
}

/**
 * Keeps one selected target per live pane, ordered like the qmux tab list.
 * The browser already open in the full Agent overlay is omitted because the
 * full mirror is the better view of the same target.
 */
export function selectBrowserPipTargets(
  targets: BrowserAutomationTarget[],
  paneOrder: string[],
  expandedAgentPaneId: string | null,
  maxVisible = BROWSER_PIP_MAX_VISIBLE,
): BrowserPipSelection {
  const order = new Map(paneOrder.map((paneId, index) => [paneId, index]));
  const selectedByPane = new Map<string, BrowserAutomationTarget>();
  for (const target of targets) {
    if (
      target.paneId === expandedAgentPaneId ||
      !order.has(target.paneId) ||
      selectedByPane.has(target.paneId)
    ) {
      continue;
    }
    selectedByPane.set(target.paneId, target);
  }
  const selected = [...selectedByPane.values()].sort(
    (left, right) =>
      (order.get(left.paneId) ?? Number.MAX_SAFE_INTEGER) -
      (order.get(right.paneId) ?? Number.MAX_SAFE_INTEGER),
  );
  const limit = Math.max(0, Math.floor(maxVisible));
  return {
    visible: selected.slice(0, limit),
    overflow: Math.max(0, selected.length - limit),
  };
}

export function browserPipPageLabel(target: BrowserAutomationTarget): string {
  const title = target.title?.trim();
  if (title) {
    return title;
  }
  if (target.url) {
    try {
      const parsed = new URL(target.url);
      if (parsed.hostname) {
        return parsed.hostname;
      }
    } catch {
      // Fall through to the stable generic label.
    }
  }
  return "Agent browser";
}
