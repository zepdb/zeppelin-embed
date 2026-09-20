# Native relational barriers and eligibility: exact component plan

Planning ticket ZE-153. Source pin `f08898e6d34c3983ab07dc085567481e2ae81c18`. Execution: Sol/xhigh after root reviews this plan and creates the flat implementation ticket. Commit subjects use that assigned key. This is one complete private component, not authorization to start blocked ZE-50/51/53/56.

## Deliverable

Extend **NativePattern's existing occurrence engine** with Aggregate, Distinct, Sort and OffsetLimit, composing its existing native sources/traversals/joins/optional/scalars with ZE-125 Rows kernels and the typed execute_in driver. Support these barriers above and below native pattern work: MATCH → WITH aggregate/DISTINCT/ORDER/LIMIT → MATCH and legal join/optional compositions. Add actual native singleton-domain eligibility preparation using EligibleNodeSet.

Search execution, Eager/Mutate, retrieval/report production, public APIs, completed entity packaging, ABI and compaction/reopen qualification remain with their original owners. Unsupported Search/Eager/Mutate in an occurrence subtree fail explicitly; never drop a CALL or fabricate success/report. The eligibility helper consumes an actual validated Search descriptor's non-search singleton input without pretending to execute the Search.

No second engine/driver, external-source injection framework, duplicate hash/group/sort implementation, compiler edits or dependency changes. No broad/full/adversarial/coverage/fuzz/performance/release execution. After two unsuccessful corrections of one issue, or 20 minutes without a useful milestone, send root the concrete blocker; do not expand scope or tune budgets until GREEN.

## Exact ownership

Executor production paths below are relative to `crates/zeppelin-embed/src/property_graph/`:

1. `query/pattern.rs`: four occurrence states/count/build/dispatch/reset; preserve existing constructors and PatternCapacity where possible. Reuse current rows/expression capacities for bounded relational owners. No renaming/reformatting sweep.
2. New `query/pattern/relational.rs`: relational occurrence state/methods, evaluated keys/operands and representative-sidecar transport. It accesses the parent owner directly; it is not another interpreter.
3. New `query/pattern/relational/eligibility.rs`: genuine singleton preparation and owned same-view eligibility.
4. `query/relational.rs`: only checked crate-private selected-original-row access on Rows.
5. `query/relational/aggregate.rs`: only the common existing aggregate path needed to return Group.first representatives with output. Existing aggregate API/callers/semantics remain intact.

Executor test/tooling paths:

- New `query/pattern/relational/tests.rs` and `query/pattern/relational/test_support.rs`. Share actual native fixture/run helpers between tests and directed receipts under `cfg(any(test, feature = "test-support"))`; wire tests in the new relational module.
- New `tests/adversarial/graph_native_relational.rs`: wrapper/comparator consuming those same real helper outputs, following graph_pattern.rs. This permits directed source integration, not an adversarial runner execution.
- `tasks/evidence/native-relational-component.md` and `tasks/evidence/native-relational-component/**`: exact plan, RED/GREEN logs, manifests/hashes, final bounded checks and limitations.

Root-only additive registration after the executor's individual commit: `query/mod.rs`, `tests/adversarial/mod.rs`, `tests/adversarial/runner.rs`, `tests/adversarial/coverage.rs`, as specified below. The new pattern/relational module's visibility may be `pub(super)` solely for that tooling export; production types stay crate-private.

Do not edit plan/validator/lineage, expression.rs, eligibility.rs, runtime/driver, kernel ordering.rs, existing pattern tests/probe/planner, Cypher, lifecycle/native_graph, storage/VFS/reclamation, public API, Cargo files or inherited AGENTS/CONTEXT/symlinks. If a necessary fix is outside this allowlist, report its exact seam to root first.

Preflight checks: assigned ticket claimed/unblocked, exact assigned pin/worktree, clean owned paths, inherited hashes and symlinks preserved. Root owns scheduling and tracker/spec updates; executor uses only its assigned worktree.

## Existing producers and invariant ownership

