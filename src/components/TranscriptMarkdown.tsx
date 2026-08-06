import {
  createContext,
  isValidElement,
  memo,
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  useSyncExternalStore,
} from "react";
import type {
  ComponentPropsWithoutRef,
  ReactElement,
  ReactNode,
} from "react";
import { createPortal } from "react-dom";
import { Ellipsis } from "lucide-react";
import ReactMarkdown from "react-markdown";
import type { Components, Options } from "react-markdown";
import remarkBreaks from "remark-breaks";
import remarkGfm from "remark-gfm";
import { placePanePopover, turnPaneRectFrom } from "../lib/appHelpers";
import { writeClipboardText } from "../lib/clipboard";
import { safeHref } from "../lib/links";
import DiagramBlock, { diagramLangFromClassName, nodeText } from "./DiagramBlock";

// The TeX pipeline (remark-math + rehype-mathjax) weighs a couple of
// megabytes of SVG glyph tables, so like mermaid/viz it lives in its own
// chunk instead of the startup bundle. Unlike diagrams it cannot load on
// demand — rehype plugins run synchronously inside ReactMarkdown — so the
// chunk starts fetching as soon as this module loads and mounted transcripts
// swap the plugins in via useSyncExternalStore once it lands. Until then
// (and if the chunk fails to load) text renders with the base plugins and
// TeX stays visible as its literal source.
type MarkdownPluginList = NonNullable<Options["remarkPlugins"]>;

interface MathPlugins {
  remark: MarkdownPluginList;
  rehype: MarkdownPluginList;
}

const baseRemarkPlugins: MarkdownPluginList = [remarkGfm, remarkBreaks];

let mathPlugins: MathPlugins | null = null;
const mathPluginListeners = new Set<() => void>();

/** Resolves once the math chunk is in (or failed); tests and SSR callers
 * await it so the first render already includes MathJax output. */
export const transcriptMathPluginsReady: Promise<void> = import("../lib/markdownPlugins")
  .then((module) => {
    mathPlugins = {
      remark: module.transcriptRemarkPlugins,
      rehype: module.transcriptRehypePlugins,
    };
    for (const listener of mathPluginListeners) {
      listener();
    }
  })
  .catch(() => undefined);

function subscribeToMathPlugins(listener: () => void) {
  mathPluginListeners.add(listener);
  return () => {
    mathPluginListeners.delete(listener);
  };
}

function readMathPlugins() {
  return mathPlugins;
}

// Shared by ordinary transcript Markdown and research documents. The provider
// keeps link closures stable above memoized message items while giving ordinary
// links and links injected into diagram SVGs exactly the same behavior.
export interface LinkActions {
  openLink: (url: string) => void;
  openLinkMenu: (url: string, x: number, y: number) => void;
}

const LinkActionsContext = createContext<LinkActions>({
  openLink: () => undefined,
  openLinkMenu: () => undefined,
});

export function TranscriptLinkActionsProvider({
  actions,
  children,
}: {
  actions: LinkActions;
  children: ReactNode;
}) {
  return <LinkActionsContext.Provider value={actions}>{children}</LinkActionsContext.Provider>;
}

function MarkdownLink({ href, ...props }: ComponentPropsWithoutRef<"a">) {
  const { openLink, openLinkMenu } = useContext(LinkActionsContext);
  const safe = safeHref(href);
  if (!safe) {
    return <span {...props} />;
  }
  return (
    <a
      {...props}
      href={safe}
      onClick={(event) => {
        event.preventDefault();
        openLink(safe);
      }}
      onAuxClick={(event) => {
        // A middle (or other auxiliary) click would otherwise keep WebKit's
        // native navigation and bypass openLink — where qmux routes loopback
        // file-server URLs into the sandboxed overlay and everything else
        // through the guarded external opener. Never let it navigate natively;
        // route a middle click through the same classifier as a primary click.
        event.preventDefault();
        if (event.button === 1) {
          openLink(safe);
        }
      }}
      onContextMenu={(event) => {
        event.preventDefault();
        openLinkMenu(safe, event.clientX, event.clientY);
      }}
    />
  );
}

