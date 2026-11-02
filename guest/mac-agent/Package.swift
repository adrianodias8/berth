// swift-tools-version: 6.0
// berth-gui-mac: the guest agent that runs inside berth's macOS VMs and
// answers docs/gui-protocol.md. GuiCore holds the pure logic (wire format, key
// combos, label matching, image comparison) so it can be unit-tested on any
// Mac; the executable adds the sockets and the AX / CGEvent / ScreenCaptureKit
// calls that need a logged-in GUI session.
import PackageDescription

let package = Package(
    name: "BerthGuiMac",
    // SCScreenshotManager needs macOS 14.
    platforms: [.macOS(.v14)],
    targets: [
        .target(
            name: "GuiCore",
            path: "Sources/GuiCore",
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
        .executableTarget(
            name: "berth-gui-mac",
            dependencies: ["GuiCore"],
            path: "Sources/berth-gui-mac",
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
        .testTarget(
            name: "GuiCoreTests",
            dependencies: ["GuiCore"],
            path: "Tests/GuiCoreTests",
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
    ]
)
