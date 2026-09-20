# ZE-44 integration contract review

Read-only review of integrated main `0e0064ddb3b1630e8c18050cb78c30ac7bcfb638`
and the frozen proposal in the ZE-44 worktree. No repository or tracker file was
edited and no product suite was rerun.

## Verdict

Proceed with the proposed 40-byte range key, 328-byte descriptor, unchanged
ZE-124 kernel, checked predecessor seam, and unified native candidate. The
overall ownership split is sound. The adapter should not be frozen until the
requirements below are explicit in code and focused tests, especially new-value
validation, adjacent-interval validation, the batch-aware catalog, and the
missing-endpoint versus tombstone distinction.

The proposal's statement that 328 bytes is within an existing 512-byte value
limit is inaccurate. `INLINE_BYTES = 512` is a tree key/separator emission
threshold in `storage/tree/directory.rs:316-318`; the page codec has no general
512-byte value limit. A 328-byte value with the 40-byte key fits the 16 KiB page
geometry. Define and pin `RANGE_DESCRIPTOR_BYTES = 328` directly instead of
relying on a nonexistent value limit.

## Compiled seams the adapter must preserve

- The OUT/IN directory key is already fixed at 40 bytes. Its comparator is
  unsigned numeric `(NodeId:u128, RelTypeId:u64, lower RelId:u128)` at offsets
  0/16/24, not little-endian byte ordering
  (`storage/tree.rs:269-313`). `TreeKind::OutRanges` and `InRanges` are separate
  comparator/root roles (`storage/tree.rs:15-31`), so direction is implicit in
  the root and must also agree with the redundant descriptor and inner payload.
- `GraphRoots` fixes the eight positions as Node, Rel, Fence, Label, Type, OUT,
  IN, Inventory (`storage/tree/directory/roots.rs:4-38`). `replace` refuses a
  root from another store or generation (`roots.rs:68-80`). The combined
  producer should start from the ZE-43 candidate roots, replace both adjacency
  roots there, and return that one bundle.
- `prepare_directories` proves exact batch/base/catalog/root identity, checks
  the real `StorageMemory` owner, derives target graph generation by checked
  base-generation increment, and validates every normalized delta's original
  generation (`storage/participant.rs:53-120`). It intentionally leaves OUT/IN
  unchanged (`participant.rs:32-47`). This is the correct private first half of
  `prepare_native_graph`.
- Relationship topology is already checked immutable during native record
  replacement (`storage/participant.rs:249-277`). The adapter should derive
  delete topology from the verified old relationship record and insert topology
  from the verified new relationship record; no caller-supplied parallel shape
  should be accepted.
- `GraphGeneration` and WAL sequence are separate compiled fields.
  `prepare_directories` advances generation (`participant.rs:86-98`), while
  `CommitState` carries an independent `sequence:u64`
  (`property_graph/wal/mod.rs:183-205`). WAL transition validation advances both
  independently (`wal/framing.rs:6-16`). The adapter must use checked
  `base.sequence + 1`, never the graph generation as an adjacency sequence.
- ZE-124's `RangeKey` retains full NodeId, RelTypeId, direction, lower RelId and
  tagged upper bound (`storage/adjacency/mod.rs:24-79`). Its limits are base
  4096, eight runs, 2048 pending, and 6144 merged output
  (`adjacency/mod.rs:13-22`). `merge` validates base watermark, cutoff and run
  order before a full preflight, then copies output and performs a final control
  checkpoint (`adjacency/merge.rs:57-158`). Do not duplicate or weaken that
  logic in the adapter.

## Required descriptor invariants

The proposed offsets are coherent and should be frozen exactly:

