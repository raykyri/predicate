@preconcurrency import AppKit
import GhosttyTerminal
import WebKit

@_silgen_name("qmux_native_terminal_did_receive_app_shortcut")
private func nativeTerminalDidReceiveAppShortcut(
    _ key: UnsafePointer<CChar>,
    _ shift: Int32,
    _ control: Int32,
    _ option: Int32,
    _ command: Int32,
    _ repeat: Int32
) -> Int32

@_silgen_name("qmux_native_terminal_did_commit_geometry")
private func nativeTerminalDidCommitGeometry(_ paneID: UnsafePointer<CChar>)

@_silgen_name("qmux_native_terminal_did_resume_after_wake")
private func nativeTerminalDidResumeAfterWake() -> UInt64

@_silgen_name("qmux_native_terminal_will_sleep")
private func nativeTerminalWillSleep()

@_silgen_name("qmux_native_terminal_did_detect_unhealthy_webview")
private func nativeTerminalDidDetectUnhealthyWebView(_ generation: UInt64)

@MainActor
final class NativeTerminalHost {
    static let shared = NativeTerminalHost()

    private(set) var container: NativeTerminalContainerView?
    /// Tauri owns these views; the terminal bridge only needs their identity
    /// to distinguish the privileged app from the external child browser.
    private weak var appWebView: WKWebView?
    private weak var humanBrowserWebView: WKWebView?
    private var panes: [String: NativeTerminalPane] = [:]
    private var backstop: NSView?
    private var eventMonitor: Any?
    private var resignKeyObserver: NSObjectProtocol?
    private var becomeKeyObserver: NSObjectProtocol?
    private var appBecameActiveObserver: NSObjectProtocol?
    private var appResignedActiveObserver: NSObjectProtocol?
    private var windowMiniaturizedObserver: NSObjectProtocol?
    private var workspaceWakeObserver: NSObjectProtocol?
    private var workspaceWillSleepObserver: NSObjectProtocol?
    private var wakeHealthCheckPending = false
    private var wakeHealthCheckInFlight = false
    private var webViewHealthProbeGeneration: UInt64 = 0
    private var webViewHealthRustGeneration: UInt64?
    private var webViewHealthSnapshotAttempt = 0
    /// React's logical keyboard target, ordered independently from geometry.
    /// The pane may not exist or be eligible yet; reconciliation applies it
    /// once creation/visibility catches up without letting a resize choose a
    /// different owner.
    private var desiredKeyboardOwnerPaneID: String?
    private var desiredKeyboardOwnerRevision: UInt64 = 0
    private weak var keyboardOwnerPane: NativeTerminalPane?
    private weak var pointerCapturePane: NativeTerminalPane?
    /// Cancels a deferred release handoff (see setKeyboardOwner) when
    /// ownership changes again before it fires — most often a pane-to-pane
    /// move whose claim lands one bridge call after the release.
    private var webViewHandoffGeneration: UInt64 = 0
    private var consumedAppShortcutKeyCodes: Set<UInt16> = []
    private var webPointerRoutingClaimed = false
    /// When true, the next pointer-up clears `webPointerRoutingClaimed`. Used for
    /// mid-gesture drag claims (button already down when claimed). Sticky claims
    /// such as open sidebar menus start with buttons up and stay until explicit release.
    private var webPointerClaimClearsOnPointerUp = false
    /// True while a pointer gesture that began over web content — an overlay
    /// region, chrome outside any pane rect, or a pane with pointer input
    /// disabled — is still in progress. Its drags and final release must keep
    /// flowing to WKWebView: hit-testing them by position would hand them to
    /// whichever terminal surface the pointer crosses mid-gesture, so Ghostty
    /// would receive a drag it never saw a press for while the DOM selection or
    /// control that owns the gesture never sees its pointer-up (a text
    /// selection freezes, a pressed button sticks in its active state).
    private var webGesturePointerActive = false
    /// DOM rectangles (webview CSS coordinates, matching the flipped container)
    /// that own pointer events even where they overlap a terminal surface —
    /// small controls floating over the terminal, like the right-bar restore
    /// button. Unlike a web pointer claim, the rest of the terminal stays live.
    private var webOverlayRegions: [String: CGRect] = [:]
    /// True while the frontend reports DOM focus inside a cross-document
    /// iframe (the browser overlay's page). Keys typed there are delivered to
    /// the framed document only — the host document's window-level shortcut
    /// handlers never fire — so the key monitor must claim recognized ⌘ app
    /// shortcuts itself or they die inside the frame.
    private var iframeShortcutFallbackActive = false
    private var windowLiveResizeActive = false
    private var clientDeferredGeometryPaneIDs: Set<String> = []
    private var pendingPaneFrames: [String: CGRect] = [:]
    private var pendingFitPaneIDs: Set<String> = []
    /// The theme every pane currently uses. Settings arrive per pane, but the
    /// frontend sends one theme for all of them; new panes and the stage
    /// backstop read this instead of waiting for their first settings update.
    private var currentThemeName = QmuxTerminalTheme.defaultName
    /// The newest revisioned full settings snapshot, seeded by the frontend at
    /// startup and refreshed by every per-pane settings update. A pane created
    /// while a snapshot exists applies it immediately, which assigns its
    /// view's controller and creates the Ghostty surface at creation time —
    /// concurrent with the webview's mount/layout work — instead of after
    /// that pane's own settings round-trip. Panes created before any snapshot
    /// arrives keep the original deferred-controller behavior.
    private var currentSettings: TerminalPaneSettings?

    private init() {}

    /// Resolved live rather than captured at attach time: attach can run before
    /// the supplied view tree is windowed, and a stale/nil reference here would
    /// silently disable first-responder handoff and all key/pointer routing
    /// while leaving rendering intact.
    private var window: NSWindow? {
        container?.window
    }

    func attach(to suppliedView: NSView) -> Bool {
        if container != nil {
            return true
        }

        let webView = findWebView(in: suppliedView)
        appWebView = webView as? WKWebView
        let parent = webView?.superview ?? suppliedView
        let container = NativeTerminalContainerView(
            frame: webView?.frame ?? parent.bounds
        )
        // Always track the parent's size ourselves. A constraint-managed
        // WKWebView reports an empty autoresizingMask, and copying that would
        // freeze the container's bounds on window resize while pointer
        // hit-testing keeps converting window coordinates through them.
        container.autoresizingMask = [.width, .height]
        container.onLiveResizeChange = { [weak self] active in
            self?.setWindowLiveResizeActive(active)
        }
        parent.addSubview(container, positioned: .below, relativeTo: webView)

        self.container = container
        installEventMonitor()
        return true
    }

    /// Keeps an opaque terminal-colored view under the webview's terminal stage
    /// rectangle, below every pane surface. The webview's stage pixels are fully
    /// transparent while panes are shown, and pane surfaces chase their DOM rects
    /// asynchronously (rAF + IPC + layout), so without this any transient gap —
    /// pane spawn, Home→pane switches, split-resize lag — would expose the
    /// window's vibrancy material instead of terminal-colored pixels.
    func setStageBackstop(frame: CGRect) -> Bool {
        guard let container else { return false }
        let view: NSView
        if let backstop {
            view = backstop
        } else {
            let created = NSView(frame: frame)
            created.wantsLayer = true
            created.layer?.backgroundColor = QmuxTerminalTheme.backgroundColor(
                named: currentThemeName
            )
            container.addSubview(created, positioned: .below, relativeTo: nil)
            backstop = created
            view = created
        }
        if view.frame != frame {
            view.frame = frame
        }
        return true
    }

