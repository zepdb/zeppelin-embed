# ZE-154 focused commit scope review

Read-only review, 2026-09-20, of the accepted `/tmp/ze-153-native-relational-execution-plan.md`, current ZE-154 worktree production/tests and existing `crates/zeppelin-embed/tests/graph_relational.rs`. No edits to worker files, builds or tests. Root owns any acceptance/evidence amendment. Source presence below does not mean a case has passed.

**Recommendation: finish the eight existing named groups with one representative check per new native branch; stop expanding scalar/kernel permutations and stop the unified eligibility-fixture redesign.** Preserve the existing production architecture and original ZE-51 full eventual acceptance. This is a finite reduction in redundant component qualification, not permission to call unverified new behavior complete.

## Source distinction

New native responsibilities are occurrence build/dispatch/reset; evaluated-key/operand copying; sparse schemas; original-row/Group.first relationship-use transport; optional inheritance; singleton-domain execution and typed error transport; and ownership/release of overlapping native state. These are in `pattern.rs`, new `pattern/relational.rs`, and `pattern/relational/eligibility.rs`.

Unchanged semantic kernels are `Rows::sort`, `Rows::distinct`, equivalence/hash/value ordering and EligibleNodeSet. The aggregate diff routes the old method through the same `aggregate_inner` and optionally records the already selected `Group.first`; it does not introduce another aggregate algorithm. Existing `graph_relational.rs` has 13 tests, including:

- `distinct_and_stable_sort_use_exact_recursive_query_equivalence`: nulls, distinct NaN payloads, mixed I64/F64, >2^53 ordering, recursive lists and descending stable ties.
- `grouped_and_empty_global_aggregates_skip_null_and_keep_ordered_collect`: CountAll, count DISTINCT, ordinary/DISTINCT collect, null skipping, ordered list contents, empty global and empty grouped results.
- `eligible_set_is_full_width_same_view_and_caps_materialized_ids_not_duplicates`, `maximum_materialized_eligibility_fits_one_packed_owner_and_rejects_one_over`, and `eligible_hashes_mix_high_identity_bits_before_bucket_selection`: set identity, duplicate/cap and hash behavior.
- Existing blocking-chain, work/cancellation and owner-allocation/release cases test those kernel mechanisms.

Rerun that existing target once because aggregate routing changed. Do not reproduce its complete value table or maximum-population/allocation matrix in native fixtures.

## Minimal remaining commit-required list

Keep the eight current test names. Reuse any already passing assertions/evidence; only fill the gaps below that are absent from current code or verified evidence.

| Existing group | Minimal native evidence required |
| --- | --- |
| `native_relational_offset_scope_then_match` (`tests.rs:319`) | Existing real offset/renamed sparse scope/later-MATCH tuple and discarded work. Add only limit-zero and beyond-end termination if absent; these are distinct native termination branches. Chained bound permutations across different batches are deferred. |
| `native_relational_sort_keys_ordered_collect` (`:748`) | Current actual property sort then later MATCH must stay. One compact two-key case with different directions and one computed/string key proves descriptor direction, scratch copying and selected original row; assert visible columns remain unchanged. Connect a sorted actual stream to one native collect and assert list order. The current function name alone does not prove ordered collect: its current completion asserts two later-MATCH tuples. No numeric/null/list ordering table, many tied-key arrangements or 256-column fixture is required here. |
| `native_relational_distinct_provenance_then_match` (`:1035`) | Retain actual equal-visible-row/different-relationship-use input, known ordered representative, and same/fresh PatternId exact tuples. This directly tests the new selected_source_row sidecar mapping. The shared kernel already proves scalar/list equivalence; do not add native copies of that table. |
| `native_relational_aggregate_native_empty_and_groups` (`:1445`) | Retain empty-global CountAll and current Group.first same/fresh PatternId controls. One compact nonempty native aggregate must map CountAll, count with operand and collect with operand correctly, including one DISTINCT descriptor and one computed grouping key/operand. A small null operand suffices to show the correct operand column is passed. Reuse eligibility's already-real collect(DISTINCT node) path for its descriptor branch. Add one empty-grouped no-output assertion if absent. Do not multiply these by scalar types, sort directions, or all DISTINCT/null combinations. |
| `native_relational_barriers_join_optional_reset` (`:1825`) | Keep the actual Aggregate shared-slot replacement/right-marker regression. New reset arms exist separately for OffsetLimit, Sort, Distinct and Aggregate. Exercise each at least once under an actual repeated correlated right invocation; combine the four in one small valid chain, with distinguishable per-anchor outcomes so stale cached output would fail. Retain one real join composition with a barrier on a consumed side and the existing common-origin compatibility contract. Do not add every left/right placement × Hash/Nested × nested depth × batch size. The existing aggregate/Sort-only marker fixture does not prove Distinct/Offset reset. |
| `native_relational_eligibility_singleton_domains` (`:1569`) | Retain real global collect(DISTINCT n), omitted AllIndexed versus explicit empty Set, duplicate binding/list preserved while a capacity-one set deduplicates, one actual member-type rejection with exact eligibility ExprId, capacity failure, and actual separate-admission rejection. `eligibility.rs` introduces the retained singleton row/exhaustion check and error wrapping, so these cannot be replaced by kernel-only tests. Use the existing two-node/two-parallel-edge store and small per-case valid plans; no unified five-node/operator fixture is needed. A statically invalid scalar/non-singleton plan should be asserted as a validator rejection, not repeatedly forced into execution. It is not a runtime negative-control receipt. One valid dynamic wrong-kind/member path is enough; no scalar/null/relationship Cartesian expansion. |
| `native_relational_limits_controls_errors_release` (`:1914`) | Retain the existing actual shared probe: operator/expression/hash/copy work rejection, scratch cancellation, post-work deadline/close, late native map failure, arithmetic error, no completion and measured release. Do not duplicate those controls per operator. Add only one real native blocking capacity exhaustion after useful input and one authentic query-memory admission failure if not already covered by the probe; both must have a clean control and restored reservation baseline. Aggregate evaluated-operand error transport needs one exact ExprId assertion if existing arithmetic control exercises only Sort; same evaluator arithmetic value permutations are unnecessary. |
| `native_relational_directed_probe_can_fire` (`:1928`) | Existing receipt-derived pipeline/set oracle and deliberate missing/duplicate/wrong-representative rejection, exact unique key inventory, observed counters and release, same-seed clean. Keep the actual registered wrapper. Source currently uses **10** receipt keys and both groups 7/8 invoke the same `run_actual_probe`; neither needs a larger fixture/campaign. |

