# Native pattern execution against integrated producers

Proposed executable handoff for one GPT-5.6-Sol/xhigh executor. Planner: GPT-6-Astra/xhigh, ZE-151. Source pin: `6f37c4180e8a4785b882f50d3cfaa196edaa06b2`. Root must accept the flat implementation ticket and this plan before implementation. This document is source inspection, not build/test evidence.

## Assignment and complete component

Implement one crate-private native pattern-region executor over the existing validated typed plan. It implements actual Unit, ScanNodes, LookupNode, LookupRelationship, LookupKey, Expand, BoundedExpand, Join and OptionalApply production, with the scalar Filter/Project/With and Collect pass-through adapters needed to execute complete pattern regions. It consumes the existing native read view, scalar evaluator and single typed pull driver. Implement both join algorithms and a conservative selective-start/build-side chooser within legal inner-pattern regions. No new public facade or Cypher entry point is exposed.

The component is a complete reusable physical pattern producer; it is not a generic row kernel, test source, alternative admission path, or claim of complete graph-plan execution. Explicitly reject unsupported non-pattern operations before pulling; Aggregate/Distinct/Sort/OffsetLimit integration, search execution, mutation, public completion and result conversion remain their original owners. Do not return successful empty rows for an unsupported node. Do not advertise the private supported region as full ZE-50/51/public execution.

Read AGENTS.md, `/tmp/graph-sol-executor-rules.md`, and canonical main `docs/graph/plans/{execution,parallel-contracts}.md`. Use a fresh root-assigned worktree from the source pin or root's integrated refresh. The user authorized safe parallel implementation; root alone approves the scheduling split and changes the tracker/specs. Preserve every inherited hash and symlink. One ticket, no agents, no dependencies, no public ABI, no lifecycle/recovery change, no push or integration/closure by executor.

No workspace/full/adversarial campaigns, benchmarks, release builds, coverage/fuzz/soak runs. Named nextest only, `-j 4 --retries 0`, then the required compile controls. ZE-118 retains broad execution. Report first behavioral RED, first compile and first useful GREEN promptly. Two unsuccessful fixes of one issue or 20 minutes without an acceptance milestone require a precise root blocker report; do not change budgets or invent a new producer.

## Exact ownership

Executor may change only:

- `crates/zeppelin-embed/src/property_graph/query/mod.rs`: private pattern declaration and the guarded test-support export below.
- `crates/zeppelin-embed/src/property_graph/query/pattern.rs` (new): component construction, bounded physical state, schemas, scalar adapters and typed pull dispatch.
- `crates/zeppelin-embed/src/property_graph/query/pattern/{source,expand,join,planner}.rs` (new): source/key adapter, fixed/bounded DFS, joins/optional, conservative physical choices.
- `crates/zeppelin-embed/src/property_graph/storage/view/expression.rs`: one read-only `GraphReadView::lookup_application_key` method using the existing fence/tree/record producers. Preserve every current method.
- `crates/zeppelin-embed/src/property_graph/query/pattern/{tests,test_support}.rs` (new): actual published-store fixtures, independent primitive oracle, directed controls and hook report.
- `tests/adversarial/graph_pattern.rs` (new): adapter over the real directed hook and independent observations.
- `tasks/evidence/ze-N/README.md`, `tasks/evidence/ze-N/astra-plan.md` (exact accepted copy), and bounded logs, where N is the root-assigned implementation key.

No `native_graph`/lifecycle, `storage/view.rs`, mapping, VFS, WAL, sparse checkpoint, storage format, evaluator, runtime/driver, existing relational kernels, typed IR, compiler, Cargo, core `lib.rs`, or shared runner registration edits. The helper in existing `storage/view/expression.rs` is explicitly accepted by root because it is disjoint from ZE-40. Stop before expanding this allowlist. Root owns the exact additive runner registration on main after integrating the pattern executor commit, whether ZE-40 has finished or not; component closure waits for that registration and compilation. If ZE-40 later changes shared registration, root resolves the additive conflict preserving both probes and coverage keys. This component has no ZE-40 completion dependency.

## Frozen inputs and new private seam

These are compiled inputs at the pin, not proposed APIs:

1. `Store::create_native_graph(path, OpenOptions, Option<EmbeddingTower>)`, `apply_native_graph(&[StructuredWrite], &QueryControl)` and `with_native_read(control, RuntimeLimits, memory_limit, source_slots, consumer)` are the actual publication/read path. See `lifecycle/native_graph/persistence.rs:741`, `write.rs:934`, `native_graph.rs:1416`.
2. `NativeReadConsumer<T>::consume(view: &GraphReadView<'s,'lease,'m,'g>, runtime: &mut RuntimeContext<'lease,'m,'g>) -> Result<T, TreeError>` (`native_graph.rs:136`). Run production pattern code inside this actual callback. For test/composition boundaries use `T = Result<Execution<OwnedObservation>, RuntimeFailure<NativeExecutionError>>`: `Ok(execute_in(...))` preserves the inner typed result; the outer actual admission/final-check failure remains `NativeGraphError`. Never stringify/erase the inner error into TreeError, and never mistake outer `Ok(inner Err)` for query success. No lifecycle generic-error redesign is required.
3. `GraphReadView::{node_cursor, scan_nodes, expansion_cursor, expand, lookup_node, lookup_relationship}` and `CursorState::{More,Done}` (`storage/view.rs:143-338`). Native cursors bind lease token, runtime identity and QueryMemory and own their charged selection storage. `RelationshipTypeSelection::{All,Any}` and `LabelSelection::{All,AllOf}` take actual numeric catalog IDs. Empty requested alternatives mean All; nonempty alternatives whose names all miss mean Any(empty), never All.
4. `GraphReadView::{validate_expression_owner, expression_symbol, expression_symbol_name}` (`storage/view/expression.rs:8-44`) resolves exact names against that same catalog. `NativeExpressionEvaluator::new` and `evaluate` (`query/expression.rs:519,570`) already consume real `RuntimePlan`, `Schema`, RowBatch and that native view. Evaluation returns scratch-borrowed QueryValue and exact `ExpressionError { expression, failure }`.
5. `execute_in(context, runtime_plan, source, completion, capacity)` (`runtime/driver.rs:313`) drains the existing `PullOperator<..., NativeExecutionError>` and privately completes with a final close-first check. The existing error union preserves Runtime/Expression/Plan/Tree by value (`runtime/native_error.rs`). Do not add a second drain or new error hierarchy.
6. `GraphPlan::facts`, `NodeFacts::{width,slot_at,slot}`, `Schema`, `RowBatch`, `Rows`, `QueryArena`, `QueryInputs` and `RuntimePlan` are existing owners. Rooting/fact backing/parameter backing must be authentically retained by `QueryInputs` in the same memory. Use the actual ordinal accessor; SlotId is never a column index.

New component constructors are private implementation APIs, not frozen before they compile. Keep one `NativePattern` operator borrowing the actual native view and RuntimePlan for its whole lifetime, retaining explicit bounded state and implementing `RowOperator<..., NativeExecutionError>` plus the existing `PullOperator`. Constructor receives validated region root, real parameter bindings, explicit capacities and the existing mutable RuntimeContext. Expose no callback that can manufacture a view/source, and no lifetime-free entity/cursor output. Use `execute_in` for complete region tests; original ZE-51 can compose the same RowOperator later.

Do not force this native view into `OperatorFactory`'s unrelated fresh-view construction. Here admission already supplied one context, and `execute_in` exists for that case. The native source/catalog/view must outlive the operator and all cursors; scalar scratch values are copied into charged RowBatch storage before evaluator reset. No `unsafe` lifetime extension, leaked allocation, detached cursor, or clone of a Store to fake ownership.

### The only storage helper

Add `lookup_application_key(&self, key: ApplicationKey<'_>, resources: &mut TreeResources<'_>) -> Result<Option<EntityId>, TreeError>` in the existing view/expression implementation. It validates the current lease/owner through the existing controlled read path, resolves `SymbolKind::Namespace` in the admitted catalog, constructs `FenceKey::new(kind, namespace, exact_text)`, calls `lookup_fence_entry` on this bundle's KeyFences root, and invokes `records::verify_fence_entry` with the same source/catalog/document. These leaves are already production code: `tree/directory/fence_input.rs:10-34` and `records/fence.rs:62-95`; the actual write-side consumer is `native_graph/base.rs:600-658`.

