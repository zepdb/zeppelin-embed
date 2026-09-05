# Execution and measured value

Status: plan 00 landed as 8d5991c after prerequisite 12ee589. Plans 01-03 landed as 6b37820, aa9c409 and 1f12858; 60a0e9d fixes assembly accounting. Plan 04 landed as fcc5999; plan 05 landed as b743113; plan 06 landed as d66b161 with measured dense benefit; plan 07 landed as 1f9f8ad with pinned text correctness and measured copy/admission reduction. Plan 09 landed as 3befa3c with query preparation/score/buffer reuse and measured validator outlining. Plan 10 landed as 8cecfcf with bounded combined structured scoring and measured fuzzy/hybrid stress gains. Plan 11 landed as cee49da with selective persisted positions and 95.86-97.72% phrase stress p95 gains. Plan 12 landed as b417501 with cached vocabulary and warm prefix p95 -96.84%; its instrumented post-update p95 regression remains explicit. Plan 13 landed as bfc2945 with focused GREEN and paired fuzzy Store/hybrid p95 gains of 48.62%/39.22%, with cache and first-use costs explicit. Full graph/held-out/broad qualification remains pending.

The user narrowed execution to finish Step 19 and stop. Steps 00–19 have
individual implementation and evidence entries; Steps 20–33 are unexecuted.
The Step 19 implementation, tests and measurements are contained in this commit.
Its resolved commit receipt is `/private/tmp/ze-astra19-host-0_z5i12g/commit-receipt.json`.

Update as the goal runs. "Landed" means an actual scoped commit exists.
Focused GREEN, benchmark confirmation and broad qualification are separate.
Log incidental problems in [ISSUES.md](ISSUES.md), then continue unless they
block the current plan or a trustworthy required comparison.

