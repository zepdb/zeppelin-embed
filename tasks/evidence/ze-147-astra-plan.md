# ZE-76 execution readiness and one independent interval-accounting component

Source: main `70f8cc0a645bf85577d8d034622c291e586acaa9`, 2026-09-20. Planner-only inspection; no implementation, builds, tests, worktrees or new tickets. Executor operating rules: `/tmp/graph-sol-executor-rules.md`.

## Decision

**Do not assign original ZE-76 for completion now.** Its native prerequisite list is satisfied, but its public integration acceptance is not implementable on this checkout. Root agreed to preserve that acceptance and correct the dependencies. A separate, complete **allocation-interval observation** component is justified by a concrete gap in compiled accounting, and root accepted the single-active-observation design below. Root creates its flat ticket and records the scheduling refinement; this document does not create it.

### Original ZE-76 dependency correction

Preserve existing ZE-29/37/45/49/55/60 prerequisites. Add explicit completion edges to:

| Required ticket | ZE-76 obligation that needs it |
| --- | --- |
| ZE-39 | Actual durable mixed-write publication and artifact/WAL/full/directory-sync observations; oversized writes publish nothing. |
| ZE-53 | Actual final execution/result handoff, no partial values and authentic final counters/result construction. |
| ZE-64 | Actual graph text/vector/hybrid execution and work across calls, eligibility and full-live preparation. |
| ZE-66 | Actual public Rust graph apply/query/get/close observations. |
| ZE-70 | Canonical qualification's Rust/C/Swift boundary fixtures. It transitively requires ZE-69, which requires real C results, Cypher read/mutation/search and the Rust facade. |

These explicit edges are intentionally partly redundant for discoverability. ZE-70 is the smallest single additional transitive gate for the currently inspected public-surface dependency graph; root must validate cycles before applying it. ZE-46/40 consolidation, retention and recovery are also real requirements reached through the execution/public chain; do not treat retained-file fixtures as public retention/recovery proof. Add the new interval component as a mandatory ZE-76 prerequisite once created. No reverse edge from the independent component to ZE-76.

The source confirms the distinction: `crates/zeppelin-embed/src/lifecycle/native_graph.rs:998-1015` has a test-support bundle installer; `:1047-1083` is a crate-private admitted consumer. `query/completed.rs:168-180,338-365` has genuine owned results but explicitly receives final counters/outcome from the future final driver. Live ZE-39/40/50/53/64/66/68/69/70 remain todo/blocked. Qualification requires shared actual-site counts and both public paths (`docs/graph/plans/qualification.md:116-147`), not fabricated public observations from those kernels.

## Existing accounting and exact missing behavior

All following `src/` paths are under `crates/zeppelin-embed/`.

- `src/lifecycle/stats.rs:38-54,106-118`: one shared `AccountingState` under one mutex enforces budgets, records current resident reservations and a **lifetime** peak. Successful reserve/grow passes this seam; failed budget checks return before mutation. `:267-344` owns shrink/drop release.
- `src/property_graph/resources.rs:16-47`: `GraphResources` clones the same store authority, rejects configurations above 256 MiB, and exposes current/lifetime peak. Its documentation explicitly says the peak starts when Accounting is created. There is no interval API. An old 4,096-byte spike hides a later 192-byte spike if the lifetime maximum is presented as that later interval's peak.
- `src/property_graph/query/resources.rs:50-71,125-139,213-239`: real `QueryMemory`/`QueryArena` capacities feed that same authority. `src/property_graph/staging/memory.rs:135-152` and `storage/memory.rs:21-42,68-83` nest writer/storage reservations in it. `crates/zeppelin-embed-cypher/src/shared_resources.rs:1-73` borrows QueryMemory for outer compilation. No new allocator or participant budget is needed.
- `src/property_graph/query/runtime.rs:144-152,253-286` already has cumulative typed work counters and pre-consumption checks. This component changes none of them. Public integration, any missing actual-site counts, retained completed payload metrics, canonical/fence retention and real WAL/sync attribution remain ZE-76 obligations.

The component supplies exactly the declared measurement interval requested in `qualification.md:143`, over reservations that already exist. It is neither a new budget nor an aggregate proof for unfinished paths.

## Proposed flat ticket text

**Title:** Measure exact shared allocation peaks over declared intervals

**Epic/type/themes:** E9 / story / graph, correctness

