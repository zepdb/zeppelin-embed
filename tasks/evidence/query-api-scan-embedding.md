# Query API latency: scan and embedding attribution

2026-09-05. Scope: the two requested follow-ups, scan attribution plus a matched
graph comparison, and the worker/direct embedding gap. The user accepts roughly
1 ms and explicitly rejects FiQA-specific tuning. Runtime worker, QoS, model,
graph-search and retrieval defaults are unchanged.

## Main finding

The regular benchmark crate enables `test-support` in the engine. Bit4 scoring
calls two global fault-observer mutexes and allocates one result vector per
four-row batch, even without a fault controller installed. These hooks compile
out of ordinary application builds. The previously measured ~2.3 ms retrieval
span therefore includes substantial benchmark-only contention. Removing that
feature from the timing harness corrects measurement; it does not speed up an
already normal application. Integrity checks and adversarial hooks remain intact.

The normal-feature dense API reaches 0.966 ms p50 / 1.113 ms p95 in attribution.
An independent repetition set with stage timers and probes disabled measures
1.020 ms p50 / 1.215 ms p95. This supports approximately 1 ms on this workload,
not a reliable strict sub-1 ms promise or a multi-corpus qualification.

## Inputs and protocol

- Host: Apple M3 Max, Mac15,9, 16 cores, 128 GiB RAM; macOS 27.0 26A5388g.
- Main parent: `6b38f0292df4ad22765ea6151490f81ec0ccd7bc` on main. That commit
  contains the prior output-copy fix and first-wave tooling/evidence.
- Measured runtime: `/private/tmp/ze-clean71-smoke-bmoljupi/worktree`, detached
  at `cf312af668e31e352a90a30eb2ce53a29bf0451d`, with preserved custom clean71
  adapters and the archived experimental instrumentation patch. Main has later
  retrieval changes; these are not timing claims for current main.
- Bundle: `/private/tmp/ze-clean71-4bit-mscs_8rg/faithful/clean71-fp32-normalizer.zem`.
- Fixture: `/private/tmp/ze-clean71-4bit-mscs_8rg/faithful/full-fixture.json`.
  57,638 documents, 58,980 chunks, 768 dimensions, all 648 judged FiQA queries,
  k=10, original chunking and parent-dedup quality policy.
- Query model: `/private/tmp/ze-clean71-coreml4-_r2_rxbl/clean71-fp16.mlmodelc`,
  fixed 64 tokens, CPU+NE; document embeddings reuse the new FP32 pair index.
- Fresh APFS scan/graph clones preserve the original first-wave index.
- Separate target: `/private/tmp/ze-clean71-4bit-mscs_8rg/target`.
- Same standalone source/manifest/profile for normal/fault controls; optional
  `fault-instrumentation` enables only core `test-support`. Opt3, fat LTO,
  one codegen unit, debug info, bench profile. Feature trees and binary hashes
  are recorded. Normal builds retain experimental probe code compiled in but
  disabled when not requested; this is a production-feature control, not an
  entirely unpatched shipping binary.
- Each cell: fresh process, 20 warmups, fixture order, three repetitions with
  alternating order, pre-run one-minute load <=3. All task-owned builds and
  hardware measurements are sequential. Median process p50/p95, nearest rank;
  raw per-process values are in the summary JSONs. This is warm-cache evidence.
- API clocks enclose actual TextStore tokenization, embedding, retrieval and
  returned-text construction; JSON serialization/probe extraction follow timing.
  Core mode reuses correctly normalized precomputed query vectors and excludes
  embedding/returned text. Direct tower excludes tokenization and retrieval.

## Scan attribution

All core/API comparisons preserve ordered document IDs, chunk IDs, revisions
and score bits for all 648 queries. API controls also preserve serialized text
byte lengths; text content itself is not serialized. Probe interval identities
pass for every sample. Dispatch and join overlap worker execution; do not sum
worker times with caller spans or sum stage medians.

| Arm | p50 ms | p95 ms |
| --- | ---: | ---: |
| core-fault-w12 | 2.266167 | 2.387042 |
| core-normal-w1 | 0.691334 | 0.730042 |
| core-normal-w2 | 0.370083 | 0.387834 |
| core-normal-w4 | 0.211708 | 0.226167 |
| core-normal-w8 | 0.140833 | 0.159417 |
| core-normal-w12 | 0.129500 | 0.179792 |
| query-fault-w12 | 3.572791 | 3.725750 |
| query-normal-w12 | 0.966417 | 1.112750 |

The fault-build slowest partition is 2.235 ms p50; dispatch is 0.023 ms and
merge 0.0017 ms. Normal-feature core partitions fall to 0.074 ms, with dispatch
0.021 ms and merge 0.0015 ms. The expensive work was inside the partition, not
a two-millisecond dispatch/join tax. Approximately 29,496 observer locks and
14,748 result-vector allocations per query follow from source and 12 partitions
of 4,915 rows; these counts are derived, not observed hardware counters.
Process receipts corroborate contention: fault query median system CPU time
12.93 s and 356,918 involuntary context switches, versus 0.38 s and 19,527 in
the normal query control. Maximum RSS is approximately 4.094 GB in both.
No change to the default 12-worker scan follows from this sweep.

## Embedding gap and independent validation

Native prediction p50 falls from 1.222 ms in the fault-instrumented API to
0.763 ms in the normal API. Direct tower prediction is 0.807-0.810 ms.
The former ~0.41 ms worker/direct penalty is absent in the normal feature
control. This isolates benchmark configuration as a confounder; it does not
claim an internal hardware trace explaining every CoreML scheduling effect.
Input allocation and provider setup are only about 1-2 microseconds each;
output copy is below 1 microsecond on the worker. They cannot explain 0.41 ms.