| Plan | Implementation / commit | Focused validation | Value-add benchmark / evidence | Issues |
| --- | --- | --- | --- | --- |
| [00](00-query-evidence/plan.md) | Implemented, 8d5991c; prerequisite 12ee589 | Named round/cross-fill/ceiling/evaluator RED to GREEN; core 2, hybrid 3, evaluator 15, native detail on/off 1 each | [Evidence](../evidence/astra-00-query-evidence.md): 27 scan cells + 9 lexical recheck; exact ranking/work controls; disabled lexical p95 +7.87%; graph NOT MEASURED | 001 resolved; 002-004 open |
| [01](01-hybrid-coverage-contract/plan.md) | Implemented, 6b37820 | Public graph and omitted-winner RED to GREEN; exact 3-round std-reference, pure-contract and directed controls GREEN | [Evidence](../evidence/astra-01-coverage.md): hybrid p95 17.6313 to 17.6485 ms; exact IDs/bits/work/nDCG unchanged; 64 unsupported certificates withdrawn | 002 remains pending after-03 oracle integration |
| [02](02-candidate-bm25/plan.md) | Implemented, aa9c409 | Three intended REDs; 6 unit, 1 filtered integration, 128-case parity, dense branch and directed seed-11 controls GREEN | [Evidence](../evidence/astra-02-candidate-bm25.md): 400-row streams 800 to 2 / blocks 1536 to 26; core p95 109.10 to 46.82 us; 1-row +0.1943 us; RSS +0.0156 MiB; full feature at 03 | None blocking |
| [03](03-hybrid-score-contract/plan.md) | Implemented, 1f12858; prerequisite 60a0e9d | Six named score/structured tests; 12-case Store checkpoint; independent v3 oracle/plants; graph/identity/pure/tier controls GREEN | [Evidence](../evidence/astra-03-hybrid-score-contract.md): 64-query scan parent nDCG .18871 to .20075 (+6.38%); p95 17.5436 to 17.5722 ms; 64 complete stable certificates / one round; vector work unchanged | 002 resolved; 005 resolved in 60a0e9d |
| [04](04-bounded-exact-scan/plan.md) | Implemented, fcc5999 | Literal materialization RED to GREEN; 4 public + 3 unit tests, isolated allocation audit, directed cancel/nonfinite/allocation and hybrid work controls GREEN | [Evidence](../evidence/astra-04-bounded-exact-scan.md): k=7 uses 14 slots at N=129/1025; 60k-row core p95 -48.46%; 18 native cells preserve ranks/bits/work; Exact dense p95 -9.37%, hybrid -9.18% | 006 small/all-tied regressions retained |
| [05](05-parallel-exact-scan/plan.md) | Implemented, b743113; close prerequisite cf312af | Worker/shared-capacity RED to GREEN; 7 focused tests, 12 barrier scenarios, budget refusal and armed cancellation GREEN; serial/crossover acceptance passed | [Evidence](../evidence/astra-05-parallel-exact-scan.md): final native Exact p95 -64.77%, hybrid -63.39%; default dense +1.86%, lexical -0.46%; largest serial regression +0.84%; exact bits/IDs and non-worker work preserved | 007 resolved cf312af; 009/010 resolved b743113; final held-out/graph qualification pending |
| [06](06-scan-exact-rescore/plan.md) | Implemented, d66b161 | Eight vector, one hybrid, one native text test GREEN; directed corruption/clean retry, masked mixed-source score checks and stress measurement GREEN | [Evidence](../evidence/astra-06-scan-exact-rescore.md): held-out dense 2x p95 -31.47%, recall .997432, nDCG unchanged, RSS -1.91%; hybrid p95 -5.65% fails 10% target; small mixed-store rescore slower | No default change; graph/independent-corpus qualification pending |
| [07](07-pinned-result-materialization/plan.md) | Implemented, 1f9f8ad | Snapshot/admission RED to GREEN; seven core tests including purge/current-revision/publication, three native controls, FFI mapping and affected regressions GREEN | [Evidence](../evidence/astra-07-pinned-materialization.md): additional admissions and reverse version lookups zero at k=1/10/100; one copy per hit; final native p95 default dense -0.02%, Exact -0.26%, lexical +1.60%, hybrid +0.25%; unchanged bits/IDs/work; k10 copy p95 3.958 to 1.334 us | No production blocker found; qualification pending |
| [08](08-tokenizer-parity/plan.md) | Implemented, b496446; WordPiece correction and epoch revision | Token IDs/masks 22 cases per role, normalization/ASCII, native document/query parity GREEN; exporter and epoch RED to GREEN; deny/size gates passed | [Evidence](../evidence/astra-08-tokenizer-parity.md): outlier error .00802 to 2.23e-7; tokenizer p95 query 2.167 to 6.375 us and document 20.167 to 69.917 us; full rebuild complete; all 648 queries: default/Exact/hybrid parent nDCG +1.805%/+1.509%/+0.363%, lexical identical; API p95 -1.261%/+0.097%/-1.162% (lexical -0.025%) | No external blocker |
| [09](09-hybrid-round-reuse/plan.md) | Implemented, 3befa3c | Preparation, physical identity, f64 order, retained-buffer and budget/release RED to GREEN; 25 final focused cases plus 3 unchanged graph cases GREEN | [Evidence](../evidence/astra-09-round-reuse.md): cross-score calls 350 -> 200/leg, graph exact calls 36 -> 12, lexical prep 6 -> 1; final one-round p95 -6.92%, tiny widening +1.67%; native hybrid -0.14%, lexical -2.18% after measured outlining repair; exact IDs/score bits unchanged | Per-change complete; final graph/held-out/broad qualification pending |
| [10](10-structured-combined-topk/plan.md) | Implemented, 8cecfcf | Retention, temporary budget and empty-expansion RED to GREEN; 3 unit + 7 public and affected controls GREEN; both score plants fired | [Evidence](../evidence/astra-10-structured-topk.md): retained rows 6000 -> 11/202, corpus prep 17 -> 1; final stress p95 prefix -2.62%, fuzzy -33.86%, hybrid -81.37%; native dense/lexical/hybrid -2.46%/-0.06%/-0.49%, exact hits/bits/work unchanged | Per-change complete; final assembled qualification pending |
| [11](11-positional-phrases/plan.md) | Implemented, cee49da | Reanalysis RED to GREEN; 3 units, 3 public and 4 affected controls GREEN; positional-corruption plant fired, restored control GREEN | [Evidence](../evidence/astra-11-positional-phrases.md): eligibility analyzer/text work zero; phrase core p95 common -97.50%, rare -97.13%, repeated -95.86%, hybrid -97.72%; all 3072 core and 1152 native calls preserve outputs/work; native controls complete | Per-change complete; final assembled qualification pending |
| [12](12-vocabulary-prefix/plan.md) | Implemented, b417501 | Prefix/copy and grouping RED to GREEN; 5 units, 2 public tests plus affected expansion/allocation/close controls GREEN | [Evidence](../evidence/astra-12-vocabulary-prefix.md): warm p95 447 -> 14.125 us (-96.84%); copied bytes 84024 -> 0, visits 12004 -> 4 (+15 seek); cache +492256 bytes; instrumented update p95 +56.68%; all 870 core/1152 native calls preserve outputs/work | 014 resolved; 015 negative update-rebuild cost; final qualification pending |
| [13](13-bounded-fuzzy/plan.md) | Implemented, bfc2945 | Length/scratch RED to GREEN; five focused release tests and seven vocabulary/public regressions pass | [Evidence](../evidence/astra-13-bounded-fuzzy.md): same-length near expansion p95 -89.08%; Store fuzzy -48.62%, hybrid -39.22%; 2610 exact core payloads and 1152 unchanged native controls | Cache +65,576 bytes; prefix initial/update overhead extends issue 015 |
| [14](14-phonetic-index/plan.md) | Implemented, 2ca47fc | Warm-encoding RED to GREEN; five release tests, public/reuse controls and actual reservation plant pass | [Evidence](../evidence/astra-14-phonetic-index.md): Store rare phonetic p95 -97.87%, hybrid -85.81%; warm encodes 4100 -> 1; 3480 exact core payloads and 1152 unchanged native results/work | Cache +65,664 bytes; collision p95 +1.18%, cold/update tradeoff extends 015; unrelated formatting 016 |
| [15](15-bitmap-validation/plan.md) | Implemented, 818a433 | Two executed work REDs to GREEN; six focused tests; exact first-invalid/error order and BM25 controls pass | [Evidence](../evidence/astra-15-bitmap-validation.md): 58,980-row setup p95 178.042 -> 0.125 us; assembly 343.750 -> 178.459 us; six uninstrumented processes, 240 exact scored control hits | Core setup only; unchanged memory layout, no new cache; pause lifted by user |
| [16](16-incremental-lexical-assembly/plan.md) | Implemented, 87421b7 | Actual row-walk RED to GREEN; seven focused and six affected cases pass; memory/stale plants fire | [Evidence](../evidence/astra-16-lexical-assembly.md): post-update core setup p95 -59.85% to -75.22%, zero unchanged sealed statistics walks; 11,520 identical scored hits | Warm absent p95 +0.041-0.042 us; cache +320/+1,488 bytes; issue 018 logged; combined native checkpoint with 17 complete |
| [17](17-live-document-frequency/plan.md) | Implemented, ee016e3 | Literal reuse/admission RED to GREEN; 9 focused tests, 2 firing plants, 5 selected integration cases GREEN | [Evidence](../evidence/astra-17-live-df.md): final tombstoned common core p95 -93.18% to -99.53%; 24 native processes / all 648 queries preserve full payloads; fixed-deletion lexical/hybrid p95 -9.23%/-6.10%; intact APIs unchanged | Initial unique-term regressions rejected; final all-live core control +0.125us; cache +6,768B per partial contribution; broad qualification pending |
| [18](18-query-cancellation/plan.md) | Implemented, 192689b | 52 cases GREEN:39 core18+6 regressions+7 text controls; seven firing/restored plants | [Evidence](../evidence/astra-18-query-cancellation.md): bounded preparation/queue work;384 native cancellations+recoveries pass; CoreML/MLX residual p95 0.894/5.659ms;48-process ordinary screen has exact payload parity | Lexical p95 +9.24–13.32%,deleted hybrid +10.84%;24-process diagnostic attributes material cost to polling (issue020); full qualification pending |
| [19](19-embedding-lexical-overlap/plan.md) | Implemented in this commit; resolved SHA in host commit receipt | Five core + three TextStore cases GREEN; early-preparation plant fires/restores; directed panic/clean control and native public case pass | [Evidence](../evidence/astra-19-embedding-overlap.md): 66 ordinary + 18 diagnostic processes, 5,376 calls; graph hybrid p50 -34.57%, p95 -18.28%; scan hybrid p95 -11.20% intact / -18.06% deleted; exact payload/work/quality parity; 576/576 after queries overlap native embedding and lexical work | Single-leg timing noise within 5%; no universal 1 ms claim; broad qualification unrun; user stop after 19 |
| [20](20-query-runtime-isolation/plan.md) | Not executed: user stop after 19 | NOT RUN | NOT RUN | Outside revised scope |
| [21](21-lexical-worker-capacity/plan.md) | Not executed: user stop after 19 | NOT RUN | NOT RUN | Outside revised scope |
| [22](22-live-graph-results/plan.md) | Not executed: user stop after 19 | NOT RUN | NOT RUN | Outside revised scope |
| [23](23-graph-quality-tuning/plan.md) | Not executed: user stop after 19 | NOT RUN | NOT RUN | Outside revised scope |
| [24](24-graph-scan-crossover/plan.md) | Not executed: user stop after 19 | NOT RUN | NOT RUN | Outside revised scope |
| [25](25-maintenance-priority/plan.md) | Not executed: user stop after 19 | NOT RUN | NOT RUN | Outside revised scope |
| [26](26-lexical-strategy-tuning/plan.md) | Not executed: user stop after 19 | NOT RUN | NOT RUN | Outside revised scope |
| [27](27-lexical-bound-reuse/plan.md) | Not executed: user stop after 19 | NOT RUN | NOT RUN | Outside revised scope |
| [28](28-distinct-parent-results/plan.md) | Not executed: user stop after 19 | NOT RUN | NOT RUN | Outside revised scope |
| [29](29-fusion-calibration/plan.md) | Not executed: user stop after 19 | NOT RUN | NOT RUN | Outside revised scope |
| [30](30-query-embedding-cache/plan.md) | Not executed: user stop after 19 | NOT RUN | NOT RUN | Outside revised scope |
| [31](31-coreml-shape-buckets/plan.md) | Not executed: user stop after 19 | NOT RUN | NOT RUN | Outside revised scope |
| [32](32-reranker-evaluation/plan.md) | Not executed: user stop after 19 | NOT RUN | NOT RUN | Outside revised scope |
| [33](33-mrl-candidate-evaluation/plan.md) | Not executed: user stop after 19 | NOT RUN | NOT RUN | Outside revised scope |

