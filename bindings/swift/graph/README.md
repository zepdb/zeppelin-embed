# Swift graph component

The `ZeppelinEmbedGraph` product wraps the opt-in `graph-cypher` C artifact.
It exposes `ZeppelinGraphStore`, keyed typed batches with ordered receipts,
structured queries and mutations, vector/text/hybrid search, entity getters,
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

`GraphPlan` uses typed indexed arenas matching the existing C ABI. Operator,
expression, slot, parameter, search and eligibility IDs are distinct. All 21
operators, 9 expression kinds and 6 mutation kinds are exposed; native admission
validates references, DAGs, types, scopes and capabilities before effects. Lists
are bounded nonentity parameters (depth 16, 524288 elements). Numeric lists also
carry search vectors. Eager search operator IDs must be supplied in source order;
call IDs are their zero-based positions, including calls outside the root DAG.

```swift
let plan = GraphPlan(
    root: GraphOperatorID(1),
    operators: [
        .scanNodes(output: GraphSlotID(0), label: "Item"),
        .project(input: GraphOperatorID(0), bindings: [
            GraphProjection(GraphSlotID(7), GraphExpressionID(1))
        ])
    ],
    expressions: [.slot(GraphSlotID(0)), .property(GraphExpressionID(0), "name")]
)
let rows = try await store.query(plan) // column: slot_7
let nodes = try await store.getNodes(ids, fields: GraphNodeFields(text: true, vector: true))
let relationships = try await store.getRelationships(relationshipIDs)
// Getter entries are ordered optionals; missing IDs are nil, duplicates remain.
```

`GraphQueryOptions` is shared by `query` and `cypher`. It declares an optional
query tower, explicit alignment digest and `GraphQueryLimits` for memory and typed
work allowances. Absent limits retain native defaults; an explicit zero is a
real allowance. Getter calls accept the same limits. No model is loaded.
`GraphControls` exposes the Cypher row cap and compiler limits. Query/getters
accept `GraphInterruption`: either cancellation or a relative native deadline.
Even empty getter requests call native validation.

At base `cf8812c2`, the C structured-query adapter rejects named bindings with
"unproved query input ownership": it does not retain the decoded parameter
backing in its plan ownership inventory. The Swift API marshals named bindings
without substitution; the public regression
`GraphStructuredQueryTests/testStructuredQueryReturnsOwnedRows` fails loudly
until that **ZE-241 retained parameter backing repair** lands. Cypher named/list
parameters already work. This source component is not final qualification.
Full counters wait for ZE-76; mutation limit qualification waits for ZE-290;
installed packaging waits for ZE-71 and the final graph size decision for ZE-287.
Rust/C/Swift parity remains ZE-72. The Swift wrappers in this directory are the
ZE-278 implementation and do not require a separate unlanded wrapper input.

Task cancellation does not automatically cancel native calls. Explicit tokens
have no public free and stay alive until native completion. Both success and
errors retain native diagnostics and the actual disposition, even when the calling Task was cancelled.
Concurrent writes retain native Busy admission; close rejects further Swift
admissions while it awaits the native drain. HEAD consumes the C handle on
close even when final checkpoint/drain returns an error. Response arrays are
borrowed only during the synchronous call and responses are freed after copying
on success, native error and decode failure.

From the repository root:

```sh
CARGO_BUILD_JOBS=3 cargo build -p zeppelin-embed-ffi --release --features graph-cypher
ZE_USE_LOCAL_FFI=1 \
CLANG_MODULE_CACHE_PATH="$PWD/target/clang-ze278" \
SWIFTPM_MODULECACHE_OVERRIDE="$PWD/target/clang-ze278" \
swift test --disable-sandbox --package-path bindings/swift/graph \
  --scratch-path target/swift-ze278 --filter 'Graph(StructuredQuery|Getter|QueryOptions|ReadBoundary|Store)Tests'
```

The cache variables keep SwiftPM's compiler cache inside the writable worktree.
`ZE_LOCAL_FFI_ARCHIVE` can select an explicit graph-enabled archive. Local archive
mode never downloads or substitutes the legacy artifact. For a locally produced
XCFramework use `ZE_USE_LOCAL_GRAPH_XCFRAMEWORK=1` instead. See
[`ze-278-swift.md`](../../../tasks/evidence/ze-278-swift.md) for exact evidence and
unverified gates.
