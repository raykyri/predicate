import type { AgentStatus, ComposerPolicy } from "../adapters";
import type { QueuedTurn, QueuedTurnDelivery, WaitTarget } from "../types";
import { agentStatusLabel, agentStatusTone } from "./appHelpers";
import { isTuiCommandMessage, type ParsedComposerSlashCommand } from "./composerSlashCommands";

export const FORK_REQUIREMENT_TITLE =
  "Forking requires a supported agent session that has run a turn";

export const BTW_SAFETY_INSTRUCTION = [
  '<qmux_instruction source="agent_driver">',
  "Do not change the working tree or codebase unless explicitly instructed to.",
  "</qmux_instruction>",
].join("\n");

/** Capability gating shared by the right-pane composer and the global task
 * launcher, derived in one place so both surfaces enable Send/Send Now/Queue
 * and route the submit shortcut identically. */
export interface ComposerGating {
  canSend: boolean;
  canSteer: boolean;
  canAppendQueue: boolean;
  submitShortcutWouldTargetSend: boolean;
  submitShortcutWouldTargetQueue: boolean;
}

export function deriveComposerGating(
  policy: ComposerPolicy | null,
  status: AgentStatus | null,
  queueLength: number,
  submitting: boolean,
): ComposerGating {
  const canSend = Boolean(policy && status && policy.readyStatuses.includes(status));
  const canQueue = Boolean(policy && status && policy.queueStatuses.includes(status));
  const canSteer = Boolean(policy && status && policy.steerStatuses.includes(status));
  const hasQueue = queueLength > 0;
  const canAppendQueue = Boolean(status && status !== "failed" && (canQueue || hasQueue));
  // Where the submit shortcut lands: send to a ready agent with an empty
  // queue, queue behind everything else.
  const submitShortcutWouldTargetSend = !submitting && canSend && !hasQueue;
  const submitShortcutWouldTargetQueue =
    !submitShortcutWouldTargetSend && !submitting && canAppendQueue;
  return {
    canSend,
    canSteer,
    canAppendQueue,
    submitShortcutWouldTargetSend,
    submitShortcutWouldTargetQueue,
  };
}

/** How a composer submission dispatches given its parsed slash command, shared
 * so the slash commands behave identically in the composer and the launcher.
 * A loop with `runOnce` is a message the agent TUI would intercept (`/` or `!`):
 * it can't be looped (no completion to wait on), so it is sent a single time. */
export type ComposerSubmissionPlan =
  | { kind: "reject"; message: string }
  | { kind: "fork"; useWorktree: boolean; prompt: string }
  | { kind: "btw"; prompt: string }
  | { kind: "loop"; prompt: string; runOnce: boolean }
  | { kind: "turn" };

export function planComposerSubmission(
  parsed: ParsedComposerSlashCommand,
  canFork: boolean,
): ComposerSubmissionPlan {
  if (parsed.kind === "incomplete") {
    return { kind: "reject", message: `Add a message after ${parsed.command.token}` };
  }
  if (parsed.kind === "ready") {
    if (parsed.command.kind === "loop") {
      return {
        kind: "loop",
        prompt: parsed.prompt,
        runOnce: isTuiCommandMessage(parsed.prompt),
      };
    }
    if (!canFork) {
      return { kind: "reject", message: FORK_REQUIREMENT_TITLE };
    }
    if (parsed.command.kind === "btw") {
      return {
        kind: "btw",
        prompt: `${BTW_SAFETY_INSTRUCTION}\n\n${parsed.prompt}`,
      };
    }
    return { kind: "fork", useWorktree: parsed.command.useWorktree, prompt: parsed.prompt };
  }
  return { kind: "turn" };
}

/** Status text for a queue-after wait target, shared by the right-pane
 * composer's queue dropdown and the global task launcher's. */
export function waitTargetStatusLabel(target: WaitTarget) {
  const queueCount = target.queueCount ?? 0;
  if ((target.status === "done" || target.status === "idle") && queueCount > 0) {
    return target.queueBlocked ? "Waiting" : `${queueCount} queued`;
  }
  switch (target.status) {
    case "starting":
      return "Starting";
    case "running":
      return "Working";
    case "awaitingInput":
      return "Awaiting input";
    case "awaitingPermission":
      return "Awaiting decision";
    default:
      return target.status;
  }
}

export function waitTargetStatusDotClass(target: WaitTarget) {
  return `${agentTabStatusDotClass(target.status, Boolean(target.queueBlocked))} wait-target-status-dot`;
}

/** True when the first queued turn waits on a different agent's session. */
export function queueWaitsOnOtherAgent(agentId: string, queue: QueuedTurn[]): boolean {
  const first = queue[0];
  return Boolean(first?.waitFor && first.waitFor.agentId !== agentId);
}

/** Status pill text for a tab row, shared by the sidebar and the launcher:
 * queue count while working or idle, otherwise the status label — with
 * "Running" left to the pulsing dot alone. */
export function agentTabStatusPill(
  status: AgentStatus,
  queueCount: number,
  waitsOnOtherPane: boolean,
): string | null {
  if ((status === "running" || status === "idle") && queueCount > 0) {
    return `${queueCount} ${waitsOnOtherPane ? "waiting" : "queued"}`;
  }
  const label = agentStatusLabel(status);
  return label === "Running" ? null : label;
}

/** The pane-tab status dot's class string, shared by every surface that
 * renders one: sidebar rows, collapsed-group icons, launcher rows, and
 * wait-target menu items. */
export function agentTabStatusDotClass(
  status: AgentStatus | undefined,
  waitsOnOtherPane: boolean,
): string {
  const tone = status ? agentStatusTone(status) : "idle";
  const awaitingInput = status === "awaitingInput" ? " status-awaiting-input" : "";
  const waiting = waitsOnOtherPane ? " is-waiting-on-pane" : "";
  return `pane-tab-dot status-${tone}${awaitingInput}${waiting}`;
}

/** Delivery choices shared by the right-pane composer and global launcher. */
export const QUEUE_DELIVERY_OPTIONS: ReadonlyArray<{
  label: string;
  title: string;
  needsFork: boolean;
  delivery: QueuedTurnDelivery;
}> = [
  {
    label: "Queue and fork",
    title: "When reached, fork this session and send the message to the fork",
    needsFork: true,
    delivery: { kind: "fork" },
  },
  {
    label: "Queue and fork in worktree",
    title:
      "When reached, fork this session into a fresh git worktree and send the message to the fork",
    needsFork: true,
    delivery: { kind: "fork", useWorktree: true },
  },
  {
    label: "Queue in new session",
    title: "When reached, start a fresh session in the same directory with this message",
    needsFork: false,
    delivery: { kind: "newSession" },
  },
];
