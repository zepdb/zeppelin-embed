# Internal Cypher syntax frontend

This dependency-free workspace crate parses the accepted first-release syntax
profile in `docs/graph/plans/cypher.md`. It does not bind names or parameter
values, validate entity types, execute queries, or expose a reusable public
prepared-query API. Those tasks remain in the compiler/binder and typed core
integration tickets. In particular, a parsed query is not an executable plan.

`parse` applies the default limits and a 24 MiB conservative allocation budget.
`parse_with` accepts tighter `CompileLimits` and a caller-supplied `Resources`
adapter for shared allocation accounting and cancellation. `parse_bytes` first
validates limits and cancellation, rejects overlong input before scanning, and
then validates UTF-8. The text path owns a charged copy of its input.

The maximums are 65,536 input bytes, 8,192 tokens, 4,096 syntax nodes, 64 active
expression parser frames, 256 distinct named parameters, 256 explicit projection
items, 16 nested list literals, and 16 path hops. A caller cannot widen these
limits. Expanded `*` column counts, parameter-value list limits and semantic
limits still require the binder and execution admission. Arena nodes and
auxiliary lists use fallible reservations; capacity growth, strings and source
copies are charged before allocation. Charges accumulate conservatively even
when scratch storage is released. `Budget` also accepts a shared atomic cancel
flag. The caller's resource adapter owns translating to the shared query budget.

The lexer polls within comments, identifiers, numbers and quoted strings.
Parsing polls on every consumed token and allocated node. `Ast::visit` walks
postorder iteratively with cancellation; AST edges contain only `AstId`s, so
walkers and destruction need no recursive ownership. Lexical `TextId`, syntax
`AstId` and future core IDs are separate types. Spans are half-open UTF-8 byte
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
binder/runtime checks; their parse success is not a support/conformance claim.

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
conformance. The original scenario/execution manifest remains ZE-59's work.

`fuzz/fuzz_targets/cypher_parser.rs` exercises malformed bytes and text in the
excluded tooling workspace. `scripts/cypher-parser-footprint.sh` measures a
current parser-reachable C consumer and static archives on macOS. Its private
probe functions are tooling, not a shipped ABI. Final graph packaging and size
qualification belong to ZE-107 and the release qualification tickets.
