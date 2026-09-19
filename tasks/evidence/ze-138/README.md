# ZE-138 typed search-plan lowering evidence

## Scope and source

- Base source: `a3ecc9483723967f5a70fbbf47909b57d3399018`.
- Worktree: `/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-138`.
- Compiler scope only: typed binder output, immutable `GraphPlan` search nodes,
  scoped `compile_read_in` lowering, controls, and PG20. No retrieval execution,
  admission, ranking, public API, TCK, or ABI claim is made.
- First compiled checkpoint archive:
  `/tmp/ze138-compiled-checkpoint-a3ecc948-20260919.tgz`, SHA-256
  `9fce6985656281d05279053e92d8d98cb2a777be8fc58d108fdf0fc917258bfa`.
  Its 15-path SHA manifest is
  `/tmp/ze138-compiled-checkpoint-a3ecc948-20260919.manifest`, SHA-256
  `4f2a42a3305aaf108c690c359305be26a8a1712b57d43bb71c207ddfd8552325`.

Final changed-path spot hashes before commit:

```text
0fb79413142d2410cf62e6805ce27ec21bc365f47d520d596a954b2a48f2ecc0  crates/zeppelin-embed-cypher/src/lowering/search.rs
59897f41a0eaa4d9ac0edecdafe5c5c9be35d970928b9f49b5506700a9f2bf06  crates/zeppelin-embed/src/property_graph/query/plan/validate.rs
185838bc173abc1b4e20fb9b9db615bd6ea86d64beba7c190b2025f90fe0e919  crates/zeppelin-embed-cypher/tests/search_lowering.rs
ec54dd4dfddd25be9822a93e8b40115a4378b3e8706e0a7b841bef0147676d2b  tests/adversarial/graph_search_lowering.rs
5771fc32c77813f26f35db1a61b6e08eb9c1e179b4c97fe9c0ad0ea69a147eb0  tests/adversarial-oracle/src/graph_search_lowering.rs
```

## Hardware and toolchain

```text
Darwin Anups-MacBook-Pro.local 27.0.0 Darwin Kernel Version 27.0.0:
Tue Jul 14 21:42:16 PDT 2026; root:xnu-13432.0.94.501.4~1/RELEASE_ARM64_T6031 arm64
macOS 27.0 (26A5388g)
Apple M3 Max, 16 logical CPUs
rustc 1.93.0 (254b59607 2026-01-19), host aarch64-apple-darwin, LLVM 21.1.8
cargo 1.93.0 (083ac5135 2025-12-15)
```

## Literal RED to GREEN

The core representation test was written before the new types. Its first
compile failed because `SearchOutputs`, the four lossless modes, and the typed
`outputs` field did not exist. The first direct compiler test was also written
before lowering and failed with `Unsupported: CALL requires search lowering`.
After the smallest representation, binder, and lowerer slices, the terminal
focused commands were:

```text
env NEXTEST_RETRIES=0 cargo nextest run -j 4 --status-level fail --final-status-level fail \
  -p zeppelin-embed --features graph-cypher \
  --test graph_query_plan --test graph_query_runtime_control
Summary: 33 tests run: 33 passed, 0 skipped

env NEXTEST_RETRIES=0 cargo nextest run -j 4 --status-level fail --final-status-level fail \
  -p zeppelin-embed-cypher --test binding --test read_lowering \
  --test lowering_semantics --test search_lowering \
  --test lowering_allocation --test runtime_lowering
Summary: 52 tests run: 52 passed, 0 skipped

env NEXTEST_RETRIES=0 cargo nextest run -j 4 --status-level fail --final-status-level fail \
  --features graph-cypher --test adversarial_tests \
  -E 'test(/property_graph_(binding|lowering|search_lowering)_probe/) | test(=one_runner_episode_reaches_required_search_lowering_contracts)'
Summary: 4 tests run: 4 passed, 469 skipped
```

The matching serial libtest runs used `-- --test-threads=1`: core 33/33;
Cypher binder/lowerer/allocation/runtime 52/52; PG11 1/1; PG17 plus PG20
2/2; and the PG20 runner episode 1/1. The higher-ranked callback compile-fail
test also passed:

```text
cargo test -p zeppelin-embed-cypher --doc -- --test-threads=1
test result: ok. 1 passed; 0 failed
```

