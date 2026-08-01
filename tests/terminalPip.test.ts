import assert from "node:assert/strict";
import test from "node:test";
import { formatTerminalPipText } from "../src/lib/terminalPip";

test("formatTerminalPipText drops trailing blank rows", () => {
  assert.equal(formatTerminalPipText("hello\nworld\n\n\n"), "hello\nworld");
});

test("formatTerminalPipText keeps the last N lines", () => {
  const lines = Array.from({ length: 30 }, (_, index) => `line-${index}`);
  const formatted = formatTerminalPipText(lines.join("\n"), 5);
  assert.equal(formatted, "line-25\nline-26\nline-27\nline-28\nline-29");
});

test("formatTerminalPipText normalizes CR and CRLF", () => {
  assert.equal(formatTerminalPipText("a\r\nb\rc\n"), "a\nb\nc");
});
