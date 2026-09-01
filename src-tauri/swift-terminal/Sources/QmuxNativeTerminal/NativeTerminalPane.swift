import AppKit
import Foundation
import GhosttyTerminal

@_silgen_name("qmux_native_terminal_did_close")
private func nativeTerminalDidClose(_ paneID: UnsafePointer<CChar>, _ processAlive: Int32)

@_silgen_name("qmux_native_terminal_did_change_title")
private func nativeTerminalDidChangeTitle(
    _ paneID: UnsafePointer<CChar>,
    _ title: UnsafePointer<CChar>
)

@_silgen_name("qmux_native_terminal_did_change_cwd")
private func nativeTerminalDidChangeCwd(
    _ paneID: UnsafePointer<CChar>,
    _ cwd: UnsafePointer<CChar>
)

@_silgen_name("qmux_native_terminal_did_resize")
private func nativeTerminalDidResize(_ paneID: UnsafePointer<CChar>, _ columns: Int32, _ rows: Int32)

@_silgen_name("qmux_native_terminal_did_change_annotation_viewport")
private func nativeTerminalDidChangeAnnotationViewport(
    _ paneID: UnsafePointer<CChar>,
    _ json: UnsafePointer<CChar>
)

@_silgen_name("qmux_native_terminal_did_write")
private func nativeTerminalDidWrite(
    _ paneID: UnsafePointer<CChar>,
    _ bytes: UnsafePointer<UInt8>,
    _ length: Int
)

@_silgen_name("qmux_native_terminal_did_request_search")
private func nativeTerminalDidRequestSearch(_ paneID: UnsafePointer<CChar>)

@_silgen_name("qmux_native_terminal_did_request_paste")
private func nativeTerminalDidRequestPaste(
    _ paneID: UnsafePointer<CChar>,
    _ text: UnsafePointer<CChar>
)

@_silgen_name("qmux_native_terminal_did_receive_user_input")
private func nativeTerminalDidReceiveUserInput(_ paneID: UnsafePointer<CChar>)

@_silgen_name("qmux_native_terminal_did_receive_escape")
private func nativeTerminalDidReceiveEscape(_ paneID: UnsafePointer<CChar>)

@_silgen_name("qmux_native_terminal_did_receive_shortcut")
private func nativeTerminalDidReceiveShortcut(
    _ paneID: UnsafePointer<CChar>,
    _ key: UnsafePointer<CChar>,
    _ shift: Int32,
    _ control: Int32,
    _ option: Int32,
    _ command: Int32,
    _ repeat: Int32
) -> Int32

@_silgen_name("qmux_native_terminal_did_change_command_modifier")
private func nativeTerminalDidChangeCommandModifier(
    _ paneID: UnsafePointer<CChar>,
    _ active: Int32
)

@_silgen_name("qmux_native_terminal_did_activate")
private func nativeTerminalDidActivate(_ paneID: UnsafePointer<CChar>)

@_silgen_name("qmux_native_terminal_did_open_url")
private func nativeTerminalDidOpenURL(
    _ paneID: UnsafePointer<CChar>,
    _ url: UnsafePointer<CChar>,
    _ kind: Int32
)