Absent namespace/key or a verified deletion fence returns None. A live fence must resolve its exact incarnation and matching live record/provenance/revision/key; inconsistency is corruption, not absence. Verify exact content/provenance with existing streamed fields/bytes as needed, not digest-only equality or a copied decoder. For relationship keys, first distinguish missing authoritative relationship corruption from legitimate visibility filtering, then apply the existing both-endpoint liveness rule; logical DETACH may make the relationship invisible without deleting its fence. Do not use a live key fence itself as endpoint-liveness authority. Node/relationship direct lookups retain their existing semantics. No writer admission, storage cache, prepared bundle, whole-graph key scan, new index or new source.

Freeze LookupKey's runtime behavior as follows. The existing `plan/validate.rs:403-427` accepts STRING|NULL and gives the output a nonnullable NODE or REL kind. Evaluate the key expression against its validated input; preserve any evaluator error unchanged. `QueryValue::Null` produces zero matches for that input row, with no null entity row and no storage lookup. A non-string, non-null value returns `NativeExecutionError::Expression(ExpressionError { expression: key, failure: ExpressionFailure::Runtime(RuntimeError::Value(QueryError::Type)) })`. A String is checked against both existing key bounds before lookup: checked namespace-byte-length plus key-byte-length must fit `MAX_GRAPH_INPUT_BYTES` (`ApplicationKey::new`, `names.rs:45-56`), and checked 9-byte fence prefix plus key-byte-length must fit the same bound (`FenceKey::new`, `tree/directory/fence_input.rs:21-34`). Overflow or an oversized key returns `NativeExecutionError::Expression(ExpressionError { expression: key, failure: ExpressionFailure::Plan(PlanError::Limit) })`; this preserves ExprId and uses the existing typed limit cause without adding an enum variant. Statically oversized literal application keys already fail validation with `PlanError::Limit`. Storage corruption/I/O failures remain the original TreeError. Names and keys remain exact UTF-8, with empty/NUL text and kind/namespace distinctions preserved; no null-to-string conversion.

## Representation, semantics and accounting

Retain bounded physical operator occurrences for the validated DAG; independent executions of a shared DAG node must not accidentally share cursor position. A correlated optional right anchor is a specific left PlanNodeId substituted by that exact preserved left row. It is not a same-named scan. Charge physical descriptors, occurrence stack, schemas, all row backing, retained build-side rows, join buckets/chains, path frames, selections, uniqueness metadata, expression scratch and parameter copies before allocation. Use QueryArena or an exact QueryReservation guarding fallible actual-capacity backing. Check overflow and actual capacities, including stack/fixed control state. Never reserve one allowance per operator.

Use one runtime/view/QueryMemory and cumulative work counters throughout. Fixed scheduling batches are at most 256; retained join storage uses the existing separately bounded storage representation. Failure to fit is typed failure, never truncated output. Keep simultaneous input/output/scalar scratch/old and replacement backing charged until genuinely released. Query24MiB, aggregate store256MiB and existing limits remain unchanged. No spill or all-graph materialization.

For each hidden relationship use, retain `(PatternId, logical traversal origin, full RelId)` alongside the row, including across projections/rebinding needed inside a region. A bound variable can disappear from public slots without erasing same-MATCH uniqueness. Combining branches rejects reuse from distinct origins in the same PatternId; the identical correlated/common binding is retained once, not treated as a second use. A fresh MATCH PatternId permits reuse. No bit truncation or identity inferred from a row ordinal. Keep hidden metadata private; it never becomes a public PATH value.

Fixed expansion consumes native `RelationshipRow` values and determines the neighbor from requested direction plus original source/target. Preserve parallel edges and one undirected self-loop. Storage already validates both endpoints and charges physical adjacency work; do not double-count it. Operator counters count their own actual rows, probes, emitted paths and copies. Label/type misses produce no matches while unknown plan slots remain errors.

Bounded expansion uses depth-first resumable frames of depth <=16 and a per-candidate used-RelId check; repeated nodes are permitted. It must retain the cursor and input row while yielding batches. At depth zero emit the start node and a real empty relationship list. `*1..1` still emits a list. Candidate-edge predicates use input plus the private current-edge slot before an edge enters the path. Completed-edge predicates use input plus the complete public relationship list and private edge slot; evaluate every member before emission, excluding the new target-node slot. False/null rejects only that candidate; errors abort. Zero-hop candidates evaluate neither predicate. Copy the retained list under the same budget, and retain it through the entire completed-edge check.

