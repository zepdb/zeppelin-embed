# Step 17: exact live document-frequency reuse

Implementation: retained. Focused validation and selective integration checkpoint:
GREEN. Matched core and native TextStore confirmation: complete. Broad qualification:
NOT RUN. The implementation commit is identified by the subject
`Cache exact live lexical document frequencies` and the source receipts below.

## Contract and scope

Each immutable partially-live contribution owns a bounded exact DF cache keyed
by term bytes and the normalized, sorted, unique field set. Its contribution
identity includes the validated reader and exact alive membership from Step 16.
A new deletion rebuilds the affected contribution; active appends and retention
reuse unchanged survivors even when source ordinals change. No approximate DF,
persisted format, C ABI, model, query default or worker policy changes.

The cache has 64 entries and 4,096 key bytes, preallocated and reserved before
publication. A mutex serializes first counts and bounded LRU eviction. Oversized
keys bypass admission, never exact evaluation. Allocation/budget failures leave
no partial cache. Poisoned locks and invalid internal offsets produce typed
IndexError::LiveFrequencyCache; errors do not become zero DF. Reservations follow
the last cache Arc, including old query owners after assembly eviction.

Zero/single-posting queries use exact dictionary/first-row membership inspection
without cache lookup or eviction. Multiple field runs retain exact union semantics.
An index with no cached contributions uses the original frequency loop; the
all-live dictionary path remains. Public infallible DF inspection remains uncached;
all four scoring/preparation sites use the private fallible method. Duplicate terms
reuse DF while preserving their existing additive score behavior.

## RED, GREEN and selective checkpoint

- Initial public RED: a repeated common term still visits 2,048 doc IDs / 32 blocks.
  Same command GREEN: zero DF walks/doc IDs/blocks, identical IDs and score bits.
- Independent literal N/tokens/DF/BM25 and field-union cases cover partial/all/zero
  live membership, strict field subsets, duplicate/reordered fields, filtering,
  active writes, deletion invalidation and retention with source ordinal changes.
- Deterministic concurrent misses perform one exact count. Eviction/long field keys
  remain bounded; budget refusal and poisoned-cache errors are typed. A real Store
  admission proves reservation lifetime through eviction and final Arc drop.
- Two deliberate plants release the reservation early and ignore field identity.
  Both intended tests fail; restored code passes. Commands/exits/logs are preserved
  in astra-17-raw/directed-plants.json.
- Admission RED: a singleton query increases cache occupancy from one to two.
  Same command GREEN: singleton/deleted-singleton/absent queries preserve occupancy
  and exact DF, while the common term stays cached. One first GREEN build failed
  on a test observer field accessor; the compile failure and successful retry remain.

Final focused tests: six unit and three public tests, all passing. Five selected
checkpoint cases also pass: independent lifecycle BM25, physical-purge parity,
structured term/prefix/fuzzy standalone-versus-hybrid scores, persisted phrase
positions, and prefix/fuzzy/phonetic provenance. See fast-path-checks.json and
checkpoint.json under astra-17-raw. No zero-match case or ignored test is counted.
Final targeted Clippy passes with the same five pre-existing warnings; its exact
receipt is astra-17-raw/final-clippy.json. Scoped rustfmt and git diff --check pass.

## Measured revisions

1. astra-17-measured: initial admission caches every fitting key. Repeated common
   queries improve, but mostly-unique p95 regresses 19.35%/58.34% on 8K one/eight
   tombstoned segments. This policy is rejected; complete negative artifacts remain.
2. astra-17-admitted: exact zero/singleton handling removes those regressions.
   All-live controls still cost 0.041-0.500 us; this intermediate result is preserved.
3. astra-17-final: restore the original no-cache frequency loop. Retain the
   repeated-term and mostly-unique improvements below. One all-live warm-common
   control remains +0.125 us (+9.37%); other required controls are within 5% or improve.
   The >5% screen triggered the admission and no-cache-path investigations above.
   No further corpus-specific tuning is introduced; native relevance is checked
   separately. Source and binary identities for every revision are retained.

## Final matched core screen

Apple M3 Max / Mac15,9, 128 GiB, macOS 27.0 (26A5388g), Rust 1.93.0 aarch64.
Opt3, fat LTO, one codegen unit, panic=unwind, stripped symbols. Before=501948b;
after=501948b plus the scoped patch and new live_df.rs. Every timing binary has
core_features=[] verified from Cargo artifacts. Fault observers are absent.

