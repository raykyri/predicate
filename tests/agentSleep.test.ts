import assert from "node:assert/strict";
import test from "node:test";
import { agentStatusKeepsMachineAwake } from "../src/lib/appHelpers";
import type { AgentInfo } from "../src/types";

test("in-flight agent states keep the machine awake", () => {
  for (const status of ["starting", "running", "awaitingPermission"] as const) {
    assert.equal(agentStatusKeepsMachineAwake(status), true, status);
  }
});

test("settled and user-blocked agent states allow normal sleep", () => {
  for (const status of ["awaitingInput", "done", "idle", "failed"] as const) {
    assert.equal(agentStatusKeepsMachineAwake(status), false, status);
  }
});

test("an unexpected future status retains the wake lock", () => {
  const futureStatus = "pausing" as AgentInfo["status"];
  assert.equal(agentStatusKeepsMachineAwake(futureStatus), true);
});
