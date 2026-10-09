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

## S2 one scratch reservation per Expand (ZE-413)

The required BEFORE gate is the immediately preceding S1 final smoke:
92.86 s, 14 operations, 14/14 feature faults, zero violations. No product
change occurred between that smoke and the S2 regression. Hardware is the
S0 host; HEAD 90be79bd plus S2 working changes.

The structured-query regression builds 32 source/target pairs in one
batch, labels every source Anchor, limits the source scan to 0/16/32,
expands one relationship per anchor, and counts at the real admitted
executor seam. Existing test-only preparation_work_capture records each
query scratch's actual work delta and owned bytes. No public work counter
or ABI changes. RED: at 16 anchors, 32 scratch reservations, each
196,608 work units and 197,992 owned bytes. LIMIT 0 already allocates none.

GREEN expand_scratch_is_reserved_once_per_operator: 1 test, 0.28 s.
16/32 anchors each reserve one buffer and charge exactly 196,608 units;
peak_query_bytes is 3,660,001 for both. LIMIT 0 reserves no scratch, peak
3,395,503. Raw logs S2-scratch-red-final.log and
S2-scratch-green-final.log fully read.

ExpandCursor now retains a lazily charged boxed scratch descriptor; the
operator retains its cursor across anchors and occurrence resets. Rebind
checks exact lease/runtime/query-memory ownership and failed state, then
resets resume, type index and direction phase. RangeScratch keeps its
existing require_owner check. Query initialization charges one complete
196,608-unit step before initializing 6,144 slots, with a final checkpoint.
Its guard retains the query memory rather than a temporary mutable runtime
borrow. No prefetch or row batching was added.

The existing foreign-runtime/admission owner tests also drive rebind
rejection followed by clean original-owner reuse: 2 passed (0.02 s).
These same helpers run in the adversarial read-view probe. The directed
Cypher-entry probe passes (1.81 s) across P anchors including empty
expansions and nested MATCH resets. New required coverage:
property-graph.cypher-entry.expand-rebind.clean. Existing budget and
page-scope guards remain strict; no new durable or concurrency fault site
was introduced. Required AFTER smoke and full per-step gates are pending.

```sh
cargo test -p zeppelin-embed --features graph-cypher --lib lifecycle::native_graph::tests::relationship_speed::expand_scratch_is_reserved_once_per_operator -- --exact --nocapture
cargo test -p zeppelin-embed --features graph-cypher --lib lifecycle::native_graph::tests::native_read_cursor_rejects_ -- --nocapture
cargo test -p zeppelin-embed-workspace-tests --features graph-result-test-support --test adversarial_tests cypher_entry_probe_fires_refusals_and_clean_control -- --exact --nocapture
```

S2 AFTER property-graph smoke passes: 88.36 s, 14 operations, 14/14
feature faults, zero violations. Complete S2-adversarial-after.log read.
No unexpected warning/error/FAILED diagnostic. Full S2 per-step gates
are now running serially; no S2 completion/commit is claimed yet.

All S2 per-step gates pass; every completed output line read. Static
gates clean; Cypher 22 graph-search (615.86 s), 14 lowering, 12 execution
tests; format_compat 3 pass/4 default historical ignores; ffi_contract
21 pass; unified 19 pass (51.53 s); both required registry pins 1 each.
Logs S2-*.log, prescribed commands .ctx/run-gates.py S2. The scratch and
owner regressions and Cypher directed probe are also green as recorded
above. No query timings claimed before the prescribed S4/S5 measurements.

## S3 authoritative rows and bound-node proof (ZE-414; in progress)

BEFORE is S2's completed AFTER smoke (88.36 s, 14/14 faults, zero
violations). HEAD fc707132 plus S3 working changes; same S0 hardware.

RED outgoing_expand_reads_each_relationship_once: 300 parallel edges
between two adopted document nodes required 1,805 Lookups for pairs,
exceeding 2E + 2S + 8 = 610. GREEN: pairs 606 untyped/607 typed, counts
602/603, 1 test (43.40 s). Literal ordered [1, RelId 1..300, 2] rows and
with_original_node_sources agree, including LIMIT 0/1/255/256/257/300/301.
Deletion of key parallel-0 revision 2 excludes relationship 1 after
reopen; detaching target 2 and reopening produces no rows on both paths.
Complete S3-authoritative-{red-final,green}.log read.