    func createPane(id: String, workingDirectory: String?) -> Bool {
        guard let container else { return false }
        if panes[id] != nil {
            return true
        }

        let pane = NativeTerminalPane(
            paneID: id,
            workingDirectory: workingDirectory,
            themeName: currentThemeName
        )
        pane.view.isHidden = true
        pane.view.setSurfaceVisible(false)
        container.addSubview(pane.view)
        panes[id] = pane
        // Publish the session for the lock-based receive path (PTY output
        // bytes resolve it from Rust reader threads without a main-thread hop).
        TerminalSessionRegistry.shared.register(pane.terminalSession, for: id)
        // Applying the cached snapshot assigns the view's controller, creating
        // the Ghostty surface now rather than after this pane's mount-time
        // settings round-trip. A failure is not a creation failure: the pane's
        // first settings update retries the same assignment.
        if let currentSettings {
            _ = pane.applySettings(currentSettings)
        }
        if desiredKeyboardOwnerPaneID == id {
            _ = reconcileDesiredKeyboardOwner()
        }
        return true
    }

    /// True once the pane's surface is live and has been fitted to a real
    /// frame, i.e. replayed scrollback would render at the width the pane
    /// actually keeps rather than the zero-frame default grid.
    func paneIsReadyForReplay(id: String) -> Bool {
        guard let pane = panes[id] else { return false }
        return pane.hasCommittedGeometry && pane.view.isSurfaceLive
    }

    func removePane(id: String) {
        guard let pane = panes.removeValue(forKey: id) else { return }
        TerminalSessionRegistry.shared.unregister(id)
        if desiredKeyboardOwnerPaneID == id {
            desiredKeyboardOwnerPaneID = nil
        }
        if keyboardOwnerPane === pane {
            // Schedules the deferred webview handoff; the removal below then
            // strands the responder on the window, a state that handoff's
            // guard accepts and routes to the webview one hop later.
            setKeyboardOwner(nil)
        } else {
            // A view can hold AppKit first responder without being the
            // tracked owner (responder churn qmux never granted). Removing
            // it while it holds the responder would strand the responder on
            // the window with no handoff pending, dead-ending keys the DOM
            // should receive.
            releaseFirstResponderIfHeld(by: pane.view)
        }
        if pointerCapturePane === pane {
            pointerCapturePane = nil
        }
        clientDeferredGeometryPaneIDs.remove(id)
        pendingPaneFrames.removeValue(forKey: id)
        pendingFitPaneIDs.remove(id)
        pane.view.removeFromSuperview()
    }

    func surfaceDidClose(id: String) {
        DispatchQueue.main.async { [weak self] in
            self?.removePane(id: id)
        }
    }

    func setLayout(
        id: String,
        frame: CGRect,
        visible: Bool,
        acceptsPointerInput: Bool,
        acceptsKeyboardClaim: Bool,
        deferGeometry: Bool
    ) -> Bool {
        guard let pane = panes[id] else { return false }
        let keyboardClaimChanged = pane.acceptsKeyboardClaim != acceptsKeyboardClaim
        pane.acceptsPointerInput = acceptsPointerInput
        pane.acceptsKeyboardClaim = acceptsKeyboardClaim
        if !acceptsPointerInput, pointerCapturePane === pane {
            pointerCapturePane = nil
        }
        let visibilityChanged = pane.view.isHidden != !visible
        if visibilityChanged {
            if !visible,
               let window,
               let responder = window.firstResponder as? NSView,
               responder === pane.view || responder.isDescendant(of: pane.view)
            {
                // Hiding the view that holds first responder makes AppKit
                // promote the next valid key view on its own — in a split
                // that is the still-visible sibling surface, which then
                // draws a focused cursor and receives keystrokes qmux never
                // granted it. Promotion also skips the release handoff below:
                // its guards would find the responder already moved. Park the
                // responder on the window instead — deliberately not the
                // webview, which would have WebKit re-emit focus on its
                // remembered element (the churn the deferred release handoff
                // exists to avoid). The ownership release or the next claim
                // then routes it from the window.
                window.makeFirstResponder(nil)
            }
            pane.view.isHidden = !visible
            pane.view.setSurfaceVisible(visible)
        }
        // A newly-created surface starts with Ghostty's zero-frame grid and
        // must be fitted on its first reveal even when its NSView already has
        // the requested frame. Once real geometry has been committed, merely
        // hiding and showing that same frame must not fit again: Ghostty treats
        // the fit as a terminal resize, which resets a parked scroll viewport.
        let needsInitialRevealFit =
            visible && visibilityChanged && !pane.hasCommittedGeometry

        if deferGeometry {
            clientDeferredGeometryPaneIDs.insert(id)
        } else {
            clientDeferredGeometryPaneIDs.remove(id)
        }
        let shouldDeferGeometry =
            deferGeometry
            || windowLiveResizeActive
            || container?.inLiveResize == true
        if shouldDeferGeometry {
            pendingPaneFrames[id] = frame
            if needsInitialRevealFit {
                pendingFitPaneIDs.insert(id)
            }
        } else {
            pendingPaneFrames.removeValue(forKey: id)
            let needsDeferredFit = pendingFitPaneIDs.remove(id) != nil
            applyGeometry(
                frame,
                to: pane,
                forceFit: needsInitialRevealFit || needsDeferredFit
            )
        }
        // Layout is never allowed to select or transfer keyboard ownership.
        // It may release an owner that has become invalid, or apply the
        // already-stored desired owner when that exact target becomes newly
        // eligible after creation/visibility/policy catches up.
        if keyboardOwnerPane === pane, !visible || !acceptsKeyboardClaim {
            setKeyboardOwner(nil)
        }
        if desiredKeyboardOwnerPaneID == id,
           visible,
           acceptsKeyboardClaim,
           visibilityChanged || keyboardClaimChanged
        {
            _ = reconcileDesiredKeyboardOwner()
        }
        return true
    }

    /// Applies a complete, revisioned desired owner from React. Stale invokes
    /// are successful no-ops: their state was superseded before reaching the
    /// main actor and must never reclaim the keyboard.
    func setDesiredKeyboardOwner(id: String?, revision: UInt64) -> Bool {
        guard revision > desiredKeyboardOwnerRevision else { return true }
        desiredKeyboardOwnerRevision = revision
        desiredKeyboardOwnerPaneID = id
        return reconcileDesiredKeyboardOwner()
    }

    @discardableResult
    private func reconcileDesiredKeyboardOwner() -> Bool {
        guard let desiredKeyboardOwnerPaneID else {
            return setKeyboardOwner(nil)
        }
        guard let pane = panes[desiredKeyboardOwnerPaneID],
              !pane.view.isHidden,
              pane.acceptsKeyboardClaim
        else {
            // Keep the desired id pending so create/show/unblock can apply it,
            // but no ineligible or unrelated pane may retain the keyboard.
            return setKeyboardOwner(nil)
        }
        return setKeyboardOwner(pane)
    }

