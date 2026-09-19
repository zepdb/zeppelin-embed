# ZE-48 bounded typed-plan review

Snapshot: hashes.json. All copied files were stable during their individual copy.
No source/worktree/target/tracker edits. Plan arithmetic/value code was previously
reviewed separately; this pass focuses on borrowed accounting, scope, aggregates,
write/read/search barriers and relationship origins. This is not final acceptance.

## Findings

1. Actual retained-byte accounting overcounts aliases. plan/validate.rs:95-107 and
164-169 sum each visible string/name occurrence independently. Three literal cells
that share one 8MiB backing string exceed the 24MiB sum once descriptors/facts/stack
are included, though a truthful retained-capacity declaration is about 8MiB. A
visible-span sum with overlapping ranges is not a lower bound on retained bytes.
Owner acknowledged and is adding named RED plus a reviewed range-inventory seam.

2. Same-pattern relationship origins can be laundered through a join.
plan/validate.rs:588-599 preserves only left metadata for matching slots. A left
LookupRelationship(r), whose pattern is None, joined with right Expand(r, pattern7,
originA) drops originA. Joining independent Expand(r, pattern7, originB) then admits
a repeated same-pattern relationship binding. Direct originA/originB join rejects.
The accumulated pattern/origin obligations must survive compatible joins.

3. Compatible nullable shared slots reject. plan/validate.rs:593 requires exact
ValueKinds equality. OptionalApply(lookup,expand) makes new m/r bindings nullable;
an inner Join with that same expand then rejects Scope even though the origins
match and NODE|NULL/NODE and REL|NULL/REL are compatible. The inner match should
exclude null-key rows, deriving correct output kinds, rather than reject on
nullability alone. Retain incompatible-kind and conflicting-origin controls.

## Disposable reproduction

origin_probe.rs uses public GraphPlan APIs and a frozen copy of the worktree's
existing compiled rlib (SHA256 recorded below); it does not rebuild or alter any
worktree. The snapshot source independently confirms the implicated guards.

nullable shared-origin inner join: Err(Scope)
direct duplicate: Err(Scope)
lookup-laundered duplicate: Ok(())

Frozen rlib SHA256: eab2ef41dd1a7387c96b8bae35e9fc33afda826fb001fa9728a830d619d51b68

No further concrete finding in SearchBounds handoff, eager inventory/order,
singleton aggregate inputs, global read/write/search classification, expression
scope revalidation or bounded expression traversal. Hidden allocation capacities
remain a truthful owner-attestation/ZE49 responsibility; dynamic search bounds,
vector/eligibility contents and actual execution must be checked by later runtime
owners as explicitly documented, and are not claimed implemented by validation.
