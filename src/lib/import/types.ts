// Shared types for the conversation-import parsing layer.
//
// All three sources (claude.ai exports, ChatGPT exports, Claude Code
// transcripts) normalize into trajectory-v1 records — the format of
// @letta-ai/trajectory, which handles Claude Code natively; the two export
// parsers are hand-written against the same record shape so one converter
// (trajectoryToTurns) carries every source into qmux Turns.

import type { Turn } from "../../types";

export interface TrajectoryMetaRecord {
  role: "meta";
  source: string;
  cwd?: string;
  git_branch?: string;
  model?: string;
}

export interface TrajectoryUserRecord {
  role: "user";
  content: string;
  timestamp: string;
}

export interface TrajectoryReasoningRecord {
  role: "reasoning";
  content: string;
  timestamp: string;
}

export interface TrajectoryToolCall {
  id: string;
  name: string;
  /** Stringified JSON arguments, per the trajectory schema. */
  args: string;
}

export interface TrajectoryAssistantRecord {
  role: "assistant";
  /** Null exactly when tool_calls is present, per the schema constraint. */
  content: string | null;
  timestamp: string;
  tool_calls?: TrajectoryToolCall[];
}

export interface TrajectoryToolRecord {
  role: "tool";
  tool_call_id: string;
  content: string;
  timestamp: string;
}

export type TrajectoryRecord =
  | TrajectoryMetaRecord
  | TrajectoryUserRecord
  | TrajectoryReasoningRecord
  | TrajectoryAssistantRecord
  | TrajectoryToolRecord;

/** One source conversation normalized to trajectory records. */
export interface ParsedConversation {
  title: string | null;
  /** Epoch ms of the source conversation's creation, when known. */
  createdAt: number | null;
  records: TrajectoryRecord[];
  /** Non-fatal oddities encountered while parsing, for the import summary. */
  warnings: string[];
}

/** A conversation converted to qmux turns, ready for the import command. */
export interface ImportedConversationDraft {
  title: string | null;
  createdAt: number | null;
  turns: Turn[];
  warnings: string[];
}