The tests cover all 21 nonempty legal YIELD subsets, exact aliases and slots,
nullable hybrid components, four request modes, omitted/empty/global
eligibility, two independent searches, Cartesian joins, eager source order,
LIMIT 0, projection, literal and parameter aliases, arithmetic, grouping,
`size`, list indexing, and nested invariant lists. Aggregate-derived values and
entity/property reads remain outside independently reconstructable request
arguments; global `collect(DISTINCT node)` eligibility retains its current
singleton aggregate input instead.

## Allocation and control observations

The search allocation test runs the real system allocator failure at every
observed allocation site while the parser, binder, invariant remap, lowerer,
validator, and returned owner set overlap:

```text
cargo test -p zeppelin-embed-cypher --test lowering_allocation \
  search_lowering_real_allocator_fail_at_each_site_releases_all_backing \
  -- --exact --test-threads=1 --nocapture
search_allocator_sites=228 real_heap_peak=50254 query_reservation_peak=278074 final=56
test result: ok. 1 passed; 0 failed
```

All 228 failures returned `ResourceError::Allocation`, did not enter the
consumer, restored the baseline, and a clean retry observed the same 228 sites.
The authentic `ReadContext` close-first test observed 2,524 clean checkpoints,
fired at the late three-quarter checkpoint, returned `ReadCancelled` before
the consumer, and restored all owners. The existing cumulative work counter
tests stayed green.

## PG20 seeded probe

PG20 is registered additively under `property-graph.search-lowering.*`. Its
oracle imports no engine/compiler type and compares primitive mode,
eligibility, output-kind, eager-order, Cartesian-join, LIMIT-0, and invariant
slot observations. Seeds `0`, `1`, `138`, and `18446744073709551615` each
reported:

```text
ProbeReport { observations: 5, fault_fires: 2, clean_controls: 2, comparator_fires: 6 }
```

The two real failures are a scheduled deadline fire inside the measured
post-binder search-lowering interval and a query-memory budget fire. Each is
followed by a same-seed clean control. One integrated runner episode completed
59 operations with zero violations. All 11 registered PG20 keys were hit only
after the corresponding observation, fault, clean control, comparator receipt,
or release check. Existing PG11 and PG17 probes remain green.

## Directed mutants and restoration

Each source mutation was applied alone, the named test was observed failing,
and the exact hunk was restored before the next mutation:

| Mutation | Focused RED receipt |
| --- | --- |
| `Scan -> Auto` | mode preservation assertion failed; exit 101 |
| hybrid `vector_distance` loses `NULL` | left `ValueKinds(8)`, right `ValueKinds(9)`; exit 101 |
| literal empty eligibility becomes omitted | `Option::unwrap()` saw `None`; exit 101 |
| omit second eager source | generated plan rejected with `Plan(Search)`; exit 101 |
| omit independent Cartesian Join | generated plan rejected with `Plan(Scope)`; exit 101 |

The terminal restored `search_lowering` suite passed 7/7. Immediately after
restoration and before formatting, the two mutated production files exactly
matched their clean checkpoint hashes:

```text
55930aac3a006a4e9aba60a160784b3e7805d242c8089033a19d2c1c111a5a5f  crates/zeppelin-embed-cypher/src/lowering/search.rs
54a1f3aa70d8f0f9f0ef37ed7bf57cc7075c234056d5078256324e00202267a0  crates/zeppelin-embed/src/property_graph/query/plan/validate.rs
```

## Formatting, lint, and preservation

```text
cargo fmt --all --check
PASS

cargo clippy -p zeppelin-embed --features graph-cypher \
  --test graph_query_plan --test graph_query_runtime_control -- -D warnings
PASS

cargo clippy -p zeppelin-embed-workspace-tests --features graph-cypher \
  --test adversarial_tests -- -D warnings
PASS

cargo clippy -p zeppelin-embed-cypher --lib --test binding \
  --test read_lowering --test lowering_semantics --test search_lowering \
  --test lowering_allocation --test runtime_lowering --no-deps -- -D warnings
PASS
```

The same Cypher lint without `--no-deps` reaches 14 inherited `cfg(test)`
warnings in unowned `crates/zeppelin-embed/src/kernels/mod.rs` and
`crates/zeppelin-embed/src/lifecycle/mod.rs` (11 `unit_arg`, one
`drop_non_drop`, two `needless_lifetimes`). The scoped owned-target lint is
clean; those files were not edited.

The supplied `/tmp/ze-138-preservation.json` was rehashed mechanically:
`entries=45 mismatches=0`. PG18 and PG19 probe files were not edited. Per the
ZE-118 boundary, no broad campaign, coverage rebaseline, size gate, TCK, ABI,
or ranking claim was run for this compiler-only ticket.
