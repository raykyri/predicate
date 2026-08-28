import assert from "node:assert/strict";
import test from "node:test";
import {
  adapterReadinessLabel,
  preferredReadyAdapter,
  readyAdaptersFirst,
} from "../src/lib/adapterReadiness";
import type { AgentAdapterMetadata } from "../src/types";

function adapter(
  id: string,
  readiness: AgentAdapterMetadata["readiness"],
  isDefault = false,
): AgentAdapterMetadata {
  return {
    id,
    label: id,
    default: isDefault,
    supportsFork: true,
    supportsResearch: true,
    supportsForkAtMessage: true,
    configuredBinary: id,
    resolvedBinary: readiness === "ready" ? `/bin/${id}` : null,
    readiness,
    message: null,
  };
}

test("prefers a remembered ready adapter over the static default", () => {
  const adapters = [adapter("claude", "ready", true), adapter("codex", "ready")];
  assert.equal(preferredReadyAdapter(adapters, "codex")?.id, "codex");
});

test("skips an unavailable remembered choice and unavailable default", () => {
  const adapters = [adapter("claude", "missing", true), adapter("codex", "ready")];
  assert.equal(preferredReadyAdapter(adapters, "claude")?.id, "codex");
});

test("sorts ready adapters first without hiding setup choices", () => {
  const adapters = [
    adapter("claude", "missing", true),
    adapter("codex", "ready"),
    adapter("grok", "missing"),
    adapter("pi", "ready"),
  ];
  assert.deepEqual(
    readyAdaptersFirst(adapters).map(({ id }) => id),
    ["codex", "pi", "claude", "grok"],
  );
  assert.equal(adapterReadinessLabel(adapters[0]), "Not installed");
});
