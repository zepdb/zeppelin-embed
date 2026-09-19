# ZE-114 — Restore FFI crate line coverage

## Result

The focused FFI coverage gate passes at **3484/3867 = 90.09568140677528%**,
up from **3284/3867 = 84.92371347297647%**. The same five production source
files and the same 3867 measured lines are present in both reports and in
the parent ZE-32 workspace report. Only one integration test file was added;
production code, generated header, dependencies, coverage script, exclusions,
and threshold are unchanged. No new test lines enter the measured inventory.

| Source | Baseline covered / measured | Final covered / measured |
| --- | ---: | ---: |
| `error.rs` | 135 / 193 | 155 / 193 |
| `lib.rs` | 2379 / 2811 | 2552 / 2811 |
| `marshal.rs` | 150 / 155 | 150 / 155 |
| `registry.rs` | 335 / 408 | 342 / 408 |
| `slots.rs` | 285 / 300 | 285 / 300 |
| **Total** | **3284 / 3867** | **3484 / 3867** |

The baseline gate exited 1 despite all selected tests passing. The final
fresh-profile gate exited 0 after every mutation was restored. Top-level
test totals changed from 97 passing / 0 failing / 3 ignored to 112 passing /
0 failing / 3 ignored. Nested panic-probe subprocess summaries are excluded
from these totals. The new `ffi_boundary_validation` binary has 15 tests.

## Environment and exact commands

- Base: `366cdf44ed5cd6b2cccb99e534eba09a58d9e274`.
- Worktree: `/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-114`,
  branch `codex/ze-114-ffi-coverage`, independent of ZE-32, ZE-54 and ZE-113.
- Apple M3 Max, Mac15,9, 137438953472 bytes RAM, native arm64.
- macOS 27.0, build 26A5388g; rustc/cargo 1.93.0; cargo-llvm-cov 0.9.0.
- Dataset: deterministic temporary stores, small literal vector/attribute
  batches, honest caller-owned ABI buffers, and synthetic epoch metadata.

Baseline, before adding tests:

```sh
cargo llvm-cov -p zeppelin-embed-ffi --features abi-panic-probe \
  --fail-under-lines 90 \
  --ignore-filename-regex '(^|/)(registry/|crates/zeppelin-embed-bench|fuzz/|target/)' \
  --json --output-path /tmp/ze-114-qualification/baseline-featured.json \
  > /tmp/ze-114-qualification/baseline-featured.log 2>&1
```

Terminal GREEN, after restoring every isolated mutation:

```sh
cargo llvm-cov -p zeppelin-embed-ffi --features abi-panic-probe \
  --fail-under-lines 90 \
  --ignore-filename-regex '(^|/)(registry/|crates/zeppelin-embed-bench|fuzz/|target/)' \
  --json --output-path /tmp/ze-114-qualification/green.json \
  > /tmp/ze-114-qualification/green.log 2>&1
```

`abi-panic-probe` matches the feature unified by the workspace test consumer.
A preliminary default-feature diagnostic measured only 3841 lines and is
not acceptance. Intermediate `--no-clean` diagnostics guided test selection;
the terminal command above does not use them or `--no-clean`. It is the
workspace lane's unchanged threshold and exclusion expression, scoped to
the FFI package. The parent retains responsibility for the combined workspace
gate; this result is not a claim that the full workspace was rerun here.

Raw reports: [baseline-featured.json.gz](ze-114/baseline-featured.json.gz),
[green.json.gz](ze-114/green.json.gz); logs:
[baseline-featured.log](ze-114/baseline-featured.log),
[green.log](ze-114/green.log). The gzip streams preserve the exact JSON bytes.
[coverage-summary.json](ze-114/coverage-summary.json) gives the explicit
five-file inventory, counts, gate exits and top-level test totals.

## Public contracts and observed failure controls

The added tests exercise the exported C ABI through actual request/result
buffers. They assert typed errors and diagnostics, no mutation for rejected
requests, literal attribute round trips, generation stability, purge-token
lifecycle, cancellation, and caller ownership after rejected frees. Malformed
pointer tests point into real backing storage and are rejected before reads.
Rejected frees retain a valid original allocation for exactly one final free.
Tests sharing the process-global handle-zero error buffer are serialized by
a local mutex. No private production seam or implementation fallback was added.

Each row below is one independently applied mutation, one named test failure
with exit 101, then byte-for-byte restoration before the next row. The full
test names, commands, source paths and restoration hashes are recorded in
[mutation-results.json](ze-114/mutation-results.json). The exact edits and
restoration assertions are in [mutation-controls.py](ze-114/mutation-controls.py).
The terminal full suite proves every named test GREEN on restored source.

