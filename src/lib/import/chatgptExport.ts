// Parses one conversation object from a ChatGPT data export's
// conversations.json into trajectory records.
//
// A ChatGPT conversation is a `mapping` tree of nodes (edits and regenerated
// answers fork branches). Only the canonical thread is imported — the walk
// follows `parent` links from `current_node` to the root, matching what the
// ChatGPT UI shows — because an import is a point-in-time copy, the same
// semantics conversation exports already have. Abandoned branches are
// dropped; a dangling or missing `current_node` falls back to the newest
// message-bearing node.

import type { ParsedConversation, TrajectoryRecord } from "./types";

interface ChatgptNode {
  id?: unknown;
  parent?: unknown;
  message?: {
    author?: { role?: unknown };
    content?: { content_type?: unknown; parts?: unknown };
    create_time?: unknown;
    metadata?: { is_visually_hidden_from_conversation?: unknown };
  } | null;
}

export function parseChatgptConversation(json: string): ParsedConversation {
  const conversation = JSON.parse(json) as Record<string, unknown>;
  const warnings: string[] = [];
  const title = typeof conversation.title === "string" && conversation.title.trim() !== ""
    ? conversation.title
    : null;
  const createdAt = parseEpochSecondsMs(conversation.create_time);

  const mapping =
    conversation.mapping && typeof conversation.mapping === "object"
      ? (conversation.mapping as Record<string, ChatgptNode>)
      : {};

  let leafId = typeof conversation.current_node === "string" ? conversation.current_node : null;
  if (leafId === null || !(leafId in mapping)) {
    if (leafId !== null) {
      warnings.push("current_node is missing from the mapping; using the newest message instead");
    }
    leafId = newestMessageNode(mapping);
  }

  // Walk leaf → root via parent links; a cycle or dangling parent ends the
  // walk rather than looping (`seen` guards hand-edited exports).
  const chain: ChatgptNode[] = [];
  const seen = new Set<string>();
  let cursor = leafId;
  while (cursor !== null && cursor in mapping && !seen.has(cursor)) {
    seen.add(cursor);
    const node = mapping[cursor];
    chain.push(node);
    cursor = typeof node.parent === "string" ? node.parent : null;
  }
  chain.reverse();

  const records: TrajectoryRecord[] = [];
  for (const node of chain) {
    const message = node.message;
    if (!message) {
      continue;
    }
    const role = message.author?.role;
    if (role !== "user" && role !== "assistant") {
      continue;
    }
    if (message.metadata?.is_visually_hidden_from_conversation === true) {
      continue;
    }
    const text = chatgptMessageText(message.content);
    if (text === "") {
      continue;
    }
    records.push({
      role,
      content: text,
      timestamp: new Date(parseEpochSecondsMs(message.create_time) ?? createdAt ?? 0).toISOString(),
    });
  }
  if (records.length === 0 && Object.keys(mapping).length > 0) {
    warnings.push("no usable messages in this conversation");
  }
  return { title, createdAt, records, warnings };
}

function chatgptMessageText(content: unknown): string {
  if (!content || typeof content !== "object") {
    return "";
  }
  const { content_type: contentType, parts } = content as {
    content_type?: unknown;
    parts?: unknown;
  };
  if (contentType !== "text" && contentType !== "multimodal_text") {
    // Tool payloads (code, execution_output, …) never render as prose in the
    // source product's conversation view; skip them like system rows.
    return "";
  }
  if (!Array.isArray(parts)) {
    return "";
  }
  const lines: string[] = [];
  for (const part of parts) {
    if (typeof part === "string") {
      if (part.trim() !== "") {
        lines.push(part);
      }
    } else if (part && typeof part === "object") {
      // multimodal parts are image pointers and similar payloads.
      lines.push("[attachment omitted]");
    }
  }
  return lines.join("\n\n").trim();
}

function newestMessageNode(mapping: Record<string, ChatgptNode>): string | null {
  let best: { id: string; at: number } | null = null;
  for (const [id, node] of Object.entries(mapping)) {
    if (!node.message) {
      continue;
    }
    const at = parseEpochSecondsMs(node.message.create_time) ?? 0;
    if (best === null || at > best.at) {
      best = { id, at };
    }
  }
  return best?.id ?? null;
}

/** Epoch ms from ChatGPT's float-second timestamps, or null. */
export function parseEpochSecondsMs(value: unknown): number | null {
  if (typeof value !== "number" || !Number.isFinite(value) || value <= 0) {
    return null;
  }
  // Second-resolution epochs sit far below any millisecond epoch of the same
  // era; scale them up rather than misreading them as 1970.
  return value >= 1_000_000_000_000 ? Math.round(value) : Math.round(value * 1000);
}
