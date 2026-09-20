# ZE-159: complete native mutation execution readiness

**Decision: not ready for a complete independent implementation assignment from the inspected commit.** Wait for the already active ZE-154 and ZE-156 producers to land on main, then hand the full mutation execution assignment to one owner. Do not create another readiness ticket, duplicate those producers, or reduce the assignment to scalar-only/no-result mutations. Root accepted this bounded conclusion during the review.

Inspected production/source pin: `856cfa602742b4bd6eaefda14300c91e9f8753d8`, 2026-09-20. All product reads used `git show`/`git grep` against that immutable commit. No moving ZE-46/154/156 worktree was read. Tracker descriptions for ZE-159/52/53/57/127/140/154/156 supplied ownership and acceptance, not evidence of landed production. Canonical ignored main `execution.md`, `writes.md` and `parallel-contracts.md` supplied accepted contracts; their hashes below distinguish them from committed source.

This is source inspection, not compilation or behavioral evidence. No code, tests, builds, tracker mutations or further agents were used. No `/tmp/ze-159-native-mutation-plan.md` is produced because the readiness condition is false. Original ZE-52 remains blocked and unstarted, with every dependency and acceptance criterion retained.

## Exact upstream gaps

### ZE-154: real relational occurrences

At the pin, `crates/zeppelin-embed/src/property_graph/query/pattern.rs:1958` counts only Unit, native lookups/scans/expansion, joins, OptionalApply, Filter, Project, With and Collect. `occurrence_count` rejects Aggregate, Distinct, Sort and OffsetLimit with `PlanError::Reference`; `build_occurrence` has the corresponding terminal rejection at approximately line 2467. The existing `query/relational` kernels operate on evaluated typed rows but do not make these operators executable inside the recursively owned native occurrence tree. Committed `research/native-relational-after-ze152.md` documents this exact ownership restriction.

This prevents the complete mutation contract from consuming legal ordered/grouped/distinct/limited input or executing permitted following projection barriers. A decisive example is total `WITH n,m ORDER BY m.x, ze.node_id(m)` before conflicting SET rows. A final `RETURN ... LIMIT 0` is another required composition: a mutation barrier must run even when the real downstream limit emits no rows. Pretending to handle these by removing operators, manufacturing their rows, or writing another relational chain would not satisfy the assignment.

**Required actual handoff:** root-integrated immutable ZE-154 production commit plus its reviewed focused evidence and actual registered-consumer compilation. Consume Aggregate, Distinct, Sort and OffsetLimit in the existing NativePattern occurrence engine, preserving actual selected-row/Group.first relationship-use ownership, scope, optional-anchor reset, typed errors, same-view resources and cumulative counters. The compiler's current typed operators remain the inputs. No new mutation hook is required from ZE-154 and no guessed private signature is frozen here; the mutation owner reads the final compiled source after handoff. ZE-154's eligibility work may land in the same commit, but mutation execution must still reject search/write mixing rather than depend on ANN ranking.

### ZE-156: actual native rows and records to existing completed pools

At the pin, `query/completed.rs:68,135,251,346` supplies typed Pools, the ResultSource contract, checked `PreparedGraphResult::copy_from`, and allocation-free detach. It does not resolve a native row's entities or construct those pools. The committed module has no native child. Exact-pin searches for ResultSource implementations find test/test-support fixtures, not a production native row/record producer. `query/runtime/driver.rs:100,126,308` supplies PreparedRows, the Completion adapter and `execute_in`; its generic callback is not that producer. Existing pattern completions such as FreezeNodeIds/FreezePrimitiveRows are directed test observers, not owned application results.

Committed `research/native-results-after-ze152.md` explicitly identifies the missing native translation and assigns it to the active ZE-156 component. ZE-127's twelve owned pools and staging's ResultMaterializer callback establish storage and sequencing, not real entity materialization. A scalar-only or fabricated ResultSource acceptance path would leave the required returned nodes/relationships/lists, actual property/type/key/revision provenance and lifetime/error cases unresolved.

