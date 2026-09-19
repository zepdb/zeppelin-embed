# ZE-54: bounded outer Cypher syntax frontend

Verified locally on 2026-09-18. Branch `codex/ze-54-cypher-parser`, isolated
worktree `/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-54`, forked from
committed main `366cdf44ed5cd6b2cccb99e534eba09a58d9e274`. The worktree contains
no ZE-32 `property_graph` source. Main's dirty files and other targets/profiles
were not used or modified.

Host: Apple M3 Max, 137,438,953,472 bytes RAM (128 GiB), arm64 macOS 27.0
build 26A5388g. Rust/cargo 1.93.0; Apple clang 21.0.0
(clang-2100.1.1.101). These are local macOS arm64 observations, not minimum-OS,
Intel, Windows, CI, or final graph-release qualification.

## Scope, source reuse and acceptance boundary

The approved parser ticket is implemented in the new internal std-only
`crates/zeppelin-embed-cypher` crate. There are no direct or transitive product
crate dependencies and no core source edits. This adds only a workspace member,
its local lockfile entry, and the same local dependency in the excluded fuzz
workspace. Shopify is source, not a dependency. No dependency policy changed.

Read and adapted selected Shopify lexer token/error/punctuation/comment
structure, complete-input clause/pattern/projection descent and syntax
classification from `a7b822fbece9ee2c3f2b57ecfd4e90a2ca215383`. The clone's HEAD
was verified; MIT copyright/license is retained in `SHOPIFY-LICENSE-MIT` and
adapted source headers. Zeppelin's Pratt expressions, byte spans, limits,
fallible accounting, flat arena, cancellation, writes, parameters, floats and
profile rejection rules replace/extend those source portions. No Shopify
executor or recursively owned AST was imported.

Read the complete accepted `docs/graph/plans/cypher.md`, live ZE-54, ZE-29,
ZE-112 and E6 decision record. The parser accepts the documented lexical and
syntax inventory; explicit tests cover each positive/rejection area. The
`README.md` in the crate documents AST child order, resource semantics and the
remaining binder obligations. AST IDs, lexical IDs and future core IDs remain
distinct. Complete-input parsing has no store/writer/execution access and
cannot return an executable prefix of a rejected statement.

This ticket proves syntax representation, errors, resource behavior and current
frontend footprint. It does **not** claim name/type binding, parameter-value
validation, procedure signature/provenance validation, row bags/order, graph
side effects, durable writes, deleted-entity behavior or TCK execution. Those
remain ZE-55–ZE-59 and the core/bindings/qualification tickets. No stub core
execution or false conformance adapter was added. New syntax allocation and
cancellation are exercised at the parser resource seam; no existing core
operation ordering or fault site changes in this ticket.

## RED -> GREEN and independent review

All commands below run in the isolated worktree. Named tests run through the
crate's parser seam and inspect returned syntax/error data. Logs (trailing whitespace/blank EOF lines removed) are in
[ZE-54-cypher-parser](ZE-54-cypher-parser/).

| Test | Observed intended RED | Terminal behavior |
| --- | --- | --- |
| `parses_literals_and_expression_precedence` | Initial seam returned `syntax frontend is not implemented` | Full I64 edges, float/string/bool/null literals and arithmetic parse |
| `parses_complete_clause_pattern_projection_and_write_inventory` | Initial RETURN-only parser rejected first MATCH | Patterns, writes, WITH, OPTIONAL MATCH, modifiers and CALL inventory parses |
| `aggregate_placement_and_with_alias_are_profile_checked` | Accepted `RETURN count(*)+1` | Rejects compound/nested/misplaced aggregates and unaliased nontrivial WITH |
| `reserved_words_require_escaping_as_variables_but_allow_schema_names` | Accepted `MATCH (RETURN) RETURN 1` | Reserved variables require backticks; schema names remain usable |
| `truncated_utf8_error_span_covers_the_remaining_bytes` | Returned span 8..8 for truncated two-byte tail | Returns exact remaining-byte span 8..10 |
| `grouping_preserves_top_level_aggregates_and_variable_forwarding` | Rejected `RETURN (count(*))` | Group wrappers preserve valid aggregates/plain variable forwarding |
| `duplicate_property_error_names_the_second_key` | Pointed to colon 16..17 | Points to duplicate key 15..16 |
| `byte_admission_validates_limits_before_scanning_input` | Invalid widened limit returned Syntax for malformed input | InvalidLimits is checked before UTF-8 scanning |
| `unknown_function_and_procedure_errors_identify_the_name` | `RETURN id(n)` pointed at parenthesis 9..10 | Identifies exact function name 7..9 and equivalent extension/procedure names |
| `recognized_out_of_profile_syntax_has_a_distinct_profile_error` | Maps and later MATCH/UNWIND suffix returned generic Syntax | Known unsupported grammar forms return distinct Unsupported errors |

