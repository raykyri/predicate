// @letta-ai/trajectory's public entry also exports filesystem session listing,
// Deep Agents subprocess loading, and canonical hashing. Those eager exports
// pull Node built-ins into Vite's browser worker even though conversation
// imports only use transcript normalization. Assemble that browser-safe subset
// from the package's adapters until it provides a dedicated browser export.
import { claudeCodeAdapter } from "../../../node_modules/@letta-ai/trajectory/dist/adapters/claude-code/index.js";
import { codexAdapter } from "../../../node_modules/@letta-ai/trajectory/dist/adapters/codex/index.js";
import { hermesAdapter } from "../../../node_modules/@letta-ai/trajectory/dist/adapters/hermes/index.js";
import { lettaCodeAdapter } from "../../../node_modules/@letta-ai/trajectory/dist/adapters/letta-code/index.js";
import { openClawAdapter } from "../../../node_modules/@letta-ai/trajectory/dist/adapters/openclaw/index.js";
import { openHandsAdapter } from "../../../node_modules/@letta-ai/trajectory/dist/adapters/openhands/index.js";
import { piAdapter } from "../../../node_modules/@letta-ai/trajectory/dist/adapters/pi/index.js";
import { resolveBounds } from "../../../node_modules/@letta-ai/trajectory/dist/bounds.js";
import { normalizeDecodedSession } from "../../../node_modules/@letta-ai/trajectory/dist/core.js";
import { resolveFilters } from "../../../node_modules/@letta-ai/trajectory/dist/filters.js";
import { NormalizationError } from "../../../node_modules/@letta-ai/trajectory/dist/types.js";
import type {
  NormalizeInput,
  NormalizeResult,
} from "@letta-ai/trajectory";

const ADAPTERS = {
  "claude-code": claudeCodeAdapter,
  codex: codexAdapter,
  hermes: hermesAdapter,
  "letta-code": lettaCodeAdapter,
  openclaw: openClawAdapter,
  openhands: openHandsAdapter,
  pi: piAdapter,
};

export function normalizeTranscript(input: NormalizeInput): NormalizeResult {
  if (!input || typeof input !== "object") {
    throw new NormalizationError("invalid_input", "Input must be an object.");
  }
  if (typeof input.transcript !== "string") {
    throw new NormalizationError(
      "invalid_input",
      "Input transcript must be a string containing the source transcript.",
    );
  }

  const adapter = ADAPTERS[input.source];
  if (!adapter) {
    throw new NormalizationError(
      "unknown_source",
      `Unknown trajectory source ${JSON.stringify(input.source)}. Supported sources: ${Object.keys(ADAPTERS).join(", ")}.`,
    );
  }

  return normalizeDecodedSession(
    adapter.decode(input.transcript),
    resolveBounds(input.bounds),
    {
      partial:
        (input.sourceContext?.partial ?? false) ||
        (input.sourceContext?.baseByteOffset ?? 0) > 0,
      filters: resolveFilters(input.filters),
    },
  );
}
