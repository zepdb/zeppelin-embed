# ZE-34: graph key revision and incarnation classification

This ticket implements the pure logical classifier and its coordinator-facing
preparation results. It does not implement a GraphStore mutation, allocator,
adjacency lookup, publication, WAL/checkpoint carriage or crash recovery. The
candidate is based on `a3e093348ba03e42218613bda598daf2c2c061b2` in isolated
worktree `zeppelin-embed-wt-ze-34`, branch `codex/ze-34-key-lifecycle`.

The accepted ZE-98/99/109 decisions govern. In particular, ZE-109 supersedes the
older incident-edge enumeration paragraph in identity.md: DETACH is one node
tombstone with later endpoint-liveness filtering and eventual sweep. No degree
limit or incident-edge enumeration is introduced here.

## Implemented contract and boundary

`classify_key` consumes an immutable admitted NeverUsed/Live/Deleted state and an
explicit create/put/delete/recreate request. A wrong full-width incarnation
rejects before revision ordering, so a larger old-incarnation revision cannot
cross replacement. Lower revisions are stale. An equal revision replays only
when the installing operation, requested revision, explicit precondition,
delete mode and complete lossless canonical contents match. Replay preserves the
original identity, revision and changed generation. Hash equality never proves
content equality. Deleted keys retain logical fences; ordinary create/put does
not resurrect them. Recreate acknowledges the exact deletion revision and uses
a strictly newer revision and fresh private identity. Relationship replacements
preserve both directed endpoints and exact type.

`classify_cypher` accepts the final normalized entity effect once per statement,
after all expressions have been evaluated. Equal final contents are NoOp;
actual replacements/deletions use checked revision advancement. This applies to
unkeyed entities too. No Cypher retry receipt is inferred. Missing/null bound
targets are NoOp, while changed-at-MAX revision rejects. This pure seam neither
performs expression evaluation nor discards evaluation errors.

`validate_distinct_targets` rejects every repeated full key or resolved entity,
including identical requests and aliases. It sorts caller-owned descriptors
in place with a fallible allocation-free heapsort, checking cancellation at
every heap operation. Full names compare in at most 64 KiB chunks. It admits no
more than 16,384 targets or 8 MiB of descriptor/name input. Private scratch may be
reordered on refusal; admitted state is never mutated. `summarize_key_batch`
preserves all per-item decisions, checks cancellation during the bounded scan,
and requests a checked new generation only if entity or other durable state
changes. Its explicit other-durable-work input preserves net-empty
create/delete allocator/fence effects; it is a staging fact, not a proof that a
public call performed no WAL write.

`PendingKeyChange::install` finalizes logical evidence after coordinator-private
ID/generation selection. It retains an existing incarnation, rejects reuse of
the current retired incarnation and requires a newer changed generation. The
real allocator must prove global historical nonreuse/high-water durability.
This is not a caller-selected-ID feature on a public store mutation. A controlled
provenance constructor forwards cancellation through complete framing; parity
and refusal cases pin its fields, length and exact stream against the existing
constructor. Existing canonical.rs/provenance.rs remain complete byte-identical
prefixes; only EOF definitions are added. Other existing core production change
is module/export wiring. The source inventory records the exact hashes.

Storage/staging must supply validated bounded canonical sources and keep their
leases; this ticket does not decode untrusted durable frames. Endpoint existence,
Restrict adjacency checks, DETACH liveness, swept-fence retention, persisted
allocator nonreuse, public no-WAL/no-generation outcomes and durable retry after
crash remain their corresponding later tickets' integration proofs. No size
claim is made for a shipped graph artifact; enabled-artifact qualification is
later work, including ZE-107.

## Environment and sources

Local native aarch64 macOS 27.0 (26A5388g), Mac15,9, 16 logical CPUs, 128 GiB RAM;
rustc/cargo 1.93.0, LLVM 21.1.8, cargo-llvm-cov 0.9.0. See environment.log.
Inputs are owned synthetic graph keys, 200 KiB names, full-u128 IDs, exact IEEE
payloads, bounded descriptor arrays and deterministic seeded operation histories.
No model, network service, credentials or user dataset is involved.

Revalidated local Kuzu HEAD `89f0263cc7a1fd9c396d2c4953747a013556a7f9`, clean
checkout, and inspected the cited `NodeTable::validatePkNotExists/commit`,
`RelTable::updateRelOffsets`, `CreateRelRead1`, `DeleteFirstNodeGroup` and
`DeleteAllTuples` sources. Hashes are in reference-audit.json. These support the
identity-versus-placement distinction; they do not supply Zeppelin retry or
fence semantics. No Kuzu source was copied and no Kuzu runtime test was run.
No dependency or Cargo manifest/lock/deny-policy change is present.