| QoS control | p50 ms | p95 ms |
| --- | ---: | ---: |
| query-normal-w12 | 0.966417 | 1.112750 |
| query-normal-qos33 | 0.962541 | 1.102667 |
| tower-normal-qos21 | 0.812375 | 0.910750 |
| tower-normal-qos33 | 0.816292 | 0.904250 |

QoS changes do not establish a useful improvement. Native requested QoS is 21
on the default worker and 33 on the caller. The SDK states that this getter
does not report effective overrides. Keep current model ownership/threading.

| Independent validation | p50 ms | p95 ms | Process p50 ms | Process p95 ms |
| --- | ---: | ---: | --- | --- |
| timed-probed | 1.013166 | 1.214709 | 1.040792, 0.960208, 1.013166 | 1.214709, 1.097875, 1.227333 |
| untimed-w12 | 1.020125 | 1.215250 | 1.026916, 0.962833, 1.020125 | 1.215250, 1.104167, 1.224166 |
| untimed-w8 | 1.055458 | 1.158375 | 1.055375, 1.055458, 1.089959 | 1.158375, 1.153042, 1.352209 |

All nine validation processes complete all 648 queries with identical returned
hit metadata/score bits/text lengths. Untimed controls have no stage timings
or pool/native samples. Eight workers do not improve median API latency, and
tail variation does not support replacing the default from this one workload.

## Matched graph comparison

PENDING: standard maintenance was paused at a durable checkpoint on the fresh
graph clone to resume the authorized Astra queue. Initial graph publication and
renumbering completed; alpha-reprune remains unfinished. No graph
quality or speed result is claimed until terminal maintenance and query runs.

## Experiment register

| Experiment | Status | Decision |
| --- | --- | --- |
| First-wave API attribution, output-copy, length/depth and placement probes | COMPLETE, 84 processes | Copy fix committed as 6b38f02; historical API clocks include fault hooks |
| Partition/dispatch/join attribution and 1/2/4/8/12 scan sweep | COMPLETE | Keep existing worker default |
| Matched test-support on/off build | COMPLETE | Use standalone normal-feature benchmark manifest |
| Native allocation/prediction/copy and worker/direct clocks | COMPLETE | Old worker penalty disappears in normal-feature control |
| Worker/caller QoS controls | COMPLETE, no retained change | No useful gain |
| Timers/probes off and 8/12-worker API controls | COMPLETE | Approximately 1 ms; no FiQA-specific tuning |
| Shipped-default matched clean71 graph versus scan/Exact | PENDING, maintenance paused | No ef/R tuning authorized |
| One-op CoreML fixed-cost floor | NOT RUN | No longer needed to chase a strict 1 ms threshold |
| CPU gather/normalization/mask moved outside model | NOT RUN | Placement alone is not a speed result |
| Production enumerated-shape routing | NOT RUN | Short-shape screen failed the 0.1 ms gain threshold |
| Preallocated inputs, output backings, fastPrediction | NOT RUN | Measured allocation cost too small to explain the old gap |
| Caller-thread TextStore or concurrent model pool | NOT RUN | No ownership/concurrency change justified |
| 20-concurrent-query qualification | NOT RUN | Separate workload, not single-query qualification |
| Query-tower distillation | NOT RUN | Depth probes are unaligned timing probes only |
| New palettization/4-bit placement alternatives | NOT RUN in this work | Prior group-64 rejection remains historical evidence |
| Independent corpora/languages, long/filtered/cold queries | NOT RUN | No broad latency or quality claim |

## Reproduction and retained artifacts

Artifact root: `/private/tmp/ze-query-investigate-vci05npf`.
`initial-state.json`, `before/`, `attribution.patch` and source hashes record
the original and measured source. `normal-features.txt` / `fault-features.txt`
record feature differences. Each manifest records exact argv/env; each run
directory has exit receipts, binary hashes and process resource logs.
`attribution-summary.json`, `validation-summary.json`, `process-cost-summary.json`
and analyzer scripts retain all absolute process values. Source archive/patch
and manifests are committed with the follow-up evidence; multi-gigabyte models,
indexes, binaries and raw query outputs remain in the distinct artifact root.

```sh
python3 /private/tmp/ze-query-investigate-vci05npf/run.py MANIFEST FRESH_RUN_LOGS
CARGO_TARGET_DIR=/private/tmp/ze-clean71-4bit-mscs_8rg/target \
  cargo build --offline --manifest-path /private/tmp/ze-clean71-smoke-bmoljupi/worktree/tools/query-investigate/Cargo.toml \
  --profile bench --features stage-timing
# Add fault-instrumentation for the matched fault build; omit stage-timing
# for the untimed normal control. Copy each binary before building the next.
```

Original manifests intentionally refer to completed output paths and refuse
reuse. A rerun must change every output path and preserve the named inputs.

## Failures and limits

An initial native compile used the wrong pthread QoS getter signature; the
local SDK declaration corrected it before measurement. A core pilot failed
with EpochUndeclared because a raw Store open did not bind the text epoch.
The repair uses the already validated TextStore accessor; no epoch relabeling
or validation bypass. Failed logs remain. Seven successful pilots are separate
from the 33 attribution and nine validation processes. No pilot substitutes
for a three-process comparison.
Main standalone manifest check passed offline with query-timing; its feature
graph excludes test-support. No broad test/coverage qualification was rerun
for this tooling-only correction. Previous model/runtime adapters and original
first-wave artifacts remain preserved. Astra resumes at 16 only after this
work and its experiment status are committed.
