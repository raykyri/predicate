import assert from "node:assert/strict";
import test from "node:test";

import {
  closeImageLightbox,
  getImageLightbox,
  openImageLightbox,
  subscribeImageLightbox,
} from "../src/lib/imageLightbox";

test("lightbox subscribers observe open and close transitions", () => {
  closeImageLightbox();
  const snapshots: Array<ReturnType<typeof getImageLightbox>> = [];
  const unsubscribe = subscribeImageLightbox(() => {
    snapshots.push(getImageLightbox());
  });

  const opened = { src: "data:image/png;base64,AA==", alt: "Pasted image" };
  openImageLightbox(opened);
  closeImageLightbox();
  unsubscribe();

  assert.deepEqual(snapshots, [opened, null]);
  assert.equal(getImageLightbox(), null);
});
