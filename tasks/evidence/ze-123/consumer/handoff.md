# ZE-123 scoped pattern-consumer handoff

Owned repository change only: `crates/zeppelin-embed-cypher/tests/pattern_contract.rs`.
SHA-256: `61d13a6281401c98fa418011e887c9a666eee8ba6722100a03b22baf9e789e59`.
No source/index/commit/tracker edits outside that assigned test. Root owns the
core contract and adversarial changes in this same worktree.

## Implemented bounded proof

One representative query is compiled with real `compile_in`:

```cypher
MATCH (a)-[r:BASE|ALT]->(m)
OPTIONAL MATCH (m)-[p:FIRST|SECOND*0..2 {weight:7}]->(b)
WHERE b.ok=true
RETURN a,r,m,p,b
```

The test-only helper consumes final BoundQuery, resolves real bound slots and
reachable sparse expressions, copies every retained name and the complete source
into QueryArena backing, remaps expressions, constructs the per-edge property
predicate under a fresh private REL slot, and copies OR alternatives, projections,
operator inputs/operators, and operator/expression source spans. The actual
NodeFacts Vec is reserved before allocation and reconciles its full capacity.
The inventory is itself a charged QueryArena; validator scratch, lowering scratch,
fact Vec descriptor and plan/control metadata are reserved separately. Compiler
and all copied plan owners coexist under the same QueryMemory/GraphResources.
Compiler/caller buffers are not included in the copied plan's address inventory.

The exact correlated DAG is Unit -> Scan(a) -> fixed Expand; the right bounded
expansion directly depends on that same left Expand, and OptionalApply has
[left Expand, right bounded Expand] inputs. The attached WHERE expression remains
on OptionalApply, followed by all five RETURN projections. Literal checks pin
both complete OR lists, fresh PatternIds0/1, private current-edge scope, weight=7,
optional b.ok=true, nonnullable inherited a/r/m, nullable p/b, output slot order,
and exact original source fragments. This proves contract construction and
facts, not runtime execution, a general lowerer, row results, or TCK acceptance.

The genuinely higher-ranked consumer takes GraphPlan with independently bound
plan/fact lifetimes, copied source and source-map borrows. Positive/negative
rustc probes use the exact helper source plus a final probe function. Returning
usize compiles; returning GraphPlan fails with lifetime diagnostics. This is
new-owner-seam proof, not the existing compile_with doctest. `lifetime-proof.json`
pins the helper, probe source bytes, exact rustc arguments and linked rlib hashes.
The probe source and logs remain in this directory for root review.

## Focused results

- Initial test-only scaffold runtime RED: complete RETURN width0 !=5. This is
  a missing-tracer assertion, not an engine product failure.
- During construction, a Unit input omission caused PlanError::Arity; then an
  incorrect expected syntax span omitted arrow punctuation. Those are harness
  corrections (`build-1.log`, `build-2.log`), not product RED evidence.
- Deliberate source mutation dropping SECOND from the bounded OR list exits100
  at the literal completeness assertion.
- Deliberate compiler-borrowed property name substitution exits100 with
  PlanError::Footprint. This demonstrates real copied-owner inventory enforcement.
- Both mutations restored byte-for-byte; exact hashes/commands in `mutants.json`.
- Terminal focused nextest2/2 GREEN in `terminal-green.log`.
- Scoped strict Clippy passed in `clippy.log`; owned rustfmt/diff checks passed.
- Positive ownership compile probe exit0; escaping GraphPlan probe exit1 with
  lifetime errors, in `positive.log`/`escape.log`.

Commands:

```sh
cargo nextest run -p zeppelin-embed-cypher --test pattern_contract \
  --test-threads 4 --success-output immediate-final
cargo clippy -p zeppelin-embed-cypher --test pattern_contract --no-deps -- -D warnings
rustfmt --edition 2024 --check crates/zeppelin-embed-cypher/tests/pattern_contract.rs
```

Actual reported reservation/capacity numbers for this query:

| Quantity | Bytes |
|---|---:|
| Live compiler/query baseline at consumer entry | 98,245 |
| Additional copied plan owners/control/scratch | 121,655 |
| Copied heap including inventory and fact Vec | 14,207 |
| Simultaneous query reservation / measured peak | 219,900 |
| Query cap | 1,048,576 |
| Tight refusal cap | 219,899 |
| Store configured aggregate cap | 4,194,304 |

Lowering uses 4096-entry bounded remapping arrays, 64KiB validator scratch, a
separately charged conservative 4096-byte small-frame envelope, six operators,
and an 8,000,000-unit validation-work bound. The returned borrowed plan contains
no admission or actual native view capability. Real runtime integration must
retain its own view/producer/resource proof. Root owns the actual changed-path
PG6 runner checks; no broad suite or product qualification was run here.

The one-byte-too-small query cap rejects before calling the scoped consumer.
Cancellation is deliberately triggered inside the consumer after construction;
the final checkpoint returns a typed cancellation instead of its success value.
Both cases release all copied/temporary charges back to QueryMemory baseline,
and dropping QueryMemory returns the same store owner to its initial accounting.
