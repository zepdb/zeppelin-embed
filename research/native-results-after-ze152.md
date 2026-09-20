# ZE-155 native read-result readiness

Decision: **GO for one complete private native read-result materializer**, subject to root's component approval. Audited main is `e13c48c6d7cbf1dafb0704888c191d6582830046`; its production baseline is `f08898e6d34c3983ab07dc085567481e2ae81c18`. No unfinished ZE-46, ZE-154 or ZE-52 interface is required. This is a source decision, not execution or qualification evidence.

The missing work is substantive: translate the real driver's complete native rows and admitted entity records into ZE-127's existing typed `ResultSource`, copy with `PreparedGraphResult::copy_from`, and detach the existing owned representation after successful final driver checks. The component owns that translation, its charged staging, its real native consumer and focused evidence. It does not introduce another row collector, result format or C converter.

## Committed seams that make it possible

Paths below are under `crates/zeppelin-embed/src/property_graph/` unless otherwise stated.

| Existing source | Usable contract |
| --- | --- |
| `query/pattern.rs`, `NativePattern::new` | Complete private native scan/key/expand/path/join/optional/scalar producer already compiled by ZE-152. The component tests use these committed operators; relational barriers are unnecessary. |
| `lifecycle/native_graph.rs`, `Store::with_native_read` | Real retained admission, catalog, view, runtime and memory authority; final lifecycle check remains in force. |
| `query/runtime/driver.rs:100,126,308` | `PreparedRows`, lifetime-independent `Completion::Output`, typed `execute_in`; all rows stay private, completion happens under admission, final counters/checks precede `Execution`. |
| `storage/view.rs:153,175` | Native live node/relationship lookup, verified record/topology and relationship endpoint liveness. |
| `storage/view/expression.rs:14,43` | Authentic view/runtime/memory ownership check and exact symbol-to-name reads. |
| `storage/records/native.rs:66` and `records/provenance.rs` | Verified shape, revision, original generation, exact key and canonical scalar/list payloads. |
| `query/completed.rs:68,135,249,347` | Fixed typed `Pools`, `ResultSource`, validated charged copy, allocation-free post-driver detach. Existing completed storage already owns every supported scalar/entity/list representation. |
| `crates/zeppelin-embed-ffi/src/graph_result/conversion.rs:242,293` | ZE-141 consumes this same `ResultSource`, calls ZE-127, and converts/finalizes the existing C representation. Keep it unchanged and compile its real consumer. Do not duplicate it. |

One small production seam is absent: enumeration of the verified native property index. `RecordView::property` only accepts a known key. A single checked `property_at(index, resources)` can reuse the existing private `property_row`, `PropertyKeyId` validation and canonical `PayloadSlice::subslice`. It needs no new codec, storage format, recovery operation or view interface. Root has approved this isolated additive method and will coordinate its hunk with ZE-46; **the whole file is not reserved**.

The driver requires completion output independent of its fresh row/view lifetime. `PreparedGraphResult<'m,'g>` already meets this requirement while retaining genuine reservations. Validate native owner identity when constructing the completion, retain its authentic runtime/memory/view identities, and check those identities during completion. Native tree methods consume the existing `TreeResources`; no cast or manufactured admission is needed.

`NativeExecutionError` does not contain `CompletedError`. Preserve it unchanged for ZE-154: a local completed/native error sum and concrete forwarding adapter around `NativePattern` let the existing generic driver transport both typed families. No error payload needs flattening or shared runtime edit.

## Exact completeness boundary

The result preserves rows/columns/bag multiplicity, nested query lists and bit-exact supported scalars. Full-ID-sorted unique entity payload pools preserve every repeated cell/list reference. Native records supply labels/type, scalar/typed-list properties, keys, revision, record's original generation and directed endpoints. Catalog symbol order must be translated into the completed format's UTF-8 name order. Default entity copies omit stored text/vector payloads; existing explicit stored-text scalar expressions are copied normally. Endpoints do not trigger recursive neighboring-node copying.

Prepared rows, native staging and ZE-127's destination overlap in real query accounting. The prepared destination remains charged through the driver's final byte charges/checkpoint. Only successful `Execution` supplies the counters/peak used by `detach`. Failure drops every temporary owner and yields no completed result. A subsequent `with_native_read` lifecycle failure drops its local owned return before exposing it.

Base-view lookup absence is a typed `SourceError::Missing(EntityId)`; actual storage errors retain their `TreeError` payload. An immutable base view cannot establish ZE-52's pending-overlay deletion cause, so this component makes no such claim. It does not fabricate write outcomes, receipts, eager search reports or vector projection APIs.

The paired execution plan is `/tmp/ze-155-native-result-execution-plan.md`. Original ZE-53 retains full read/write/search/public composition and original acceptance; ZE-52 retains overlay/write semantics, ZE-64 retrieval/report integration, ZE-66/68/69 their compiled/public/binding qualification. ZE-46 compaction/reopen and ZE-118 broad qualification are unchanged. Native-to-C public composition is not established merely by compiling ZE-141.

No builds, tests, product edits, tracker changes or worker-source reads were performed for this audit. No remaining producer blocker was found within this boundary.
