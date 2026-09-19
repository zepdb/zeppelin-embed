# ZE-43 overflow and record payload review proposal

Engineering proposal, not yet tag allocation. Preserve block tags 1..9; ZE-38 has
reviewed tag10 CommitParticipant. Reserve tag11 PayloadChunk and tag12
OperationProvenance for this component, subject to independent review. Do not
reuse CanonicalImage for key bytes or treat an extent descriptor as a raw stream.

Use one nonrecursive extent descriptor codec for a bounded logical byte stream.
Its enclosing block is ExtentList8, except a tree Key::Overflow root is always
OverflowKey7 as frozen by ZE-42. Header is 32 bytes: magic ZGEX at0, version:u16=1
at4, logical-role BlockKind:u16 at6, total_length:u64 at8, count:u32 at16,
chunk_bytes:u32=65536 at20, reserved:u64=0 at24. Then exactly count PhysicalRefs
(32 bytes each). Every reference names PayloadChunk11/v1; every nonfinal chunk
has exactly65536 payload bytes; final has exactly remaining positive bytes.
Count must equal ceil(total/65536), no recursion, overlaps or invented sentinels.
At most8MiB total logical bytes, hence128 chunks and4128 descriptor bytes. Empty
logical data uses its direct enclosing block, not an empty extent list. Overflow
key logical role must be OverflowKey7 and must include its9-byte kind/namespace
prefix; all required chunk refs resolve under the same admitted store/generation.
References may share immutable physical chunks where their exact bytes belong in
the logical stream, but a duplicated chunk ref is not automatically corruption:
repeated logical bytes are legal. Offsets/lengths and exact reconstructed total
remain checked. The whole logical key must be valid exact UTF-8 after the prefix;
validation handles a scalar split over a chunk boundary without copying all bytes.

A PayloadRef is typed expected logical role + total_length:u64 + PhysicalRef.
Direct roots use their expected role (e.g. CanonicalImage4, StoredText5,
StoredVector6, OperationProvenance12); indirect roots use ExtentList8 with the
same required role in its header. Direct/indirect encoding does not change
logical bytes. A bounded read-at/Read adapter resolves one chunk at a time,
retains the source lease, polls/control-charges every at-most64KiB byte span,
and allocates nothing. It supplies identity's exact canonical stream comparator;
metadata/hash equality never replaces actual bytes. Storage reads validate every
referenced block before use; a complete verifier touches every chunk before
accepting the logical stream, including same-length wrong-kind substitutions.

Overflow-key production always emits a descriptor root using this codec. Inline
keys <=512 bytes remain direct page keys. Valid larger inline keys read from
existing page-v1 data are normalized to descriptor form while preparing COW pages
or promoting separators; they are not rejected by the emission threshold. Compare
kind byte and namespace numerically, then stream exact remaining key bytes in
bounded chunks. No concatenated8MiB key copy is created. Unchanged overflow refs
are shared by parent separators and untouched pages.

OperationProvenance12 holds complete existing ZGOPv1 bytes (or a role12 extent
list). The record/fence owner validates the full identity-owned fields and keeps
original request/preconditions/incarnation/deletion mode/generation. Live payload
refs are optional explicitly; deleting live contents retains provenance and key
fences but permits dead canonical/text/vector streams to leave the current root.
ZE-38 remains owner of its WAL frame/proof roles; no competing mark/reclaim format
is introduced here. Record property/value details are a separate follow-up to
this extent codec review; these tags only provide bounded lossless carriage.

Directed tests: direct versus relocated/extent canonical equality; at-cap8MiB
key with long common prefix and unequal final byte; scalar UTF-8 across chunk;
wrong-kind/store/ref, missing middle chunk, bad count/final length, recursive
extent attempt, overflow/wrapped length and cancellation at each read boundary.
Test >=4MiB logical stream spans separately framed <=4MiB objects. Golden bytes
pin descriptor offsets and both new tags. Same-seed fault/clean controls and
exactly restored mutants; no broad qualification or whole-store claim.


Engineering review completed by ZE-38 owner: /tmp/ze43-payload-review/review.md.
Approved tags11/12 and32-byte descriptor geometry; implementation retains every
listed limitation and adds exact duplicate-chunk positive control. "No overlaps"
means malformed/partial physical or logical extents, not repeated exact refs.
Artifacts1..9 remain unchanged; tag10 belongs to concurrent ZE-38.
