# ZE-113 — Restore the benchmark crate line coverage gate

## Result

The unchanged Darwin benchmark coverage command now passes without changing
its 90% threshold, exclusions, dependencies, or measured source-line count.
This work also fixes one demonstrated benchmark-statistics bug: overflow of
finite positive samples could produce NaN relative standard deviation (RSD),
which the old `rsd > maximum` comparison accepted. The only implementation
change rejects nonfinite RSD through the existing whole-run rejection budget.

| Snapshot | Covered lines | Measured lines | Line coverage | Exit |
| --- | ---: | ---: | ---: | ---: |
| Unchanged main | 4,534 | 5,258 | 86.23050589577787% | 1 |
| Final restored source | 4,744 | 5,258 | 90.2244199315329% | 0 |

Both reports contain exactly the same 16 benchmark source files. The 13 new
tests live in an external integration-test module; their lines do not enter
the coverage denominator. Final test totals: 148 passed, zero failed, two
intentionally ignored (27 library tests plus 121 frontier tests).

## Environment and scope

- Date: 2026-09-18 America/Los_Angeles (2026-09-19 UTC).
- Host: Mac15,9, Apple M3 Max, arm64, 137,438,953,472 bytes RAM.
- OS: macOS 27.0, build 26A5388g.
- Toolchain: rustc 1.93.0 (254b59607 2026-01-19), cargo-llvm-cov 0.9.0.
- Baseline: committed main `366cdf44ed5cd6b2cccb99e534eba09a58d9e274`.
- Worktree: `/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-113`, branch
  `codex/ze-113-benchmark-coverage`, independent of ZE-32 and ZE-54 changes.
- Fixtures: fixed synthetic reporting/statistics inputs, temporary local
  ledger/calibration files, and controlled public machine/sample-source
  adapters. No downloaded datasets, trained models, or measured throughput.
- Source changes: `src/frontier/measure.rs`, `tests/frontier.rs`, and
  `tests/frontier_contracts/benchmark_contracts.rs` in the benchmark crate.

The root agent reviewed the one-line behavior change and external tests and
reported no concrete finding. No core engine ordering, concurrency, storage,
or failure path changed; no seeded engine fault site is added by a benchmark
statistical rejection. Full workspace qualification remains with ZE-32.

## Exact coverage RED and GREEN

Run from this worktree, with its own target directory and LLVM profiles:

```sh
cargo llvm-cov -p zeppelin-embed-bench --lib --test frontier \
  --fail-under-lines 90 \
  --ignore-filename-regex '(^|/)(registry/|crates/zeppelin-embed/|crates/zeppelin-embed-bench/src/(bin|platform|recall)/|fuzz/|target/)' \
  --json --output-path /tmp/ze-113-qualification/baseline.json \
  > /tmp/ze-113-qualification/baseline.log 2>&1
```

This is the benchmark lane of `scripts/coverage.sh` with only JSON output
options. Before any source edits it exited 1: 135 passed, zero failed, two
ignored; 4,534/5,258 lines. The final command used `green.json` and `green.log`
for the two output paths and exited 0: 4,744/5,258 lines, 148 passed, zero
failed, two ignored. Terminal GREEN was run after restoring all mutations.

Raw archived reports are [baseline.json.gz](ze-113/baseline.json.gz) and
[green.json.gz](ze-113/green.json.gz); their embedded source paths reference
the isolated worktree. Logs are [baseline.log](ze-113/baseline.log) and
[green.log](ze-113/green.log). [coverage-summary.json](ze-113/coverage-summary.json)
retains exact line counts for every measured source file and command exits.

## Correctness RED and smallest fix

```sh
cargo test -p zeppelin-embed-bench --test frontier \
  measurement_rejects_nonfinite_variance_from_finite_samples
```

Observed RED, exit 101: 31 finite positive `f64::MAX` samples returned a
successful `MeasurementResult` with `accepted_run_rsd_percent: [NaN]` and
`discarded_runs: 0`. The named test required variance rejection. Raw output:
[nonfinite-variance-red.log](ze-113/nonfinite-variance-red.log).

At the public `measure_source` seam, the correction changes the acceptance
guard to `!rsd_percent.is_finite() || rsd_percent > maximum`. The same input
now returns `VarianceBudgetExhausted` with one discarded run and one required
accepted run. Finite low-variance samples still pass, including the separately
tested odd-sized run whose literal median is 100 ns. No policy is weakened.

## Contract tests and deliberate failures

Every one of the 13 new tests was observed failing against one isolated
behavioral mutation. Each command was `cargo test -p zeppelin-embed-bench
--test frontier <test-name>` and exited 101 with that test marked FAILED.
The controller restores the original source bytes in `finally` and verifies
byte equality; [mutation-results.json](ze-113/mutation-results.json) records
each exact command, source path, exit, and restored SHA-256.

| Test | Deliberate regression caught |
| --- | --- |
| `populated_user_report_keeps_backend_tier_and_missing_measurements_distinct` | Add scan/graph medians instead of subtracting for payoff |
| `user_statistics_handle_empty_sets_duplicates_and_nearest_rank_boundaries` | Change the recall denominator |
| `seeded_query_order_is_repeatable_and_contains_every_row_once` | Ignore the supplied shuffle seed |
| `kernel_gate_reports_all_invalid_measurements_before_computing_ratios` | Admit zero/negative finite kernel latencies |
| `process_summary_rejects_invalid_counts_and_values_with_actionable_diagnostics` | Admit a zero process observation |
| `compute_calibration_stays_idle_on_load_and_rejects_overflow_before_sampling` | Return measured status for busy preflight |
| `compute_calibration_honors_a_load_veto_after_warmup_without_publishing_a_rate` | Replace the load failure with an unrelated iteration error |
| `attested_compute_calibration_cannot_override_busy_preflight_or_zero_work` | Treat zero work as an idle success |
| `ledger_io_failures_name_the_affected_path_and_preserve_observed_history` | Drop the failed ledger path |
| `ledger_rejects_blank_attestation_and_corrupt_counter_evidence_before_adoption` | Admit an attestation with a blank timestamp |
| `calibration_loading_retains_io_and_json_causes_through_the_roofline_api` | Drop the underlying JSON cause |
| `measurement_odd_runs_use_the_middle_observation` | Select the preceding observation as the odd median |
| `measurement_rejects_nonfinite_variance_from_finite_samples` | Restore the original NaN-admitting comparison |

The controller is [mutation-controls.py](ze-113/mutation-controls.py); its
13 `mutation-*.log` files are alongside it. The calibration load-veto tests
run one warmup iteration and stop before collecting timings. They establish
error/status handling, not saturation-loop numerical correctness or rates.

## Terminal verification and limits

All commands completed after mutation restoration:

```sh
cargo fmt -p zeppelin-embed-bench -- --check
cargo clippy -p zeppelin-embed-bench --all-targets -- -D warnings
git diff --check
```

All exited 0. [fmt.log](ze-113/fmt.log) is empty on success;
[clippy.log](ze-113/clippy.log) records the successful lint build. The exact
coverage command also reran the complete library/frontier subset after the
last source change. No threshold, exclusion, lockfile, or dependency manifest
changed; [source-sha256.txt](ze-113/source-sha256.txt) includes those unchanged
inputs and the three changed source/test files.

The existing ignored full GloVe recall/multi-hour build and quiet-host compute
calibration tests remain ignored. This is native macOS benchmark-contract
evidence, not Windows, native Intel, hosted CI, full graph release, or quiet
host performance qualification. No new throughput or memory-footprint claim
is made. Artifact hashes are in [artifact-sha256.txt](ze-113/artifact-sha256.txt).
