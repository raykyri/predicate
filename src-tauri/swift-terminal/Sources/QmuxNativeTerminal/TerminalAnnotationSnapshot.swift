import AppKit
import Foundation
import GhosttyTerminal

struct TerminalAnnotationScrollbar: Codable, Equatable {
    let totalRows: UInt64
    let offsetRows: UInt64
    let visibleRows: UInt64
}

struct TerminalAnnotationSelectionSnapshot: Codable, Equatable {
    let selectedText: String
    let viewportCellStart: UInt32
    let viewportCellLength: UInt32
    let selectionStartXPoints: Double
    let selectionBaselineYPoints: Double
    let scrollbar: TerminalAnnotationScrollbar
    let scrollbarIsInitialized: Bool
    let columns: UInt16
    let rows: UInt16
    let cellWidthPoints: Double
    let cellHeightPoints: Double
    let gridOriginXPoints: Double
    let gridOriginYPoints: Double
    let backingScaleFactor: Double
    let viewportRevision: UInt64
    let contentGeneration: UInt64
    let viewportFullyContained: Bool
}

struct TerminalAnnotationViewportSnapshot: Codable, Equatable {
    let scrollbar: TerminalAnnotationScrollbar
    let scrollbarIsInitialized: Bool
    let columns: UInt16
    let rows: UInt16
    let cellWidthPoints: Double
    let cellHeightPoints: Double
    let gridOriginXPoints: Double
    let gridOriginYPoints: Double
    let backingScaleFactor: Double
    let viewportRevision: UInt64
    let contentGeneration: UInt64
}

enum TerminalAnnotationGeometry {
    static func viewportSnapshot(
        scrollbar: TerminalAnnotationScrollbar,
        scrollbarIsInitialized: Bool,
        metrics: TerminalGridMetrics,
        bounds: CGRect,
        backingScaleFactor: Double,
        gridPaddingPoints: CGPoint,
        viewportRevision: UInt64,
        contentGeneration: UInt64
    ) -> TerminalAnnotationViewportSnapshot? {
        guard let grid = gridRect(
            bounds: bounds,
            metrics: metrics,
            backingScaleFactor: backingScaleFactor,
            gridPaddingPoints: gridPaddingPoints
        ) else { return nil }
        return TerminalAnnotationViewportSnapshot(
            scrollbar: scrollbar,
            scrollbarIsInitialized: scrollbarIsInitialized,
            columns: metrics.columns,
            rows: metrics.rows,
            cellWidthPoints: Double(metrics.cellWidthPixels) / backingScaleFactor,
            cellHeightPoints: Double(metrics.cellHeightPixels) / backingScaleFactor,
            gridOriginXPoints: grid.minX,
            gridOriginYPoints: grid.minY,
            backingScaleFactor: backingScaleFactor,
            viewportRevision: viewportRevision,
            contentGeneration: contentGeneration
        )
    }

    static func gridRect(
        bounds: CGRect,
        metrics: TerminalGridMetrics,
        backingScaleFactor: Double,
        gridPaddingPoints: CGPoint
    ) -> CGRect? {
        guard backingScaleFactor.isFinite,
              backingScaleFactor > 0,
              metrics.columns > 0,
              metrics.rows > 0,
              metrics.cellWidthPixels > 0,
              metrics.cellHeightPixels > 0
        else { return nil }

        let width = Double(metrics.columns) * Double(metrics.cellWidthPixels)
            / backingScaleFactor
        let height = Double(metrics.rows) * Double(metrics.cellHeightPixels)
            / backingScaleFactor
        guard width.isFinite,
              height.isFinite,
              gridPaddingPoints.x >= 0,
              gridPaddingPoints.y >= 0,
              width <= bounds.width - gridPaddingPoints.x,
              height <= bounds.height - gridPaddingPoints.y
        else { return nil }
        return CGRect(
            x: bounds.minX + gridPaddingPoints.x,
            y: bounds.minY + gridPaddingPoints.y,
            width: width,
            height: height
        )
    }

    static func fullyContainsGesture(
        start: CGPoint,
        end: CGPoint,
        bounds: CGRect,
        metrics: TerminalGridMetrics,
        backingScaleFactor: Double,
        gridPaddingPoints: CGPoint,
        startRevision: UInt64,
        endRevision: UInt64,
        startContentGeneration: UInt64,
        endContentGeneration: UInt64,
        startScrollbarOffset: UInt64,
        endScrollbarOffset: UInt64,
        scrollbarIsInitialized: Bool
    ) -> Bool {
        guard startRevision == endRevision,
              startContentGeneration == endContentGeneration,
              startContentGeneration.isMultiple(of: 2),
              startScrollbarOffset == endScrollbarOffset,
              scrollbarIsInitialized,
              let grid = gridRect(
                bounds: bounds,
                metrics: metrics,
                backingScaleFactor: backingScaleFactor,
                gridPaddingPoints: gridPaddingPoints
              )
        else { return false }
        return grid.contains(start) && grid.contains(end)
    }

    static func snapshot(
        selection: TerminalSelectionSnapshot,
        scrollbar: TerminalAnnotationScrollbar,
        metrics: TerminalGridMetrics,
        bounds: CGRect,
        backingScaleFactor: Double,
        gridPaddingPoints: CGPoint,
        viewportRevision: UInt64,
        contentGeneration: UInt64,
        scrollbarIsInitialized: Bool,
        gestureWasFullyContained: Bool
    ) -> TerminalAnnotationSelectionSnapshot? {
        guard let grid = gridRect(
            bounds: bounds,
            metrics: metrics,
            backingScaleFactor: backingScaleFactor,
            gridPaddingPoints: gridPaddingPoints
        ) else { return nil }

        let selectionEnd = UInt64(selection.viewportOffsetStart)
            + UInt64(selection.viewportOffsetLength)
        let viewportCells = UInt64(metrics.columns) * UInt64(metrics.rows)
        let rangeFitsViewport = !selection.text.isEmpty
            && selection.viewportOffsetLength > 0
            && UInt64(selection.viewportOffsetStart) < viewportCells
            && selectionEnd <= viewportCells
        let scrollbarIsValid = scrollbarIsInitialized
            && scrollbar.visibleRows > 0
            && scrollbar.visibleRows == UInt64(metrics.rows)
            && scrollbar.offsetRows <= scrollbar.totalRows
            && scrollbar.visibleRows <= scrollbar.totalRows - scrollbar.offsetRows
        return TerminalAnnotationSelectionSnapshot(
            selectedText: selection.text,
            viewportCellStart: selection.viewportOffsetStart,
            viewportCellLength: selection.viewportOffsetLength,
            selectionStartXPoints: selection.startPointX,
            selectionBaselineYPoints: selection.baselinePointY,
            scrollbar: scrollbar,
            scrollbarIsInitialized: scrollbarIsInitialized,
            columns: metrics.columns,
            rows: metrics.rows,
            cellWidthPoints: Double(metrics.cellWidthPixels) / backingScaleFactor,
            cellHeightPoints: Double(metrics.cellHeightPixels) / backingScaleFactor,
            gridOriginXPoints: grid.minX,
            gridOriginYPoints: grid.minY,
            backingScaleFactor: backingScaleFactor,
            viewportRevision: viewportRevision,
            contentGeneration: contentGeneration,
            viewportFullyContained: gestureWasFullyContained
                && rangeFitsViewport
                && scrollbarIsValid
                && contentGeneration.isMultiple(of: 2)
        )
    }
}
