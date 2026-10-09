# ZE-422: final release gates

2026-10-09. Base main 573a1aa279bb7cf4b8c4c01419a47b5e5d9362c9.
Host Mac15,9, Apple M3 Max, 128 GiB, macOS 27.0 (26A5388g).
Python 3.13.7; Apple clang/ar and platform size/strip.

## Change

The Windows Node shipping job only recorded archive disk usage. It now
runs the existing `scripts/size-budget.sh --graph-archive` against
`target/native-archives/x86_64-pc-windows-msvc/zeppelin_embed_ffi.lib`.
Rust's installed llvm-tools-preview directory is resolved with
`rustc --print target-libdir` and translated for Git Bash with cygpath.
The graph archive cap stays 12,288 KiB; graph-free stays 5,632 KiB.

The v0.7.0 workflow_dispatch native-graph invocation alone skips
`lifecycle::native_graph::tests::mapping_slots::ze316_count_reclaim_bounds_census_and_work`
under the explicit owner waiver. PR and future release runs keep it.
No other test is skipped by this change.

The old installed-checker test's positive fixture omitted Rust and
resource receipts already required by production. The fixture now has
all four consumer receipts with resources; its negative missing-structured
case is retained. Production qualification was not changed.

## RED and GREEN

Commands run from the release-070 worktree. Complete terminal outputs
were read; local raw logs are in `.ctx/ZE422-*`.

- `python3.13 scripts/tests/graph-release.py ReleaseTests.test_windows_shipping_archive_rejects_over_budget`:
  RED one failure, 0.056 s: no linked-section gate in Windows job.
- `python3.13 scripts/tests/graph-release.py ReleaseTests.test_installed_checker_requires_structured_handoff`:
  RED one error, 0.084 s: positive fixture missing required receipt.
- Both named tests plus `test_prebuilt_size_gate_retains_raw_receipt`:
  GREEN three tests, 0.679 s. The Windows command extracted from the
  workflow gates a real clang/ar archive with initialized data larger
  than 12,288 KiB; it exits 1 with exceeds-budget and retains a receipt
  with original archive SHA and measured section bytes above the cap.
  The existing tiny-archive test passes the cap and rejects a zero cap.
- `PATH="$PWD/.ctx/python-bin:$PATH" python3 scripts/tests/graph-release.py -v`:
  GREEN 25 tests, 6.846 s; exit 0. `.ctx/python-bin/python3` points to the
  installed Python 3.13 so subprocesses use a tomllib-capable interpreter.
- `python3.13 scripts/release/check-installed-graph.py --self-test`:
  GREEN 11 tests, 0.843 s; exit 0.
- Ruby Psych parses `.github/workflows/node.yml`; the extracted Windows
  archive step passes `bash -n`; `git diff --check` passes.

Intermediate unsuccessful attempts are not GREEN evidence: an uninitialized
C common symbol exposed no measurable sections, so the oversize fixture
uses initialized data; the default system Python 3.9 and nonexistent
Homebrew libexec path failed on tomllib before correcting the local PATH.
No production checker requirement was weakened to address either issue.

## Limits

This local archive regression verifies command wiring and budget rejection
on macOS, not Windows runtime or COFF archive qualification. PowerShell
execution and the exact Windows shipping archive remain mandatory checks
in the Node publication workflow under ZE-369. These synthetic checker
fixtures do not qualify a release or replace the actual ZE-421 installed
archive receipts. No engine, persisted format, C ABI, dependency or
architecture change. The final post-ZE-420 full suite, refreshed archive
pins, one-hour adversarial run and publication remain under ZE-369.
