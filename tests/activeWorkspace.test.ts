import assert from "node:assert/strict";
import test from "node:test";
import {
  agentDisplayBranch,
  agentDisplayCheckoutRoot,
  agentDisplayDirectory,
  agentDisplayWorktreeRoot,
  agentEventAffectsThinkingState,
  agentShowsLaunchDirectory,
} from "../src/lib/appHelpers";
import type { AgentInfo } from "../src/types";

function agent(overrides: Partial<AgentInfo> = {}): AgentInfo {
  return {
    id: "agent-1",
    groupId: "group-1",
    adapter: "codex",
    worktreeDir: "/repo/.qmux/worktrees/agent-1",
    branch: "qmux/agent-1",
    status: "running",
    paused: false,
    createdAt: 1,
    ...overrides,
  };
}

test("workspace-only agent events do not affect thinking lifecycle", () => {
  assert.equal(agentEventAffectsThinkingState("agent.workspace_changed"), false);
  assert.equal(agentEventAffectsThinkingState("agent.running"), true);
  assert.equal(agentEventAffectsThinkingState("agent.done"), true);
});

test("live command cwd and branch override launch metadata for display", () => {
  const current = agent({
    activeWorkspace: {
      cwd: "/repo/other/packages/app",
      gitRoot: "/repo/other",
      branch: "feature/other",
      kind: "linkedWorktree",
      source: "codex",
      managedByQmux: false,
    },
  });

  assert.equal(agentDisplayDirectory(current, "/pane"), "/repo/other/packages/app");
  assert.equal(agentDisplayBranch(current), "feature/other");
  assert.equal(agentDisplayWorktreeRoot(current), "/repo/other");
});

test("main checkout does not masquerade as the Qmux launch worktree", () => {
  const current = agent({
    activeWorkspace: {
      cwd: "/repo/src",
      gitRoot: "/repo",
      branch: "main",
      kind: "mainCheckout",
      source: "claude",
      managedByQmux: false,
    },
  });

  assert.equal(agentDisplayBranch(current), "main");
  assert.equal(agentDisplayWorktreeRoot(current), null);
  assert.equal(agentDisplayCheckoutRoot(current), "/repo");
});

test("an observed branchless directory does not inherit the launch branch", () => {
  const current = agent({
    activeWorkspace: {
      cwd: "/tmp/output",
      gitRoot: null,
      branch: null,
      kind: "directory",
      source: "codex",
      managedByQmux: false,
    },
  });

  assert.equal(agentDisplayBranch(current), null);
  assert.equal(agentDisplayWorktreeRoot(current), null);
});

test("older agent payloads retain launch-directory fallbacks", () => {
  const legacy = agent();
  assert.equal(agentDisplayDirectory(legacy, "/pane"), legacy.worktreeDir);
  assert.equal(agentDisplayBranch(legacy), legacy.branch);
  assert.equal(agentDisplayWorktreeRoot(legacy), legacy.worktreeDir);
  assert.equal(agentDisplayCheckoutRoot(legacy), legacy.worktreeDir);
  assert.equal(agentDisplayDirectory(undefined, "/pane"), "/pane");
});

test("macOS /private/tmp aliases do not show a redundant Launch directory", () => {
  const current = agent({
    worktreeDir: "/tmp/qmux-worktree",
    activeWorkspace: {
      cwd: "/private/tmp/qmux-worktree",
      gitRoot: "/private/tmp/qmux-worktree",
      branch: "qmux/agent-1",
      kind: "linkedWorktree",
      source: "codex",
      managedByQmux: true,
    },
  });

  assert.equal(agentShowsLaunchDirectory(current), false);
  assert.equal(
    agentShowsLaunchDirectory(
      agent({
        worktreeDir: "/tmp/qmux-worktree/",
        activeWorkspace: {
          cwd: "/private/tmp/qmux-worktree",
          gitRoot: null,
          branch: null,
          kind: "directory",
          source: "codex",
          managedByQmux: false,
        },
      }),
    ),
    false,
  );
});

test("a nested command cwd still shows the launch directory", () => {
  const current = agent({
    worktreeDir: "/tmp/qmux-worktree",
    activeWorkspace: {
      cwd: "/tmp/qmux-worktree/packages/app",
      gitRoot: "/tmp/qmux-worktree",
      branch: "qmux/agent-1",
      kind: "linkedWorktree",
      source: "codex",
      managedByQmux: true,
    },
  });

  assert.equal(agentShowsLaunchDirectory(current), true);
});
