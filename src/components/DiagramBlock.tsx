import { isValidElement, useEffect, useRef, useState } from "react";
import type { MouseEvent as ReactMouseEvent, ReactNode, RefObject } from "react";
import DOMPurify from "dompurify";
import { openDiagramLightbox } from "../lib/diagramLightbox";
import { safeHref } from "../lib/links";

// Renders fenced ```mermaid and ```dot/```graphviz blocks from transcript markdown as SVG,
// entirely in the webview (no server, no external fetch). Both libraries are dynamically
// imported so they land in their own chunk and only load once a diagram actually scrolls
// into view. On any parse/render failure we fall back to the raw source so no content is lost.

type DiagramLang = "mermaid" | "dot";

// Maps a fenced-block info string (react-markdown hands it to us as `language-xxx`) to the
// diagram engine that should handle it, or null for ordinary code blocks.
export function diagramLangFromClassName(className?: string): DiagramLang | null {
  if (!className) return null;
  const match = /language-([\w-]+)/.exec(className);
  const lang = match?.[1]?.toLowerCase();
  if (lang === "mermaid") return "mermaid";
  if (lang === "dot" || lang === "graphviz" || lang === "gv") return "dot";
  return null;
}

// Flattens the react-markdown children of a <code> element back into its source text.
export function nodeText(node: ReactNode): string {
  if (node == null || node === false || node === true) return "";
  if (typeof node === "string") return node;
  if (typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(nodeText).join("");
  if (isValidElement(node)) {
    return nodeText((node.props as { children?: ReactNode }).children);
  }
  return "";
}

type MermaidApi = {
  initialize: (config: Record<string, unknown>) => void;
  render: (id: string, text: string) => Promise<{ svg: string }>;
};

let mermaidPromise: Promise<MermaidApi> | null = null;
function getMermaid(): Promise<MermaidApi> {
  if (!mermaidPromise) {
    mermaidPromise = import("mermaid").then((mod) => {
      const api = mod.default as unknown as MermaidApi;
      api.initialize({
        startOnLoad: false,
        theme: "dark",
        securityLevel: "strict",
        // sanitizeSvg strips <foreignObject>, so labels must render as plain
        // SVG <text> or flowchart node/edge labels are removed entirely.
        htmlLabels: false,
        // Resolve through the application token so diagram labels follow body
        // font changes just like the surrounding Markdown.
        themeVariables: { fontFamily: "var(--font-ui)" },
      });
      return api;
    });
  }
  return mermaidPromise;
}

type Viz = { renderString: (src: string, options?: { format?: string }) => string };

let vizPromise: Promise<Viz> | null = null;
function getViz(): Promise<Viz> {
  if (!vizPromise) {
    vizPromise = import("@viz-js/viz").then((mod) => mod.instance() as unknown as Promise<Viz>);
  }
  return vizPromise;
}

// The rendered SVG is agent-authored and injected via dangerouslySetInnerHTML into the
// (privileged) main webview, so it is sanitized with DOMPurify's SVG profile — a
// maintained sanitizer that handles the mutation-XSS foot-guns (XML-serialize →
// HTML-reparse, namespace confusion) a hand-rolled denylist misses. DOMPurify strips
// scripts, event handlers, foreignObject, and javascript:/data: URIs; the hook below
// additionally routes anchor destinations through our safeHref allowlist and the
// delegated React click handler (data-qmux-href) so a diagram link is never directly
// navigable, and keeps non-anchor references local (#id) so injected paint-server /
// use / image references can't fetch external resources.
const DIAGRAM_SANITIZE_CONFIG = {
  USE_PROFILES: { svg: true, svgFilters: true },
  ADD_ATTR: ["data-qmux-href"],
};

function diagramSanitizerHook(node: Element): void {
  // href and namespaced xlink:href both surface with local name "href".
  const hrefAttrs = Array.from(node.attributes).filter(
    (attribute) => attribute.localName.toLowerCase() === "href",
  );
  if (hrefAttrs.length === 0) {
    return;
  }
  if (node.nodeName.toLowerCase() === "a") {
    const raw = hrefAttrs.map((attribute) => attribute.value).find((value) => value.trim() !== "");
    for (const attribute of hrefAttrs) {
      node.removeAttributeNode(attribute);
    }
    const safe = safeHref(raw);
    if (safe) {
      node.setAttribute("data-qmux-href", safe);
      node.setAttribute("href", "#");
    }
    return;
  }
  // Non-anchor element: keep only local (#id) references; drop anything that could
  // fetch a network/file/data resource.
  for (const attribute of hrefAttrs) {
    if (!attribute.value.trim().startsWith("#")) {
      node.removeAttributeNode(attribute);
    }
  }
}

function sanitizeSvg(svg: string): string {
  DOMPurify.addHook("afterSanitizeAttributes", diagramSanitizerHook);
  let clean: string;
  try {
    clean = DOMPurify.sanitize(svg, DIAGRAM_SANITIZE_CONFIG);
  } finally {
    DOMPurify.removeHook("afterSanitizeAttributes");
  }
  if (!clean.trim()) {
    throw new Error("diagram renderer returned invalid SVG");
  }
  return clean;
}

// Diagram source is agent-authored and untrusted, and rendering runs
// synchronously on the privileged webview's main thread: Viz.renderString is
// synchronous, and Mermaid's cancellation flag only suppresses the later React
// state update — it cannot interrupt layout already in progress. Neither
// DOMPurify nor Mermaid strict mode bounds algorithmic cost, so a dense graph
// well under the 100k-char Markdown cap can still stall the UI during layout,
// sanitization, and DOM insertion. Reject an over-budget graph before layout,
// and an over-budget generated SVG before it is sanitized and inserted.
const MAX_DIAGRAM_SOURCE_CHARS = 20_000;
// Non-blank source lines — a cheap proxy for node/edge count that bounds the
// graph the engines lay out without parsing either dialect.
const MAX_DIAGRAM_STATEMENTS = 500;
const MAX_DIAGRAM_SVG_CHARS = 2_000_000;

function assertDiagramWithinLimits(code: string): void {
  if (code.length > MAX_DIAGRAM_SOURCE_CHARS) {
    throw new Error(
      `diagram source exceeds the ${MAX_DIAGRAM_SOURCE_CHARS.toLocaleString()}-character limit`,
    );
  }
  let statements = 0;
  for (const line of code.split("\n")) {
    if (line.trim() === "") {
      continue;
    }
    statements += 1;
    if (statements > MAX_DIAGRAM_STATEMENTS) {
      throw new Error(
        `diagram exceeds the ${MAX_DIAGRAM_STATEMENTS.toLocaleString()}-statement limit`,
      );
    }
  }
}

function assertRenderedSvgWithinLimits(svg: string): string {
  if (svg.length > MAX_DIAGRAM_SVG_CHARS) {
    throw new Error(
      `rendered diagram exceeds the ${MAX_DIAGRAM_SVG_CHARS.toLocaleString()}-character limit`,
    );
  }
  return svg;
}

let mermaidSeq = 0;

async function renderDiagram(lang: DiagramLang, code: string): Promise<string> {
  assertDiagramWithinLimits(code);
  if (lang === "mermaid") {
    const mermaid = await getMermaid();
    const id = `qmux-mermaid-${mermaidSeq++}`;
    const { svg } = await mermaid.render(id, code);
    return sanitizeSvg(assertRenderedSvgWithinLimits(svg));
  }
  const viz = await getViz();
  return sanitizeSvg(assertRenderedSvgWithinLimits(viz.renderString(code, { format: "svg" })));
}

// Defers work until the element is near the viewport so long transcripts don't pay to build
// every diagram (and the WASM/mermaid chunks) up front.
function useInView(ref: RefObject<Element | null>): boolean {
  const [inView, setInView] = useState(false);
  useEffect(() => {
    const el = ref.current;
    if (!el || inView) return;
    if (typeof IntersectionObserver === "undefined") {
      setInView(true);
      return;
    }
    const observer = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (entry.isIntersecting) {
            setInView(true);
            observer.disconnect();
            break;
          }
        }
      },
      { rootMargin: "200px" },
    );
    observer.observe(el);
    return () => observer.disconnect();
  }, [ref, inView]);
  return inView;
}

