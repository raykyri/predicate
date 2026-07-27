import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { parseClaudeAiConversation } from "../src/lib/import/claudeAiExport";
import { parseChatgptConversation } from "../src/lib/import/chatgptExport";
import { parseHarnessTranscript } from "../src/lib/import/harnessTranscript";
import { parseOpencodeSession } from "../src/lib/import/opencodeSession";
import type {
  TrajectoryAssistantRecord,
  TrajectoryRecord,
} from "../src/lib/import/types";

function prose(records: TrajectoryRecord[]): Array<[string, string | null]> {
  return records
    .filter((record) => record.role === "user" || record.role === "assistant")
    .map((record) => [record.role, (record as TrajectoryAssistantRecord).content]);
}

test("claude.ai conversations parse content blocks, legacy text, and attachments", () => {
  const conversation = JSON.stringify({
    uuid: "conv-1",
    name: "Trip planning",
    created_at: "2026-06-01T09:00:00.000Z",
    chat_messages: [
      {
        sender: "human",
        created_at: "2026-06-01T09:00:01.000Z",
        content: [
          { type: "text", text: "Plan a two-day Kyoto trip." },
          { type: "unknown_widget", data: "ignored" },
        ],
        attachments: [{ file_name: "itinerary.pdf" }],
      },
      {
        // Legacy shape: no content array, bare text.
        sender: "assistant",
        created_at: "2026-06-01T09:00:05.000Z",
        text: "Day one: Fushimi Inari at dawn.",
      },
      // Neither text nor attachments — dropped.
      { sender: "human", created_at: "2026-06-01T09:00:06.000Z", content: [] },
      // Unknown sender — dropped.
      { sender: "system", created_at: "2026-06-01T09:00:07.000Z", text: "internal" },
    ],
  });
  const parsed = parseClaudeAiConversation(conversation);
  assert.equal(parsed.title, "Trip planning");
  assert.equal(parsed.createdAt, Date.parse("2026-06-01T09:00:00.000Z"));
  assert.deepEqual(prose(parsed.records), [
    ["user", "Plan a two-day Kyoto trip.\n\n[attachment omitted: itinerary.pdf]"],
    ["assistant", "Day one: Fushimi Inari at dawn."],
  ]);
  assert.equal(parsed.records[0].role === "user" && parsed.records[0].timestamp,
    "2026-06-01T09:00:01.000Z");
  assert.deepEqual(parsed.warnings, []);
});

test("chatgpt conversations follow the canonical branch and skip hidden rows", () => {
  const conversation = JSON.stringify({
    id: "g-1",
    title: "Rust question",
    create_time: 1_782_900_000.5,
    current_node: "n3b",
    mapping: {
      root: { id: "root", parent: null, children: ["n1"] },
      n1: {
        id: "n1",
        parent: "root",
        children: ["n2"],
        message: {
          author: { role: "system" },
          content: { content_type: "text", parts: ["system preamble"] },
          metadata: { is_visually_hidden_from_conversation: true },
        },
      },
      n2: {
        id: "n2",
        parent: "n1",
        children: ["n3a", "n3b"],
        message: {
          author: { role: "user" },
          create_time: 1_782_900_010.0,
          content: { content_type: "text", parts: ["How do lifetimes work?"] },
        },
      },
      // The abandoned branch: regenerated away, must not import.
      n3a: {
        id: "n3a",
        parent: "n2",
        children: [],
        message: {
          author: { role: "assistant" },
          create_time: 1_782_900_020.0,
          content: { content_type: "text", parts: ["First, worse answer."] },
        },
      },
      n3b: {
        id: "n3b",
        parent: "n2",
        children: [],
        message: {
          author: { role: "assistant" },
          create_time: 1_782_900_030.0,
          content: {
            content_type: "multimodal_text",
            parts: [{ asset_pointer: "file-service://file-abc" }, "They tie borrows to scopes."],
          },
        },
      },
    },
  });
  const parsed = parseChatgptConversation(conversation);
  assert.equal(parsed.title, "Rust question");
  assert.equal(parsed.createdAt, 1_782_900_000_500);
  assert.deepEqual(prose(parsed.records), [
    ["user", "How do lifetimes work?"],
    ["assistant", "[attachment omitted]\n\nThey tie borrows to scopes."],
  ]);
  assert.deepEqual(parsed.warnings, []);
});

test("chatgpt parsing falls back to the newest message on a dangling current_node", () => {
  const conversation = JSON.stringify({
    title: "Dangling",
    create_time: 1_782_900_000.0,
    current_node: "gone",
    mapping: {
      root: { id: "root", parent: null, children: ["n1"] },
      n1: {
        id: "n1",
        parent: "root",
        children: ["n2"],
        message: {
          author: { role: "user" },
          create_time: 1_782_900_001.0,
          content: { content_type: "text", parts: ["Question?"] },
        },
      },
      n2: {
        id: "n2",
        parent: "n1",
        children: [],
        message: {
          author: { role: "assistant" },
          create_time: 1_782_900_002.0,
          content: { content_type: "text", parts: ["Answer."] },
        },
      },
    },
  });
  const parsed = parseChatgptConversation(conversation);
  assert.deepEqual(prose(parsed.records), [
    ["user", "Question?"],
    ["assistant", "Answer."],
  ]);
  assert.equal(parsed.warnings.length, 1);
  assert.match(parsed.warnings[0], /current_node/);
});

