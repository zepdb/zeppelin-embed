# ZE-42: required native graph artifact codecs

Status: accepted after clean current-source main integration qualification.
Base: `bf13bc11239c8161a389d3ce26a43b97412847b8`, isolated branch
`codex/ze-42-artifact-codecs`. No main or other worktree source/profile changes.

## Scope and reviewed boundary

This implements packed native graph objects, a root **envelope**, framed pages,
framed-block references, fallible nonce allocation and exclusive object creation.
It does not implement GraphStore open, a complete checkpoint, tree routing,
logical records, overflow extent resolution, WAL, publication or reclamation.
ZE-38/40 must validate required logical roots, provenance and high-watermarks
before replay/cleanup. ZE-43 must resolve overflow streams and validate strict
ordering before a `FramedPage` becomes a usable tree. A checksum is never an
identity substitute.

The root agent reviewed these engineering format choices against the accepted
ZE-29 storage/identity/writes plans and the current registry before permanent
IDs were edited. This was a coordinated format review, not an owner scope
change. A subsequent review corrected an early proposed body-only 4 MiB bound:
the accepted implementation caps the **entire encoded artifact at 4 MiB**,
including headers, directory and trailer. The dedicated regression was observed
failing against the weaker bound before the correction.

Source reference: pinned redb `9e8302b17877fd315e726361dfaa5d9cdd9199cc`
`btree_base` checksum, separator and split discipline informed the review.
No redb code or foreign persisted format was copied; Kuzu was not a format donor.
No dependencies, allocator, legacy format bytes or coverage exclusions changed.

## Frozen v1 layout

All integer fields use little endian. Required family 17 is NativeGraphObject;
18 is NativeGraphRoot. Both require version 1. Existing families 1–16 retain
all byte and acceptance semantics. Required unknown kind/version/flags fail.

| Artifact offset | Field |
|---|---|
| 0..32 | Existing ZEPEMBED header: magic, family, version, zero flags, header length 96, exact file length |
| 32..48 | Nonzero StoreInstanceId:u128 |
| 48..64 | Nonzero ArtifactId:u128 |
| 64..72 | GraphGeneration:u64 |
| 72..80 | Total framed body bytes:u64 |
| 80..84 | Block count:u32 |
| 84..88 | Required zero flags:u32 |
| 88..96 | Creation serial:u64 |
| 96..body end | Contiguous framed blocks |
| body end..trailer | Exactly count directory entries, each 24 bytes |
| final 8 bytes | xxh3_64 of every preceding file byte |

Each block header is `(kind:u16, version:u16, zero_flags:u32,
payload_length:u64, payload_checksum:u64)`. Checksum is xxh3_64 of payload only.
Each directory entry is `(offset:u64, framed_length:u32, kind:u16, version:u16,
payload_checksum:u64)`. Entries must appear in exact contiguous body order,
one-to-one, without hidden extents, gaps, overlap, padding or trailing bytes.
Directory checksum equals the matching block's checksum; payload verification
is separate. Offsets and lengths cover the **whole 24-byte-header block**.

Block tags: 1 TreePage, 2 NodeRecord, 3 RelRecord, 4 CanonicalImage, 5 StoredText,
6 StoredVector, 7 OverflowKey, 8 ExtentList, 9 CheckpointPayload. Block framing
version is 1. Inner payload semantics remain with their owning codecs.
RootEnvelope requires exactly one nonempty CheckpointPayload; ordinary objects
reject that tag. Empty ordinary objects are structurally valid.

PhysicalRef is exactly 32 bytes: `(artifact:u128, offset:u64, length:u32,
kind:u16, version:u16)`. It must name a nonzero artifact and bounded nonzero
whole framed block. `resolve_framed_block` requires the owning artifact and an
exact validated directory entry. This does not validate an arbitrary embedded
record extent. Encoders compute full geometry and check caller capacity before
changing output; storage reservation includes every directory/header/trailer byte.

