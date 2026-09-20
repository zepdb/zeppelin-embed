# ZE-39 — durable native publication

Implemented from `ce7c5f948e60287697b395e1b27d6363b9f79b6b` in `codex/ze-39-atomic-publication`. Root/Astra took over the preserved Sol worktree at the owner's request and completed the remaining fixes, validation and review.

Environment: macOS 27.0, arm64; stable Rust 1.93.0; cargo-nextest 0.9.145. Fixtures are disposable path-backed local stores using real `StdVfs` mappings. Every nextest command below uses four isolated processes and zero retries. Timing is diagnostic, not a performance claim.

## Implementation

The actual production internal constructor creates an exclusive native store, catalog, checked checkpoint and separate native WAL. The coordinator stages through the real native base, prepares native and sparse artifacts, registers/protects immutable participants, fully syncs files and directory, appends and fully syncs one complete WAL envelope, then swaps one complete bundle and transfers already prepared results. Durable+Ordered is rejected. Retained readers preserve their original source and roots.

The private transition derives both encoded WAL state and the next bundle from authentic staged/prepared owners. It binds all eight candidate roots, base/sequence/checkpoint, sparse membership ordinals, catalog and exact supplemental inventory. Its immutable encoded slice is the only append input. All normal post-sync publication/result handoff work is fixed ownership movement; core/ABI backing and actual shared charges survive together.

Native properties cover all scalar and homogeneous list tags, preserving UTF-8/NUL, integer extrema and exact floating-point bits. Canonical replay reads stream through retained payload references, cumulative bounded resources and the first exact storage error. Catalog/inventory/checkpoint/WAL buffers retain authentic reservations and poll bounded copying/framing work.

NoOp/replay avoid publication, artifacts, serial allocation and checkpoint work. Count and exact byte triggers rotate synchronously before changed writes exceed 64 envelopes or 16 MiB. Pending-size retry is bounded and drops prior scratch. Every prepared inventory remains in checked WAL/checkpoint state until real consolidation; the bounded 8,192-reference limit fails before append rather than forgetting coverage. ZE-46 still owns consolidation and reclamation.

Append/Full-sync/publication uncertainty stops writes and new admissions without invalidating retained leases. Checkpoint failures preserve acknowledged logical state and block changed writes until explicit successful retry. Read capture includes retained/current bundles, prepared and uncheckpointed descriptors, WAL identity/length and a publication-mutex serial fence. Maintenance uses retained admission plus exact stale-base/serial recheck; this ticket exercises the bounded checkpoint producer.

## RED and can-fire evidence

- Original real-path RED: `ze39_fresh_mixed_commit_is_durable_and_coherent`, run `9d751200-41fb-4edb-80aa-6aebe6bd7ad5`, reached fresh production construction and failed with `native graph structured apply is not implemented`. First coherent GREEN: `6b4edad9-eb3a-4f96-9a93-604adc2dd32c`.
- Root checkpoint RED: run `0a287681-fb31-451c-81a0-365eb37daae2` failed because only 1 prepared inventory remained after 64 complete commits. Carrying the inventory chain through checked state produced GREEN `5f2abc50-620a-47f1-b405-cedf173adf7b`; final nine-case run below includes the same assertion.
- Deliberate OUT-root guard omission: run `5bb072e9-60e0-4853-9dea-b481403703f4` failed the expected metadata-substitution refusal in `ze39_protection_capture_and_maintenance_recheck_are_atomic`. The exact original production file was restored and hashed before terminal GREEN. No mutation remains.
- Fault cases assert actual fires and execute matching clean controls. They cover private create/collision/partial create/sync, attempted WAL append/partial append/Full-sync, post-durable publication failure, materializer capacity/failure, cancellation and close, and checkpoint object/directory sync, append-handle opening, selector rename and post-rename directory sync.

## Terminal focused checks

| Selection | Result | Run ID |
| --- | --- | --- |
| nine | 9 tests run: 9 passed, 628 skipped | `d3de4ff9-3676-42ae-b379-c2e3c99302ab` |
| native-regressions | 4 tests run: 4 passed, 633 skipped | `83c7dfc7-b0ef-46df-aacd-6e3f38800866` |
| graph-wal | 3 tests run: 3 passed, 17 skipped | `21ebe220-e0bb-4066-9224-5a0d25dc89fb` |
| legacy-wal | 3 tests run: 3 passed, 46 skipped | `7b152e7f-6cf3-4e0c-a129-a804dc7ba5e3` |
| result-allocation | 1 test run: 1 passed, 653 skipped | `eef013a6-9bff-44f1-b2d5-aea908d686ea` |

