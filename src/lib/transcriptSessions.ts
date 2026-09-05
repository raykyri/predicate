import type { TranscriptOption } from "../types";

/**
 * Per-agent request generations for filesystem-backed session lists. A scan can
 * be slow enough for a newer scan to finish first, so only the newest response
 * started for an agent may replace its picker snapshot.
 */
export class TranscriptOptionsRequestTracker {
  private readonly sequenceByAgent = new Map<string, number>();

  begin(agentId: string): number {
    const sequence = (this.sequenceByAgent.get(agentId) ?? 0) + 1;
    this.sequenceByAgent.set(agentId, sequence);
    return sequence;
  }

  isLatest(agentId: string, sequence: number): boolean {
    return this.sequenceByAgent.get(agentId) === sequence;
  }

  retain(agentIds: ReadonlySet<string>): void {
    for (const agentId of this.sequenceByAgent.keys()) {
      if (!agentIds.has(agentId)) {
        this.sequenceByAgent.delete(agentId);
      }
    }
  }
}

/** Events that can add/remove a selectable session or change an "In use" badge. */
export function sessionPickerTopologyChanged(eventType: string): boolean {
  return (
    eventType === "agent.session_start" ||
    eventType === "agent.transcript_bound" ||
    eventType === "agent.transcript_recovered" ||
    eventType === "pane.removed"
  );
}

// A one-line title for a past session: prefer its first usable user-message
// preview, then a short session id, then a generic fallback. Shared by the header
// session menu and empty-state transcript picker so they read identically.
export function sessionMenuTitle(option: TranscriptOption): string {
  const preview = option.preview?.trim();
  if (preview) {
    return preview;
  }
  const shortId = option.sessionId ? option.sessionId.split("-")[0] : null;
  return shortId ? `Session ${shortId}` : "Untitled session";
}

// Coarse "x ago" label for a session's last-modified time, shown as gray
// subordinate text under each session title.
export function formatRelativeTime(modifiedMs: number, now = Date.now()): string {
  const diffMs = now - modifiedMs;
  if (diffMs < 60_000) {
    return "just now";
  }
  const minutes = Math.floor(diffMs / 60_000);
  if (minutes < 60) {
    return `${minutes} min ago`;
  }
  const hours = Math.floor(minutes / 60);
  if (hours < 24) {
    return `${hours} hr ago`;
  }
  const days = Math.floor(hours / 24);
  if (days < 7) {
    return `${days} day${days === 1 ? "" : "s"} ago`;
  }
  if (days < 30) {
    const weeks = Math.floor(days / 7);
    return `${weeks} wk ago`;
  }
  if (days < 365) {
    const months = Math.floor(days / 30);
    return `${months} mo ago`;
  }
  const years = Math.floor(days / 365);
  return `${years} yr ago`;
}
