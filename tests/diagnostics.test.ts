import assert from "node:assert/strict";
import test from "node:test";
import {
  DiagnosticsBuffer,
  MainThreadStallDetector,
  SLOW_EVENT_BATCH_MS,
  STALL_THRESHOLD_MS,
  STALL_TICK_MS,
  noteEventBatch,
  startDiagnostics,
  summarizeEventBatch,
  type FrontendDiagnosticEntry,
} from "../src/lib/diagnostics";

test("breadcrumbs stay local and reports queue for the backend", () => {
  let now = 1_000;
  const buffer = new DiagnosticsBuffer(() => now);
  buffer.breadcrumb("ui.pane_switch", "switched to pane-1");
  now = 2_000;
  buffer.report("frontend.slow_event_batch", "42 events handled in 80ms", { total: 42 });

  const pending = buffer.drainPending();
  assert.equal(pending.length, 1);
  assert.equal(pending[0].category, "frontend.slow_event_batch");
  assert.equal(pending[0].timestamp, 2_000);
  assert.deepEqual(pending[0].data, { total: 42 });
  // Drained means drained.
  assert.deepEqual(buffer.drainPending(), []);
  // The report is also visible as a breadcrumb, after the earlier one.
  const breadcrumbs = buffer.recentBreadcrumbs(10);
  assert.equal(breadcrumbs.length, 2);
  assert.equal(breadcrumbs[0].category, "ui.pane_switch");
  assert.equal(breadcrumbs[1].category, "frontend.slow_event_batch");
});

test("breadcrumb ring and pending queue are bounded, with drops surfaced", () => {
  const buffer = new DiagnosticsBuffer(() => 0);
  for (let i = 0; i < 400; i += 1) {
    buffer.breadcrumb("test", `crumb ${i}`);
  }
  const recent = buffer.recentBreadcrumbs(1_000);
  assert.equal(recent.length, 300);
  assert.equal(recent[0].message, "crumb 100");
  assert.equal(recent[recent.length - 1].message, "crumb 399");

  for (let i = 0; i < 250; i += 1) {
    buffer.report("test", `report ${i}`);
  }
  const drained = buffer.drainPending();
  // 200 kept + 1 overflow marker leading the batch.
  assert.equal(drained.length, 201);
  assert.equal(drained[0].category, "frontend.diagnostics_overflow");
  assert.deepEqual(drained[0].data, { dropped: 50 });
  assert.equal(drained[1].message, "report 50");
  // The overflow marker is one-shot.
  buffer.report("test", "after");
  assert.equal(buffer.drainPending().length, 1);
});

test("summarizeEventBatch builds a capped, sorted histogram and flags slow batches", () => {
  const types = [
    ...Array(5).fill("turn.appended"),
    ...Array(3).fill("agent.status_changed"),
    "pane.removed",
  ];
  const fast = summarizeEventBatch(types, SLOW_EVENT_BATCH_MS - 1);
  assert.equal(fast.total, 9);
  assert.equal(fast.slow, false);
  assert.deepEqual(fast.byType[0], ["turn.appended", 5]);
  assert.deepEqual(fast.byType[1], ["agent.status_changed", 3]);

  const slow = summarizeEventBatch(types, SLOW_EVENT_BATCH_MS);
  assert.equal(slow.slow, true);

  const many = summarizeEventBatch(
    Array.from({ length: 20 }, (_, i) => `type-${i}`),
    0,
  );
  assert.equal(many.byType.length, 8);
});

test("noteEventBatch breadcrumbs fast batches and reports slow ones", () => {
  const buffer = new DiagnosticsBuffer(() => 0);
  noteEventBatch(["turn.appended", "turn.appended"], 5, buffer);
  assert.deepEqual(buffer.drainPending(), []);
  assert.equal(buffer.recentBreadcrumbs(10)[0].category, "events.batch");

  noteEventBatch(Array(30).fill("turn.appended"), 120, buffer);
  const pending = buffer.drainPending();
  assert.equal(pending.length, 1);
  assert.equal(pending[0].category, "frontend.slow_event_batch");
  const data = pending[0].data as { total: number; durationMs: number; slow: boolean };
  assert.equal(data.total, 30);
  assert.equal(data.durationMs, 120);
  assert.equal(data.slow, true);
});

