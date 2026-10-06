# Internal Cypher frontend

This workspace crate parses and binds the accepted first-release profile in
`docs/graph/plans/cypher.md`. Its only production dependency is the approved
internal core crate; it adds no third-party package. A successful parse is syntax
admission, and a successful binding is a typed, source-preserving description.
Neither grants execution, writer admission, or a reusable prepared-query API.
Read, write and search operator lowering/execution are provided by ZE-56/57/58.
Focused Rust profile evidence is documented below in “Focused execution receipts
(ZE-59)” and in `tests/profile_conformance.rs`.

`parse` applies the default limits and a 24 MiB conservative allocation budget.
`parse_with` accepts tighter `CompileLimits` and a caller-supplied `Resources`
adapter for shared allocation accounting and cancellation. `parse_bytes` first
validates limits and cancellation, rejects overlong input before scanning, and
then validates UTF-8. The text path owns a charged copy of its input.

The maximums are 65,536 input bytes, 8,192 tokens, 4,096 syntax nodes, 64 active
expression parser frames, 256 distinct named parameters, 256 explicit projection
items, 16 nested list literals, and 16 path hops. A caller cannot widen these
limits. Expanded `*` column counts and parameter-value list limits are checked by
the binder; final typed operator/work limits also require execution admission. Arena nodes and
auxiliary lists use fallible reservations; capacity growth, strings and source
copies are charged before allocation. Charges accumulate conservatively even
when scratch storage is released. `Budget` also accepts a shared atomic cancel
flag. The caller's resource adapter owns translating to the shared query budget.

The lexer polls within comments, identifiers, numbers and quoted strings.
Parsing polls on every consumed token and allocated node. `Ast::visit` walks
postorder iteratively with cancellation; AST edges contain only `AstId`s, so
walkers and destruction need no recursive ownership. Lexical `TextId`, syntax
`AstId` and core IDs are separate types. Spans are half-open UTF-8 byte
ranges into `Ast::source`; error line/scalar-column rendering is lazy and polled.

The AST retains information for later binding without catalog/store access:

| Node | Ordered children |
| --- | --- |
| Statement | Clauses in textual order |
| Match | Patterns, then optional attached Predicate |
| Create | Patterns |
| Pattern | Node, then alternating relationship/node pairs |
| NodePattern | Conjunctive label Names, then optional Properties |
| RelationshipPattern | Alternative type Names, then optional Properties |
| Properties / Property | Property entries / one expression per entry |
| Projection | ProjectionItems or Star, Order items, optional Skip, Limit, Predicate |
| Call | Argument expressions, followed by explicit Yield items |
| Set / Remove | Ordered property or label update items |
| Delete | Variable items |
| Group / Unary / PropertyAccess | One expression |
| Binary / Index | Left and right expressions |
| LabelPredicate | Receiver expression, then conjunctive Names |
| List / Function | Arguments; count(*) has zero arguments |

Comparisons such as `a < b <= c` retain two comparisons joined by AND, with a
shared arena ID for `b`. Transparent grouping preserves top-level aggregates and
plain variable forwarding. Parser-side profile checks reject unknown syntax,
compound/nested aggregates, unnamed WITH expressions, invalid finite ranges,
whole-map property parameters, writes mixed with search, and reading clauses
after updates. Type-dependent restrictions, variable scope/rebinding, aliases,
YIELD signatures/provenance, deleted-entity results and parameter bindings remain
binder/runtime checks; parse success alone is not their acceptance evidence.

Source adaptation: `src/lexer.rs`, `src/parser.rs` and `src/ast.rs` follow selected
reviewed portions of Shopify/cypher-parser at
`a7b822fbece9ee2c3f2b57ecfd4e90a2ca215383`. The retained MIT notice is in
`SHOPIFY-LICENSE-MIT`. Its recursive owned AST, character-index positions,
unbounded allocation, executor and different language subset were not adopted.
The std-only source adaptation adds the local profile, Pratt expressions,
charged arenas, UTF-8 spans, cancellation, arithmetic/parameters/floats/writes
and rejection checks. Shopify is not a Cargo dependency.

The original openCypher excerpts in `tests/fixtures/selected-tck-syntax.txt` are
syntax-only inputs from the 99 selected scenarios and 56 setup statements at
`007895aff5f33097d67b2e48a0a2babd6bd18590`. Notices and Apache license accompany
them. `scripts/cypher-parser-fixtures.py <pinned-checkout>` verifies the source
objects and exact excerpt bytes; `--write` regenerates this fixture. The parser
checks 153 accepted inputs and two original InvalidParameterUse inputs. It does
not run their result/error semantics or state effects, and establishes no TCK
conformance. The binding manifest described below separately records supported
coordinates and binding results; the separate ZE-59 profile records focused local execution; pinned-source
verification and full qualification remain pending.

