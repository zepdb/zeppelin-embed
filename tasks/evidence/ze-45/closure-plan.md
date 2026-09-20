# ZE-45 fixed closure checklist

Audit scope: base `3a0ac9dc6e9419a3709e5304ade721915cbf5991` through frozen candidate `ebb5f411ce908fa4c6e100a7f89e9d82c0b5f250`, plus explicitly identified uncommitted corrections in `/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-45`. Main was `16ec865` when inspected. This audit ran no builds, tests or runner and edited no repository files.

Authority: live ZE-45/E4; actual AGENTS.md; `docs/graph/plans/storage.md`, its query-side split and counter precision addenda; ZE-45's mandatory handoff `/tmp/ze-45-astra-read-view-plan.md` (SHA256 `daab3d4227a0f1cc051ab60ceb7860b596be2d2b1cff6bd999c356b06bbe4ce4`); final Spec and Standards reviews at `/tmp/ze-45-{spec,standards}-review-final.md`; owner's newer instruction deferring **all adversarial-runner execution and large suites** to ZE-118. Section references below refer to the mandatory handoff. Local planning files are not inferred from Git.

## Completion boundary

ZE-45 delivers the real writes-owned read admission/lease/capture/close machinery; one admitted immutable source/catalog/GraphReadView; typed source-bound reads and bounded resumable cursors; and an authentic owned preparation handoff. Controlled fixture installation/replacement/reclamation may prove these contracts. It does not prove durable publication, recovery, production GC, search or public Cypher execution.

Close once the six items below and their finite verification/record requirements pass on the frozen integrated source. No redesign, file-size refactor, new dependency, new benchmark, broader runner campaign or downstream feature is part of this closure. Existing implementation decisions may vary in spelling/layout provided these observable contracts hold.

Status vocabulary: **existing** means inspected source plus recorded historical narrow evidence support the stated sub-contract; **pending** means a concrete defect/proof gap; **WIP** means a source correction is present but uncommitted/unverified. Historical GREEN is not final-source GREEN.

## Fixed remaining work

### C1 — Finish authentic preparation ownership (Spec finding 1; §§4,7)

**Pending; coordinator WIP present.** At ebb5f411 tests manually produce/finish/wrap and manually wrap one failure; that does not implement the specified coordinator. The dirty `view/prepared.rs` adds `GraphPreparation` and complete lease comparison, but at inspection it accepts arbitrary `PreparedObjects<S>` and independently supplied leases. Equality of those leases does not establish that S/catalog belongs to that admission.

- [ ] Establish one admitted, immutable **preparation source/catalog scope**, using actual StorageMemory -> WriteMemory -> GraphResources backing and close-first prepare-mode control. Bind its source to the admitted lease through construction/private capability, not a caller assertion or PhantomData alone. Existing checked storage readers may be reused; do not duplicate the producer or create a second publication path.
- [ ] Have GraphPreparation own PreparedObjects and the retained base while invoking the actual `prepare_native_graph` and `finish`. Every prepare/finalize/descriptor-registration failure returns original typed error plus still-owned packs/abort inventory/base; success owns finished packs, candidate, finalized descriptors/inventory and reservations.
- [ ] Bind/compare base store/generation, root-envelope RequiredRef, independent WAL sequence, all eight exact root descriptors, catalog, vector/text/reclaim participants, high-waters, prepared inventories, lexical interpretation and document declaration. Target roots/store/generation must match finalized objects. Capturing some identity immutably in the scoped coordinator is acceptable; do not require adding non-WAL fields to the persisted candidate schema.
- [ ] Exercise the coordinator, not manual wrapping, in the existing two `native_prepared_artifacts_*` tests: exact handoff survives replacement; same-generation foreign identity/source rejection; actual create/append/finish failures retain inventory and release to baseline. Preserve existing prepared-protection capture assertions. Existing generic error enums with explicit variants/messages are acceptable; no renaming requirement.
- [ ] Refresh actual preparation lifetime negative/positive controls: result cannot outlive source or storage owner; valid scoped result inspection/transfer compiles. `into_parts` is an explicit ownership transfer, not publication or deletion permission. Callback-created paths before an artifact exists remain writes-owned and are explicitly documented.

Files: `storage/view/prepared.rs`, an admitted preparation-source helper if needed, narrow `storage/prepared.rs` accessors/exports, and `lifecycle/native_graph.rs` tests. Do not solve this by accepting arbitrary source + unrelated lease.

### C2 — Complete label-index traversal (Spec finding 2; §6)

**WIP:** dirty `scan_live_nodes_after` now selects TreeKind::Labels and validates empty membership values; frozen tip scanned all Nodes.