Pages are exactly 16 KiB. Their 64-byte header contains `ZGTP` at 0, version:u16
at 4, tree kind:u16 at 6, page length:u32 at 8, count:u32 at 12, level:u16 at 16,
zero flags:u16 at 18, slot bytes:u32 at 20, generation:u64 at 24, zero reserved
bytes 32..56, checksum:u64 at 56. xxh3_64 covers the whole page with checksum
bytes zeroed. Slots are `(offset:u32,length:u32)` from 64; cells are contiguous
and the unused tail is zero. Tree tags: 1 Nodes, 2 Relationships, 3 KeyFences,
4 Labels, 5 RelationshipTypes, 6 OutRanges, 7 InRanges, 8 ObjectInventory.

Leaf cell: `(encoded_key_length:u32, value_length:u32, key_descriptor, value)`.
Branch cell: `(upper_tag:u8, reserved_zero:[u8;3], encoded_key_length:u32,
child:PhysicalRef, key_descriptor)`. Upper tag 0 is bounded; 1 is infinity with
zero key length. Exactly the final child is infinity. Children require TreePage
v1 references. Key descriptor: `(tag:u8, reserved_zero:[u8;3], logical_length:u64,
data)`. Tag 0 has exact inline bytes; tag 1 has one OverflowKey v1 PhysicalRef,
never bytes masquerading as a logical key. Only KeyFences permits overflow.
Logical length includes the kind/namespace prefix and is bounded by 8 MiB.
This does not promise an 8 MiB application key fits a whole admitted request;
staging must charge canonical contents, key/provenance and all framing together.

Fixed keys compare their declared full u128/u64 fields. KeyFence compares entity
kind, NamespaceId then exact remaining bytes. Page framing does not claim key
ordering for unresolved overflow contents. Extent-list payload semantics and
streamed comparison remain with ZE-43.

Five independent Python `struct` + xxhash golden fixtures freeze object, root,
reference, inline leaf, and overflow/infinity branch bytes. The generator is
retained in the evidence directory; it does not call the Rust encoder.
All 46 pre-existing format golden files match base HEAD byte-for-byte.

## Allocation and failure semantics

OS getentropy supplies nonces on macOS/Linux; other platforms return explicit
Unsupported. Tests inject deterministic full-width nonces, reserved zero,
entropy failure and collisions. Native graph platform release qualification is
separate from legacy Windows behavior. No Windows runtime claim is made.

Allocation preflights the caller buffer before entropy, draws one nonce,
encodes/validates and exclusively creates the candidate. No automatic retry,
clock/counter fallback, overwrite, adoption, sync, publish or delete occurs.
`Collision` carries an explicitly unowned existing nonce. `CreateFailed` carries
a candidate that may have a newly created prefix. `attempted_artifact` grants
neither ownership nor cleanup authority; writes must classify it. A partial-file
fixture retains exactly 37 bytes and proves a later collision preserves them.

## RED, mutation controls and GREEN

Raw logs are retained losslessly as `.log.gz` alongside this document
(log names below omit the `.gz` suffix). Initial REDs were actual named
assertion failures after adding API stubs, except the separately labelled first
missing-method compile probe. The valid API then passed after the minimal change.

| RED log | Named behavior |
|---|---|
| exclusive-create-unsupported-red | Exclusive creation must exist and preserve existing bytes |
| exclusive-decorators-red | Memory/crash/fault decorators must support the exclusive seam |
| format-registry-red | Families 17/18 must be explicitly registered |
| object-roundtrip-red | Graph-only bytes must reopen with full-width IDs |
| entropy-red | Fresh identity must come from the fallible provider |
| tree-frame-red | Inline/overflow/infinite page cells must roundtrip |
| total-object-cap-red | Framing overhead must not escape the 4 MiB total cap |
| partial-create-red | Failed create must report its nonce without collision ownership |
| artifact-probe-red | ScheduledVfs must forward exclusive create through the fault site |

`mutants.json` records exact original SHA-256, intended failing test and source
restoration for each isolated deliberate regression: truncating creation,
ignored header flags, skipped payload checksum, changed golden serial, u64
key truncation, ignored page reserved bytes, disabled oracle comparison, and
late allocation preflight, unchecked framed-reference membership and omitted
runner probe. Each mutant failed its intended assertion and was
restored byte-for-byte. Terminal public/core and seeded probe tests pass. Removing exact directory
membership accepted a corrupt offset 97 reference (expected 96), satisfying the
intentionally incomplete reference-parser RED. Removing the runner probe made
the actual episode coverage assertion fail; restoring it passed (1.41 seconds).