**Required actual handoff:** root-integrated immutable ZE-156 native materializer commit plus reviewed focused evidence and actual registered consumers. The handoff must translate genuine PreparedRows and checked GraphReadView records/catalogs into the existing ResultSource/Pools, retain all actual staging and destination charges through final driver checks, and detach the existing result after successful Execution. It includes the additive checked RecordView property-at-index method, authentic owner validation, typed missing/storage failures, bit-exact scalar/list/entity copying and complete row multiplicities. No new result format or parallel collector is needed. ZE-156 is a read producer: pending-overlay resolution, deleted-result policy, final mutation receipts and write outcomes remain mutation-owned integration; do not make ZE-156 invent them.

These are concrete absent production paths owned by two current implementation tickets. Neither worker-reported first GREEN nor a planning artifact substitutes for a committed producer. There is no need for a new interface-only ticket or another research chain.

## Existing real foundations, not additional blockers

All paths below are relative to `crates/zeppelin-embed/src/` unless stated otherwise.

| Inspected source | What already exists |
| --- | --- |
| `property_graph/staging/overlay.rs:46–212,386` | GraphBatchReadView, branded BatchEntityRef local/existing bindings, create/replace/delete, progressive property/text access, and final canonical normalization. Repeated replace uses one pending entity entry; finalization compares the complete final image and advances an existing entity once. |
| `property_graph/staging/result.rs:7–42,111–213` | Mandatory result layout/materialize capability, real core/ABI/registration capacity, and precommit private ownership. `MaterializedBatch::adopt_result_memory` can transfer the actual shared reservations into a consuming query owner while retaining writer-local accounting. |
| `lifecycle/native_graph/write.rs:946–1078` | Sole writer acquired before native base admission, authentic staging, NoOp/Replayed early return, checkpoint admission, and actual GraphPreparation over StagedBatch. |
| `lifecycle/native_graph/write.rs:702–944,1240–1407` | Producer-bound committed transition validation and real immutable artifact Full sync, directory Full sync, WAL append/Full sync, coherent bundle publication and infallible result ownership handoff. Post-attempt failures are CommitIndeterminate and stop admission. |
| `lifecycle/native_graph/tests/publication.rs:1630,1744,1892` | Existing real-path definite failure, attempted-commit and precommit-result/postcommit-cancellation fixtures to extend/reuse. Their presence is not new mutation evidence. |
| `lifecycle/native_graph/tests/recovery.rs:676,1171,1384` | Actual native reopen, lost acknowledgement and empty-graph identity/fence controls. ZE-40 recovery is a landed producer, not a hypothetical prerequisite. |
| `property_graph/query/pattern.rs:274,1921` and `query/expression.rs:570` | Actual native pattern pulls and scalar evaluation under one retained source/runtime/memory owner. |
| `crates/zeppelin-embed-cypher/src/lowering/mutation.rs:114,146–169` | Real scoped compile/bind lowering emits Eager followed by ordered Mutate descriptors, retaining source/item spans and dynamic-deleted obligations. This is ZE-140 lowering, with no mutation execution claim. |

## Substantive mutation-owned work after those handoffs

These are required implementation responsibilities, not reasons to open more prerequisite tickets or narrow ZE-52. Exact hunks/signatures and finite test selectors belong to the eventual implementation handoff against the integrated producer commits.