expand_from now returns the authoritative RelationshipRow it already
validated. The adjacency-versus-authoritative bound/neighbor/type guard
remains before liveness. ExpandCursor retains a lazy bound-node proof
only within its exact node/admission and clears it on rebind. Every edge
still checks the neighbor even if the bound node is tombstoned, so hidden
edges cannot conceal a corrupt required endpoint. Empty implicit nodes
with no edges require no graph-state record. The duplicate relationship
lookup in scan_inner is removed. Undirected incoming self-loop suppression
uses verified source == target. Actual 64-byte authoritative row copies
replace the former 48-byte adjacency row charge. Public component expand
and visible retain their existing verification paths.

Existing tiny ordered oracle passes (0.56 s): multiple types, self-loop,
parallel edges, cycle, zero/full-width IDs, adopted ZGOP v2 endpoints,
tombstoned sources/targets, two-hop paths, incoming/undirected and excluded
shapes. Native hidden-endpoint/property/label/type guard passes (0.08 s).
Directed Cypher entry passes (2.49 s), including a new WITH/DESC read
visiting implicit nodes before the live source, and checking literal
source v = fixture base, target Guard, and authoritative type R. Required
new coverage: property-graph.cypher-entry.expand.authoritative-row.

Correction to S2's wording: the original P-only directed probe has one P
anchor, so it did not specifically demonstrate empty anchors. S2's real
32-source structured regression and owner checks did cover reuse. The
new directed read strengthens expand-rebind.clean coverage; ZE413's
resolution now links this correction. No S2 gate result changed.

Required S3 AFTER smoke passes: 88.69 s, 14 operations, 14/14 feature
faults, zero violations. All 34 output lines read, only expected scheduled
checksum/I/O error observations. Full S3 per-step gate sequence is running.
No S3 completion or timing target is claimed yet.

```sh
cargo test -p zeppelin-embed-cypher --features zeppelin-embed/graph-cypher --test graph_search relationship_speed::outgoing_expand_reads_each_relationship_once -- --exact --nocapture
cargo test -p zeppelin-embed-cypher --features zeppelin-embed/graph-cypher --test graph_search incident_sources::ze404_incident_source_preserves_expand -- --exact --nocapture
cargo test -p zeppelin-embed --features graph-cypher --lib lifecycle::native_graph::tests::expression_tests::native_expression_reads_real_properties_labels_types_and_text -- --exact --nocapture
cargo test -p zeppelin-embed-workspace-tests --features graph-result-test-support --test adversarial_tests cypher_entry_probe_fires_refusals_and_clean_control -- --exact --nocapture
```

All S3 per-step gates pass; every output line read. Cypher: 23 graph-search
(628.48 s), 14 lowering, 12 execution; format_compat: 3 pass, 4 default
historical ignores; ffi_contract: 21 pass; unified: 19 pass (49.55 s);
both required registry pins: 1 pass each. Static gates clean. Complete
logs S3-*.log; exact gate commands in .ctx/run-gates.py S3. No format, ABI,
golden or dependency change. No S4/S5 timing target claimed yet.

## S4 bounded page checkpoints and lazy errors (ZE-415; in progress)

BEFORE: S3 AFTER smoke, 88.69 s, 14/14 feature faults, zero violations.
Named RED page_cell_work_is_charged_once: 256 numeric leaf cells made
768 checkpoints for 768 work units. GREEN: at most 3 polls and the same
768 units. Companion cancellation_is_observed_within_one_page checks
typed non-partial cancellation at polls 1, 2, and 3. Initial fixture
configuration was corrected to the existing 256 MiB resident fixture
before the intended RED. Full logs S4-page-red-final.log and focused logs.

Numeric validation and sortedness are unchanged; work is charged at the
bounded 16 KiB page boundary with a final poll before memo publication.
Overflow fence payloads retain their chunk checks. Lookup/routing cell
work is batched; hot-path errors are constructed only on failure.
Focused directory suite: 35 pass (14.15 s); directed probe: 1 pass
(2.42 s), including required relationship-cancel.fire coverage. A final
fence-cell bookkeeping adjustment and all full gates/timings are pending.

Final S4 product checks pass; every completed log line read. Exact page
and cancellation tests: 1 each; directory suite: 35 pass (13.96 s);
directed entry: 1 pass (2.32 s). Clippy rejected the initial lazy closures
as unnecessary_lazy_evaluations; explicit let-Some/else failure branches
passed without lint exceptions. Static gates all clean. AFTER smoke:
88.30 s, 14/14 feature faults, zero violations; complete output read.
Full Cypher: 23 graph-search (622.76 s), 14 lowering, 12 execution;
format_compat: 3 pass/4 historical default ignores; FFI: 21 pass; unified:
19 pass (50.48 s); both required registry pins: 1 each. Logs S4-*.log;
commands .ctx/run-S4-focused.py, .ctx/run-gates.py S4 fmt clang-format,
.ctx/run-gates.py S4 cypher-tests, and scripts/adversarial.sh smoke
--campaign property-graph --profile none. Fresh-addon timings follow
the S4 engine commit. No format, ABI, golden or dependency change.

