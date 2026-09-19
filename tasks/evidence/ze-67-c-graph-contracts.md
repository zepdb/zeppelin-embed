# ZE-67: versioned C graph contracts

Candidate base: `f9d81e0806ca68c876a26ed1983a3fffaf72b902`.
Date: 2026-09-19. Host: Apple M3 Max, 128 GiB, macOS 27.0 arm64;
Rust 1.93.0, clang 21.0.0, cbindgen 0.29.4, nextest 0.9.145.
The exact inventory is [hardware.json](ze-67/hardware.json).

This ticket supplies the graph data contract, generated C declarations and
allocation-free scalar validation/error mappings. It does not add graph
runtime exports, pointer traversal, execution, response allocation/free,
publication, packaging or a working graph facade. Those remain ZE-68/69/107.
The default artifact retains its existing header and symbol set, with generic
error numbers appended. This host is not macOS 14 runtime qualification.

## Implemented contract

The separately generated `zeppelin_graph_contracts.h` defines 40 C structs and
19 discriminant enums with 141 pinned values. Strong node/relationship IDs
remain distinct 16-byte high/low structs; graph handles are an 8-byte wrapper.
Conversions retain the full u128 value and reject zero. C rejects assigning a
relationship struct to a node struct. The bits alone do not establish an ID's
store or entity origin.

Versioned fixed-stride descriptors cover value/entity pools, typed plans and
batches, caller controls and limits, open/query/Cypher/entity-get requests,
completed results, receipts, search reports, work counters and diagnostics.
Inputs use integer tags, checked ranges and pointer/count arrays. Shape
helpers explicitly stop before pointer or semantic admission. Empty graph
names/keys, embedded NUL, absent versus empty text/vector, full-width IDs,
typed empty lists, and the zero-count-only EmptyList sentinel remain distinct.

Search representations retain omitted/Auto/Exact/Scan/Graph tiers, optional
hybrid component slots, same-view eligibility, OR type lists and bounded
per-edge predicates. Document interpretation belongs to open; query tower and
alignment remain query options. Explicit query/work and all eight compiler
limits distinguish zero from absence. Graph runtime owners must preserve these
accepted semantics even where the current core IR still needs extension.

Error values 0 through 34 and their names remain unchanged; values 35 through
54 are appended. Swift's error enum and the legacy header are regenerated.
The graph contract module is default-disabled and refuses unsupported
OS-family/architecture selection. ZE-107 still owns complete feature
forwarding, the macOS 14 deployment floor, packaged inventories and symbols.

## Focused evidence

The terminal graph-feature command was:

```sh
cargo nextest run -p zeppelin-embed-ffi --features graph-cypher \
  --test ffi_graph_contract --test ffi_graph_error \
  --test ffi_graph_header --test ffi_graph_layout \
  --test ffi_contract --test ffi_header
```

Result: **36 passed**, including 22 graph tests and 14 existing contract/header
tests. Two preexisting manual release-build header tests were ignored; their
deferred command is below. The seven selected default-feature regression
checks also passed. The exact argv, exit status and raw-log paths are in
[commands.json](ze-67/commands.json). Nextest used the committed default
profile: four isolated test processes, one libtest thread per process and no
retry. These counts are test counts, not unique acceptance requirements.

The graph suite compiles and runs actual external C consumers against the
generated header, rejects cross-kind assignment, compiles a C++ consumer,
checks independent clang-measured sizes/alignment/field offsets, pins every
discriminant, and checks both generated headers exactly. The C fixtures test
representability; their example requests are not admitted or executed plans.
The C++ test caught the C++ keyword `operator` in a diagnostic member before
freeze; it is named `operator_index` in the final C/Rust contract.

Strict scoped clippy (`--no-deps -- -D warnings`), workspace format check and
`git diff --check` pass. `Cargo.lock`, the legacy symbol allowlist, cbindgen
config, release header trim script and existing header tests are byte-identical
to the base. [restoration-audit.json](ze-67/restoration-audit.json) records those
hashes and current hashes matching every scalar/layout mutation restoration.

## RED and restoration

Initial RED records are retained under [raw/](ze-67/raw/). The external C ID
consumer failed because the graph header was missing. Batch/plan/result C
consumers subsequently failed for missing declarations. ID conversion,
shape-helper and native-error tests first failed compilation because their
APIs were missing; these are compile RED, not runtime semantic failures.
The appended-error runtime test observed `ZE_ERR_UNKNOWN` instead of
`ZE_ERR_STORE_KIND` before the new mapping was implemented.

Deliberate negative controls then fired at named assertions for swapped ID
offsets, changed EmptyList tag, accepting nonempty EmptyList, accepting a
larger nested-array stride, wrong arithmetic error mapping, accepting inactive
negative-zero bits, losing explicit search-tier presence, graph header drift,
legacy header graph advertisement and cross-kind C type collapse. The seven
production mutations have exit 100 and before/restored hashes in
[mutants.json](ze-67/mutants.json); two header controls have the same record in
[header-mutants.json](ze-67/header-mutants.json). The cross-kind control's raw
failure is `cross-kind-mutant-red.log.gz`. Terminal tests ran on restored bytes.

Earlier `clippy.log.gz` records a redundant test semicolon, corrected before
the resumed strict pass. Earlier `focused-final-green.log.gz` actually records
an invalid nextest binary filter and is not GREEN evidence. The authoritative
terminal records have the `resumed-` prefix. Keeping these failed invocations
avoids silently treating an attempted check as a passing one.

## Review and remaining qualification

[proposal-independent-review.md](ze-67/proposal-independent-review.md) and
[schema-v1-root-review.md](ze-67/schema-v1-root-review.md) record the independent
proposal and root schema reviews. Their findings led to full tier/component
representation, document/query interpretation separation, empty/NUL name
support and explicit memory/work limits before freeze. Final candidate source
hashes are in [candidate-source-inventory.json](ze-67/candidate-source-inventory.json).
[schema-v2-to-final.diff](ze-67/schema-v2-to-final.diff) records the final
diagnostic member rename. These historical reviews do not replace root's
final candidate review and main integration checks.

ZE-118 retains broad workspace/adversarial/coverage/release qualification.
For these changes, the two ignored existing release-header checks are:

```sh
cargo nextest run -p zeppelin-embed-ffi --test ffi_header --run-ignored only
```

They are `the_committed_header_matches_the_exported_symbol_table_and_the_allowlist`
and `header_gate_passes_twice_in_a_row_after_an_instrumented_build`. Both build
real release/text archives; the second also exercises instrumented-cache
isolation across release target directories. They were not run here. No
release symbol, size, minimum-OS, runtime marshalling, allocation, lifecycle,
recovery, fuzz, sanitizer, Miri or broad adversarial acceptance is implied by
this contract-only ticket. Actual graph runtime fault coverage belongs with
the later owners that implement those paths; no operation ordering changed.
