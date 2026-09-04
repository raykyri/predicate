import assert from "node:assert/strict";
import test from "node:test";
import { findAgentUiAdapter, getAgentUiAdapter } from "../src/adapters";
import { ANTIGRAVITY_ADAPTER_ID, antigravityUiAdapter } from "../src/adapters/antigravity";
import { adapterLabel } from "../src/lib/threadGraph";
import { buildTimelineItems } from "../src/lib/turnTimeline";
import type { Turn } from "../src/types";

test("antigravity ui adapter is registered correctly", () => {
  assert.equal(antigravityUiAdapter.id, ANTIGRAVITY_ADAPTER_ID);
  assert.equal(antigravityUiAdapter.label, "Antigravity");
  assert.equal(findAgentUiAdapter(ANTIGRAVITY_ADAPTER_ID)?.label, "Antigravity");
  assert.equal(getAgentUiAdapter(ANTIGRAVITY_ADAPTER_ID).id, "antigravity");
  assert.equal(antigravityUiAdapter.supportsFork, false);
});

test("antigravity adapter label in thread graph", () => {
  assert.equal(adapterLabel(ANTIGRAVITY_ADAPTER_ID), "Antigravity");
});

test("buildTimelineItems handles Antigravity turns sequence with thinking and tools", () => {
  const turns: Turn[] = [
    {
      id: "agent-1-0",
      agentId: "agent-1",
      role: "user",
      blocks: [{ type: "text", text: "Please list the files in this directory." }],
      sourceIndex: 0,
    },
    {
      id: "agent-1-1",
      agentId: "agent-1",
      role: "assistant",
      blocks: [
        {
          type: "raw",
          value: { type: "thinking", thinking: "I should inspect the directory." },
        },
        {
          type: "toolUse",
          id: null,
          name: "list_dir",
          input: { DirectoryPath: "/Users/test" },
        },
      ],
      sourceIndex: 1,
    },
    {
      id: "agent-1-2",
      agentId: "agent-1",
      role: "tool",
      blocks: [
        {
          type: "toolResult",
          toolUseId: null,
          content: '["file1.txt", "file2.txt"]',
          isError: false,
        },
      ],
      sourceIndex: 2,
    },
    {
      id: "agent-1-3",
      agentId: "agent-1",
      role: "assistant",
      blocks: [{ type: "text", text: "The directory contains file1.txt and file2.txt." }],
      sourceIndex: 3,
    },
  ];

  const items = buildTimelineItems(turns);
  // User message
  assert.equal(items[0].role, "user");
  assert.equal(items[0].blocks[0].type, "text");

  // Assistant message with activities (thinking and tool calls form an activityGroup)
  const assistantItem = items.find((item) => item.role === "assistant");
  assert.ok(assistantItem, "assistant item should exist");
  assert.ok(assistantItem.activities.length > 0, "assistant item should have activities");
  const group = assistantItem.activities[0];
  assert.equal(group.type, "activityGroup");
  if (group.type === "activityGroup") {
    assert.equal(group.children.length, 2);
    const thinkingChild = group.children.find((c) => c.type === "thinking");
    assert.ok(thinkingChild, "thinking child should exist");
    const toolChild = group.children.find((c) => c.type === "tool");
    assert.ok(toolChild, "tool child should exist");
    if (toolChild && toolChild.type === "tool") {
      assert.equal(toolChild.name, "list_dir");
      assert.equal(toolChild.isError, false);
      assert.ok(toolChild.result);
    }
  }
});