    /// Hands first responder to the webview when `view` (or a descendant)
    /// currently holds it despite not going through an ownership release —
    /// pathological responder state ahead of removing the view, where AppKit
    /// would otherwise strand the responder on the window with no release
    /// handoff pending. Ordinary ownership changes route the responder via
    /// setKeyboardOwner and its deferred webview handoff instead.
    private func releaseFirstResponderIfHeld(by view: NSView) {
        guard let window,
              let responder = window.firstResponder as? NSView,
              responder === view || responder.isDescendant(of: view)
        else { return }
        if let webView = appWebView {
            window.makeFirstResponder(webView)
        } else {
            window.makeFirstResponder(nil)
        }
    }

    private func setWindowLiveResizeActive(_ active: Bool) {
        windowLiveResizeActive = active
        if !active {
            flushPendingGeometry()
        }
    }

    private func flushPendingGeometry() {
        let paneIDs = pendingPaneFrames.keys.filter {
            !clientDeferredGeometryPaneIDs.contains($0)
        }
        for paneID in paneIDs {
            guard let frame = pendingPaneFrames.removeValue(forKey: paneID),
                  let pane = panes[paneID]
            else { continue }
            let needsDeferredFit = pendingFitPaneIDs.remove(paneID) != nil
            applyGeometry(frame, to: pane, forceFit: needsDeferredFit)
        }
    }

    /// Assigning a TerminalView frame and fitting it recomputes Ghostty's grid,
    /// which resizes the PTY. Keep that work out of live resize loops.
    private func applyGeometry(
        _ frame: CGRect,
        to pane: NativeTerminalPane,
        forceFit: Bool
    ) {
        let frameChanged = pane.view.frame != frame
        if frameChanged {
            pane.view.frame = frame
        }
        if !pane.view.isHidden, frameChanged || forceFit {
            pane.view.fitToSize()
            // First fit to a real frame: the surface's grid now matches the
            // pane's actual size, so restored scrollback can replay without
            // being scrambled by a later reflow. Tell Rust, which parks
            // replay (pane_attach) until this moment.
            if !pane.hasCommittedGeometry, frame.width > 0, frame.height > 0 {
                pane.hasCommittedGeometry = true
                pane.paneID.withCString { nativeTerminalDidCommitGeometry($0) }
            }
        }
    }

    func focusPane(id: String) -> Bool {
        // Imperative recovery may reassert the logical owner after WebKit
        // first-responder churn, but it cannot choose a new owner.
        guard desiredKeyboardOwnerPaneID == id else { return false }
        return reconcileDesiredKeyboardOwner()
    }

    func setWebPointerRoutingClaimed(_ claimed: Bool) -> Bool {
        guard container != nil else { return false }
        webPointerRoutingClaimed = claimed
        if claimed {
            // A left-down over a web overlay is initially visible to both the
            // native monitor and WKWebView. Relinquish any optimistic Ghostty
            // capture once React identifies that gesture as a web drag.
            pointerCapturePane = nil
            // Drag claims arrive while a button is still down and should end with
            // that gesture. Menu/overlay claims arrive between gestures and must
            // survive subsequent clicks until the frontend releases them.
            webPointerClaimClearsOnPointerUp = NSEvent.pressedMouseButtons != 0
        } else {
            webPointerClaimClearsOnPointerUp = false
        }
        return true
    }

    func setWebOverlayRegion(id: String, frame: CGRect?) -> Bool {
        guard container != nil else { return false }
        if let frame {
            webOverlayRegions[id] = frame
        } else {
            webOverlayRegions.removeValue(forKey: id)
        }
        return true
    }

    func setIframeShortcutFallback(_ active: Bool) -> Bool {
        guard container != nil else { return false }
        iframeShortcutFallbackActive = active
        return true
    }

    /// Registers Tauri's external child WKWebView for responder routing. Rust
    /// owns its lifecycle and geometry; Swift retains only a weak reference.
    func setHumanBrowserWebView(_ webView: WKWebView?, active: Bool) -> Bool {
        guard container != nil else { return false }
        if active {
            guard let webView else { return false }
            humanBrowserWebView = webView
            iframeShortcutFallbackActive = false
            return true
        }

        guard humanBrowserWebView == nil || humanBrowserWebView === webView else {
            return true
        }
        if let humanBrowserWebView,
           let window,
           let responder = window.firstResponder as? NSView,
           responder === humanBrowserWebView
               || responder.isDescendant(of: humanBrowserWebView)
        {
            window.makeFirstResponder(appWebView)
        }
        humanBrowserWebView = nil
        return true
    }

    /// Drops routing state owned by the current DOM document before WKWebView
    /// reloads. Terminal panes and their Ghostty surfaces deliberately survive;
    /// the new document will republish layout, pointer policy, and keyboard
    /// ownership after its event listener is live.
    func prepareForWebViewReload() -> Bool {
        guard container != nil else { return false }
        webViewHealthProbeGeneration &+= 1
        wakeHealthCheckPending = false
        wakeHealthCheckInFlight = false
        webViewHealthRustGeneration = nil
        desiredKeyboardOwnerPaneID = nil
        _ = setKeyboardOwner(nil)
        consumedAppShortcutKeyCodes.removeAll()
        for pane in panes.values {
            pane.consumedShortcutKeyCodes.removeAll()
        }
        pointerCapturePane = nil
        webPointerRoutingClaimed = false
        webPointerClaimClearsOnPointerUp = false
        webGesturePointerActive = false
        webOverlayRegions.removeAll()
        iframeShortcutFallbackActive = false
        humanBrowserWebView = nil
        // Drop document-owned deferral flags; the new document will re-request
        // geometry. Keep in-flight frames/fit IDs so a reload mid-resize does
        // not strand panes at pre-reload sizes until the next set_layout pass.
        clientDeferredGeometryPaneIDs.removeAll()
        windowLiveResizeActive = false
        return true
    }

    func sendText(id: String, text: String) -> Bool {
        guard let pane = panes[id] else { return false }
        // Report a dead/not-yet-created surface as failure: the surface only
        // exists after the frontend's first settings round-trip, and Rust
        // callers record successful writes as delivered turns.
        return pane.view.sendText(text)
    }

    func submit(id: String) -> Bool {
        guard let pane = panes[id], pane.view.isSurfaceLive, let window else { return false }
        let timestamp = ProcessInfo.processInfo.systemUptime
        let makeEvent = { (type: NSEvent.EventType) in
            NSEvent.keyEvent(
                with: type,
                location: .zero,
                modifierFlags: [],
                timestamp: timestamp,
                windowNumber: window.windowNumber,
                context: nil,
                characters: "\r",
                charactersIgnoringModifiers: "\r",
                isARepeat: false,
                keyCode: 0x24
            )
        }
        guard let keyDown = makeEvent(.keyDown),
              let keyUp = makeEvent(.keyUp)
        else { return false }
        pane.view.keyDown(with: keyDown)
        pane.view.keyUp(with: keyUp)
        return true
    }

