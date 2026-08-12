import AppKit
import CoreGraphics
import CoreText
import Foundation
import GhosttyTerminal
import WebKit

private func terminalString(_ pointer: UnsafePointer<CChar>?) -> String? {
    guard let pointer else { return nil }
    return String(cString: pointer)
}

private func onTerminalMain<T: Sendable>(
    _ operation: @escaping @MainActor () -> T
) -> T {
    if Thread.isMainThread {
        return MainActor.assumeIsolated {
            operation()
        }
    }
    return DispatchQueue.main.sync {
        MainActor.assumeIsolated {
            operation()
        }
    }
}

@MainActor
private enum CompletionSoundPlayer {
    private static var soundsByName: [String: NSSound] = [:]

    static func play(systemName: String) -> Bool {
        let sound: NSSound
        if let cached = soundsByName[systemName] {
            sound = cached
        } else {
            guard let loaded = NSSound(named: NSSound.Name(systemName)) else {
                return false
            }
            soundsByName[systemName] = loaded
            sound = loaded
        }
        sound.stop()
        return sound.play()
    }
}

@_cdecl("qmux_native_completion_sound_play")
public func qmuxNativeCompletionSoundPlay(
    _ systemName: UnsafePointer<CChar>?
) -> Int32 {
    guard let systemName = terminalString(systemName) else {
        return 0
    }
    return onTerminalMain {
        CompletionSoundPlayer.play(systemName: systemName) ? 1 : 0
    }
}

@_cdecl("qmux_native_application_is_active")
public func qmuxNativeApplicationIsActive() -> Int32 {
    onTerminalMain {
        NSApp.isActive ? 1 : 0
    }
}

@_cdecl("qmux_native_terminal_register_font")
public func qmuxNativeTerminalRegisterFont(
    _ bytes: UnsafePointer<UInt8>?,
    _ length: Int
) -> Int32 {
    guard let bytes, length > 0 else { return 0 }
    let data = Data(bytes: bytes, count: length) as CFData
    guard let provider = CGDataProvider(data: data),
          let font = CGFont(provider)
    else { return 0 }

    var registrationError: Unmanaged<CFError>?
    if CTFontManagerRegisterGraphicsFont(font, &registrationError) {
        return 1
    }

    // A locally installed copy or a second initialization makes registration
    // idempotently successful: the requested family is already available.
    guard let error = registrationError?.takeRetainedValue() else { return 0 }
    return CFErrorGetCode(error) == CTFontManagerError.alreadyRegistered.rawValue ? 1 : 0
}

@_cdecl("qmux_native_terminal_initialize")
public func qmuxNativeTerminalInitialize(
    _ nativeView: UnsafeMutableRawPointer?
) -> Int32 {
    guard let nativeView else { return 0 }
    let nativeViewAddress = UInt(bitPattern: nativeView)
    return onTerminalMain {
        if ProcessInfo.processInfo.environment["QMUX_NATIVE_DEBUG"] != nil {
            TerminalDebugLog.isEnabled = true
        }
        guard let nativeView = UnsafeMutableRawPointer(
            bitPattern: nativeViewAddress
        ) else {
            return 0
        }
        let view = Unmanaged<NSView>.fromOpaque(nativeView).takeUnretainedValue()
        return NativeTerminalHost.shared.attach(to: view) ? 1 : 0
    }
}

@_cdecl("qmux_native_terminal_create_host_managed")
public func qmuxNativeTerminalCreateHostManaged(
    _ paneID: UnsafePointer<CChar>?,
    _ workingDirectory: UnsafePointer<CChar>?
) -> Int32 {
    guard let paneID = terminalString(paneID) else { return 0 }
    let cwd = terminalString(workingDirectory)
    return onTerminalMain {
        NativeTerminalHost.shared.createPane(id: paneID, workingDirectory: cwd) ? 1 : 0
    }
}

@_cdecl("qmux_native_terminal_receive")
public func qmuxNativeTerminalReceive(
    _ paneID: UnsafePointer<CChar>?,
    _ bytes: UnsafePointer<UInt8>?,
    _ length: Int
) -> Int32 {
    guard let paneID = terminalString(paneID),
          length >= 0,
          bytes != nil || length == 0
    else { return 0 }
    let data = bytes.map { Data(bytes: $0, count: length) } ?? Data()
    // The per-chunk output hot path: resolve the session through the
    // thread-safe registry rather than a main-thread hop, so PTY throughput
    // is not serialized behind whatever the main thread is currently doing.
    guard let session = TerminalSessionRegistry.shared.session(for: paneID)
    else { return 0 }
    return session.receive(data) ? 1 : 0
}

