# Benchmark execution and value-add

## Feature provenance correction (2026-09-05)

The regular `zeppelin-embed-bench` dependency enables core `test-support`. Its
hot fault observers materially affect parallel scan timing even when unarmed
(ASTRA-ISSUE-017). Do not use that feature graph for production latency claims.
Separate deterministic work/fault builds from normal-feature timing controls;
record `cargo tree -e features -i zeppelin-embed` for each. The standalone
`tools/query-budget/Cargo.toml` reuses the public API harness without those
features. Clean71 still requires its experimental runtime. Historical results
retain their original labels and are not silently rebaselined.

Objective: land the applicable plans, run the associated comparisons and show
which changes improved speed, relevance, memory or correctness. This is part of
goal completion, not optional documentation after implementation.

## Fast attribution for each change

1. Reuse the most recent compatible before binary/artifact with its hash and
   relevant code identity. Build the after binary incrementally. Do not rebuild
   an unchanged embedding corpus or graph just to obtain another before cell.
2. Run the plan's deterministic work/correctness oracle. For core changes,
   precomputed query vectors can isolate retrieval cost; label those numbers
   core-only and keep a real TextStore end-to-end confirmation later.
3. Run a bounded matched screen on the full compatible corpus using a frozen
   calibration subset of up to 64 queries, seed 0x5eed, with the same warmup,
   query order and three paired repetitions. Add only the plan-specific stress
   fixture, such as widening, tombstones, long terms or concurrent ingestion.
   This is a provisional screen, not final quality qualification.
4. Record parent-to-change deltas for the primary metric below, score/identity
   controls, memory and any regressions. If a helper cannot produce a meaningful
   wall delta by itself, record its deterministic work reduction and measure the
   resulting full feature at its integration checkpoint.
5. Stop a losing optional experiment after its bounded screen and record the
   actual result. Required correctness fixes still need implementation; they
   need not manufacture a latency win to justify correct behavior.

Do not use a different embedding epoch or graph topology as an uncontrolled
before/after comparison. If plan 08 changes tokenizer semantics, rebuild once and
treat the new corpus as a new baseline for subsequent graph/fusion work.

## Full confirmation

At the final assembled-change checkpoint, or an earlier checkpoint needed to
select an approximate/relevance policy, run the full relevant held-out matrix:

- Full FiQA judged query set with unique-parent metrics and raw chunk metrics
  separately; include an independent available corpus for calibrated defaults.
- Dense, lexical and hybrid on physically verified scan and graph stores.
  Reuse compatible corrected graph artifacts; do not rebuild once per leg.
- k=10 as the principal comparable cell. Additional k/width, filters, active
  rows, tombstones and concurrency are required only for plans affecting them.
- AB/BA/AB matched ordering, at least three independent processes and raw
  per-query samples, following README.md's full confirmation protocol.
- Report end-to-end p50/p95, work counters, relevant recall/nDCG, memory,
  requested backend and actual epoch/corpus/query identities.
- For policy selection, freeze calibration/held-out split, quality floors and
  candidate budgets before examining held-out outcomes.

Reuse earlier completed full cells when their binary behavior and artifact
inputs are unchanged. Never rerun the whole matrix after each private helper
commit. Explain interactions when the combined final delta is not the sum of
individual deltas; stage timings overlap and score policies can change work.

Missing graph/model/native execution is an explicit incomplete cell. A screen
or directory named store-graph cannot substitute for a verified graph result.
The final report must distinguish landed implementation, focused GREEN,
provisional screen, full benchmark confirmation and broad test qualification.

## Primary metric by plan

| Plan | Required value-add evidence |
| --- | --- |
| 00 | All-round/cross-fill counter reconciliation; evaluator correctness; instrumentation overhead |
| 01 | False certificate count; approximate/default work unchanged |
| 02 | Cursor/scorer constructions, seeks/decodes and candidate-score parity |
| 03 | Bounded/exhaustive parity; unique-parent nDCG; rounds and cross-fill cost |
| 04 | Retained scratch, full-result sorts, exact score bits and scan latency |
| 05 | Exact scan p50/p95, worker wait, throughput and scratch bytes |
| 06 | Candidate/final recall, unique-parent nDCG, coarse/rescore bytes and latency |
| 07 | Additional admissions/lookups, materialization time and snapshot identity |
| 08 | Token IDs/masks, embedding parity, nDCG and tokenization cost |
| 09 | Preparation/score calls across rounds, scratch high-water and p95 |
| 10 | Combined top-k correctness, postings/setup work, retained memory and p95 |
| 11 | Phrase reanalysis calls, eligibility bytes/positions and p95 |
| 12 | Vocabulary bytes copied, terms visited, cache bytes and expansion latency |
| 13 | DP cells, allocations, complete expansions and fuzzy latency |
| 14 | Phonetic encoder calls, bucket visits, cache bytes and latency |
| 15 | Validation row visits, exact range errors and lexical setup latency |
| 16 | Sealed contribution rebuilds, exact live stats, cache accounting and setup |
| 17 | DF-only posting work, hit rate, exact BM25 and cache bytes |
| 18 | Canceled work/queue occupancy, cancellation work bound and native residual time |
| 19 | Actual overlap, same-generation parity and end-to-end p50/p95 |
| 20 | Query p95 under ingest, ingest throughput, memory and idle latency |
| 21 | Concurrent-query throughput/p95, queue wait and bounded memory |
| 22 | Restarts/visited/rescored work, live recall and tombstone-query p95 |
| 23 | Graph recall/nDCG, ef/work, build cost/bytes and p50/p95 |
| 24 | Dispatch regret versus controls, all-round work, p95 and recall |
| 25 | Avoidable scans, coverage over time, query p95 and maintenance cost |
| 26 | Strategy work/latency, parity and held-out misrouting |
| 27 | Bound evaluation calls, candidate/score parity and net latency |
| 28 | Unique parents returned, unique-parent nDCG, extra rounds and p95 |
| 29 | Held-out nDCG wins/losses, coverage, uncertainty and latency |
| 30 | Hit rate, saved model calls, miss overhead, p95 and cache bytes |
| 31 | Cold/warm shape latency, native parity, nDCG and residency |
| 32 | Retrieved-union recall ceiling, reranked nDCG, added p95 and model memory |
| 33 | Reduced-space candidate/final recall, nDCG, vector bytes, latency and build cost |

## Result record

Record the following in each tasks/evidence/astra-XX-*.md and link it from
RESULTS.md: before/after code and binary identity, exact commands and exit codes,
raw artifact paths, compatible input hashes, actual executed cases, primary
metric before/after/delta, quality and memory controls, final disposition and
associated issue IDs. "Faster" requires measured values; "more accurate"
requires a valid reference or held-out evaluation.

For a retained optimization with no standalone wall-time signal, say exactly
which work was removed and whether the combined feature benchmark moved.
For a negative/inapplicable conditional plan, retain the evidence and its
decision. For a required plan with missing evidence, leave benchmark status
pending instead of marking all plans complete.

## Issue handling

Follow ISSUES.md. An unrelated warning or baseline test failure is logged once
and does not derail benchmark collection. Incorrect results, mismatched epochs,
invalid timers, leaked qrels or unsafe execution that affects a comparison
block that comparison and must not be hidden in the issue log.
