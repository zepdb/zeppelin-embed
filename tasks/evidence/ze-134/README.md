# ZE-134 native-view and retrieval readiness

Date: 2026-09-19

## Decision

One substantive production component can start against the compiled ZE-43 and
ZE-49 contracts plus the exact frozen ZE-44 candidate. It is a private
query-to-native-storage resource bridge, query-owned adjacency scratch, and the
node/payload point-read kernel that consumes those resources. It does not admit
or construct a `GraphReadView`.

No second ZE-60 or ZE-61 component is dependency-ready. The useful identity
accessors already exist, while view-bound index rows, lexical membership and a
published retrieval participant do not.

This is a scheduling recommendation. It does not change any dependency or
acceptance criterion on ZE-44, ZE-45, ZE-60 or ZE-61.

## Exact inputs

| Input | Pin and state | Relevant compiled contract |
| --- | --- | --- |
| Audit checkout | `89ab56983854e75d7e6ac6e31942fb3e6172cf10` | Contains the integrated ZE-43 native tree/record source and ZE-49 query runtime used by this audit. |
| ZE-43 | integrated source `190e702e996f1638993566e88ccfd10e885089f3`, main integration `0e0064ddb3b1630e8c18050cb78c30ac7bcfb638`; tracker `done` | `BlockSource`, `TreeResources`, `GraphRoots`, `PayloadSlice`, `RecordView`, and native record validation. |
| ZE-49 | source `68f2d7d90c0418d4c599cb4237fc43ca5616d0bc`, main integration `f9d81e0806ca68c876a26ed1983a3fffaf72b902`; tracker `done` | `RuntimeContext`, typed `WorkKind`, `QueryMemory`, `QueryArena`, and close-first retained-view checks. |
| ZE-44 | candidate `1052fb475403a5f6a6e1934e13c51ba91fb0d24b`, parent `273eb33e66c281da18264d7f5eb7c08165e63ddf`; tracker `in_progress` | `NativeGraphReader`, `RangeScratch`, and paired adjacency preparation. The 12 production files match `/tmp/ze44-fixture-final-review/sha256.json`, whose SHA-256 is `9149f0876c84610994d36018489b561a51ee2fafe7661f97a12c490d5053c209`. |
| ZE-133 | prerequisite database pin `db98cc516cdf90194003ad8d7ece102eedad68dd`; tracker `in_progress` | Final ZE-44 runner qualification remains outside this recommendation. |

The canonical plan bytes read for this audit were:

- `storage.md`:
  `177ceaf6f68e3220eeff05770e481ea79358d8776d6cadf76eef3da72ce12344`
- `writes.md`:
  `08cd63fc8db2e52e70a2ca01f1be2c791bb756ff4992b605f1dea3b23bf44ee4`
- `retrieval.md`:
  `adf94b0f437ae4f3db546bfee22d9a849ab31f4bda4d6ab248ffa7ac9109db11`
- `execution.md`:
  `92baa66d5ac23c4a64dd7f2ad172b1bf8e7cc72945dcebdd7f2207a950ed8af2`
- `parallel-contracts.md`:
  `34cdaf82eef72625a5d4704b5c706a110479d83832688a917717006a6e875607`

## Readiness table

| Component | Ready? | Exact owned production paths | Current prerequisites | Retained gate |
| --- | --- | --- | --- | --- |
| Query-native storage resource bridge, query `RangeScratch`, and node/payload point reads | **Yes, as one private follow-on component** | `crates/zeppelin-embed/src/property_graph/query/{runtime.rs,resources.rs}`; `crates/zeppelin-embed/src/property_graph/storage/{mod.rs,stream.rs,view.rs}`; `crates/zeppelin-embed/src/property_graph/storage/tree/directory.rs`; `crates/zeppelin-embed/src/property_graph/storage/adjacency/{range.rs,read.rs}` | Landed ZE-43 and ZE-49 APIs; exact ZE-44 source `1052fb4`. Root must materialize that exact source before work begins and keep the adapter as a separate commit while ZE-44/ZE-133 finish. | It cannot expose a public view or execute without the later writes-owned admitted source/catalog/lease. ZE-45 retains admission, cursor/view identity, old-reader lazy-open, close and full integration acceptance. |
| `GraphReadView` admission and durable multi-artifact source | **No** | ZE-45 plus the writes-owned lifecycle paths selected by the coordinator owner | ZE-44 must close; writes' publication mutex/lease registry does not exist yet. | All ZE-45, ZE-39, ZE-40 and ZE-46 lifecycle, recovery and reclamation acceptance. |
| ZE-60 retrieval adapter | **No** | No production retrieval path exists yet | Original dependencies ZE-29, ZE-32 and ZE-37 are done; ZE-45 is still open. | Same admitted view across eligibility, search and materialization; checked full-width row/version translation and actual search execution. |
| ZE-61 sparse membership/checkpoint participant | **No** | No compiled row-map or retrieval participant path exists yet | Original ZE-29, ZE-35 and ZE-42 dependencies are done; ZE-60 is still open. | Sparse `T`/`V` membership, checkpoint/replay, atomic publication and recovery remain complete ZE-61 acceptance. |

