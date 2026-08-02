import { Bot, ExternalLink, Globe, RotateCw, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type {
  CSSProperties,
  KeyboardEvent as ReactKeyboardEvent,
  PointerEvent as ReactPointerEvent,
} from "react";
import type { UnlistenFn } from "@tauri-apps/api/event";
import type { BrowserOverlayMode, BrowserOverlaySize } from "../appTypes";
import {
  claimNativeTerminalPointerForWebDrag,
  getBrowserAutomationSnapshot,
  insertBrowserAutomationText,
  listenToBrowserScreencastFrames,
  navigateBrowserAutomation,
  reloadBrowserAutomation,
  sendBrowserAutomationKey,
  sendBrowserAutomationMouse,
  setNativeTerminalIframeShortcutFallback,
  startBrowserScreencast,
  stopBrowserScreencast,
} from "../lib/api";
import type { BrowserAutomationSnapshot } from "../lib/api";

const MIN_BROWSER_OVERLAY_WIDTH = 360;
const MIN_BROWSER_OVERLAY_HEIGHT = 240;
const BROWSER_OVERLAY_LEFT_INSET_FALLBACK = 64;
const BROWSER_OVERLAY_BOTTOM_INSET = 50;
// Chromium pushes a frame whenever the mirrored page composites, so the only
// polling left is a metadata refresh: it keeps the address bar current and
// restarts the stream after the overlay is resized or its tab is replaced.
const SCREENCAST_HEARTBEAT_MS = 1000;
// A headless build that refuses to composite would leave the mirror blank
// forever, so give the first frame a deadline and fall back to screenshots.
const SCREENCAST_FIRST_FRAME_MS = 2500;
// Screencast frames are always CSS-resolution — Chromium's screencast scale
// only ever shrinks a frame, so no device scale makes it stream at 2x. Once
// the page stops compositing, replace the last frame with a screenshot, which
// does honour the emulated scale and comes back Retina-sharp.
const SCREENCAST_SETTLE_MS = 250;
// Chromium occasionally composites once more just after a capture. That frame
// shows what the capture already shows, at half the resolution, so displaying
// it would soften a settled mirror for no reason.
const SCREENCAST_SETTLE_ECHO_MS = 200;
const SNAPSHOT_POLL_MS = 500;
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
  // False when opening a target that already exists (for example from a PiP):
  // observe its current page without replaying the React state's URL.
  navigateAgentOnOpen?: boolean;
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
  navigateAgentOnOpen = true,
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
  const [automationSnapshot, setAutomationSnapshot] = useState<BrowserAutomationSnapshot | null>(
    null,
  );
  // The live mirror image — a streamed frame, or the sharper capture it
  // settles on. Held apart from the metadata above so a heartbeat that carries
  // no screenshot can't blank the view between frames.
  const [mirrorFrame, setMirrorFrame] = useState<string | null>(null);
  const overlayRef = useRef<HTMLDivElement | null>(null);
  const bodyRef = useRef<HTMLDivElement | null>(null);
  const addressInputRef = useRef<HTMLInputElement | null>(null);
  const frameRef = useRef<HTMLIFrameElement | null>(null);
  const lastAutomationMoveRef = useRef(0);
  const cleanupResizeRef = useRef<(() => void) | null>(null);

  const automated = mode === "agent" && !sandbox;
  const displayedUrl = automated ? (automationSnapshot?.url ?? url) : url;
  const mirrorImage = mirrorFrame ?? automationSnapshot?.imageDataUrl ?? null;

  useEffect(() => {
    if (document.activeElement !== addressInputRef.current) {
      setDraft(displayedUrl ?? "");
    }
  }, [displayedUrl]);

  useEffect(() => {
    if (!automated) {
      setAutomationSnapshot(null);
      setMirrorFrame(null);
      return;
    }
    let cancelled = false;
    let unlisten: UnlistenFn | null = null;
    let heartbeatTimer: number | null = null;
    let firstFrameTimer: number | null = null;
    let settleTimer: number | null = null;
    let snapshotTimer: number | null = null;
    let requestInFlight = false;
    let streaming = false;
    // Bumped by every streamed frame so a slow refinement capture can tell it
    // has been overtaken by newer motion and drop its result.
    let frameSequence = 0;
    let settledAt = 0;

    const measure = () => {
      const bodyRect = bodyRef.current?.getBoundingClientRect();
      return {
        width: Math.round(clampSize(bodyRect?.width ?? 1280, MIN_BROWSER_OVERLAY_WIDTH, 4096)),
        height: Math.round(clampSize(bodyRect?.height ?? 900, MIN_BROWSER_OVERLAY_HEIGHT, 4096)),
        scaleFactor: Math.min(Math.max(window.devicePixelRatio || 1, 1), 2),
      };
    };

    const reportError = (error: unknown, currentUrl: string | null = null) => {
      if (cancelled) {
        return;
      }
      streaming = false;
      setMirrorFrame(null);
      setAutomationSnapshot({
        available: false,
        tabId: null,
        url: currentUrl,
        title: null,
        imageDataUrl: null,
        width: 1280,
        height: 900,
        error: error instanceof Error ? error.message : String(error),
      });
    };

    const pollSnapshot = async () => {
      if (cancelled || requestInFlight) {
        return;
      }
      requestInFlight = true;
      try {
        const { width, height, scaleFactor } = measure();
        const snapshot = await getBrowserAutomationSnapshot(paneId, width, height, scaleFactor);
        if (!cancelled) {
          setAutomationSnapshot(snapshot);
        }
      } catch (error) {
        reportError(error);
      } finally {
        requestInFlight = false;
      }
    };

    // One-way switch: once this pane is known not to stream, stop asking for a
    // screencast and go back to the screenshot poll for the rest of the mount.
    const fallBackToSnapshots = () => {
      if (cancelled || snapshotTimer !== null) {
        return;
      }
      if (heartbeatTimer !== null) {
        window.clearInterval(heartbeatTimer);
        heartbeatTimer = null;
      }
      if (settleTimer !== null) {
        window.clearTimeout(settleTimer);
        settleTimer = null;
      }
      setMirrorFrame(null);
      void stopBrowserScreencast(paneId).catch(() => undefined);
      void pollSnapshot();
      snapshotTimer = window.setInterval(() => void pollSnapshot(), SNAPSHOT_POLL_MS);
    };

    // Trade the last streamed frame for a screenshot at the display's scale.
    // A capture occasionally nudges Chromium into compositing once more; the
    // echo window below keeps that from softening the mirror back down.
    const settleAtDisplayScale = async () => {
      if (cancelled || snapshotTimer !== null) {
        return;
      }
      if (requestInFlight) {
        armSettle();
        return;
      }
      const sequence = frameSequence;
      requestInFlight = true;
      try {
        const { width, height, scaleFactor } = measure();
        const snapshot = await getBrowserAutomationSnapshot(paneId, width, height, scaleFactor);
        if (cancelled || frameSequence !== sequence || !snapshot.imageDataUrl) {
          return;
        }
        setAutomationSnapshot(snapshot);
        setMirrorFrame(snapshot.imageDataUrl);
        settledAt = performance.now();
      } catch {
        // The stream is still the source of truth; the next quiet moment retries.
      } finally {
        requestInFlight = false;
      }
    };

    function armSettle() {
      if (settleTimer !== null) {
        window.clearTimeout(settleTimer);
      }
      settleTimer = window.setTimeout(() => {
        settleTimer = null;
        void settleAtDisplayScale();
      }, SCREENCAST_SETTLE_MS);
    }

    const refreshScreencast = async () => {
      if (cancelled || requestInFlight || snapshotTimer !== null) {
        return;
      }
      requestInFlight = true;
      try {
        const { width, height, scaleFactor } = measure();
        const status = await startBrowserScreencast(paneId, width, height, scaleFactor);
        if (cancelled) {
          return;
        }
        setAutomationSnapshot(status);
        if (!status.available) {
          streaming = false;
          setMirrorFrame(null);
          if (firstFrameTimer !== null) {
            window.clearTimeout(firstFrameTimer);
            firstFrameTimer = null;
          }
          return;
        }
        if (status.available && !streaming && firstFrameTimer === null) {
          firstFrameTimer = window.setTimeout(() => {
            firstFrameTimer = null;
            if (!streaming) {
              fallBackToSnapshots();
            }
          }, SCREENCAST_FIRST_FRAME_MS);
        }
      } catch (error) {
        reportError(error);
      } finally {
        requestInFlight = false;
      }
    };

    void listenToBrowserScreencastFrames((frame) => {
      if (cancelled || frame.paneId !== paneId || snapshotTimer !== null) {
        return;
      }
      streaming = true;
      frameSequence += 1;
      if (firstFrameTimer !== null) {
        window.clearTimeout(firstFrameTimer);
        firstFrameTimer = null;
      }
      // An echo is still worth re-settling on — if the page genuinely changed,
      // the capture that follows picks it up sharply — it just isn't worth
      // showing in place of the capture it echoes.
      if (performance.now() - settledAt >= SCREENCAST_SETTLE_ECHO_MS) {
        setMirrorFrame(frame.imageDataUrl);
      }
      armSettle();
      // Frames carry the tab's current identity, so navigation shows up in the
      // address bar as soon as it paints rather than on the next heartbeat.
      setAutomationSnapshot((previous) =>
        previous && (previous.url !== frame.url || previous.title !== frame.title)
          ? { ...previous, url: frame.url, title: frame.title }
          : previous,
      );
    }).then(
      (stop) => {
        if (cancelled) {
          stop();
          return;
        }
        unlisten = stop;
      },
      () => fallBackToSnapshots(),
    );

    void (async () => {
      if (
        navigateAgentOnOpen &&
        url &&
        lastAutomationNavigationByPane.get(paneId) !== reloadNonce
      ) {
        try {
          await navigateBrowserAutomation(paneId, url);
          rememberAutomationNavigation(paneId, reloadNonce);
        } catch (error) {
          reportError(error, url);
        }
      }
      await refreshScreencast();
    })();
    heartbeatTimer = window.setInterval(() => void refreshScreencast(), SCREENCAST_HEARTBEAT_MS);

    return () => {
      cancelled = true;
      if (heartbeatTimer !== null) {
        window.clearInterval(heartbeatTimer);
      }
      if (snapshotTimer !== null) {
        window.clearInterval(snapshotTimer);
      }
      if (firstFrameTimer !== null) {
        window.clearTimeout(firstFrameTimer);
      }
      if (settleTimer !== null) {
        window.clearTimeout(settleTimer);
      }
      unlisten?.();
      void stopBrowserScreencast(paneId).catch(() => undefined);
    };
  }, [automated, navigateAgentOnOpen, paneId, reloadNonce, url]);

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
        {automated && mirrorImage ? (
          <img
            className="browser-overlay-frame is-automated"
            src={mirrorImage}
            alt={automationSnapshot?.title || "Automated browser tab"}
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
