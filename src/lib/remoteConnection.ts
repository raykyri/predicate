import type { PaneInfo, RemoteConnectionInfo } from "../types";
import { formatRelativeTime } from "./transcriptSessions";

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
  for (const key of ["attempt", "nextRetryAt", "disconnectedAt", "lastConnectedAt", "lastVerifiedAt", "recoveryDurationMs", "startupStartedAt"] as const) {
    const number = value[key];
    if (typeof number === "number" && Number.isFinite(number) && number >= 0) connection[key] = number;
  }
  if (value.startupTimings && typeof value.startupTimings === "object") {
    connection.startupTimings = Object.fromEntries(Object.entries(value.startupTimings)
      .filter(([key, duration]) => ["reserved", "planned", "prerequisites", "bootstrapped", "attachmentSpawned", "firstOutput", "ready"].includes(key)
        && typeof duration === "number" && Number.isFinite(duration) && duration >= 0));
  }
  connection.sessionExists = typeof value.sessionExists === "boolean" ? value.sessionExists : null;
  return connection;
}

const RECOVERY_TITLES = new Map([
  ["initialConnection", "Connecting"],
  ["appRestart", "Restoring session"],
  ["connectionLost", "Restoring lost connection"],
  ["systemWake", "Resuming after sleep"],
]);

type ProgressStep = "connecting" | "checking" | "reconnecting" | "configuring" | "restoringHistory" | "attaching";

const STATE_STEPS = new Map<string, ProgressStep>([
  ["connecting", "connecting"],
  ["checking", "checking"],
  ["reconnecting", "reconnecting"],
]);

const STAGE_STEPS = new Map<string, ProgressStep>([
  ["checking", "checking"],
  ["configuring", "configuring"],
  ["restoringHistory", "restoringHistory"],
  ["attaching", "attaching"],
]);

const STEP_LABELS: Record<ProgressStep, string> = {
  connecting: "Connecting",
  checking: "Checking connection",
  reconnecting: "Reconnecting",
  configuring: "Preparing session",
  restoringHistory: "Restoring history",
  attaching: "Reattaching",
};

type ConnectionErrorKind = "none" | "timeout" | "credentialRecovery" | "other";

interface ConnectionError {
  kind: ConnectionErrorKind;
  text: string;
}

export interface RemoteConnectionPresentation {
  title: string;
  lines: string[];
  lastConnection: string | null;
  refreshEveryMs: number | null;
}

/** Resolve lifecycle first, then compose copy without inspecting formatted text. */
export function remoteConnectionPresentation(
  connection?: RemoteConnectionInfo | null,
  now = Date.now(),
): RemoteConnectionPresentation {
  const view: RemoteConnectionPresentation = {
    title: "Disconnected", lines: [], lastConnection: null, refreshEveryMs: null,
  };
  if (!connection) return view;
  if (connection.state === "connected") {
    view.title = remoteHooksNeedAttention(connection) ? "Connected · hooks need attention" : "Connected";
    view.lines = connectedDetails(connection);
    return view;
  }
  if (connection.lastConnectedAt != null) {
    view.lastConnection = `Last connection: ${formatRelativeTime(connection.lastConnectedAt, now)}`;
    view.refreshEveryMs = 1000;
  }
  if (connection.stage === "sleeping") {
    view.title = "Sleeping";
    view.lines = ["Waiting for wake signal..."];
    return view;
  }
  if (connection.stage === "sessionEnded" || connection.sessionExists === false) {
    view.title = "Session ended";
    return view;
  }

  const error = classifyConnectionError(connection.message);
  if (connection.state === "failed" || connection.stage === "needsAttention") {
    view.title = "Connection failed";
    if (error.text) view.lines.push(error.text);
    return view;
  }
  const stateStep = STATE_STEPS.get(connection.state);
  if (!stateStep) {
    if (error.text) view.lines.push(error.text);
    return view;
  }

  const stepKey = STAGE_STEPS.get(connection.stage ?? "") ?? stateStep;
  const step = STEP_LABELS[stepKey];
  const credentialRecovery = error.kind === "credentialRecovery";
  const recoveryTitle = credentialRecovery ? error.text : RECOVERY_TITLES.get(connection.reason ?? "");
  const showStep = recoveryTitle !== undefined;
  view.title = recoveryTitle ?? step;

  const retrySeconds = connection.nextRetryAt == null ? null : Math.max(0, Math.ceil((connection.nextRetryAt - now) / 1000));
  const combinedRetry = showStep && stepKey === "reconnecting" && retrySeconds != null && retrySeconds > 0;
  if (showStep) {
    const descriptionStep = connection.reason === "initialConnection" && !credentialRecovery && stepKey === "connecting"
      ? "Establishing connection" : step;
    view.lines.push(combinedRetry ? `${descriptionStep}... (retrying in ${retrySeconds} sec)` : `${descriptionStep}...`);
  }
  const hideTimeout = connection.state === "reconnecting" && connection.reason === "connectionLost" && error.kind === "timeout";
  if (error.text && !credentialRecovery && !hideTimeout) view.lines.push(error.text);
  if (retrySeconds != null) {
    view.refreshEveryMs = 1000;
    if (!combinedRetry) view.lines.push(retrySeconds > 0 ? `Retrying in ${retrySeconds} sec...` : "Waiting to retry...");
  }
  return view;
}

export function remoteConnectionLabel(connection?: RemoteConnectionInfo | null): string {
  return remoteConnectionPresentation(connection).title;
}

export function remoteConnectionDetails(connection?: RemoteConnectionInfo | null, now = Date.now()): string {
  const view = remoteConnectionPresentation(connection, now);
  return [...view.lines, ...(view.lastConnection ? [view.lastConnection] : [])].join("\n");
}

export function remoteHooksNeedAttention(connection?: RemoteConnectionInfo | null): boolean {
  return connection?.state === "connected" && (connection.hookHealth === "authenticationFailed" || connection.hookHealth === "unavailable");
}

function connectedDetails(connection: RemoteConnectionInfo): string[] {
  const details: string[] = [];
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
  return details;
}

function classifyConnectionError(message?: string | null): ConnectionError {
  if (!message || /^Unable to establish the remote connection\.?$/i.test(message.trim())) return { kind: "none", text: "" };
  if (message.includes("require tmux 3.2 or newer")) return { kind: "other", text: "Remote terminals require tmux 3.2 or newer" };
  if (message.includes("Permission denied") || message.includes("Too many authentication failures")) return { kind: "other", text: "SSH authentication failed" };
  if (message.includes("Host key verification failed") || message.includes("REMOTE HOST IDENTIFICATION HAS CHANGED")) return { kind: "other", text: "SSH host key verification failed" };
  if (/timed out/i.test(message)) return { kind: "timeout", text: "Connection timed out" };
  if (message.includes("could not recover remote hook credential")) return { kind: "credentialRecovery", text: "Could not restore agent authentication" };
  return { kind: "other", text: message };
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
