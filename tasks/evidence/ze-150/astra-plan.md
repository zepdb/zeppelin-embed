# ZE-106 readiness and bounded execution recommendation

Prepared 2026-09-20 against `56e4d93a30dbdc607090f939f9cf7c7400bf48b9`, worktree `/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-106`, branch `codex/ze-106-native-lock-modes`.

This is a source-based planning recommendation. No implementation, test, build, fault campaign, or native-platform acceptance was performed. The root owns acceptance of this recommendation, new flat tickets, dependency edges and canonical plan updates.

## Recommendation

Do not execute or close the full ZE-106 scope on this pin. Add the missing producer dependencies `ZE-106 --by ZE-40` and `ZE-106 --by ZE-46`, retaining its existing ZE-29/ZE-39 dependencies and every original acceptance criterion. Real read-only graph recovery and durable reclaim-intent production are absent. Neither schema fixtures nor the legacy read-only Store can establish the missing behavior.

A useful independent flat ticket is **Add shared native graph lock ownership**, epic E3, type story, themes graph/storage/correctness. It can implement and test the lock primitive now in disjoint files. Its deliverable is explicitly the lock primitive; it does not satisfy ZE-106's graph reopen, zero-mutation recovery, reclaim, old-reader/sweep, or release qualification acceptance. Add this producer as a prerequisite of ZE-40 and ZE-106. Do not make ZE-40 depend on all of ZE-106: that would create a cycle after the missing recovery edge is restored.

The ZE-40 planner agrees to consume the integrated primitive and own native constructor access-mode routing. ZE-40 will not edit `lock.rs`; this flat lock ticket will not edit native lifecycle/recovery. Interface spelling below is proposed until root approval and compiled integration, not an existing or frozen API.

## Current producer evidence

All source coordinates below are relative to `crates/zeppelin-embed/src/` at the pin above.

| Requirement | Actual source and implication |
| --- | --- |
| Exclusive process ownership | `lifecycle/lock.rs:35-79` uses a process-local `BTreeSet<StoreKey>` and one file per exclusive guard. `:142-169` uses nonblocking POSIX `F_SETLK/F_WRLCK`; Windows remains the existing `LockFileEx` path at `:172-179`. There is no shared operation. |
| Read-only lock without mutation | `lock.rs:109-118` opens with create/read/write. It cannot serve a read-only acquisition. `lifecycle/mod.rs:2474-2490` deliberately returns no lock for legacy ReadOnly; do not change that legacy contract. |
| Native constructor | `lifecycle/mod.rs:2584-2600` rejects ReadOnly, then acquires exclusive ownership. `native_graph/persistence.rs:525-565` exclusively creates a native directory and calls this constructor; `:740-767` exposes native creation only. No native existing-store recovery constructor exists. |
| Existing lifetime owner | `lifecycle/mod.rs:2438` retains `Mutex<Option<StoreLock>>`; `lifecycle/close.rs:204-207,278-286` drains native work/readers before releasing it. ZE-40 can retain a shared guard in the same slot. |
| Incomplete graph tail | `property_graph/wal/replay.rs:90-106` provides `ReplayEnd { complete_bytes, incomplete_tail }`; this is a codec classification, not a Store reopen path. Real checkpoint-to-recovered-view integration is ZE-40. |
| Pending reclaim intent | `property_graph/wal/replay.rs:31-75,421-440` requires semantic `ReplayValidator` proof hooks. WAL maintenance schemas/codecs are present, but `property_graph/storage/` contains no `reclaim` or `consolidation` module. No real producer can durably capture/mark/commit a reclaim intent and then resume unlink. ZE-46 owns it. |
| Existing maintenance entry | `native_graph/write.rs:1418-1464` admits/rechecks maintenance but currently calls checkpoint only. It does not generate durable reclamation proof or perform safe unlink. |
| Atomic lease/capture foundation | `native_graph.rs:556-585,865-899,912-966,969-1077` uses one publication-state mutex for admission, registration, publication and protected capture. ZE-39's actual-producer test is `native_graph/tests/publication.rs:1358-1450`. This foundation is useful, but there is still no actual sweep producer for ZE-106's race/old-root acceptance. |
| Shipping platform | Live ZE-103 resolution and `src/lib.rs:30-34` restrict graph to macOS 14+ arm64. No Windows/Intel graph implementation or qualification is required. Existing legacy platform behavior remains unchanged. |

