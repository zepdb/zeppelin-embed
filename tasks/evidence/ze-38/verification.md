# ZE-38 complete native graph WAL participant

Implementation in `codex/ze-38-graph-wal`, based on
`fc505558ddc4c1c64f40a78a27714058952e73ba`. Root integration and ticket closure
remain separate. Hardware/tool versions are in `environment.json`: Mac15,9,
arm64 macOS27,128GiB RAM, rustc1.93.0 and cargo-nextest0.9.145. Four isolated
nextest processes, captured output, no retries. No production dependency added;
Cargo.lock is unchanged. All78 pre-existing core fixture files are byte-identical
(`preserved-fixtures.json`).

## Delivered boundary

Hand-written family19/v1 file and Begin/Change/Commit framing carries full store,
batch, entity, artifact, root, allocation and operation identities. Mutation and
Maintenance are explicit envelope classes. Replay is private, read-only and
latched on failure. It does not publish a graph, append/sync a file, delete an
object, or substitute an older state after corruption. Every required semantic
resolver method is mandatory; those methods must bind base lifecycle, catalog,
search, inventory, protected-root/mark, candidate and completion facts before a
batch is returned. Scalar framing checks do not establish those owner semantics.

`Replay::at_watermark` validates all still-present historical frames/envelopes
through an exact complete checkpoint boundary, correlates the whole state, and
requires objects only for newer commits. It never requires now-retired historical
objects or trusts a bare offset. A cut header must start at checkpoint.sequence+1.
No writer/checkpoint/publication/recovery coordinator is added (ZE39/40).

Caller storage owns and charges retained input/output/descriptor capacities.
WAL code owns no heap memory, requires a64KiB stack allowance and finite work,
and checks cancellation inside bounded byte/descriptor work and immediately
before returning success. ArtifactFrame admission remains storage-owned: the
existing artifact decoder allocates error strings and scans up to4MiB without
in-work cancellation. The descriptor helper takes an already validated frame;
no pre-poll is misrepresented as bounded artifact cancellation. ZE43/46/40 must
provide controlled admission, extent/proof and complete semantic validation.

## Literal RED to GREEN

Raw command output is retained as `red-*.log.gz` and `green-*.log.gz`. Initial
public seams used explicit Unsupported stubs, so their intended RED was a runtime
failure, not an unresolved-import/build failure. Named observations include:

- `complete_graph_envelope_encodes_full_width_identity_and_state`:
  header Unsupported, then complete full-width encoding.
- `every_prefix_exposes_only_complete_envelopes_and_requires_validation`:
  replay Unsupported, then correct complete/prefix results and failed-hook latch.
- `mutation_preserves_complete_provenance_membership_and_atomic_framing`:
  mutation encoding Unsupported, then exact full fields/membership; separate
  change-reader RED and GREEN establish borrowed post-validation frame iteration.
- `maintenance_keeps_required_proofs_separate_from_missing_deletion_targets`:
  maintenance Unsupported, then required proof resolution with candidate targets
  never mistaken for required-live objects.
- `complete_malformed_change_is_never_hidden_by_incomplete_commit`:
  actual defect accepted a checksum-repaired unsupported provenance version as an
  incomplete tail; complete preceding changes now undergo semantic scalar decode.
- `partial_headers_reject_impossible_kind_length_and_batch_prefixes`:
  actual defect accepted observed kind255 in a short header; known prefix bytes,
  partial lengths and nonzero completed batch identity now constrain continuation.
- `checked_checkpoint_watermark_skips_only_complete_retired_history`:
  checked constructor Unsupported, then exact checkpoint scan and live-only
  post-checkpoint resolver calls. Misaligned/missing boundaries and wrong complete
  checkpoint state reject; no retired-object resolver call occurs.
- `retired_history_still_checks_mutation_ids_against_committed_high_waters`:
  actual defect admitted checksum-repaired node19 with committed high-water0 in
  the historical skip; historical changes now decode against committed bounds.
- `cypher_deleted_rows_require_a_preexisting_entity_expectation`:
  actual defect encoded expected-Absent deletion (`Ok(659)`); deleted Cypher rows
  now require expected Entity. Net-empty creation/deletion may still advance IDs.
- `cancellation_triggered_by_final_validation_cannot_publish_a_batch`:
  actual review defect returned an envelope after the state hook set cancellation;
  final polling now precedes state/offset advance and success. Encoding also has
  a final success checkpoint.
- Independent PG7 oracle deliberately accepted malformed observations at first;
  both negative-observation tests failed. Primitive state/prefix/fault checks then
  made all three oracle tests pass, including their clean controls.

The old artifact registry control also failed when it still expected family19 to
be unknown. Approved append-only registration now explicitly requires17/18/19,
rejects unsupported version2, and rejects unknown family20. This updates the
control to the reviewed new family without altering any older persisted bytes.

## Terminal focused checks

Commands were run from this worktree. Each associated compressed log retains the
actual output; all listed terminal commands exited0.

```
cargo nextest run -p zeppelin-embed --test graph_wal \
  --test graph_artifact --test format_golden --test wal_recovery
```

