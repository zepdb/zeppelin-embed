// swift-tools-version: 5.10

import Foundation
import PackageDescription

let repositoryRoot = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
let environment = ProcessInfo.processInfo.environment
let ffiArchive = environment["ZE_LOCAL_FFI_ARCHIVE"] ?? repositoryRoot
    .appendingPathComponent("target/release/libzeppelin_embed_ffi.a")
    .standardizedFileURL.path
let useLocalFFI = environment["ZE_USE_LOCAL_FFI"] == "1"
let useLocalXCFramework = environment["ZE_USE_LOCAL_XCFRAMEWORK"] == "1"
let checksumPath = repositoryRoot
    .appendingPathComponent("bindings/swift/binary-checksum.txt")
guard let binaryChecksum = try? String(contentsOf: checksumPath, encoding: .utf8)
    .trimmingCharacters(in: .whitespacesAndNewlines),
    binaryChecksum.count == 64
else {
    fatalError("bindings/swift/binary-checksum.txt must contain a SHA-256 checksum")
}

let cTarget: Target
if useLocalFFI {
    cTarget = .systemLibrary(
        name: "CZeppelinEmbed",
        path: "bindings/swift/Sources/CZeppelinEmbed"
    )
} else if useLocalXCFramework {
    cTarget = .binaryTarget(
        name: "CZeppelinEmbed",
        path: "target/xcframework/ZeppelinEmbed.xcframework"
    )
} else {
    cTarget = .binaryTarget(
        name: "CZeppelinEmbed",
        url: "https://github.com/zepdb/zeppelin-embed/releases/download/v0.2.0/ZeppelinEmbed.xcframework.zip",
        checksum: binaryChecksum
    )
}

let localLinkerSettings: [LinkerSetting]? = useLocalFFI
    ? [
        .unsafeFlags([
            "-Xlinker", "-force_load",
            "-Xlinker", ffiArchive,
        ]),
    ]
    : nil

let package = Package(
    name: "ZeppelinEmbed",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "ZeppelinEmbed", targets: ["ZeppelinEmbed"]),
    ],
    targets: [
        cTarget,
        .target(
            name: "ZeppelinEmbed",
            dependencies: ["CZeppelinEmbed"],
            path: "bindings/swift/Sources/ZeppelinEmbed",
            linkerSettings: localLinkerSettings
        ),
        .testTarget(
            name: "ZeppelinEmbedTests",
            dependencies: ["ZeppelinEmbed"],
            path: "bindings/swift/Tests/ZeppelinEmbedTests"
        ),
    ]
)
