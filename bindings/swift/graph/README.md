# Swift graph component

The `ZeppelinEmbedGraph` product wraps the opt-in `graph-cypher` C artifact.
It exposes `ZeppelinGraphStore`, keyed typed batches with ordered receipts,
parameterized Cypher, owned typed rows and `GraphError` with native error codes,
diagnostics and write disposition. Use this artifact instead of the legacy
archive; both contain the core symbols and must not be linked together.

The package layout, binary identity, checksum and C smoke test are reused from
ZE-107 commit `e912af5`. The downloaded artifact and XCFramework paths retain
that packaging contract; this change verifies the source-tree archive mode.
The deployment floor remains macOS 14. Published/minimum-OS qualification is
separate from these local tests.

```swift
import ZeppelinEmbedGraph

let store = try await ZeppelinGraphStore.open(at: directory, mode: .create)
var batch = GraphBatch()
let local = batch.node(
    key: GraphKey(namespace: "app", key: "first"), revision: 1,
    .create(GraphNodeImage(labels: ["Item"], properties: ["name": .string("")]))
)
let written = try await store.apply(batch)
let result = try await store.cypher(
    "MATCH (n:Item) WHERE n.name = $name RETURN n",
    parameters: ["name": .string("")], controls: GraphControls(rowLimit: 100)
)
try await store.close()
// result and written contain owned Swift values and remain usable here.
```

Local references belong to one batch and identify node operations; relationship
IDs cannot be supplied as node endpoints. Put/delete take the expected
incarnation. Recreate takes the deletion revision. There are no implicit retries.
An optional `EmbeddingTower` at open declares the persisted vector space;
without it, native vector admission rejects. No model is loaded by this wrapper.
Presence is explicit: nil payloads differ from present empty strings/vectors;
wrong-dimension present vectors reject. Property list element types and the
untyped `emptyList` sentinel survive independently of query-list equality.

`GraphControls` exposes the row cap, complete compiler limit tightening and
**either** an explicit cancellation token **or** a relative deadline. HEAD
rejects query options and open controls, so those are not exposed. Structured
queries/getters belong to the follow-up blocked by ZE-241; their attachment
point is documented in `ZeppelinGraphStore.swift`.

Task cancellation does not automatically cancel native calls. Explicit tokens
have no public free and stay alive until native completion. Both success and
errors retain the actual disposition, even when the calling Task was cancelled.
Concurrent writes retain native Busy admission; close rejects further Swift
admissions while it awaits the native drain. HEAD consumes the C handle on
close even when final checkpoint/drain returns an error. Response arrays are
borrowed only during the synchronous call and responses are freed after copying
on success, native error and decode failure.

From the repository root:

```sh
CARGO_BUILD_JOBS=3 cargo build -p zeppelin-embed-ffi --release --features graph-cypher
ZE_USE_LOCAL_FFI=1 \
CLANG_MODULE_CACHE_PATH="$PWD/target/clang-ze70" \
SWIFTPM_MODULECACHE_OVERRIDE="$PWD/target/clang-ze70" \
swift test --disable-sandbox --package-path bindings/swift/graph \
  --scratch-path target/swift-ze70 --filter GraphStoreTests
```

The cache variables keep SwiftPM's compiler cache inside the writable worktree.
`ZE_LOCAL_FFI_ARCHIVE` can select an explicit graph-enabled archive. Local archive
mode never downloads or substitutes the legacy artifact. For a locally produced
XCFramework use `ZE_USE_LOCAL_GRAPH_XCFRAMEWORK=1` instead. See
[`ze-70-swift.md`](../../../tasks/evidence/ze-70-swift.md) for exact evidence and
unverified gates.
