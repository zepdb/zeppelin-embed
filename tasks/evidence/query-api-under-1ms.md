# Query API latency: first measured wave

> Follow-up correction: these historical API clocks include the benchmark
> crate's core `test-support` fault observers. A matched feature-only comparison
> measures roughly 1 ms through the normal-feature dense API. The output-copy
> change remains committed, but the 2.3 ms scan and worker/direct gap below must
> not be treated as ordinary application costs. See
> [the follow-up and experiment register](query-api-scan-embedding.md).

2026-09-05. **Complete for the declared first-wave scope; the sub-1 ms target
is not achieved.** All 84 benchmark processes exited successfully. No graph,
caller-thread TextStore integration, concurrent-load qualification or training
result is implied.

The output-copy change reduces matched full dense API p50 from **3.733500 to
3.528583 ms (0.204917 ms / 5.49%)**, with unchanged compared results. The
remaining retrieval span is about **2.29 ms** and worker embedding evaluation
about **1.21 ms**. Shortening padding from 64 to 16 saves only **0.093 ms** in
the matched short-query diagnostic. These results do not establish a path below
1 ms without further retrieval and embedding improvements.

## Scope and source

- Main: `818a4337f07b80a5dfbf5994b893ad4356a9754a`, branch `main`.
- Measured runtime: `/private/tmp/ze-clean71-smoke-bmoljupi/worktree`, detached
  at `cf312af668e31e352a90a30eb2ce53a29bf0451d`, with its pre-existing clean71,
  tokenizer and output-layout adapters plus this experiment's harness/shim.
- Separate build target: `/private/tmp/ze-clean71-4bit-mscs_8rg/target`.
  Main's later retrieval work is absent from this runtime; these numbers are
  not current-main performance measurements. Main only received the generic
  harness and CoreML copy change. Model, retrieval and threading defaults were
  not reconfigured. Astra remains paused after Step 15.
- Host: Apple M3 Max, Mac15,9, 16 physical/logical cores, 128 GiB unified RAM,
  macOS 27.0 build 26A5388g. The measured scan uses 12 workers.
- Rust 1.93; `--profile bench`: opt-level 3, fat LTO, one codegen unit, debug
  information and no stripping. The earlier CoreML report used release;
  fresh same-profile controls are the basis for every treatment effect here.
- Export environment: `/private/tmp/ze-coreml-v3/bin/python`, coremltools 9.0,
  torch 2.12.1. The exporter warns this torch version is newer than its tested
  version; generated shapes are diagnostic, not fully qualified exports.

[Preservation and input hashes](/private/tmp/ze-query-budget-hsslyrjg/preservation-audit.json),
[original experimental patch](/private/tmp/ze-query-budget-hsslyrjg/initial-experimental.patch),
[final experimental patch](/private/tmp/ze-query-budget-hsslyrjg/final-experimental.patch),
[measured source/binary hashes](/private/tmp/ze-query-budget-hsslyrjg/copy-source.json),
[before binary hashes](/private/tmp/ze-query-budget-hsslyrjg/binary-before.json).
Source copies are archived in the artifact root's `source/` directory.

## Model and fixture

The pair is clean71-v1 / Arctic m-v2. Documents use the new pair's FP32
embeddings in the existing index; queries use the existing fixed-64 CoreML
FP16 program with Float16 output, Int32 token IDs/mask, and CPU+NE policy.
No weight-quantization treatment ran in this wave.

- Bundle: `/private/tmp/ze-clean71-4bit-mscs_8rg/faithful/clean71-fp32-normalizer.zem`.
- Query model: `/private/tmp/ze-clean71-coreml4-_r2_rxbl/clean71-fp16.mlmodelc`.
- Fixture: `/private/tmp/ze-clean71-4bit-mscs_8rg/faithful/full-fixture.json`.
  Full 57,638-document FiQA corpus, 58,980 chunks, 768 dimensions, all 648
  judged queries, k=10. Existing chunk boundaries and embeddings are reused
  identically across API arms. This is not the 256-document smoke fixture.
- Store: `/private/tmp/ze-query-budget-hsslyrjg/store-scan`, an APFS clone of
  `/private/tmp/ze-clean71-coreml4-_r2_rxbl/store-fp32`. One sealed segment,
  no tombstones, zero graph coverage. Segment, manifest and WAL hashes still
  match the original. No ingestion or graph-building time was measured.
