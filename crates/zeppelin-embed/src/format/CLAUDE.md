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
- Manifest family 10 emits and accepts v2 only; the preserved v1 fixtures are
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