    func pasteApprovedText(id: String, text: String) -> Bool {
        guard let pane = panes[id] else { return false }
        return pane.pasteApprovedText(text)
    }

    func performAction(id: String, action: String) -> Bool {
        guard let pane = panes[id] else { return false }
        return pane.view.performBindingAction(action)
    }

    /// Caches `settings` without touching any pane, so panes created later can
    /// build their surface immediately. Called by the frontend at startup and
    /// whenever terminal settings change; deliberately independent of pane or
    /// container state so a seed can never fail.
    func seedSettings(_ settings: TerminalPaneSettings) {
        rememberSettings(settings)
    }

    func updateSettings(id: String, settings: TerminalPaneSettings) -> Bool {
        guard let pane = panes[id] else { return false }
        rememberSettings(settings)
        return pane.applySettings(settings)
    }

    private func rememberSettings(_ settings: TerminalPaneSettings) {
        guard currentSettings == nil || settings.revision > currentSettings!.revision else {
            return
        }
        currentSettings = settings
        if settings.themeName != currentThemeName {
            currentThemeName = settings.themeName
            backstop?.layer?.backgroundColor = QmuxTerminalTheme.backgroundColor(
                named: settings.themeName
            )
        }
    }

    func shutdown() {
        if let eventMonitor {
            NSEvent.removeMonitor(eventMonitor)
            self.eventMonitor = nil
        }
        if let resignKeyObserver {
            NotificationCenter.default.removeObserver(resignKeyObserver)
            self.resignKeyObserver = nil
        }
        if let becomeKeyObserver {
            NotificationCenter.default.removeObserver(becomeKeyObserver)
            self.becomeKeyObserver = nil
        }
        if let appBecameActiveObserver {
            NotificationCenter.default.removeObserver(appBecameActiveObserver)
            self.appBecameActiveObserver = nil
        }
        if let appResignedActiveObserver {
            NotificationCenter.default.removeObserver(appResignedActiveObserver)
            self.appResignedActiveObserver = nil
        }
        if let windowMiniaturizedObserver {
            NotificationCenter.default.removeObserver(windowMiniaturizedObserver)
            self.windowMiniaturizedObserver = nil
        }
        if let workspaceWakeObserver {
            NSWorkspace.shared.notificationCenter.removeObserver(workspaceWakeObserver)
            self.workspaceWakeObserver = nil
        }
        if let workspaceWillSleepObserver {
            NSWorkspace.shared.notificationCenter.removeObserver(workspaceWillSleepObserver)
            self.workspaceWillSleepObserver = nil
        }
        wakeHealthCheckPending = false
        wakeHealthCheckInFlight = false
        webViewHealthProbeGeneration &+= 1
        webViewHealthRustGeneration = nil
        setKeyboardOwner(nil)
        desiredKeyboardOwnerPaneID = nil
        consumedAppShortcutKeyCodes.removeAll()
        pointerCapturePane = nil
        webPointerRoutingClaimed = false
        webPointerClaimClearsOnPointerUp = false
        webGesturePointerActive = false
        webOverlayRegions.removeAll()
        iframeShortcutFallbackActive = false
        for pane in panes.values {
            pane.view.removeFromSuperview()
        }
        panes.removeAll()
        TerminalSessionRegistry.shared.unregisterAll()
        clientDeferredGeometryPaneIDs.removeAll()
        pendingPaneFrames.removeAll()
        pendingFitPaneIDs.removeAll()
        windowLiveResizeActive = false
        backstop?.removeFromSuperview()
        backstop = nil
        container?.onLiveResizeChange = nil
        container?.removeFromSuperview()
        container = nil
        humanBrowserWebView = nil
        appWebView = nil
    }

    /// The React layout is the authority for normal ownership changes, while a
    /// pointer-down can claim a pane optimistically until that layout arrives.
    /// Keeping one explicit owner avoids inferring intent from AppKit's current
    /// first responder, which WKWebView may replace during unrelated focus churn.
    @discardableResult
    private func setKeyboardOwner(_ pane: NativeTerminalPane?) -> Bool {
        let previous = keyboardOwnerPane
        if previous !== pane {
            previous?.isFocused = false
            previous?.acceptsKeyboardInput = false
            previous?.consumedShortcutKeyCodes.removeAll()
            keyboardOwnerPane = pane
            if pane == nil {
                previous?.reportCommandModifier(active: false)
            }
        }
        // Every ownership call supersedes a still-pending release handoff: a
        // new owner takes the responder directly below, and a re-release
        // schedules its own.
        webViewHandoffGeneration &+= 1

        guard let pane else {
            let previousView = previous?.view
            if let window,
               let responder = window.firstResponder,
               responder === previousView || responder === window
            {
                // Hand the responder to the webview rather than the bare
                // window: ownership is released exactly when web UI (dialogs,
                // editables, Escape handlers) needs the keyboard, and a
                // window-level first responder dead-ends key events that
                // WebKit would deliver to the DOM. But not synchronously —
                // a release is often followed within a beat by a claim: the
                // desired owner parks ineligible until its surface's first
                // layout lands (a spawning split pane), or a transient web
                // blocker flickers off again. Making the webview first
                // responder for that gap has WebKit re-emit focus on its
                // remembered DOM element, churn the frontend can misread as
                // user intent (composer restore, split-cell activation).
                // Defer one main-queue hop so an immediately following claim
                // cancels the handoff; a genuine release still reaches the
                // webview within the same event-loop turn.
                //
                // The guard accepts a responder already parked on the window
                // as well as one still on the released view: hiding an owning
                // surface parks the responder on the window (see setLayout)
                // rather than let AppKit promote a sibling, and a repeated
                // release — layout releases the owner, then the owner update
                // releases again with no previous pane — cancelled the
                // earlier pending handoff via the generation bump above, so
                // it must schedule its own or the responder dead-ends on the
                // window and the DOM never gets the keyboard back.
                let generation = webViewHandoffGeneration
                DispatchQueue.main.async { [weak self] in
                    guard let self,
                          generation == self.webViewHandoffGeneration,
                          self.keyboardOwnerPane == nil,
                          let window = self.window
                    else { return }
                    let responder = window.firstResponder
                    // The released view may since have left the hierarchy
                    // (pane close), which strands the responder on the window
                    // itself — exactly the dead-end this handoff exists to fix.
                    guard responder === previousView || responder === window else {
                        return
                    }
                    if let webView = self.appWebView {
                        window.makeFirstResponder(webView)
                    } else {
                        window.makeFirstResponder(nil)
                    }
                }
            }
            return true
        }

        pane.isFocused = true
        pane.acceptsKeyboardInput = true
        guard let window else { return false }
        if window.firstResponder === pane.view {
            return true
        }
        return window.makeFirstResponder(pane.view)
    }

    private func findWebView(in root: NSView) -> NSView? {
        if root is WKWebView {
            return root
        }
        for child in root.subviews {
            if let result = findWebView(in: child) {
                return result
            }
        }
        return nil
    }