- Model source: `/private/tmp/ze-clean71-smoke-bmoljupi/zeppelin-embed-clean71-v1-arctic-m-v2/query`.
  Six layers, hidden size 512, eight heads, intermediate size 1536, projection
  rank 512, vocabulary 98,304, maximum positions 256, RoPE base 160,000.
  Source safetensors SHA256:
  `6cdcfe08e282a1493742f528ece7a7ed1397652fb4f5d961bdb62960c90f2298`.

The old [full-FiQA report](/private/tmp/ze-clean71-4bit-mscs_8rg/faithful/REPORT.md)
and [CoreML report](/private/tmp/ze-clean71-coreml4-_r2_rxbl/group64/REPORT.md)
provide inherited index/model provenance. Their latencies are not substituted
for the fresh controls. New exports use the actual local eager source with
CLS/output projection and normalization; no replacement architecture was used.

## Measurement protocol

Every API cell performs 20 warmups, then all 648 queries in fixture order.
A timer surrounds `query_text_with_diagnostics`, including tokenization,
embedding, retrieval and returned-text construction. Serialization is outside
that timer. This diagnostics entry point uses the real TextStore query path.
Timers-enabled and timers-disabled builds are separately reported.

Three independent process repetitions, alternating treatment/control order
(AB/BA/AB; the curve also reverses model order). No task-owned build or other
GPU/Metal measurement overlapped a timed cell. Before each process, the driver
required one-minute load average <=3, waiting up to five minutes. This is a
pre-run gate, not continuous proof of exclusive host idleness. All before/after
loads and `/usr/bin/time -l` receipts remain available.

Percentiles use nearest rank within each process. Tables report the median of
three process percentiles, not a pooled percentile or a sum of stage medians.
Per-query disjoint stage sums were checked against end-to-end duration.

| Batch | Successful processes | Measured samples |
| --- | ---: | ---: |
| E0 | 12 | 3,888 API calls + 3,888 direct vectors |
| E3 | 18 | 7,776 API calls + 3,888 direct vectors |
| Length/depth curve | 54 | 10,800 direct vectors |
| Total | 84 | 11,664 API calls + 18,576 direct vectors |

## Full-FiQA dense API

All arms have nDCG@10 **0.4210720249** and recall@10 **0.4862899960**.
Every compared hit ID, revision, chunk, score bit and returned-text byte count
matches across all 11,664 calls. Quality uses positive qrels and parent-document
deduplication of returned chunks; recall divides by all relevant parent documents.
Text contents were constructed during timing but only their byte lengths were
serialized for equality checking.

| Arm | p50 ms | p95 ms | Process p50 values | Process p95 values |
| --- | ---: | ---: | --- | --- |
| e0-api-timed | 3.704167 | 3.993709 | 3.704167, 3.725667, 3.703834 | 3.899167, 3.993709, 4.227167 |
| e0-api-untimed | 3.720000 | 4.092667 | 3.720000, 3.656334, 3.738417 | 4.043750, 4.092667, 4.237417 |
| e3-api-before-timed | 3.718834 | 3.971833 | 3.653625, 3.723208, 3.718834 | 3.867250, 3.971833, 4.384083 |
| e3-api-before-untimed | 3.733500 | 4.403125 | 3.750125, 3.733500, 3.721167 | 4.468666, 4.403125, 4.102917 |
| e3-api-copy-timed | 3.563250 | 3.756750 | 3.551208, 3.580042, 3.563250 | 3.756750, 3.835209, 3.756208 |
| e3-api-copy-untimed | 3.528583 | 3.750625 | 3.519958, 3.536625, 3.528583 | 3.773167, 3.746458, 3.750625 |

The E3 timers-disabled p50 improvement is **5.49%**; timers-enabled p50 improves
**4.18%**. Timers-disabled p95 improves **14.82%**, but the before tails vary
4.103–4.469 ms. Timers-enabled p95 improves **5.42%**. The robust conclusion is
a roughly 0.15–0.20 ms API improvement, not a guaranteed 14.82% tail improvement.

### Stage attribution

| Stage | Initial p50 ms | Initial p95 ms | After-copy p50 ms | After-copy p95 ms |
| --- | ---: | ---: | ---: | ---: |
| tokenization | 0.023292 | 0.039750 | 0.023875 | 0.039708 |
| embedding_queue | 0.005458 | 0.009875 | 0.005667 | 0.009625 |
| embedding_evaluation | 1.344125 | 1.479583 | 1.210125 | 1.302583 |
| embedding_normalization | 0.001458 | 0.001708 | 0.001458 | 0.001584 |
| retrieval | 2.298000 | 2.433334 | 2.287000 | 2.417750 |
| materialization | 0.021459 | 0.028959 | 0.023583 | 0.029875 |
| unattributed | 0.004667 | 0.009375 | 0.004501 | 0.008668 |
| end_to_end | 3.703916 | 3.993417 | 3.563042 | 3.756416 |

