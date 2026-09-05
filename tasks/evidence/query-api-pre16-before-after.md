# Pre-Step-16 TextStore query API: matched before and after

Original artifact directory: `/private/tmp/ze-query-pre16-ielk83g_`. Relative
source/raw-result paths below refer to that directory. A [compact committed
archive](query-api-pre16-before-after/README.md) retains the harness, source
patches, metadata, summaries and resource receipts.

2026-09-05. **Complete:** 27 successful independent full-query processes, 17,496 timed API calls, all 648 judged FiQA queries per process, three source revisions and three APIs. Six three-query compatibility pilots are separate from these full results.

Dense default is approximately 1 ms on this warm setup: **0.958833 ms p50 / 1.076667 ms p95**. Lexical reaches **0.926042 ms p50**, but its p95 is **3.323084 ms**. Hybrid improves substantially to **5.524125 / 6.866458 ms** and remains well above 1 ms. It would be inaccurate to say all three APIs generally have 1 ms latency. This experiment measures one warm FiQA scan workload. Independent corpora, long/filtered/cold queries, concurrency and graph-serving remain unmeasured here.

The main comparison holds the clean71 model, corrected tokenizer, document vectors and retrieval settings fixed. Before is the pre-Astra engine at `0a8caf5`; after is `7d0f9ef`, containing Astra 00–15 plus the CoreML output-copy fix. Step 16 and later changes are excluded. The common clean71 adapter necessarily includes corrected tokenizer evaluation semantics, so this comparison does **not** measure or credit Step 08's tokenizer-quality correction.

## Exact latency table

Milliseconds, nearest-rank percentile within each process, then median of three independent-process percentiles. Clocks enclose plain `TextStore::query_text`: tokenization, embedding where applicable, retrieval and returned-text construction. Returned-text serialization and token-ID validation are outside the clock. Lexical does not invoke the query embedding model.

| API | Before p50 | After p50 | p50 change | Before p95 | After p95 | p95 change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Dense default | 1.178583 | 0.958833 | -0.219750 ms (-18.65%) | 1.338542 | 1.076667 | -0.261875 ms (-19.56%) |
| Lexical | 1.110958 | 0.926042 | -0.184916 ms (-16.64%) | 3.546209 | 3.323084 | -0.223125 ms (-6.29%) |
| Hybrid default | 18.417917 | 5.524125 | -12.893792 ms (-70.01%) | 18.715125 | 6.866458 | -11.848667 ms (-63.31%) |

## Immediate recent change, separately

This middle control is `818a433`: Astra 00–15 already landed, before the CoreML output-copy fix. Both sides use normal production core features. Therefore removing benchmark fault instrumentation is held fixed and is not claimed as an application speedup. The remaining immediate code change is the CoreML output-copy implementation (`6b38f02`, retained in `7d0f9ef`). Lexical's small change is a matched-control variation; that path does not execute CoreML, so it is not credited to the copy fix.

| API | 818a433 p50 | 7d0f9ef p50 | p50 change | 818a433 p95 | 7d0f9ef p95 | p95 change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Dense default | 1.150125 | 0.958833 | -0.191292 ms (-16.63%) | 1.294125 | 1.076667 | -0.217458 ms (-16.80%) |
| Lexical | 0.941375 | 0.926042 | -0.015333 ms (-1.63%) | 3.331875 | 3.323084 | -0.008791 ms (-0.26%) |
| Hybrid default | 5.721417 | 5.524125 | -0.197292 ms (-3.45%) | 7.010542 | 6.866458 | -0.144084 ms (-2.06%) |

These are observed warm-process medians, not a universal improvement guarantee. No confidence interval or multi-host inference is claimed. Retain every process value below rather than treating the median as noise-free.

## Retrieval quality and exact output controls

Quality uses the same original FiQA qrels, deduplicates returned chunks to parent documents in rank order, and does not fetch extra chunks to fill ten distinct parents. Recall divides by every positively judged relevant parent; nDCG uses graded gains and the ideal top ten positive judgments. All query IDs and token IDs/masks match across every process; public embedding/tokenizer epoch identity also matches.

| API | Before nDCG@10 | After nDCG@10 | Before recall@10 | After recall@10 | Top-10 chunk overlap |
| --- | ---: | ---: | ---: | ---: | ---: |
| Dense default | 0.421072025 | 0.421072025 | 0.486289996 | 0.486289996 | 100.0000% |
| Lexical | 0.234286132 | 0.234286132 | 0.288113049 | 0.288113049 | 100.0000% |
| Hybrid default | 0.261614051 | 0.284278117 | 0.324154348 | 0.350369375 | 90.2315% |

