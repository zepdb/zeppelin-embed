# Step 17 initial cache screen: admission revision required

Disposition: do not retain this initial admission policy. All six normal-feature
processes passed and all returned IDs/revisions/score bits agree. Mostly-unique
p95 regresses 19.35% and 58.34% on the two 8,192-row tombstoned fixtures.
The revision must preserve the repeated-term gain without this avoidable work.

Source: `501948b` plus `source.patch.gz` (losslessly compressed) and hash-verified `live_df.rs`.
Both binaries have no core features. 20 warmups, 128 samples per workload,
three alternating AB/BA/AB independent processes. Values are median process
nearest-rank percentiles. This is public core Store latency, not TextStore
embedding API latency. No graph, reference inference or builds overlap timing.

| Rows | Segments | Live rows | Workload | Before p95 us | After p95 us | Change |
| ---: | ---: | ---: | --- | ---: | ---: | ---: |
| 8192 | 1 | 8192 | warm-common | 1.458 | 1.458 | +0.00% |
| 8192 | 1 | 8192 | repeated-groups | 9.375 | 9.292 | -0.89% |
| 8192 | 1 | 8192 | mostly-unique | 1.166 | 1.167 | +0.09% |
| 8192 | 1 | 8192 | absent | 0.625 | 0.625 | +0.00% |
| 8192 | 1 | 6144 | warm-common | 32.875 | 1.375 | -95.82% |
| 8192 | 1 | 6144 | repeated-groups | 11.833 | 9.250 | -21.83% |
| 8192 | 1 | 6144 | mostly-unique | 1.292 | 1.542 | +19.35% |
| 8192 | 1 | 6144 | absent | 0.625 | 0.542 | -13.28% |
| 8192 | 8 | 8192 | warm-common | 4.750 | 4.625 | -2.63% |
| 8192 | 8 | 8192 | repeated-groups | 20.584 | 20.375 | -1.02% |
| 8192 | 8 | 8192 | mostly-unique | 3.167 | 3.250 | +2.62% |
| 8192 | 8 | 8192 | absent | 1.584 | 1.667 | +5.24% |
| 8192 | 8 | 6144 | warm-common | 71.584 | 3.917 | -94.53% |
| 8192 | 8 | 6144 | repeated-groups | 26.833 | 19.542 | -27.17% |
| 8192 | 8 | 6144 | mostly-unique | 3.500 | 5.542 | +58.34% |
| 8192 | 8 | 6144 | absent | 1.709 | 0.875 | -48.80% |
| 65536 | 1 | 65536 | warm-common | 1.208 | 1.166 | -3.48% |
| 65536 | 1 | 65536 | repeated-groups | 63.292 | 64.208 | +1.45% |
| 65536 | 1 | 65536 | mostly-unique | 1.750 | 1.833 | +4.74% |
| 65536 | 1 | 65536 | absent | 0.458 | 0.458 | +0.00% |
| 65536 | 1 | 49152 | warm-common | 256.666 | 1.084 | -99.58% |
| 65536 | 1 | 49152 | repeated-groups | 80.917 | 63.333 | -21.73% |
| 65536 | 1 | 49152 | mostly-unique | 2.167 | 2.042 | -5.77% |
| 65536 | 1 | 49152 | absent | 0.417 | 0.250 | -40.05% |

Exact controls: 18,432 timed calls / 96,192 returned hits.
The all-live eight-segment absent control increases 0.083 us (5.24%);
it is retained here and must be checked again with the revised binary.
Cold first common-query samples and full accounting/ingestion/RSS evidence
remain in `summary.json`, individual `results.json` files and receipts.
No first-query percentile is claimed from one cold observation per process.
