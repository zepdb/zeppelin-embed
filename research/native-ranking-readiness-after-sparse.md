# ZE-157: native constrained-ranking readiness

**Decision: NO-GO for the complete Exact/Scan/ANN component. Stop and wait for a real native derived vector-index producer.**

Audited committed main: `6e368be01d96fa28eb629cc3dd8262da5c777d29` (same production as `e13c48c6d7cbf1dafb0704888c191d6582830046`). Read the claimed ZE-157 description, original ZE-62 and accepted `docs/graph/plans/retrieval.md`. Original ZE-62 is still `todo`, blocked by ZE-29/51/61, and was not started. No execution plan is produced: a required actual producer is absent, rather than merely needing an adapter or additional query tests.

## Exact blocking source finding

The committed **native sparse vector source has no ANN graph/index artifact to rank against**. It owns membership, source-local rows, live masks and references to original native vectors. Its vector source manifest does not carry quantized codes/factors, ANN node blocks/entry seeds or a corresponding rescore-region owner.

The positive source evidence is decisive:

1. `crates/zeppelin-embed/src/property_graph/storage/search/codec.rs:127` defines `SourceManifest` with exactly modality, generation, sequence, row count, row-table payload and optional lexical payload. Encoding at line 136 and decoding at line 158 cover that fixed representation. The geometry check at line 178 requires lexical payload precisely for Text, so a Vector manifest cannot smuggle another artifact through that field.
2. `crates/zeppelin-embed/src/property_graph/storage/search/prepare.rs:777` publishes each vector source through `prepare_source(... Modality::Vector, vector_rows, None, ...)`. That preparation writes a `RetrievalRows` table, source manifest and live mask; the vector payload remains in each authenticated native record. Text separately receives its real prepared lexical region at line 753. No native vector-index builder is invoked.
3. `crates/zeppelin-embed/src/property_graph/storage/search/view.rs:252` represents a resolved vector member as full NodeId/revision/source-local row plus `Option<StoredVector>`. The actual source API at lines 1141, 1164 and 1173 provides row count, live-bit checking and row resolution. The only derived-index reader is `lexical()` at line 1236. `resolve_row` checks exact current membership/source/ordinal and native record correlation; it does not expose an ANN graph.
4. `crates/zeppelin-embed/src/property_graph/storage/search/trace.rs:574` traces the source row table and at line 579 its optional lexical region, then native record descendants. This matches the real manifest. It is not evidence that ANN descendants exist, and adding an untracked query-side artifact would not satisfy accepted native publication/reachability semantics.

The missing seam is therefore **a committed native vector-source index owner, published and admitted with the graph's exact source/row/version membership, that supplies the existing ANN kernel's actual graph blocks and compatible rescore backing**. Native quantized source rows/factors are absent too. This report does not prescribe a new format, maintenance strategy, ticket chain or ownership split to manufacture that producer.

## Existing accepted ownership

This missing production work is already within the accepted first-release retrieval architecture; it is not a proposal to add ANN scope:

- `docs/graph/plans/retrieval.md:13`: **“Retrieval owns derived index rows, postings, ANN artifacts and their membership/checkpoint records.”** The same paragraph assigns bounded private index-delta staging and one graph/search-root publication to writes, retains checkpoint bases plus committed active deltas, and forbids the independently publishing document ingestion path.
- `docs/graph/plans/retrieval.md:11` assigns sparse mappings, scoring preparation and complete search-artifact reachability to retrieval. Its “Search reachability for reclamation” section explicitly includes derived vector/ANN artifacts and descriptor descendants under every protected root.
- `docs/graph/plans/storage.md:71`: **“Storage owns lossless original text/vector payload objects; retrieval owns derived indexes and sparse row-to-NodeId membership.”** Dense search copies must be explicit measured duplication. `storage.md:13` reserves the sole commit/publication protocol to writes, while `storage.md:23` gives storage the bounded original-vector payload reader and retrieval scoring/top-k.

The architectural owner is therefore specified: retrieval produces derived indexes, storage supplies canonical payload/artifact mechanisms, writes coordinates their publication. This audit does **not** identify an already complete native ANN producer or assign its missing implementation to ZE-46, ZE-62 or another worker. Existing sparse-membership/active-checkpoint implementation and legacy ANN code do not by themselves fulfill that production seam; root retains the concrete work-allocation decision.

## Why the existing ANN kernel is insufficient by itself

`crates/zeppelin-embed/src/graph/search.rs:1221` defines `GraphSearcher` over `GraphNodeBlocks`, a matching contiguous f32 rescore slice, persisted entry seeds and `GraphSearchScratch`. `with_entry_row_ids` validates rescore geometry and persisted graph entry flags. A sparse membership list or `StoredVector` handle cannot instantiate those inputs.

