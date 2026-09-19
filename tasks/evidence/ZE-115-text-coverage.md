# ZE-115: restore text crate line coverage

The unchanged text implementation now has **3,245 / 3,550 = 91.40845070%**
line coverage. Baseline was **2,623 / 3,550 = 73.88732394%**. The exact same
12 source-file inventories and line counts were measured before and after.
There are no production edits, new dependencies, coverage exclusions, threshold
changes, or new in-source test lines. Only integration tests and small invented
fixture artifacts were added.

## Environment and isolation

- Worktree: `/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-115`.
- Branch: `codex/ze-115-text-coverage`; base committed main
  `366cdf44ed5cd6b2cccb99e534eba09a58d9e274`, without ZE-32, ZE-54 or ZE-113.
- Host: Mac15,9, Apple M3 Max, 16 cores, 128 GiB, macOS 27.0 build 26A5388g.
- Rust: rustc 1.93.0 (254b59607, 2026-01-19), rustfmt 1.8.0-stable,
  cargo-llvm-cov 0.9.0.
- Fixture tools already installed: Python 3.11.8 (PyTorch generator), Python 3.9.6 (stdlib tools),
  PyTorch 2.12.1 CPU,
  xxhash 3.8.0; CoreML uses the installed platform `xcrun coremlcompiler`.
- Target directory exclusively `target/ze115-coverage`; llvm-cov uses its
  `llvm-cov-target` child. No main worktree targets/profiles were used.

The initial focused report exactly reproduced each file's counts in the parent
workspace report `/tmp/ze-32-qualification/workspace-coverage.json`. Its test
run passed 37 tests with 9 ignored. The final focused run passes **49 tests,
0 failures, 9 ignored** across 14 suites. The CoreML discovery test also runs
an explicitly checked child-process store query; the helper's ordinary test
invocation does no work unless the parent supplied its fixture marker.

## Added contracts and qualification boundaries

`runtime_contracts.rs` covers real one-layer BERT/GTE evaluation through public
`Bundle` and `MlxRuntime` APIs. Invented deterministic weights use hidden width
4, one head, intermediate width 6, token types and a three-coordinate dense
head. Literal expected vectors come from independently evaluated PyTorch CPU
float64 operations, including attention, layer norm and GELU. Tests compare
mean/CLS/last pooling, 1/3-coordinate outputs, CPU/GPU, differently padded rows,
and 33 rows across the 32-row evaluation boundary, at absolute error <0.00002.
They also check typed unsupported placements and dtypes, missing tensors,
truncated token/mask chunks, overflow, oversized output declarations, exact
row padding, and successful use after errors.

`tokenizer_contracts.rs` tests public bundle tokenization: globally scored
Unigram paths, lowercase and decomposed combining-mark removal, empty and
unknown input, truncation, rectangular masks, special-token preservation and
exact query padding. Insufficient special slots, empty batches and empty
special-token vocabulary fail with their typed contracts.

`coreml_contracts.rs` compiles a portable **150-byte** invented arithmetic
model in each temporary test directory. The model implements exact
`embedding[i] = input_ids[i] + attention_mask[i]`. Tests call the actual
Rust/CoreML native bridge for all four requested compute policies, warmup,
multiple rows, positive/zero/negative/NaN/infinite masks, repeat calls,
invalid paths, zero/overflow/truncated shapes, wrong prediction width and
model sequence length, error recovery, and drop. A subprocess supplies the
model path/sequence environment without mutating the concurrent test process.
Its public `TextStore` ingests via MLX and queries via CoreML, verifies backend
reporting and joins the owner on close. Requested policy is explicitly tested
separately from `observed_compute_units == None`; this proves no actual ANE
placement.

`lifecycle_contracts.rs` tests separate document/query tower routing using
opposite document vectors, serialized ingest/seal, bounded maintenance and
invalid budgets, close/reopen, recovered chunk identity, deduplicated deletes
and missing callers. A present empty CoreML artifact must fail open; removing
it permits clean reopen under the explicitly absent-artifact policy.

See the [fixture provenance](../../crates/zeppelin-embed-text/tests/fixtures/ze115/README.md)
and [hash inventory](ZE-115-text-coverage/fixture-hashes.json). The CoreML fixture
uses Apple's pinned schema; its hand-written generator uses only Python's
stdlib and copies no implementation source or trained weights. The final
fixture files regenerate byte-for-byte, including rustfmt-formatted references.

The advertised external fixture directory contained **no `.zem` files** and
only an **empty** `leaf-v1.5-pair.mlmodelc` directory. The nine pre-existing
ignored production-reference/placement/large-model tests remain ignored.
These small models do not claim production embedding accuracy, minimum-OS
compatibility, cross-model tolerance, or observed accelerator placement.

## RED, can-fire, restoration, GREEN

The exact-inventory coverage gate observed baseline RED (exit 1, 73.8873%) and
final GREEN (exit 0, 91.4085%). It derives source line counts from the unmodified
llvm-cov JSON reports; it rejects any change to the baseline 12-file inventory
or 3,550-line denominator. No source files are excluded from the crate result.

