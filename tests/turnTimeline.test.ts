import assert from "node:assert/strict";
import test from "node:test";
import {
  assistantGroupTimestamp,
  annotationsForTimeline,
  assistantRunCopyTextByItemKey,
  assistantTextFromTimelineItems,
  buildTimelineItems,
  formatAbsoluteMessageTimestamp,
  formatMessageTimestamp,
  formatPlainTextTranscript,
  messageItemCopyText,
  messageItemText,
  shouldShowAssistantGroupTimestamp,
  thinkingProseText,
  timelineItemsAfterLastToolCall,
  timelineItemsContainTranscriptActivity,
} from "../src/lib/turnTimeline";
import type { TranscriptAnnotation, Turn, TurnBlock } from "../src/types";

let nextIndex = 0;

function turn(role: string, blocks: TurnBlock[], overrides: Partial<Turn> = {}): Turn {
  const sourceIndex = nextIndex++;
  return {
    id: `agent-1-${sourceIndex}`,
    agentId: "agent-1",
    role,
    blocks,
    sourceIndex,
    ...overrides,
  };
}

function text(t: string): TurnBlock {
  return { type: "text", text: t };
}

function toolUse(id: string | null, name = "Read", input: unknown = {}): TurnBlock {
  return { type: "toolUse", id, name, input };
}

function toolResult(
  toolUseId: string | null,
  content: unknown,
  isError = false,
): TurnBlock {
  return { type: "toolResult", toolUseId, content, isError };
}

test("activities of a new reply render below the user message that triggered them", () => {
  nextIndex = 0;
  const turns = [
    turn("assistant", [text("earlier reply")]),
    turn("user", [text("new question")]),
    // The reply opens with thinking and a tool call before any text — the
    // normal shape of an agent response record.
    turn("assistant", [
      { type: "raw", value: { thinking: "hmm" } },
      { type: "toolUse", id: "t1", name: "Read", input: { file_path: "a.ts" } },
    ]),
    turn("user", [{ type: "toolResult", toolUseId: "t1", content: "data", isError: false }]),
    turn("assistant", [text("the answer")]),
  ];
  const items = buildTimelineItems(turns);
  const shapes = items.map((item) => ({
    role: item.role,
    text: item.blocks.map((block) => (block.type === "text" ? block.text : "raw")).join("|"),
    activityCount: item.activities.length,
  }));
  // The user question must come before any item holding the reply's
  // activities; the earlier assistant reply must hold none of them.
  assert.equal(shapes[0].role, "assistant");
  assert.equal(shapes[0].text, "earlier reply");
  assert.equal(shapes[0].activityCount, 0);
  assert.equal(shapes[1].role, "user");
  assert.equal(shapes[1].text, "new question");
  const activityOwner = items[2];
  assert.equal(activityOwner.role, "assistant");
  assert.ok(activityOwner.activities.length > 0, "reply activities attach after the question");
});

test("tracks source turns through message folding and places saved highlights", () => {
  nextIndex = 0;
  const first = turn("assistant", [text("first")]);
  const second = turn("assistant", [text("second")]);
  const question = turn("user", [text("next")]);
  const items = buildTimelineItems([first, second, question]);
  assert.deepEqual(items[0].sourceTurnIds, [first.id, second.id]);
  assert.deepEqual(items[0].blockSourceTurnIds, [first.id, second.id]);

  const annotations: TranscriptAnnotation[] = [
    { id: "later", sourceTurnId: second.id, text: "second", createdAt: 20 },
    { id: "earlier", sourceTurnId: first.id, text: "first", createdAt: 10 },
    { id: "missing", sourceTurnId: "gone", text: "gone", createdAt: 30 },
  ];
  const placed = annotationsForTimeline(items, annotations);
  assert.deepEqual(
    placed.byItemKey.get(items[0].key)?.map(({ id }) => id),
    ["earlier", "later"],
  );
  assert.deepEqual(placed.orphaned.map(({ id }) => id), ["missing"]);
});

test("assistant text copy preserves original markdown and omits non-answer content", () => {
  nextIndex = 0;
  const items = buildTimelineItems([
    turn("user", [text("question")]),
    turn("assistant", [text("# Heading"), toolUse("tool-1")]),
    turn("user", [toolResult("tool-1", "result")]),
    turn("assistant", [text("A **bold** conclusion.")]),
  ]);

  assert.equal(
    assistantTextFromTimelineItems(items),
    "# Heading\n\nA **bold** conclusion.",
  );
});

