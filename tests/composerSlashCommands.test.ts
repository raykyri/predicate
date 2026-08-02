import assert from "node:assert/strict";
import test from "node:test";
import {
  completeComposerSlashCommand,
  matchingComposerSlashCommands,
  parseComposerSlashCommand,
} from "../src/lib/composerSlashCommands";
import {
  BTW_SAFETY_INSTRUCTION,
  composerSlashCommandSubmitLabels,
  planComposerSubmission,
} from "../src/lib/composerActions";

test("matches command prefixes only in the first unfinished token", () => {
  assert.deepEqual(
    matchingComposerSlashCommands("/").map((command) => command.name),
    ["fork", "worktree", "btw"],
  );
  assert.deepEqual(
    matchingComposerSlashCommands("/f").map((command) => command.name),
    ["fork"],
  );
  assert.deepEqual(
    matchingComposerSlashCommands("/w").map((command) => command.name),
    ["worktree"],
  );
  assert.deepEqual(matchingComposerSlashCommands("/l"), []);
  assert.deepEqual(matchingComposerSlashCommands("/fork "), []);
  assert.deepEqual(matchingComposerSlashCommands("prefix /fork"), []);
  assert.deepEqual(matchingComposerSlashCommands("/unknown"), []);
});

test("completes a selected command with a message separator", () => {
  const [fork] = matchingComposerSlashCommands("/f");
  assert.equal(completeComposerSlashCommand(fork), "/fork ");
});

test("parses fork commands and strips only the qmux command prefix", () => {
  assert.deepEqual(parseComposerSlashCommand("/fork investigate this"), {
    kind: "ready",
    command: {
      name: "fork",
      token: "/fork",
      description: "Fork this session and send the following message",
      kind: "fork",
      useWorktree: false,
    },
    prompt: "investigate this",
  });
  const parsed = parseComposerSlashCommand("/worktree\t first line\nsecond line ");
  assert.equal(parsed.kind, "ready");
  if (parsed.kind === "ready") {
    assert.equal(parsed.command.useWorktree, true);
    assert.equal(parsed.prompt, "first line\nsecond line");
  }
});

test("treats the removed loop command as ordinary agent input", () => {
  assert.deepEqual(parseComposerSlashCommand("/loop keep fixing the tests"), { kind: "none" });
  assert.deepEqual(parseComposerSlashCommand("/loop"), { kind: "none" });
});

test("parses btw as a side-branch command", () => {
  const parsed = parseComposerSlashCommand("/btw answer this side question");
  assert.equal(parsed.kind, "ready");
  if (parsed.kind === "ready") {
    assert.equal(parsed.command.kind, "btw");
    assert.equal(parsed.prompt, "answer this side question");
  }
  assert.equal(parseComposerSlashCommand("/btw").kind, "incomplete");
  assert.deepEqual(
    matchingComposerSlashCommands("/b").map((command) => command.name),
    ["btw"],
  );
});

test("plans btw as an immediate fork prompt with the safety instruction", () => {
  assert.equal(
    BTW_SAFETY_INSTRUCTION,
    [
      '<qmux_instruction source="agent_driver">',
      "Do not change the working tree or codebase unless explicitly instructed to.",
      "</qmux_instruction>",
    ].join("\n"),
  );
  const parsed = parseComposerSlashCommand("/btw inspect the failing request");
  assert.deepEqual(planComposerSubmission(parsed, true), {
    kind: "btw",
    prompt: `${BTW_SAFETY_INSTRUCTION}\n\ninspect the failing request`,
    titlePrompt: "inspect the failing request",
  });
  assert.deepEqual(planComposerSubmission(parseComposerSlashCommand("/btw   "), true), {
    kind: "reject",
    message: "Add a message after /btw",
  });
});

test("labels immediate, now, and queued slash-command actions", () => {
  const fork = parseComposerSlashCommand("/fork investigate");
  const worktree = parseComposerSlashCommand("/worktree investigate");
  const btw = parseComposerSlashCommand("/btw investigate");
  assert.equal(fork.kind, "ready");
  assert.equal(worktree.kind, "ready");
  assert.equal(btw.kind, "ready");
  if (fork.kind === "ready" && worktree.kind === "ready" && btw.kind === "ready") {
    assert.deepEqual(composerSlashCommandSubmitLabels(fork.command), {
      immediate: "Fork & send",
      now: "Fork now",
      queued: "Queue fork",
    });
    assert.deepEqual(composerSlashCommandSubmitLabels(worktree.command), {
      immediate: "Fork in worktree & send",
      now: "Worktree now",
      queued: "Queue worktree",
    });
    assert.deepEqual(composerSlashCommandSubmitLabels(btw.command), {
      immediate: "Fork below & send now",
      now: "BTW now",
      queued: "Queue BTW",
    });
  }
});

test("recognizes known commands without a message as incomplete", () => {
  assert.equal(parseComposerSlashCommand("/fork").kind, "incomplete");
  assert.equal(parseComposerSlashCommand("/fork   ").kind, "incomplete");
  assert.equal(parseComposerSlashCommand("/worktree\t").kind, "incomplete");
});

test("leaves unknown, embedded, and lookalike slash commands alone", () => {
  for (const value of [
    "/compact now",
    "/forked now",
    "/Fork now",
    " /fork now",
    "explain /fork now",
    "/fork\nnow",
  ]) {
    assert.deepEqual(parseComposerSlashCommand(value), { kind: "none" }, value);
  }
});