Every added substantive contract test was observed failing under a focused
regression and then passing after byte-exact production restoration. All 11
controls exit 101 at an executed named test assertion, not a build error.
The repeatable [control runner](ZE-115-text-coverage/regression_controls.py)
restores source bytes in `finally`, checks SHA-256, and reruns the same exact
test to GREEN before moving to the next control. It was run from the worktree
root. The runner writes its logs to `/tmp/ze-115-evidence`; committed copies are
adjacent to this report.

| Control | Test (full names and logs retained in control output) | Observed RED |
| --- | --- | --- |
| attention-mask | tiny_transformers_match_independent_cpu_reference_across_pooling_and_chunk_boundaries | Removing mask scale gives 0.09750441 vs oracle 0.07547742. |
| query-routing | paired_towers_keep_query_routing_and_chunk_deletion_across_reopen | Document tower used for queries returns caller 1 instead of 2. |
| unigram-scores | bundle_unigram_uses_global_scores_normalization_and_rectangular_masks | Reversed score sign produces width 4 instead of 5. |
| coreml-mask | coreml_runtime_evaluates_exact_rows_and_masks_under_every_requested_policy | Treating zero as positive yields 12 instead of 11. |
| mlx-placement | mlx_rejects_unsupported_placement_and_malformed_chunk_shapes | Admitting Neural Engine policy as MLX CPU violates refusal. |
| tensor-dtype | mapped_model_tensor_metadata_is_rejected_with_qualified_error_context | Wrong typed error violates UnsupportedDtype contract. |
| padding | token_batches_preserve_every_row_when_padding_and_refuse_shape_loss | Nonzero padding corrupts the expected rows. |
| broken-discovery | discovered_broken_coreml_artifact_fails_open_without_silent_backend_fallback | Ignoring a present broken model wrongly accepts open. |
| query-padding | bundle_query_padding_preserves_special_tokens_and_refuses_truncation | Returning unpadded query violates exact width. |
| coreml-zero-shape | coreml_reports_bad_artifacts_paths_shapes_and_prediction_width_without_partial_results | Bypassing admission allows a zero shape. |
| coreml-discovery | discovered_coreml_query_model_is_reported_and_used_by_the_store | Ignoring a present model reports MLX, failing the child/store assertion. |

[Control summary](ZE-115-text-coverage/regression-controls.log) records each
restored source hash and exact named GREEN. `control-*-red.log` and
`control-*-green.log.gz` retain individual raw runs losslessly. The ordinary empty-marker
`coreml_store_child` helper is exercised through its checked parent control.

## Terminal validation

Commands ran from the isolated worktree:

```sh
CARGO_TARGET_DIR=target/ze115-coverage cargo llvm-cov \
  -p zeppelin-embed-text --lib --tests --json \
  --output-path /tmp/ze-115-evidence/final-coverage.json
python3 /tmp/ze-115-evidence/coverage_gate.py \
  /tmp/ze-115-evidence/baseline-coverage.json \
  /tmp/ze-115-evidence/final-coverage.json
cargo fmt --all -- --check
CARGO_TARGET_DIR=target/ze115-coverage cargo clippy \
  -p zeppelin-embed-text --all-targets --no-deps -- -D warnings
```

All exit 0. The terminal coverage run occurred after all 11 controls restored
production bytes. Baseline used the identical coverage command with the
`baseline-coverage.json` output path. Raw JSON is retained losslessly in
`baseline-coverage.json.gz` and `final-coverage.json.gz`; corresponding complete
logs and the gate program/output are committed alongside this report.

| Source | Baseline covered / lines | Final covered / lines |
| --- | ---: | ---: |
| arch/bert.rs | 93 / 232 | 216 / 232 |
| arch/gte.rs | 0 / 118 | 108 / 118 |
| bundle.rs | 541 / 625 | 571 / 625 |
| epoch.rs | 32 / 32 | 32 / 32 |
| error.rs | 51 / 61 | 51 / 61 |
| ingest.rs | 1277 / 1495 | 1343 / 1495 |
| query.rs | 39 / 39 | 39 / 39 |
| runtime/coreml.rs | 0 / 152 | 143 / 152 |
| runtime/mlx.rs | 162 / 284 | 258 / 284 |
| runtime/mod.rs | 51 / 51 | 51 / 51 |
| tokenizer.rs | 357 / 405 | 378 / 405 |
| tower.rs | 20 / 56 | 55 / 56 |
| **Crate total** | **2623 / 3550** | **3245 / 3550** |

The broader `cargo clippy -p zeppelin-embed-text --all-targets -- -D warnings`
command exits 101 on 14 existing core dependency lints in the clean-main
feature configuration (`unit_arg`, `drop_non_drop`, `needless_lifetimes`).
[Full failure log](ZE-115-text-coverage/clippy.log) is preserved. No core source
was changed; the crate-scoped `--no-deps` command passes without allowances.
The parent integration will check its combined workspace feature configuration.

[Source hashes](ZE-115-text-coverage/source-hashes.json) verify all text Rust
source files, the native bridge, root/text manifests, lockfile and coverage
script are byte-identical to the base commit. A parent read-only review of the
four new test files and transformer generator reported no concrete findings;
it specifically checked independent vectors, chunk boundaries, real CoreML
semantics, recovery, backend reporting and corrupted artifact handling. This
is focused text-crate qualification, not a rerun of the full core adversarial
workspace or a claim that unrelated package gates pass.
