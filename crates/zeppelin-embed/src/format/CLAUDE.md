# Persisted format rules

- Family ids, region-kind ids, scheme ids, field widths, and declaration order
  are append-only contracts. Never reorder, renumber, remove, or reuse a field.
- Writers emit each family's `current_version`; readers accept only the
  registry's declared inclusive range. An incompatible change mints a new
  version and new golden bytes.
- Every integer and float is hand-written little-endian. Persisted paths do not
  use Serde. All checksums are xxh3-64 with a permanently u64-wide field.
- Validate lengths, checked offsets, alignment, and checksums before slicing or
  casting. Unknown segment region kinds are bounded and skipped, not rejected.
- The checked-in `tests/fixtures/format/*.hex` corpus is the byte authority.
  Changing bytes without an intentional version bump is a format regression.
- WAL is append-only family id 11. Its v1 file header is 40 bytes: the shared
  header has zero flags, exact header length 40, and must-be-zero file length;
  WAL-owned bytes 32..40 carry `first_seq:u64`. It has no whole-file checksum
  trailer. Each record header is little-endian `payload_len:u32`, `seq:u64`,
  and `op:u16`; its trailing xxh3-64 covers that complete header plus the
  payload. The four `wal_*_v1.hex` fixtures freeze the complete file bytes.
- Fixed-stride graph node blocks are family id 12 in segment region kind id 7.
  The region begins directly with dense row-id blocks so block `i` is at
  `i * stride`; no leading header may break that address arithmetic. The two
  `graph_node_blocks_*_FROZEN_v1.hex` fixtures are the byte authority.
- Each graph block is `ceil(padded_dims/2)` frozen Bit4 code bytes, the 12-byte
  persisted `Bit4Factors` record, `degree:u8`, `flags:u8`, two zero bytes,
  `max_degree` little-endian u32 neighbour slots, then zero bytes through the
  next 128-byte boundary. Unused slots are `u32::MAX`; flag bits above 1 are
  reserved zero.
- The final 128-byte graph trailer is: magic `ZEGRNB01` at 0, version u16 at 8,
  zero flags u16 at 10, logical dims u32 at 12, padded dims u32 at 16,
  max-degree u8 at 20, zero bytes 21..24, stride u32 at 24, node count u32 at
  28, and xxh3-64 at 32 over all preceding region bytes through trailer byte
  31. Bytes 40..128 are zero. Incompatible interpretation changes mint a new
  family version and new owner-approved goldens.
- Sealed document versions are family id 13 in optional segment region kind id
  12. Each dense row is exactly `doc_id:u128` little-endian followed by
  `revision:u64` little-endian (24 bytes); task-07 segments without the region
  remain readable and report no document identity.
- Manifest family 10 emits v2 for graph-free manifests and v3 only when the
  graph section is present; readers accept v2 and v3. The preserved v1 fixtures are
  explicit rejected inputs. After the existing generation/log-sequence/counts
  prefix and reserved-zero u32, the alias is a presence byte, seven zero bytes,
  and embedding/tokenizer u64 ids. Each existing segment record is followed by
  a presence byte, seven zero bytes, and an embedding u64 id. Each epoch record
  is embedding id, tokenizer id, full document tower, full query tower, and a
  length-prefixed alignment digest. A tower is length-prefixed model id,
  version, and weights digest; dims u32; normalization u16; length-prefixed
  prompt; max tokens u32; runtime u16; compute units u16; then an OS-build
  presence byte, three zero bytes, and optional length-prefixed build string.
- Manifest schema records retain their prior exact encoding. When every segment
  range is `Unstamped`, writers append no range bytes. Otherwise the schema is
  followed by `TSR1`, `segment_count:u32`, then one 24-byte record per segment:
  `tag:u8`, seven reserved-zero bytes, `min_ts:i64`, `max_ts:i64`. Tag 0 is
  `Unstamped` and tag 1 is `Empty`; both require zero bounds. Tag 2 is inclusive
  bounded and requires `min_ts <= max_ts`.
- Manifest v3 retains the exact v2 payload as its prefix, including reserved-zero
  fields and optional `TSR1`. It appends `ZGR3`, `section_len:u32` (body only),
  then `graph_present:u8 = 1`, seven reserved-zero bytes,
  `graph_absorbed_through:u64`, `graph_state_len:u32`, existing ZE-38 CommitState
  bytes, `graph_object_count:u32`, and that many `(artifact_id:u128, length:u64,
  checksum:u64)` rows. A trailing xxh3-64 covers `ZGR3`, length and body; the
  existing enclosing block/file checksums remain. Unknown fields, non-zero
  reserved bytes, absent/unknown presence tags, bad lengths/checksums and
  trailing bytes are refused. After the object rows and before the section
  checksum, graph sections may carry the 16-byte generation cutoff extension:
  `ZGEN`, version `1:u32`, `counted_through:u64` (all integers little-endian).
  It records WAL batches already counted in the manifest generation without
  advancing either replay watermark. Manifest-only increments use version 2:
  `ZGEN`, `2:u32`, `counted_through:u64`, `row_count:u32`, reserved-zero `u32`,
  then `row_count` pairs of `(after_sequence:u64, increment_count:u64)`.
  Counts are positive, rows strictly ordered by sequence, and no row is beyond
  the cutoff. Replay subtracts the covered batches and manifest-only increments,
  then restores each increment after its recorded WAL boundary. Both extensions
  are written only when the graph section exists; graph-free v2 bytes are
  unchanged. Pre-ZGEN v3 readers refuse the 16-byte extension as trailing object
  bytes; the previous v1-cutoff decoder refuses the v2 extension. Unknown ZGEN
  versions are refused. Vector/text roots are absent; required state
  participants must match the live inventory. Artifact ids are non-zero nonces,
  never paths. Graph-free builds refuse v3 at the codec boundary.
