# ZE-122 final compiler and relational contract review

Review performed 2026-09-19 under root-owned ZE-122. Read-only review of the
proposed split, not implementation or runtime qualification. No tracker,
specification, product, or worktree changes were made by this reviewer.

## Sources and review boundary

- Main observed at `03d90adf1bfafc946cd5a64706c89be6a9c86bee`.
- `tasks/evidence/ze-122/parallel-contracts.md`, SHA256
  `9292258628b7ef17bd0f956c76c9ca9942bd5923e68b01be48fbd0826195181c`.
- Live ZE-123, ZE-125, ZE-126 descriptions and dependencies; original ZE-50
  and ZE-56 descriptions including their ZE-122 additions; ZE-102 resolution.
- Accepted `docs/graph/plans/{execution,cypher,retrieval,qualification}.md`.
- Landed `query/plan/mod.rs` NodeFacts and `query/runtime.rs` work limits.
- Prior detailed compiler audit in `compiler-lowering-audit.md` and the
  relational seam in `tasks/evidence/ze-122/execution-bindings-audit.md`.

No tests were run: the finding concerns contradictory documentary constants,
and this review adds no product behavior.

## Blocking finding: cardinality became examined work

`parallel-contracts.md:42` says "Enforce524288 members examined/list entries";
ZE-125 similarly says "<=524288 examined/list IDs". That wording introduces
an additional examined-input limit absent from the accepted decision. The
execution-bindings audit at lines 93–96 uses the same narrower formulation.

The accepted qualification table distinguishes:

- 524,288 total nested elements **in one list value** (line 124).
- 524,288 **materialized eligible IDs per set** (line 125).
- Separately cumulative per-query row visits, expression evaluations, hash
  probes, memory, and cancellation limits (lines 117–131).

ZE-102's resolution calls this a list/eligible *cardinality* limit. Landed
`WorkKind::EligibilityEntries` also has no 524,288 hard default: it falls
through to `u64::MAX`; actual bounded work remains subject to the other
query-wide limits and any explicit caller tightening.

For example, a DISTINCT collect/eligibility pipeline may examine 524,289
repetitions of one same-view node and materialize one ID. This is not itself
a list/set cardinality violation. Subject to its actual cumulative work and
memory, it must not acquire a new rejection solely from this scheduling split.
A supplied already-materialized list with 524,289 elements is different and
continues to fail the accepted per-value limit.

Required correction: state the list-value and materialized-set caps separately
from examined entries, sorting/hashing/copy work and cumulative counters. Do
not give DISTINCT input, repeated sets, or multiple calls a new 524,288 global
work ceiling. Apply the clarification to ZE-125 and the decision document;
annotate the supporting audit so it cannot reintroduce the narrowed rule.
The existing 4M row/16M hash limits are not waived by this correction.

## Compiler readiness and acceptance: no additional blocker

The proposed split now preserves the concrete missing prerequisites:

- ZE-123 owns the actual typed OR relationship alternatives, bounded
  current-edge predicate scope, validator and lineage updates. Both fixed and
  bounded paths retain every alternative. Predicate scope, null behavior,
  zero hops, private edge versus public list, 16-hop maximum and MATCH-level
  uniqueness are explicit.
- OptionalApply must prove its correlated anchor/rebinding and null extension
  after the entire right-side predicate; reused variables require equality and
  reprojection, not slot overwrite. These are required contract demonstrations,
  not presumed runtime support.
- ZE-123 must produce a compiled charged BoundQuery-to-GraphPlan scoped
  consumer and immutable API/commit before ZE-126 starts. ZE-126's native
  dependency on ZE-123 correctly enforces that readiness gate.
- ZE-126 covers the complete accepted non-search read lowerer, including
  sparse remapping, source spans, scope/barriers, alias precedence, retained
  names/parameters/facts, and simultaneous genuine backing ownership. It may
  start before physical pattern/relational execution once the compiled contract
  lands. It must not discard CALL or invent missing search IR; ZE-58 remains
  responsible for search mode/components/eligibility lowering.