NativePattern::new (`pattern.rs:274`) uses real GraphReadView, RuntimePlan, parameters, capacities and NativeExpressionEvaluator. Its typed RowOperator/PullOperator implementations are at lines 1913/1921. Extend occurrence_count/build at 1958/2019, preserving lexical AnchorBinding. Existing `execute_in` (`runtime/driver.rs:308`) is the single final row drain/no-partial-output boundary.

Use authentic QueryInputs/facts/parameter retention, one RuntimeContext/QueryMemory/QueryView and cumulative counters. Schema is derived from NodeFacts.slot_at and accessed with Schema.column; SlotId is never a column index. Expression scratch is reset on each evaluation: copy every retained key/operand before evaluating the next expression. Aggregate descriptors supply scalar operands; never send Expression::Aggregate to the scalar evaluator (`expression.rs:640`).

Rows owns real data and an original-row order vector (`relational.rs:81`). Existing Distinct/Sort/Aggregate kernels remain authoritative. No spill, row truncation, hidden all-graph collector or fallback replay after an allocation/expression failure. Charge descriptors, schemas, private evaluated cells, row data, order/representative arrays, hidden-use spans, scratch and all simultaneous old/new backing. Allocation is fallible and reserved first; no synthetic credit or a whole-query allowance per operator.

Scheduling batches remain <=256; existing path/list/set/result bounds, <=24 MiB query allowance and shared 256 MiB target are unchanged. Poll at existing bounded row/comparison/expression/copy checkpoints, including newly introduced loops. Query budget and store infrastructure budget remain distinct.

## Root-approved representative provenance rule

This exact rule was reviewed and accepted during ZE-153 planning:

- Filter, Project/With, OffsetLimit and Sort transport the selected input row's existing RelationshipUse triples.
- DISTINCT transports the **actual selected physical row's** sidecar. Equal visible rows may have different hidden uses; hidden uses are not DISTINCT keys.
- Aggregate transports **exactly Group.first's sidecar**, matching the source row from which the existing kernel copies key cells (`aggregate.rs:225–239`). Empty global output has no source use. Count/collect values invent no traversal origin.
- Do not union groups, allocate membership arrays, clear uses merely at With or build a generic provenance framework. Preserve identical/common-origin deduplication and full relationship/origin IDs.
- Structured lineage through With/aggregate-key aliases stays unchanged (`plan/lineage.rs:50–61`; existing graph_query_plan.rs renamed-origin test). Do not assume the structured validator retires PatternIds.
- The compiler gives every MATCH a fresh PatternId (`lowering/pattern.rs:123–134`). Current PatternId equality lets a legal later MATCH reuse prior relationships. Retain the representative's old sidecar while that row lives; discard it only with its actual row/state owner. No retirement optimization is needed.

Tests must distinguish this representative rule from both unconditional clearing and all-input union, choose representatives by explicit order, preserve common-origin aliases, and prove fresh-PatternId later MATCH bags.

## First real-source milestone before remaining branches

Add `native_relational_offset_scope_then_match` first. Publish actual A→B parallel relationships and B→C. Authentically admit a typed plan: Lookup(A) → Expand(P0) → With(B under a sparse renamed slot) → OffsetLimit(offset=1, limit=1) → Expand(P1) → Project/Collect. Use apply receipts for IDs and batch_rows=1. Independent expected bag: exactly one B→C tuple.

Current NativePattern construction rejects OffsetLimit. Assert expected successful execution to observe a behavioral RED, not a compile error/helper panic. Implement only OffsetLimit and necessary occurrence count/build/dispatch/reset. Retain original bounds and resumable remaining skip/limit counters. Consume skipped child rows normally, charging their real work; never emit empty More. Copy chosen row/schema/sidecar. Reset restores original bounds per correlated right replay. Safe read LIMIT0 may stop pulls but must not reorder earlier predicates or substitute for work budgets.

Obtain GREEN and report exact first RED, first compile and first useful GREEN to root before Sort/Distinct/Aggregate/eligibility.

## Remaining production work

### Sort and DISTINCT

Add `Rows::selected_source_row(position) -> Result<usize, RuntimeError>` as a checked crate-private accessor of its existing order entry. No mutable order access, detached borrow or new row abstraction.

