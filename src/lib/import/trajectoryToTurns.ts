// Converts trajectory records — the common intermediate for every import
// source — into qmux Turns for import_research_conversations.
//
// The backend re-runs the conversation sanitizer on whatever arrives, so this
// conversion is a payload optimization, not a trust boundary: meta, reasoning
// and tool-result records are dropped (the sanitizer would discard them
// anyway) and tool-call arguments are emptied (only the call count survives
// as an activity marker), which keeps IPC proportional to visible text.

import type { Turn, TurnBlock } from "../../types";
import type { TrajectoryRecord } from "./types";

export function trajectoryRecordsToTurns(records: TrajectoryRecord[], agentId: string): Turn[] {
  const turns: Turn[] = [];
  for (const record of records) {
    if (record.role !== "user" && record.role !== "assistant") {
      continue;
    }
    const blocks: TurnBlock[] = [];
    if (record.role === "assistant" && record.tool_calls !== undefined) {
      for (const call of record.tool_calls) {
        blocks.push({ type: "toolUse", id: call.id, name: call.name, input: {} });
      }
    }
    if (typeof record.content === "string" && record.content !== "") {
      blocks.push({ type: "text", text: record.content });
    }
    if (blocks.length === 0) {
      continue;
    }
    const timestamp = Date.parse(record.timestamp);
    turns.push({
      id: `${agentId}-src-${turns.length}`,
      agentId,
      role: record.role,
      blocks,
      sourceIndex: turns.length,
      timestamp: Number.isFinite(timestamp) && timestamp > 0 ? timestamp : null,
    });
  }
  return turns;
}
