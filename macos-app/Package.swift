// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "QuicKVM",
    platforms: [.macOS(.v13)], // SMAppService（開機自啟）需 macOS 13
    targets: [
        .executableTarget(
            name: "QuicKVM",
            path: "Sources/QuicKVM")
    ]
)