## Literal RED and terminal GREEN

Named absent-API compiler REDs established successive public contracts for
first create, exact create replay, put/delete/recreate, Cypher advancement and
structured duplicate/batch classification. Their raw logs are retained; these
are explicitly compiler/API REDs, not behavioral runtime failures.

Behavioral review findings were independently reproduced:

- `cancellation_interrupts_target_sort_before_its_first_comparison_mutates_input`:
  the old sort reordered both descriptors before the cancellation checkpoint;
  its assertion failed. The fallible sort now stops before that mutation.
- `cancellation_interrupts_long_key_comparison_before_reporting_key_mismatch`:
  a 200 KiB comparison returned InvalidState instead of reaching cancellation;
  chunked exact comparison now returns Cancelled.
- `primitive_lifecycle_oracle_preserves_creation_replay_and_incarnation_fences`:
  the initial inert model returned NoOp instead of the literal expected created
  record. The completed primitive model now passes creation, exact replay and
  same-low/different-high incarnation controls.

The controlled-finalization seam had its own missing-callback-API compiler RED.
The uncancellable-finalization mutant below additionally proves its behavioral
cancellation assertion can fire.

Terminal restored public suite: 17 passed. It includes 44 explicit transition
rows, exact installing retries for all four structured operations, old-ID late
higher revisions, mode/precondition/content conflicts, fixed relationship
shape, original generations, typed refusal cases, MAX revision/generation,
unkeyed Cypher advancement, 128 seeded target-permutation proptest cases,
cancellation at every observed target-sort checkpoint, 200 KiB key/type
comparisons, 16,384-item summary cancellation and constructor parity/refusal.
The PG5 primitive oracle unit test and runner integration/CAN FIRE cases also
pass. Raw terminal logs retain exact test names and commands.

Fourteen isolated intentional faults each produced an intended test assertion
failure (exit 101, `test result: FAILED`, named test and panic), then restored
byte-identical source. The driver stops if a compiler failure substitutes for
an assertion. `mutations.json` records every command, source path, expected test,
exit and matching pre/post SHA-256. `mutations.py` is reproducible from this
worktree and deliberately uses only the ticket's target directory.

| Deliberate fault | Intended guard |
| --- | --- |
| Ignore wrong incarnation | Late old-ID delete cannot cross recreation |
| Replay on equal fingerprints | Same-hash changed canonical bytes conflict |
| Allow ordinary tombstone resurrection | Explicit state-transition matrix |
| Ignore observed deletion revision | Explicit recreate precondition matrix |
| Ignore delete mode on retry | Exact installing delete provenance |
| Treat every Cypher final image as unchanged | Actual final change advances once |
| Suppress revision overflow | Unkeyed MAX revision delete rejects |
| Ignore duplicate resolved entity | Aliases and identical targets reject |
| Restore uncancellable target sort | Cancellation precedes scratch mutation |
| Skip long-name chunk checkpoints | Long key comparison observes cancellation |
| Drop other durable participants | Net-empty durable work still changes |
| Wrap changed generation | Generation MAX rejects changed work |
| Ignore finalization stream callback | Long provenance finalization cancels |
| Make oracle always accept | Corrupted results/retained fields trigger PG5 |

## Independent oracle and seeded runner

PG5 uses only primitive IDs/revisions/generations, explicit origins and
preconditions, and original F64 bit integers; it imports no engine code. Each
runner episode executes 25 fixed history steps, 96 seeded varied requests and
three MAX-revision controls through actual canonical producers and the public
classifier. All same-length canonical hashes are deliberately equal. The
adapter checks returned operation evidence; the oracle computes expected
history independently. Result-kind corruption plus seven retained-field
corruptions are deliberate CAN FIRE controls. Full-u128 identities distinguish
same-low/different-high values. Twelve required coverage keys cover create,
replay, conflict, stale, incarnation, delete, recreate, Cypher, NoOp, overflow,
duplicates and cancellation. The new component is pure, so no fictional VFS
fault site or durable publication proof is added.

Before-change adversarial smoke: 224 episodes, seeds 0–223, zero violations,
107.51 seconds. After-change smoke: 224 episodes, seeds 0–223, zero violations,
117.47 seconds. These elapsed times are supporting observations, not performance
acceptance thresholds.
Both runs use the unchanged exact smoke entry point, with the new PG5 coverage
required in the after run.

## Commands and bounded allocation evidence

All commands run in this ticket's isolated worktree. Targets/profiles are never
shared with main or another worktree.

