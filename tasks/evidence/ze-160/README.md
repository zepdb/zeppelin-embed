# ZE-160 native close owner release evidence

Date: 2026-09-20

- Planning/source pin: `d2693a5e185f2768527133d96d0262d91e5522f5`.
- Accepted plan: `astra-plan.md`, SHA-256
  `1b6507d4e7dc99b7b6546ae2214c494277b05fad2acf632ebb3a1ae077c543ac`.

## Result

Both native close cancellation walks now visit their fixed registry slot count,
upgrade at most one owner while holding `publication.state`, set its existing
cancellation flag, then unlock before destroying that temporary owner. Normal
close preserves the exact `native graph publication` synchronization error on
relock; best-effort close preserves poisoned-mutex recovery. Grace, drain,
notification, current-publication clearing, admission, and registration-removal
policies are unchanged.

The test-only token hook pauses after the successful Weak upgrade and real
cancellation store. It does not unlock the mutex, retain an owner, alter
cancellation, or perform the correction. Both schedules use a real durable
`Store::create_native_graph`, zero reader-drain grace, a leading empty registry
slot, a target owner, and a later surviving reader. The best-effort schedule
transfers the only Store into the teardown thread so it exercises final
`Store::drop`.

## RED to GREEN

The original normal-drain loop produced the intended bounded RED:

```text
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(native_close_drain_releases_last_temporary_owner)'
```

Nextest run `e7f158ea-a249-4faf-868e-3065f6fce47c` failed after 10.029s with
`close-owner probe timed out after reached after-upgrade two-to-one last-owner
handoff`. The hook had observed exact strong count two, exact
`ReadCancelled`, and strong count one after the external target was dropped.
The original loop then deadlocked when its final temporary owner destroyed the
registration under the same mutex. Raw output is `red-close.log`.

The original real Store-drop loop independently produced the same intended
RED with the exact best-effort test selector. Nextest run
`52d92b0a-1391-40ad-864f-0e9ba513afe5` failed after 10.013s at the same confirmed
two-to-one handoff. Raw output is `red-drop.log`.

After the two unlock/drop/relock corrections, the exact individual tests
passed:

- normal drain: run `2932260d-3bbe-479e-907f-6ec642bfff88`, 1/1 passed;
- final Store drop: run `00d32d8c-b064-47cc-8cef-6798efd07913`, 1/1 passed.

Raw outputs are `green-close.log` and `green-drop.log`.

The exact final three-new-plus-four-existing selection passed in nextest run
`ae12ada8-cf52-423e-8b2b-da89cc2ed307`: 7 passed, 662 skipped, zero retries.
Raw output is `final-tests.log`. It covered:

1. `native_close_drain_releases_last_temporary_owner`
2. `native_close_best_effort_releases_last_temporary_owner`
3. `native_close_owner_walk_preserves_poison_policy`
4. `native_read_close_cancels_and_drains_current_and_retired_leases`
5. `native_read_drop_cancels_without_destroying_borrowed_mapping`
6. `native_read_clone_retains_one_registry_entry_until_final_drop`
7. `native_read_admission_registers_before_replacement_capture`

The poison control proved normal close still returns
`StoreError::Synchronization { component: "native graph publication" }` and
best-effort close still uses the poisoned state, cancels two actual readers,
clears current without draining them, and permits later registration cleanup.

## Compile and lint controls

These four required compilation commands passed with `-j4`; their raw outputs
are the matching `check-*.log` files:

```text
cargo check -j4 -p zeppelin-embed --lib --no-default-features
cargo check -j4 -p zeppelin-embed --lib --features graph-cypher
cargo check -j4 -p zeppelin-embed-workspace-tests --test adversarial_tests
cargo check -j4 -p zeppelin-embed-workspace-tests --test adversarial_tests --features graph-cypher
```

The two workspace-test commands are baseline consumer compilation only. Root
owns addition of the two exact registry keys and the actual registered-consumer
compilation after integration. The adversarial runner was not executed.

The planned strict command was run and is recorded as a failure, not GREEN:

```text
cargo clippy -j4 -p zeppelin-embed --lib --features graph-cypher -- -D warnings
```

It reported 157 unrelated existing warnings as errors, beginning with unused
search imports and continuing through existing dead-code and pattern
`result_large_err` lints. No diagnostic points to ZE-160 code. The same command
at the immutable planning pin `d2693a5` reproduced the same 157 errors. Raw
outputs are `clippy-strict.log` and `clippy-strict-baseline.log`. Per root's
explicit qualification ruling, this pre-existing strict limitation remains
with ZE-118 and was not repaired here.

The same focused Clippy command without `-D warnings` completed successfully,
enforcing the repository's existing deny-level production lint rules. Its raw
output is `clippy-deny-level.log`.

## Scope and preservation

Scoped Rust formatting and `git diff --check` passed. All six inherited hashes
in `/tmp/ze-160-preservation.json` remained exact. `.agents` and `tracker`
still point to the main worktree, and `CLAUDE.md` still points to `AGENTS.md`.
The accepted plan copy remains byte-identical to the `/tmp` artifact.

The native source probe records only these two new keys after each exact
schedule succeeds:

```text
property-graph.read-view.close-drain-last-owner
property-graph.read-view.close-drop-last-owner
```

No full/workspace/adversarial execution, coverage, fuzz, soak, performance,
release, FFI, or platform qualification was run. Those broader obligations
remain with ZE-118.