- Every repetition within each revision/API returns identical full payloads: document IDs, revisions, chunk IDs, full text contents, fused/selected score bits, optional vector/lexical score bits and hit epochs.
- Dense and lexical also preserve those full payloads across all three revisions, for all 648 queries.
- All three APIs preserve full payloads between `818a433` and `7d0f9ef`, for all 648 queries. Their nDCG and recall are identical.
- Pre-Astra hybrid differs on all 648 full scored payloads. This is expected from the intervening fixed normalization and exact cross-fill corrections; it is not presented as a pure implementation speedup with unchanged scores. Hybrid nDCG improves by 0.022664066 absolute / 8.66% relative; recall improves by 0.026215027 / 8.09%. Chunk overlap is 90.2315%. These quality changes are from engine/fusion changes with the tokenizer fixed, not a new model.

`summary.json` preserves unrounded quality values and checks. The analyzer asserts exact full payload equality wherever expected before producing the table.

## Per-process latency and memory

Each array is repetition 1, 2, 3. Maximum RSS is `/usr/bin/time -l` process maximum resident set size; it is not total system footprint, GPU allocation or cache-only memory. The common harness retains a second `Bundle` mapping for token-ID validation. This makes absolute RSS higher than a bare TextStore process or the earlier attribution harness; do not interpret it as an application memory regression or compare it directly with the old ~4.1 GB figure.

| Revision arm | API | Process p50 ms | Process p95 ms | Median max RSS bytes |
| --- | --- | --- | --- | ---: |
| before | dense | 1.178583, 1.102417, 1.187125 | 1.356209, 1.237584, 1.338542 | 5652709376 |
| before | lexical | 1.118166, 1.110958, 1.100709 | 3.546209, 3.592166, 3.528125 | 5888344064 |
| before | hybrid | 17.928500, 18.417917, 18.421000 | 18.489333, 18.748959, 18.715125 | 6099943424 |
| recent | dense | 1.163041, 1.150125, 1.084708 | 1.347291, 1.294125, 1.229500 | 5656199168 |
| recent | lexical | 0.941375, 0.943959, 0.935916 | 3.375125, 3.331875, 3.331458 | 5891457024 |
| recent | hybrid | 5.774000, 5.721417, 5.608917 | 6.925459, 7.010542, 7.040750 | 6098157568 |
| after | dense | 0.953875, 1.027750, 0.958833 | 1.076667, 1.164417, 1.075959 | 5654839296 |
| after | lexical | 0.929667, 0.926042, 0.922500 | 3.323084, 3.320916, 3.409333 | 5890654208 |
| after | hybrid | 5.629583, 5.524125, 5.398333 | 6.866458, 6.877125, 6.697959 | 6095306752 |

No ingestion or bundle-size treatment was run: the same existing FP32 document index and exact same model artifacts are inputs to every arm. No memory-saving claim follows from these small process-RSS differences.

## Sources, inputs and controls

