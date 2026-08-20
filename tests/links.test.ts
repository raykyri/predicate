import assert from "node:assert/strict";
import { test } from "node:test";
import {
  QMUX_FILE_HREF_PREFIX,
  absoluteLocalFilePath,
  canPreviewLocalFilePath,
  isFileServerUrl,
  isQmuxFileHref,
  loopbackHtmlUrl,
  pathFromQmuxFileHref,
  safeHref,
} from "../src/lib/links";

test("loopbackHtmlUrl recognizes only loopback HTML documents", () => {
  assert.equal(
    loopbackHtmlUrl("http://127.0.0.1:8631/mockup-1-unified.html"),
    "http://127.0.0.1:8631/mockup-1-unified.html",
  );
  assert.equal(
    loopbackHtmlUrl("https://localhost/mockup.HTML?mode=compact#result"),
    "https://localhost/mockup.HTML?mode=compact#result",
  );
  assert.equal(loopbackHtmlUrl("http://localhost/app.js"), undefined);
  assert.equal(loopbackHtmlUrl("http://localhost.example.com/mockup.html"), undefined);
  assert.equal(loopbackHtmlUrl("http://user@localhost/mockup.html"), undefined);
  assert.equal(loopbackHtmlUrl(" http://localhost/mockup.html"), undefined);
});

test("safeHref keeps real http(s)/mailto URLs", () => {
  assert.equal(safeHref("https://example.com/a"), "https://example.com/a");
  assert.equal(safeHref("http://localhost:5173/"), "http://localhost:5173/");
  assert.equal(safeHref("mailto:hi@example.com"), "mailto:hi@example.com");
});

test("safeHref blocks javascript and custom schemes", () => {
  assert.equal(safeHref("javascript:alert(1)"), undefined);
  assert.equal(safeHref("tauri://localhost/"), undefined);
  assert.equal(safeHref("asset://localhost/etc/passwd"), undefined);
});

test("safeHref does not promote absolute Unix paths to https://qmux.invalid", () => {
  const path = "/Users/raymond/Code/multitool/dev/menubar-design-variants.html";
  const href = safeHref(path);
  assert.equal(href, `${QMUX_FILE_HREF_PREFIX}${path}`);
  assert.ok(href && !href.startsWith("https://"), `got ${href}`);
  assert.equal(pathFromQmuxFileHref(href!), path);
});

test("safeHref recognizes file: URLs and common filesystem roots", () => {
  assert.equal(
    safeHref("file:///Users/raymond/report.html"),
    `${QMUX_FILE_HREF_PREFIX}/Users/raymond/report.html`,
  );
  assert.equal(
    absoluteLocalFilePath("/home/ray/out/diagram.svg"),
    "/home/ray/out/diagram.svg",
  );
  assert.equal(
    absoluteLocalFilePath("/tmp/preview.html"),
    "/tmp/preview.html",
  );
});

test("safeHref does not treat ordinary site-relative paths as files", () => {
  // /docs/intro has no file extension and no known FS root — leave it alone.
  // Resolving against the dummy base would make https://qmux.invalid/docs/intro,
  // which is still not a navigable real URL we want to surface; safeHref keeps
  // that behavior for non-file absolute paths (https on the dummy host).
  const docs = safeHref("/docs/intro");
  assert.equal(docs, "https://qmux.invalid/docs/intro");
  assert.equal(absoluteLocalFilePath("/docs/intro"), undefined);
});

test("absoluteLocalFilePath accepts extension-bearing multi-segment paths", () => {
  assert.equal(
    absoluteLocalFilePath("/workspace/out/report.html"),
    "/workspace/out/report.html",
  );
  assert.equal(absoluteLocalFilePath("/only-one-segment.html"), undefined);
});

test("isQmuxFileHref and pathFromQmuxFileHref round-trip", () => {
  const path = "/Users/me/file.html";
  const href = `${QMUX_FILE_HREF_PREFIX}${path}`;
  assert.equal(isQmuxFileHref(href), true);
  assert.equal(isQmuxFileHref("https://example.com"), false);
  assert.equal(pathFromQmuxFileHref(href), path);
  assert.equal(pathFromQmuxFileHref("https://example.com"), undefined);
});

test("local preview hints allow renderable files and reject binary packages", () => {
  assert.equal(canPreviewLocalFilePath("/tmp/report.HTML"), true);
  assert.equal(canPreviewLocalFilePath("/tmp/notes.markdown"), true);
  assert.equal(canPreviewLocalFilePath("C:\\tmp\\chart.PNG"), true);
  assert.equal(canPreviewLocalFilePath("/tmp/qmux_0.3.1_universal.dmg"), false);
  assert.equal(canPreviewLocalFilePath("/tmp/installer.pkg"), false);
  assert.equal(canPreviewLocalFilePath("/tmp/archive.zip"), false);
  assert.equal(canPreviewLocalFilePath("/tmp/no-extension"), false);
});

test("local preview hints cover renderable source files and location suffixes", () => {
  assert.equal(canPreviewLocalFilePath("/repo/src/client/session.rs"), true);
  assert.equal(canPreviewLocalFilePath("/repo/src/client/session.rs:8"), true);
  assert.equal(canPreviewLocalFilePath("/repo/src/client/session.rs:8:14"), true);
  assert.equal(canPreviewLocalFilePath("/repo/app/main.py"), true);
  assert.equal(canPreviewLocalFilePath("/repo/Makefile"), true);
  assert.equal(canPreviewLocalFilePath("/repo/Dockerfile"), true);
  // A location suffix never rescues a non-previewable file.
  assert.equal(canPreviewLocalFilePath("/tmp/release.dmg:8"), false);
});

test("isFileServerUrl recognizes token-bearing loopback paths", () => {
  const token = "a".repeat(64);
  assert.equal(
    isFileServerUrl(`http://127.0.0.1:8123/${token}/Users/me/file.html`, 8123),
    true,
  );
  // Without a known port, the 64-hex first path segment is the signal.
  assert.equal(
    isFileServerUrl(`http://127.0.0.1:9000/${token}/Users/me/file.html`, null),
    true,
  );
  // Dev-server URLs without a token segment are not file-server URLs.
  assert.equal(isFileServerUrl("http://localhost:5173/", null), false);
  assert.equal(isFileServerUrl("http://localhost:5173/app", 8123), false);
});
