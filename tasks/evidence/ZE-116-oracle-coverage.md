# ZE-116: independent oracle production coverage

Verified 2026-09-18 on Apple M3 Max (Mac15,9), 128 GiB RAM, arm64,
macOS 27.0 build 26A5388g, Rust 1.93.0, cargo-llvm-cov 0.9.0.
Worktree `codex/ze-116-oracle-coverage` starts at
`366cdf44ed5cd6b2cccb99e534eba09a58d9e274`.

The unchanged twelve-file oracle production inventory improved from
**13,961 / 16,225 = 86.0462249614792%** to
**14,673 / 16,225 = 90.43451463790447%**. Including ZE-32's separate
`property_graph.rs` module, the actual combined LLVM result is
**14,698 / 16,250 = 90.44923076923077%**, up from 13,986 / 16,250.
Every production file retains its original denominator.

## Diagnosis and scope

The default cargo-llvm-cov directory exclusion hides the real library at
`tests/adversarial-oracle/src`. The root agent's supplemental export of
completed workspace profiles exposed the below-90% production subtotal.
The default workspace gate and its exclusions were preserved. Supplemental
exports also contain test drivers; none enter the production subtotal here.

The baseline input was the completed workspace run, not a fresh focused run.
Existing oracle consumers in core integration tests cover paths absent from
an oracle-only run. For comparison, the isolated existing oracle unit suite
measured 11,345 / 16,225 = 69.92295839753467%; it is retained in
`ZE-116-oracle-coverage/baseline.json.gz` and is not the acceptance baseline.

This change adds thirteen external integration tests in three files. It
changes no oracle production source, dependency, predicate, coverage policy,
core implementation, or ZE-32 source. The test seams are the oracle's public
primitive byte parsers and observation/checker contracts, as authorized by
the ticket. A checked-in manifest format fixture supplies literal bytes;
there are no engine helper imports. Hand-authored malformed payloads,
literal expected observations, and same-case clean controls exercise:

- Manifest, segment-directory and WAL framing, checksums, reserved fields,
  sequence/range boundaries, schema definitions and corruption diagnostics.
- WAL-prefix, format-refusal and reachability observations, including wrong
  acknowledgements, partial results and cleanup directory-sync evidence.
- Metadata physical columns and canonical replay; bitmap live/dead sets;
  execution receipt identity, counters and lifecycle; complete pruning
  baselines, exact source identities, returned hits and delete WAL records.

## RED and restored GREEN

The production coverage checker fails on the original baseline and passes
on the final export. It requires the exact thirteen-file inventory and
unchanged per-file denominators, and applies 90% to the twelve-file clean
baseline. Its committed raw input fragments contain every oracle source
file record from the original LLVM JSON exports, including segments and
line summaries. Each fragment records the original report SHA-256.

```sh
python3 tasks/evidence/ZE-116-oracle-coverage/check-production-coverage.py \
  tasks/evidence/ZE-116-oracle-coverage/workspace-baseline-oracle.json.gz \
  tasks/evidence/ZE-116-oracle-coverage/workspace-baseline-oracle.json.gz
# exit 1: independent oracle production coverage below 90%
python3 tasks/evidence/ZE-116-oracle-coverage/check-production-coverage.py \
  tasks/evidence/ZE-116-oracle-coverage/workspace-baseline-oracle.json.gz \
  tasks/evidence/ZE-116-oracle-coverage/workspace-final-oracle.json.gz
# exit 0: exact counts above
```

Ten isolated acceptance-predicate mutations each produced an actual named
test failure (exit 101, not a compile failure), then were restored to their
original source SHA-256. `mutation-controls.json` records each exact
substitution, command, named test, restored hash and raw RED log. Examples:

- `manifest_header_rejects_each_malformed_framing_field` detects a disabled
  flags check by requiring the precise `Flags` error at byte 12.
- `column_roundtrip_checks_each_independent_observation_surface` detects
  acceptance of the wrong raw row count.
- `bitmap_algebra_refuses_duplicate_dead_and_uncorrelated_observations`
  detects acceptance of duplicated public results.
- `execution_receipts_require_exact_correlations_counters_and_lifecycle`
  detects a mismatched allow-list threshold.
- `pruning_requires_complete_live_baselines_and_exact_source_evidence`
  detects a wrong delete-WAL record count.

