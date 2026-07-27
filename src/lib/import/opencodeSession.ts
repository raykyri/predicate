// Parses an assembled OpenCode session — the backend's
// `{"session": …, "messages": [{…, "parts": […]}, …]}` payload from
// read_opencode_session — into trajectory records.
//
// OpenCode stores prose, reasoning, and tool activity as typed parts per
// message. Text parts join into one prose record per message; tool parts
// surface as payload-free tool_calls (the import pipeline strips arguments
// anyway); reasoning and step/compaction bookkeeping parts are dropped.

import type {
  ParsedConversation,
  TrajectoryRecord,
  TrajectoryToolCall,
} from "./types";

interface OpencodePart {
  id?: unknown;
  type?: unknown;
  text?: unknown;
  tool?: unknown;
  callID?: unknown;
}

interface OpencodeMessage {
  id?: unknown;
  role?: unknown;
  time?: unknown;
  parts?: unknown;
}

export function parseOpencodeSession(json: string): ParsedConversation {
  const payload = JSON.parse(json) as Record<string, unknown>;
  const session = (payload.session ?? {}) as Record<string, unknown>;
  const warnings: string[] = [];

  const title =
    typeof session.title === "string" && session.title.trim() !== ""
      ? session.title
      : null;
  const createdAt = timeCreatedMs(session.time);

  const rawMessages = Array.isArray(payload.messages)
    ? (payload.messages as OpencodeMessage[])
    : [];
  // The backend already orders messages; sort defensively — stable, so
  // missing/equal timestamps keep payload order.
  const messages = rawMessages
    .map((message, index) => ({ message, index, at: timeCreatedMs(message.time) }))
    .sort((a, b) => (a.at ?? 0) - (b.at ?? 0) || a.index - b.index);

  const records: TrajectoryRecord[] = [];
  for (const { message, index, at } of messages) {
    const role =
      message.role === "user" ? "user" : message.role === "assistant" ? "assistant" : null;
    if (role === null) {
      continue;
    }
    const timestamp = new Date(at ?? createdAt ?? 0).toISOString();
    const texts: string[] = [];
    const toolCalls: TrajectoryToolCall[] = [];
    const parts = Array.isArray(message.parts) ? (message.parts as OpencodePart[]) : [];
    parts.forEach((part, partIndex) => {
      if (part?.type === "text" && typeof part.text === "string" && part.text.trim() !== "") {
        texts.push(part.text);
      } else if (part?.type === "tool" && typeof part.tool === "string" && part.tool !== "") {
        // Arguments are intentionally empty: the backend sanitizer strips
        // payloads anyway, so only the call structure crosses over.
        toolCalls.push({
          id:
            typeof part.callID === "string" && part.callID !== ""
              ? part.callID
              : typeof part.id === "string" && part.id !== ""
                ? part.id
                : `opencode-call-${index}-${partIndex}`,
          name: part.tool,
          args: "{}",
        });
      }
      // reasoning, step-start/step-finish, compaction, and nameless tool
      // parts are dropped.
    });
    if (texts.length > 0) {
      records.push({ role, content: texts.join("\n\n"), timestamp });
    }
    if (toolCalls.length > 0 && role === "assistant") {
      // content is null exactly when tool_calls is present, per the
      // trajectory schema constraint.
      records.push({ role, content: null, timestamp, tool_calls: toolCalls });
    }
  }
  if (records.length === 0 && rawMessages.length > 0) {
    warnings.push("no usable messages in this session");
  }
  return { title, createdAt, records, warnings };
}

/** Epoch ms from an OpenCode `time` object's `created` field, or null.
 * OpenCode stores millisecond epochs (verified against real stores). */
function timeCreatedMs(time: unknown): number | null {
  if (typeof time !== "object" || time === null) {
    return null;
  }
  const created = (time as Record<string, unknown>).created;
  return typeof created === "number" && Number.isFinite(created) && created > 0
    ? created
    : null;
}
