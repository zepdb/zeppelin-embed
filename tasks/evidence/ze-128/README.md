# ZE-128 aligned response owner evidence

Base main `7028a42f2f4898bb7aa729dc935bd51b4033f2ba`, branch
`codex/ze-128-aligned-graph-results`, Apple M3 Max arm64, 128 GiB,
Rust 1.93.0, nextest 0.9.145, llvm-cov 0.9.0. Exact platform/tool output is in
`host.txt`. Dataset: bounded synthetic owned typed C pools and actual
QueryArena source buffers, with real existing Store accounting and snapshot
control. No authentic graph admission, native conversion or commit is claimed.

## Delivered component

The frozen compiled contract is `owner-contract-proposal.md`; source inventory
is `frozen-source-inventory.json`. Six source paths add an opt-in Rust component
without new third-party dependencies, core edits, C exports, or header changes.

One fallible aligned allocation stores all fourteen C arrays; one fallible node
owns authoritative geometry. Actual padded capacity, node and control bytes are
reserved before allocation. Private admission precedes commit. Expose is a fixed
metadata publication with no registry/fallible lock or allocation, then authentic
infallible accounting-guard release. All 42 root/pool fields and caller-root
aliasing are checked before freeing authoritative allocations. The process-wide
nonreused token prevents cross-registry confusion even for empty results.

The embedded gate replaces a real observed infallible 64-byte first-lock
allocation in this host's std Mutex. Normal preparation/free return typed Busy
without waiting. Private abort alone waits/yields, preserves poison, unlinks,
frees backing and only then releases its charge. Lookup is O(outstanding nodes),
with an explicit configured admission bound; bounded destruction is a separate
two-allocation property. Busy/Poisoned public mapping and retry ownership policy
remain explicit ZE-68/69 integration work. The existing authentic accounting
Drop uses its own cleanup mutex; publication is not claimed lock-free.

An independent outcome cell outside unwind retains known commit generation;
unknown outcomes contain no fresh IDs and cannot publish a prepared success
payload. Real coordinator authentication remains mandatory in ZE-68.

## RED and terminal GREEN

All commands below run from this worktree with the checked-in nextest profile:
four isolated processes, one libtest thread each, no retries. Logs retain every
intermediate failure; their names are chronological, not success assertions.
The readable .log files normalize trailing whitespace only; byte-exact original
terminal output is in raw-terminal-logs.tar.gz with hashes in
raw-terminal-inventory.json.

- `01-layout-red.log`: missing owner/layout API, intended compile RED.
  `02-layout-green.log`: independent 14-array offsets and overflow checks pass.
- `03-owner-red.log`: missing actual prepare/expose/free API, intended compile
  RED. `04-owner-green.log` is a fixture failure, **not GREEN**: the default
  legacy Store was configured above the graph aggregate maximum. The fixture
  was explicitly configured to the actual 256 MiB aggregate, then `05` passes.
- `06-cross-registry-red.log`: two empty registries reused token one and could
  steal the other owner. Global nonreused tokens fix it; `07` passes.
- `08-outcome-red.log`: missing independent outcome cell, intended compile RED;
  `09` passes, including known commit followed by panic and rejected downgrade.
- `10-allocator-check.log` and `11-allocator-debug.log`: actual allocator denial
  detects the first-lock 64-byte std Mutex allocation, terminating SIGABRT. This
  is the observed RED behind the embedded gate, not a waived allocator failure.
  `12-allocator-green.log` passes both actual allocation failures and no-allocation
  publication/free. The diagnostic stage prints were later removed.
- `16-alias-red.log`: caller root inside its returned arena was incorrectly
  accepted for free, exposing clear-after-free UAF. Authoritative whole-arena
  and node extent checks reject it; `17-alias-green.log` passes.
- `23-mutant-*.log`: three sequential product mutants all fail the intended
  assertions with exit 100: suppressed arena deallocation, disabled complete
  geometry comparison, and suppressed final cancellation checkpoint.
  `mutants.json` records exact replacement, command, seed and identical original/
  restored hashes. `mutate.py` restores source in finally blocks. `24` is the
  restored GREEN. No mutant remains in the frozen source.

Final focused owner command, `25-final-owner.log`, **13 passed, 14 other lib
cases filtered out**:

```sh
ZE_TEST_SEED=128 cargo nextest run -p zeppelin-embed-ffi --features graph-cypher --lib -E 'test(graph_result_)'
```

