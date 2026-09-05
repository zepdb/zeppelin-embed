"""Render completed Step 19 receipts without rerunning measured processes."""
import json
import sys
from pathlib import Path

root = Path(sys.argv[1])
ordinary = json.loads((root / "ordinary-summary.json").read_text())
diagnostic = json.loads((root / "diagnostic-summary.json").read_text())
source = json.loads((root / "final-source-verification.json").read_text())
assert ordinary["status"] == diagnostic["status"] == "COMPLETE"
assert ordinary["processes"] == 66 and diagnostic["processes"] == 18
assert source["main_matches_measured_after"]
assert diagnostic["full_work_generation_plan_parity"]
labels = {"all": "Intact scan", "tomb": "Deleted scan", "graph": "Intact graph"}
lines = ["""# Step 19: overlap lexical retrieval with query embedding

Completed per-change implementation, focused validation and matched host
measurements. The user requested finishing Step 19 and stopping the roadmap;
Steps 20–33 were not executed. Broad workspace/CI/coverage/long adversarial and
assembled held-out qualification were not run for this change.

The change queues lexical retrieval under one pinned admission before query
embedding. The existing scoped worker joins on success, preparation error,
panic, cancellation and close. Vector preparation is retained across widening;
fusion, scores and text use that original admission. It adds no model, graph,
retrieval/fusion default, persisted-format or core dependency change.

## Measured result

The production-pair graph control improves hybrid API p50 from 2.441833 to
1.597708 ms (-34.57%) and p95 from 5.788042 to 4.730041 ms (-18.28%).
Hybrid p95 improves 11.20% on the intact scan store and 18.06% with deletions.
Scan hybrid p50 improves only about 1%; the useful change there is in the tail.
Single-leg controls fluctuate within 5% and do not establish an overlap gain.
No query quality, score bits, returned text, token IDs or epoch changes occur.

All 66 ordinary processes (4,224 timed calls) and 18 diagnostic processes
(1,152 timed calls) exit 0 and pass aggregation. Diagnostics observe actual
native-embedding/lexical overlap in all 576 after queries, versus zero of 576
before queries. Work, generation, plan, fusion and materialization receipts
match; only process-specific worker thread IDs are excluded from equality.

## Fixture and timing contract

- Host: Apple M3 Max, 128 GiB, macOS 27.0 build 26A5388g.
- Complete FiQA corpus: 57,638 parents / 58,980 chunks; chunking uses 510
  whitespace words with 32-word overlap. This is a **64-query screen**, not
  all 648 judged queries. Selection is unchanged from Step 18: the lowest
  SHA256(`astra18:0x5eed:` + query ID), restored to source order. Original
  selection receipt: `/private/tmp/ze-astra18-host-4b960428/selection.json`.
- k=10 chunks, unchanged retrieval/fusion defaults. Quality uses official
  linear-gain nDCG@10 and labeled Recall@10 after first-parent deduplication
  without refill, retaining fixture judgments including deleted parents.
- Intact/deleted scan stores use clean71-v1 / Arctic m-v2, with the tested
  clean71/tokenizer adapters in both source arms. Deleted means every fourth
  parent removed. The intact graph store uses production LEAF / Arctic m-v1.5
  and the published BEIR FiQA index, graph coverage 1, maintenance mask 1;
  optional maintenance remains pending. Comparisons are within each state,
  not across these different model pairs and index configurations.
- Query placement is CoreML CPU/ANE, fixed 64 tokens, in every cell. Scan
  bundle: `/private/tmp/ze-clean71-4bit-mscs_8rg/faithful/clean71-fp32-normalizer.zem`;
  sidecar: `/private/tmp/ze-clean71-coreml4-_r2_rxbl/clean71-fp16.mlmodelc`.
  Graph bundle: `/private/tmp/ze-model-bundles-v2-c1/leaf-v1.5-pair.zem` and
  its sibling `.mlmodelc`. These are unchanged models, not quantization tests.
- Three alternating independent-process repetitions, AB/BA/AB, 20 warmups
  each. Processes run sequentially, with a load-average <=3.0 start gate and
  a 300-second refusal bound. The runner waited as needed; the gate was not
  relaxed. No competing task GPU/Metal measurements or builds ran during
  authoritative windows. The host is not an exclusively reserved machine.
- The ordinary timer directly wraps `TextStore::query_text`: tokenization,
  embedding when applicable, retrieval, and construction of owned returned
  text. Startup, extra provenance tokenization and JSON serialization are
  outside the timer. No component latency is subtracted or added.
- p50/p95 below are nearest-rank quantiles within each 64-query process,
  then the median of three process quantiles. Ranges retain all repetitions.
  This differs from the BEIR report's linear quantile convention.
- Separate cloned stores per source arm. Final content hashes confirm every
  before/after/source index file is byte identical in all three states.
  Ingestion throughput, cold-cache latency and concurrent-query capacity
  were not measured. These warm results do not qualify every API at 1 ms.

## Ordinary whole-API latency

All values are milliseconds; negative change means faster.

| State | API | Before p50 | After p50 | Before p95 | After p95 | p95 delta ms | p95 change |
|---|---|---:|---:|---:|---:|---:|---:|"""]
for c in ordinary["cells"]:
    b, a = c["before"], c["after"]
    lines.append(f"| {labels[c['state']]} | {c['api']} | {b['p50_ms']:.6f} | {a['p50_ms']:.6f} | {b['p95_ms']:.6f} | {a['p95_ms']:.6f} | {c['p95_delta_ms']:+.6f} | {c['p95_change_percent']:+.2f}% |")
