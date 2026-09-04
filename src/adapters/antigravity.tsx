import type { AgentUiAdapter, ComposerPolicy } from ".";

export const ANTIGRAVITY_ADAPTER_ID = "antigravity";

const antigravityComposerPolicy: ComposerPolicy = {
  readyStatuses: ["awaitingInput", "done", "idle"],
  queueStatuses: ["starting", "running", "awaitingPermission"],
  steerStatuses: ["starting", "running"],
  permissionActions: [],
};

export const antigravityUiAdapter: AgentUiAdapter = {
  id: ANTIGRAVITY_ADAPTER_ID,
  label: "Antigravity",
  composerPolicy: () => antigravityComposerPolicy,
  supportsFork: false,
};
