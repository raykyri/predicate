import assert from "node:assert/strict";
import test from "node:test";
import {
  HumanBrowserLifecycleQueue,
  isHumanBrowserLifecycleBusy,
  retryHumanBrowserLifecycle,
} from "../src/lib/humanBrowserLifecycleQueue";

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

test("busy lifecycle errors retry and then succeed", async () => {
  assert.equal(isHumanBrowserLifecycleBusy("human browser lifecycle is busy; retry the request"), true);
  assert.equal(isHumanBrowserLifecycleBusy(new Error("failed to hide")), false);

  let attempts = 0;
  const result = await retryHumanBrowserLifecycle(async () => {
    attempts += 1;
    if (attempts < 3) {
      throw new Error("human browser lifecycle is busy; retry the request");
    }
    return "hidden";
  });
  assert.equal(attempts, 3);
  assert.equal(result, "hidden");
});

test("non-busy lifecycle errors fail immediately", async () => {
  let attempts = 0;
  await assert.rejects(
    retryHumanBrowserLifecycle(async () => {
      attempts += 1;
      throw new Error("failed to hide the new human browser");
    }),
    /failed to hide the new human browser/,
  );
  assert.equal(attempts, 1);
});