### S4 rebased release verification and measurements

The release-only rebase onto e2db37a6 preserves this independent engine
implementation. S4 engine HEAD: 4445163fb3195b0449d7fdb0a84592614d36bf81
(original code commit 0a6ebf9c). The later evidence receipt changes docs
only. Main's overlapping ZE-417 commit c78f319d is not incorporated in
this S4 measurement; final integration remains required after S5.

All prescribed gates on the rebased 0.7 HEAD pass; every output line
read. Static logs S4-release-*.log: fmt, both Clippy gates, rustdoc and
clang-format clean. One Cypher attempt was terminated by SIGTERM without
a test failure and is retained as interrupted, not passed. Complete
S4-release-rerun logs: Cypher 23 graph-search (625.30 s), 14 lowering,
12 execution; format_compat 3 pass/4 default historical ignores; FFI 21
pass (7.06 s); unified 19 pass (51.23 s); both registry pins 1 each.
Exact page/cancellation regressions pass again; 35 directory tests
(16.01 s), directed probe 1 (2.49 s).

Hardware verified: Apple M3 Max, Mac15,9, 128 GiB, macOS 27.0 build
26A5388g, rustc 1.93.0, Node v24.21.0. Both addons rebuilt at 0.7.0;
measured darwin-arm64 SHA-256:
16c292cde5db8ee6b4fe92e0a77145a5330a4e623c15ff286b40a129d956c2a8.
Uptime at timing: 19:11, load averages 3.26/5.70/6.41. Waited for the
one-minute load to return to the plan's baseline range; another session
still had background test threads. These wall-clock numbers are
supporting measurements, not a deterministic qualification. Prepared
store is read-only; complete filename/SHA-256 maps before and after
match the original S0 map: 221 files, zero changed entries.

```sh
scripts/cy_time.sh /Users/aghatage/Documents/code/zeppelin-embed-worktrees/rel-speed /Users/aghatage/Documents/code/zeppelin-embed/tasks/evidence/ze-perf-unified/stores
python3 .ctx/hash-store.py S4-before-timing
python3 .ctx/hash-store.py S4-after-timing
```

| Query | Three raw samples (ms) | Median (ms) | S4 target/result |
|---|---|---:|---|
| count all nodes | 18.242, 9.620459, 9.577584 | 9.620459 | supporting |
| count Document label | 9.707208, 9.563958, 9.585959 | 9.585959 | supporting |
| point lookup by node_id | 0.331875, 0.249875, 0.249375 | 0.249875 | supporting |
| 10 docs, LIMIT 10 | 0.622875, 0.587084, 0.587125 | 0.587125 | supporting |
| count relationships | 67.855959, 67.502916, 67.767 | 67.767 | <= 20 / MISS |
| count PERF_LINK | 81.582667, 81.502209, 81.409709 | 81.502209 | <= 20 / MISS |
| all 500 rel pairs | 82.049416, 81.905042, 81.664 | 81.905042 | <= 25 / MISS |
| 2-hop count | 107.081167, 107.521459, 107.434375 | 107.434375 | <= 30 / MISS |
| incoming count | 9795.448541, 9797.388333, 9774.516958 | 9795.448541 | S5 pending |
| undirected count | 17887.439958, 17808.544292, 17857.817167 | 17857.817167 | S5 pending |
| labelled start count | 10496.587208, 10496.352958, 10492.208541 | 10496.352958 | S5 pending |
| id-anchored expand | 68.077375, 67.607, 67.329083 | 67.607 | S5 pending |
| rel pairs, LIMIT 10 | 1.646416, 1.503875, 1.502 | 1.503875 | <= 2 / PASS |

S4 misses are explicit: untyped count 67.767 > 20 ms; typed count
81.502209 > 20 ms; 500 pairs 81.905042 > 25 ms; two-hop 107.434375
> 30 ms. Relationship LIMIT 10 meets 2 ms at 1.503875 ms. S5 still
addresses the incoming, undirected, labelled and ID-anchored access
paths. Complete raw log S4-timings.log read; no unexpected diagnostics.

## ZE-416 / S5: sparse anchors in all directions

