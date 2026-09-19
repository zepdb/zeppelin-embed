# ZE-132 Cypher fixture directory isolation

Date: 2026-09-19

Base: `51f657c60b2bfde991887643126e88e279a75063`

Host and toolchain are recorded in `environment.txt`. Raw command output is
recorded beside this report. This is focused ZE-132 evidence, not full-suite,
workspace, adversarial, coverage, fuzz, size, Windows, or CI qualification.

## Fixture audit

The audit covered every `temp_dir`, `create_dir`, `remove_dir_all`, and
`Scratch` use under `crates/zeppelin-embed-cypher/tests`.

| Integration target | Tests | Directory-using tests | Result |
| --- | ---: | ---: | --- |
| `runtime_lowering` | 4 | 4 | Changed |
| `read_lowering` | 13 | 13 | Changed |
| `lowering_semantics` | 8 | 8 | Changed |
| `binding_resources` | 4 | 2 | Changed |
| `core_lowering` | 4 | 1 | Unchanged |
| `pattern_contract` | 2 | 2 | Changed |
| `lowering_allocation` | 1 | 1 | Unchanged |

The five changed multi-fixture targets now derive an owned path from the
existing prefix, process ID, and a process-wide `AtomicU64` invocation counter.
`Ordering::Relaxed` is sufficient because only uniqueness is required. Each
existing fixture still creates and removes only its own path. Test bodies,
store close ordering, allocation observations, memory baselines, retained
owner checks, and lifetime assertions are unchanged.

`core_lowering` and `lowering_allocation` each create one directory in one
test, so they cannot collide with another fixture in the same integration-test
process. They remain byte-identical to the base hashes. No extra helper unit
test was needed because the named `runtime_lowering` command reproduced the
actual collision before the change and passed after it.

## RED

Command:

```text
cargo test -p zeppelin-embed-cypher --test runtime_lowering -- --test-threads=4
```

The unchanged base fixture exited 101. One test passed and three failed at
`runtime_lowering.rs:20` with `AlreadyExists`. The three failing tests were:

- `compiled_read_drains_through_the_same_runtime_context_and_real_owners`
- `compiled_read_final_check_discards_actual_completed_output_and_all_owners`
- `compiled_read_validation_reports_original_cumulative_work_exhaustion`

Exact output: `red-runtime-libtest.log`.

## GREEN

Same-process libtest command:

```text
cargo test -p zeppelin-embed-cypher --test runtime_lowering --test read_lowering --test lowering_semantics --test binding_resources --test pattern_contract -- --test-threads=4
```

Result: exit 0; 31 passed, 0 failed across the five affected targets. This is
the evidence that the same-process fixture collision is fixed. Exact output:
`green-parallel-libtest.log`.

Normal repository nextest command:

```text
cargo nextest run -p zeppelin-embed-cypher --test runtime_lowering --test read_lowering --test lowering_semantics --test binding_resources --test pattern_contract -j4
```

Result: run `739cc296-3820-4096-9fd5-57e0b8d5d667`; 31 tests across five
binaries passed, 0 skipped. Nextest executes tests in isolated processes, so
this proves the normal focused path remains green but is not the evidence that
exposed the original same-process collision. Exact output:
`green-nextest.log`.

## Static checks and preservation

Strict scoped Clippy:

```text
cargo clippy -p zeppelin-embed-cypher --no-deps --test runtime_lowering --test read_lowering --test lowering_semantics --test binding_resources --test pattern_contract -- -D warnings
```

Result: exit 0. `rustfmt --edition 2024 --check` over the helper and five
changed targets, plus `git diff --check`, also exited 0.

An initial Clippy command without `--no-deps` exited 101 before reaching the
owned test lint because the unchanged `zeppelin-embed` dependency has 14
`-D warnings` findings in `kernels/mod.rs` and `lifecycle/mod.rs`. No ZE-132
source is involved. The scoped `--no-deps` command is the ticket's changed-test
lint. Scoped output and the baseline limitation are in `lint-and-format.log`.

All 45 inherited paths in `/tmp/ze-132-preservation.json` matched their
recorded SHA-256 values after implementation. The manifest SHA-256 is
`57c7fb2ef567896491b0ce3b1118f8e0f00733ea51130c934c7d84999c13376a`.
Source hashes are in `source-sha256.json`.
