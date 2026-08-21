import assert from "node:assert/strict";
import test from "node:test";
import {
  agentStatusKeepsMachineAwake,
  desiredPreventSleepState,
} from "../src/lib/appHelpers";
import type { AgentInfo } from "../src/types";

test("in-flight agent states keep the machine awake", () => {
  for (const status of [
    "starting",
    "running",
    "awaitingPermission",
    "awaitingInput",
  ] as const) {
    assert.equal(agentStatusKeepsMachineAwake(status), true, status);
  }
});

test("settled agent states allow normal sleep", () => {
  for (const status of ["done", "idle", "failed"] as const) {
    assert.equal(agentStatusKeepsMachineAwake(status), false, status);
  }
});

test("an unexpected future status retains the wake lock", () => {
  const futureStatus = "pausing" as AgentInfo["status"];
  assert.equal(agentStatusKeepsMachineAwake(futureStatus), true);
});

test("agent hydration preserves an existing backend wake lock during reload", () => {
  assert.equal(desiredPreventSleepState(false, true, false), null);
  assert.equal(desiredPreventSleepState(false, false, false), null);
});

test("hydrated agent state drives the backend wake lock", () => {
  assert.equal(desiredPreventSleepState(true, true, true), true);
  assert.equal(desiredPreventSleepState(true, true, false), false);
  assert.equal(desiredPreventSleepState(true, false, true), false);
});
