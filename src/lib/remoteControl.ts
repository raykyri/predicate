// Remote control: the shapes the backend serves and the pure display logic the
// popover renders from them. Everything here is a function of its arguments —
// `now` is always passed in — so the sections, ordering, and copy can be tested
// without a DOM or a running endpoint.

/** How far the endpoint reaches. `local` is mDNS only; `anywhere` adds relays. */
export type RemoteReach = "local" | "anywhere";

/** One paired device, plus whether it currently holds a session. */
export interface RemoteDevice {
  endpointId: string;
  name: string;
  pairedAt: number;
  lastSeen?: number | null;
  readOnly: boolean;
  connected: boolean;
}

/** One live session, keyed by the endpoint that opened it. */
export interface RemoteSession {
  endpointId: string;
  deviceName: string;
  connectedAt: number;
}

/** A pairing attempt waiting on the approval dialog. */
export interface RemotePendingPair {
  requestId: string;
  deviceName: string;
  endpointId: string;
}

/** Everything the popover renders, as served by `remote_status_get`. */
export interface RemoteStatus {
  enabled: boolean;
  reach: RemoteReach;
  launchEnabled: boolean;
  /** This Mac's endpoint id while enabled. */
  endpointId: string | null;
  devices: RemoteDevice[];
  sessions: RemoteSession[];
  pendingPair: RemotePendingPair | null;
}

/** What `remote_pairing_begin` returns: the invite plus its rendered QR. */
export interface RemotePairingPanel {
  payload: string;
  code: string;
  expiresInMs: number;
  qrSvg: string;
}

/** The `remote.*` slice of the qmux-event channel. */
export const REMOTE_EVENT_PREFIX = "remote.";

/** The status a closed popover shows before the first round-trip lands. */
export const EMPTY_REMOTE_STATUS: RemoteStatus = {
  enabled: false,
  reach: "local",
  launchEnabled: false,
  endpointId: null,
  devices: [],
  sessions: [],
  pendingPair: null,
};

/**
 * The line under the popover title. Live sessions outrank reach: while a phone
 * is driving the Mac that is the fact worth reading, and the reach is still one
 * glance away in the mode toggle.
 */
export function remoteStatusSummary(status: RemoteStatus): string {
  if (!status.enabled) {
    return "Off";
  }
  const connected = status.sessions.length;
  if (connected > 0) {
    return `${connected} device${connected === 1 ? "" : "s"} connected`;
  }
  return status.reach === "anywhere" ? "On · reachable anywhere" : "On · this network only";
}

/**
 * Paired devices in reading order: whoever is connected right now, then the
 * most recently seen, then alphabetically so a list of never-connected devices
 * is still stable across refreshes.
 */
export function sortRemoteDevices(devices: readonly RemoteDevice[]): RemoteDevice[] {
  return [...devices].sort((left, right) => {
    if (left.connected !== right.connected) {
      return left.connected ? -1 : 1;
    }
    const leftSeen = left.lastSeen ?? 0;
    const rightSeen = right.lastSeen ?? 0;
    if (leftSeen !== rightSeen) {
      return rightSeen - leftSeen;
    }
    return left.name.localeCompare(right.name);
  });
}

/**
 * Coarse "x ago" label, in the shape the transcript session list uses, with the
 * clock passed in so the countdown and the device list can be tested.
 */
export function formatRemoteRelativeTime(timestampMs: number, now: number): string {
  const diffMs = now - timestampMs;
  if (diffMs < 45_000) {
    return "just now";
  }
  const minutes = Math.floor(diffMs / 60_000);
  if (minutes < 60) {
    return `${minutes} min ago`;
  }
  const hours = Math.floor(minutes / 60);
  if (hours < 24) {
    return `${hours} hr ago`;
  }
  const days = Math.floor(hours / 24);
  if (days < 7) {
    return `${days} day${days === 1 ? "" : "s"} ago`;
  }
  if (days < 30) {
    return `${Math.floor(days / 7)} wk ago`;
  }
  if (days < 365) {
    return `${Math.floor(days / 30)} mo ago`;
  }
  return `${Math.floor(days / 365)} yr ago`;
}

