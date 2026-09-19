# ZE-43 record and directory-value engineering proposal

This proposes the still-unallocated record payload geometry. Existing object,
page, canonical ZGCIv1 and provenance ZGOPv1 bytes remain unchanged. Outer block
kinds2/3 already designate node/relationship records. No new record block tag.
The eight directory comparator tags and shared root order remain ZE42/38's.

Use compact typed directory leaf values, with a separately framed native record.
A node/relationship directory value is exactly one PhysicalRef32, requiring
NodeRecord2/RelRecord3 respectively. A label/type membership value is exactly
empty. The key/fence ledger value is a fixed descriptor containing entity kind,
live/deleted state, full incarnation ID, installed/deletion revision, original
changed generation, a required role12 provenance PayloadRef and optional role4
live canonical PayloadRef. A deleted fence has no canonical/text/vector ref;
provenance and the namespace/key tree key remain. The generic raw tree remove
operation is not entity deletion; the records participant retains keyed fences.
OUT/IN range values stay the adjacency owner's required validator, not invented
by this ticket. Inventory is allocation metadata only, never inferred liveness.

A PayloadRef wire field is48 bytes: logical role:u16, version:u16=1,
reserved:u32=0, complete length:u64, PhysicalRef32. Optional fields use a one-byte
presence tag plus seven zero bytes and the fixed48-byte slot; absent slots are
all zero. All refs are validated against the one source/store/view generation;
required descendants must exist before a whole-record verifier succeeds.

Native node payload retains the proposed40-byte header: NodeId:u128,
revision:u64, flags:u32=0, label_count:u32, property_bytes:u64. Relationship
retains proposed80-byte header: RelId:u128, source:u128, target:u128,
RelTypeId:u64, revision:u64, flags:u32=0, reserved:u32=0, property_bytes:u64.
Node headers are followed by ascending nonzero LabelIds:u64. Both are followed
by a sorted property-row index, then required canonical and provenance refs.
Records contain full entity identity/revision; canonical bytes carry all original
names, lossless scalar/list values, optional text and original f32 vector bits.

To avoid a second full property/value/text/vector copy, property-row entries are
an index into the retained exact canonical stream: PropertyKeyId:u64,
canonical_value_offset:u64, canonical_value_length:u64 (24 bytes each), preceded
by count:u32 and reserved:u32. property_bytes is exactly8+24*count. Offsets point
to the existing canonical type tag and complete value encoding, not to a raw
memory address or a rewritten symbol-sorted serialization. Duplicate/zero symbol
IDs, overlapping or out-of-bounds spans reject. The record verifier walks the
identity-owned canonical ZGCIv1 stream with <=64KiB reads, validates every scalar,
typed/untyped empty-list distinction, UTF-8/name ordering and original vector
bits, and binds each property index to its actual canonical value boundaries and
catalog symbol/name mapping. Numeric and list tags retain ZE33 semantics.

The same canonical walk exposes optional stored-text/vector byte spans; callers
receive typed bounded readers retaining the source and full canonical descriptor,
plus offset/length, so present-empty remains distinguishable from absent. Reads
cannot cross the canonical slice bounds; there is no whole-blob allocation.
A vector reader additionally retains actual validated dimension/tower metadata;
retrieval still owns scoring and population membership. This is a logical
payload reader over canonical physical chunks, not mislabeling a CanonicalImage
block as StoredText or StoredVector. Existing tags5/6 remain usable by other
payload owners, but this record representation need not duplicate their bytes.

Provenance is read through the existing role12 stream. Its ZGOPv1 decoder is
bounded and preserves every field; borrowing discontiguous namespace/key text
cannot yield &str automatically. The records component exposes exact checked
field scalars and bounded namespace/key readers; a caller needing identity's
borrowed OperationFields supplies/reserves its real contiguous text backing.
No default/inferred fields or uncharged entire-provenance copy is introduced.
The ledger verifier correlates kind/incarnation/revision/original generation,
live/deleted operation mode and exact key namespace/key bytes with that stream.

Preparation consumes already classified identity fields plus a catalog-name
resolver and canonical/property descriptors; all validation precedes candidate
root handoff. Use the shared GraphResources owner, <=32MiB storage preparation,
actual-capacity scratch and sink inventory charges. No publish/sync/reclaim or
GraphReadView admission seam. Whole-tree verification requires explicit leaf-role
validators; successful raw ordered-tree framing is never record validity.

Directed tests must cover label/type add/remove, full128-bit record relocation,
unchanged record-reference sharing, deleting every live entry while retaining
its keyed fence/provenance, exact canonical equality across different physical
refs, malformed/property-offset repaired-checksum corruptions, wrong role and
missing middle payload, typed-empty/list/numeric bit preservation and bounded
cancellation. PG8 independent primitive oracle and necessary restored mutants
remain required; broader qualification remains ZE118.