function BlockedMarkdownImage({ src, alt }: ComponentPropsWithoutRef<"img">) {
  const { openLink, openLinkMenu } = useContext(LinkActionsContext);
  const safe = safeHref(src);
  if (!safe) {
    return alt ? <span>{alt}</span> : null;
  }
  return (
    <button
      type="button"
      className="control-button research-blocked-image"
      onClick={() => openLink(safe)}
      onContextMenu={(event) => {
        event.preventDefault();
        openLinkMenu(safe, event.clientX, event.clientY);
      }}
    >
      {alt ? `Open image: ${alt}` : "Open external image"}
    </button>
  );
}

function MarkdownDiagramBlock({ lang, code }: { lang: "mermaid" | "dot"; code: string }) {
  const { openLink, openLinkMenu } = useContext(LinkActionsContext);
  return (
    <DiagramBlock lang={lang} code={code} openLink={openLink} openLinkMenu={openLinkMenu} />
  );
}

const CODE_MENU_PREFERRED_WIDTH = 180;

// Fenced-code wrapping is a single session-wide preference, not a per-block
// toggle: flipping the menu checkbox on any code block applies (or clears)
// wrap for every fenced block in the app. The visual is driven by a root CSS
// class so the layout change is synchronous with the click — React does not
// need to re-render every block — which lets the toggle also pin the clicked
// block's viewport position while siblings above it grow or shrink.
const WRAP_CODE_BLOCKS_CLASS = "wrap-code-blocks";
let wrapCodeBlocks = false;

function applyWrapCodeBlocksClass(wrap: boolean) {
  if (typeof document === "undefined") {
    return;
  }
  document.documentElement.classList.toggle(WRAP_CODE_BLOCKS_CLASS, wrap);
}

// Keep the root class in sync with the module flag (clears a stale class after
// HMR reloads the module with wrapCodeBlocks reset to false).
applyWrapCodeBlocksClass(wrapCodeBlocks);

/** Nearest ancestor that can scroll vertically, if any. */
function verticalScrollParent(element: HTMLElement): HTMLElement | null {
  let node: HTMLElement | null = element.parentElement;
  while (node) {
    const { overflowY } = getComputedStyle(node);
    if (overflowY === "auto" || overflowY === "scroll" || overflowY === "overlay") {
      return node;
    }
    node = node.parentElement;
  }
  return null;
}

/**
 * Set the global fenced-code wrap preference. When `anchor` is the code block
 * the user toggled from, adjust its scroll parent so that block stays at the
 * same viewport Y after every other block's height changes with wrap.
 */
function setWrapCodeBlocks(next: boolean, anchor?: HTMLElement | null) {
  if (next === wrapCodeBlocks) {
    return;
  }
  const scroller = anchor ? verticalScrollParent(anchor) : null;
  const beforeTop = anchor?.getBoundingClientRect().top ?? 0;
  wrapCodeBlocks = next;
  applyWrapCodeBlocksClass(next);
  if (anchor && scroller) {
    // Reading layout after the class toggle forces a reflow so afterTop
    // reflects every code block's new height, not a stale pre-wrap geometry.
    const afterTop = anchor.getBoundingClientRect().top;
    scroller.scrollTop += afterTop - beforeTop;
  }
}

