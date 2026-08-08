import assert from "node:assert/strict";
import test from "node:test";
import {
  artifactCanPreview,
  artifactKind,
  artifactName,
} from "../src/lib/artifacts";
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

test("HTML artifacts receive hover previews", () => {
  for (const path of ["/tmp/report.html", "/private/tmp/report.HTM"]) {
    const entry = artifact(path);
    assert.equal(artifactKind(entry), "html");
    assert.equal(artifactCanPreview(entry), true);
  }
});

test("images remain previewable while generic files and URLs do not", () => {
  assert.equal(artifactKind(artifact("/tmp/chart.svg")), "image");
  assert.equal(artifactCanPreview(artifact("/tmp/chart.svg")), true);
  assert.equal(artifactCanPreview(artifact("/tmp/data.csv")), false);
  assert.equal(artifactCanPreview(artifact(undefined, "http://localhost:3000")), false);
});

test("artifact names are compact for files and loopback URLs", () => {
  assert.equal(artifactName(artifact("/tmp/report.html")), "report.html");
  assert.equal(
    artifactName(artifact(undefined, "http://localhost:3000/dashboard/")),
    "localhost:3000/dashboard",
  );
});