The independent branch widens the existing fresh-read source path to
`InRanges`, and numerically merges `OutRanges`/`InRanges` candidates once
per node for undirected expansion. Expand retains its Out-then-In row
phases and self-loop rule. Incident candidates apply the same explicit
node label predicate as ordinary scans, including synthesized Document
and graph-only nodes carrying that label. Cypher represents a simple
first-node label in the existing structured ScanNodes label field.
Canonical ID equality on the first node of a fixed-length relationship
chain lowers to LookupNode; the WHERE remains a residual predicate.
OPTIONAL, writes, WITH, correlated and variable-length anchors retain
their existing eligibility exclusions. No format, ABI, golden or
production dependency changed.

Named RED tests, command:

```
cargo test -p zeppelin-embed-cypher \
  --features zeppelin-embed/graph-cypher --test graph_search \
  anchor_ -- --nocapture
cargo test -p zeppelin-embed --features graph-cypher \
  incident_source_requires_fresh_read_region -- --nocapture
```

All four shape tests failed at the intended seam: incoming, undirected
and Document visited 34 documents; ID made 46 lookups and 22 scans.
Planner incoming qualification failed. The initial ID bound of 10
omitted projection materialization; corrected bound 24 still excludes
the observed RED 46. A first implementation exposed the label-filter
lowering seam and that overstrict ID bound. An intermediate compiler
error was corrected before GREEN; neither attempt is claimed a pass.
Logs `.ctx/S5-shapes-red.log`, `S5-planner-red.log`, and
`S5-shapes-green*.log` preserve those receipts.

Final shape GREEN uses 32 and 64 documents with the same edges. Every
shape has zero document visits and identical work at both sizes:

| shape | Scans | Lookups |
|---|---:|---:|
| incoming R | 18 | 77 |
| undirected R | 52 | 109 |
| Document outgoing R | 26 | 87 |
| graph-only Anchor outgoing R | 10 | 21 |
| graph-only Anchor undirected R | 19 | 22 |
| canonical ID outgoing R | 4 | 23 |

Ordered complete rows and LIMIT 0/1/3/7/20 prefixes match the original
source oracle. The ID oracle uses equivalent `IN [canonical-id]` to
prevent its compiler lowering. Fixtures include document-bound zero
and maximum-width endpoints, parallel edges, a cycle, a self-loop,
multiple relationship types, a graph-only Anchor/Document node, and
tombstoned endpoints after close/reopen. The existing literal outgoing
oracle uses the original fixture unchanged through a shared helper.
Read-lowering cases pin incoming/undirected/chained ID lookup plus
OPTIONAL/WITH/properties/variable-length exclusions.

The adversarial directed probe now exercises incoming, undirected,
labelled and point-anchored expansions with independent expected counts,
zero-document-visit checks, original-source comparison and lazy LIMIT 0.
Four coverage keys register those actual paths. S4's final AFTER smoke
(88.30s, 14/14 feature faults, zero violations) is S5's BEFORE receipt;
there was no intervening product mutation.

S5 verification on the independent branch: all per-step gates pass.
Cypher 27 + 14 + 12 tests (graph-search 638.89s); format_compat 3 pass,
4 historical/default ignores (0.25s); ffi_contract 21 (7.14s); unified
19 (53.15s); both named campaign-registry pins 1 each. Exact planner,
scratch, tiny order oracle, four shape regressions and ID lowering pass.
Directed Cypher entry probe passes (2.37s) after replacing an incorrect
2-scan ID bound with comparison against the independently deoptimized
control; the expansion's resume scan is real work. S5 AFTER smoke passes
in 92.03s: 14 operations, 14/14 feature faults, zero violations. All
completed output lines were read, including expected scheduled fault
errors. A premature Cypher attempt was terminated while waiting on the
artifact lock; its one-line log is preserved and not credited. The serial
completed rerun above is the qualification receipt. Final integration,
fresh S5 timing, final main gates, size and full suite remain pending.

### Integration with already landed ZE-417 and ZE-418

Rebased onto main `f443c7b3`, preserving both regression suites. Main's
suite lives in `relationship_speed_main`; the independent suite keeps
its existing name. Retained memory-accounting helpers distinguish the
statement's page memo from released operator buffers. Both scratch
observers remain wired to actual query allocation. Main's infallible
combined-label filter traversal is retained alongside the independent
structured single-label and canonical ID lowering.

The retained main regressions found two merge omissions, repaired at
their existing seams without weakening tests. Sparse source liveness
must be reused after the incident scan: RED 128 lookups for 32 edges
on 32 sources; GREEN 96 for untyped and typed counts (bound <=100).
Main's minimum-RelId predecessor avoidance, self-loop liveness reuse
and lazy endpoint errors are retained. The independent cursor still
checks owner on rebind and keeps the adjacency mismatch guard.

