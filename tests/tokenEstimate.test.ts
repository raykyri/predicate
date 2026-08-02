import assert from "node:assert/strict";
import test from "node:test";
import {
  estimateTokenCount,
  formatEstimatedTokenCount,
} from "../src/lib/tokenEstimate";

test("estimates ASCII text at roughly four characters per token", () => {
  assert.equal(estimateTokenCount(""), 0);
  assert.equal(estimateTokenCount("hello world!"), 3);
  assert.equal(estimateTokenCount("a".repeat(4_000)), 1_000);
});

test("weights non-ASCII text more heavily than ASCII text", () => {
  assert.equal(estimateTokenCount("abcd"), 1);
  assert.equal(estimateTokenCount("你好世界"), 4);
  assert.equal(estimateTokenCount("😀😀"), 2);
  assert.equal(estimateTokenCount("界".repeat(10_000)), 10_000);
});

test("formats estimates as compact token labels", () => {
  assert.equal(formatEstimatedTokenCount(""), "~0 tok");
  assert.equal(formatEstimatedTokenCount("a".repeat(4_000)), "~1.0k tok");
  assert.equal(formatEstimatedTokenCount("a".repeat(400_000)), "~100k tok");
});