The nine-case selection includes real pending-overflow rotation using 512 KiB application keys and physical WAL lengths, actual retained/property/OUT/IN/text/vector observations, and catalog/checkpoint classifier paths. The allocation-enabled case observed zero allocator calls and zero allocation-denial fires during actual publication and result-owner transfer; its core/ABI bytes remained readable afterward.

Exact commands:

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(=lifecycle::native_graph::tests::publication::ze39_fresh_mixed_commit_is_durable_and_coherent) | test(=lifecycle::native_graph::tests::publication::ze39_retained_reader_and_new_admission_observe_whole_generations) | test(=lifecycle::native_graph::tests::publication::ze39_noop_replay_and_mixed_receipts_do_not_publish_extra_work) | test(=lifecycle::native_graph::tests::publication::ze39_checkpoint_thresholds_and_failure_preserve_acknowledged_state) | test(=lifecycle::native_graph::tests::publication::ze39_protection_capture_and_maintenance_recheck_are_atomic) | test(=lifecycle::native_graph::tests::publication::ze39_precommit_failure_keeps_graph_and_path_ownership_private) | test(=lifecycle::native_graph::tests::publication::ze39_commit_attempt_errors_are_indeterminate_and_stop_admission) | test(=lifecycle::native_graph::tests::publication::ze39_result_preparation_and_postcommit_cancel_obey_commit_boundary) | test(=lifecycle::native_graph::tests::publication::ze39_fresh_create_failures_never_adopt_or_replace_identity)'
```

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_read_clone_retains_one_registry_entry_until_final_drop) | test(native_read_close_cancels_and_drains_current_and_retired_leases) | test(native_prepared_artifacts_retain_exact_base_source_and_abort_owners) | test(ze61_complete_search_handoff_precedes_pack_finish)'
```

```sh
cargo nextest run -p zeppelin-embed --test graph_wal --features graph-cypher -j 4 --retries 0 -E 'test(=mutation_preserves_complete_provenance_membership_and_atomic_framing) | test(=checked_checkpoint_watermark_skips_only_complete_retired_history) | test(=required_object_validation_binds_full_descriptor_role_and_checksum)'
```

```sh
cargo nextest run -p zeppelin-embed --test wal_recovery -j 4 --retries 0 -E 'test(=commit_many_uses_one_append_and_one_sync_for_one_group) | test(=durable_retains_every_group_whose_flush_returned) | test(=store_lock_rejects_a_second_writer_and_releases_on_drop)'
```

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher,allocation-audit -j 4 --retries 0 -E 'test(=lifecycle::native_graph::tests::publication::ze39_result_preparation_and_postcommit_cancel_obey_commit_boundary)'
```

## Compile controls

All eight controls passed on the final source. All are compile-only; no adversarial campaign is executed. Logs are retained beside this record, with generated warnings preserved. No warning-free or broad qualification claim is made.

```sh
cargo check -p zeppelin-embed --lib -j 4
```

```sh
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher
```

```sh
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,test-support
```

```sh
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,allocation-audit,query-timing
```

```sh
cargo check -p zeppelin-embed --lib -j 4 --features allocation-audit,query-timing
```

```sh
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4
```

```sh
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4 --features graph-cypher
```

```sh
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4 --features graph-result-test-support
```

Scoped `cargo clippy -p zeppelin-embed --lib --features graph-cypher -j 4`, scoped rustfmt and `git diff --check` complete the source checks. The six inherited dirty file hashes are preserved in both the source worktree and main.

## Qualification boundaries

Runner source now calls the same nine production-path bodies, checks observed fault receipts and feeds actual adjacency rows to the existing independent oracle with a missing-edge negative control. This new runner integration is compiled in default, graph and graph-result hook modes. The broad runner is not executed, per owner instruction; execution/coverage/fuzz/soak/performance/release obligations remain ZE-118.

Existing legacy fault VFS implementations were inspected: they do not drive this separate path-backed native coordinator. The directed adapter therefore supplies its own real `StdVfs` recording/fault wrapper rather than claiming legacy fixture coverage.

No public graph facade, reopened recovery, lost-ack recovery proof, GC/unlink authority, process-lock platform matrix, packaged artifact/Swift qualification, or C result conversion is claimed. These remain ZE-40, ZE-46, ZE-53/66/68/69, ZE-106/107 and their existing acceptance criteria. The later generic materializer consumer receives an owned result, not a success placeholder for absent public APIs.

Accepted plan: [astra-plan.md](astra-plan.md), SHA256 `838b889e65cbc1443bd29cf05d84a3f724e911afca0b53417c88f757120ebda4`. Tracker ZE-39 records the later exact transition clarification and root takeover; no requirement was waived.
