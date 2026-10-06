# v0.5.0 release fixture

Built by scripts/fixtures/build-release-fixtures.sh using the public Rust API
at tag v0.5.0, commit 13ca340138dc4815c0f288e6c003b6b8359cf93c. ZE_TEST_SEED=0; fixed ids, vectors and timestamps;
no randomized inputs (the seeded_rng convention therefore needs no RNG).

Documents 1, 2, 3 were sealed; 3 was then deleted; 4 remains in the unsealed
WAL tail. The seeding process leaks the durable writer and exits without close.
Every live row carries text, vector, timestamp, rank=id and opaque metadata.
expected.json records queries and generation from this release's own reader.

Reproducibility: no randomized inputs. For v0.4.2/v0.5.0, manifest, segment
and WAL are deterministic by construction; a second release-tag rebuild matched
every data file and expected.json byte-for-byte. Empty lock files contain no data.

No data files are known to be non-byte-stable with these fixed inputs.
This is a plain store, without namespaces or op 9.
