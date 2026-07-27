import assert from "node:assert/strict";
import test from "node:test";
import { trajectoryRecordsToTurns } from "../src/lib/import/trajectoryToTurns";
import { draftsFromPayloads } from "../src/lib/import/importWorker";
import type { TrajectoryRecord } from "../src/lib/import/types";

test("trajectory records convert to prose turns with emptied tool calls", () => {
  const records: TrajectoryRecord[] = [
    { role: "meta", source: "claude-code", model: "claude-x" },
    { role: "user", content: "Question?", timestamp: "2026-07-01T10:00:00.000Z" },
    { role: "reasoning", content: "private thinking", timestamp: "2026-07-01T10:00:01.000Z" },
    {
      role: "assistant",
      content: null,
      timestamp: "2026-07-01T10:00:02.000Z",
      tool_calls: [
        { id: "t1", name: "Read", args: '{"file_path":"/tmp/big-secret-args"}' },
        { id: "t2", name: "Bash", args: '{"command":"ls"}' },
      ],
    },
    { role: "tool", tool_call_id: "t1", content: "tool payload", timestamp: "2026-07-01T10:00:03.000Z" },
    { role: "assistant", content: "Answer.", timestamp: "not a date" },
  ];
  const turns = trajectoryRecordsToTurns(records, "import-0");

  // meta/reasoning/tool records vanish; the sanitizer would drop them anyway
  // and their payloads must not cross the IPC bridge.
  assert.deepEqual(
    turns.map((turn) => turn.role),
    ["user", "assistant", "assistant"],
  );
  assert.equal(JSON.stringify(turns).includes("thinking"), false);
  assert.equal(JSON.stringify(turns).includes("tool payload"), false);
  assert.equal(JSON.stringify(turns).includes("big-secret-args"), false);

  // Tool calls survive as count-bearing ToolUse blocks with emptied input.
  const toolTurn = turns[1];
  assert.deepEqual(toolTurn.blocks, [
    { type: "toolUse", id: "t1", name: "Read", input: {} },
    { type: "toolUse", id: "t2", name: "Bash", input: {} },
  ]);

  // Timestamps map to epoch ms; unparseable ones become null.
  assert.equal(turns[0].timestamp, Date.parse("2026-07-01T10:00:00.000Z"));
  assert.equal(turns[2].timestamp, null);

  // Identities are sequential under the given agent id; the backend reissues
  // them at import time regardless.
  assert.deepEqual(
    turns.map((turn) => [turn.id, turn.sourceIndex, turn.agentId]),
    [
      ["import-0-src-0", 0, "import-0"],
      ["import-0-src-1", 1, "import-0"],
      ["import-0-src-2", 2, "import-0"],
    ],
  );
});

test("draftsFromPayloads maps parse failures to errors without failing the batch", () => {
  const good = JSON.stringify({
    name: "Fine",
    created_at: "2026-06-01T09:00:00.000Z",
    chat_messages: [
      { sender: "human", created_at: "2026-06-01T09:00:01.000Z", text: "Q" },
      { sender: "assistant", created_at: "2026-06-01T09:00:02.000Z", text: "A" },
    ],
  });
  const { drafts, errors } = draftsFromPayloads("claudeAi", [good, "{not json"]);
  assert.equal(drafts.length, 1);
  assert.equal(drafts[0].title, "Fine");
  assert.equal(drafts[0].turns.length, 2);
  assert.equal(errors.length, 1);
});
