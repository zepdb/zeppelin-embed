# ADR-004 — Remove the PDX dimension-major scan layout

Status: ACCEPTED, 2026-08-20. Owner-directed.

## Context

Task 05 introduced PDX as a dimension-major layout within blocks of vectors so
that early abandonment and autovectorization could coexist. Neither premise
survived measurement. ADR-003 removed early abandonment after it never won, and
this engine hand-writes runtime-dispatched NEON rather than depending on
autovectorization.

PDX actively obstructs the adjacent-lane input shape required by SDOT. At 768
dimensions the vertical Bit4 kernel issues 192 vector operations per row: 48
SDOT, 48 shift/mask, and 96 ZIP operations. The horizontal row-major kernel
issues 96 vector operations with no ZIPs because it block-interleaves query
nibbles once during query preparation. The 96 ZIPs are a transpose tax caused
by storing one row's coordinates at a stride equal to the block row count.

The decisive measurements used an M3 Max in a single-tenant run pinned to
performance cores, `[profile.bench]` (`opt-level = 3`, fat LTO, one codegen
unit), and a clustered fixture of 1,000,000 rows by 768 dimensions. Cell =
row-major/PDX elapsed-time ratio; below 1.0 means row-major was faster:

| scheme | t=1 | t=2 | t=4 | t=8 | t=12 |
| --- | ---: | ---: | ---: | ---: | ---: |
| Bit4 | 0.513 | 0.521 | 0.765 | 1.098 | 1.104 |
| Int8 | 0.923 | | | | 0.981 |
| f32 | 6.382 | | | | 1.991 |

At one thread Bit4 took 20.42 ns/row in PDX and 10.50 ns/row in row-major.
The crossover at roughly six to seven threads is roofline arithmetic, not a
layout advantage: PDX remains compute-bound until `20.42 / N` falls below the
approximately 3.1 ns/row memory-bandwidth floor. The product deployment regime
is one to four threads on device, not twelve saturated performance cores.

The apparent PDX wins in the table were artifacts of row-major arms written as
correctness oracles rather than production paths. `scan_bit4_rows` allocated a
partition-wide score vector, filled it in one complete pass, then made a second
complete pass into bounded top-k. `scan_f32_rows` made a full corpus-byte
non-finite validation pass on every query. PDX cached the f32 validation result
at encode time, so this validation asymmetry—not dimension-major storage—caused
the 6.382 ratio.

Later tiers make PDX still less suitable. Task 16's small-allow-list branch and
Task 19's graph tier score a few thousand scattered rows. Reading one 768-d
Bit4 row from PDX touches about 192 distinct cache lines to recover 384 packed
bytes, approximately 62x read amplification over contiguous row-major bytes.

## Decision

Remove PDX from the scan path and make row-major the only scan layout. Delete
`PdxMatrix`, all PDX `ScanRows` variants, the vertical scalar/NEON/AVX2 dispatch
slots and implementations, PDX parsing and fuzzing, PDX-specific tests, and the
benchmark's layout-comparison machinery.

Promote the former oracle arms to production implementations. Bit4 scores
fixed four-row windows into a stack buffer and emits candidates inline without
a partition-wide allocation or second pass. Full-precision rows are wrapped in
an immutable row-major container that caches the first non-finite scalar once,
preserving the typed rejection and scalar index while removing the per-query
corpus sweep.

## Rationale

Row-major is the universal donor and therefore the safe direction to be wrong
in. A future PDX or clustered-column representation can be re-derived from
row-major bytes at seal time. The measured encode cost is 0.78 seconds per one
million Bit4 rows and 1.62 seconds per one million Int8 rows. Persisting PDX as
the primary representation and later needing efficient scattered-row access
would instead require migrating every sealed segment.

Task 07 has not made PDX bytes durable, so removal now changes no persisted
format. Quantization scheme ids are unaffected; retired ids 3 and 5 remain
permanently reserved.

## Conditions for reintroduction

Dimension-major storage may return only with either:

- measured evidence on a real non-NEON target showing that it wins the product
  workload; or
- a clustered-block design of the kind contemplated by ADR-003, where
  whole-block skipping genuinely requires column-wise reads.

Task 07's region directory can readmit such a representation additively. It
must be encoded from the row-major universal donor at seal time rather than
replacing row-major as the durable source.

## Consequences

- Horizontal `dot_f32`, `dot_f16`, `dot_i8`, `dot_bit4`, their batch variants,
  and the campaign-tuned Bit4 four-row path are the only scoring kernels.
- Bounded top-k, deterministic parallel merge, row masks, candidate streaming,
  and the descending-score/ascending-row-id tie contract remain unchanged.
- `dims_touched` and `bytes_read` now describe row-major scoring work directly
  and remain deterministic across worker counts.
- Random scattered candidates in Tasks 16 and 19 retain contiguous row access.
- The static library carries no PDX decoder, block geometry, transpose kernels,
  or dormant vertical dispatch tables.