Evidence includes a hand-worked C layout oracle (624 bytes, alignment 8, including
padding); actual alignment of every array; source-independent immutable bytes;
all 42 immutable-field mutations rejected; private, stale, forged, foreign and
root-aliased free refusal; concurrent publish/free, double-free and free/abort;
zero allocator attempts in expose/free; two actual fallible allocation sites;
128 allocations and 128 frees, zero live-byte delta over 64 all-pool iterations;
the same exact counts for 64 separately charged 70,000-byte source abort/free
iterations; actual capacity/peak overlap and query-release equality; registry
full/busy/poison, token exhaustion, copy-work and query-memory rejection.

The seed is a deterministic exhaustive site script, not a randomized generator.
Seed 128 selects 70,128 source bytes. All five cancellation checkpoints fire in
turn and clean every real allocation; the same-seed clean control completes.
Actual CopiedBytes are 0, 0, 65,536, 70,128, 70,128 respectively, and 70,128 for
control. CompletedAbiBytes remains zero here because the real driver/coordinator
owns its single charge from measured `represented_bytes()`.

Final existing C/ABI regression command, `22-contracts.log`, **36 passed, two
existing release-only cases skipped**:

```sh
cargo nextest run -p zeppelin-embed-ffi --features graph-cypher --test ffi_graph_contract --test ffi_graph_error --test ffi_graph_layout --test ffi_graph_header --test ffi_contract --test ffi_header
```

The two unchanged ignored release cases are
`the_committed_header_matches_the_exported_symbol_table_and_the_allowlist` and
`header_gate_passes_twice_in_a_row_after_an_instrumented_build`. ZE-118 retains
`cargo nextest run -p zeppelin-embed-ffi --test ffi_header --run-ignored only`.
They are not owner-component acceptance and were not silently treated as passing.

Strict target checks pass in `26-final-clippy.log` and `27-final-fmt.log`:

```sh
cargo clippy -p zeppelin-embed-ffi --features graph-cypher --lib --tests --no-deps -- -D warnings
cargo fmt --package zeppelin-embed-ffi --check
```

`14-fmt-pre.log` preserves pre-format output. `15-clippy-pre.log` is an initial
non-scoped invocation stopped by 14 inherited core warnings; no core warning was
suppressed or edited. The final target check explicitly uses --no-deps.

## Scoped coverage and deferred qualification

A focused instrumented owner run passed the same 13 tests in `21-coverage.log`:

```sh
cargo llvm-cov nextest -p zeppelin-embed-ffi --features graph-cypher --lib --ignore-filename-regex '(/graph_result/(audit|tests)\.rs$|/graph_result/registration\.rs$^|/crates/zeppelin-embed/)' --json --output-path /tmp/ze-128-evidence/coverage.json -E 'test(graph_result_)'
```

The complete raw report is compressed losslessly as `coverage.json.gz`; its
uncompressed SHA-256 and exact selected source rows are in
`coverage-summary.json`. The redundant registration `$^` alternative matches
nothing; registration production and its two cfg(test) gate controls were both
included. Results: graph_result.rs 175/190 lines (92.11%); outcome.rs 33/36
(91.67%); registration.rs 284/306 (92.81%, includes those two test controls).
The final source differs only by an additional all-pool repetition in the
separate excluded tests.rs file. This is scoped component evidence, not final
whole-FFI or workspace 90% qualification.

ZE-118 retains full feature-qualified workspace/adversarial/per-crate coverage,
release archives, platform sanitizer/Miri qualification and packaging checks
when implementation is complete. Root appends the integrated candidate SHA.
ZE-68/69 retain real native producer/source ownership, exact conversion overlap,
actual staging hook, true commit-window allocation denial and real coordinator
outcome faults, public request/error/free behavior, genuine graph close/reopen
and packaging acceptance. Internal fixtures discharge none of those gates.

## Independent review

Reviewer /root/ze73_fixtures found no concrete blocker in the frozen six-source
inventory and independently reran four directed allocator/publication-race/
root-alias/cancellation checks: four passed, 23 filtered out, all six hashes
unchanged. See independent-review.md and independent-directed.{log,json}. This
is an independent rerun of existing tests and source review, not a new oracle.
The review retains all authentic producer and public integration qualifications.

## Required seeded-runner followup

The initial candidate alone lacked the canonical seeded-runner route required
for changed concurrency/failure paths. The separate followup is documented in
`runner/README.md`: PG16 executes real owner operations through the actual runner,
adds required feature-gated coverage keys and an independent primitive oracle,
and observes missing-route and missing-fault-receipt RED with restored GREEN.
Root integration and this mandatory followup must both complete before closure.
