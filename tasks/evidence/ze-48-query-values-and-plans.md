# ZE-48: typed graph plans and query values

The component adds borrowed query values and immutable structured-plan validation.
It does not execute GraphStore operators, admit a view, prove entity liveness,
reserve the shared store budget, perform retrieval, or return completed rows.
The implementation was developed in `codex/ze-48-query-values`, based on
`a3e093348ba03e42218613bda598daf2c2c061b2`, independently of other worktrees.

## Environment and scope

Measured 2026-09-19 on Apple M3 Max, 137438953472 bytes RAM (128 GiB), macOS 27.0
build 26A5388g, aarch64-apple-darwin. Rust 1.93.0 (254b59607 2026-01-19),
cargo-nextest 0.9.145 (00af4550ec3b3b9f0e574b897b06acb95d325ba2).
Fixtures are deterministic synthetic values, typed DAGs and one seed-0 runner
program; no real dataset or wall-clock performance improvement is claimed.

Core dependencies and persisted formats are unchanged. Storage/canonical replay
identity remains separate from query comparison/grouping. Query references carry
full u128 typed IDs and one pointer-owned admission token; matching store and
generation metadata cannot substitute for that token. Creating a token/reference
proves no liveness and acquires no lease. Lists are borrowed, bounded and private;
packed identity lists retain 16 bytes per ID plus a shared view token. The maximum
524288-ID fixture occupies 8388608 ID bytes and rejects one-over.

Plans borrow immutable arenas and caller facts. Every expression is validated at
each use scope, and implicit common-slot join keys exclude null. All syntactic
search calls are explicit eager obligations in source/dependency order, including
under LIMIT 0; grouped/per-row sources reject. Mutation has an immediate eager
input and cannot precede a reading clause or share a statement with search.
Dynamic search arguments are not evaluated here: later runtime must call the
shared `SearchBounds` constructor and validate actual vector/eligibility contents
before retrieval. A zero candidate window is a tightened capacity allowance;
requiring candidates must fail, not silently omit work or imply exactness.

`PlanBacking` verifies visible spans against sorted disjoint numeric address
regions, never dereferencing those addresses. Aliased backing is charged once;
adjacent regions can cover a span. Region capacities, full inventory capacity and
the separate 65536-byte validator stack envelope must fit the declared retained
capacity, which cannot exceed 25165824 bytes. Inventory backing cannot overlap a
region. Caller-owned hidden capacity and allocator ownership are truthful
assertions; ZE-49 must supply actual reservations and runtime/shared accounting.
This component does not discover an allocation's capacity from a borrowed slice.

## RED to GREEN

Raw logs, SHA-256 inventories, mutation commands and review snapshots are in
[`ze-48-query-values-and-plans/`](ze-48-query-values-and-plans/).
The original two root-authored RED/GREEN cycles and 224-episode before baseline
were preserved. Each subsequent behavior was introduced through a named public
seam test; missing-API RED logs and terminal GREEN logs retain exact diagnostics.