```sh
CARGO_TARGET_DIR=target/ze34 cargo test -p zeppelin-embed --test graph_key_lifecycle -- --nocapture
CARGO_TARGET_DIR=target/ze34 cargo test -p zeppelin-embed-adversarial-oracle graph_key_lifecycle -- --nocapture
CARGO_TARGET_DIR=target/ze34 cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests lifecycle -- --nocapture
CARGO_TARGET_DIR=target/ze34 cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests smoke -- --exact --nocapture
CARGO_TARGET_DIR=target/ze34-audit cargo test -p zeppelin-embed --features allocation-audit full_target_sort_and_exact_retry_have_zero_allocator_calls --lib -- --nocapture
CARGO_TARGET_DIR=target/ze34 cargo clippy -p zeppelin-embed -p zeppelin-embed-adversarial-oracle -p zeppelin-embed-workspace-tests --all-targets -- -D warnings
CARGO_TARGET_DIR=target/ze34-doc RUSTDOCFLAGS='-D warnings' cargo doc -p zeppelin-embed -p zeppelin-embed-adversarial-oracle --no-deps
cargo fmt --all -- --check
cargo deny check
python3 tasks/evidence/ZE-34-key-lifecycle/mutations.py
```

Strict clippy, rustdoc, format and dependency checks pass. Cargo deny retains
existing SPDX/dependency-duplication warnings and reports advisories/bans/licenses/
sources all OK; no manifest, lockfile or policy changed. The allocation audit
sorts all 16,384 targets and verifies exact replay with a 200,000-byte key:
zero allocator calls, zero attributed/unattributed bytes. Duplicate rejection
also allocates zero times. A deliberate owned 17-byte vector produces exactly
one allocation and 17 unattributed bytes, proving the counter can fire.

## Coverage and review boundary

Fresh focused instrumented tests use `CARGO_LLVM_COV_TARGET_DIR=target/ze34-coverage`:
`cargo llvm-cov test --no-report` with the three public/oracle/runner selectors
above. Reports use `cargo llvm-cov report --json --output-path PATH`. The second
report adds `--no-default-ignore-filename-regex` to include the independent
oracle source under tests/. Raw compressed exports and report logs are retained.
No test/profile source was mutated during an instrumented build; intentional
faults used the separate normal target, followed by exact restoration.

Diagnostic new-file line results are classifier 386/412 (93.689320%), bounded
helper 94/97 (96.907216%), independent oracle 161/161 (100%). These are component
diagnostics, not a replacement denominator or whole-crate acceptance claim.
The focused run does not exercise all pre-existing core code. The owner subsequently deferred broad plan-only qualification until code
completion (ZE-118, E12 Backlog). No threshold, coverage exclusion, dependency
rule or source inventory is waived; full final-source qualification remains
unverified and is not an implementation closure claim.

Root's read-only review captured exact source hashes and found no remaining
concrete product defect after the cancellation fixes. Its report is retained
as source-review.json. All four reviewed production source hashes still match
the final restored source exactly. Source-inventory.json records the complete
final source/test/guide hashes.


## Main integration with isolated focused tests

Source candidate `3463cb8c30b07a85b2e4d207c11d920361b1062c` was cherry-picked
onto main `fc505558ddc4c1c64f40a78a27714058952e73ba`, which includes ZE-35,
ZE-42 and ZE-117. Shared module declarations were appended after the complete
old file contents. All four reviewed production hashes match the candidate;
all four audited prior core/oracle files remain complete byte-identical
prefixes. Existing runner probes/coverage keys were retained and PG5 appended.
All 45 inherited user file hashes are unchanged. The audit and exact commands
are in `ZE-34-key-lifecycle/integration/`.

Using cargo-nextest 0.9.145 and the committed four-process default profile,
with one selected libtest case per isolated process and no retries:

- Public lifecycle 17, adjacent canonical 11 and exact allocation audit 1:
  **29 passed**, 586 skipped, across three binaries.
- Independent primitive oracle and two PG5 direct probes: **3 passed**,
  529 skipped, across two binaries.
- Strict Clippy on core, oracle and workspace tests, with test-support and
  allocation-audit features: exit 0. Workspace formatting check: exit 0.

Raw integration logs are compressed losslessly. Original candidate artifacts
remain byte-identical, including terminal whitespace in raw `.log` files;
source whitespace checks exclude those immutable logs. This integration did
not rerun full lib/workspace/adversarial smoke or coverage. Those broad final
source checks are explicitly deferred to ZE-118 under the owner's code-first
instruction. The earlier candidate smoke and focused coverage results above
remain scoped historical evidence, not full integrated release qualification.
