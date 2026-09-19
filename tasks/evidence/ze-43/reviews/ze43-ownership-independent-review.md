# ZE-43 bounded storage ownership review

Disposition: **no concrete code blocker found in the reviewed ownership scope**.
The identity-source/abort-inventory contract needed explicit wording; the owner
confirmed it and supplied the reviewed comment-only clarification below.
This is not final ZE-43, publication, recovery, COW or PG8 acceptance.

Reviewer: `/root/ze119_capacity`, delegated by root. No source/tracker/index
changes, new ticket/agent, broad suite or independent engine-test rerun.

## Source pin and scope

Snapshot `/tmp/ze43-cow-corrected-review`, based on local `81e6e95`.
All 28 hashes in `hashes.json` verified and reverified; manifest SHA-256:
`4c797fda991952f9260fd5a846215db553a428b30fdfb0a893319e1eb968c36e`.

Primary reviewed files and hashes:

| Path under src/property_graph | SHA-256 |
| --- | --- |
| storage/memory.rs | 3fc72c2822d4b66053c6745e3bcdda6439de764e75356259c9816132de134fb1 |
| storage/prepared.rs | 5037de13120da1a2074628e2d850c927f9f9b0f19ad5168c13f3f659af26406b |
| storage/artifact/private.rs | 30e39cec87a8ee8b676207a753dc25206b8279b1c864491d599790fd5b94aaf3 |
| storage/artifact.rs | 9a531c5b032e244bd67e5f759bccad42c77a84a3d4fccb4db3bf48ecd83d8040 |

Supporting inspection was limited to authentic WriteMemory/StagedBatch ownership,
TreeResources workspace/accounting, the participant admission seam, and directed
capacity/private-failure/reopen tests. The complete source/evidence inventory is
`/tmp/ze43-ownership-independent-checks.json`. Neither COW behavior nor PG8 was
re-reviewed. The later records/native.rs getter is outside this review.

The final documentation delta is frozen at
`/tmp/ze43-final-review-delta/crates/zeppelin-embed/src/property_graph/storage/prepared.rs`,
SHA-256 `44ff1889e8e467043eb786228e0627ebeb07cf671c32e102be963d0de967c628`.
Its non-doc source is byte-equivalent after removing doc-comment lines.

## Ownership and limits

StorageMemory owns its control reservation through the actual WriteMemory and
refuses limits above 32 MiB. WriteLimits permits only tightening the existing
64 MiB ceiling; the unchanged GraphResources adapter uses the Store's actual
accounting and rejects configurations above 256 MiB. Reservations and resize
therefore update the nested participant, writer and shared owners, with checked
arithmetic and no independent replacement budget.

StorageBuffer reserves requested capacity before fallible Vec allocation,
reconciles complete actual capacity, then forbids growth beyond the admitted
logical capacity. Failure locals drop the Vec before its reservation; successful
owners declare backing before guards. PreparedObjects' charged descriptor vector
contains each PrivateArtifact, whose byte and entry arrays also have real full
capacity reservations. Rollover keeps all earlier packs charged. Fixed operation
workspace reserves 256 KiB for bounded stack/control scratch, including physical
read scratch; retained cursor/candidate and dynamic descriptor collections have
separate explicit charges. Arbitrary caller/source/callback backing is not covered
by that fixed workspace and must remain charged by its external owner.

The staging seam compares the exact WriteMemory pointer for all three private
batch arenas. TreeResources must carry the exact same StorageMemory pointer.
The participant checks the exact batch/base/catalog token before any append and
retains actual staging owners; numeric generation equality is not substituted
for ownership. The private StagedBatch constructors remain the authority for its
nested buffers. No authentic graph admission is inferred from this pointer check.

## Failure and identity contract

`identity_source` supplies an identity only. It may burn a nonce/serial before
capacity allocation succeeds. Returning that value does not transfer cleanup
ownership of a file created by the callback. Writes retains any such path and
its genuine backing/accounting, even when construction subsequently fails. The
new comments state this explicitly. `abort_inventory` enumerates allocated
private packs only, including failed packs; it is not an all-side-effects log.
Store/generation and within-preparation serial/nonce consistency are checked.
Global exclusive creation and store-local serial allocation remain writes-owned.

A pack is inserted before its first append can mutate it. Append/seal enter failed
state before mutation; failed finalization exposes no artifact, even if earlier
packs already sealed. Aborting releases all real arrays and nested guards.
Finalization validates complete framing and checks control again before granting
borrowed finalized bytes. The candidate itself cannot publish or delete anything.

OwnedArtifact allocates a charged buffer, reads at most 65,536 bytes per call,
polls before every physical attempt (including Interrupted retries), accounts
returned reads/copies, checks exact EOF, validates the complete framed file and
polls once more before constructing an admitted immutable owner. Truncated,
trailing, malformed and wrong-store images release the buffer on error. Later
resolution is an exact-reference bounded binary search over admitted immutable
metadata; it neither allocates nor re-reads the file. This is an artifact framing
capability, not a graph read-view lease.

## Evidence inspected and limits

Owner run `f4597842-0f17-495c-af9d-c899ca657a3f` passes 55 focused tests, including
capacity reconciliation, writer/shared refusal before allocator entry, actual
allocation cleanup, physical read bounds/Interrupted cancellation, private
append/seal/finish failure and true five-generation file reopen. The scoped
strict Clippy log passes. I inspected only the ownership-related assertions and
their passing records; I do not reinterpret this as a new COW review.

The final named allocation test, run
`76e935b7-4e63-46f0-aacf-ab20ec981bf7`, reports 7 actual attributed allocations,
7 injected allocation failures, zero unattributed bytes and an 831,032-byte
participant reservation peak for that fixture. This is measured reservation
accounting, not RSS, a whole-workload bound or a complete public-store proof.
The raw reviewed logs are copied and hashed under
`/tmp/ze43-ownership-reviewed-evidence` and the check record above.

No additional suite is requested by this review. Real coordinated filesystem
creation/cleanup, source retention, publication/leases, WAL protection and
recovery remain with the owning writes/storage integrations. Rooted inventory
and private cleanup inventory are not themselves liveness or deletion authority.
Broad qualification remains ZE-118.
