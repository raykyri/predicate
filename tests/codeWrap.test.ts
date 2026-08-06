import assert from "node:assert/strict";
import test from "node:test";

// The store reads its persisted value while the module initializes, so the
// storage stub has to be installed before the import.
const stored = new Map<string, string>();
(globalThis as { localStorage?: unknown }).localStorage = {
  getItem: (key: string) => stored.get(key) ?? null,
  setItem: (key: string, value: string) => {
    stored.set(key, value);
  },
};

const { getCodeWrap, setCodeWrap, subscribeCodeWrap } = await import("../src/lib/codeWrap");

test("every subscriber sees the one app-wide wrap value", () => {
  setCodeWrap(false);
  const blockA: boolean[] = [];
  const blockB: boolean[] = [];
  const unsubscribeA = subscribeCodeWrap(() => blockA.push(getCodeWrap()));
  const unsubscribeB = subscribeCodeWrap(() => blockB.push(getCodeWrap()));

  setCodeWrap(true);
  // Re-setting the value it already holds must not churn subscribers: every
  // code block would re-render (and re-anchor) for no visible change.
  setCodeWrap(true);
  setCodeWrap(false);
  unsubscribeA();
  unsubscribeB();
  setCodeWrap(true);

  assert.deepEqual(blockA, [true, false]);
  assert.deepEqual(blockB, [true, false]);
  assert.equal(getCodeWrap(), true);
});

test("the wrap choice is persisted for the next launch", () => {
  setCodeWrap(true);
  assert.equal(stored.get("qmux.code-wrap.v1"), "true");
  setCodeWrap(false);
  assert.equal(stored.get("qmux.code-wrap.v1"), "false");
});
