# ZE125 bounded root review

Disposition: one signed-oracle error fixed; no outstanding concrete blocker in
reviewed component scope. Root reviewed immutable snapshots1/2/3/5 against
base7028a42 and the accepted ZE122/125 contracts. All20 snapshot3 hashes were
verified; snapshot5 additionally contains formatting-only changes. This is a
source/evidence review, not a rerun of the owner's whole focused test inventory.

## Production scope

Read eligibility, schemas/rows/map, blocking/sort/distinct/aggregate, retained
operator factory and actual shared drain/owner changes. The kernels consume
already evaluated typed columns and resolve logical slots explicitly; they do
not invent expression/property/storage producers. Null filtering, offset/limit,
query equivalence with collision equality, stable comparison order, complete
input aggregation, per-list expanded descendants/depth and exact full-ID sets
are preserved. Eligibility uses one packed retained ID arena plus temporary
same-account hash scratch; every duplicate is examined/charged while only the
materialized unique set has524288cap. AllIndexed versus empty Set remains distinct.

Sort/order/group links and input/output/scratch owners overlap explicitly;
consuming blocking operations cannot return successful partial output on error.
Scoped RowBatch membership binds actual query memory/view, and lifetime-bearing
operators remain inside the owned runtime view. The GAT factory uses the same
drain as the stateless adapter, preserving eager source order, completion,
close-first final checkpoint and sole completed-counter authority.

The owner's self-audit corrected prepare_search as well as pull to the bound
execution lifetimes; the compiled retained eager buffer test fills actual owned
rows for calls1/2 before LIMIT0 and tests failure before any pull. No fresh
independent context or uncharged retained buffer is used. This correction is
required to support real search producers; the component test is not such a
producer's execution acceptance.

## Found and repaired

Review2 primitive ordering oracle converted signed inputs to unsigned transport
before sorting, so mixed negative/positive values had the wrong expected order.
Root reported literal[-3,1,-3]; owner observed runtimeRED100 and fixed ordering
in signed i64 before encoding. The actual seeded probe now spans-15..15 and
uses positive full-width node IDs separately. Same-seed fault controls remain
GREEN. Root independently compiled the exact final std-only oracle and checked
mixed signed values, i64 extrema, distinct order, high-ID set order and fault
ledger refusal: exit0. Commands/source/probe hashes are in signed-proof.json.

Inspected owner tests cover sparse logical slots, bags/stability/collisions,
null/global/grouped aggregates,600-row composed scheduling,70k intermediates,
16-to17 nested depth refusal, every observed allocation failure, full524288-ID
owner and one-over, same-view refusal, actual counters and close/cancel controls.
The PG14 oracle uses primitive std expectations and actual kernel calls;
registry/runner routing and four-seed clock fire/clean ownership release checks
are present. Source mutations and terminal exact-restoration evidence remain
owner evidence, not newly rerun campaigns. Final integration tests remain root-owned.

## Boundaries

ZE51/52 must bind real validated plans, evaluate actual expressions/property/
stored-text producers and eligibility under the admitted view. ZE53/64 retain
real completed ownership/search reports and coordinator semantics. Factory
fixtures and synthetic rows prove component composition, not GraphStore/TCK.
No broad suite, product retrieval, recovery or performance qualification was
run for this review; ZE118 and original dependent acceptance remain required.