After all mutations were restored, `git diff --exit-code HEAD --
tests/adversarial-oracle/src` passed and the terminal test run passed
**101 tests: 88 existing library tests and 13 new integration tests**, with
zero failures or ignores. The new files are frozen by `frozen-tests.json`.
The independently reviewed tests had no concrete outstanding findings.

```sh
CARGO_TARGET_DIR=target/ze116 cargo test \
  -p zeppelin-embed-adversarial-oracle --lib --tests
CARGO_TARGET_DIR=target/ze116 cargo clippy \
  -p zeppelin-embed-adversarial-oracle --all-targets -- -D warnings
rustfmt --edition 2024 --check \
  tests/adversarial-oracle/tests/storage_formats.rs \
  tests/adversarial-oracle/tests/storage_observations.rs \
  tests/adversarial-oracle/tests/metadata_contracts.rs
```

All pass. Logs: `restored-green.log.gz`, `clippy.log.gz`. The initial RED header
test log predates later cases and contains unused-helper warnings; the
terminal full tests and clippy run are warning-free.

## Actual combined LLVM qualification

The coordinating agent retained the completed main-worktree instrumentation
profiles and backed them up before augmentation. It copied only the three
new tests, verified their SHA-256 hashes, and ran:

```sh
cargo llvm-cov --no-report -p zeppelin-embed-adversarial-oracle \
  --test storage_formats --test storage_observations \
  --test metadata_contracts
cargo llvm-cov report --no-default-ignore-filename-regex \
  --ignore-filename-regex '(^|/)(registry/|crates/zeppelin-embed-bench|fuzz/|target/|rustc/)|/\.rustup/' \
  --json \
  --output-path /tmp/ze-32-qualification/workspace-tooling-augmented.json \
  > /tmp/ze-32-qualification/workspace-tooling-augmented.log 2>&1
```

All thirteen added tests pass under retained instrumentation; export exits
0. cargo-llvm-cov 0.9.0 `--no-report` implies `--no-clean`. An initial attempt
to supply both flags was rejected by the CLI before any changes; the valid
command above preserved the original profiles. The source hashes in
`progress.json` match the root workspace for all twelve clean-base files
except the documented ZE-32 module declaration in `lib.rs`; that file's
coverage count stays 36 / 37. The separate ZE-32 module stays 25 / 25.

| Production file | Original covered | Final covered | Original/final count |
| --- | ---: | ---: | ---: |
| diagnostics_health.rs | 67 | 67 | 67 |
| ffi_bindings.rs | 82 | 82 | 82 |
| fts.rs | 864 | 864 | 1,024 |
| hybrid_fusion.rs | 597 | 597 | 697 |
| ingest_retention.rs | 1,803 | 1,803 | 2,050 |
| lib.rs | 36 | 36 | 37 |
| lifecycle_accounting.rs | 81 | 81 | 81 |
| metadata_filter_planner.rs | 3,406 | 3,735 | 3,945 |
| storage_durability.rs | 2,185 | 2,568 | 2,915 |
| tiering_maintenance.rs | 400 | 400 | 431 |
| vamana_graph.rs | 675 | 675 | 757 |
| vector_execution.rs | 3,765 | 3,765 | 4,139 |
| ZE-32 property_graph.rs, separate | 25 | 25 | 25 |

The raw final oracle file records are `workspace-final-oracle.json.gz`;
machine-readable comparison is `production-coverage-green.json`.
`oracle-coverage-augmentation.log.gz` and `workspace-tooling-augmented.log.gz`
retain the actual main-worktree commands' output.

During development, physical LCOV `DA` maps gave conservative progress of
646 newly covered physical lines, on identical source hashes and exact
unchanged DA key sets. Adding that lower bound to the original JSON covered
count gave 14,607 / 16,225 = 90.02773497688752%. This was scheduling evidence
only. LLVM's physical DA total differs from its line-summary denominator;
we neither substituted DA totals for LLVM counts nor summed covered counts
from different runs. The final actual combined JSON result above supersedes
that estimate. `progress.json` preserves all new physical line numbers and
baseline hashes; `current.lcov.gz` retains the isolated instrumented report.

The original workspace gate, FFI/text qualification, parser qualification,
full adversarial campaign, platforms, and release checks remain separately
owned. This ticket verifies the oracle production subtotal only, using the
already completed full workspace baseline plus its focused augmentation.

Raw command logs are gzip-compressed without altering their bytes; use
`gzip -dc <path>.log.gz` to inspect them.