- [ ] Finish panic-free membership-key decoding and correct exclusive resume. For nonempty conjunction choose one requested label index, validate remaining labels against the live authoritative record, suppress tombstones, and reject malformed membership values. All selection retains Nodes. Unknown requested label produces empty results; repeated selectors do not duplicate rows.
- [ ] One direct regression with many unrelated nodes and a small chosen-label membership proves bounded work follows that index; verify conjunction/unknown/duplicate selection and malformed membership shape in the same focused test or existing fixture. The deliberate old Nodes-scan behavior must fail the index/work assertion. Preserve full u128 IDs and MAX termination.

Files: `storage/view.rs` and native view tests. No index design change.

### C3 — Replace the weak old-view fixture with the real combined proof (Spec finding 3; §§4–6,9)

**Pending.** Existing old-view test opens a synthetic optional participant directly via BlockSource. Existing `native_read_all_operations_use_one_admitted_bundle` runs only one producer view; the separate replacement consumer checks sequence only. Existing `use-current-root.patch` changes the test consumer's result, so it is not a product-root substitution proof.

- [ ] Produce two immutable real native fixture generations with distinguishable properties/text/edges/catalog/sequence; install via the controlled installer. Hold the old admission, map only what setup requires, and identify an actual producer descendant artifact whose per-path open count is zero.
- [ ] Replace current state; capture real protected roots and run a small fixture-only reachability/reclamation adapter. It may delete only fixture-owned files proven unprotected; it is not production GC.
- [ ] Through **GraphReadView**, perform old-view lookup/property, label/type scan, expansion, stored text and copied materialization across replacement. The unopened actual descendant must open afterward (0 -> 1), old results must remain exact, and a new admission must observe replacement values. Keep stored endpoint orientation/parallel multiplicity. Include a pair of distinct full IDs sharing low 64 bits if that explicit §9 identity assertion is not already present elsewhere.
- [ ] Direct narrow negative control omits real lease protection and observes the actual missing lazy file; restore exact bytes and terminal GREEN. If retaining a current-root mutant claim, mutate the actual product read path used by this fixture, not its expected result/test consumer. Preserve proof of admission/capture mutex ordering and no-lost-wake close behavior already present.

Files: primarily `lifecycle/native_graph.rs` fixture/tests. No durable writer, reopening or production sweep implementation.

### C4 — Prove resume through a persisted same-type physical split (Spec finding 4; §§6,9)

**Pending.** The current two descriptors represent different types. `exact_split_fixture` proves a codec split only. `review-physical-split-green.log` is an actual terminal failure (0 passed/1 failed, Memory), and its named test is absent at ebb5f411.

- [ ] Build real producer output containing at least two persisted physical ranges for the **same node, direction and relationship type**; assert their distinct directory keys/range bounds before exercising GraphReadView. Add a second type with interleaved RelIds to retain the already-fixed type-boundary case.
- [ ] Resume via GraphReadView at capacities 1, 2 and 256 around a physical boundary; concatenate exact output and compare to primitive independently expected relationships, preserving bag multiplicity and u128::MAX termination. Check actual cumulative work for the exercised pulls, including any bounded remerge; no reset per range/pull and no last-RelId-only resume across types.
- [ ] Keep fixture preparation within existing 32/64/256-MiB limits: use multiple bounded real producer preparations/persisted snapshots if a single overlarge staging fixture refuses. Do not raise budgets, substitute codec-only output or reduce to two types.
- [ ] Run the direct split/resume test with terminal success and exact selected count. Retain and clearly relabel the previous failed log as historical failure. Existing physical-position product fix need not be rewritten if this proves it.

Files: native fixture/tests; `adjacency/read.rs`/`view/cursor.rs` only if the direct test exposes a real defect.

### C5 — Finish exact transient-path accounting and panic-free code (Standards findings; §§5,8)

**WIP:** dirty `view/source.rs` now separates temporary path charge from mapped slot; dirty `catalog.rs` destructures high-waters.

- [ ] Account actual simultaneous filename + pathname capacities before initialization/use, reconcile actual capacities, drop backing before reservation release, and retain no pathname charge after its allocation is gone. Source slots/mapped controls/catalog/cursor/result allocations retain existing authentic ownership; mapped bytes/residency remain separate figures.
- [ ] One narrow path test observes peak/current accounting across two lazy opens, cache hit and open failure; proves no accumulated phantom charge, no uncharged simultaneous buffer, clean baseline after drop and correct memory refusal. Extend existing stats/accounting test if convenient.
- [ ] Remove all production indexing introduced by corrections too (e.g. dirty label_lower slicing); use checked slices or fixed-array destructuring. Run scoped strict Clippy and rustfmt/diff checks. Do not refactor the 4,611-line test-bearing lifecycle module for stylistic size.

