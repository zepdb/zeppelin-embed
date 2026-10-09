# Unified-store relationship query evidence

Execution of `research/unified-store/plans/relationship-speed.md` S0–S5.
Tickets: ZE-411 (S0), ZE-412 (S1), ZE-413 (S2), ZE-414 (S3), ZE-415
(S4), ZE-416 (S5). S0 is recorded below; engine steps are pending.

## Hardware, data and protocol

2026-10-08, Apple M3 Max (Mac15,9), 128 GiB, macOS 27.0 (26A5388g),
rustc 1.93.0, Node 24.21.0. Worktree:
`/Users/aghatage/Documents/code/zeppelin-embed-worktrees/rel-speed`.
Prepared store: `/Users/aghatage/Documents/code/zeppelin-embed/tasks/evidence/ze-perf-unified/stores`,
namespace `perf`, 150,000 documents and 500 `PERF_LINK` relationships.
Every timing opens it with `readOnly: true`. The complete 221-file name/SHA256
map was identical before and after S0, including lock files.

Each query runs three times after Store open; all raw query latencies are
reported, and median is the middle sorted sample. Open and build time are
excluded. Wall clock is supporting evidence; deterministic engine regressions
belong to S1–S5. The addon rebuild is mandatory before every table, and a
failed build aborts the harness. Query errors abort the run. Counts/IDs returned
as BigInt are serialized as decimal strings without changing timing values.

## S0: fresh-addon baseline (ZE-411)

Engine HEAD: `4ab78da3070ad1a9576ec1493c571001736475a3`. Engine source was unchanged; harness and FFI test
invocation changes were pending commit. Fresh arm64 addon SHA256:
`199672d0c2e0b7df45b2a43477e24205ffc9764cb8f4299f8399ea68329be8dc`. Both arm64 and Intel prebuilds rebuilt successfully.
Uptime at query start: 15:53; load averages 2.52, 3.58, 4.31.

Exact command from the worktree root:

```sh
scripts/cy_time.sh /Users/aghatage/Documents/code/zeppelin-embed-worktrees/rel-speed /Users/aghatage/Documents/code/zeppelin-embed/tasks/evidence/ze-perf-unified/stores
```

The original scratchpad `cy_time.sh` now delegates to this worktree-owned
harness. Full output: `.ctx/S0-timings-green.log` in the worktree.

| Query | Raw ms (three samples) | Median ms | Rows | First row |
|---|---|---:|---:|---|
| count all nodes | 18.227291, 9.351167, 9.218666 | 9.351167 | 1 | `["150000"]` |
| count Document label | 9.273125, 9.188458, 9.257542 | 9.257542 | 1 | `["150000"]` |
| point lookup by node_id | 0.342125, 0.252459, 0.256667 | 0.256667 | 1 | `["00000000000000000000000000001388"]` |
| 10 docs, LIMIT 10 | 1.209167, 1.170333, 1.194333 | 1.194333 | 10 | `["00000000000000000000000000000001"]` |
| count relationships | 217.671375, 216.762417, 216.578125 | 216.762417 | 1 | `["500"]` |
| count PERF_LINK | 231.364459, 231.168166, 231.008916 | 231.168166 | 1 | `["500"]` |
| all 500 rel pairs | 233.924458, 231.6815, 231.888042 | 231.888042 | 500 | `["00000000000000000000000000000001", "00000000000000000000000000000002"]` |
| 2-hop count | 279.95125, 279.854625, 280.149791 | 279.95125 | 1 | `["0"]` |
| incoming count | 19557.680791, 20316.344542, 19438.416834 | 19557.680791 | 1 | `["500"]` |
| undirected count | 33408.809042, 34613.200334, 33627.554542 | 33627.554542 | 1 | `["1000"]` |
| labelled start count | 19982.704583, 20192.958959, 20003.496625 | 20003.496625 | 1 | `["500"]` |
| id-anchored expand | 222.494292, 220.576958, 221.175084 | 221.175084 | 1 | `["00000000000000000000000000000002"]` |
| rel pairs, LIMIT 10 | 4.994167, 4.82275, 4.764958 | 4.82275 | 10 | `["00000000000000000000000000000001", "00000000000000000000000000000002"]` |