- WAL mutation operation id 4 is timestamped-upsert v1. It retains operation
  id 1's header/document/vector encoding and inserts `ts:i64` little-endian
  between `revision:u64` and `dims:u32`. Operation id 1 and all existing WAL,
  segment, graph, and manifest-without-range goldens remain byte-identical.
- Stored metadata is family id 14 in optional segment region kind id 13. Its v1
  payload is `row_count:u32`, reserved-zero `u32`, an explicit zero start
  offset followed by one little-endian `end_offset:u64` per dense row, then the
  concatenated row bytes. The last end offset must equal the payload extent.
- Purge intent is family id 15. Its v1 framed payload is `token_id:u64`,
  `id_count:u32`, reserved-zero `u32`, then exactly `id_count` little-endian
  `doc_id:u128` values. `purge_intent_v1.hex` freezes the complete artifact.
- WAL operation ids 5 and 6 add stored metadata without changing ids 1 or 4.
  Both insert `metadata_len:u32` immediately before `dims:u32`, then encode
  metadata bytes before vector f32 bytes; id 6 also carries id 4's timestamp.
  The two `wal_upsert_*_metadata_record_v1.hex` fixtures freeze these records.
- `stored_metadata_one_v1.hex` freezes the new region payload. Every pre-10-D
  format fixture remains byte-identical; new content uses only additive family,
  region-kind, and WAL-operation ids.
- Stored text is family id 16 in optional segment region kind id 14. Its v1
  payload is `row_count:u32`, reserved-zero `u32`, a `ceil(row_count/8)`
  presence bitmap, one monotonic little-endian `end_offset:u64` per row, then
  concatenated UTF-8 bytes. Absent rows have an unchanged end offset; readers
  validate every present row as UTF-8 before returning any row.
- Native graph WAL is append-only family19/v1, separate from family11. Its file
  and record headers are64 bytes, checksums are default-seed xxh3-64, frame kinds
  Begin1/Mutation2/Inventory3/ReclaimIntent4/ReclaimComplete5/Commit6 are required,
  and the complete envelope limit is16MiB including all framing. Explicit field
  offsets and full-width fixtures are frozen in tasks/evidence/ze-38/schema-review.md
  and tests/fixtures/graph-wal/complete-v1.bin; its matching hex is readable byte
  authority. Object BlockKind10 requires ZGCP role:u16 and version:u16 at4 and6.
  Unsupported roles/versions are never opaque optional participants.
- ZE-380 adds required ZGCP role 7/v1 CapturedBase (role 6 remains RetrievalState).
  Its payload uses the existing SpillBinding at 16..80, absorbed-through u64 at
  80, manifest generation u64 at 88, state length u32 at 96, reserved-zero u32
  at 100, CommitState bytes at 104, then xxh3-64 of the preceding payload.
  ProtectedValue slots remain 112 bytes: tag 5 is FoldAuthority (four u64 at
  8..40, zero thereafter); tag 6 is CapturedBase (RequiredRef at 8..104 and
  sequence u64 at 104). Old tags 3/4 are refused, including pending/completed
  old proofs on open, before resume or deletion; finish those cycles with the
  previous binary before upgrading. The directory remains safely resumable
  by that reader. Frozen 0.6.0 rejects role 7 as Unsupported and tags 5/6 as
  an invalid protected-stream value. Family-18/19 S6c candidate reclamation
  stays until ZE-346 deletes the graph WAL and root selector. Legacy control
  references still protect their checkpoint and WAL identities, independently
  of the self-contained proof's folded CommitState, without reading WAL bytes.
- Unified WAL op 10 `GRAPH_COMMIT_V1` embeds exactly one complete ZE-38
  Begin/Change/Commit envelope without its family-19 file header. Op 11
  `MIXED_BATCH_MEMBER_V1` is `index:u32, count:u32, inner_op:u16, inner payload`,
  with inner op 2, 7 or 10 only; count is at least 2 and index is below count.
  Complete member runs commit together; incomplete runs are withheld and orphan
  continuations fail loudly. Ops 8 and 11 never continue each other's runs.
  `wal_graph_commit_v1.hex` and `wal_mixed_member_v1.hex` freeze complete
  family-11 records; all prior goldens are unchanged. Framing validation does
  not admit graph state or artifacts. Graph replay is owned by ZE-346.
  The mixed-run group helper checks `H + sum(inner_payload_bytes + 32) <= B`
  using the existing typed `WalWriteError::GroupTooLarge`; arithmetic overflow
  is also rejected. H is 40 for a fresh WAL and 0 otherwise, B is the actual
  policy-dependent WAL group cap. Graph-apply enforcement is a later ticket.
