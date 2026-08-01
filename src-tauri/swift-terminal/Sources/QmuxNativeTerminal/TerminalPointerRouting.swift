/// The native terminal and transparent WebKit host both observe the first left
/// mouse-down so React can activate the pane that Ghostty received the click in.
/// WebKit must not see the third and later clicks in the same sequence: it treats
/// a triple-click over the transparent terminal host as a web paragraph selection,
/// which makes qmux hand keyboard ownership away from the terminal.
func shouldForwardTerminalLeftMouseDownToWeb(clickCount: Int) -> Bool {
    clickCount < 3
}

/// C-ABI probe used by the Rust suite because Command Line Tools installations
/// can compile the Swift bridge but do not include XCTest.
@_cdecl("qmux_native_terminal_should_forward_left_mouse_down_to_web")
public func qmuxNativeTerminalShouldForwardLeftMouseDownToWeb(
    _ clickCount: Int32
) -> Int32 {
    shouldForwardTerminalLeftMouseDownToWeb(clickCount: Int(clickCount)) ? 1 : 0
}