Queries and parameter values are pinned in
`bindings/node/bench/relationship-timing.mjs`. Incoming/undirected/labelled
counts and id expansion remain slow at this baseline; S1–S5 are not yet
implemented. No target attainment is claimed by S0.

## S0 RED and GREEN

- `timing_harness_rebuilds_an_existing_addon_on_every_run`: RED at the old
  guard observed no builds across two runs with an existing prebuild. GREEN
  observes two builds and two timing invocations.
- `timing_harness_refuses_to_time_a_failed_rebuild`: GREEN preserves build
  failure and observes no timing invocation.
- `timing_harness_records_head_and_addon_hash_before_timing`: GREEN records
  the HEAD and exact SHA256 of the rebuilt fixture addon.
- `timing_table_accepts_bigint_query_values`: RED reproduced the live
  TypeError on BigInt. GREEN emits all 13 records, each with three finite
  timing samples and a lossless decimal count value.
- `namespace_tokenizer_open_catches_its_named_panic_probe`: RED with
  `RUSTFLAGS='-D warnings'` failed because the child Cargo build dropped the
  parent's `graph-cypher` feature and emitted five graph-accounting dead-code
  errors. GREEN preserves that feature in the panic-probe child, with no
  production, persisted-format or C ABI change.

The four harness tests passed together. Complete RED/GREEN logs:
`.ctx/S0-red.log`, `.ctx/S0-table-red.log`, `.ctx/S0-green-final.log`,
`.ctx/S0-panic-probe-red.log`, `.ctx/S0-panic-probe-green.log`.

## S0 gates

Every line of the completed logs was read, including the initially failing
FFI warning output. The corrected FFI gate and final static reruns are clean.
Exact commands:

```sh
node --test bindings/node/test/relationship-timing-harness.test.mjs
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --features zeppelin-embed-workspace-tests/graph-result-test-support -- -D warnings
cargo clippy -p zeppelin-embed --features graph-cypher --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --features zeppelin-embed-workspace-tests/graph-result-test-support
xcrun clang-format --dry-run --Werror bindings/node/native/addon.cc
cargo test -p zeppelin-embed-cypher --features zeppelin-embed/graph-cypher --test graph_search --test search_execution --test read_lowering
cargo test -p zeppelin-embed --features graph-cypher --test format_compat
cargo test -p zeppelin-embed-ffi --features graph-cypher --test ffi_contract
cargo test -p zeppelin-embed-workspace-tests --features zeppelin-embed-workspace-tests/graph-result-test-support --test adversarial_tests unified -- --nocapture
cargo test -p zeppelin-embed-workspace-tests --features zeppelin-embed-workspace-tests/graph-result-test-support --test adversarial_tests campaign_registry_is_complete_unique_and_smoke_bounded -- --exact --nocapture
cargo test -p zeppelin-embed-workspace-tests --features zeppelin-embed-workspace-tests/graph-result-test-support --test adversarial_tests feature_campaign_registry_owns_exact_ranges_without_generic_credit -- --exact --nocapture
RUSTFLAGS="-D warnings" cargo test -p zeppelin-embed-ffi --features graph-cypher --test ffi_contract namespace_tokenizer_open_catches_its_named_panic_probe -- --exact --nocapture
```

Results: formatting/static/doc checks clean; Cypher 21+14+12=47 passed;
format compatibility 3 passed with 4 default historical ignores; FFI 21 passed;
unified adversarial 18 passed; both registry pins 1 passed each.
`graph_search` took 653.60 s. A one-second process sample located the long
sparse-fixture wait in read-only reopen's `exact_relationship_membership`,
which the plan explicitly excludes. That production path was not changed.

Limitations: full workspace suite and archive size qualification remain final
S5 gates. GitHub CI is waived by the goal. An additional, unrequested baseline
check `every_family_requires_every_layered_coverage_key` failed with
`property-graph does not require fault.layer.io` at unchanged HEAD 4ab78da3.
The two requested registry pins passed; no coverage contract was relaxed.
Its complete diagnostic is `.ctx/S0-every_family_requires_every_layered_coverage_key.log`.
