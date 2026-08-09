import assert from "node:assert/strict";
import test from "node:test";
import {
  artifactKind,
  artifactName,
  artifactTrayVisible,
  isArtifactBrowserOpen,
} from "../src/lib/artifacts";
import type { BrowserOverlayState } from "../src/appTypes";
import type { ArtifactInfo } from "../src/types";

function artifact(path?: string, url?: string): ArtifactInfo {
  return {
    id: "artifact-1",
    groupId: "group-1",
    paneId: "pane-1",
    path,
    url,
    createdAt: 1,
  };
}

test("HTML artifacts are classified by extension", () => {
  for (const path of ["/tmp/report.html", "/private/tmp/report.HTM"]) {
    assert.equal(artifactKind(artifact(path)), "html");
  }
});

test("images, generic files, and URLs have distinct artifact kinds", () => {
  assert.equal(artifactKind(artifact("/tmp/chart.svg")), "image");
  assert.equal(artifactKind(artifact("/tmp/data.csv")), "file");
  assert.equal(artifactKind(artifact(undefined, "http://localhost:3000")), "url");
});

test("artifact names are compact for files and loopback URLs", () => {
  assert.equal(artifactName(artifact("/tmp/report.html")), "report.html");
  assert.equal(
    artifactName(artifact(undefined, "http://localhost:3000/dashboard/")),
    "localhost:3000/dashboard",
  );
});

test("an open browser toggles only for the artifact that owns its page", () => {
  const browser: BrowserOverlayState = {
    url: "http://localhost:3000/report",
    open: true,
    artifactId: "artifact-1",
    reloadNonce: 1,
    sandbox: false,
    mode: "webkit",
  };

  assert.equal(isArtifactBrowserOpen(browser, "artifact-1"), true);
  assert.equal(isArtifactBrowserOpen(browser, "artifact-2"), false);
  assert.equal(isArtifactBrowserOpen({ ...browser, open: false }, "artifact-1"), false);
});

test("only the top split shows its artifact tray by default", () => {
  assert.equal(artifactTrayVisible(true, undefined, true), true);
  assert.equal(artifactTrayVisible(true, undefined, false), false);
  assert.equal(artifactTrayVisible(true, false, false), true);
  assert.equal(artifactTrayVisible(true, true, true), false);
  assert.equal(artifactTrayVisible(false, false, true), false);
});
