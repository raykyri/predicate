import { Bot, ExternalLink, Globe, RotateCw, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type {
  CSSProperties,
  KeyboardEvent as ReactKeyboardEvent,
  PointerEvent as ReactPointerEvent,
} from "react";
import type { BrowserOverlayMode, BrowserOverlaySize } from "../appTypes";
import {
  claimNativeTerminalPointerForWebDrag,
  getBrowserAutomationSnapshot,
  insertBrowserAutomationText,
  navigateBrowserAutomation,
  reloadBrowserAutomation,
  sendBrowserAutomationKey,
  sendBrowserAutomationMouse,
  setNativeTerminalIframeShortcutFallback,
} from "../lib/api";

const MIN_BROWSER_OVERLAY_WIDTH = 360;
const MIN_BROWSER_OVERLAY_HEIGHT = 240;
const BROWSER_OVERLAY_LEFT_INSET_FALLBACK = 64;
const BROWSER_OVERLAY_BOTTOM_INSET = 50;
const lastAutomationNavigationByPane = new Map<string, number>();
const MAX_REMEMBERED_AUTOMATION_PANES = 256;

function rememberAutomationNavigation(paneId: string, reloadNonce: number) {
  lastAutomationNavigationByPane.delete(paneId);
  lastAutomationNavigationByPane.set(paneId, reloadNonce);
  while (lastAutomationNavigationByPane.size > MAX_REMEMBERED_AUTOMATION_PANES) {
    const oldest = lastAutomationNavigationByPane.keys().next().value;
    if (oldest === undefined) {
      break;
    }
    lastAutomationNavigationByPane.delete(oldest);
  }
}

function ignoreBrowserCommand(command: Promise<unknown>) {
  void command.catch(() => undefined);
}

function clampSize(value: number, min: number, max: number) {
  return Math.min(max, Math.max(min, value));
}

function cssPixelValue(value: string, fallback: number) {
  const parsed = Number.parseFloat(value);
  return Number.isFinite(parsed) ? parsed : fallback;
}

// The browser overlay floats over the sidebar + center terminal (leaving a left
// strip of the tabs visible) and renders a URL bound to the active tab. A minimal
// navigation bar at the top shows the current URL and lets the user navigate. The
// close control stays on the left while browser-engine selection stays on the right.
interface BrowserOverlayProps {
  paneId: string;
  url: string | null;
  // Bumped on open/refresh so WebKit reloads and Agent navigation is replayed.
  reloadNonce: number;
  // True for token-bearing file-server URLs: sandbox the frame so served (possibly
  // untrusted) content gets an opaque origin and can't read the token back to fetch
  // other workspace files. Protected previews are always kept in WebKit mode.
  sandbox: boolean;
  mode: BrowserOverlayMode;
  // Passed to the token-gated file server so Markdown documents rendered in
  // this isolated frame use the same body font as the application. Arbitrary
  // localhost pages remain untouched.
  bodyFontId: string;
  size?: BrowserOverlaySize | null;
  toggleShortcutLabel?: string | null;
  // Navigate to a typed address (a URL, or a bare host that gets http:// prefixed).
  onNavigate: (rawInput: string) => void;
  // Reload the current page.
  onRefresh: () => void;
  // Open the current page in the system's default external browser.
  onOpenExternal: (currentUrl?: string) => void;
  // Close the overlay.
  onClose: () => void;
  // Switch between the native WebKit preview and the mirrored Chromium target.
  onModeChange: (mode: BrowserOverlayMode, currentUrl?: string | null) => void;
  // Persist a user-resized overlay size in the app's per-pane React state.
  onResize: (size: BrowserOverlaySize) => void;
}

export default function BrowserOverlay({
  paneId,
  url,
  reloadNonce,
  sandbox,
  mode,
  bodyFontId,
  size,
  toggleShortcutLabel,
  onNavigate,
  onRefresh,
  onOpenExternal,
  onClose,
  onModeChange,
  onResize,
}: BrowserOverlayProps) {
  // Editable copy of the address, re-synced whenever the loaded URL changes so the
  // bar tracks navigation without clobbering what the user is mid-typing.
  const [draft, setDraft] = useState(url ?? "");
  const [resizing, setResizing] = useState(false);
  const [automationSnapshot, setAutomationSnapshot] = useState<
    Awaited<ReturnType<typeof getBrowserAutomationSnapshot>> | null
  >(null);
  const overlayRef = useRef<HTMLDivElement | null>(null);
  const bodyRef = useRef<HTMLDivElement | null>(null);
  const addressInputRef = useRef<HTMLInputElement | null>(null);
  const frameRef = useRef<HTMLIFrameElement | null>(null);
  const lastAutomationMoveRef = useRef(0);
  const cleanupResizeRef = useRef<(() => void) | null>(null);

  const automated = mode === "agent" && !sandbox;
  const displayedUrl = automated ? (automationSnapshot?.url ?? url) : url;

  useEffect(() => {
    if (document.activeElement !== addressInputRef.current) {
      setDraft(displayedUrl ?? "");
    }
  }, [displayedUrl]);

  useEffect(() => {
    if (!automated) {
      setAutomationSnapshot(null);
      return;
    }
    let cancelled = false;
    let polling = false;
    const poll = async () => {
      if (polling) {
        return;
      }
      polling = true;
      try {
        const bodyRect = bodyRef.current?.getBoundingClientRect();
        const width = Math.round(
          clampSize(bodyRect?.width ?? 1280, MIN_BROWSER_OVERLAY_WIDTH, 4096),
        );
        const height = Math.round(
          clampSize(bodyRect?.height ?? 900, MIN_BROWSER_OVERLAY_HEIGHT, 4096),
        );
        const snapshot = await getBrowserAutomationSnapshot(paneId, width, height);
        if (!cancelled) {
          setAutomationSnapshot(snapshot);
        }
      } catch (error) {
        if (!cancelled) {
          setAutomationSnapshot({
            available: false,
            tabId: null,
            url: null,
            title: null,
            imageDataUrl: null,
            width: 1280,
            height: 900,
            error: error instanceof Error ? error.message : String(error),
          });
        }
      } finally {
        polling = false;
      }
    };
    void (async () => {
      if (url && lastAutomationNavigationByPane.get(paneId) !== reloadNonce) {
        try {
          await navigateBrowserAutomation(paneId, url);
          rememberAutomationNavigation(paneId, reloadNonce);
        } catch (error) {
          if (!cancelled) {
            setAutomationSnapshot({
              available: false,
              tabId: null,
              url,
              title: null,
              imageDataUrl: null,
              width: 1280,
              height: 900,
              error: error instanceof Error ? error.message : String(error),
            });
          }
        }
      }
      await poll();
    })();
    const timer = window.setInterval(() => void poll(), 500);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [automated, paneId, reloadNonce, url]);

  // Keys typed into the framed page belong to its document: the host
  // document's window-level shortcut handlers never fire, and the native key
  // monitor deliberately leaves keys to a healthy WKWebView responder — so
  // every app shortcut (⌘-backtick, the ⌘⇧E toggle that closes this overlay…)
  // goes dead the moment a click lands inside the frame. Report frame focus
  // to the native layer so the monitor claims ⌘ chords for qmux while it
  // holds. Focus crossing into an iframe blurs the host window (its browsing
  // context loses focus) and leaves activeElement on the frame element, so
  // sample on window focus transitions — the framed document never forwards
  // focusin/focusout to this one. The rAF matches the app-level samplers:
  // activeElement settles after the event.
  useEffect(() => {
    if (!url || automated) {
      return;
    }
    let frame: number | null = null;
    let reported = false;
    const report = (active: boolean) => {
      if (active === reported) {
        return;
      }
      reported = active;
      void setNativeTerminalIframeShortcutFallback(active).catch(() => undefined);
    };
    const sample = () => {
      frame = null;
      report(frameRef.current !== null && document.activeElement === frameRef.current);
    };
    const schedule = () => {
      if (frame === null) {
        frame = requestAnimationFrame(sample);
      }
    };
    window.addEventListener("blur", schedule);
    window.addEventListener("focus", schedule);
    window.addEventListener("focusin", schedule);
    schedule();
    return () => {
      window.removeEventListener("blur", schedule);
      window.removeEventListener("focus", schedule);
      window.removeEventListener("focusin", schedule);
      if (frame !== null) {
        cancelAnimationFrame(frame);
      }
      // The frame is unmounting (overlay closed, or remounted by the reload
      // key); WebKit fires no focus event for a removed element, so release
      // the claim explicitly instead of leaving it wedged on.
      report(false);
    };
  }, [automated, url, reloadNonce]);

  useEffect(() => {
    return () => {
      cleanupResizeRef.current?.();
    };
  }, []);

  function startResize(event: ReactPointerEvent<HTMLDivElement>) {
    const overlay = overlayRef.current;
    const parent = overlay?.offsetParent instanceof HTMLElement ? overlay.offsetParent : null;
    if (!overlay || !parent) {
      return;
    }

    event.preventDefault();
    event.currentTarget.setPointerCapture(event.pointerId);
    const releaseNativePointer = claimNativeTerminalPointerForWebDrag();

    const overlayRect = overlay.getBoundingClientRect();
    const parentRect = parent.getBoundingClientRect();
    const parentStyles = getComputedStyle(parent);
    const leftInset = cssPixelValue(
      parentStyles.getPropertyValue("--browser-overlay-left"),
      BROWSER_OVERLAY_LEFT_INSET_FALLBACK,
    );
    const maxWidth = Math.max(
      MIN_BROWSER_OVERLAY_WIDTH,
      overlayRect.right - parentRect.left - leftInset,
    );
    const maxHeight = Math.max(
      MIN_BROWSER_OVERLAY_HEIGHT,
      parentRect.bottom - overlayRect.top - BROWSER_OVERLAY_BOTTOM_INSET,
    );
    const startX = event.clientX;
    const startY = event.clientY;
    const startWidth = overlayRect.width;
    const startHeight = overlayRect.height;
    const handle = event.currentTarget;
    const previousCursor = document.body.style.cursor;
    const previousUserSelect = document.body.style.userSelect;

    document.body.style.cursor = "nesw-resize";
    document.body.style.userSelect = "none";
    setResizing(true);

    const cleanup = () => {
      document.body.style.cursor = previousCursor;
      document.body.style.userSelect = previousUserSelect;
      window.removeEventListener("pointermove", handlePointerMove);
      window.removeEventListener("pointerup", stopResize);
      window.removeEventListener("pointercancel", stopResize);
      if (handle.hasPointerCapture(event.pointerId)) {
        handle.releasePointerCapture(event.pointerId);
      }
      releaseNativePointer();
      setResizing(false);
      cleanupResizeRef.current = null;
    };

    const stopResize = () => cleanup();

    const handlePointerMove = (moveEvent: PointerEvent) => {
      const width = clampSize(
        startWidth - (moveEvent.clientX - startX),
        MIN_BROWSER_OVERLAY_WIDTH,
        maxWidth,
      );
      const height = clampSize(
        startHeight + (moveEvent.clientY - startY),
        MIN_BROWSER_OVERLAY_HEIGHT,
        maxHeight,
      );
      onResize({ width: Math.round(width), height: Math.round(height) });
    };

    cleanupResizeRef.current?.();
    cleanupResizeRef.current = cleanup;
    window.addEventListener("pointermove", handlePointerMove);
    window.addEventListener("pointerup", stopResize);
    window.addEventListener("pointercancel", stopResize);
  }

  const overlayStyle: CSSProperties | undefined = size
    ? { width: `${size.width}px`, height: `${size.height}px` }
    : undefined;
  const closeTitle = toggleShortcutLabel
    ? `Hide browser (Esc, ${toggleShortcutLabel})`
    : "Hide browser (Esc)";
  const frameUrl = (() => {
    if (!url || !sandbox) {
      return url;
    }
    try {
      const parsed = new URL(url);
      parsed.searchParams.set("qmux-body-font", bodyFontId);
      return parsed.toString();
    } catch {
      return url;
    }
  })();
  function automationPoint(event: {
    clientX: number;
    clientY: number;
    currentTarget: HTMLImageElement;
  }) {
    const rect = event.currentTarget.getBoundingClientRect();
    const width = automationSnapshot?.width ?? 1280;
    const height = automationSnapshot?.height ?? 900;
    return {
      x: ((event.clientX - rect.left) / rect.width) * width,
      y: ((event.clientY - rect.top) / rect.height) * height,
    };
  }

  function automationButton(button: number) {
    if (button === 1) {
      return "middle" as const;
    }
    if (button === 2) {
      return "right" as const;
    }
    return "left" as const;
  }

  function automationModifiers(event: {
    altKey: boolean;
    ctrlKey: boolean;
    metaKey: boolean;
    shiftKey: boolean;
  }) {
    return (
      (event.altKey ? 1 : 0) |
      (event.ctrlKey ? 2 : 0) |
      (event.metaKey ? 4 : 0) |
      (event.shiftKey ? 8 : 0)
    );
  }

  function handleAutomationKey(event: ReactKeyboardEvent<HTMLImageElement>) {
    if (event.metaKey || event.ctrlKey || event.altKey) {
      return;
    }
    if (event.key.length === 1) {
      event.preventDefault();
      ignoreBrowserCommand(insertBrowserAutomationText(paneId, event.key));
      return;
    }
    const virtualKeys: Record<string, number> = {
      Backspace: 8,
      Tab: 9,
      Enter: 13,
      Escape: 27,
      PageUp: 33,
      PageDown: 34,
      End: 35,
      Home: 36,
      ArrowLeft: 37,
      ArrowUp: 38,
      ArrowRight: 39,
      ArrowDown: 40,
      Delete: 46,
    };
    const virtualKey = virtualKeys[event.key];
    if (virtualKey !== undefined) {
      event.preventDefault();
      ignoreBrowserCommand(
        sendBrowserAutomationKey(
          paneId,
          event.key,
          event.code,
          virtualKey,
          automationModifiers(event),
        ),
      );
    }
  }

  return (
    <div
      ref={overlayRef}
      className={`browser-overlay${url ? "" : " is-empty"}${resizing ? " is-resizing" : ""}`}
      style={overlayStyle}
      role="region"
      aria-label="Browser overlay"
    >
      <div className="browser-overlay-nav">
        <button
          type="button"
          className="icon-button browser-overlay-button browser-overlay-close-button"
          title={closeTitle}
          aria-label="Hide browser"
          onClick={onClose}
        >
          <X size={14} aria-hidden="true" />
        </button>
        <form
          className="browser-overlay-nav-form"
          onSubmit={(event) => {
            event.preventDefault();
            onNavigate(draft);
            event.currentTarget.querySelector("input")?.blur();
          }}
        >
          <input
            ref={addressInputRef}
            type="text"
            className="form-field browser-overlay-url"
            value={draft}
            onChange={(event) => setDraft(event.currentTarget.value)}
            onBlur={() => setDraft(displayedUrl ?? "")}
            onKeyDown={(event) => {
              if (event.key === "Escape") {
                setDraft(displayedUrl ?? "");
                event.currentTarget.blur();
              }
            }}
            placeholder="Enter a URL"
            spellCheck={false}
            autoComplete="off"
            autoCapitalize="off"
            aria-label="Address"
          />
        </form>
        <div className="browser-overlay-nav-controls">
          <button
            type="button"
            className="icon-button browser-overlay-button"
            title="Refresh browser"
            aria-label="Refresh browser"
            onClick={() => {
              if (automated) {
                ignoreBrowserCommand(reloadBrowserAutomation(paneId));
              } else {
                onRefresh();
              }
            }}
          >
            <RotateCw size={14} aria-hidden="true" />
          </button>
          <button
            type="button"
            className="icon-button browser-overlay-button"
            title={
              sandbox
                ? "Can't open file content externally (would leak the access token)"
                : "Open in external browser"
            }
            aria-label="Open in external browser"
            onClick={() => onOpenExternal(displayedUrl ?? undefined)}
            // File-server content carries a capability token in its URL; opening it in the
            // OS browser would leak that token. The backend refuses it too, but disable the
            // affordance so the action isn't offered in the first place.
            disabled={!displayedUrl || sandbox}
          >
            <ExternalLink size={14} aria-hidden="true" />
          </button>
          <div className="browser-overlay-mode-group" role="group" aria-label="Browser engine">
            <button
              type="button"
              className={`icon-button browser-overlay-button browser-overlay-mode-button${mode === "webkit" ? " is-active" : ""}`}
              title="Use the WebKit browser"
              aria-label="Use the WebKit browser"
              aria-pressed={mode === "webkit"}
              onClick={() => onModeChange("webkit", automationSnapshot?.url ?? url)}
            >
              <Globe size={14} aria-hidden="true" />
            </button>
            <button
              type="button"
              className={`icon-button browser-overlay-button browser-overlay-mode-button${mode === "agent" ? " is-active" : ""}`}
              title={
                sandbox
                  ? "Agent browser is unavailable for protected file previews"
                  : "Use the mirrored Chromium agent browser"
              }
              aria-label="Use the mirrored Chromium agent browser"
              aria-pressed={mode === "agent"}
              disabled={sandbox}
              onClick={() => onModeChange("agent", url)}
            >
              <Bot size={14} aria-hidden="true" />
            </button>
          </div>
        </div>
      </div>
      <div ref={bodyRef} className="browser-overlay-body">
        {automated && automationSnapshot?.imageDataUrl ? (
          <img
            className="browser-overlay-frame is-automated"
            src={automationSnapshot.imageDataUrl}
            alt={automationSnapshot.title || "Automated browser tab"}
            draggable={false}
            tabIndex={0}
            onPointerDown={(event) => {
              event.preventDefault();
              const point = automationPoint(event);
              event.currentTarget.setPointerCapture(event.pointerId);
              event.currentTarget.focus();
              ignoreBrowserCommand(
                sendBrowserAutomationMouse(
                  paneId,
                  "down",
                  point.x,
                  point.y,
                  undefined,
                  undefined,
                  automationButton(event.button),
                  event.buttons,
                  automationModifiers(event),
                ),
              );
            }}
            onPointerUp={(event) => {
              event.preventDefault();
              const point = automationPoint(event);
              ignoreBrowserCommand(
                sendBrowserAutomationMouse(
                  paneId,
                  "up",
                  point.x,
                  point.y,
                  undefined,
                  undefined,
                  automationButton(event.button),
                  event.buttons,
                  automationModifiers(event),
                ),
              );
              if (event.currentTarget.hasPointerCapture(event.pointerId)) {
                event.currentTarget.releasePointerCapture(event.pointerId);
              }
            }}
            onPointerCancel={(event) => {
              const point = automationPoint(event);
              ignoreBrowserCommand(
                sendBrowserAutomationMouse(
                  paneId,
                  "up",
                  point.x,
                  point.y,
                  undefined,
                  undefined,
                  automationButton(event.button),
                  0,
                  automationModifiers(event),
                ),
              );
            }}
            onPointerMove={(event) => {
              const now = performance.now();
              if (now - lastAutomationMoveRef.current < 50) {
                return;
              }
              lastAutomationMoveRef.current = now;
              const point = automationPoint(event);
              ignoreBrowserCommand(
                sendBrowserAutomationMouse(
                  paneId,
                  "move",
                  point.x,
                  point.y,
                  undefined,
                  undefined,
                  "none",
                  event.buttons,
                  automationModifiers(event),
                ),
              );
            }}
            onContextMenu={(event) => event.preventDefault()}
            onWheel={(event) => {
              event.preventDefault();
              const point = automationPoint(event);
              ignoreBrowserCommand(
                sendBrowserAutomationMouse(
                  paneId,
                  "scroll",
                  point.x,
                  point.y,
                  event.deltaX,
                  event.deltaY,
                  "none",
                  event.buttons,
                  automationModifiers(event),
                ),
              );
            }}
            onKeyDown={handleAutomationKey}
            onPaste={(event) => {
              const text = event.clipboardData.getData("text/plain");
              if (!text) {
                return;
              }
              event.preventDefault();
              ignoreBrowserCommand(insertBrowserAutomationText(paneId, text));
            }}
          />
        ) : automated ? (
          <div className="browser-overlay-empty">
            <p>
              {automationSnapshot?.error ??
                "Connecting to the qmux automation browser…"}
            </p>
          </div>
        ) : frameUrl ? (
          <iframe
            key={`${frameUrl}::${reloadNonce}`}
            ref={frameRef}
            className={`browser-overlay-frame${sandbox ? " is-file-content" : ""}`}
            src={frameUrl}
            title="Browser overlay"
            // allow-scripts (so scripted reports still render) without
            // allow-same-origin (opaque origin → can't read the token-gated server).
            sandbox={sandbox ? "allow-scripts" : undefined}
            referrerPolicy="no-referrer"
          />
        ) : (
          <div className="browser-overlay-empty">
            <p>
              Nothing loaded yet. Run <code>qmux open &lt;file&gt;</code> (or enter a
              <code>http://localhost</code> URL above) to render a page here.
            </p>
          </div>
        )}
      </div>
      <div
        className="browser-overlay-resize-handle"
        role="separator"
        aria-label="Resize browser overlay"
        title="Resize browser overlay"
        onPointerDown={startResize}
      />
    </div>
  );
}
