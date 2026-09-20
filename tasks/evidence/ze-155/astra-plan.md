# ZE-155: executable private native read-result component

Status: proposed finite plan for root approval, then Sol/xhigh execution. Baseline main: `e13c48c6d7cbf1dafb0704888c191d6582830046` (production `f08898e6d34c3983ab07dc085567481e2ae81c18`). Do not consume uncommitted ZE-46/154 source. Read the claimed implementation ticket, AGENTS.md and this entire plan before work. Root supplies the flat implementation ticket/worktree and preservation hashes; retain the existing dirty files and symlinks. This plan neither starts blocked ZE-53 nor changes original dependencies or acceptance.

## Deliverable and exact ownership

Implement one complete crate-private native read completion: actual admitted `NativePattern` -> existing `execute_in`/`PreparedRows` -> resolved native `ResultSource` -> existing ZE-127 `PreparedGraphResult` -> final driver snapshot -> existing `PreparedGraphResult::detach` ownership transfer into `CompletedGraphResult`. No new row collector, generic result framework, persisted encoding, public facade, C converter or dependency.

Executor's product allowlist:

- `crates/zeppelin-embed/src/property_graph/query/completed.rs`: only the graph-cypher-gated crate-private native module declaration; existing owned-storage APIs/validation remain unchanged.
- `crates/zeppelin-embed/src/property_graph/query/completed/native.rs`: completion, local typed error/NativePattern forwarding adapter, charged native staging owner and real execution/finalization entry.
- `crates/zeppelin-embed/src/property_graph/query/completed/native/values.rs`: checked geometry and query/stored scalar/list translation.
- `crates/zeppelin-embed/src/property_graph/query/completed/native/entities.rs`: full-ID discovery/indexing, native record/catalog/key translation and exact name ordering.
- `crates/zeppelin-embed/src/property_graph/query/completed/native/tests.rs` and `test_support.rs`: exactly the eight acceptance groups below and shared actual-native helpers/receipts.
- `crates/zeppelin-embed/src/property_graph/storage/records/native.rs`: **only one additive checked property-at-index method**, coordinated by root with ZE-46. Do not claim the whole file, change existing decoders or touch recovery/lifecycle.
- `tests/adversarial/graph_native_result.rs`: new thin directed wrapper calling the same actual-native helper; root registers it after executor commit.
- `tasks/evidence/<implementation-ticket>-native-result.md`: exact RED/GREEN, commands, counts, observed counters, reservations and limitations.

Root owns additive `query/mod.rs`, adversarial `mod.rs`, `runner.rs` and `coverage.rs` registration. No executor edits to `pattern.rs`, expression/runtime/relational files, `storage/view*`, lifecycle, FFI, compiler, manifests, graph plans or tracker. Narrow changes outside this allowlist require root review; do not redesign around a compile error.

## Immutable producer and error contracts

Use the committed `Store::create_native_graph`, `apply_native_graph`, `with_native_read`, real retained `RuntimePlan`, `NativePattern::new`, existing native evaluator and `execute_in`. Output column descriptors are explicit private input names plus ordered root slots; validate them against actual root facts and infer allowed kinds from those facts. Copy column names into charged ownership. Do not import compiler `ReadColumn` into core, infer columns from sample rows or silently reorder/dismiss a slot. Zero rows still retain correct metadata, and zero-column row cardinality remains distinct from no rows.

Add a local error sum retaining `NativeExecutionError` and `CompletedError` by value. `From<RuntimeError>` maps to the former's Runtime variant. A concrete wrapper forwards NativePattern node/prepare_search/pull unchanged, mapping only the error sum. Do not add a shared runtime error variant or replace execute_in. Storage lookup/decoder failures remain the original `TreeError`, including source and control causes; upstream expression errors retain ExprId. Missing resolved identity is `CompletedError::Source(SourceError::Missing(id))`. Never convert storage failure to null/empty entity or unit `Storage` when a typed payload exists. Pending-overlay `Deleted` belongs to ZE-52/53.

The completion constructor validates `GraphReadView::validate_expression_owner` with its authentic lease runtime. Retain that runtime identity and actual QueryView/QueryMemory references. At every complete call checkpoint and compare all three actual identities before native reads; matching generation/store numbers are insufficient. This handles Completion's fresh row lifetime without unsafe coercions or a new storage-view method. Validate each QueryValue through the existing value context; list traversal must consume all declared positions or fail.

## Native staging and translation

