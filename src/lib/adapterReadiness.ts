import type { AgentAdapterMetadata } from "../types";

export function adapterIsReady(adapter: AgentAdapterMetadata | null | undefined) {
  return adapter?.readiness === "ready";
}

export function adapterCanLaunchTerminal(adapter: AgentAdapterMetadata | null | undefined) {
  return adapter?.readiness === "ready" || adapter?.readiness === "needsAuth";
}

export function adapterCanLaunchResearch(adapter: AgentAdapterMetadata | null | undefined) {
  return adapter?.researchReadiness === "ready";
}

/** Keeps every supported provider discoverable while putting usable choices
 * first. Stable sorting preserves the backend's intended order within each
 * section. */
export function readyAdaptersFirst(adapters: readonly AgentAdapterMetadata[]) {
  return [...adapters].sort(
    (left, right) => Number(adapterIsReady(right)) - Number(adapterIsReady(left)),
  );
}

export function researchReadyAdaptersFirst(adapters: readonly AgentAdapterMetadata[]) {
  return [...adapters].sort(
    (left, right) =>
      Number(adapterCanLaunchResearch(right)) - Number(adapterCanLaunchResearch(left)),
  );
}

/** Honors a remembered choice only while it remains usable, then prefers the
 * configured default when ready and finally the first ready provider. */
export function preferredReadyAdapter(
  adapters: readonly AgentAdapterMetadata[],
  preferredId?: string | null,
) {
  return (
    adapters.find((adapter) => adapter.id === preferredId && adapterIsReady(adapter)) ??
    adapters.find((adapter) => adapter.default && adapterIsReady(adapter)) ??
    adapters.find(adapterIsReady) ??
    adapters.find(
      (adapter) => adapter.id === preferredId && adapterCanLaunchTerminal(adapter),
    ) ??
    adapters.find(adapterCanLaunchTerminal) ??
    null
  );
}

export function preferredResearchAdapter(
  adapters: readonly AgentAdapterMetadata[],
  preferredId?: string | null,
) {
  return (
    adapters.find(
      (adapter) => adapter.id === preferredId && adapterCanLaunchResearch(adapter),
    ) ??
    adapters.find((adapter) => adapter.default && adapterCanLaunchResearch(adapter)) ??
    adapters.find(adapterCanLaunchResearch) ??
    null
  );
}

export function adapterReadinessLabel(adapter: AgentAdapterMetadata) {
  switch (adapter.readiness) {
    case "ready":
      return adapter.auth === "authenticated" ? "Signed in" : "Ready";
    case "missing":
      return "Not installed";
    case "needsAuth":
      return "Sign in";
    case "unsupportedVersion":
      return "Needs update";
    case "error":
      return "Needs attention";
  }
}

export function adapterReadinessMessage(adapter: AgentAdapterMetadata) {
  if (adapter.message) {
    return adapter.message;
  }
  if (adapter.readiness === "ready") {
    return `${adapter.label} is ready.`;
  }
  return `${adapter.label} was not found. Install ${adapter.configuredBinary} or configure its binary path.`;
}

export function researchReadinessLabel(adapter: AgentAdapterMetadata) {
  if (adapter.researchReadiness === "unsupportedVersion") {
    return "Needs update";
  }
  return adapterReadinessLabel(adapter);
}
