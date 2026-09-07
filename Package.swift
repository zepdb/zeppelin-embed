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
// The checksum has to be a literal. When a consumer depends on this package by
// URL, SwiftPM compiles the manifest in a sandbox where `#filePath` is
// `/Package.swift` and the rest of the repository is not reachable, so any
// attempt to read the checksum from a file next to this one fails and takes the
// consumer's whole dependency resolution down with it. The value is CI's, not
// a local build's (BL-167), and the `swift-release` workflow is the gate: it
// fails the release when this literal disagrees with the archive it attaches,
// and prints the value to pin. `scripts/xcframework/build.sh` only reports a
// local mismatch.
let binaryChecksum = "0000000000000000000000000000000000000000000000000000000000000000" // ze:xcframework-checksum

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
        url: "https://github.com/zepdb/zeppelin-embed/releases/download/v0.3.0/ZeppelinEmbed.xcframework.zip",
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