Inner join compares all shared slots using existing query equality: null does not join, numeric equality is exact, lists and full identity follow existing semantics. Hash buckets use existing compatible equivalence/hash operations only as an index and always check real join equality; hash equivalence alone cannot accept NaN/null. Preserve duplicate rows and evaluate residual predicates on the combined schema. Implement bounded hash join when a real bounded build side is chosen and resumable nested-loop otherwise; no silent memory-failure fallback that replays observable expressions after an error. Choose before execution or propagate allocation failure.

OptionalApply drains the entire right candidate pattern and attached predicate for each left row. Only zero surviving candidates emits one row with right-only slots null. Shared slots retain left values. Independent right DAGs use shared equality; correlated DAGs substitute the actual left anchor and do not rerun it. Reused variables use the compiled fresh-candidate equality/projection structure without overwriting old cells. Bags survive every branch.

The planner is deliberately small: rank existing exact lookup sources before label scans before unconstrained scans; choose bounded join build/probe direction and greedy legal connected extension using the existing typed patterns. Use only available concrete information; no fabricated label-count statistics. Do not discover an index from arbitrary property equality. A source/start permutation is allowed only where dependency, scalar-error evaluation, PatternId and bag semantics are preserved. Treat optional, WITH, projection scope, sort/group/limit, search and mutation boundaries as opaque. If a connected reorientation cannot be proved safe using the typed facts, retain its existing start and still execute it correctly. There is no requirement to rewrite every pattern or an exhaustive optimizer.

Poll before/after bounded native steps and at <=256 comparison/row/expression/probe units, <=64KiB copied byte spans. Charge actual work even for rejected/dead/duplicate candidates; native read counters come from existing TreeResources. `Paths` counts emitted paths only. Keep no-progress internal scan/DFS work internal: never return empty `PullState::More`, and never report Done merely because one native scan batch yielded no surviving rows. Close-first checks retain precedence. Every late error returns no completed output, and dropping the component releases all temporary owners.

## Ordered RED -> GREEN milestones

Use a new fixture under the pattern module that creates a temporary real native store and applies small StructuredWrite batches through actual create/apply. Read receipts for actual IDs and map them to input-history expected tuples. Do not reuse ZE-145's staged-bundle installer fixtures; those predate actual publication. No decoder output can construct expected rows. Test control VFS wrappers delegate to StdVfs and affect only a named real read operation, with recorded fires.

Each case below is one named nextest test. Use the exact prefix `native_pattern_`; final selection is ten tests. Add a failing assertion for each unimplemented behavior before its production change. A compile error is not RED. If a temporary missing-pattern body is needed, return a typed explicit failure and remove it in the first milestone; no success stub survives. Tests use tiny graphs and independent literal/reference bags, not broad property campaigns.

