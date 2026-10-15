# Cypher support profile

This is the implemented bounded profile, not a claim of full openCypher/TCK
conformance. `parse` admits syntax; `compile_with`/`compile_in` also bind names,
types and parameters. `execute`/`StoreCypherExt::cypher` execute one complete
statement; C callers use `ze_store_cypher` with the `graph-cypher` feature.
Unsupported constructs fail explicitly.

## Clauses and expressions

| Surface | Supported forms and restrictions |
| --- | --- |
| Statement | One statement, optional final semicolon; reads end in `RETURN`, writes may omit it. Keywords are case-insensitive; names are case-sensitive. ASCII identifiers, backtick names, line/block comments and quoted UTF-8 strings are accepted. |
| `MATCH`, `OPTIONAL MATCH` | Comma-separated node/relationship patterns; directed or undirected reads, conjunctive node labels, alternative relationship types, inline static property bags and attached `WHERE`. Relationships cannot be reused within one pattern. |
| Bounded traversal | Finite literal `*n`, `*lo..hi`, `*..hi` bounds, including zero hops, with `lo <= hi <= 16`; a bound relationship variable denotes a list. No named path values, shortest paths or unbounded `*`/`*lo..`. |
| `WITH`, `RETURN` | Expressions, aliases, `*`, `DISTINCT`, grouping/aggregation, `ORDER BY` with `ASC`/`DESC`, `SKIP`, `LIMIT`; `WITH` also accepts `WHERE` and requires aliases for computed expressions. Duplicate output names fail. |
| `CREATE` | Nodes and directed, single-type relationships with static property bags. |
| `SET`, `REMOVE` | Static property assignments/removals and node label additions/removals. No whole-map replacement, `+=` or dynamic property names. Stored lists must contain homogeneous nonnull scalars; maps and entity references are not stored property values. |
| `DELETE`, `DETACH DELETE` | Entity variables; plain deletion must satisfy relationship constraints. Returning/accessing a deleted entity is rejected statically when known, otherwise at runtime. |
| `CALL` | Only the three `ze.*` search procedures below, with explicit `YIELD` names and optional aliases. No `YIELD *`, subqueries or user procedures. |

`MATCH` and search cannot follow an update, and search cannot share a statement
with mutations. `MERGE`, `UNWIND`, `UNION`, `FOREACH`, schema/transaction commands,
`LOAD CSV`, `EXPLAIN` and `PROFILE` are outside this profile.

Expressions include null, booleans, signed 64-bit integers, binary64 floats,
strings, mixed/nested lists, static map literals (`{key: expression}`), variables,
`$name` parameters, static property/map access, list indexing (negative indices
allowed; out-of-range gives null), node label predicates and parentheses.
Arithmetic is unary `+`/`-` and binary `+ - * / %`; comparisons are
`= <> < <= > >=` (including chained comparisons), `IN`, `IS [NOT] NULL`;
boolean operators are `NOT`, `AND`, `OR`, `XOR`. String predicates are
`STARTS WITH`, `ENDS WITH`, `CONTAINS`. No exponentiation, regex or list slices.
Null uses three-valued boolean logic; filters retain only true.

Both `CASE value WHEN value THEN expression ... [ELSE expression] END` and
`CASE WHEN predicate THEN expression ... [ELSE expression] END` are supported.
The first matching branch wins; an omitted `ELSE` gives null. Unselected result
branches are not evaluated. List comprehensions accept
`[x IN list [WHERE predicate] [| expression]]`, with lexical variable scope;
null input gives null. Maps can nest and participate in projection/grouping;
missing keys give null. Correlated existential patterns, bare or
`exists((a)-[:T]->(b))`, return booleans without multiplying rows; named variables
must already be in scope and relationships must be fixed-length. Pattern
comprehensions and `EXISTS { ... }` are absent.

## Functions and procedures

Built-in function names are case-insensitive. Scalar functions are `labels`,
`type`, `size`, `head`, `last`, `abs`, `toInteger`, `toFloat`, `floor`, `round`
(one argument each), and `coalesce` (one or more arguments; returns the first
nonnull value and skips later operands). `size` accepts lists or strings.
`labels` takes a node; `type` takes a relationship. Unknown functions fail.