| Offset | Width | Required interpretation |
|---:|---:|---|
| 0 | 2 | version 1 |
| 2 | 1 | OUT=1 / IN=2, equal to containing tree kind |
| 3 | 1 | finite=0 / infinity=1 |
| 4 | 1 | active delta count, 0..=8 |
| 5 | 3 | zero |
| 8 | 16 | finite exclusive upper, or all zero exactly for infinity |
| 24 | 8 | base consolidation WAL watermark |
| 32 | 4 | exact sum of active delta entry counts, 0..=2048 |
| 36 | 4 | exact base entry count, 0..=4096 |
| 40 | 32 | required exact `AdjacencyBase` `PhysicalRef` |
| 72 | 256 | eight `AdjacencyDelta` slots; active prefix exact, inactive suffix zero |

Add the following canonical rules:

1. The base reference is always present, including for a newly created range
   whose base has zero entries. ZE-124 `merge` requires a base payload
   (`adjacency/merge.rs:61-80`), and generic `PhysicalRef` has no zero/sentinel
   absence convention (`storage/artifact.rs:145-159,598-652`). Encode a real
   empty base at the admitted base watermark when the first delta creates a
   range.
2. Do not persist an empty descriptor: `base_count == 0 && delta_count == 0`
   rejects/removes the directory entry. A persisted delta run must contain at
   least one observation even though the reusable ZE-124 codec accepts an empty
   run. This keeps property-only changes from consuming run slots and makes
   `pending == 0` equivalent to `delta_count == 0`.
3. Decode every active reference with the existing explicit reference codec,
   require base kind 13 and delta kind 14 at version 1, and require every
   inactive 32-byte slot to be all zero. Never widen `PayloadRef`; these are
   direct `PhysicalRef`s.
4. After complete ZE-124 validation succeeds, compare descriptor `base_count`
   and `pending` against the validated inner header counts, descriptor watermark
   against the base header watermark, and delta slots against the validated
   nondecreasing sequences. The descriptor key, direction and upper bound must
   equal every inner `RangeKey` exactly. Reading count/sequence header fields is
   safe only after full kernel validation; do not create a second permissive
   decoder.
5. Finite upper must be nonzero and strictly greater than key lower. Infinity
   requires zero upper bits and includes `u128::MAX`; no successor arithmetic is
   permitted.

## COW and containing-generation checks

This is the highest-risk integration point.

- A resolved adjacency block must match the requested reference exactly and
  have the same store/artifact, correct kind/version, and an artifact creation
  generation no later than the descriptor's actual containing leaf generation.
  The existing tree makes this distinction deliberately: a `DirectoryRoot`
  generation is only the bundle upper generation (`directory.rs:223-225`),
  while `DirectoryEntry::creation_generation` records the actual leaf generation
  (`directory.rs:1645-1673`). Tree descent narrows parent/child generation bounds
  (`directory.rs:418-539`). Use the entry generation for retained descriptors;
  use the target generation for a newly constructed descriptor.
- `insert_checked` calls the typed `LeafValidator` for retained entries on the
  copied leaf and checks immediate copied branch children
  (`directory.rs:418-509,936-1045`). It does **not** invoke that validator on the
  new caller-supplied value. Therefore validate every newly encoded descriptor
  and every referenced base/delta block before calling `insert_checked`. A
  malformed new value otherwise becomes durable despite a correct old-value
  validator.
- A per-entry `LeafValidator` cannot prove interval nonoverlap because the tree
  comparator sees only `(node,type,lower)`, not the stored upper. Before every
  replace, split or gap insertion, obtain the same-prefix floor/predecessor and
  first greater successor, decode both descriptors, and prove
  `predecessor.upper <= new.lower` and `new.upper <= successor.lower` when those
  neighbors exist. Exact split neighbors should meet at the actual split RelId.
  A predecessor from another node/type prefix is not a candidate range.
- Specify the new checked seek as greatest key `<= probe` (a floor), including
  exact-hit, in-leaf, cross-leaf, before-first and after-last cases. Reuse the
  existing checked path/generation/source machinery; do not implement a linear
  group scan. The forward cursor can supply the first greater successor. Tests
  must cover a predecessor in the prior prefix and corruption/control failure on
  both the selected path and the cross-leaf fallback.
