# Isolated parallel Rust test execution (ZE-117)

Use `cargo nextest run` for the focused Rust unit/integration tests needed by a change. The owner paused graph implementation on 2026-09-19 to install this runner and requested process isolation plus parallel scheduling for subsequent tests. Installed locally: cargo-nextest 0.9.145, native arm64; Rust remains 1.93.0. No Cargo dependency changed.

## Select the work before selecting concurrency

Run named RED/GREEN tests and affected regressions now. A broad suite required only by a plan belongs in a concrete ticket in **E12 Backlog**, with its exact command, affected commits and missing evidence. Run it after implementation is complete. ZE-118 currently records deferred graph workspace/adversarial/coverage qualification for ZE-34/35/48. This changes scheduling, not the eventual acceptance threshold; report deferred evidence as unverified.

## Commands

From the appropriate worktree, retain its own default target directory:

```sh
cargo nextest run -p zeppelin-embed --test graph_catalog --locked
cargo nextest run -p zeppelin-embed --features allocation-audit --lib -E 'test(property_graph::catalog::allocation_tests::)' --locked
cargo nextest run -p zeppelin-embed-workspace-tests --test adversarial_tests -E 'test(property_graph_catalog_)' --locked
cargo nextest run -p zeppelin-embed-ffi --test ffi_ownership --locked
```

`-E` is a nextest filterset. An exact test can use `test(=exact_test_name)`. Zero selected tests must fail; inspect the selected/pass/skip counts. Keep retries at zero so a flaky or deliberately planted failure cannot be converted into GREEN. The existing `fail-fast = false` behavior is preserved.

Default configuration runs four separate test processes at once. Four active worktrees therefore use up to sixteen test slots on this 16-logical-CPU host. Build concurrency is separate; Cargo may serialize builds that incorrectly share one target directory. Keep targets separate across worktrees; combine compatible targets into one invocation when appropriate. Change limits explicitly if other CPU/memory-heavy work is running.

When this is the sole runner, `--profile solo` uses available CPUs. Use `--profile measurement` for wall-clock/RSS/footprint measurements and keep other runners/builds idle. Parallel functional-test timings are not benchmark evidence. Tests in the outer text crate use the `native-model` group (one at a time within a nextest invocation) because model evaluation shares accelerator capacity. Coordinate such tests across worktrees as well; nextest groups are not machine-wide locks. This initial conservative text group can be narrowed after directed resource tests show it is safe.

Do not carry over `-- --nocapture` or use `--no-capture`: nextest serializes execution with that option. Use normal captured output; failures print immediately and at the end. If successful output contains evidence counters, use `--success-output immediate` or `--success-output final`, which retain concurrency. `run-extra-args = ["--test-threads=1"]` applies inside each one-test process, not to nextest's scheduler; it preserves single-thread libtest execution while nextest runs several processes.

## Isolation boundaries

The existing FFI heap/error counters, allocation-audit state and seeded runner's process-global fault plants are separated by a process per test. Existing mutexes remain in source for `cargo test` and do not serialize separate nextest processes. `single_writer` subprocess/lock-descriptor tests also pass under nextest. Each uses its own temporary store.

Process isolation does not isolate fixed filesystem paths, ports, the GPU/ANE, disk bandwidth or an external service. Audit these before widening a suite; use a narrowly matched test group or per-test resource weighting for a real shared resource, and coordinate any machine-wide exclusion across worktrees. Do not add a blanket serial group for ordinary allocator or process-global-state tests.

Nextest schedules individual test functions. It cannot split the 224 episodes inside the single `smoke` function or parallelize a loop inside one long adversarial test. Such splitting would need a separate implementation ticket preserving seeds, oracle coverage and fault controls. Do not launch the broad suite merely to measure a potential speedup.

## Coverage and tools that remain separate

The installed cargo-llvm-cov 0.9.0 exposes `cargo llvm-cov nextest`; official nextest documentation confirms the integration. Use it when coverage is actually due, with a dedicated profile directory and frozen source. The setup checks below did not run a coverage campaign.

```sh
CARGO_LLVM_COV_TARGET_DIR=target/ticket-coverage cargo llvm-cov nextest --no-report -p zeppelin-embed --test graph_catalog
CARGO_LLVM_COV_TARGET_DIR=target/ticket-coverage cargo llvm-cov report --json --output-path /tmp/ticket-coverage.json
```

Do not clean or merge another worktree's profiles. Source-position changes can leave stale executable mappings; final broad coverage should use a fresh frozen-source target and audit actual per-crate source inventories. Never alter exclusions/denominators to reach 90%.

Nextest does not run doctests: use the affected crate's `cargo test --doc` when relevant. Fuzz, Miri, sanitizer, Criterion, packaging, size and shell gates keep their own runners. Installing nextest does not establish those qualification results or change platform scope.

## Current rollout and evidence

Configuration and this runbook live in the isolated ZE-117 worktree until its tooling commit is integrated. During the pause, the executable is already available globally. From any existing worktree, the validated configuration can be selected with:

```sh
cargo nextest run --config-file /Users/aghatage/Documents/code/zeppelin-embed-wt-ze-117/.config/nextest.toml -p zeppelin-embed --test graph_catalog --locked
```

Setup verification on current main source: 14 catalog tests, 2 FFI ownership tests and 6 store-lock tests passed (1 existing child helper skipped as a top-level test). These are compatibility checks, not a measured whole-suite speedup. Evidence and exact commands: `tasks/evidence/ZE-117-nextest.md`.

## Primary references

- [Official prebuilt installation](https://nexte.st/docs/installation/pre-built-binaries/)
- [Process-per-test design](https://nexte.st/docs/design/why-process-per-test/)
- [Runner concurrency, capture and filters](https://nexte.st/docs/running/)
- [Test groups for external resources](https://nexte.st/docs/configuration/test-groups/)
- [Per-process extra arguments](https://nexte.st/docs/configuration/extra-args/)
- [Coverage integration and doctest boundary](https://nexte.st/docs/integrations/test-coverage/)
