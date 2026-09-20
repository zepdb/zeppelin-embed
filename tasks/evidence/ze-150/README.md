# ZE-150 shared native graph lock ownership

Implemented against `56e4d93a30dbdc607090f939f9cf7c7400bf48b9` on macOS 27.0 arm64 with Rust 1.93.0 and cargo-nextest 0.9.145. The shared primitive is crate-private and graph-feature-only. It is process-lock evidence, not native graph reopen or minimum-macOS-14 runtime qualification.

## Result

`StoreLock::acquire` remains the exclusive legacy API. `StoreLock::acquire_shared` opens only an existing regular `writer.lock` with read authority, takes `F_SETLK/F_RDLCK`, and returns the same RAII guard. One directory-identity registry entry owns the process's sole descriptor, mode, and checked `u32` shared-holder count. Local mode conflicts reject before opening; overflow leaves the descriptor/count unchanged; non-final drops retain ownership; the final drop removes the entry and closes while registry admission remains excluded.

The native fixture is produced by `Store::create_native_graph` with Durable+Durable. Child processes use exact test-helper selection plus stdin/stdout acknowledgements, bounded waits, and exact-child cleanup. The terminal run acknowledged these actual PIDs and modes:

- shared readers 85388 and 85391 concurrently; external exclusive writer excluded
- orderly shared child 85383 and killed exclusive child 85387; first post-exit acquisitions succeeded
- exclusive children 85386, 85390, and 85395 for read-only/no-write, process-conflict, and final-local-drop probes

Observed error kinds were `WouldBlock` for lock conflicts, `NotFound` for a missing lock, `IsADirectory` for a non-regular lock path, and `Other` for checked holder-count overflow. The held shared descriptor reported `F_GETFL` access flags `0x0` (`O_RDONLY`), and the native fixture's zero-byte lock file and complete directory inventory were unchanged across successful and failed shared acquisition. Shared acquisition also succeeded with lock-file mode 0444.

## RED, can-fire, and GREEN

Behavioral RED used the final two-child scenario and unchanged oracle, with the child `shared` role temporarily bound to the existing exclusive `StoreLock::acquire` seam. Run `a1813296-61ed-41bb-ab86-4420b9b6c0fd` failed because child 84911 reported `CONFLICT ... shared` where the second reader had to acknowledge ownership. Only that role binding changed to `acquire_shared` after the production primitive existed; run `647ea507-0e59-4207-985b-4cd0bf45cde6` passed.

The premature-release control changed a non-final shared drop to remove/close the registry entry. Run `8de917bb-6486-40b2-8293-d256efae9896` failed on the actual wrong OS admission: child 85192 acknowledged an exclusive lock while one local reader remained. The mutation was restored byte-for-byte before rerunning. Immediate pre-mutation and post-restoration hashes were identical:

```text
012612ecbbdf226ada2a263d7c6dde084a2ea80842028bb5fa6f1435e876dd5b  crates/zeppelin-embed/src/lifecycle/lock.rs
95e131b7d211af2741ce0023ade7c73ba2256c9c36442cd3a4fa5281fdb9d05b  crates/zeppelin-embed/src/lifecycle/lock/native_tests.rs
```

Final formatting and the intentional `_file` ownership spelling produced the final hashes below. No can-fire mutation remains:

```text
cc6f2d72f70d7ae59d98446cc917b410727a7cb8d274f4d55b94febf392afaeb  crates/zeppelin-embed/src/lifecycle/lock.rs
2cdbbc6d35e94f48f40a5940c1f05e023deaa2f4557ad2a0362c3af32a6e988a  crates/zeppelin-embed/src/lifecycle/lock/native_tests.rs
```

The terminal named run was:

```sh
cargo nextest run --profile default -j 4 --retries 0 --success-output immediate -p zeppelin-embed --features graph-cypher --lib -E 'test(native_shared_lock_)'
```

Run `69a06aca-cf8d-4db3-bd4d-ff5c431e16f9`: 5 passed, 642 skipped. The same required command without success-output logging passed earlier as run `b93b0606-ec1c-4b55-9e48-9ed99538da57`.

## Legacy and compile controls

```sh
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --test single_writer
```

Run `a0ccec2e-14c8-4549-8fd1-d1331454ec18`: 6 passed, 1 skipped. This retains legacy writer/read-only behavior, alias identity, descriptor inheritance, and refused-acquire ownership.

```sh
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --test wal_recovery -E 'test(store_lock_rejects_a_second_writer_and_releases_on_drop)'
```

Run `eb7db3bf-b487-49cc-b34d-04e076d8678d`: 1 passed, 48 skipped.

Both compile controls passed with existing unrelated dead-code/unused-import warnings and no new lock warnings:

```sh
cargo check -j 4 -p zeppelin-embed --features graph-cypher
cargo check -j 4 -p zeppelin-embed
```

Scoped rustfmt and `git diff --check` passed. No workspace/full suite, adversarial runner, coverage, fuzz, sanitizer, soak, size, performance, or release build ran.

## Runner and qualification boundary

The existing adversarial runner has legacy second-writer `StoreBusy` and descriptor-inheritance controls (`tests/adversarial/lifecycle_accounting.rs` and lock paths in `tests/adversarial/runner.rs`). Its graph coverage registry has no shared process-lock path. This primitive therefore adds a later schedulable graph process boundary: ZE-106/ZE-118 must cover shared/shared admission, both mode conflicts, last-local-drop ownership, and normal/crash release through actual graph opens. No runner coverage is claimed here.

ZE-40 still owns native access-mode routing and read-only recovery. ZE-106/ZE-118 retain real graph reopen, zero-mutation replay, reclaim/sweep races, public/platform acceptance, broad campaigns, and minimum macOS 14 runtime evidence.

## Scope and preservation

Final ticket paths are exactly:

- `crates/zeppelin-embed/src/lifecycle/lock.rs`
- `crates/zeppelin-embed/src/lifecycle/lock/native_tests.rs`
- `tasks/evidence/ze-150/README.md`
- `tasks/evidence/ze-150/astra-plan.md`

The accepted plan copy exactly matches `/tmp/ze-106-astra-execution-plan.md`:

```text
3146e705b61ed241c5d0a8656f92498dd6ed7150ece5281d2a8e80a3758f2d97  tasks/evidence/ze-150/astra-plan.md
```

Inherited paths and symlinks were not staged or changed. Their preserved hashes are:

```text
8e19c948be4fa3ac026e7cb833118744fb50a4b3b4c87844a0c156497166255b  .gitignore
e34cf1436cd36f815a941dc64113932cabd4096c78be0ed0493ff09c812d8e97  AGENTS.md
0126721b94a3b3fd8f23c7a99d89a07b83d61332055e0c19e025635d078449cc  README.md
df909e56a30ee1d9c64e047077a9c9260237b78275058807f3f4ab6559be8cb1  CONTEXT.md
1ee843ba44604a55118c5f1c24b0c59157bc15e4d44a524320543e4513346822  plan.md
a0811ce6d4364c99a5261e8ad683ed6795203607c30ee359a9d4c332b7759ec1  skills-lock.json
```
