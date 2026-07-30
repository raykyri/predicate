// swift-tools-version: 6.0

import PackageDescription
import Foundation

let ghosttyPackagePath = ProcessInfo.processInfo.environment["QMUX_GHOSTTY_PACKAGE_PATH"]
    ?? "../../vendor/libghostty-spm"

let package = Package(
    name: "QmuxNativeTerminal",
    platforms: [.macOS(.v13)],
    products: [
        .library(
            name: "QmuxNativeTerminal",
            type: .static,
            targets: ["QmuxNativeTerminal"]
        ),
    ],
    dependencies: [
        .package(path: ghosttyPackagePath),
    ],
    targets: [
        .target(
            name: "QmuxNativeTerminal",
            dependencies: [
                .product(name: "GhosttyTerminal", package: "libghostty-spm"),
                .product(name: "GhosttyTheme", package: "libghostty-spm"),
            ]
        ),
        .testTarget(
            name: "QmuxNativeTerminalTests",
            dependencies: ["QmuxNativeTerminal"]
        ),
    ]
)
