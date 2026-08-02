import AppKit
import Foundation
import XCTest
@testable import QmuxNativeTerminal

final class NativeTerminalLayoutTests: XCTestCase {
    func testStaleSettingsCannotReplaceNewerTheme() async throws {
        try await MainActor.run {
            let pane = NativeTerminalPane(
                paneID: "native-settings-revision-test-pane",
                workingDirectory: nil,
                themeName: QmuxTerminalTheme.defaultName
            )
            var newer = Self.settings
            newer.revision = 2
            newer.themeName = "Cursor Dark"
            XCTAssertTrue(pane.applySettings(newer))

            var stale = Self.settings
            stale.revision = 1
            XCTAssertTrue(pane.applySettings(stale))

            XCTAssertEqual(
                pane.controller.theme,
                QmuxTerminalTheme.theme(named: "Cursor Dark")
            )
        }
    }

    func testUnchangedTabRevealDoesNotEmitResizeOrMoveViewport() async throws {
        try await MainActor.run {
            try Self.withPane { paneID, frame in
                let session = try XCTUnwrap(
                    TerminalSessionRegistry.shared.session(for: paneID)
                )
                session.receive(
                    (0..<120)
                        .map { String(format: "line-%03d", $0) }
                        .joined(separator: "\r\n")
                )
                XCTAssertTrue(
                    NativeTerminalHost.shared.performAction(
                        id: paneID,
                        action: "scroll_to_top"
                    )
                )
                let viewportBefore = try XCTUnwrap(session.readViewportText())
                XCTAssertTrue(viewportBefore.contains("line-000"))

                NativeTerminalCallbackRecorder.shared.reset()
                XCTAssertTrue(
                    Self.setLayout(paneID: paneID, frame: frame, visible: false)
                )
                XCTAssertTrue(
                    Self.setLayout(paneID: paneID, frame: frame, visible: true)
                )

                XCTAssertTrue(NativeTerminalCallbackRecorder.shared.resizes.isEmpty)
                XCTAssertEqual(session.readViewportText(), viewportBefore)
            }
        }
    }

    func testRealFrameChangeStillEmitsResize() async throws {
        try await MainActor.run {
            try Self.withPane { paneID, frame in
                NativeTerminalCallbackRecorder.shared.reset()
                let widerFrame = CGRect(
                    x: frame.minX,
                    y: frame.minY,
                    width: frame.width + 180,
                    height: frame.height
                )

                XCTAssertTrue(
                    Self.setLayout(paneID: paneID, frame: frame, visible: false)
                )
                XCTAssertTrue(
                    Self.setLayout(
                        paneID: paneID,
                        frame: widerFrame,
                        visible: true
                    )
                )

                let resizes = NativeTerminalCallbackRecorder.shared.resizes
                XCTAssertEqual(resizes.count, 1)
                let resize = try XCTUnwrap(resizes.first)
                XCTAssertGreaterThan(resize.columns, 0)
                XCTAssertGreaterThan(resize.rows, 0)
            }
        }
    }