Sort drains child occurrences into retained visible rows/use spans. Evaluate SortKey expressions against each original child schema/row, in descriptor order, and copy values into a separate bounded key Rows with private internal slots. Call existing Rows::sort/OrderKey on key Rows. Its selected_source_row indexes the retained original visible row and sidecar to emit. Separate key rows avoid adding hidden slots to a legal 256-column public schema. Keys never enter NodeFacts/public output. Stable ties, descending/null/numeric/list/entity order remain existing kernel semantics.

DISTINCT drains visible rows into ordinary Rows plus parallel use spans, calls existing Rows::distinct over exactly that visible schema, and emits values/sidecars using the checked original-row mapping. No ordinal or hidden-use value participates in equivalence.

Both construct without pulling/evaluating, drain fully on first pull or fail, and then resume output without expression replay. Internal empty surviving batches do not imply Done. Zero final rows produce Done only after source exhaustion.

### Aggregate

Inspect validated top-level aggregate descriptors. Prepare private evaluated columns for group keys and count/collect operands; count(*) has no operand. The number of private columns is bounded by validated keys+aggregates, not input-width plus hidden temporaries. Use private slots and copy each evaluated value before scratch reuse. Count with no operand/distinct=false maps to CountAll; remaining descriptors map to existing AggregateColumn variants. Output schema/slots exactly match validated facts.

Add a crate-private aggregate entry returning `(Rows, QueryArena<Option<usize>>)` with one entry per actual output: existing Group.first raw input index, or None for empty global. Use one common existing aggregate implementation with optional representative recording. The existing public aggregate signature stays unchanged and does not allocate unused representative backing. Record indices beside the kernel's actual output rows; do not recompute grouping/hash or add a group-membership owner.

NativePattern retains original use spans until mapping those representatives. Copy exactly each chosen source sidecar, then release dead input backing. Check output/representative lengths and every index. Empty global emits one 0/[] row with empty sidecar; empty grouped emits none. Reuse existing null skipping, exact equivalence, checked counts and bounded/ordered collect.

### Reset and composition

Add all four unary kinds to occurrence counting/building without changing anchor substitution. Extend reset to restore bounds and drop/clear relational key/input/output/representative owners and cursors, marking blocking states unstarted. Reset only the intended right subtree; Anchor clears its emitted bit without pulling/resetting the preserved left. Keep view/parameters/symbols and cumulative work. Newly length-dependent reset loops use runtime checkpoints; pass context through reset where needed.

Existing Join/Optional consume these occurrences through unchanged current-row/equality/uses logic. Do not propagate planner source ranks through these semantic barriers or reorder expressions. Optional matches only after full right/predicate/uses success. Empty global aggregate count=0 is a real matching row; empty grouped input may null-extend. Preserve inherited-null versus independent equality behavior. No prior left row's sorted/grouped/limited cache may survive reset.

### Native eligibility

Add a crate-private preparation function and owner in the new eligibility child module. Inputs: authentic view, admitted RuntimePlan, actual Search PlanNodeId, bindings, explicit native/set capacities and same runtime. Read the validated Search's sole input and optional eligible ExprId; require its real NodeFacts.singleton. Do not infer singleton from LIMIT=1, observed row count or caller flags.

Build NativePattern for that non-search input subtree using the same RuntimePlan. Prepare exactly one row: copy the first actual occurrence row/schema into a charged owner, then verify exhaustion. This bounded eager domain prepass is not a second general query driver or fabricated root plan. Preserve that original singleton binding/list so set deduplication cannot change it or force later argument evaluation to rerun the domain.

Omitted eligibility creates owned AllIndexed. Explicit empty list creates an actual empty EligibleNodeSet. Otherwise evaluate the eligible expression on the preserved row with the existing evaluator, require List, validate its complete geometry and every member, and call existing EligibleNodeSet::build with same-view NodeRefs. Keep scratch/list ownership live through construction. Null/scalar/relationship members fail; no filter_map/map_while truncation or default values. A checked iterator verifies all declared positions were consumed.

Wrap list/type/member failures with the eligibility ExprId and exact existing Runtime/Query cause; retain memory/control errors. Descriptor/ownership failures remain their exact Plan/Runtime causes. Return a private owner retaining row/schema and optional actual set, borrowed through existing Eligibility. Full-ID unique capacity and examined-duplicate work remain separate; no new examined-row cap, second hidden ID arena or borrowed-ID double charge. Real foreign-admission view checks remain mandatory.

