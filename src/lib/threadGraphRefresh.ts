/**
 * Per-thread request generations shared by initial hydration and live refreshes.
 * A response may update React state only while it is still the newest request
 * started for that thread.
 */
export class ThreadGraphRequestTracker {
  private readonly sequenceByThread = new Map<string, number>();

  begin(threadId: string): number {
    const sequence = (this.sequenceByThread.get(threadId) ?? 0) + 1;
    this.sequenceByThread.set(threadId, sequence);
    return sequence;
  }

  isLatest(threadId: string, sequence: number): boolean {
    return this.sequenceByThread.get(threadId) === sequence;
  }
}

/**
 * Resolves a dirty-agent batch independently. Agents can disappear while the
 * trailing debounce is armed; one missing agent must not discard valid peers.
 */
export function uniqueResolvedThreadIds(
  agentIds: Iterable<string>,
  resolve: (agentId: string) => string | null,
): string[] {
  const threadIds = new Set<string>();
  for (const agentId of agentIds) {
    const threadId = resolve(agentId);
    if (threadId) {
      threadIds.add(threadId);
    }
  }
  return [...threadIds];
}