test("message copy strips tagged instructions and rejects system messages", () => {
  nextIndex = 0;
  const items = buildTimelineItems([
    turn("assistant", [
      text("# Visible answer"),
      text(
        [
          "```xml",
          "<system-reminder>",
          "Literal code sample.",
          "</system-reminder>",
          "```",
          "",
          "<system-reminder>",
          "Hidden system instructions.",
          "</system-reminder>",
          "",
          "Keep **this** conclusion.",
        ].join("\n"),
      ),
      { type: "raw", value: { thinking: "hidden reasoning" } },
    ]),
    turn("system", [text("Never copy this system message.")]),
  ]);

  assert.equal(
    messageItemCopyText(items[0]),
    [
      "# Visible answer",
      "",
      "```xml",
      "<system-reminder>",
      "Literal code sample.",
      "</system-reminder>",
      "```",
      "",
      "",
      "",
      "Keep **this** conclusion.",
    ].join("\n"),
  );
  assert.equal(messageItemCopyText(items[1]), null);
});

test("assistant run copy appears once per user block and skips system messages", () => {
  nextIndex = 0;
  const items = buildTimelineItems([
    turn("system", [text("Initial system context.")]),
    turn("assistant", [text("Opening answer.")]),
    turn("system", [text("System context between assistant messages.")]),
    turn("assistant", [text("Opening continuation.")]),
    turn("user", [text("First question.")]),
    turn("system", [text("System context before the response.")]),
    turn("assistant", [text("First response."), toolUse("tool-1")]),
    turn("user", [toolResult("tool-1", "tool output")]),
    turn("assistant", [text("Second response.")]),
    turn("user", [
      text("<system-reminder>\nUser-role system context.\n</system-reminder>"),
    ]),
    turn("system", [text("More hidden system context.")]),
    turn("assistant", [text("Third response.")]),
    turn("user", [text("Second question.")]),
    turn("assistant", [
      text("<system-reminder>\nHidden assistant instruction.\n</system-reminder>"),
    ]),
    turn("system", [text("System context after empty assistant text.")]),
    turn("assistant", [text("Final response.")]),
  ]);
  const copyTextByKey = assistantRunCopyTextByItemKey(items);
  const itemWithText = (value: string) => {
    const item = items.find((candidate) => messageItemText(candidate) === value);
    assert.ok(item, `missing timeline item for ${value}`);
    return item;
  };

  const opening = itemWithText("Opening answer.");
  const openingContinuation = itemWithText("Opening continuation.");
  const first = itemWithText("First response.");
  const second = itemWithText("Second response.");
  const third = itemWithText("Third response.");
  const final = itemWithText("Final response.");

  assert.deepEqual([...copyTextByKey.entries()], [
    [opening.key, "Opening answer.\n\nOpening continuation."],
    [first.key, "First response.\n\nSecond response.\n\nThird response."],
    [final.key, "Final response."],
  ]);
  assert.equal(copyTextByKey.has(openingContinuation.key), false);
  assert.equal(copyTextByKey.has(second.key), false);
  assert.equal(copyTextByKey.has(third.key), false);
  assert.equal(
    [...copyTextByKey.values()].some((value) => value.includes("System context")),
    false,
  );
});

test("plain text transcript copy includes only user and assistant message text", () => {
  nextIndex = 0;
  const transcript = formatPlainTextTranscript(
    [
      turn("system", [text("system prompt")]),
      turn("user", [text("Question before tools")]),
      turn("assistant", [
        { type: "raw", value: { thinking: "private reasoning" } },
        toolUse("tool-1", "Read", { file_path: "secret.txt" }),
      ]),
      turn("user", [toolResult("tool-1", "tool result")]),
      turn("assistant", [text("Answer with **markdown**.")]),
      turn("user", [
        text(`# Injected\n<system-reminder>\nDo not copy this.\n</system-reminder>\n\nVisible follow-up`),
      ]),
      turn("user", [
        text(`<user-instructions>\nHidden only.\n</user-instructions>`),
      ]),
    ],
    "Codex",
  );

  assert.equal(
    transcript,
    [
      "User:\nQuestion before tools",
      "Codex:\nAnswer with **markdown**.",
      "User:\nVisible follow-up",
    ].join("\n\n"),
  );
});