Do not implement prepare_search as a success stub, invoke retrieval, charge pretend SearchInvocations or fabricate reports. ZE-64 later consumes this real producer from the existing eager integration. One preparation executes its domain once; no per-outer-row callback.

## Exactly eight new acceptance groups

Use `pattern::relational::tests` and these names; keep subcases within them. Add intended RED before each corresponding production change. All fixture rows come from Store::create_native_graph/apply_native_graph/with_native_read and authentic retained typed plans. Independent expected bags/sets come from write history/receipts, not another production strategy or decoder. Use small ordinary-stack fixtures, not multi-megabyte stack arrays/high-stack qualification.

1. **native_relational_offset_scope_then_match**: first milestone; then offset beyond end, limit0 and chained bounds across batch1. Exact tuples, sparse renamed scope and actual discarded work. Existing scope-invalid validation remains unchanged.
2. **native_relational_sort_keys_ordered_collect**: native/computed hidden keys, multiple ascending/descending keys, stable ties with explicit final ID tie-break, null/exact numeric ordering; later MATCH and ordered collect preserve expected order. One full-width public schema with separate hidden sort keys; heap-backed retained facts/columns.
3. **native_relational_distinct_provenance_then_match**: null/NaN/exact numeric/list equivalence from actual input values; parallel relationships create equal visible rows with different uses. Explicit order selects a known representative; subsequent same-PatternId traversal distinguishes representative retention from union/clearing. Common-origin alias join passes; fresh-PatternId later MATCH reuses both relationships with the exact bag.
4. **native_relational_aggregate_native_empty_and_groups**: empty global 0/[] vs grouped no rows; count(*)/count(x)/collect(x) null behavior, evaluated grouping keys, DISTINCT operands and bounded ordered collect. Differing hidden-use inputs share keys; explicit order selects Group.first, and subsequent same/fresh PatternId cases verify its sidecar. Empty global has no source uses; count/collect invent none. Assert exact values/types/order and actual GroupKeys/Expressions.
5. **native_relational_barriers_join_optional_reset**: relational stages on either side of joins and inside correlated optional right subtrees; batch1, duplicate left, multiple right matches, nested anchors, grouped-empty null extension, global-empty count=0 match. Exact independent bags under existing forced Hash/Nested where legal. No state leaks across anchors; inherited null/common origins stay correct.
6. **native_relational_eligibility_singleton_domains**: actual native global collect(DISTINCT n); omitted vs explicit empty; duplicate preserved binding/list unchanged while eligible full IDs become sorted unique; duplicate examined entries fit small unique capacity and remain counted; real type/member/capacity failure and separate-admission foreign-view rejection. Existing validator rejects grouped/non-singleton Search input. No ranking/report claim.
7. **native_relational_limits_controls_errors_release**: actual query-memory/capacity rejection, blocking row/payload/collect bounds, actual expression/hash/operator/copy work limits; later sort-key/aggregate-operand arithmetic failure with exact ExprId after useful native work; actual late native read failure propagates through a new blocking path using the existing delegated-VFS fixture approach. Cancel/deadline/close must fire after observed work and use a real synchronization/receipt, not pre-expiry or a racing spawned close. Exact typed causes, positive relevant counters after prior work, completion=false, measured pre-component reservation baseline restored, and same-fixture paired clean with only the schedule disabled. Construct exact capacity pressure; do not sweep arbitrary budgets.
8. **native_relational_directed_probe_can_fire**: invoke the same shared actual helpers, verify exact receipt inventory and independent bags/sets, reject deliberate missing/duplicate/wrong-representative observations, then same-seed clean. Return genuine per-case counters/causes/ExprId/completion/release. Scan counters cannot substitute for group/eligibility/control evidence; no hardcoded success/release counts.

Preserve NativeExecutionError payloads by value. Every late failure yields no completed output. Check runtime.memory().reserved_bytes() before the component and after all temporary owners drop. Actual observed events may produce a receipt count of one; constants pretending observation may not.