    /// Consumed-shortcut key codes arm a one-shot swallow of a matching keyUp.
    /// When the window loses key status mid-chord (the global show/hide
    /// shortcut, app deactivation with ⌘ held) those keyUps are delivered to
    /// another app or nowhere, so the armed entries would otherwise wait to
    /// swallow an unrelated future release. The keyDown-side invariant in
    /// routeKeyEvent already heals same-window staleness; this clears the rest
    /// the moment the keyUps stop being ours to see.
    private func windowDidResignKey(_ resigned: NSWindow?) {
        guard let resigned, resigned === window else { return }
        consumedAppShortcutKeyCodes.removeAll()
        for pane in panes.values {
            pane.consumedShortcutKeyCodes.removeAll()
        }
    }

    private func systemDidWake() {
        wakeHealthCheckPending = true
        schedulePostWakeHealthCheckIfVisible()
    }

    private func systemWillSleep() {
        wakeHealthCheckPending = false
        wakeHealthCheckInFlight = false
        webViewHealthProbeGeneration &+= 1
        webViewHealthRustGeneration = nil
        nativeTerminalWillSleep()
    }

    private func deferPostWakeHealthCheckIfNeeded(_ changedWindow: NSWindow? = nil) {
        if let changedWindow, changedWindow !== window {
            return
        }
        guard wakeHealthCheckPending || wakeHealthCheckInFlight ||
                webViewHealthRustGeneration != nil
        else { return }
        wakeHealthCheckPending = true
        wakeHealthCheckInFlight = false
        webViewHealthProbeGeneration &+= 1
        webViewHealthRustGeneration = nil
        nativeTerminalWillSleep()
    }

    private func isEligibleForPostWakeHealthCheck(_ candidate: NSWindow) -> Bool {
        NSApp.isActive &&
            candidate === window &&
            candidate.isVisible &&
            !candidate.isMiniaturized
    }

    private func schedulePostWakeHealthCheckIfVisible() {
        guard wakeHealthCheckPending,
              NSApp.isActive,
              let window,
              window.isVisible,
              !window.isMiniaturized
        else { return }
        wakeHealthCheckPending = false
        wakeHealthCheckInFlight = true
        webViewHealthProbeGeneration &+= 1
        let generation = webViewHealthProbeGeneration
        // Give WindowServer/WebKit a short grace period to reconnect after wake.
        // The actual watchdog below is deliberately much longer, so ordinary
        // resume scheduling or a busy frontend cannot trigger a reload.
        DispatchQueue.main.asyncAfter(deadline: .now() + 1) { [weak self] in
            guard let self,
                  generation == self.webViewHealthProbeGeneration
            else { return }
            guard NSApp.isActive,
                  let window = self.window,
                  window.isVisible,
                  !window.isMiniaturized
            else {
                self.wakeHealthCheckPending = true
                self.wakeHealthCheckInFlight = false
                return
            }
            self.startPostWakeHealthCheck(in: window, generation: generation)
        }
    }

    private func startPostWakeHealthCheck(in window: NSWindow, generation: UInt64) {
        // This event requires an acknowledgement from the current document, so
        // it catches a dead JavaScript event loop even when WebKit did not report
        // a WebContent process termination.
        let rustGeneration = nativeTerminalDidResumeAfterWake()
        webViewHealthRustGeneration = rustGeneration == 0 ? nil : rustGeneration
        webViewHealthSnapshotAttempt = 0
        takePostWakeSnapshot(
            in: window,
            generation: generation,
            rustGeneration: rustGeneration,
            attempt: 0
        )
    }

    private func takePostWakeSnapshot(
        in window: NSWindow,
        generation: UInt64,
        rustGeneration: UInt64,
        attempt: Int
    ) {
        guard generation == webViewHealthProbeGeneration,
              attempt == webViewHealthSnapshotAttempt
        else { return }
        guard isEligibleForPostWakeHealthCheck(window) else {
            deferPostWakeHealthCheckIfNeeded(window)
            return
        }
        // A JavaScript round-trip alone is insufficient for the observed failure:
        // WebKit's GPU process can die while WebContent remains responsive. A tiny
        // native snapshot exercises the compositor without allocating a full-window
        // image under the same memory pressure that caused the original failure.
        guard let webView = appWebView,
              webView.bounds.width > 0,
              webView.bounds.height > 0
        else {
            finishPostWakeSnapshot(
                generation: generation,
                rustGeneration: rustGeneration,
                attempt: attempt,
                healthy: false
            )
            return
        }
        let configuration = WKSnapshotConfiguration()
        configuration.rect = CGRect(
            x: 0,
            y: 0,
            width: min(webView.bounds.width, 64),
            height: min(webView.bounds.height, 64)
        )
        webView.takeSnapshot(with: configuration) { [weak self] image, error in
            DispatchQueue.main.async {
                MainActor.assumeIsolated {
                    self?.finishPostWakeSnapshot(
                        generation: generation,
                        rustGeneration: rustGeneration,
                        attempt: attempt,
                        healthy: image != nil && error == nil
                    )
                }
            }
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 8) { [weak self] in
            guard let self,
                  generation == self.webViewHealthProbeGeneration,
                  attempt == self.webViewHealthSnapshotAttempt
            else { return }
            self.finishPostWakeSnapshot(
                generation: generation,
                rustGeneration: rustGeneration,
                attempt: attempt,
                healthy: false
            )
        }
    }

