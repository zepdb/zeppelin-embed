# ZE-43: immutable directories, native records and permanent fences

This is the component implementation and evidence for ZE-43. The candidate
adds native directory preparation and checked immutable readers; it does not
implement the writes coordinator, GraphStore read admission, adjacency, leases,
crash recovery or reclamation. Those remain with ZE-39/40/44/45/46. Nonessential
workspace, broad adversarial, coverage, size and workload campaigns are deferred
through ZE-118 in E12 under the owner's explicit instruction. None is reported
as passing here.

The worktree is `codex/ze-43-immutable-directories`, based on main `234dd4f`
plus the reviewed ZE-121 oracle prerequisite, source `6fa8d5f`, cherry-picked
locally as `81e6e95`. ZE-121's files are not part of the ZE-43 candidate diff.
All inherited unrelated changes are excluded. Root reviews and cherry-picks
the individual candidate before closing the ticket.

## Implemented boundary

The 16 KiB immutable B+tree supports all eight declared comparator domains,
full u128 IDs and u64 symbols, bounded path updates, splits, deletion/root
collapse, ancestor-stack cursors, exact root descriptors and whole-directory
verification. Overflow keys stream through nonrecursive extents, including the
8 MiB admitted logical cap. No whole-directory materialization is required.

Native records retain the original lossless canonical image, complete installing
provenance, sorted symbolized labels and property indexes. Validation correlates
all native fields with canonical bytes, exact catalog names, full identities,
revisions and actual containing generations. Canonical scalar/list/text/vector
streams preserve original bits and typed-empty distinctions. Physical relocation
does not change logical byte comparison. Derived record streams may be larger
than the original canonical input while retaining the original input cap.

Node tombstones preserve full deletion provenance with no dead canonical image.
The permanent key fence separately retains live/dead state, incarnation, revision,
original changed generation and exact request/precondition/delete-mode history.
An exact borrowed key probe avoids copying a long key during lookup. Label/type
membership and object inventory are updated through the same COW primitives.
Inventory descriptors retain exact identity, creation and reclamation state;
membership is not liveness or deletion authority.

`prepare_directories` consumes an authentic `StagedBatch`/`NormalizedDelta` and
checks its exact base token, matching catalog token and the actual same writer
memory owner before appending. It prepares node/relationship records, permanent
fences, label/type membership and candidate roots privately. OUT/IN roots remain
with ZE-44. A failed preparation returns no candidate and retains the allocated
private packs for the caller's abort handling.

`StorageMemory` is a nested participant under the real `WriteMemory` and Store
`GraphResources`, enforcing a combined 32 MiB preparation cap under the existing
64 MiB writer and 256 MiB shared limits. All buffers, retained capacities,
descriptors, index arrays and cursors are charged before allocation and reconcile
actual capacity. The 256 KiB fixed workspace accounts for bounded stack scratch.
Arbitrary external source/callback heap backing remains its actual owner's
responsibility; it cannot be credited to this fixed reservation.

Private artifacts are append-only charged owners. Complete append-time framing
and hash validation creates a private `VerifiedEntry`; complete exact-reference
matching then permits immutable reuse without hashing the same payload again.
Failure refuses subsequent access, and sealing still runs the complete decoder.
Finalized bytes cannot escape from a partial container. `OwnedArtifact` performs
controlled physical reads of at most 64 KiB, exact EOF and whole-file admission
before immutable cached resolution. Interrupted reads poll between every retry.
The controlled framing codec has exact byte parity with the existing wrappers.

## Durable layouts and integration contracts

Existing artifact/page wire geometry and tags remain fixed. Native extent tags
11/12 are distinct from the separately owned adjacency tags 13/14. `PayloadRef`
is 48 bytes and retains logical length, exact role and direct/extent reference.
Live native nodes have the 40-byte header, sorted 8-byte labels and 24-byte
property index rows, followed by required 48-byte canonical and provenance
references. Relationship headers are 80 bytes and retain full u128 endpoints.

Node tombstones are exactly 88 bytes: the node header's flag bit 0 is set,
label count and property bytes are zero, and the only following field is the
required 48-byte deletion provenance reference. Unknown bits, wrong kind,
non-deletion provenance, mismatched identity/revision and future children reject.

