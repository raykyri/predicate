import XCTest
@testable import QmuxNativeTerminal

final class TerminalPointerRoutingTests: XCTestCase {
    func testInitialClicksStillReachWebActivation() {
        XCTAssertTrue(shouldForwardTerminalLeftMouseDownToWeb(clickCount: 1))
        XCTAssertTrue(shouldForwardTerminalLeftMouseDownToWeb(clickCount: 2))
    }

    func testTripleAndLaterClicksStayNative() {
        XCTAssertFalse(shouldForwardTerminalLeftMouseDownToWeb(clickCount: 3))
        XCTAssertFalse(shouldForwardTerminalLeftMouseDownToWeb(clickCount: 4))
    }
}