lines.append("""
## Quality and memory

Quality is identical before/after in every row (absolute and relative delta
zero). RSS is the median of three whole-process maximum RSS observations,
including model loading; it is not incremental query memory or peak physical
footprint. No memory-saving claim is made for Step 19.

| State | API | nDCG@10 before = after | Recall@10 before = after | RSS before MiB | RSS after MiB | RSS change |
|---|---|---:|---:|---:|---:|---:|""")
for c in ordinary["cells"]:
    b, a = c["before"], c["after"]
    assert b["ndcg10"] == a["ndcg10"] and b["recall10"] == a["recall10"]
    bm, am = b["median_max_rss_bytes"], a["median_max_rss_bytes"]
    lines.append(f"| {labels[c['state']]} | {c['api']} | {b['ndcg10']:.9f} | {b['recall10']:.9f} | {bm/2**20:.3f} | {am/2**20:.3f} | {100*(am/bm-1):+.3f}% |")
lines.append("""
## Repetition ranges

The graph dense control has one high-p95 repetition on both arms. Its median
must not be presented as a guarantee that every repetition was below 1 ms.

| State | API | Before p95 min–max ms | After p95 min–max ms |
|---|---|---:|---:|""")
for c in ordinary["cells"]:
    b, a = c["before"]["p95_range_ms"], c["after"]["p95_range_ms"]
    lines.append(f"| {labels[c['state']]} | {c['api']} | {b[0]:.6f}–{b[1]:.6f} | {a[0]:.6f}–{a[1]:.6f} |")
lines.append("""
## Actual overlap and queue wait

Separate instrumented binaries use `query-timing`, never `test-support`, and
scratch-only tracing. A common process monotonic clock measures lexical
execution and the actual embedding-owner `embed` call. That native span
includes runtime setup/copy; it is not isolated device-instruction time.
After spans additionally verify native execution lies inside deferred
preparation and vector retrieval starts only after preparation completes.
Intersection is computed from these actual spans, not component medians.
The table pools the 192 observations per state/arm using nearest rank.

| State | Arm | Queries with overlap | Native/lex overlap p50/p95 ms | Embedding queue p50/p95 ms | Lexical queue p50/p95 ms |
|---|---|---:|---:|---:|---:|""")
for s in diagnostic["span_summary"]:
    lines.append(f"| {labels[s['state']]} | {s['arm']} | {s['queries_with_overlap']}/{s['queries']} | {s['overlap_p50_ms']:.6f} / {s['overlap_p95_ms']:.6f} | {s['embedding_queue_p50_ms']:.6f} / {s['embedding_queue_p95_ms']:.6f} | {s['lexical_queue_p50_ms']:.6f} / {s['lexical_queue_p95_ms']:.6f} |")