The same public Store example uses 8,192 rows in one/eight segments and 65,536
rows in one segment, each all-live or with every fourth row deleted. Two-dimensional
vectors and literal common/group/unique text isolate core retrieval; no embedding
model or FiQA data participates. k=10; 20 warmups then 128 samples per workload.
Before/after, after/before, before/after independent processes; graph maintenance,
compilation and BEIR heavy jobs are paused. The fixed load<=3 idle gate delayed
the final matrix before starting; unrelated user processes were not changed.

Timer includes the public Store lexical call, not query-term construction or
result JSON serialization. This is core latency, not the TextStore query API.
Percentiles are nearest-rank per process, then median across three processes.
All 18,432 steady calls / 96,192 returned hits agree exactly; 36 separately timed
first-DF observations also preserve their hit payloads. Full per-process ranges
and raw samples are in astra-17-final/summary.json and each result file.

| Rows | Segments | Live rows | Workload | Before p50 us | After p50 us | Before p95 us | After p95 us | p95 change |
| ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 8192 | 1 | 8192 | warm-common | 1.250 | 1.334 | 1.334 | 1.459 | +9.37% |
| 8192 | 1 | 8192 | repeated-groups | 9.292 | 9.166 | 9.458 | 9.459 | +0.01% |
| 8192 | 1 | 8192 | mostly-unique | 1.000 | 1.041 | 1.125 | 1.167 | +3.73% |
| 8192 | 1 | 8192 | absent | 0.583 | 0.584 | 0.625 | 0.625 | +0.00% |
| 8192 | 1 | 6144 | warm-common | 32.583 | 1.333 | 33.291 | 1.375 | -95.87% |
| 8192 | 1 | 6144 | repeated-groups | 11.459 | 9.291 | 11.708 | 9.458 | -19.22% |
| 8192 | 1 | 6144 | mostly-unique | 1.125 | 1.042 | 1.291 | 1.208 | -6.43% |
| 8192 | 1 | 6144 | absent | 0.583 | 0.583 | 0.625 | 0.625 | +0.00% |
| 8192 | 8 | 8192 | warm-common | 4.458 | 4.417 | 4.584 | 4.542 | -0.92% |
| 8192 | 8 | 8192 | repeated-groups | 19.375 | 19.500 | 20.500 | 20.458 | -0.20% |
| 8192 | 8 | 8192 | mostly-unique | 2.875 | 2.875 | 3.208 | 3.167 | -1.28% |
| 8192 | 8 | 8192 | absent | 1.500 | 1.500 | 1.583 | 1.583 | +0.00% |
| 8192 | 8 | 6144 | warm-common | 68.833 | 4.708 | 70.916 | 4.833 | -93.18% |
| 8192 | 8 | 6144 | repeated-groups | 25.334 | 20.167 | 26.750 | 21.417 | -19.94% |
| 8192 | 8 | 6144 | mostly-unique | 3.042 | 3.000 | 3.375 | 3.250 | -3.70% |
| 8192 | 8 | 6144 | absent | 1.625 | 1.584 | 1.667 | 1.625 | -2.52% |
| 65536 | 1 | 65536 | warm-common | 1.083 | 1.084 | 1.125 | 1.167 | +3.73% |
| 65536 | 1 | 65536 | repeated-groups | 62.125 | 62.083 | 64.375 | 64.250 | -0.19% |
| 65536 | 1 | 65536 | mostly-unique | 1.417 | 1.209 | 1.917 | 1.666 | -13.09% |
| 65536 | 1 | 65536 | absent | 0.375 | 0.375 | 0.458 | 0.458 | +0.00% |
| 65536 | 1 | 49152 | warm-common | 249.208 | 1.125 | 255.250 | 1.209 | -99.53% |
| 65536 | 1 | 49152 | repeated-groups | 77.500 | 61.750 | 79.541 | 64.500 | -18.91% |
| 65536 | 1 | 49152 | mostly-unique | 1.625 | 1.209 | 2.042 | 1.542 | -24.49% |
| 65536 | 1 | 49152 | absent | 0.334 | 0.375 | 0.375 | 0.375 | +0.00% |

## First DF and memory

An absent query primes assembly before the first common-term query. Cold values
below are medians of three single first-DF observations, not cold-API percentiles;
initial cache-capacity allocation is outside that timer. Memory is the measured
Store cache charge, constant from initial assembly through all query workloads.

| Rows | Segments | Tombstones | First DF before us | First DF after us | Cache before B | Cache after B | Added B |
| ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 8192 | 1 | False | 4.041 | 4.000 | 66136 | 66152 | 16 |
| 8192 | 1 | True | 34.792 | 34.166 | 66136 | 72920 | 6784 |
| 8192 | 8 | False | 7.166 | 7.042 | 18856 | 18928 | 72 |
| 8192 | 8 | True | 71.500 | 74.458 | 14760 | 68976 | 54216 |
| 65536 | 1 | False | 4.375 | 3.958 | 66136 | 66152 | 16 |
| 65536 | 1 | True | 257.292 | 244.875 | 66136 | 72920 | 6784 |

