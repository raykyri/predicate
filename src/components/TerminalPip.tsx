import { useEffect, useState } from "react";
import type { CSSProperties } from "react";
import { ChevronDown, ChevronUp } from "lucide-react";
import {
  readNativeTerminalViewportText,
  type NativeTerminalTheme,
} from "../lib/api";
import { formatTerminalPipText } from "../lib/terminalPip";

const POLL_MS = 450;

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
  theme: NativeTerminalTheme | null;
  fontFamily: string;
  fontSize: number;
  onRestore: () => void;
}

/**
 * Floating monospaced preview of a native terminal's live viewport, used while
 * the right-pane transcript is expanded. Clicking the preview restores the
 * terminal stage; its chrome can collapse the card to the title bar. Text only
 * (no SGR colors) — good enough to see agent progress without a Metal capture
 * path.
 */
export default function TerminalPip({
  paneId,
  title,
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

  const background = theme ? themeCssColor(theme.background) : null;
  const foreground = theme ? themeCssColor(theme.foreground) : null;
  const style = {
    ...(background ? { "--terminal-pip-bg": background } : null),
    ...(foreground ? { "--terminal-pip-fg": foreground } : null),
    "--terminal-pip-font-family": fontFamily,
    "--terminal-pip-font-size": `${Math.max(9, Math.round(fontSize * 0.62))}px`,
  } as CSSProperties;

  const displayTitle = title.trim() || "Terminal";
  const body = text.trim().length > 0 ? text : "Waiting for terminal output…";

  return (
    <section
      className={`terminal-pip${collapsed ? " is-collapsed" : ""}`}
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
