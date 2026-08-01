import test from "node:test";
import assert from "node:assert/strict";
import {
  collapseImageMarkers,
  imageMarkerSourcePath,
  splitImageMarkers,
} from "../src/lib/imageMarkers";

const CACHE_MARKER =
  "[Image: source: /Users/raymond/.claude/image-cache/0da57d2c-6591-467c-8abf-6961554736e0/2.png]";
// The qmux paste form: an absolute path with no "source:" prefix, delivered to
// the agent as text and rendered as a thumbnail in the queue.
const PASTE_MARKER = "[Image: /Users/raymond/.claude/image-cache/qmux-paste-42-0.png]";
const CODEX_IMAGE_BLOCK =
  '<image name=[Image] path="/var/folders/example/T/codex-clipboard-BvUGfw.png">\n</image>';
const NUMBERED_CODEX_IMAGE_BLOCK =
  '<image name=[Image #1] path="/var/folders/example/T/codex-clipboard-numbered.png">\n</image>';

test("splitImageMarkers returns plain text untouched", () => {
  assert.deepEqual(splitImageMarkers("fix the login bug"), [
    { kind: "text", text: "fix the login bug" },
  ]);
});

test("splitImageMarkers keeps empty text as a single text segment", () => {
  assert.deepEqual(splitImageMarkers(""), [{ kind: "text", text: "" }]);
});

test("splitImageMarkers isolates a marker-only message", () => {
  assert.deepEqual(splitImageMarkers(CACHE_MARKER), [{ kind: "image", text: CACHE_MARKER }]);
});

test("splitImageMarkers splits inline numbered references", () => {
  assert.deepEqual(splitImageMarkers("fix this [Image #1] and this [Image #2]"), [
    { kind: "text", text: "fix this " },
    { kind: "image", text: "[Image #1]" },
    { kind: "text", text: " and this " },
    { kind: "image", text: "[Image #2]" },
  ]);
});

test("splitImageMarkers handles cache paths containing spaces", () => {
  const marker = "[Image: source: /Users/raymond/My Files/image cache/1.png]";
  assert.deepEqual(splitImageMarkers(`look: ${marker}`), [
    { kind: "text", text: "look: " },
    { kind: "image", text: marker },
  ]);
});

test("splitImageMarkers does not match across lines or unclosed brackets", () => {
  const text = "[Image: source: /a\n/b] and [Image #x]";
  assert.deepEqual(splitImageMarkers(text), [{ kind: "text", text }]);
});

test("splitImageMarkers handles adjacent markers", () => {
  assert.deepEqual(splitImageMarkers(`${CACHE_MARKER}[Image #1]`), [
    { kind: "image", text: CACHE_MARKER },
    { kind: "image", text: "[Image #1]" },
  ]);
});

test("splitImageMarkers collapses a Codex clipboard image block", () => {
  assert.deepEqual(splitImageMarkers(`inspect this:\n${CODEX_IMAGE_BLOCK}\nthanks`), [
    { kind: "text", text: "inspect this:\n" },
    { kind: "image", text: CODEX_IMAGE_BLOCK },
    { kind: "text", text: "\nthanks" },
  ]);
});

test("splitImageMarkers accepts quoted Codex image names and reordered attributes", () => {
  const marker =
    "<image path='/var/folders/example/image.png' name=\"[Image]\">\r\n  </image>";
  assert.deepEqual(splitImageMarkers(marker), [{ kind: "image", text: marker }]);
});

test("splitImageMarkers accepts Codex's numbered clipboard image blocks", () => {
  assert.deepEqual(splitImageMarkers(NUMBERED_CODEX_IMAGE_BLOCK), [
    { kind: "image", text: NUMBERED_CODEX_IMAGE_BLOCK },
  ]);
});

test("splitImageMarkers leaves non-empty or incomplete image tags visible", () => {
  const text =
    '<image name=[Image] path="/tmp/a.png">caption</image> <image name=[Image] path="/tmp/b.png">';
  assert.deepEqual(splitImageMarkers(text), [{ kind: "text", text }]);
});

test("collapseImageMarkers replaces every marker shape with [Image]", () => {
  assert.equal(
    collapseImageMarkers(
      `before ${CACHE_MARKER} middle [Image #3] then ${CODEX_IMAGE_BLOCK} after`,
    ),
    "before [Image] middle [Image] then [Image] after",
  );
});

test("collapseImageMarkers leaves marker-free text unchanged", () => {
  assert.equal(collapseImageMarkers("nothing to see"), "nothing to see");
});

test("imageMarkerSourcePath extracts the cache path from a source marker", () => {
  assert.equal(
    imageMarkerSourcePath(CACHE_MARKER),
    "/Users/raymond/.claude/image-cache/0da57d2c-6591-467c-8abf-6961554736e0/2.png",
  );
});

test("imageMarkerSourcePath keeps interior spaces but trims edge whitespace", () => {
  assert.equal(
    imageMarkerSourcePath("[Image: source: /Users/raymond/My Files/image cache/1.png ]"),
    "/Users/raymond/My Files/image cache/1.png",
  );
});

test("imageMarkerSourcePath extracts paths from Codex image blocks", () => {
  assert.equal(
    imageMarkerSourcePath(CODEX_IMAGE_BLOCK),
    "/var/folders/example/T/codex-clipboard-BvUGfw.png",
  );
  assert.equal(
    imageMarkerSourcePath(NUMBERED_CODEX_IMAGE_BLOCK),
    "/var/folders/example/T/codex-clipboard-numbered.png",
  );
});

test("imageMarkerSourcePath returns null for pathless references and non-markers", () => {
  assert.equal(imageMarkerSourcePath("[Image #1]"), null);
  assert.equal(imageMarkerSourcePath("[Image: source: ]"), null);
  assert.equal(imageMarkerSourcePath("plain text"), null);
  assert.equal(imageMarkerSourcePath(`prefixed ${CACHE_MARKER}`), null);
});

test("splitImageMarkers isolates the qmux paste marker inline", () => {
  assert.deepEqual(splitImageMarkers(`what is this? ${PASTE_MARKER}`), [
    { kind: "text", text: "what is this? " },
    { kind: "image", text: PASTE_MARKER },
  ]);
});

test("splitImageMarkers does not treat bracketed prose as a paste marker", () => {
  // Only a leading-slash absolute path qualifies, so "[Image: figure 2]" stays text.
  const text = "see [Image: figure 2] below";
  assert.deepEqual(splitImageMarkers(text), [{ kind: "text", text }]);
});

test("imageMarkerSourcePath extracts the path from a qmux paste marker", () => {
  assert.equal(
    imageMarkerSourcePath(PASTE_MARKER),
    "/Users/raymond/.claude/image-cache/qmux-paste-42-0.png",
  );
});

test("collapseImageMarkers replaces the qmux paste marker too", () => {
  assert.equal(collapseImageMarkers(`look ${PASTE_MARKER} here`), "look [Image] here");
});