The DF cache itself reserves 6,768 bytes per partially-live contribution. Small
additional layout costs also affect all-live assemblies. Process maximum RSS is
202,358,784–202,997,760 B before and 198,164,480–203,997,184 B after; ranges overlap.
RSS does not establish a memory saving: exact accounting shows the cache increase.
Ingestion/close are outside query timing; their observations remain in raw files.

## Native TextStore confirmation

Full report/raw root: `/private/tmp/ze-astra17-native-mmcjqffv`. Before=`7d0f9ef`;
after=`501948b` plus final Step17, confirming Steps16+17 together. Identical tested
clean71 adapters, harness, FP32 document bundle and CoreML FP16 query sidecar.
Original experimental worktree and prior artifacts are preserved. A separate
CARGO_TARGET_DIR is used; both native builds prove core_features=[].

All 648 judged queries are executed per cell. Intact FiQA contains 57,638 parents
and 58,980 chunks. A fixed every-fourth-parent deletion removes 14,410 parents /
14,761 chunks, leaving 43,228 parents / 44,219 live chunks. Both arms use byte-
identical independent index clones for each state, with matched health checks.
The stress quality column retains the original qrels, so it includes unavailable
deleted relevant documents; it is a before/after parity control, not a new model
quality comparison against the intact corpus.

All 24 AB/BA/AB independent processes succeed: 15,552 measured public TextStore
calls after 20 warmups per process. Query timing includes lexical analysis, model
tokenization/embedding where applicable, retrieval and returned-text construction.
Result serialization, store open and model loading are outside the API timer.
Exact full payloads (including returned text, epoch, IDs, revisions, chunks, score
bits and optional component scores) and model token IDs match across every arm
and repetition. nDCG/recall are unique-parent metrics from original judged IDs.

| Index | API | Before p50 ms | After p50 ms | Before p95 ms | After p95 ms | p95 change | nDCG@10 before/after | recall@10 before/after |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Intact | lexical | 0.929333 | 0.923708 | 3.320500 | 3.322125 | +0.05% | 0.234286132 / 0.234286132 | 0.288113049 / 0.288113049 |
| Intact | hybrid | 5.318333 | 5.311750 | 6.101417 | 6.073000 | -0.47% | 0.284278117 / 0.284278117 | 0.350369375 / 0.350369375 |
| Fixed deletions | lexical | 1.260542 | 1.140875 | 4.190250 | 3.803500 | -9.23% | 0.200316874 / 0.200316874 | 0.245075141 / 0.245075141 |
| Fixed deletions | hybrid | 4.410958 | 4.412708 | 6.629625 | 6.225458 | -6.10% | 0.242766263 / 0.242766263 | 0.299486111 / 0.299486111 |

The intact API controls are effectively unchanged. On the fixed deletion stress,
lexical p95 improves 9.23% and hybrid p95 6.10%, with no quality or payload change.
These are fresh matched values for this experiment; do not splice their absolute
latencies into earlier sessions. Dense is unchanged by these lexical-cache steps
and was not rerun at this checkpoint. This does not establish all APIs near 1 ms
across workloads, cold starts or concurrency.

Per-process percentile ranges, maximum RSS, full source hashes, adapter parity,
commands and process exits are recorded in the native summary/receipts. Original
full-text results stay in the raw root; the repository archive retains samples,
all score/identity fields and SHA256 text digests plus whole-result file hashes.

## Reproduction and artifacts

Source patches are committed as lossless `source.patch.gz` files; decompress
them to recover the byte-identical patches named in original build receipts.
Uncompressed originals remain in their local experiment directories.

- Public core example: crates/zeppelin-embed-bench/examples/live-df-cost.rs.
- astra-17-raw: focused RED/GREEN, fault plants, nine final tests, five checkpoint
  commands, compile failure, lint receipts and exact stdout/stderr.
- astra-17-measured, astra-17-admitted, astra-17-final: source/flags/hashes,
  build/run/analyze scripts, raw timings/control payloads and process receipts.
- Each matrix is six processes. Previous before binary is reused by hash, not rebuilt.
  Never use these microsecond core values as millisecond end-to-end API latencies.
- Full workspace/coverage/linked-size qualification remains for the final assembled
  change pass; focused GREEN does not claim that qualification.
