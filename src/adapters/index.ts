import type { ComponentType, ReactNode } from "react";
import { claudeUiAdapter } from "./claude";
import { codexUiAdapter } from "./codex";
import { cursorUiAdapter } from "./cursor";
import { devinUiAdapter } from "./devin";
import { grokUiAdapter } from "./grok";
import { museUiAdapter } from "./muse";
import { opencodeUiAdapter } from "./opencode";
import { piUiAdapter } from "./pi";
import type { AgentInfo, PaneInfo, RuntimeConfig, Turn, TurnBlock } from "../types";

export type AgentStatus = AgentInfo["status"];

export interface PermissionAction {
  id: string;
  label: string;
  input: string;
}

export interface ComposerPolicy {
  readyStatuses: AgentStatus[];
  queueStatuses: AgentStatus[];
  steerStatuses: AgentStatus[];
  permissionActions: PermissionAction[];
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
  composerPolicy: (agent: AgentInfo) => ComposerPolicy;
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
