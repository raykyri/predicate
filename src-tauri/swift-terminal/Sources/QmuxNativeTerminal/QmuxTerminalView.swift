import AppKit
import GhosttyTerminal

/// qmux-specific native menu actions that must live in AppKit rather than in a
/// transparent React overlay above the Metal-backed terminal surface.
final class QmuxTerminalView: TerminalView {
    var onPasteRequest: (() -> Void)?
    /// Offers a ⌘ chord to qmux's shortcut layer (the same Rust classifier the
    /// NativeTerminalHost key monitor uses). Returns true when qmux consumed it.
    var onAppShortcutKeyEquivalent: ((NSEvent) -> Bool)?
    /// Whether this view may offer a chord to the qmux classifier even though
    /// it is not first responder: the host answers true only for the explicit
    /// keyboard owner while the actual responder cannot deliver keys to the
    /// DOM (see performKeyEquivalent below).
    var shouldOfferAppShortcutFallback: (() -> Bool)?

    override func paste(_: Any?) {
        onPasteRequest?()
    }

    // The NativeTerminalHost key monitor normally claims qmux app shortcuts
    // (⌘-backtick, ⌘T, ...) before AppKit dispatches the event at all. But
    // upstream GhosttyTerminal's performKeyEquivalent is a catch-all: any
    // command chord that reaches dispatch — every monitor missed path
    // (keyboard-owner/first-responder desync, pre-init gaps) — is consumed on
    // its second pass and fed to ghostty core, where a chord with no Ghostty
    // binding (like ⌘-backtick, which has no default) silently dies. Keybind
    // unbinds can't help there: they only remove bindings, not the catch-all.
    // So before Ghostty gets a look, offer the chord to qmux's own shortcut
    // classifier. The first-responder guard mirrors upstream: AppKit walks the
    // whole view hierarchy for key equivalents, and a chord typed into a web
    // dialog must not trigger terminal-scoped shortcuts. The fallback extends
    // the offer to the one desync the monitor cannot recover on its own — this
    // pane is the explicit keyboard owner but re-asserting first responder
    // failed, stranding the responder somewhere that can't take the chord —
    // which previously ended in the catch-all as a consumed no-op.
    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        if event.type == .keyDown,
           let onAppShortcutKeyEquivalent,
           window?.firstResponder === self || shouldOfferAppShortcutFallback?() == true,
           onAppShortcutKeyEquivalent(event)
        {
            return true
        }
        // Native menu chords must keep falling through to AppKit: Ghostty's
        // catch-all would otherwise consume them for a focused terminal.
        // This covers system window management and qmux's WebKit-independent
        // Reload Interface escape hatch.
        if event.type == .keyDown, Self.isNativeMenuChord(event) {
            return false
        }
        return super.performKeyEquivalent(with: event)
    }

    private static func isNativeMenuChord(_ event: NSEvent) -> Bool {
        let mods = event.modifierFlags.intersection([.shift, .control, .option, .command])
        guard mods == .command || mods == [.command, .option],
              let key = event.charactersIgnoringModifiers?.lowercased()
        else {
            return false
        }
        // Hide/minimize are AppKit-owned window commands. Reload Interface is
        // intentionally native too: it must remain available when the WebKit
        // document and qmux's JavaScript shortcut listener are unhealthy.
        return key == "h" || key == "m" || (mods == [.command, .option] && key == "r")
    }

    /// Key codes whose *unmodified* character is already a control byte
    /// (Return, keypad Enter, Tab, Escape). For these, a C0 in
    /// `event.characters` is the key itself, not evidence of ctrl
    /// translation, and chords like ctrl+enter must keep Ghostty's richer
    /// encoding (CSI 27;5;13~) instead of collapsing to a raw byte.
    private static let selfControlKeyCodes: Set<UInt16> = [36, 48, 53, 76]

    // Ctrl chords that macOS translates to a C0 control character
    // (ctrl+j -> \n, ctrl+shift+- -> \x1f, ctrl+i -> \t, ...) are sent to the
    // pty as that raw byte, exactly like Terminal.app/iTerm2. The upstream
    // GhosttyTerminal key handler mishandles these two ways: its
    // "interpreted command" replay drops the event text, so ghostty core
    // refuses to encode ctrl+shift chords at all (dead C-_ undo in emacs),
    // and when the translated text does survive (via insertText collection)
    // ghostty's ctrlSeq table has no entry for the control byte and falls
    // back to a fixterms CSI-u sequence — emacs sees `^[[10;5u` for ctrl+j.
    // Sending the byte macOS already computed sidesteps both paths.
    //
    // Deliberately skipped when composing (IMEs own ctrl chords like
    // Japanese ctrl+j mid-composition) and for option/command chords
    // (alt-as-ESC prefixing and app shortcuts keep the normal path).
    override func keyDown(with event: NSEvent) {
        if let scalar = legacyControlScalar(for: event) {
            // A `text:` binding action writes the parsed byte to the pty
            // verbatim. sendText would instead take the text-input path,
            // which normalizes \n to \r and would turn ctrl+j into ctrl+m.
            performBindingAction(String(format: "text:\\x%02x", scalar))
            return
        }
        super.keyDown(with: event)
    }

    private func legacyControlScalar(for event: NSEvent) -> UInt8? {
        let mods = event.modifierFlags.intersection([.shift, .control, .option, .command])
        guard mods.contains(.control),
              !mods.contains(.command),
              !mods.contains(.option),
              !hasMarkedText(),
              !Self.selfControlKeyCodes.contains(event.keyCode),
              let characters = event.characters,
              characters.unicodeScalars.count == 1,
              let scalar = characters.unicodeScalars.first,
              scalar.value < 0x20
        else {
            return nil
        }
        return UInt8(scalar.value)
    }
}
