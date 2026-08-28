import type { AgentAdapterMetadata } from "../types";

export function adapterIsReady(adapter: AgentAdapterMetadata | null | undefined) {
  return adapter?.readiness === "ready";
}

/** Keeps every supported provider discoverable while putting usable choices
 * first. Stable sorting preserves the backend's intended order within each
 * section. */
export function readyAdaptersFirst(adapters: readonly AgentAdapterMetadata[]) {
  return [...adapters].sort(
    (left, right) => Number(adapterIsReady(right)) - Number(adapterIsReady(left)),
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
    null
  );
}

export function adapterReadinessLabel(adapter: AgentAdapterMetadata) {
  return adapter.readiness === "ready" ? "Ready" : "Not installed";
}

export function adapterReadinessMessage(adapter: AgentAdapterMetadata) {
  if (adapter.readiness === "ready") {
    return `${adapter.label} is ready.`;
  }
  return (
    adapter.message ??
    `${adapter.label} was not found. Install ${adapter.configuredBinary} or configure its binary path.`
  );
}
