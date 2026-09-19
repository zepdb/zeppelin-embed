# ZE-33 canonical graph contents and exact replay evidence

Verified qualification, 2026-09-18–19. The final integration lane passes the
actual full per-crate coverage gate. Focused-file measurements below remain
diagnostic; the complete integration report is recorded separately.

## Source and scope

Worktree: `/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-33`, branch
`codex/ze-33-canonical-contents`, base
`bf13bc11239c8161a389d3ce26a43b97412847b8`. No ZE-42 physical storage source is
included. The only modified existing production files are module/export wiring
in core `property_graph/mod.rs` and independent oracle `lib.rs`; existing
production function bodies, all dependency manifests, and Cargo.lock are
byte-identical to this base. Exact new/modified source SHA256 values are in
[source-hashes.sha256](ZE-33-canonical-contents/raw/source-hashes.sha256).

The approved identity/writes contracts and live ZE-29/ZE-98 decisions govern:

- Canonical node labels form an exact UTF-8 byte set. Properties sort by exact
  name and reject every duplicate, including equal duplicate values. The caller
  supplies and retains descriptor/value backing memory; normalization sorts
  descriptors in place without copying payloads. A failed admission can leave
  private descriptors reordered, but publishes nothing.
- Explicit logical framing distinguishes scalar types, list element types and
  the empty-list sentinel; original I64/F64/F32 little-endian bits survive. Full
  u128 directed endpoints, relationship type, optional text presence and bytes,
  and complete document-tower interpretation are included. Finite vectors are
  retained losslessly and never reduced to search codes.
- `CanonicalEmbedding` borrows the existing strong `EmbeddingTower` document
  type. Query-tower and pairing metadata are excluded, matching the accepted
  stored-document interpretation rule. Every document field is independently
  varied in a public contract test.
- `OperationProvenance` requires explicit version 1 and all ZE-98 logical
  fields: operation, complete kind-scoped optional key, requested/installed
  revision, explicit precondition, full affected incarnation, delete policy and
  original changed generation. Missing/unsupported versions and contradictory
  entity kinds fail. Lifecycle classification supplies a normalized installing
  record; this is not an API to guess IDs/generations for an unclassified create.
- Hash and encoded length only reject mismatches. Equal metadata falls through
  to exact lossless bytes with bounded caller scratch. Physical offsets and
  quantized codes do not enter equality. Packed sources must expose a bounded
  reader ending exactly at their validated canonical descriptor boundary.

`ZGCI` and `ZGOP` version-1 frames are logical comparison images, not persisted
family headers or a durable decoder. Storage owns validated descriptors,
checksums, leases and physical framing. ZE-38/43 own durable mutation/checkpoint
integration, inventory/reclamation state and crash/retry publication; ZE-34 owns
lifecycle conflict classification; ZE-35 owns catalog compatibility; ZE-48 owns
query numeric semantics. The test contrasting numeric `+0 == -0` with distinct
canonical bytes is not a claim that the future query executor has been qualified.

The Kuzu sibling is clean at approved pin
`89f0263cc7a1fd9c396d2c4953747a013556a7f9`. Read-only source revalidation inspected
`NodeTable::validatePkNotExists`, `NodeTable::commit`,
`RelTable::updateRelOffsets`, `CreateRelRead1`, `DeleteFirstNodeGroup` and
`DeleteAllTuples`. Temporary relationship offsets change at commit and reclaimed
pages are a physical property, so neither establishes Zeppelin logical identity
or replay equality. No Kuzu source was copied, compiled, or used as an engine
dependency; no foreign implementation or new third-party package was introduced.

## Environment and commands

Apple M3 Max, Mac15,9, 128 GiB RAM, arm64 macOS 27.0 build 26A5388g. Rust/Cargo
1.93.0, LLVM 21.1.8, cargo-llvm-cov 0.9.0. Full tool output is in
[environment.log](ZE-33-canonical-contents/raw/environment.log). Native macOS
only; no Windows, Linux, Intel, or shipping graph-artifact claim.

Run commands from the worktree root. Normal commands use
`CARGO_TARGET_DIR=target/ze33`; allocator commands use `target/ze33-audit`; fresh
local diagnostic coverage uses `target/ze33-coverage`. No main build/profile
directory was touched. Supporting elapsed times below are not performance gates.

