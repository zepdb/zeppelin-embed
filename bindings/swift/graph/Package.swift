// swift-tools-version: 5.10
import Foundation
import PackageDescription

let local = ProcessInfo.processInfo.environment["ZE_USE_LOCAL_GRAPH_XCFRAMEWORK"] == "1"
// Early artifact pin; release CI must match the actual archive before upload.
let binaryChecksum = "3e0b9c2810c1097ee54047593c9a5a33b701b67c9c4f4562957c74ca5f50c0b4" // ze:xcframework-checksum
let binary: Target = local
    ? .binaryTarget(name: "CZeppelinEmbedGraph", path: "../../../target/xcframework-graph-cypher/ZeppelinEmbedGraph.xcframework")
    : .binaryTarget(name: "CZeppelinEmbedGraph", url: "https://github.com/zepdb/zeppelin-embed/releases/download/v0.5.0/ZeppelinEmbedGraph.xcframework.zip", checksum: binaryChecksum)
let package = Package(
    name: "ZeppelinEmbedGraph",
    platforms: [.macOS(.v14)],
    products: [.library(name: "CZeppelinEmbedGraph", targets: ["CZeppelinEmbedGraph"])],
    targets: [binary, .testTarget(name: "GraphArtifactSmokeTests", dependencies: ["CZeppelinEmbedGraph"], path: "Tests")]
)
