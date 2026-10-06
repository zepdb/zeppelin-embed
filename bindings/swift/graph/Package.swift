// swift-tools-version: 5.10
import Foundation
import PackageDescription

let local = ProcessInfo.processInfo.environment["ZE_USE_LOCAL_GRAPH_XCFRAMEWORK"] == "1"
// This separate package is the graph shipping contract. CI pins release bytes;
// the legacy root package never resolves this binary.
let binaryChecksum = "3e0b9c2810c1097ee54047593c9a5a33b701b67c9c4f4562957c74ca5f50c0b4"  // ze:xcframework-checksum
let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent(
  "../../.."
).standardizedFileURL.path
let useLocalFFI = ProcessInfo.processInfo.environment["ZE_USE_LOCAL_FFI"] == "1"
let archive =
  ProcessInfo.processInfo.environment["ZE_LOCAL_FFI_ARCHIVE"] ?? root
  + "/target/release/libzeppelin_embed_ffi.a"
let binary: Target =
  useLocalFFI
  ? .systemLibrary(name: "CZeppelinEmbedGraph", path: "Sources/CZeppelinEmbedGraph")
  : local
    ? .binaryTarget(
      name: "CZeppelinEmbedGraph",
      path: "../../../target/xcframework-graph-cypher/ZeppelinEmbedGraph.xcframework")
    : .binaryTarget(
      name: "CZeppelinEmbedGraph",
      url:
        "https://github.com/zepdb/zeppelin-embed/releases/download/v0.5.0/ZeppelinEmbedGraph.xcframework.zip",
      checksum: binaryChecksum)
let package = Package(
  name: "ZeppelinEmbedGraph",
  platforms: [.macOS(.v14)],
  products: [
    .library(name: "CZeppelinEmbedGraph", targets: ["CZeppelinEmbedGraph"]),
    .library(name: "ZeppelinEmbedGraph", targets: ["ZeppelinEmbedGraph"]),
  ],
  targets: [
    binary,
    .target(
      name: "ZeppelinEmbedGraph", dependencies: ["CZeppelinEmbedGraph"],
      path: "Sources/ZeppelinEmbedGraph",
      linkerSettings: useLocalFFI
        ? [.unsafeFlags(["-Xlinker", "-force_load", "-Xlinker", archive])] : nil),
    .executableTarget(name: "GraphWorkload", dependencies: ["ZeppelinEmbedGraph"],
      path: "Examples", exclude: ["InstalledConsumer"], sources: ["GraphWorkload.swift"]),
    .testTarget(name: "GraphStoreTests", dependencies: ["ZeppelinEmbedGraph"], path: "Tests"),
  ]
)
