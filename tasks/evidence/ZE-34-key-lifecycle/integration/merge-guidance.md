# ZE-34 append-only integration handoff

Read-only preparation against main HEAD `0a6312a94262b5c35607a93849dee2b3ea6606ce` and its working-tree EOF
catalog export correction. Candidate `3463cb8c30b07a85b2e4d207c11d920361b1062c`, base `a3e093348ba03e42218613bda598daf2c2c061b2`.
Only this guidance file was written. No source edits, tests/builds, profile
operations or tracker updates were performed. ZE-34 remains in_progress.

## Existing production source changes

There are exactly three modified existing core production files in the
candidate: property_graph/canonical.rs, property_graph/provenance.rs and
property_graph/mod.rs. The independent oracle has one modified existing
production file, tests/adversarial-oracle/src/lib.rs. No other existing core or
oracle production body changes. New core files are key_lifecycle.rs and its
bounded.rs helper; allocation_tests.rs is test-only. The primitive PG5 oracle
is new. Full source inventory is committed in the candidate evidence.

| Existing file | Candidate relationship to its base | Candidate SHA-256 |
| --- | --- | --- |
| `crates/zeppelin-embed/src/property_graph/canonical.rs` | 22253 unchanged prefix bytes; 1414 appended bytes | `cf377cb5d7ad52dd6e803bb632b29b0db701789b17c56b61134784a7975cb628` |
| `crates/zeppelin-embed/src/property_graph/provenance.rs` | 8226 unchanged prefix bytes; 1016 appended bytes | `d422d1a78d862e65c0c8566b811a6313febb32dc4c09e25a07e9339df6968e77` |
| `tests/adversarial-oracle/src/lib.rs` | 4649 unchanged prefix bytes; 30 appended bytes | `19deb3f88942ca56ce1e64c049cd575891c382a23473d33dbcf3d3ff3a27d2dd` |

Canonical/provenance main files currently equal their candidate-base files,
so their candidate append patches can apply without moving any old byte.
Do not transplant the candidate's entire oracle lib: main also owns ZE-42/35
module declarations. Append the ZE-34 declaration to the final main bytes.

The candidate core mod.rs inserts declarations near its top. That placement
must change at integration: this file contains executable DomainError Display
code and insertion moves its old LLVM source positions. Merely retaining the
same method text is insufficient for retained-profile accounting.

## Exact core module append

Retain the final ZE-35 core module file byte-for-byte, including storage's
existing declaration and catalog's new EOF declaration. Do not add EntityShape
to the earlier canonical export group and do not insert key_lifecycle near the
top. Append exactly these bytes (one leading newline):

```rust

mod key_lifecycle;

pub use canonical::EntityShape;
pub use key_lifecycle::{
    BatchClassification, BatchDisposition, BatchTarget, CanonicalRecord, CurrentEntity, CypherEdit,
    KeyDecision, KeyLifecycleError, KeyRequest, KeyState, PendingKeyChange, classify_cypher,
    classify_key, summarize_key_batch, validate_distinct_targets,
};
```

Rust item resolution is independent of textual declaration order. This exposes
exactly the candidate's public API while preserving the prior entire module as
a prefix, including every existing executable line.

Observed main core module before this append:

- SHA-256 `373d7ebfef28660c31fbf9647fa8fa6df0c7d23a39b8feb19fa24705d49efd56`
- Length 3575 bytes
- Complete a0b240c ZE-42 module prefix verified: 3490 bytes
- Proposed merged SHA-256 for precisely this snapshot plus the snippet:
  `ce8d848b7a49499a3e27417c60f890efe96efcf67c8faea77cbb5bdfc2caf471`

If main changes again, recompute these hashes against its final ZE-35 bytes.
The invariant is `merged_bytes.startswith(pre_merge_main_bytes)`; source hashes
must be recorded after any rustfmt operation too.

## Exact oracle and driver module appends

Root confirmed it will move ZE-35's `pub mod graph_catalog;` to EOF in
`tests/adversarial-oracle/src/lib.rs`, preserving the complete ZE-42 prefix.
The inspected provisional main still inserted that declaration before
executable OracleRecord/escape code; root was notified and is correcting it
before the final ZE-35 report. Append ZE-34 only after the final corrected main
prefix, without changing or reordering its earlier declarations:

```rust

pub mod graph_key_lifecycle;
```

