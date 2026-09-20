# ZE-158: publish genuine native vector indexes

Status: concrete plan for root review, then Sol/xhigh execution on the claimed ZE-158 ticket. Immutable source pin: **`856cfa602742b4bd6eaefda14300c91e9f8753d8`**. All source findings below use committed main; no moving ZE-46/154/156 source is a prerequisite. Read this complete plan, AGENTS.md and ZE-158 before implementation. Root supplies the isolated worktree and preservation inventory. No implementation/build/test evidence is claimed by this plan.

## Final source decision

Implement a real derived index for **every nonempty newly prepared native vector source**, including one-row sources. Build only from the bounded changed-vector rows of the current atomic native batch; published sources are immutable and shared. Replacement/deletion changes existing sparse live masks and creates the new source for surviving changed vectors. Never rebuild older sources or the whole store during a small write. The existing native checkpoint records these already durable sparse roots and sources; no new query-time build or independently publishing maintenance/legacy store is introduced.

This is accepted first-release production work: retrieval.md:13 assigns derived rows, ANN artifacts and membership/checkpoint records to retrieval, storage.md:71 assigns original payloads to storage, and writes.md:57/65/94 requires bounded private index preparation followed by the sole coherent native commit. Building a bounded immutable source for the changed rows satisfies that lifecycle; it does not seal or rebuild every existing index. Source packing may divide one atomic batch into several physical index sources under the same commit, never split the logical transaction.

**Small sources are genuine ANN graphs, not a permanent fallback class.** `graph/build.rs::refined_entry_points` already chooses `min(4,N)` distinct real seeds, and `graph_build_handles_the_minimum_shape_and_rejects_an_empty_segment` proves a real one-row graph with one flagged seed and degree zero. The frozen node-block format permits that shape. Correct `GraphSearcher::discover_entry_row_ids` to require exactly `min(4,N)` distinct flagged rows and return the existing four-entry adapter by repeating the first real seed only in unused slots. `with_entry_row_ids` already accepts those flagged entries, and traversal's `mark_visited` deduplicates them before scoring/counters. For N>=4 preserve the exact existing four-seed requirement. Never fabricate nodes, edges, flags or visited counts. Empty population has no source and no graph.

Thus a store grown entirely through one-vector writes obtains actual usable graph artifacts from its first vector onward. A bounded active-source merge is unnecessary for correctness or availability in this component. Future consolidation/quality/performance remains its original owner; no speed or scaling qualification follows from the source-local design.

Index availability is distinct from query policy. Keep SearchMode Default/Auto/Exact/Scan and all existing SearchOptions requested-tier/precision rules unchanged. The committed structured SearchMode has no explicit ANN variant. Legacy explicit SearchTier::Graph rejects a sealed source without a graph (`planner/exec.rs:194`), so this plan does not assume universal missing-index fallback. Existing thresholds/profiles are not retuned. ZE-62 decides actual routing/coverage from real source facts; rescoring does not establish exhaustive candidate coverage.

## Frozen implementation allowlist

Paths are repository-relative. Keep helpers in the named owned children; no broad module rewrite.