Main's page regression initially observed 7 checkpoints, then 26 for
leaf probes. The query's final page-validation counter charge now
serves as its final close-first checkpoint before memo publication;
preparation/recovery keep their explicit final checkpoint. Restored
main's numeric leaf-probe batching. GREEN: 128-cell page 6 polls/384
work units; found lookup 20 polls/788 units; absent 19/788. Independent
page-charge and cancellation regressions also pass, as do all 35
public directory tests (13.99s). Directed entry passes (2.46s).

Integrated shape work at both 32 and 64 documents, still zero visits:
incoming Scans/Lookups 15/71; undirected 40/89; Document outgoing
20/74; graph-only Anchor outgoing 9/19 and undirected 17/19;
canonical ID outgoing 3/22. Complete rows and all prefixes match.
Exact scratch reservation remains once at anchors16/32: 196608 work
units, 197992 owned bytes, peak3660001; LIMIT0 has no allocation.

S5 AFTER is integration BEFORE (92.03s); integration AFTER property-
graph smoke passes in 89.42s with 14/14 feature faults, zero violations.
All completed logs read in full, including expected fault diagnostics.
Logs are `.ctx/Integration-*-final.log` plus
`integration-main-{lookups,page}-*.log`; intermediate RED failures are
preserved. Final main qualification and S5 timing are still pending.

### S5 fresh-addon measurements after integration

Source HEAD `1f9731c525be0037715c99a578819586fae129c1`.
ARM addon SHA256
`818e38ff435a90599b49dcb0ee39b925dd2cb3c10c787275e067b49a761230cc`.
Host: Apple M3 Max Mac15,9, 128GiB, macOS27.0/26A5388g,
rustc1.93.0, Node24.21.0. Fresh ARM and Intel archive/addon builds
completed in 1m44s and 1m38s; the harness rebuilt both again (cached
0.02s each). Waited after compilation: pre-timing load3.62/6.59/6.62;
measured uptime02:02, up20:04, load3.63/6.45/6.57. The one-minute load
returned to the plan's 2.1–3.9 baseline range. Another session's core
test remained active; these wall times remain supporting evidence.

Command:

```
scripts/cy_time.sh \
  /Users/aghatage/Documents/code/zeppelin-embed-worktrees/rel-speed \
  /Users/aghatage/Documents/code/zeppelin-embed/tasks/evidence/ze-perf-unified/stores
```

Prepared namespace `perf`: 150000 documents, 500 relationships.
Full filename/SHA256 maps before and after are identical to S0:
221 files, zero changed names or bytes. Maps
`.ctx/prepared-store-S5-before-timing.json` and
`.ctx/prepared-store-S5-after-timing.json`. Complete raw timing and
build logs read; no warnings or unexpected errors.

| Query | raw three runs (ms) | median ms | rows | target ms |
|---|---|---:|---:|---:|
| count all nodes | 17.996084, 9.611125, 9.530125 | 9.611125 | 1 | supporting |
| count Document label | 9.555541, 9.512375, 9.377792 | 9.512375 | 1 | supporting |
| point lookup by node_id | 0.350291, 0.25175, 0.251208 | 0.25175 | 1 | supporting |
| 10 docs, LIMIT 10 | 0.618417, 0.58525, 0.58125 | 0.58525 | 10 | supporting |
| count relationships | 60.825334, 60.73025, 60.734666 | 60.734666 | 1 | 20 |
| count PERF_LINK | 60.485792, 60.282333, 60.510625 | 60.485792 | 1 | 20 |
| all 500 rel pairs | 60.641542, 60.808416, 60.680917 | 60.680917 | 500 | 25 |
| 2-hop count | 72.58275, 72.098708, 72.548292 | 72.548292 | 1 | 30 |
| incoming count | 60.634417, 60.563, 60.614459 | 60.614459 | 1 | 25 |
| undirected count | 166.760417, 167.469875, 167.265291 | 167.265291 | 1 | 45 |
| labelled start count | 64.078458, 63.355583, 63.481708 | 63.481708 | 1 | 25 |
| id-anchored expand | 0.613834, 0.519292, 0.510042 | 0.519292 | 1 | 1 |
| rel pairs, LIMIT 10 | 1.226708, 1.212, 1.225334 | 1.225334 | 10 | 2 |

