# ZE-127 / PG15: owned native completed values

Implemented on main base `0fff7f4d92843c207251a152b4dff26da3198966`, in the
isolated `codex/ze-127-owned-results` worktree. All 41 inherited file hashes remain
unchanged. There are no dependency, lockfile, public C ABI or main-branch edits.

## Component and accepted ownership seam

`query::completed` validates and copies twelve typed native pools using real
QueryArena backing. Final values hold exact scalar bits, full native identities,
keys/revisions/generations, original relationship endpoints/types, optional
selected text/vector payload, tagged stored lists and heterogeneous query lists.
Cells preserve bag duplicates; copied entity pools are deduplicated/ordered by
full ID, independently of row order. Postorder list references reject cycles;
depth and expanded descendant checks include repeated children.

The builder checks exact admitted token identity, complete geometry, referenced
UTF-8, labels/properties, all entity and list pools, receipt order and report
consistency. It enforces 65,536 rows and a complete 4 MiB represented result
including the result descriptor. Genuine capacity, control and scratch charges
remain within both the query allowance and shared aggregate. Copies poll in at
most 64 KiB chunks and charge actual CopiedBytes; the existing driver remains the
single authority for completed rows/core/ABI counters.

Each write Receipt retains an authenticated item ordinal and deleted flag as
well as the coordinator's original ItemReceipt. Search reports retain separate
requested/actual route, coverage/precision, epochs, empty-leg state, alpha,
normalization/rules versions, counts and work. Contradictory mode/route/leg states
reject; empty precision is never inferred from counts.

PreparedGraphResult exposes immutable pools and stable rows/generation/outcome.
Pre-detach counter/peak fields are unfinished. Native-to-C conversion must use
this prepared owner while its actual charges and producer backing remain live.
Detach consumes the same twelve Vec allocations, releases authentic temporary
guards once and accepts the final driver's counters/peak. It allocates/copies
nothing and returns a lifetime-free immutable CompletedGraphResult. Its slice
lengths do not prove full backing capacity for subsequent query adoption.

The resource change is an eleven-line `pub(super)` detach method restricted to
the private owned native Copy + 'static set. Arbitrary borrowed QueryValue cannot
enter it. [Compiled interface](ze-127/owned-contract-proposal.md) and
[independent review with cleared findings](ze-127/independent-api-review.md)
record the reviewed API. All eight review-2 snapshot hashes were verified by the
independent reviewer; that partial-snapshot review claimed no independent build.

## Literal RED, GREEN and controls

The raw logs are preserved byte-for-byte in `ze-127/raw/*.gz`; the adjacent raw
manifest records both uncompressed and compressed SHA-256. Test commands, final
run IDs and ten restored source controls are in `ze-127/final-checks.json`.

Observed intended runtime REDs before the corresponding changes:

- `malformed_lists_and_ranges_never_expose_partial_owned_results`: accepted a
  self-referencing list before geometry validation (07/08 logs).
- `malformed_entity_metadata_and_unreferenced_pool_entries_reject`: accepted a
  relationship key on a node before record validation (09/10).
- `report_geometry_rejects_wrong_generation_call_and_nonfinite_policy`: accepted
  a foreign-generation report before report validation (11/12).
- `one_runner_episode_reaches_completed_owner_controls`: PG15 registry was not
  reached until the actual runner invoked the probe (19/22).
- `complete_reports_reject_contradictory_kind_route_and_modality_states`:
  accepted lexical execution with no requested lexical leg (23/24).

Missing module/record/receipt APIs produced compile REDs (01, 05, 20). Logs 02,
03 and 26-capacity-harness-compile record corrected test harness mistakes, not
product RED evidence. The first capacity mutant removed only the initial query
check; the independent reconciliation check still rejected it. This successful
redundant-guard control is retained in 26-redundant-query-guard-control and is
explicitly not counted as a caught mutant. Removing both guards then failed the
literal expected-rejection assertion.

Ten planted source defects each exited nextest 100 at the intended assertion
or actual allocator denial; exact original source hashes were restored:
foreign-token admission bypass, 8 MiB represented cap, omitted expanded bag
count, guard-before-buffer drop order, lost byte owner, allocating byte detach,
missing copy charge, omitted query reservation/reconciliation caps, omitted
shared reservation/reconciliation caps, and allocating vector detach. The two
allocating-detach controls abort their isolated nextest child on the explicitly
denied 3-byte/4-byte allocations. Reproducers: `ze-127/mutate.py` and
`ze-127/capacity-mutants.py`.

Terminal focused checks on restored source, using four isolated nextest workers
and zero retries:

| Check | Result | Run ID |
| --- | --- | --- |
| Native component tests | 15 passed | bb066e48-4523-416c-9fe1-57567f0ebb92 |
| Same component with allocation-audit feature | 12 passed | 338e23bb-74d6-4384-b71e-7c53b33a07f8 |
| PG15 probe and actual runner route | 2 passed | 97c86436-782a-4993-b7bd-d4098c3cd634 |
| Native and runner strict Clippy | Both passed | exact commands in final-checks.json |

The three custom actual-System-allocator tests compile in the default feature
build; allocation-audit already supplies a global allocator, so that feature
build intentionally excludes the competing custom hook. Default tests deny all
five observed scalar allocations and all twelve observed allocations in each
full read/write fixture (eleven nonempty typed pools plus validation scratch).
They verify buffer release before accounting guards, no partial owner, flat
32 scalar and 8-per-mode complete allocation/free loops, exact original vector
bits, and zero allocator attempts with unchanged pointers for all twelve pools
through consuming detach. Real retained Vec capacity forces query and aggregate
failures while the represented payload remains below 4 MiB. Both counters and
actual shared/query reservations return to the starting values.

PG15 uses primitive literal row expectations outside the native builder. Seeds
0, 1, 127 and u64::MAX each produce 12 comparisons, 2 real fires and 3 same-seed
clean controls. Corrupting high ID bits, scalar bits or bag multiplicity is
rejected. Actual CopiedBytes exhaustion and actual cancellation invoke the
production builder; seven required registry keys are reached by the seeded
runner. One real runner episode passes; no full campaign was run.

## Environment and retained acceptance boundaries

Apple M3 Max, 128 GiB RAM, macOS 27.0 arm64 (26A5388g), Rust/Cargo 1.93.0,
nextest 0.9.145. `raw/environment.json.gz` records exact version commands and
outputs. Inputs are the bounded literal and seeded fixtures named above;
there is no corpus benchmark or wall-clock performance claim.

This is a production native ownership component. Synthetic entity producer and
retained-view adapters are explicitly not real GraphStore admission, graph
entity reads, mutation commit, or graph close/reopen/relocation acceptance.
ZE-53 retains actual admitted base/overlay conversion and lifecycle execution;
ZE-64 retains ranking/report truth; ZE-68 retains native-to-C conversion, complete
source/core/C overlap and real coordinator commit-window faults. ZE-128 owns
aligned C storage; no C layout is cast from these native Rust records.

Nonessential full workspace/adversarial/coverage/sanitizer/qualification work is
deferred through backlog ZE-118. These focused checks do not claim crate-wide
coverage, TCK, recovery, release or cross-platform qualification. Follow-up broad
commands are `cargo nextest run --workspace`,
`cargo nextest run -p zeppelin-embed-workspace-tests --test adversarial_tests`,
and `scripts/coverage.sh`, pinned to the final
integrated ticket commit by the coordinating root.
