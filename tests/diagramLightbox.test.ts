import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import {
  closeDiagramLightbox,
  getDiagramLightbox,
  openDiagramLightbox,
  subscribeDiagramLightbox,
} from "../src/lib/diagramLightbox";

test("diagram lightbox subscribers observe open and close transitions", () => {
  closeDiagramLightbox();
  const snapshots: Array<ReturnType<typeof getDiagramLightbox>> = [];
  const unsubscribe = subscribeDiagramLightbox(() => {
    snapshots.push(getDiagramLightbox());
  });

  const opened = {
    lang: "mermaid" as const,
    label: "mermaid",
    svg: '<svg xmlns="http://www.w3.org/2000/svg"></svg>',
  };
  openDiagramLightbox(opened);
  // Opening a second diagram replaces the first rather than stacking.
  const replaced = { lang: "dot" as const, label: "graphviz", svg: "<svg></svg>" };
  openDiagramLightbox(replaced);
  closeDiagramLightbox();
  closeDiagramLightbox();
  unsubscribe();

  assert.deepEqual(snapshots, [opened, replaced, null]);
  assert.equal(getDiagramLightbox(), null);
});

test("diagram lightbox panel clicks do not dismiss through the backdrop", () => {
  const source = readFileSync(
    new URL("../src/components/DiagramLightbox.tsx", import.meta.url),
    "utf8",
  );
  assert.match(
    source,
    /className="diagram-lightbox-panel"\s+onClick=\{\(event\) => event\.stopPropagation\(\)\}/,
  );
});