Corruption tables recompute enclosing checksums to reach semantic checks rather
than stopping at an unrelated checksum. Tests cover every truncation prefix,
invalid and valid-wrong family/kind/version, store/artifact identity, length/count
and checked overflow, directory overlap/gap, reserved bytes, mismatched key
length/ref kind and trailing bytes. The actual-VFS incompatible-root test
compares exact before/after path inventory and bytes for root, published object
and orphan. It proves read-only validation, not later writes admission ordering.

## Adversarial and fuzz qualification

The unchanged command before and after production changes:

```
cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests storage_
```

Before: 76 passed, 0 failed, 1 existing ignored, 358 filtered, 106.38 seconds.
After: 76 passed, 0 failed, 1 existing ignored, 360 filtered, 97.62 seconds.

Independent std-only **PG3** checks actual artifact preparation/reopen facts.
Its seven cases are clean, collision, entropy, EIO before create, error after
create, torn write and bit flip. The probe runs a real same-seed clean control
for every case, checks exact fault-fire count (one for faults, zero otherwise),
full-width identity/payload, candidate presence, protected byte inventory,
entropy draws and successful write/byte counters. Four seeds (0, 1, 42, u64::MAX)
pass; every run of the regular seeded runner also hits the required seven
coverage registry entries. No GraphStore durability is claimed.
The oracle self-test alters each of ten observables in every case and requires
rejection. Temporarily replacing the comparator with unconditional success
makes that test fail. Scheduled, simulated-crash and process-crash VFS adapters
forward `create_new` without a truncating fallback.

Fuzz target `native_graph_artifact` exercises both required container decoders,
references, all eight page kinds and reference/cell access after successful
framing. Seed corpus is decoded from the five committed hex goldens. Command:

```
cargo fuzz run native_graph_artifact -- -max_total_time=60 -max_len=32768
```

The preliminary run overlapped source-control work and is supporting only;
final restored-source run completed 773940 executions in 61 seconds, exit 0
(`fuzz-final.log`). Only rustfmt formatting of the fuzz target followed; no
semantic or codec source change followed that run. It is bounded parser smoke,
not an exhaustive 4 MiB input campaign or a platform qualification.

## Coverage and final checks

Candidate handoff: focused storage source coverage is 777/844 = 92.0616%
(allocation 80/93, artifact 365/394, tree 332/357). Independent PG3 oracle is
35/35 = 100%; probe source is 172/176 = 97.7273%. These are file subtotals,
not whole-crate acceptance. The worker candidate remained in progress until
main integration supplied the clean current-source per-crate proof below.
No threshold, exclusions, dependency or inventory reductions occurred.

Exact successful focused commands (isolated worktree profile directory only):

```
cargo llvm-cov -p zeppelin-embed --features test-support \
  --test graph_artifact --test format_golden --no-clean --json \
  --output-path /tmp/ze-42-qualification/coverage-final.json
cargo llvm-cov -p zeppelin-embed -p zeppelin-embed-adversarial-oracle \
  -p zeppelin-embed-workspace-tests --features zeppelin-embed/test-support \
  --test adversarial_tests --no-clean --json \
  --output-path /tmp/ze-42-qualification/coverage-final-workspace.json \
  -- property_graph
```

The initial core run used the same targets without --no-clean. The subsequent
core and probe runs retained those profiles. cargo-llvm-cov's default report
excludes all /tests/ paths, including the oracle source. An explicit LLVM export
of the same merged profiles/binaries retains the complete source inventory;
`llvm-export-command.json` contains the exact argv, and
`focused-full-inventory.json.gz` retains that raw report. No report-only change
was used to claim a whole-crate percentage. Transient CLI mistakes (placing the
test filter before --, and combining --no-report with --no-clean) produced no
usable report; the successful commands above supersede them.

Strict command:

```
cargo clippy -p zeppelin-embed -p zeppelin-embed-adversarial-oracle \
  -p zeppelin-embed-workspace-tests --all-targets \
  --features zeppelin-embed/test-support --no-deps -- -D warnings
cargo fmt --all --check
```