Explicit final measured misses, allowed by the goal DONE alternative:
- count relationships: 60.734666 ms > 20 ms.
- count PERF_LINK: 60.485792 ms > 20 ms.
- all 500 rel pairs: 60.680917 ms > 25 ms.
- 2-hop count: 72.548292 ms > 30 ms.
- incoming count: 60.614459 ms > 25 ms.
- undirected count: 167.265291 ms > 45 ms.
- labelled start count: 63.481708 ms > 25 ms.

Passes: canonical ID expansion0.519292ms <=1ms and relationship
LIMIT10 1.225334ms <=2ms. No optional S6 or other root-cause work was
started. All raw triples, returned rows and first values are in
`.ctx/S5-timings.log`. Final main gates, archive size qualification,
once final full suite, ancestry check and push remain to be completed.

### Final labelled oracle: preserve document-bound node zero

The final main Cypher gate exposed the retained
`relationship_speed_main::ze418_labelled_preserves_original_rows_and_limits`
regression. Its optimized Source-labelled query included node0's three
relationships (301/302/303); the ordinary structured label-directory
source omitted all three because its fresh seek used node1 as the lower
bound. Changed that one fresh bound to node0. This restores the original
Cypher label-filter semantics for adopted zero-ID documents; resume,
visibility, ordering, label validation and all residual predicates remain
unchanged. No new fault site/mode or durable ordering change is required;
the existing labelled-source registry paths and the retained oracle cover
this read source. Integration AFTER89.42s is the fix's BEFORE receipt.

Exact RED and GREEN command:

```
cargo test -p zeppelin-embed-cypher \
  --features zeppelin-embed/graph-cypher --test graph_search \
  relationship_speed_main::ze418_labelled_preserves_original_rows_and_limits \
  -- --exact --nocapture
```

RED mismatched the complete Source rows (303 versus300). GREEN passes
in48.40s, including Document/Source/combined/Missing label queries and
LIMIT0/1/10/256/301. Both complete logs read; RED preserved in
`.ctx/S5-labelled-main-red.log`, GREEN in `S5-labelled-main-green.log`.
The failed first final-gate attempt was terminated after observing the
failure and is not counted as a qualification pass. All51 completed
lines, including its SIGTERM diagnostic, were read and preserved in
`Final-main-cypher-tests-before-labelled-fix.log`. Fresh timing and final
main gates will be repeated after this source fix.

Zero-label fix AFTER property-graph smoke passes in87.25s:14/14
feature faults, zero violations. All38 log lines read, with only the
expected scheduled fault diagnostics. Log `S5-labelled-zero-after.log`.

### Final S5 measurements after the zero-label fix

Source HEAD `cc9c9f19e09dabd7b6693dde06d58a6138b9b163`.
ARM addon SHA256
`a571ec135e44f6b46c09d8948e0d62b0f9644e6d8dc22da054128cd8476a23c2`.
Same M3 Max Mac15,9, 128GiB, macOS27.0/26A5388g,
rustc1.93.0, Node24.21.0 host and timing command as above.
Fresh ARM/Intel builds took 1m41s/1m39s; the harness rebuilt both
again (cached0.02s each). Uptime02:22, up20:23, load3.44/5.17/6.22;
one-minute load is within the plan baseline. Another session's
Cypher test used one CPU; wall time remains supporting evidence.

Full maps `.ctx/prepared-store-S5-zero-fixed-{before,after}.json`
match S0:221 files, zero changed names or bytes. Complete rebuild and
27-line timing logs read without warnings or unexpected errors:
`.ctx/S5-addon-zero-fixed.log` and `S5-zero-fixed-timings.log`.

| Query | raw three runs (ms) | median ms | rows | target ms |
|---|---|---:|---:|---:|
| count all nodes | 17.64825, 9.395333, 9.336416 | 9.395333 | 1 | supporting |
| count Document label | 9.383583, 9.333583, 9.34 | 9.34 | 1 | supporting |
| point lookup by node_id | 0.331625, 0.245125, 0.248667 | 0.248667 | 1 | supporting |
| 10 docs, LIMIT 10 | 0.60475, 0.59, 0.64225 | 0.60475 | 10 | supporting |
| count relationships | 59.912375, 59.372334, 59.186833 | 59.372334 | 1 | 20 |
| count PERF_LINK | 59.199334, 59.48125, 59.250584 | 59.250584 | 1 | 20 |
| all 500 rel pairs | 59.623292, 59.552125, 59.628541 | 59.623292 | 500 | 25 |
| 2-hop count | 70.957458, 70.90975, 70.78025 | 70.90975 | 1 | 30 |
| incoming count | 59.416708, 59.425708, 59.35875 | 59.416708 | 1 | 25 |
| undirected count | 163.272959, 162.836792, 163.240583 | 163.240583 | 1 | 45 |
| labelled start count | 62.673334, 61.8295, 62.38325 | 62.38325 | 1 | 25 |
| id-anchored expand | 0.583, 0.5065, 0.505125 | 0.5065 | 1 | 1 |
| rel pairs, LIMIT 10 | 1.203667, 1.204541, 1.194375 | 1.203667 | 10 | 2 |

