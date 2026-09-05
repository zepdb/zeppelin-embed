# Query budget experiments

[First-wave results](../../tasks/evidence/query-api-under-1ms.md),
[follow-up and experiment register](../../tasks/evidence/query-api-scan-embedding.md).
These scripts do not change model or retrieval defaults.

## API timings without fault-test instrumentation

Use the standalone manifest for production-feature timing:

```sh
CARGO_TARGET_DIR=/private/tmp/ze-query-budget-api-target \
  cargo build --offline --manifest-path tools/query-budget/Cargo.toml \
  --profile bench --features query-timing
cargo tree --offline --manifest-path tools/query-budget/Cargo.toml \
  --features query-timing -e features -i zeppelin-embed
```

The dependency tree must not contain `test-support`. Omit `--features
query-timing` for the matching control with stage timers disabled. The binary
is `release/query-budget-api` under the selected target directory.

The regular benchmark crate unconditionally enables the engine's
`test-support` feature. On the measured revision, its Bit4 kernel observers
take two global mutex locks and allocate a result vector per four-row batch
even with no fault controller installed. Such timings include test machinery;
they are not a measurement of a normal application build. The standalone
workspace prevents those benchmark/dev features from being unified into this
API control. Keep the existing benchmark configuration for adversarial tests.

The standalone target uses the same query harness and does not bypass
validation, checksums, cancellation, or the public API. Clean71 still requires
its custom runtime adapters; the follow-up investigation uses a separate
run-specific manifest in that existing experimental worktree.

## Reproducing the first-wave instrumented baseline

The clean71 bundles require the custom adapters in
`/private/tmp/ze-clean71-smoke-bmoljupi/worktree`, detached at `cf312af`.
Preserve its modifications and use its separate build target:

```sh
cd /private/tmp/ze-clean71-smoke-bmoljupi/worktree
CARGO_TARGET_DIR=/private/tmp/ze-clean71-4bit-mscs_8rg/target \
  cargo build --profile bench -p zeppelin-embed-bench \
  --example query-budget --features text,query-timing
```

Copy the binary from `release/examples/query-budget` under that target directory
to a distinct artifact name before building `--features text` with timers
disabled. Bench uses optimized code with debug information; the prior CoreML
comparison used release. Main's example compiles but its runtime does not
substitute for the experimental adapters.

## Harness

```text
query-budget query BUNDLE FIXTURE FRESH_OUTPUT STORE dense|exact|lexical|hybrid
query-budget tower BUNDLE FIXTURE FRESH_OUTPUT MODELC SEQUENCE cpu|ane
```

`query` checks TextStore row/tombstone geometry and graph coverage, performs
20 warmups, and times public queries including returned text. Set
`ZE_QUERY_COREML` and `ZE_QUERY_COREML_TOKENS` for the fixed query model.
`ZE_BUDGET_GRAPH_COVERAGE` defaults to zero. Before graph qualification, add a
typed assertion of per-query graph-serving counters: coverage is insufficient.

`tower` pads/tokenizes before timing `embed_batch` on the caller thread. It
includes shim allocation, prediction and copying, excluding tokenization,
worker channels and retrieval. Only fitting queries run, without truncation.
`ZE_BUDGET_QUERY_MAX_TOKENS=16` selects the common 200-query diagnostic subset.

`run.py MANIFEST FRESH_LOG_DIRECTORY` runs cells serially. Each JSON cell has
`label`, `command` (argv), and optional `environment`. The manifest encodes
repetitions and alternating order. Receipts include binary hash, load, exit,
wall time and `/usr/bin/time -l` output. The driver waits up to five minutes
for pre-run load average <=3; it does not reserve hardware against other
sessions. Coordinate exclusive performance work separately.

Recorded manifests contain this run's exact commands:

```sh
python3 tools/query-budget/run.py /private/tmp/ze-query-budget-hsslyrjg/e0-manifest.json NEW_LOG_DIRECTORY
python3 tools/query-budget/analyze.py /private/tmp/ze-query-budget-hsslyrjg \
  /private/tmp/ze-clean71-4bit-mscs_8rg/faithful/full-fixture.json
python3 tools/query-budget/analyze_curve.py /private/tmp/ze-query-budget-hsslyrjg
```

To rerun, copy a manifest and change **every cell output path** and any binary
paths; original paths intentionally fail if reused. Analysis writes summaries
in the artifact root. API analysis validates query IDs, hit tuples and stage
accounting before speed claims. Curve analysis validates common query IDs and
stable per-device embedding bits; cosines are not retrieval qualification.

## Export and placement

```sh
/private/tmp/ze-coreml-v3/bin/python tools/query-budget/export_sweep.py \
  /private/tmp/ze-clean71-smoke-bmoljupi/zeppelin-embed-clean71-v1-arctic-m-v2/query \
  FRESH_EXPORT_DIRECTORY \
  --python-packages /private/tmp/ze-clean71-smoke-bmoljupi/reference-packages \
  --python-packages /private/tmp/ze-clean71-smoke-bmoljupi/python-packages
/private/tmp/ze-coreml-v3/bin/python tools/query-budget/placement.py \
  MODEL.mlmodelc FRESH_PLAN.json
```

The exporter checks source eager/trace equality on a synthetic input, exports
FP16 compute, and records source hashes. That is not full CoreML source parity.
Depth truncations must never be deployed. New full-depth shapes also need
full-query source parity and API validation before production routing.

The default placement gate requires every nonconstant op to prefer NE.
`--baseline-preparation EXACT_ALLOWLIST.json` permits explicit diagnostic
exceptions tied to op plus output identity. Strict all-ANE remains separate.
Unknown devices fail. Compute plans are compiler intent, not hardware traces.

## Focused checks

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tools/query-budget -p 'test_*.py'
clang -O3 -fobjc-arc -fblocks -framework CoreML -framework Foundation \
  crates/zeppelin-embed-text/tests/coreml_output_copy.m -o /tmp/coreml_output_copy
/tmp/coreml_output_copy
rustfmt --edition 2024 --check crates/zeppelin-embed-bench/examples/query-budget.rs
git diff --check
```

The native probe checks FP16 special values and zero boxed reads, contiguous
FP32, singleton strides and strided logical output. It is a focused macOS
probe, not workspace coverage or concurrency qualification.
