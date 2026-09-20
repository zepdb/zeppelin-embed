# ZE-45 final evidence

ZE-45 implements coherent bounded native graph read views on base `3a0ac9dc6e9419a3709e5304ade721915cbf5991`. The reviewed candidate began with commits `092af38984929306b93cb698830062011d7f9cd1` and `ebb5f411ce908fa4c6e100a7f89e9d82c0b5f250`; the final correction commit is `ef2eca09f9ccf6c54ec6cba9daf52b9621f693eb`.

The final correction closes the fixed C1-C5 review findings:

- C1: `GraphPreparation` owns the admitted storage-backed source/catalog, base lease, actual `prepare_native_graph` call, finalization, inventory, and typed failure return. Success, injected creation failure, foreign accounting owner, replacement retention, and stale/foreign base rejection are direct tests.
- C2: nonempty label conjunctions traverse one selected `Labels` membership index, validate empty membership values and exact keys, then validate the live node record and remaining labels. Duplicate and unknown labels are direct cases.
- C3: the retained old `GraphReadView` lazily maps an actual producer descendant after controlled replacement, its roots remain captured, and the new view observes the tombstoned endpoint while old node/text/relationship observations remain exact.
- C4: 27 bounded incremental producer preparations force the actual ninth-run consolidation and a persisted same-type physical split. `GraphReadView` returns the exact 5,405-row bag across that split and the following type boundary at capacities 1, 2, and 256 with cumulative bounded work.
- C5: path filename and pathname capacities are charged simultaneously, reconciled to actual capacity, and released before steady-state mapping ownership. Two lazy opens, cache hit, missing-file failure, memory refusal, and final baseline are direct cases. Immutable artifact validation metadata is cached beside each read-only mapping so repeated block resolution does not rehash an unchanged file.

## Final focused verification

Environment: Apple arm64 `Mac15,9`, macOS 27.0 build 26A5388g, rustc 1.93.0, cargo-nextest 0.9.145. Full details remain in `environment.txt`.

- Native direct set: `cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher --lib -E 'test(native_read_) | test(native_prepared_artifacts_) | test(native_preparation_coordinator_)'` — run `5abe7001-4ad9-4388-b18a-839338d9361a`, 25 passed, 580 skipped. See `final-native-direct-green.log`.
- Persisted split: release-profile command recorded in `final-physical-split-green.log` — run `19441e6a-da96-4a59-9118-bd70165695cb`, 1 passed, 604 skipped.
- Storage preparation: run `395885e6-e956-4c0d-bfc1-99d64b5588ae`, 10 passed. See `final-graph-storage-prepare-green.log`.
- Query storage: run `763ab92e-bb86-4f2f-b109-2841995eda27`, 8 passed. See `final-graph-query-storage-green.log`.
- Focused Clippy: `cargo clippy -p zeppelin-embed --features graph-cypher --lib -- -A warnings -D clippy::indexing_slicing` passed. `git diff --check` passed.
- Current lifetime controls: `lifetime/negative-current.log` exits 101 for the intended node, cursor, text, relationship, prepared result, preparation source, and query-memory nonescape errors. `lifetime/positive-current.log` exits 0 and includes scoped reads plus explicit prepared `into_parts` transfer.
- Preservation: `preservation-current.json` verifies all 44 inherited dirty files/symlink entries unchanged.

The strict `-D warnings` crate-wide Clippy invocation remains blocked by 14 pre-existing findings in `kernels/mod.rs` and `lifecycle/mod.rs`; none is in a ZE-45 changed path. The exact output is `final-strict-clippy-blocked.log`. The ZE-45 indexing/panic-sensitive changed paths pass the focused lint command above.

## RED and correction record

The final Spec review found missing authentic preparation orchestration, label-index traversal, an actual old/new retained-view proof, a real same-type physical split, and stale final evidence. The Standards review found transient pathname accounting and panic-capable indexing. Their immutable reports are `/tmp/ze-45-spec-review-final.md` (`e523361b9c19dcfc464bce66723bfc30585668815d7c1de75ac8381b4c449a45`) and `/tmp/ze-45-standards-review-final.md` (`851373e645da52271aced8108244b1c4d4de29fd1886765827159f7425183eae`). The fixed closure checklist is `/tmp/ze-45-closure-plan.md` (`8f7dbda46423c0f2a0b30921b6c81810e9b961d13061824b8fc101402b8c24a5`).

The C4 fixture exposed, in order: a 64-slot preparation-source exhaustion, a pack-budget refusal, stale per-generation catalog high-water metadata, a test request above the 24 MiB query limit, the absence of a physical split before ninth-run consolidation, a 64-slot query-source exhaustion, and an over-strict per-range remerge assertion. Each failure was corrected without raising the 32 MiB storage, 64 MiB writer, 24 MiB query, or 256 MiB store limits. `review-physical-split-green.log` is retained only as historical failure evidence; `final-physical-split-green.log` is terminal GREEN.

## Qualification boundary

The controlled fixture installer is not durable publication, recovery, production GC, search, public Cypher execution, ABI, or release qualification. Those remain with ZE-39, ZE-40, ZE-46, ZE-50, ZE-53, ZE-60/61, and downstream release work. Per owner instruction, adversarial runner execution, broad workspace/full suites, coverage, fuzz, size, soak, performance corpus, Windows, and packaging are deferred to ZE-118 and were not run here.
