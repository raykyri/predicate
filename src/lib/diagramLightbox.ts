// A single app-wide diagram lightbox. Clicking a rendered mermaid/graphviz
// diagram in a transcript opens its SVG here, full-page. Kept as a module-level
// store rather than App state so a diagram nested deep in the render tree can
// open it with a bare import, without threading a callback through props
// (same shape as imageLightbox).
export interface DiagramLightboxState {
  lang: "mermaid" | "dot";
  label: string;
  // Already rendered and DOMPurify-sanitized by DiagramBlock before it is
  // stored here, so opening is instant and shows the exact bytes on screen.
  svg: string;
}

let current: DiagramLightboxState | null = null;
const listeners = new Set<() => void>();

function emit() {
  for (const listener of listeners) {
    listener();
  }
}

export function openDiagramLightbox(state: DiagramLightboxState) {
  current = state;
  emit();
}

export function closeDiagramLightbox() {
  if (current === null) {
    return;
  }
  current = null;
  emit();
}

export function getDiagramLightbox(): DiagramLightboxState | null {
  return current;
}

export function subscribeDiagramLightbox(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}