| Files | Exact purpose |
| --- | --- |
| `crates/zeppelin-embed/src/quant/bits4.rs`, `quant.rs`; new `quant/bits4/controlled.rs` | One common Bit4 algorithm with bounded caller-owned scratch/control entry; minimal internal re-export. Preserve existing public entry and numeric bytes. |
| `crates/zeppelin-embed/src/graph/build.rs`; new `graph/build/native.rs` | Extract the current borrowed row input from SegmentReader construction; reuse current Vamana BuildState/process/search/prune/entry/encoding algorithm with controlled, fallible, accounted native ownership. Keep legacy checkpoint/publication wrapper unchanged. |
| `crates/zeppelin-embed/src/graph/block.rs` | Shared controlled encode-into/checked decode leaves over existing frozen node-block bytes, without a second graph codec. |
| `crates/zeppelin-embed/src/graph/search.rs` | Only the checked small-population seed discovery correction and its focused regression. No ranking policy or traversal algorithm redesign. |
| `crates/zeppelin-embed/src/property_graph/storage/artifact.rs`, `storage/payload.rs` | Append the one required native derived-vector payload role and existing exhaustive role decoding/validation. Preserve old tag numbers. |
| `crates/zeppelin-embed/src/property_graph/storage/search.rs`, `search/codec.rs`, `search/prepare.rs`, `search/view.rs`, `search/checkpoint.rs`, `search/trace.rs` | Source descriptor/index ownership, actual staged production/read/validation/replay/trace integration. Shared overlap contract below. |
| New `crates/zeppelin-embed/src/property_graph/storage/search/vector_index.rs` and `vector_index/{prepare,tests,test_support}.rs` | Native index format, bounded owners/build adapter/reader and exactly eight native acceptance groups/shared directed helpers. |
| `crates/zeppelin-embed/src/lifecycle/native_graph.rs` | At most a test-module declaration/export; no coordinator state-machine rewrite. Native tests may instead reside in the owned index child if committed public/private visibility suffices. |
| New `tests/adversarial/graph_native_vector_index.rs` | Thin wrapper around the same real directed helper. Root owns registration. |
| `tasks/evidence/ze-158-native-vector-index.md` | Exact source pin, RED/GREEN, hashes, focused commands/counts and actual work/allocation/failure receipts. |

No executor edits to ZE-154 pattern/relational or ZE-156 completed modules, compiler/FFI, manifests, legacy tier maintenance/publication, graph plans or tracker. No dependencies. Root performs shared module/runner/coverage registration. If the concrete native tests require a test-only child declaration, make only that additive declaration, not edits to existing large native fixture modules.

## Exact persisted native contract

Append **BlockKind::RetrievalVectorIndex = 18** and add it to the existing typed PayloadRef encode/decode/role/extent-validation cases. Use `prepare_payload`/PayloadSlice for the complete logical index image, including multi-object extent streams. Do not fit an overlarge image into one 4 MiB object or add a separate file/VFS lifecycle. Every reference belongs to the existing PreparedObjects inventory and source lease.

Extend only the sparse **inner source participant** to version 2, 200 bytes. The outer CommitParticipant block framing version and every PhysicalRef.version remain **1**; the root descriptor's inner version/layout also remain **1**. Version-2 source retains the existing first 144-byte fields (with its explicit inner version changed to 2) and appends index presence at offset 144, seven zero reserved bytes, and a 48-byte PayloadRef at offset 152. New Text sources have their required lexical region and no vector index; new Vector sources require a vector-index reference and no lexical region. Index role must equal RetrievalVectorIndex. Unknown version/tag/presence, nonzero reserved bytes and inconsistent modality fail loudly.

Existing source v1 is an explicitly recognized older representation: decode its exact 144-byte shape unchanged with **no index capability**. SourceManifest retains a checked explicit inner-format discriminator (V1 or V2) obtained from the wire. Encoding dispatches on that retained discriminator and enforces its exact width/fields; it must never choose a version from vector_index presence, guessed geometry, defaults or a failed v2 validation. Physical-only rewrites preserve the decoded version: a v1 source with vector_index=None re-encodes as v1/144 bytes, while v2 remains v2/200 bytes and a v2 Vector with no index is an error. Constructors for newly prepared logical sources explicitly select V2; newly prepared logical Vector sources always require their genuine index. Do not infer an index, reinterpret lexical bytes, automatically rebuild on query/open, or report that v1 supplies ANN. This versioned decoder preserves existing native sparse fixtures/stores without hiding missing capability; it is not an ANN fallback. Automatic conversion/consolidation of pre-component v1 sources is outside this producer and must not be silently inserted. ZE-62's existing tier/availability behavior remains required for any such mixed admission.

The new index image uses this hand-written little-endian **256-byte header**, followed by five contiguous checked sections. No serde, pointer/usize persistence or native struct dumps.

