enum WebAppShortcutResponderState: Int32 {
    case outsideWebView
    case outerWebView
    case webViewDescendant
}

/// Native fallback is reserved for responder states that cannot deliver a key
/// to the DOM. A healthy WebKit descendant must keep the event so focused
/// inputs and component-level shortcut exclusions continue to work.
func shouldClaimWebAppShortcut(
    hasTerminalKeyboardOwner: Bool,
    responderState: WebAppShortcutResponderState
) -> Bool {
    guard !hasTerminalKeyboardOwner else { return false }
    if responderState == .webViewDescendant {
        return false
    }
    return true
}

/// C-ABI probe used by the Rust suite to exercise the production Swift routing
/// decision without depending on XCTest, which is absent from Command Line
/// Tools-only macOS installations.
@_cdecl("qmux_native_terminal_should_claim_web_app_shortcut")
public func qmuxNativeTerminalShouldClaimWebAppShortcut(
    _ hasTerminalKeyboardOwner: Int32,
    _ responderStateValue: Int32
) -> Int32 {
    guard let responderState = WebAppShortcutResponderState(
        rawValue: responderStateValue
    ) else {
        return 0
    }
    return shouldClaimWebAppShortcut(
        hasTerminalKeyboardOwner: hasTerminalKeyboardOwner == 1,
        responderState: responderState
    ) ? 1 : 0
}
