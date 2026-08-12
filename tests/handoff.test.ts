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

function keyFor(turns: Turn[], role: string, occurrence: number) {
  const matching = itemsFor(turns).filter((item) => item.role === role);
  return matching[occurrence].key;
}

function build(turns: Turn[], anchorOccurrence: number, overrides = {}) {
  return buildHandoffDocument({
    items: itemsFor(turns),
    anchorKey: keyFor(turns, "user", anchorOccurrence),
    assistantLabel: "Claude",
    ...overrides,
  });
}

/** Anchored on an assistant message — the same menu action, from the other role. */
function buildFromAssistant(turns: Turn[], anchorOccurrence: number, overrides = {}) {
  return buildHandoffDocument({
    items: itemsFor(turns),
    anchorKey: keyFor(turns, "assistant", anchorOccurrence),
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

test("skips visible records excluded from active context", () => {
  const turns = [
    turn("user", [text("Original ask.")], { contextStatus: "rolledBack" }),
    turn("assistant", [text("Old answer.")], { contextStatus: "rolledBack" }),
    turn("user", [text("Current ask.")]),
    turn("assistant", [text("Current answer.")]),
  ];

  const document = build(turns, 1);

  assert.ok(document);
  assert.equal(document.includes("Original ask."), false);
  assert.equal(document.includes("Old answer."), false);
  assert.match(document, /Current ask\./);
});

test("hands off an assistant reply as where the previous agent left off", () => {
  const turns = [
    turn("user", [text("Add retries to the fetch helper.")]),
    turn("assistant", [text("Done — a backoff loop. Next I'd cover it with a test.")]),
    turn("user", [text("Ship it.")]),
  ];

  const document = buildFromAssistant(turns, 0);

  assert.ok(document);
  assert.match(document, /Read it, then carry on from where the previous agent left off\./);
  assert.match(document, /### User\nAdd retries to the fetch helper\./);
  assert.match(
    document,
    /## Where the previous agent left off\n\n<last-turn>\nDone — a backoff loop\. Next I'd cover it with a test\.\n<\/last-turn>/,
  );
  assert.match(document, /^Pick up from there: continue the work/m);
  // No outstanding ask travels with an assistant handoff, and nothing after the
  // anchored reply belongs to it.
  assert.equal(document.includes("## Current request"), false);
  assert.equal(document.includes("Ship it."), false);
  // The anchored reply is the trailing section, not a history entry.
  assert.equal(document.includes("### Claude"), false);
});

test("spans an assistant run split by activity and counts its work as done", () => {
  const turns = [
    turn("user", [text("Fix the parser.")]),
    turn("assistant", [
      text("Looking now."),
      toolUse("Read", { file_path: "/work/repo/src/parser.ts" }, "t1"),
      toolResult("export function parse() { /* secret token abc123 */ }", "t1"),
      toolUse("Edit", { file_path: "/work/repo/src/parser.ts" }, "t2"),
      toolResult("ok", "t2"),
      text("Fixed it."),
    ]),
  ];

  // The menu hangs off the first card of the run; the handoff covers all of it.
  const document = buildFromAssistant(turns, 0);

  assert.ok(document);
  assert.match(document, /<last-turn>\nLooking now\.\n\nFixed it\.\n\[tools: Edit, Read\]\n<\/last-turn>/);
  assert.match(document, /- Files edited: `\/work\/repo\/src\/parser\.ts`/);
  // Tool results still never travel with a handoff.
  assert.equal(document.includes("secret token abc123"), false);
});

test("rolled-back messages neither split nor extend an active assistant run", () => {
  const turns = [
    turn("user", [text("Start.")]),
    turn("assistant", [text("First part.")]),
    turn("user", [text("Discarded steer.")], { contextStatus: "rolledBack" }),
    turn("assistant", [text("Discarded answer.")], { contextStatus: "rolledBack" }),
    turn("assistant", [text("Active continuation.")]),
    turn("user", [text("Next request.")]),
  ];

  const document = buildFromAssistant(turns, 0);

  assert.ok(document);
  assert.match(document, /First part\.\n\nActive continuation\./);
  assert.equal(document.includes("Discarded steer."), false);
  assert.equal(document.includes("Discarded answer."), false);
  assert.equal(document.includes("Next request."), false);
});

test("marks an interrupted assistant anchor", () => {
  const turns = [
    turn("user", [text("Start.")]),
    turn("assistant", [text("Partial answer")], { status: "interrupted" }),
  ];

  const document = buildFromAssistant(turns, 0);

  assert.ok(document);
  assert.match(document, /## Where the previous agent left off \(interrupted\)/);
});

test("drops the anchor section when an assistant run said and did nothing", () => {
  const turns = [
    turn("user", [text("Start.")]),
    turn("assistant", [{ type: "raw", value: { type: "thinking", thinking: "just musing" } }]),
  ];

  const document = buildFromAssistant(turns, 0);

  assert.ok(document);
  assert.equal(document.includes("## Where the previous agent left off"), false);
  assert.equal(document.includes("just musing"), false);
  assert.match(document, /^Pick up from there: continue the work/m);
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

test("carries an oversized history message whole when the budget allows", () => {
  const long = "x".repeat(20_000);
  const request = "y".repeat(3_000);
  const turns = [
    turn("user", [text(long)]),
    turn("assistant", [text("Ack.")]),
    turn("user", [text(request)]),
  ];

  const document = build(turns, 1);

  assert.ok(document);
  // Nothing is trimmed to a per-message cap while the document as a whole fits.
  assert.equal(document.includes("characters omitted"), false);
  assert.ok(document.includes(long));
  assert.ok(document.includes(request));
});

test("keeps a long request intact", () => {
  const request = "y".repeat(20_000);
  const turns = [
    turn("user", [text("Start.")]),
    turn("assistant", [text("Ack.")]),
    turn("user", [text(request)]),
  ];

  const document = build(turns, 1);

  assert.ok(document);
  assert.ok(document.includes(request));
});

test("spends a tight budget on the last turns before the older ones", () => {
  const older = "a".repeat(5_000);
  const recentReply = "b".repeat(5_000);
  const recentAsk = "c".repeat(5_000);
  const turns = [
    turn("user", [text("The original task statement.")]),
    turn("assistant", [text(older)]),
    turn("user", [text("A middle steer.")]),
    turn("assistant", [text(recentReply)]),
    turn("user", [text(recentAsk)]),
    turn("user", [text("Final ask.")]),
  ];

  const document = build(turns, 2, {
    limits: {
      totalCharacters: 13_500,
      messageCharacters: 1_000,
      recentMessageCharacters: 8_000,
    },
  });

  assert.ok(document);
  // The last two turns of history survive whole; the older reply pays for them.
  assert.ok(document.includes(recentReply));
  assert.ok(document.includes(recentAsk));
  assert.equal(document.includes(older), false);
  assert.match(document, /\[… \d+ characters omitted …\]/);
  assert.ok(document.includes("The original task statement."));
  assert.ok(document.length <= 13_500);
});

test("elides the middle of a long conversation and says so", () => {
  const turns: Turn[] = [turn("user", [text("The original task statement.")])];
  for (let index = 0; index < 40; index += 1) {
    turns.push(turn("assistant", [text(`reply ${index} ${"z".repeat(8_000)}`)]));
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
  assert.ok(document.length <= 120_000);
  // The freshest reply is carried in full even though its peers were dropped or
  // trimmed: the drops buy budget, and recency decides who spends it.
  assert.ok(document.includes(`reply 39 ${"z".repeat(8_000)}`));
  assert.equal(document.includes(`reply 0 ${"z".repeat(8_000)}`), false);
});

test("lists more tool names for a recent run than for an older one", () => {
  const run = (prefix: string) =>
    Array.from({ length: 12 }, (_, index) =>
      toolUse(`${prefix}${String(index + 1).padStart(2, "0")}`, {}, `${prefix}-${index}`),
    );
  const turns = [
    turn("user", [text("Start.")]),
    turn("assistant", [text("Older work."), ...run("OldTool")]),
    turn("user", [text("Keep going.")]),
    turn("assistant", [text("Recent work."), ...run("NewTool")]),
    turn("user", [text("Final ask.")]),
  ];

  const document = build(turns, 2);

  assert.ok(document);
  assert.ok(document.includes("NewTool12"));
  assert.equal(document.includes("OldTool12"), false);
  assert.match(document, /\[tools: OldTool01(, OldTool\d\d){7}, \+4 more\]/);
});

test("truncates on line boundaries", () => {
  const lines = Array.from({ length: 600 }, (_, index) =>
    `line ${String(index).padStart(4, "0")} end`,
  ).join("\n");
  const turns = [
    turn("user", [text(lines)]),
    turn("assistant", [text("Ack.")]),
    turn("user", [text("Final ask.")]),
  ];

  const document = build(turns, 1, { limits: { messageCharacters: 1_000, recentTurns: 0, totalCharacters: 4_000 } });

  assert.ok(document);
  // Neither half of the cut lands mid-line.
  assert.match(document, / end\n\n\[… \d+ characters omitted …\]\n\nline \d{4} end/);
});

test("keeps a message that only just overruns its cap", () => {
  const barelyOver = "d".repeat(1_200);
  const turns = [
    turn("user", [text("The original task statement.")]),
    turn("assistant", [text(barelyOver)]),
    turn("user", [text("Keep going.")]),
    turn("assistant", [text("e".repeat(20_000))]),
    turn("user", [text("Final ask.")]),
  ];

  const document = build(turns, 1, {
    limits: { totalCharacters: 8_000, messageCharacters: 1_000, recentTurns: 0 },
  });

  assert.ok(document);
  // Cutting 200 characters would cost more in notice than it saves.
  assert.ok(document.includes(barelyOver));
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
