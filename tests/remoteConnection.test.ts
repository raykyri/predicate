import assert from "node:assert/strict";
import test from "node:test";
import { parseRemoteConnection, remoteConnectionLabel, remoteConnectionDetails, remoteGroupStatus, remoteHooksNeedAttention, remoteConnectionPresentation } from "../src/lib/remoteConnection";
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
  assert.doesNotMatch(remoteConnectionDetails({ state: "reconnecting" }), /status is unknown/);
  const ended = { state: "failed" as const, stage: "sessionEnded", sessionExists: false };
  assert.equal(remoteConnectionLabel(ended), "Session ended");
  assert.doesNotMatch(remoteConnectionDetails({ ...ended, attempt: 1,
    message: "The remote session has ended. It will not be recreated automatically." }),
    /Attempt|session has ended|recreated automatically|status is unknown/);
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

test("retry copy prioritizes errors and counts down without stale timing on failures", () => {
  const now = 1_000_000;
  const connection = { state: "reconnecting" as const, stage: "waitingToRetry", reason: "appRestart",
    attempt: 2, nextRetryAt: now + 5000, lastConnectedAt: now - 300000,
    message: "check remote session timed out" };
  assert.equal(remoteConnectionLabel(connection), "Restoring session");
  assert.equal(remoteConnectionDetails(connection, now),
    "Reconnecting... (retrying in 5 sec)\nConnection timed out\nLast connection: 5 min ago");
  assert.match(remoteConnectionDetails(connection, now + 2000), /retrying in 3 sec/);
  assert.match(remoteConnectionDetails(connection, now + 6000), /Waiting to retry/);
  const failed = { ...connection, state: "failed" as const, stage: "needsAttention", message: "Permission denied (publickey)." };
  assert.equal(remoteConnectionDetails(failed, now), "SSH authentication failed\nLast connection: 5 min ago");
  assert.equal(remoteConnectionDetails({ ...failed, stage: "sessionEnded", sessionExists: false }, now), "Last connection: 5 min ago");
});

test("progress and fallback copy avoids redundant explanations and zero-minute ages", () => {
  const now = 1_000_000;
  assert.equal(remoteConnectionDetails(null), "");
  assert.equal(remoteConnectionDetails({ state: "checking", reason: "manualRetry" }), "");
  assert.equal(remoteConnectionDetails({ state: "connecting", reason: "initialConnection" }), "Establishing connection...");
  assert.equal(remoteConnectionDetails({ state: "checking", reason: "systemWake" }), "Checking connection...");
  assert.equal(remoteConnectionDetails({ state: "disconnected", stage: "sleeping", reason: "systemSleep" }), "Waiting for wake signal...");
  for (const [stage, step] of [["configuring", "Preparing session"], ["restoringHistory", "Restoring history"], ["attaching", "Reattaching"]]) {
    const connection = { state: "reconnecting" as const, stage, reason: "appRestart", sessionExists: true };
    assert.equal(remoteConnectionLabel(connection), "Restoring session");
    assert.equal(remoteConnectionDetails(connection), `${step}...`);
  }
  for (const reason of ["initialConnection", "appRestart", "systemWake"]) {
    assert.equal(remoteConnectionDetails({ state: "failed", stage: "needsAttention", reason, message: "Host key verification failed." }), "SSH host key verification failed");
  }
  assert.equal(remoteConnectionDetails({ state: "failed", message: "Unable to establish the remote connection." }), "");
  assert.equal(remoteConnectionDetails({ state: "disconnected", lastConnectedAt: now - 59000 }, now), "Last connection: just now");
  assert.equal(remoteConnectionDetails({ state: "disconnected", lastConnectedAt: now - 60000 }, now), "Last connection: 1 min ago");
});

test("recovery reasons share steps and return timestamp metadata separately", () => {
  const now = 1_000_000;
  for (const [reason, title] of [
    ["initialConnection", "Connecting"], ["appRestart", "Restoring session"],
    ["connectionLost", "Restoring lost connection"], ["systemWake", "Resuming after sleep"],
  ]) {
    for (const [state, stage, step] of [
      ["connecting", undefined, "Connecting"], ["checking", "checking", "Checking connection"],
      ["reconnecting", "configuring", "Preparing session"], ["reconnecting", "restoringHistory", "Restoring history"],
      ["reconnecting", "attaching", "Reattaching"], ["reconnecting", "waitingToRetry", "Reconnecting"],
    ] as const) {
      const connection = { state, stage, reason, attempt: 3, lastConnectedAt: now - 300000 };
      const view = remoteConnectionPresentation(connection, now);
      const description = reason === "initialConnection" && state === "connecting" ? "Establishing connection" : step;
      assert.deepEqual(view, { title, lines: [`${description}...`], lastConnection: "Last connection: 5 min ago", refreshEveryMs: 1000 });
      assert.equal(remoteConnectionDetails(connection, now), `${description}...\nLast connection: 5 min ago`);
    }
  }
});

test("credential titles, combined retry countdowns, and final states have explicit precedence", () => {
  const now = 1_000_000;
  const retry = { state: "reconnecting" as const, stage: "waitingToRetry", reason: "appRestart",
    message: "could not recover remote hook credential", nextRetryAt: now + 5000 };
  assert.deepEqual(remoteConnectionPresentation(retry, now), {
    title: "Could not restore agent authentication", lines: ["Reconnecting... (retrying in 5 sec)"],
    lastConnection: null, refreshEveryMs: 1000,
  });
  assert.deepEqual(remoteConnectionPresentation({ ...retry, stage: "sessionEnded", sessionExists: false }, now), {
    title: "Session ended", lines: [], lastConnection: null, refreshEveryMs: null,
  });
  assert.deepEqual(remoteConnectionPresentation({ ...retry, state: "disconnected", stage: "sleeping" }, now), {
    title: "Sleeping", lines: ["Waiting for wake signal..."], lastConnection: null, refreshEveryMs: null,
  });
  const failed = remoteConnectionPresentation({ ...retry, state: "failed", stage: "needsAttention" }, now);
  assert.equal(failed.title, "Connection failed");
  assert.deepEqual(failed.lines, ["Could not restore agent authentication"]);
  assert.equal(failed.refreshEveryMs, null);
  assert.equal(remoteConnectionPresentation({ state: "checking", reason: "__proto__", stage: "constructor" }).title, "Checking connection");
});
