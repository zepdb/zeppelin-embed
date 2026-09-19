# ZE-124 bounded adjacency codecs and consolidation kernels

Implemented in an isolated worktree from main
`7028a42f2f4898bb7aa729dc935bd51b4033f2ba`. The complete reviewed inner format and
compiled seam are in [design.md](ze-124/design.md). This is independent adjacency
production code; real directories, admitted views and atomic publication remain
ZE-44/45/47. No ZE-43 uncommitted code or new dependency was imported.

## Result and boundaries

Version-one 96-byte headers bind full-u128 node/range identity, typed relationship
type, OUT/IN, tagged infinity, count and native WAL u64 sequence/watermark. Bases
hold at most4096 32-byte entries; deltas hold 40-byte entries including deletion
neighbors. Append admission requires consolidation before run9/entry2049. This
is a per-range/run bound, never a product batch or degree cap: ZE-44 can prepare
spatial partitions or bounded bases under the same atomic publication.

The merge admits at most8 runs/2048 pending entries, validates complete input
before copying output, and compares all128 identity bits. A first nine-cursor
pass checks neighbor consistency across *all* supplied sequences and equal-sequence
action consistency, and calculates exact capacity/splitting. A second pass copies
survivors, then a final control checkpoint gates the only valid result. Newest
sequence wins, deletes suppress, exact equal-sequence duplicates coalesce. Split
boundaries are actual RelIds; empty results have zero partitions and infinity
never requires MAX+1. Errors invalidate private scratch and preserve the caller's
typed control error without allocation or formatting.

Artifact/WAL codecs now recognize append-only tags13/14. ZE-43 owns11/12, which
are absent from this base and must be composed at integration. No unrelated
PayloadRef role changed. The kernel consumes already admitted immutable payloads;
no uncontrolled outer decode is represented as cancellable.

## Verification

Host: Apple M3 Max, Mac15,9,16 logical CPUs,128GiB RAM, macOS27.0 build26A5388g,
aarch64. Stable rustc1.93.0; nextest0.9.145. This is local macOS arm64 evidence.
[environment.json](ze-124/environment.json) contains exact tool output.
All nextest checks use four isolated test processes, default zero retries.

| Check | Observed result |
|---|---|
| Pre-change artifact baseline |15/15 pass |
| Immutable base7028a42 directed runner episode |1/1 pass; reconstructed from git archive, no source edits |
| Final core tests |28/28 pass:12 adjacency public tests,15 existing artifact tests,1 actual allocation audit |
| Final PG12 tests |2/2 pass:four-seed probe plus actual runner episode |
| Independent primitive oracle |1/1 pass |
| Production source mutants |6/6 intended runtime RED, exit100; exact source hashes restored |
| Directed byte-parser fuzz |2,794,685 runs in61s, no crash;307 coverage points,565 features;44 final corpus units/5658B |
| Core scoped Clippy |all targets, test-support/allocation-audit, no-deps, -D warnings: pass |
| Runner/oracle scoped Clippy |no-deps, -D warnings: pass |
| Owned Rust formatting and diff whitespace |pass |

Exact final command arrays, exit codes and elapsed seconds are in
[final-checks.json](ze-124/final-checks.json); corresponding raw logs are adjacent.
Earlier baseline and original runtime RED logs are retained too. Raw tool logs
retain their original trailing whitespace; the source whitespace check excludes
those exact evidence bytes. The initial
missing-module compilation failure also contained a wrong RelTypeId import;
[initial-red.log](ze-124/initial-red.log) is construction history, not runtime
behavior proof.

The actual allocator reports zero allocations/bytes for successful encode/merge,
malformed input, insufficient output and final-control refusal. The sample caller
buffers occupy296 bytes; fixed simultaneously live run/cursor representations
occupy1224 bytes on this host. This excludes ordinary call-stack representation
and does not claim authentic native storage32MiB/writer64MiB/shared256MiB
admission. ZE-44 must charge its real adapter, buffers, retained inputs and stack.
The exhaustive checkpoint test refuses every observed merge and base-encode
callback, including final completion; maximum indivisible reported byte work96B.

For seeds0,1,124,u64::MAX, PG12 records4 exact comparator checks,5 fault triggers
and5 clean controls each. Three cancellation positions (initial/middle/final),
actual cumulative-work refusal and corrupt last-entry reserved bytes all fire on
the real kernel. Actual seeded runner seed0 completed59 operations/0 violations.
The independent std-only BTreeMap oracle has no engine dependency. It detects a
production ignored-delete mutant and a missing reverse entry in its primitive
paired model. The paired model includes self/parallel edges and real codec/merge
calls, but **does not qualify a real OUT/IN participant or authoritative topology**.

## Literal RED and restoration

The original new-tag artifact test failed with `unknown required graph block kind`;
the WAL tag test failed `Err(Unsupported)` before decoder support. Both returned
GREEN after the minimal decoder additions. The WAL check was then moved from an
internal helper test to the public `ReferenceList::Encoded(...).get(...)` seam;
a deliberate tag-refusal mutant proves that final public test fails too.

[mutants/results.json](ze-124/mutants/results.json) records every exact command and
RED100 result: ignored delete, ignored neighbor consistency, narrowed RelId,
skipped final control, premature split, and refused WAL tags.
[mutants/restoration.json](ze-124/mutants/restoration.json) records matching
before/after production hashes. Final checks ran after restoration.

The first narrowed-ID mutant did not compile because its method receiver needed
an explicit type. After that harness correction, the original high-ID fixture
passed the mutant: its heads never forced the wrong cross-run comparison. That
pass was **not evidence**. A separate cross-run255 versus2^64+1 fixture was added,
observed RED100 under narrowing, then GREEN with restored full-width comparison.
Both excluded attempts and raw logs remain in the mutant directory.

## Review and qualification

Storage owner and root reviewed the exact format before freeze. Independent
[review-1](ze-124/independent-review.md) verified all11 frozen hashes and found no
production correctness blocker. It found a missing unit argument in the gated
allocation-test closure; this was corrected and the actual audit passed. The
production adjacency mod/codec/merge bytes match reviewed snapshot1 exactly.
Final [source-manifest.json](ze-124/source-manifest.json) freezes22 source/design/
test/fixture paths; only tests and WAL test placement differ from the reviewed
snapshot, plus the subsequent fault/oracle/fuzz additions.

The initial fuzz invocation mistakenly supplied a corpus path already injected
by the repository wrapper. A second invocation hit the macOS bash empty
array `RUN_OPTIONS[@]: unbound variable` startup issue. Neither ran the parser.
The successful exact command was:

```sh
cargo fuzz run native_graph_adjacency --jobs 1 -- -max_total_time=60 -max_len=164000 -print_final_stats=1
```

No wrapper change was made. Fuzz peakRSS378MiB includes the sanitizer/harness and
is not engine-managed storage-memory evidence or a performance budget result.

All41 inherited files match their setup hashes; [inherited-preserved.json](ze-124/inherited-preserved.json)
records this. The old ZE-55 worktree, main and its inherited changes were not
modified. No push or native-ticket close is performed by this worker.

Full workspace/full adversarial campaigns, whole-crate coverage, release size,
platform and end-to-end qualification remain nonessential-to-this-commit gates
on ZE-118/E12. They were not run or claimed passed. ZE-44 retains real directory
routing/OUT/IN/authoritative records, same-batch endpoints, liveness and atomic
roots; ZE-45/47 retain real leases, file faults/reopen and recovery. Kernel success
or the synthetic paired comparator never increments those acceptance counts.
