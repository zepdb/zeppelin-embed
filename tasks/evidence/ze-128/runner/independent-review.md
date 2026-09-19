# ZE-128 PG16 independent follow-up review

Disposition: **no concrete blocker found in this bounded follow-up**. The prior
aligned-owner review remains intact. This does not accept native conversion,
actual commit/public C behavior, packaging, or a broad campaign.

Reviewer: `/root/ze119_capacity`, delegated by root. No repository source,
tracker, index or commit changes; no new ticket/agent and no broad suite.

## Exact reviewed scope

Frozen snapshot `/tmp/ze-128-pg16-review-1`, based on
`f1c4d20dd4086a77ca7fdfbe7276311ea796833a`. Manifest SHA-256:
`1d0dad808fe1aeddec88b251508f9b57b1b57fa0dbb521d179d6a4817536b9a9`.
All twelve file hashes and byte lengths verified, then reverified after review.
`/tmp/ze-128-pg16-independent-checks.json` retains the complete source inventory.

The delta is the two allocation-site wrappers/controller, primitive oracle,
runner adapter/route/coverage, feature definitions and directed tests. I also
read the immutable baseline FFI module gate and the oracle Cargo manifest.
The oracle package has no dependencies, and its new module imports neither
engine/FFI types nor production layout, registry, or outcome logic.

## Findings and verification

- Both genuine allocation sites call `allocate_raw`: the nonzero padded arena
  and the actual intrusive registry node. Zero-byte arenas still skip allocation.
  Reservations precede the same sites and original typed null-allocation error
  paths/cleanup remain unchanged. The wrapper calls the real system allocator
  when unarmed. Explicit scheduled refusal is correctly described as site
  injection, separate from the original actual-System-allocator-denial evidence.
- The controller uses const-initialized thread-local Cell state. Its guard is
  calling-thread-only, restores the previous state on drop/unwind, and records
  only sites actually reached. It records the refused ordinal once, rather than
  equating configuration with a fire. The probe asserts exact counts for arena
  and node faults and two matches/zero fires on the corresponding clean path.
  The unwind test leaves ordinal one armed before unwinding and performs real
  preparation afterward; leaked scope state would refuse that operation.
- The canonical `run_program_for_with_clock` invokes the probe and propagates
  failure. The twelve required smoke keys, module, route and directed tests share
  the workspace-test `graph-cypher` gate. Coverage is awarded only after the
  primitive oracle or explicit actual-operation assertions pass. The default
  key-absence test prevents adding unreachable PG16 requirements to legacy runs.
- Primitive observations check integer and exact UTF-8/NUL bytes, root counts,
  alignment, owner states, returned empty root, refund, race winner count and
  known outcome. Expected values are literal or seeded independently of owner
  codecs/validators. Controlled private/forged/stale frees, abort, publication
  versus free and two free contenders call the actual owner. Outcome checks
  invoke the actual cell but explicitly do not claim a durable commit.
- Allocation receipts are controller observations; cancellation fires are actual
  token-trigger counts. Memory/work/registry "fires" are observed exact typed
  refusals, not allocator event receipts: the adapter maps the concrete error and
  the oracle requires the intended kind, restored charge and identical-input
  clean completion before credit. An unrelated rejection cannot earn coverage.
- Feature direction is one-way: FFI `graph-result-test-support` enables graph;
  FFI `graph-cypher` alone does not enable test support. The module and hook branch
  have explicit test-support cfgs. Workspace runner feature selection alone adds
  the hook. No new dependency, C export or generated-header change is in scope.
  The supplied locked/offline Cargo resolution confirms default and ordinary
  graph modes exclude the hook, while runner mode includes it. A copy is retained
  at `/tmp/ze-128-pg16-reviewed-feature-matrix.json`, hashed in the check record.
  Preexisting workspace `abi-panic-probe` is unrelated to this delta. This is
  source/configuration isolation, not a packaged-symbol or release-size audit.

## RED/GREEN evidence audit

Both mutation records have nextest exit 100, expected runtime failure text, and
original/restored SHA-256 equal to the frozen source manifest. Removing the real
route rejects missing `property-graph.response.aligned-owner`; suppressing fire
recording preserves the actual allocation refusal but rejects
`matches=1 fires=0 charge_restored=true clean=true`. These prove route and receipt
checks can fail. The controller is not credited merely because allocation failed.

Owner terminal logs show four seeds (0, 1, 128, u64::MAX), each with 16 cases,
6 fires and 6 clean controls; actual seed-128 runner has 58 operations and zero
violations. Nextest run `66b55c96-69e6-47bd-a78c-ecf192a8c896` passes both directed
checks. Feature-off, 13 original owner regression tests, primitive-oracle test,
default/graph-only checks, scoped strict Clippy and formatting are separately
recorded. The retained-context setup failure is disclosed as a fixture error,
not product RED. No full campaign or ignored release case is claimed passing.

I independently compiled the frozen oracle module directly with rustc and ran
its single named negative-control test: one passed, zero failed/filtered.
This additionally verifies the module needs only std. The exact commands and
result are in `/tmp/ze-128-pg16-independent-checks.json` and the output is
`/tmp/ze-128-pg16-oracle-review.log`. An initial exact filter omitted `tests::`
and selected zero tests; that miss is retained separately and not counted.
All engine/runner results above are inspected owner evidence, not independent
reruns. No moving production source was used for this review.

## Retained acceptance boundary

The adapter uses synthetic typed C pools, real Store accounting/SnapshotLease,
and a fixture QueryView token. This does not prove GraphStore admission, native
producer ownership or complete native/C overlap. ZE-68/69 retain real conversion,
coordinator outcomes, commit-window denial, Busy/Poisoned/free policy, exports
and graph close/reopen/lifecycle acceptance. PG16 runs under the selected nextest
process isolation; this review does not qualify parallel same-process probe
invocations or a broad scheduler campaign. Broad qualification stays ZE-118.
