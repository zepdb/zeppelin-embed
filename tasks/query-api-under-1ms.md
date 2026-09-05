# Query API under 1 ms

Updated 2026-09-05 after the scan/embedding follow-up. The user accepts roughly
1 ms and does not want FiQA-specific tuning. The normal-feature dense API
measures 0.966 ms p50 / 1.113 ms p95 in attribution and 1.020 ms / 1.215 ms in
an independent timers/probes-off repetition set. A strict sub-1 ms guarantee
and broader workload qualification are not demonstrated.

The earlier ~3.5 ms API and ~2.3 ms scan included benchmark-only fault observers.
Use the standalone manifest in `tools/query-budget/Cargo.toml` for future
production-feature timing. This corrects measurement, not normal application
execution. Keep current worker count, QoS, model and retrieval defaults.

[Follow-up results and complete experiment register](evidence/query-api-scan-embedding.md)
separate completed, negative and not-run experiments. The original first-wave
findings below are retained as historical instrumented measurements; their
proposed next steps are superseded by that register and the user's no-tuning
direction. Astra is authorized to resume at 16 after this work is committed.

[First-wave evidence](evidence/query-api-under-1ms.md) preserves the 84-process
record. Original proposal: `/private/tmp/ze-query-budget-hsslyrjg/original-plan.md`.
Both waves use the custom clean71 worktree detached at `cf312af`; these are not
current-main latency claims.

## Completed pre-Step-16 all-API follow-up

[Matched all-API comparison](evidence/query-api-pre16-before-after.md) now
compares pre-Astra `0a8caf5`, pre-copy-fix `818a433`, and `7d0f9ef`, with the
same clean71/CoreML model, corrected tokenizer and index. All 27 processes and
17,496 timed calls pass. Step 08 tokenizer quality is held fixed; Step 16 is
excluded. Whole-API warm p50/p95 in milliseconds:

| API | Pre-Astra | Before output-copy fix | After pre-Step-16 work |
| --- | ---: | ---: | ---: |
| Dense default | 1.178583 / 1.338542 | 1.150125 / 1.294125 | 0.958833 / 1.076667 |
| Lexical | 1.110958 / 3.546209 | 0.941375 / 3.331875 | 0.926042 / 3.323084 |
| Hybrid | 18.417917 / 18.715125 | 5.721417 / 7.010542 | 5.524125 / 6.866458 |

Dense is approximately 1 ms and lexical's median is below 1 ms on this setup.
Lexical p95 and hybrid remain higher. These are matched production-feature
controls; the earlier fault-instrumented numbers are not the before cells.

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

Start with the existing profile. Do not tune ef/R for FiQA in this follow-up. The earlier proposal to freeze a seeded 64-query
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
target. Do not label older timings current. The user separately authorized resuming
Astra at 16 after committing the completed experiment work.
