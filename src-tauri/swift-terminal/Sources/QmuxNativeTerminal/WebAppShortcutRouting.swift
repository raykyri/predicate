enum WebAppShortcutResponderState: Int32 {
    case outsideWebView
    case outerWebView
    case webViewDescendant
}

/// Native fallback is reserved for responder states that cannot deliver a key
/// to the DOM. A healthy WebKit descendant must keep the event so focused
/// inputs and component-level shortcut exclusions continue to work — except
/// while `iframeFallbackEligible` reports that the chord was typed with DOM
/// focus inside a cross-document iframe (the browser overlay's page): keys
/// there are delivered to the framed document only, never to the host
/// document's window-level handlers, so an unclaimed app shortcut would die
/// inside the frame.
func shouldClaimWebAppShortcut(
    hasTerminalKeyboardOwner: Bool,
    responderState: WebAppShortcutResponderState,
    iframeFallbackEligible: Bool
) -> Bool {
    guard !hasTerminalKeyboardOwner else { return false }
    if responderState == .webViewDescendant {
        return iframeFallbackEligible
    }
    return true
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