```sh
CARGO_TARGET_DIR=target/ze33 cargo test -p zeppelin-embed --test graph_canonical -- --nocapture
CARGO_TARGET_DIR=target/ze33 cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests property_graph_contents_probe_preserves_exact_logical_equality -- --exact --nocapture
CARGO_TARGET_DIR=target/ze33 cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests adversarial::graph_contents::canonical_oracle_rejects_changed_observations -- --exact --nocapture
CARGO_TARGET_DIR=target/ze33 cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests smoke -- --exact --nocapture
CARGO_TARGET_DIR=target/ze33-audit cargo test -p zeppelin-embed --features allocation-audit --lib property_graph::canonical::allocation_tests -- --nocapture
CARGO_TARGET_DIR=target/ze33 cargo clippy -p zeppelin-embed -p zeppelin-embed-adversarial-oracle -p zeppelin-embed-workspace-tests --all-targets -- -D warnings
cargo deny check
python3 tasks/evidence/ZE-33-canonical-contents/mutation_proof.py
```

`rustfmt --edition 2024 --check` passed over every Rust path in the source-hash
inventory. Strict Clippy passed. `cargo deny check` passed advisories, bans,
licenses and sources; its existing duplicate-package warnings are retained in
the raw log. The earlier ordinary Clippy run found one collapsible `if` in new
test tooling; it was corrected, with no production suppression added.

## Literal RED, GREEN and firing controls

The first named canonical-order and provenance-version tests failed to compile
because their public APIs were absent (`red-canonical-api.log`,
`red-provenance-api.log`), then passed after their implementation. These initial
missing-API failures are distinguished from the behavioral controls below.

Independent read-only review covered the canonical/provenance sources, public
tests, full identity contracts, IEEE/vector fidelity, strong document identity,
provenance and bounds. It found one concrete error allocation: trailing-byte
rejection used a heap-backed custom I/O error. The new named
`malformed_stream_rejection_allocates_zero_bytes` failed with **3 allocations
versus 0**, then passed after using allocation-free `ErrorKind::InvalidData`.
No remaining source finding was reported. Exact RED and terminal GREEN logs are
retained; the final reviewed canonical source SHA is
`7d92f70e5616de43d0396192fdb680a045a907b7e247cecea65436e22143a6a5`.

The reproducible mutation driver applies one fault at a time, requires exit 101
from the named test's assertion (not a compilation failure), restores exact
source bytes in `finally`, verifies SHA256 and runs the full restored suite.

| Deliberate fault | Named failing contract |
| --- | --- |
| Omit property sorting | `canonical_property_order_and_labels_are_byte_exact` |
| Accept duplicate properties | `canonical_property_order_and_labels_are_byte_exact` |
| Collapse signed zero | `scalar_list_tags_and_float_payloads_have_literal_goldens` |
| Drop an original vector mantissa bit | `original_vector_bits_and_every_document_field_determine_contents` |
| Accept equal hash without comparing bytes | `forced_hash_collision_requires_every_byte_and_relocated_sources_compare_equal` |
| Stop charging complete framing | `framing_is_charged_to_the_eight_mib_limit` |
| Ignore cancellation | `stream_failures_cancellation_and_boundaries_fail_loudly` |
| Omit original provenance generation | `replay_provenance_fields_are_versioned_and_never_defaulted` |
| Skip full provenance comparison | `same_revision_replay_requires_contents_and_all_operation_fields` |
| Disable independent oracle disagreement | `adversarial::graph_contents::canonical_oracle_rejects_changed_observations` |

All **10** faults produced their intended named assertion failure. Every source
restored to its starting SHA. The terminal restored suite passed **11 tests**,
and the independent oracle can-fire test passed. Exact commands, restoration
hashes and log paths are in
[mutation-results.json](ZE-33-canonical-contents/raw/mutation-results.json).

The 11 public tests cover literal complete byte goldens, UTF-8/NUL names,
absence versus empty text, all scalar/list tags, typed empties, NaN payloads,
signed zero, full-width directed endpoints, every document identity field,
query-only tower swap, actual equal search quantization codes with unequal
original vectors, every provenance field, same-revision different contents,
forced hash collision, physical relocation, truncated and trailing bytes,
short/interrupted I/O, sink/source failures, invalid scratch, cancellation at
every observed checkpoint, exact 8 MiB admission, and 256 seeded proptest cases
for permutation equivalence and changed-byte rejection. Randomness comes from
`test_support::seeded_rng`; `ZE_TEST_SEED` remains reproducible.

## Bounded work and independent adversarial proof

The complete logical frame, including names, tags, lengths and presence bits,
is capped at **8,388,608 bytes**. Supplied name descriptors are bounded before
sorting, including repeated labels; labels later deduplicate in output. Each
write chunk is at most **65,536 bytes**. Exact comparison accepts 2–65,536 total
caller scratch bytes, splits that scratch between sources, and rejects both
truncation and overlong equal-prefix streams. Constructor admission is bounded;
streaming and comparison poll the caller checkpoint between bounded chunks.

