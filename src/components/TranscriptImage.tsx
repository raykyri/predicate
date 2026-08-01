import { useEffect, useState } from "react";
import { readTranscriptImage } from "../lib/api";
import { openImageLightbox } from "../lib/imageLightbox";
import { COLLAPSED_IMAGE_LABEL, imageMarkerSourcePath } from "../lib/imageMarkers";

// Data URLs keyed by source path, shared across every mounted transcript so a
// pane rerender (streaming polls arrive continuously) never re-reads or
// re-encodes the same paste. Bounded because each entry can hold megabytes of
// base64: past the cap the oldest entry is dropped — insertion order is a good
// enough recency proxy for a cache whose entries are immutable files. Failed
// reads are evicted immediately so a transient error (e.g. a cache file still
// being written) retries on the next mount instead of sticking forever.
const MAX_CACHED_IMAGES = 32;
const imageCache = new Map<string, Promise<string>>();

function loadTranscriptImage(path: string): Promise<string> {
  const cached = imageCache.get(path);
  if (cached) {
    return cached;
  }
  const loading = readTranscriptImage(path);
  loading.catch(() => {
    imageCache.delete(path);
  });
  imageCache.set(path, loading);
  if (imageCache.size > MAX_CACHED_IMAGES) {
    const oldest = imageCache.keys().next().value;
    if (oldest !== undefined && oldest !== path) {
      imageCache.delete(oldest);
    }
  }
  return loading;
}

/** Renders one path-bearing transcript image marker as the actual pasted image.
 *  Numbered "[Image #N]" references (no path), reads still in flight, and failed
 *  reads all fall back to the muted "[Image]" chip the compact views use, so the
 *  transcript never shows a raw cache path.
 *
 *  "inline" is the full-width transcript image; "thumbnail" is the small
 *  rail-card preview. Either opens the shared lightbox on click. */
export default function TranscriptImage({
  marker,
  variant = "inline",
}: {
  marker: string;
  variant?: "inline" | "thumbnail";
}) {
  const path = imageMarkerSourcePath(marker);
  const [dataUrl, setDataUrl] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    setDataUrl(null);
    setError(null);
    if (!path) {
      return;
    }
    let cancelled = false;
    loadTranscriptImage(path).then(
      (url) => {
        if (!cancelled) {
          setDataUrl(url);
        }
      },
      (err) => {
        if (!cancelled) {
          setError(typeof err === "string" ? err : String(err));
        }
      },
    );
    return () => {
      cancelled = true;
    };
  }, [path]);

  if (!path || !dataUrl) {
    return (
      <span
        className={variant === "thumbnail" ? "queued-turn-image-chip" : "turn-image-chip"}
        title={error ?? path ?? undefined}
      >
        {COLLAPSED_IMAGE_LABEL}
      </span>
    );
  }
  return (
    <button
      type="button"
      className={variant === "thumbnail" ? "turn-image-thumb" : "turn-image-button"}
      title={path}
      aria-label="View pasted image"
      onClick={(event) => {
        // Rail cards are drag handles and double-click selects text; keep the
        // click that opens the lightbox from bubbling into either.
        event.stopPropagation();
        openImageLightbox({ src: dataUrl, alt: "Pasted image" });
      }}
    >
      <img className="turn-image" src={dataUrl} alt="Pasted image" />
    </button>
  );
}
