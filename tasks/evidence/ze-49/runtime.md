# ZE-49: bounded flat runtime and actual resource ownership

Candidate worktree: `codex/ze-49-query-runtime`, based on
`77695e0461ca8e7d37c68fff7fe83e80188b4923`. The ticket remains in progress until
root integrates the individual commit. Exact commands are in `commands.json`;
raw selected logs are in `logs/`, with the complete local run material retained
at `/tmp/ze-49-evidence`. Source hashes are in `source-inventory.json`.

## Scope and accepted seams

This implements the execution/resource foundation, not pattern/relational/search
operators or public GraphStore admission. `GraphResources` borrows the existing
Store Accounting authority; its peak is the real monotone reservation maximum.
`QueryMemory` enforces 24 MiB within that shared owner. Fixed QueryArena storage
reserves before `try_reserve_exact`, reconciles actual capacity and releases
backing before its charge. There is no implicit growth or bulk-growth method.

Actual retained Vec/String/Box/array owners and same-query arenas establish input
capacity. Aliases charge once; no spare bytes are read. Missing spans and foreign
owners fail. Runtime plan admission requires the private full-capacity facts Vec
certificate, not a raw-slice declaration. Plan/runtime descriptors and the 64 KiB
validation scratch are simultaneously charged. Writer/core/ABI buffers can move
one authentic aggregate reservation into an immutable joint query-local owner;
failure returns the original charge. It does not expose a forged prepaid pointer.

A required RetainedView adapter supplies the same lease through pulling,
completion and the final close-first control check. Public construction of
QueryView remains no admission or entity-existence proof. The driver runs every
source-order eager obligation before rows (including LIMIT 0), rejects empty
More, copies complete bags privately, checks collector row/byte limits before
copying, and returns no rows on any error. Completion output cannot borrow the
fresh view/row lifetime and is dropped if final checks fail. There is no new
worker pool and no durable publication in this preparation driver.

Logical prepared payload is reported separately from represented completion
bytes. Internal Completion supplies initialized core/ABI representation metadata;
each representation has a 4 MiB cap including its in-arena descriptors. Actual
retained capacities and registration/control overhead are additional nested
charges. ZE-52 still owns actual copied entities/representation and application
result accounting. ZE-51 owns the immutable EligibleNodeSet and its 524,288-entry
cap; retrieval borrows it. No memory or liveness claim is delegated to a raw
PlanFootprint declaration.

## Literal RED and GREEN

All initial seams were introduced vertically with named missing-interface REDs:
shared accounting, arena ownership, retained inputs, runtime counters, real plan
owners, flat scalar/variable batches, joint reservation transfer and pull/freeze.
Complete first-cycle logs remain in `/tmp/ze-49-evidence/red-*.log` and matching
`green-*.log`. The following observed contract failures were then corrected:

| Named assertion | Observed RED | Correction and GREEN |
|---|---|---|
| `retained_input_capabilities_charge_spare_capacity_once_and_reject_missing_spans` | 1560 charged versus 1496 expected | Removed duplicate embedded region-arena descriptor charge. |
| `failed_variable_reservation_does_not_count_uncopied_bytes` | CopiedBytes 5 with zero bytes copied | Check actual fixed capacity before copy/work accounting. |
| `execution_plan_requires_retained_owners_for_every_visible_span` | Descriptor delta 0 versus 128 before adding its own guard | Reserve actual plan/runtime descriptor storage together with validator scratch. Full spare facts capacity is retained; raw-slice facts cannot upgrade. |
| `completed_row_limit_refuses_collection_before_copying_the_unconsumed_row` | 32 copied bytes instead of the 24 source bytes when completed-row limit is zero | Check upcoming row capacity before collector copying. |
| `prepared_byte_limit_refuses_collection_before_copying_the_unconsumed_payload` | 32 copied bytes instead of 24 with prepared-byte limit zero | Store exact per-row logical payload and check before copying. |

PG9 first failed on its missing probe interface, then passed against an independent
std-only primitive bag/work oracle. A seeded 11-row fixture includes duplicate
values, signed extremes and I64 >2^53. The oracle checks exact multiplicity and
three actual eight-byte copies per retained integer. Two real scheduled deadline
faults fire once each: during a source pull after eight copied bytes, and at the
final check after all three copy passes. Same-seed clean controls complete; every
fault leaves zero output and restores reservations. Registry/runner wiring includes
six explicit `property-graph.runtime.*` paths. This is component proof, not
GraphStore, retrieval, language or durable-write conformance.

## Directed negative controls

