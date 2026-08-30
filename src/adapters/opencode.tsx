import type { AgentUiAdapter } from ".";

export const OPENCODE_ADAPTER_ID = "opencode";

export const opencodeUiAdapter: AgentUiAdapter = {
  id: OPENCODE_ADAPTER_ID,
  label: "OpenCode",
  supportsFork: true,
};