test("final answer view starts after the last tool call group", () => {
  nextIndex = 0;
  const items = buildTimelineItems([
    turn("assistant", [
      text("investigating"),
      toolUse("tool-1", "Read"),
      toolUse("tool-2", "Bash"),
    ]),
    turn("user", [
      toolResult("tool-1", "first result"),
      toolResult("tool-2", "second result"),
    ]),
    turn("assistant", [text("Final **answer**.")]),
  ]);

  assert.equal(items[0].activities[0]?.type, "activityGroup");
  assert.equal(timelineItemsContainTranscriptActivity(items), true);
  const answerItems = timelineItemsAfterLastToolCall(items);
  assert.equal(answerItems.length, 1);
  assert.equal(assistantTextFromTimelineItems(answerItems), "Final **answer**.");
});

test("final answer survives when trailing tool activity attaches to the answer item", () => {
  nextIndex = 0;
  // The run ends with a wrap-up tool call after its final text (a TodoWrite,
  // a post-answer check). The trailing call attaches to the same timeline
  // item as the answer text, so the item after the last tool-bearing item
  // does not exist — the boundary item IS the answer and must be kept.
  const items = buildTimelineItems([
    turn("user", [text("question")]),
    turn("assistant", [toolUse("tool-1", "Read")]),
    turn("user", [toolResult("tool-1", "research data")]),
    turn("assistant", [text("# Final Report"), toolUse("tool-2", "TodoWrite")]),
    turn("user", [toolResult("tool-2", "todos saved")]),
  ]);

  const answerItems = timelineItemsAfterLastToolCall(items);
  assert.equal(assistantTextFromTimelineItems(answerItems), "# Final Report");
  // The carried boundary copy sheds its activities: the wrap-up call stays
  // out of the answer view (it is still visible in the full trace).
  assert.ok(answerItems.every((item) => item.activities.length === 0));
  assert.equal(timelineItemsContainTranscriptActivity(items), true);
});

test("transcript activity detection includes thinking-only timelines", () => {
  nextIndex = 0;
  const items = buildTimelineItems([
    turn("assistant", [{ type: "raw", value: { thinking: "Considering options" } }]),
    turn("assistant", [text("Answer")]),
  ]);
  assert.equal(timelineItemsContainTranscriptActivity(items), true);
  const answerItems = timelineItemsAfterLastToolCall(items);
  assert.equal(assistantTextFromTimelineItems(answerItems), "Answer");
  assert.ok(answerItems.every((item) => item.activities.length === 0));
});

test("collapsed answer strips thinking attached to answer text", () => {
  nextIndex = 0;
  const items = buildTimelineItems([
    turn("assistant", [
      text("Answer"),
      { type: "raw", value: { thinking: "Considering options" } },
    ]),
  ]);

  assert.equal(items.length, 1);
  assert.equal(items[0].activities[0]?.type, "thinking");
  const answerItems = timelineItemsAfterLastToolCall(items);
  assert.equal(assistantTextFromTimelineItems(answerItems), "Answer");
  assert.ok(answerItems.every((item) => item.activities.length === 0));
});

test("transcript activity detection stays false for answer-only timelines", () => {
  nextIndex = 0;
  const items = buildTimelineItems([turn("assistant", [text("Answer only")])]);
  assert.equal(timelineItemsContainTranscriptActivity(items), false);
});

test("keys derive from turn ids so truncating old turns keeps suffix keys stable", () => {
  nextIndex = 0;
  const turns = [
    turn("user", [text("q1")]),
    turn("assistant", [text("a1")]),
    turn("user", [text("q2")]),
    turn("assistant", [text("a2")]),
  ];
  const before = buildTimelineItems(turns);
  const after = buildTimelineItems(turns.slice(2));
  const beforeKeys = new Map(before.map((item) => [item.blocks[0], item.key]));
  for (const item of after) {
    assert.equal(item.key, beforeKeys.get(item.blocks[0]), "same content keeps its key");
  }
});

test("participants that differ only by label do not merge into one message item", () => {
  nextIndex = 0;
  const participantA = { kind: "assistant" as const, actorId: "x", label: "Claude" };
  const participantB = { kind: "assistant" as const, actorId: "x", label: "Renamed" };
  const turns = [
    turn("assistant", [text("one")], { participant: participantA }),
    turn("assistant", [text("two")], { participant: participantB }),
  ];
  const items = buildTimelineItems(turns);
  assert.equal(items.length, 2);
});

