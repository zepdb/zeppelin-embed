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
Vector, text and hybrid search read Store documents. Include an explicit `id`
when creating a searchable document through `GraphNodeImage`.

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
The graph XCFramework is the full macOS 14+ Apple silicon artifact and includes
the model-backed text C ABI; the default universal, Intel and non-macOS artifacts
are text-free. Its dynamic `ZeppelinEmbedGraph.framework` carries
`Resources/mlx.metallib`, so SwiftPM consumers need no shader-copy step. For local
artifact validation, also set `ZE_USE_LOCAL_XCFRAMEWORK=1`; the package selects
`target/xcframework-graph-cypher/ZeppelinEmbedGraph.xcframework`.

### TextStore

`TextStore` is available now with a locally built full XCFramework
(`ZE_USE_LOCAL_XCFRAMEWORK=1 ZE_ENABLE_GRAPH=1`), on macOS 14+ arm64, and from
the next release via the release URL. It is not
available with `ZE_USE_LOCAL_FFI=1`. It adds no binary target or second copy of core.
The dynamic framework includes `Resources/mlx.metallib`; preserve that resource
when distributing your app. The default universal artifact has no text API.

```swift
import ZeppelinEmbed

let store = try await TextStore.open(at: storeURL, bundle: bundleURL)
let report = try await store.ingest([
    TextDocument(id: DocumentID(high: 0, low: 1), revision: 1, text: "hello world")
])
let result = try await store.query("hello", legs: .hybrid, k: 10)
let health = try await store.health()
// Run more slices if the returned status is .budgetExhausted.
let maintenance = try await store.maintain(
    wallTimeNanoseconds: 25_000_000, byteBudget: 64 * 1024 * 1024)
_ = try await store.delete([DocumentID(high: 0, low: 1)])
try await store.close()
```

To download a pinned model, use
`TextStore.open(at:model:cacheDirectory:offline:options:progress:)` with a
`TextModel(repo:revision:manifestSHA256:)`. Omit `model` to use `TextModel.default`:
`aghatage/zeppelin-embed-clean71-arctic-m-v2` at
`d64b817c4021a3f1b9bded3152d9afdf9ac813e7`. The revision is a 40-hex commit and
the manifest pin is a 64-hex SHA-256. The call runs on a detached task, like
`ZeppelinStore`. The optional `@Sendable` progress closure receives
`completedBytes` and `totalBytes` on that task's thread; total is zero while
fetching the manifest. Dispatch UI updates to the main actor. A cache
directory overrides the default cache; `offline: true` prohibits downloading
and an offline miss throws `ZeppelinError.notFound`.

Ingest and query use V2 calls. Ordinary calls bridge Task cancellation to a
native cancel token. A nonzero `deadlineNanoseconds` instead selects the native
relative deadline; the C ABI makes cancellation tokens and deadlines mutually
exclusive. Cancellation and expiry are cooperative; ingest can acknowledge
partial writes before interruption. `tier: nil` leaves tier selection to the
engine. Text caller IDs must fit in 96 bits. `TextIngestOptions` exposes bounded
batching and optional token-window chunking.

`try await store.observedComputeUnits()` explicitly loads CoreML compute-plan
placement counts (`cpu`, `gpu`, `neuralEngine`, `other`), or returns `nil` when
unavailable. These are planned placements, not an execution trace. Query never
loads the compute plan.

Local full-artifact tests and installed-package qualification:

```sh
scripts/xcframework/build.sh --artifact graph-cypher
scripts/macos-sdk/build.sh --artifact graph-cypher
CLANG_MODULE_CACHE_PATH=/tmp/ze509-clang ZE_USE_LOCAL_XCFRAMEWORK=1 ZE_ENABLE_GRAPH=1 swift test --disable-sandbox --jobs 4 --filter TextStoreTests
python3 scripts/release/check-installed-text.py --artifact-root target --output /tmp/ze509-installed-text --disable-swift-sandbox
```

The installed check also needs the full C SDK release artifact under
`target/macos-sdk-graph-cypher`; it exercises the Swift API outside the checkout,
including the existing missing-metallib negative control. The Swift HTTP test
uses a local Python fixture server and needs permission to listen on loopback.
