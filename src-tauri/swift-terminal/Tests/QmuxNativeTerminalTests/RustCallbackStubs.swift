import Foundation

final class NativeTerminalCallbackRecorder: @unchecked Sendable {
    static let shared = NativeTerminalCallbackRecorder()

    private let lock = NSLock()
    private var resizeValues: [(columns: Int32, rows: Int32)] = []

    func reset() {
        lock.lock()
        resizeValues.removeAll()
        lock.unlock()
    }

    func recordResize(columns: Int32, rows: Int32) {
        lock.lock()
        resizeValues.append((columns, rows))
        lock.unlock()
    }

    var resizes: [(columns: Int32, rows: Int32)] {
        lock.lock()
        defer { lock.unlock() }
        return resizeValues
    }
}

@_cdecl("qmux_native_terminal_did_close")
func nativeTerminalDidCloseStub(
    _: UnsafePointer<CChar>,
    _: Int32
) {}

@_cdecl("qmux_native_terminal_did_change_title")
func nativeTerminalDidChangeTitleStub(
    _: UnsafePointer<CChar>,
    _: UnsafePointer<CChar>
) {}

@_cdecl("qmux_native_terminal_did_change_cwd")
func nativeTerminalDidChangeCwdStub(
    _: UnsafePointer<CChar>,
    _: UnsafePointer<CChar>
) {}

@_cdecl("qmux_native_terminal_did_resize")
func nativeTerminalDidResizeStub(
    _: UnsafePointer<CChar>,
    _ columns: Int32,
    _ rows: Int32
) {
    NativeTerminalCallbackRecorder.shared.recordResize(columns: columns, rows: rows)
}

@_cdecl("qmux_native_terminal_did_write")
func nativeTerminalDidWriteStub(
    _: UnsafePointer<CChar>,
    _: UnsafePointer<UInt8>,
    _: Int
) {}

@_cdecl("qmux_native_terminal_did_request_search")
func nativeTerminalDidRequestSearchStub(_: UnsafePointer<CChar>) {}

@_cdecl("qmux_native_terminal_did_request_paste")
func nativeTerminalDidRequestPasteStub(
    _: UnsafePointer<CChar>,
    _: UnsafePointer<CChar>
) {}

@_cdecl("qmux_native_terminal_did_receive_user_input")
func nativeTerminalDidReceiveUserInputStub(_: UnsafePointer<CChar>) {}

@_cdecl("qmux_native_terminal_did_receive_escape")
func nativeTerminalDidReceiveEscapeStub(_: UnsafePointer<CChar>) {}

@_cdecl("qmux_native_terminal_did_receive_shortcut")
func nativeTerminalDidReceiveShortcutStub(
    _: UnsafePointer<CChar>,
    _: UnsafePointer<CChar>,
    _: Int32,
    _: Int32,
    _: Int32,
    _: Int32,
    _: Int32
) -> Int32 {
    0
}

@_cdecl("qmux_native_terminal_did_change_command_modifier")
func nativeTerminalDidChangeCommandModifierStub(
    _: UnsafePointer<CChar>,
    _: Int32
) {}

@_cdecl("qmux_native_terminal_did_activate")
func nativeTerminalDidActivateStub(_: UnsafePointer<CChar>) {}

@_cdecl("qmux_native_terminal_did_open_url")
func nativeTerminalDidOpenURLStub(
    _: UnsafePointer<CChar>,
    _: UnsafePointer<CChar>
) {}

@_cdecl("qmux_native_terminal_did_receive_app_shortcut")
func nativeTerminalDidReceiveAppShortcutStub(
    _: UnsafePointer<CChar>,
    _: Int32,
    _: Int32,
    _: Int32,
    _: Int32,
    _: Int32
) -> Int32 {
    0
}

@_cdecl("qmux_native_terminal_did_commit_geometry")
func nativeTerminalDidCommitGeometryStub(_: UnsafePointer<CChar>) {}

@_cdecl("qmux_native_terminal_did_rebuild_surface")
func nativeTerminalDidRebuildSurfaceStub(_: UnsafePointer<CChar>) -> Int32 { 1 }

@_cdecl("qmux_native_terminal_did_request_surface_recovery")
func nativeTerminalDidRequestSurfaceRecoveryStub(_ markAll: Int32) -> Int32 {
    MainActor.assumeIsolated {
        NativeTerminalHost.shared.recoverTerminalSurfaces(markAll: markAll == 1) ? 1 : 0
    }
}

@_cdecl("qmux_native_terminal_did_begin_interface_health_check")
func nativeTerminalDidBeginInterfaceHealthCheckStub() -> UInt64 {
    0
}

@_cdecl("qmux_native_terminal_did_cancel_interface_health_check")
func nativeTerminalDidCancelInterfaceHealthCheckStub() {}

@_cdecl("qmux_native_terminal_did_detect_unhealthy_webview")
func nativeTerminalDidDetectUnhealthyWebViewStub(_: UInt64) {}

@_cdecl("qmux_global_task_launcher_did_trigger")
func globalTaskLauncherDidTriggerStub() {}