The ten frozen `native_pattern_` tests are the finite engine regression group. The only existing kernel target rerun is `graph_relational`, covering the small shared aggregate/accessor changes. No whole-workspace suite.

## Commands and terminal evidence

First RED/GREEN command:

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_relational_offset_scope_then_match)'
```

After the first GREEN, implement groups 2–6 in dependency order, then actual controls/shared receipts. Final named run expects **18 tests** (8 new + 10 frozen), followed by affected kernel target and narrow feature checks:

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_relational_) | test(native_pattern_)'
cargo nextest run -p zeppelin-embed --test graph_relational --features graph-cypher -j 4 --retries 0
cargo check -p zeppelin-embed --lib -j 4
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,test-support
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,allocation-audit,query-timing
cargo check -p zeppelin-embed-cypher --lib -j 4
cargo check -p zeppelin-embed-ffi --lib -j 4
cargo check -p zeppelin-embed-ffi --lib -j 4 --features graph-cypher
cargo clippy -p zeppelin-embed --lib -j 4 --features graph-cypher
```

Use scoped rustfmt on owned files and git diff --check; no workspace formatter rewrite. Preserve qualification of pre-existing warnings and fix newly introduced warnings/panic-policy violations. Repeat checks only after relevant changes/failures. Record commands, counts, exact causes, literal RED/GREEN and restored deliberate controls. Handoff one individual executor commit with only allowlisted files and matching hashes.

## Directed registration, independent of ZE-46

Executor supplies real shared `run_actual_probe(seed)`/report and the new adversarial wrapper. Register exactly **10 keys** with exact inventory/duplicate/positive-evidence checks:

```text
property-graph.native-relational.pipeline
property-graph.native-relational.representative
property-graph.native-relational.group
property-graph.native-relational.eligibility
property-graph.native-relational.limit.fire
property-graph.native-relational.cancel.fire
property-graph.native-relational.late-error.fire
property-graph.native-relational.release
property-graph.native-relational.same-seed-control
property-graph.native-relational.oracle.can-fire
```

Root's exact additive changes after executor commit:

- Beside query/mod.rs's graph-cypher+test-support pattern export, add identically gated `native_relational_test_support`, re-exporting only the new report/run function from `pattern::relational::test_support`.
- Beside tests/adversarial/mod.rs's gated graph_pattern, add identically gated `pub mod graph_native_relational;`.
- Beside runner.rs's `super::graph_pattern::probe(seed, &mut coverage)?;`, add `super::graph_native_relational::probe(seed, &mut coverage)?;` in the same gated directed group.
- Add the 10 keys beside existing pattern keys in coverage.rs. Registry delta is exactly +10; preserve every existing key/probe.

Root integrates registration after the executor commit whether ZE-46 has finished or not. Resolve later additive conflicts preserving both. Before component closure, compile the ACTUAL registered new probe:

```sh
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4 --features graph-cypher
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4 --features graph-result-test-support
```

Baseline-only unregistered compilation is insufficient. Do not execute the broad adversarial target. The named direct probe tests exercise shared actual production helpers; ZE-118 retains campaign qualification.

## Proposed flat ticket and retained gates

Title: **Implement native relational barriers and eligibility domains**.

Goal: complete the private native occurrence composition and singleton eligibility described here, using compiled producers and existing kernels. Genuine inputs: approved ZE-153 plan, ZE-152, ZE-125, ZE-145, ZE-149, ZE-49, ZE-45/39 and ZE-123/126/138 typed/scoped contracts. Root records exact dependency edges, including transitive source prerequisites as appropriate. No artificial dependency on ZE-46 completion.

Acceptance: 8 actual-native groups plus 10 frozen engine regressions and affected kernel target GREEN; representative provenance and genuine memory/control/error/release seams implemented; exact literal RED/GREEN commit/evidence; narrow feature checks; genuine directed receipts and root's actual registered-runner compilation. Root verifies preservation/allowlist before integration.

Original ZE-50/51/53/56 retain every dependency and native/public/oracle/compaction/reopen/result/conformance requirement. ZE-64 owns retrieval/eager reports; ZE-118 owns broad qualification. No implementation or runtime evidence follows from this planning document.