test("tool results pair with their tool calls by id", () => {
  nextIndex = 0;
  const turns = [
    turn("user", [text("go")]),
    turn("assistant", [{ type: "toolUse", id: "t9", name: "Bash", input: { command: "ls" } }]),
    turn("user", [{ type: "toolResult", toolUseId: "t9", content: "files", isError: false }]),
  ];
  const items = buildTimelineItems(turns);
  const owner = items.find((item) => item.activities.length > 0);
  assert.ok(owner);
  const [activity] = owner.activities;
  assert.equal(activity.type, "tool");
  if (activity.type === "tool") {
    assert.equal(activity.result, "files");
    assert.equal(activity.isError, false);
  }
});

test("assistant text, tool activity, and continued text retain response order", () => {
  nextIndex = 0;
  const items = buildTimelineItems([
    turn("user", [text("investigate")]),
    turn("assistant", [text("First I will inspect it.")]),
    turn("assistant", [toolUse("read-1", "Read", { file_path: "a.ts" })]),
    turn("user", [toolResult("read-1", "contents")]),
    turn("assistant", [text("The file confirms the answer.")]),
  ]);

  assert.equal(items.length, 3);
  assert.equal(items[1].role, "assistant");
  assert.equal(items[1].blocks[0].type, "text");
  assert.equal(items[1].activities.length, 1);
  assert.equal(items[2].role, "assistant");
  assert.equal(items[2].blocks[0].type, "text");
  if (items[2].blocks[0].type === "text") {
    assert.equal(items[2].blocks[0].text, "The file confirms the answer.");
  }
});

test("an activity-only assistant item carries no empty message blocks", () => {
  nextIndex = 0;
  const items = buildTimelineItems([
    turn("user", [text("go")]),
    turn("assistant", [toolUse("t1", "Bash", { command: "pwd" })]),
  ]);
  const activityOnly = items[1];
  assert.equal(activityOnly.role, "assistant");
  assert.deepEqual(activityOnly.blocks, []);
  assert.equal(activityOnly.activities.length, 1);
});

test("tool results pair independently of the turn role carrying the result", () => {
  nextIndex = 0;
  const items = buildTimelineItems([
    turn("assistant", [toolUse("claude", "Read")]),
    turn("user", [toolResult("claude", "from-user-role")]),
    turn("assistant", [toolUse("codex", "Bash")]),
    turn("assistant", [toolResult("codex", "from-assistant-role")]),
  ]);
  const owner = items[0];
  const group = owner.activities[0];
  assert.equal(group.type, "activityGroup");
  if (group.type === "activityGroup") {
    const tools = group.children.filter((item) => item.type === "tool");
    assert.deepEqual(
      tools.map((entry) => entry.result),
      ["from-user-role", "from-assistant-role"],
    );
  }
});

test("exact ids win, missing ids use the oldest call, and unmatched results remain visible", () => {
  nextIndex = 0;
  const items = buildTimelineItems([
    turn("assistant", [
      toolUse("first", "Read", { file_path: "first" }),
      toolUse("second", "Read", { file_path: "second" }),
    ]),
    turn("user", [toolResult("second", "second-result")]),
    turn("user", [toolResult(null, "first-result")]),
    turn("assistant", [toolResult("missing", "orphan-result", true)]),
  ]);
  const group = items[0].activities[0];
  assert.equal(group.type, "activityGroup");
  if (group.type === "activityGroup") {
    const tools = group.children.filter((item) => item.type === "tool");
    assert.deepEqual(
      tools.map((entry) => [entry.id, entry.result, entry.isError]),
      [
        ["first", "first-result", false],
        ["second", "second-result", false],
        ["missing", "orphan-result", true],
      ],
    );
  }
});

