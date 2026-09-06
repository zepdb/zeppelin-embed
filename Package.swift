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
// consumer's whole dependency resolution down with it. Kept in sync by
// `scripts/xcframework/build.sh` and the `swift-release` workflow, which both
// verify this line against the archive they built.
let binaryChecksum = "24068e6aa00f0edd2037a8b72588607c360d8e3f322194df9b6aeb729f8e8a9a" // ze:xcframework-checksum

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
