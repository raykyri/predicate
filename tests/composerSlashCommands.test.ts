import assert from "node:assert/strict";
import test from "node:test";
import {
  completeComposerSlashCommand,
  matchingComposerSlashCommands,
  parseComposerSlashCommand,
} from "../src/lib/composerSlashCommands";
import {
  composerSlashCommandSubmitLabels,
  planComposerSubmission,
} from "../src/lib/composerActions";

test("matches command prefixes only in the first unfinished token", () => {
  assert.deepEqual(
    matchingComposerSlashCommands("/").map((command) => command.name),
    ["fork", "worktree"],
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

test("treats the removed btw command as ordinary agent input", () => {
  assert.deepEqual(parseComposerSlashCommand("/btw answer this side question"), { kind: "none" });
  assert.deepEqual(parseComposerSlashCommand("/btw"), { kind: "none" });
  assert.deepEqual(matchingComposerSlashCommands("/b").map((command) => command.name), []);
});

test("labels immediate, now, and queued slash-command actions", () => {
  const fork = parseComposerSlashCommand("/fork investigate");
  const worktree = parseComposerSlashCommand("/worktree investigate");
  assert.equal(fork.kind, "ready");
  assert.equal(worktree.kind, "ready");
  if (fork.kind === "ready" && worktree.kind === "ready") {
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