- Host: Apple M3 Max / Mac15,9, 16 cores, 128 GiB RAM; macOS 27.0 build 26A5388g. `host.json` retains exact OS, CPU, Rust, Cargo and Xcode command outputs and relevant build-environment overrides.
- Before source: `0a8caf5` (immediately before Astra 00/prerequisite work). Middle: `818a433`. After: `7d0f9ef`. Full revisions and source/harness hashes are in `source.json`.
- Three immutable Git archives were expanded in `before/`, `recent/`, `after/`; these are not replacement worktrees. Main's dirty/staged Step 16 work was never part of the builds. Main's later advancement is irrelevant to these frozen after sources.
- Experimental adapter origin: `/private/tmp/ze-clean71-smoke-bmoljupi/worktree`, preserved detached at `cf312af668e31e352a90a30eb2ce53a29bf0451d`. Each arm receives the same tested layout-3 dual-tokenizer/precompiled-normalizer bundle support, tokenizer and evaluation epoch derivation, plus their pinned Unicode dependencies. `before-adapters.patch`, `recent-adapters.patch`, `after-adapters.patch` show exact changes. The FP32 input does not require the experimental packed-U32 quantized runtime, so that extension and MLX quantized operators were not transplanted. Retrieval/core source and CoreML shim remain revision-specific and unmodified.
- The copied bundle adapter retains its document-token parity helper and omits the newer, unused `tokenize_document_chunks` helper from middle/after archive code. Neither helper is called by this harness or query path. This is an experimental adapter overlay, not a claim that a stock main binary loads this experimental model bundle.
- Bundle: `/private/tmp/ze-clean71-4bit-mscs_8rg/faithful/clean71-fp32-normalizer.zem`.
- Query model: `/private/tmp/ze-clean71-coreml4-_r2_rxbl/clean71-fp16.mlmodelc`, fixed 64 tokens, CPU+Neural Engine. `ZE_QUERY_COREML` and `ZE_QUERY_COREML_TOKENS=64` select the explicit existing model. Source inspection confirms this path fails on a found model's load error and cannot silently fall back to MLX. No new export or quantization is involved.
- Fixture: `/private/tmp/ze-clean71-4bit-mscs_8rg/faithful/full-fixture.json`; 57,638 documents, 58,980 chunks, 768 dimensions, all 648 judged queries, top-k 10. Same original text chunking, vector bytes, query order and bundle fusion alpha. No explicit tier or candidate-budget override. Physical graph coverage is zero, with one sealed live segment and no tombstones. Dense's default is the Bit4 scan; hybrid's no-graph default selects the Exact vector leg. No graph result is claimed.
- Input store: `/private/tmp/ze-query-investigate-vci05npf/store-scan`. Separate APFS clones `store-before`, `store-recent`, `store-after` start with identical bytes; `input-hashes.json` hashes every store file, model file and fixture. End-of-run store and source preservation are checked in `preservation.json`.
- Both before and after exclude `test-support`, `query-timing`, pool/native probes, and worker/QoS overrides. Each standalone manifest uses opt-level 3, fat LTO, one codegen unit, debug info, unwind panic semantics, `--profile bench`. `*-features.txt` plus actual compiler artifact receipts verify core features `[]` for every binary. Cargo profiles and binary hashes are in `build-receipts.json`.
- Isolated `CARGO_TARGET_DIR`: this artifact root's `target/`; not main's target or the prior experiment's target. The three builds ran sequentially and retained all logs. Dependency reuse explains the approximately 138 / 60 / 60 second build times.
- One common harness calls plain `query_text`, because the pre-Astra revision predates the diagnostics entry point. Current `query_text` uses the real same query implementation and returns text hits. No query result is precomputed or cached by the harness. The separate token-ID pass is outside timing and identical on all arms.
- Each fresh process performs 20 warmups, then all 648 queries. Revision order alternates before/recent/after, after/recent/before, before/recent/after, separately for each API. Each process has a pre-run one-minute load-average gate of at most 3. This is a pre-run condition, not continuous proof of a perfectly idle host. All loads and resource costs are retained in `runs/receipts.json` and logs.
- Parent paused graph process 67837 before compilation and measurement; its stopped state and process inventory were recorded before full timing. Parent did no overlapping build/test/GPU/Metal work during the exclusive window. After all 27 processes completed, the machine was released before analysis/report writing.

## Reproduction and raw artifacts

All paths below are relative to this report's directory unless absolute:

- `build.py`, `build-receipts.json`, `*-build.jsonl`, `*-build.log`: exact build commands, exit statuses, compiler features/profile, binary hashes.
- `before.tar`, `recent.tar`, `after.tar`, expanded source directories, `*-adapters.patch`, `source.json`: immutable revision and adapter provenance.
- `before/tools/matched-api/src/main.rs` and matching middle/after copies: identical common timing harness. Standalone manifests/lockfiles are beside them.
- `manifest.json`, `run.py`, `run-driver.log`, `runs/receipts.json`, `runs/complete.json`: all 27 exact commands/environments, start loads, independent-process exits and completion marker.
- `{before,recent,after}-{dense,lexical,hybrid}-r{1,2,3}/results.json`: raw per-query latencies, full returned texts/IDs/score bits, full token IDs/masks, epoch and health.
- `runs/*.log`: independent `/usr/bin/time -l` resource receipts.
- `analyze.py`, `analyze.log`, `summary.json`: exact equality/quality checks, per-process percentiles, absolute and relative comparisons.
- `pilot-manifest.json`, `pilots/`, `pilot-*/results.json`, `pilot-audit.json`: six separate compatibility pilots, first three original queries, never substituted for the full comparison.

```sh
python3 /private/tmp/ze-query-pre16-ielk83g_/build.py
python3 /private/tmp/ze-query-pre16-ielk83g_/run.py \
  /private/tmp/ze-query-pre16-ielk83g_/manifest.json \
  /private/tmp/ze-query-pre16-ielk83g_/runs
python3 /private/tmp/ze-query-pre16-ielk83g_/analyze.py
```

These commands document the completed experiment. A rerun must use a new artifact root and new output paths, preserving these binaries, sources and receipts. The run driver refuses existing output directories.

## Limits and failures

All three builds, six pilots and 27 full processes succeeded; all analyzer assertions passed. Existing compilation warnings remain in logs. No failed benchmark cell is excluded from the table.

This is not a full project qualification or Step 16/17 checkpoint. The old fault-instrumented 3–4 ms dense numbers are not matched before cells and must not be subtracted from these new numbers as an optimization effect. The earlier normal-feature 1.020125/1.215250 ms result used the older custom `cf312af` core and a different evidence harness; this report supplies fresh matched controls on the requested pre-Step-16 source revisions. The current full table supports approximately 1 ms dense latency and sub-1 ms lexical median on this setup. Lexical tail latency and the entire hybrid path still leave substantial work before a general 1 ms claim.