function MarkdownCodeBlock({ children, ...props }: ComponentPropsWithoutRef<"pre">) {
  const [open, setOpen] = useState(false);
  const blockRef = useRef<HTMLDivElement | null>(null);
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const popoverRef = useRef<HTMLDivElement | null>(null);
  const [pos, setPos] = useState<{
    left: number;
    top: number;
    maxHeight: number;
    maxWidth: number;
  } | null>(null);

  const positionMenu = useCallback(() => {
    const trigger = triggerRef.current;
    const popover = popoverRef.current;
    if (!trigger || !popover) {
      return;
    }
    const { height } = popover.getBoundingClientRect();
    setPos(
      placePanePopover({
        triggerRect: trigger.getBoundingClientRect(),
        popoverSize: { width: CODE_MENU_PREFERRED_WIDTH, height },
        paneRect: turnPaneRectFrom(trigger),
        align: "end",
        prefer: "below",
      }),
    );
  }, []);

  useEffect(() => {
    if (!open) {
      return;
    }
    const handlePointerDown = (event: MouseEvent) => {
      const target = event.target as Node;
      if (!triggerRef.current?.contains(target) && !popoverRef.current?.contains(target)) {
        setOpen(false);
      }
    };
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handlePointerDown);
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("mousedown", handlePointerDown);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [open]);

  useLayoutEffect(() => {
    if (!open) {
      setPos(null);
      return;
    }
    positionMenu();
    const onReflow = () => positionMenu();
    window.addEventListener("resize", onReflow);
    window.addEventListener("scroll", onReflow, true);
    return () => {
      window.removeEventListener("resize", onReflow);
      window.removeEventListener("scroll", onReflow, true);
    };
  }, [open, positionMenu]);

  // Snapshot the global preference when the menu opens (or re-renders while
  // open). The root CSS class is the source of truth for layout; this value is
  // only for the checkbox label/aria state.
  const wrap = wrapCodeBlocks;

  return (
    <div ref={blockRef} className="turn-markdown-code-block">
      <button
        ref={triggerRef}
        type="button"
        className="icon-button turn-markdown-code-menu-trigger"
        title="Code block options"
        aria-label="Code block options"
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((current) => !current)}
      >
        <Ellipsis aria-hidden="true" />
      </button>
      {open
        ? createPortal(
            <div
              ref={popoverRef}
              className="popover-surface popover-surface--context turn-message-menu-popover turn-markdown-code-menu-popover"
              role="menu"
              aria-label="Code block options"
              style={
                pos
                  ? {
                      left: pos.left,
                      top: pos.top,
                      maxHeight: pos.maxHeight,
                      width: Math.min(CODE_MENU_PREFERRED_WIDTH, pos.maxWidth),
                      maxWidth: pos.maxWidth,
                    }
                  : { left: -9999, top: -9999 }
              }
            >
              <button
                type="button"
                role="menuitemcheckbox"
                aria-checked={wrap}
                className="menu-item turn-message-menu-item"
                onClick={() => {
                  setWrapCodeBlocks(!wrap, blockRef.current);
                  setOpen(false);
                }}
              >
                {wrap ? "Unwrap code blocks" : "Wrap code blocks"}
              </button>
              <button
                type="button"
                role="menuitem"
                className="menu-item turn-message-menu-item"
                onClick={() => {
                  setOpen(false);
                  void writeClipboardText(nodeText(children));
                }}
              >
                Copy code block
              </button>
            </div>,
            document.body,
          )
        : null}
      <pre {...props}>{children}</pre>
    </div>
  );
}

const markdownComponents: Components = {
  a: ({ node: _node, href, ...props }) => <MarkdownLink href={href} {...props} />,
  table: ({ node: _node, ...props }) => (
    <div className="turn-markdown-table-wrap">
      <table {...props} />
    </div>
  ),
  pre: ({ node: _node, children, ...props }) => {
    const codeElement = isValidElement(children)
      ? (children as ReactElement<{ className?: string; children?: ReactNode }>)
      : null;
    const lang = diagramLangFromClassName(codeElement?.props.className);
    if (codeElement && lang) {
      return <MarkdownDiagramBlock lang={lang} code={nodeText(codeElement.props.children)} />;
    }
    return <MarkdownCodeBlock {...props}>{children}</MarkdownCodeBlock>;
  },
};

const researchMarkdownComponents: Components = {
  ...markdownComponents,
  img: ({ node: _node, ...props }) => <BlockedMarkdownImage {...props} />,
};