| Offset | Field |
| --- | --- |
| 0 | 8 bytes magic `ZGNVIDX1` |
| 8 / 10 / 12 | u16 image version=1 / quantization scheme=4 / squared-L2 metric tag=1 |
| 14 / 15 | u8 supported existing build profile (Sift=1, Angular=2) / distinct seed count |
| 16 | full u128 StoreInstanceId |
| 32 / 36 / 40 | u32 rows / logical dimensions / padded dimensions |
| 44 / 45 / 46 | u8 r_target / u8 r_max / u16 l_build |
| 48 / 52 | exact f32 alpha_build / alpha_refine bits |
| 56 | u64 deterministic build seed |
| 64 | existing exact 96-byte RequiredRef for the authentic interpretation catalog |
| 160 | five ordered `(offset:u64,length:u64)` pairs: identities, codes, factors, rescore, graph |
| 240 | four u32 seed adapter entries; first min(4,N) distinct actual seeds, unused positions repeat first |

Section geometry:

- identities: exactly N records of NodeId:u128 plus installed revision:u64 (24*N bytes), in **the same row ordinal order** as SourceManifest.row_table;
- codes: exactly N*ceil(D/2) existing MSB-first Bit4 bytes, no new quantization semantics;
- factors: exactly N*12 bytes, the existing three f32 persisted fields in declaration order;
- rescore: exactly N*D*4 original supplied f32 bits in row-major order, not normalized/rounded reconstructed values;
- graph: existing unchanged ZEGRNB01 node-block image; its row count, dims, padding, max degree, codes/factors and seed flags agree with the header and same ordinal rows.

Sections start at 256, are ordered/contiguous/nonoverlapping with no trailing bytes, and all products/sums/usize conversions are checked. Existing native object/payload checksums cover the image; the existing node-block trailer/checksum remains intact. Empty vectors/sources, nonfinite factors, invalid normalization/metric/profile, wrong degree/neighbor IDs, duplicate/self neighbors, unknown refinement flags or inconsistent graph/codes/factors reject through the existing validators. No unchecked aligned cast of persisted bytes to f32/factors.

The RequiredRef authenticates the full original document interpretation through the existing checked catalog reader and exact GraphInterpretation/EmbeddingTower comparison, including all model/weights/dimension/normalization fields. A hash or matching dimension alone is insufficient. Select the existing build parameters from the declared document normalization (None -> SiftClass; L2 -> AngularClass) under squared-L2; preserve the existing Angular actual-row unit-norm check. Do not invent a query tower, normalize supplied coordinates or change query-tower alignment policy; later ranking validates its query interpretation. Persist the actual profile/params/seed used. Use one named deterministic native seed **0x20_00c0_ffee**, the current maintenance seed value, not allocator IDs/time/system randomness.

**Physical relocation must not invalidate a logically unchanged index.** Bind index identities to exact NodeId/revision/vector bits and document interpretation, not to the row table's physical PayloadRef or current native record location. When resolving a live row, correlate index identity and rescore bits with that row's verified canonical vector/version. During full source validation, check every original source row's identity/vector content through its existing record reference, including nonlive rows retained in the immutable index. A relocated source may retain its original index and catalog refs while changing only proven record locations. A different ID, revision, dimension or coordinate must not be excused as relocation.

## Bounded construction using the real algorithms

Production entry lives in search/vector_index/prepare.rs and receives the actual candidate source rows, admitted document/catalog, existing BlockSink, StorageMemory and TreeResources from `prepare_sparse`. It does not admit another snapshot or invoke legacy build_graph_checkpointed/SegmentReader/segment writer/commit_manifest.

Partition changed vector rows into consecutive physical source cohorts of **at most 1,024 rows**, with an additional **4 MiB original-f32 rescore limit per cohort**. These are internal source-packing bounds, not new logical input/search limits. A single valid row larger than 4 MiB is its own cohort and must fit the existing 8 MiB graph-input and real 32 MiB storage allowance; if the complete native build cannot fit, fail the atomic preparation with the existing resource error. Never truncate rows or widen memory. The cohort geometry is checked before allocation. Same atomic batch may publish several cohorts; each has independent dense ordinals, exact IDs/versions and a real graph, even N=1. No existing source is added to a new cohort.