`fuzz/fuzz_targets/cypher_parser.rs` exercises malformed bytes and text in the
excluded tooling workspace. `scripts/cypher-parser-footprint.sh` measures a
current parser-reachable C consumer and static archives on macOS. Its private
probe functions are tooling, not a shipped ABI. Final graph packaging and size
qualification belong to ZE-107 and the release qualification tickets.

## Binding and shared ownership

`compile_with` parses and binds the complete statement before calling a
higher-ranked consumer. Its borrowed BoundQuery cannot escape that callback.
The view retains exact clause order, pattern types/bounds/properties, symbolic
names, source spans, scoped columns, expression facts, complete core scalar
operations, and per-call search mode/eligibility provenance. AST-indexed
expressions deliberately include syntax holes; later lowering must prune and
remap reachable nodes before GraphPlan validation. Synthesized expressions have
a separate bounded allowance; this does not widen the final typed plan limit.

Known scope/type errors, missing/surplus/duplicate parameters, unsupported
syntax, duplicate aliases, malformed search signatures, same-pattern relationship
reuse, invalid stored-property list shapes, and provably deleted results reject
before consumption. Runtime-dependent property/list/deleted access remains
explicit. Counts of deleted entities bind; identity functions and size of
collections containing deleted references require later dynamic validation.
No compile operation owns a store writer or can publish a graph mutation.

`compile_in` reserves every frontend-created allocation and 64 KiB compiler
scratch under the existing QueryMemory/GraphResources pair. Actual capacities
and old/new growth overlap are charged before controlled moves; released
scratch remains conservatively charged until the invocation ends. Borrowed
caller source/parameter capacities are not inferred or credited. The opaque
external-capacity guard grants no retained-owner capability. Later execution
must retain actual QueryInputs owners or make separately charged copies. The
integration-only RETURN tracer proves real copied QueryArena backing and an
actual NodeFacts Vec certificate; it makes no runtime admission claim.

`scripts/cypher-binding-manifest.py <pinned-checkout>` checks exact original
query/source hashes and the 268 scenarios in the selected 26 feature files:
99 supported, 20 explicitly profile-rejected, and 149 not selected. The selected
155 original statements bind 152 and reject three at compile time, preserving
original error phases. Every execution entry remains `unexecuted`; this is not
TCK result/side-effect or full-corpus conformance evidence. The manifest's
negative controls use an isolated source clone:
`scripts/tests/cypher_binding_manifest.py <pinned-checkout>`.

The PG11 primitive oracle independently checks ordered output/type/parameter-bit
and mode observations, compile refusal, reached cancellation/budget fault counts
and same-seed clean controls. Broad original-TCK, full adversarial/workspace,
coverage and release-size campaigns remain deferred to ZE-118/E12.


## Focused execution receipts (ZE-59)

`tests/profile_conformance.rs` executes the unchanged selected fixture queries
through `execute`, shares typed comparators with the read/write runners, checks
populated-state refusal/reopen behavior, and compares progressive mutations with
a small independent model. Its maintenance case requires physical work before
claiming logical-state preservation. Original profile rejections and local
outline observations never become original semantic passes.

Inventory rows are identified by the `ze59_*_positive_and_boundary_evidence`
test names in `tests/profile_conformance.rs`. Comments beside the cases point to
existing read/write TCK, search execution/parity, and numeric aggregate tests.
The binding manifest, TCK fixtures, typed comparators, and Rust receipt helpers
remain unchanged. The execution JSON manifest and Python verifier/controls were
removed by the ZE-297 owner decision of 2026-10-05.

Historical verification in commit `00a9c0df` used upstream pin
`007895aff5f33097d67b2e48a0a2babd6bd18590`: 99 original scenarios verified,
none reclassified; 30 `rejected_profile` coordinates and one local observation.
The recorded verifier result was `130 coordinates; 130 local GREEN; sources
verified`, with the Result.scala bag-comparator review completed. The historical
resolution is preserved locally in `tasks/evidence/ze-59-resolution.md`; this
cleanup does not repeat upstream verification. C/Swift and coverage/size/performance
qualification remain separate pending gates.