// Paragraphs and Markdown hard breaks are block boundaries in the source, but
// inline contexts (titles, prompts, compact answer previews) need them to flow
// as ordinary spaces. Keeping the inline children preserves links and emphasis
// without stacking every short paragraph on its own line; code is deliberately
// flattened to ordinary text so compact previews retain one visual type style.
const inlineComponentOverrides: Components = {
  p: ({ node: _node, children }) => <span>{children} </span>,
  br: () => <span> </span>,
  code: ({ node: _node, children }) => <span>{children}</span>,
};

const inlineMarkdownComponents: Components = {
  ...markdownComponents,
  ...inlineComponentOverrides,
};

const inlineResearchMarkdownComponents: Components = {
  ...researchMarkdownComponents,
  ...inlineComponentOverrides,
};

// Block-level elements stripped (but their inline children kept) in inline
// mode, so a stray heading/list/code fence in a one-line context like a title
// renders as plain rich text instead of promoting to a full block.
const INLINE_DISALLOWED_ELEMENTS = [
  "h1",
  "h2",
  "h3",
  "h4",
  "h5",
  "h6",
  "ul",
  "ol",
  "li",
  "blockquote",
  "pre",
  "hr",
  "table",
  "thead",
  "tbody",
  "tr",
  "th",
  "td",
];

export interface OversizedMarkdownPolicy {
  maxCharacters: number;
  /** Cap on what the plain-text fallback puts in the DOM. Laying out a single
   * multi-megabyte text node freezes the interface as surely as parsing it,
   * so text beyond the cap is elided with a truncation notice. */
  maxDisplayCharacters?: number;
  fallbackClassName?: string;
}

interface TranscriptMarkdownProps {
  text: string;
  className?: string;
  imageBehavior?: "render" | "open";
  oversizedContent?: OversizedMarkdownPolicy;
  /** Strip block-level wrappers (headings, lists, code fences, tables), retain
   * safe inline formatting such as emphasis and links, and render code as plain
   * text. For one-line contexts where promoted or code-styled content would
   * break the compact layout. */
  inline?: boolean;
}

// Memoized because ReactMarkdown re-parses on every render and callers rerender
// far more often than their text changes (streaming polls deliver fresh block
// objects whose `text` is value-equal). `text` is a primitive, so the default
// shallow compare skips the parse; link handling stays live because MarkdownLink
// reads its actions through context, which bypasses the memo. Callers must pass
// a stable `oversizedContent` object or the compare degrades to identity.
export default memo(function TranscriptMarkdown({
  text,
  className = "",
  imageBehavior = "render",
  oversizedContent,
  inline = false,
}: TranscriptMarkdownProps) {
  const math = useSyncExternalStore(subscribeToMathPlugins, readMathPlugins, readMathPlugins);
  if (oversizedContent && text.length > oversizedContent.maxCharacters) {
    const displayLimit = oversizedContent.maxDisplayCharacters;
    const shown =
      displayLimit !== undefined && text.length > displayLimit
        ? `${text.slice(0, displayLimit)}\n… (truncated: showing ${displayLimit.toLocaleString()} of ${text.length.toLocaleString()} characters)`
        : text;
    return (
      <pre className={oversizedContent.fallbackClassName ?? "research-plaintext"}>{shown}</pre>
    );
  }
  return (
    <div className={`turn-markdown${className ? ` ${className}` : ""}`}>
      <ReactMarkdown
        components={
          inline
            ? imageBehavior === "open"
              ? inlineResearchMarkdownComponents
              : inlineMarkdownComponents
            : imageBehavior === "open"
              ? researchMarkdownComponents
              : markdownComponents
        }
        remarkPlugins={math ? math.remark : baseRemarkPlugins}
        rehypePlugins={math ? math.rehype : undefined}
        disallowedElements={inline ? INLINE_DISALLOWED_ELEMENTS : undefined}
        unwrapDisallowed={inline}
      >
        {text}
      </ReactMarkdown>
    </div>
  );
});
