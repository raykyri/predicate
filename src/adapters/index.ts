import type { ComponentType, ReactNode } from "react";
import { claudeUiAdapter } from "./claude";
import { codexUiAdapter } from "./codex";
import { cursorUiAdapter } from "./cursor";
import { devinUiAdapter } from "./devin";
import { grokUiAdapter } from "./grok";
import { museUiAdapter } from "./muse";
import { opencodeUiAdapter } from "./opencode";
import { piUiAdapter } from "./pi";
import type {
  AgentAdapterMetadata,
  AgentComposerPolicy,
  AgentInfo,
  AgentPermissionAction,
  PaneInfo,
  RuntimeConfig,
  Turn,
  TurnBlock,
} from "../types";

export type AgentStatus = AgentInfo["status"];

export type PermissionAction = AgentPermissionAction;
export type ComposerPolicy = AgentComposerPolicy;

/**
 * Composer gating tables, hydrated from the backend's adapter metadata at
 * startup. The Rust adapters are the single source (each AgentAdapter's
 * composer_policy); the per-adapter copies this file's adapters used to carry
 * are gone so the two sides cannot drift.
 */
const adapterPolicies = new Map<string, ComposerPolicy>();

/**
 * Used only for the frames before the runtime config resolves. Matches the
 * shape every adapter shares, with no permission actions: a button that
 * appears a frame late beats one wired to the wrong keystroke.
 */
const PRE_HYDRATION_POLICY: ComposerPolicy = {
  readyStatuses: ["awaitingInput", "done", "idle"],
  queueStatuses: ["starting", "running", "awaitingPermission"],
  steerStatuses: ["starting", "running"],
  permissionActions: [],
};

export function hydrateAdapterPolicies(adapters: AgentAdapterMetadata[]): void {
  for (const adapter of adapters) {
    adapterPolicies.set(adapter.id, adapter.composerPolicy);
  }
}

/** Mirrors getAgentUiAdapter's unknown-id fallback to Claude. */
export function composerPolicyFor(adapterId: string | null | undefined): ComposerPolicy {
  return (
    adapterPolicies.get(adapterId ?? "") ??
    adapterPolicies.get("claude") ??
    PRE_HYDRATION_POLICY
  );
}

export interface LauncherOptionsProps {
  value: Record<string, unknown>;
  onChange: (next: Record<string, unknown>) => void;
  /** Runtime configuration for adapter-specific launcher options. */
  config: RuntimeConfig | null;
}

export interface AgentUiAdapter {
  id: string;
  label: string;
  LauncherOptions?: ComponentType<LauncherOptionsProps>;
  normalizeTurns?: (turns: Turn[]) => Turn[];
  renderBlock?: (block: TurnBlock, role: string) => ReactNode | null;
  supportsFork?: boolean;
  supportsForkAtMessage?: boolean;
  canFork?: (agent: AgentInfo) => boolean;
  contextRows?: (agent: AgentInfo, pane: PaneInfo) => Array<{ label: string; value: string }>;
}

export const agentUiAdapters = [
  claudeUiAdapter,
  codexUiAdapter,
  opencodeUiAdapter,
  grokUiAdapter,
  museUiAdapter,
  piUiAdapter,
  cursorUiAdapter,
  devinUiAdapter,
];

export function findAgentUiAdapter(adapterId: string | null | undefined): AgentUiAdapter | null {
  return agentUiAdapters.find((adapter) => adapter.id === adapterId) ?? null;
}

export function getAgentUiAdapter(adapterId: string | null | undefined): AgentUiAdapter {
  return findAgentUiAdapter(adapterId) ?? claudeUiAdapter;
}

export function getDefaultAgentUiAdapter(adapterId?: string | null): AgentUiAdapter {
  return getAgentUiAdapter(adapterId ?? "claude");
}
