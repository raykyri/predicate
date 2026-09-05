import type { PaneInfo, RemoteConnectionInfo } from "../types";

// Bounded, process-local diagnostics. Entries never contain paths or tokens.
const launches = new Map<string, { started: number; stages: Set<string> }>();

export function trackRemoteStartup(paneId: string, started: number): void {
  launches.set(paneId, { started, stages: new Set() });
  while (launches.size > 128) launches.delete(launches.keys().next().value!);
}

export function recordRemoteStartup(paneId: string, stage: "reserved" | "visible" | "ready", now = performance.now()): number | null {
  const launch = launches.get(paneId);
  if (!launch || launch.stages.has(stage)) return null;
  launch.stages.add(stage);
  const elapsed = Math.max(0, now - launch.started);
  console.debug("qmux: remote startup", { paneId, stage, elapsedMs: elapsed });
  if (launch.stages.has("visible") && launch.stages.has("ready")) launches.delete(paneId);
  return elapsed;
}

export function forgetRemoteStartup(paneId: string): void {
  launches.delete(paneId);
}

// Completion can arrive before the invoke response is inserted into React's
// pane list. Preserve it when that response still describes a reservation.
const observations = new Map<string, RemoteConnectionInfo>();
export function rememberRemoteStartupConnection(paneId: string, connection: RemoteConnectionInfo): void {
  observations.set(paneId, connection);
  while (observations.size > 128) observations.delete(observations.keys().next().value!);
}

export function reconcileRemoteReservation(pane: PaneInfo): PaneInfo {
  const connection = observations.get(pane.id);
  return pane.remoteConnection?.stage === "starting" && connection
    ? { ...pane, remoteConnection: connection } : pane;
}

export function forgetRemotePane(paneId: string): void {
  forgetRemoteStartup(paneId);
  observations.delete(paneId);
}