1. **`native_pattern_scan_expand_scalar_bag` — FIRST, before remaining branches.** Real create/apply two labeled nodes, two parallel directed edges and one self-loop with distinct scalar properties; actual admitted ScanNodes -> Expand -> scalar Filter -> Project -> Collect through the existing `execute_in`. Batch rows=1 to force retained cursors and scalar copies. Compare exact node/edge/endpoint/property tuples and multiplicities from input history. Add one narrow omission control (remove one parallel result from the observed test copy) and show the comparator rejects it. Implement only the correct source/fixed-expand/scalar/completion route needed here, obtain GREEN and report immediately. Do not build joins/DFS/optional before this first useful result works.
2. **`native_pattern_keys_labels_liveness_full_ids`.** Direct node/rel and application-key lookup; unknown label/type/namespace/key; OR duplicate alternatives and all-missing alternatives; exact empty/NUL/long keys, node/rel same key domain separation, deletion/recreation and logical DETACH. In this same case, LookupKey null produces zero matches and zero native key lookups; a dynamically non-string/non-null value preserves the key ExprId with Runtime(Value(Type)); dynamic strings exceeding either existing key bound preserve the key ExprId with Plan(Limit), and an oversized literal application key fails initial validation with PlanError::Limit. Both endpoints must be live for lookup/expand. Real low IDs and requested absent IDs with identical low64 bits but different upper64 bits must never alias; also compare same-view full-width query refs directly through the operator's real equality path. Do not manufacture a high-water installer to claim published high IDs.
3. **`native_pattern_bounded_paths_predicates`.** Tiny cycle with self/parallel edges, 0..0/0..2/1..1/1..3 and cap16. Literal path oracle permits repeated nodes and rejects repeated relationships; exact ordered relationship lists, not counts. Distinct pre-edge and completed-edge predicates, self-list RHS (property equals size(full-list)), false/null on a later member, zero-hop no evaluation and no target-node leakage. A completed predicate error after a previous private path produces no completed output.
4. **`native_pattern_pattern_uniqueness_across_joins`.** Same PatternId across comma/disconnected parts, hidden uses after projection, aliases/common origins, and fresh PatternId on subsequent MATCH. Exact bags show no relationship reused within the same pattern, permitted reuse in a fresh pattern and no false rejection of the shared anchor.
5. **`native_pattern_optional_anchor_and_rebinding`.** Correlated shared DAG anchor versus independent right DAG equality, two left duplicates, multiple right matches, attached WHERE rejecting all candidates, null shared key, nested optional, reused node/relationship/list variables lowered as fresh candidates/equality/reprojection. Assert one null extension only after whole right failure and only new columns null.
6. **`native_pattern_hash_nested_and_selective_equivalence`.** Force each actual join strategy via private test-only deterministic choice on the same validated region; production selection remains bounded. Shared multiple keys, disjoint cross product, null/NaN/numeric/list/entity equality, hash collision with equality rejection, residual false/null, duplicate build rows and multiple batches. Compare exact independent bags for legal lookup/label/full-scan start and join permutations. Counter controls prove actual join/hash probes; expected output is not derived from one strategy's output. Preserve barrier/error cases rather than reordering them.
7. **`native_pattern_same_view_after_publication`.** Pause a real old admitted operator between small pulls, publish a later actual batch on another thread, continue old cursors/scalars and compare its exact old bag. A new admitted execution observes the new bag. All labels/types/properties/text/list refs are from the one admitted generation; no second admission inside an operator. Copied scalar completion remains readable after closing the store. This proves live publication retention only; not compaction or reopen.
8. **`native_pattern_limits_cancel_close_no_output`.** Tight real memory capacity, actual adjacency/operator/path/expression/join/copy work limits, result row/byte bound, caller cancel and timeout during continued work, and actual store close during held traversal. Include long filtered/discarded work and empty surviving batches. Fail at the expected typed limit/control, assert completion not called/no output, exact nonzero real counters and release to pre-execution reservation baseline. Paired clean runs use the same seed/fixture/limits with only the named fault/limit schedule disabled. Keep Store infrastructure allowance separate from the tested query allowance; no arbitrary budget tuning.
9. **`native_pattern_late_expression_and_storage_errors`.** Through real native sources, make a later row divide by zero and make a later not-yet-mapped required artifact read fail via delegated VFS after at least one private row. Assert exact ExprId/cause or TreeError respectively survives the existing driver, completion never publishes, and reservations release. A fake operator returning an error is insufficient. Restore test copy mutation/VFS schedule and rerun clean.
10. **`native_pattern_seeded_directed_probe_can_fire`.** A small `test_support::seeded_rng` history invokes the same actual directed operation/control code used by the runner hook, independently compares exact tuples/lists/bags, and proves the missing-edge/duplicate-path comparator controls fail. Assert all directed receipts and clean controls are observed, no assumed/manual fault hits. This is a named small component test; do not execute the full adversarial runner.

Use bounded dedicated tests for the new key helper inside these cases; no separate broad storage audit. For existing safeguards where no missing implementation remains, one deliberate omitted check in a local test/product copy must make the relevant case fail, then restore exact bytes. Do not carry mutations into the commit or create a redundant audit matrix.

## Directed runner seam and root-only registration

Under `cfg(any(test, feature = "test-support"))`, the private pattern module includes `test_support`. Add this public tooling-only wrapper in query/mod.rs; property_graph is already graph-cypher gated, but keep the explicit feature guard for clarity:

```rust
#[cfg(all(feature = "graph-cypher", feature = "test-support"))]
pub mod pattern_test_support {
    pub use super::pattern::test_support::{ProbeReport, run_actual_probe};
}
```

The re-exported report/function must have actual public visibility; all production internals remain private. Report contains owned primitive observations, exact typed-to-primitive control tags, observed nonzero work, fired fault counts and same-seed clean-control counts. It must not manufacture coverage success or expose a Store/native lease. Plain graph-only and default artifacts omit this wrapper.