The above is the complete remaining branch checklist, not a demand to reimplement already present cases. Worker should mark each item as existing verified evidence, one small missing assertion/case, or a concrete production failure. Only the latter two justify more work. Keep current useful RED/GREEN transcripts; do not fabricate retroactive RED or rerun successful permutations merely to generate more logs.

## Exact optional expansion to defer

Root may create one flat E12 ticket, e.g. **Extend native relational integration qualification**, retaining these larger plan-derived combinations for eventual original ZE-51/qualification acceptance:

1. Native replay of the complete null/NaN/mixed-numeric/>2^53/list equality and ordering table; broad stable-tie/ascending/descending arrangements. Already covered in unchanged Rows kernels. A native scalar-copy witness above remains required.
2. Full 256-column public-schema Sort boundary, many computed hidden keys, all scalar/list widths and maximum-size payloads. Current private key rows are structurally separate; a small multi-key native case is required now, the maximum geometry is extended qualification.
3. Full aggregate function × DISTINCT × null × value-kind × grouped/global/empty × input-order matrix. Keep the finite native descriptor mappings, empty branches and representative proof above; defer the cross-product.
4. Every barrier at every join side, both join strategies for every shape, deeper nested optional anchors, repeated common-origin alias permutations and multiple scheduling batch sizes. Keep each new reset arm and one real join/optional composition now. Existing ten native pattern regressions already cover underlying join/anchor/uniqueness machinery.
5. Native maximum 524,288-ID/one-over eligibility fixtures, many high-bit hash distributions and allocation-failure-at-every-set-owner repetitions. Existing unchanged kernel tests own those proofs; keep native duplicate/cap/type/foreign-view integration controls now.
6. Exhaustive per-operator resource/cancellation/fault-position or allocation-site matrices, arbitrary budget sweeps, repeated seeds and extra final reruns without source changes. The actual branch controls above remain mandatory. Broad/full/adversarial/fuzz/coverage/soak/performance/release work remains ZE-118.

Do not label missing reset, representative, descriptor conversion, dynamic error or allocation-owner evidence as optional just because its fixture takes effort. No production acceptance is inferred from a test name or a validator failure.

## Commands to keep and commands to defer

Keep the existing finite final selections, once the required branches are complete:

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_relational_) | test(native_pattern_)'
cargo nextest run -p zeppelin-embed --test graph_relational --features graph-cypher -j 4 --retries 0
```

Keep the accepted narrow feature checks and scoped formatting/diff check; root must still compile the actual registered new probe under default, graph-cypher and graph-result-test-support after integration. No compile gate is traded away for redundant test reduction. Use only the single named group currently being corrected before the final selection.

Defer **additional** expanded native semantic selections/matrices and duplicate invocations of the whole `native_relational_`/`graph_relational` selections after an unchanged GREEN; no existing test is deleted. Do not run `cargo test --workspace`, the full `adversarial_tests` target, runner episodes, coverage/fuzz/performance/release commands for ZE-154. There is no reason to add more test groups or a fixture abstraction before completing these eight. The accepted plan's ten registry keys remain ten; an earlier tracker shorthand saying eight is not authority to omit receipts.

Original ZE-51, ZE-50/53/56 and ZE-64 public/oracle/search/lifetime/conformance obligations remain unchanged. Root records any reduced component-only qualification and the exact deferred list explicitly; this review itself changes no tracker or accepted plan.