**Prerequisites:** closed ZE-49 (shared accounting authority) and root's accepted ZE-76 scheduling/ownership note; pin current integrated main before execution. Existing ZE-37/45/55/60 producers are inspected inputs, not interfaces to edit. No dependency on unfinished ZE-61/145.

**Goal:** Add an allocation-free, store-bound interval observation API to existing GraphResources/Accounting. Capture absolute start/current/peak shared reserved bytes at reservation events, including transient overlaps. Keep the lifetime peak and every numerical budget unchanged. One observation can be active per Accounting; a concurrent observation returns typed Busy without restricting readers/writers. Finish or early drop releases the observation slot. Independently prove exact small deltas, old-history exclusion, failed-reservation exclusion, concurrent overlap, no observer allocation, close survival and reuse. This is completed interval instrumentation over compiled producers; original ZE-76 retains all public execution/write/result/retrieval/retention criteria.

**Acceptance:** all eight named tests below and narrowly affected existing accounting checks pass after literal observed RED. No approximation, polling, per-participant sum, new engine allowance or reset of the lifetime peak. The observer owns no Store/view/query lease. Evidence states the metric covers reserved managed bytes, not physical allocation success, mapped residency, process footprint or detached application payloads.

## Exact ownership and API

Allowlist:

1. `crates/zeppelin-embed/src/lifecycle/stats.rs`: graph-feature-gated interval state/module; one successful-add event hook under the existing mutex. Preserve current accounting/budget/drop behavior.
2. New `crates/zeppelin-embed/src/lifecycle/stats/allocation_interval.rs`: fixed interval state, guard, snapshot and typed error; focused private tests only if poison cleanup cannot be exercised externally.
3. `crates/zeppelin-embed/src/property_graph/resources.rs`: public graph-scoped reexports and `begin_allocation_interval` forwarding to that same Accounting.
4. New `crates/zeppelin-embed/tests/graph_memory_intervals.rs`.
5. `crates/zeppelin-embed/Cargo.toml`: only one `[[test]]` registration named `graph_memory_intervals`, `required-features = ["graph-cypher"]`. Root serializes this additive registration with ZE-61's manifest changes; no other shared edit is authorized.
6. `tasks/evidence/<assigned-key>-allocation-intervals.md` and its small exact-command logs.

No ZE-61 retrieval/storage preparation edits, ZE-145 expression/list/input/catalog edits, query runtime changes, completed-result changes, global Stats layout changes, FFI changes, work-limit changes or fixture redesign.

Reviewed intended API:

```rust
GraphResources::begin_allocation_interval(&self)
    -> Result<AllocationInterval, AllocationIntervalError>;
AllocationInterval::snapshot(&self)
    -> Result<AllocationIntervalSnapshot, AllocationIntervalError>;
AllocationInterval::finish(self)
    -> Result<AllocationIntervalSnapshot, AllocationIntervalError>;
// Snapshot: start_reserved_bytes, current_reserved_bytes, peak_reserved_bytes: u64.
// Errors: Busy; accounting synchronization failure; impossible inactive-state error.
```

Keep the new error local to this API; do not expand StoreError/ABI classifications. Guard is non-Clone with private construction and owns only an Arc clone of existing Accounting plus fixed inline ownership state. One optional fixed state sits inside AccountingState; no Vec, Box, registry, new mutex, callback or heap allocation. This is the root-approved allocation-free observation option. Document the fixed accounting-control fields rather than inventing a payload charge or double-counting store control.

Begin, snapshot and finish linearize under Accounting's existing mutex. Begin checks Busy before changing anything, then records `start=current` and `peak=current`; existing live allocations are included. Successful reserve/grow updates interval peak only after budget acceptance. Shrink/drop affects current, never lowers peak. Failed reservations change neither current nor peak; a successful reservation followed by a backing-allocation error **does** belong in a reserved-byte peak. Lifetime peak remains untouched by begin/finish.

Finish snapshots and clears ownership in the same critical section; its later Drop must not clear a subsequently started interval. Early drop clears the slot, including error-unwind cleanup. Match existing poisoned-lock cleanup policy in Drop but return typed synchronization errors from fallible observations; never return a fabricated successful snapshot after poison. A guard can observe completion of store close through its Accounting owner; it neither admits work nor retains a graph read lease. A second store has independent observation ownership.

## Ordered implementation and narrow verification