| Named test | Deliberate regression / observed failure log |
| --- | --- |
| `open_rejects_unknown_options_and_read_only_handles_reject_writes` | [Admit writes through read-only access](ze-114/mutation-read-only-access.log) |
| `epoch_identity_validates_tags_and_preserves_runtime_compute_and_os_identity` | [Accept nonzero reserved epoch field](ze-114/mutation-epoch-reserved.log) |
| `filter_grammar_rejects_irrelevant_children_bounds_and_values` | [Accept children on a leaf filter](ze-114/mutation-filter-children.log) |
| `filter_values_reject_null_unknown_tags_wrong_columns_and_invalid_bounds` | [Treat null filter value as integer zero](ze-114/mutation-null-filter-value.log) |
| `typed_attributes_round_trip_through_get_and_match_exact_filters` | [Invert returned boolean attribute](ze-114/mutation-returned-bool.log) |
| `malformed_result_frees_preserve_caller_bytes_and_live_allocations` | [Accept zero-sized result with live allocation](ze-114/mutation-zero-sized-search-free.log) |
| `namespace_schema_rejects_invalid_columns_flags_and_epoch_geometry` | [Admit zero-dimensional namespace](ze-114/mutation-namespace-zero-dimensions.log) |
| `error_copy_and_scalar_outputs_reject_invalid_buffers_without_writing_them` | [Report an extra error-message byte](ze-114/mutation-error-copy-length.log) |
| `wrong_result_type_or_missing_count_never_consumes_the_owned_allocation` | [Admit missing count beyond document count](ze-114/mutation-missing-count-bound.log) |
| `purge_busy_and_consumed_tokens_are_typed_without_losing_the_first_request` | [Map busy purge to invalid argument](ze-114/mutation-purge-busy-code.log) |
| `read_only_maintenance_and_purge_retain_access_mode_errors` | [Collapse maintenance access error to internal](ze-114/mutation-maintenance-access-code.log) |
| `filtered_search_reports_cancelled_and_invalid_vector_errors_with_empty_output` | [Collapse cancelled search to internal](ze-114/mutation-filtered-cancel-code.log) |
| `empty_mutation_batches_and_zero_dimensions_never_advance_generation` | [Change empty-ingest error code](ze-114/mutation-empty-ingest-code.log) |
| `scan_cursor_and_timestamp_flags_reject_ambiguous_requests` | [Admit unknown scan-order discriminant](ze-114/mutation-scan-order-admission.log) |
| `search_options_reject_reserved_bits_conflicting_controls_and_unknown_profiles` | [Accept nonzero search reserved field](ze-114/mutation-search-reserved.log) |

No original production defect was established. During test construction an
incorrect expectation that attached durability opens successfully was corrected
to the existing `Unsupported` contract; that test-development failure is not
presented as product RED. The evidence above is the coverage gate RED and the
15 deliberate regressions, all observed and restored.

## Formatting, lint and inventory integrity

```sh
cargo fmt --all -- --check
cargo clippy -p zeppelin-embed-ffi --all-targets --features abi-panic-probe \
  --no-deps -- -D warnings
```

Both exit 0: [fmt.log](ze-114/fmt.log) (empty successful output) and
[clippy-ffi.log](ze-114/clippy-ffi.log). The same strict clippy command without
`--no-deps` exits 101 on 14 existing errors in unmodified core `kernels/mod.rs`
and `lifecycle/mod.rs`: `unit_arg`, `drop_non_drop`, and `needless_lifetimes`.
That failure is preserved in [clippy.log](ze-114/clippy.log). It arises in this
focused dependency feature configuration; the parent owns the combined
workspace lint configuration. No warnings were suppressed in source.

[source-sha256.txt](ze-114/source-sha256.txt) inventories every FFI production
and Rust test file, C fixtures/examples, generated header, allowlist, cbindgen
configuration, manifests, lockfile and coverage script. All 35 pre-existing
inventoried paths were compared byte-for-byte against the base after mutation
restoration; [restoration.json](ze-114/restoration.json) records the checked
paths. Only the new test file differs from that source inventory. The archive
checksums are in [artifact-sha256.txt](ze-114/artifact-sha256.txt).

## Qualification boundaries

This is native arm64 macOS FFI correctness/coverage evidence. The usual selected
FFI tests include C compilation/execution, header drift, panic containment,
ownership and adversarial boundary checks. Three existing tests remain ignored:
two manual release/header/symbol qualifications and the 10000-round soak.
Windows-only binaries have zero tests on this host. This ticket did not run
Windows, sanitizers, Miri, loom, the ignored soak or the full core adversarial
campaign, and does not claim that evidence.

Epoch runtime/compute-unit rows validate identity metadata only: they do not
load models or qualify CoreML/MLX hardware. Attached durability remains explicitly
unsupported. Production behavior and operation ordering are unchanged, so no
new adversarial fault site or registry entry is introduced.
