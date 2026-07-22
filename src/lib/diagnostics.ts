// Frontend half of the freeze diagnostics (see src-tauri/src/diagnostics.rs
// for the backend half and the durable JSONL log both halves land in).
//
// The freeze under investigation: a heavy research/encyclopedia run streams
// backend events while the user switches to a native terminal pane, and the
// webview's main thread stalls. This module gives that moment a paper trail:
//
// - breadcrumbs: a cheap in-memory ring of "what just happened" (event
//   batches, pane switches, errors) that never leaves the process on its own;
// - reports: notable observations (main-thread stalls, slow event batches,
//   uncaught errors) queued for forwarding to the backend, which appends them
//   to the durable log — each stall report carries the breadcrumbs leading up
//   to it, which is the actual "what was going on right before it froze";
// - a heartbeat + stall watchdog: heartbeats tell the backend watchdog the
//   main thread is alive; a watchdog tick arriving very late *is* a measured
//   main-thread stall, reported with its duration once the thread recovers.
//
// Everything in this file is pure and dependency-free so it can be unit
// tested; the timers and Tauri invokes are injected via startDiagnostics
// (wired in main.tsx).

/** Matches the backend's FrontendDiagnosticEntry (diagnostics.rs). */
export interface FrontendDiagnosticEntry {
  timestamp: number;
  category: string;
  message: string;
  data?: unknown;
}

/** A stored record as returned by the backend's diagnostics_snapshot. */
export interface DiagnosticRecord {
  timestamp: number;
  source: "frontend" | "backend";
  category: string;
  message: string;
  data?: unknown;
}

/** Breadcrumbs kept in memory for stall context. */
const BREADCRUMB_CAPACITY = 300;
/** Reports queued for the backend; beyond this the oldest are dropped (and counted). */
const PENDING_CAPACITY = 200;
/** Breadcrumbs attached to a stall/error report. */
const BREADCRUMBS_IN_REPORT = 25;
/** Event batches slower than this are reported, not just breadcrumbed. */
export const SLOW_EVENT_BATCH_MS = 50;
/** Event types listed per batch summary. */
const BATCH_SUMMARY_TOP = 8;
/** Watchdog tick cadence (main.tsx schedules at this interval). */
export const STALL_TICK_MS = 500;
/** A tick this much later than scheduled counts as a main-thread stall. */
export const STALL_THRESHOLD_MS = 1_000;
/** Heartbeat cadence — keep well under the backend's 6s stall cutoff. */
export const HEARTBEAT_INTERVAL_MS = 1_000;
/** How often queued reports are flushed to the backend. */
export const FLUSH_INTERVAL_MS = 2_000;

export class DiagnosticsBuffer {
  private breadcrumbs: FrontendDiagnosticEntry[] = [];
  private pending: FrontendDiagnosticEntry[] = [];
  private droppedPending = 0;
  private readonly now: () => number;

  constructor(now: () => number = () => Date.now()) {
    this.now = now;
  }

  /** Cheap local trace: ring only, never forwarded on its own. */
  breadcrumb(category: string, message: string, data?: unknown): void {
    const entry: FrontendDiagnosticEntry = {
      timestamp: this.now(),
      category,
      message,
      ...(data === undefined ? {} : { data }),
    };
    this.breadcrumbs.push(entry);
    if (this.breadcrumbs.length > BREADCRUMB_CAPACITY) {
      this.breadcrumbs.splice(0, this.breadcrumbs.length - BREADCRUMB_CAPACITY);
    }
  }

  /** Notable observation: breadcrumbed and queued for the backend log. */
  report(category: string, message: string, data?: unknown): void {
    this.breadcrumb(category, message, data);
    if (this.pending.length >= PENDING_CAPACITY) {
      this.pending.shift();
      this.droppedPending += 1;
    }
    this.pending.push({
      timestamp: this.now(),
      category,
      message,
      ...(data === undefined ? {} : { data }),
    });
  }

  /** Newest-last slice of the breadcrumb ring for attaching to reports. */
  recentBreadcrumbs(limit: number = BREADCRUMBS_IN_REPORT): FrontendDiagnosticEntry[] {
    return this.breadcrumbs.slice(-limit);
  }

  /**
   * Takes everything queued for the backend. If the queue overflowed since the
   * last drain, the batch leads with a record saying how many were lost — a
   * silent gap in the log would read as "nothing happened", the opposite of
   * the truth under exactly the load being diagnosed.
   */
  drainPending(): FrontendDiagnosticEntry[] {
    const drained = this.pending.splice(0, this.pending.length);
    if (this.droppedPending > 0) {
      drained.unshift({
        timestamp: this.now(),
        category: "frontend.diagnostics_overflow",
        message: `dropped ${this.droppedPending} queued diagnostic records`,
        data: { dropped: this.droppedPending },
      });
      this.droppedPending = 0;
    }
    return drained;
  }
}

export interface EventBatchSummary {
  total: number;
  durationMs: number;
  /** [event type, count], highest count first, capped at BATCH_SUMMARY_TOP. */
  byType: [string, number][];
  slow: boolean;
}

/** Histogram + slowness verdict for one coalesced event batch. */
export function summarizeEventBatch(types: string[], durationMs: number): EventBatchSummary {
  const counts = new Map<string, number>();
  for (const type of types) {
    counts.set(type, (counts.get(type) ?? 0) + 1);
  }
  const byType = [...counts.entries()]
    .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
    .slice(0, BATCH_SUMMARY_TOP);
  return {
    total: types.length,
    durationMs: Math.round(durationMs),
    byType,
    slow: durationMs >= SLOW_EVENT_BATCH_MS,
  };
}