Canonical contracts are `docs/graph/plans/writes.md` admission/leases, recovery and reclaim sections; `storage.md:91-98`; `bindings.md:17`. They require shared/exclusive process locks, Busy on conflict, private read-only replay, and no truncation/checkpoint/spill/intent/unlink during read-only open. A committed pending intent must come from the actual completed mark/protected-set/candidate producer. Planned/schema-only data is not evidence of that behavior.

## Flat lock producer: exact boundary

Allow at most these source changes:

1. `crates/zeppelin-embed/src/lifecycle/lock.rs`.
2. New child unit-test file `crates/zeppelin-embed/src/lifecycle/lock/native_tests.rs`, declared under `cfg(all(test, feature = "graph-cypher"))`.
3. Its assigned ticket's evidence README and exact approved-plan copy under `tasks/evidence/`.

No lifecycle `mod.rs`, close, native_graph, WAL, storage, VFS, Windows sys, Cargo/dependency, existing test, feature/platform, or inherited-file edits. No public GraphStore/open facade. No new lock file or different locking protocol. Existing `StoreLock::acquire(&Path)` remains the exclusive API and keeps its errors and legacy behavior.

Add one crate-private graph-feature method, provisionally:

```rust
pub(crate) fn acquire_shared(directory: &Path) -> Result<Self, StoreLockError>
```

It returns the same RAII guard type. On macOS it opens the already-existing `writer.lock` read-only, with no create/truncate/write option, and takes the same entire-file `F_SETLK` range using `F_RDLCK`. Missing or inaccessible lock files return their actual IO errors; they must not be created or reported as Busy. Conflicting modes continue to use `WouldBlock`, which ZE-40 maps through the existing StoreBusy convention. No in-place upgrade/downgrade operation is introduced.

The shared primitive must participate in the same directory-identity registry as exclusive acquisition. The registry owns **one OS file descriptor per admitted store in this process**, its mode, and a checked `u32` shared-holder count (maximum `u32::MAX`, no per-reader registry allocation). Each returned guard owns one registry claim. A second same-process shared acquire uses `checked_add`; overflow returns an explicit IO error without changing the entry, opening a file or issuing another lock call. Successful increment likewise performs no file open or lock syscall. This is integer-exhaustion handling, not a new 1,024-handle policy inferred from the unrelated per-coordinator read-lease limit. Exclusive-versus-any and shared-versus-exclusive reject before opening another descriptor. Last guard drop removes the entry and closes its descriptor while still under registry exclusion; earlier shared drops leave it open. A failed first acquisition removes no other claim and leaves no entry. Stable directory identity preserves alias-path exclusion.

This is required by the actual existing lock mechanism. The host's `man 2 fcntl` confirms that closing any descriptor for a file removes that process's record locks for the file; merely adding F_RDLCK while retaining one independently opened File per guard is wrong. Holding registry exclusion through first open/nonblocking acquisition and last removal/close also prevents local admission/release races. Keep the established Windows exclusive open and locking code unchanged. Do not switch to flock/OFD locks or add platform abstractions/dependencies.

## Narrow execution and proof for the flat ticket

Use one Sol/xhigh executor after root accepts the split and a reviewed pin. Tests are unit tests because the graph shared API and native create producer are crate-private. A child test invoked through `current_exe --exact <fully-qualified-helper> --nocapture` performs real OS acquisitions. Use stdin/stdout acknowledgement barriers with bounded test deadlines and deterministic cleanup; no sleep-based ordering, ignored evidence tests, persistent extra binary, or Cargo changes.

Create the fixture through existing `Store::create_native_graph`, Durable+Durable, then close it. The real native producer supplies the persistent lock file. Subsequent lock probes are explicitly primitive tests, not successful graph reopen claims. The child helper does nothing when its explicit role environment is absent and is excluded from the named acceptance selection.

Write these five named cases:

