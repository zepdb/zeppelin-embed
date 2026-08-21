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