1. Add `graph_memory_interval_ignores_old_higher_peak`. First expose the old lifetime-only answer on the existing API with an initial 4,096-byte reserve/drop, then a fresh 192-byte reserve/drop: expected later interval peak is B+192, not the historical B+4096. Observe the assertion RED. Keep the expected numbers; replace only the observation call when adding the actual interval API. An API compile RED alone is not the semantic evidence.
2. Implement only the API and successful reservation-event hook above. Run that test to GREEN. No producer changes are needed.
3. Add the remaining exact tests below. Use existing small public Store fixture setup, GraphResources, QueryMemory/QueryArena and WriteMemory reservation producers. No fake graph population or publication. Do not inspect implementation-produced peak values to construct expectations.
4. Temporarily omit the interval peak event update; the transient/concurrent test must fail its exact B+192 assertion. Restore exact source and rerun terminal GREEN. Keep literal RED/GREEN plus restored diff evidence; no adverse runner execution.
5. Run only the focused commands below, inspect scope, and commit the assigned ticket's files. Follow the two-failed-fixes escalation rule and 20-minute milestone rule in `/tmp/graph-sol-executor-rules.md`.

Named tests and independent expectations:

| Test | Exact assertion |
| --- | --- |
| `graph_memory_interval_ignores_old_higher_peak` | After old B+4096 peak, new interval starts B, ends B, peaks B+192. Lifetime peak remains at least B+4096. |
| `graph_memory_interval_tracks_growth_and_excludes_failed_reservations` | Reserve 96 and 64, resize first to128: current/peak B+192. Rejected over-budget grow leaves 128-byte owner/current/peak unchanged. Drop both: current B, peak B+192. |
| `graph_memory_interval_records_concurrent_overlap_without_sampling` | Two scoped workers hold128 and64 behind barriers before release; take no snapshot during overlap. After join: start/current B, peak B+192 exactly. No sleeps. |
| `graph_memory_interval_counts_nested_capacity_once` | Existing QueryMemory plus QueryArena<u64>(16), and a distinct writer reservation64 overlap. Expected shared delta is `size_of::<QueryMemory>() + size_of::<QueryArena<u64>>() + 16*8 + 64`; assert arena capacity16. Never sum query/writer/shared observations as separate allocations. Drop all: B. |
| `graph_memory_interval_busy_drop_finish_and_reuse_are_exact` | Second begin returns Busy with unchanged accounting/first observation; early Drop permits another begin; finish permits another begin; a finished guard cannot clear the replacement; another store can observe concurrently. |
| `graph_memory_interval_survives_store_close_without_admission` | Close a real Store while observer lives; observe remaining shared reservations consistently, finish and drop without a read lease or close blockage. No post-close query is run. |
| `graph_memory_interval_observation_is_allocation_free` | Under existing allocation-audit support, isolate begin/snapshot/finish and begin/drop after setup: zero allocation/reallocation events. Assertions/formatting/harness setup are outside the audit span. |
| `graph_memory_interval_poison_returns_error_and_drop_releases_slot` | Private focused poison injection: observations return typed error; Drop clears ownership without panic. Inspect cleanup state only through a scoped test seam; never clear poison in production to report success. |

Commands (execute in assigned worktree; none executed by this planner):

```sh
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher,allocation-audit --test graph_memory_intervals
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher --test graph_query_runtime -E 'test(=graph_resources_share_store_reservations_and_record_transient_peak) | test(=query_arena_charges_actual_capacity_overlap_and_releases_on_failure)'
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --lib -E 'test(stats_bytes_are_conserved) | test(budget_exceeded_is_typed_and_pre_allocation)'
cargo check -p zeppelin-embed --lib
cargo check -p zeppelin-embed --features graph-cypher --lib --test graph_memory_intervals
cargo check -p zeppelin-embed-workspace-tests --features graph-result-test-support --test adversarial_tests
git diff --check
```

If the poison test is a private lib test, run its exact name separately with graph-cypher. Gate the allocation-audit-only test so ordinary graph compilation remains valid. The hook compile is compile-only; no adversarial execution. Run a targeted rustfmt check for edited Rust files, not a formatter that rewrites unrelated files. Keep existing accounting regression semantics, error ordering and budgets.

Original ZE-76's work-ledger and public-path acceptance remains pending with its original owners and added dependencies. ZE-118 retains final integrated workspace/adversarial/coverage/size/dependency-policy/platform/sanitizer/release campaigns; deferring those does not waive ZE-76's unavailable public integration or make this interval component a completed resource qualification.
