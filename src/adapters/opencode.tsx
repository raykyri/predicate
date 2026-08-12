import type { AgentUiAdapter, ComposerPolicy } from ".";

export const OPENCODE_ADAPTER_ID = "opencode";

const opencodeComposerPolicy: ComposerPolicy = {
  readyStatuses: ["awaitingInput", "done", "idle"],
  queueStatuses: ["starting", "running", "awaitingPermission"],
  steerStatuses: ["starting", "running"],
  permissionActions: [],
};

export const opencodeUiAdapter: AgentUiAdapter = {
  id: OPENCODE_ADAPTER_ID,
  label: "OpenCode",
  composerPolicy: () => opencodeComposerPolicy,
  supportsFork: true,
};
