# ZE-126 independent semantic review — accepted completed-edge correction

Reviewer: `/root/ze55_binder`, delegated under root-owned ZE-126 review. Only immutable snapshot reads and isolated `/tmp` test writes; no moving worktree, tracker, or main changes.

## Final disposition

The blocking self-list lowering defect is resolved by the independently verified immutable additions snapshot. Original RED is retained below; the exact same positive query now passes. No open concrete semantic finding remains in this bounded review. The approved completed-candidate predicate contract is represented faithfully without changing the existing prefix predicate scope. This is semantic source/plan acceptance only; root retains ownership/accounting qualification and reviewer120 independently reviews UTF-8 changes.

## Original review-1 disposition (preserved history)

One concrete blocking defect on snapshot 1: `MATCH (a)-[r*1..2 {x:size(r)}]->(b) RETURN r` binds successfully but native lowering returns `Plan(Scope)` with span `0..0`. No additional concrete semantic blocker found in this bounded review. Root has approved preserving the accepted form using a separate completed-candidate edge predicate seam; its correction is not yet frozen/reviewed here. This report is not final acceptance.

The defect is at frozen `lowering/pattern.rs:219–225`: every bounded inline property becomes a prefix `EdgePredicate`, whose legal input is the incoming row plus a fresh private REL. The bound RHS still reads the newly produced public relationship LIST `r`. That output is intentionally absent from prefix predicate scope, so the existing core guard correctly rejects it. `lowering/mod.rs:671–683` maps that rejection to a generic `Plan(Scope)` at `Span::default()`. Do not weaken the prefix scope guard, silently omit the property constraint, or claim rejection is a profile rule without normative support. Root's approved direction distinguishes completed-candidate scope (incoming row + whole produced LIST + fresh private REL, no new target node) from prefix scope, with all-member true and vacuous zero-hop acceptance.

## Exact source identity

Base commit: `736e8440221d4b53d797637e61840b53d0cdc7c7`. Isolated archive commit: immutable ZE-125 `beb1a1e44730b56494ec2cb4aafc5d9ea5419763`.

- `/tmp/ze-126-semantic-review-1/inventory.json`: SHA256 `2c6e01af32886494503d6e6c61fde0e88b0f2349e144e32f92fe417627264155`, all 31 entries verified.
- `/tmp/ze-126-control-review-1/inventory.json`: SHA256 `5bbef6172d2c451d093e5b683770ea3fc6de88b5b5fdb1ba996405cd04ded059`, all 4 entries verified. This intentionally supersedes the first snapshot's runtime test file; shared_resources and lowering/context are identical.
- `/tmp/ze-126-core-adapters/source-hashes.json`: SHA256 `aa241cea33ba700d007024ab46533ba7f5be8621e81d743306fc484969583f24`, all 4 adapter entries verified against `frozen/`.
- Combined 39 manifest entries produce 36 distinct effective paths; all match the isolated reproduction tree. Receipt: `/tmp/ze-126-independent/review-1-receipt.json`, SHA256 `30d0a6bbfa64d8d06421ab7b81f1007fd8a0c1690b16713c5f9a6fe41a760b7f`.

## Reproduction and directed checks

Scratch: `/tmp/ze-126-independent/repro`. Test additions: `crates/zeppelin-embed-cypher/tests/ze126_review.rs`. No production mutations were needed: the source itself fails the independent positive assertion for the accepted self-list form.

Exact terminal command, from the scratch directory:

```sh
CARGO_TARGET_DIR=/tmp/ze-126-independent/target cargo nextest run -p zeppelin-embed-cypher --test ze126_review --test read_lowering --test runtime_lowering --test-threads 4
```

Raw output: `/tmp/ze-126-independent/expanded-semantic-probes-3.log`. Exit 100, 17 tests: 16 pass, 1 intended RED (`independent_bound_path_self_reference_is_preserved`). The six existing native-plan tests and four frozen runtime-context tests all pass; six independent tests pass.

The independent tests include 40 literal positive queries (10 pattern, 12 projection, 18 reuse/projection) and exact assertions beyond mere successful validation:

- `WITH 1 AS x RETURN DISTINCT -x AS x ORDER BY -x` sorts by Negate(Slot(projected x)), preserving output-alias precedence.
- Comma-separated paths in one MATCH retain one PatternId; separate MATCH clauses use distinct PatternIds.
- `MATCH (a)-[r]->(b) WITH a AS x, b AS y, r AS q MATCH (x)-[q]->(y) RETURN x,q,y` allocates fresh candidate relationship/node slots, compares each to the original slot, and prunes candidates while retaining the original x/y/q scope.
- Whole OPTIONAL MATCH conjunction stays on OptionalApply; its right DAG reaches the actual left input. Only introduced relationship/node slots become nullable, while the existing left anchor stays NODE.
- Hidden ORDER input slots, DISTINCT alias swaps, grouping with interleaved key/aggregate output order, count/collect variants, WITH scope, copied alias reuse, reused nullable entities, per-edge incoming bindings, zero hops, and matching node/relationship self-properties validate.
- Parameter source order differs from supplied binding order while ParameterIds still refer to the correct supplied binding. Copied expression spans slice the exact `$first` / `$second` source tokens.
- Missing parameter, negative parameter LIMIT, and mutation through the read-only entry point return the intended typed error with nonempty in-range source spans before the consumer executes.