Build one private, immutable-at-publication `ResultSource` staging owner using the existing twelve typed pool shapes. It holds actual QueryArena backing and its control reservation; reports/receipts/vectors are empty for this read-only component. `result_input()` only borrows a complete initialized description bound to the actual view and `Outcome::Read`; it performs no deferred I/O. The owner exists because ZE-127 requires resolved retained input; it is not a second collector. The existing driver alone owns and drains intermediate rows.

Use checked counting followed by exact bounded allocation/fill. Count passes and fill passes both perform and account their actual work; immutable native reads may be repeated without claiming a single lookup. QueryArena reserves actual capacity plus controls before allocation. Account ID indexes, geometry/descriptors, bounded recursion/work stack and sort scratch as well as all staging pools; no uncharged Vec/HashMap/String scratch or speculative max-sized stack arrays. A geometry overflow/limit fails before allocating the prohibited representation. Controls run at least every 256 scalar/index/name units and every bounded byte chunk (at most 64 KiB).

Discover node/relationship references recursively in actual result values. Use separate charged full-u128 ID indexes; compare the complete ID and domain. Deduplicate entity payloads only. Preserve every row, repeated cell and list occurrence. Sort final node/relationship pools by full IDs and resolve value indices through the checked indexes. Do not narrow IDs or recursively load endpoint nodes that were never returned. Use a bounded cancellable sort/index implementation with charged scratch; no uninterruptible library sort over an unbounded collection. Charge actual copied descriptors/bytes and use existing cumulative value work for element/comparison traversal; do not fabricate operator/lookup counters from estimates.

For each unique ID call the actual same-view live lookup. Read shape, revision and `provenance().original_generation()`, not admission generation as last-change generation. Preserve exact optional stored key kind/namespace/key, including embedded NUL and present-empty distinctions permitted by existing validation. Resolve native labels/property keys/type through the admitted catalog. A missing required symbol is a typed failure. The completed format requires labels and properties in strict UTF-8 byte-name order; native symbol ID order is not that order. Sort the descriptor spans accordingly while retaining exact values.

The single additive `RecordView::property_at(index, resources)` checks index against `canonical().property_count`, decodes the existing `property_row`, checks `PropertyKeyId`, and returns its key plus `canonical_bytes.subslice(offset,length)`. Reuse existing geometry/codecs; no new module just for this method, full-record parser, canonical re-encoding or storage-format change. Its in-range/end-bound behavior is exercised through the native entity case.

Translate verified property payloads with existing `PayloadCursor`/`PayloadSlice` readers. Preserve existing tags exactly: string, bool, i64, f64 bits, canonical untyped empty list, string/bool/i64/f64 homogeneous lists. These become their existing `ListKind` variants; query lists always use `ListKind::Query`, including mixed/nested/null/entity elements. Preserve all floating bits supported by the producer (including signed zero and nonfinite payloads); do not perform arithmetic or normalize bits. Query values and properties share output Value/Span geometry, not a copied evaluator/validator. Finish each encoded property cursor; no trailing-byte tolerance.

Emit postorder values: every list child's value index precedes its parent, while each parent has its own contiguous ordered child-index span. Checked private placeholder slots may be filled before exposing the completed staging description; no uninitialized or partially filled description reaches ZE-127. Preserve null versus empty string/list. Default Node.text/vector are None; explicit stored-text expressions already produce ordinary String/Null cells. Original directed relationship endpoints remain unchanged for IN/undirected traversal.

## Simultaneous ownership and finalization

At native translation time the driver retains its batch/private PreparedRows, pattern/plan/admission owners remain alive, and staging plus indexing/sort scratch are genuinely charged to the SAME QueryMemory/store aggregate. During `PreparedGraphResult::copy_from`, native staging and the copied destination coexist with private rows. Keep all authentic backing charges until those allocations are freed; never drop a reservation early or substitute represented bytes for capacity. ZE-127 performs validation and destination copying once; reuse it unchanged.

Drop staging and dead scratch only after copy_from succeeds. Return `FrozenOutput<PreparedGraphResult>` with exact `represented_bytes`, actual row count and ABI bytes zero. The prepared destination remains charged through execute_in's CompletedBytes charge and final checkpoint. The private execution entry consumes a successful `Execution`, then invokes existing `detach(execution.counters, execution.peak_query_bytes)`. It must not detach in Completion or create a result on RuntimeFailure. The existing outer with_native_read checkpoint remains authoritative; failure there drops the local detached result before caller exposure.

