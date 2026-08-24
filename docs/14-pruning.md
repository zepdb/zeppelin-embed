# Task 14 — dynamic pruning: what shipped, what is owed

## What shipped

Block-max MAXSCORE and block-max WAND over the task-13 posting format, a
selection rule, NEON decode kernels with a scalar oracle, and deterministic
counter contracts.

The permanent guard is `tests/prune_equivalence.rs::prop_pruned_topk_equals_
exhaustive`: pruning must return exactly what the exhaustive scorer returns —
same ids, same scores, same tie-break — across 1..=4 segments, both
strategies, and a deliberately skewed vocabulary. Green at 2,048 cases.

Two real defects it caught during implementation, both recorded because they
are the failure modes the property exists for:

1. The MAXSCORE essential/non-essential split tested the **suffix** sum of
   term bounds where the rule requires the **prefix** sum. Effect: terms were
   declared non-essential while still able to lift a document, and a true
   top-1 was dropped. A wrong answer, not a slow one.
2. MAXSCORE summed term contributions in **bound-sorted order** while the
   oracle sums in **query order**. Floating-point addition is not
   associative, so results differed by one ULP. Fixed by accumulating per
   term slot and summing in query order — not by widening the tolerance.

## Counter contracts

Wall-clock is evidence; counters are the gate. Contracts are flat
`key = value` files under `tests/prune_contracts/`, parsed by a nine-line
helper. This is a deliberate deviation from the spec's "TOML contracts": no
TOML crate is in the dependency budget and a hand-written TOML parser is
incidental complexity. Literal TOML would be a recorded dev-dependency
decision for the owner.

Captured on a deterministic Zipf corpus of 2,000 documents:

| scenario | docs evaluated | blocks decoded | blocks skipped | postings decoded |
| --- | ---: | ---: | ---: | ---: |
| short list, 20 docs, 1 term | 20 | 0 | 0 | 20 |
| Zipf, terms t0+t5, k=10, WAND | 379 | 38 | 31 | 713 |
| Zipf, terms t0..t5, k=10, MAXSCORE | 53 | 80 | 46 | 2,254 |

Read honestly: the two-term WAND case evaluates 379 of 2,000 matching
documents, about 19%. The plan's "at most 2% of matching docs" is a GOV2
figure from a 25M-document x86 collection and is **not reached at this corpus
size**, which is expected — pruning's advantage grows with collection size,
and 2,000 documents is three blocks per list. The contract pins the exact
measured count, per the plan's own instruction that capture pins the number.
Nothing was widened to reach green.

## OWED: the NEON roofline campaign

**Status: NOT DONE. Deferred by owner decision on 2026-08-23, to be run after
task 15.**

The kernels in `src/kernels/postings.rs` are correct and property-tested
against the scalar oracle at every bit width 0..=32. They are **not**
optimized the way the vector side was, and this document exists so that gap
is not mistaken for completion.

What the vector path did, and what this path owes:

| step | vector side | postings side |
| --- | --- | --- |
| establish a hardware ceiling | 80.689179 GB/s single-core wide load (`tasks/evidence/02-platform-truth.md:261`) | **owed** — no decode-rate ceiling derived |
| express the kernel against it | Bit4 at 36.634 GB/s = 45.40% of ceiling (ADR-002) | **owed** — no percentage-of-roofline figure exists |
| iterate with a recorded ledger | `tasks/evidence/opt-ledger/B1-bit4.md`, 10 iterations | **owed** — no ledger entry |
| register variants for the frontier harness | appended dispatch slots, Task 03/04 convention | **owed** — `unpack`/`prefix_sum` are plain functions, not registered variants |

Specific work items:

1. Derive the decode roofline. Postings decode is not obviously
   bandwidth-bound the way a vector scan is: a 64-posting block at 16-bit
   deltas is 128 bytes, one cache line, and the arithmetic per byte is
   higher. The ceiling may be issue-width or dependency-chain bound rather
   than bandwidth bound, and establishing **which** is the first task, not an
   assumption to carry in.
2. Then measure the current kernels against it, single-tenant, with the same
   taint discipline as the WAL and graph harnesses (commit 489bb43).
3. Then sweep the knobs the plan already names: unpack-kernel shape per bit
   width (shift/mask ladder against a `tbl`-based gather), block size
   {32, 40, 64, 128}, and prefix-sum strategy.
4. Register every shape as a dispatch variant so task 27-B4 can search them.

Known weaknesses in the current kernels, to attack in that campaign:

- `unpack_neon` falls back to the scalar oracle for bit widths above 25,
  because a lane would span more than the four bytes the current gather
  loads. Widths 26..32 therefore get no vectorization at all.
- The per-lane byte gather is scalar code inside the vector loop; it builds
  `words` and `shifts` arrays element by element before a single vector
  shift. That is very likely the bottleneck, and a `tbl`-based shuffle is the
  obvious replacement.
- `prefix_sum_neon` uses saturating adds (`vqaddq_u32`) to match the scalar
  oracle exactly, which is a correctness requirement, not a performance
  choice. Whether saturation costs anything against wrapping adds is
  unmeasured.

No number in this section has been measured. This machine is not
single-tenant, and a contaminated decode rate is worse than no decode rate.

## Also owed

- Task 11's adversarial runner does not exist (`scripts/adversarial-smoke.sh`
  is absent), so the spec's pruning-op extension is a named backlog item
  rather than built. No private runner was invented.
- The tantivy and SQLite FTS5 comparison harness is not built. It must live
  outside the production workspace graph, like the fuzz workspace, so neither
  ever enters the engine `Cargo.lock`.