Aggregates are `count(*)`, `count(expr)`, `collect`, `sum`, `avg`, `min`, `max`;
the expression forms accept `DISTINCT`. Aggregates must be top-level projection
items (parentheses allowed), not nested or combined inside another expression.
`avg` accepts numbers, ignores null and returns a float, or null for no numeric
inputs. `sum`/`avg` reject nonnumeric inputs. `count(expr)`/`collect` omit null.

Case-sensitive extensions are `ze.node_id(node)`,
`ze.relationship_id(relationship)` (32 lowercase hexadecimal digits), and
`ze.stored_text(node)` (stored text or null).

| Procedure signature | Available `YIELD` fields |
| --- | --- |
| `ze.vector_search(vector, k, mode [, eligible])` | `node`, `distance` |
| `ze.text_search(text, k [, eligible])` | `node`, `score` |
| `ze.hybrid_search(vector, text, k, mode [, eligible])` | `node`, `score`, `vector_distance`, `lexical_score` |

Search arguments are query-invariant and nonnull; vectors contain numeric
elements, `k` is an integer in `1..=4096`, and `mode` is exactly `default`,
`auto`, `exact`, `scan` or `graph`. Omitting eligibility searches all indexed
nodes. Explicit eligibility is either literal `[]` or a materialized global
`collect(DISTINCT node)` with singleton-row provenance. Arbitrary lists and
per-row correlated searches are refused. Hybrid component scores may be null.
Results retain per-call search reports; at most eight calls are admitted.

## Parameters, results and limits

Supply exactly the named parameters referenced by the query. Missing, surplus
and duplicate bindings fail. Scalar null/bool/integer/float/string and bounded
mixed/nested lists (including empty lists and numeric vectors) are supported.
Entity and map parameters are not supported by execution lowering. Parameters
are values, not clause text, label/type/property names or whole pattern maps.

Completed results own their column names and values: null, bool, i64, f64,
UTF-8 string, copied node, copied relationship, list and map. Maps have unique
keys in UTF-8 order; rows have bag semantics unless ordered explicitly. Results
include generation/outcome and work counters; C responses must be released with
`ze_graph_response_free`. Failures do not return partial rows.

| Limit | Default hard maximum |
| --- | ---: |
| Query text bytes / tokens / AST nodes | 65,536 / 8,192 / 4,096 |
| Active expression parser frames / distinct parameters / columns | 64 / 256 / 256 |
| Nested list depth / path hops | 16 / 16 |
| Lowered plan nodes / plan depth | 4,096 / 64 |
| Query memory | 24 MiB |
| Query list elements (including nested elements) | 524,288 |
| Operator rows / adjacency entries / expression work / hash probes | 4,000,000 / 2,000,000 / 8,000,000 / 16,000,000 |
| Completed rows | 65,536 |
| Completed bytes / prepared payload bytes / completed ABI bytes | 4 MiB each |
| Vector coordinates / vector bytes | 2,147,483,648 / 8 GiB |
| Lexical postings / lexical blocks / search invocations | 64,000,000 / 4,000,000 / 8 |

`CompileLimits`, `GraphQueryOptions` and `RuntimeLimits` may tighten limits, not
widen them. Standalone parsing uses a conservative 24 MiB allocation
budget, reserving replacement overlap and releasing freed old capacity.
Execution shares query accounting and reserves compiler scratch.
Operator/result/source capacities and store admission can refuse work earlier;
`LIMIT` does not bypass accounting for work already performed. Cancellation and
deadlines apply through compilation and execution.

## Errors visible to callers

Rust returns `StatementError::Compile(ParseError)` or
`StatementError::Query(GraphQueryError)`. A `ParseError` carries a kind, message
and half-open UTF-8 byte span; lazy locations are one-based line/scalar-column.
The table gives the current compile-kind mapping at `ze_store_cypher` (decimal
C codes); `ZE_OK` is 0.