test("an id match pairs a call and result even when their turn statuses differ", () => {
  nextIndex = 0;
  // A fork/interruption can mark the call's turn but not the result's (or vice
  // versa). The id must still win over the oldest same-status pending call, or
  // the result renders under an unrelated tool row.
  const items = buildTimelineItems([
    turn("assistant", [toolUse("live", "Read", { file_path: "live" })]),
    turn("assistant", [toolUse("cut", "Bash", { command: "make" })], {
      status: "interrupted",
    }),
    turn("user", [toolResult("cut", "cut-result")]),
    turn("user", [toolResult("live", "live-result")]),
  ]);
  const results = items.flatMap((item) =>
    item.activities.flatMap((activity) =>
      activity.type === "activityGroup"
        ? activity.children.filter((child) => child.type === "tool")
        : activity.type === "tool"
          ? [activity]
          : [],
    ),
  );
  assert.deepEqual(
    results.map((entry) => [entry.id, entry.result]),
    [
      ["live", "live-result"],
      ["cut", "cut-result"],
    ],
  );
});

test("a result with an unmatched id does not steal a pending call's slot", () => {
  nextIndex = 0;
  // The visible turn window starts after the "truncated-away" call was
  // dropped (per-agent turn cap), while "live" still awaits its own result.
  const items = buildTimelineItems([
    turn("assistant", [toolUse("live", "Bash", { command: "sleep 1" })]),
    turn("user", [toolResult("truncated-away", "late result of a truncated call")]),
    turn("user", [toolResult("live", "real result")]),
  ]);
  const group = items[0].activities[0];
  assert.equal(group.type, "activityGroup");
  if (group.type === "activityGroup") {
    const tools = group.children.filter((item) => item.type === "tool");
    assert.deepEqual(
      tools.map((entry) => [entry.id, entry.name, entry.result]),
      [
        ["live", "Bash", "real result"],
        ["truncated-away", "Tool result", "late result of a truncated call"],
      ],
    );
  }
});

test("one activity stays a leaf while multiple activities form one disclosure group", () => {
  nextIndex = 0;
  const single = buildTimelineItems([turn("assistant", [toolUse("one")])]);
  assert.equal(single[0].activities[0].type, "tool");

  const grouped = buildTimelineItems([
    turn("assistant", [toolUse("one"), { type: "raw", value: "thinking" }]),
  ]);
  assert.equal(grouped[0].activities[0].type, "activityGroup");
  if (grouped[0].activities[0].type === "activityGroup") {
    assert.equal(grouped[0].activities[0].children.length, 2);
  }
});

test("duplicate tool ids count once in an activity-group label count", () => {
  nextIndex = 0;
  const items = buildTimelineItems([
    turn("assistant", [toolUse("duplicate", "Read"), toolUse("duplicate", "Read")]),
  ]);
  const group = items[0].activities[0];
  assert.equal(group.type, "activityGroup");
  if (group.type === "activityGroup") {
    assert.equal(group.children.length, 2);
    assert.equal(group.toolCallCount, 1);
  }
});

test("running, error, superseded, interrupted, and uncertain activity metadata survives", () => {
  nextIndex = 0;
  const items = buildTimelineItems([
    turn("assistant", [toolUse("running", "Bash")]),
    turn("user", [text("boundary")]),
    turn("assistant", [toolUse("failed", "Bash")], { status: "superseded" }),
    turn("assistant", [toolResult("failed", "boom", true)], { status: "superseded" }),
    turn("user", [text("next")]),
    turn("assistant", [{ type: "raw", value: "interrupted thought" }], {
      status: "interrupted",
    }),
    turn("user", [text("again")]),
    turn("assistant", [text("uncertain answer")], { status: "uncertain" }),
  ]);

  const running = items[0].activities[0];
  assert.equal(running.type, "tool");
  if (running.type === "tool") {
    assert.equal(running.result, undefined);
  }
  const failedOwner = items.find((item) => item.status === "superseded");
  assert.ok(failedOwner);
  const failed = failedOwner.activities[0];
  assert.equal(failed.type, "tool");
  if (failed.type === "tool") {
    assert.equal(failed.isError, true);
    assert.equal(failed.result, "boom");
  }
  assert.ok(items.some((item) => item.status === "interrupted"));
  assert.ok(items.some((item) => item.status === "uncertain"));
});

test("prepending history keeps existing keys stable", () => {
  nextIndex = 0;
  const suffix = [
    turn("user", [text("question")]),
    turn("assistant", [text("answer")]),
  ];
  const before = buildTimelineItems(suffix);
  const prepended = buildTimelineItems([
    turn("user", [text("older question")]),
    turn("assistant", [text("older answer")]),
    ...suffix,
  ]);
  const keysByBlock = new Map(prepended.map((item) => [item.blocks[0], item.key]));
  for (const item of before) {
    assert.equal(keysByBlock.get(item.blocks[0]), item.key);
  }
});

