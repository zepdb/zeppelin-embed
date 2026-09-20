# ZE-152 native pattern execution evidence

Date: 2026-09-20

## Inputs and integration boundary

- Frozen execution plan: `astra-plan.md`, SHA-256
  `80a50d90a2494d6b516ad9250d98d06f865afde41df5b6a818926cb619f7b733`.
- Approved join/anchor addendum: `/tmp/ze-152-join-anchor-addendum.md`, SHA-256
  `79c22ba107c36835ef1bb18358614cdc9d659c8dcb7684413987bd3cd0ba5685`.
- Canonical research: `research/native-pattern-after-publication.md`, SHA-256
  `44f25b219716397f530adaf487763837b5d8bbd25a48e4364379a17cc9ac0b58`.
- The worktree was mechanically refreshed to main
  `74055cbd6cba38f71c504c8bb21b1a24dc7ba717` after ZE-40. The refresh changed
  no ZE-152-owned or inherited dirty bytes. The only consumer adaptation was
  delegating ZE-40's `Vfs::for_each_direct_child` method in the test fault VFS.
- Root owns the pending additive three-file shared-runner registration patch.
  This component supplies `tests/adversarial/graph_pattern.rs` and its exact 12
  keys; the full adversarial runner was not executed here.

The storage helper added for this component is:

```rust
GraphReadView::lookup_application_key(
    &self,
    key: ApplicationKey<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<Option<EntityId>, TreeError>
```

It validates the authoritative entity and exact canonical bytes before applying
relationship endpoint visibility. Missing or tombstoned authoritative data
behind a live fence is corruption; a valid relationship hidden by a detached
endpoint is absent from the admitted view.

## RED and GREEN

- RED: `native_pattern_scan_expand_scalar_bag` initially returned
  `NativeExecutionError::Plan(PlanError::Reference)` before the native pattern
  producer existed.
- GREEN: the first `ScanNodes -> Expand -> Filter -> Project -> Collect` route
  passed through the existing typed `execute_in` driver with `batch_rows = 1`.
- RED: the first full ten-test selection exposed a close interleaving in which a
  polling read observed exact `ReadCancelled` before `StoreError::Closing`.
- GREEN: the close control now resumes traversal only after a polling admission
  observes exact `StoreError::Closing`; exact `ReadCancelled` remains an
  intermediate state. The complete selection then passed 10/10.

The final command was:

```text
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_pattern_)'
```

Result: 10 passed, 656 skipped, 0 failed. Raw bounded output is in
`final-tests.log`. The exact selected tests were:

1. `native_pattern_scan_expand_scalar_bag`
2. `native_pattern_keys_labels_liveness_full_ids`
3. `native_pattern_bounded_paths_predicates`
4. `native_pattern_pattern_uniqueness_across_joins`
5. `native_pattern_optional_anchor_and_rebinding`
6. `native_pattern_hash_nested_and_selective_equivalence`
7. `native_pattern_same_view_after_publication`
8. `native_pattern_limits_cancel_close_no_output`
9. `native_pattern_late_expression_and_storage_errors`
10. `native_pattern_seeded_directed_probe_can_fire`

The directed probe used actual native operators and observed every receipt once
before hitting coverage. Its exact Boolean fault/control receipts were cancel=1,
limit=1, uniqueness=1, full-id=1, retained-view=1, late-error=1, release=1,
same-seed=1, and oracle=2 rows. Native-source, path-predicate, and join/optional
receipts came from their real nonzero work counters. The late storage receipt
arms the delegated VFS only after the first private row; a later native
`ScanNodes` artifact read produces exact `TreeError::Io`, `RowsOut > 0`, and one
injected fire. The disarmed scan over the same history returns the exact node-ID
bag.

## Compile and lint controls

Each required command completed with exit status 0. Existing warnings were
preserved.

```text
cargo check -p zeppelin-embed --lib -j 4
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,test-support
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,allocation-audit,query-timing
cargo check -p zeppelin-embed-cypher --lib -j 4
cargo check -p zeppelin-embed-ffi --lib -j 4
cargo check -p zeppelin-embed-ffi --lib -j 4 --features graph-cypher
```

The following three commands also passed, but they are baseline-only until root
applies the pending additive shared-runner registration. Root will record the
actual new-probe compilation afterward.

```text
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4 --features graph-cypher
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4 --features graph-result-test-support
```

The crate lint also passed:

```text
cargo clippy -p zeppelin-embed --lib -j 4 --features graph-cypher
```

Scoped `rustfmt` and `git diff --check` completed successfully. The six inherited
dirty-file SHA-256 values in `/tmp/ze-152-preservation.json` and the `.agents`,
`tracker`, and `CLAUDE.md` symlink targets were unchanged. No full workspace,
adversarial-runner, coverage, fuzz, soak, size, performance, or release command
was run. Those broader qualifications remain with ZE-118. Original ZE-50 remains
blocked by ZE-46 and retains its public/oracle/compaction/reopen acceptance.
