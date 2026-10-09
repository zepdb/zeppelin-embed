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

## S1 prerequisite diagnostics (ZE-412; ZE-419)

The required pre-change adversarial smoke failed before any engine edit:

```sh
scripts/adversarial.sh smoke --campaign property-graph --profile none
```

Exit 101 on `codex/rel-speed` HEAD `27a68716`:

```text
campaign=property-graph seed=0 profile=none: identity-history: receipt history differs
```

The same failure was reproduced on a clean detached worktree at original
main/plan baseline `4ab78da3070ad1a9576ec1493c571001736475a3`, with a fresh
Cargo target and clean `git status --porcelain`:

```sh
CARGO_TARGET_DIR=/Users/aghatage/Documents/code/zeppelin-embed-worktrees/rel-speed-baseline-target scripts/adversarial.sh smoke --campaign property-graph --profile none
```

Worktree: `/Users/aghatage/Documents/code/zeppelin-embed-worktrees/rel-speed-baseline`.
It compiled in 35.47 s and failed in 0.49 s, exit 101, with the same exact
seed/profile/diagnostic. All log lines were read; logs are
`.ctx/S1-adversarial-before.log` and `.ctx/S1-adversarial-clean-baseline.log`
in the relationship worktree. The report is at
`tests/adversarial_tests.rs:8636`; the independent receipt-history comparison
is `tests/adversarial-oracle/src/graph_lifecycle.rs:54`.

At the initial failure, engine and adversarial source were byte-identical to the original main
baseline. Main advanced to `e2db37a6` during S0; that commit changes release
versions/changelog, not the relevant logic. The failure precedes the proposed
page memo and concerns write-receipt history, outside the plan's named query
read-path root causes. The initial response stopped without diagnosis. That
was premature: the STOP clause applies to gates that cannot be fixed. The
bounded diagnosis below identifies test-fixture drift against existing
contracts, with no engine change or coverage relaxation.

ZE-419 records the prerequisite diagnosis/fix. ZE-412 returned to todo and is
blocked by it; ZE-413..416 remain blocked in order. A first page-validation
counter regression is drafted but unrun and uncommitted. No S1 engine change
was made. The relationship goal, final main gates/full suite, target
qualification, fast-forward landing and push remain incomplete.

### ZE-419 test-fixture alignment

The native empty-store creation publishes generation 1
(`lifecycle/native_graph/persistence.rs::create_with_high_waters`). The
primitive lifecycle model started at 0. Temporary receipt diagnostics showed
identical entity IDs, revisions and replay flags, but expected installing
generations 1/2 versus actual 2/3 at ArtifactCreate fault. The existing lower
recovery comparator already pins first installing generation 2. Checkpoint
replacement also advances the store generation without changing entity
installing generations; rename faults precede replacement and selector-sync
faults follow it.

The test-only prefix comparator now takes an explicit initial generation.
Standalone logical fixtures retain 0; native fixtures use 1. The lifecycle
runner models checkpoint publication and independently verifies recovered
generation. Complete-state and receipt equality remain strict.

Named regression:
`lifecycle_oracle_preserves_native_creation_and_checkpoint_generations`.
RED: ArtifactCreate fault receipt-generation mismatch (0.16 s).
GREEN: all eight fault/control cases (1.28 s), covering ArtifactCreate,
WalSync, CheckpointReplace and CheckpointSync.

The smoke then progressed to a second obsolete fixture assumption: its
FaultVfs byte probe searched for `graph-wal-*`, while unified native stores
use `wal.ze*`. The probe now uses the existing recovery fixture's file
selection. Named regression:
`adversarial::graph_lifecycle::fault_vfs_qualifies_unified_native_wal`.
RED: `native WAL missing` (0.11 s). GREEN: 1 passed (0.13 s), retaining
barrier power-loss and full-sync byte-equality assertions.

Commands for these regressions:

```sh
cargo test -p zeppelin-embed-workspace-tests --features graph-result-test-support --test adversarial_tests lifecycle_oracle_preserves_native_creation_and_checkpoint_generations -- --exact --nocapture
cargo test -p zeppelin-embed-workspace-tests --features graph-result-test-support --test adversarial_tests adversarial::graph_lifecycle::fault_vfs_qualifies_unified_native_wal -- --exact --nocapture
```

Raw logs: `.ctx/ZE419-diagnostic-red.log`, `ZE419-regression-red.log`,
`ZE419-regression-green.log`, `ZE419-wal-red.log`, `ZE419-wal-green.log`.
Full logs read. Required smoke now passes: 14 operations, 14/14 feature
faults, zero violations (97.99 s). Comparator plants still reject partial
batches, reused IDs, early unlink and lying outcomes (1 passed, 0.62 s).
Standalone oracle plants pass (1 test). Both Clippy commands, fmt, rustdoc
and clang-format pass without diagnostics. Logs: `ZE419-smoke-final.log`,
`ZE419-plants.log`, `ZE419-oracle.log`, `ZE419final-*.log`. Clean-checkout
verification remains before closing ZE-419. No production engine, format,
ABI, or golden was changed.

## S1 statement-scoped validation (ZE-412)

ZE-419 is now closed: clean detached `f1ccec4b` checkout repeated the
required smoke, 34.46 s compile and 96.35 s test, 14 operations,
14/14 feature faults, zero violations, clean status before/after. This is
the repaired BEFORE gate for S1. All lines of `ZE419-clean-smoke.log` read.

Native query mappings now retain a charged directory-page bitset. Its exact
size comes from the mapped length and 16 KiB page buckets: validated framed
pages cannot overlap, each page spans 16 KiB plus framing, and only TreePage
references may access these bits. The mapping's query-memory owner charges
both actual backing capacity and the separately boxed control descriptor.
Allocation refusal propagates; there is no fallback.

First use keeps complete checksum/layout/key-order validation. Every
traversal checks kind, depth, creation generation and extremal ancestor
bounds. Only an already fully validated page may use the header path in
later cursor reads. The proof ends with its query source. Preparation,
recovery and trace sources keep the false/no-op defaults. Leaf point lookup
now searches the proven sorted keys by binary search. Record-block
checksums already had statement mapping admission through
`ValidatedArtifact`; that existing path is unchanged.

RED/GREEN on the S0 hardware, HEAD `f1ccec4b` plus uncommitted S1 changes:

| Named regression | RED | GREEN |
|---|---|---|
| `relationship_speed::relationship_pattern_validates_each_page_once_per_statement` | 16/32 parallel edges: 273/529 full validations | 3/3, correct ordered endpoint IDs, 1 pass (0.44 s) |
| `lifecycle::native_graph::tests::storage_faults::page_memo_does_not_cross_statements` | same-source repeated lookup: 2 to 4 validations | reused proof within each source; positive fresh validation in both statements, 1 pass (0.07 s) |
| `lifecycle::native_graph::tests::storage_faults::corrupt_page_fails_on_first_use_after_memo` | warming repeated lookup: 2 to 4 | fresh statement rejects a flipped sealed-page checksum byte with typed FileChecksum, 1 pass (0.05 s) |
| `cypher_entry_probe_fires_refusals_and_clean_control` | same pages consumed once/twice/once: 181/362/181 | equal positive full validations, independent count oracle and work-limit refusal, 1 pass (2.03 s) |

An intermediate implementation reduced the first regression to 38/70,
exposing framing-only cursor decodes that still repeated full checksums.
Those reads now consume an existing proof and never publish a key proof.
The new coverage key is
`property-graph.cypher-entry.page-validation.statement-scope`.

