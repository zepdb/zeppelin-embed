# ZE-128 combined main integration

Initial source f1c4d20dd4086a77ca7fdfbe7276311ea796833a was individually
cherry-picked as e015b3018326ddec0210d90859444321abd87838. All48 initial
paths were byte-exact. The initial combined-main check passed49 focused
owner/ABI/header tests with16 skipped (14 unrelated lib cases and two existing
ignored release archive gates). Initial logs/inventory are preserved here.

The ticket remained open because a new canonical seeded-runner route was
still required. Followup bf463537bcf18ed8977f8482c3e727eb9dd5c33e adds PG16
and an explicit nonshipping allocation-site controller. All40 followup paths
were audited:34 byte-exact; six shared Cargo/module/coverage/runner files retain
whole prior main content plus exact additions. The initial production changes
outside the two explicit allocation-site wrapper files remain byte-exact.
All45 inherited current file hashes are preserved.

Root read the frozen followup controller, complete independent primitive oracle,
actual operation adapter, coverage/feature wiring and negative controls. The
independent reviewer found no concrete blocker and separately compiled/ran the
std-only oracle test. Its report/checks are committed in ../runner. The two real
allocation sites retain prior reservations and typed failure cleanup; fault
receipts count reached sites/fires, not requested schedules. Scope drop restores
thread-local state through unwind. The original actual System-allocator denial
and no-allocation expose/free proofs remain separate from site injection.

Final combined-main tests pass17 checks:13 owner tests with explicit test hook,
2 PG16 directed/actual runner tests,1 independent oracle and1 default feature-off
registry test. All four PG16 seeds report16 cases/6actual fault fires/6same-seed
controls; actual seed128 runner reports58operations/0violations. Strict scoped
FFI and runner Clippy pass; ordinary shipping graph feature compiles. Locked
Cargo resolution confirms default and graph-only FFI omit test support while
opt-in workspace runner includes it. Exact commands/counts/logs are recorded;
no zero-selection command is counted as proof.

PG16 compares exact primitive bytes/integers/root geometry, private/forged/stale
ownership, cleanup, one-winner races, and known outcome preservation. Memory,
work and registry fault observations are exact typed refusals paired with clean
identical-input completion; only allocation has allocator-site receipts. The
outcome cell fixture does not establish an actual durable commit.

ZE68/69 retain authentic native preparation/source/core/C capacity overlap,
coordinator outcome/commit-window proof, Busy/Poisoned policy, public exports,
free and real graph lifecycle. Feature-source isolation is not release packaging
qualification. Broad graph-feature runner/workspace/per-crate coverage and the
two ignored archive gates remain ZE118, with final shipping matrix owned ZE107.
