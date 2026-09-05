import assert from "node:assert/strict";
import test from "node:test";
import { trackRemoteStartup, recordRemoteStartup, forgetRemoteStartup } from "../src/lib/remoteStartup";

test("startup diagnostics measure from request and accept readiness before paint", () => {
  trackRemoteStartup("p", 100);
  assert.equal(recordRemoteStartup("p", "reserved", 110), 10);
  assert.equal(recordRemoteStartup("p", "ready", 160), 60);
  assert.equal(recordRemoteStartup("p", "ready", 170), null);
  assert.equal(recordRemoteStartup("p", "visible", 180), 80);
  assert.equal(recordRemoteStartup("p", "visible", 190), null);
});

test("cancelled and evicted launches cannot accumulate diagnostics", () => {
  trackRemoteStartup("closed", 0);
  forgetRemoteStartup("closed");
  assert.equal(recordRemoteStartup("closed", "ready", 1), null);
  for (let index = 0; index < 129; index++) trackRemoteStartup(`p-${index}`, 0);
  assert.equal(recordRemoteStartup("p-0", "ready", 1), null);
  for (let index = 1; index < 129; index++) forgetRemoteStartup(`p-${index}`);
});

test("a completion before pane insertion overrides the stale reservation", async () => {
  const { rememberRemoteStartupConnection, reconcileRemoteReservation, forgetRemotePane } = await import("../src/lib/remoteStartup");
  const pending = { id: "early", remoteConnection: { state: "connecting", stage: "starting" } } as import("../src/types").PaneInfo;
  rememberRemoteStartupConnection("early", { state: "failed", stage: "launchFailed", message: "unreachable" });
  assert.equal(reconcileRemoteReservation(pending).remoteConnection?.state, "failed");
  rememberRemoteStartupConnection("early", { state: "connected" });
  assert.equal(reconcileRemoteReservation(pending).remoteConnection?.state, "connected");
  const later = { ...pending, remoteConnection: { state: "reconnecting" as const } };
  assert.equal(reconcileRemoteReservation(later), later);
  forgetRemotePane("early");
  assert.equal(reconcileRemoteReservation(pending), pending);
});
