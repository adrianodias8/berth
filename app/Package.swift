// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "BerthApp",
    platforms: [.macOS(.v14)],
    dependencies: [
        .package(url: "https://github.com/migueldeicaza/SwiftTerm", exact: "1.20.0"),
    ],
    targets: [
        // Acquire/release loads on the display rings' shared memory.
        .target(name: "BerthAtomics", path: "Sources/BerthAtomics"),
        .executableTarget(
            name: "Berth",
            dependencies: ["SwiftTerm", "BerthAtomics"],
            path: "Sources/Berth",
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
        .testTarget(
            name: "BerthTests",
            dependencies: ["Berth", "BerthAtomics"],
            path: "Tests/BerthTests",
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
    ]
)
