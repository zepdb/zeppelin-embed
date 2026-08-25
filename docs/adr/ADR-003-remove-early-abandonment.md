# ADR-003 — Remove scan-tier early abandonment

Status: ACCEPTED, 2026-08-20. Owner-directed.

## Context

Early abandonment was implemented as a conservative, block-granular scan
optimization, validated for correctness, and measured across schemes, thread
counts, and fixtures. It never won a measurement. As with the Bit1 and Bit2
retirement in ADR-002, the implemented feature is deleted rather than retained
behind a flag when its value depends on infrastructure that does not exist.

The decisive performance measurements were taken on an M3 Max in a
single-tenant run pinned to performance cores, using `[profile.bench]`
(`opt-level = 3`, fat LTO, one codegen unit). The fixture was 1,000,000 rows by
768 dimensions with clustered values. Relative standard deviation was below
1%:

| scheme | abandon off | abandon on | penalty |
| --- | ---: | ---: | ---: |
| Bit4, 1 thread | 20.54 ns/row | 37.10 ns/row | +81% |
| Bit4, 12 threads | 3.26 ns/row | 6.79 ns/row | +108% |
| f32, 1 thread | 45.91 ns/row | 54.34 ns/row | +18% |

> **CROSS-REFERENCE (BL-084, added 2026-08-25).** `tasks/evidence/05-scan-wallclock.md`
> Table C publishes the one-thread row of this experiment as 20.39 -> 36.48
> (+79%). The twelve-thread row agrees exactly in both. **This ADR is the
> record of decision; cite these numbers, not Table C's.**

Abandonment did not win for any measured scheme, thread count, or fixture.
On clustered data `blocks_skipped` was zero. `rows_abandoned` reached 179,000
of 1,000,000, but saved no bytes: a PDX column read serves every row in its
block, so abandoning individual rows after that read cannot reduce traffic.

## Decision

Remove early abandonment from the scan tier entirely. Delete its public option,
error, and counters; its deterministic threshold prepass; its bounds and
dispatch slots; its in-memory per-block extrema; its tests and properties; and
the scan benchmark's `--abandon` flag.

The PDX byte layout, block geometry, persisted bytes, and vertical SIMD scan
kernels remain unchanged. Whether PDX itself remains on the future scan path is
a separate owner decision.

## Rationale

The disqualifier is the data layout, not correctness. PDX blocks are arbitrary,
unsorted 64-row slices. In 768 dimensions, an axis-aligned minimum/maximum box
over 64 unrelated vectors spans nearly the full value range on almost every
coordinate. Its upper bound therefore almost never clears the current top-k
threshold, including on the clustered fixture. A sound bound that cannot skip
a whole block before payload access has no opportunity to save work or bytes.

Bound evaluation then adds pure cost. Bit4's penalty is about 4.5 times f32's
in the single-thread comparison because Bit4 bound evaluation is scalar while
the f32 bound evaluator is SIMD. Vectorizing Bit4's bound evaluation was NOT
attempted. That work could reduce evaluation cost, but it cannot repair the
zero whole-block skip rate, which is the actual disqualifier.

Several less invasive designs were implemented before deletion:

- A thread-invariant deterministic prepass seeded every worker with the same
  initial top-k threshold.
- Whole-block skip consulted extrema before reading any block payload.
- Slab-boundary checks exited a block's column sweep early when the remaining
  upper bound could not reach the threshold.
- Abandonment was block-granular rather than per-row, so a successful decision
  could actually avoid shared PDX column reads.

None overcame the loose bounds produced by arbitrary row grouping.

The correctness work was sound and is not the reason for removal. The bounds
were conservative; abandonment was lossless in candidate ids and scores,
including the permanent score/id tie contract; and the implementation was
mutation-verified.

## Conditions for reintroduction

Early abandonment should return only as a new design after all of the following
conditions hold:

- Rows are grouped by similarity so each PDX block is a coherent cluster, as
  contemplated by Tasks 19/20, rather than an arbitrary unsorted slice.
- The bound shape is tighter than an axis-aligned box, for example a
  centroid-plus-radius bound or a Cauchy-Schwarz norm bound.
- Bound evaluation is vectorized for every supported scheme, including Bit4.
- The decision can skip whole blocks BEFORE reading their payload, and measured
  evidence shows a meaningful nonzero whole-block skip rate.

Per-row abandonment inside a PDX block can never save payload bytes because a
column read serves every row in that block. A future design that cannot reject
the whole block before the first payload read does not satisfy these conditions.

Reintroduction is therefore a rewrite tied to row organization and a different
bound representation, not a tuning or feature-flag exercise.

## Consequences

- `ScanOptions` controls only the worker budget.
- `ScanStats` reports only deterministic work that can vary structurally:
  `dims_touched`, `bytes_read`, and `threads_used`.
- PDX construction no longer computes f32 or Bit4 nibble extrema, and scan
  setup no longer computes Bit4 factor extrema.
- The static library carries no dormant abandonment implementation.
