import {
  forwardRef,
  memo,
  useCallback,
  useEffect,
  useImperativeHandle,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import type { CSSProperties } from "react";
import { useConfirm } from "../hooks/useConfirm";
import {
  focusNativeTerminal,
  pasteApprovedNativeTerminalText,
  performNativeTerminalAction,
  setNativeTerminalLayout,
  updateNativeTerminalSettings,
} from "../lib/api";
import { TERMINAL_PANE_CLASS } from "../lib/appHelpers";
import { readClipboardText } from "../lib/clipboard";
import {
  nativeTerminalLayoutRect,
  type NativeTerminalRect,
} from "../lib/nativeTerminalLayout";
import { inspectPaste } from "../lib/paste";
import type { PasteProtectionSettings } from "../lib/paste";
import type { PaneInfo } from "../types";
import PaneSearchBar from "./PaneSearchBar";

interface TerminalPaneProps {
  pane: PaneInfo;
  visible?: boolean;
  active: boolean;
  /** Previewed as the destination of a queue/wait option in another pane. */
  waitTargetPreview?: boolean;
  style?: CSSProperties;
  fontSize: number;
  fontFamily: string;
  letterSpacing: number;
  cursorBlink: boolean;
  cursorStyle: "block" | "underline" | "bar";
  scrollbackRows: number;
  scrollOnUserInput: boolean;
  scrollSensitivity: number;
  lineHeight: number;
  copyOnSelect: boolean;
  selectionClearOnCopy: boolean;
  themeName: string;
  pasteProtection: PasteProtectionSettings;
  /// Hold the last native frame while an internal resize drag updates the DOM.
  /// The final rectangle is applied when the drag ends.
  deferGeometryUpdates: boolean;
  /// True while this pane's surface is fully hidden behind an opaque web overlay
  /// (the expanded transcript). The surface keeps its geometry and stays visible,
  /// but its hidden→visible reveal is held one frame on the switch that first
  /// covers it so the overlay paints before the native surface appears.
  coveredByOverlay: boolean;
  inputBlocked: boolean;
  /** Allows viewing and scrolling while preventing all terminal input. */
  readOnly?: boolean;
  /// True while a web editable (composer, rename field, search input…) holds
  /// DOM focus. The native pane must not claim first responder then, or it
  /// would steal the keyboard mid-typing.
  webEditableFocused: boolean;
  requestAttach: (paneId: string) => void;
  onUserInput?: (agentId: string) => void;
  onActivate?: (paneId: string) => void;
  onOverlayStateChange?: (paneId: string, open: boolean) => void;
}

export interface TerminalPaneHandle {
  focus: () => void;
  openSearch: () => void;
  requestPaste: (text?: string | null) => void;
  reportUserInput: () => void;
}

const TerminalPane = forwardRef<TerminalPaneHandle, TerminalPaneProps>(function TerminalPane(
  {
    pane,
    visible: visibleProp,
    active,
    waitTargetPreview = false,
    style,
    fontSize,
    fontFamily,
    letterSpacing,
    lineHeight,
    cursorBlink,
    cursorStyle,
    scrollbackRows,
    scrollOnUserInput,
    scrollSensitivity,
    copyOnSelect,
    selectionClearOnCopy,
    themeName,
    pasteProtection,
    deferGeometryUpdates,
    coveredByOverlay,
    inputBlocked,
    readOnly = false,
    webEditableFocused,
    requestAttach,
    onUserInput,
    onActivate,
    onOverlayStateChange,
  },
  ref,
) {
  const visible = visibleProp ?? active;
  const hostRef = useRef<HTMLDivElement | null>(null);
  const lastVisibleRectRef = useRef<NativeTerminalRect | null>(null);
  const activeRef = useRef(active);
  const visibleRef = useRef(visible);
  const inputBlockedRef = useRef(inputBlocked);
  const readOnlyRef = useRef(readOnly);
  const webEditableFocusedRef = useRef(webEditableFocused);
  const pasteProtectionRef = useRef(pasteProtection);
  const onUserInputRef = useRef(onUserInput);
  const onActivateRef = useRef(onActivate);
  activeRef.current = active;
  visibleRef.current = visible;
  inputBlockedRef.current = inputBlocked;
  readOnlyRef.current = readOnly;
  webEditableFocusedRef.current = webEditableFocused;
  pasteProtectionRef.current = pasteProtection;
  onUserInputRef.current = onUserInput;
  onActivateRef.current = onActivate;

  const { confirm, dialog: confirmDialog } = useConfirm();
  const confirmOpen = Boolean(confirmDialog);
  const confirmRef = useRef(confirm);
  confirmRef.current = confirm;
  const searchInputRef = useRef<HTMLInputElement | null>(null);
  // WebKit does not reliably emit focusout when a focused element is removed
  // with its subtree. If this pane unmounts while its find input holds focus
  // (process-exit auto-close, close via context menu), blur it while it is
  // still attached — layout-effect cleanup runs before React detaches the
  // node — so the app-level sampler clears webEditableFocused instead of
  // leaving it wedged true, which would revoke keyboard from every remaining
  // terminal until the next real focus event.
  useLayoutEffect(() => {
    return () => {
      const input = searchInputRef.current;
      if (input && document.activeElement === input) {
        input.blur();
      }
    };
  }, []);
  const [searchOpen, setSearchOpen] = useState(false);
  const [searchTerm, setSearchTerm] = useState("");
  const searchOpenRef = useRef(searchOpen);
  const searchFocusPendingRef = useRef(false);
  const terminalFocusPendingRef = useRef(false);
  // The last visibility reported to the native surface, and whether the one-frame
  // reveal hold has already been spent for the current covered→visible transition.
  // Together these keep the surface's hidden→visible AppKit reveal from beating
  // WebKit's paint of the opaque expanded-transcript overlay above it — otherwise
  // the raw terminal flashes for a frame when control-tabbing to a covered pane.
  const nativeSurfaceVisibleRef = useRef(false);
  const coveredRevealHeldRef = useRef(false);
  const confirmOpenRef = useRef(confirmOpen);
  searchOpenRef.current = searchOpen;
  confirmOpenRef.current = confirmOpen;

  const focus = useCallback(() => {
    if (
      !activeRef.current ||
      !visibleRef.current ||
      inputBlockedRef.current ||
      readOnlyRef.current ||
      webEditableFocusedRef.current ||
      searchOpenRef.current ||
      confirmOpenRef.current
    ) {
      return;
    }
    void focusNativeTerminal(pane.id).catch(() => undefined);
  }, [pane.id]);

  const focusSearchInput = useCallback(() => {
    const input = searchInputRef.current;
    input?.focus({ preventScroll: true });
    input?.select();
  }, []);

  const openSearch = useCallback(() => {
    if (searchOpenRef.current) {
      focusSearchInput();
      return;
    }
    terminalFocusPendingRef.current = false;
    searchOpenRef.current = true;
    searchFocusPendingRef.current = true;
    setSearchOpen(true);
  }, [focusSearchInput]);

  const closeSearch = useCallback((restoreTerminalFocus: boolean) => {
    if (!searchOpenRef.current) {
      return;
    }
    searchOpenRef.current = false;
    searchFocusPendingRef.current = false;
    terminalFocusPendingRef.current = restoreTerminalFocus;
    // WebKit does not reliably emit focusout when the focused element is
    // removed. Blur it first so the app can clear webEditableFocused before
    // this search bar unmounts. The ref guard above ignores the nested blur
    // callback from PaneSearchBar.
    searchInputRef.current?.blur();
    setSearchOpen(false);
    setSearchTerm("");
    void performNativeTerminalAction(pane.id, "end_search").catch(() => undefined);
  }, [pane.id]);

  const reportUserInput = useCallback(() => {
    if (pane.agentId) {
      onUserInputRef.current?.(pane.agentId);
    }
  }, [pane.agentId]);

  // Prefer the text captured natively inside the paste gesture: reading the
  // clipboard from here is programmatic access, which macOS 15+ answers with
  // its pasteboard privacy alert on every paste. The read fallback covers
  // callers that have no native capture.
  const requestPaste = useCallback((capturedText?: string | null) => {
    if (inputBlockedRef.current || readOnlyRef.current) {
      return;
    }
    const textPromise =
      typeof capturedText === "string" ? Promise.resolve(capturedText) : readClipboardText();
    void textPromise
      .then(async (text) => {
        if (!text) {
          return;
        }
        const verdict = inspectPaste(text, pasteProtectionRef.current);
        if (verdict.action === "reject") {
          await confirmRef.current({ message: verdict.message, confirmLabel: "OK" });
          return;
        }
        if (
          verdict.action === "confirm" &&
          !(await confirmRef.current({ message: verdict.message, confirmLabel: "Paste" }))
        ) {
          return;
        }
        reportUserInput();
        await pasteApprovedNativeTerminalText(pane.id, text);
      })
      .catch(() => undefined);
  }, [pane.id, reportUserInput]);

  useImperativeHandle(
    ref,
    () => ({
      focus,
      openSearch,
      requestPaste,
      reportUserInput,
    }),
    [focus, openSearch, reportUserInput, requestPaste],
  );

  useLayoutEffect(() => {
    // Publish terminal-local blockers in the same commit in which their DOM
    // focus target appears/disappears. App owns the logical native keyboard
    // target, so a passive effect would leave one paint where its calculation
    // still grants the terminal keyboard.
    onOverlayStateChange?.(pane.id, searchOpen || confirmOpen);
    return () => onOverlayStateChange?.(pane.id, false);
  }, [confirmOpen, onOverlayStateChange, pane.id, searchOpen]);

  useEffect(() => {
    // Surface creation and the pane-created event cross the native/web boundary
    // independently. The backend keeps attach retryable until Ghostty accepts
    // the restored history and startup backlog, so request it as soon as this
    // pane mounts instead of coupling delivery to the settings request below.
    requestAttach(pane.id);
  }, [pane.id, requestAttach]);

  useEffect(() => {
    void updateNativeTerminalSettings({
      paneId: pane.id,
      fontSize,
      fontFamily,
      letterSpacing,
      lineHeight,
      cursorBlink,
      cursorStyle,
      scrollbackRows,
      scrollOnUserInput,
      scrollSensitivity,
      copyOnSelect,
      selectionClearOnCopy,
      themeName,
    }).catch((err) => {
      console.error(`qmux: failed to apply native terminal settings for ${pane.id}:`, err);
    });
  }, [
    copyOnSelect,
    cursorBlink,
    cursorStyle,
    fontFamily,
    fontSize,
    letterSpacing,
    lineHeight,
    pane.id,
    scrollSensitivity,
    scrollbackRows,
    scrollOnUserInput,
    selectionClearOnCopy,
    themeName,
  ]);

  useEffect(() => {
    if (!searchOpen) {
      return;
    }
    const action = searchTerm ? `search:${searchTerm}` : "start_search";
    void performNativeTerminalAction(pane.id, action).catch(() => undefined);
  }, [pane.id, searchOpen, searchTerm]);

  useLayoutEffect(() => {
    const host = hostRef.current;
    if (!host) {
      return;
    }
    let frame: number | null = null;
    const syncLayout = () => {
      frame = null;
      const measuredRect = host.getBoundingClientRect();
      const wantSurfaceVisible =
        visible && measuredRect.width > 0 && measuredRect.height > 0;
      const rect = nativeTerminalLayoutRect(
        measuredRect,
        wantSurfaceVisible,
        lastVisibleRectRef.current,
      );
      if (wantSurfaceVisible) {
        lastVisibleRectRef.current = rect;
      }
      let surfaceVisible = wantSurfaceVisible;
      // When a pane that is fully covered by the opaque expanded-transcript overlay
      // becomes visible, its native surface's hidden→visible transition can reach
      // AppKit before WebKit paints the covering overlay, flashing the raw terminal
      // for a frame on a control-tab switch. Hold that first reveal one frame so the
      // overlay paints first; the surface then shows behind it and stays visible, so
      // collapsing the transcript still reveals the terminal instantly. Only the
      // hidden→visible transition is held: a surface already visible when the
      // transcript expands is never hidden (that would SIGWINCH the TUI).
      if (
        wantSurfaceVisible &&
        coveredByOverlay &&
        !nativeSurfaceVisibleRef.current &&
        !coveredRevealHeldRef.current
      ) {
        // Release the hold only from the rAF callback, never synchronously: a
        // second layout pass in the same pre-paint commit must keep holding
        // rather than reveal early (which would defeat the hold and flash again).
        surfaceVisible = false;
        if (frame !== null) {
          cancelAnimationFrame(frame);
        }
        frame = requestAnimationFrame(() => {
          coveredRevealHeldRef.current = true;
          syncLayout();
        });
      } else if (!wantSurfaceVisible) {
        coveredRevealHeldRef.current = false;
      }
      nativeSurfaceVisibleRef.current = surfaceVisible;
      const ownsKeyboard =
        surfaceVisible &&
        active &&
        !inputBlocked &&
        !readOnly &&
        !searchOpen &&
        !confirmOpen &&
        !webEditableFocused;
      const settleSearchFocus = () => {
        if (!searchOpen || !searchOpenRef.current || !searchFocusPendingRef.current) {
          return;
        }
        searchFocusPendingRef.current = false;
        focusSearchInput();
      };
      const settleTerminalFocus = () => {
        if (!ownsKeyboard || !terminalFocusPendingRef.current) {
          return;
        }
        terminalFocusPendingRef.current = false;
        // setNativeTerminalLayout has now re-enabled keyboard input in Swift;
        // explicitly focus once more so Escape/X always complete the handoff.
        focus();
      };
      // Wait until AppKit has synchronously released the native surface as
      // first responder. Focusing the DOM input on an unrelated animation
      // frame can race this bridge call and leave the find field unfocused.
      void setNativeTerminalLayout({
        paneId: pane.id,
        x: rect.left,
        y: rect.top,
        width: rect.width,
        height: rect.height,
        visible: surfaceVisible,
        acceptsPointerInput: !inputBlocked && !searchOpen && !confirmOpen,
        // Unlike ownsKeyboard this ignores transient web focus states: a click
        // may still claim the keyboard away from a composer, but never into a
        // pane whose input is blocked by policy (read-only research panes).
        acceptsKeyboardClaim: !inputBlocked && !readOnly,
        deferGeometry: deferGeometryUpdates,
      }).then(
        () => {
          settleSearchFocus();
          settleTerminalFocus();
        },
        settleSearchFocus,
      );
    };
    const scheduleLayout = () => {
      if (frame !== null) {
        cancelAnimationFrame(frame);
      }
      frame = requestAnimationFrame(syncLayout);
    };
    const observer = new ResizeObserver(scheduleLayout);
    observer.observe(host);
    // Ownership changes must reach AppKit in the same React layout phase. A
    // second animation-frame delay leaves a window where a newly focused web
    // editor can have its first key stolen by the native surface.
    syncLayout();
    return () => {
      observer.disconnect();
      if (frame !== null) {
        cancelAnimationFrame(frame);
      }
    };
  }, [
    active,
    confirmOpen,
    coveredByOverlay,
    deferGeometryUpdates,
    focus,
    focusSearchInput,
    inputBlocked,
    readOnly,
    pane.id,
    searchOpen,
    style,
    visible,
    webEditableFocused,
  ]);

  const findNext = () => {
    void performNativeTerminalAction(pane.id, "navigate_search:next").catch(() => undefined);
  };
  const findPrevious = () => {
    void performNativeTerminalAction(pane.id, "navigate_search:previous").catch(() => undefined);
  };

  return (
    <div
      className={[
        TERMINAL_PANE_CLASS,
        "is-native",
        visible ? "is-visible" : "",
        active ? "is-focused" : "",
        waitTargetPreview ? "is-wait-target-preview" : "",
      ]
        .filter(Boolean)
        .join(" ")}
      aria-hidden={!visible}
      style={style}
      onPointerDown={() => onActivateRef.current?.(pane.id)}
    >
      <div ref={hostRef} className="terminal-host" />
      {confirmDialog}
      {searchOpen ? (
        <PaneSearchBar
          inputRef={searchInputRef}
          placeholder="Find in terminal"
          term={searchTerm}
          onTermChange={setSearchTerm}
          matchIndex={-1}
          matchCount={null}
          caseSensitive={false}
          onCaseSensitiveChange={() => undefined}
          useRegex={false}
          onUseRegexChange={() => undefined}
          showOptions={false}
          onFindNext={findNext}
          onFindPrevious={findPrevious}
          onClose={() => closeSearch(true)}
          onFocusLeave={() => closeSearch(false)}
        />
      ) : null}
    </div>
  );
});

export default memo(TerminalPane);