## Final comparison

Full assembled held-out comparison NOT RUN under the revised stop-after-19
scope. Individual per-change evidence and the completed pre-Step-16/full-query
comparison below remain available; they do not substitute for that qualification.
Step 19 adds a matched 64-query full-corpus screen including a production-pair
graph control. The separately authorized full-query BEIR/native comparison is
complete for three full datasets and the prepared NQ prefix; its frozen source
precedes Steps 17–19: [comparison](../cross-bench-results/2026-09-05-prepared-core-graph.md).

## Broad qualification

NOT RUN. Full workspace/CI/coverage/long adversarial qualification is separate
from the focused implementation and benchmark pass, as specified in README.md.


## Current execution checkpoint

The implementation queue through Step 19 has focused GREEN and per-change
measurements. Selected after-03, after-07 and after-17 integration checkpoints
pass. The Step17 commit contains exact live-frequency caching, rejected admission
experiments and the full native Step16+17 confirmation. Steps 18 and 19 have
separate completed per-change evidence. Plans 20–33 and the final assembled
held-out/graph confirmation remain unexecuted under the revised user scope. Step06 retains its hybrid/
small-store negative results without changing defaults. Corrected Step08 corpus
comparisons cover all 648 queries. Unrelated examples/python remains untouched.

Step 05's final matched 64-query full-FiQA screen measures Exact dense p95
15.510958->5.463792 ms (-64.77%) and hybrid 15.670000->5.737000 ms (-63.39%).
Default dense is +1.86%, lexical -0.46%. All ranks, score bits, relevance and
non-worker counters match. Exact reports four scan workers; hybrid's five
vector participants also include the caller doing exact cross-fill.

