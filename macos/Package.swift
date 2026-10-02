// swift-tools-version:5.10
import PackageDescription

// The Rust core is built first (`cargo build --release -p lankvm-core`, see scripts/bundle.sh)
// and linked as a static library.
let rustLib = "../target/release"

let package = Package(
    name: "LanKVM",
    platforms: [.macOS(.v14)],
    targets: [
        .target(name: "CLanKVM", path: "Sources/CLanKVM"),
        .executableTarget(
            name: "LanKVM",
            dependencies: ["CLanKVM"],
            path: "Sources/LanKVM",
            linkerSettings: [
                .unsafeFlags(["-L", rustLib]),
                .linkedLibrary("lankvm_core"),
                .linkedLibrary("iconv"),
                .linkedFramework("Security"),
                .linkedFramework("QuartzCore"),
                .linkedFramework("VideoToolbox"),
                .linkedFramework("ScreenCaptureKit"),
                .linkedFramework("UniformTypeIdentifiers"),
                .linkedFramework("AVFoundation"),
                .linkedFramework("CoreMedia"),
                .linkedFramework("CoreVideo"),
                .linkedFramework("CoreAudio"),
                .linkedFramework("CoreGraphics"),
                .linkedFramework("Metal"),
                .linkedFramework("IOSurface"),
            ]
        ),
    ]
)
