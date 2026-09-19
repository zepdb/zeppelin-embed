# ZE-122 final adjacency seam review

Read-only review, 2026-09-19. Read the complete adjacency section in
`tasks/evidence/ze-122/parallel-contracts.md`, live ZE-124, storage plan lines
73-81, identity's fixed relationship endpoint/type contract, main
`storage/artifact.rs`, `storage/tree.rs`, and `wal::CommitState`. Inspected the
ongoing ZE-43 compact-value bound only to check the descriptor-size assertion;
its unfinished interfaces are not adopted by ZE-124. No code/tracker edits or
product tests were performed for this review.

## Conclusion

The proposed allocation-free borrowed-input/caller-output codec and bounded
merge are independently implementable from main after ZE-122 closes. They do
not require moving ZE-43 source/sink/memory APIs. Main has complete strong ID and
PhysicalRef types. Main BlockKind currently ends at 10; reserving explicit tags
13/14 while leaving 11/12 to ZE-43 is additive and avoids a collision. A direct
base with 4,096 32-byte entries and a delta with 2,048 40-byte entries fit well
inside the existing 4 MiB artifact limit. Nine 32-byte physical references leave
224 bytes of a 512-byte compact value for descriptor metadata; the final exact
layout still needs the promised implementation review.

No acceptance shortcut was found: the documents explicitly leave real OUT/IN
agreement, same-batch endpoints, both-endpoint liveness, authoritative records,
root atomicity, admitted views, publication/recovery and whole-participant
missing-reverse proof with ZE-44/45/47. The paired direction oracle is correctly
limited to kernel/model evidence. No synthetic row count or caller allocation
claim substitutes for authentic storage/writer/shared accounting.

## Two semantics to make explicit before freezing the codec

1. **Sequence lineage and view cutoff.** “Compatible sequences” must become a
   concrete check. Use the native committed WAL sequence domain (currently u64
   in `wal::CommitState`, distinct from GraphGeneration). The base/range needs a
   known consolidation watermark, and each accepted delta must be newer than
   that watermark and no later than the admitted input cutoff. State whether
   run order is nondecreasing and retain the advertised equal-sequence exact
   duplicate rule. Otherwise a stale run can resurrect an already consolidated
   deletion, or a future run can leak into an old view. This can be explicit
   validated input metadata or reviewed persisted header fields; it need not
   depend on GraphReadView or publication code in ZE-124. Where the kernel is
   given only already-selected committed runs, document exactly which cutoff
   check remains mandatory in the ZE-44 adapter rather than implying the kernel
   proved it.

2. **Topology agreement across different sequences.** The same RelId must keep
   the same other endpoint across every supplied base/delta observation, even
   when a later sequence wins or deletes it. Checking inconsistent neighbors
   only at equal sequences is insufficient. A newer delete with a wrong neighbor
   must not hide corrupt older topology; a newer insert must not relocate an
   existing RelId. The existing fixed endpoint/type identity contract already
   requires this, so it is clarification rather than a new product decision.
   The kernel checks its complete supplied group; authoritative cross-group or
   missing-record validation still belongs to ZE-44.

## Implementation-review checklist retained by the seam

- Validate every complete encoded run before any output is valid, including
  trailing/truncated bytes, version/tags/reserved bytes, declared exact length,
  bounds/count arithmetic, strict full-u128 order, duplicate rules, and complete
  node/type/direction/range correlation. A late corrupt entry cannot hide behind
  an early LIMIT/output capacity. Separate absent/empty output from malformed
  zero-width or reversed interval geometry in the typed constructors.
- A ninth run or 2,049th pending entry triggers private consolidation before it
  is admitted, not truncation or silent widening. Define the planner/kernel
  result that reports required split/output capacity. A merged 4,097-entry range
  splits by a real next RelId; infinity remains a tag, including u128::MAX.
  Empty result ranges are omitted without losing routing for later insertion.
- Input validation, merge comparisons, partitioning and encoding must checkpoint
  actual entry/byte work and a final success boundary. Typed limit/corruption/
  control errors return no valid partial result; partially filled caller output
  stays private and is discarded. There is no success-bearing streaming callback
  capable of publishing rows before all-input validation.
- The inner kernel should not call the current uncontrolled outer artifact
  decoder and then claim bounded cancellation. It consumes borrowed bytes; the
  real storage adapter owns admitted outer framing and close-first view checks.
  Existing FormatError construction owns an error String, so an allocation-free
  error-path claim also needs a compact nonallocating inner error representation
  or an accurately narrower allocation claim. The required control callback may
  carry the caller's typed error without allocating inside the codec.
- Reserve tags 13/14 explicitly in both enum and decoder; preserve all existing
  tags and goldens, and coordinate the two append-only match arms with ZE-43 at
  cherry-pick. Do not broaden PayloadRef role admission. Account real fixed
  scratch/control storage at integration as well as caller heap capacity.
- Independent edge-oracle controls must catch omitted deletes, stale/future
  sequence selection, wrong full IDs/neighbors, and a missing reverse-model
  entry, with same-seed clean controls. They do not prove real participant OUT/IN
  publication or endpoint liveness before those producers are integrated.

The two clarifications and exact offsets/signatures should be recorded in the
ZE-124 design/evidence before its first format commit. They do not require
starting a blocked storage integration ticket or copying unfinished ZE-43 APIs.
