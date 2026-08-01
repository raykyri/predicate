/// A full snapshot of the frontend's terminal settings, as delivered over the
/// bridge. The host caches the most recent snapshot so a pane created later
/// can build its Ghostty surface at creation time instead of waiting for its
/// own mount-time settings round-trip from the webview.
struct TerminalPaneSettings {
    var revision: UInt64
    var fontSize: Double
    var fontFamily: String
    var letterSpacing: Double
    var lineHeight: Double
    var cursorBlink: Bool
    var cursorStyle: String
    var scrollbackRows: UInt32
    var scrollOnUserInput: Bool
    var scrollSensitivity: Double
    var copyOnSelect: Bool
    var selectionClearOnCopy: Bool
    var themeName: String
}
