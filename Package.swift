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
let binaryChecksum = "2a21fc62d3c9c2458bba1fd2b3133d94bc773b3c50111279734f5d244ab7d7be" // ze:xcframework-checksum

// Graph release bytes are independently pinned by CI (BL-167).
let graphBinaryChecksum = "3e0b9c2810c1097ee54047593c9a5a33b701b67c9c4f4562957c74ca5f50c0b4" // ze:graph-xcframework-checksum
let graphCTarget: Target = environment["ZE_USE_LOCAL_GRAPH_XCFRAMEWORK"] == "1"
    ? .binaryTarget(name: "CZeppelinEmbedGraph",
                    path: "target/xcframework-graph-cypher/ZeppelinEmbedGraph.xcframework")
    : .binaryTarget(name: "CZeppelinEmbedGraph",
                    url: "https://github.com/zepdb/zeppelin-embed/releases/download/v0.5.0/ZeppelinEmbedGraph.xcframework.zip",
                    checksum: graphBinaryChecksum)

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
        url: "https://github.com/zepdb/zeppelin-embed/releases/download/v0.5.0/ZeppelinEmbed.xcframework.zip",
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
        .library(name: "ZeppelinEmbedGraph", targets: ["ZeppelinEmbedGraph"]),
    ],
    targets: [
        cTarget,
        graphCTarget,
        .target(name: "ZeppelinEmbedGraph", dependencies: ["CZeppelinEmbedGraph"],
                path: "bindings/swift/graph/Sources/ZeppelinEmbedGraph"),
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
