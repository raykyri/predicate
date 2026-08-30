import assert from "node:assert/strict";
import test from "node:test";
import {
  EMPTY_REMOTE_STATUS,
  formatPairingCode,
  formatPairingCountdown,
  formatRemoteRelativeTime,
  middleTruncate,
  pairingRemainingMs,
  remoteButtonIndicator,
  remoteDeviceStatusLine,
  remotePopoverSections,
  remoteSessionStatusLine,
  remoteStatusSummary,
  sortRemoteDevices,
  type RemoteDevice,
  type RemoteStatus,
} from "../src/lib/remoteControl";

const NOW = 1_700_000_000_000;

const status = (overrides: Partial<RemoteStatus> = {}): RemoteStatus => ({
  ...EMPTY_REMOTE_STATUS,
  ...overrides,
});

const device = (overrides: Partial<RemoteDevice> = {}): RemoteDevice => ({
  endpointId: "endpoint-1",
  name: "Phone",
  pairedAt: NOW - 86_400_000,
  lastSeen: NOW - 60_000,
  readOnly: false,
  connected: false,
  ...overrides,
});

const session = (endpointId: string) => ({
  endpointId,
  deviceName: "Phone",
  connectedAt: NOW - 120_000,
});

test("summarizes the header line for every reach and connection state", () => {
  assert.equal(remoteStatusSummary(status()), "Off");
  // Off wins over a stale reach: nothing is listening either way.
  assert.equal(remoteStatusSummary(status({ reach: "anywhere" })), "Off");
  assert.equal(
    remoteStatusSummary(status({ enabled: true })),
    "On · this network only",
  );
  assert.equal(
    remoteStatusSummary(status({ enabled: true, reach: "anywhere" })),
    "On · reachable anywhere",
  );
});

test("live sessions outrank reach in the header line, and pluralize", () => {
  assert.equal(
    remoteStatusSummary(status({ enabled: true, sessions: [session("a")] })),
    "1 device connected",
  );
  assert.equal(
    remoteStatusSummary(
      status({ enabled: true, reach: "anywhere", sessions: [session("a"), session("b")] }),
    ),
    "2 devices connected",
  );
});

test("orders devices by connection, then recency, then name", () => {
  const ordered = sortRemoteDevices([
    device({ endpointId: "stale", name: "Old iPad", lastSeen: NOW - 8_640_000_000 }),
    device({ endpointId: "never-b", name: "Bravo", lastSeen: null }),
    device({ endpointId: "recent", name: "Zulu phone", lastSeen: NOW - 60_000 }),
    device({ endpointId: "live", name: "Zeta phone", connected: true, lastSeen: null }),
    device({ endpointId: "never-a", name: "Alpha", lastSeen: undefined }),
  ]);
  assert.deepEqual(
    ordered.map((entry) => entry.endpointId),
    ["live", "recent", "stale", "never-a", "never-b"],
  );
});

test("sorting a device list leaves the input untouched", () => {
  const input = [
    device({ endpointId: "b", name: "B", lastSeen: NOW - 10_000 }),
    device({ endpointId: "a", name: "A", connected: true }),
  ];
  const sorted = sortRemoteDevices(input);
  assert.deepEqual(
    input.map((entry) => entry.endpointId),
    ["b", "a"],
  );
  assert.deepEqual(
    sorted.map((entry) => entry.endpointId),
    ["a", "b"],
  );
});

test("formats relative times from the clock it is handed", () => {
  assert.equal(formatRemoteRelativeTime(NOW - 10_000, NOW), "just now");
  assert.equal(formatRemoteRelativeTime(NOW - 300_000, NOW), "5 min ago");
  assert.equal(formatRemoteRelativeTime(NOW - 7_200_000, NOW), "2 hr ago");
  assert.equal(formatRemoteRelativeTime(NOW - 86_400_000, NOW), "1 day ago");
  assert.equal(formatRemoteRelativeTime(NOW - 3 * 86_400_000, NOW), "3 days ago");
  assert.equal(formatRemoteRelativeTime(NOW - 10 * 86_400_000, NOW), "1 wk ago");
  assert.equal(formatRemoteRelativeTime(NOW - 60 * 86_400_000, NOW), "2 mo ago");
  assert.equal(formatRemoteRelativeTime(NOW - 800 * 86_400_000, NOW), "2 yr ago");
});

