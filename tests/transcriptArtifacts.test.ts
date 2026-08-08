import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import TranscriptMarkdown, {
  TranscriptLinkActionsProvider,
  transcriptMathPluginsReady,
  type LinkActions,
} from "../src/components/TranscriptMarkdown";

await transcriptMathPluginsReady;

const actions: LinkActions = {
  openLink: () => undefined,
  openLinkMenu: () => undefined,
  openCodexInlineVisualization: () => undefined,
};

function render(text: string, artifactLinks = true) {
  return renderToStaticMarkup(
    createElement(
      TranscriptLinkActionsProvider,
      { actions },
      createElement(TranscriptMarkdown, { text, artifactLinks }),
    ),
  );
}

function artifactButtonCount(html: string) {
  return html.match(/class="turn-markdown-artifact-open"/gu)?.length ?? 0;
}

test("plain loopback HTML URLs receive an adjacent launch button", () => {
  const html = render("Preview http://127.0.0.1:8631/mockup-1-unified.html now.");
  assert.equal(artifactButtonCount(html), 1);
  assert.match(html, /href="http:\/\/127\.0\.0\.1:8631\/mockup-1-unified\.html"/u);
  assert.doesNotMatch(html, />Open local HTML in browser</u);
});

test("an exact inline-code loopback HTML URL receives a launch button", () => {
  const html = render("`http://localhost/mockup.html`");
  assert.equal(artifactButtonCount(html), 1);
  assert.match(html, /<code>http:\/\/localhost\/mockup\.html<\/code>/u);
});

test("fenced code and non-exact inline code do not receive launch buttons", () => {
  assert.equal(
    artifactButtonCount(render("```\nhttp://localhost/mockup.html\n```")),
    0,
  );
  assert.equal(
    artifactButtonCount(render("`open http://localhost/mockup.html`")),
    0,
  );
});

test("only a standalone valid codex-inline-vis directive receives a launch button", () => {
  const directive = '::codex-inline-vis{file="artifact-tray-options.html"}';
  assert.equal(artifactButtonCount(render(directive)), 1);
  assert.equal(artifactButtonCount(render(`Open ${directive}`)), 0);
  assert.equal(artifactButtonCount(render(`\`${directive}\``)), 0);
  assert.equal(artifactButtonCount(render(`\`\`\`\n${directive}\n\`\`\``)), 0);
  assert.equal(
    artifactButtonCount(render('::codex-inline-vis{file="../artifact.html"}')),
    0,
  );
});

test("explicit markdown labels and non-loopback HTML do not gain buttons", () => {
  assert.equal(
    artifactButtonCount(render("[preview](http://localhost/mockup.html)")),
    0,
  );
  assert.equal(artifactButtonCount(render("https://example.com/mockup.html")), 0);
});

test("artifact controls stay scoped to transcript renderers that opt in", () => {
  assert.equal(
    artifactButtonCount(render("http://localhost/mockup.html", false)),
    0,
  );
});