Files: `view/source.rs`, `view/catalog.rs`, `view.rs`; direct tests.

### C6 — Freeze final narrow evidence, integrate once, then close (Spec finding 5; §§9–11)

**Pending.** Manifest/environment identify `092af389`; required README is absent; historical 20-test terminal set predates correction tests; existing lifetime source uses obsolete GraphReadView constructor/PreparedGraphArtifacts generic shape.

- [ ] Freeze a correction commit and explicit changed-path manifest, preserving inherited dirty files. Record exact parent/candidate, final source hashes, hardware/toolchain/feature/config, commands/exits/selected counts and all corrected failures in `tasks/evidence/ze-45/README.md`. Include this closure checklist and both review dispositions.
- [ ] Run the finite direct acceptance set below once on final source. A correction test that fails is fixed and rerun; do not rerun unrelated passing sets or broad campaigns. Record actual counts; zero selected is failure.
- [ ] Update lifetime probes to **current APIs**, including RelView source lifetime and C1's real preparation source/memory lifetime. Each negative must fail for the illegal lifetime/borrow, paired positive must compile. Preserve ZE-135 RangeScratch nonescape and its two positive controls; only rerun that unchanged probe if signatures/ownership were affected.
- [ ] Preserve authentic prior RED/restore/clean records. Do not manufacture a retroactive baseline or call the failed split log GREEN. Historical baseline/source-verification logs remain explicitly historical; no new broad baseline work is required.
- [ ] Source-only review frozen correction against C1–C6, then integrate only the reviewed product delta. Resolve shared-file/registry conflicts against current main additively, preserving other ticket keys. Run only direct native acceptance tests affected by integration and scoped static checks.
- [ ] Record integrated source/evidence in ZE-45; append exact integrated changes/features and deferred commands to ZE-118; close ZE-45 with controlled-fixture limitations/downstream owners and export tracker backup. Do not claim first release complete.

## Existing acceptance coverage to preserve (no redesign/reproof campaign)

The following is a complete disposition map for the handoff's named acceptance groups. Existing rows require preservation in the final direct set, not new bespoke test families. Pending subcases are assigned to C1–C6 above.

| Contract / named test suffix | Current evidence and exact disposition |
|---|---|
| admission_registers_before_replacement_capture | Native publication mutex + barrier race test exists (native_graph.rs ~2231); historical terminal set passed. Preserve; C3 covers actual descendant consequence. |
| clone_retains_one_registry_entry_until_final_drop | Shared clone owner, distinct admission tokens, registry/accounting release test exists (~2272). Preserve. |
| old_view_lazily_opens_unmapped_artifact_after_replacement | Synthetic participant proves low source retention only; C3 replaces with required actual adapter proof. |
| all_operations_use_one_admitted_bundle | Existing single-view real producer read checks plus sequence-only replacement are partial; C3 supplies combined replacement proof. |
| graph_only_optional_payloads_and_full_width_ids | Real no-embedding catalog, absent/empty text, unembedded nodes, high u128/MAX and separate exact-vector fixture exist (~3293,3444,3511). Preserve; equal-low64 assertion, if absent, belongs C3. |
| catalog_and_required_refs_cannot_be_substituted | Bundle descriptor/store/role validation, catalog interpretation/high-water checks and actual bad-root checksum test exist (~2557; view/catalog.rs). Preserve exact source checks; C1 covers full preparation identity. |
| missing_or_corrupt_lazy_file_is_not_absence | Actual path-backed error/absence test exists (~2425); source framing/container checks remain. Preserve; C3 adds real old-descendant missing proof. |
| scan_resume_preserves_interleaved_types_and_max_ids | Typed relationship scan capacities, MAX and actual directory branch checks exist; C2 fixes label path, C4 supplies physical same-type split traversal. |
| cursor_rejects_same_metadata_foreign_admission | View token/runtime/memory/selection retained in opaque cursors; direct test exists (~3219). Preserve no output/counter/I/O mutation on rejection and failure latch. |
| cursor_rejects_same_view_memory_different_runtime | Private RuntimeInstanceId plus direct test exists (~3112); capability-construction correction has separate 1-test GREEN. Preserve. |
| runtime_identity_exhaustion_never_reuses_a_stamp | Private local allocator exhaustion test passed in 20-test set; no global counter mutation. Preserve. |
| undirected_self_loop_and_parallel_edges_are_exact | Real producer comparator and OUT/IN orientation/physical resume exist (~4183); historical direct GREEN. Preserve; C4 extends physical ranges. |
| every_edge_path_checks_both_endpoint_states | Actual live/tombstoned/missing endpoint tests cover relationship lookup/type scan/expand (~3860). Preserve source-bound RelView/property correction. |
| close_cancels_and_drains_current_and_retired_leases | Real close/drain, release signal, barriers and close-first checks exist (~2624). Preserve; no public lifecycle expansion. |
| drop_cancels_without_destroying_borrowed_mapping | Retained mapping/lease Drop test exists (~2492). Preserve. |
| limits_are_cumulative_and_refused_batch_is_private | Existing exact counters/refusal/sentinel test (~4055) and live mapping/residency/active-query stats test (~4499). Preserve; C2/C4 counters may truthfully change, C5 adds transient pathname proof. Keep ZE-135 unchanged-path 16-entry/644-byte evidence distinct. |
| prepared_artifacts_retain_exact_base_source_and_abort_owners | Real producer/finished packs/inventory/capture exists, but orchestration/source admission/failure ownership is incomplete. C1. |
| prepared_artifacts_reject_foreign_or_stale_base | Existing test varies generation; full same-generation admitted identity/source proof is C1. |
| pull_consumer_resumes_and_materializes_same_view | Actual PullOperator/execute_in consumer >256 rows plus materialization exists (~3702). Preserve; not ZE-50 pattern execution or public API acceptance. |
| lifetime/source/memory nonescape controls | Historical negatives diagnose real lifetimes, but old constructor and preparation generic shape are stale. C6 refreshes affected probes and paired positives. |
| actual runner/seed/fault/mutant registration | Source registration and receipt/comparator gating exist; earlier logs retain their actual source scope. All runner execution, four-seed campaigns and additional runner-based mutants are deferred ZE-118 by latest owner direction. Direct C3/C2/C5 controls may run. |
| feature boundary / dependency / panic-free / preservation | No Cargo.lock or third-party dependency addition in frozen range; graph installer gated to test/test-support; format tags/ABI/Swift/compiler untouched. Preserve, scoped static C5/C6. No Windows runtime claim. |

