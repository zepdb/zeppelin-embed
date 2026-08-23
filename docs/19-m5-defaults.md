# 19-M5 shipped-default provenance

Base commit: `a34b3b7`. Machine: Apple M3 Max, one P-core per query,
`[profile.bench]`, 10,000 SIFT-1M queries per process. Every timing below is
load-tainted and **NON-AUTHORITATIVE**; the orchestrator's single-tenant rerun
is the release authority. All 20 tabulated-process canaries passed: the
one-pass/default runs covered 3.893-4.052 GHz and the matched-recall two-pass
runs covered 3.738-4.045 GHz.

## Adaptive ef and f32 pool rescore

The checked-in smoke curve under
`tasks/cross-benchmark/results/zeppelin-embed/sift-128-euclidean/` measured:

| ef | recall@100 | p50 ms |
| ---: | ---: | ---: |
| 100 | 0.738 | 0.062250 |
| 140 | 0.862 | 0.079333 |
| 200 | 0.932 | 0.109417 |
| 300 | 0.965 | 0.153791 |
| 400 | 0.978 | 0.203959 |
| 600 | 0.992 | 0.294250 |

`ef=140` is therefore only the research floor, not a passing shipped point.
The SIFT-class default is `max(2*k, max(ceil(1.4*k), 140))`, clamped to the
graph row count. It chooses `ef=200` for `k=100`, the first measured arm above
hnswlib's 0.9233 recall. Angular uses the owner-specified provisional `4*k`
floor from `tasks/reports/index-design-research.md` section 3.1. The full M4b
measurement in `tasks/evidence/19-M4b-cross-dataset-graphs.md` proves `2*k`
is insufficient on glove, but does not prove `4*k` sufficient; M10 owns that
campaign.

The query path exactly rescores every retained `ef` candidate from the stored
f32 rows through `quant::rescore::rescore_top_k`. The 10-process measurements
below observed `mean_rescored=200.000` in every shipped-default process.

## Alpha pass decision at matched recall

The complete 10,000-query check places the closest integer-ef recall match at
one-pass `ef=200` and two-pass `ef=195` (0.000290 recall apart). The one-pass
arm below used the CLI with no build-pass, prefetch, or ef override; it is the
literal shipped-default path.

| graph | recall@100 | p50 us, median across 10 processes | hops | C | rescored | pushes | between-process p50 spread |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| alpha=1.0, one pass, adaptive `ef=200` | 0.935179 | **124.8540** | 205.383 | 3021.162 | 200.000 | 638.340 | 121.834-126.084 us; 4.250 us (3.404%) |
| alpha=1.0 then 1.2, two pass, matched `ef=195` | 0.935469 | **130.7080** | 199.955 | 3485.158 | 195.000 | 627.544 | 128.000-132.834 us; 4.834 us (3.698%) |

One-pass process p50 values:
`121.834, 124.750, 124.875, 125.166, 124.833, 124.625, 125.708, 124.542, 124.958, 126.084` us.
Median within-process RSD was 10.751%.

Two-pass process p50 values:
`128.000, 131.458, 130.583, 132.834, 130.583, 129.958, 129.875, 131.084, 130.833, 131.917` us.
Median within-process RSD was 11.7885%.

At matched recall, pass 2 saves 5.428 hops/query (2.643%) and 10.796 pushes
(1.691%), but scores 463.996 more candidates (15.358%) and is 5.854 us
(4.689%) slower. That search result does not earn roughly half a build's cost.
The shipped build default is therefore one alpha=1.0 pass. Two-pass remains an
explicit research/refinement arm. For reference, at the same `ef=200` the
two-pass recall is 0.938780, only +0.003601 over one-pass.

Commands:

```text
cargo run --profile bench -p zeppelin-embed-bench --bin vamana-recall -- --passes one --cache-dir /private/tmp/zeppelin-embed-m5-sift1m-one
target/release/vamana-recall --passes two --cache-dir /private/tmp/zeppelin-embed-m3-sift1m
target/release/graph-search --queries 10000 --run N
target/release/graph-search --build-passes two --cache-dir /private/tmp/zeppelin-embed-m3-sift1m --ef 195 --queries 10000 --run N
```

## Other shipped graph constants

`R_target=32`, `R_max=44`, alpha build `1.0`, and `L_build=100` are the M3
SIFT-1M configuration whose recall measurements are recorded in
`tasks/latency.md` section 4.16 and were rechecked above. Alpha refine `1.2`
is retained only for the explicit two-pass arm; it is not a shipped default.
The 65,536-row checkpoint batch is an M3 operational resumability choice, not
a measured quality or latency fit (**NOT MEASURED**). The 10,000-row graph
crossover is the single-core crossover derivation in
`tasks/reports/index-design-research.md` section 3.3; it remains provisional
until task 20 measures the tier threshold.
