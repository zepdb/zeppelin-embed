# ZE-120 independent binder review

The three concrete findings below are resolved in frozen ZE-55 **review-7**.
No open finding remains in the reviewed binder scope. This is source/binding
and resource-seam evidence; it does not certify graph execution or main-branch
integration.

## Reviewed identity and boundary

The review worktree was created from main
`234dd4fe4ab45c55e16bf7866556e8b78f42ee8b`. The implementation candidate is based
on `f9d81e0806ca68c876a26ed1983a3fffaf72b902`. Only immutable source/test
snapshots supplied by its owner were copied; the implementation owner's
worktree, other workers and main were not modified.

All 34 files in each supplied snapshot were SHA-256 verified. The initial
[review-2 inventory](ze-120/review-2-hashes.json) has manifest hash
`f466d9058db10ed1d5ebf564fda2bc17e50e7f9401a578ce3a267062d3d641df`.
The final [review-7 inventory](ze-120/review-7-hashes.json) has manifest hash
`270f2a79a436fa20753f88985a097840d659621105bb5b6f9ba992b4eadfc14f`.
Local snapshots remain `/tmp/ze-55-review-2` and `/tmp/ze-55-review-7`.
Final candidate commit `5c9e757c05f977aa864a82fb3ab40630aff976b8` was then
independently checked against all 34 review-7 hashes using committed Git objects;
every file matches exactly.

The review read the accepted Cypher/execution plans, live ZE-55 acceptance and
its confirmed ZE-55 versus ZE-56/57/58 ownership boundary. It inspected every
new binder production module, the AST-facing representation, changed lexer
growth, frontend/shared resources, appended core reservation capability,
focused tests, manifest generator/controls, PG11 oracle/probe/registration and
fuzz changes. Existing parser/AST code was checked as an interface; this is
not a fresh exhaustive parser audit. No third-party dependency was added.

## Resolved findings

1. **P2: computed homogeneous property lists were rejected.** In review-2,
   `binding/mutation.rs::assignment` required exact scalar facts or two special
   broad unions. Thus `CREATE (n {p: [1 = 1]}) RETURN n.p`, `[n.x + 1]` and
   `[type(r)]` rejected even though their actual values can be valid homogeneous
   scalars. Facts describe possible types, so nullable/conservative facts are
   not proof of an invalid value. The correction intersects possible scalar
   kinds across all elements while retaining rejection when no homogeneous
   scalar type is possible. Actual null/mixed-value rejection remains the
   execution/property-conversion obligation. Three independent probes failed
   on review-2 and pass on review-7; candidate tests also retain known-invalid
   list cases and prove no consumer call after rejection.

2. **P1: dynamic deleted-entity validation was omitted.** In review-2,
   `binding/mutation.rs::check_deleted` treated different/missing slot origins
   as sufficient to avoid runtime validation. `RETURN refs[0]` can return a
   deleted entity, and separately matched `m` can identify deleted `n`.
   Review of the first correction additionally exposed optional relationship
   lists (`LIST|NULL`) and `DELETE victim` where the victim came from list
   indexing and therefore had no static origin. The final correction records
   every DELETE independently of the exact-origin set and conservatively
   carries the runtime-validation requirement for entity-capable results,
   access and lists. Exact origins still support provable compile rejection;
   copied scalars and counts retain their accepted boundary. Four independent
   probes failed on review-2 and pass on review-7. This verifies the required
   handoff flag, not the still-pending atomic runtime implementation in ZE-57.

3. **P1: grouped/DISTINCT ORDER BY used pre-projection aliases.** In review-2,
   `binding/projection.rs::projected_alias` structurally substituted a previous
   projection expression before resolving shadowing output names. For
   `RETURN DISTINCT -x AS x ORDER BY -x`, the order expression became just the
   projected `x` slot. Swapping `x AS y, y AS x` similarly made `ORDER BY x`
   select projected `y`. This can change ordering. The correction at
   `projection.rs:268` refuses that structural reuse when an identifier is
   available in output scope, allowing normal alias binding first. It retains
   unshadowed grouping-expression reuse such as `ORDER BY n.name` when `n` is
   no longer available. Both independent probes failed on review-2 and pass
   on review-7. Candidate controls also cover aggregation and unshadowed
   grouping. The source trail is pinned WithOrderBy4 scenarios 7–10, whose
   bytes match the pinned Git object ([identity](ze-120/order-source.json)).
   These are local adapted binding checks, not selected original-TCK passes.