`mutants.json` records five actual intended test failures and SHA-256 equality
before/after exact restoration: omit final checkpoint; omit eager obligations;
use facts length instead of capacity; bypass the work cap; suppress real shared
peak accounting. Each nextest run exited 100 at the relevant named assertion,
not at compilation. Terminal checks run after restoration. The source inventory
also records subsequent review corrections; the mutant hashes identify the exact
snapshot on which each control was exercised.

The allocation-audit helper refuses a selected real attributed GlobalAlloc call,
not merely a budget preflight. Initial and replacement Vec allocations each fire
once and return Allocation; old fixed arena contents/capacity remain intact and
new reservations release. A same-size clean allocation reports exactly one
256-byte attributed allocation. An explicit 37-byte unaccounted Vec proves the
attribution detector can disagree.

The full drain/freeze audit initially exposed 112 unattributed bytes. An independent
cold Store::snapshot/drop with no graph calls reproduces exactly two allocations,
112 unattributed bytes; a second lease drop has zero allocations. The unchanged
`lifecycle/snapshot.rs` SHA-256 is
`1ea2119e91dfc00767249a4d84d12662a9ea8472df846f58c71a01d846447e05`, identical
to the base. Source inspection points to first-use platform mutex/condition-variable
backing in existing ReaderRelease; this is an inference, not an allocator stack
trace. Root explicitly accepted separating this pre-existing baseline rather than
broadening lifecycle work or warming it away. The committed runtime audit measures
a separate cold Store each time and requires exact equality of that baseline;
it additionally pins the new runtime's two attributed offset allocations, 48 bytes.
The raw cold diagnostic and the deliberately failing zero-baseline assertion are
retained. No claim is made that legacy first-use allocations are accounted.

The actual 8 MiB packed-ID fixture retains and charges both the input Vec and owned
query arena, stays below 17 MiB query peak, preserves IDs above u64, and rejects
copying that one value into a 4 MiB prepared-result payload with no exposed rows.
Byte-copy cancellation after 131072 bytes rolls back all cells; its clean control
copies 131073 bytes. UTF-8 crosses the 64 KiB boundary inside a three-byte character.
Close tests use a real SnapshotLease, a close thread and condition-variable wait,
not sleep/poll timing. They cover close after the last pull and during completion,
prove close precedes caller cancellation, discard output, and unblock drain.

## Independent source review

Root reviewed frozen `/tmp/ze-49-review/hashes.json` and found two concrete issues:
unused QueryArena::grow moved a potentially large Vec without control; accessing
String cells rescanned full UTF-8 without control. The unused growth interface was
removed. String cells are now created only after exact complete copies of valid
&str bytes; private range checks and immutable borrowing preserve that invariant,
so a documented unchecked conversion avoids an unpolled scan. Root reviewed the
exact corrections and the cold-baseline distinction and reported no remaining
concrete source blocker. Scope/lifecycle/representation limits above remain explicit.

## Qualification boundary

Host: Apple M3 Max, Mac15,9, 16 CPUs, 128 GiB RAM, macOS 27.0 (26A5388g), arm64;
rustc 1.93.0 / LLVM 21.1.8. Data are deterministic tiny typed fixtures, one packed
524288-ID boundary list and controlled byte strings; no external dataset/network.
There are no throughput, minimum-OS, Windows, Intel, public result or release claims.

Owner-directed nonessential broad/full workspace, full adversarial and per-crate
coverage work remains ZE-118 in E12. This ticket does not claim module-only or
focused evidence establishes 90% whole-crate coverage. No threshold/exclusion or
persisted format changed, and no dependency was added. The pre-existing self-dev-
dependency duplicates the allocator if allocation-audit is used on a lib-test
binary; focused integration binaries exercise one real allocator instead. That
configuration failure is logged and is not presented as an executed test failure.

## Terminal focused results

- Core runtime + allocation audit + affected ZE-48 values/plans: **47 passed,
  0 failed**, five integration binaries, 0.148 seconds reported execution.
- Existing affected lifecycle accounting: **4 passed**, 572 skipped, 0.018 seconds.
- PG9 oracle/probe and collector preflights: **4 passed**, 535 skipped,
  0.016 seconds; probe prints cases=4, fires=2, clean_controls=2.
- Post-review correction controls: **4 passed**, including full cold allocation
  distinction and UTF-8 crossing the copy boundary.
- Scoped strict core/allocation and oracle/runner clippy: **passed**.
- `cargo fmt --all -- --check` and `git diff --check`: **passed**.

Execution times are raw nextest supporting measurements, not performance gates.
The integration owner will record its cherry-pick and focused main checks before
closing ZE-49. No full-suite or whole-crate coverage claim is made.
