import AppKit
import CoreGraphics
import Foundation
import GhosttyTerminal
import Testing

@testable import QmuxNativeTerminal

struct TerminalAnnotationSnapshotTests {
    private let metrics = TerminalGridMetrics(
        columns: 80,
        rows: 24,
        widthPixels: 1_280,
        heightPixels: 768,
        cellWidthPixels: 16,
        cellHeightPixels: 32
    )

    @Test
    func `modified gestures cannot claim a linear contained selection`() {
        #expect(QmuxTerminalView.annotationGestureCanProveLinearSelection([]))
        for modifier in [
            NSEvent.ModifierFlags.shift,
            .control,
            .option,
            .command,
        ] {
            #expect(!QmuxTerminalView.annotationGestureCanProveLinearSelection(modifier))
        }
    }

    @Test
    func `content registry exposes odd mutations and even stable generations`() {
        let paneID = "annotation-test-\(UUID().uuidString)"
        let registry = TerminalAnnotationContentRegistry.shared
        #expect(registry.generation(for: paneID) == 0)
        registry.beginContentMutation(for: paneID)
        #expect(registry.generation(for: paneID) == 1)
        registry.beginContentMutation(for: paneID)
        registry.endContentMutation(for: paneID)
        #expect(registry.generation(for: paneID) == 1)
        registry.endContentMutation(for: paneID)
        #expect(registry.generation(for: paneID) == 2)
        registry.remove(paneID)
    }

    @Test
    func `grid geometry converts backing pixels with unbalanced top left padding`() {
        let rect = TerminalAnnotationGeometry.gridRect(
            bounds: CGRect(x: 0, y: 0, width: 661, height: 405),
            metrics: metrics,
            backingScaleFactor: 2,
            gridPaddingPoints: CGPoint(x: 10, y: 10)
        )
        #expect(rect == CGRect(x: 10, y: 10, width: 640, height: 384))
    }

