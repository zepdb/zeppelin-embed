# Graph artifact

Select this alternative package's `CZeppelinEmbedGraph` product for the
`graph-cypher` artifact: full legacy core plus native graph and Cypher compiler.
It supports macOS 14+ arm64. Do not link it alongside `ZeppelinEmbed` or the
legacy C archive: both contain the same core symbols. The root Swift package
remains the legacy distribution. ZE-71 owns the higher-level graph facade.

Local smoke: `ZE_USE_LOCAL_GRAPH_XCFRAMEWORK=1 swift test --package-path
bindings/swift/graph --filter GraphArtifactSmokeTests`.

The checksum is an early local artifact pin, not final release qualification.
Release CI requires the pinned checksum to equal the archive being uploaded.
ZE-71/78 still require full consumer and minimum-runtime evidence.