1. `native_shared_lock_allows_two_processes_and_excludes_writer`: two separate processes hold shared guards concurrently after both acknowledgements; a real writer attempt returns WouldBlock. Exercise an actual native writable creator against a shared acquisition while its coordinator remains alive, then release/close before the closed-store probe portion.
2. `native_shared_lock_retains_kernel_ownership_until_last_local_drop`: two same-process shared claims, including an alias spelling, use one retained descriptor. A refused same-process writer does not damage ownership; after dropping one reader an external writer still returns WouldBlock; only final drop admits it.
3. `native_shared_lock_rejects_conflicting_modes_and_preserves_errors`: writer/shared conflict in both process directions and same-process directions; missing lock is NotFound with unchanged directory inventory; a deterministic directory-at-lock-path IO error is preserved rather than reclassified Busy. After failed admission, a valid acquisition still succeeds. A scoped test-only registry count mutation to `u32::MAX` must prove overflow rejects without count wrap, descriptor close or ownership loss; restore the actual holder count before teardown and prove external writer exclusion. Do not use permission-only controls that root can bypass.
4. `native_shared_lock_releases_on_child_exit_and_kill`: acknowledge the held shared or exclusive lock, prove conflict, then request orderly child exit or kill that exact owned child; wait for termination and prove immediate first-attempt acquisition. No stale-lock cleanup or retry loop.
5. `native_shared_lock_never_creates_or_writes_lock_file`: compare exact file bytes/length and directory inventory across acquisition/drop and failed acquisition; verify the held descriptor is read-only using the existing local descriptor inspection pattern and `F_GETFL`. Success also holds when the existing lock file has no write permission. The primitive never passes through VFS, so an empty VFS trace alone cannot establish its behavior.

The behavioral RED for shared concurrency can run the identical scenario against the currently available exclusive-only acquisition seam: the second reader attempt is observably rejected with WouldBlock, violating the unchanged assertion that both reader attempts succeed. After the shared method is implemented, change only the scenario's shared-acquisition binding to that method and keep its process protocol/oracle fixed. Record this binding change explicitly; do not invent a production stub, call compilation failure behavioral RED, or call it native read-only recovery.

Require a deliberate can-fire control for premature local release: in a temporary test-only mutation, make a non-final shared drop release kernel ownership; the retained-reader/external-writer test must fail. Restore the mutation byte-for-byte and rerun the named cases. A test that never observed the wrong OS admission is insufficient. Use the existing exclusive-registry regression as another control against closing a descriptor on refused admission.

Commands after root-approved implementation (no command below has run):

```sh
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher --lib -E 'test(native_shared_lock_)'
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --test single_writer
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --test wal_recovery -E 'test(store_lock_rejects_a_second_writer_and_releases_on_drop)'
cargo check -j 4 -p zeppelin-embed --features graph-cypher
cargo check -j 4 -p zeppelin-embed
git diff --check
```

The small existing `single_writer` target specifically retains legacy simultaneous writer/read-only behavior, inode-alias ownership, descriptor-inheritance and refused-acquire lock retention. Do not edit those assertions to accommodate the new graph-only method. Add no broad/full/adversarial execution: ZE-118 retains those campaigns. Inspect the existing runner's lock-related coverage before implementation, document whether this primitive adds a schedulable path, and record required downstream graph process-lock controls for ZE-106/ZE-118 without claiming unrun coverage. Preserve existing fault/coverage contracts.

Record native macOS arm64 host/OS, actual child PIDs and acknowledged modes, each failure kind, exact named RED/GREEN commands and outputs, restored mutation hashes, final source allowlist and preservation hashes. Current-host success is not minimum macOS 14 runtime evidence; ZE-106/ZE-118 retain that acceptance. Stop after these narrow checks pass. Two unsuccessful fixes of one issue or an ownership expansion requires a blocker report, not a larger framework.

## Consumer integration and remaining ZE-106 acceptance

After the producer is integrated, ZE-40 owns routing the native owner's OpenOptions access mode to exclusive/shared acquisition and keeping the guard in the current lifetime slot. Native creation must continue to require writable ownership explicitly; relaxing the shared owner constructor must not admit a read-only create. Read-only recovery installs the coherent bundle with no NativeWriter, validates/replays privately, and does not open an append handle, truncate, checkpoint or resume reclamation. Existing legacy read-only Store remains unchanged. Mutation/maintenance entries must refuse the native read-only mode before effects.

ZE-40 supplies a real existing-store recovery path and an incomplete tail produced by an interrupted actual native append; ZE-46 supplies durable protected roots/complete mark/candidate proof, committed pending intent and resumed writable cleanup. ZE-106 then exercises both using actual graph opens in separate processes, exact zero mutation events/unchanged on-disk bytes, real pending-intent and lazy-old-root behavior, registration/publication/sweep can-fire races, close/crash release, and supported-platform evidence. No closing criterion is removed or declared complete by this flat split.

Next actual action: root records the missing dependency edges and, if accepted, creates/claims the flat lock producer and assigns its narrow implementation. Return ZE-106 to todo with this readiness note while its real producers are pending. Keep canonical writes/storage/bindings/index and the epic decision record consistent with any accepted split, then export the tracker backup. This planning agent makes no dependency or scope decisions.