Copy original coordinates for only the current cohort into charged f32 backing through verified StoredVector readers, preserving exact bits and charging each real read. Build codes/factors with the existing Bit4 algorithm. The current quantize_bit4 allocates a seven-events-per-coordinate Vec and performs an uncontrolled sort: it is **not** ready merely by wrapping its call in a reservation. Add a shared controlled scratch entry; existing public quantize_bit4 remains a compatibility entry to the same algorithm. Caller-owned critical-event/magnitude arrays have checked fixed capacities, fallible allocation and real lifetime charges. Controlled comparison/sort/event/packing loops poll at <=256 units and preserve the complete existing comparator/numeric order; golden codes/factor bits must remain identical.

Decouple the existing borrowed `SegmentVectors` computational fields from its SegmentReader/checkpoint identity wrapper. The native constructor accepts checked borrowed codes/factors/original f32 rows plus dimensions/count. The legacy constructor fills the same computational view from SegmentReader. Reuse the **same** BuildState, refined_entry_points, process_node, search_candidates, robust_prune, reciprocal-edge and encode_artifact logic. Do not copy Vamana into property_graph or call only a generic prune helper while implementing another builder.

The native build runs One pass using the existing supported profile params and seed, with no legacy disk-build checkpoint. Its cohort is one bounded preparation unit; cancellation/failure drops it, success becomes a native durable participant under the outer atomic commit. Existing graph build checkpoint semantics and legacy byte layout remain unchanged.

Make the shared native-callable leaves genuinely controlled and fallible. Initial medoid O(sample^2) work, distance coordinate loops, candidate/neighbor scans, sorting, reciprocal pruning, initialization, final graph validation/encoding and checksum chunks all need bounded polling, not only BuildState's present every-64-row checkpoint. Use a typed mandatory control callback that preserves TreeResources failures, and checked fixed scratch capacities. Refactor allocation sites reached by the native entry to fallible exact reservations; no infallible Vec::with_capacity/vec!/collect growth on this path. Keep one algorithm; do not substitute an arbitrary graph when budgets fail.

Charge complete simultaneous actual capacities to the same StorageMemory -> WriteMemory -> GraphResources chain: original cohort rows, code/factor rows, quantizer scratch, shuffled order, entries, adjacency degrees/slots, inserted/visited arrays, candidate/prune/medoid scratch, padded codes/node inputs, encoded graph, combined index image, row table and still-retained PreparedObjects. Existing optional AccountedCounter estimates alone do not establish native ownership. A small native owner can retain one aggregate StorageReservation for its actual owned Vec capacities if every allocation is admitted beforehand, allocator rounding is reconciled immediately and every scratch overlap is included; do not build a new general allocator framework. Prefer existing StorageBuffer where it naturally owns the backing. Shrink charges only after corresponding allocations are freed, not after transferring bytes into another still-live buffer.

Controlled node-block encode/decode must reuse the frozen codec. Provide caller-owned encode-into backing and bounded callback validation/checksum leaves as needed; existing public Vec-returning/borrowed APIs delegate to the same codec. No second parser or re-encoding oracle. Decoding does not rebuild or re-quantize an index.

Native preparation failures preserve the existing specific TreeError memory/work/control/source cause. Preserve detailed graph/quantization validation errors in the native index error at its owned boundary; integration may map impossible producer geometry to an explicit static TreeError::Invalid reason, but must not flatten a real cancellation/memory/I/O cause or claim success with a raw source. All allocation/index validation completes before protect_and_commit; no fallible index construction after durable publication.

## Publication, replay, checkpoint and trace

`prepare_sparse` builds each index from the exact already-verified candidate node rows, writes index payload then row table/source manifest/mask through the existing PreparedObjects sink, and installs membership for that cohort. Reuse PreparedSparseCandidate ownership/finalize and PreparedGraphArtifacts; existing create/apply/protect_and_commit carries every new object and coherent sparse root. No independently committed index. Mutation membership booleans/cardinality stay unchanged by physical cohort packing, and original per-node revisions/provenance remain unchanged.