Final measured misses to record in ZE-416 resolution:
- count relationships: 59.372334 ms > 20 ms.
- count PERF_LINK: 59.250584 ms > 20 ms.
- all 500 rel pairs: 59.623292 ms > 25 ms.
- 2-hop count: 70.90975 ms > 30 ms.
- incoming count: 59.416708 ms > 25 ms.
- undirected count: 163.240583 ms > 45 ms.
- labelled start count: 62.38325 ms > 25 ms.

ID expansion0.5065ms <=1ms and relationship LIMIT10 1.203667ms
<=2ms pass. These replace the preceding historical S5 measurements
for final acceptance; no optional S6 work was added. Final main gates,
size qualification, the once final full suite and push remain pending.

### Graph-free size prerequisite: compile graph accounting only with graph

Final-main focused checks passed: Node harness4; fresh memo, corruption,
scratch, page charge, both cancellation tests and planner1each;
public directories35 (13.99s); directed entry1 (2.59s). Full logs read.
The graph-free size build emitted five dead-code warnings for the graph
ledger enum/methods, accounting field and merge method. Under the goal's
no-warning rule this attempt failed and only its owned process tree was
terminated (exit143). Complete114-line output read and preserved in
`.ctx/Final-main-size-before-feature-gating.log`; no size pass claimed.

Compile regression RED:
`RUSTFLAGS='-D warnings' cargo check -p zeppelin-embed --lib`
reported the five unused graph accounting items. Feature-gated the graph
ledger and private accounting machinery with `graph-cypher`, as their
callers already are. Graph-enabled code remains unchanged. Graph-free
builds no longer carry an unused graph-work mutex. One intermediate
check identified the ledger type also needed the same feature guard;
that failure is preserved. Final compile GREEN: exit0,3.36s, no warnings.
Existing `property_graph::resources::work_batch::tests::ze76_request_work_flushes_once_including_failure_prefix`
passes1/1; graph diagnostic saturation/poison coverage remains in its
existing tests. No persisted format, C ABI, golden or dependency change.
No new fault site/mode or operation ordering change is introduced.

Logs `.ctx/S5-graphfree-warnings-{red,intermediate,green}.log` and
`S5-graph-accounting-green.log`, all read in full. Final source changes
require a fresh addon and final-main gate rerun before the once full suite.

### Final S5 measurements after graph-free feature gating

Source HEAD `ad1e2807fec4229d7a5f13432eabb145a92df9db`.
ARM addon SHA256
`12544b1d46e6d88c609e6e997835ce6793e9c34dd1a7f714cb7becc71c806b7a`.
Same M3 Max Mac15,9,128GiB,macOS27.0/26A5388g,
rustc1.93.0,Node24.21.0 host and timing command as above.
Fresh ARM/Intel builds1m41s/1m37s; harness rebuilt both cached0.02s.
Waited for compilation load to settle: uptime02:46,up20:48,
load2.99/5.68/6.90, within the one-minute plan baseline. Another
session's core test remained active; wall time is supporting evidence.

Full maps `.ctx/prepared-store-S5-feature-fixed-{before,after}.json`
match S0:221 files, zero changed names or bytes. Full rebuild and timing
logs read without warnings or unexpected errors. Logs
`.ctx/S5-feature-fixed-addon.log` and `S5-feature-fixed-timings.log`.

| Query | raw three runs (ms) | median ms | rows | target ms |
|---|---|---:|---:|---:|
| count all nodes | 17.58325, 9.431167, 9.462709 | 9.462709 | 1 | supporting |
| count Document label | 9.456291, 9.314375, 9.434959 | 9.434959 | 1 | supporting |
| point lookup by node_id | 0.35475, 0.245625, 0.248167 | 0.248167 | 1 | supporting |
| 10 docs, LIMIT 10 | 0.616583, 0.5745, 0.569542 | 0.5745 | 10 | supporting |
| count relationships | 59.644541, 59.157, 59.539083 | 59.539083 | 1 | 20 |
| count PERF_LINK | 59.362958, 59.195958, 59.142416 | 59.195958 | 1 | 20 |
| all 500 rel pairs | 59.575167, 59.440125, 59.556042 | 59.556042 | 500 | 25 |
| 2-hop count | 71.274292, 71.99925, 71.159917 | 71.274292 | 1 | 30 |
| incoming count | 59.19475, 59.166083, 59.169875 | 59.169875 | 1 | 25 |
| undirected count | 162.9725, 163.732583, 163.27575 | 163.27575 | 1 | 45 |
| labelled start count | 63.090625, 62.670292, 61.862958 | 62.670292 | 1 | 25 |
| id-anchored expand | 0.576458, 0.515708, 0.503375 | 0.515708 | 1 | 1 |
| rel pairs, LIMIT 10 | 1.209833, 1.2, 1.20625 | 1.20625 | 10 | 2 |

