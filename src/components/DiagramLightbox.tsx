import { useSyncExternalStore } from "react";
import { createPortal } from "react-dom";
import {
  closeDiagramLightbox,
  getDiagramLightbox,
  subscribeDiagramLightbox,
} from "../lib/diagramLightbox";

// Mounted once at the app root, mirroring ImageLightbox: renders the sanitized
// SVG openDiagramLightbox last set, full-page over a dimmed backdrop; dismissed
// by clicking the backdrop or the close button. Escape is handled by the
// app-level Escape dispatcher in App (which reads this component's module
// store), so there is no keydown listener here — it keeps the dispatcher's
// fixed overlay ordering the whole story. The SVG is already rendered and
// sanitized by DiagramBlock, so there is no loading state — expansion reuses
// the exact bytes already on screen.
export default function DiagramLightbox() {
  const state = useSyncExternalStore(
    subscribeDiagramLightbox,
    getDiagramLightbox,
    getDiagramLightbox,
  );

  if (!state) {
    return null;
  }
  return createPortal(
    <div
      className="diagram-lightbox"
      role="dialog"
      aria-modal="true"
      aria-label={`Expanded ${state.label} diagram`}
      onClick={closeDiagramLightbox}
    >
      <button
        type="button"
        className="image-lightbox-close control-button"
        aria-label="Close diagram"
        onClick={closeDiagramLightbox}
      >
        ✕
      </button>
      <div
        className="diagram-lightbox-panel"
        onClick={(event) => event.stopPropagation()}
      >
        <div className="diagram-lightbox-lang">{state.label}</div>
        {/* Reuses .turn-diagram-svg so the expanded diagram renders exactly
            like the inline one (font override, graphviz light canvas). The
            click handler swallows everything: an injected anchor must not
            navigate its inert href="#" and a panel click must not bubble to
            the backdrop, which would dismiss the lightbox. Diagram links stay
            clickable in the inline view; this surface is for reading. */}
        <div
          className="turn-diagram-svg diagram-lightbox-canvas"
          data-lang={state.lang}
          onClick={(event) => {
            event.preventDefault();
            event.stopPropagation();
          }}
          dangerouslySetInnerHTML={{ __html: state.svg }}
        />
      </div>
    </div>,
    document.body,
  );
}