    func testKeyboardFocusReturnsAfterGeometryDragBlockerClears() async throws {
        try await MainActor.run {
            let paneID = "native-resize-focus-test-pane"
            let frame = CGRect(x: 24, y: 18, width: 720, height: 360)
            NativeTerminalHost.shared.shutdown()
            NativeTerminalCallbackRecorder.shared.reset()
            let root = NSView(frame: CGRect(x: 0, y: 0, width: 1200, height: 800))
            let window = NSWindow(
                contentRect: root.bounds,
                styleMask: [.borderless],
                backing: .buffered,
                defer: false
            )
            window.contentView = root
            defer {
                NativeTerminalHost.shared.shutdown()
                NativeTerminalCallbackRecorder.shared.reset()
                window.close()
                withExtendedLifetime(root) {}
            }

            XCTAssertTrue(NativeTerminalHost.shared.attach(to: root))
            NativeTerminalHost.shared.seedSettings(Self.settings)
            XCTAssertTrue(
                NativeTerminalHost.shared.createPane(
                    id: paneID,
                    workingDirectory: nil
                )
            )
            XCTAssertTrue(Self.setLayout(paneID: paneID, frame: frame, visible: true))
            let terminalView = try XCTUnwrap(Self.terminalView(in: root))

            // A split drag enters the shared input-blocked state, releasing
            // the native owner while WebKit handles the gesture. Clearing the
            // blocker must make the same active pane first responder again.
            XCTAssertTrue(
                NativeTerminalHost.shared.setDesiredKeyboardOwner(
                    id: paneID,
                    revision: 1
                )
            )
            XCTAssertTrue(window.firstResponder === terminalView)
            XCTAssertTrue(
                NativeTerminalHost.shared.setDesiredKeyboardOwner(
                    id: nil,
                    revision: 2
                )
            )
            XCTAssertTrue(
                NativeTerminalHost.shared.setLayout(
                    id: paneID,
                    frame: frame,
                    visible: true,
                    acceptsPointerInput: false,
                    acceptsKeyboardClaim: false,
                    deferGeometry: true
                )
            )
            XCTAssertTrue(Self.setLayout(paneID: paneID, frame: frame, visible: true))
            XCTAssertTrue(
                NativeTerminalHost.shared.setDesiredKeyboardOwner(
                    id: paneID,
                    revision: 3
                )
            )
            XCTAssertTrue(window.firstResponder === terminalView)
        }
    }

    @MainActor
    private static func withPane(
        _ body: (_ paneID: String, _ frame: CGRect) throws -> Void
    ) rethrows {
        let paneID = "native-layout-test-pane"
        let frame = CGRect(x: 24, y: 18, width: 720, height: 360)
        NativeTerminalHost.shared.shutdown()
        NativeTerminalCallbackRecorder.shared.reset()
        let root = NSView(frame: CGRect(x: 0, y: 0, width: 1200, height: 800))
        XCTAssertTrue(NativeTerminalHost.shared.attach(to: root))
        NativeTerminalHost.shared.seedSettings(Self.settings)
        XCTAssertTrue(
            NativeTerminalHost.shared.createPane(
                id: paneID,
                workingDirectory: nil
            )
        )
        XCTAssertTrue(Self.setLayout(paneID: paneID, frame: frame, visible: true))
        XCTAssertTrue(NativeTerminalHost.shared.paneIsReadyForReplay(id: paneID))
        defer {
            NativeTerminalHost.shared.shutdown()
            NativeTerminalCallbackRecorder.shared.reset()
            withExtendedLifetime(root) {}
        }
        try body(paneID, frame)
    }

    @MainActor
    private static func setLayout(
        paneID: String,
        frame: CGRect,
        visible: Bool
    ) -> Bool {
        NativeTerminalHost.shared.setLayout(
            id: paneID,
            frame: frame,
            visible: visible,
            acceptsPointerInput: visible,
            acceptsKeyboardClaim: true,
            deferGeometry: false
        )
    }

    @MainActor
    private static func terminalView(in root: NSView) -> QmuxTerminalView? {
        if let terminal = root as? QmuxTerminalView {
            return terminal
        }
        for child in root.subviews {
            if let terminal = terminalView(in: child) {
                return terminal
            }
        }
        return nil
    }

    private static let settings = TerminalPaneSettings(
        revision: 1,
        fontSize: 13,
        fontFamily: "Menlo",
        letterSpacing: 0,
        lineHeight: 1.2,
        cursorBlink: false,
        cursorStyle: "block",
        scrollbackRows: 10_000,
        scrollOnUserInput: true,
        scrollSensitivity: 1,
        copyOnSelect: false,
        selectionClearOnCopy: false,
        themeName: QmuxTerminalTheme.defaultName
    )
}
