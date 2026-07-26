import assert from "node:assert/strict";
import test from "node:test";
import { buildHandoffDocument } from "../src/lib/handoff";
import { buildTimelineItems } from "../src/lib/turnTimeline";
import type { Turn, TurnBlock } from "../src/types";

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

function text(value: string): TurnBlock {
  return { type: "text", text: value };
}

function toolUse(name: string, input: unknown, id: string | null = null): TurnBlock {
  return { type: "toolUse", id, name, input };
}

function toolResult(content: unknown, toolUseId: string | null = null): TurnBlock {
  return { type: "toolResult", toolUseId, content, isError: false };
}

function itemsFor(turns: Turn[]) {
  return buildTimelineItems(turns, true);
}

function userKey(turns: Turn[], occurrence: number) {
  const items = itemsFor(turns);
  const users = items.filter((item) => item.role === "user");
  return users[occurrence].key;
}

function build(turns: Turn[], anchorOccurrence: number, overrides = {}) {
  return buildHandoffDocument({
    items: itemsFor(turns),
    anchorKey: userKey(turns, anchorOccurrence),
    assistantLabel: "Claude",
    ...overrides,
  });
}

test("renders the sections a receiving agent needs", () => {
  const turns = [
    turn("user", [text("Add retries to the fetch helper.")]),
    turn("assistant", [text("Done — I added a backoff loop.")]),
    turn("user", [text("Now cover it with a test.")]),
  ];

  const document = build(turns, 1, {
    context: {
      cwd: "/work/repo",
      branch: "feature/retries",
      agentLabel: "Claude",
      model: "claude-opus-5",
    },
  });

  assert.ok(document);
  assert.match(document, /^# Session handoff\n/);
  assert.match(document, /Working directory: `\/work\/repo`/);
  assert.match(document, /Git branch: `feature\/retries`/);
  assert.match(document, /Previous agent: Claude \(claude-opus-5\)/);
  assert.match(document, /## Conversation so far/);
  assert.match(document, /<transcript>/);
  assert.match(document, /### User\nAdd retries to the fetch helper\./);
  assert.match(document, /### Claude\nDone — I added a backoff loop\./);
  // The clicked message is the outstanding ask, so it lands in the request
  // section rather than in the history.
  assert.match(document, /## Current request\n\n<request>\nNow cover it with a test\.\n<\/request>/);
  assert.equal(document.includes("### User\nNow cover it with a test."), false);
});

test("omits sections that have no content", () => {
  const turns = [turn("user", [text("First ask.")])];
  const document = build(turns, 0);

  assert.ok(document);
  assert.equal(document.includes("## Environment"), false);
  assert.equal(document.includes("## Work already done"), false);
  assert.equal(document.includes("## Conversation so far"), false);
  assert.match(document, /<request>\nFirst ask\.\n<\/request>/);
});

test("summarizes tools and splits edited from read files, without tool payloads", () => {
  const turns = [
    turn("user", [text("Fix the parser.")]),
    turn("assistant", [
      text("Looking now."),
      toolUse("Read", { file_path: "/work/repo/src/parser.ts" }, "t1"),
      toolResult("export function parse() { /* secret token abc123 */ }", "t1"),
      toolUse("Read", { file_path: "/work/repo/src/lexer.ts" }, "t2"),
      toolResult("ok", "t2"),
      toolUse("Edit", { file_path: "/work/repo/src/parser.ts" }, "t3"),
      toolResult("ok", "t3"),
      toolUse("Bash", { command: "npm test" }, "t4"),
      toolResult("passed", "t4"),
    ]),
    turn("assistant", [text("Fixed.")]),
    turn("user", [text("Ship it.")]),
  ];

  const document = build(turns, 1);

  assert.ok(document);
  assert.match(document, /- Files edited: `\/work\/repo\/src\/parser\.ts`/);
  assert.match(document, /- Files read or searched: `\/work\/repo\/src\/lexer\.ts`/);
  // parser.ts was read first and edited later: it belongs to one list only.
  assert.equal(/read or searched:.*parser\.ts/.test(document), false);
  assert.match(document, /\[tools: Read ×2, Bash, Edit\]/);
  // Tool results never travel with a handoff.
  assert.equal(document.includes("secret token abc123"), false);
  assert.equal(document.includes("npm test"), false);
});

test("drops thinking blocks and system messages", () => {
  const turns = [
    turn("user", [text("Start.")]),
    turn("system", [text("System noise nobody asked for.")]),
    turn("assistant", [
      { type: "raw", value: { type: "thinking", thinking: "internal deliberation" } },
      text("Working on it."),
    ]),
    turn("user", [text("Continue.")]),
  ];

  const document = build(turns, 1);

  assert.ok(document);
  assert.equal(document.includes("internal deliberation"), false);
  assert.equal(document.includes("System noise"), false);
  assert.match(document, /### Claude\nWorking on it\./);
});

test("strips qmux tagged instructions and instruction-only messages", () => {
  const turns = [
    turn("user", [
      text(
        [
          "Please refactor.",
          "",
          "<system-reminder>",
          "hidden plumbing",
          "</system-reminder>",
        ].join("\n"),
      ),
    ]),
    turn("assistant", [text("Sure.")]),
    turn("user", [
      text(["<environment_context>", "only plumbing", "</environment_context>"].join("\n")),
    ]),
    turn("assistant", [text("Ok.")]),
    turn("user", [text("Keep going.")]),
  ];

  const document = build(turns, 2);

  assert.ok(document);
  assert.equal(document.includes("hidden plumbing"), false);
  assert.equal(document.includes("only plumbing"), false);
  assert.match(document, /### User\nPlease refactor\./);
});

test("skips superseded turns and labels interrupted ones", () => {
  const turns = [
    turn("user", [text("Original ask.")]),
    turn("assistant", [text("Abandoned branch work.")], { status: "superseded" }),
    turn("assistant", [text("Partial answer")], { status: "interrupted" }),
    turn("user", [text("Carry on.")]),
  ];

  const document = build(turns, 1);

  assert.ok(document);
  assert.equal(document.includes("Abandoned branch work."), false);
  assert.match(document, /### Claude \(interrupted\)\nPartial answer/);
});

test("returns null when the anchor key is not in the items", () => {
  const turns = [turn("user", [text("Hello.")])];
  assert.equal(
    buildHandoffDocument({
      items: itemsFor(turns),
      anchorKey: "message-user-nonexistent:0",
      assistantLabel: "Claude",
    }),
    null,
  );
});

test("truncates an oversized history message but keeps the request intact", () => {
  const long = "x".repeat(5_000);
  const request = "y".repeat(3_000);
  const turns = [
    turn("user", [text(long)]),
    turn("assistant", [text("Ack.")]),
    turn("user", [text(request)]),
  ];

  const document = build(turns, 1);

  assert.ok(document);
  assert.match(document, /\[… 1000 characters omitted …\]/);
  assert.ok(document.includes(request));
});

test("elides the middle of a long conversation and says so", () => {
  const turns: Turn[] = [turn("user", [text("The original task statement.")])];
  for (let index = 0; index < 40; index += 1) {
    turns.push(turn("assistant", [text(`reply ${index} ${"z".repeat(3_000)}`)]));
    turns.push(turn("user", [text(`follow-up ${index}`)]));
  }
  turns.push(turn("assistant", [text("Ready.")]));
  turns.push(turn("user", [text("Final ask.")]));

  const document = build(turns, 41);

  assert.ok(document);
  assert.match(document, /\[… \d+ earlier message\(s\) omitted for length …\]/);
  // The first message and the tail survive the middle-out drop.
  assert.ok(document.includes("The original task statement."));
  assert.ok(document.includes("follow-up 39"));
  assert.ok(document.includes("Final ask."));
  assert.ok(document.length <= 60_000);
});

test("respects an explicit limit override", () => {
  const turns = [
    turn("user", [text("a".repeat(2_000))]),
    turn("assistant", [text("b".repeat(2_000))]),
    turn("user", [text("c".repeat(2_000))]),
  ];

  const document = build(turns, 1, { limits: { totalCharacters: 1_200 } });

  assert.ok(document);
  assert.ok(document.length <= 1_300);
  assert.match(document, /\[… handoff truncated …\]/);
});
