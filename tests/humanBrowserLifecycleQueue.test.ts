import assert from "node:assert/strict";
import test from "node:test";
import { HumanBrowserLifecycleQueue } from "../src/lib/humanBrowserLifecycleQueue";

test("human browser lifecycle operations never overlap", async () => {
  const queue = new HumanBrowserLifecycleQueue();
  const events: string[] = [];
  let releaseFirst!: () => void;
  const firstGate = new Promise<void>((resolve) => {
    releaseFirst = resolve;
  });

  const first = queue.enqueue(async () => {
    events.push("first:start");
    await firstGate;
    events.push("first:end");
  });
  const second = queue.enqueue(async () => {
    events.push("second:start");
    events.push("second:end");
  });

  await Promise.resolve();
  assert.deepEqual(events, ["first:start"]);
  releaseFirst();
  await Promise.all([first, second]);
  assert.deepEqual(events, ["first:start", "first:end", "second:start", "second:end"]);
});

test("a rejected lifecycle operation does not poison later cleanup", async () => {
  const queue = new HumanBrowserLifecycleQueue();
  const failure = queue.enqueue(async () => {
    throw new Error("WebKit failed");
  });
  const cleanup = queue.enqueue(async () => "destroyed");

  await assert.rejects(failure, /WebKit failed/);
  assert.equal(await cleanup, "destroyed");
});