Expose `SparseSource::vector_index(resources)` as a checked owned/view-bound reader. Text has no vector capability; explicit source-v1 has none; v2 Vector must open its required index or error. Query/preparation owners use existing SparseOwner checks/reservations, actual RuntimeContext/lease and typed TreeResources. The reader retains complete backing for borrowed codes/GraphNodeBlocks; if logical payload extents require copying, that bounded source-local buffer is actually charged. Decode f32/factors into charged typed backing if safe borrowing is unavailable. Never heap-materialize all sources or the whole eligible population. Return exact source-local rows/identities and accessors for real scan rows, GraphNodeBlocks, rescore and seed entries; a borrower cannot outlive the source/lease/backing owner.

The source reader validates geometry/header/catalog/node-block consistency and identity correlation, including direct canonical original-f32 comparison at validation rather than trusting duplicated ID metadata alone. Existing source liveness remains authoritative: dead/superseded rows may exist as ANN traversal structure but cannot become live rank candidates. ZE-62 owns real masks and final candidate filtering; this producer must preserve the ordinal mapping it will need.

Extend existing validate_all/checkpoint/persisted replay traversal to validate the required index and exact source row correlation. A malformed, missing or foreign v2 index is a required-source error; it cannot become optional or trigger rebuild/Exact fallback. Source-v1 remains explicitly distinguishable and is never counted as indexed. Native checkpoints reuse already published index refs; WAL replay validates complete committed participants, does not rerun quantization/Vamana, and uses the existing full native reopen path. No new WAL envelope kind or independent index checkpoint is needed.

Extend SearchTraceCursor with the index logical payload and its extent/chunk descendants, plus the index's authenticated interpretation-catalog RequiredRef and its already-defined descendants. Preserve <=256 refs per next call, exact lease/cursor owner, explicit completion and cumulative work/memory. Missing/corrupt/unsupported required index or interpretation aborts the trace with no deletion authority. Do not use inventory membership as reachability. Validate/trace one bounded source at a time, not a retained Vec of all native indexes.

## Root merge contract with ZE-46

The only expected storage overlap is search/{codec,prepare,view,checkpoint,trace}. ZE-46 changes physical source-row relocation and bounded tracing; this component adds an index field/owner/descendants. Root integrates individual commits and reconciles both additive contracts. The executor targets the pinned existing APIs and must not copy/wait for moving ZE-46 helpers.

For any ZE-46 source rewrite, root must preserve the **explicit decoded inner source version and vector_index** exactly when logical IDs/revisions/vector bits are unchanged, alongside lexical payload and live-mask semantics. A legitimate v1 physical relocation keeps v1/144 bytes and no index; it must not flow through the new-logical-source V2 constructor. A v2 relocation stays v2/200 bytes and preserves its required vector index. Outer CommitParticipant/PhysicalRef versions and the root descriptor stay 1 in both cases. Index binding deliberately excludes native record locations, so no re-quantization/rebuild is needed. Every source-manifest literal/encoder/decoder path must carry the explicit version and optional field; presence-based version selection, silent v1-to-v2 upgrade and default index omission are forbidden. Replay's relocation comparator retains its full original checks and also proves source-version/index identity/payload preservation. Do not relax mutation validation to accommodate a physical rewrite.

For tracing, root composes the new index/captured-catalog descendant states with ZE-46's bounded cursor, budgets and protected-root traversal. Neither side may replace the other's cases or reintroduce O(all-source) retained allocations. If ZE-46 has appended block tags meanwhile, root assigns the next unused tag and updates this literal encoding plus its golden tests consistently before merging; no tag reuse. Root runs the affected focused case/compile after reconciliation. ZE-158 closure requires this actual integrated trace/registration compile, not a baseline-only build.

## First useful native RED -> GREEN

First implement the named test **native_vector_index_single_write_real_kernels** against ordinary create_native_graph/apply_native_graph/with_native_read. Write **one real vector node**, open its actual vector SparseSource and assert an actual indexed source, literal full ID/revision/original-f32 row, and usable GraphNodeBlocks. Current production has only a raw source: the initial additive reader exposes that actual absence, so the test's required-index assertion gives a behavioral RED. Do not generate synthetic blocks, fake a source or return a fabricated error to force RED.