test("stall detector reports only ticks past the threshold, measured from schedule", () => {
  const detector = new MainThreadStallDetector(STALL_TICK_MS, STALL_THRESHOLD_MS);
  // First tick is baseline only.
  assert.equal(detector.noteTick(10_000), null);
  // On-time tick: quiet.
  assert.equal(detector.noteTick(10_000 + STALL_TICK_MS), null);
  // Late but under threshold: quiet.
  assert.equal(
    detector.noteTick(10_000 + STALL_TICK_MS * 2 + STALL_THRESHOLD_MS - 1),
    null,
  );
  // A genuine stall reports how far past schedule the tick landed. The
  // previous tick already ran late, so schedule counts from it.
  const last = 10_000 + STALL_TICK_MS * 2 + STALL_THRESHOLD_MS - 1;
  const stalled = detector.noteTick(last + STALL_TICK_MS + 4_000);
  assert.equal(stalled, 4_000);
});

test("startDiagnostics heartbeats, flushes, and reports stalls with breadcrumbs", () => {
  let now = 0;
  const buffer = new DiagnosticsBuffer(() => now);
  const heartbeats: number[] = [];
  const batches: FrontendDiagnosticEntry[][] = [];
  const timers = new Map<number, { handler: () => void; ms: number }>();
  let nextTimer = 1;

  const stop = startDiagnostics(
    {
      heartbeat: async () => {
        heartbeats.push(now);
      },
      recordBatch: async (entries) => {
        batches.push(entries);
      },
    },
    {
      buffer,
      now: () => now,
      setInterval: (handler, ms) => {
        const id = nextTimer;
        nextTimer += 1;
        timers.set(id, { handler, ms });
        return id;
      },
      clearInterval: (id) => {
        timers.delete(id);
      },
    },
  );

  assert.equal(timers.size, 3);
  const stallTimer = [...timers.values()].find((timer) => timer.ms === STALL_TICK_MS);
  assert.ok(stallTimer, "a stall watchdog timer is scheduled");

  // Heartbeat timer fires the transport.
  const heartbeatTimer = [...timers.values()].find((timer) => timer.ms === 1_000);
  heartbeatTimer?.handler();
  assert.equal(heartbeats.length, 1);

  // On-time watchdog ticks stay quiet.
  now = 1_000;
  stallTimer?.handler();
  now = 1_500;
  stallTimer?.handler();
  assert.equal(batches.length, 0);

  // A tick arriving 5s late is a stall: reported and flushed immediately.
  buffer.breadcrumb("ui.pane_switch", "switched to terminal pane-3");
  now = 1_500 + STALL_TICK_MS + 5_000;
  stallTimer?.handler();
  assert.equal(batches.length, 1);
  const stall = batches[0].find(
    (entry) => entry.category === "frontend.main_thread_stall",
  );
  assert.ok(stall, "stall record forwarded");
  const stallData = stall?.data as { stallMs: number; breadcrumbs: FrontendDiagnosticEntry[] };
  assert.equal(stallData.stallMs, 5_000);
  assert.ok(
    stallData.breadcrumbs.some((crumb) => crumb.category === "ui.pane_switch"),
    "stall report carries the breadcrumbs leading up to it",
  );

  // Flush timer forwards queued reports and skips empty queues.
  buffer.report("frontend.slow_event_batch", "queued");
  const flushTimer = [...timers.values()].find((timer) => timer.ms === 2_000);
  flushTimer?.handler();
  assert.equal(batches.length, 2);
  flushTimer?.handler();
  assert.equal(batches.length, 2);

  stop();
  assert.equal(timers.size, 0);
});