90 passed,3 existing ignored tests skipped,0 failed. Includes20 new graph WAL
public tests. The three ignored tests are the subprocess helper, real SIGKILL
loop and local throughput evidence in wal_recovery; none was newly ignored.
Tests enumerate every available prefix64..6617 of the independent three-envelope
fixture, and check exact full framing, mutation/maintenance kinds, malformed
complete predecessors, true missing/reordered frames, wrong index/sequence/batch,
repaired checksums/count/digest/high-water mismatches, full descriptor checksum,
swapped tree role, unknown participant role/version, and exact generation/ID bounds.
Capacity tests include framing in the16MiB cap,16,385 mutations, unchanged output
on admission failure and overflow/regression controls. Long Unicode names split
at all three internal positions of a four-byte codepoint pass and cancel during
real encoding/decoding after more than64KiB of admitted work.

```
cargo nextest run -p zeppelin-embed --features allocation-audit --lib \
  -E 'test(wal::allocation_tests)'
```

2 passed: complete replay plus encoding, and checksum-repaired malformed zero
ArtifactId rejection. Both assert exactly0 allocator calls,0 attributed bytes,
and0 unattributed bytes. Fixture construction and caller backing are outside the
audit. No claim about allocations in storage-owned ArtifactFrame admission.

```
cargo nextest run -p zeppelin-embed-adversarial-oracle --test graph_wal
cargo nextest run -p zeppelin-embed-workspace-tests --test adversarial_tests \
  -E 'test(property_graph_wal_probe)'
```

3 oracle tests passed. The directed probe passed seeds0,1,42,u64::MAX, with each
of10 required coverage keys hit exactly once per seed: prefixes, transitions,
and fire/clean pairs for corruption, missing object, cancellation and work limit.
The cancellation callback fires at poll32 during real processing; budget failure
is an observed WorkLimit after nonzero work. Same-seed clean replay must match
all complete primitive states. PG7 is called by the seeded runner and keys are
in its required coverage registry. No broad campaign was run or claimed here.

```
cargo clippy -p zeppelin-embed --features allocation-audit --lib \
  --test graph_wal --test graph_artifact --test format_golden \
  --test wal_recovery --locked -- -D warnings
cargo clippy -p zeppelin-embed-adversarial-oracle --lib --test graph_wal \
  --locked -- -D warnings
cargo clippy -p zeppelin-embed-workspace-tests --test adversarial_tests \
  --locked -- -D warnings
cargo fmt --all --check
```

All passed. Allocation-only unit bodies are additionally compiled by the focused
allocation test invocation. Cargo.lock and dependency manifests are unchanged,
except an additional bin target in the already excluded fuzz workspace.

## Goldens, fuzz, mutants and review

`schema-review.md` records pre-mint root engineering approval and final offsets.
`mint-golden.py` independently packs explicit fields with Python struct/xxhash,
without importing the Rust encoder. The6617-byte golden includes all six frame
tags, full high128 bits, eight graph slots, all optional participants, inventories,
full provenance and reclaim carriage. The test compares both binary/hex, replays
and re-encodes exact bytes. Synthetic referenced objects pin carriage only; they
are not actual mark/reachability proofs or cleanup evidence.

The missing-middle extent control uses a fixture resolver with an actual framed
ExtentList and a real canonical stream split into three chunks. It observes one
missing-middle lookup and rejects before the final state hook; the paired clean
case succeeds. The fixture does not implement or claim ZE43 chunk-artifact
admission or ZE46 mark proofs.

```
python3 tasks/evidence/ze-38/mint-golden.py --fuzz-corpus /tmp/ze-38-fuzz-corpus
cargo fuzz run native_graph_wal /tmp/ze-38-fuzz-corpus -- \
  -max_total_time=60 -max_len=262144
```

Seed3027875054;521,535 executions in61 seconds; no failure. Raw and checksum-
repaired legs both run on each accepted-sized input. Seeds were the6617-byte
three-envelope golden and a198704-byte single-envelope split-UTF8 provenance
image. Final libFuzzer counters: cov1232, ft5137, corpus533/3261KiB, reported
RSS530MiB. These are tool counters, not production line coverage or exhaustive
proof. The split seed can be reproduced using the final snippet in the packer.

Seven isolated mutants were killed by their exact named runtime tests, with
original SHA256 restored after each (see `mutants.json` and `mutant-*.log.gz`):
aggregate digest, commit count, tree role, required canonical extent hook,
completed/remaining overlap, node high-water monotonicity, final cancellation.
The portable harness is `run-mutants.py`. Final focused checks ran after exact
restoration. No mutation remains in product source.

Root independently reviewed framing/watermark, required hooks/descriptors,
provenance/mutation and maintenance ordering/fence/subset checks. The final
cancellation issue was reproduced and corrected; staging separately identified
the Cypher deleted-row expectation, also reproduced and corrected. Frozen review
hashes and the changes from the first snapshot are retained in this directory.
Final candidate review/integration results are recorded separately by root.

## Deferred qualification

The explicit owner scheduling instruction defers broad workspace suites,
full adversarial matrices, per-crate coverage, size/footprint and other large
plan-only campaigns to ZE-118/E12 until code completion. This candidate does not
claim those gates passed and does not replace their final release obligation.
Focused tests, real fault/control observations, allocation checks and mutants
above are implementation evidence only. Windows, whole GraphStore reopen,
publication/crash recovery, real reclaim marks, unlink durability and retained
reader lifecycle remain their owning tickets' work.
