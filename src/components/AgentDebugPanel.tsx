import { Bug, LoaderCircle, X } from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { PointerEvent as ReactPointerEvent } from "react";
import { composerPolicyFor, getAgentUiAdapter } from "../adapters";
import { agentTabStatusDotClass } from "../lib/composerActions";
import {
  getAgentDeliveryDebug,
  sendAgentDebugInput,
  sendNextQueuedAgentTurn,
  submitAgentTurn,
} from "../lib/api";
import { agentStatusLabel } from "../lib/appHelpers";
import type {
  AgentDeliveryDebugInfo,
  AgentInfo,
  QueuedTurn,
  SubmitAgentTurnMode,
} from "../types";

export interface AgentDebugPanelPosition {
  top: number;
  right: number;
}

export interface AgentDebugTarget {
  agent: AgentInfo;
  paneId: string;
  label: string;
}

interface AgentDebugPanelProps {
  agent: AgentInfo;
  paneId: string;
  targets: AgentDebugTarget[];
  position: AgentDebugPanelPosition | null;
  onPositionChange: (position: AgentDebugPanelPosition) => void;
  onQueueChange: (agentId: string, queuedTurns: QueuedTurn[]) => void;
  onClose: () => void;
}

type ActionState =
  | { kind: "idle" }
  | { kind: "running"; label: string }
  | { kind: "success"; message: string }
  | { kind: "error"; message: string };

const FRAME_INSET = 8;
const SNAPSHOT_INTERVAL_MS = 500;

function elapsedLabel(sentAtMs: number): string {
  const elapsed = Math.max(0, Date.now() - sentAtMs);
  return elapsed < 1_000 ? `${elapsed}ms` : `${(elapsed / 1_000).toFixed(1)}s`;
}

function turnFlags(turn: AgentDeliveryDebugInfo["queuedTurns"][number]): string[] {
  const flags: string[] = [];
  if (turn.possiblyPasted) flags.push("possibly pasted");
  if (turn.pauseAfter) flags.push("pause after");
  if (turn.waitFor) flags.push("waiting");
  if (turn.delivery) flags.push(turn.delivery.kind === "fork" ? "fork delivery" : "new session");
  return flags;
}

