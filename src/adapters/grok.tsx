import type { AgentUiAdapter, ComposerPolicy } from ".";

export const GROK_ADAPTER_ID = "grok";

// Mirrors the Rust GrokAdapter::composer_policy so the composer enables/queues/
// steers turns identically to the backend.
const grokComposerPolicy: ComposerPolicy = {
  readyStatuses: ["awaitingInput", "done", "idle"],
  queueStatuses: ["starting", "running", "awaitingPermission"],
  steerStatuses: ["starting", "running"],
  permissionActions: [],
};

export const grokUiAdapter: AgentUiAdapter = {
  id: GROK_ADAPTER_ID,
  label: "Grok",
  composerPolicy: () => grokComposerPolicy,
  supportsFork: true,
};