Final measured misses to record in ZE-416 resolution:
- count relationships: 59.539083 ms > 20 ms.
- count PERF_LINK: 59.195958 ms > 20 ms.
- all 500 rel pairs: 59.556042 ms > 25 ms.
- 2-hop count: 71.274292 ms > 30 ms.
- incoming count: 59.169875 ms > 25 ms.
- undirected count: 163.27575 ms > 45 ms.
- labelled start count: 62.670292 ms > 25 ms.

Passes: ID0.515708ms<=1ms; relationship LIMIT10 1.20625ms<=2ms.
This table replaces prior historical S5 tables for final acceptance.
Feature-gating AFTER smoke87.61s:14/14feature faults, zero violations;
all38 lines read, including only scheduled fault diagnostics.
Log `.ctx/S5-feature-fixed-after.log`. No optional S6 work added.
Final main gates/size, once full suite, ancestry and push remain pending.

### Final-suite accounting fixtures and descriptor limit

On the same M3 Max/macOS host recorded above, the first final-main full
workspace attempt at `a58623fd` reported seven failures before an unexpected
SIGTERM (exit 143). It did not produce a terminal summary and is not credited
as a pass. All 1353 lines of `.ctx/Final-main-full-suite.log` were read.

Exact RED probes on unchanged main reproduced six old release-fixture
assumptions: statement-lived validation memo capacity (65 or 130 bytes) was
counted as leaked operator backing. The test-only correction subtracts the
view's exact retained memo capacity when comparing operator/preparation
release. Real plan footprints, budgets, typed failures, cancellation, zero
partial rows, repeat-read peak accounting, and full source teardown checks
remain intact. Production code is unchanged.

Each probe used `cargo test -p zeppelin-embed --features graph-cypher --lib
<qualified-selector> -- --exact --nocapture`. GREEN ran on the isolated branch
with `ulimit -n 65536` inherited by Cargo; compilation took 15.11s without
warnings. Qualified selectors and individual results:

| Selector | RED | GREEN (seconds) |
|---|---|---:|
| `lifecycle::native_graph::tests::native_read_path_accounting_releases_transient_capacity` |3899 !=3769 retained bytes|1 pass,0.03|
| `property_graph::query::pattern::mutation::tests::native_eager_cancel_during_drain_releases` |operator-release assertion|1 pass,0.06|
| `property_graph::query::pattern::mutation::tests::native_eager_capacity_limit_rejects_without_partial_rows` |operator-release assertion|1 pass,0.05|
| `property_graph::query::pattern::relational::tests::native_relational_directed_probe_can_fire` |101476 !=101411 retained bytes|1 pass,0.35|
| `property_graph::query::pattern::relational::tests::native_relational_limits_controls_errors_release` |101476 !=101411 retained bytes|1 pass,0.76|
| `property_graph::query::pattern::relational::tests::native_relational_eligibility_singleton_domains` |101476 !=101411 retained bytes|1 pass,0.42|

The seventh selector,
`lifecycle::native_graph::tests::recovery::ze393_duplicate_reader_references_reopen_within_the_recovery_allowance`,
failed at maintenance with `Read(Io(EMFILE))` after 223.17s under the default
4096-descriptor soft limit (hard unlimited; OS per-process maximum 245760).
Unchanged recovery code passed after 216.29s with the 65536-descriptor limit:
2000 nodes, 16 readers, 161621 protected references, 68 distinct marked artifacts;
read-only reopen preserved bytes, writable reopen removed reclaim candidates,
and a subsequent restart retained the data. No recovery gate is relaxed and
no persisted format, C ABI, golden, or dependency is changed.

All exact RED/GREEN logs were read completely:
`.ctx/Full-failure-{RED,GREEN}-{1..7}.log`. The interrupted full-suite attempt
will be repeated on final main with the higher descriptor limit. These focused
passes alone do not constitute a full-workspace pass.
