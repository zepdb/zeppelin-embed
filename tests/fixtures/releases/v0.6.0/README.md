# v0.6.0 release fixture

Built by scripts/fixtures/build-release-fixtures.sh using the public Rust API
at tag v0.6.0, commit f39087d7b20016138d9a1946055bc229cdffdd23. ZE_TEST_SEED=0; fixed ids, vectors and timestamps;
no randomized inputs (the seeded_rng convention therefore needs no RNG).

Documents 1, 2, 3 were sealed; 3 was then deleted; 4 remains in the unsealed
WAL tail. The seeding process leaks the durable writer and exits without close.
Every live row carries text, vector, timestamp, rank=id and opaque metadata.
expected.json records queries and generation from this release's own reader.

For this plain store, fixed inputs and deterministic seal ids make manifest,
segment and WAL bytes deterministic by construction; repeated-build byte
identity has not been measured. No data files are known to be non-byte-stable.
Empty lock files contain no data. No namespaces or op 9 are present.

ZE-344 old-reader refusal uses a scratch copy of this v2 fixture, upgraded by
`Store::enable_graph()` to a real v3 manifest and its empty catalog `.zgraph`
object. The committed release data files above remain the original v0.6.0 bytes.
The frozen v0.6.0 reader refuses that upgraded copy in both read-only and
read-write mode at its earlier `NativeGraphDirectory` guard, with ABI code 1
(`ZE_ERR_INVALID_ARGUMENT`; this historical mapping is documented by ZE-340).
ZE-340 adds code 58 for that error in the newer binary only; frozen v0.6.0
cannot return 58. Every filename and complete file byte string in the upgraded
copy must remain identical after each refused Rust/C open.

The manifest version-refusal path (`ZE_ERR_FORMAT_TOO_NEW`, 56) is unreachable
for a real v3 directory because the `.zgraph` guard runs first. The harness
accepts only that version refusal or the specific native-directory refusal,
and checks the corresponding ABI code and absence of a returned handle.
`crates/zeppelin-embed/tests/manifest_v3.rs` and ZE-343's unmodified
`tests/fixtures/format/v2_reader` still cover the old frame reader's v3 version
refusal independently. No header-only v3 fixture is used for Store-open proof.
