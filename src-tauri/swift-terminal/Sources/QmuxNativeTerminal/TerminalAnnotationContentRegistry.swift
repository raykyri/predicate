import Foundation

/// Exact per-pane content generations are updated on the PTY receive thread;
/// display notifications are globally coalesced before reaching AppKit/Rust.
final class TerminalAnnotationContentRegistry: @unchecked Sendable {
    static let shared = TerminalAnnotationContentRegistry()

    private let lock = NSLock()
    private var generations: [String: UInt64] = [:]
    private var mutationDepths: [String: UInt32] = [:]
    private var monitoredPaneIDs: Set<String> = []
    private var pendingPaneIDs: Set<String> = []
    private var flushScheduled = false

    /// Enter an in-memory Ghostty write. Odd generations mean the surface is
    /// actively mutating; even generations are stable snapshot boundaries.
    func beginContentMutation(for paneID: String) {
        lock.lock()
        if mutationDepths[paneID, default: 0] == 0 {
            generations[paneID, default: 0] &+= 1
        }
        mutationDepths[paneID, default: 0] &+= 1
        lock.unlock()
    }

    func endContentMutation(for paneID: String) {
        var shouldSchedule = false
        lock.lock()
        let depth = mutationDepths[paneID, default: 0]
        guard depth > 0 else {
            lock.unlock()
            return
        }
        mutationDepths[paneID] = depth - 1
        let mutationCompleted = depth == 1
        if mutationCompleted {
            generations[paneID, default: 0] &+= 1
        }
        if mutationCompleted, monitoredPaneIDs.contains(paneID) {
            pendingPaneIDs.insert(paneID)
            if !flushScheduled {
                flushScheduled = true
                shouldSchedule = true
            }
        }
        lock.unlock()

        guard shouldSchedule else { return }
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.0 / 60.0) { [weak self] in
            guard let self else { return }
            let paneIDs: Set<String>
            self.lock.lock()
            paneIDs = self.pendingPaneIDs
            self.pendingPaneIDs.removeAll(keepingCapacity: true)
            self.flushScheduled = false
            self.lock.unlock()
            MainActor.assumeIsolated {
                for paneID in paneIDs {
                    NativeTerminalHost.shared.annotationContentDidChange(id: paneID)
                }
            }
        }
    }

    func generation(for paneID: String) -> UInt64 {
        lock.lock()
        defer { lock.unlock() }
        return generations[paneID, default: 0]
    }

    func setMonitoring(_ enabled: Bool, for paneID: String) {
        lock.lock()
        if enabled {
            monitoredPaneIDs.insert(paneID)
        } else {
            monitoredPaneIDs.remove(paneID)
            pendingPaneIDs.remove(paneID)
        }
        lock.unlock()
    }

    func remove(_ paneID: String) {
        lock.lock()
        generations.removeValue(forKey: paneID)
        mutationDepths.removeValue(forKey: paneID)
        monitoredPaneIDs.remove(paneID)
        pendingPaneIDs.remove(paneID)
        lock.unlock()
    }
}
