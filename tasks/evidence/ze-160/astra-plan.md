# ZE-160: release temporary native read owners outside the close lock

Planning pin: `d2693a5e185f2768527133d96d0262d91e5522f5` (production identical to the ticket's `856cfa6`). This finite plan follows read-only inspection of that committed source and ZE-160. No moving worker source, product edits, builds or tests were used. Root owns approval, ticket state, overlap reconciliation and integration; GPT-5.6-Sol/xhigh executes the accepted plan in its preserved isolated worktree. No further planning/implementation agent chain is needed.

## Confirmed defect and smallest correction

`crates/zeppelin-embed/src/lifecycle/native_graph.rs:1119–1175` has two cancellation loops. Both upgrade a registry Weak to `Arc<NativeReadOwner>` under `publication.state`, set `cancelled`, then implicitly drop the temporary under the same mutex. The final `NativeReadOwner` drops its `NativeReadRegistration`, whose destructor at line 1246 locks `publication.state` to remove the matching slot/token and notify waiters. When the query releases its last external strong owner after close's upgrade, close's temporary is the final owner and self-deadlocks. This is a lifetime/lock-order bug, not slow reader cooperation.

The required invariant is: **no temporary upgraded NativeReadOwner obtained by either close walk is destroyed while that walk holds publication.state.** Keep the two methods' existing policies visible; a new generalized cancellation framework is unnecessary.

In each method, replace only its cancellation `for entry in ...` loop with a checked slot walk:

1. Keep setting `state.closing = true` under the publication mutex before any unlock. Preserve normal drain's initial notify and entire existing grace/deadline loop.
2. Capture the fixed `state.leases.len()` once. Iterate `0..slot_count`, retrieving with `.get(index).and_then(Option::as_ref)` and upgrading the Weak while locked. Do not retain a borrowed entry across unlock. Empty/expired slots are skipped, not a reason to stop the walk.
3. For a successful upgrade, set its existing AtomicBool with `Ordering::Release`. Then explicitly `drop(state)`, explicitly `drop(owner)`, and reacquire `state` before accessing another slot or the final predicate. The only extra retained production state is the slot ordinal/count and at most one temporary Arc. No Vec/array of owners, owner-population allocation, snapshot copy or accounting change.
4. Normal `drain_and_clear` reacquisition uses the existing `StoreError::Synchronization { component: "native graph publication" }` mapping. Best-effort reacquisition uses the existing `PoisonError::into_inner`. Drop the temporary before this fallible reacquisition: a reacquire error must not leave the owner to destruct under a guard.
5. Keep normal drain's notification, final `while any lease slot is occupied { changed.wait(state) }`, and `current.take()` ordering. Keep best-effort's immediate current clearing and final notification, without waiting for the external readers to release. Preserve both method signatures and Store::close/Drop behavior.

A single private helper is optional only if it makes the mandatory unlock/drop/relock ownership obvious and preserves both poison policies without extra allocation. The direct short loops are the default; do not generalize registry/capture code.

## Why the walk is complete and safe

`NativeGraphPublication::new` (approximately lines 649–698) allocates and fixes the lease vector length once. `admit` (lines 950–1005) checks `state.closing` under that same mutex before choosing a free slot and creating/registering an owner. `NativeReadRegistration::drop` removes only its matching token and never shifts slots. No successful admission can fill/reuse a slot after the walk sets closing. During unlocked intervals slots can only become empty; existing lease clones retain the same owner and therefore observe its same cancellation flag. A successful upgrade keeps that owner's registration alive until this walk drops the temporary outside the lock.

If upgrade fails because final destruction already started, that destructor removes its slot when it acquires the mutex; normal drain still waits on the actual occupied-slot predicate. No manual slot removal, stale Weak cleanup policy, token rewriting, early success or `try_lock` fallback is needed. A release notification cannot be lost: after reacquiring, normal close checks the predicate while holding the mutex before Condvar::wait atomically releases it. Preserve the existing release ordering of bundle/view/charge before registration removal.

The grace period remains exactly as implemented. Best-effort remains non-cooperative cancellation/clear rather than reader draining. Ordinary mutex poisoning remains a returned error in normal close and recovered poison in best-effort. Existing mapped-byte/read ownership is unchanged.

## Exact allowed files and overlap

Executor production/test changes:

- `crates/zeppelin-embed/src/lifecycle/native_graph.rs`: the two close loops; a cfg(test or test-support) token-specific close scheduling hook and its initialization; one new test-module declaration; two calls/receipts in the existing `tests::run_adversarial_probe`. No changes to capture, admission behavior, registration destructors, maintenance, write/recovery paths, or public APIs.
- New `crates/zeppelin-embed/src/lifecycle/native_graph/tests/close_owner.rs`: focused fixtures, bounded scheduling probe and named tests below. Reuse existing test-module permissions and real Store/native APIs.
- `tasks/evidence/ze-160/astra-plan.md`: exact accepted copy of this artifact, without rewriting it.
- `tasks/evidence/ze-160/README.md` and narrow raw `.log` evidence files: exact pin, commands, intended REDs, GREENs, preservation and limitations.

Root-owned additive registration, applied before required consumer compilation:

- `tests/adversarial/graph_read_view.rs`: add exactly the two keys below to existing REQUIRED_COVERAGE.
- `tests/adversarial/coverage.rs`: add those same two keys to REQUIRED_GRAPH_SMOKE_COVERAGE.

No Cargo/dependency/feature, nextest-config, lifecycle/close.rs, lib.rs, storage/view.rs, generic runner, oracle or persisted-format change. The current `tests/adversarial_tests.rs` receipt fixture derives its rows/count from REQUIRED_COVERAGE and needs no edit. Existing runner.rs already invokes graph_read_view::probe; existing lib.rs already forwards into the actual native probe, so do not add another adapter or runner entry point.

ZE-46 owns moving capture/reclamation and other native_graph changes. Root merges these close-only hunks, preserving that work; do not copy its file. ZE-154's pending test-only cancellation-observer helpers in NativeReadLease/GraphReadView are independent. This plan does not need or duplicate them: its probe lives within native_graph tests and can observe the real cancellation flag through check_active. Root resolves additive declaration/initialization/test-call conflicts only.

## Deterministic real-path reproduction

Use a one-shot test-support hook targeted by the real registration token, armed only on the fixture's publication. It pauses immediately after the **actual successful Weak upgrade and cancellation store**, before the temporary owner is dropped. Use explicit enter/release synchronization, not sleeps or sampled timing. Both old and corrected methods execute that same hook at that same semantic point; it must not release the publication mutex, retain an extra read owner, alter cancellation, or perform the fix itself. Use one `close_owner_hook: Option<(u64, Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>` in PublicationState, initialized to None under the same cfg as admission_hook. The u64 is the target registration token; take the hook only for that matching successful upgrade. The barriers acknowledge the pause and release it. The hook and its backing exist only with cfg(test or test-support), analogous to the existing admission hook. No shipping allocation/hook is added.

Each named race uses a genuinely created native store, not install_native_graph_for_test or a mocked publication. Create via Store::create_native_graph with existing durable options and reader drain timeout zero; obtain real leases through admit_native_read. Admit/drop one lease to leave a hole, then arrange target and surviving-reader registrations with a hole before a later live slot (e.g. admit gap, target, survivor, then drop gap). Retain a Weak to target's owner for non-owning strong-count/final-release observations. Do not accidentally retain a target clone in the hook or fixture.

Exact schedule, shared by both modes:

1. Arm the hook for target's actual token. Transfer the closer's Store ownership to the close thread: normal mode calls real Store::close; best-effort mode drops the real Store and reaches close_best_effort. Normal-close mode may retain an Arc<Store> inside the owned fixture worker for admission checks while the closer holds its own Arc; the outside supervisor owns neither. Best-effort mode must drop the final Store owner, so no surviving Arc<Store> may bypass real Store::drop. A retained publication Arc is allowed for postconditions; it is not a Store or read owner.
2. Wait for the hook's after-upgrade acknowledgement. While close is paused, assert target's strong count is exactly two (external target plus the closer's temporary), and target.check_active is ReadCancelled. Drop the external target; assert its Weak strong count is now exactly one. Report this reached condition in the evidence, then release the close thread.
3. Old code reaches the actual last temporary drop under state and deadlocks. Corrected code releases state, drops the final owner, and continues over the remaining live slot. Observe survivor.check_active == ReadCancelled using bounded synchronization with publication.changed and the actual predicate; no fabricated flag or result is acceptable.
4. Normal mode: while survivor is retained, assert close has not returned and new admission is rejected as Closing/Closed. Release survivor, receive successful close, join on success, assert target Weak has no strong owner, every lease slot is empty and current is None.
5. Best-effort mode: receive teardown completion **before releasing survivor**, assert survivor is cancelled, current is None, and its actual registration is still present. Then release survivor; assert all slots empty and target owner gone. This proves best-effort did not become draining.

Bound every failed run without blocking the test harness: execute the complete fixture/schedule (including every Store, lease, source, and teardown-owning value) inside one owned worker, with a supervisor outside it waiting at most 10 seconds on a result channel. The supervisor owns no Store/read lease and performs no blocking destructor or join on timeout. On timeout it fails the named test with the reached after-upgrade/last-external-drop marker; dropping a JoinHandle detaches it. Use the repository's existing nextest one-test-per-process configuration, so failed test-process termination releases any intentionally stuck worker. Do not use thread::scope or join on the failure path, and do not allow panic unwinding of a supervisor-owned lease to reacquire the deadlocked mutex. Time is only a fixed failure bound, never the interleaving mechanism or a performance assertion. Send bounded progress messages to the supervisor so timeout diagnostics distinguish setup failure from the confirmed last-owner handoff. Use one absolute 10-second deadline for the complete probe; progress messages must not restart it. No new child dispatch or subprocess framework is needed.

For source-proof receipts, keep the probe reusable under test-support. The bounded supervisor is also the registered-probe failure boundary; it must not claim a receipt until the exact schedule and all postconditions complete. The old code's bounded last-owner timeout is the intended RED, not an upstream setup/compile failure or a loose wall-clock hang.

## Finite checks and exact registration

Add exactly these focused named tests:

1. `native_close_drain_releases_last_temporary_owner`: normal real close schedule above. Observe intended RED against the original loop, then terminal GREEN after the correction.
2. `native_close_best_effort_releases_last_temporary_owner`: real Store-drop schedule above. Observe its own intended RED before fixing that loop, then terminal GREEN. Keep the survivor held until teardown returns.
3. `native_close_owner_walk_preserves_poison_policy`: two compact isolated controls. With state deliberately poisoned by a scoped test thread, normal drain returns the exact Synchronization component. Best-effort still cancels multiple actual readers and clears current without waiting for them; releasing the temporary and reacquiring a still-poisoned mutex must succeed via into_inner. Release readers afterward and verify registration cleanup. Run inside the same bounded supervisor so a broken poison/relock branch cannot hang cleanup. Do not clear poison or alter production error policy to pass.

No Cartesian stress permutations, high-reader-count soak or new timing experiment. Existing grace code is unchanged and is reviewed as such. Run only these necessary existing unit regressions alongside the three new checks:

- `native_read_close_cancels_and_drains_current_and_retired_leases`
- `native_read_drop_cancels_without_destroying_borrowed_mapping`
- `native_read_clone_retains_one_registry_entry_until_final_drop`
- `native_read_admission_registers_before_replacement_capture`

Required new directed keys:

```text
property-graph.read-view.close-drain-last-owner
property-graph.read-view.close-drop-last-owner
```

The existing native `run_adversarial_probe` calls each real schedule once and records its corresponding key only after success. Use `fires=0, clean_controls=1` for the successful fixed race schedule; do not report an injected error that did not happen. Retain the actual token/last-owner/cancel/release assertions inside the helper and the literal old-code failure in evidence. Existing close-first-drain/release keys and relationship oracle output remain unchanged. Root adds these exact keys to the two existing registries; no other runner operation/fault profile changes are needed for this lock-order correction. Full adversarial execution remains ZE-118.

Execution order and commands (all source changes made before broadening beyond the first genuine milestone):

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_close_drain_releases_last_temporary_owner)'
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_close_best_effort_releases_last_temporary_owner)'
```

Add hook/tests first, observe each original-loop RED, correct only the owned loop(s), then rerun the same names. Finally one finite selection:

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_close_drain_releases_last_temporary_owner) | test(native_close_best_effort_releases_last_temporary_owner) | test(native_close_owner_walk_preserves_poison_policy) | test(native_read_close_cancels_and_drains_current_and_retired_leases) | test(native_read_drop_cancels_without_destroying_borrowed_mapping) | test(native_read_clone_retains_one_registry_entry_until_final_drop) | test(native_read_admission_registers_before_replacement_capture)'
cargo check -j4 -p zeppelin-embed --lib --no-default-features
cargo check -j4 -p zeppelin-embed --lib --features graph-cypher
cargo check -j4 -p zeppelin-embed-workspace-tests --test adversarial_tests
cargo check -j4 -p zeppelin-embed-workspace-tests --test adversarial_tests --features graph-cypher
cargo clippy -j4 -p zeppelin-embed --lib --features graph-cypher -- -D warnings
git diff --check
```

The executor runs the listed consumer checks on its implementation as baseline compile controls. Root repeats both actual registered-consumer checks after cherry-picking that individual commit and adding the two exact registration keys to both registry files. Executor baseline compilation is not root registered-consumer compilation and cannot satisfy that integration gate. No FFI layout/callback/result or compiler API changed, so a new binding/compiler matrix is unnecessary. Format only owned Rust files, inspect the exact diff and preservation hashes; avoid repo-wide formatting. No broad/full/advanced/adversarial runner execution, coverage, fuzzing, performance, soak or release build.

## Delivery

One ticket-prefixed implementation commit: `ZE-160: release native close owners outside the publication lock`. Its body records both exact named intended RED outcomes, corrected GREEN and focused controls. Evidence states that the timeout followed the confirmed two-to-one owner handoff for each real path, reports any fixture/compiler failures separately, and records necessary registered consumer compilation. No code/test evidence exists at planning time. Preserve all inherited files/symlinks and moving workers. Root reviews the narrow hunks and registered source, cherry-picks the individual commit and closes only after verified acceptance. Two unsuccessful fixes to one failure or 20 minutes without a useful milestone require a precise root report before another correction.
