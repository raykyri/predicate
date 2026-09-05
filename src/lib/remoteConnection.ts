import type { PaneInfo, RemoteConnectionInfo } from "../types";

export function parseRemoteConnection(raw: unknown): RemoteConnectionInfo | null {
  if (!raw || typeof raw !== "object") return null;
  const value = raw as Record<string, unknown>;
  if (typeof value.state !== "string" || !["connecting", "checking", "connected", "reconnecting", "disconnected", "failed"].includes(value.state)) return null;
  const connection: RemoteConnectionInfo = { state: value.state as RemoteConnectionInfo["state"] };
  if (["checking", "healthy", "authenticationFailed", "unavailable"].includes(value.hookHealth as string)) {
    connection.hookHealth = value.hookHealth as RemoteConnectionInfo["hookHealth"];
  }
  for (const key of ["message", "stage", "reason", "recoveryAction"] as const) {
    connection[key] = typeof value[key] === "string" ? value[key] : null;
  }
  for (const key of ["attempt", "nextRetryAt", "disconnectedAt", "lastConnectedAt", "lastVerifiedAt", "recoveryDurationMs"] as const) {
    const number = value[key];
    if (typeof number === "number" && Number.isFinite(number) && number >= 0) connection[key] = number;
  }
  connection.sessionExists = typeof value.sessionExists === "boolean" ? value.sessionExists : null;
  return connection;
}

export function remoteConnectionLabel(connection?: RemoteConnectionInfo | null): string {
  if (remoteHooksNeedAttention(connection)) return "Connected · hooks need attention";
  if (connection?.stage === "sleeping") return "Sleeping";
  if (connection?.stage === "sessionEnded") return "Session ended";
  if (connection?.stage === "needsAttention") return "Needs attention";
  if (connection?.stage === "restoringHistory") return "Restoring history";
  if (connection?.stage === "attaching") return "Reattaching";
  if (connection?.stage === "configuring") return "Preparing session";
  switch (connection?.state) {
    case "connected": return "Connected";
    case "connecting": return "Connecting";
    case "checking": return "Checking connection";
    case "reconnecting": return "Reconnecting";
    case "failed": return "Connection failed";
    default: return "Disconnected";
  }
}

export function remoteHooksNeedAttention(connection?: RemoteConnectionInfo | null): boolean {
  return connection?.state === "connected" && (connection.hookHealth === "authenticationFailed" || connection.hookHealth === "unavailable");
}

export function remoteConnectionDetails(connection?: RemoteConnectionInfo | null): string {
  if (!connection) return "Connection has not been verified.";
  const details: string[] = [];
  const reason = {
    systemWake: "Checking after sleep",
    systemSleep: "Checks resume when this computer wakes",
    appRestart: "Restoring after app restart",
    manualRetry: "Reconnect requested",
    connectionLost: "SSH connection closed",
    initialConnection: "Initial connection",
  }[connection.reason ?? ""];
  if (connection.state === "connected") {
    if (connection.hookHealth === "checking") details.push("Checking agent hooks; the terminal is ready.");
    if (connection.hookHealth === "healthy") details.push("Agent hook authentication verified.");
    if (connection.hookHealth === "authenticationFailed") details.push("Agent hook authentication failed (invalid QMUX_TOKEN). The terminal remains usable, but agent tracking may not update.");
    if (connection.hookHealth === "unavailable") details.push("Agent hooks could not be verified. The terminal remains usable, but agent tracking may not update.");
    if (connection.reason === "systemWake") details.push(connection.recoveryAction === "reattached"
      ? "Reattached to the existing session after sleep."
      : "Connection verified after sleep; no reattachment needed.");
    if (connection.reason === "appRestart") details.push("Reattached to the existing session after app restart.");
    if (connection.reason === "connectionLost") details.push("Reattached to the existing session after connection loss.");
    if (connection.lastConnectedAt) details.push(`Attached: ${new Date(connection.lastConnectedAt).toLocaleString()}.`);
    if (connection.lastVerifiedAt) details.push(`Verified: ${new Date(connection.lastVerifiedAt).toLocaleString()}.`);
    if (connection.recoveryDurationMs != null && connection.reason !== "initialConnection") {
      details.push(`Recovery took ${(connection.recoveryDurationMs / 1000).toFixed(1)} seconds.`);
    }
  } else {
    if (reason) details.push(`${reason}.`);
    if (connection.attempt) details.push(`Attempt ${connection.attempt}.`);
    if (connection.nextRetryAt) details.push(`Next retry: ${new Date(connection.nextRetryAt).toLocaleTimeString()}.`);
    if (connection.lastConnectedAt) details.push(`Last attached: ${new Date(connection.lastConnectedAt).toLocaleString()}.`);
    if (connection.message) details.push(connection.message);
  }
  if (connection.sessionExists === false) details.push("The session will not be recreated automatically.");
  else if (connection.state !== "connected" && connection.stage !== "sleeping") {
    details.push(connection.sessionExists ? "Existing session found; restoring the connection." : "Remote session status is unknown.");
  }
  return details.join("\n");
}

export function remoteGroupStatus(panes: PaneInfo[]): { label: string; detail: string } | null {
  const connections = panes.filter(pane => pane.remoteSession).map(pane => pane.remoteConnection);
  if (!connections.length) return null;
  const connected = connections.filter(connection => connection?.state === "connected").length;
  const priority = (connection: RemoteConnectionInfo | null | undefined) => {
    if (connection?.state === "failed") return 0;
    if (connection?.stage === "sleeping") return 1;
    if (connection?.state === "reconnecting") return 2;
    if (connection?.state === "checking") return 3;
    if (connection?.state === "connecting") return 4;
    if (remoteHooksNeedAttention(connection)) return 5;
    if (connection?.state === "connected") return 6;
    return 5;
  };
  const representative = [...connections].sort((a, b) => priority(a) - priority(b))[0];
  return {
    label: connected > 0 && connected < connections.length
      ? `${connected} of ${connections.length} connected`
      : remoteConnectionLabel(representative),
    detail: `${connected} of ${connections.length} connected\n${remoteConnectionDetails(representative)}`,
  };
}