The existing real producer is the **legacy document segment** path: `segment/reader.rs:1984` decodes `RegionKind::GraphNodeBlocks`; its vector code/factor/rescore regions and `lifecycle/graph_cache.rs` provide the checked graph/cached scratch. `planner/exec.rs:1088` consumes that `SegmentReader`, obtains the prepared graph and `query_rescore_rows(segment)`, then constructs `GraphSearcher`. Its quantized Scan branches at lines 737 and 770 similarly consume Int8/Bit4 code/factor regions from SegmentReader.

Native `GraphReadView::sparse_view` (`property_graph/storage/view/search.rs:10`) opens only the admitted native sparse roots/catalog/lease under the native query owner. It does not contain or admit a legacy SegmentReader. Calling legacy Store search, ingesting native nodes into a separate document store, or constructing an index from the entire admitted population during a query would introduce a different producer/lifecycle and cannot count as this independently ready ranking component.

The exact existing legacy **build and commit** producer is also concrete:

- `graph/build.rs:577`, `build_graph_checkpointed(store, reader: &SegmentReader, request, lease: &SnapshotLease)`, constructs the Vamana artifact through `GraphBuildSession`. `SegmentVectors::new` at line 648 requires nonempty Bit4 scheme 4 and borrows `bit4_codes`, `bit4_factors` and contiguous `rescore_f32` from that segment. It has no native SparseSource/StoredVector input.
- `GraphBuildArtifact::write_segment_with_graph` at `graph/build.rs:113` delegates to `write_segment_with_encoded_graph` at line 132. That function rebuilds a legacy `SegmentBuild` from segment codes/factors/rescore/columns/alive/document-version regions, then calls the existing segment writer. It emits `SegmentMeta`, not a native sparse participant/root.
- `tier/maintain.rs:292` admits a legacy snapshot, selects its segments, calls that builder at line 357 and the segment writer at line 399. `publish_transition` (called at line 407; defined at line 1317) replaces the input in the legacy manifest, invokes `commit_manifest` at line 1380 and replaces `store.snapshot` at line 1396. The consolidation branch similarly builds an intermediate SegmentReader at line 788 and calls `publish_consolidation` at line 839.

These are reusable algorithm/format references, but their accepted inputs and commit authority are the legacy segment lifecycle. They cannot currently consume native sparse rows or publish under the admitted native graph/search root without substantive producer integration. Merely calling them from the new ranker would not establish same-view identity, ownership, replay or reclamation.

## What is genuinely ready

- **Same-view preparation and identity:** `property_graph/retrieval.rs` has `NativeRetrievalContext`, lossless NodeId(u128)/DocumentVersion conversion, dimension/finite-vector checks, explicit NoVectorSpace, authenticated node-version resolution, and exact runtime/memory/view checks through `storage/view/retrieval.rs`. `PreparedNativeVector` retains requested `SearchMode`; this is preparation, not ranking or proof of actual tier/coverage.
- **Complete execution-owned eligibility:** `query/eligibility.rs` has a charged sorted unique full-ID set, separate AllIndexed/explicit empty cases, complete construction or failure, and actual view-token checking through `ids_for`. It can be borrowed without waiting for ZE-154; retrieval still has to implement source-local candidate masks and charge their real capacity.
- **Actual sparse populations:** ZE-61 supplies per-source live masks, row-to-node/version correlation, native original-vector readers and real lexical sources. Those are sufficient inputs for a prospective bounded original-f32 exhaustive scorer and real source-local membership translation. This is not permission to split out Exact or relabel it as Scan/ANN.
- **Existing control/accounting:** Sparse query owners check the real lease/runtime/memory, native readers use `TreeResources`, and the runtime supplies cumulative work/control and reservations. These mechanisms do not supply missing index bytes or prove approximate coverage.

## Acceptance consequence and recommendation

Original ZE-62 requires all requested Exact/Scan/ANN behavior, actual per-source restriction, omitted tier versus explicit Auto, honest precision/coverage, full-ID ties, and proof that ANN does not emit ineligible nodes or mistake an empty window for an empty eligible population. The accepted retrieval plan specifically distinguishes exact eligible streaming from quantized/ANN execution and forbids coverage upgrades merely because retained ANN hits were rescored.

With no native ANN source, a test cannot exercise that accepted ANN path against genuine admitted native artifacts. Always taking exact fallback, rejecting every ANN request, using synthetic graph blocks or marking full coverage from membership would evade that requirement. The presence of SearchMode/report descriptor types does not change this source finding.

**Do not launch a ranking executor from ZE-157.** Keep ZE-62/63/64 dependencies and acceptance unchanged. Reconsider only after an actual native derived-index producer is committed and its admitted source binding is reviewable. Completion of ZE-46/154/156 is not asserted to provide it; their moving work was not inspected. No additional readiness chain or partial implementation plan is proposed.

This bounded audit performed source/tracker reads only. No builds, tests, product edits, tracker writes, worker-source reads, subagents, broad campaigns or downstream hybrid/public redesign were performed. This report is planning evidence, not measured runtime evidence.