- Order private split mutations so no intermediate candidate contains overlap.
  A temporary private gap is acceptable because failure returns no candidate;
  overlapping descriptors should never be admitted even transiently to a later
  checked mutation.

## Sequence and admitted-base binding

Retain both exact identities in the returned candidate:

- `BaseIdentity` supplies store, graph generation and the immutable root-envelope
  token (`staging.rs:18-27`).
- `CommitState` supplies the independent committed WAL cutoff and all eight root
  slots (`wal/mod.rs:183-205`).

At preparation, compare store/generation exactly, compare all eight optional
root positions (`None` as well as `Some`), and for each present WAL root compare
its exact `RequiredRef.block` to the corresponding `GraphRoots` reference.
Validate the `RequiredRef` object/block relation, store, generation/high-water,
TreePage role and version rather than trusting the publicly constructible
`CommitState` struct. Retain the expected base sequence and root array/digest in
the native candidate so the future sole coordinator can recheck the same base
at publication; checking only `BaseIdentity` would lose the adjacency cutoff.

Use `target_sequence = base.sequence.checked_add(1)` only for a changed batch.
An empty/replayed/no-op `StagedBatch` must not manufacture a target sequence,
generation or adjacency artifact. Add a positive control with deliberately
different numbers (for example generation 7 and sequence 100) and require delta
sequence 101, plus stale/future sequence negatives.

Incoming observations above 2048 may be spatially converted into bounded bases
inside the same private candidate as proposed. They must not become an
over-limit delta, additional public commits, or invented intermediate WAL
sequences. A base that includes the incoming candidate changes may carry the
single target sequence as watermark because it cannot escape before that exact
target commit publishes.

## Authoritative topology and catalog

The current `BatchCatalog` merges the retained catalog with the batch's new
symbol assignments (`storage/participant.rs:101-156`), but it is private to
`participant.rs`. The adjacency adapter must reuse or expose that same merged
view. Passing only the base `PreparationCatalog` cannot verify a relationship
using a newly assigned type, nor a same-batch endpoint node using newly assigned
label/property symbols. Do not create a second name-to-symbol interpretation.

Derive changes as follows:

- Insert: read the fully verified new relationship record from the final
  candidate relationship root; require both final candidate endpoints live;
  emit exactly one OUT `(source,type,rel,target)` and one IN
  `(target,type,rel,source)` observation at the target WAL sequence.
- Delete: read the fully verified old authoritative relationship record from the
  admitted base; retain its neighbor in both delete observations; relationship
  deletion remains valid even if a same-batch node DETACH makes an endpoint a
  tombstone.
- Property-only edit: verify old/new identity and topology agree and emit no
  adjacency observation. A batch containing only such edits must preserve both
  adjacency root references exactly.
- Self-loop: still emit one OUT and one IN copy in separate roots. Parallel
  relationships remain distinct by full RelId.

Avoid a blanket rule that every property-only relationship edit must have live
final endpoints. If staging permits an edit followed by a same-batch DETACH, the
raw authoritative record may remain for lazy sweeping while visibility is
suppressed by the tombstone. Endpoint-live rejection is mandatory for a newly
inserted/observable edge; delete and property-only semantics should follow the
already validated staged operation order.

## Liveness, lazy DETACH and readers

The compiled node codec distinguishes a live record from an explicit tombstone
(`storage/records/tombstone.rs:30-60`). Preserve three outcomes:

- missing node on an ordinary direct node lookup: absent;
- tombstone encountered as an endpoint of a retained relationship: relationship
  is logically hidden;
- missing/corrupt endpoint referenced by an extant authoritative relationship:
  corruption, not a hidden edge.

This distinction is necessary because DETACH retains the node tombstone until
the last incident relationship is swept. Treating a missing endpoint like a
tombstone would turn a broken required record into a silent wrong answer.
Likewise, an adjacency entry with a missing or mismatched relationship record is
corruption. A verified record must match full RelId, source, target and type for
the bound direction before its row is usable.

