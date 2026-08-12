import type { AgentUiAdapter, ComposerPolicy } from ".";

export const PI_ADAPTER_ID = "pi";

// Mirrors PiAdapter::composer_policy. Pi owns authentication, model/thinking
// selection, trust, and extension UI inside its native TUI, so the qmux launcher
// intentionally exposes no adapter options.
const piComposerPolicy: ComposerPolicy = {
  readyStatuses: ["awaitingInput", "done", "idle"],
  queueStatuses: ["starting", "running", "awaitingPermission"],
  steerStatuses: ["starting", "running"],
  permissionActions: [],
};

export const piUiAdapter: AgentUiAdapter = {
  id: PI_ADAPTER_ID,
  label: "Pi",
  composerPolicy: () => piComposerPolicy,
  supportsFork: true,
  supportsForkAtMessage: true,
  canFork: (agent) => Boolean(agent.transcriptPath && agent.nativeLeafId),
};
