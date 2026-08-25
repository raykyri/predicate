import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import TranscriptMarkdown, {
  transcriptMathPluginsReady,
} from "../src/components/TranscriptMarkdown";

// The math pipeline lives in a lazy-loaded chunk; server-side renders (and
// these tests) must wait for it before the first render includes MathJax.
await transcriptMathPluginsReady;

function render(text: string, inline = false) {
  return renderToStaticMarkup(createElement(TranscriptMarkdown, { text, inline }));
}

test("inline TeX renders as a MathJax SVG container", () => {
  const html = render("Euler: $e^{i\\pi}+1=0$");
  assert.match(html, /<mjx-container class="MathJax" jax="SVG">/);
  assert.match(html, /<svg /);
  // The raw TeX source must not leak into the rendered text.
  assert.equal(html.includes("e^{i\\pi}"), false);
});

test("fenced display TeX renders as block math", () => {
  const html = render("$$\n\\int_0^1 x^2\\,dx = \\frac{1}{3}\n$$");
  assert.match(html, /<mjx-container class="MathJax" jax="SVG" display="true">/);
});

test("LaTeX bracket delimiters render as block math", () => {
  const html = render(
    "\\[\n\\text{competitive deployment}\n\\rightarrow\n\\text{deep dependence}\n\\]",
  );
  assert.match(html, /<mjx-container class="MathJax" jax="SVG" display="true">/);
  assert.equal(html.includes("competitive deployment"), false);
  assert.equal(html.includes("\\rightarrow"), false);
});

test("LaTeX parenthesis delimiters render as inline math", () => {
  const html = render("Euler: \\(e^{i\\pi}+1=0\\).");
  assert.match(html, /<mjx-container class="MathJax" jax="SVG">/);
  assert.equal(html.includes("e^{i\\pi}"), false);
});

test("a standalone single-line $$…$$ paragraph is promoted to block math", () => {
  const html = render("$$\\int_0^1 x^2\\,dx = \\frac{1}{3}$$");
  assert.match(html, /<mjx-container class="MathJax" jax="SVG" display="true">/);
});

test("$$…$$ inside a sentence stays inline math", () => {
  const html = render("mid $$a+b$$ sentence");
  assert.match(html, /<mjx-container class="MathJax" jax="SVG">/);
  assert.equal(html.includes('display="true">'), false);
});

test("MathJax container styles are injected alongside rendered math", () => {
  const html = render("$x$");
  assert.match(html, /<style>[^<]*mjx-container/);
});

test("dollar amounts in prose stay plain text", () => {
  const html = render("The first costs $5 and the second $10 more.");
  assert.equal(html.includes("mjx-container"), false);
  assert.equal(html.includes("The first costs $5 and the second $10 more."), true);
});

test("per-unit prices stay plain text", () => {
  const html = render("about $3/M input and $15/M output");
  assert.equal(html.includes("mjx-container"), false);
  assert.equal(html.includes("about $3/M input and $15/M output"), true);
});

test("TeX inside code spans and fences is left literal", () => {
  const inlineCode = render("Use `$x^2$` in your prompt.");
  assert.equal(inlineCode.includes("mjx-container"), false);
  assert.equal(inlineCode.includes("$x^2$"), true);

  const fence = render("```\n$$a+b$$\n```");
  assert.equal(fence.includes("mjx-container"), false);
  assert.equal(fence.includes("$$a+b$$"), true);

  const alternateInlineCode = render("Use `\\(x^2\\)` in your prompt.");
  assert.equal(alternateInlineCode.includes("mjx-container"), false);
  assert.equal(alternateInlineCode.includes("\\(x^2\\)"), true);

  const alternateFence = render("```tex\n\\[\na+b\n\\]\n```");
  assert.equal(alternateFence.includes("mjx-container"), false);
  assert.equal(alternateFence.includes("\\["), true);
});

test("markdown without math renders no MathJax artifacts", () => {
  const html = render("Just **bold** text.");
  assert.equal(html.includes("mjx-container"), false);
  assert.equal(html.includes("<style>"), false);
});

test("inline transcript contexts render math without block promotion", () => {
  const html = render("title with $a^2+b^2=c^2$", true);
  assert.match(html, /<mjx-container class="MathJax" jax="SVG">/);
});