## Startable component contract

The implementation should be one flat component. Splitting only the node lookup
would rearrange already available internals without resolving the resource seam.
The minimum private API direction is:

```rust
pub(crate) enum NativeReadWork {
    Lookup,
    Scan,
    AdjacencyEntry,
    CopiedBytes(u64),
}

impl TreeResources<'_> {
    pub(crate) fn for_query(
        context: &mut RuntimeContext<'_, '_, '_>,
    ) -> Result<TreeResources<'_>, TreeError>;

    pub(crate) fn read_work(
        &mut self,
        work: NativeReadWork,
    ) -> Result<(), TreeError>;
}

impl RangeScratch<'_> {
    pub(crate) fn for_query(
        memory: &QueryMemory<'_>,
        resources: &mut TreeResources<'_>,
    ) -> Result<RangeScratch<'_>, TreeError>;
}

pub(crate) fn lookup_node_state<'a, S, C>(
    source: &'a S,
    roots: GraphRoots,
    node: NodeId,
    catalog: &C,
    document: Option<&EmbeddingTower>,
    resources: &mut TreeResources<'_>,
) -> Result<Option<NodeRecordState<'a, S>>, TreeError>
where
    S: BlockSource,
    C: RecordCatalog<S>;
```

The exact Rust lifetime spelling may use a private split-borrow adapter around
`RuntimeContext`; the semantic ownership may not change:

- query `TreeResources` borrows the existing `RuntimeContext` for one storage
  operation. Its workspace reservation comes from `context.memory()`, so it is
  nested under the same `QueryMemory` and shared store accounting. It creates no
  per-lookup, per-range or per-operator budget;
- query `RangeScratch` owns one fixed-capacity `QueryArena<Edge>` for
  `MAX_MERGED_ENTRIES` plus the exact `MERGE_STATE_BYTES`/descriptor charge. It
  is tied to that same `QueryMemory`, reusable across pulls, and must reject a
  mismatched `TreeResources` owner;
- the preparation branch keeps `StorageMemory`, `StorageBuffer`,
  `StorageReservation` and `require_preparation` ownership. Adding enum/query
  branches changes source bytes, so "preserve `for_prepare`" means identical
  preparation behavior and ownership proved by the existing ZE-44 tests plus
  focused regression tests, not byte-identical source after the follow-on
  commit;
- `lookup_node_state` uses `lookup_entry` -> `PayloadRef` ->
  `verify_node_state`. Its `NodeRecordState`/`RecordView` and optional
  text/vector `PayloadSlice` values remain borrowed from `&'a S`. The function
  opens no file, retains no unprotected path, assembles no public roots and
  allocates no per-record backing;
- the future admitted view is the only production caller allowed to supply
  `source`, `roots`, `catalog` and `document`. Tests may supply explicit fixture
  ownership but do not prove admission.

### Accounting and control

`TreeResources::step` currently adds bytes and comparisons into one scalar
(`storage/tree/directory.rs:151-164`). ZE-44's range adapter also collapses
`Work::{HeaderBytes, EntryBytes, CopyBytes, Compare, Finish}` into that scalar
(`1052fb4:crates/zeppelin-embed/src/property_graph/storage/adjacency/range.rs:195-200`).
That sum must never be converted
to `WorkKind::AdjacencyEntries` or any other runtime counter.

The query branch instead uses actual-site events:

- `Lookup`: charge `WorkKind::Lookups` where a directory lookup is actually
  entered, including missing results;
- `Scan`: charge `WorkKind::Scans` once when a source cursor/scan is actually
  entered, including an empty result;
- `AdjacencyEntry`: first charge `WorkKind::AdjacencyEntries` once for every
  actual physical base/delta entry visit reported by `Work::EntryBytes(width)`
  from `codec::Run::entry`, including delete observations discarded by the
  merge
  (`1052fb4:crates/zeppelin-embed/src/property_graph/storage/adjacency/codec.rs:11-20`;
  `1052fb4:crates/zeppelin-embed/src/property_graph/storage/adjacency/merge.rs:170,210`).
  Then charge the distinct merged-edge examination in
  `NativeGraphReader::visit_adjacency` before query-range rejection,
  authoritative relationship validation or endpoint-liveness filtering
  (`1052fb4:crates/zeppelin-embed/src/property_graph/storage/adjacency/read.rs:387-408`).
  An event is charged once at its actual site; a physical
  entry visit and its later surviving merged-edge visit are separate work, and
  repeated real visits remain work. Header bytes and comparisons are not entry
  events;
- `CopiedBytes(n)`: charge `WorkKind::CopiedBytes` only after checking the exact
  copy size and before a real copy. Valid sites include `PayloadSlice::read_at`,
  `PayloadCursor::read_array`, actual output-row copies, and only
  `Work::CopyBytes(n)` from the adjacency merge. Header bytes, entry visits and
  comparisons are not copy events.

Every low-level query checkpoint must call `RuntimeContext::checkpoint`, which
checks the retained view/close state before caller cancellation. It must not
fall back to the raw `QueryControl`. `RuntimeContext::charge` remains the sole
typed cumulative limit owner. Existing bounded parser steps may remain for
preparation and diagnostics, but the query branch may not introduce an
independent scalar work allowance.

