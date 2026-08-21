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
