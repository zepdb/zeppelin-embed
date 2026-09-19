# ZE-38 codec/replay schema (engineering review accepted)

Scope: a bounded codec and private replay-validation participant. No file writer,
GraphStore, manifest publication, cleanup, or checkpoint scheduler is introduced.
This proposal is recorded before permanent tag/offset allocation.

## Public seams and resources

- `encode_header` into a caller buffer; `encode_envelope` from typed borrowed
  changes and metadata into a caller buffer. No owned output allocation.
- `Replay::new(bytes, complete_checkpoint_state, resources)` validates the file header. `next_envelope` checks one entire
  envelope and its required artifacts before yielding a borrowed validated batch.
  A failure latches the reader; no invalid batch or individual-frame effect is
  delivered. Previously returned batches belong only to a private recovery view;
  later corruption forbids publication of the whole recovered view.
- Artifact validation is an explicit read-only resolver seam, returning borrowed
  immutable object bytes for full descriptor/reference validation. Missing required
  roots, canonical data, inventories and proof objects are errors. Candidate
  deletion targets are never passed to that resolver as required-live objects.
- All buffers and descriptors remain caller-owned and charged by their owner.
  The codec allocates zero heap memory; its fixed stack/hash state is explicitly
  reported. Encoded input/output spans and actual retained capacity are caller
  reservations, not inferred from visible lengths. Work units are checked and
  cancellation polls occur per frame/descriptor and within 64 KiB byte work.
- Maximum complete envelope is 16 MiB including all framing. Maximum normalized
  Mutation records is 16,384. All other frame/count bounds derive from minimum
  wire widths and the 16 MiB envelope bound, not unchecked allocations. Referenced
  canonical logical images stay at the accepted 8 MiB cap and may use extent lists.

## Outer family and record framing

Propose required family 19 `NativeGraphWal`, v1, distinct from legacy family11;
no legacy op IDs are changed. Shared file header 0..32 is ZEPEMBED/family19/v1,
flags0, header_length64, file_length0. Bytes32..48 StoreInstanceId:u128;
48..56 first_commit_sequence:u64; 56..64 xxh3-64 of bytes0..56 (default seed).
The header must be fully durable before append; missing/truncated headers reject.

Each record has a 64-byte header: 0..4 `ZGWF`; 4..6 record kind:u16;
6..8 version:u16=1; 8..12 payload length:u32; 12..16 frame index:u32;
16..24 commit sequence:u64; 24..40 BatchId:u128(nonzero); 40..48 reserved0;
48..56 reserved0; 56..64 xxh3-64 of bytes0..56. Payload then an8-byte xxh3-64
of header+payload. Separate header checksum makes corrupted lengths a loud error
before classifying a short terminal body. Frame index0 Begin, 1..N Change,
N+1 Commit. Every frame binds identical batch/sequence. Sequence advances once
per complete envelope, without wrap. Tags: Begin1, Mutation2, Inventory3,
ReclaimIntent4, ReclaimComplete5, Commit6. Unknown tags/versions reject.

Begin payload: store:u128, kind:u8(Mutation1/Maintenance2), reserved7,
base_generation:u64, target_generation:u64, change_count:u32,reserved4,
complete_envelope_bytes:u64. Width56. Require target=base+1 without overflow.
Commit repeats target/sequence/count and contains the default-seed xxh3 digest
of the complete encoded Begin and Change frames in order, plus complete state.
No checksum asserts authenticity or exact logical retry equality.

## Shared descriptor and state carriage

`ArtifactDescriptor`: StoreInstanceId:u128, ArtifactId:u128, creation_generation:u64,
creation_serial:u64, complete_file_length:u32, family:u16,version:u16,
whole_file_checksum:u64 (64 bytes). A required block adds the existing exact
32-byte PhysicalRef. Descriptor/ref artifact IDs must agree; required family is
NativeGraphObject17/v1, bounded by the existing complete4MiB artifact cap.
Optional required references use present:u8 + reserved7 + descriptor/ref when
present, with no invented null IDs. Logical payloads >one object use a required
ExtentList reference; inner canonical extent resolution remains storage-owned.

Complete state includes all8 named GraphRoots (node/relationship directories,
key fences, labels, relationship types, OUT/IN ranges, object inventory), required
catalog root, optional vector/text search roots, optional active-reclaim proof
root, node/relationship:u128 and four symbol:u64 high-waters, creation_serial:u64,
and a bounded list of prepared-inventory required references. Generation and
commit sequence are repeated in Commit. Roots are explicit, not opaque root bytes.
A descriptor's generation/serial must not exceed committed state; high-waters
cannot regress relative to the supplied checkpoint/preceding complete state.
Root ownership/type placement will be coordinated with ZE43.

