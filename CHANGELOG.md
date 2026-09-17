# Changelog

All notable changes to Zeppelin Embed are recorded here. Versions follow
semantic versioning, with the 0.x rule that a new public surface is a minor
release and a compatible correction is a patch release.

## 0.4.0 - 2026-09-17

A platform release: Zeppelin Embed runs natively on Windows x64. The engine,
the C ABI and the Node package all gain the platform. No search behaviour
changes and the generated C header is byte-identical to 0.3.0, so an existing
macOS consumer recompiles against this release unchanged.

### Added

- Windows 10/11 x64 support for the Rust core and the C ABI. The port is
  native Win32, not a compatibility shim: owned file mappings with explicit
  unmap, single-writer ownership through an exclusive file handle, the durable
  publication protocol expressed in `CreateFileW`/`FlushFileBuffers`/
  `MoveFileExW` terms, and recovery, maintenance and physical purge qualified
  against retained readers, which is where Windows differs most from POSIX
  because an open mapping refuses the delete rather than deferring it.
  - `ZE_KERNEL` dispatch selects AVX2 on x86-64 and refuses `neon` by name.
    The vectorised arm is compared with the scalar oracle bit-for-bit, with no
    tolerance introduced.
  - The on-disk format is unchanged. Both platforms verify it against the same
    hand-written portable fixture. The macOS-to-Windows hop itself needs two
    attached machines and has not been run, so cross-platform exchange is
    checked one side at a time; the two exchange halves ship `#[ignore]`d so an
    ordinary run reports them as ignored rather than passed.
- `@zepdb/zeppelin-embed` ships Windows x64 binaries: one for Node with
  Node-API 8 or later, one for Electron 44. The addon carries the engine
  inside it and needs no Zeppelin DLL beside it, matching the macOS
  arrangement. It links the Visual C++ runtime, so a consumer machine needs
  the Visual C++ redistributable.
  - The loader selects the binary from `process.platform`, `process.arch` and
    `process.versions.electron`, and refuses an unsupported pair by name. There
    is deliberately no try-each-and-catch fallback, which would turn a
    packaging mistake into a confusing failure somewhere else.
  - New `UnsupportedRuntimeError` (`ERR_ZEPPELIN_UNSUPPORTED_RUNTIME`) for a
    supported platform with no shipped binary for the running runtime. The
    TypeScript declarations export it alongside `UnsupportedPlatformError`.
  - Qualified from a packaged Electron application, not only from a
    development tree: a `.node` binary cannot be loaded from inside
    `app.asar`, so the unpacked layout is checked before the application runs.
- `.gitattributes` normalises the working tree to LF on every platform and
  exempts the byte-exact fixtures entirely, so a Windows clone and a
  macOS clone hold the same bytes.

### Changed

- The npm release now builds on both platforms. `Node package` gained a
  `windows-latest` job, and a new assemble job combines the two prebuilds into
  one tarball and runs `verify:release` against it. `npm pack` silently omits
  `files` entries that are absent, so packing on one machine produced a
  tarball that advertised both platforms and shipped one; npm would install
  that on Windows and fail at `require` time. That gate now runs on every
  change, not only at release.

### Not in this release

- No Windows Python wheel and no Windows C SDK archive. The PyPI wheel and the
  release archives remain macOS arm64. On Windows, C and C++ consumers build
  `zeppelin-embed-ffi` from source.
- The Windows engine and FFI suites do not yet run in CI; `ci.yml` is still
  macOS-only. They were run on the implementer's Windows 11 26100 host with
  MSVC 14.29 and the results are recorded in the `W02`-`W10` commit bodies.

## 0.3.0 - 2026-09-07

A feature release: type-ahead search. A new public surface, so a minor
version.

### Added

- `last_as_prefix`: treat the last typed word of a lexical query as a prefix,
  so a search box that shows results while the user types finds `meeting`
  from `mee`. Off by default; with the flag off every query is byte-identical
  to 0.2.1 (same query, same dispatch, same diagnostics counters). Works for
  lexical-only and hybrid queries. The index does not change and no reindex
  is needed.
  - C ABI: `ZeQueryRequest.lexical_flags`, bit `ZE_QUERY_LAST_AS_PREFIX`.
  - Swift: `QueryOptions(lastAsPrefix:)`, default `false`.
  - Python: `Store.query(last_as_prefix=...)`, default `False`.
  - Node has no text query surface, so nothing changes there.
  - Rules: the prefix is the analysed token that reaches the end of the raw
    input, so a trailing space or a stopword tail means the word is finished
    and the query is the plain exact query. A prefix shorter than three
    analysed bytes is also treated as exact. Because the index stores stems,
    the expansion matches both directions (`meeti` still finds the stem
    `meet`), and the prefix leg's boost is divided by the expansion count so
    a wide prefix cannot outweigh the finished words.
  - Core: `LexicalQuery::TermsWithPrefix { terms, prefix, fields }`.

### Changed

- `ZeQueryRequest.reserved` (offset 76) is renamed `lexical_flags`. The
  struct's size, alignment and every field offset are unchanged, so a 0.2.1
  caller that zeroed the field is binary compatible. C callers that used the
  field name in a designated initializer must rename it. Unknown bits are
  `ZE_ERR_INVALID_ARGUMENT`, as a nonzero `reserved` was.

### Fixed

- The Python binding passes strict mypy 2.x again (the `Python quality`
  workflow was red on 0.2.1). Typing-only changes, same runtime behaviour.

## 0.2.1 - 2026-09-06

A correction release. The 0.2.0 binary is unchanged; only the Swift package
manifest was wrong, and it was wrong in a way that made the package impossible
to depend on.

### Fixed

- The Swift package can be consumed by URL again. 0.2.0's `Package.swift` read
  the XCFramework checksum out of `bindings/swift/binary-checksum.txt`, but
  SwiftPM compiles a remote manifest in a sandbox where `#filePath` is
  `/Package.swift` and the rest of the repository is unreachable. Every
  consumer that depended on this package by URL therefore hit the manifest's
  `fatalError` and lost its entire dependency resolution:

      main/Package.swift:19: Fatal error: bindings/swift/binary-checksum.txt
      must contain a SHA-256 checksum

  The checksum is now a literal in `Package.swift`, marked with a
  `ze:xcframework-checksum` comment, and the file is gone. Both gates that
  verified it -- `scripts/xcframework/build.sh` and the `swift-release`
  workflow -- read the literal instead, so a mismatch still fails a release.
- The checksum pin loop is workable (BL-167). A local build and the macos-14
  runner do not produce byte-identical archives, and the archive that ships is
  CI's, so `scripts/xcframework/build.sh` now reports a local mismatch instead
  of refusing to build. The `swift-release` workflow remains the hard gate and
  prints the value to pin when it disagrees.

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