The immediate E3 timed before control has embedding p50 1.344250 ms and retrieval
p50 2.308708 ms. Embedding evaluation falls by 0.134125 ms; retrieval was not
changed. Core timers overlap the outer retrieval span and must not be added to it.
The inbound queue is about 5 microseconds. The outbound reply is not separately
attributed; the roughly 0.41 ms worker/direct evaluation difference is unresolved.

### What the query-to-disk map establishes

The historical [call-path map](/Users/aghatage/Documents/code/zeppelin-embed/tasks/query-call-paths.md)
is based on `d74ca4b`. Direct source inspection confirms both `cf312af` and
`818a433` already use the query Bit4 accessors and cached sealed norm ranges.
Repeated full-region checksumming and norm walks described in the old map are
not new warm-query optimization opportunities. Integrity settings were not changed.

Actual measured plans are `SealedScan`, `MaskedScan`, `GraphPending`, all 58,980
rows eligible, no filter. Counters report 45,296,640 dimensions, 22,648,320 scan
code bytes, 12 workers and zero graph traversal. These bytes are not total memory
traffic or physical disk reads. The 2.30 ms includes preparation, partition work,
scheduling/join and merging; no profile has separated those terms yet.

Current experimental seams: `TextStore::query_vector` at ingest.rs:843,
`dense_hits` at :874, `scan_sealed_segment` at lifecycle/mod.rs:5836,
query Bit4 accessors at :5924/:5928, and `QueryPool::execute` at
lifecycle/pool.rs:130. Norm enclosure caching starts at lifecycle/mod.rs:5317.
Use symbols rather than carrying historical line numbers into future patches.

## Code change and correctness

`ze_coreml_copy_embedding` uses `getBytesWithHandler` for contiguous Float16
or Float32 arrays, verifies strides and buffer size, bulk-converts Float16 to
float, and retains logical-element copying for strided/other arrays. Singleton
strides do not affect contiguity. FFI signatures, model ownership, serialized
runtime, input allocation and worker channels are unchanged.

The native regression probe first failed with **768 boxed reads** and incorrect
strided Float32 output. After the change it passes with **zero boxed reads**.
The final probe covers signed zero, subnormal/min-normal/max-finite FP16 values,
contiguous FP32 with singleton strides, and strided 2x2 FP16 logical ordering.

- [RED log](/private/tmp/ze-query-budget-hsslyrjg/coreml-copy-red.log),
  [GREEN log](/private/tmp/ze-query-budget-hsslyrjg/coreml-copy-green.log),
  [final native probe](/private/tmp/ze-query-budget-hsslyrjg/coreml-copy-final.log).
- Main harness cargo check passed; all four experimental builds succeeded.
- Four placement-gate tests pass, including CPU transformer rejection and
  unknown-device rejection. Scoped Rust formatting and diff checks pass.
- No full workspace test/coverage, adversarial or concurrency qualification is
  claimed. This is a native output-copy change with focused behavioral evidence.

