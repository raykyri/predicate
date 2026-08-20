import claudeModelIconUrl from "../assets/model-icons/claude-ai.svg";
import cursorModelIconUrl from "../assets/model-icons/cursor.svg";
import devinModelIconUrl from "../assets/model-icons/devin.svg";
import metaModelIconUrl from "../assets/model-icons/meta.svg";
import openAiModelIconUrl from "../assets/model-icons/openai.svg";
import openCodeModelIconUrl from "../assets/model-icons/opencode-dark.svg";
import grokModelIconUrl from "../assets/model-icons/grok.svg";
import piModelIconUrl from "../assets/model-icons/pi.svg";
import { CLAUDE_ADAPTER_ID } from "../adapters/claude";
import { CODEX_ADAPTER_ID } from "../adapters/codex";
import { CURSOR_ADAPTER_ID } from "../adapters/cursor";
import { DEVIN_ADAPTER_ID } from "../adapters/devin";
import { GROK_ADAPTER_ID } from "../adapters/grok";
import { MUSE_ADAPTER_ID } from "../adapters/muse";
import { OPENCODE_ADAPTER_ID } from "../adapters/opencode";
import { PI_ADAPTER_ID } from "../adapters/pi";

/* Adapter icons for LauncherSelect chips — shared by the Home launcher and the
   new-research composer so every agent picker renders the same marks. */
export const ADAPTER_ICON_BY_ID: Record<string, string> = {
  [CLAUDE_ADAPTER_ID]: claudeModelIconUrl,
  [CODEX_ADAPTER_ID]: openAiModelIconUrl,
  [OPENCODE_ADAPTER_ID]: openCodeModelIconUrl,
  [GROK_ADAPTER_ID]: grokModelIconUrl,
  [MUSE_ADAPTER_ID]: metaModelIconUrl,
  [PI_ADAPTER_ID]: piModelIconUrl,
  [CURSOR_ADAPTER_ID]: cursorModelIconUrl,
  [DEVIN_ADAPTER_ID]: devinModelIconUrl,
};

// Codex's mark is dark-on-transparent, so invert it for the launcher's dark surface.
export function adapterIconClassName(adapterId: string): string | undefined {
  if (adapterId === CODEX_ADAPTER_ID) {
    return "is-mono-light";
  }
  return undefined;
}
