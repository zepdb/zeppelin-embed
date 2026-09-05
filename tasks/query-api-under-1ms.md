# Query API under 1 ms

Updated 2026-09-05 after the first measured experiment wave. Target:
`TextStore::query_text`, dense leg, steady-state single-query p50 below 1 ms.
**The target is not yet demonstrated.** The matched experiment reaches
3.529 ms p50 / 3.751 ms p95 on the full FiQA query set.

[Measured evidence](evidence/query-api-under-1ms.md) contains results and raw
artifact paths. The original proposal is preserved verbatim at
`/private/tmp/ze-query-budget-hsslyrjg/original-plan.md`.
The measured runtime is the existing clean71 worktree, detached at `cf312af`,
with its original adapters plus the benchmark/shim patch. The first-wave main base is `818a433`.
These timings are not a performance claim for main's later retrieval changes.
Astra work remains paused after Step 15.

## What the measurements change

The full scan is the largest measured term. The output-copy change provides a
small improvement with unchanged results. Shorter sequences do not justify the
proposed bucket implementation for latency. Layer count is promising only with
training; raw truncation destroys alignment.

| Measurement, ms | Before | After output-copy change |
| --- | ---: | ---: |
| Dense API p50, timers disabled | 3.733500 | 3.528583 |
| Dense API p95, timers disabled | 4.403125 | 3.750625 |
| Worker embedding p50, timers enabled | 1.344250 | 1.210125 |
| Retrieval p50, timers enabled | 2.308708 | 2.287000 |
| Direct tower p50, CPU+NE | 0.938584 | 0.799917 |

API p50 improves 0.205 ms / 5.49%. The 14.82% p95 improvement is less stable:
control p95 ranges from 4.103 to 4.469 ms, and the timer-enabled comparison
improves p95 by 5.42%. Do not promise a universal 14.82% gain.

Initial stage p50: 0.023 ms tokenization, 0.005 ms inbound embedding queue,
1.344 ms embedding evaluation, 0.0015 ms normalization, 2.298 ms retrieval,
0.021 ms materialization. Do not sum stage medians. Per-query disjoint spans
and residuals were checked against end-to-end time.

The roughly 0.41 ms worker/direct tower gap after the fix is **unattributed**.
It is not evidence of 0.41 ms channel overhead: inbound queue time is about
5 microseconds and the outbound reply is not separately timed.

## Retrieval: use the call-path map with a revision check

[Query to disk call paths](query-call-paths.md) maps the instrumentation seams.
Its line numbers and defects describe `d74ca4b`. Both current checkouts use
`query_bit4_codes` / `query_bit4_factors` and remember sealed vector norm
ranges. The map's repeated full-region hashes and norm walks are already
addressed. Do not disable integrity checks to reproduce those old savings.

The measured path is `SealedScan` / `MaskedScan` / `GraphPending`: one segment,
58,980 live chunks, zero graph traversal. Counters report 45,296,640 dimensions,
22,648,320 scan code bytes and **12 workers**. That byte counter is not a
measurement of all mmap traffic, factor reads, page faults or physical disk I/O.

Next diagnostic: split `scan_sealed_segment` preparation, `QueryPool::execute`
dispatch, worker start/wait, partition scoring/top-k, join and `merge_partitions`.
Do not sum overlapping worker times as wall time. Sweep core
`SearchOptions::new(ScanOptions { thread_budget: ... })` at 1/2/4/8/12 workers
with identical precomputed vectors. Require identical IDs and score bits, then
repeat a winning policy through TextStore. TextStore query options do not
currently expose this budget; a core-only result must stay retrieval-only.
No worker default changes follow from inspection.

## E1. Matched clean71 graph experiment — next largest potential gain

Build on a fresh APFS clone; preserve the scan control. Record maintenance
budgets/checkpoints. Require physical `graph_coverage == 1.0` and actual
per-query graph-serving counters/branches, with no unexpected scan fallback.
Coverage alone is insufficient.

Use the same 57,638 documents, 58,980 chunks, all 648 queries, k=10, embeddings,
qrels and parent-dedup policy. Compare both scan and Exact rankings. Exact
rescoring does not make graph candidate membership exact. Retrieval p50 below
0.35 ms remains a target, not a measured prediction.

Start with the existing profile. If tuning is needed, freeze a seeded 64-query
calibration set and reserve 584 queries for final evaluation. Proposed gate,
to declare before running: recall of Exact top-10 at least 0.99 and nDCG@10
loss at most 0.005 absolute, with paired-query bootstrap intervals. These
criteria are not achieved results. Separate document-level quality from chunk
overlap. Stable deterministic rankings have no run-to-run quality noise.

Build time is unknown for this model. The prior FiQA graph took **4,394 seconds /
73.2 minutes**, not tens of seconds ([evidence](evidence/fiqa-graph-store-and-hybrid-block.md)).
That older model's timings are not matched clean71 controls.