    private func finishPostWakeSnapshot(
        generation: UInt64,
        rustGeneration: UInt64,
        attempt: Int,
        healthy: Bool
    ) {
        guard generation == webViewHealthProbeGeneration,
              attempt == webViewHealthSnapshotAttempt
        else { return }
        guard let window, isEligibleForPostWakeHealthCheck(window) else {
            deferPostWakeHealthCheckIfNeeded()
            return
        }
        if healthy {
            wakeHealthCheckInFlight = false
            webViewHealthProbeGeneration &+= 1
            // Rust's event-loop watchdog owns the generation until its
            // acknowledgement window closes. Forget it just after that so a
            // much later app hide does not schedule a redundant wake probe.
            DispatchQueue.main.asyncAfter(deadline: .now() + 8.25) { [weak self] in
                guard let self,
                      self.webViewHealthRustGeneration == rustGeneration
                else { return }
                self.webViewHealthRustGeneration = nil
            }
            return
        }
        if attempt == 0 {
            webViewHealthSnapshotAttempt = 1
            // A first failed/missing snapshot can be transient while WebKit's
            // GPU process reconnects. Retry once after a short foreground-only
            // delay before allowing the compositor half to claim a reload.
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.65) { [weak self] in
                guard let self,
                      generation == self.webViewHealthProbeGeneration,
                      self.webViewHealthSnapshotAttempt == 1
                else { return }
                guard let window = self.window,
                      self.isEligibleForPostWakeHealthCheck(window)
                else {
                    self.deferPostWakeHealthCheckIfNeeded()
                    return
                }
                self.takePostWakeSnapshot(
                    in: window,
                    generation: generation,
                    rustGeneration: rustGeneration,
                    attempt: 1
                )
            }
            return
        }
        wakeHealthCheckInFlight = false
        webViewHealthProbeGeneration &+= 1
        if rustGeneration != 0 {
            nativeTerminalDidDetectUnhealthyWebView(rustGeneration)
            DispatchQueue.main.asyncAfter(deadline: .now() + 8.25) { [weak self] in
                guard let self,
                      self.webViewHealthRustGeneration == rustGeneration
                else { return }
                self.webViewHealthRustGeneration = nil
            }
        }
    }

    private func installEventMonitor() {
        guard eventMonitor == nil else { return }
        if resignKeyObserver == nil {
            resignKeyObserver = NotificationCenter.default.addObserver(
                forName: NSWindow.didResignKeyNotification,
                object: nil,
                queue: .main
            ) { notification in
                // Delivered on the main queue; hop into the main actor for the
                // host's state. The window comparison happens inside, since the
                // host resolves its window lazily.
                let resigned = notification.object as? NSWindow
                MainActor.assumeIsolated {
                    NativeTerminalHost.shared.windowDidResignKey(resigned)
                }
            }
        }
        if becomeKeyObserver == nil {
            becomeKeyObserver = NotificationCenter.default.addObserver(
                forName: NSWindow.didBecomeKeyNotification,
                object: nil,
                queue: .main
            ) { _ in
                MainActor.assumeIsolated {
                    NativeTerminalHost.shared.schedulePostWakeHealthCheckIfVisible()
                }
            }
        }
        if appBecameActiveObserver == nil {
            appBecameActiveObserver = NotificationCenter.default.addObserver(
                forName: NSApplication.didBecomeActiveNotification,
                object: nil,
                queue: .main
            ) { _ in
                MainActor.assumeIsolated {
                    NativeTerminalHost.shared.schedulePostWakeHealthCheckIfVisible()
                }
            }
        }
        if appResignedActiveObserver == nil {
            appResignedActiveObserver = NotificationCenter.default.addObserver(
                forName: NSApplication.didResignActiveNotification,
                object: nil,
                queue: .main
            ) { _ in
                MainActor.assumeIsolated {
                    NativeTerminalHost.shared.deferPostWakeHealthCheckIfNeeded()
                }
            }
        }
        if windowMiniaturizedObserver == nil {
            windowMiniaturizedObserver = NotificationCenter.default.addObserver(
                forName: NSWindow.didMiniaturizeNotification,
                object: nil,
                queue: .main
            ) { notification in
                let changedWindow = notification.object as? NSWindow
                MainActor.assumeIsolated {
                    NativeTerminalHost.shared.deferPostWakeHealthCheckIfNeeded(changedWindow)
                }
            }
        }
        if workspaceWakeObserver == nil {
            workspaceWakeObserver = NSWorkspace.shared.notificationCenter.addObserver(
                forName: NSWorkspace.didWakeNotification,
                object: nil,
                queue: .main
            ) { _ in
                MainActor.assumeIsolated {
                    NativeTerminalHost.shared.systemDidWake()
                }
            }
        }
        if workspaceWillSleepObserver == nil {
            workspaceWillSleepObserver = NSWorkspace.shared.notificationCenter.addObserver(
                forName: NSWorkspace.willSleepNotification,
                object: nil,
                queue: .main
            ) { _ in
                MainActor.assumeIsolated {
                    NativeTerminalHost.shared.systemWillSleep()
                }
            }
        }
        let mask: NSEvent.EventTypeMask = [
            .leftMouseDown,
            .leftMouseUp,
            .leftMouseDragged,
            .rightMouseDown,
            .rightMouseUp,
            .rightMouseDragged,
            .otherMouseDown,
            .otherMouseUp,
            .otherMouseDragged,
            .mouseMoved,
            .scrollWheel,
            .keyDown,
            .keyUp,
            .flagsChanged,
        ]
        eventMonitor = NSEvent.addLocalMonitorForEvents(matching: mask) {
            @MainActor [weak self] event in
            guard let self else { return event }
            return routeEvent(event)
        }
    }

    private func routeEvent(_ event: NSEvent) -> NSEvent? {
        switch event.type {
        case .keyDown:
            return routeKeyEvent(event)
        case .keyUp:
            return routeKeyUpEvent(event)
        case .flagsChanged:
            return routeFlagsChangedEvent(event)
        default:
            return routePointerEvent(event)
        }
    }

    private func keyboardPane(for event: NSEvent) -> NativeTerminalPane? {
        guard event.window === window,
              let pane = keyboardOwnerPane,
              panes[pane.paneID] === pane,
              !pane.view.isHidden,
              pane.acceptsKeyboardInput
        else {
            return nil
        }
        if window?.firstResponder !== pane.view {
            // The webview holding first responder is a signal, not a stray: a
            // click or programmatic focus() just landed in web content and
            // React's ownership revocation is still crossing the IPC bridge.
            // Yanking the responder back here would blur the web editable and
            // type this key into the terminal, so let the event flow to
            // WebKit instead. Re-asserting stays for the nil/window responder
            // states, which genuinely strand keyboard input.
            if let responder = window?.firstResponder as? NSView,
               isInWebView(responder)
            {
                return nil
            }
            if window?.makeFirstResponder(pane.view) != true {
                return nil
            }
        }
        return pane
    }

    private func isInWebView(_ view: NSView) -> Bool {
        var current: NSView? = view
        while let candidate = current {
            if candidate is WKWebView {
                return true
            }
            current = candidate.superview
        }
        return false
    }

    private func routeKeyEvent(_ event: NSEvent) -> NSEvent? {
        // A fresh (non-repeat) keyDown proves this key was physically released
        // since any earlier claim, so a lingering consumed-shortcut entry for
        // its key code belongs to a keyUp that never reached this window (the
        // window hid or lost key status mid-chord — e.g. the global show/hide
        // shortcut). Left in place it would swallow THIS press's own release,
        // which a kitty-keyboard-protocol TUI observes as a stuck key. The
        // claim paths below re-insert when they consume this press.
        if !event.isARepeat, event.window === window {
            consumedAppShortcutKeyCodes.remove(event.keyCode)
            for pane in panes.values {
                pane.consumedShortcutKeyCodes.remove(event.keyCode)
            }
        }
        guard let pane = keyboardPane(for: event) else {
            if claimWebAppShortcut(event) {
                return nil
            }
            return event
        }
        let modifiers = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        let shortcutModifiers = modifiers.intersection([.shift, .control, .option, .command])
        let key = event.charactersIgnoringModifiers?.lowercased()
        if shortcutModifiers == .command, key == "f" {
            pane.consumedShortcutKeyCodes.insert(event.keyCode)
            pane.requestSearch()
            return nil
        }
        if shortcutModifiers == .command, key == "v" {
            pane.consumedShortcutKeyCodes.insert(event.keyCode)
            pane.requestPaste()
            return nil
        }
        if claimAppShortcut(event, for: pane) {
            return nil
        }
        if key == "\u{1b}",
           !modifiers.contains(.command),
           !modifiers.contains(.control),
           !modifiers.contains(.option)
        {
            pane.reportEscape()
        }
        if key != nil, !modifiers.contains(.command) {
            pane.reportUserInput()
        }
        return event
    }

    private func routeKeyUpEvent(_ event: NSEvent) -> NSEvent? {
        guard event.window === window else { return event }
        if consumedAppShortcutKeyCodes.remove(event.keyCode) != nil {
            return nil
        }
        if panes.values.contains(where: {
            $0.consumedShortcutKeyCodes.remove(event.keyCode) != nil
        }) {
            return nil
        }
        guard event.modifierFlags.contains(.command),
              let pane = keyboardPane(for: event)
        else {
            return event
        }
        // AppKit does not deliver Command-modified key-up events through the
        // ordinary responder chain. Match Ghostty's upstream AppKit host and
        // forward them exactly once.
        pane.view.keyUp(with: event)
        return nil
    }

    /// When no terminal can accept this event, claim recognized qmux shortcuts
    /// before AppKit dispatches them. Terminal teardown can replace its first
    /// responder while React's ownership update crosses the bridge, leaving
    /// nil, the window, the outer WKWebView, or a hidden terminal as responder.
    /// Checking one exact responder misses those states and strands shortcuts
    /// until the user clicks the webview. Unrecognized keys still continue
    /// through the ordinary responder chain, including text input and native
    /// Command-C/Command-Q behavior.
    private func claimWebAppShortcut(_ event: NSEvent) -> Bool {
        guard event.type == .keyDown,
              let window,
              event.window === window,
              let shortcutKey = appShortcutKey(for: event)
        else {
            return false
        }
        let responderState = webAppShortcutResponderState(in: window)
        if responderState == .humanBrowser,
           humanBrowserDefersEditableSensitiveShortcut(
               key: shortcutKey,
               shift: event.modifierFlags.contains(.shift),
               control: event.modifierFlags.contains(.control),
               option: event.modifierFlags.contains(.option),
               command: event.modifierFlags.contains(.command)
           )
        {
            return false
        }
        guard
              shouldClaimWebAppShortcut(
                  hasTerminalKeyboardOwner: keyboardOwnerPane != nil,
                  responderState: responderState,
                  // Only ⌘ chords are pulled out of a focused iframe; option
                  // and bare-control chords (word navigation, readline-style
                  // editing) stay with the framed page, mirroring how the DOM
                  // classifier defers those to editable targets.
                  iframeFallbackEligible: iframeShortcutFallbackActive
                      && event.modifierFlags.contains(.command)
              )
        else {
            return false
        }
        let handled = shortcutKey.withCString { key in
            nativeTerminalDidReceiveAppShortcut(
                key,
                event.modifierFlags.contains(.shift) ? 1 : 0,
                event.modifierFlags.contains(.control) ? 1 : 0,
                event.modifierFlags.contains(.option) ? 1 : 0,
                event.modifierFlags.contains(.command) ? 1 : 0,
                event.isARepeat ? 1 : 0
            ) == 1
        }
        if handled {
            consumedAppShortcutKeyCodes.insert(event.keyCode)
        }
        return handled
    }

    private func webAppShortcutResponderState(
        in window: NSWindow
    ) -> WebAppShortcutResponderState {
        guard let responder = window.firstResponder as? NSView else {
            return .outsideWebView
        }
        if let humanBrowserWebView,
           responder === humanBrowserWebView
               || responder.isDescendant(of: humanBrowserWebView)
        {
            return .humanBrowser
        }
        if responder is WKWebView {
            return .outerWebView
        }
        return isInWebView(responder) ? .webViewDescendant : .outsideWebView
    }

    private func routeFlagsChangedEvent(_ event: NSEvent) -> NSEvent? {
        guard let pane = keyboardPane(for: event) else {
            return event
        }
        pane.reportCommandModifier(active: event.modifierFlags.contains(.command))
        return event
    }

    /// Whether `pane` may offer a monitor-missed ⌘ chord to the qmux
    /// classifier from performKeyEquivalent despite not being first responder.
    /// It must be the explicit keyboard owner — AppKit walks every view for
    /// key equivalents, and only one pane's offer should count — and the
    /// actual responder must be unable to deliver the chord to the DOM: a
    /// chord typed into a healthy WebKit descendant (a web dialog, the
    /// composer) belongs to the DOM's own handlers and exclusions.
    func shouldOfferKeyEquivalentFallback(for pane: NativeTerminalPane) -> Bool {
        guard keyboardOwnerPane === pane, let window else { return false }
        let state = webAppShortcutResponderState(in: window)
        return state != .webViewDescendant && state != .humanBrowser
    }

    /// Asks Rust to classify the chord into an exact qmux command. Returns true
    /// (and consumes the chord) only when a semantic command was emitted;
    /// everything else stays in the native responder chain for Ghostty/AppKit.
    /// Called from two places that must agree: the local event monitor's
    /// keyDown routing, and QmuxTerminalView.performKeyEquivalent — the
    /// backstop for chords the monitor missed, which Ghostty's key-equivalent
    /// handler would otherwise swallow (see that override for the mechanism).
    func claimAppShortcut(_ event: NSEvent, for pane: NativeTerminalPane) -> Bool {
        let modifiers = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        let key = event.charactersIgnoringModifiers?.lowercased()
        let hasShortcutModifier = modifiers.contains(.command)
            || modifiers.contains(.control)
            || modifiers.contains(.option)
        let isNativeCommand = modifiers.contains(.command) && (key == "c" || key == "q")
        guard hasShortcutModifier, !isNativeCommand,
              let shortcutKey = appShortcutKey(for: event)
        else {
            return false
        }
        return pane.reportShortcut(key: shortcutKey, event: event)
    }

    private func appShortcutKey(for event: NSEvent) -> String? {
        // Match React's physical-code handling for Cmd-backtick. WebKit may
        // expose this key as Dead or a composed character after focus churn,
        // while the ANSI key code remains stable.
        if event.keyCode == 50 {
            return "`"
        }
        guard let key = event.charactersIgnoringModifiers?.lowercased() else {
            return nil
        }
        return webShortcutKey(for: key)
    }

    /// Translates AppKit key characters into the key names shared by the Rust
    /// and React shortcut classifiers. Returns nil for keys with no equivalent,
    /// which then stay in the native responder chain.
    private func webShortcutKey(for key: String) -> String? {
        switch key {
        case "\r": return "Enter"
        case "\t", "\u{19}": return "Tab"
        case "\u{1b}": return "Escape"
        case "\u{7f}": return "Backspace"
        case "\u{F700}": return "ArrowUp"
        case "\u{F701}": return "ArrowDown"
        case "\u{F702}": return "ArrowLeft"
        case "\u{F703}": return "ArrowRight"
        default:
            guard let scalar = key.unicodeScalars.first,
                  key.unicodeScalars.count == 1,
                  scalar.value >= 0x20,
                  // Function keys land in the Unicode private use area.
                  scalar.value < 0xF700
            else {
                return nil
            }
            return key
        }
    }

    private func routePointerEvent(_ event: NSEvent) -> NSEvent? {
        guard event.window === window else { return event }
        // WKWebView owns the complete gesture while a web control claims pointer
        // routing (resize drags, open menus over the terminal, etc.). Returning
        // the event is essential: consuming it here would bypass DOM hit-testing
        // as soon as the cursor crossed a terminal surface.
        if webPointerRoutingClaimed {
            if isPointerUp(event), webPointerClaimClearsOnPointerUp {
                // Safety net for a drag control unmounting before its cleanup
                // invoke: a mouse gesture cannot remain active after button-up.
                // Sticky claims (menus) intentionally survive until released.
                webPointerRoutingClaimed = false
                webPointerClaimClearsOnPointerUp = false
            }
            return event
        }
        if webGesturePointerActive {
            let pressedButtons = NSEvent.pressedMouseButtons
            if isPointerDown(event), pressedButtons == 1 << event.buttonNumber {
                // A fresh press with no other button held starts a new gesture;
                // route it normally instead of leaking the finished gesture's
                // routing into it.
                webGesturePointerActive = false
            } else if pressedButtons == 0, !isPointerUp(event) {
                // No buttons are down and this is not the gesture's own release:
                // the release never reached this window (it landed in another
                // window or was consumed elsewhere). Drop the stale state so
                // hover motion and scrolling return to normal routing.
                webGesturePointerActive = false
            } else {
                if isPointerUp(event), pressedButtons == 0 {
                    webGesturePointerActive = false
                }
                return event
            }
        }
        guard let container else { return event }
        let pane: NativeTerminalPane?
        if isPointerContinuation(event), let pointerCapturePane {
            pane = pointerCapturePane
        } else {
            let point = container.convert(event.locationInWindow, from: nil)
            // Web controls floating over the terminal own their rectangle
            // outside an active terminal drag: returning the event keeps DOM
            // hit-testing (and the button's click) working, while a gesture
            // that started inside the terminal stays captured above.
            if webOverlayRegions.values.contains(where: { $0.contains(point) }) {
                if isPointerDown(event) {
                    webGesturePointerActive = true
                }
                return event
            }
            pane = panes.values.first {
                !$0.view.isHidden
                    && $0.acceptsPointerInput
                    && $0.view.frame.contains(point)
            }
        }
        guard let pane else {
            // The press belongs to web content (sidebar, transcript, chrome, a
            // blocked pane). Mark the gesture so its drags and release keep
            // going to WKWebView even where they cross a terminal surface.
            if isPointerDown(event) {
                webGesturePointerActive = true
            }
            return event
        }

        // Ghostty treats the middle button as an unconditional paste trigger.
        // Route it through qmux's approval flow instead, and consume the whole
        // gesture so Ghostty never sees a dangling press or release.
        if isMiddleMouseEvent(event) {
            if event.type == .otherMouseDown {
                if pane.acceptsKeyboardClaim {
                    setKeyboardOwner(pane)
                }
                pane.reportActivation()
                pane.requestPaste()
            }
            return nil
        }

        if isPointerDown(event) {
            pointerCapturePane = pane
            // Claiming the keyboard optimistically keeps the first keystroke
            // after a click out of WKWebView, but only where React could ever
            // agree: a hard-blocked pane (read-only research) gets pointer
            // events for scrolling/selection while its keyboard stays wherever
            // it was — otherwise this claim would persist with no corrective
            // layout and feed keystrokes into a pane the app promises is
            // read-only.
            if pane.acceptsKeyboardClaim {
                setKeyboardOwner(pane)
            }
        }
        forward(event, to: pane.view)
        if event.type == .scrollWheel {
            pane.reportPassiveActivation()
        }
        if isPointerUp(event) {
            pointerCapturePane = nil
        }
        // Let the webview observe presses so React can update the active pane;
        // continuations and scrolling stay native to avoid duplicate handling.
        // The webview may reclaim first responder while processing the returned
        // press. Re-assert the explicit owner once dispatch completes, or the
        // first keystroke after a terminal click can land in WKWebView.
        if isPointerDown(event) {
            // A right-click must stay native so WKWebView cannot open a second
            // context menu. Tell React which split pane became active through
            // the event bridge instead of returning the press to the webview.
            if event.type == .rightMouseDown {
                pane.reportActivation()
                return nil
            }
            // The returned press also reaches the webview below, where React
            // activates the pane under the *DOM* rect at dispatch time. Mid
            // right-pane transition — or while live-resize defers geometry —
            // the DOM has already re-laid out while the surfaces on screen
            // (and this hit-test) still show the old frames, so the two can
            // disagree. Report the native routing decision too: it reflects
            // the pixels the user actually clicked, and arriving through the
            // event bridge it lands after the DOM's optimistic activation
            // and corrects it whenever they differ.
            pane.reportActivation()
            DispatchQueue.main.async { [weak self, weak pane] in
                guard let self, let pane,
                      self.keyboardOwnerPane === pane,
                      pane.acceptsKeyboardInput,
                      self.panes[pane.paneID] === pane,
                      self.window?.firstResponder !== pane.view
                else { return }
                self.window?.makeFirstResponder(pane.view)
            }
            // WebKit only needs an early press in the sequence to activate the
            // React pane. A third press makes the transparent terminal host look
            // like a selectable web paragraph; the resulting DOM selection trips
            // the web-selection keyboard guard and releases this native owner.
            // Ghostty already received the press above, so consume only WebKit's
            // duplicate while preserving native line selection and TUI mouse input.
            if event.type == .leftMouseDown,
               !shouldForwardTerminalLeftMouseDownToWeb(clickCount: event.clickCount)
            {
                return nil
            }
            return event
        }
        // The terminal container deliberately opts out of hit testing, so
        // WKWebView remains responsible for AppKit cursor selection even while
        // Ghostty renders beneath it. Give passive movement to both views:
        // Ghostty needs it for terminal mouse reporting, while WebKit needs it
        // to clear a resize cursor when the pointer leaves a web scrubber.
        if event.type == .mouseMoved {
            return event
        }
        return nil
    }

    private func forward(_ event: NSEvent, to view: TerminalView) {
        switch event.type {
        case .leftMouseDown: view.mouseDown(with: event)
        case .leftMouseUp: view.mouseUp(with: event)
        case .leftMouseDragged: view.mouseDragged(with: event)
        case .rightMouseDown: view.rightMouseDown(with: event)
        case .rightMouseUp: view.rightMouseUp(with: event)
        case .rightMouseDragged: view.rightMouseDragged(with: event)
        case .otherMouseDown: view.otherMouseDown(with: event)
        case .otherMouseUp: view.otherMouseUp(with: event)
        case .otherMouseDragged: view.otherMouseDragged(with: event)
        case .mouseMoved: view.mouseMoved(with: event)
        case .scrollWheel: view.scrollWheel(with: event)
        default: break
        }
    }

    private func isPointerDown(_ event: NSEvent) -> Bool {
        switch event.type {
        case .leftMouseDown, .rightMouseDown, .otherMouseDown: true
        default: false
        }
    }

    private func isPointerUp(_ event: NSEvent) -> Bool {
        switch event.type {
        case .leftMouseUp, .rightMouseUp, .otherMouseUp: true
        default: false
        }
    }

    private func isPointerContinuation(_ event: NSEvent) -> Bool {
        switch event.type {
        case .leftMouseDragged,
             .leftMouseUp,
             .rightMouseDragged,
             .rightMouseUp,
             .otherMouseDragged,
             .otherMouseUp:
            true
        default:
            false
        }
    }

    private func isMiddleMouseEvent(_ event: NSEvent) -> Bool {
        switch event.type {
        case .otherMouseDown, .otherMouseUp, .otherMouseDragged:
            event.buttonNumber == 2
        default:
            false
        }
    }
}
