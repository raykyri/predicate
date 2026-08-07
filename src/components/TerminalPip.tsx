import { useEffect, useMemo, useState } from "react";
import type { CSSProperties } from "react";
import { ChevronDown, ChevronUp } from "lucide-react";
import {
  readNativeTerminalViewportText,
  type NativeTerminalTheme,
} from "../lib/api";
import { fitTerminalPipFontSize, formatTerminalPipText } from "../lib/terminalPip";

const POLL_MS = 450;
/** Content-box budget for the mini-map body (px). The fit also honors the
 *  window height — see the useMemo below. */
const PIP_MAX_BODY_WIDTH = 420;
const PIP_MAX_BODY_HEIGHT = 360;
const PIP_VIEWPORT_HEIGHT_FRACTION = 0.45;
/** Must match .terminal-pip-body's line-height so the fit math agrees with
 *  the rendered layout. */
const PIP_LINE_HEIGHT = 1.2;
const MONO_FALLBACK_ADVANCE_PER_PX = 0.6;

let pipMeasureContext: CanvasRenderingContext2D | null | undefined;

/** One monospace glyph's advance at 1px font size, canvas-measured once. */
function monoAdvancePerPx(fontFamily: string): number {
  try {
    if (pipMeasureContext === undefined) {
      pipMeasureContext = document.createElement("canvas").getContext("2d");
    }
    const context = pipMeasureContext;
    if (!context) {
      return MONO_FALLBACK_ADVANCE_PER_PX;
    }
    const reference = 10;
    context.font = `${reference}px ${fontFamily}`;
    const advance = context.measureText("0".repeat(100)).width / 100;
    return advance > 0 ? advance / reference : MONO_FALLBACK_ADVANCE_PER_PX;
  } catch {
    return MONO_FALLBACK_ADVANCE_PER_PX;
  }
}

function themeCssColor(hex: string): string | null {
  if (!/^#?[0-9a-fA-F]{6}$/.test(hex)) {
    return null;
  }
  return hex.startsWith("#") ? hex : `#${hex}`;
}

export interface TerminalPipProps {
  paneId: string;
  /** Shown in the chrome strip above the viewport text. */
  title: string;
  /** Offset the preview below the expanded transcript's header. */
  hasPaneHeader: boolean;
  /** The pane's live grid; the mini-map frame and font fit derive from it. */
  columns: number;
  rows: number;
  theme: NativeTerminalTheme | null;
  fontFamily: string;
  fontSize: number;
  onRestore: () => void;
}

/**
 * Floating mini-map of a native terminal's live viewport, used while the
 * right-pane transcript is expanded. The card is sized to the pane's grid so
 * the whole screen stays visible — TUI layouts keep their alignment and blank
 * space stays on screen. Clicking the preview restores the terminal stage;
 * its chrome can collapse the card to the title bar. Text only (no SGR
 * colors) — good enough to see agent progress without a Metal capture path.
 */
export default function TerminalPip({
  paneId,
  title,
  hasPaneHeader,
  columns,
  rows,
  theme,
  fontFamily,
  fontSize,
  onRestore,
}: TerminalPipProps) {
  const [text, setText] = useState("");
  const [collapsed, setCollapsed] = useState(false);

  useEffect(() => {
    let cancelled = false;
    let timer: number | null = null;
    // Drop the previous pane's dump immediately so a tab switch does not show
    // stale text under the new title while the first poll is in flight.
    setText("");

    const poll = async () => {
      try {
        const next = await readNativeTerminalViewportText(paneId);
        if (!cancelled) {
          setText(formatTerminalPipText(next));
        }
      } catch {
        // Keep the last good snapshot; a transient miss (surface not ready)
        // should not flash the card empty after we already have content.
      } finally {
        if (!cancelled) {
          timer = window.setTimeout(() => {
            void poll();
          }, POLL_MS);
        }
      }
    };

    void poll();
    return () => {
      cancelled = true;
      if (timer !== null) {
        window.clearTimeout(timer);
      }
    };
  }, [paneId]);

  const gridColumns = columns > 0 ? Math.max(1, Math.min(500, Math.floor(columns))) : 80;
  const gridRows = rows > 0 ? Math.max(1, Math.min(300, Math.floor(rows))) : 24;

  const fittedFontSize = useMemo(() => {
    const maxHeight = Math.min(
      PIP_MAX_BODY_HEIGHT,
      Math.max(160, window.innerHeight * PIP_VIEWPORT_HEIGHT_FRACTION),
    );
    const fitted = fitTerminalPipFontSize(
      gridColumns,
      gridRows,
      monoAdvancePerPx(fontFamily),
      PIP_MAX_BODY_WIDTH,
      maxHeight,
      PIP_LINE_HEIGHT,
    );
    // Never larger than the previous fixed preview size.
    return Math.min(fitted, Math.max(9, Math.round(fontSize * 0.62)));
  }, [fontFamily, fontSize, gridColumns, gridRows]);

  const background = theme ? themeCssColor(theme.background) : null;
  const foreground = theme ? themeCssColor(theme.foreground) : null;
  const style = {
    ...(background ? { "--terminal-pip-bg": background } : null),
    ...(foreground ? { "--terminal-pip-fg": foreground } : null),
    "--terminal-pip-font-family": fontFamily,
    "--terminal-pip-font-size": `${fittedFontSize.toFixed(2)}px`,
    "--terminal-pip-grid-columns": String(gridColumns),
    "--terminal-pip-grid-rows": String(gridRows),
  } as CSSProperties;

  const displayTitle = title.trim() || "Terminal";
  // A dump can carry one extra trailing line from its final newline; keep the
  // frame at exactly the grid's rows.
  const lines = text === "" ? [] : text.split("\n");
  const gridText = lines.length > gridRows ? lines.slice(0, gridRows).join("\n") : text;
  const body = gridText.trim().length > 0 ? gridText : "Waiting for terminal output…";

  return (
    <section
      className={`terminal-pip${
        hasPaneHeader ? " has-pane-header" : ""
      }${collapsed ? " is-collapsed" : ""}`}
      style={style}
      onPointerDown={(event) => event.stopPropagation()}
      aria-label={`Terminal preview: ${displayTitle}`}
    >
      <div className="terminal-pip-chrome">
        <span className="terminal-pip-title">{displayTitle}</span>
        <button
          type="button"
          className="terminal-pip-collapse"
          title={collapsed ? "Show terminal preview" : "Collapse terminal preview"}
          aria-label={collapsed ? "Show terminal preview" : "Collapse terminal preview"}
          aria-expanded={!collapsed}
          onClick={(event) => {
            event.stopPropagation();
            setCollapsed((current) => !current);
          }}
        >
          {collapsed ? (
            <ChevronDown size={12} aria-hidden="true" />
          ) : (
            <ChevronUp size={12} aria-hidden="true" />
          )}
        </button>
      </div>
      {collapsed ? null : (
        <button
          type="button"
          className="terminal-pip-preview"
          title={`Restore terminal (${displayTitle})`}
          aria-label={`Restore terminal: ${displayTitle}`}
          onClick={(event) => {
            event.stopPropagation();
            onRestore();
          }}
        >
          <pre className="terminal-pip-body">{body}</pre>
        </button>
      )}
    </section>
  );
}