## E2. Sequence lengths and CPU preparation

The diagnostic used the same 200 queries of at most 16 tokens, without
truncation, at lengths 16/24/32/48/64/128. CPU+NE p50 was
0.742/0.801/0.758/0.788/0.835/1.135 ms. **The stop rule fires:** 16 versus 64
saves 0.093 ms, below 0.1 ms. Defer enumerated shapes as a latency project.
Long-query support remains a separate requirement; these results do not qualify
128/256-token routing.

Next export arms: move CPU embedding gather, first normalization and mask
preparation separately, preserving token IDs, gathered bytes, RoPE and padding.
Measure their costs; placement alone does not explain the worker/direct gap.
Moving preparation outside CoreML does not guarantee every remaining op runs
on the Neural Engine.

If buckets are later needed, warm every accepted shape. Apple documents
optimized finite enumerated shapes, first-use work for nondefault shapes and
platform constraints for multiple enumerated inputs:
[Flexible Input Shapes](https://apple.github.io/coremltools/docs-guides/source/flexible-inputs.html).

## E3. CoreML call path

Completed: replace 768 boxed FP16 output reads with a checked contiguous-buffer
conversion. Keep logical-element access for strided arrays; this also fixes
Float32 memcpy on strided output. Native RED/GREEN and all 11,664 matched API
results support the change. Runtime ownership and concurrency did not change.

Next separate arms: preallocated inputs, output backings, `fastPrediction`,
then caller-thread prediction with serialized model ownership. Use actual
TextStore arms and direct tower controls. Time enqueue, worker start,
prediction and reply send/receive separately. Run a 20-concurrent-query
throughput/tail cell before changing concurrency.

Do not share one MLModel for concurrent predictions without serialization.
Apple instructs using a model on one thread/queue at a time, or separate
instances. Caller-thread execution needs an ownership/lock contract; a pool
needs memory and concurrency measurements:
[MLModel](https://developer.apple.com/documentation/coreml/mlmodel?changes=_2).

## E4. Depth ladder — training required

At length 32, CPU+NE p50 for 2/3/4/6 layers was 0.380/0.531/0.591/0.758 ms.
CPU-only was faster for the two- and three-layer probes. Neural Engine
placement is not itself a latency guarantee.

Naive truncations have mean cosine to the six-layer control near zero
(-0.0056 to -0.0073). They are timing probes, never deployable models.
If retrieval and call-path changes still miss the budget, distill a three- or
four-layer query tower against the frozen document tower using the
[distillation prompt](query-tower-distill-prompt.md). Declare quality margins
before training; qualify multiple corpora/languages before changing defaults.
No training or full retrieval-quality test of depth probes ran.

## Weight compression and placement

The previous group-64 int4 conversion fails the explicit placement gate with
233 unapproved nonconstant operations. No new 4-bit model is claimed here.
Palettization or other granularities are experiments, not guarantees of Neural
Engine placement, speed or quality. A tie between two models' total API time
does not bound weight-streaming time at 0.1 ms.

`tools/query-budget/placement.py` separates strict all-ANE from explicit
allowlist results. Skip constants/constexpr only; reject unknown devices.
Full-depth exports retain the control's nine exact CPU preparation operations,
so **strict all-ANE fails**. Depth-probe mask-output renames were checked
against serialized dependencies before approval, without blanket op exemptions.

Compute plans are anticipated placement, not hardware traces. Record CPU-only
versus CPU+NE ratios. Several short models fail the original >2x witness
criterion; their timings remain diagnostic, not production qualification.
CoreML has CPU+NE policy, not an NE-only switch:
[MLComputeUnits](https://developer.apple.com/documentation/coreml/mlcomputeunits).

## Scope and remaining qualification

Completed: attribution, length/depth diagnostics, output-copy fix, placement
rejection tests and 84 successful independent benchmark processes. E0's one-op
floor, E1 graph, E2 production routing/gather, E3 allocations/caller-thread/
concurrency, and E4 training remain **NOT RUN**.

All API timings include tokenization, embedding, retrieval and returned text.
Direct tower cells exclude tokenization/retrieval. Three independent process
repetitions, alternating order, 20 warmups, pre-run load average <=3, no
overlapping task-owned GPU/Metal work, `--profile bench`. Report median process
p50/p95 and retain every process value. Historical cross-profile/runtime numbers
are context, not matched treatment effects.

Before a sub-1 ms claim, qualify graph/model quality together, default segment
policy, held-out corpora/languages, long and filtered queries, and concurrency.
A one-op floor above 0.5 ms reduces the budget; it would not mathematically
prove every sub-1 ms path impossible. Measure CPU-only alternatives too.

Hybrid, lexical and Exact are selectable in the harness but were not timed in
this first wave. Hybrid remains the API default and is outside this dense
target. Do not label older timings current or resume paused Astra steps under
this experiment's name.
