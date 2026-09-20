# ZE-154: narrow correction for the native-pattern stack regression

Read-only Astra review, 2026-09-20, of the active ZE-154 worktree. No builds, tests, product edits, or tracker changes were performed by this reviewer. Root approves this correction; Sol retains implementation ownership. This does not reopen the accepted relational scope or expand qualification.

## Finding and evidence boundary

The smallest source-supported correction is to move exactly the three new blocking state payloads out of `PhysicalState` into one-element, already-supported charged `QueryArena` owners. Keep every existing algorithm and recursive control-flow contract.

Observed RED, supplied by the executing worker: final native selection `c20cf771-2f58-4c3c-b057-e69390f39856` passed 17/18 tests, including all eight new relational groups, but the unchanged `native_pattern_keys_labels_liveness_full_ids` aborted with stack overflow. Isolated run `c6862c89-f03c-4be9-9c15-51f0c2e929a7` reproduced the same SIGABRT at 0.140s. These are real regression evidence; do not relabel this as a preexisting fixture failure.

The worker additionally supplied compiler layout output: `PhysicalState` is 1,744 bytes (alignment 16), `Occurrence` 2,352, `SortState` 1,728, `AggregateState` 1,192, and `DistinctState` 872. Those are worker-reported measurements, not measurements independently rerun by this review. They prove the current enum is dominated by the newly inline sort payload. They do not alone measure a complete machine stack frame or establish the exact overflow instruction.

Source inspected:

- `query/pattern.rs:128`: new Sort, Distinct, Aggregate variants embed their entire respective states. `pattern/relational.rs:9-47` shows seven/eight arena handles plus owned `Rows` fields; Sort contains two `Option<Rows>` values.
- `query/relational.rs:81` and `query/runtime/batch.rs:294`: Rows embeds a schema, RowBatch and order arena; RowBatch itself embeds variable/cell/offset ownership. These are substantial control payloads even though variable data is heap-owned.
- `pattern.rs:343-364`, `:638-657`: recursive next/reset temporarily move the entire enum through `mem::replace(..., Vacant)`. Enum growth therefore affects every occurrence, including plans with no relational operator.
- `pattern.rs:2092`, `:2524-2610`: recursive build calls build_unary, constructs new full state values, then pushes a full Occurrence. Large constructor-result temporaries must also be kept out of this recursive function.
- `pattern/tests.rs:988-1100`: unchanged KeyPatternConsumer builds six operators (Unit, LookupNode, LookupRelationship, two LookupKey, Collect), with no new blocking operator. The failing test at `:1758` invokes this actual native plan and further unchanged key/label/liveness consumers after authentic store writes. Its test setup is not authority to hide a production frame regression by relocating fixture locals.

The causal prediction is concrete: changing only new state ownership, without changing the failing fixture or stack limits, shrinks PhysicalState and makes the same isolated test pass. If that prediction fails, report it and localize the remaining failing call; do not claim the diagnosis complete or start unrelated fixture rewrites.

## Exact implementation boundary

Only these existing ZE-154-owned production files need changes:

1. `crates/zeppelin-embed/src/property_graph/query/pattern.rs`.
2. `crates/zeppelin-embed/src/property_graph/query/pattern/relational.rs`.

No new public API, dependency, allocator, format, registry key, test group, capacity/budget change, or edits to existing fixtures are required.

In PhysicalState, replace only the three new `state: relational::...State<'v, 'm, 'g>` fields with `state: QueryArena<'m, 'g, relational::...State<'v, 'm, 'g>>`. Each arena has capacity one and exactly one initialized value after successful construction. Existing variants and occurrences remain as they are.

Have each existing private `SortState::new`, `DistinctState::new`, and `AggregateState::new` return its one-element QueryArena instead of a bare state, preserving its existing error type. Allocate the owner using `QueryArena::new(context.memory(), 1)` inside that nonrecursive constructor; retain the existing state-building body, push its completed Self into the owner with checked error propagation, and return the owner. This is preferable to constructing a large state and calling push directly inside recursive build_occurrence: the latter can retain the heavyweight state/result temporary in every construction frame even after the enum is narrowed. A tiny private owned-return wrapper around the unchanged constructor is equivalent if it keeps those temporaries outside build_occurrence. Do not add a general state-allocation abstraction or unsafe placement initialization.

At next dispatch (`:621-627`) and reset (`:766-777`), obtain the single value through `state.as_mut_slice().first_mut().ok_or(RuntimeError::Batch)?` before calling the unchanged methods. Perform these accesses **inside the existing result-producing closures**, so failures still reach the existing code that restores the extracted PhysicalState. Do not introduce an early `?` that bypasses restoration. Existing next_sort/next_distinct/next_aggregate parameters and state algorithms need no changes.

At aggregate inheritance (`:2755`), use `state.as_slice().first().ok_or(RuntimeError::Batch)?` before `inherited_key_expression`. Missing owned state is corruption of the internal execution structure and must be an error, not false inheritance or empty output. Other transparent inheritance branches remain unchanged.

Reset retains the same one-element owner and invokes the same reset implementation on its element; never clear that owner. Inner row/representative/evaluation arenas retain their existing reset/drop behavior. Dropping an occurrence drops its state exactly once, including all query reservations. QueryArena (`resources.rs:208-284`) already reserves actual element capacity plus its control descriptor before fallible allocation and prevents implicit growth. Its existing accounting stays authoritative: the smaller occurrence array and each separately charged state owner are both accounted, with inner storage charges unchanged. Do not hand-adjust byte totals or increase query allowances to make an assertion pass.

## Finite validation and stop conditions

The two recorded SIGABRT runs already establish RED. Do not reproduce another crashing run merely to obtain a third receipt. Preserve exact commands/output from those worker records in ZE-154 evidence. Apply the narrow ownership correction, then run the unchanged regression and one existing reset group, using ordinary stack settings and the same build mode/features as RED:

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_pattern_keys_labels_liveness_full_ids)'
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_relational_barriers_join_optional_reset)'
```

The first must exit successfully with every existing result, identity, error and liveness assertion intact; absence of SIGABRT alone is insufficient if the test is weakened. The second must preserve its actual repeated correlated execution, optional shared-slot behavior, join composition and barrier reset assertions. No new semantic permutations are needed.

Reuse the already captured layout measurement. If a post-change layout check is useful, repeat the same bounded compiler diagnostic once, scoped to the library and capped at four compile jobs:

```sh
RUSTC_BOOTSTRAP=1 cargo rustc -p zeppelin-embed --lib --features graph-cypher -j 4 -- -Zprint-type-sizes
```

Record only PhysicalState, Occurrence and the three unchanged state sizes. Expect the first two to shrink while the payload sizes remain unchanged. This diagnostic is optional supporting evidence, not a shipping flag, stack knob, permanent size-golden test, or substitute for behavioral GREEN. Do not run further layout/compiler campaigns.

After both focused checks pass, resume only the already-required finite final ZE-154 selection/compile checks affected by the source change. No extra suite is created by this correction. Existing registered native relational probe calls still traverse the same constructor/dispatch/reset paths; registry inventory is unchanged, and the accepted actual consumer/registered compile requirements remain. Broad/full/adversarial execution, fuzzing, coverage, soak, performance and release work remain deferred as already recorded.

If the unchanged regression still overflows, stop this proposed fix from becoming accepted evidence. The next bounded diagnostic is one temporary phase marker around the actual failing consumer/NativePattern construction-versus-pull boundary, or an existing crash backtrace, to distinguish the remaining stack source. Do not increase stack size, add threads, move test locals to hide the failure, or generalize the executor. Root should review that new observation before another production change.