| Compile kind | C error |
| --- | --- |
| `Syntax` | `ZE_ERR_QUERY_SYNTAX` (37) |
| `Unsupported`, `SearchContext` | `ZE_ERR_QUERY_UNSUPPORTED` (38) |
| `InvalidParameterUse`, `Parameter` | `ZE_ERR_PARAMETER` (39) |
| `Type` | `ZE_ERR_TYPE` (40) |
| `UnknownVariable`, `DuplicateVariable` | `ZE_ERR_SCOPE` (41) |
| `DeletedEntity` | `ZE_ERR_DELETED_ENTITY` (46) |
| `InvalidLiteral`, `InvalidRange`, `DuplicateProperty`, `InvalidLimits`, `RelationshipUniqueness` | `ZE_ERR_INVALID_ARGUMENT` (1) |
| `Limit(TextBytes/Tokens/AstNodes/Depth/Parameters/Columns/ListDepth)` | `ZE_ERR_BUDGET_EXCEEDED` (15) |
| `Resource(Cancelled/ReadCancelled)` / `Resource(Timeout)` | `ZE_ERR_CANCELLED` (12) / `ZE_ERR_TIMEOUT` (13) |
| `Resource(WorkLimit/Memory)` / `Resource(Allocation)` | `ZE_ERR_BUDGET_EXCEEDED` (15) / `ZE_ERR_OUT_OF_MEMORY` (14) |
| `BindingInvariant`, `Resource(Control)` | `ZE_ERR_INTERNAL` (22) |
| `Plan(PlanError)` | Limit/Footprint → 15; Scope → 41; Type → 40; Parameter → 39; ReadAfterWrite/ReadWriteSearch → 38; structural errors → 1; Control delegates to its query error. |

Runtime `GraphQueryErrorKind` is non-exhaustive. Its current kinds map through
`GraphStoreError` at the C statement entry point as follows:

| Runtime kind | C error |
| --- | --- |
| `InvalidPlan`, `Parameter`, `Expression` (including arithmetic failures) | `ZE_ERR_INVALID_ARGUMENT` (1) |
| `Constraint` | `ZE_ERR_ENDPOINT` (45) |
| `Limit`, `Cancelled`, `Timeout`, `Closed` | `ZE_ERR_BUDGET_EXCEEDED` (15), `ZE_ERR_CANCELLED` (12), `ZE_ERR_TIMEOUT` (13), `ZE_ERR_CLOSED` (3), respectively |
| `Corruption`, `Storage`, `Unavailable`, `WriteIndeterminate`, `Internal` | `ZE_ERR_CORRUPT` (10), `ZE_ERR_IO` (9), `ZE_ERR_ACCESS_MODE` (21), `ZE_ERR_INDETERMINATE_COMMIT` (50), `ZE_ERR_INTERNAL` (22), respectively |

`PlanError::Control` can also map type/budget/cancel/timeout/internal/invalid
argument errors, or `ZE_ERR_ARITHMETIC_DOMAIN` (47),
`ZE_ERR_ARITHMETIC_OVERFLOW` (48), `ZE_ERR_DIVISION_BY_ZERO` (49).
These specific arithmetic codes are not the runtime expression mapping above.
The handle/ABI/admission/output layers can additionally report invalid argument,
invalid handle (2), closing (4), poisoned (5), panic (6), busy (7), store
busy (8), unsupported (11), allocation/budget errors, access mode and store kind
(35). Read C diagnostics with `ze_last_error_message` and inspect response
disposition. Rust exposes `nothing_committed()`, optional operator and counters;
an indeterminate commit may have changed the store and requires reopening.

## Evidence and historical verifier reproduction

The surface above is derived from `src/parser.rs`, `src/binding/`,
`src/lowering/`, core query values/limits and the FFI error mappings. Focused
execution evidence lives in `tests/profile_conformance.rs`,
`tests/relational_execution.rs`, `tests/ze452_expressions.rs`,
`tests/pattern_predicates.rs` and the read/write/search execution suites. Older
ZE-59 boundary cases describe the profile at their original revision; subsequent
positive tests establish extensions such as map literals.

At historical commit `00a9c0df`, from the repository root, the bare command was:

```sh
python3 scripts/cypher-profile-conformance.py
```

It exits 1 (`ValueError: profile differs; inspect before --write`) without the
pinned upstream checkout and execution receipts. The source pin is
`007895aff5f33097d67b2e48a0a2babd6bd18590`; the verification form was:

```sh
python3 scripts/cypher-profile-conformance.py /path/to/openCypher --log /path/to/corpus.log
```

`--run` could produce the receipts instead of `--log`; execution alone without
verified upstream sources did not satisfy acceptance. ZE-297 (`c9bc9ebe`)
removed this script and its execution manifest. On the current checkout the
bare command exits 2 because the script is absent. This document preserves the
historical reproduction, not an instruction to restore that retired verifier.
