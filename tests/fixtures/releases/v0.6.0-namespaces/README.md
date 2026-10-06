# v0.6.0 release fixture

Built by scripts/fixtures/build-release-fixtures.sh using the public Rust API
at tag v0.6.0, commit f39087d7b20016138d9a1946055bc229cdffdd23. ZE_TEST_SEED=0; fixed ids, vectors and timestamps;
no randomized inputs (the seeded_rng convention therefore needs no RNG).

Documents 1, 2, 3 were sealed; 3 was then deleted; 4 remains in the unsealed
WAL tail. The seeding process leaks the durable writer and exits without close.
Every live row carries text, vector, timestamp, rank=id and opaque metadata.
expected.json records queries and generation from this release's own reader.

This is additionally a namespace root: a and b are identical participants in
a committed public namespace_batch (op 9 prepared mutation). The batch revises
id 4 to revision 2 with its identical payload; both participant WALs retain op 9.

Reproducibility: no randomized inputs. For v0.4.2/v0.5.0, manifest, segment
and WAL are deterministic by construction; repeated-build identity was not
measured. Empty lock files contain no data.

NOT relocatable today: bug ZE-370. A copied namespace fails read-only open with
"namespace requires its original transaction root". Rewriting that reference
alone also fails committed op 9 replay with TransactionBinding. Both bind to
the original canonical root. The ignored format_compat test preserves these
bytes, asserts refusal and compares hashes of every file before and after.
The main compatibility gate uses the separate plain v0.6.0 fixture.

Non-byte-stable: namespace records, references, op-9 WAL records,
transaction-named accepted/manifest artifacts, prepared segment names and
manifests, because identities depend on root path and transaction PID/time.