The failed first baseline compile lacked immutable core supplements and is packaging evidence only. An early probe test used a struct pattern for a tuple enum variant; that test-author compile error is excluded. Four future-variable queries and one sum/avg/min/max query were initially included but rejected identically by the original binder as outside the profile; they were removed from the positive set and are not product findings. These attempts remain in earlier raw logs to avoid concealing them.

## Source review coverage and boundaries

Reviewed the full lowering driver, sparse expression remapping, pattern construction, projection construction, parameter copying, and source/error mapping against frozen binder and core plan contracts. Checked all expression variants for child remapping; one PatternId per MATCH; reused bindings through explicit equality and reprojection; complete optional predicate/correlation placement; prefix edge private scope; textual output ordering after aggregate key partition; projected-alias priority; hidden ORDER retention and final pruning; literal/parameter SKIP/LIMIT; and copying of parameter names, nested lists, strings, and exact float bits. Root independently owns the actual retained-allocation/context review.

These are controlled compilation/plan observations plus the frozen runtime-context tracer. They do not execute the selected openCypher TCK or prove graph traversal, sorting, grouping, distinctness, empty-group results, OPTIONAL row semantics, or mutation execution against a real graph participant. Completed-path predicate execution will remain ZE-50; selected TCK execution remains ZE-56. PG17 owner evidence and the final corrected source are pending. Nonessential broad suites remain ZE-118.


## Corrected frozen delta and independent terminal checks

Reviewed `/tmp/ze-126-additions-review-1/inventory.json`, SHA256 `c167ae5b590a65894bbb31150e670c8e7ac22f7202cf8e6666a068bfdc73d49a`, and verified all 16 frozen files before overlay. All effective source paths match after the terminal checks; exact receipt and test-source hashes are in `/tmp/ze-126-independent/final-receipt.json`.

The core `CompletedEdgePredicate` is separate from `EdgePredicate`. Its validator begins from incoming scope, inserts the completed public LIST, and inserts a fresh private nonnullable REL. It excludes the new target node, detects collisions with incoming and both output slots, type-checks Boolean/null, and leaves private scope absent from output facts. The existing prefix validator is unchanged. Both validators still run when maximum is zero; the normative execution contract requires vacuous zero-member evaluation, not omission of structural validation.

Lowering determines stage from transitive dependencies in the copied postorder expression DAG. It visits unary, binary, list, property/label and aggregate operand children. The freshly allocated current-edge slot cannot be mistaken for the newly produced relationship LIST. When any property RHS depends on the new LIST, the full conjunction moves to the completed field and the prefix field is empty. Previously bound path lists remain ordinary incoming bindings and do not trigger the new stage. The `size(r)` expression keeps its exact original source span.

Terminal commands from `/tmp/ze-126-independent/repro`:

```sh
CARGO_TARGET_DIR=/tmp/ze-126-independent/target cargo nextest run -p zeppelin-embed-cypher --test ze126_review --test read_lowering --test runtime_lowering --test pattern_contract --test-threads 4
CARGO_TARGET_DIR=/tmp/ze-126-independent/target cargo nextest run -p zeppelin-embed --test ze126_completed_review --test graph_query_plan --test-threads 4
```

- First command: exit 0, 22/22 pass, raw `/tmp/ze-126-independent/completed-edge-expanded-green-2.log`.
- Second command: exit 0, 25/25 pass, raw `/tmp/ze-126-independent/completed-edge-core-green-3.log`.
- Original literal `MATCH (a)-[r*1..2 {x:size(r)}]->(b) RETURN r`: original snapshot RED100, corrected snapshot GREEN. Original logs/receipt remain intact.
- New independent source: `crates/zeppelin-embed-cypher/tests/ze126_review.rs` and `crates/zeppelin-embed/tests/ze126_completed_review.rs` in the isolated scratch tree only.

Additional independent positive routing probes:

```cypher
MATCH (a)-[r*1..2 {x:size([r][0])+1,y:7}]->(b) RETURN r
MATCH (a)-[r*0..2]->(b)-[s*0..2 {x:size(r)}]->(c) RETURN s
MATCH (a)-[r*0..2]->(b)-[s*0..2 {x:size(r)+size(s),y:7}]->(c) RETURN s
```

These assert, respectively: nested list/index/arithmetic dependency routes the whole `x`+`y` bag to completed stage; incoming `r` alone stays prefix stage; incoming `r` plus produced `s` selects completed stage for the whole bag. Owner tests additionally verify optional zero-hop routing and exact `size(r)` source span.

Independent core probe runs 9 distinct cases with maximum 0 and 2 (18 cases): valid full-LIST/private-REL predicate, valid incoming source access, forbidden fresh target, forbidden foreign slot, unchanged prefix-LIST exclusion, private output escape, and private collisions with source/node/list. Positive cases validate; all seven invalid cases return exactly `PlanError::Scope` at both bounds. The fixture uses declared backing only for core validation; it is not an actual runtime ownership/admission claim.

During probe construction, a missing local `validate_plan` test helper caused compile101, and then deliberately unused fixture expressions triggered `Unreachable` before the intended scope assertion. The test-only helper was supplied and each fixture expression set narrowed to its reachable graph. Those attempts remain in raw earlier logs but are excluded from product RED evidence. No production source was mutated; all source hashes remain exact.

No broad suites were run. Actual traversal, all-member predicate execution, error/false/null runtime behavior and zero-hop emission remain ZE-50; selected TCK remains ZE-56. UTF-8 source correctness is assigned to reviewer120. Final ZE-126 integration/PG17/accounting/commit disposition belongs to root and the ticket owner.
