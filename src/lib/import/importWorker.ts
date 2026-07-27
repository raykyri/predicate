// Module Worker that turns source payloads into import drafts off the main
// thread — a multi-year ChatGPT conversation can be megabytes of JSON, and
// parsing a selection of them on the UI thread is exactly the kind of stall
// the freeze diagnostics exist to catch. Everything imported here is pure
// (no Tauri, no DOM), so the worker bundles cleanly.

import { parseChatgptConversation } from "./chatgptExport";
import { parseClaudeAiConversation } from "./claudeAiExport";
import { parseHarnessTranscript, type HarnessTranscriptFormat } from "./harnessTranscript";
import { parseOpencodeSession } from "./opencodeSession";
import { trajectoryRecordsToTurns } from "./trajectoryToTurns";
import type { ImportedConversationDraft, ParsedConversation } from "./types";

export type ImportWorkerFormat =
  | "claudeAi"
  | "chatgpt"
  | "opencode"
  | HarnessTranscriptFormat;

export interface ImportWorkerRequest {
  requestId: number;
  format: ImportWorkerFormat;
  /** Raw payloads: staged conversation JSON slices, or a transcript's text. */
  payloads: string[];
}

export interface ImportWorkerResponse {
  requestId: number;
  drafts: ImportedConversationDraft[];
  /** One entry per payload that failed to parse outright. */
  errors: string[];
}

export function draftsFromPayloads(
  format: ImportWorkerFormat,
  payloads: string[],
): { drafts: ImportedConversationDraft[]; errors: string[] } {
  const drafts: ImportedConversationDraft[] = [];
  const errors: string[] = [];
  payloads.forEach((payload, index) => {
    try {
      const parsed: ParsedConversation =
        format === "claudeAi"
          ? parseClaudeAiConversation(payload)
          : format === "chatgpt"
            ? parseChatgptConversation(payload)
            : format === "opencode"
              ? parseOpencodeSession(payload)
              : parseHarnessTranscript(format, payload);
      drafts.push({
        title: parsed.title,
        createdAt: parsed.createdAt,
        turns: trajectoryRecordsToTurns(parsed.records, `import-${index}`),
        warnings: parsed.warnings,
      });
    } catch (error) {
      errors.push(error instanceof Error ? error.message : String(error));
    }
  });
  return { drafts, errors };
}

// The worker entry point. Guarded so importing this module from tests (for
// draftsFromPayloads) does not require a worker global.
if (typeof self !== "undefined" && typeof (self as unknown as Worker).postMessage === "function") {
  self.onmessage = (event: MessageEvent<ImportWorkerRequest>) => {
    const { requestId, format, payloads } = event.data;
    const { drafts, errors } = draftsFromPayloads(format, payloads);
    const response: ImportWorkerResponse = { requestId, drafts, errors };
    (self as unknown as Worker).postMessage(response);
  };
}
