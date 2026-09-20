# ZE-40 — recover native graph checkpoints and committed search deltas

Implemented from `6f37c4180e8a4785b882f50d3cfaa196edaa06b2` on `codex/ze-40-native-recovery`.

Environment: macOS 27.0 (`26A5388g`), arm64; stable Rust 1.93.0; cargo 1.93.0; cargo-nextest 0.9.145. Fixtures are disposable path-backed stores using the real native coordinator, WAL, artifacts and `StdVfs`. Every nextest command uses four isolated processes and zero retries. Timing is diagnostic, not a performance claim.

## Implementation

Native open now authenticates the selector, selected checkpoint, catalog, retained inventories and WAL header, then replays only complete committed envelopes after the checked checkpoint watermark. It restores the exact graph roots, catalog/high-water marks, key fences, sparse text/vector roots and the next allocation serial. Writable recovery rotates a torn terminal WAL before the next append; read-only recovery uses the shared store lock, does not mutate the directory, and retains the recovered owners for the lifetime of active readers.

Replay validation uses the real borrowed `ChangeReader` and charged native sources. It classifies actual base lifecycles, reconciles the entire streamed base and target node/relationship/fence directories, preserves existing symbol/name mappings, and rejects omitted or unlisted changes. Every replay target and the selected checkpoint are checked for canonical label/type membership, physical OUT/IN adjacency including DETACH-retained rows, key-fence agreement, sparse roots and exact prepared-inventory coverage. First concrete storage, control, cancellation and work-limit errors remain typed through the recovery boundary.

Opening an existing native graph never creates a missing writer lock before admission. Legacy open refuses recognized native initialization artifacts before mutation in graph-enabled and feature-disabled builds. Artifact serial enumeration is bounded and polls every directory entry; header-selected decoding is controlled and single-pass. Unknown or unvalidated proof roles fail closed and cannot authorize cleanup.

The VFS contract now exposes bounded direct-child enumeration. `StdVfs` streams `read_dir`; in-memory implementations retain only the current/last path and release their map lock before callbacks; wrappers delegate while preserving their liveness and fault policy. Out-of-tree `Vfs` implementations must add this required method.

## RED and GREEN evidence

- First coherent-path RED: `ze40_complete_mixed_commit_close_reopen_is_coherent`, run prefix `4327ba47`, reached the real close/reopen path and failed because native graph open had no recovery implementation. The minimal real create/apply/close/open/read/next-write path first passed in run prefix `ed7df`.
- Semantic omission RED: a checksum-correct WAL fixture with one real `Mutation` removed was admitted after framing was repaired. The focused test failed at the expected admission assertion. Exact streamed base-to-target reconciliation made the same control reject before admission; focused GREEN run `1368f1c7-acf3-4c12-85b9-f59e2e7df400`.
- Final exact nine-case GREEN after receipt transport: run `ed6280e1-036b-440e-838a-64a0ef386d04`, 9 passed and 647 skipped. The earlier complete nine-case run was `dd2bcba4-9ac5-439e-81a7-375876db8843`.
- Fault cases assert each scheduled fire and run a clean control before returning a receipt. The runner consumes those returned observations directly. It does not synthesize fire or clean counts from labels.

## Terminal focused checks

| Selection | Result | Run ID |
| --- | --- | --- |
| ZE-40 exact nine | 9 passed, 647 skipped | `ed6280e1-036b-440e-838a-64a0ef386d04` |
| native admission, default | 1 passed | `8630733b-3034-4347-9577-62a03d6900c3` |
| native admission, graph | 1 passed | `f1b38e75-f233-4da5-a07e-d7c4edbd1818` |
| graph WAL exact five | 5 passed | `15569d21-b97e-4107-b02e-d8bbfc58bc6a` |
| ZE-39 publication exact three | 3 passed | `01cba6df-f99d-426f-9d2f-ad8907a76c97` |
| legacy WAL recovery exact two | 2 passed | `1c64fe34-758e-42a2-b572-aa2ac7aecd75` |

The nine cases cover coherent mixed graph/search reopen and a subsequent write; unsupported/corrupt/incomplete refusal without mutation; lost acknowledgement and stopped-writer recovery; torn-tail rotation; checksum-valid semantic corruption, missing/corrupt artifacts and checkpoint-only damage; empty history, high-water/fence restoration and same-key recreate; allocation serial orphan/collision/overflow; read-only replay; and exact checkpoint count/byte thresholds, retained historical WAL prefixes, retained inventories/leases and object-sync/selector-replace/post-replace-sync faults.

Exact primary command:

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(ze40_)'
```

The required native-admission, graph-WAL, ZE-39 publication and legacy-WAL selections were run with their exact accepted-plan filters and the same `-j 4 --retries 0` controls.

## Compile and source controls

All eight required feature configurations passed. The final receipt change invalidated only test-support consumers, so the graph adversarial target was recompiled after it; previously passed unaffected production controls were retained as directed.

```sh
cargo check -p zeppelin-embed --lib -j 4
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,test-support
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,allocation-audit,query-timing
cargo check -p zeppelin-embed --lib -j 4 --features allocation-audit,query-timing
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4 --features graph-cypher
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4 --features graph-result-test-support
```

The ten mechanically changed core integration targets and `zeppelin-embed-bench` `wal-throughput` binary compile. `cargo clippy -p zeppelin-embed --lib -j 4 --features graph-cypher` passes with existing warnings. Scoped rustfmt and `git diff --check` complete the source checks. No warning-free claim is made.

## Qualification boundaries

Runner source is registered in the existing seeded adversarial framework, invokes the same real case bodies, transports their observed fault/clean receipts, and compares actual adjacency rows with the independent oracle and its missing-edge control. The runner is compile-qualified only; the broad adversarial campaign was not executed.

ZE-46 retains original reclaim-proof production/parsing and resumed unlink. ZE-106 retains actual pending-intent/process/platform shared-lock qualification. ZE-118 retains large-history/two-second recovery, broad runner, fuzz, coverage, soak, performance, size and release qualification. No public `GraphStore`/ABI facade or persisted-format change is claimed.

Accepted plan: [astra-plan.md](astra-plan.md), SHA256 `4efb37054ae60d8aaff6774f3c165731b91fcdf1a3387d5bb7ba39aa8dc00982`.

Root-approved execution addenda were limited to mechanical `ReplayValidator` forwarding, the crate-private recovery mapping re-export, required VFS-method migration, `StoreLock::acquire_existing`, and exact native base-to-target transition reconciliation. The latter was recorded in `/tmp/ze-40-native-transition-addendum.md`, SHA256 `b7424118ee59152933358fae903c0417b3950614fef3f6acef0ea26b39092a27`. The frozen accepted plan bytes were not changed.