Seven focused tests, twelve deterministic worker barrier scenarios, armed
cancellation, six serial before/after controls and the final crossover passed.
Generated assembly and timing establish the collector-inlining repair; the
largest serial p95 regression is +0.84%, within the unchanged tolerance.
Crossover gains are 21-40% for qualifying shapes. Earlier uncontended capacity
measurements select a conservative shared maximum of four; their small
four-caller throughput regression remains explicit. Final-code concurrency
numbers beyond the focused shared-capacity test have not been remeasured.

Targeted Clippy and format checks passed with the two existing plan-04 test
warnings. Full held-out/graph qualification and later shared checkpoints remain
pending; warm-query results do not claim cold-cache behavior.

## Separately authorized clean71 model comparison

The clean71-v1 / Arctic m-v2 comparison completed in the existing detached
cf312af experimental worktree, with its own CARGO_TARGET_DIR and no model-default
change on main. Report and raw-artifact index:
/private/tmp/ze-clean71-4bit-mscs_8rg/faithful/REPORT.md.
All 65 measured processes exited 0. The full arms each use 57,638 documents,
58,980 identical text chunks and three independent repetitions of all 648
judged queries for dense default, dense Exact, lexical and hybrid. The parent
audit covers all 23,328 timed full-query calls and original corpus/qrel equality.

