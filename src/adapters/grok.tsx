import type { AgentUiAdapter } from ".";

export const GROK_ADAPTER_ID = "grok";

export const grokUiAdapter: AgentUiAdapter = {
  id: GROK_ADAPTER_ID,
  label: "Grok",
  supportsFork: true,
};
