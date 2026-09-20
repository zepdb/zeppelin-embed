# ZE-44 native adjacency integration proposal

Base: integrated main `0e0064ddb3b1630e8c18050cb78c30ac7bcfb638`.
This document records the bounded interface review requested before new durable
descriptor code. It does not claim implementation or acceptance. The live
ZE-44 ticket and canonical main storage/parallel-contracts/writes plans govern.

## Owned paths and reused interfaces

New production modules under `storage/adjacency/`: `range.rs`, `prepare.rs`,
`read.rs`, with additive exports from `mod.rs`. Integrate the real
`storage/participant.rs` candidate and existing checked tree mutations. Add a
small checked predecessor seek to the existing `tree/directory.rs` path/cursor,
without a replacement tree algorithm or alternate framing. Reuse the landed
ZE-124 codec/merge unchanged, its typed IDs/ranges and existing outer tags13/14.
The new whole producer must own both directions and the native record roots.

Core tests will be `tests/graph_adjacency_store.rs`, with focused existing
directory regressions only where the shared predecessor seam changes behavior.
An actual producer observer/runner extends `tests/adversarial` and its required
registry; it does not substitute synthetic paired arrays for real OUT/IN roots.
The independent primitive model is the reviewed, integrated ZE-129 module under
`tests/adversarial-oracle/src/graph_adjacency_store.rs`; the actual adapter consumes
that implementation without duplicating its expected-value logic. No new dependency or broad suite.

## Range descriptor

The existing tree key is 40 bytes: full NodeId at0, RelTypeId at16, inclusive
lower RelId at24, compared numerically by the existing OutRanges/InRanges role.
Proposed fixed `RANGE_DESCRIPTOR_BYTES` is328 bytes, which fits existing page
geometry. `INLINE_BYTES=512` is a key/separator emission threshold, not a value
limit. Little endian; every unused byte/reference slot must be zero.

| Offset | Width | Field |
| --- | --- | --- |
| 0 | 2 | Descriptor version1 |
| 2 | 1 | Direction:1 OUT,2 IN, matching containing tree |
| 3 | 1 | Upper tag:0 finite,1 infinity |
| 4 | 1 | Delta count, at most8 |
| 5 | 3 | Reserved zero |
| 8 | 16 | Exclusive upper RelId, zero exactly for infinity |
| 24 | 8 | Base consolidation WAL watermark |
| 32 | 4 | Total pending delta observations, at most2048 |
| 36 | 4 | Base entry count, at most4096 |
| 40 | 32 | Required exact AdjBase PhysicalRef |
| 72 | 256 | Up to8 required exact AdjDelta refs; unused slots zero |

Interpretation verifies all references, exact store/kind/version, complete inner
group/range identity, counts, watermark/run cutoff and actual containing leaf
creation generation before returning a usable range. Every old descriptor on a
copied leaf uses the mandatory typed LeafValidator under its original context.
The base reference is required even for a zero-entry base; a persisted descriptor
with zero base entries and zero runs is invalid. Every persisted run is nonempty;
counts equal the fully validated inner header values. Every new descriptor and
its references are explicitly validated before insertion: `insert_checked`
validates retained old values, not the new supplied value.
Adjacent intervals must not overlap; empty merged intervals are removed, and
gap insertion preserves neighboring finite/infinite bounds without MAX+1 math.
Cross-entry nonoverlap is checked using same-prefix predecessor/successor
routing around every replacement/split/gap insertion, in safe private mutation
order; a leaf-value validator alone cannot establish that invariant.

## Whole preparation and sequence identity

Proposed `prepare_native_graph` privately calls the authentic ZE-43
`prepare_directories` with the same staged batch, catalog, sink, StorageMemory
and controls. It derives directed adjacency changes from the actual old/new
native relationship records. Full topology cannot be supplied independently by
a caller. It groups charged changes by node/type/direction/range, produces at
most one run per touched range, and prepares both directions before returning
one successful native graph candidate. Any error exposes no complete candidate;
the same PreparedObjects owns every private allocation and abort descriptor.

The base WAL cutoff is bound to the borrowed real `wal::CommitState` by checking
store, generation and all eight required physical roots against DirectoryBase;
the existing exact BaseIdentity must also match the batch and retained catalog.
The next sequence uses checked base.sequence+1. Never infer WAL sequence from
GraphGeneration. This is coherent metadata supplied by the sole admission
owner, not a replacement GraphReadView/lease. The actual future coordinator
must retain the same admitted envelope token and complete CommitState together.
The candidate retains the exact expected base sequence alongside BaseIdentity
and all eight root slots for recheck. Empty/no-op deltas retain the base cutoff;
they cannot fabricate a target sequence. A focused fixture deliberately uses
different generation and sequence numbers.