The same appended declaration belongs at EOF of `tests/adversarial/mod.rs`,
retaining all existing driver modules. Keep the existing tests, catalog/storage
required coverage keys, and root's fuzz extension. Merge the candidate's twelve
`property-graph.lifecycle.*` coverage strings into the required smoke list.

The runner's existing probe sequence currently ends with:

```rust
    super::property_graph::probe(seed, &mut coverage)?;
    super::graph_contents::probe(seed, &mut coverage)?;
    super::property_graph_storage::probe(seed, &mut coverage)?;
    super::graph_catalog::probe(seed, &mut coverage)?;
```

Add the following next line, preserving every existing probe:

```rust
    super::graph_key_lifecycle::probe(seed, &mut coverage)?;
```

Runner line shifts affect test-driver source, not the production per-crate
inventory. Core CLAUDE.md and adversarial_tests.rs are simple EOF appends; keep
the current ZE-42/35 material, then append the candidate's ZE-34 material.

## Focused actual-commit checks under the new user priority

The user now defers big, nonessential plan-driven full/adversarial suites until
the code is done. Accordingly, the commands below are narrow checks on the
actual merged ZE-34 commit, not a requirement to repeat full lib/workspace,
smoke, coverage or campaign suites now. These are proposed commands only; this
read-only handoff executed no tests. Root owns the linked deferred-qualification
backlog tickets and will record them in the implementation resolution.

Run from main after root has finalized the append-only merged source. Use a
separate normal target selected by root; preserve the coverage target/profiles.

The exact new allocation audit, one test, verifies full 16,384-target sorting,
200 KiB-key exact replay, duplicate refusal, zero allocations and a positive
17-byte allocation control:

```sh
CARGO_TARGET_DIR=target/ze34-integration-audit cargo test -p zeppelin-embed --features allocation-audit --lib property_graph::key_lifecycle::allocation_tests::full_target_sort_and_exact_retry_have_zero_allocator_calls -- --exact --nocapture
```

Do not remove the exact filter or run all core lib tests for this handoff. Keep
feature-enabled allocation-audit build artifacts separate from retained default
coverage artifacts; enabling this feature changes accounting code in existing
modules.

ZE-34 public contract suite, 17 candidate tests:

```sh
cargo test -p zeppelin-embed --test graph_key_lifecycle -- --nocapture
```

This exercises the exact finalization-constructor parity/refusal behavior and
new CanonicalContents shape accessor as well as the lifecycle decisions. If an
append-only merge needs a direct regression check of the adjacent existing
provenance/stream seam, its bounded public suite has 11 tests:

```sh
cargo test -p zeppelin-embed --test graph_canonical -- --nocapture
```

Independent PG5 oracle unit test, one candidate test:

```sh
cargo test -p zeppelin-embed-adversarial-oracle --lib graph_key_lifecycle -- --nocapture
```

Exact test name:
`graph_key_lifecycle::tests::primitive_lifecycle_oracle_preserves_creation_replay_and_incarnation_fences`.

Runner filter `graph_key_lifecycle` selects only the two new direct probes;
it does not execute the big seeded/adversarial smoke suite and avoids the four
unrelated existing tests selected by the candidate's broader `lifecycle` filter:

```sh
cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests graph_key_lifecycle -- --nocapture
```

The selected names are:

- `property_graph_key_lifecycle_probe_preserves_exact_history`
- `adversarial::graph_key_lifecycle::lifecycle_oracle_can_fire_on_result_and_retained_field_corruption`

The source candidate already records 14 deliberate assertion mutants with exact
restoration, all 17 public cases, the primitive oracle, direct probes, and
before/after 224-episode smoke results. Those remain historical evidence for the
candidate source. They are not a claim that broad qualification ran on the final
combined main commit. Do not rerun source mutations in main while integration
owns its source/binary/profile inventory.

## Explicit deferred qualification boundary

No full core lib suite, combined adversarial smoke/campaign, full workspace
suite or full per-crate coverage run is required now by this handoff. Preserve
those unexecuted-on-final-commit obligations in root's deferred-qualification
backlog tickets and link them from ZE-34's resolution. Do not claim the focused
checks are equivalent to those suites or to full per-crate acceptance. The
candidate's 93.69/96.91/100 percent new-file diagnostics are component-only
observations, not whole-crate coverage. Later coverage runs must retain the
source-position/prefix and unchanged-denominator safeguards described above.

This prioritization changes qualification scheduling; it does not weaken the
implemented key semantics or invent allocator/WAL/publication/DETACH durability
proof. No source semantic change is proposed in this merge guidance.