The fence value is exactly 144 bytes: version at 0 (u16), entity kind at 2,
state at 3, reserved 4..8, incarnation at 8 (u128), revision at 24 (u64), original
generation at 32 (u64), provenance at 40..88, canonical-presence byte at 88,
reserved 89..96, and canonical reference at 96..144 or all zero when absent.
The key supplies exact kind/numeric namespace/key-byte ordering. The inventory
value reuses the existing exact 88-byte WAL descriptor/state layout.

The actual containing leaf generation is retained in `DirectoryEntry`, which
also requires the exact root context. When COW would widen a containing
generation, typed validators check every old value on the touched leaf under
its original context, and every immediate child of a copied branch under the
old parent generation/level/bounds. This prevents malformed future references
from becoming valid merely because an unrelated neighbor changes. It does not
perform a recursive sweep of untouched subtrees.

Production callers use the checked mutation seam with the appropriate native,
fence, membership or inventory validator. The raw primitive APIs intentionally
accept opaque fixture values and are not substitutes for semantic admission.
Standalone `Objects` fixtures are not storage/query quota proof. Public readers
must hide node tombstones and require both endpoints live for relationship
lookup/scan/degree/count/expand. DETACH does not enumerate incident relationships;
raw relationship/type rows remain until later sweeping, and the tombstone cannot
be removed until every stored incident relationship is gone (ZE-109).

`PreparedObjects::identity_source` supplies identity only. A burned nonce/serial
does not imply that storage acquired a path or its cleanup obligation. Writes
must retain any path created before the pack exists. `abort_inventory` enumerates
allocated packs, including failed packs. This exact handoff is now recorded by
root in canonical `docs/graph/plans/writes.md` under "ZE-43 private artifact
identity handoff" and ZE-39's ticket description. That plan update is separate
from this older worktree and must remain when integrating.

Finalized rooted inventory is combined with explicit protected prepared
descriptors until a later inventory fold. Newly created inventory pages and
the prepared-inventory artifact remain protected by their enclosing prepared
inventory/WAL/checkpoint descriptors. The prepared inventory's checksum belongs
in its enclosing descriptor, avoiding a self-checksum fixed point. ZE-39/40/46
own exclusive-create, fsync, publication, checkpoint cutoffs and protected-union
cleanup; these primitives grant none of that authority.

## Verification and measured results

Host: Apple M3 Max, 137,438,953,472 physical memory bytes (128 GiB), native arm64,
macOS 27.0 build 26A5388g. Rust 1.93.0; cargo-nextest 0.9.145. The committed
default nextest profile runs four isolated test processes and one libtest test
per process, retries zero. Concurrent work/builds may run on the host, so times
below are supporting observations, not performance gates or standalone
benchmarks. All datasets are named synthetic fixtures in the committed tests.

| Check | Final result and raw evidence |
| --- | --- |
| Affected storage suite, allocation-audit + test-support | 58/58 pass, `raw/storage-final-after-reuse.log` |
| Actual PG8 runner + independent history probe (seeds 0/41) | 2/2 pass, `raw/pg8-final-private-backing.log` |
| Strict core/storage-test Clippy | Pass, `raw/storage-lint-after-reuse.log` |
| Strict actual adversarial-test Clippy | Pass, `raw/pg8-final-private-lint.log` |
| Changed-source formatting | Pass, `raw/owned-format-check.log` |
| Allocator ownership and each observed allocation failure | 7 attributed allocations, 7 injected refusals, zero unattributed bytes, 831,032-byte participant peak, `raw/allocation-final-numbers.log` |
| Chunk-spanning 2,048 labels + 2,048 properties | 71,705 canonical bytes, 65,680 native bytes, 879,696-byte participant peak, 54,075,056 work units under the original 200M allowance, `raw/green-bounded-admitted-reads.log` |
| Repeated private-reference reuse | Ten reads: zero allocations, exact full-reference substitutions reject; `raw/green-bounded-admitted-reads.log` |

The focused tests cover numeric IDs above u64, all comparator domains, leaf and
multilevel split/reopen/deletion, old roots, shared unchanged records, complete
canonical/provenance validation, >64 KiB fields, full 8 MiB overflow keys,
capacity boundaries, lower-owner refusals, cancellation, private failed-state
inventory and actual malformed/truncated/trailing/wrong-store physical files.
The staged participant trace changes labels/properties, DETACHes, deletes and
recreates while retaining permanent fences. It writes finalized packs to real
temporary files, drops original packs, then freshly admits the files and checks
genuine earlier-generation roots. Same-generation saved-root evidence in the
earlier primitive test is identified separately, not substituted for that trace.