Against the same new FP32 pair, MLX group-64 4-bit weights reduce dense-default
API p95 7.5915 -> 7.296292 ms (-3.89%), with Exact effectively unchanged
21.109417 -> 21.126541 ms (+0.08%) and hybrid -1.18%. Dense-default/Exact nDCG@10
decline 4.65%/4.67% relative; hybrid declines 1.55%; lexical hits and score bits
are identical. Exact top-10 chunk overlap is 78.27%. Bundle bytes decline 83.81%
and query-process maximum RSS declines 68-75%. Single fresh ingestion observations
are 1,977.018887 seconds FP32 and 2,004.759330 seconds 4-bit (+1.40%), with no
demonstrated ingestion speed gain. All three arms, including status quo, reach
approximately 104 GiB macOS peak footprint, distinct from maximum RSS.

The matched MLX LEAF/Arctic m-v1.5 control has Exact nDCG 0.416746; the new FP32
pair reaches 0.420729 (+0.96%), while 4-bit reaches 0.401091 (-3.76% versus the
control). New FP32 Exact recall is 0.65% lower than the control, so the quality
gain is not uniform. New FP32/4-bit hybrid nDCG remain 3.98%/2.38% above control.
The control truncates 2,671 common chunks at 512 model tokens; the new document
tower retains their full context. All experimental arms use the older core
and MLX GPU query placement. These timings do not qualify main's newer core
optimizations or its CoreML placement, and are not used as their before/after
values. Issue 013 records the source-tokenizer repair and preserved prior runs.

## Query-budget follow-up and resumed queue

Prior output-copy/tooling commit: 6b38f02. The scan/embedding attribution work
is recorded in `tasks/evidence/query-api-scan-embedding.md`, including a table
of completed, negative and not-run experiments. Normal-feature API p50 is
approximately 1 ms; test-support distorted historical benchmark clocks
(ASTRA-ISSUE-017). No worker/QoS/model policy changes are retained. The user
accepts approximately 1 ms and rejects FiQA-specific tuning.

The default clean71 graph is published, but refinement/comparison is pending
at a preserved checkpoint. This is not a qualified graph result. The completed
diagnostics and pending status are committed before resuming Step 16. Plans
20–33 remain unexecuted under the user stop after 19.

Step 16 retains immutable contribution sharing after public-path RED/GREEN,
13 distinct focused/regression cases, two firing fault plants and six matched
normal-feature core processes. Post-update setup p95 improves 59.85-75.22%;
warm absent p95 rises one approximate timer tick and remains an explicit
negative result. Step16 landed as `87421b7`. The shared after-17 native checkpoint
is complete: on fixed deletions, lexical/hybrid p95 improve 9.23%/6.10%; intact
controls are unchanged and all 15,552 full payloads are identical. Step 18 landed
as 192689b with implementation and per-change measurements complete. Its64-query full-corpus
screen has48 successful processes and exact payload parity. Six native processes
verify384 cancellation/recovery cases;24 diagnostic processes attribute the
lexical slowdown to cooperative polling. The required cancellation repair is
retained with issue020 recording that cost;full qualification remains separate.
The user requests completion of Step19 followed by a stop on this plan.
Step 19 is complete in this commit; Steps 20–33 remain unstarted. The separate
BEIR comparison is complete for its revised three-full-dataset/NQ-prefix scope.


The user-requested pre-Step-16 all-API comparison is complete: [report](../evidence/query-api-pre16-before-after.md).
Three immutable revisions, three APIs, three repetitions, all 648 queries:
27 processes / 17,496 calls. Pre-Astra to `7d0f9ef` p95 improves dense
1.338542 -> 1.076667 ms, lexical 3.546209 -> 3.323084 ms, hybrid
18.715125 -> 6.866458 ms. Both controls use normal core features and identical
clean71/tokenizer/index inputs; Step 08 tokenizer semantics are held fixed,
and Step 16 is excluded. Dense/lexical full results are unchanged; hybrid's
intervening correctness changes improve nDCG 0.261614 -> 0.284278. This is one
warm FiQA fixture and does not qualify all APIs at 1 ms. The report separately
isolates the recent output-copy change with `818a433` as the middle control.