The grouping, duplicate-key span and byte-admission defects were found by a
separate read-only reviewer, reproduced before fixing, then rechecked by that
reviewer. The review covered the parser/resources/lexer boundary and explicitly
did not claim binder/execution qualification. No remaining frontend finding was
reported after source reinspection. The final unknown-name-span and profile-error
classification refinements came from a subsequent local completion audit, with
additional observed RED/GREEN tests; those two changes are not attributed to the
independent reader.

Exact focused command pattern:

```sh
CARGO_TARGET_DIR=target/ze54 cargo test -p zeppelin-embed-cypher --test parser TEST_NAME -- --exact
CARGO_TARGET_DIR=target/ze54 cargo test -p zeppelin-embed-cypher -- --nocapture
```

Terminal result: **19 passed, 0 failed**, plus zero doctests. Besides the named
REDs, assertions verify Pratt operator precedence, shared middle operands in
comparison chains, decoded Unicode/control/quote escapes, exact UTF-8 spans,
full-width numeric limits, preserved pattern direction/labels/types/bounds,
projection/CALL payload, complete-input rejection and all resource boundaries.

## Resource and deliberate-defect evidence

`cancellation_and_allocation_faults_can_fire_at_every_reachable_site` parses
three actual statements first, then injects cancellation at every observed
checkpoint and reservation failure at every observed allocation request. Each
injected run must return the exact resource error at that point; a fresh clean
run must restore identical AST nodes. Counts are measured, not hardcoded clean
flags. All syntax lists and strings reserve fallibly and charge capacity growth
before allocation; charges include scratch capacity and the owned source.

| Actual input | Checkpoints tested | Reservation failures tested | Clean cumulative charged bytes |
| --- | ---: | ---: | ---: |
| Comment/Unicode MATCH, bounded relationship, aggregate/WITH/order/filter | 323 | 65 | 11,900 |
| CREATE/SET/REMOVE/DETACH DELETE | 152 | 39 | 5,946 |
| Vector-search CALL and projection | 128 | 26 | 3,040 |

Total: 603 cancellation injections and 130 reservation failures, with three
same-input clean controls. Additional checks cancel every iterative visitor
step and the lazy location renderer; an actual atomic cancellation flag fires
and clears. Limits reach the actual tokenizer, AST arena, nested expressions,
list nesting, distinct named parameters, projection items, text bytes and
finite path range processing. Default maxima cannot be widened.

Four deliberate source defects were applied individually: disable resource
polling, bypass token limit, accept trailing input, and lower multiplication
precedence to addition. Each exact named acceptance test failed with exit 101;
restoring byte-identical source returned exit 0. The mutation script asserted
restoration before the clean test. Exact hashes/results and separate RED/GREEN
logs are in `can-fire-summary.json` and `can-fire-*-{red,restored}.log`.

## Pinned original TCK syntax inputs

`scripts/cypher-parser-fixtures.py` checks local openCypher HEAD
`007895aff5f33097d67b2e48a0a2babd6bd18590`, verifies every selected source file
against its Git object, and regenerates/checks byte-exact query excerpts. The
fixture retains original copyright/attribution and the Apache license.

```sh
python3 scripts/cypher-parser-fixtures.py /private/tmp/opencypher-graph-research-20260916
CARGO_TARGET_DIR=target/ze54 cargo test -p zeppelin-embed-cypher --test parser selected_original_tck -- --nocapture
```

The 99 selected scenarios supply 99 tested statements and 56 setup statements:
**153 syntax parses and two syntax rejections**. The exact rejected originals
are `clauses/match/Match1.feature [6]` (node whole-map pattern parameter) and
`clauses/match/Match2.feature [8]` (relationship whole-map pattern parameter).
Both original scenarios expect compile-time `InvalidParameterUse`; the
accepted profile explicitly rejects a parameter as the whole property bag.
The parser currently reports Unsupported, so this is **not a claim of matching
the original TCK semantic error category**. Binder mapping remains required.
Relationship-uniqueness and connected-node-delete original error scenarios
parse and still require later semantic/runtime errors. They are not substituted
with parser rejection. Original result/side-effect expectations were not run;
**zero original TCK conformance scenarios are claimed passing** here.

## Fuzzing and regression gates

