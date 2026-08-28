import assert from "node:assert/strict";
import test from "node:test";
import {
  adapterCanLaunchResearch,
  adapterCanLaunchTerminal,
  adapterReadinessLabel,
  preferredResearchAdapter,
  preferredReadyAdapter,
  researchReadyAdaptersFirst,
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
    resolvedBinary: readiness === "missing" ? null : `/bin/${id}`,
    readiness,
    researchReadiness: readiness,
    message: null,
    version: null,
    auth: "unknown",
    checkedAt: null,
    loginCommand: null,
    installUrl: null,
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

test("allows interactive sign-in without admitting headless research", () => {
  const needsAuth = adapter("claude", "needsAuth", true);
  assert.equal(adapterCanLaunchTerminal(needsAuth), true);
  assert.equal(adapterCanLaunchResearch(needsAuth), false);
  assert.equal(adapterReadinessLabel(needsAuth), "Sign in");
});

test("research preference uses its stricter readiness", () => {
  const oldClaude = adapter("claude", "ready", true);
  oldClaude.researchReadiness = "unsupportedVersion";
  const codex = adapter("codex", "ready");
  assert.equal(preferredResearchAdapter([oldClaude, codex], "claude")?.id, "codex");
  assert.deepEqual(
    researchReadyAdaptersFirst([oldClaude, codex]).map(({ id }) => id),
    ["codex", "claude"],
  );
});