@_cdecl("qmux_native_terminal_is_ready_for_replay")
public func qmuxNativeTerminalIsReadyForReplay(
    _ paneID: UnsafePointer<CChar>?
) -> Int32 {
    guard let paneID = terminalString(paneID) else { return 0 }
    return onTerminalMain {
        NativeTerminalHost.shared.paneIsReadyForReplay(id: paneID) ? 1 : 0
    }
}

@_cdecl("qmux_native_terminal_remove")
public func qmuxNativeTerminalRemove(_ paneID: UnsafePointer<CChar>?) {
    guard let paneID = terminalString(paneID) else { return }
    // Cut the receive path first, on the caller's thread: once the session is
    // unregistered, PTY reader threads can no longer resolve it, so no output
    // lands on a surface that is queued for teardown. The view teardown itself
    // (removeFromSuperview + Ghostty surface deinit) is main-actor AppKit work,
    // but the caller is typically a backend kill path (research-pane retirement,
    // kill_all_panes) that must not park on DispatchQueue.main.sync behind a
    // busy runloop — the research auto-end stall. Queue it instead, matching
    // the already-async surfaceDidClose removal path. removePane's missing-key
    // guard keeps a duplicate delivery (kill racing EOF) a no-op.
    TerminalSessionRegistry.shared.unregister(paneID)
    DispatchQueue.main.async {
        MainActor.assumeIsolated {
            NativeTerminalHost.shared.removePane(id: paneID)
        }
    }
}

@_cdecl("qmux_native_terminal_set_stage_backstop")
public func qmuxNativeTerminalSetStageBackstop(
    _ x: Double,
    _ y: Double,
    _ width: Double,
    _ height: Double
) -> Int32 {
    onTerminalMain {
        NativeTerminalHost.shared.setStageBackstop(
            frame: CGRect(x: x, y: y, width: width, height: height)
        ) ? 1 : 0
    }
}

@_cdecl("qmux_native_terminal_set_layout")
public func qmuxNativeTerminalSetLayout(
    _ paneID: UnsafePointer<CChar>?,
    _ x: Double,
    _ y: Double,
    _ width: Double,
    _ height: Double,
    _ visible: Int32,
    _ acceptsPointerInput: Int32,
    _ acceptsKeyboardClaim: Int32,
    _ deferGeometry: Int32,
    _ revision: UInt64
) -> Int32 {
    guard let paneID = terminalString(paneID) else { return 0 }
    return onTerminalMain {
        NativeTerminalHost.shared.setLayout(
            id: paneID,
            frame: CGRect(x: x, y: y, width: width, height: height),
            visible: visible == 1,
            acceptsPointerInput: acceptsPointerInput == 1,
            acceptsKeyboardClaim: acceptsKeyboardClaim == 1,
            deferGeometry: deferGeometry == 1,
            revision: revision
        ) ? 1 : 0
    }
}

@_cdecl("qmux_native_terminal_set_keyboard_owner")
public func qmuxNativeTerminalSetKeyboardOwner(
    _ paneID: UnsafePointer<CChar>?,
    _ revision: UInt64
) -> Int32 {
    let ownerPaneID = terminalString(paneID)
    return onTerminalMain {
        NativeTerminalHost.shared.setDesiredKeyboardOwner(
            id: ownerPaneID,
            revision: revision
        ) ? 1 : 0
    }
}

@_cdecl("qmux_native_terminal_set_web_pointer_claimed")
public func qmuxNativeTerminalSetWebPointerClaimed(_ claimed: Int32) -> Int32 {
    onTerminalMain {
        NativeTerminalHost.shared.setWebPointerRoutingClaimed(claimed == 1) ? 1 : 0
    }
}

@_cdecl("qmux_native_terminal_set_web_overlay_region")
public func qmuxNativeTerminalSetWebOverlayRegion(
    _ regionID: UnsafePointer<CChar>?,
    _ x: Double,
    _ y: Double,
    _ width: Double,
    _ height: Double,
    _ visible: Int32
) -> Int32 {
    guard let regionID = terminalString(regionID) else { return 0 }
    return onTerminalMain {
        NativeTerminalHost.shared.setWebOverlayRegion(
            id: regionID,
            frame: visible == 1
                ? CGRect(x: x, y: y, width: width, height: height)
                : nil
        ) ? 1 : 0
    }
}

@_cdecl("qmux_native_terminal_set_iframe_shortcut_fallback")
public func qmuxNativeTerminalSetIframeShortcutFallback(_ active: Int32) -> Int32 {
    onTerminalMain {
        NativeTerminalHost.shared.setIframeShortcutFallback(active == 1) ? 1 : 0
    }
}

