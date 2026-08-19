import type { AgentInfo } from "../types";

export interface TerminalAttentionState {
  activeSurface: "pane" | "research";
  activePaneId: string | null;
  paneId: string | null;
  paneExists: boolean;
  /**
   * True when the qmux window is the focused app surface. This is not the same
   * as `document.hasFocus()`: a native Ghostty terminal can be first responder
   * while the webview document is blurred, and keyboard tab switches happen in
   * exactly that state.
   */
  appFocused: boolean;
  documentVisible: boolean;
}

export function terminalPaneHasUserAttention(state: TerminalAttentionState): boolean {
  return (
    state.activeSurface === "pane" &&
    state.paneId !== null &&
    state.activePaneId === state.paneId &&
    state.paneExists &&
    state.appFocused &&
    state.documentVisible
  );
}

/**
 * Intentional activation (tab click, keyboard cycle, menu-bar select) means the
 * user chose this pane. The active-pane match is still required so a stale
 * callback cannot clear a different pane, but webview document focus
 * is not — native terminals own first responder during keyboard navigation.
 */
export function terminalPaneWasIntentionallyActivated(
  state: Pick<
    TerminalAttentionState,
    "activeSurface" | "activePaneId" | "paneId" | "paneExists" | "documentVisible"
  >,
): boolean {
  return (
    state.activeSurface === "pane" &&
    state.paneId !== null &&
    state.activePaneId === state.paneId &&
    state.paneExists &&
    state.documentVisible
  );
}

// A speculative acknowledgement asks the backend to atomically re-check an
// agent whose Done event may still be in flight. Only apply an Idle response:
// a non-Idle snapshot can predate a newer status event that React has already
// received, and applying it would roll that newer state back.
export function applicableSpeculativeAcknowledgements(agents: AgentInfo[]): AgentInfo[] {
  return agents.filter((agent) => agent.status === "idle");
}

export const TERMINAL_ATTENTION_PROBE_INTERVAL_MS = 250;

export function terminalAttentionProbeIsDue(
  lastProbeAt: number | undefined,
  now: number,
): boolean {
  return (
    lastProbeAt === undefined ||
    now < lastProbeAt ||
    now - lastProbeAt >= TERMINAL_ATTENTION_PROBE_INTERVAL_MS
  );
}