Apple documents buffer access through
[MLMultiArray](https://developer.apple.com/documentation/coreml/mlmultiarray).
The local SDK's `getBytesWithHandler` contract was checked before implementation.

## Direct tower control

These include input allocation, CoreML prediction and output copying on the
caller thread, with tokenization/padding outside the timer. They exclude retrieval
and TextStore worker-channel handling. All 7,776 full-query direct vectors are
bit-identical to their same-compute-unit before reference. This does not assert
bit equality across CPU and NE devices.

| Arm | p50 ms | p95 ms |
| --- | ---: | ---: |
| e0-tower-ane | 0.938584 | 1.038583 |
| e0-tower-cpu | 1.778500 | 1.899917 |
| e3-tower-ane | 0.799917 | 0.902750 |
| e3-tower-cpu | 1.650250 | 1.800041 |

CPU+NE direct p50 falls 0.138667 ms (14.77%); CPU-only falls 0.128250 ms (7.21%).
The full-64 after-copy CPU/NE ratio is 2.063x. Direct and worker contexts differ;
there is no caller-thread TextStore treatment in this wave.

## Length/depth diagnostic — separate from full-FiQA API results

Every cell uses the same **200 actual queries of <=16 nonpadding tokens**, in
the same order, without truncation. Three repetitions for each model/device,
20 warmups, 54 independent processes. Lengths are separate compiled models,
not production bucket routing. Depth truncations are timing-only.

| Shape / depth | CPU+NE p50 ms | CPU+NE p95 ms | CPU p50 ms | CPU/NE p50 | Mean cosine to 64/6 | Minimum cosine |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| s64-d6 | 0.835208 | 0.916375 | 1.620583 | 1.940 | 1.000000000 | 1.000000000 |
| s16-d6 | 0.741917 | 0.816167 | 0.878959 | 1.185 | 0.999987945 | 0.999980662 |
| s24-d6 | 0.800500 | 0.880167 | 0.969875 | 1.212 | 0.999987945 | 0.999980662 |
| s32-d6 | 0.758084 | 0.835791 | 0.959250 | 1.265 | 1.000000000 | 1.000000000 |
| s48-d6 | 0.787625 | 0.868916 | 1.604917 | 2.038 | 0.999987945 | 0.999980662 |
| s128-d6 | 1.135000 | 1.201750 | 2.881541 | 2.539 | 1.000000000 | 1.000000000 |
| s32-d2 | 0.380292 | 0.428042 | 0.296666 | 0.780 | -0.005607277 | -0.136601772 |
| s32-d3 | 0.531250 | 0.592917 | 0.492542 | 0.927 | -0.007131690 | -0.146354794 |
| s32-d4 | 0.590667 | 0.697917 | 0.635792 | 1.076 | -0.007296018 | -0.151709534 |

16 versus 64 saves **0.093291 ms / 11.17%** of direct tower time; 32 versus 64
saves **0.077124 ms / 9.23%**. The original length stop rule (<0.1 ms) fires.
Do not infer a 0.2–0.6 ms API win or deploy buckets from this subset.
Length 128 is a fixed-cost diagnostic on short inputs, not a long-query parity test.

Two/three/four layers save 0.377792/0.226834/0.167417 ms versus six layers at
length 32, but embeddings are nearly orthogonal to the control. Retrieval
quality was not measured for these invalid untrained replacements. CPU-only is
faster than CPU+NE for two/three layers. Distillation and quality qualification
are required before any model change.

The exporter checked synthetic PyTorch eager-versus-trace equality. It did not
establish full-query CoreML-versus-PyTorch reference parity. Per-model/device
outputs were stable across repeats. Source/shape/export receipts are in
[sweep/exports.json](/private/tmp/ze-query-budget-hsslyrjg/sweep/exports.json).

## Placement and 4-bit implications

The baseline and all full-depth exports retain nine specific CPU preparation
operations: greater_equal, add, select, gather, add, first layer_norm,
expand_dims, expand_dims, cast. **Strict all-ANE fails.** Diagnostic allowlists
bind exact op and output identities; unknown placement fails. The prior int4
model is rejected with **233 unapproved operations**.

The depth probes initially failed three allowlist entries because output names
changed. The serialized MIL input dependency chain verified attention_mask ->
expand_dims -> expand_dims -> cast before those exact renames were accepted.
No general exemption for transformer operations was added.

[Verified plans](/private/tmp/ze-query-budget-hsslyrjg/sweep-plans-verified.json),
[mask dependency evidence](/private/tmp/ze-query-budget-hsslyrjg/depth-mask-bindings.json),
[int4 rejection](/private/tmp/ze-query-budget-hsslyrjg/int4-gate.json).

These are anticipated compiler placements, not observed hardware traces.
Only lengths 48 and 128 exceed 2x CPU/NE p50 on the matched short-query curve;
several faster short models fail that original witness threshold. Treat their
numbers as diagnostic, not as exports that passed the original strict gate.
CoreML offers CPU+NE policy, not an NE-only execution switch:
[MLComputeUnits](https://developer.apple.com/documentation/coreml/mlcomputeunits).
No new palettized/4-bit export or quality measurement ran here.

## Memory and size

Whole-process values include TextStore open and the document tower, not just
query-model live memory. No memory reduction is claimed for the copy fix.

| API arm | Maximum RSS range MiB | Peak footprint range MiB |
| --- | ---: | ---: |
| e0-api-timed | 3897.81–4031.75 | 2171.16–2174.72 |
| e0-api-untimed | 3897.27–4032.05 | 2170.10–2174.50 |
| e3-api-before-timed | 3897.53–3898.08 | 2171.08–2171.38 |
| e3-api-before-untimed | 3897.38–3897.62 | 2170.75–2171.52 |
| e3-api-copy-timed | 3897.97–4031.88 | 2171.75–2176.49 |
| e3-api-copy-untimed | 3897.28–4031.98 | 2170.10–2174.99 |

Bundle size is unchanged at **1,514,289,168 bytes** (1444.14 MiB). Query model files are unchanged and hashed in the preservation receipt. No ingestion speed result was produced.

## Recommended next experiments

1. **Attribute the remaining scan cost, then test graph retrieval.** Add nested
   preparation/dispatch/worker/merge spans using the query-to-disk map. Compare
   1/2/4/8/12 workers with identical vectors/results before selecting a policy.
   Build a graph on a fresh clone and assert actual graph serving, with matched
   Exact/scan controls and held-out quality gates. Graph remains the largest
   potential reduction; its clean71 benefit has not been measured. Historical
   FiQA graph construction took 4,394 s, not tens of seconds.
2. **Resolve worker versus direct tower cost.** Measure native preparation,
   prediction, copying and reply boundaries separately. Test CPU gather/mask,
   buffer reuse, output backings and prediction hints as separate arms. Any
   caller-thread treatment must serialize each MLModel or use separate instances,
   as [Apple's MLModel guidance](https://developer.apple.com/documentation/coreml/mlmodel?changes=_2)
   requires. Do not assume concurrent access is safe or that 0.41 ms is queueing.
3. **Distill a smaller tower if the budget still demands it.** Measure both CPU
   and NE; preserve shared-space alignment with the fixed document tower and
   predeclare quality margins on held-out queries and other corpora/languages.
4. **Defer latency bucket integration and weight-bit changes.** Bucket savings
   did not clear the stop rule. Compression still needs placement, quality and
   same-runtime API measurements; it is not guaranteed to accelerate prediction.

The revised [experiment plan](/Users/aghatage/Documents/code/zeppelin-embed/tasks/query-api-under-1ms.md)
defines proposed quality gates and separates every NOT RUN item. E0's one-op
floor, E1 graph, E2 production routing/preparation, E3 allocation/caller-thread/
20-concurrent-query arms, and E4 training are not complete. Neither filtered,
long-query, cross-corpus nor loaded-host behavior was qualified. Dense Exact,
lexical and hybrid are selectable in the harness but were not timed here.

## Reproduction, raw artifacts and failures

Root: `/private/tmp/ze-query-budget-hsslyrjg`. Exact argv/environment for every
cell is in `e0-manifest.json`, `e3-manifest.json`, `curve-manifest.json`.
Per-cell `results.json` contains timings, IDs, score bits, plans/counters or
vectors. Each `*-runs/receipts.json` records binary SHA256, load, exit and time;
`complete.json` counts terminal successes. `*-runs/*.log` records process RSS
and footprint. `api-summary.json`, `curve-summary.json`, and
`memory-summary.json` are derived summaries. `source/` and the patches preserve
implementation provenance. No previous experiment artifacts were overwritten.

Build, after copying the recorded harness/shim into the existing runtime tree:

```sh
cd /private/tmp/ze-clean71-smoke-bmoljupi/worktree
CARGO_TARGET_DIR=/private/tmp/ze-clean71-4bit-mscs_8rg/target \
  cargo build --profile bench -p zeppelin-embed-bench \
  --example query-budget --features text,query-timing
```

Archive that binary under a unique name; build `--features text` separately
for untimed controls. Native regression command:

```sh
clang -O3 -fobjc-arc -fblocks -framework CoreML -framework Foundation \
  crates/zeppelin-embed-text/tests/coreml_output_copy.m -o /tmp/coreml_output_copy
/tmp/coreml_output_copy
```

From main, analysis commands:

```sh
python3 tools/query-budget/analyze.py /private/tmp/ze-query-budget-hsslyrjg \
  /private/tmp/ze-clean71-4bit-mscs_8rg/faithful/full-fixture.json
python3 tools/query-budget/analyze_curve.py /private/tmp/ze-query-budget-hsslyrjg
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tools/query-budget -p 'test_*.py'
```

Export/placement/driver instructions are in
[tools/query-budget/README.md](/Users/aghatage/Documents/code/zeppelin-embed/tools/query-budget/README.md).
A rerun must use fresh cell output paths, not just a fresh driver-log directory.

Failures retained: intentional native RED; intended strict/int4 placement
rejections; the initial depth allowlist mismatch and dependency diagnosis; an
initial build invoked from main with the experimental target was stopped (143)
and rebuilt in the correct worktree before any timing. That aborted build
produced no measured binary. Existing core lifetime warnings remained; no
benchmark process failed. Host-load waits are in driver logs.

Both HEADs, all pre-existing dirty/untracked file hashes, and original index
bytes were checked at completion. Measurements precede the implementation commit; Git records commit status.