@_cdecl("qmux_native_terminal_set_human_browser_webview")
public func qmuxNativeTerminalSetHumanBrowserWebView(
    _ nativeView: UnsafeMutableRawPointer?,
    _ active: Int32
) -> Int32 {
    let nativeViewAddress = nativeView.map(UInt.init(bitPattern:))
    return onTerminalMain {
        let webView = nativeViewAddress.flatMap {
            UnsafeMutableRawPointer(bitPattern: $0)
        }.map {
            Unmanaged<WKWebView>.fromOpaque($0).takeUnretainedValue()
        }
        return NativeTerminalHost.shared.setHumanBrowserWebView(
            webView,
            active: active == 1
        ) ? 1 : 0
    }
}

@_cdecl("qmux_native_terminal_set_human_browser_loading_background")
public func qmuxNativeTerminalSetHumanBrowserLoadingBackground(
    _ nativeView: UnsafeMutableRawPointer?,
    _ active: Int32
) -> Int32 {
    let nativeViewAddress = nativeView.map(UInt.init(bitPattern:))
    return onTerminalMain {
        let webView = nativeViewAddress.flatMap {
            UnsafeMutableRawPointer(bitPattern: $0)
        }.map {
            Unmanaged<WKWebView>.fromOpaque($0).takeUnretainedValue()
        }
        return NativeTerminalHost.shared.setHumanBrowserLoadingBackground(
            webView,
            active: active == 1
        ) ? 1 : 0
    }
}

@_cdecl("qmux_native_terminal_prepare_for_webview_reload")
public func qmuxNativeTerminalPrepareForWebViewReload() -> Int32 {
    onTerminalMain {
        NativeTerminalHost.shared.prepareForWebViewReload() ? 1 : 0
    }
}

@_cdecl("qmux_native_terminal_focus")
public func qmuxNativeTerminalFocus(_ paneID: UnsafePointer<CChar>?) -> Int32 {
    guard let paneID = terminalString(paneID) else { return 0 }
    return onTerminalMain {
        NativeTerminalHost.shared.focusPane(id: paneID) ? 1 : 0
    }
}

@_cdecl("qmux_native_terminal_send_text")
public func qmuxNativeTerminalSendText(
    _ paneID: UnsafePointer<CChar>?,
    _ text: UnsafePointer<CChar>?
) -> Int32 {
    guard let paneID = terminalString(paneID),
          let text = terminalString(text)
    else {
        return 0
    }
    return onTerminalMain {
        NativeTerminalHost.shared.sendText(id: paneID, text: text) ? 1 : 0
    }
}

@_cdecl("qmux_native_terminal_submit")
public func qmuxNativeTerminalSubmit(_ paneID: UnsafePointer<CChar>?) -> Int32 {
    guard let paneID = terminalString(paneID) else { return 0 }
    return onTerminalMain {
        NativeTerminalHost.shared.submit(id: paneID) ? 1 : 0
    }
}

@_cdecl("qmux_native_terminal_action")
public func qmuxNativeTerminalAction(
    _ paneID: UnsafePointer<CChar>?,
    _ action: UnsafePointer<CChar>?
) -> Int32 {
    guard let paneID = terminalString(paneID),
          let action = terminalString(action)
    else {
        return 0
    }
    return onTerminalMain {
        NativeTerminalHost.shared.performAction(id: paneID, action: action) ? 1 : 0
    }
}

@_cdecl("qmux_native_terminal_paste_approved_text")
public func qmuxNativeTerminalPasteApprovedText(
    _ paneID: UnsafePointer<CChar>?,
    _ text: UnsafePointer<UInt8>?,
    _ textLength: Int
) -> Int32 {
    guard let paneID = terminalString(paneID),
          textLength >= 0,
          text != nil || textLength == 0
    else {
        return 0
    }
    let text = text.map {
        String(decoding: UnsafeBufferPointer(start: $0, count: textLength), as: UTF8.self)
    } ?? ""
    return onTerminalMain {
        NativeTerminalHost.shared.pasteApprovedText(id: paneID, text: text) ? 1 : 0
    }
}

