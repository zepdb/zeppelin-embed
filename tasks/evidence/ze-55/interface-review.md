# ZE-55 binding interface review

Initial design, 2026-09-19. This is not implementation or acceptance evidence.
Worktree base: `34b59f5748fcb5f9691f2f8e1270fa0196bd451e`.

The live ZE-55 scope owns profile admission, symbols, scopes, types, named
parameter validation, source errors, and literal/property conversion. ZE-56
owns read operator lowering; ZE-57 owns mutation/coordinator lowering; ZE-58
owns search procedure eligibility and operator lowering. Their implementations
must consume the binding facts instead of reparsing or silently rejecting a
supported form. Root has been asked to confirm this boundary before freezing
the source interface. No later operator is claimed implemented by this review.

## Proposed private seam

One complete-input operation parses and binds, then invokes a scoped callback
only on success. Its borrowed bound view retains source, exact AST nodes,
per-expression symbolic slot/parameter identities and types, source spans,
ordered output columns, clause scopes and mutation/search classification.
It owns no store handle, catalog identity, writer, lease or reusable public
prepared query. A lifetime-generic callback cannot return a borrowed bound
object. An ordinary owned test observation can leave the callback.

The binding representation adds semantic facts to the existing flat AST;
it does not duplicate syntax, erase type alternatives, detach OPTIONAL MATCH
predicates, discard update order, or lower unsupported core operators to
generic rejection. Symbolic names remain exact UTF-8. Unknown labels/types
remain symbolic storage-admission names, distinct from unknown variables.

Core expression conversion uses the existing typed Expression/Unary/Binary/
Aggregate definitions. No comparison or arithmetic evaluator is cloned into
the compiler. Full-width ID functions map to NodeIdText and RelIdText;
StoredText remains separate from ordinary property access. Parameter values
retain F64 bits, and no constant folding moves arithmetic-domain failures to
an unrelated phase. The core's to_property conversion provides canonical
EmptyList, homogeneous exact scalar lists, and null removal semantics.

The first vertical tracer is a scalar RETURN bound to a validated core plan,
through the same private one-shot lifetime. General read/write/search operator
execution and result/side-effect assertions remain independently recorded as
pending their owning tickets. Positive binding is never called execution.

## Resource handoff under review

Synchronous caller source and parameters are borrowed; their actual capacities
are not inferred from slices or claimed prepaid. Every allocation created by
the frontend (source copies, decoded tokens, child vectors, bound facts, scopes,
expression storage and descriptors) is reserved before allocation, reconciled
against actual capacity, and charged during old/new growth overlap. Every
walk, scope/name comparison and parameter/list visit polls within bounded
work. CompileLimits may only tighten the established profile bounds.

ZE-49's validate_with_fact_vec captures actual full NodeFacts Vec capacity;
raw-slice GraphPlan validation and PlanFootprint declarations do not establish
runtime ownership. Runtime QueryInputs requires lifetime-bound complete owner
capabilities and unions aliases. ZE-55 must neither duplicate these accounting
proofs nor charge a frontend-owned allocation twice during transfer. The
existing parser Resources interface is a conservative cumulative account;
the exact owned-arena handoff is being coordinated with ZE-49 before runtime
integration. No new independent per-store budget is authorized.

## Source checks

The following local source HEADs were verified before implementation:

- openCypher M23: `007895aff5f33097d67b2e48a0a2babd6bd18590`.
  Read the selected Match3[29] relationship-uniqueness compile error, basic
  grammar literal/parameter productions and comparison CIP numeric rules.
  The accepted local profile deliberately narrows the larger grammar.
- Kuzu: `89f0263cc7a1fd9c396d2c4953747a013556a7f9`.
  Read bind_graph_pattern.cpp:634-654 and bind_updating_clause.cpp:159-178.
  Their schema table lookup, required primary key and single-label creation
  are dialect differences, not behavior to copy into this binder.
- Existing Shopify adaptation remains pinned by ZE-54's notices. This ticket
  does not add Shopify as a dependency or inherit its execution semantics.

Broad original-TCK execution, whole-workspace/adversarial qualification,
per-crate coverage and footprint campaigns are deferred to ZE-118/E12 by the
current scheduling instruction. Directed checks needed for this source commit
remain required and will record literal RED/GREEN separately.

## Confirmed implementation boundary

Root confirmed the ZE-55/56/57/58 split and the opaque grow-only
QueryExternalReservation seam. The guard shares QueryMemory and GraphResources,
outlives all frontend backing, and supplies no owner capability or alias credit.
The integration-only RETURN tracer separately copies its reachable retained
inputs into charged QueryArena owners and validates an actual facts Vec.
Execution must still obtain QueryInputs owner proofs or charged copies for
every retained caller input; binding success grants no runtime admission.

AST-indexed bound expressions retain syntax holes for exact source mapping.
Synthesized expressions have an independently bounded allowance; later lowering
prunes/remaps reachable expressions before the exact GraphPlan node limit.
Node type alternatives, bounded relationship predicates, complete hybrid yield
fields and all four search modes remain exact in syntax/call facts. Existing
core operators cannot yet express all those forms; ZE-56/58 own those additions.

Count of deleted entities and scalars copied before deletion bind positively.
Returning/collecting provably deleted entities and reading deleted properties
reject. Identity functions and size of deleted-reference collections retain a
flag requiring later dynamic deleted-access validation in ZE-57; binding does
not promise their positive runtime semantics.
