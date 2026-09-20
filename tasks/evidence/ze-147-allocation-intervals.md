# ZE-147 allocation interval evidence

ZE-147 is based on `70f8cc0a645bf85577d8d034622c291e586acaa9`.
The accepted Astra plan is preserved in
[`ze-147-astra-plan.md`](ze-147-astra-plan.md), SHA-256
`78ac55f48c4780382e5866949e3e19ebc3ac5748371d05bdc020634b7e471518`.

## Implemented boundary

`GraphResources::begin_allocation_interval` starts one observation on the
existing store Accounting owner. Begin, snapshot and finish linearize under the
existing accounting mutex. One fixed optional state and owner token live inside
`AccountingState`; the non-Clone guard owns only an `Arc<Accounting>`, that
token and an active flag. No allocation, registry, mutex, participant allowance
or numerical budget was added.

The successful reservation-add seam records the interval peak only after budget
acceptance. Current bytes continue to come from the existing shared total, so
shrink and drop lower current without lowering peak. Failed reservations change
neither current nor peak. The lifetime peak and public `Stats` layout are
unchanged. Drop clears only its matching owner token, including poisoned-lock
cleanup, while fallible observation methods return the local typed
`Synchronization` error.

The measurement is exact reserved managed bytes over the declared interval. It
does not measure physical allocation success, mapped residency, process
footprint, detached application payloads, work counters or public execution.

## RED and GREEN

The first semantic test initially used the lifetime-only peak. After an old
4,096-byte reservation and a later 192-byte reservation, run
`6a0c4618-62f9-4a8f-ad3a-4f107c0353e8` failed with actual `577848`
(`B+4096`) versus expected `573944` (`B+192`). The interval API made the same
literal assertion GREEN in run `65f183c9-0061-42c2-8a25-f7841775844c`.

The required product mutant removed only the successful-add interval peak
update. Both barrier-held worker reservations completed and released without a
snapshot during overlap; run `ea7464f8-a755-430a-9951-28780fc1becc` then failed
with actual `573752` (`B`) versus expected `573944` (`B+192`). The exact hook
was restored before terminal GREEN.

```text
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher,allocation-audit --test graph_memory_intervals
```

Run `55329dbd-f67b-4c42-bb0b-87e0bb26e184`: 7 passed. These cover historical
peak exclusion, exact growth and failed-reservation exclusion, unsampled
concurrent overlap, nested query/arena/writer capacity counted once, Busy/drop/
finish/independent-store reuse, close survival, and zero allocator events for
begin/snapshot/finish and begin/drop.

```text
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher --lib -E 'test(graph_memory_interval_poison_returns_error_and_drop_releases_slot)'
```

Run `758e80d3-ad17-4d3e-9e15-1a0ae45eb977`: 1 passed, 610 skipped. The poisoned
mutex returns a typed error; guard Drop recovers only to clear its matching
inline slot and never fabricates a successful observation or clears poison.

## Existing regressions and fixture preconditions

The two prescribed `graph_query_runtime` regressions originally failed before
their assertions because their pre-existing 65,536-byte Store ceilings were
below the current 573,752-byte Store-open reservation. Run
`2d56db2b-e3d2-4b1d-9cc8-939465c70ec6` records both precondition failures.
With root approval, only those two fixtures now use the existing
`MAX_GRAPH_RESIDENT_BYTES` ceiling. The first test's intentional oversized grow
uses that same ceiling as its requested reservation, so it still exceeds the
remaining shared capacity. Its exact `BudgetExceeded`, unchanged 128-byte
owner/current total and lifetime-peak assertions remain intact. The second
test's 1,024-byte query allowance and exact capacity/refusal assertions are
unchanged.

```text
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher --test graph_query_runtime -E 'test(=graph_resources_share_store_reservations_and_record_transient_peak) | test(=query_arena_charges_actual_capacity_overlap_and_releases_on_failure)'
```

Run `939a2890-28bc-438d-9c4b-a02cc9429a73`: 2 passed, 10 skipped.

```text
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --lib -E 'test(stats_bytes_are_conserved) | test(budget_exceeded_is_typed_and_pre_allocation)'
```

Run `775e4140-8093-44f4-b275-30c06eee0b98`: 2 passed, 568 skipped.

All prescribed compile and source controls exited zero:

```text
cargo check -p zeppelin-embed --lib
cargo check -p zeppelin-embed --features graph-cypher --lib --test graph_memory_intervals
cargo check -p zeppelin-embed-workspace-tests --features graph-result-test-support --test adversarial_tests
rustfmt --check --edition 2024 crates/zeppelin-embed/src/lifecycle/stats.rs crates/zeppelin-embed/src/lifecycle/stats/allocation_interval.rs crates/zeppelin-embed/src/property_graph/resources.rs crates/zeppelin-embed/tests/graph_memory_intervals.rs crates/zeppelin-embed/tests/graph_query_runtime.rs
git diff --check
```

The adversarial target was compiled only. No broad/full/adversarial execution,
coverage, fuzz, size, performance, sanitizer, soak, release or platform suite
was run. Those integrated obligations and any broader stale-fixture audit remain
with ZE-118. ZE-76 retains public write/query/result/retrieval/WAL/sync/retention
and Rust/C/Swift resource acceptance.
