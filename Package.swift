// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "splaude",
    platforms: [.macOS(.v14)],
    targets: [
        .executableTarget(
            name: "splaude",
            path: "Source/splaude",
            swiftSettings: [.swiftLanguageMode(.v5)],
            linkerSettings: [
                .linkedFramework("AppKit"),
                .linkedFramework("AVFoundation"),
                .linkedFramework("Carbon"),
            ]
        )
    ]
)