The touched Cypher-entry fixture also contained a baseline inconsistency:
it ingests exactly 32 documents but expected zero Document nodes. The
original test reproduces the same node-count failure on clean pre-S1
`f1ccec4b`, with the baseline Cargo target (0.12 s). Its primitive expected
counts now include those 32 fixture inputs. No count implementation or
observed-engine-derived expectation was added. `S1-entry-count-baseline.log`
and `S1-entry-green-final.log` contain the RED/GREEN proof.

Commands:

```sh
cargo test -p zeppelin-embed-cypher --features zeppelin-embed/graph-cypher --test graph_search relationship_speed::relationship_pattern_validates_each_page_once_per_statement -- --exact --nocapture
cargo test -p zeppelin-embed --features graph-cypher --lib lifecycle::native_graph::tests::storage_faults::page_memo_does_not_cross_statements -- --exact --nocapture
cargo test -p zeppelin-embed --features graph-cypher --lib lifecycle::native_graph::tests::storage_faults::corrupt_page_fails_on_first_use_after_memo -- --exact --nocapture
cargo test -p zeppelin-embed-workspace-tests --features graph-result-test-support --test adversarial_tests cypher_entry_probe_fires_refusals_and_clean_control -- --exact --nocapture
```

Logs in `.ctx/`: `S1-pages-red.log`, `S1-pages-green-final.log`,
`S1-scope-{red,green}.log`, `S1-corruption-{red,green}.log`,
`S1-entry-red-final.log`, `S1-entry-green-final.log`. All lines read. The
AFTER smoke and remaining per-step gates are pending; no S1 commit or
completion is claimed yet.

The first complete Cypher run passed 19 tests and exposed three pins.
The LIMIT case still expected repeated page validation (5/35); its exact
scan/lookup counts remain 1/1 and 11/11, and the updated full-validation
count is one for both prefixes. Exact GREEN: 1 pass, 0.52 s, including
LIMIT 0 (0 scans/lookups/validations).

Both ZE-402 eligibility tests refused at 100,000 documents because the
added memo pointer enlarged each of 8,192 eagerly reserved mapping slots.
The mapping duplicated an artifact ID already held in ValidatedArtifact.
Using that retained proof's ID removes the duplicate and saves 131,072
charged bytes; all memo backing/control bytes remain charged. Original
24 MiB limits and Store/Cypher parity assertions stay unchanged. Exact
GREEN: both tests pass, 119.31 s; peak bytes 25,092,915 hybrid and
25,091,489 text at n=100,000. Logs S1-cypher-tests.log (RED),
S1-eligible-green.log and S1-limit-green.log (GREEN), fully read.

Required AFTER smoke passed: 14 operations, 14/14 feature faults, zero
violations, 91.68 s; every line of S1-adversarial-after.log read. Final
per-step gate rerun is in progress under S1-final-*.log.

S1 final gate receipt (all output lines read, no warning/error/FAILED
diagnostics outside expected adversarial fault observations): fmt, both
Clippy commands, rustdoc and clang-format clean. Full Cypher: 22
graph-search tests (635.82 s), 14 lowering tests, 12 search-execution
tests. Format fixtures: 3 passed, 4 explicitly default-ignored historical
reader jobs. FFI contracts: 21 passed. Unified pins: 19 passed (49.98 s),
including the repaired native WAL fixture; both required registry pins
passed 1 test each. Commands are the prescribed per-step gate sequence,
run serially by .ctx/run-gates.py S1-final; complete logs S1-final-*.log.

Final exact scope/corruption guards passed (0.06/0.04 s), directed Cypher
probe passed (2.38 s), and required final AFTER property-graph smoke
passed (92.86 s): 14 operations, 14/14 feature faults, zero violations.
The existing fault sites still reject corrupted checksums and injected
I/O failures. All lines of S1-final-{scope,corruption,entry,smoke}.log read.
No persisted bytes, C ABI, golden, dependency or write-side proof changed.
Query timings are deliberately measured after S4 and S5 as prescribed.
