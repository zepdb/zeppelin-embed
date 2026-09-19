# ZE-126: complete scoped Cypher read lowering

ZE-126 turns the final BoundQuery into a complete validated native nonsearch read plan. The implementation copies all retained source, names, expressions, parameters, operators, source maps, columns and facts into actual charged ownership. The HRTB consumer receives the same sealed ValueContext or RuntimeContext used for preparation. A retained runtime can admit these authentic owners and run the existing drain without resetting value work, runtime counters, cancellation or close checks. CALL is explicitly refused by this read-only seam with SearchContext; its bound search metadata remains available to ZE-58.

Base main was `736e8440221d4b53d797637e61840b53d0cdc7c7`. Root authorized the immutable ZE-125 implementation import, resulting in parent `beb1a1e44730b56494ec2cb4aafc5d9ea5419763`; only this ticket's subsequent individual commit is intended for cherry-pick. No new dependency or lockfile change. The 36 source paths are enumerated in `owned-source-allowlist.json` and hashed in `source-hashes.json`. The 39 inherited instruction/context files and the CLAUDE/tracker symlink roles were preserved; `inherited-final.json` verifies the file hashes and canonical tracker target. Older worktrees were not modified.

## Implemented contracts

MATCH/OPTIONAL retain directions, exact OR names, finite path bounds, public LIST relationship bindings, node/relationship reuse by equality and reprojection, one PatternId per MATCH including comma parts, and a genuinely correlated optional DAG. Complete optional predicates remain before null extension. Projection preserves textual column order, aliases, wildcard scope, aggregate grouping, DISTINCT, hidden ordering inputs, explicit WITH barriers and checked literal/parameter SKIP/LIMIT. Sparse and synthesized scalar IDs are remapped into a compact copied DAG; all accepted scalar operation families and exact UTF-8 source/name/parameter bits are retained.

Independent review exposed one missing native contract: the binder accepts `MATCH (a)-[r*1..2 {x:size(r)}]->(b) RETURN r`, but the prefix edge predicate correctly cannot see the not-yet-produced list. Root approved a distinct `CompletedEdgePredicate` on BoundedExpand. It sees incoming bindings, the complete public LIST and one fresh private REL; the fresh destination is unavailable. Every member must pass, zero-hop evaluates nothing, false/null rejects only the candidate, and checked work/control errors abort without partial output. The entire inline bag uses this stage if a transitive RHS reads the newly produced list. Ordinary prefix predicates are unchanged. Root recorded the exact contract in canonical `docs/graph/plans/parallel-contracts.md` and `execution.md`, and explicitly blocked actual traversal ZE-50 on ZE-126. The compiled field contract is in `query/plan/mod.rs`; actual traversal evaluation remains ZE-50.

A second review finding was a potentially multi-MiB unpolled UTF-8 rescan. The existing catalog bounded validator was extracted into safe `property_graph::checked_utf8`, preserving catalog checkpoint/error behavior. It validates at most 64 KiB per window, carries split codepoints forward and makes one documented unchecked conversion only after full validation. The outer compiler continues denying unsafe code. This helper creates no allocation owner or alternate control account.

The contributed `QueryArena<NodeFacts>::validate_plan` capability comes from the actual full fact capacity and QueryMemory identity, avoiding duplicate fact charges. `execute_in` forwards the unchanged existing drain. Root independently reviewed those four contributed files; the contributor independently reviewed only the frontend and UTF-8 delta afterward. These roles are recorded in `core-adapters/` and `reviews/`.

## Literal RED and GREEN

All original log/patch bytes, including harness mistakes and intermediate failures, are retained in `raw-evidence.tar.gz` and pinned by `raw-evidence-hashes.json`. Readable `.log` files only remove trailing whitespace and final blank lines; the contributed patch is in the raw archive. No earlier failed run is presented as terminal success.

| Change | Observed RED | Subsequent proof |
| --- | --- | --- |
| Scoped API and scalar plan | `01-scalar-red.log`: missing compile_read_in; `02-expressions-red.log`: pending scalar lowering | copied projection, sparse scalar DAG and owner admission GREEN |
| Patterns, ordering, parameters | `06-pattern-red.log`, `08-projection-red.log`, `11-parameter-red.log` | optional correlation/OR/edge predicates, grouped/hidden order, exact nested parameters GREEN |
| Actual fact owner and same runtime account | `core-adapters/01-missing-api-red.log`; isolated credit-removal and context-reset mutants `05`/`06` | real full-capacity identity, cumulative work/exhaustion, final close/output release GREEN |
| Completed path dependency | `20-completed-edge-api-red.log`; `24-completed-lowering-red.log`: original accepted query returns Plan(Scope) at 0..0 | same query, transitive/mixed dependencies, all-bag routing, exact size(r) span and private/list/target scopes GREEN |
| Bounded UTF-8 | `22-utf8-api-red.log`; independent wire-check bypass produced polls 0 instead of 3 | exact restoration, split 4-byte codepoints, malformed/truncated/overlong/surrogate/range errors, three cancellation windows and large copied parameter GREEN |
| PG17 | planted whole-plan/output/predicate omissions all rejected; actual scheduled faults fire | five independent primitive plan recipes; same-seed clean controls; actual runner reaches all coverage keys |

