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
        ),

        // Tests the executable target directly rather than splitting the app
        // into a library first. SwiftPM has supported that since 5.5, and the
        // alternative — moving files into a new module — would mean marking a
        // large internal surface `public` purely to be able to see it.
        .testTarget(
            name: "splaudeTest",
            dependencies: ["splaude"],
            path: "Test/splaudeTest",
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
    ]
)