Apply the same helper to relationship lookup/scan, expand, degree/count and the
plain-DELETE incident probe. Check both endpoints before counting against caller
output capacity. The incident probe may stop at the first visible relationship
not present in the bounded explicit-removal set; it must propagate corruption,
work and cancellation errors. DETACH itself performs no adjacency enumeration
and must never acquire a degree-based rejection path.

## Ownership, accounting and all-or-none composition

- Use the same `PreparedObjects` sink for records, adjacency blocks and both
  directory roots. It resolves exact private references first and then the
  admitted base (`storage/prepared.rs:210-224`); appends enforce the one target
  generation (`prepared.rs:226-248`).
- Keep all change arrays, merged-edge output, encode bytes, descriptor buffers
  and fixed ZE-124 cursor state under the same `StorageMemory`. `StorageMemory`
  is nested under the authentic `WriteMemory` and checks that the staged batch
  has the same writer owner (`storage/memory.rs:21-86`; `participant.rs:62-64`).
  Reserve `MERGE_STATE_BYTES` as well as caller-buffer capacities; the constant
  excludes ordinary call-stack state (`adjacency/merge.rs:45-55`). Do not use an
  uncharged `Vec` because `StorageBuffer` begins with capacity but length zero;
  either initialize it within its admitted capacity or add a narrow charged
  helper rather than bypassing accounting.
- The identity callback is identity-only. `PreparedObjects` may burn a serial
  before `PrivateArtifact` allocation succeeds, and `abort_inventory` lists
  actual packs only (`storage/prepared.rs:64-113,151-201`). Writes must retain any
  exclusively created path and its accounting/cleanup owner even when it is not
  in storage's abort iterator. Captured callback/base heap remains with its real
  external owner.
- Return one wrapper that retains the ZE-43 candidate, exact expected base and
  WAL cutoff, and final eight roots. Do not expose an intermediate OUT-only or
  directory-only success from `prepare_native_graph`. A failure after records,
  after OUT, during IN, during split, or at final control returns no whole
  candidate; all created packs remain discoverable through the caller-owned
  `PreparedObjects` abort inventory. Storage still has no sync, publication,
  overwrite or unlink authority (`tree/directory.rs:50-73`).

## Focused acceptance checks

In addition to the proposal's tests, make these checks explicit:

1. literal 328-byte golden plus every active/inactive slot, count mismatch,
   base-absent, empty-descriptor, zero-entry-delta and direction/tree mismatch;
2. new descriptor malformed before insertion, retained old descriptor malformed
   under its old leaf generation, and a valid older base/delta referenced by a
   newer descriptor leaf;
3. predecessor/successor exact hit, cross leaf, first/last, different prefix,
   overlap attempt, finite gap, adjacent split and infinity/u128::MAX;
4. generation 7 / WAL sequence 100 positive control, overflow at sequence max,
   stale/future run and exact all-eight optional-root mismatch;
5. same-batch new endpoint plus new relationship type/labels through the merged
   batch catalog;
6. missing endpoint causes corruption, tombstone hides the edge, DETACH performs
   zero incident work, and plain DELETE checks only live incidents;
7. a failure after the first directional root proves no candidate escapes and
   preserves the complete private abort/path ownership evidence;
8. real-producer ignored-delete and missing-IN mutations fail the independent
   exact authoritative oracle, followed by byte-restored terminal GREEN.

## Short implementation recommendation

Freeze `range.rs` first with the exact canonical rules and one reusable
`validate_descriptor(source, root, entry-or-target-generation, cutoff, scratch)`
path. Add the bounded floor/predecessor seam and neighbor-overlap tests next.
Then implement `prepare_native_graph` around ZE-43's candidate using the shared
batch-aware catalog and one charged scratch owner. Build read/incident helpers on
the same authoritative validation and liveness function so lookup, scan, expand,
degree/count and DELETE cannot drift.

The runtime did not expose a verifiable effective model identifier or reasoning
setting, so this review makes no claim about them.