```sh
CARGO_TARGET_DIR=target/ze54-fuzz cargo fuzz run cypher_parser -- -max_total_time=60 -max_len=65537
```

The excluded tooling target feeds arbitrary bytes through bounded UTF-8
admission and the real parser, then validates every successful AST's source,
node count, spans and referenced children via the iterative visitor. First run:
5,715,828 executions in 61 seconds, no crash. After review fixes, the final run
included original TCK statement seeds, Unicode/I64/search and malformed/overlong
boundary seeds: **3,619,415 executions in 61 seconds, no crash**. This is a
bounded local fuzz observation, not exhaustive correctness. The preserved
fuzz logs contain command/build prefix, INITED/DONE counters and terminal count.

```sh
CARGO_TARGET_DIR=target/ze54-coverage cargo llvm-cov -p zeppelin-embed-cypher --tests --fail-under-lines 90 --json --output-path /tmp/ze-54-evidence/coverage-final.json
CARGO_TARGET_DIR=target/ze54 cargo clippy -p zeppelin-embed-cypher --all-targets -- -D warnings
CARGO_TARGET_DIR=target/ze54 cargo fmt --all -- --check
CARGO_TARGET_DIR=target/ze54 RUSTDOCFLAGS='-D warnings' cargo doc -p zeppelin-embed-cypher --no-deps
CARGO_TARGET_DIR=target/ze54 cargo deny check
CARGO_TARGET_DIR=target/ze54 cargo tree -p zeppelin-embed-cypher --edges normal
CARGO_TARGET_DIR=target/ze54 cargo test -p zeppelin-embed-ffi --test ffi_header
```

Crate line coverage: **1,514 / 1,554 = 97.43%**. Per-file raw counts are in
`coverage-summary.json`. Strict crate clippy, workspace format check, strict
crate rustdoc and cargo-deny all passed. Cargo tree contains only this crate.
Header drift: one passing exact-cbindgen-output test; the two explicitly ignored
manual full-build checks were not claimed run. `cargo package --list
--allow-dirty` includes both source-license notices; this internal crate remains
`publish = false`.

The existing adversarial overall smoke was run before completing this frontend
and again after product changes, using the same worktree-local target:

```sh
CARGO_TARGET_DIR=target/ze54 cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests smoke -- --exact
CARGO_TARGET_DIR=target/ze54 cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests smoke -- --exact --nocapture
```

Baseline: one smoke passed in 88.01 seconds. Post-implementation core smoke:
**224 unique seeds, 0..223, zero violations, terminal GREEN in 89.23 seconds**. These exercise unchanged core behavior; frontend failures are
covered by the separate actual-parser schedules above. Whole-workspace and
final graph integration qualification remains with the root integration run.

## Measured current frontend footprint

```sh
CARGO_TARGET_DIR=target/ze54 scripts/cypher-parser-footprint.sh
```

The tooling composes current core/FFI rlibs and the parser in a single Rust
static link with opt-level 3, fat LTO, one codegen unit and panic=unwind. A C
consumer compiled against the existing generated header actually calls the
parser and the existing core lifecycle ABI. Both baseline/frontend consumers
passed their runtime checks; the frontend query produced a nonzero AST node
count. One Rust runtime is linked, with no duplicate-symbol suppression.
Archives/binaries are stripped with `strip -S -x`; platform `size -m` sums
linkable sections excluding LLVM metadata, while `du -k` reports physical size.

| Current artifact | Linkable bytes | Physical KiB |
| --- | ---: | ---: |
| Core/FFI C baseline | 2,544,086 | 2,564 |
| Same C consumer calling frontend | 2,630,094 | 2,644 |
| Standalone parser probe static archive (including runtime) | 426,450 | 2,100 |
| Composed baseline stripped static archive | 2,694,105 | 4,892 |
| Composed frontend stripped static archive | 2,781,515 | 4,996 |

Observed C-consumer increment: **86,008 linkable bytes**. Observed composed
archive increment: **87,410 linkable bytes**. These are current parser-reachable
**lower bounds**, not a shipped graph artifact, final 5,120 KiB gate acceptance,
or evidence for missing binder/lowering/core graph code. ZE-107 and release
qualification must measure the actual opt-in graph artifact and shipping
consumer. The probe's private symbols exist only in tooling fixtures.

The first tooling attempt exposed Bash 3 empty-array nounset behavior; fixed by
portable conditional array expansion. A second attempt linked two Rust
staticlibs and failed on duplicate runtime symbols; fixed by one composition of
rlibs before the C link. Neither failure was hidden or accepted as a footprint
result. `footprint-final.log` contains the successful runtime and raw size output.