## Typed changes

Mutation: versioned complete OperationProvenance using existing ZGOP v1 logical
fields (decode retains all fields and exact key UTF-8, no inferred defaults),
explicit live/deleted outcome, optional required lossless canonical reference,
and explicit before/after node text+vector membership flags. A live mutation
requires canonical contents; a deletion forbids a live canonical reference and
new membership. Relationships forbid node search membership. Provenance affected
incarnation/kind/revisions/original generation must match normalized outcome and
target generation; decoding enforces supported tags and operation/precondition
shape, while lifecycle classification against the admitted base stays ZE34/37.
A mutation is allowed only in a Mutation envelope. No incident-edge expansion for
DETACH. Exact retry compares retained provenance/content later; digest is no proof.

Inventory: one complete ArtifactDescriptor plus tagged state (prepared/retained/
reclaim-pending/reclaimed) and optional IntentId:u128 for the latter two states.
Inventory listing alone implies neither required-live status nor cleanup authority.
Preparation inventory required reference(s) must be present in Commit and validated
before a batch can be returned. Artifact identity/state correlation is checked.

ReclaimIntent: IntentId:u128, capture_generation:u64, capture_sequence:u64,
creation_serial_fence:u64, required protected-root-set ref+digest, required completed
mark-manifest ref+digest, explicit completed-mark tag, bounded candidate descriptor
list and intended inventory transitions. Proof streams include run descriptors,
counts/checksums under required referenced manifests. Candidates must have serial
<=fence and correlate with pending inventory transitions. Candidate refs are deletion
targets: existence is not required after a committed intent. Storage's mark/protected
reachability proof validator is mandatory before exposing an intent as validated;
framing success alone never grants unlink authority.

ReclaimComplete: IntentId:u128, required original intent/proof reference, exact
completed candidate descriptor subset plus remaining subset/count, explicit durable
unlink+directory-sync state. Validation correlates the subsets to that committed
intent through the read-only proof validator. This records a coordinator's durable
evidence; it does not execute unlink or infer durability from a boolean. Only
Maintenance envelopes admit reclaim frames; incompatible mixed kinds reject.

## Prefix and corruption obligations

Every byte prefix of a valid envelope after a valid header is either a complete
previous-batch prefix or a terminal incomplete append, never a partial returned
batch. Validate every fully present preceding frame and any complete current
header before ignoring a terminal short body. Complete malformed headers/payloads,
checksum failures, impossible lengths/counts, wrong store/batch/sequence/generation,
reordering/splicing and missing committed artifacts are errors even at EOF.
Partial headers must agree with all fixed/expected bytes already available.
A complete declared envelope with a missing Commit is corruption, not an ignored
append; declared total bytes/counts must agree exactly. Tests state the crash-prefix
model; arbitrary damaged media is not promised automatic recovery.

Goldens freeze Mutation and Maintenance with all state/proof fields and high128
bits. Directed controls repair checksums to reach semantic guards. Every valid
prefix, missing/reordered/spliced frames, wrong count/version/hash, oversize and
missing required object get named public tests. Independent primitive oracle plus
narrow seeded fault/control probe, checksum-repaired fuzz leg and restored mutants
supply focused evidence. Broad workspace/coverage/size qualification is ZE118.

## Engineering review accepted (root, 2026-09-19)

Root accepted family19/v1, both64-byte headers,16MiB complete-envelope cap and
record tags1..6. Block10 CommitParticipant is selected. Its prefix is magic
ZGCP at0..4, role:u16 at4..6, version:u16=1 at6..8; required role IDs are
catalog1, prepared-inventory2, protected-roots3, completed-mark4, reclaim-state5,
retrieval-state6. Unknown roles/versions reject. All prior block tags stay fixed.
ZE46 owns inner mark/reachability layouts; no opaque proof is silently accepted.

The eight WalGraphRoots slots are fixed in this order/discriminants: nodes0,
relationships1, key-fences2, labels3, relationship-types4, OUT5, IN6, inventory7.
Each must resolve a TreePage of corresponding TreeKind1..8. ZE43 owns GraphRoots.
Replay receives the complete checkpoint state, including high-waters and serial,
not merely generation/sequence. Required read-only validation hooks have no
permissive default and receive work/cancellation context. Their contracts require
full artifact/extent and semantic proof validation before a batch escapes, but
no returned value grants deletion authority. Tests explicitly reject failed hook
validation, checkpoint high-water regressions, swapped root roles, same-size
wrong descriptor checksums and incomplete extents. Every available expected byte
of a partial header is checked; fully malformed predecessors are never ignored.