`tests/adversarial/graph_pattern.rs::probe(seed, &mut CoverageRegistry) -> Result<(), String>` imports `zeppelin_embed::property_graph::query::pattern_test_support::run_actual_probe`. It independently checks input-history bags, requires each receipt once, validates real nonzero fires for control/error keys, and only then hits the following twelve exact keys:

```text
property-graph.pattern.native-source
property-graph.pattern.path-predicates
property-graph.pattern.uniqueness
property-graph.pattern.join-optional
property-graph.pattern.full-id
property-graph.pattern.retained-view
property-graph.pattern.cancel.fire
property-graph.pattern.limit.fire
property-graph.pattern.late-error.fire
property-graph.pattern.same-seed-control
property-graph.pattern.release
property-graph.pattern.oracle.can-fire
```

Executor supplies this new probe file and a textual patch in evidence, but DOES NOT edit shared registration files. Root applies these exact additive changes on main after the pattern executor commit is integrated, whether ZE-40 has finished or not. Preserve any ZE-40 entries already present; if ZE-40 later changes these files, root resolves the additive conflict while retaining both registrations. There is no dependency on ZE-40 completion:

```diff
--- a/tests/adversarial/mod.rs
+++ b/tests/adversarial/mod.rs
@@ -151,1 +151,4 @@
 pub mod graph_publication;
+
+#[cfg(feature = "graph-cypher")]
+pub mod graph_pattern;
--- a/tests/adversarial/runner.rs
+++ b/tests/adversarial/runner.rs
@@ -2924,1 +2924,2 @@
         super::graph_publication::probe(seed, &mut coverage)?;
+        super::graph_pattern::probe(seed, &mut coverage)?;
--- a/tests/adversarial/coverage.rs
+++ b/tests/adversarial/coverage.rs
@@ -14,1 +14,13 @@
     "property-graph.publication.oracle.can-fire",
+    "property-graph.pattern.native-source",
+    "property-graph.pattern.path-predicates",
+    "property-graph.pattern.uniqueness",
+    "property-graph.pattern.join-optional",
+    "property-graph.pattern.full-id",
+    "property-graph.pattern.retained-view",
+    "property-graph.pattern.cancel.fire",
+    "property-graph.pattern.limit.fire",
+    "property-graph.pattern.late-error.fire",
+    "property-graph.pattern.same-seed-control",
+    "property-graph.pattern.release",
+    "property-graph.pattern.oracle.can-fire",
```

Expected delta: one graph-gated module, one graph-gated probe call inside the existing graph block, twelve unique graph coverage keys. Do not assert an absolute global count because ZE-40 adds its own keys. Do not edit core lib.rs. An unregistered probe is not compilation evidence: root must compile the actual registered runner and record results before component closure.

## Focused commands and final handoff

For each milestone select only its name, e.g. first:

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_pattern_scan_expand_scalar_bag)'
```

Before final prefix run list the actual selection and require exactly the ten tests above, then:

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_pattern_)'
cargo check -p zeppelin-embed --lib -j 4
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,test-support
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,allocation-audit,query-timing
cargo check -p zeppelin-embed-cypher --lib -j 4
cargo check -p zeppelin-embed-ffi --lib -j 4
cargo check -p zeppelin-embed-ffi --lib -j 4 --features graph-cypher
```

After root adds the actual runner registration, root runs:

```sh
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4 --features graph-cypher
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4 --features graph-result-test-support
```

No runner execution in this component assignment. Record exact deferred before/after broad commands/commits in ZE-118. Do not claim >=90% coverage or broad adversarial GREEN from targeted checks. Run scoped formatting and `git diff --check`, inspect exact allowlist and verify inherited hashes/symlinks. Commit only implementation-ticket files with ticket prefix and literal named RED/GREEN in the body. Return commit, ten final results, all compile outcomes separately, real fault/control receipts, source helper signature, source/runner-registration pending state and preserved hashes to root.

Original ZE-50 retains EVERY previous dependency and its exact full native independent-oracle/public/compaction/reopen acceptance. ZE-46 remains mandatory for original acceptance; no recovery API is consumed early. ZE-51/52/53/56/64/66 retain complete relational, mutation, result, TCK, search and public producer obligations. Root adds the new component as a mandatory prerequisite, never removes an old edge, and updates E5/execution/parallel contracts in the same approval session. Final public query construction and native admission error conversion are not delivered by this component. No original ticket closes from these tests.
