import assert from "node:assert/strict";
import test from "node:test";
import {
  fitTerminalPipFontSize,
  formatTerminalPipText,
  shouldShowTerminalPip,
  shouldShowTerminalPipToggle,
  TERMINAL_PIP_MAX_FONT_SIZE,
  TERMINAL_PIP_MIN_FONT_SIZE,
} from "../src/lib/terminalPip";

test("terminal PiP toggle is only offered for an expanded right pane", () => {
  const visible = {
    transcriptExpanded: true,
    rightPaneCollapsed: false,
    nativeTerminalAvailable: true,
  };
  assert.equal(shouldShowTerminalPipToggle(visible), true);
  assert.equal(shouldShowTerminalPipToggle({ ...visible, transcriptExpanded: false }), false);
  assert.equal(shouldShowTerminalPipToggle({ ...visible, rightPaneCollapsed: true }), false);
  assert.equal(shouldShowTerminalPipToggle({ ...visible, nativeTerminalAvailable: false }), false);
});

test("terminal PiP is opt-in for an open expanded transcript", () => {
  const visible = {
    transcriptExpanded: true,
    toggledOn: true,
    rightPaneCollapsed: false,
    nativeTerminalAvailable: true,
  };
  assert.equal(shouldShowTerminalPip(visible), true);
  assert.equal(shouldShowTerminalPip({ ...visible, toggledOn: false }), false);
  assert.equal(shouldShowTerminalPip({ ...visible, transcriptExpanded: false }), false);
  assert.equal(shouldShowTerminalPip({ ...visible, rightPaneCollapsed: true }), false);
  assert.equal(shouldShowTerminalPip({ ...visible, nativeTerminalAvailable: false }), false);
});

test("formatTerminalPipText preserves trailing blank rows for the grid shape", () => {
  assert.equal(formatTerminalPipText("hello\nworld\n\n\n"), "hello\nworld\n\n\n");
});

test("formatTerminalPipText normalizes CR and CRLF", () => {
  // Content is preserved verbatim apart from the line endings; the component
  // drops the extra artifact line a trailing newline splits into.
  assert.equal(formatTerminalPipText("a\r\nb\rc\n"), "a\nb\nc\n");
});

test("fitTerminalPipFontSize is width-bound for wide grids", () => {
  // 420 / (80 * 0.6) = 8.75 < 342 / (24 * 1.2) = 11.875.
  assert.equal(fitTerminalPipFontSize(80, 24, 0.6, 420, 342), 8.75);
});

test("fitTerminalPipFontSize is height-bound for tall grids", () => {
  // 120 / (10 * 1.2) = 10 < 800 / (10 * 0.6) = 133.
  assert.equal(fitTerminalPipFontSize(10, 10, 0.6, 800, 120), 10);
});

test("fitTerminalPipFontSize clamps to the min when the box is too small", () => {
  assert.equal(
    fitTerminalPipFontSize(200, 60, 0.6, 200, 100),
    TERMINAL_PIP_MIN_FONT_SIZE,
  );
});

test("fitTerminalPipFontSize caps narrow grids at the max", () => {
  assert.equal(
    fitTerminalPipFontSize(10, 5, 0.6, 1000, 1000),
    TERMINAL_PIP_MAX_FONT_SIZE,
  );
});

test("fitTerminalPipFontSize falls back to the min on degenerate input", () => {
  assert.equal(fitTerminalPipFontSize(0, 24, 0.6, 420, 342), TERMINAL_PIP_MIN_FONT_SIZE);
  assert.equal(fitTerminalPipFontSize(80, 24, 0, 420, 342), TERMINAL_PIP_MIN_FONT_SIZE);
});
