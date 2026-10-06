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
