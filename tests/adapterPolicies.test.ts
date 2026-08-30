import assert from "node:assert/strict";
import test from "node:test";
import { composerPolicyFor, hydrateAdapterPolicies } from "../src/adapters";
import type { AgentAdapterMetadata, AgentComposerPolicy } from "../src/types";

const policy = (overrides: Partial<AgentComposerPolicy> = {}): AgentComposerPolicy => ({
  readyStatuses: ["awaitingInput", "done", "idle"],
  queueStatuses: ["starting", "running", "awaitingPermission"],
  steerStatuses: ["starting", "running"],
  permissionActions: [],
  ...overrides,
});

const metadata = (id: string, composerPolicy: AgentComposerPolicy): AgentAdapterMetadata => ({
  id,
  label: id,
  default: id === "claude",
  supportsFork: true,
  supportsForkAtMessage: false,
  composerPolicy,
});

test("before hydration the fallback carries no permission actions", () => {
  // A button that appears a frame late beats one wired to the wrong
  // keystroke: the pre-hydration policy must never invent permission input.
  const fallback = composerPolicyFor("claude");
  assert.deepEqual(fallback.permissionActions, []);
  assert.ok(fallback.readyStatuses.includes("awaitingInput"));
});

test("hydration makes the backend's tables the source, with the Claude fallback", () => {
  hydrateAdapterPolicies([
    metadata(
      "claude",
      policy({
        permissionActions: [
          { id: "approve", label: "Approve", input: "y" },
          { id: "deny", label: "Deny", input: "n" },
        ],
      }),
    ),
    metadata("codex", policy()),
  ]);

  assert.equal(composerPolicyFor("claude").permissionActions[0]?.input, "y");
  assert.deepEqual(composerPolicyFor("codex").permissionActions, []);
  // Unknown adapters mirror getAgentUiAdapter's fallback to Claude, so a
  // stale frontend renders Claude's buttons rather than none.
  assert.equal(composerPolicyFor("brand-new").permissionActions.length, 2);
  assert.equal(composerPolicyFor(null).permissionActions.length, 2);
});
