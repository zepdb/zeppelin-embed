# ADR-002 — Retire the 1-bit and 2-bit RaBitQ schemes

Status: ACCEPTED, 2026-08-20. Owner-directed.
Supersedes the BL-010 requirement that 2-bit be implemented and measured.

## Context

BL-010 recorded that 2-bit RaBitQ had been omitted from the quantization plan
without a reason, and the owner directed that it be implemented and measured on
all three axes rather than dropped on assumption. The explicit condition was:
"Acceptable outcome if 2-bit loses: an ADR recording that it was implemented,
measured, and dominated at every recall target, with the numbers. What is NOT
acceptable is omitting it or quoting the prior repo's result as if it were
measured here."

Both schemes were implemented, given native packed NEON kernels, and measured.
This ADR records the numbers and retires them.

## Measured evidence, all on M3 Max, single-tenant

Speed, matched ~512 MB working set at 10.67x the system-level cache, d=768,
against the 80.689179 GB/s wide-load ceiling:

| scheme | bytes/row | ns/row | GB/s | % of ceiling |
| --- | ---: | ---: | ---: | ---: |
| Bit1 | 108 | 416.108 | 0.260 | 0.32% |
| Bit2 | 204 | 43.309 | 4.710 | 5.84% |
| Bit4 | 396 | 10.810 | 36.634 | 45.40% |
| Int8 | 776 | 13.219 | 58.703 | 72.75% |

Recall@10 >= 0.95 at N=100,000 x 768, 32 queries, exact brute-force f64 ground
truth, across five distributions. Cell = qualifies / best recall reached within
a 16x oversample budget:

| distribution | Bit1 | Bit2 | Bit4 | Int8 |
| --- | --- | --- | --- | --- |
| uniform | NO 0.6906 | yes 0.9563 | yes 0.9844 | yes 0.9688 |
| anisotropic | NO 0.5406 | NO 0.8250 | yes 0.9656 | yes 0.9750 |
| clustered | NO 0.5656 | NO 0.8938 | yes 0.9500 | yes 0.9594 |
| heavy-tailed | yes 0.9500 | yes 0.9688 | yes 1.0000 | yes 0.9781 |
| correlated | NO 0.9375 | yes 0.9812 | yes 0.9531 | yes 0.9969 |
| **qualifies** | **1 of 5** | **3 of 5** | **5 of 5** | **5 of 5** |

## Decision

Retire Bit1 and Bit2. Make Bit4 the default. Keep Int8 as a configurable
non-default option.

## Rationale

The disqualifier is RECALL, not speed, which matters because it is not fixable
by optimization. Bit2 fails the 0.95 target on anisotropic and clustered data
even at 16x oversample; Bit1 fails on four of five distributions. No kernel
work changes that: scoring faster does not make a coarse ranking more accurate.

This is worth stating plainly because both schemes DO have known, unexploited
speed headroom. Bit2's kernel carries the same defect Bit4 had before its
campaign — four multiply-accumulates chained into a single accumulator — and
Bit1 has no runtime-dispatched SIMD slot at all, running a scalar loop at 0.32%
of the memory ceiling, where a bit-plane popcount decomposition was assessed as
worth 50-100x. Both were left unfixed deliberately: a faster kernel for a
scheme that cannot reach the recall target is wasted work.

Bit4 dominates both on the deciding metric anyway. It reaches 0.95 on every
distribution, and after its optimization campaign it is also 1.22x FASTER than
Int8 (10.810 vs 13.219 ns/row) at half the bytes, so there is no recall-versus-
speed trade left to arbitrate.

The prior repository's 2-bit result (`../zeppelin/tasks/July10Quant/`) was NOT
used as evidence. It was measured on an IVF plus object-store architecture with
1 MiB coalescing gaps, none of which exist here, and it self-flagged its recall
parity as "close to vacuous". Everything above was re-measured in this engine.

## Consequences

- Scheme ids 3 (Bit1) and 5 (Bit2) are PERMANENTLY RESERVED. Ids are an
  append-only storage contract: never renumber, never reuse, even though the
  code is gone.
- Campaign 27-B3's headline experiment was 4-bit query x 1-bit doc, BBQ-style
  scoring. Removing Bit1 voids the document side, so B3 must be re-scoped to
  asymmetric Int8/Bit4 variants or dropped.
- BL-027 is closed by construction: it recorded that Bit1 was the only scheme
  without an accuracy-bound test, and that a 2x scale error in est_dot_bit1
  passed all 29 quant tests. With Bit1 removed the gap cannot be reached.
- If a future workload is bandwidth-starved enough that 96 or 192 bytes per row
  outweighs the recall loss, these schemes must be REINTRODUCED UNDER NEW IDS
  with fresh recall evidence, not resurrected under 3 or 5.
