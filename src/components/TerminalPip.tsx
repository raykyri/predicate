import { useEffect, useState } from "react";
import type { CSSProperties } from "react";
import { Minimize2 } from "lucide-react";
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
 * the right-pane transcript is expanded. Click restores the terminal stage.
 * Text only (no SGR colors) — good enough to see agent progress without a
 * Metal capture path.
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
    <button
      type="button"
      className="terminal-pip"
      style={style}
      title={`Restore terminal (${displayTitle})`}
      aria-label={`Restore terminal: ${displayTitle}`}
      onPointerDown={(event) => event.stopPropagation()}
      onClick={(event) => {
        event.stopPropagation();
        onRestore();
      }}
    >
      <span className="terminal-pip-chrome">
        <span className="terminal-pip-title">{displayTitle}</span>
        <span className="terminal-pip-restore" aria-hidden="true">
          <Minimize2 size={12} />
        </span>
      </span>
      <pre className="terminal-pip-body">{body}</pre>
    </button>
  );
}
