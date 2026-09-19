# ZE-123 independent bounded review

Verdict: no production correctness blocker found. One documentation correction
was identified and accepted by root: OptionalApply must distinguish an actual
left-node correlated anchor from an independent right DAG's shared-slot equality
semantics. Root is updating that wording; no new validation rejection is needed.
Terminal checks after root's documentation/mutation restoration remain root-owned.

## Frozen inputs and method

Verified all seven SHA256 values in `/tmp/ze-123-review-1/manifest.json` and compared
each file with base7028a42f2f4898bb7aa729dc935bd51b4033f2ba. Read the live ZE-123
contract, ZE-122 adjacency/compiler scheduling contract and execution OptionalApply
requirements. The reviewed core source was the immutable snapshot throughout;
root's moving core files/mutants were not read or changed. Full diff is saved at
`/tmp/ze-123-independent/all.diff`.

After explicit restoration notification, additionally reviewed the finalized
`crates/zeppelin-embed-cypher/tests/pattern_contract.rs`, SHA256
61d13a6281401c98fa418011e887c9a666eee8ba6722100a03b22baf9e789e59.
Verified both ownership-probe source hashes and that their prefix is the exact
helper source, not a simplified stand-in. Receipt:
`/tmp/ze-123-independent/ownership-proof-audit.json`.

This is a source/evidence review, not an independent rerun claim. No code/index,
tracker, claim, main, worktree or broad-suite mutation was performed. Only these
/tmp review artifacts were written.

## Core contract checks

- Expand and BoundedExpand now borrow exact-name OR slices. Preflight accounts
  the complete GraphName descriptor array plus every name's retained bytes,
  polling each item. Empty slices are unrestricted and duplicate alternatives
  remain metadata with an explicit no-row-multiplication obligation for execution.
  Backing omission tests separately remove the array and both distinct names.
- EdgePredicate contains a private current-edge SlotId and expression ID.
  Validation clones the input environment, adds a nonnullable REL, rejects
  collisions with any input and either new output, and validates the Boolean/null
  expression in that temporary scope. The actual output never receives this slot.
  Output-only/foreign references fail; a following Filter cannot reuse it.
  Seen-expression accounting covers the predicate without skipping scope
  revalidation. Existing path bounds still enforce min<=max<=16.
- Null rejection and no predicate evaluation for zero-hop paths are runtime
  obligations documented by the typed contract. This code proves their metadata
  and valid expression scope; it does not execute paths. Keeping validation of a
  zero-hop predicate is correct and does not claim runtime edge evaluation.
- NodeFacts::slot_at exposes ordered (logical SlotId, kinds) through checked
  ordinal access; it does not reinterpret an ID as an offset or permit mutation.
- Existing relationship-origin lineage is unchanged. It follows both fixed
  and bounded public relationship outputs, projections/renames, and dependency
  origins. Same-Pattern relationships from different origins remain rejected;
  preserved shared origins and distinct PatternIds remain accepted. The new
  private edge is not a public relationship output and introduces no lineage.
- OptionalApply's existing merge_scope preserves shared left kinds during null
  extension, checks compatible shared bindings, and null-extends only right-only
  slots. The initial new doc overclaimed that every right DAG contains its left
  anchor; the validator does not enforce that. Root explicitly retained the
  existing general DAG acceptance and will document the two cases: a real shared
  left node is rebound per left row, while independent right graphs require
  shared-binding equality and full candidate/predicate failure before extension.
  The compiler tracer uses the former directly; no silent overwrite is implied.

## Scoped compiler consumer

The representative query genuinely enters compile_in and consumes final BoundQuery.
Its sparse expressions and real slots are remapped into separately charged copied
source/name/type/expression/operator/input/projection/span backing. The entire facts
Vec capacity is reserved/reconciled; the inventory itself is charged. Compiler
owners coexist with all copied owners under the same QueryMemory/GraphResources.
Compiler and caller buffers are absent from the copied plan's address inventory,
so the borrowed-compiler-name negative control is meaningful.

The actual six-node DAG uses Unit -> Scan -> fixed Expand as left anchor; bounded
Expand directly depends on that same left Expand. OptionalApply takes that pair,
retains its WHERE, and final Project retains all five RETURN bindings. Literal
assertions cover both OR lists, PatternIds0/1, the current edge property predicate,
nullable right outputs/nonnullable inherited left outputs, and exact source spans.
This is an honest single-shape tracer rather than a claimed general lowerer.

The consumer is higher-ranked over independent plan/facts lifetimes and its result
T cannot retain either borrow. Exact helper-plus-probe sources demonstrate positive
usize return and rejected GraphPlan escape; the recorded errors specifically name
both lifetime escapes, rather than an unrelated compile error. Cancellation occurs
inside the consumer after construction and a final checkpoint rejects success;
one-byte-too-small capacity rejects before consumer invocation. Both return the
same owners' reservations to baseline and then release the shared store charge.

The reported 98245 frontend +121655 additional plan =219900 simultaneous bytes,
14207 heap bytes and one-byte-tight219899 refusal are supported by the source
and terminal logs. Conservative scratch/frame reservations are explicitly named.
They are not evidence of native read-view admission or whole-query production
lowering. Test-only panicking shape assumptions are scoped to this controlled
fixture and are not added to production code.

## Oracle and fault evidence

The PG6 extension calls real GraphPlan validation. Its independent primitive set
model distinguishes input, temporary edge and output environments and compares
observed acceptance, with deliberate corrupt-observation controls. The late clock
fault uses actual measured validation work and asserts its exact location, fire
count and same-seed clean-work separation. Source/metadata assertions and retained
backing checks supplement that model; none is represented as traversal results.
Recorded expanded PG6 tests pass3/3 and an actual runner episode reports59
operations/0 violations. The consumer logs record2/2 GREEN, two RED100 mutations
(omitted OR alternative and borrowed compiler name) with byte-exact restoration,
and positive/negative ownership compilation. Root separately supplied four core
production-mutant RED100/restoration records; terminal root checks follow.

## ZE-124 correspondence

Independently reviewed ZE-124 production geometry was already cleared in
`/tmp/ze-124-independent-review.md`; its one allocation-test constructor error
was corrected and focused allocation checks passed. Candidate06de2eb1942a75917a3f20ce4c7eb988238e461e
retains all22 frozen source hashes; production adjacency mod/codec/merge match
the reviewed immutable snapshot exactly. Source shape is ready for additive main
integration with ZE-43 tags11/12 plus adjacency13/14. Root still owns integrated
checks and ticket closure; no whole-participant credit follows from PG12 models.