1. **Writer admission and sole protocol:** add the genuine query execution/preparation handoff under the existing writer-before-base ordering. The current entry accepts `&[StructuredWrite]` and immediately calls `stage_structured_with_results`; it does not accept a query-produced overlay. Factor/reuse the actual preparation and irreversible protocol while retaining the producer-bound transition capability. Do not add a raw next-bundle/encoded-bytes constructor, second publisher or postcommit materializer. Internal checkpoint/reprepare must not expose results, duplicate query side effects or claim a retry after attempted commit.
2. **Authentic base access for query-discovered targets:** `lifecycle/native_graph/base.rs:550` sizes/preloads its cache from explicit StructuredWrite keys, targets and endpoints; `entity` at line 797 only reads that cache. An empty request slice is not a general query base, and a cache miss is not evidence that an actual selected record/property is absent. Extend the same native checked load/accounting ownership for the bounded actual target set or a reviewed scoped acquisition path. Retain complete canonical images, membership, endpoints and authentic provenance; do not manufacture structured keys/requests or widen the whole-graph cache.
3. **Eager execution and actual local binding lifetime:** add Eager/Mutate execution to the one existing operator architecture. Freeze the complete upstream bag and computed scalars per clause under authentic charges; consume each input occurrence and textual item exactly once before a following clause observes effects. Preserve local create/endpoint dependencies using staging's branded bindings and real normalization/receipt identity authority. Do not fabricate global IDs or leak local handles through QueryValue/completed results. Resolve fresh identities and per-entity outcome metadata before result exposure, preserving consumed-ID durability for create-then-delete.
4. **Progressive evaluation without a second evaluator:** `query/expression.rs:570,620,774` currently takes GraphReadView directly and performs immutable record reads. It cannot see GraphBatchReadView. Add the narrow authentic overlay-aware entity access in the existing evaluator, preserving its scalar/list/null/arithmetic/control logic and frozen Slot versus fresh Property distinction. Preserve native owner checks for base records and charge actual pending backing. Pending labels/type/deleted state and results also need the correct existing/local identity treatment; property/text access alone is not proof of complete following-clause behavior. This permits no overlay scan, traversal, or post-update reading-clause support.
5. **Precommit result and outcome integration:** reuse the landed ZE-156 typed native materialization and ZE-127 representation with authentic overlay/final-normalized entity data. Copy every requested result, reject deleted entity objects and invalid deleted property access, reserve core/ABI/registration where applicable, and retain real query/writer/shared overlap before any WAL attempt. Publish only coordinator-authenticated NoOp/Committed outcome and receipts; no new generic Cypher retry format. Preserve typed native/expression/staging/completed/commit failures and no partial rows. Postcommit transfer cannot run fallible copies, conversions, registration growth or ordinary cancellation that reports rollback.

## Acceptance remains complete

The eventual single assignment must retain all original ZE-52 semantics and actual-native acceptance. In particular:

- Eager MATCH inputs exclude own creates and preserve pre-update eligibility; all writes run under downstream LIMIT 0 and without RETURN. Following WITH/RETURN observe the fully completed previous clause, including actual relational barriers.
- Duplicate node and relationship bindings execute repeatedly. Starting p=0, two fresh `p=p+1` updates end at 2 and return two 2s; a frozen old=0 alias assigned twice ends at 1. Textual SET item order is observable. Ordered conflicts use the explicit total order; tied/unordered conflicts use an independent oracle for the joint permissible final state and returned bag, never unique-entity deduplication or conflict rejection.
- CREATE endpoints and property dependencies, SET/REMOVE/null behavior, canonical EmptyList and complete homogeneous stored-list conversion, zero-match/equal-final-image NoOp, one final revision per changed entity, and create-then-delete allocator durability are retained.
- Plain node DELETE still rejects live incident relationships unless explicitly removed as required by staging. Node DETACH follows ZE-109's node tombstone, endpoint filtering and preserved neighbors; it does not enumerate/synthesize per-edge deletions or revisions. Relationship DETACH is ordinary relationship deletion; null target handling and retained identity bookkeeping remain valid. Dynamic deleted-property/entity-result checks cannot be skipped by a compiler flag.
- Late expression/type/list/result/sort/budget/cancellation/close failures, including failure after the second staged update, reject the entire private statement with zero partial output and released charges. Requested result preparation is part of the precommit contract, not deferred until durable success.
- Actual artifact/WAL/publication controls distinguish definite precommit rejection from attempted-commit indeterminacy. Cancellation after attempt cannot claim rollback. Lost acknowledgement and successful writes are checked by actual native close/reopen and independent exact state/result/provenance observations, not codecs, mock installers or counts alone. No automatic CREATE/increment retry is introduced.