Implement only enough of the shared bounded quantizer/build/codec/sparse path and checked small-source seed fix to publish that genuine one-row artifact and invoke the real existing Bit4 scan and GraphSearcher kernels from its admitted reader. Assert literal expected one-row distance/identity; instrument actual source/index reads and ensure no index build occurs at query time. Obtain GREEN and report exact first RED, first compile and useful GREEN to root before remaining groups. One-row Vamana is real but does not establish nontrivial traversal; group 2 supplies that proof.

## Exactly eight native acceptance groups

All native fixtures use actual create/apply/admission and real files/reopen; write receipts and independent literal calculations are the oracle. Keep subcases within these names and ordinary heap-backed test stacks. Existing helper techniques may be reused from committed source without editing moving fixtures.

1. **native_vector_index_single_write_real_kernels**: first milestone, then 2/3/4 row sources; literal original bits/codes/factors where independently known, min(4,N) distinct real flagged seeds, deduplicated traversal work and no fabricated rows. Same tiny result before/after native close/reopen.
2. **native_vector_index_small_writes_and_source_isolation**: grow a store exclusively through separate one-vector writes and prove every new source has a usable genuine graph. Add one nontrivial >=32-row source and run real filtered GraphSearcher with a strict source-local mask; observe positive non-seed traversal and no ineligible row. Update one node and delete another; only new/affected sparse state changes, old index bytes remain unchanged, old admitted reader stays valid, fresh row/version mapping excludes stale hits. Assert exact read/build receipt counts so an all-store rebuild or query-time build fails.
3. **native_vector_index_identity_space_and_geometry**: full IDs sharing low64 bits via existing nonshipping allocator-seed setup; exact row order/revision and source catalogue interpretation; dimensions/normalization/profile/metric; repaired-envelope malformed version/section extent/seed flag/neighbor/codes-factor disagreement or swapped identity rejected. Include an existing-format v1 source's physical-only row-table rewrite: decode retains V1, rewrite/encode stays 144-byte inner v1 with no index, and outer/root versions stay 1. Pair with a genuine v2 physical rewrite preserving its index, and assert a newly prepared logical Vector selecting V2 cannot encode without an index or fall back to V1. Root reruns this same subcase against the integrated ZE-46 relocation path; no ninth group. Original coordinate bits preserved, including signed zero. No-space store has no vector index; real invalid profile/unit-norm inputs fail with existing causes, never silent normalization.
4. **native_vector_index_quantizer_builder_reuse**: small deterministic cohorts exercise actual controlled Bit4/Vamana leaves and compare frozen codes/factor/graph bytes with the existing same-algorithm legacy fixture for identical input/profile/seed (a reuse regression, not the only ranking oracle). Literal squared-L2 calculations independently verify the admitted kernel's rescore rows. Nontrivial graph neighbor uniqueness/range/connectivity checked without claiming ANN exact top-k/recall.
5. **native_vector_index_prepare_limits_controls_release**: exact preparation memory and work rejection after real quantizer/build work; cancellation/deadline/real-close at controlled internal medoid/quantizer/build/encode points. Observe each schedule firing, original native generation/membership unchanged, no published partial index, and actual reservation baseline restored after all private owners drop. Paired clean differs only by schedule. Include actual buffer overlap/capacity, not estimated/faked counters. No arbitrary budget tuning loops.
6. **native_vector_index_publication_reopen_required_refs**: real native apply then public native reopen, old retained admission through a later publication, WAL replay and explicit checkpoint/reopen preserve rows/index bytes/space. Directed existing VFS fault before commit proves private failure/no logical change; required index chunk or interpretation object missing/corrupt rejects reopen rather than rebuilding. The test proves this participant's required-reference behavior, not full ZE-40 recovery qualification.
7. **native_vector_index_trace_complete_and_owner**: actual SearchTraceCursor includes index root, every extent/chunk, interpretation catalog descendants and unchanged source/record descendants with <=256 returned refs and explicit end. Foreign runtime/view/memory is refused; cancellation and late missing descendant leave no successful completed trace and release actual charges. Shared fixture supports root's later ZE-46 physical-relocation integration check without changing its logical semantics.
8. **native_vector_index_directed_probe_can_fire**: use these same real source helpers for exact case receipts and independent source/identity/value oracle; deliberately remove one required observed ref or corrupt an observed row/seed receipt in the comparator, observe rejection, restore it and run the same-seed clean. Every fire/release/copy/traversal value comes from observed work; no hardcoded success counts or normal scan counters masquerading as ANN work.