Final commands (all run from this worktree):

```sh
cargo nextest run -p zeppelin-embed --features allocation-audit,test-support --test graph_artifact --test graph_directories --test graph_storage_prepare --test graph_storage_failures
cargo nextest run -p zeppelin-embed-workspace-tests --test adversarial_tests -E 'test(property_graph_directories_probe_checks_native_roots_and_independent_history) | test(one_runner_episode_reaches_required_native_directory_contracts)'
cargo clippy -p zeppelin-embed --features allocation-audit,test-support --lib --test graph_artifact --test graph_directories --test graph_storage_prepare --test graph_storage_failures --no-deps -- -D warnings
cargo clippy -p zeppelin-embed-workspace-tests --test adversarial_tests --no-deps -- -D warnings
```

## RED, corrections and independent controls

New APIs first had named missing-API REDs, recorded in the corresponding
`raw/red-*-api` logs. Incidental compilation/lint errors are retained as history
but do not count as behavioral RED. Behavioral failures and deliberate controls
are distinguished below; every reported mutant was restored byte-for-byte.

| Named failure/control | Correction and terminal evidence |
| --- | --- |
| Leaf/multilevel splits, root-collapse level, full overflow, inline reopen and final cancellation | Focused failures retained in `raw/red-{leaf-split,collapse-level,overflow,reopen-inline,final-cancel}.log`; final 58-test run passes |
| Uncharged fixed scratch/read workspace | `raw/red-{scratch-accounting,read-resources}.log`; real shared owner reservation and final suite pass |
| Zero original native generation | `raw/red-native-zero-generation.log`; full generation validation and `green-native-zero-generation.log` |
| Interrupted read retries skipped cancellation | `raw/red-owned-interrupted-cancel.log`; poll every physical attempt, `green-owned-interrupted-cancel.log` |
| Entry moved to a different root context | `raw/red-entry-root-context.log`; exact-root-bound entry, `green-entry-root-context.log` |
| Future leaf reference, inventory descriptor and unselected branch child laundered through COW | Original independent report and three REDs in `reviews/ze43-directory-independent*`; corrected independent 11-test GREEN and two restored mutants in `reviews/ze43-cow-independent*` |
| Repeated private hashing and unrelated payload-window tail charging | `raw/red-bounded-admitted-reads.log`, `raw/red-pg8-private-reuse.log`; append-admitted immutable entry and bounded window charging, final storage/PG8 GREEN |
| Codec final-poll omission and offset-only reference match | Three runtime RED100 controls, exact restoration in `raw/controlled-framing-mutants.json` |
| Incomplete property-index bijection and wrong parent generation | Two runtime RED100 controls, exact restoration in `raw/native-mutation-results.json` |
| Omitted old label deletion, permanent dead fence and wrong fence generation bound | Three runtime RED100 controls, exact restoration in `raw/fence-participant-mutation-results.json` |

The large valid index case exposed a real complexity error rather than a
production bound needing expansion. Diagnostic counts before the correction:

| labels + properties each | canonical/native bytes | prepare work | private resolves | bytes repeatedly rehashed | independent verification work |
| --- | --- | --- | --- | --- | --- |
| 128 | 4,505 / 4,240 | 31,921,243 | 4,497 | 19,802,757 | 19,078,449 |
| 256 | 8,985 / 8,336 | 132,236,235 | 9,491 | 82,749,733 | 81,433,649 |
| 512 | 17,945 / 16,528 | 551,246,739 | 19,989 | 346,293,573 | 349,173,553 |

Raw counters are `raw/index-diagnostic-{128,256,512}.log`. A temporary exploratory
10B test allowance still failed the 2,048 case in independent verification after
about 26 seconds (`raw/large-native-index.log`). The original 200M test allowance
is restored; no production limit changed. Native index sorting/correlation is
still bounded O(n log n). The correction removes an O(payload length) private
hash for every small field lookup and stops charging an unrelated physical tail
before clipping to a logical window. Actual copied/compared/UTF-8 bytes still
have <=64 KiB work/cancellation checkpoints. First admission and full seal
validation remain mandatory. `OwnedArtifact` already cached whole-file admission
and was not weakened.

## Independent PG8 model and runner

