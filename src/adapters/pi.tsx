import type { AgentUiAdapter } from ".";

export const PI_ADAPTER_ID = "pi";

export const piUiAdapter: AgentUiAdapter = {
  id: PI_ADAPTER_ID,
  label: "Pi",
  supportsFork: true,
  supportsForkAtMessage: true,
  canFork: (agent) => Boolean(agent.transcriptPath && agent.nativeLeafId),
};
