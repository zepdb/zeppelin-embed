// swift-tools-version: 5.10

import Foundation
import PackageDescription

let repositoryRoot = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
let environment = ProcessInfo.processInfo.environment
let ffiArchive = environment["ZE_LOCAL_FFI_ARCHIVE"] ?? repositoryRoot
    .appendingPathComponent("target/release/libzeppelin_embed_ffi.a")
    .standardizedFileURL.path
let useLocalFFI = environment["ZE_USE_LOCAL_FFI"] == "1"
let graph = environment["ZE_ENABLE_GRAPH"] == "1"
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
let binaryChecksum = "4016bfdeaecae0617ef3f3e879b80451601b53902194fc384ec6eb67a58705b9" // ze:xcframework-checksum

let graphBinaryChecksum = "b33dae4c31e46043c338faa1ae913132ae9a951bdf052f8f211d680ebd73f9e7" // ze:graph-xcframework-checksum

let cTarget: Target
if useLocalFFI {
    cTarget = .systemLibrary(
        name: "CZeppelinEmbed",
        path: "bindings/swift/Sources/CZeppelinEmbed"
    )
} else if useLocalXCFramework {
    cTarget = .binaryTarget(
        name: "CZeppelinEmbed",
        path: graph
            ? "target/xcframework-graph-cypher/ZeppelinEmbedGraph.xcframework"
            : "target/xcframework/ZeppelinEmbed.xcframework"
    )
} else {
    cTarget = .binaryTarget(
        name: "CZeppelinEmbed",
        url: graph
            ? "https://github.com/zepdb/zeppelin-embed/releases/download/v0.7.0/ZeppelinEmbedGraph.xcframework.zip"
            : "https://github.com/zepdb/zeppelin-embed/releases/download/v0.7.0/ZeppelinEmbed.xcframework.zip",
        checksum: graph ? graphBinaryChecksum : binaryChecksum
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

// Packaged graph headers already expose the complete C module. Only the
// source shim needs its graph declarations enabled through the C compiler.
let graphSwiftSettings: [SwiftSetting]? = graph
    ? [.define("ZE_GRAPH")] + (useLocalFFI
        ? [.unsafeFlags(["-Xcc", "-DZE_GRAPH"])] : [])
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
            swiftSettings: graphSwiftSettings,
            linkerSettings: localLinkerSettings
        ),
        .testTarget(
            name: "ZeppelinEmbedTests",
            dependencies: ["ZeppelinEmbed"],
            path: "bindings/swift/Tests/ZeppelinEmbedTests",
            swiftSettings: graphSwiftSettings
        ),
    ] + (graph ? [
        .executableTarget(name: "GraphWorkload", dependencies: ["ZeppelinEmbed"],
            path: "bindings/swift/Examples", exclude: ["InstalledGraphConsumer", "FiveVectorsSearch", "RecordStore"],
            sources: ["GraphWorkload.swift"]),
    ] : [])
)
