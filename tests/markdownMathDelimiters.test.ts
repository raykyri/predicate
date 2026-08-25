import assert from "node:assert/strict";
import test from "node:test";
import { normalizeLatexMathDelimiters } from "../src/lib/markdownMathDelimiters";

test("normalizes block and inline LaTeX delimiters", () => {
  assert.equal(normalizeLatexMathDelimiters("\\[\nx^2\n\\]"), "$$\nx^2\n$$");
  assert.equal(normalizeLatexMathDelimiters("before \\(x^2\\) after"), "before $x^2$ after");
});

test("leaves unmatched and explicitly escaped delimiters untouched", () => {
  assert.equal(normalizeLatexMathDelimiters("unmatched \\(x"), "unmatched \\(x");
  assert.equal(normalizeLatexMathDelimiters("literal \\\\(x\\)"), "literal \\\\(x\\)");
});

test("does not normalize delimiters in Markdown code", () => {
  const markdown = [
    "`\\(inline\\)`",
    "````",
    "\\[",
    "fenced",
    "\\]",
    "````",
    "    \\(indented\\)",
    "outside \\(math\\)",
  ].join("\n");
  const normalized = normalizeLatexMathDelimiters(markdown);
  assert.equal(
    normalized,
    [
      "`\\(inline\\)`",
      "````",
      "\\[",
      "fenced",
      "\\]",
      "````",
      "    \\(indented\\)",
      "outside $math$",
    ].join("\n"),
  );
});

test("supports tilde fences and backtick code spans with embedded runs", () => {
  assert.equal(
    normalizeLatexMathDelimiters("~~~tex\n\\(literal\\)\n~~~\n\\(math\\)"),
    "~~~tex\n\\(literal\\)\n~~~\n$math$",
  );
  assert.equal(
    normalizeLatexMathDelimiters("``code ` \\(literal\\)`` then \\(math\\)"),
    "``code ` \\(literal\\)`` then $math$",
  );
});