No first mutation behavioral RED/GREEN is claimed in this review. Once the two producers land, the first focused RED must pass through an authentic native store/admission, executable typed plan, real eager/overlay mutation and existing publication/reopen boundary; a current `PlanError::Reference` caused solely by an unimplemented upstream relational operator is not evidence for a mutation correction. A complete implementation needs finite representative branch groups and an actual independent oracle, necessary narrow feature/consumer compilation, and registered runner hooks/coverage keys for changed paths. Broad/full/adversarial execution, fuzz/coverage/performance/soak/release qualification remains ZE-118; required registration and compilation is not deferred.

## Ownership reconciliation and next action

Root waits for reviewed ZE-154 and ZE-156 commits, integrates their real consumers, and then gives one full mutation owner the resulting immutable main pin. Refresh concrete signatures/diffs as normal implementation preparation within that assignment; no additional readiness chain is proposed. Original ZE-52 is not claimed or closed by this report, and ZE-53/57 retain their full public/read-write-search/TCK integration requirements.

ZE-154 owns the current pattern/relational files until its handoff; ZE-156 owns completed/native plus the exact additive property iterator until its handoff. ZE-46 remains the lifecycle/base/write/recovery/maintenance owner. Root must reconcile any necessary shared lifecycle/base/write and native-record hunks with ZE-46 before authorizing edits, preserving its physical-sweep fence/provenance rules and sole commit protocol. The existing mutation/checkpoint recovery path is already real; this report adds no blanket ZE-46 completion prerequisite. ZE-158's ANN producer planning is independent: search in a mutating statement remains rejected, so ranking is not a new mutation dependency.

## Inspected source hashes

SHA-256 of exact commit contents, paths relative to the repository:

```text
d594ce2a71479873f918842aa042aeb8fb954b5b29e823cb8dfcfa8560b4525a  crates/zeppelin-embed/src/property_graph/query/pattern.rs
6b64fd9fcd7e06b9a6c415784c47c2d3fb5cf974c2c42339f1cb9a921d10ff47  crates/zeppelin-embed/src/property_graph/query/expression.rs
dc2689bc422b0e1f31ad825ba57f94d78b2841d182c210c9db91fdfbf7de6efe  crates/zeppelin-embed/src/property_graph/query/completed.rs
8616d16d5096a364d555ead966168d070a06ac73ee49bcfb027485678b92a1be  crates/zeppelin-embed/src/property_graph/query/runtime/driver.rs
b0ffeb3a28a678006e74c179fdeeb35f79d1afaa75c0b2b769afa0bc8a405f3e  crates/zeppelin-embed/src/property_graph/staging/overlay.rs
d30130877d2c773ca1cfa34197a149d74f584c2e60cb9efa18b625a8975f9845  crates/zeppelin-embed/src/property_graph/staging/result.rs
d9930dba7dbeda59b7fc54a88a4d3f169a9d7aae110128fce79213a91120d971  crates/zeppelin-embed/src/lifecycle/native_graph/base.rs
00aaaed836cdf49cfd892eae2904e4b5c05145ee632cb90152615698db27405a  crates/zeppelin-embed/src/lifecycle/native_graph/write.rs
cb68c7faf4b052cb5be1ff2dc126eeffcfbf3e554da8022e5fc0743805557547  crates/zeppelin-embed-cypher/src/lowering/mutation.rs
```

Canonical ignored contract files read on main, separately from the immutable source:

```text
632a06b31d90ab6786c6e39ed531c82a522907c8a93df959fb8f21f255706868  docs/graph/plans/execution.md
bc12be50f91c69f2514793dc92cf88af866d3a61da8123040a42755eb3940799  docs/graph/plans/writes.md
d9accf036bc8bba5b5484c52556d71658475bade7bd580d13712e6b986cd095d  docs/graph/plans/parallel-contracts.md
```
