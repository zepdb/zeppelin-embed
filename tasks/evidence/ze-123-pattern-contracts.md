# ZE123 typed relationship pattern contracts

Base main7028a42f2f4898bb7aa729dc935bd51b4033f2ba. This freezes the
compiled component used by ZE50 and ZE126; it does not execute a graph pattern
or qualify Cypher TCK results. Source inventory is ze-123/source-manifest.json.
No third-party dependency or Cargo.lock change was introduced.

## Compiled contract

Expand and BoundedExpand now retain exact-name relationship_types slices.
An empty slice is unrestricted. Duplicate OR names remain representable and
must never multiply runtime rows; execution resolves names in its admitted
catalog. The entire descriptor array and every UTF8 name require retained
backing and are checked with control/work accounting.

BoundedExpand optionally carries EdgePredicate {current_edge,expression}.
The private slot is a fresh nonnullable REL beside the input slots, distinct
from both new outputs; its Boolean/null expression cannot see future outputs
or foreign slots. The slot never becomes a public result/list binding. Even a
zero-hop plan validates the expression, while runtime must evaluate no edges
for zero hops and reject null for every candidate edge before path extension.
Existing16-hop bounds, original endpoints, relationship uniqueness and bags
remain unchanged. PatternId remains shared across comma parts of one MATCH and
fresh for a later MATCH; those execution obligations are not metadata results.

OptionalApply explicitly distinguishes its existing two DAG forms. A right DAG
containing the left input substitutes each left row at that actual anchor.
An independent right DAG matches shared bindings by equality, with null never
matching. Only right-only slots null-extend after the entire right candidate
and attached predicate fail; shared left bindings remain. The scoped compiler
consumer uses the actual correlated anchor. Reused node/relationship variables
lower to fresh candidate slots, explicit equality and projection of original
bindings, never overwriting a slot. A directed structured test pins this shape
and rejects an attempted overwrite. Existing origin-lineage checks are retained.
NodeFacts::slot_at exposes checked ordinal schema access, not ID-as-offset.

## Scoped compiler and owner proof

The real final BoundQuery of a representative fixed OR + bounded OR/per-edge
property + correlated OPTIONAL/WHERE + complete RETURN enters a controlled
lowering tracer. All retained source/name/type/expression/input/operator/span
and projection storage is copied into actual QueryArena owners. Actual fact
Vec capacity, inventory, remapping/control storage and64KiB validation scratch
coexist with the compiler under the same QueryMemory/GraphResources. No caller
or compiler borrowed region is relabeled as an owned plan allocation.

Measured on the host in ze-123/host.json: compiler baseline98245B, additional
plan121655B, simultaneous reservation/peak219900B, actual copied heap14207B.
A219899B limit fails before consumption; cancellation inside the consumer is
caught at its final checkpoint. Both release to baseline. Exact helper-source
rustc probes allow owned usize to escape and reject GraphPlan escape through
the genuinely higher-ranked plan/facts consumer. Full commands, linked hashes,
source probes and logs are retained under ze-123/consumer. This is one complete
shape tracer; no general lowerer, admitted native read view or row oracle claim.

## RED, changed-path controls and terminal GREEN

The initial named positive test failed compilation because EdgePredicate,
relationship_types,edge_predicate and slot_at did not exist. This is API RED,
not a runtime product failure. Four later product mutations failed runtime
assertions(exit100): omit OR descriptor backing, permit private/output alias,
validate the edge predicate in the new-output scope, and expose unused schema
cells. Each source restored byte-for-byte; raw/mutation-results.json records
commands and hashes. The helper separately catches an omitted OR alternative
and a compiler-borrowed property name, with exact restoration and terminal GREEN.

Old PG6 was run against unchanged main0fff7f4:390value comparisons,2scope cases,
3fires/3clean controls; two focused tests passed, including the actual59-op
runner. The expanded probe adds nine primitive private-scope cases and an
in-flight deadline at the final three actual validation units. It reports390
comparisons,11scope cases,4fires/4same-seed clean controls, and59actual runner
operations/0violations. Registry entries cover scope,metadata and clock.pattern.
The expanded report assertion first failed at runtime on old counts. An early
probe spelling error and one new test closure-borrow error were harness-only
compile failures and corrected, not counted as product RED.

Terminal nextest run86d61ea5-4dd8-44b1-b293-8359500c89ea:28/28pass
(23graph_query_plan,2scoped consumer,3PG6/oracle/runner),453filtered out,
four isolated processes with one libtest thread and retries0. Strict core
all-target/test-support and scoped compiler/oracle/runner Clippy pass. Owned
format/whitespace checks pass. The independent review is ze-123/independent-review.md;
its one OptionalApply documentation clarification is applied. Two extra directed
reused-variable/reference cases were added afterward; production validation
remains the reviewed implementation. All41 inherited local paths are unchanged.

Commands:

```sh
cargo nextest run -p zeppelin-embed --test graph_query_plan \
  -p zeppelin-embed-cypher --test pattern_contract \
  -p zeppelin-embed-workspace-tests --test adversarial_tests \
  -E 'binary(graph_query_plan) | binary(pattern_contract) | test(property_graph_query_probe_checks_values_scopes_and_inflight_faults) | test(query_oracle_rejects_corrupted_primitive_observations) | test(one_runner_episode_reaches_required_query_contracts)' \
  --success-output immediate-final
cargo clippy -p zeppelin-embed --all-targets --features test-support --no-deps -- -D warnings
cargo clippy -p zeppelin-embed-cypher --test pattern_contract \
  -p zeppelin-embed-workspace-tests --test adversarial_tests \
  -p zeppelin-embed-adversarial-oracle --no-deps -- -D warnings
```

Broad workspace/adversarial/per-crate coverage remainsZE118. ZE126 owns complete
nonsearch read lowering; original ZE56 owns actual execution conformance.
ZE50/51/52/53 retain actual producer/view/row semantics and completed ownership.
No original dependency or acceptance criterion was removed by this component.