lines.append("""
Instrumented API p50/p95 below are supporting observations, not the ordinary
headline. Logging affects their timing. Hybrid stage timings overlap, so the
new retrieval interval includes deferred embedding and must not be summed
with the embedding duration.

| State | Before p50/p95 ms | After p50/p95 ms |
|---|---:|---:|""")
for c in diagnostic["cells"]:
    b, a = c["before"], c["after"]
    lines.append(f"| {labels[c['state']]} | {b['p50_ms']:.6f} / {b['p95_ms']:.6f} | {a['p50_ms']:.6f} / {a['p95_ms']:.6f} |")
lines.append("""
## Focused correctness and failures

Test artifact root: `/private/tmp/ze-astra19-bc63nyj_`.

- `red1.json` -> `green1.json`: actual lexical-start and original-snapshot
  tests fail the serial adapter and pass deferred execution. Concurrent ingest
  during preparation leaves admitted revision/text unchanged.
- `text-red1.json` is only an `EpochUndeclared` fixture error. Declaring its
  document epoch gives behavioral RED (`started=false`) in `text-red2.json`,
  then GREEN (`started=true`) in `text-green1.json` through the real public
  TextStore/bounded-channel owner seam with only embedding evaluation faked.
- `cancel-red1.json` / `join-red1.json` expose loss of typed control errors
  in the new preparation-error path. Existing `map_scan_error` restores
  cancellation/timeout; identical commands pass in the corresponding GREEN
  receipts. A deterministic lexical gate proves joins on preparation error,
  panic, timeout and close; an admitted cancellation wins over a late error.
- `join-receipt-red1.json` -> `join-receipt-green1.json` preserves the existing
  adversarial completion receipt before returning preparation errors.
- `early-prepare-plant.json`: moving preparation ahead of admission makes
  four of five core checks fail. `early-prepare-restored.json`: the same
  command/seed 0x5eed passes all five after restoring the measured code.
- `directed-panic.json`: the existing scoped-leg adversarial case passes,
  including vector/lexical panic, both-completed/no-partial checks, same-seed
  clean recovery and its deliberately falsified missing-join oracle. Runner
  fault/coverage needs were inspected first; the existing safe scoped-worker
  boundary remains unchanged. No new persisted or VFS fault mode is added.
- `core-final.json`: five core tests pass, including exactly one embedding
  across three fusion rounds, 2,800 scan dimensions, score-bit/text/work
  parity against ready-vector execution. `text-final.json`: three TextStore
  tests pass, including lexical/zero-k avoiding the embedding owner and the
  original producer error type. Admitted control errors remain core hybrid
  failures; producer errors retain their original type unless control wins.
- `native-public.json`: one real native public materialization case passes
  (100 documents, k=1/10/100 across dense/lexical/hybrid, 3.21 s test runtime).
- `format.json` passes. `clippy-final.json` exits 0 with the existing 25 core
  and five text warnings; the new nested-if warning was fixed before timing.
  These are focused checks, not full-suite/coverage qualification.

Exact focused commands (run from the repository with ZE_TEST_SEED=0x5eed):

```sh
cargo test -p zeppelin-embed --lib lifecycle::hybrid_overlap_tests::astra_19_ -- --nocapture
cargo test -p zeppelin-embed-text --lib --features test-support ingest::query_overlap_tests::astra_19_ -- --nocapture
cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests adversarial::hybrid_fusion::tests::astra_00_scoped_lexical_worker_panic_and_clean_control -- --exact --nocapture
cargo test -p zeppelin-embed-text --test query astra_07_result_materialization_uses_one_admission -- --exact --nocapture
cargo clippy -p zeppelin-embed -p zeppelin-embed-text --lib
```

## Source, commands and raw artifacts

Before revision: `192689b5dfb19a77a3f580aaf3d0f9b667219c5d`.
After is that revision plus `final-production.patch`; all seven changed Rust
file hashes match main and the final measured after archive. Final commit is
recorded after landing in `commit-receipt.json`. The final review verifies 330
baseline files and identical benchmark harnesses. The four identical scratch
adapter files are text `bundle.rs`, `tokenizer.rs`, `epoch.rs` and `Cargo.toml`.
These enable the previously tested experimental bundles and never alter main.
The production-pair graph arm also compiles against these identical adapters.

Normal binaries have core features `[]`; diagnostic binaries have only
`[query-timing]`. Builds use opt-level 3, fat LTO, one codegen unit and the
separate CARGO_TARGET_DIR `/private/tmp/ze-astra17-native-mmcjqffv/target`.
No target-cpu=native or main-target sharing. Build commands, profile, exit,
elapsed time and executable hashes are in `before-build-receipt.json`,
`after-final-build-receipt.json` and both `*-diag-build-receipt.json` files.

`source.json`, `source.patch`, `after-build-receipt.json` and the preserved
`after-initial-api` record the earlier pre-Clippy capture; they are not the
final measured after provenance. The initially reused scan-only baseline was
also preserved as `reused-before-scan-only-api`, then rebuilt for the identical
graph-aware harness on both arms before measurement.

The executed preparation/build/diagnostic/analysis scripts live in repository
`tasks/evidence/astra-19-native/`. `prepare.py` intentionally checks the
pre-change HEAD and references the preserved Step 18 source/store artifacts;
it is not a fresh-install downloader. The frozen `before`, `after`,
`before-diag`, `after-diag` trees retain the exact benchmark source. Use fresh
output names when repeating; the runner refuses existing output directories.

Executed host commands, with H set to this report's absolute directory:

```sh
python3 tasks/evidence/astra-19-native/prepare.py
python3 tasks/evidence/astra-19-native/prepare-graph.py "$H"
python3 tasks/evidence/astra-19-native/build.py "$H" before
python3 tasks/evidence/astra-19-native/build.py "$H" after after-final
python3 "$H/run.py" "$H/manifest.json" "$H/ordinary-runs"
python3 tasks/evidence/astra-19-native/analyze.py "$H"
python3 tasks/evidence/astra-19-native/prepare-diagnostic.py "$H"
python3 tasks/evidence/astra-19-native/add-native-spans.py "$H"
python3 tasks/evidence/astra-19-native/build.py "$H" before-diag
python3 tasks/evidence/astra-19-native/build.py "$H" after-diag
python3 "$H/run.py" "$H/diagnostic-manifest.json" "$H/diagnostic-runs"
python3 tasks/evidence/astra-19-native/analyze.py "$H" diagnostic
python3 tasks/evidence/astra-19-native/report.py "$H"
```

Raw references, relative to this report:

- `manifest.json`, `diagnostic-manifest.json`: exact per-cell command,
  environment, model, fixture, store and result paths.
- `ordinary-runs/receipts.json`, `diagnostic-runs/receipts.json`: all 84 exits,
  executable hashes, load observations and process durations; corresponding
  `complete.json` files mark terminal campaigns, and `*.log` retains raw output.
- Each manifest output directory's `results.json`: every query, score bit,
  text, token identity, epoch and API duration; diagnostics add all work/stages.
- `ordinary-summary.json`, `diagnostic-summary.json`: exact aggregates,
  every repetition's quantiles/RSS/quality and successful parity gates.
- `actual-spans.json`: all 1,152 diagnostic per-query common-clock spans.
- `final-production-source.json`, `final-source-verification.json`,
  `final-production.patch`: final measured/main source verification.
- `final-input-content-hashes.json`: models, fixtures and identical index
  trees; `graph-provenance.json`: graph source/geometry/model identity.
- `diagnostic-source.json`, `native-spans-source.json`: scratch instrumentation.

Step 19 is retained for demonstrated hybrid latency benefit and unchanged
retrieval behavior. Remaining roadmap ideas and broad qualification are
explicitly unexecuted under the user's stop-after-19 instruction. This report
does not replace the separate full-query BEIR/native/competitor comparison.
""")
(root / "REPORT.md").write_text("\n".join(lines).strip() + "\n")
print(root / "REPORT.md")