    @Test
    func `gesture must remain inside one unchanged viewport`() {
        let bounds = CGRect(x: 0, y: 0, width: 660, height: 404)
        #expect(TerminalAnnotationGeometry.fullyContainsGesture(
            start: CGPoint(x: 20, y: 20),
            end: CGPoint(x: 640, y: 390),
            bounds: bounds,
            metrics: metrics,
            backingScaleFactor: 2,
            gridPaddingPoints: CGPoint(x: 10, y: 10),
            startRevision: 4,
            endRevision: 4,
            startContentGeneration: 8,
            endContentGeneration: 8,
            startScrollbarOffset: 120,
            endScrollbarOffset: 120,
            scrollbarIsInitialized: true
        ))
        #expect(!TerminalAnnotationGeometry.fullyContainsGesture(
            start: CGPoint(x: 20, y: 20),
            end: CGPoint(x: 640, y: 390),
            bounds: bounds,
            metrics: metrics,
            backingScaleFactor: 2,
            gridPaddingPoints: CGPoint(x: 10, y: 10),
            startRevision: 4,
            endRevision: 5,
            startContentGeneration: 8,
            endContentGeneration: 10,
            startScrollbarOffset: 120,
            endScrollbarOffset: 121,
            scrollbarIsInitialized: true
        ))
        #expect(!TerminalAnnotationGeometry.fullyContainsGesture(
            start: CGPoint(x: 5, y: 20),
            end: CGPoint(x: 640, y: 390),
            bounds: bounds,
            metrics: metrics,
            backingScaleFactor: 2,
            gridPaddingPoints: CGPoint(x: 10, y: 10),
            startRevision: 4,
            endRevision: 4,
            startContentGeneration: 8,
            endContentGeneration: 8,
            startScrollbarOffset: 120,
            endScrollbarOffset: 120,
            scrollbarIsInitialized: true
        ))
    }

    @Test
    func `invalid grids and scale fail closed`() {
        let zeroColumns = TerminalGridMetrics(
            columns: 0,
            rows: 24,
            widthPixels: 0,
            heightPixels: 768,
            cellWidthPixels: 16,
            cellHeightPixels: 32
        )
        #expect(TerminalAnnotationGeometry.gridRect(
            bounds: CGRect(x: 0, y: 0, width: 660, height: 404),
            metrics: zeroColumns,
            backingScaleFactor: 2,
            gridPaddingPoints: CGPoint(x: 10, y: 10)
        ) == nil)
        #expect(TerminalAnnotationGeometry.gridRect(
            bounds: CGRect(x: 0, y: 0, width: 660, height: 404),
            metrics: metrics,
            backingScaleFactor: 0,
            gridPaddingPoints: CGPoint(x: 10, y: 10)
        ) == nil)
    }

    @Test
    func `selection range overflow cannot become a paintable anchor`() {
        let selected = TerminalSelectionSnapshot(
            text: "outside",
            viewportOffsetStart: UInt32.max - 1,
            viewportOffsetLength: 10,
            startPointX: 20,
            baselinePointY: 30
        )
        let snapshot = TerminalAnnotationGeometry.snapshot(
            selection: selected,
            scrollbar: TerminalAnnotationScrollbar(
                totalRows: 500,
                offsetRows: 476,
                visibleRows: 24
            ),
            metrics: metrics,
            bounds: CGRect(x: 0, y: 0, width: 660, height: 404),
            backingScaleFactor: 2,
            gridPaddingPoints: CGPoint(x: 10, y: 10),
            viewportRevision: 9,
            contentGeneration: 10,
            scrollbarIsInitialized: true,
            gestureWasFullyContained: true
        )
        #expect(snapshot?.selectedText == "outside")
        #expect(snapshot?.viewportFullyContained == false)
    }

    @Test
    func `empty and uninitialized selections fail closed`() {
        let selected = TerminalSelectionSnapshot(
            text: "",
            viewportOffsetStart: 1_920,
            viewportOffsetLength: 0,
            startPointX: 20,
            baselinePointY: 30
        )
        let snapshot = TerminalAnnotationGeometry.snapshot(
            selection: selected,
            scrollbar: TerminalAnnotationScrollbar(
                totalRows: 0,
                offsetRows: 0,
                visibleRows: 0
            ),
            metrics: metrics,
            bounds: CGRect(x: 0, y: 0, width: 660, height: 404),
            backingScaleFactor: 2,
            gridPaddingPoints: CGPoint(x: 10, y: 10),
            viewportRevision: 0,
            contentGeneration: 1,
            scrollbarIsInitialized: false,
            gestureWasFullyContained: true
        )
        #expect(snapshot?.viewportFullyContained == false)
    }

    @Test
    func `selection snapshot survives a JSON round trip`() throws {
        let selected = TerminalSelectionSnapshot(
            text: "wide 界 and combining e\u{301}",
            viewportOffsetStart: 80,
            viewportOffsetLength: 12,
            startPointX: 10,
            baselinePointY: 42
        )
        let snapshot = try #require(TerminalAnnotationGeometry.snapshot(
            selection: selected,
            scrollbar: TerminalAnnotationScrollbar(
                totalRows: 100,
                offsetRows: 76,
                visibleRows: 24
            ),
            metrics: metrics,
            bounds: CGRect(x: 0, y: 0, width: 661, height: 405),
            backingScaleFactor: 2,
            gridPaddingPoints: CGPoint(x: 10, y: 10),
            viewportRevision: 3,
            contentGeneration: 8,
            scrollbarIsInitialized: true,
            gestureWasFullyContained: true
        ))
        let encoded = try JSONEncoder().encode(snapshot)
        let decoded = try JSONDecoder().decode(
            TerminalAnnotationSelectionSnapshot.self,
            from: encoded
        )
        #expect(decoded == snapshot)
        #expect(decoded.viewportFullyContained)
    }
}