Query failures should retain their type, for example with a
`TreeError::Runtime(RuntimeError)` branch, instead of flattening a typed
`RuntimeError::Limit(WorkKind)`, `Value(QueryError)` or `Memory(MemoryError)`
into generic `Work`/`Memory`. I/O, missing-object, format and corruption errors
remain distinct. Any error invalidates private output and returns no partial
successful rows; reservations and scratch charges release by ownership/drop.

### Why the compiled producers are sufficient

- `RuntimeContext` already owns one retained view, `QueryMemory`, typed limits
  and cumulative counters (`query/runtime.rs:187-265`).
- `QueryArena::new` reserves exact capacity under `QueryMemory` before fallible
  allocation and never grows implicitly (`query/resources.rs:203-276`).
- `BlockSource::resolve` already makes source ownership explicit, and
  `lookup_entry` returns a source-borrowed entry
  (`storage/tree/directory.rs:50-60,578-585`).
- `RecordView` exposes correlated identity/revision/canonical payloads without
  per-record backing (`storage/records/native.rs:51-104`), and
  `PayloadSlice` preserves present-empty data and <=64 KiB spans
  (`storage/stream.rs:10-176`).
- frozen `NativeGraphReader` borrows one source/root/catalog/document set and
  labels its constructor as metadata rather than admission
  (`1052fb4:crates/zeppelin-embed/src/property_graph/storage/adjacency/read.rs:54-81`).

The component therefore needs an additive resource/event adapter, not an
unavailable producer format or a coordinator decision. Its consumers remain
blocked until the real admitted source exists.

## Remaining blockers and unsupported assumptions

The current production tree has no definition of `GraphReadView`,
`PublicationMutex`, `LeaseRegistry`, `admit_read` or
`PreparedGraphArtifacts`. `GraphRoots` explicitly holds metadata only and
acquires no lease (`storage/tree/directory/roots.rs:1-10`). The only compiled
`BlockSource` owners are preparation/private-object implementations; there is
no admitted multi-artifact source that can lazy-open protected old files.

Legacy `PublishedSnapshot` holds the vector/search snapshot fields and segment
readers, not native graph/catalog/search roots
(`lifecycle/snapshot.rs:296-311`). `SnapshotLease` retains that legacy snapshot
and cancellation state (`lifecycle/snapshot.rs:680-749`), while
`Store::snapshot` admits only that state (`lifecycle/mod.rs:2957-2986`). It is
not a native graph lease. `GraphResources`, `QueryMemory` and `QueryView` are
accounting/identity capabilities, not admission.

The frozen `RangeScratch` is preparation-only and
`validate_descriptor` calls `require_preparation`
(`1052fb4:crates/zeppelin-embed/src/property_graph/storage/adjacency/range.rs:34-69,124-149`).
The proposed query branch
must be added explicitly; pretending the current constructor already supports
reads would be incorrect. `NativeGraphCandidate` retains roots, sequence and a
storage reservation, but not the source/private pack or a base read lease
(`1052fb4:crates/zeppelin-embed/src/property_graph/storage/adjacency/prepare.rs:20-72`).

Retrieval cannot use `staging::Membership` as lexical `T` membership. The
current encoder sets `text = self.text.is_some()` and
`vector = self.embedding.is_some()`
(`property_graph/staging/encode.rs:221-224`), while the accepted retrieval plan
excludes absent, explicit-empty and analyzed-to-empty text from `T`. There is no
sparse row-to-`NodeId`/version mapping, search-root participant, checkpoint
codec or replay owner. `RecordView::{shape, incarnation, revision}` already
preserves the only safe identity atom, so a separate identity-wrapper component
would duplicate landed behavior without making ZE-60 usable.

Do not infer any of the following from this recommendation:

- loose `GraphRoots` plus a `BlockSource` is a coherent admitted view;
- `RuntimeContext`, `QueryView`, `GraphResources` or `QueryMemory` owns a graph
  lease;
- a fixture/private-pack source permits lazy production file opens;
- scalar `TreeResources::work()` units equal semantic query counters;
- optional payload presence equals retrieval membership;
- the adapter qualifies ZE-44, ZE-45, ZE-60, ZE-61 or the later public
  lifecycle/recovery/reclamation work.

## Retained acceptance

ZE-44 remains subject to its full ZE-133 qualification and exact candidate
review. The follow-on adapter must start from `1052fb4`, preserve the
preparation path by regression proof, and remain a separate source change so it
does not rewrite the frozen ZE-44 evidence.

ZE-45 still owns writes-coordinated atomic capture/register, the real retained
multi-artifact source, same-view catalog/graph/search roots, view-bound cursors,
lazy old-file protection, close cancellation, mixed lookup/expand/materialize
integration and reservation release. ZE-60 still owns actual view-bound search
and full-width row/version translation. ZE-61 still owns sparse membership,
checkpoint/replay and publication participation. ZE-39, ZE-40 and ZE-46 still
own final coordinator, recovery and reclamation behavior.
