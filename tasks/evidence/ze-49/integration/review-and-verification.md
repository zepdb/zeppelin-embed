# ZE-49 main integration

Source candidate `68f2d7d90c0418d4c599cb4237fc43ca5616d0bc` was individually
cherry-picked onto main after ZE-38 `34b59f5748fcb5f9691f2f8e1270fa0196bd451e`.
Root reviewed the frozen runtime/resource/input ownership and narrow existing
plan/accounting changes. The unused unbounded arena grow method was removed and
private String cells preserve once-validated UTF-8 through exact copies. No
production behavioral change was needed during integration.

Conflicts preserve five complete pre-integration file prefixes and append both
WAL and runtime probes/declarations. Nineteen candidate source/test files remain
byte-identical; all 30 candidate evidence files and 45 inherited user files are
unchanged. The allocation helper re-export was moved immediately before its
existing test module to satisfy the broader scoped all-target strict Clippy
check; no helper or test body changed. Both its initial diagnostic and successful
check are retained. The new runner test's formatting correction is also recorded.

Focused main nextest: 47 core/value/plan/runtime/allocation tests, four existing
accounting tests and three independent-oracle/probe/actual-runner tests pass
(54 unique tests). All-target strict Clippy across core/oracle/workspace-tests
with allocation-audit/test-support passes, as does whole-workspace fmt check.
A targeted allocator test also rechecks the unchanged export after its move.

The added actual-runner test failed when only the PG9 probe invocation was
removed, then passed after byte-exact restoration: seed0, 59 operations, zero
violations and all six runtime coverage keys. PG9's directed probe reports four
cases, two scheduled faults and two same-seed clean controls. Candidate five
mutants and resource/close controls retain their own evidence. No mutant remains.

Whole-driver allocation audit separately measures 48 bytes/two graph-attributed
allocations and 112 bytes/two existing cold-lease allocations. The latter is
reproduced by a separate Store with no graph calls, followed by a zero-allocation
second lease drop. It is not hidden by warming the runtime or claimed accounted.

This is component implementation/verification, not GraphStore admission, actual
record/result representation, final per-crate coverage or release/platform
qualification. Broad workspace/adversarial/coverage remains ZE-118/E12 after code
completion. No new dependency or push. Exact commands, original diagnostic exits,
compressed raw logs, merge patch and preservation audit are adjacent.
