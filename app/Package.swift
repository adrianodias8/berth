// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "BerthApp",
    platforms: [.macOS(.v14)],
    dependencies: [
        .package(url: "https://github.com/migueldeicaza/SwiftTerm", exact: "1.20.0"),
    ],
    targets: [
        .executableTarget(
            name: "Berth",
            dependencies: ["SwiftTerm"],
            path: "Sources/Berth",
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
    ]
)
