import AppKit
import Foundation
import XCTest
@testable import QmuxNativeTerminal

final class NativeTerminalLayoutTests: XCTestCase {
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

    private static let settings = TerminalPaneSettings(
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