The public encode and complete-envelope replay seams above are confirmed by that
review and the assigned ticket. Focused tests use those seams under the TDD skill.

## Resolver allocation/cancellation boundary

Existing artifact::decode returns owned FormatError strings and scans an entire
<=4MiB object without in-work cancellation. ZE38 does not assert otherwise or
duplicate that codec. Its optional descriptor/role helper consumes an already
validated immutable ArtifactFrame, checks exact directory membership with polls,
and borrows bytes through an EOF-only accessor. The required resolver hook owns
actual object/extent admission, caching, allocation accounting and cancellation.
ZE43/46/40 integration must supply controlled validation before using this seam
on uncached objects; a caller pre-poll is not64KiB cancellation evidence.
The WAL's own32-byte reference parser uses closed tags and allocation-free errors.
Root explicitly reviewed and accepted this ownership split on2026-09-19.

## Final field offsets frozen by the independent v1 fixture

All integers are unsigned little-endian except logical values inside existing
canonical/provenance streams. `RequiredRef` is64-byte ArtifactDescriptor followed
by PhysicalRef32 (artifact:u128, offset:u64, length:u32, kind:u16, version:u16).
An optional RequiredRef is8 bytes (presence:u8 + zero7) when absent,104 when
present. Lists use count:u32 + zero4, followed by fixed-width descriptors.

Commit payload starts count:u32 at0, zero4 at4, aggregate digest:u64 at8, state
at16. State fixed prefix is104 bytes: store:u128 at0, generation:u64 at16,
sequence:u64 at24, node high:u128 at32, relationship high:u128 at48, four symbol
highs:u64 at64/72/80/88, physical serial:u64 at96. Eight optional graph roots
follow in fixed order, then required catalog96, optional vector/text/reclaim,
then the prepared-inventory RequiredRef list. No inferred roots or high-waters.

Mutation has live:u8 at0, membership bits:u8 at1 (text-before1, text-after2,
vector-before4, vector-after8; all other bits zero), zero6 at2, complete ZGOP
length:u64 at8, exact supported ZGOPv1 bytes at16, then optional canonical ref.
The entire ZGOP stream is at most8MiB. It retains the existing tags/field widths,
exact key UTF-8 and every expected state/delete mode/revision/generation field.
Cypher deleted rows require expected Entity; new unexposed create-then-delete
rows normalize away while consumed ID high-waters remain advanced.

Inventory is88 bytes: ArtifactDescriptor at0, state:u8 at64 (Prepared1,Retained2,
ReclaimPending3,Reclaimed4), zero7 at65, IntentId:u128 at72 (zero for1/2, nonzero
for3/4). Required-live and inventory descriptor roles remain distinct.

ReclaimIntent fixed prefix is264 bytes before candidates: id:u128 at0,
capture_generation:u64 at16, capture_sequence:u64 at24, serial_fence:u64 at32,
completed-mark-state:u8=1 at40, zero7 at41, protected RequiredRef at48,
protected digest:u64 at144, completed-mark RequiredRef at152, mark digest:u64
at248, candidate count:u32 at256, zero4 at260, then ArtifactDescriptor64 rows.
Rows are strictly increasing ArtifactIds and all serials are at most the fence.
Capture generation/sequence precede the new commit. Owner proof validation must
correlate roots, runs, counts/digests and inventory transitions; bytes alone do
not establish unreachability.

ReclaimComplete starts id:u128 at0, retained original intent RequiredRef at16,
durable state:u8=3 at112 (both unlink and directory-sync evidence), zero7 at113,
completed descriptor list at120, then remaining descriptor list. Each list is
strictly increasing by full ArtifactId; they are disjoint. The required proof
owner validates exact union/subsets against the original committed intent and
actual durable evidence. No boolean or decoded envelope grants cleanup authority.

Checked watermark constructor review was subsequently approved by root. It
scans supported complete historical framing, aggregate digests, generation and
sequence chains, actual committed mutation high-waters, and exact full state
through the byte boundary without loading now-retired objects. A partial or
misaligned historical prefix rejects. Only later envelopes invoke mandatory
object/proof hooks. Offset64 means an explicitly cut header with first sequence
checkpoint.sequence+1; it is not an unchecked skip mode.

The independent Python packer mints one6617-byte fixture containing three
complete envelopes, all six tags, full high128 bits, all eight root positions,
all optional participants and prepared inventories. Its synthetic required
references pin wire carriage only. They do not assert real mark/proof semantics.