ZE-121's std-only model is fed separately authored primitive operations and
literal/manual canonical bytes. Expected state does not call the engine codec
or classifier and is not derived from observations. The exact comparator never
sorts or normalizes actual output. The staging-base/catalog fixture copies actual
staged state only to serve later production requests; it is not the oracle.

PG8 executes actual staging, preparation, finalized pack writes, original-pack
drop and fresh `OwnedArtifact` reads for eight generations. Full high-bit IDs,
empty/NUL/Unicode keys, labels/types, full provenance, deletion/recreation,
tombstones and every earlier saved root are compared. The one-namespace fixture
keeps physical symbol order aligned with logical namespace byte order; separate
tree tests prove numeric namespace ordering.

First/middle/final append, source-resolution and cancellation cuts each prove
can-fire with same-seed clean controls, exact failure type, no escaped candidate,
retained abort inventory and capacity release. One-work-unit refusal has its own
clean control. Fault-prefix observations are previously captured snapshots, not
a claim of post-crash recovery. The added required registry key
`property-graph.directories.private-reuse` performs eight repeated exact private
root-reference reads per generation through actual `PreparedObjects`, checks
bounded work and unchanged capacity, and refuses altered full references.

The independent review ran five isolated runtime RED100 controls: altered actual
decoded observation bytes, missing freshly reopened source, corrupt actual file
header before admission, omitted actual runner invocation, and production
omission of a node tombstone. The latter failed at generation 5 with expected
one tombstone versus observed zero. All were restored, followed by two terminal
PG8 tests and strict lint. These supplement the built-in canonical/provenance/
order discrepancy controls. Reports, commands, hardware, hashes and raw logs are
under `reviews/ze43-pg8-independent*`. The explicitly excluded wrong-cwd
compile-only log is not evidence and is not packaged.

## Parser target and review records

`native_graph_records` exercises valid-outer-framed canonical, provenance, node,
node-tombstone, relationship, fence, inventory and payload-reference parsers.
Twenty-four seeds include valid minimal values and raw empty bodies. Its bounded
tooling source is not production admission, quota or cache evidence.

The first run exposed a harness classification bug: one-byte input `[134]`
correctly rejected an empty inventory body, but the harness incorrectly treated
every one-byte input as a valid seed. Classification now requires the real seed
lane (`data[0] < 8`). The original RED is retained in
`raw/fuzz-harness-raw-empty-red.log` and the exact input is
`retained-inputs/raw-empty-inventory`. That input was explicitly replayed GREEN
before any corrected bounded run (`raw/fuzz-raw-empty-replay.log`). The corrected
pre-performance-delta ASan run completed 321,697 executions in 61 seconds
(`raw/fuzz-records-corrected-60s.log`). Final-source run details are recorded in
`final-checks.json` and `raw/fuzz-records-final-60s.log`; only those qualify the
final source. Tooling/ASan RSS is not an engine memory-cap measurement.

```sh
env RUSTUP_TOOLCHAIN=nightly cargo-fuzz run native_graph_records fuzz/artifacts/native_graph_records/crash-a9de501bd96364662356b29faee5662ed5d8a33e --sanitizer address -- -runs=1
cargo fuzz run native_graph_records --sanitizer address -- -max_total_time=60 -max_len=65536 -seed=43
```

Independent review reports preserve their exact scope and source pins:
preparation, original COW finding, corrected COW, ownership, PG8, and the final
immutable-entry/work-accounting delta. The original and corrected findings are
both retained. Review manifests identify prior source precisely; final source
correspondence and raw evidence checksums are in `source-sha256.json` and
`evidence-sha256.json`. Historical logs are not relabeled as final-source runs.
The final review strengthened two test-only assertions: a successfully admitted
entry in the same pack is refused after its second append fails, using fresh
ample resources (`raw/green-prior-entry-after-failed-append.log`, strict lint
`raw/prior-entry-lint.log`); PG8 selects the changed KeyFences root and asserts
its artifact is in the current abort inventory before crediting private reuse.
The previous Nodes selection correctly failed this added assertion on a
relationship-only generation (`raw/red-pg8-private-backing-receipt.log`), then
both final PG8 tests and strict lint passed. These two exact test deltas are
separately frozen under reviews, and no production source changed. The final
58-test suite precedes the same-pack assertion strengthening; the one affected
test was rerun afterward.

No new third-party dependency, silent fallback, unaccounted spill or raised
production bound was introduced.
