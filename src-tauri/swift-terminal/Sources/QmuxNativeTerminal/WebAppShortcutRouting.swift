enum WebAppShortcutResponderState: Int32 {
    case outsideWebView
    case outerWebView
    case webViewDescendant
    case humanBrowser
}

/// Native fallback is reserved for responder states that cannot deliver a key
/// to the DOM. A healthy WebKit descendant must keep the event so focused
/// inputs and component-level shortcut exclusions continue to work. The child
/// human-browser WKWebView is a separate document, however, so recognized qmux
/// shortcuts must be claimed for both its outer view and content descendants.
/// The legacy iframe fallback follows the same rule when explicitly armed.
func shouldClaimWebAppShortcut(
    hasTerminalKeyboardOwner: Bool,
    responderState: WebAppShortcutResponderState,
    iframeFallbackEligible: Bool
) -> Bool {
    guard !hasTerminalKeyboardOwner else { return false }
    if responderState == .humanBrowser {
        return true
    }
    if responderState == .webViewDescendant {
        return iframeFallbackEligible
    }
    return true
}

/// These are the only app shortcuts whose DOM classifier depends on whether
/// the focused target is editable. AppKit exposes only WKContentView here, not
/// the child document's focused DOM node, so leave these chords with every
/// human-browser page instead of risking stealing an editing command.
func humanBrowserDefersEditableSensitiveShortcut(
    key: String,
    shift: Bool,
    control: Bool,
    option: Bool,
    command: Bool
) -> Bool {
    let key = key.lowercased()
    if command && !control && option && !shift {
        return key == "arrowup" || key == "arrowdown"
    }
    return !command && control && !option && !shift && key == "w"
}

/// C-ABI probe used by the Rust suite to exercise the production Swift routing
/// decision without depending on XCTest, which is absent from Command Line
/// Tools-only macOS installations.
@_cdecl("qmux_native_terminal_should_claim_web_app_shortcut")
public func qmuxNativeTerminalShouldClaimWebAppShortcut(
    _ hasTerminalKeyboardOwner: Int32,
    _ responderStateValue: Int32,
    _ iframeFallbackEligible: Int32
) -> Int32 {
    guard let responderState = WebAppShortcutResponderState(
        rawValue: responderStateValue
    ) else {
        return 0
    }
    return shouldClaimWebAppShortcut(
        hasTerminalKeyboardOwner: hasTerminalKeyboardOwner == 1,
        responderState: responderState,
        iframeFallbackEligible: iframeFallbackEligible == 1
    ) ? 1 : 0
}

/// C-ABI probe used by the Rust suite to keep this native-only exception in
/// lockstep with the React classifier's editable-target exclusions.
@_cdecl("qmux_native_terminal_human_browser_defers_editable_sensitive_shortcut")
public func qmuxNativeTerminalHumanBrowserDefersEditableSensitiveShortcut(
    _ key: UnsafePointer<CChar>?,
    _ shift: Int32,
    _ control: Int32,
    _ option: Int32,
    _ command: Int32
) -> Int32 {
    guard let key else { return 0 }
    return humanBrowserDefersEditableSensitiveShortcut(
        key: String(cString: key),
        shift: shift == 1,
        control: control == 1,
        option: option == 1,
        command: command == 1
    ) ? 1 : 0
}
