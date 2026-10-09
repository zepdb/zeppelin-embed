# ZeppelinEmbed for Swift

`ZeppelinStore` is the async Swift actor over the frozen Zeppelin Embed C ABI.
The package targets macOS 14 or newer on Apple silicon and Intel. Release
consumers use the checksum-pinned XCFramework binary target in the root
`Package.swift`:

```swift
.package(url: "https://github.com/zepdb/zeppelin-embed", from: "0.7.0")
```

Local source builds set `ZE_USE_LOCAL_FFI=1` after building the release FFI
archive. Release validation can set `ZE_USE_LOCAL_XCFRAMEWORK=1` to test the
artifact in `target/xcframework` before uploading it.

Store databases in Application Support. `OpenOptions.excludeFromBackup`
defaults to `true`, which applies `NSURLIsExcludedFromBackupKey` to the store
directory. Do not put a store in iCloud Drive, Dropbox, or another synchronized
folder: the engine owns its rename, lock, and durability protocol.

The host owns maintenance scheduling. The package does not register or schedule
background work.

### Prepare lexical queries after open

Use `try await store.warmLexical()` before exact or `lastAsPrefix` queries.
It prepares the lexical assembly and prefix vocabulary, including record-only
namespaces. Repeated calls reuse preparation; mutations invalidate it.
Optional `cancellationToken` and `deadlineNanoseconds` controls are mutually
exclusive. A cancelled call may retain a complete assembly and can be retried.

Graph methods use the same `ZeppelinStore` handle: `enableGraph`, `graphApply`,
`graphQuery`, `cypher`, `getNodes`, `getRelationships`, and `graphResources`.
Call `enableGraph()` explicitly before graph queries. `GraphNodeID` is an alias for `DocumentID`. A document node created with
`GraphNodeImage(id:timestamp:attributes:metadata:)` uses the appended V2 ABI;
create requires a caller ID, and put uses its existing node ID.

For local graph builds, link one full archive:

```sh
CARGO_BUILD_JOBS=4 cargo build -p zeppelin-embed-ffi --release --features graph-bindings-test-support
CLANG_MODULE_CACHE_PATH=/tmp/ze-swift-clang ZE_USE_LOCAL_FFI=1 ZE_ENABLE_GRAPH=1 swift build --disable-sandbox --jobs 4
CLANG_MODULE_CACHE_PATH=/tmp/ze-swift-clang ZE_USE_LOCAL_FFI=1 ZE_ENABLE_GRAPH=1 swift test --disable-sandbox --jobs 4 -Xswiftc -DZE72_TEST_BRIDGE
```

The separate graph package is retired. The default package resolves the
universal graph-free XCFramework on Apple silicon and Intel. Set
`ZE_ENABLE_GRAPH=1` when resolving and building the package to select the
checksum-pinned graph XCFramework and expose graph methods on `ZeppelinStore`.
The graph XCFramework supports macOS 14+ on Apple silicon only. For local
artifact validation, also set `ZE_USE_LOCAL_XCFRAMEWORK=1`; the package selects
`target/xcframework-graph-cypher/ZeppelinEmbedGraph.xcframework`.