The reproduction source contains the nine exact test names and queries:
[review_reproductions.rs](ze-120/review_reproductions.rs). Initial defect
locations are `mutation.rs:52`/`:98` and `projection.rs:248`; the final source
is identified by the complete hash inventory rather than moving line numbers.

## Independent execution evidence

Host: Apple M3 Max, arm64 macOS 27.0; rustc 1.93.0; cargo-nextest 0.9.145.
[Context](ze-120/review-context.json) records exact versions and bases. Tests
used nextest's four separate processes, zero retries, and one libtest thread
per process. No wall-clock or platform-portability claim is made.

| Check | Exact command from review worktree | Observed result |
|---|---|---|
| Initial nine reproductions | `cargo nextest run -p zeppelin-embed-cypher --test ze120_review --test-threads 4` | review-2: 0 passed, 9 failed, exit 100; all intended assertions |
| Terminal frontend plus review probes | `cargo nextest run -p zeppelin-embed-cypher --tests --test-threads 4` | review-7: 55 passed, 0 skipped, exit 0; 46 candidate tests plus 9 review tests |
| Core capacity capability | `cargo nextest run -p zeppelin-embed --features test-support --test graph_query_external_capacity --test-threads 4` | 1 passed, exit 0 |
| Non-escaping callback lifetime | `cargo test -p zeppelin-embed-cypher --doc` | 1 compile-fail doctest passed |
| Scoped lint | `cargo clippy -p zeppelin-embed-cypher --all-targets --no-deps -- -D warnings` | exit 0 |
| Pinned manifest | `python3 scripts/cypher-binding-manifest.py /private/tmp/opencypher-graph-research-20260916` | manifest SHA-256 `160537cbca04274859f3b0230ac16cac121cb3039f99fb88050459d7110b6a69`; 99 selected, 152 bound/3 compile rejections, zero executed |
| Manifest negative controls | `python3 scripts/tests/cypher_binding_manifest.py /private/tmp/opencypher-graph-research-20260916` | 4 passed |

Raw logs are [original RED](ze-120/all-nine-review2-red.log.gz),
[terminal GREEN](ze-120/final-frontend-green.log.gz),
[core capacity](ze-120/core-capacity-green.log.gz),
[lifetime](ze-120/lifetime-doctest-green.log.gz),
[lint](ze-120/focused-clippy.log.gz) and
[manifest controls](ze-120/manifest-controls.log.gz). The original replay was
followed by exact restoration of all 34 review-6 files before adopting the
verified review-7 changes; [receipt](ze-120/snapshot-replay.json).
An initial attempt to invoke the custom manifest controls with unittest
discovery failed before any tests because that script requires its source
checkout argument; the corrected explicit command above ran all four tests.

To reproduce the nine review probes on a candidate checkout, copy the supplied
Rust fixture to `crates/zeppelin-embed-cypher/tests/ze120_review.rs` in a scratch
worktree and run the first command. The evidence-only ZE-120 commit contains
no candidate production changes.

## Ownership, evidence and integration conclusions

No additional concrete issue was found in the reservation seam. The existing
[independent resource review](ze-120/prior-shared-resource-review.md) remains
applicable: frontend Vec/String replacement capacity includes old/new overlap,
actual capacities are reconciled, moves poll, and compiler backing drops before
the grow-only shared QueryMemory reservation. That guard grants no retained
address/owner credit. Source/parameters are synchronous caller borrows; later
execution must obtain actual owners or make charged copies. The checked
RETURN tracer copies actual expression/string/list backing into QueryArena and
validates an actual NodeFacts Vec while frontend storage still coexists.

The bound representation preserves full syntax and typed facts for subsequent
read/write/search lowering, including relationship type alternatives and
per-edge properties, path bounds, projection scopes, and search mode/YIELD/
eligibility distinctions. General GraphPlan lowering, physical admission,
result bags/order, mutation effects and original-TCK execution remain
ZE-56/57/58/59 and downstream owners. Binding success and the directed tracer
are explicitly separate from those claims. The manifest retains 268 rows:
99 supported, 20 rejected-profile, 149 not-selected, all execution-unexecuted.

The candidate predates ZE-37. Main integration must preserve both PG10 staging
and PG11 binding registrations/tests and both core CLAUDE invariant sections;
whole-file snapshot replacement would lose the newer main additions. Root
owns that additive integration and its focused checks. Full workspace/TCK/
adversarial/coverage/release qualification remains ZE-118 and is not claimed.