Report copied/staged bytes at real sites and let native tree operations report lookups/scans. Completion must not increment CompletedRows/CompletedBytes itself: the driver is their sole authority. Counters/peak in the final owned result come from successful Execution, not a pre-completion snapshot. Post-detach allocations are intentionally application-owned; their construction overlap remains in the recorded peak.

## First real-source RED -> GREEN

First add **native_result_actual_rows_survive_close**. Publish a tiny actual graph, admit a validated native scan/project returning a node and scalar, run the real private native consumer with batch_rows=1, then close/drop the store and assert the independent owned result's literal cells, native identity and column metadata. Expected values come from the write request/receipt, not a decoder or another execution strategy.

Introduce only the real completion scaffold needed to compile this test. Before row/entity translation, its authentic native staging contains no fabricated rows/records; ZE-127's existing shape validation rejects the absent cell/value representation for the driver's actual nonempty rows. Observe that behavioral rejection against the expected successful owned result. Then implement the smallest real scan/node/scalar translation and post-driver detach for GREEN. Do not use a stand-in PullOperator, fake ResultSource fixture, synthetic error injector or panic to manufacture RED. Record the exact rejection and first useful GREEN, and report it to root before remaining branches. No scaffold rejection survives the implementation.

## Exactly eight focused acceptance groups

All successful rows originate in the committed actual NativePattern pipeline under native admission. All control failures are directed at real work. Keep subcases within these eight test names; no new suite or broad matrix. Use heap-backed fixtures and ordinary test stacks.

1. **native_result_actual_rows_survive_close**: first milestone; then zero rows/metadata, null and empty values, bag duplicate rows, zero-column row cardinality if permitted by the existing validated plan, explicit stored-text String/Null. Returned storage remains readable after original parameters/plan/store are dropped.
2. **native_result_records_properties_and_provenance**: returned node and relationship with keys, labels/type, all stored scalar/list tags (including Empty versus typed empty), name order deliberately different from symbol insertion order, directed endpoints, revision/original generation after a later unrelated publication. Default text/vector absent. Exact property accessor range behavior, UTF-8/NUL and literal metadata oracle. Base lookup of an actually missing/deleted ID tests the resolver's exact Missing identity separately; do not call this an overlay-deleted row or forge such a NativePattern output.
3. **native_result_lists_bits_and_entity_identity**: native projection/list expressions and parameters provide every supported query value/nested mixed list; exact numeric bit patterns and boundary integers; repeated node/relationship cells and nested references map to one sorted payload record per full ID while all multiplicities remain. Include IDs differing in high bits from real committed native allocation fixtures; never manufacture entity rows. Assert postorder/list order and independent expected pools/cells.
4. **native_result_same_view_and_owner_rejection**: publish a changed revision after the old real admission and before its materialization using a deterministic barrier; old result copies old data/provenance and a fresh admission sees new data. Separately use two real admissions to reject the wrong runtime/view/memory owner before copy. Do not substitute a same-generation token or run only synchronous publication before old admission.
5. **native_result_limits_and_no_partial_output**: actual query-memory rejection while staging and destination overlap; represented core-byte cap reached by native entity properties while private row references still fit; real CopiedBytes limit after materialization has consumed positive work; a driver CompletedBytes limit that fires after preparation while destination reservations are still live. All failures return no CompletedGraphResult, retain exact cause/counters and restore the measured pre-component reservation baseline after temporary owners drop. Construct each precise pressure once; no budget sweep or oversized literal fixture.
6. **native_result_controls_and_late_errors_release**: cancellation, deadline and real store-close after observed materialization work, using a bounded test-only stage receipt/synchronization point and actual control/lifecycle action. Assert the action fired, exact close-first cause, no output, positive relevant copied/native work and measured release. Pair the identical fixture with only the schedule disabled. A later actual native source-read failure through the real producer preserves TreeError and prior private work; reuse the committed delegated-VFS fixture technique, not a fake operator/error return. A later genuine scalar arithmetic failure retains ExprId. These last producer errors prove the attached result path exposes no prior private rows, not that native completion performed unavailable late I/O.
7. **native_result_final_counters_and_owned_copy**: assert the existing prepared owner remains charged at the final driver boundary; final result's all existing counters/peak equal actual successful Execution, including final CompletedBytes and zero CompletedAbiBytes. Observe real row/staging/destination overlap and measured baseline release after detach/failure. Repeat a small successful real materialization/drop loop to verify ownership release; reuse ZE-127 validation/detach, not a second copy implementation. Compile unchanged ZE-141 consumer as described below; do not call that native-to-C runtime qualification.
8. **native_result_directed_probe_can_fire**: call the same shared actual-native helpers; exact receipt inventory, independent value/metadata oracle, measured counters/release and directed cause. Deliberately perturb an observed cell/metadata or suppress a receipt in the verifier and see rejection, restore it, then paired same-seed clean. No hardcoded release/same-view/fire=1 and no scan counters standing in for materialization evidence.