/**
 * The status line under a paired device's name. A connected device names its
 * transport so "on" is never ambiguous; anything else reports when it was last
 * here, or that it has never arrived.
 */
export function remoteDeviceStatusLine(device: RemoteDevice, now: number): string {
  if (device.connected) {
    return "Connected · direct";
  }
  if (device.lastSeen == null) {
    return "Never connected";
  }
  return `Last seen ${formatRemoteRelativeTime(device.lastSeen, now)}`;
}

/** The status line beside a live session: how long it has been driving. */
export function remoteSessionStatusLine(session: RemoteSession, now: number): string {
  return `Connected ${formatRemoteRelativeTime(session.connectedAt, now)}`;
}

/**
 * The pairing code, grouped for reading aloud: uppercase, non-alphanumerics
 * dropped, a dash every four characters ("K7M2-P4QX-3D").
 */
export function formatPairingCode(code: string): string {
  const cleaned = code.replace(/[^0-9a-zA-Z]/g, "").toUpperCase();
  return (cleaned.match(/.{1,4}/g) ?? []).join("-");
}

/** Milliseconds left on a pairing window, floored at zero. */
export function pairingRemainingMs(expiresAt: number, now: number): number {
  return Math.max(0, expiresAt - now);
}

/** The countdown beside the QR, as `m:ss`. */
export function formatPairingCountdown(remainingMs: number): string {
  const totalSeconds = Math.max(0, Math.ceil(remainingMs / 1000));
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${minutes}:${String(seconds).padStart(2, "0")}`;
}

/**
 * Middle-truncates an endpoint key so both ends stay comparable against what
 * the phone shows. Short values are returned untouched.
 */
export function middleTruncate(value: string, head = 10, tail = 8): string {
  if (value.length <= head + tail + 1) {
    return value;
  }
  return `${value.slice(0, head)}…${value.slice(value.length - tail)}`;
}

/** Local, non-persisted popover state the section reducer reads. */
export interface RemotePopoverUiState {
  /** A pairing window has been opened from this popover. */
  pairingOpen: boolean;
  /** "Anywhere" was clicked and is waiting on its separate consent. */
  reachConfirmOpen: boolean;
}

/** Which parts of the popover are on screen for a given status + UI state. */
export interface RemotePopoverSections {
  offExplainer: boolean;
  modeToggle: boolean;
  reachConfirm: boolean;
  pairButton: boolean;
  pairingPanel: boolean;
  devices: boolean;
  devicesEmpty: boolean;
  sessions: boolean;
  launchToggle: boolean;
}

/**
 * Off means absent, and the popover says so with copy rather than a page of
 * disabled controls: nothing below the master switch applies until something is
 * listening. "Turn on when qmux launches" is the exception — it is the one
 * setting that is only useful while the feature is off.
 */
export function remotePopoverSections(
  status: RemoteStatus,
  ui: RemotePopoverUiState,
): RemotePopoverSections {
  if (!status.enabled) {
    return {
      offExplainer: true,
      modeToggle: false,
      reachConfirm: false,
      pairButton: false,
      pairingPanel: false,
      devices: false,
      devicesEmpty: false,
      sessions: false,
      launchToggle: true,
    };
  }
  return {
    offExplainer: false,
    modeToggle: true,
    reachConfirm: ui.reachConfirmOpen,
    pairButton: !ui.pairingOpen,
    pairingPanel: ui.pairingOpen,
    devices: status.devices.length > 0,
    devicesEmpty: status.devices.length === 0,
    sessions: status.sessions.length > 0,
    launchToggle: true,
  };
}

/** What the sidebar button shows: on-ness, and whether a session is live. */
export interface RemoteButtonIndicator {
  active: boolean;
  sessionDot: boolean;
}

/**
 * The button reads as active whenever remote control is on, not only while its
 * popover is open — a listening endpoint the chrome doesn't show is the footgun
 * this button exists to prevent.
 */
export function remoteButtonIndicator(
  status: RemoteStatus,
  popoverOpen: boolean,
): RemoteButtonIndicator {
  return {
    active: status.enabled || popoverOpen,
    sessionDot: status.sessions.length > 0,
  };
}