// Pins the @letta-ai/trajectory contract this feature depends on: record
// roles, the text/tool_call record split, and reasoning/tool records for the
// converter to drop.
test("claude code transcripts normalize through the trajectory library", () => {
  const transcript = readFileSync(
    new URL("./fixtures/claude-code-session.jsonl", import.meta.url),
    "utf8",
  );
  const parsed = parseHarnessTranscript("claudeCode", transcript);
  assert.equal(parsed.title, null);
  assert.equal(parsed.createdAt, Date.parse("2026-07-01T10:00:00.000Z"));

  const roles = parsed.records.map((record) => record.role);
  assert.deepEqual(roles, [
    "meta",
    "user",
    "reasoning",
    "assistant",
    "assistant",
    "tool",
    "assistant",
  ]);
  const toolCallRecord = parsed.records[4] as TrajectoryAssistantRecord;
  assert.equal(toolCallRecord.content, null);
  assert.equal(toolCallRecord.tool_calls?.length, 1);
  assert.equal(toolCallRecord.tool_calls?.[0].name, "Read");
  assert.deepEqual(prose(parsed.records).filter(([, content]) => content !== null), [
    ["user", "Why does the retry test flake?"],
    ["assistant", "Let me look at the test."],
    ["assistant", "The test races a real 50ms timer; fake the clock instead."],
  ]);
});

// Same contract for the codex adapter: session_meta becomes a meta record,
// response_item message payloads become prose, and record timestamps carry
// through so the first timed record dates the conversation.
test("codex rollouts normalize through the trajectory library", () => {
  const transcript = readFileSync(
    new URL("./fixtures/codex-rollout.jsonl", import.meta.url),
    "utf8",
  );
  const parsed = parseHarnessTranscript("codex", transcript);
  assert.equal(parsed.title, null);
  // The session_meta line is untimed metadata; the first response_item's
  // timestamp dates the conversation.
  assert.equal(parsed.createdAt, Date.parse("2026-07-10T09:00:01.000Z"));
  assert.deepEqual(parsed.warnings, []);
  assert.equal(parsed.records[0].role, "meta");
  assert.deepEqual(prose(parsed.records), [
    ["user", "Trim the retry loop."],
    ["assistant", "Dropped the extra backoff branch."],
  ]);
});

// OpenCode sessions arrive pre-assembled by the backend: session metadata
// plus messages with their typed parts attached. Text parts join as prose,
// tool parts become payload-free tool_calls, reasoning and step bookkeeping
// parts drop out.
test("opencode sessions parse assembled store payloads", () => {
  const payload = JSON.stringify({
    session: {
      id: "ses_alpha",
      title: "Tidy the changelog",
      directory: "/Users/pat/code/demo",
      time: { created: 1770000000000, updated: 1770000009000 },
    },
    messages: [
      {
        id: "msg_early",
        role: "user",
        time: { created: 1770000001000 },
        parts: [
          { id: "prt_a1", type: "text", text: "Trim the changelog to the last release." },
          { id: "prt_a2", type: "text", text: "Keep the headings." },
        ],
      },
      {
        id: "msg_late",
        role: "assistant",
        time: { created: 1770000002000 },
        parts: [
          { id: "prt_b1", type: "step-start" },
          { id: "prt_b2", type: "reasoning", text: "internal planning notes" },
          { id: "prt_b3", type: "text", text: "Trimmed it down to two entries." },
          {
            id: "prt_b4",
            type: "tool",
            tool: "bash",
            callID: "call_1",
            state: { status: "completed", input: { command: "wc -l CHANGELOG.md" } },
          },
          // A tool part with no discoverable name is skipped.
          { id: "prt_b5", type: "tool", state: { status: "completed" } },
          { id: "prt_b6", type: "step-finish" },
        ],
      },
      // No usable parts — the whole message drops.
      { id: "msg_empty", role: "assistant", time: { created: 1770000003000 }, parts: [] },
    ],
  });
  const parsed = parseOpencodeSession(payload);
  assert.equal(parsed.title, "Tidy the changelog");
  assert.equal(parsed.createdAt, 1770000000000);
  assert.deepEqual(parsed.warnings, []);
  assert.deepEqual(prose(parsed.records), [
    ["user", "Trim the changelog to the last release.\n\nKeep the headings."],
    ["assistant", "Trimmed it down to two entries."],
    ["assistant", null],
  ]);
  // Reasoning parts never surface as records.
  assert.ok(parsed.records.every((record) => record.role !== "reasoning"));
  const toolRecord = parsed.records[2] as TrajectoryAssistantRecord;
  assert.deepEqual(toolRecord.tool_calls, [{ id: "call_1", name: "bash", args: "{}" }]);
  // Per-message creation times carry through as ISO timestamps.
  assert.equal(
    parsed.records[0].role === "user" && parsed.records[0].timestamp,
    new Date(1770000001000).toISOString(),
  );
  assert.equal(toolRecord.timestamp, new Date(1770000002000).toISOString());
});
