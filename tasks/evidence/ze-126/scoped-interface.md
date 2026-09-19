# ZE-126 compiled read-lowering seam

Base main 736e8440221d4b53d797637e61840b53d0cdc7c7, with immutable ZE-125 source 05a49fa imported as beb1a1e44730b56494ec2cb4aafc5d9ea5419763. Complete supported-form lowering is implemented, with directed checks, seeded changed-path controls, allocator failure checks and independent reviews. This is component preparation evidence; actual native graph execution and TCK acceptance remain later-owned.

```rust
pub fn compile_read_in<'v, T, C: ReadContext<'v>>(
    source: &str,
    parameters: &[ParameterBinding<'_>],
    limits: CompileLimits,
    memory: &QueryMemory<'_>,
    context: &mut C,
    consume: impl for<'plan, 'facts>
        FnOnce(LoweredRead<'plan, 'facts>, &mut C) -> Result<T, ParseError>,
) -> Result<T, ParseError>
```

`ReadContext` is sealed to the actual `ValueContext` and `RuntimeContext`. The ValueContext path is symbolic pre-admission preparation. The RuntimeContext path checks exact memory identity and passes the same mutable runtime account through lowering, admission and existing `execute_in` drain. No new value/runtime account, view or lease is created. `PreparationControl` privately derives its original caller control and retained adapter from the actual context; compiler parser/binder/lowerer checkpoints check retained view before caller control. Public `compile_in` retains its original raw QueryControl contract.

`LoweredRead` privately contains a validated GraphPlan, copied source/output metadata/operator and expression spans, copied parameter bindings, and actual retained allocation capabilities. Every parameter string/name and nested list level is copied. Read-only accessors are available only in the HRTB callback. The fact storage is an actual QueryArena<NodeFacts>; its specialized validator returns both GraphPlan and an authentic same-memory, full-capacity fact owner. The complete owner inventory includes that fact capability. No numeric prepayment or borrowed frontend/caller backing is credited. No reusable prepared handle or admission authority is created.

Builder buffers grow by allocating a replacement QueryArena while the old arena remains charged, polling each copied element, then dropping the old arena. Compiler, drafts and final borrowed native arrays overlap in the same QueryMemory/GraphResources. Validator scratch, region/capability inventory, control descriptors and source maps are reserved. Parameter values freeze deepest-first into distinct actual arenas, with immutable child borrows and no self-referential owner or unsafe conversion. Current fixed limits remain native 4096 plan nodes, 64 expression depth, 256 row slots, path maximum 16 and 16 list nesting levels; profile/parser/binder limits apply first.

Lowering consumes final BoundQuery facts/projections/scalars; it adds no parser or interpreter. Each MATCH has one PatternId across comma parts, fresh for the next MATCH. OPTIONAL uses the actual left DAG anchor and keeps the complete predicate before null extension. Reuse gets fresh candidates plus explicit equality/reprojection. Ordinary ordering temporarily preserves required hidden input slots, then removes them; aggregation/DISTINCT uses only available output keys. WITH predicates and limit barriers retain their specified stages. Sparse and synthesized expressions are copied/remapped with source maps. Plan/control errors remain typed rather than being flattened into binding failures.

The nonsearch read seam explicitly rejects accepted CALL with SearchContext and ZE-58 identified; BoundQuery call/mode/eligibility/YIELD metadata remains unchanged. Mutation lowering is separate. Controlled runtime evidence uses a literal producer extracting one scalar from the actual compiled root, and executes through the real driver and Completion. It proves the owner/account seam, not a native graph operator implementation. The controlled trace retains prior value work 11, then adds exactly six execution steps after preparation (three producer comparisons, two actual row copies and one completion comparison). Preparation work varies with actual backing address ordering in heapsort and binary search; raw runs record their individual prepared/final counters rather than promising an address-independent absolute count. A near-limit branch fails at the same cumulative limit instead of resetting. The fact capability saves exactly the actual fact capacity compared with a conservative plain plan_facts certificate. A real retained-view close plus simultaneous caller cancellation fires in the parser, before validation, and yields ReadCancelled.

No original TCK execution, native graph/result lifecycle or public compiler route is certified by these plan construction tests. PG17 covers actual changed lowering paths; broad qualification stays ZE-118 and original 57 read coordinates remain ZE-56. Focused checks also cover reuse, ordering, typed rejection, all scalar operation families and final output cleanup.


The reviewed `CompletedEdgePredicate` addition preserves the already binder-accepted self-list case `MATCH (a)-[r*1..2 {x:size(r)}]->(b) RETURN r`. It is a separate BoundedExpand field from prefix `EdgePredicate`. Its scope is incoming bindings plus the complete produced relationship LIST plus a fresh private REL, excluding the fresh destination node. All members must pass; false/null rejects the candidate, zero-hop evaluates nothing, and checked errors abort without partial rows. The complete list remains charged and retained during evaluation. Transitive reads of the newly produced list route the whole inline bag to this stage; ordinary input-only bags remain prefix predicates. Root recorded this compiled contract in canonical `docs/graph/plans/{parallel-contracts,execution}.md` and added the actual traversal owner ZE-50 dependency on ZE-126. Actual traversal implementation is still mandatory there.

The safe core `property_graph::checked_utf8` is the existing catalog algorithm extracted without changing catalog checkpoints/errors. It validates at most 64 KiB per window, preserves split multi-byte codepoints, and returns a borrowed string only after complete validation. The sole documented unchecked construction stays in core; the outer compiler continues denying unsafe code. Lowered copied strings use the same original close-first control during this validation.