type RenderState =
  | { status: "loading" }
  | { status: "done"; svg: string }
  | { status: "error"; error: string };

interface DiagramBlockProps {
  lang: DiagramLang;
  code: string;
  openLink: (url: string) => void;
  openLinkMenu: (url: string, x: number, y: number) => void;
}

function diagramLinkFromEvent(event: ReactMouseEvent<HTMLElement>): string | null {
  const target = event.target instanceof Element ? event.target : null;
  const href = target?.closest("a[data-qmux-href]")?.getAttribute("data-qmux-href");
  return safeHref(href) ?? null;
}

export default function DiagramBlock({
  lang,
  code,
  openLink,
  openLinkMenu,
}: DiagramBlockProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [state, setState] = useState<RenderState>({ status: "loading" });
  const [showSource, setShowSource] = useState(false);
  const inView = useInView(containerRef);
  const source = code.replace(/\n+$/, "");

  useEffect(() => {
    if (!inView) return;
    let cancelled = false;
    setState({ status: "loading" });
    renderDiagram(lang, source)
      .then((svg) => {
        if (!cancelled) setState({ status: "done", svg });
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          const message = err instanceof Error ? err.message : String(err);
          setState({ status: "error", error: message });
        }
      });
    return () => {
      cancelled = true;
    };
  }, [inView, lang, source]);

  const label = lang === "dot" ? "graphviz" : "mermaid";

  return (
    <div className="turn-diagram" ref={containerRef} data-lang={lang}>
      <div className="turn-diagram-bar">
        <span className="turn-diagram-lang">{label}</span>
        <button
          type="button"
          className="control-button turn-diagram-toggle"
          onClick={() => setShowSource((prev) => !prev)}
        >
          {showSource ? "Diagram" : "Source"}
        </button>
      </div>
      {showSource ? (
        <pre className="turn-diagram-source">
          <code>{source}</code>
        </pre>
      ) : state.status === "error" ? (
        <div className="turn-diagram-error">
          <div className="turn-diagram-error-msg">
            Couldn’t render {label} diagram: {state.error}
          </div>
          <pre className="turn-diagram-source">
            <code>{source}</code>
          </pre>
        </div>
      ) : state.status === "done" ? (
        <div
          className="turn-diagram-svg"
          data-lang={lang}
          onClick={(event) => {
            const href = diagramLinkFromEvent(event);
            if (!href) {
              // A plain click on the diagram (not one of its links) expands
              // it into the full-page lightbox. The SVG is already rendered
              // and sanitized, so the lightbox reuses these exact bytes.
              openDiagramLightbox({ lang, label, svg: state.svg });
              return;
            }
            event.preventDefault();
            openLink(href);
          }}
          onAuxClick={(event) => {
            if (!diagramLinkFromEvent(event)) return;
            // Injected anchors use an inert href, but explicitly suppress auxiliary
            // navigation too so a middle click cannot navigate the main webview.
            event.preventDefault();
          }}
          onContextMenu={(event) => {
            const href = diagramLinkFromEvent(event);
            if (!href) return;
            event.preventDefault();
            openLinkMenu(href, event.clientX, event.clientY);
          }}
          dangerouslySetInnerHTML={{ __html: state.svg }}
        />
      ) : (
        <div className="turn-diagram-loading">Rendering {label} diagram…</div>
      )}
    </div>
  );
}