@MainActor
final class NativeTerminalPane: NSObject,
    TerminalSurfaceCloseDelegate,
    TerminalSurfaceTitleDelegate,
    TerminalSurfacePwdDelegate,
    TerminalSurfaceResizeDelegate,
    TerminalSurfaceScrollbarDelegate,
    TerminalSurfaceOpenURLDelegate
{
    static let gridPadding = 10
    static let gridPaddingPoints = CGPoint(x: gridPadding, y: gridPadding)
    let paneID: String
    let view: QmuxTerminalView
    let controller: TerminalController
    let terminalSession: InMemoryTerminalSession
    var acceptsPointerInput = true
    /// True only while this pane is the applied keyboard owner. Logical
    /// ownership is revisioned separately in NativeTerminalHost; geometry
    /// updates never write this flag.
    var acceptsKeyboardInput = false
    /// Whether a pointer gesture may optimistically grant this pane the
    /// keyboard before React's layout update lands. False for panes whose
    /// keyboard denial is a hard policy (read-only research panes, blocked
    /// input) rather than a transient focus state. The pointer monitor may
    /// optimistically claim only panes for which this remains true.
    var acceptsKeyboardClaim = true
    var isFocused = false
    /// True once the view has been fitted to a real (nonzero) frame. Until
    /// then the surface still has its zero-frame default grid, and replaying
    /// restored scrollback into it would be reflowed — and scrambled — by the
    /// first real fit; the host gates replay on this flag.
    var hasCommittedGeometry = false
    var consumedShortcutKeyCodes: Set<UInt16> = []
    /// Tauri settings invokes may complete out of order while a theme is being
    /// previewed. Once a newer snapshot reaches this pane, an older one must
    /// never repaint it.
    private var settingsRevision: UInt64 = 0
    private var lastUserInputReport = Date.distantPast
    /// Last grid Ghostty reported that we have not yet turned into a PTY ioctl.
    private var pendingPtyColumns: Int32?
    private var pendingPtyRows: Int32?
    /// Last grid we actually delivered to Rust. Duplicate reports (the
    /// in-memory session callback plus the surface delegate, or two fits
    /// of the same frame) must not TIOCSWINSZ again.
    private var flushedPtyColumns: Int32?
    private var flushedPtyRows: Int32?
    private var ptyResizeFlushGeneration: UInt64 = 0
    private var ptyResizeFlushScheduled = false
    private var annotationScrollbar = TerminalAnnotationScrollbar(
        totalRows: 0,
        offsetRows: 0,
        visibleRows: 0
    )
    private var annotationViewportRevision: UInt64 = 0
    private var annotationViewportReportScheduled = false
    private var annotationScrollbarIsInitialized = false
    private var annotationMonitoringEnabled = false

    init(
        paneID: String,
        workingDirectory: String?,
        themeName: String
    ) {
        self.paneID = paneID
        terminalSession = InMemoryTerminalSession(
            write: { data in
                guard !data.isEmpty else { return }
                paneID.withCString { paneID in
                    data.withUnsafeBytes { buffer in
                        guard let base = buffer.baseAddress else { return }
                        nativeTerminalDidWrite(
                            paneID,
                            base.assumingMemoryBound(to: UInt8.self),
                            buffer.count
                        )
                    }
                }
            },
            resize: { viewport in
                guard let columns = Int32(exactly: viewport.columns),
                      let rows = Int32(exactly: viewport.rows)
                else { return }
                NativeTerminalHost.enqueuePanePtyResizeFromAnyThread(
                    id: paneID,
                    columns: columns,
                    rows: rows
                )
            }
        )
        // The explicit theme is load-bearing: TerminalController's own default
        // is Alabaster/Afterglow, which follows the OS appearance — Alabaster
        // would repaint every pane white whenever macOS reports light mode.
        // QmuxTerminalTheme puts the same colors in both appearance slots, so
        // panes track the selected qmux theme instead of the OS appearance.
        controller = TerminalController(
            theme: QmuxTerminalTheme.theme(named: themeName)
        ) { builder in
            builder.withWindowPaddingX(Self.gridPadding)
            builder.withWindowPaddingY(Self.gridPadding)
            builder.withCustom("shell-integration", "none")
            builder.withCustom("confirm-close-surface", "false")
            // Keep Shift as an unconditional escape hatch from application
            // mouse reporting. The default permits a TUI to request Shift
            // capture with XTSHIFTESCAPE, which can make Shift-scroll reach
            // the program instead of Ghostty's scrollback. `never` preserves
            // Ghostty's native wheel/trackpad scrolling even for those apps.
            builder.withCustom("mouse-shift-capture", "never")
            // ⌘Q is passed through the native key monitor so the app menu can
            // run qmux's exit confirmation — Ghostty's own quit binding would
            // consume it first via performKeyEquivalent.
            builder.withCustom("keybind", "super+q=unbind")
            // Ghostty's remaining surface/app-management defaults must go the
            // same way. These chords belong to qmux's shortcut layer (the
            // NativeTerminalHost key monitor + classifiers), which normally
            // consumes them before Ghostty sees a thing — but on any missed
            // path (keyboard-owner/first-responder desync, non-US layouts
            // where charactersIgnoringModifiers isn't the classifier's key,
            // pre-init state gaps) the surface's own default binding would
            // fire instead. ⌘W's close_surface tears the pane down with no
            // qmux confirmation (confirm-close-surface is false above), and
            // the font-size trio silently diverges the surface from qmux's
            // font settings. Once unbound, a missed chord falls through to
            // the running program (e.g. kitty-keyboard-protocol TUIs) or to
            // nothing — never to a divergent Ghostty action. ⌘K (clear
            // screen) and ⌘C (copy) stay bound: those are deliberately left
            // native for a focused terminal.
            for chord in [
                "super+w",  // close_surface, bypassing requestClosePane
                "super+shift+w",  // close_window
                "super+alt+shift+w",  // close_all_windows
                "super+t",  // new_tab — qmux: new pane / research
                "super+n",  // new_window — qmux: new agent / research
                "super+shift+h",  // qmux: conversation history
                "super+shift+g",  // qmux: toggle left sidebar
                "super+shift+l",  // qmux: toggle right bar
                "super+d",  // new_split:right — qmux: split pane below
                "super+shift+d",  // new_split:down — qmux: split pane left/right
                "super+comma",  // open_config — qmux: settings
                "super+equal",  // increase_font_size — qmux: font zoom
                "super+plus",  // increase_font_size
                "super+minus",  // decrease_font_size
                "super+zero",  // reset_font_size
                "ctrl+tab",  // next_tab — qmux: cycle pane tab
                "ctrl+shift+tab",  // previous_tab
                "super+shift+left_bracket",  // previous_tab — qmux: cycle all tabs
                "super+shift+right_bracket",  // next_tab — qmux: cycle all tabs
                "super+alt+up",  // qmux: move active sidebar item up
                "super+alt+down",  // qmux: move active sidebar item down
                // Native App-menu recovery for a dead WebKit renderer. The
                // QmuxTerminalView key-equivalent override lets this fall
                // through to AppKit instead of Ghostty's catch-all.
                "super+alt+r",
                // goto_tab 1-8 and last_tab — qmux: focus tab N. Ghostty's
                // defaults bind the digits by physical key, so the unbind
                // trigger must match that form exactly.
                "super+physical:one",
                "super+physical:two",
                "super+physical:three",
                "super+physical:four",
                "super+physical:five",
                "super+physical:six",
                "super+physical:seven",
                "super+physical:eight",
                "super+physical:nine",
                // No Ghostty default action today, but qmux's
                // Command-Shift-R terminal/research mode toggle — keep any
                // future Ghostty binding from claiming it. Note unbinding alone
                // can NOT reclaim a chord Ghostty has no binding for: upstream
                // performKeyEquivalent swallows every unclaimed ⌘ chord
                // regardless, which is why QmuxTerminalView.performKeyEquivalent
                // offers chords to the qmux shortcut classifier first.
                "super+shift+r",
            ] {
                builder.withCustom("keybind", "\(chord)=unbind")
            }
        }
        view = QmuxTerminalView(frame: .zero)
        super.init()

        // Match the configured Ghostty background so a freshly shown pane
        // paints terminal-colored pixels before its first Metal frame instead
        // of exposing the window's vibrancy material through the transparent
        // webview above it.
        view.layer?.backgroundColor = QmuxTerminalTheme.backgroundColor(
            named: themeName
        )
        view.delegate = self
        view.configuration = TerminalSurfaceOptions(
            backend: .inMemory(terminalSession),
            workingDirectory: workingDirectory,
            context: .split
        )
        view.autoresizingMask = []
        view.setAccessibilityElement(true)
        view.setAccessibilityLabel("Terminal")
        view.setAccessibilityIdentifier("terminal.\(paneID)")
        view.onPasteRequest = { [weak self] in self?.requestPaste() }
        view.onAppShortcutKeyEquivalent = { [weak self] event in
            guard let self else { return false }
            return NativeTerminalHost.shared.claimAppShortcut(event, for: self)
        }
        view.shouldOfferAppShortcutFallback = { [weak self] in
            guard let self else { return false }
            return NativeTerminalHost.shared.shouldOfferKeyEquivalentFallback(for: self)
        }
        view.annotationViewportState = { [weak self] in
            guard let self else { return (0, 0, 0, false) }
            return (
                self.annotationViewportRevision,
                TerminalAnnotationContentRegistry.shared.generation(for: self.paneID),
                self.annotationScrollbar.offsetRows,
                self.annotationScrollbarIsInitialized
            )
        }
    }

    func terminalDidClose(processAlive: Bool) {
        paneID.withCString { nativeTerminalDidClose($0, processAlive ? 1 : 0) }
        NativeTerminalHost.shared.surfaceDidClose(id: paneID)
    }

    func terminalDidChangeTitle(_ title: String) {
        // Only programs setting a real OSC title should reach the tab bar;
        // Ghostty's default title for a host-managed surface is empty.
        if title.isEmpty {
            return
        }
        paneID.withCString { paneID in
            title.withCString { nativeTerminalDidChangeTitle(paneID, $0) }
        }
    }

    func terminalDidChangeWorkingDirectory(_ path: String) {
        paneID.withCString { paneID in
            path.withCString { nativeTerminalDidChangeCwd(paneID, $0) }
        }
    }

    func terminalDidResize(columns: Int, rows: Int) {
        annotationViewportRevision &+= 1
        scheduleAnnotationViewportReport()
        guard let columns = Int32(exactly: columns),
              let rows = Int32(exactly: rows)
        else {
            return
        }
        enqueuePtyResize(columns: columns, rows: rows)
    }

    func terminalDidChangeScrollbar(_ metrics: TerminalScrollbarMetrics) {
        let next = TerminalAnnotationScrollbar(
            totalRows: metrics.total,
            offsetRows: metrics.offset,
            visibleRows: metrics.length
        )
        let wasInitialized = annotationScrollbarIsInitialized
        annotationScrollbarIsInitialized = true
        if next != annotationScrollbar || !wasInitialized {
            annotationScrollbar = next
            annotationViewportRevision &+= 1
            scheduleAnnotationViewportReport()
        }
    }

    /// Scrollbar actions can arrive once per streamed row. Keep the native
    /// revision exact, but cross the Swift/Rust/WebKit boundary at most once
    /// per display interval.
    private func scheduleAnnotationViewportReport() {
        guard annotationMonitoringEnabled, !annotationViewportReportScheduled else { return }
        annotationViewportReportScheduled = true
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.0 / 60.0) { [weak self] in
            guard let self else { return }
            self.annotationViewportReportScheduled = false
            guard self.annotationMonitoringEnabled else { return }
            guard let metrics = self.view.terminalGridMetrics() else { return }
            guard let scale = self.view.window?.backingScaleFactor else { return }
            guard let snapshot = TerminalAnnotationGeometry.viewportSnapshot(
                scrollbar: self.annotationScrollbar,
                scrollbarIsInitialized: self.annotationScrollbarIsInitialized,
                metrics: metrics,
                bounds: self.view.bounds,
                backingScaleFactor: scale,
                gridPaddingPoints: Self.gridPaddingPoints,
                viewportRevision: self.annotationViewportRevision,
                contentGeneration: TerminalAnnotationContentRegistry.shared
                    .generation(for: self.paneID)
            ),
            let data = try? JSONEncoder().encode(snapshot),
            let json = String(data: data, encoding: .utf8)
            else { return }
            self.paneID.withCString { paneID in
                json.withCString { nativeTerminalDidChangeAnnotationViewport(paneID, $0) }
            }
        }
    }

    func setAnnotationMonitoring(_ enabled: Bool) {
        annotationMonitoringEnabled = enabled
        TerminalAnnotationContentRegistry.shared.setMonitoring(enabled, for: paneID)
        if enabled {
            scheduleAnnotationViewportReport()
        }
    }

    func annotationContentDidChange() {
        scheduleAnnotationViewportReport()
    }

    func annotationSelectionSnapshot() -> TerminalAnnotationSelectionSnapshot? {
        let contentRegistry = TerminalAnnotationContentRegistry.shared
        let generationBefore = contentRegistry.generation(for: paneID)
        guard generationBefore.isMultiple(of: 2) else { return nil }
        guard let selection = view.readSelectionSnapshot(),
              let metrics = view.terminalGridMetrics()
        else { return nil }
        let generationAfter = contentRegistry.generation(for: paneID)
        let surfaceSnapshotWasStable = generationBefore == generationAfter
            && generationAfter.isMultiple(of: 2)
        guard let scale = view.window?.backingScaleFactor else { return nil }
        let gestureIsCurrent = surfaceSnapshotWasStable && view.annotationGestureProvesContainment(
            of: selection,
            viewportRevision: annotationViewportRevision,
            contentGeneration: generationAfter
        )
        return TerminalAnnotationGeometry.snapshot(
            selection: selection,
            scrollbar: annotationScrollbar,
            metrics: metrics,
            bounds: view.bounds,
            backingScaleFactor: scale,
            gridPaddingPoints: Self.gridPaddingPoints,
            viewportRevision: annotationViewportRevision,
            contentGeneration: generationAfter,
            scrollbarIsInitialized: annotationScrollbarIsInitialized,
            gestureWasFullyContained: gestureIsCurrent
        )
    }

    /// Record a Ghostty grid change without TIOCSWINSZ yet. `fitToSize` /
    /// `setFrameSize` apply the new size and schedule the IOSurface present
    /// as one `main.async`. Two further hops put the ioctl after that
    /// present, so a full-screen TUI's SIGWINCH redraw cannot race a stale
    /// backing store (Emacs after a split close). If no present was queued
    /// (occluded / background), the ioctl still runs — same as the old
    /// immediate path. Repeated reports before the flush coalesce to the
    /// last size.
    func enqueuePtyResize(columns: Int32, rows: Int32) {
        guard columns > 0, rows > 0 else { return }
        pendingPtyColumns = columns
        pendingPtyRows = rows
        guard !ptyResizeFlushScheduled else { return }
        ptyResizeFlushScheduled = true
        let generation = ptyResizeFlushGeneration
        DispatchQueue.main.async { [weak self] in
            DispatchQueue.main.async { [weak self] in
                guard let self, self.ptyResizeFlushGeneration == generation else { return }
                self.ptyResizeFlushScheduled = false
                self.flushPendingPtyResize()
            }
        }
    }

    func cancelPendingPtyResize() {
        ptyResizeFlushGeneration += 1
        ptyResizeFlushScheduled = false
        pendingPtyColumns = nil
        pendingPtyRows = nil
    }

    private func flushPendingPtyResize() {
        guard let columns = pendingPtyColumns, let rows = pendingPtyRows else { return }
        if flushedPtyColumns == columns, flushedPtyRows == rows {
            return
        }
        flushedPtyColumns = columns
        flushedPtyRows = rows
        paneID.withCString { nativeTerminalDidResize($0, columns, rows) }
    }

    func terminalDidRequestOpenURL(_ url: String, kind: TerminalOpenURLKind) {
        let rawKind: Int32 = switch kind {
        case .text: 1
        case .html: 2
        case .unknown: 0
        }
        paneID.withCString { paneID in
            url.withCString { nativeTerminalDidOpenURL(paneID, $0, rawKind) }
        }
    }

    func requestSearch() {
        paneID.withCString { nativeTerminalDidRequestSearch($0) }
    }

    func requestPaste() {
        // Read the pasteboard here, inside the user's ⌘V / menu / middle-click
        // event, so macOS attributes the read to a real paste. A deferred read
        // from the webview counts as programmatic access and trips the
        // pasteboard privacy alert on every paste.
        let text = NSPasteboard.general.string(forType: .string) ?? ""
        paneID.withCString { paneID in
            text.withCString { nativeTerminalDidRequestPaste(paneID, $0) }
        }
    }

    func reportUserInput() {
        let now = Date()
        guard now.timeIntervalSince(lastUserInputReport) >= 0.25 else { return }
        lastUserInputReport = now
        paneID.withCString { nativeTerminalDidReceiveUserInput($0) }
    }

    func reportEscape() {
        paneID.withCString { nativeTerminalDidReceiveEscape($0) }
    }

    func reportShortcut(key: String, event: NSEvent) -> Bool {
        let handled = paneID.withCString { paneID in
            key.withCString { key in
                nativeTerminalDidReceiveShortcut(
                    paneID,
                    key,
                    event.modifierFlags.contains(.shift) ? 1 : 0,
                    event.modifierFlags.contains(.control) ? 1 : 0,
                    event.modifierFlags.contains(.option) ? 1 : 0,
                    event.modifierFlags.contains(.command) ? 1 : 0,
                    event.isARepeat ? 1 : 0
                ) == 1
            }
        }
        if handled {
            consumedShortcutKeyCodes.insert(event.keyCode)
        }
        return handled
    }

    func reportCommandModifier(active: Bool) {
        paneID.withCString {
            nativeTerminalDidChangeCommandModifier($0, active ? 1 : 0)
        }
    }

    func reportActivation() {
        paneID.withCString { nativeTerminalDidActivate($0) }
    }

    private var lastPassiveActivationAt: TimeInterval = 0

    /// Wheel traffic can contain dozens of events per gesture. Report enough
    /// to let qmux treat scrolling as attention without flooding the event
    /// bridge for every trackpad tick.
    func reportPassiveActivation() {
        let now = ProcessInfo.processInfo.systemUptime
        guard now - lastPassiveActivationAt >= 0.25 else { return }
        lastPassiveActivationAt = now
        reportActivation()
    }

    func pasteApprovedText(_ text: String) -> Bool {
        view.pasteApprovedText(text)
    }

    func applySettings(_ settings: TerminalPaneSettings) -> Bool {
        guard settings.revision > settingsRevision else { return true }
        settingsRevision = settings.revision
        let style = TerminalCursorStyle(rawValue: settings.cursorStyle) ?? .block
        let scrollbackBytes = max(UInt64(settings.scrollbackRows) * 1024, 1_048_576)
        let configuration = TerminalConfiguration { builder in
            builder.withFontSize(Float(settings.fontSize))
            builder.withFontFamily(settings.fontFamily)
            builder.withCustom(
                "adjust-cell-width",
                "\(settings.letterSpacing / settings.fontSize * 100)%"
            )
            builder.withCustom(
                "adjust-cell-height",
                "\((settings.lineHeight - 1) * 100)%"
            )
            builder.withCursorStyle(style)
            builder.withCursorStyleBlink(settings.cursorBlink)
            builder.withCustom("scrollback-limit", "\(scrollbackBytes)")
            builder.withCustom(
                "scroll-to-bottom",
                settings.scrollOnUserInput ? "keystroke" : "no-keystroke"
            )
            builder.withCustom("mouse-scroll-multiplier", "\(settings.scrollSensitivity)")
            builder.withCustom("copy-on-select", settings.copyOnSelect ? "clipboard" : "false")
            builder.withCustom(
                "selection-clear-on-copy",
                settings.selectionClearOnCopy ? "true" : "false"
            )
        }
        // setTheme and setTerminalConfiguration decline no-op updates with
        // false, but an unchanged Ghostty config is success here; reporting
        // failure for a no-op would surface a spurious settings error in the
        // frontend.
        let theme = QmuxTerminalTheme.theme(named: settings.themeName)
        if theme != controller.theme, !controller.setTheme(theme) {
            return false
        }
        view.layer?.backgroundColor = QmuxTerminalTheme.backgroundColor(
            named: settings.themeName
        )
        if configuration != controller.terminalConfiguration,
           !controller.setTerminalConfiguration(configuration)
        {
            return false
        }
        // Settings such as scrollback-limit only affect newly-created Ghostty
        // surfaces. Hold the controller back until a real settings snapshot has
        // been applied — at creation from the host's cached snapshot when one
        // exists, else from the pane's first mount-time settings update.
        if view.controller == nil {
            view.controller = controller
        }
        return true
    }

}