@_cdecl("qmux_native_terminal_update_settings")
public func qmuxNativeTerminalUpdateSettings(
    _ paneID: UnsafePointer<CChar>?,
    _ revision: UInt64,
    _ fontSize: Double,
    _ fontFamily: UnsafePointer<CChar>?,
    _ letterSpacing: Double,
    _ lineHeight: Double,
    _ cursorBlink: Int32,
    _ cursorStyle: UnsafePointer<CChar>?,
    _ scrollbackRows: UInt32,
    _ scrollOnUserInput: Int32,
    _ scrollSensitivity: Double,
    _ copyOnSelect: Int32,
    _ selectionClearOnCopy: Int32,
    _ themeName: UnsafePointer<CChar>?
) -> Int32 {
    guard let paneID = terminalString(paneID),
          let fontFamily = terminalString(fontFamily),
          let cursorStyle = terminalString(cursorStyle),
          let themeName = terminalString(themeName)
    else {
        return 0
    }
    let settings = TerminalPaneSettings(
        revision: revision,
        fontSize: fontSize,
        fontFamily: fontFamily,
        letterSpacing: letterSpacing,
        lineHeight: lineHeight,
        cursorBlink: cursorBlink == 1,
        cursorStyle: cursorStyle,
        scrollbackRows: scrollbackRows,
        scrollOnUserInput: scrollOnUserInput == 1,
        scrollSensitivity: scrollSensitivity,
        copyOnSelect: copyOnSelect == 1,
        selectionClearOnCopy: selectionClearOnCopy == 1,
        themeName: themeName
    )
    return onTerminalMain {
        NativeTerminalHost.shared.updateSettings(id: paneID, settings: settings) ? 1 : 0
    }
}

@_cdecl("qmux_native_terminal_seed_settings")
public func qmuxNativeTerminalSeedSettings(
    _ revision: UInt64,
    _ fontSize: Double,
    _ fontFamily: UnsafePointer<CChar>?,
    _ letterSpacing: Double,
    _ lineHeight: Double,
    _ cursorBlink: Int32,
    _ cursorStyle: UnsafePointer<CChar>?,
    _ scrollbackRows: UInt32,
    _ scrollOnUserInput: Int32,
    _ scrollSensitivity: Double,
    _ copyOnSelect: Int32,
    _ selectionClearOnCopy: Int32,
    _ themeName: UnsafePointer<CChar>?
) -> Int32 {
    guard let fontFamily = terminalString(fontFamily),
          let cursorStyle = terminalString(cursorStyle),
          let themeName = terminalString(themeName)
    else {
        return 0
    }
    let settings = TerminalPaneSettings(
        revision: revision,
        fontSize: fontSize,
        fontFamily: fontFamily,
        letterSpacing: letterSpacing,
        lineHeight: lineHeight,
        cursorBlink: cursorBlink == 1,
        cursorStyle: cursorStyle,
        scrollbackRows: scrollbackRows,
        scrollOnUserInput: scrollOnUserInput == 1,
        scrollSensitivity: scrollSensitivity,
        copyOnSelect: copyOnSelect == 1,
        selectionClearOnCopy: selectionClearOnCopy == 1,
        themeName: themeName
    )
    onTerminalMain {
        NativeTerminalHost.shared.seedSettings(settings)
    }
    return 1
}

/// One process-lifetime allocation: the catalog is static data, and Rust
/// borrows the pointer without ever freeing it.
private nonisolated(unsafe) let themeCatalogCString: UnsafePointer<CChar>? =
    QmuxTerminalTheme.catalogJSON.withCString { strdup($0) }.map { UnsafePointer($0) }

@_cdecl("qmux_native_terminal_theme_catalog")
public func qmuxNativeTerminalThemeCatalog() -> UnsafePointer<CChar>? {
    themeCatalogCString
}

/// Visible viewport as plain UTF-8 (no scrollback). Caller must free with
/// `qmux_native_terminal_free_string`. Returns null when the pane or surface
/// is missing. Uses the off-main session registry so PiP polling does not
/// serialize behind AppKit layout.
@_cdecl("qmux_native_terminal_read_viewport_text")
public func qmuxNativeTerminalReadViewportText(
    _ paneID: UnsafePointer<CChar>?
) -> UnsafeMutablePointer<CChar>? {
    guard let paneID = terminalString(paneID),
          let session = TerminalSessionRegistry.shared.session(for: paneID),
          let text = session.readViewportText()
    else {
        return nil
    }
    return text.withCString { strdup($0) }
}

@_cdecl("qmux_native_terminal_free_string")
public func qmuxNativeTerminalFreeString(_ pointer: UnsafeMutablePointer<CChar>?) {
    free(pointer)
}

@_cdecl("qmux_native_terminal_shutdown")
public func qmuxNativeTerminalShutdown() {
    onTerminalMain {
        NativeTerminalHost.shared.shutdown()
    }
}