test("participant boundaries apply to activity-only assistant items", () => {
  nextIndex = 0;
  const first = { kind: "assistant" as const, actorId: "one", label: "One" };
  const second = { kind: "assistant" as const, actorId: "two", label: "Two" };
  const items = buildTimelineItems([
    turn("assistant", [toolUse("one")], { participant: first }),
    turn("assistant", [toolUse("two")], { participant: second }),
  ]);
  assert.equal(items.length, 2);
  assert.equal(items[0].participant?.actorId, "one");
  assert.equal(items[1].participant?.actorId, "two");
});

test("assistant raw blocks become thinking while non-assistant raw blocks remain messages", () => {
  nextIndex = 0;
  const assistantRaw = { type: "raw" as const, value: { thought: "inspect" } };
  const systemRaw = { type: "raw" as const, value: { notice: "context" } };
  const items = buildTimelineItems([
    turn("assistant", [assistantRaw]),
    turn("system", [systemRaw]),
  ]);
  assert.deepEqual(items[0].blocks, []);
  assert.equal(items[0].activities[0].type, "thinking");
  assert.equal(items[1].role, "system");
  assert.equal(items[1].blocks[0], systemRaw);
});

test("thinkingProseText pulls reasoning out of a provider thinking block", () => {
  // A real reasoning block carries the prose alongside a long opaque signature;
  // the reader wants the prose, never the signature.
  const value = {
    type: "thinking",
    thinking: "First I check the layout, then republish.",
    signature: "CAIS/DYKiAEIDxgCKkA9uww".repeat(50),
  };
  assert.equal(
    thinkingProseText(value),
    "First I check the layout, then republish.",
  );
});

test("thinkingProseText accepts plain strings and alternate field names", () => {
  assert.equal(thinkingProseText("bare reasoning"), "bare reasoning");
  assert.equal(thinkingProseText({ thought: "inspect" }), "inspect");
  assert.equal(thinkingProseText({ text: "reason" }), "reason");
});

test("thinkingProseText returns null for shapes without prose", () => {
  // Export markers and unfamiliar objects fall back to JSON rendering.
  assert.equal(
    thinkingProseText({ type: "qmuxToolActivity", toolCalls: 3 }),
    null,
  );
  assert.equal(thinkingProseText({ thinking: "   " }), null);
  assert.equal(thinkingProseText(""), null);
  assert.equal(thinkingProseText(42), null);
  assert.equal(thinkingProseText(["a"]), null);
});

test("adding a result preserves the originating tool key", () => {
  nextIndex = 0;
  const call = turn("assistant", [toolUse("stable", "Read")]);
  const before = buildTimelineItems([call]);
  const after = buildTimelineItems([
    call,
    turn("user", [toolResult("stable", "done")]),
  ]);
  const beforeActivity = before[0].activities[0];
  const afterActivity = after[0].activities[0];
  assert.equal(beforeActivity.key, afterActivity.key);
});

test("message items carry the latest folded turn timestamp", () => {
  nextIndex = 0;
  const items = buildTimelineItems([
    turn("user", [text("hi")], { timestamp: 1_000 }),
    turn("assistant", [text("part one")], { timestamp: 2_000 }),
    // Merges into the previous assistant card (no intervening activities).
    turn("assistant", [text("part two")], { timestamp: 3_000 }),
    turn("user", [text("again")], { timestamp: 4_000 }),
    turn("assistant", [
      { type: "raw", value: { thinking: "plan" } },
      toolUse("t1"),
    ], { timestamp: 5_000 }),
    turn("assistant", [text("done")], { timestamp: 6_000 }),
  ]);

  assert.equal(items[0].role, "user");
  assert.equal(items[0].timestamp, 1_000);
  assert.equal(items[1].role, "assistant");
  assert.equal(items[1].timestamp, 3_000);
  assert.equal(items[2].role, "user");
  assert.equal(items[2].timestamp, 4_000);
  // Tool/thinking attach to an assistant owner that then gets more text.
  const lastAssistant = items[items.length - 1];
  assert.equal(lastAssistant.role, "assistant");
  assert.equal(lastAssistant.timestamp, 6_000);
});