The real feature-gated allocator audit observed **0 allocator calls, 0 allocated
bytes** across admission, fingerprinting, writing and full 8 MiB comparison with
65,536 scratch bytes. Its comparison uses two bounded repeated-byte readers to
measure the admitted streaming length without allocating source images; the
public relocation test separately compares actual canonical image bytes from
different file offsets. The allocator positive control observes one 17-byte
allocation. Malformed trailing-byte rejection also observes zero allocations.
Caller descriptor/backing allocations and storage leases remain owned/accounted
by their later staging/storage components; no uncharged full-image copy is
introduced here.

The std-only independent oracle uses primitive tagged values with integer IEEE
payloads, BTreeSet label semantics and BTreeMap property semantics. It imports no
engine type, hash or canonical codec. Each existing runner episode now exercises
16 seeded primitive cases with reordered/duplicate properties, raw float bits,
typed empty lists, full-width endpoints, vector bits, forced equal hashes and
cancellation. All eight new required coverage keys are mandatory in smoke.

Baseline smoke: **224 episodes, seeds 0–223, zero violations, 94.46 s**.
Final smoke: **224 episodes, seeds 0–223, zero violations, 98.00 s**.
Both complete raw logs are retained. This seam has no VFS mutation or publication,
so no invented crash mode or fake durable recovery proof was added. Byte-source
malformation and seeded arbitrary-byte changes are exercised here; persisted
frame parser fuzzing belongs to its later decoder, which this ticket does not
implement.

## Coverage and acceptance boundary

Fresh diagnostic commands:

```sh
CARGO_TARGET_DIR=target/ze33-coverage cargo llvm-cov --no-report -p zeppelin-embed --test graph_canonical
CARGO_TARGET_DIR=target/ze33-coverage cargo llvm-cov test --no-report -p zeppelin-embed-workspace-tests --test adversarial_tests graph_contents -- --nocapture
CARGO_TARGET_DIR=target/ze33-coverage cargo llvm-cov report --workspace --json --output-path /tmp/ze-33-evidence/focused-coverage.json
```

The local report measures new core canonical **338/358 = 94.4134%** and provenance
**103/103 = 100%**. Its report inventory does not include the independently run
workspace/oracle files, so no local oracle coverage percentage is asserted.
Two preliminary CLI invocations used unsupported cargo-llvm-cov argument forms
and exited before running; the corrected successful commands and their raw logs
are preserved. No coverage exclusion, threshold change, denominator replacement
or unrelated production edit was used.

Final integration on main verified the candidate source inventory byte-for-byte
before running instrumented tests. Of 159 existing core/oracle production files,
only the two declared module/export files changed; all existing production
function bodies are unchanged. All 291 retained baseline raw profiles were
preserved byte-for-byte. The final report retains exactly the prior denominator
for all 151 previously reported files, adding only the two new core files. No
stale-source/profile warning, mismatch or report warning occurred.

The integrated canonical suite passed **11/11**, and the independent oracle
probe/can-fire tests passed **2/2**. The actual LLVM export, using the unchanged
coverage exclusions and 90% threshold, reports:

| Production crate | Covered / executable lines | Line coverage |
| --- | ---: | ---: |
| Core | 62,784 / 69,628 | 90.170621% |
| Cypher | 1,514 / 1,554 | 97.425997% |
| FFI | 3,484 / 3,867 | 90.095681% |
| Text | 3,222 / 3,550 | 90.760563% |
| Independent adversarial oracle | 14,735 / 16,287 | 90.470928% |

The default report excludes the whole tests/ tree, including the oracle library.
A second actual LLVM export disables that default filter; its production result
counts only tests/adversarial-oracle/src, excluding test-driver lines from the
oracle denominator. The unchanged benchmark crate retains ZE-113's independent
4,744/5,258 = 90.2244199% qualification; it was not rerun or folded into core.

Exact commands, terminal logs, complete compressed JSON exports, per-crate
summary, source audit and artifact checksums are committed under
[integration](ZE-33-canonical-contents/integration). The uncompressed local
reports remain under /tmp/ze-33-integration. Integration changed no product
source after the reviewed candidate. All 45 pre-existing user files remain
byte-identical. This satisfies this ticket's per-crate gate; it does not assert
that the future graph lifecycle or shipping feature has been qualified.

No new shipping C graph entry point exists. The baseline static-library gate is
unchanged evidence; enabled graph-artifact size and the final public graph API
remain ZE-107/later qualification. This ticket makes no new shipped-size claim.

All committed raw logs are under [ZE-33-canonical-contents/raw](ZE-33-canonical-contents/raw),
with checksums in `artifact-sha256.txt`. The large local coverage JSON remains at
`/tmp/ze-33-evidence/focused-coverage.json`; the small measured-file summary is
committed. No main profiles, copied guidance, tracker data or unrelated dirt are
part of the candidate.