Both pass. Public codec tests: 15 pass. Legacy format golden suite: 9 pass.
Independent source review found no concrete artifact/tree/allocation/VFS issue;
reviewed hashes are retained. Subsequent oracle/runner qualification is recorded
separately above, not attributed to that earlier review.

Host: Apple M3 Max, Mac15,9, native arm64, 137438953472 bytes RAM; macOS 27.0
26A5388g. Rust/cargo 1.93.0, cargo-llvm-cov 0.9.0. Synthetic exact byte fixtures
and deterministic seeded inputs only. No production data, network service,
Windows runtime, sanitizer/Miri or later graph lifecycle claim. Measurement
time: 2026-09-19 UTC. Evidence paths and source hash inventory are retained in
`tasks/evidence/ze-42-native-graph-artifact-codecs/`.

## Main integration qualification

Worker commit `96165a6da3c446ff982eda8f2e1f3cc1a44c6d31` was cherry-picked with both ZE-33 canonical and ZE-42 artifact additions retained in the shared modules, runner, required coverage, tests and core guide. All 46 legacy format goldens and 45 preexisting user files remain byte-identical. New storage code hashes match the reviewed worker snapshot.

The initial retained-profile export cleared 90% numerically but was rejected as acceptance evidence. Adding VFS methods shifted existing source locations while old executable mappings remained in the target directory. The source audit caught the inflated denominator despite LLVM emitting no warning. Diagnostic reports remain labeled and preserved. A fresh `target/ze42-clean-coverage` ran all current workspace tests without importing old profiles, exit 0; the frozen 194-file Rust inventory was verified after completion. The full adversarial matrix passed 428 tests, with zero failures and 11 existing ignores, in 2,438.68 seconds. FFI boundary feature tests then passed. No threshold or source exclusion changed. Exact commands and environment are in `integration/clean-workspace-result.json` and `integration/clean-commands.json`; full raw reports and logs are retained losslessly with SHA-256 inventory.

| Crate | Covered / executable lines | Actual coverage |
|---|---:|---:|
| zeppelin-embed | 63,617 / 70,533 | 90.194661% |
| zeppelin-embed-cypher | 1,514 / 1,554 | 97.425997% |
| zeppelin-embed-ffi | 3,484 / 3,867 | 90.095681% |
| zeppelin-embed-text | 3,245 / 3,550 | 91.408451% |
| zeppelin-embed-adversarial-oracle | 14,770 / 16,322 | 90.491361% |

The source inventory has no removed production files: 156 files are represented in the four production crate reports and 15 in the explicit oracle-source report. New storage contributes exactly three files; changed VFS denominators are 1,479, 418 and 412 lines, matching current source. The rejected export had 1,532, 445 and 511. `integration/fresh-denominator-audit.json` retains the comparison. Reports include all executable source in each crate, including uncovered paths.

Focused integration passed 24 core codec/golden tests, the property-graph probes and the actual runner required-coverage test. Combined smoke: 224 episodes, zero violations, 94.55 seconds. Strict combined core/oracle/workspace-test Clippy and workspace formatting pass. The unchanged Darwin benchmark source retains ZE-113's separate 4,744/5,258-line proof; current-source workspace tests also exercise it.

The main fuzz harness retains raw-input validation and adds repaired enclosing-trailer and 16 KiB page-checksum lanes. Payload/directory block checksums remain independent checks. Mutated framing, slot and key data can reach semantic validators without weakening production checks. Five committed goldens seeded the run: 1,408,076 executions in 61 seconds, seed 8769747, exit 0. Exact command, initial corpus hashes, harness hash, independent review and raw log are retained. This is bounded 32 KiB parser smoke, not exhaustive 4 MiB artifact or platform qualification. Instrumented counters from the changed harness are not a direct percentage comparison with the worker run.

`RootEnvelope` remains an opaque framing participant; writes owns complete checkpoint admission. `FramedPage` validates structural framing; ZE-43 owns resolved overflow ordering. Graph create/recovery, real macOS 14 runtime gates, Windows/Intel and shipping qualification remain with their named later owners.