test("writes the device and session status lines", () => {
  assert.equal(
    remoteDeviceStatusLine(device({ connected: true, lastSeen: NOW - 600_000 }), NOW),
    "Connected · direct",
  );
  assert.equal(
    remoteDeviceStatusLine(device({ lastSeen: NOW - 600_000 }), NOW),
    "Last seen 10 min ago",
  );
  assert.equal(remoteDeviceStatusLine(device({ lastSeen: null }), NOW), "Never connected");
  assert.equal(
    remoteDeviceStatusLine(device({ lastSeen: undefined }), NOW),
    "Never connected",
  );
  assert.equal(remoteSessionStatusLine(session("a"), NOW), "Connected 2 min ago");
});

test("groups the pairing code in fours and counts the window down", () => {
  assert.equal(formatPairingCode("k7m2p4qx3d"), "K7M2-P4QX-3D");
  assert.equal(formatPairingCode("K7M2-P4QX-3D"), "K7M2-P4QX-3D");
  assert.equal(formatPairingCode(""), "");
  assert.equal(pairingRemainingMs(NOW + 90_000, NOW), 90_000);
  assert.equal(pairingRemainingMs(NOW - 5_000, NOW), 0);
  assert.equal(formatPairingCountdown(180_000), "3:00");
  assert.equal(formatPairingCountdown(65_400), "1:06");
  assert.equal(formatPairingCountdown(0), "0:00");
});

test("middle-truncates only keys longer than the budget", () => {
  assert.equal(middleTruncate("short-key"), "short-key");
  assert.equal(
    middleTruncate("aaaaaaaaaabbbbbbbbbbccccccccccdddd"),
    "aaaaaaaaaa…ccccdddd",
  );
  assert.equal(middleTruncate("abcdefghijklmnop", 4, 4), "abcd…mnop");
});

test("hides everything but the explainer and the launch toggle while off", () => {
  assert.deepEqual(
    remotePopoverSections(status({ devices: [device()] }), {
      pairingOpen: false,
      reachConfirmOpen: false,
    }),
    {
      offExplainer: true,
      modeToggle: false,
      reachConfirm: false,
      pairButton: false,
      pairingPanel: false,
      devices: false,
      devicesEmpty: false,
      sessions: false,
      launchToggle: true,
    },
  );
});

test("swaps the pair button for the pairing panel, and shows live sections", () => {
  const on = status({
    enabled: true,
    devices: [device({ connected: true })],
    sessions: [session("endpoint-1")],
  });
  const idle = remotePopoverSections(on, { pairingOpen: false, reachConfirmOpen: false });
  assert.equal(idle.offExplainer, false);
  assert.equal(idle.modeToggle, true);
  assert.equal(idle.pairButton, true);
  assert.equal(idle.pairingPanel, false);
  assert.equal(idle.devices, true);
  assert.equal(idle.devicesEmpty, false);
  assert.equal(idle.sessions, true);
  assert.equal(idle.reachConfirm, false);

  const pairing = remotePopoverSections(on, { pairingOpen: true, reachConfirmOpen: true });
  assert.equal(pairing.pairButton, false);
  assert.equal(pairing.pairingPanel, true);
  assert.equal(pairing.reachConfirm, true);
});

test("an enabled endpoint with no devices shows the empty state, not the list", () => {
  const sections = remotePopoverSections(status({ enabled: true }), {
    pairingOpen: false,
    reachConfirmOpen: false,
  });
  assert.equal(sections.devices, false);
  assert.equal(sections.devicesEmpty, true);
  assert.equal(sections.sessions, false);
  // The reach confirm is a local consent step; it never appears unprompted.
  assert.equal(sections.reachConfirm, false);
});

test("the sidebar button reads as on whether or not its popover is open", () => {
  assert.deepEqual(remoteButtonIndicator(status(), false), {
    active: false,
    sessionDot: false,
  });
  assert.deepEqual(remoteButtonIndicator(status(), true), {
    active: true,
    sessionDot: false,
  });
  assert.deepEqual(remoteButtonIndicator(status({ enabled: true }), false), {
    active: true,
    sessionDot: false,
  });
  assert.deepEqual(
    remoteButtonIndicator(status({ enabled: true, sessions: [session("a")] }), false),
    { active: true, sessionDot: true },
  );
});