| Named seam/test | Observed RED | Terminal behavior |
| --- | --- | --- |
| `query_boolean_truth_tables_preserve_unknown` | Missing value/truth API | Literal three-valued tables pass |
| `mixed_numeric_predicates_never_round_integer_identity` | Missing comparison API | >2^53, extrema, fractional/subnormal, zero, NaN, infinity cases pass |
| `arithmetic_rejects_domain_overflow_and_zero_divisors` | Missing arithmetic API | Checked accepted unary/binary profile passes |
| `nested_list_predicates_preserve_unknown_and_false_dominance` | Missing list API | Nested/null predicates and membership pass |
| `full_width_entity_references_and_packed_lists_are_view_owned` | Missing entity/list API | Full IDs, distinct admission tokens and packed cap pass |
| `grouping_hash_and_total_order_follow_equivalence_not_replay_bits` | Missing grouping API | Null/NaN/numeric equivalence, hash and global order pass |
| `property_assignment_validates_whole_list_before_copy_and_preserves_empty_kind` | Missing conversion API | Rejection before copy, exact bits and typed empty distinction pass |
| `typed_id_text_preserves_all_bits_and_null_without_a_storage_fetch` | Missing ID text API | 32 lowercase hex digits, ownership/type/null controls pass |
| `bounded_string_and_list_expressions_preserve_unicode_and_null` | Missing scalar API | Unicode count, exact strings and negative indexing pass |
| `typed_plan_revalidates_shared_expressions_after_with_scope` | Missing plan API | References/cycles and each-use lexical scope pass |
| Expression, pattern, scalar, search, mutation, relational and lookup plan tests | Missing respective typed forms | Typed constraints and barrier/source/order facts pass |
| `correlated_optional_preserves_an_existing_relationship_binding` | Scope rejection of inherited relationship | Shared origin accepted |
| `plan_bounds_charge_actual_declarations_and_hide_unused_fact_capacity` | `facts()` exposed unused caller scratch | Only actual plan nodes exposed |
| `aliased_large_literal_backing_is_charged_once` | Shared 8 MiB literal repeated 3 times rejected | Required region proof accepts truthful 9 MiB declaration |
| `shared_relationship_origins_survive_lookup_joins_and_slot_renames` | Duplicate same-pattern origin incorrectly accepted | Complete DAG lineage rejects it; independent patterns pass |
| `inner_join_accepts_and_narrows_compatible_optional_bindings` | Compatible nullable bindings rejected | NODE/REL facts narrow correctly for inner equality join |
| `property_graph_query_probe_checks_values_scopes_and_inflight_faults` | Missing PG6 module | Independent values/scopes and real scheduled clock faults pass |

The initial arithmetic test draft mentioned exponentiation. Reading the accepted
profile before implementation showed it was deferred; the test was corrected and
no exponentiation API was added. Some initial test compilation/lint corrections
were ordinary harness fixes; only intended missing APIs or observed contract
failures are claimed as product RED evidence.

Eleven deliberate mutants each reached the named test failure (nextest exit 100):
integer-to-float rounding, non-equivalent numeric hashes, omitted byte poll,
relaxed list depth, stale scope, unproved span acceptance, lost relationship
lineage, missing search obligations, missing mutation barrier, incorrect primitive
oracle equality, and omitted actual-runner probe. Every original source SHA-256
was restored in `finally`; see `mutants/results.json`. No mutant remains active.
The final focused runs below are GREEN after restoration.

## Focused results and exact commands

All commands ran in this worktree with `CARGO_TARGET_DIR=target/ze48`.
The nextest config used four isolated processes, retries 0, and one libtest test
per process; the exact config is archived as `nextest.toml`. The absolute path
below refers to the installation worktree and is intentionally retained as run.
No nextest invocation used `--nocapture`.

```sh
CARGO_TARGET_DIR=target/ze48 cargo nextest run --config-file /Users/aghatage/Documents/code/zeppelin-embed-wt-ze-117/.config/nextest.toml -p zeppelin-embed --features test-support --test graph_query_values --test graph_query_plan
CARGO_TARGET_DIR=target/ze48 cargo nextest run --config-file /Users/aghatage/Documents/code/zeppelin-embed-wt-ze-117/.config/nextest.toml -p zeppelin-embed-workspace-tests --test adversarial_tests -E 'test(property_graph_query_probe) | test(query_oracle_rejects) | test(one_runner_episode_reaches_required_query_contracts)' --success-output immediate
CARGO_TARGET_DIR=target/ze48 cargo clippy -p zeppelin-embed -p zeppelin-embed-adversarial-oracle -p zeppelin-embed-workspace-tests --all-targets --features zeppelin-embed/test-support --no-deps -- -D warnings
cargo fmt --all -- --check
git diff --check
```

