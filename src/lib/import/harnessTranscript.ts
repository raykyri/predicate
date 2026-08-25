// Native harness transcripts (Claude Code, Codex, and the other agent CLIs)
// normalize through @letta-ai/trajectory's adapters (thinking/text/tool_use/
// tool_result blocks, sidechain and noise-row handling), rather than
// hand-written parsers. Adapter diagnostics surface as import warnings.

import type { TrajectorySource } from "@letta-ai/trajectory";
import { parseIsoMs } from "./claudeAiExport";
import { normalizeTranscript } from "./trajectoryBrowser";
import type { ParsedConversation, TrajectoryRecord } from "./types";

/** The transcript-file formats the import worker accepts, i.e. every source
 * that arrives as a JSONL transcript rather than an export archive. */
export type HarnessTranscriptFormat =
  | "claudeCode"
  | "codex"
  | "hermes"
  | "lettaCode"
  | "openclaw"
  | "openhands"
  | "pi";

/** Frontend format ids → the trajectory library's adapter source ids. */
const TRAJECTORY_SOURCES: Record<HarnessTranscriptFormat, TrajectorySource> = {
  claudeCode: "claude-code",
  codex: "codex",
  hermes: "hermes",
  lettaCode: "letta-code",
  openclaw: "openclaw",
  openhands: "openhands",
  pi: "pi",
};

export function parseHarnessTranscript(
  format: HarnessTranscriptFormat,
  transcript: string,
): ParsedConversation {
  const { records, diagnostics } = normalizeTranscript({
    source: TRAJECTORY_SOURCES[format],
    transcript,
  }) as { records: TrajectoryRecord[]; diagnostics: unknown[] };

  const warnings = diagnostics.map((diagnostic) =>
    typeof diagnostic === "string" ? diagnostic : JSON.stringify(diagnostic),
  );
  // Sessions have no stored title; the backend derives one from the first
  // prompt. The first timed record dates the conversation.
  let createdAt: number | null = null;
  for (const record of records) {
    if (record.role !== "meta") {
      createdAt = parseIsoMs(record.timestamp);
      break;
    }
  }
  return { title: null, createdAt, records, warnings };
}
