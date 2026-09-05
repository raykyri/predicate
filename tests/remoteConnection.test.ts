import assert from "node:assert/strict";
import test from "node:test";
import { parseRemoteConnection, remoteConnectionLabel, remoteConnectionDetails, remoteGroupStatus, remoteHooksNeedAttention } from "../src/lib/remoteConnection";
import type { PaneInfo } from "../src/types";

test("events retain recovery metadata and reject invalid states and timestamps", () => {
  const parsed = parseRemoteConnection({ state: "checking", reason: "systemWake", attempt: 3,
    nextRetryAt: 1000, lastConnectedAt: 500, stage: "checking", sessionExists: false });
  assert.equal(parsed?.reason, "systemWake");
  assert.equal(parsed?.attempt, 3);
  assert.equal(parsed?.nextRetryAt, 1000);
  assert.equal(parsed?.lastConnectedAt, 500);
  assert.equal(parsed?.sessionExists, false);
  assert.equal(parseRemoteConnection({ state: "unknown" }), null);
  assert.equal(parseRemoteConnection({ state: "connected", lastConnectedAt: NaN })?.lastConnectedAt, undefined);
});

test("unreachable and ended sessions never promise that work is still running", () => {
  assert.match(remoteConnectionDetails({ state: "reconnecting" }), /status is unknown/);
  const ended = { state: "failed" as const, stage: "sessionEnded", sessionExists: false };
  assert.equal(remoteConnectionLabel(ended), "Session ended");
  assert.match(remoteConnectionDetails(ended), /not be recreated automatically/);
  assert.doesNotMatch(remoteConnectionDetails(ended), /still running/);
});

test("group status reflects mixed health and contains no remote label or separator", () => {
  const pane = (state: "connected" | "reconnecting") => ({
    remoteSession: { remoteId: "private-host-name" }, remoteConnection: { state },
  }) as PaneInfo;
  assert.equal(remoteGroupStatus([pane("connected"), pane("reconnecting")])?.label, "1 of 2 connected");
  assert.equal(remoteGroupStatus([pane("connected")])?.label, "Connected");
  assert.equal(remoteGroupStatus([pane("reconnecting")])?.label, "Reconnecting");
  assert.equal(remoteGroupStatus([]), null);
});

test("restoration details include wake cause, verification time, and recovery duration", () => {
  const detail = remoteConnectionDetails({ state: "connected", reason: "systemWake",
    lastConnectedAt: 1000, lastVerifiedAt: 3000, recoveryDurationMs: 1200 });
  assert.match(detail, /verified after sleep/);
  assert.match(detail, /Attached:/);
  assert.match(detail, /Verified:/);
  assert.match(detail, /1.2 seconds/);
});

test("hook failures stay connected and surface in pane and group status", () => {
  const connection = parseRemoteConnection({ state: "connected", hookHealth: "authenticationFailed" })!;
  assert.equal(connection.state, "connected");
  assert.equal(remoteHooksNeedAttention(connection), true);
  assert.equal(remoteConnectionLabel(connection), "Connected · hooks need attention");
  assert.match(remoteConnectionDetails(connection), /invalid QMUX_TOKEN/);
  assert.match(remoteConnectionDetails(connection), /terminal remains usable/);
  const panes = ["healthy", "authenticationFailed"].map(hookHealth => ({
    remoteSession: { remoteId: "r" }, remoteConnection: parseRemoteConnection({ state: "connected", hookHealth }),
  }) as PaneInfo);
  assert.equal(remoteGroupStatus(panes)?.label, "Connected · hooks need attention");
  assert.match(remoteGroupStatus(panes)!.detail, /2 of 2 connected/);
  assert.match(remoteConnectionDetails({ state: "connected", hookHealth: "unavailable" }), /could not be verified/);
  assert.equal(remoteHooksNeedAttention({ state: "connected", hookHealth: "checking" }), false);
  assert.equal(remoteHooksNeedAttention({ state: "connected", hookHealth: "healthy" }), false);
  assert.equal(remoteHooksNeedAttention({ state: "reconnecting", hookHealth: "authenticationFailed" }), false);
  assert.equal(parseRemoteConnection({ state: "connected", hookHealth: "bogus" })?.hookHealth, undefined);
});