test("assistantGroupTimestamp picks the latest time in a trailing run", () => {
  nextIndex = 0;
  const items = buildTimelineItems([
    turn("assistant", [text("a")], { timestamp: 10 }),
    turn("user", [text("q")], { timestamp: 20 }),
    turn("assistant", [text("b1")], { timestamp: 30 }),
    // Tool boundary forces a new assistant item; the group ends at the last one.
    turn("assistant", [toolUse("t1")], { timestamp: 40 }),
    turn("assistant", [text("b2")], { timestamp: 50 }),
  ]);

  // First assistant group ends at index 0.
  assert.equal(assistantGroupTimestamp(items, 0), 10);
  // User message is not an assistant group end.
  assert.equal(assistantGroupTimestamp(items, 1), null);
  // Walk the second group: timestamps 30, 40, 50 → 50 at the last item.
  const lastIndex = items.length - 1;
  assert.equal(items[lastIndex].role, "assistant");
  assert.equal(assistantGroupTimestamp(items, lastIndex), 50);
  // Mid-group index still reports the latest time up through that index.
  assert.equal(assistantGroupTimestamp(items, lastIndex - 1), 40);
});

test("formatMessageTimestamp uses relative labels under 24 hours", () => {
  const now = new Date(2026, 6, 31, 15, 0, 0).getTime(); // Jul 31 2026 3:00 PM local

  assert.equal(formatMessageTimestamp(now - 30_000, now), "just now");
  assert.equal(formatMessageTimestamp(now - 60_000, now), "1 min ago");
  assert.equal(formatMessageTimestamp(now - 5 * 60_000, now), "5 min ago");
  assert.equal(formatMessageTimestamp(now - 60 * 60_000, now), "1 hour ago");
  assert.equal(formatMessageTimestamp(now - 6 * 60 * 60_000, now), "6 hours ago");
  // Just under 24 hours still relative; at/over 24 hours falls back to absolute.
  assert.equal(
    formatMessageTimestamp(now - 24 * 60 * 60_000 + 1, now),
    "23 hours ago",
  );
});

test("working transcript tails suppress only a just-now assistant timestamp", () => {
  const now = new Date(2026, 6, 31, 15, 0, 0).getTime();
  const visible = (
    ageMs: number,
    overrides: Partial<{ atTranscriptTail: boolean; working: boolean }> = {},
  ) =>
    shouldShowAssistantGroupTimestamp(now - ageMs, {
      atTranscriptTail: true,
      working: true,
      now,
      ...overrides,
    });

  assert.equal(visible(30_000), false);
  assert.equal(visible(60_000), true);
  assert.equal(visible(30_000, { working: false }), true);
  assert.equal(visible(30_000, { atTranscriptTail: false }), true);
});

test("formatAbsoluteMessageTimestamp uses time-only for today and date otherwise", () => {
  const now = new Date(2026, 6, 31, 15, 0, 0).getTime(); // Jul 31 2026 3:00 PM local
  const today = new Date(2026, 6, 31, 9, 5, 0).getTime();
  const earlierThisYear = new Date(2026, 0, 15, 14, 30, 0).getTime();
  const lastYear = new Date(2025, 11, 25, 8, 0, 0).getTime();
  const locale = "en-US";

  const todayLabel = formatAbsoluteMessageTimestamp(today, now, locale);
  assert.match(todayLabel, /9:05/);
  assert.doesNotMatch(todayLabel, /Jan|Jul|202/);

  const earlierLabel = formatAbsoluteMessageTimestamp(earlierThisYear, now, locale);
  assert.match(earlierLabel, /Jan/);
  assert.match(earlierLabel, /15/);
  assert.doesNotMatch(earlierLabel, /2026/);

  const lastYearLabel = formatAbsoluteMessageTimestamp(lastYear, now, locale);
  assert.match(lastYearLabel, /2025/);
});

test("formatMessageTimestamp falls back to absolute after 24 hours", () => {
  const now = new Date(2026, 6, 31, 15, 0, 0).getTime();
  const earlierThisYear = new Date(2026, 0, 15, 14, 30, 0).getTime();
  const locale = "en-US";
  const label = formatMessageTimestamp(earlierThisYear, now, locale);
  assert.match(label, /Jan/);
  assert.match(label, /15/);
  assert.equal(
    label,
    formatAbsoluteMessageTimestamp(earlierThisYear, now, locale),
  );
});