/**
 * Detects main-thread stalls from watchdog tick arrival times: a timer
 * scheduled every `intervalMs` that fires `thresholdMs`+ late means the main
 * thread (or the whole process) was blocked for that long. Reported from the
 * recovery side of the stall — the backend heartbeat watchdog covers the case
 * where the thread never recovers.
 */
export class MainThreadStallDetector {
  private lastTickAt: number | null = null;
  private readonly intervalMs: number;
  private readonly thresholdMs: number;

  constructor(intervalMs: number = STALL_TICK_MS, thresholdMs: number = STALL_THRESHOLD_MS) {
    this.intervalMs = intervalMs;
    this.thresholdMs = thresholdMs;
  }

  /** Returns the stall duration (ms past the scheduled fire time), or null. */
  noteTick(now: number): number | null {
    const last = this.lastTickAt;
    this.lastTickAt = now;
    if (last === null) {
      return null;
    }
    const late = now - last - this.intervalMs;
    return late >= this.thresholdMs ? late : null;
  }
}

/** The app-wide diagnostics buffer. Import this to leave breadcrumbs/reports. */
export const diagnostics = new DiagnosticsBuffer();

/**
 * Called by the event hook for every coalesced backend-event batch. Every
 * batch leaves a breadcrumb (so a stall report shows what was streaming);
 * slow batches — handling time on the main thread, the thing that freezes —
 * are reported to the durable log with their type histogram.
 */
export function noteEventBatch(
  types: string[],
  durationMs: number,
  buffer: DiagnosticsBuffer = diagnostics,
): void {
  const summary = summarizeEventBatch(types, durationMs);
  const message = `${summary.total} events handled in ${summary.durationMs}ms`;
  if (summary.slow) {
    buffer.report("frontend.slow_event_batch", message, summary);
  } else {
    buffer.breadcrumb("events.batch", message, summary);
  }
}

export interface DiagnosticsTransport {
  recordBatch: (entries: FrontendDiagnosticEntry[]) => Promise<void>;
  heartbeat: () => Promise<void>;
}

export interface DiagnosticsRuntimeOptions {
  buffer?: DiagnosticsBuffer;
  /** Timer/clock injection for tests; defaults to the real ones. */
  setInterval?: (handler: () => void, ms: number) => number;
  clearInterval?: (id: number) => void;
  now?: () => number;
  /** Error listeners are attached to this when provided (window in the app). */
  target?: Pick<Window, "addEventListener" | "removeEventListener">;
}

/**
 * Starts the timers that make diagnostics live: heartbeats to the backend
 * watchdog, periodic report flushing, the main-thread stall watchdog, and
 * uncaught-error capture. Returns a stop function. All IO is fire-and-forget
 * and error-swallowing — diagnostics must never break the app.
 */
export function startDiagnostics(
  transport: DiagnosticsTransport,
  options: DiagnosticsRuntimeOptions = {},
): () => void {
  const buffer = options.buffer ?? diagnostics;
  const setTimer = options.setInterval ?? ((handler, ms) => window.setInterval(handler, ms));
  const clearTimer = options.clearInterval ?? ((id) => window.clearInterval(id));
  const now = options.now ?? (() => Date.now());
  const target = options.target ?? (typeof window === "undefined" ? undefined : window);

  const flush = () => {
    const entries = buffer.drainPending();
    if (entries.length > 0) {
      void transport.recordBatch(entries).catch(() => undefined);
    }
  };

  const heartbeatTimer = setTimer(() => {
    void transport.heartbeat().catch(() => undefined);
  }, HEARTBEAT_INTERVAL_MS);
  const flushTimer = setTimer(flush, FLUSH_INTERVAL_MS);

  const detector = new MainThreadStallDetector();
  const stallTimer = setTimer(() => {
    const stallMs = detector.noteTick(now());
    if (stallMs !== null) {
      buffer.report(
        "frontend.main_thread_stall",
        `main thread stalled for ${Math.round(stallMs)}ms`,
        { stallMs: Math.round(stallMs), breadcrumbs: buffer.recentBreadcrumbs() },
      );
      // Ship it now: after a beachball the user may quit before a scheduled
      // flush, and this record is the whole point of the exercise.
      flush();
    }
  }, STALL_TICK_MS);

  const onError = (event: Event) => {
    const message =
      event instanceof ErrorEvent ? event.message : "uncaught error (no message)";
    buffer.report("frontend.uncaught_error", message, {
      breadcrumbs: buffer.recentBreadcrumbs(),
    });
  };
  const onRejection = (event: Event) => {
    const reason = (event as PromiseRejectionEvent).reason;
    buffer.report(
      "frontend.unhandled_rejection",
      reason instanceof Error ? reason.message : String(reason),
      { breadcrumbs: buffer.recentBreadcrumbs() },
    );
  };
  target?.addEventListener("error", onError);
  target?.addEventListener("unhandledrejection", onRejection);

  return () => {
    clearTimer(heartbeatTimer);
    clearTimer(flushTimer);
    clearTimer(stallTimer);
    target?.removeEventListener("error", onError);
    target?.removeEventListener("unhandledrejection", onRejection);
  };
}
