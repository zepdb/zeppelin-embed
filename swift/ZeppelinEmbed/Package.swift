// swift-tools-version: 6.2

import PackageDescription
import Foundation

let packageDirectory = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
let ffiArchive = packageDirectory
    .appendingPathComponent("../../target/release/libzeppelin_embed_ffi.a")
    .standardizedFileURL.path
let useBinaryArtifact = ProcessInfo.processInfo.environment["ZE_USE_XCFRAMEWORK"] == "1"
let checksumPath = packageDirectory.appendingPathComponent("binary-checksum.txt")
let binaryChecksum = (try? String(contentsOf: checksumPath, encoding: .utf8))?
    .trimmingCharacters(in: .whitespacesAndNewlines)
    ?? String(repeating: "0", count: 64)

let cTarget: Target = useBinaryArtifact
    ? .binaryTarget(
        name: "CZeppelinEmbed",
        url: "https://github.com/zepdb/zeppelin-embed/releases/download/0.1.0/ZeppelinEmbed.xcframework.zip",
        checksum: binaryChecksum
    )
    : .systemLibrary(
        name: "CZeppelinEmbed",
        path: "Sources/CZeppelinEmbed"
    )

let localLinkerSettings: [LinkerSetting]? = useBinaryArtifact
    ? nil
    : [
        .unsafeFlags([
            "-Xlinker", "-force_load",
            "-Xlinker", ffiArchive,
        ]),
    ]

let package = Package(
    name: "ZeppelinEmbed",
    platforms: [
        .macOS(.v14),
        .iOS(.v17),
    ],
    products: [
        .library(name: "ZeppelinEmbed", targets: ["ZeppelinEmbed"]),
    ],
    targets: [
        cTarget,
        .target(
            name: "ZeppelinEmbed",
            dependencies: ["CZeppelinEmbed"],
            linkerSettings: localLinkerSettings
        ),
        .testTarget(
            name: "ZeppelinEmbedTests",
            dependencies: ["ZeppelinEmbed"]
        ),
    ]
)