Control hooks, if needed, are cfg(test/test-support)-only observation/scheduling points around actual work. They may trigger the real QueryControl/clock/lifecycle operation but cannot replace source reads or production errors. No sleeping race. Necessary source failures must name the actual failed artifact/read and preserved cause.

## Consumer and compile gates

The real compiled new consumer is the private function that builds/uses the native completion with `execute_in` and detaches after Execution; the focused tests invoke it. ZE-127 is the only owned storage/copy implementation. ZE-141's existing `prepare_native` already consumes the identical ResultSource/Pools contract and calls ZE-127; do not add another C translation path or modify its visibility. Compile that unchanged consumer with graph-result-test-support. This does not claim public native-to-C composition, which remains original ZE-53/68/69 work.

First RED/GREEN:

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_result_actual_rows_survive_close)'
```

Final focused run expects exactly **8 new tests**. Run the existing owned-storage integration target once because the new component relies directly on its pool/charge/transfer contract; no other broad regression target is authorized.

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_result_)'
cargo nextest run -p zeppelin-embed --test graph_completed_results --features graph-cypher -j 4 --retries 0
cargo check -p zeppelin-embed --lib -j 4
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,test-support
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,allocation-audit,query-timing
cargo check -p zeppelin-embed-cypher --lib -j 4
cargo check -p zeppelin-embed-ffi --lib -j 4
cargo check -p zeppelin-embed-ffi --lib -j 4 --features graph-cypher
cargo check -p zeppelin-embed-ffi --lib -j 4 --features graph-result-test-support
cargo clippy -p zeppelin-embed --lib -j 4 --features graph-cypher
```

Use scoped rustfmt on owned files, git diff --check and preservation/allowlist verification. No workspace formatter, broad tests/adversarial execution, coverage, fuzz, benchmark, release or dependency changes. Record exact commands/counts, intended failures, terminal GREEN and restored deliberate controls. Fix new warnings/panic-policy violations; qualify pre-existing warnings. Repeat only after relevant source changes/failures.

## Root registration and completion

Expose only shared report/run_actual_probe through root's graph-cypher+test-support-gated `query::native_result_test_support`, re-exported from completed/native/test_support. Executor supplies the new `tests/adversarial/graph_native_result.rs` wrapper. Root adds its gated module, one probe call beside graph_pattern, and exactly these **8** coverage keys:

```text
property-graph.native-result.copy
property-graph.native-result.identity
property-graph.native-result.same-view
property-graph.native-result.limit.fire
property-graph.native-result.control.fire
property-graph.native-result.late-error.fire
property-graph.native-result.release
property-graph.native-result.oracle.can-fire
```

Each key maps to its own actual shared case receipt; wrapper rejects missing/duplicate/unexpected keys, checks relevant positive work and verifies the clean paired receipt where appropriate. Root integrates registrations after executor commit regardless of ZE-46/154 completion, preserving their additive changes when resolving conflicts. Root coordinates the one storage accessor hunk with ZE-46. No artificial worker-completion dependency, but closure requires root to compile the ACTUAL registered probe:

```sh
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4 --features graph-cypher
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4 --features graph-result-test-support
```

No adversarial suite execution. An unregistered baseline compile is not this gate.

Proposed flat implementation title: **Materialize native read results into owned completed storage**. Root records exact dependency edges to the approved plan and committed ZE-127/141/152/native admission/runtime producers. Acceptance is all eight groups, the narrow owned-storage target and compile gates, real directed receipts/registered consumer, exact RED/GREEN evidence, charge/typed-error/lifetime correctness, one allowlisted implementation commit and preservation hashes. Original ZE-53/52/64/66/68/69 retain all write/search/public/oracle/binding requirements; ZE-46 compaction/reopen and ZE-118 broad qualification remain unchanged. No original ticket is closed by this component alone.