- Core focused run: 29 passed, 0 failed, 0 skipped; 18 plan and 11 value tests.
- Oracle/probe/actual-runner run: 3 passed, 0 failed, 437 unrelated tests skipped.
- PG6: 390 independent value comparisons, 2 independent lexical-scope cases,
  3 actual scheduled clock fault fires and 3 same-seed clean controls. Each clean
  control completes more work than the interrupted operation. No partial plan,
  list or predicate success is returned by the fault path.
- Actual runner: seed 0, 59 operations, 0 violations; all eight required PG6
  coverage keys reached. Removing its production runner call causes the named
  actual-runner test to fail.
- Scoped strict clippy, formatting and diff whitespace checks passed. No new
  dependency/package, feature exclusion or threshold relaxation was introduced.

The primitive oracle crate is std-only. Its mixed numeric comparator uses bounded
float truncation and fractional sign, independently of production's bit
mantissa/exponent comparison. Query grouping requires equal hashes only for
mathematically equivalent values, never collision-free hashes. Lexical scope uses
plain set replacement. A separate independent review compared 60426 deterministic
numeric pairs to Python integer-ratio arithmetic with zero mismatches; this is
supporting review evidence, not a replacement for the committed public tests.

## Review, deferred work and limits

Read-only value review found no concrete issue. Typed-plan review found the three
alias/lineage/nullability issues above; named tests observed each failure before
correction. A second frozen correction review found no remaining concrete issue
in numeric region accounting, bounded lineage traversal or nullability. Root
reviewed the full PG6 primitive oracle and adapter and found no concrete defect.
The archived `source-sha256.json` pins final production/tests/runner/guidance,
including both graph_query.rs files; historical review snapshots remain distinct.

The user explicitly directed nonessential broad qualification to backlog ticket
**ZE-118 (E12)** instead of delaying implementation commits. Therefore the full
workspace suite, whole-crate coverage qualification and the full after-adversarial
matrix were not run here. Before implementation, the preserved baseline was
224 episodes, 0 violations, 105.56 s. The final single-runner episode is a focused
wiring/fault contract check, not the missing full after matrix. Deferred commands
include the original `cargo test -p zeppelin-embed-workspace-tests --test
adversarial_tests smoke -- --exact --nocapture` lane (now to be translated to the
adopted nextest runner) and `scripts/coverage.sh`/explicit integrated per-crate
reports. No >=90% whole-crate coverage claim is made by this ticket evidence.

No benchmark, minimum macOS 14 runtime, Intel, Windows, bindings parity, TCK,
GraphStore execution/admission, resource reservation, query result lifetime,
retrieval correctness, or graph durability acceptance is claimed. Those remain
with their owning tickets and the deferred qualification record.


## Main integration

Candidate `4ca9afd21b8f9ddfe4694e906712369bfb5ea2f3` was individually
cherry-picked onto main `cab33b758c405ba73e0785841147ecdd480d7cd6`.
Production query/oracle implementation and both public suites retain their exact
candidate hashes. Existing core/oracle/driver module declarations and core guide
remain complete byte-identical prefixes; the new exports were placed at EOF.
All earlier graph coverage keys and all six graph probes remain wired. All
45 inherited user file hashes are unchanged. The source audit, exact commands
and losslessly compressed logs are in the `integration/` evidence directory.

The committed nextest four-process profile passes **29 core tests** (0 skipped)
and **3 PG6 oracle/probe/runner tests** (443 unrelated tests skipped). The actual
combined runner performs seed 0's 59 operations with 0 violations; PG6 reports
390 comparisons, two scopes, three fired faults and three same-seed controls.
Strict scoped Clippy and workspace formatting both exit 0. Final correction
review parity and the only comment/autoderef differences are explicitly recorded.
No complete workspace, smoke matrix or coverage run was performed on the combined
source. Those obligations remain deferred to ZE-118, without changing thresholds.