Native test kernel invocations prove actual produced artifact usability. They are not the complete ZE-62 ranker: do not introduce its routing, global candidate windows/reports, hybrid, public procedures or constrained-ranking API. Retain ANN traversal/candidate coverage facts; an exact rescore value does not upgrade approximation. Search-loop control/integration acceptance remains ZE-62; native index open/validation/build/trace controls are fully exercised here.

## Necessary shared-kernel regressions and compile controls

Add exactly **one** shared seed regression, **graph_search_small_persisted_seed_sets**, covering N=1/2/3, N>=4 exact-four validation, missing/extra flags and repeated adapter entries counting each row once. Run that test plus the existing **an_unfiltered_traversal_is_byte_identical_with_the_mask_parameter_absent** after changing discovery.

For shared quantizer/build/codec extraction, run only **bit4_golden_fixture_is_stable**, **vamana_build_is_deterministic_under_seed**, **graph_build_handles_the_minimum_shape_and_rejects_an_empty_segment**, and the named native reuse case. If an existing compiler/byte regression fails, resolve it before more native branches. No whole graph/legacy suite.

First test command:

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_vector_index_single_write_real_kernels)'
```

Final native run expects **8** tests. The shared regression run expects **5** tests (four existing plus the new seed regression):

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_vector_index_)'
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(bit4_golden_fixture_is_stable) | test(vamana_build_is_deterministic_under_seed) | test(graph_build_handles_the_minimum_shape_and_rejects_an_empty_segment) | test(graph_search_small_persisted_seed_sets) | test(an_unfiltered_traversal_is_byte_identical_with_the_mask_parameter_absent)'
cargo check -p zeppelin-embed --lib -j 4
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,test-support
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,allocation-audit,query-timing
cargo check -p zeppelin-embed-cypher --lib -j 4
cargo check -p zeppelin-embed-ffi --lib -j 4
cargo check -p zeppelin-embed-ffi --lib -j 4 --features graph-cypher
cargo clippy -p zeppelin-embed --lib -j 4 --features graph-cypher
```

Use scoped rustfmt, git diff --check and exact owned-path/preservation verification. No workspace formatting, release/size/coverage/fuzz/benchmark/full/adversarial execution or dependency changes. Two failed fixes or 20 minutes without a concrete milestone require a precise report before further attempts. Commit body records literal RED/GREEN and one individual allowlisted commit; no pushes.

## Actual registered consumer and closure

The compiled native consumers are prepare_sparse -> existing native apply/publication and SparseSource::vector_index -> actual existing scan/GraphSearcher kernels in the focused helper, plus actual checkpoint/replay/trace readers. An unused index type or a hand-encoded test artifact does not satisfy this component.

Root re-exports only shared native index probe/report through the existing graph-cypher+test-support tooling module convention, adds the new wrapper module and one directed probe call, and registers exactly **8** keys:

```text
property-graph.native-vector-index.kernel
property-graph.native-vector-index.small-writes
property-graph.native-vector-index.identity
property-graph.native-vector-index.limit.fire
property-graph.native-vector-index.control.fire
property-graph.native-vector-index.reopen
property-graph.native-vector-index.trace
property-graph.native-vector-index.oracle.can-fire
```

The wrapper verifies exact inventory, no duplicate/missing receipt, real relevant work and measured release/paired clean where applicable. Root registers after executor commit independent of the other worker completion dates, resolves additive overlaps preserving both, and compiles the **actual** registered consumer:

```sh
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4 --features graph-cypher
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4 --features graph-result-test-support
```

No broad runner execution. Closure requires genuine native index publication/read/reopen/trace evidence, actual shared-kernel usability including small writes, eight focused native groups plus the finite shared regressions GREEN, source/preservation checks and root's registered integration compile. ZE-62/63/64 retain complete routing/constrained/hybrid/operator acceptance; ZE-46 retains relocation/reclamation proof, ZE-40 original recovery qualification, and ZE-118 broad qualification. This plan changes none of those gates.