- ZE-56 retains ZE-51 and now additionally depends on ZE-53 and ZE-126. Its
  public compiler-to-GraphStore acceptance still includes all 57 selected read
  coordinates (54 positive executions and 3 exact compile errors), real graph
  oracle, types/bags/order/errors, and actual resource/lifecycle evidence.
  ZE-59 retains full 99-coordinate qualification. Synthetic lowering tests
  cannot increment executed TCK counts.
- ZE-50 retains its original dependencies and now has the compiled pattern
  and actual compaction prerequisites. Kernel/source adapters cannot discharge
  the original real operator acceptance.

## Shared NodeFacts coordination

Current NodeFacts exposes `width()` and `slot(SlotId) -> Option<ValueKinds>`,
but its ordered slot array is private. ZE-125's ordered logical schema is
correct; numeric SlotId values are not physical column ordinals.

Reserve any additional NodeFacts accessor implementation to ZE-123, already
the owner of `plan/mod.rs`. A minimal possible signature is
`slot_at(ordinal: usize) -> Option<(SlotId, ValueKinds)>`, returning copied
read-only facts after checking the live width. The exact name/signature is
not frozen by this review and must be compiled and shared by its owner before
ZE-125 consumes it. Do not expose mutable facts or duplicate slot-layout
assumptions in the relational module.

This does not require blocking all ZE-125 work on ZE-123: composed lifetime
proofs and kernels can use an explicitly owned, charged ordered schema while
the small accessor is coordinated. A worker that reaches the shared accessor
must wait for its immutable producer commit rather than edit the shared file
or copy a moving implementation. Root owning ZE-123 and coordinating this
handoff is sufficient; the current proposal explicitly requires one owner.

## Disposition

Root accepted the finding and specified the correction: preserve the accepted
per-list and per-set caps, retain the original audit as immutable evidence, and
add an explicit correction ledger preventing its narrower wording from becoming
implementation authority. The normative document and live ZE-125 will carry
the corrected contract.

Root also confirmed that ZE-123 solely owns any new NodeFacts accessor.
ZE-125 will not edit `plan/mod.rs`; it can build an ordered distinct schema
from explicit operator slots and check existing `width()` / `slot()` facts.
Any later use of `slot_at` waits for ZE-123's immutable compiled commit.
Original ZE-51 already depends transitively on ZE-123 through ZE-50.

These resolutions remove the identified design objection. No further blocking
compiler prerequisite, lowering-scope, acceptance, or shared-NodeFacts
ownership defect was found. Final document/tracker write verification follows
below once root completes those writes.

### Final verification

Reread after root's completion signal: the normative evidence document now
separates both cardinality limits from examined work and explicitly assigns
the NodeFacts accessor to ZE-123. Its new SHA256 is
`08d086feb41f5393f56d0051feeb820a25a3b67dab9b1e3114098ef01ef3d68a`.
The correction ledger `review-resolutions.md`, SHA256
`6b76bf63e1e1860d5d7f77a539a548cc8d4e60943ddd41d991b76dfcc3af737c`,
explicitly overrides the historical audit's narrower sentence. Live ZE-125
updated at `2026-09-19T19:00:27.216Z` contains both corrections; live ZE-123
updated at `2026-09-19T19:00:27.154Z` confirms sole accessor ownership.

Read `dependency-audit.json`: it records all 256 original edges preserved,
the nine planned integration edges installed, release prerequisite closure
91 to 98, all 47 original implementation tickets retained, all six new
implementations included, and no cycles. This is a review of root's recorded
graph audit; the narrower compiler integration edges were also verified from
live ZE-50/56/123/125/126 earlier in this review.

**Final disposition: no blocking finding remains for this compiler and
relational contract review.** Compiled seam, real execution and TCK evidence
remain future implementation requirements exactly as recorded above.