Same-batch endpoint insertion is checked against final candidate node roots,
using ZE-43's merged BatchCatalog for both staged and base symbol assignments.
Every newly inserted relationship requires both final endpoints live.
Property-only relationship edits leave both adjacency root references unchanged
and preserve normalized staging semantics: an existing Cypher relationship may
be edited alongside same-batch DETACH, retaining its changed raw record while
becoming invisible. Structured input performs its own earlier endpoint checks.
Native preparation does not blanket-reject that valid normalized Cypher case.
Relationship deletion after DETACH remains allowed. Node DETACH
emits the existing tombstone and performs no incident enumeration. Ordinary
DELETE uses a bounded base incident probe, excluding explicitly deleted
relationships and retaining the staging contract's live-endpoint semantics.

Ninth-run and2049th-pending limits consolidate the old admitted range before
adding the new run. Incoming changes exceeding one run are spatially divided
inside the one private candidate, never silently split into public commits.
The landed merge handles up to6144 survivors and splits at the actual4097th
RelId. Empty intervals disappear. Per-range scratch remains below4MiB, and all
simultaneous scratch/index/artifact/descriptor backing uses the same32MiB
participant under the64MiB writer and256MiB aggregate owner. Capacity rejection
is explicit; no uncharged fallback or bound increase.

Implementation clarification approved by the integration owner: after fully
validating the bounded old kernel output, one bounded two-way pass may combine
it with sorted incoming changes and emit final bases of at most4096 entries at
the single target WAL sequence. Ninth-run/2049th-pending admission does not
require writing a redundant old-cutoff intermediate base plus another run.
No over-limit intermediate descriptor is admitted; remove the old interval
before adding disjoint replacements and discard the whole candidate on failure.
The proposed sub1MiB range scratch is a hypothesis until complete simultaneous
backing capacities are measured. Reuse ZE43's controlled in-place heap sort with
generic Ord fixed-width tuple keys only; do not pass unbounded string keys.

## Read and proof surface

Read methods resolve actual range trees and merge complete admitted inputs
before exposing their rows. Authoritative relationship records must match the
bound node/type/direction, RelId and neighbor. An endpoint tombstone hides the
edge; a missing endpoint for an extant authoritative relationship is corruption,
not implicit deletion. Both endpoint node states filter
visibility before bounded output capacity is counted. The shared logic serves
relationship lookup/scan, expand, degree/count and bounded plain-delete incident
checks, preserving self and parallel identities. Raw physical observations
remain available to the independent test adapter without claiming public
visibility. ZE-45 retains actual query-view lease/admission ownership.

Required focused proofs: literal complete descriptor bytes/malformed refs,
numeric predecessor routing and stale/corrupt path refusal; actual staged
same-batch endpoints and full OUT/IN/authoritative agreement; untouched roots on
property-only changes; self/parallel edges and sparse groups; real4097-edge,
ninth-run and2049-entry consolidation/splitting; DETACH with no incident work
and every edge-facing liveness filter; same-source old roots/physical reopen;
allocation/control/private-sink failures yielding no whole candidate. The
independent missing-reverse and ignored-delete mutants must affect the real
producer and fail exact comparisons, followed by restored terminal GREEN.

The existing actual PG8 and PG12 runner probes run before product edits and
after the changed producer/failure route is registered. Nonessential broad
qualification stays ZE-118. Real WAL commit/recovery/publication remains with
its existing owner; no component test is labeled that qualification.

## Implementation qualification checkpoints

The compiled source checkpoint `/tmp/ze44-threshold-checkpoint` pins the accepted
producer APIs and source hashes for the separate actual-runner adapter. Production
remains identical to the independent producer review except the accepted read
interval correction: equal lower/upper bounds return an empty query result;
persisted descriptors still require strict nonempty bounds. The native writer
retains the independent sequence and all eight exact RequiredRef slots for the
sole coordinator's later recheck.

The measured 200M mixed work allowance is an explicit test-operation choice,
not a product/default write limit. Real 2049-pending qualification uses a declared
finite 400M allowance and reports actual work. The original 200M typed refusal is
retained. All approved logical and simultaneous 32/64/256MiB bounds are unchanged.
The 4097 history uses bounded actual producer batches with the 200M allowance.
Its test-only sink routes TreePage and other blocks into two existing
PreparedObjects under one StorageMemory; every emitted file remains exact and
its complete abort inventory is retained. Test RAM retains all record packs and
page packs reachable from all eight real roots; every retained earlier snapshot
continues owning its page packs. This is fixture packing/retention, not production
GC/default-pack qualification. External fixture buffers, vectors and replacement
overlap are charged to the real shared GraphResources. Post-finish file admission
and final fresh-file reads are separate operations with explicitly reported work.
No production counter is reset midway.

Core evidence is consolidated in `core-qualification.md`. Independent runtime
oracle/registry qualification remains a required final ZE-44 gate; no whole-ticket
closure or ZE-45 public lease/admission/publication qualification is implied here.
