# ZE-117: nextest installation and isolated scheduling

On 2026-09-19 the owner paused graph implementation and requested nextest installation and a plan to use process isolation for parallel focused checks. ZE-34/35/48 were paused safely; no active worker mutant/process remained. Broad plan-only graph qualification is deferred in ZE-118 (E12 Backlog), preserving the final release dependency.

Initial availability check: `cargo nextest --version` failed with "no such command: nextest". Downloaded the official macOS universal prebuilt from `https://get.nexte.st/latest/mac`; inspected the archive as exactly one regular `cargo-nextest` entry and installed it in `/Users/aghatage/.cargo/bin/cargo-nextest`. It reports **0.9.145**, revision `00af4550ec3b3b9f0e574b897b06acb95d325ba2`, native aarch64 host. Exact archive/binary SHA-256 hashes are in `ze-117-nextest/installation.json`; no signed redirect token is retained in repository evidence. Rust remains 1.93.0; Cargo.toml/Cargo.lock are unchanged.

The existing repository configuration already set `fail-fast = false` and `retries = 0`; both values are retained. The isolated tooling worktree adds four test processes by default, a `solo` profile using available CPUs, a serial `measurement` profile, and one native-model slot per invocation for the outer text crate. Per-process libtest arguments remain single-threaded while nextest schedules separate processes. Normal output stays captured to preserve concurrency. The runbook explains external-resource and cross-worktree limits, doctests and separate fuzz/Miri/sanitizer runners.

Focused compatibility checks on main's paused source revision passed:

| Target | Passed | Skipped | Evidence |
|---|---:|---:|---|
| graph_catalog, solo profile | 14 | 0 | catalog-solo.log.gz |
| ffi_ownership, four-process profile | 2 | 0 | ffi-ownership.log.gz |
| single_writer, four-process profile | 6 | 1 existing child helper | single-writer.log.gz |

The store-lock cases include child-process execution and inherited descriptors; the FFI cases exercise process-global heap counters. `show-config test-groups` confirms the four CoreML test names are assigned to `native-model`; it only built/listed them. The installed llvm-cov help and official documentation establish runner support, but no coverage campaign was run for this setup.

No whole-suite speedup is claimed from these small compatibility checks. Nextest does not split episodes inside one long test. Existing broad suite results retain their original scope; the owner's new rule defers nonessential plan-driven campaigns until implementation is complete.

Hardware: Apple M3 Max, 128 GiB, 16 logical CPUs, native arm64 macOS 27.0 build 26A5388g. Test data is synthetic temporary stores/byte fixtures. Exact commands, installed hashes and raw compressed logs are retained in `ze-117-nextest/`. All 45 preexisting user files remained byte-identical. The paused ZE-35 main module edit was preserved, and product integration did not resume.

Configuration and runbook are in the separate `codex/ze-117-nextest` worktree. The tooling commit is queued for main integration after the paused ZE-35 amendment, avoiding an unrelated change to its in-progress commit. Until then, every worktree can use the installed binary with the explicit `--config-file` path recorded in `docs/agents/nextest.md`.

References: [official installation](https://nexte.st/docs/installation/pre-built-binaries/), [runner scheduling](https://nexte.st/docs/running/), [process isolation](https://nexte.st/docs/design/why-process-per-test/), [test groups](https://nexte.st/docs/configuration/test-groups/), [coverage support](https://nexte.st/docs/integrations/test-coverage/).
