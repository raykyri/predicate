/**
 * Serializes native human-browser lifecycle calls across all overlay owners.
 * A rejection is deliberately swallowed only by the internal tail so one
 * failed WebKit operation cannot permanently stall later cleanup requests.
 */
export class HumanBrowserLifecycleQueue {
  private tail: Promise<void> = Promise.resolve();

  enqueue<T>(operation: () => Promise<T>): Promise<T> {
    const result = this.tail.then(operation, operation);
    this.tail = result.then(
      () => undefined,
      () => undefined,
    );
    return result;
  }
}
