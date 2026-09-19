# ZE-55: bounded Cypher profile binding

2026-09-19, macOS 27.0 arm64, Apple M3 Max (Mac15,9), 128 GiB RAM;
rustc/cargo 1.93.0 and cargo-nextest 0.9.145. Worktree base
`f9d81e0806ca68c876a26ed1983a3fffaf72b902`. This is focused component
evidence, with original scenario execution still pending.

## Implemented boundary

The outer frontend binds the complete accepted syntax inventory into a bounded,
source-preserving representation. It checks symbols, projection scope, aliases,
types, parameters, mutation properties, deleted-result restrictions and search
signatures/provenance before invoking its scoped consumer. It retains clause
order, OPTIONAL predicates, alternative relationship types, bounded per-edge
properties, all four search modes and complete hybrid output fields.

`compile_with` uses a higher-ranked callback: compiler backing cannot escape.
`compile_in` holds actual frontend capacities and a 64 KiB control/stack envelope
under the existing QueryMemory and shared GraphResources owner. Full replacement
capacity is reserved before Vec/String growth, including simultaneous old/new
backing, then reconciled to actual capacity. Movement and semantic walks poll.
The core's added grow-only QueryExternalReservation grants no address ownership,
alias credit or prepaid runtime certificate. Borrowed caller capacities are not
inferred. The independent resource review is in `ze-55/resource-review.md`.

The RETURN integration tracer copies reachable expressions and their backing
into real QueryArena owners while frontend storage remains charged, then uses
`GraphPlan::validate_with_fact_vec`. General read, mutation and search operator
lowering/execution remain ZE-56/57/58; binding does not acquire a writer, admit a
GraphStore, publish mutations or implement a public prepared-query API. The
source-indexed expression arena includes syntax holes; later lowering must
select/remap reachable nodes and validate its complete operator plan.

## Literal RED and terminal GREEN

The preserved logs contain named assertion failures before corrections, not
only successful syntax checks. Initial vertical cases include scalar literals,
unknown/surplus/duplicate parameters and variables, pattern origins, projections,
mutation/deleted results, search modes/eligibility, actual allocation overlap,
shared-budget denial, wildcard synthesis at the exact syntax cap, late callback
cancellation, and the real seeded-runner route. Tests verify typed observations
and zero consumer entry on rejection. No compiled statement owns a writer.

Root review reproduced comparison-nullability and numeric unary-kind disagreement
against the actual core validator. Comparisons now retain BOOL|NULL even when
their outer operands are nonnull; numeric unary facts retain only numeric kinds
and applicable nullability. A nullable deleted result preserves required dynamic
validation without rejecting the valid empty OPTIONAL result.

Independent ZE-120 review found three additional defects, each reproduced by a
named test before its correction:

| Named regression | Observed RED | Correction and GREEN assertion |
| --- | --- | --- |
| `computed_property_lists_preserve_possible_homogeneous_scalar_values` | `[1 = 1]` rejected as an invalid stored list | Intersect possible nonnull scalar kinds; seven computed/aliased positive forms bind, seven impossible/null/nested/entity forms reject before consumption. Actual values still require runtime property conversion. |
| `possibly_aliased_deleted_entities_keep_runtime_validation_after_projection` | Indexed results and distinct bindings omitted dynamic checks | Track any DELETE independently of known origins; retain checks for possibly aliased entities, indexed deletion victims, property/text access, nested lists and nullable bounded relationship lists. Counts and pre-copied property/text remain permitted without an invented entity-access error. |
| `order_expressions_resolve_shadowing_output_aliases_before_group_key_reuse` | ORDER BY `-x` reused old `-x` instead of negating the projected alias; swapped aliases chose the old name | Resolve output variables before structural grouped-expression reuse. DISTINCT/aggregate negation, swapped names and unshadowed `n.name` grouping reuse pass. |

The nullable bounded-list and origin-free DELETE refinements each had their own
observed assertion RED before GREEN. Some initial drafts had ordinary compile
errors; these are not claimed as the behavioral RED evidence.

Final focused checks use nextest's default four separate processes, retries zero
and one libtest test per process:

```sh
cargo nextest run -p zeppelin-embed-cypher
cargo nextest run -p zeppelin-embed --test graph_query_external_capacity
cargo nextest run -p zeppelin-embed-adversarial-oracle graph_binding::tests::binding_oracle_rejects_wrong_order_types_bits_modes_and_admission
cargo nextest run -p zeppelin-embed-workspace-tests --test adversarial_tests -E 'test(property_graph_binding_probe_preserves_profile_types_and_faults) | test(one_runner_episode_reaches_required_binding_contracts)'
cargo test -p zeppelin-embed-cypher --doc
```

Results: 46 frontend tests, one core capacity test, one independent oracle test
and two directed runner tests pass: **50 focused nextest tests**. The separate
compile-fail lifetime doctest passes. The frontend count includes 19 unchanged
parser regression tests; it is not 46 new binder tests. Scoped strict Clippy
passes for outer all-targets, core lib plus the new capacity test with
test-support, independent oracle all-targets, and the runner target, each with
`--no-deps -- -D warnings`. Workspace formatting and `git diff --check` pass.
The command array, exit codes and every final raw log are retained in the bundle.

## Fault controls, source inventory and fuzz boundary

PG11 uses an independent std-only primitive oracle and the real compiler. Four
seeds (0, 1, 55, u64::MAX) each cover 52 cases, two fired allocation/cancellation
faults and two clean controls. The actual runner seed 0 reaches all seven PG11
coverage keys with no violations. A missing runner call previously fired its
required-coverage assertion; the route is restored. The pre-change existing
runtime episode also passed; these directed episodes are not a broad campaign.

Five deliberate controls corrupt scope lookup, duplicate outputs, source copying
without charge, the independent oracle comparator and source error spans. Each
fired an assertion (nextest exit 100), then passed after exact source restoration.
`ze-55/mutations.json` records commands and restored SHA-256 values. Controls
whose source changed during review were rerun on the final source.

The tooling manifest keeps independent support/binding/execution axes. Its 268
rows in the exact 26 selected original feature files comprise 99 supported,
20 profile-rejected and 149 not selected scenarios. All 155 selected original
statements are byte-linked to the pin: 152 bind and three preserve compile
rejection; the fourth original expected error is runtime and remains pending.
All original execution states remain `unexecuted`, with zero conformance passes.
Four isolated-clone controls reject changed query bytes, modified source bytes
and the wrong source HEAD, and verify the clean inventory.

```sh
python3 scripts/cypher-binding-manifest.py /private/tmp/opencypher-graph-research-20260916
python3 scripts/tests/cypher_binding_manifest.py /private/tmp/opencypher-graph-research-20260916
```

Manifest SHA-256:
`160537cbca04274859f3b0230ac16cac121cb3039f99fb88050459d7110b6a69`.
The existing MIT/Apache notices and original parser fixtures remain unchanged.
The only added dependency edges are to approved existing internal crates;
Cargo.lock's package identities are unchanged. `cargo deny check` reports
advisories/bans/licenses/sources OK in the retained pre-review log.

The directed parser+binding fuzz smoke before the final review corrections used
seed 55, an eight-file 331-byte seed corpus and this target invocation:

```sh
cargo fuzz run cypher_parser /tmp/ze-55-fuzz-corpus -- -max_total_time=60 -max_len=65536 -seed=55
```

It completed 720,058 executions in 61 seconds, coverage 2,773, features 7,274,
final corpus 1,169/60 KiB and RSS 599 MiB, without a reported crash. This is a
bounded pre-review smoke, not a final fuzz campaign, memory-budget measurement
or execution oracle. The final semantic corrections have focused regressions.

## Retained evidence and remaining qualification

`ze-55/source-sha256.json` lists the 34 owned implementation/test/tooling files.
`source-audit.json` records the base, unchanged shared-file prefixes and dependency
inventory. `raw-logs.tar.gz` retains initial RED/GREEN, compiler drafts, negative
controls, command/exit records, terminal checks and environment observations;
`raw-log-sha256.json` verifies every member. Intermediate failures are preserved
as intermediate evidence and do not replace the terminal logs.

Whole-workspace/full-adversarial campaigns, original TCK execution, per-crate
90% coverage, platform/C/Swift parity and final footprint qualification remain
ZE-118/E12 and their owning downstream tickets. No graph-state effects, durable
reopen, completed result, public entrypoint, latency target or full-language
conformance is proved by this binder component.