The independent UTF-8 reviewer reran the real generic API lifetime probes: returning an owned usize compiles, returning LoweredRead fails with both plan and fact lifetime escapes. `reviews/lifetime-after-utf8.json` has complete rustc arguments and output. The fact capability's own E0499 rejection is separately recorded in `core-adapters/08-facts-lifetime.log`.

## Terminal focused checks

Host is macOS 27.0 arm64, Rust 1.93.0; exact environment is in `environment.json`. Dataset consists of literal compiler plans and controlled real query/store owners, not a product graph benchmark.

```sh
cargo nextest run -p zeppelin-embed-cypher --lib \
  --test read_lowering --test runtime_lowering --test lowering_allocation \
  --test lowering_semantics --test pattern_contract --success-output final
```

`logs/43-terminal-frontend.log`: **29/29 pass**. The literal runtime tracer retains prior value work 11; this raw run prepares at 294 and completes at 300, an exact execution delta of six (three producer comparisons, two actual row copies, one completion comparison). Preparation work can differ across processes because real address-order sorting/binary search consume variable work; the independent run records 301→307. Neither trace resets its prior account. Near-limit execution refuses at the original 8,000,000 work limit. Authentic facts avoid exactly 4,144 duplicate bytes in the two-operator tracer; retained inventory admission costs 272 metadata bytes. Actual close plus simultaneous caller cancellation wins in the parser and after real completion; the latter discards a charged output and releases all owners.

The allocator test observes **253 actual allocation sites**, fails each in turn and verifies typed allocation refusal, no consumer entry and complete release, then restores a clean run. Actual live heap peak is **48,085 bytes**, same-query reservation peak **275,857 bytes**, and terminal reservation **56 bytes**, exactly the pre-invocation QueryMemory baseline. The allocator test uses its own observer and must run without core `allocation-audit`; `logs/35-final-frontend.log` retains the rejected attempt to enable both global allocators.

```sh
cargo nextest run -p zeppelin-embed --features allocation-audit,test-support \
  --test graph_query_plan --test graph_checked_utf8 --test graph_catalog \
  --test graph_compiled_context --test graph_completed_predicate \
  --test graph_query_runtime --test graph_query_runtime_control \
  --success-output final
```

`logs/36-final-core.log`: **65/65 pass**, including original catalog behavior, private completed-edge scope at zero and positive bounds, existing runtime ownership/controls and the contributed adapters. The independent semantic probes are retained in `tests/lowering_semantics.rs` and core `tests/graph_completed_predicate.rs`; only provenance comments and rustfmt differ from the reviewer sources.

```sh
cargo nextest run -p zeppelin-embed-workspace-tests --test adversarial_tests \
  -E 'test(property_graph_lowering_probe_checks_complete_plans_and_inflight_faults) | test(one_runner_episode_reaches_required_lowering_contracts) | test(property_graph_binding_probe_preserves_profile_types_and_faults) | test(property_graph_relational_probe_preserves_order_bags_and_inflight_faults)' \
  --success-output final
```

`logs/39-final-runner.log`: **4/4 pass**. PG17 seeds 0, 1, 126 and u64::MAX each perform five literal complete-plan comparisons, four actual fault refusals, four matching clean controls and 16 deliberate comparator failures. Its real runner episode executes **59 operations, zero violations**, reaches all registered keys and releases query/shared ownership. The oracle imports no engine/compiler types. The initial tail-checkpoint calibration was sensitive to real pointer ordering (`17-pg17-check.log`); the corrected final fault is armed at the actual callback boundary, with no producer behavior weakened. The budget refusal is an observed Memory error; the other three failures are scheduler clock fire receipts.

Strict frontend no-dependency lint and strict core feature-scoped lint pass in `41-final-frontend-clippy-green.log` and `42-final-core-clippy.log`; exact source formatting passes in `40-final-fmt.log`. The earlier plain dependency lint's preexisting cfg-sensitive core warnings are retained in log29. These are scoped lint claims, not a whole-workspace lint claim.

## Review and remaining gates

All three independent final reviews cleared their findings: `reviews/semantic.md`, `reviews/resource-control.md`, `reviews/pg17.md`. Root's independent review of the contributor's four core paths is `core-adapters/root-review.md`. Their inventories/receipts and final source correspondence are retained. Since those frozen reviews, production differences are rustfmt only; the PG17 oracle comment now correctly says five recipes. Additional directed tests and equivalent test metadata sizing were terminal-checked.

This is complete nonsearch read **plan lowering**, not full graph execution. The controlled source in the runtime tracer produces one literal row, not an alternate interpreter. Native traversal/expression execution, all 57 original read TCK coordinates, persisted tiny-graph oracle, compaction/reopen and real public lifecycle remain mandatory in ZE-50/51/53/56. Search composition remains ZE-58, full profile integration ZE-59. No TCK statement is marked executed here. Full workspace/adversarial campaigns and broad coverage qualification are deferred through ZE-118 under the user's instruction; focused changed-path controls ran. Follow-up commands include `cargo nextest run --workspace`, the full configured adversarial campaign, and `scripts/coverage.sh` on the integrated ZE-126 commit. No broad coverage percentage, release, Windows or public C ABI acceptance is claimed.