export default function AgentDebugPanel({
  agent,
  paneId,
  targets,
  position,
  onPositionChange,
  onQueueChange,
  onClose,
}: AgentDebugPanelProps) {
  const frameRef = useRef<HTMLDivElement>(null);
  const snapshotSequenceRef = useRef(0);
  const snapshotInFlightRef = useRef<{ agentId: string; sequence: number } | null>(null);
  const actionRunningRef = useRef(false);
  const [targetAgentId, setTargetAgentId] = useState(agent.id);
  const [snapshot, setSnapshot] = useState<AgentDeliveryDebugInfo | null>(null);
  const [snapshotError, setSnapshotError] = useState<string | null>(null);
  const [action, setAction] = useState<ActionState>({ kind: "idle" });
  const actionRunning = action.kind === "running";
  const target =
    targets.find((candidate) => candidate.agent.id === targetAgentId) ??
    targets.find((candidate) => candidate.agent.id === agent.id) ?? {
      agent,
      paneId,
      label: paneId.slice(0, 8),
    };
  const targetAgent = target.agent;

  useEffect(() => {
    if (targetAgentId !== targetAgent.id) {
      setTargetAgentId(targetAgent.id);
    }
  }, [targetAgent.id, targetAgentId]);

  useLayoutEffect(() => {
    snapshotSequenceRef.current += 1;
    setSnapshot(null);
    setSnapshotError(null);
  }, [targetAgent.id]);

  const refreshSnapshot = useCallback(async () => {
    if (
      snapshotInFlightRef.current?.agentId === targetAgent.id &&
      snapshotInFlightRef.current.sequence === snapshotSequenceRef.current
    ) {
      return;
    }
    const sequence = ++snapshotSequenceRef.current;
    snapshotInFlightRef.current = { agentId: targetAgent.id, sequence };
    try {
      const next = await getAgentDeliveryDebug(targetAgent.id);
      if (sequence !== snapshotSequenceRef.current) return;
      setSnapshot(next);
      setSnapshotError(null);
    } catch (error) {
      if (sequence !== snapshotSequenceRef.current) return;
      setSnapshotError(error instanceof Error ? error.message : String(error));
    } finally {
      if (snapshotInFlightRef.current?.sequence === sequence) {
        snapshotInFlightRef.current = null;
      }
    }
  }, [targetAgent.id]);

  useEffect(() => {
    let disposed = false;
    const refresh = async () => {
      if (disposed) return;
      await refreshSnapshot();
    };
    void refresh();
    const timer = window.setInterval(() => void refresh(), SNAPSHOT_INTERVAL_MS);
    return () => {
      disposed = true;
      snapshotSequenceRef.current += 1;
      window.clearInterval(timer);
    };
  }, [refreshSnapshot]);

  useLayoutEffect(() => {
    if (!position) return;
    const frame = frameRef.current;
    const parent = frame?.parentElement;
    if (!frame || !parent) return;
    const constrain = () => {
      const maxRight = Math.max(FRAME_INSET, parent.clientWidth - frame.offsetWidth - FRAME_INSET);
      const maxTop = Math.max(FRAME_INSET, parent.clientHeight - frame.offsetHeight - FRAME_INSET);
      const clamped = {
        top: Math.min(maxTop, Math.max(FRAME_INSET, position.top)),
        right: Math.min(maxRight, Math.max(FRAME_INSET, position.right)),
      };
      if (clamped.top !== position.top || clamped.right !== position.right) {
        onPositionChange(clamped);
      }
    };
    const observer = new ResizeObserver(constrain);
    observer.observe(parent);
    observer.observe(frame);
    constrain();
    return () => observer.disconnect();
  }, [position, onPositionChange]);

  function startDrag(event: ReactPointerEvent<HTMLDivElement>) {
    if (event.button !== 0) return;
    const frame = frameRef.current;
    const parent = frame?.parentElement;
    if (!frame || !parent) return;
    event.preventDefault();
    event.currentTarget.setPointerCapture(event.pointerId);
    const start = { x: event.clientX, y: event.clientY };
    const origin = {
      top: frame.offsetTop,
      right: parent.clientWidth - frame.offsetLeft - frame.offsetWidth,
    };
    const move = (moveEvent: PointerEvent) => {
      const maxRight = Math.max(FRAME_INSET, parent.clientWidth - frame.offsetWidth - FRAME_INSET);
      const maxTop = Math.max(FRAME_INSET, parent.clientHeight - frame.offsetHeight - FRAME_INSET);
      onPositionChange({
        top: Math.min(maxTop, Math.max(FRAME_INSET, origin.top + moveEvent.clientY - start.y)),
        right: Math.min(
          maxRight,
          Math.max(FRAME_INSET, origin.right - (moveEvent.clientX - start.x)),
        ),
      });
    };
    const stop = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
      window.removeEventListener("pointercancel", stop);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
    window.addEventListener("pointercancel", stop);
  }

  async function runAction(label: string, operation: () => Promise<string>) {
    if (actionRunningRef.current) return;
    actionRunningRef.current = true;
    setAction({ kind: "running", label });
    try {
      const message = await operation();
      await refreshSnapshot();
      setAction({ kind: "success", message });
    } catch (error) {
      await refreshSnapshot();
      setAction({ kind: "error", message: error instanceof Error ? error.message : String(error) });
    } finally {
      actionRunningRef.current = false;
    }
  }

  function runRaw(
    label: string,
    kind: "textOnly" | "returnOnly" | "textAndReturn",
  ) {
    void runAction(label, async () => {
      await sendAgentDebugInput(targetAgent.id, kind);
      return `${label} written to ${target.label}`;
    });
  }

  function runPipeline(label: string, mode: SubmitAgentTurnMode) {
    void runAction(label, async () => {
      const result = await submitAgentTurn(targetAgent.id, ".", mode);
      onQueueChange(targetAgent.id, result.queuedTurns);
      return result.queued ? `${label} queued (${result.pendingTurns})` : `${label} dispatched`;
    });
  }

  const policy = composerPolicyFor(targetAgent.adapter);
  const queuedTurns = snapshot?.queuedTurns ?? [];
  const queueHead = queuedTurns[0];
  const flags = queueHead ? turnFlags(queueHead) : [];
  const transport =
    targetAgent.adapter === "grok" ? "typed payload + Return" : "bracketed paste + Return";

  return (
    <div
      ref={frameRef}
      className="agent-debug-panel"
      style={position ? { top: position.top, right: position.right, bottom: "auto" } : undefined}
      data-pane-id={paneId}
      data-target-pane-id={target.paneId}
    >
      <div className="agent-debug-titlebar" onPointerDown={startDrag}>
        <Bug size={11} aria-hidden="true" />
        <span>Debug</span>
        <code>{targetAgent.adapter}</code>
        <button
          type="button"
          className="agent-debug-close"
          title="Close debug panel"
          aria-label="Close debug panel"
          onPointerDown={(event) => event.stopPropagation()}
          onClick={(event) => {
            event.stopPropagation();
            onClose();
          }}
        >
          <X size={12} aria-hidden="true" />
        </button>
      </div>

      <div className="agent-debug-body">
        <label className="agent-debug-target">
          <span>Send commands to</span>
          <select
            value={targetAgent.id}
            disabled={actionRunning}
            onChange={(event) => {
              setTargetAgentId(event.currentTarget.value);
              setSnapshot(null);
              setSnapshotError(null);
              setAction({ kind: "idle" });
            }}
          >
            {targets.map((candidate) => (
              <option key={candidate.agent.id} value={candidate.agent.id}>
                {candidate.label} · {candidate.agent.adapter} · {candidate.paneId.slice(0, 6)}
              </option>
            ))}
          </select>
        </label>

        <section className="agent-debug-status" aria-label="Agent delivery status">
          <div className="agent-debug-status-primary">
            <span
              className={agentTabStatusDotClass(targetAgent.status, false)}
              aria-hidden="true"
            />
            <strong>{agentStatusLabel(targetAgent.status)}</strong>
            {targetAgent.paused ? <span className="agent-debug-chip is-warn">paused</span> : null}
          </div>
          <dl>
            <div>
              <dt>Agent</dt>
              <dd title={targetAgent.id}>{targetAgent.id.slice(0, 12)}</dd>
            </div>
            <div>
              <dt>Transport</dt>
              <dd>{transport}</dd>
            </div>
            <div>
              <dt>Policy</dt>
              <dd>
                {policy.readyStatuses.includes(targetAgent.status) ? "send" : "—"} /{" "}
                {policy.queueStatuses.includes(targetAgent.status) ? "queue" : "—"} /{" "}
                {policy.steerStatuses.includes(targetAgent.status) ? "steer" : "—"}
              </dd>
            </div>
            <div>
              <dt>Queue</dt>
              <dd>{queuedTurns.length}</dd>
            </div>
            <div>
              <dt>Activity</dt>
              <dd>
                {snapshot?.activityRevision ?? "—"} / {snapshot?.statusRevision ?? "—"}
              </dd>
            </div>
          </dl>

          {snapshot ? (
            <div className="agent-debug-chips">
              <span className={`agent-debug-chip${snapshot.typing ? " is-active" : ""}`}>
                typing
              </span>
              <span className={`agent-debug-chip${snapshot.draining ? " is-active" : ""}`}>
                draining
              </span>
              <span className={`agent-debug-chip${snapshot.inflight ? " is-active" : ""}`}>
                inflight
              </span>
              <span className={`agent-debug-chip${snapshot.pendingPause ? " is-warn" : ""}`}>
                pending pause
              </span>
            </div>
          ) : null}

          {queueHead ? (
            <p className="agent-debug-detail">
              Head <code>{JSON.stringify(queueHead.text)}</code>
              {flags.length > 0 ? ` · ${flags.join(", ")}` : ""}
            </p>
          ) : null}
          {snapshot?.inflight ? (
            <p className="agent-debug-detail">
              Inflight <code>{JSON.stringify(snapshot.inflight.text)}</code>
              {snapshot.inflight.possiblyPasted ? " · possibly pasted" : ""}
            </p>
          ) : null}
          {snapshot?.outstandingSends.map((send) => (
            <p className="agent-debug-detail" key={send.id}>
              Awaiting echo #{send.id} · {send.source} · {elapsedLabel(send.sentAtMs)} ·{" "}
              <code>{JSON.stringify(send.text)}</code>
            </p>
          ))}
          {snapshot && snapshot.submitWatchSendIds.length > 0 ? (
            <p className="agent-debug-detail is-warn">
              Submit watch: {snapshot.submitWatchSendIds.join(", ")}
            </p>
          ) : null}
          {snapshotError ? <p className="agent-debug-detail is-error">{snapshotError}</p> : null}
        </section>

        <section className="agent-debug-actions" aria-label="Raw queued-turn transport">
          <h3>Raw transport</h3>
          <button
            type="button"
            title="Write the queued-turn payload leg without Return"
            disabled={actionRunning}
            onClick={() => runRaw("Send .", "textOnly")}
          >
            Send .
          </button>
          <button
            type="button"
            title="Write only the queued-turn Return leg"
            disabled={actionRunning}
            onClick={() => runRaw("Send ↵", "returnOnly")}
          >
            Send ↵
          </button>
          <button
            type="button"
            title="Write the complete queued-turn payload and Return sequence"
            disabled={actionRunning}
            onClick={() => runRaw("Send .↵", "textAndReturn")}
          >
            Send .↵
          </button>
        </section>

        <section className="agent-debug-actions is-pipeline" aria-label="Turn pipeline">
          <h3>Turn pipeline</h3>
          <button type="button" disabled={actionRunning} onClick={() => runPipeline("Auto .", "auto")}>
            Auto .
          </button>
          <button type="button" disabled={actionRunning} onClick={() => runPipeline("Send .", "send")}>
            Send . via pipeline
          </button>
          <button type="button" disabled={actionRunning} onClick={() => runPipeline("Queue .", "queue")}>
            Queue .
          </button>
          <button type="button" disabled={actionRunning} onClick={() => runPipeline("Steer .", "steer")}>
            Steer .
          </button>
          <button
            type="button"
            disabled={actionRunning || queuedTurns.length === 0}
            onClick={() => void runAction("Send top queued", async () => {
              const result = await sendNextQueuedAgentTurn(targetAgent.id);
              onQueueChange(targetAgent.id, result.queuedTurns);
              return result.sent ? "Top queued turn dispatched" : "Queue was not ready to dispatch";
            })}
          >
            Send top queued
          </button>
        </section>

        {action.kind !== "idle" ? (
          <p className={`agent-debug-result is-${action.kind}`} aria-live="polite">
            {action.kind === "running" ? <LoaderCircle size={11} aria-hidden="true" /> : null}
            {action.kind === "running" ? `${action.label}…` : action.message}
          </p>
        ) : null}
      </div>
    </div>
  );
}
