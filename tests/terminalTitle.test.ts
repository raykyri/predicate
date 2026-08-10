import assert from "node:assert/strict";
import test from "node:test";
import { sanitizeTerminalTitle } from "../src/lib/terminalTitle";

test("sanitizes OpenCode's terminal title prefix", () => {
  assert.equal(sanitizeTerminalTitle("OC | Fix the build", "opencode"), "Fix the build");
  assert.equal(sanitizeTerminalTitle("OC |", "opencode"), null);
  assert.equal(
    sanitizeTerminalTitle(`OC | ${"x".repeat(200)}`, "opencode"),
    `${"x".repeat(159)}…`,
  );
  assert.equal(
    sanitizeTerminalTitle(`OC | ${"x".repeat(160)}`, "opencode"),
    "x".repeat(160),
  );
});

test("only strips the OpenCode prefix at the beginning", () => {
  assert.equal(sanitizeTerminalTitle("my OC | notes", "opencode"), "my OC | notes");
  assert.equal(sanitizeTerminalTitle("OC | Fix the build", "claude"), "OC | Fix the build");
});

test("retains existing Grok title normalization", () => {
  assert.equal(sanitizeTerminalTitle("Fix the build - GROK", "grok"), "Fix the build");
});
