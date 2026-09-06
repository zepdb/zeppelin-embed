# Changelog

All notable changes to Zeppelin Embed are recorded here. Versions follow
semantic versioning, with the 0.x rule that a new public surface is a minor
release and a compatible correction is a patch release.

## 0.2.0 - 2026-09-06

This release turns Zeppelin Embed into a record store. A namespace can now
declare typed attributes, and documents can be written, read back by id,
enumerated, counted and filtered without a vector ever being involved. The
whole surface is available from Rust, C, Swift, Python and Node.

### Added

- Namespaces. `ze_namespace_open` opens a namespace against a declared
  schema and `ze_namespace_list` enumerates the namespaces in a store.
  A namespace declares its typed attributes and, optionally, a vector
  space; a namespace with no vector space is a record-only namespace.
- Typed attribute writes. `ze_upsert` writes documents that carry typed
  attribute values alongside the document id.
- Read by id. `Store::get_documents` and `ze_get` return stored documents
  for a list of ids, with the caller choosing which fields come back.
- Enumeration. `Store::scan_documents` and `ze_scan` walk a store through
  a resumable cursor, and `Store::count_documents` and `ze_count` return a
  count over the same filter language.
- Filtered search. `ze_search_filtered` applies a structured filter AST to
  a vector search, so a filter is data rather than a string.
- Three appended error codes: `ZE_ERR_SCAN_STALE` (32),
  `ZE_ERR_SCHEMA_MISMATCH` (33) and `ZE_ERR_NO_VECTOR_SPACE` (34).
- The record store in every binding: `upsert`, `get`, `scan`, `count` and
  filtered search on the Swift actor, the Python `Store` and the Node
  `Store`, with a worked sample for each language.

### Changed

- The macOS XCFramework is now one universal slice carrying both `arm64`
  and `x86_64`, in place of the single Apple silicon slice shipped in
  0.1.0. The Swift package therefore builds on Intel Macs as well.
- Error precedence in the C ABI. An invalid handle is now reported before
  a malformed request. A caller that distinguished the two by the order in
  which they were raised will see the other code first.
- `ze_ingest`, `ze_search`, and the vector and hybrid modes of `ze_query`
  now fail with `ZE_ERR_NO_VECTOR_SPACE` on a record-only namespace
  instead of operating on a hidden sentinel vector. Lexical `ze_query`
  is unaffected.
- Model placement qualification is explicit rather than inferred.

### Fixed

- Vector operations on a record-only namespace no longer overwrite the
  hidden sentinel vector or rank a query against a store of identical
  sentinels.

### Notes for upgrading

Nothing was removed from the C ABI, and every 0.1.0 struct keeps its
frozen size and field offsets. Recompiling against the 0.2.0 header is
enough. Review the two error-behaviour changes above if your code branches
on specific `ze_error_code` values.

## 0.1.0 - 2026-09-05

First public release. In-process vector, lexical and hybrid search for
macOS, with the frozen C ABI and Swift, Python and Node bindings.
