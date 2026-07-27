// Parses one conversation object from a claude.ai data export's
// conversations.json into trajectory records.
//
// The export schema is unversioned, so parsing is lenient: messages carry
// text either as a `content` block array (current) or a bare `text` field
// (legacy); unknown block types are skipped; attachments become marker lines
// so the exchange structure survives without the payload — matching the
// backend sanitizer's philosophy.

import type { ParsedConversation, TrajectoryRecord } from "./types";

interface ClaudeAiMessage {
  sender?: unknown;
  text?: unknown;
  content?: unknown;
  created_at?: unknown;
  attachments?: unknown;
  files?: unknown;
}

export function parseClaudeAiConversation(json: string): ParsedConversation {
  const conversation = JSON.parse(json) as Record<string, unknown>;
  const warnings: string[] = [];
  const title = typeof conversation.name === "string" && conversation.name.trim() !== ""
    ? conversation.name
    : null;
  const createdAt = parseIsoMs(conversation.created_at);

  const rawMessages = Array.isArray(conversation.chat_messages)
    ? (conversation.chat_messages as ClaudeAiMessage[])
    : [];
  // Exports list messages in creation order, but sort defensively — a stable
  // sort keeps equal/missing timestamps in file order.
  const messages = rawMessages
    .map((message, index) => ({ message, index, at: parseIsoMs(message.created_at) }))
    .sort((a, b) => (a.at ?? 0) - (b.at ?? 0) || a.index - b.index);

  const records: TrajectoryRecord[] = [];
  for (const { message, at } of messages) {
    const role =
      message.sender === "human" ? "user" : message.sender === "assistant" ? "assistant" : null;
    if (role === null) {
      continue;
    }
    const lines: string[] = [];
    const text = claudeAiMessageText(message);
    if (text !== "") {
      lines.push(text);
    }
    for (const name of attachmentNames(message)) {
      lines.push(`[attachment omitted: ${name}]`);
    }
    if (lines.length === 0) {
      continue;
    }
    records.push({
      role,
      content: lines.join("\n\n"),
      timestamp: new Date(at ?? createdAt ?? 0).toISOString(),
    });
  }
  if (records.length === 0 && rawMessages.length > 0) {
    warnings.push("no usable messages in this conversation");
  }
  return { title, createdAt, records, warnings };
}

function claudeAiMessageText(message: ClaudeAiMessage): string {
  if (Array.isArray(message.content)) {
    const parts: string[] = [];
    for (const block of message.content as Array<Record<string, unknown>>) {
      if (block && block.type === "text" && typeof block.text === "string") {
        parts.push(block.text);
      }
    }
    const joined = parts.join("\n\n").trim();
    if (joined !== "") {
      return joined;
    }
  }
  // Legacy exports carry the whole message as a bare text field.
  return typeof message.text === "string" ? message.text.trim() : "";
}

function attachmentNames(message: ClaudeAiMessage): string[] {
  const names: string[] = [];
  for (const list of [message.attachments, message.files]) {
    if (!Array.isArray(list)) {
      continue;
    }
    for (const entry of list as Array<Record<string, unknown>>) {
      const name = entry?.file_name ?? entry?.name;
      names.push(typeof name === "string" && name !== "" ? name : "unnamed file");
    }
  }
  return names;
}

/** Epoch ms from an ISO-8601 string field, or null. */
export function parseIsoMs(value: unknown): number | null {
  if (typeof value !== "string" || value === "") {
    return null;
  }
  const ms = Date.parse(value);
  return Number.isFinite(ms) ? ms : null;
}