## Finite verification set

Use checked-in nextest configuration (`-j4 --retries 0`, libtest threads=1), core `--lib` only to avoid building every integration binary. Suggested final direct selector, adjusted only for actual added names:

```sh
cargo nextest run -p zeppelin-embed --features graph-cypher --lib -j4 --retries 0 -E 'test(native_read_) | test(native_prepared_artifacts_) | test(expansion_resume_crosses_real_split_and_type_boundary_with_bounded_work)'
```

Ensure the C1/C2/C3/C4/C5 corrected tests are included by exact names. Add their exact names if they do not share these prefixes. This selects the native direct acceptance cases and private runtime exhaustion case; it is not an adversarial runner. A 20-ish test direct set already took seconds in the historical log, not hours.

Run targeted strict Clippy for the changed core graph library/test sources, owned-file rustfmt and git diff --check. Run existing direct regression targets only where C1–C5 change their shared production paths: graph_storage_prepare for actual producer handoff changes; graph_query_storage if resource/low-lifetime code changes. Prior graph_adjacency_store/directories/snapshot_remap/lifecycle-close logs remain historical; rerun an affected direct case when integration edits those paths. No automatic all-target/full-workspace regression expansion.

**Do not run:** adversarial_tests (even filtered), runner probes/campaigns, full workspace, coverage, fuzz, size, soak, performance corpus, Windows or release packaging. Keep all deferred commands, seed/feature registry coverage and final per-crate >=90% coverage obligation on ZE-118; deferral is not evidence of success.

## Review rubric and stop rule

1. Review the immutable correction and these concrete assertions once. Every blocking finding must cite an existing C1–C6/plan requirement, exact current source/evidence, and a reproducible counterexample or demonstrably missing required proof. Differences in helper names, module arrangement, repetition, preferred algorithms or speculative future needs are nonblocking.
2. Do not reopen already satisfied source invariants solely because a reviewer prefers another design. Do not add broad execution gates: the owner's deferral supersedes older plan command lists.
3. A new concrete correctness regression introduced by the correction is fixed within its matching C item; a newly discovered pre-existing violation of an explicit original requirement must be reported honestly, with that exact requirement, rather than silently waived or labelled style. Keep it in the same bounded closure sheet, not another open-ended planning round.
4. Completion is all six checked, authentic final direct results, reviewed integrated source and tracker/evidence closure. No additional general review round after that boundary.

Retained downstream work: ZE-39 durable admission/publication/WAL/ack/checkpoint; ZE-40 recovered installation/recovery; ZE-46 production consolidation/complete protected union/reclamation; ZE-50 pattern operators; ZE-53/public execution and bindings originals; ZE-60/61 real retrieval/search participants; ZE-47/41/76/78 integrated fault/performance/release acceptance; ZE-118 deferred broad qualification. No dependency is removed or broader goal marked complete.
