# ZE-40: native checkpoint and committed-delta recovery

Planning handoff for GPT-5.6-Sol/xhigh. Source inspected by GPT-6-Astra/xhigh; no product code, build, test, benchmark, or commit was produced by planning. Root reviews this document, assigns execution, integrates, and closes.

## Fixed assignment and operating limits

- Ticket: ZE-40, “Recover graph checkpoints and committed search deltas.” It was todo/unblocked, all ZE-29/35/39/61 prerequisites were done, and this planner claimed only ZE-40. During planning root created the accepted flat ZE-150 shared-lock prerequisite and blocked ZE-40 on it. Planning returns ZE-40 to todo; no implementation starts until ZE-150 is verified/integrated and root reclaims/reassigns ZE-40. Preserve original acceptance and decisions.
- Worktree: `/Users/aghatage/Documents/code/zeppelin-embed-wt-ze-40`; branch `codex/ze-40-native-recovery`; planning source pin `56e4d93a30dbdc607090f939f9cf7c7400bf48b9`. Root supplies the refreshed execution pin after ZE-150 integration; revalidate only the changed lock/owner seams, retaining the rest of this plan.
- Read `/tmp/graph-sol-executor-rules.md`. Follow its two-failed-fixes and 20-minutes-without-acceptance-milestone escalation. Send exact command/error, current diagnosis, and smallest proposed next action; do not keep changing fixtures/budgets.
- Preserve every inherited file and symlink. `/tmp/ze-40-preservation.json` pins six inherited file hashes; `.agents` and `tracker` link to main. Never stage any inherited path. Canonical ignored design documents remain in `/Users/aghatage/Documents/code/zeppelin-embed/docs/graph/plans/`.
- Named nextest tests only, `-j 4 --retries 0`; compile required feature paths separately. No workspace/full/adversarial campaign, fuzz, coverage, soak, size/performance corpus, or release build. ZE-118 retains those qualification obligations. Runner source and compile integration remain required.
- No dependencies, public facade/ABI additions, format-family changes, scope/dependency changes, architecture rewrite, new agents, push, main integration, or tracker closure. Commit only ticket files with a ZE-40 prefix and literal RED/GREEN evidence in the body.

## Contracts and current implementation

Read the complete canonical `writes.md`, storage's allocation/reclamation and inventory handoff, bindings' mode-safe constructor contract, and qualification's bounded-tail recovery cell. `tasks/evidence/ze-39/README.md` is prior evidence, not ZE-40 GREEN.

The existing production boundary is crate-private `Store::create_native_graph[_with_infrastructure]`, `apply_native_graph`, `with_native_read`, and `checkpoint_native_graph`. ZE-66 owns the later complete `GraphStore` public facade. Use these real lifecycle operations and a corresponding real internal open; never `install_native_graph_for_test`, a codec-only fixture, or a new success stub as recovery evidence.

Source facts at the pin:

- `lifecycle/native_graph/persistence.rs`: `graph-root.ze` is a 120-byte checked selector naming one exact family-18 root/checkpoint RequiredRef. `create` makes the actual durable store. `classify_native_graph` is a coarse fresh-create classifier, reads whole files, and classifies a missing selected root as Incomplete. Do not use it as the complete existing-store recovery validator or collapse its categories into success. A selector already naming a missing committed root is a loud recovery error.
- `property_graph/wal/checkpoint.rs`: `NativeCheckpoint` binds WAL identity, first sequence, applied sequence and full `CommitState`; all graph/search/catalog/reclaim/high-water/prepared-inventory fields already exist. Do not remint these bytes.
- `property_graph/wal/replay.rs`: `Replay::new` accepts a cut stream whose header starts at checkpoint.sequence+1. `at_watermark` authenticates a still-present historical prefix through the exact complete cutoff, without requiring retired historical objects. `ReplayEnd` identifies a proved incomplete terminal append and exact complete byte count. Fully framed damage is an error and later failure invalidates the entire private result.
- `lifecycle/native_graph/write.rs`: `NativeWriter` retains selected WAL, count/byte trigger state and a charged bounded protected-descriptor ledger. Ordinary commits retain their real older checkpoint descriptor. Checkpoint creates/full-syncs a new WAL and root, publishes/full-syncs selector, swaps bundle, and only then clears uncheckpointed ledger. All prepared-inventory refs survive in committed/checkpoint state, bounded at 8,192 until ZE-46 consolidation.
- `NativeGraphBundle::install` intentionally requires checkpoint generation equal to initial bundle generation. `assemble_committed` is private proof of one prepared live transition. Recovery needs its own narrow final assembly deriving metadata from validated checkpoint plus replay; weakening the raw equality check or pretending the latest generation has a newly created checkpoint is forbidden.
- `NativePreparationSource`, `NativePreparationCatalog`, and their mapping path require a real admitted lease. They are useful code/mechanisms, not authority to install an unvalidated recovery bundle. Sparse `validate_checkpoint` and `validate_replay_transition` exist; the latter takes a live `StagedBatch`, so it is not directly usable with persisted mutations.
- `Store::new_native_graph_owner` in `lifecycle/mod.rs` builds the otherwise empty native Store and holds the lifetime lock. It currently rejects ReadOnly. Legacy `Store::open_with_infrastructure` has no graph selector guard and can bootstrap legacy state in a graph directory; ZE-40 must refuse that before any mutation in graph-enabled and feature-disabled builds.
- `Vfs::list` returns an unbounded Vec. There is no bounded directory enumerator or truncate seam. ZE-40 needs a real streaming enumeration seam for serial restoration. It need not introduce truncation: after fully successful writable recovery of a proved torn tail, use the existing durable checkpoint/WAL-rotation protocol before allowing new appends. Retain the old file; do not append behind the ignored tail or rewrite a committed prefix.

## Exact ownership and allowed files

Primary implementation:

1. `crates/zeppelin-embed/src/lifecycle/native_graph/recovery.rs` (new): production open sequence, private recovery state/source/semantic validator, checkpoint/WAL admission, inventory/serial scan and final install.
2. `crates/zeppelin-embed/src/lifecycle/native_graph.rs`: recovery module; narrow recovered-bundle construction and mapping ownership access; typed errors/access-mode enforcement where necessary; test child declaration. Preserve current publication/lease exclusion and raw bundle guards.
3. `crates/zeppelin-embed/src/lifecycle/native_graph/persistence.rs`: share selector decode and checked descriptor helpers with recovery, and internal open constructors; no alternate create protocol.
4. `crates/zeppelin-embed/src/lifecycle/native_graph/write.rs`: restore writer count/bytes/protected descriptors from checked recovery, explicit ReadOnly mutation refusal, and reuse existing checkpoint rotation after proved incomplete tail. Do not redesign live commit/materialization.
5. `crates/zeppelin-embed/src/lifecycle/mod.rs`: graph-directory refusal before legacy open mutation, including feature-disabled builds; native owner access-mode routing after the shared-lock prerequisite is integrated. No unrelated legacy lifecycle changes.
6. `crates/zeppelin-embed/src/property_graph/wal/replay.rs`: bounded semantic handoff below and, only if needed, checked sequence-to-byte-watermark helper using existing framing. `wal/checkpoint.rs` only for a demonstrated checkpoint-validation omission, with field tests; no format change.
7. `crates/zeppelin-embed/src/property_graph/storage/search/checkpoint.rs` and `search.rs`: stored-mutation replay validation under real canonical sources; share existing sparse validations, do not fabricate StagedBatch. `search/view.rs` only to expose the existing bounded unchanged-membership comparison to that adapter if necessary.
8. `crates/zeppelin-embed/src/property_graph/storage/view/mapping.rs` and `view.rs`: narrow read-only recovery mapping constructor/export using the actual native accounting/registration owner before any external admission. Reuse the existing mapping mechanics; no second mmap implementation or dummy NativeReadLease. Object mappings still have the existing 4-MiB bound. WAL input has its separate bounded/read-only input owner.
9. `crates/zeppelin-embed/src/vfs/mod.rs`: streaming direct-child enumerator plus StdVfs and CountingVfs delegation/counters. An unsupported VFS fails explicitly; no default implementation collecting `list()`. Keep it a read-only pull/visitor interface, one entry at a time, with directory handle/path storage bounded. Test path-backed wrapper delegates and records it. Do not retrofit every legacy VFS merely to make them appear to exercise native mmap recovery.

Tests/evidence/runner:

- `crates/zeppelin-embed/src/lifecycle/native_graph/tests/recovery.rs` (new).
- `.../tests/publication.rs` only narrow `pub(super)` test-helper exposure/reuse of the actual fixture/observer/path-backed RecordingVfs; do not refactor its passing cases.
- `crates/zeppelin-embed/tests/graph_wal.rs`: mechanical mandatory validator-signature updates plus directly required watermark regression if added.
- `crates/zeppelin-embed/tests/native_graph_open_admission.rs` (new feature-neutral legacy refusal integration test; intentionally recognizable required selector bytes suffice for refusal only, not recovery).
- `crates/zeppelin-embed/src/lib.rs`, `tests/adversarial/graph_recovery.rs` (new), `tests/adversarial/{mod,runner,coverage}.rs`: focused real-path hook and observed fault/coverage/independent-comparison integration only.
- `tasks/evidence/ze-40/README.md`, `tasks/evidence/ze-40/astra-plan.md` (exact approved-plan copy), bounded logs.

Stop and ask root before another file is needed. In particular, do not edit `lifecycle/lock.rs`: the separate shared-lock producer owns it. `native_graph/base.rs` is a consumer/reference, not a planned rewrite.

## Recovery ownership, control, and error rules

Use one Store-owned `GraphResources`, existing `WriteMemory` <=64 MiB and `StorageMemory` <=32 MiB for temporary recovery descriptors/catalog/sparse decoding; keep aggregate <=256 MiB. Reserve actual capacities before fallible allocation, reconcile actual capacity, and retain owners until backing drops. WAL bytes are immutable under the lifetime process lock and bounded by the selected stream's validated size; object files use controlled decode/mappings and existing 4-MiB maximum. Never `read()` an unbounded WAL/artifact or call `list()` then claim bounded recovery. Finite cumulative work controls poll byte loops at most every 64 KiB. Reject capacity/work/cancel with no installed handle or mutation. There is no whole-population entity HashMap, vector matrix, or index rebuild.

A private recovery source is owned by the unopened Store plus retained process lock, exact store identity/cutoff, charged fixed mapping slots and actual mapping registrations. It resolves through Store VFS, `artifact::decode_with_control`, `ValidatedArtifact`, and exact RequiredRef identity/length/checksum/block/kind checks. Keep its construction private to open. Release/reset scoped sources after their borrows end; do not grow an unbounded retained mapping table while validating a long stream. Catalog decode uses exact declaration/high-water checks and actual symbol-descriptor charge, mirroring `NativePreparationCatalog`; RecordCatalog resolves borrowed exact names.

The final bundle derives from the validated selected checkpoint and final complete envelope state. Checkpoint identity remains the actual selected root; latest graph/search generation may be newer. All eight roots, catalog, sequence, modality refs, high-waters and prepared-inventory refs come from that same state. Copy only final owned descriptor/tower metadata under reservations. Install once after every required validation, serial scan and final control check. No partial Store escapes if any later envelope fails.

Preserve first concrete source errors. ReplayValidator's small WalError cannot carry an I/O/TreeError: retain the first owned NativeGraphError/TreeError in the recovery adapter, return the corresponding WalError failure to latch replay, then return the original typed error at the constructor boundary. Never turn resource, cancellation or missing/corrupt data into absence, generic success, or a recoverable torn tail.

### Required semantic handoff

Add a borrowed `ChangeReader<'_>` argument to the existing required `ReplayValidator::state(base, target, ..., resources)` callback. Construct it from the already structurally validated current envelope bytes in `next_inner`, immediately before semantic state completion. This permits streaming all changes with target roots known, without copying borrowed keys, rebuilding a StagedBatch, changing persisted bytes or allowing a callback to retain input. Update every existing implementation mechanically; none gains a default-success branch.

The production adapter's required/object hooks perform exact framing/role validation; mutation/inventory hooks validate each record's domain and canonical/provenance framing. Its state callback correlates the whole envelope with base/target:

- Full operation provenance, incarnation, installed/requested revision, key/expectation/delete mode and original changed generation match target record/fence. Read exact lossless canonical PayloadSlice; digest equality is not byte equality. Deletion matches the persisted tombstone/fence and no invented live canonical image.
- Validate node/relationship/symbol high-waters, store identity, monotone physical serial, and both OUT/IN/native root semantics needed for the committed transition. No dropped reverse edge or unexplained key/entity change.
- Stored sparse transition adapter accepts these persisted Mutation records and their canonical readers, validates target origin, before/after membership, exact unchanged populations, source masks/statistics and current/historical catalog interpretation. Use existing SparseView/validate_all and comparison logic. Relationships have no membership. Checkpoint sequence zero with empty sparse refs is valid; do not weaken the nonempty cutoff guard in existing `validate_checkpoint` to make an invalid fixture pass.
- Prepared inventory role-2 bytes are the existing `ZGCP/2/v1 + count/reserved + 64-byte descriptors` emitted by write.rs. Check reserved bytes/length, omissions, duplicate ArtifactId/serial, store/cutoff and exact coverage against envelope Inventory frames, including supplemental catalog and the enclosing inventory object's own descriptor. The inventory does not recursively contain itself. Complete allocation coverage is rooted inventory plus all retained prepared-inventory/checkpoint/WAL descriptors. A listed object is allocation bookkeeping, never deletion authority.

## Ordered implementation and named RED/GREEN

Do not implement every branch before the first milestone. Add the first named test and run it. A temporary `Unsupported` open body is acceptable only to obtain the initial explicit missing-open RED and is replaced in the same milestone; it is not a delivered producer. A compile error is not the acceptance RED.

### 1. First real mixed reopen, then report immediately

`ze40_complete_mixed_commit_close_reopen_is_coherent`

Create a fresh path using real native create, apply one small mixed batch (a keyed node with text/property, a keyed vector node with exact f32 bits, one directed relationship via local refs), record actual receipts/StoreInstanceId/generation/sequence, close/drop, and call the new production native open. Use existing admitted `with_native_read`/GraphReadView for exact node/property/text/vector/relationship OUT and IN observations. Confirm same IDs/revisions, StoreInstanceId and coherent generation, then a normal new apply advances from recovered counters. This fixture should remain small; reuse ZE39's actual setup and observer shapes, not its whole property gallery.

Implement selector/root/checkpoint/catalog admission, cut-WAL replay of Mutation envelopes with real semantic source, exact retained sparse roots, final bundle and writer restoration only to satisfy this first normal path correctly. No recovery installer, no fabricated checkpoint, no fake staged batch. Observe intended RED at missing real recovery, then this test GREEN. Report RED, core compile and first coherent GREEN as separate milestones.

### 2. Refusal and lifecycle admission

`ze40_open_refuses_unsupported_corrupt_and_incomplete_stores_without_mutation`

Cover wrong selector/root/checkpoint family/version, absent selector after interrupted creation, selected committed root missing/corrupt, selected WAL missing, wrong store/checkpoint/watermark, lexical or complete document-tower mismatch, and native open on legacy store. On every refusal compare exact file inventory/content hashes and recorded mutation calls; neither entropy nor open_append/create/sync/rename/delete is invoked. Preserve typed Incomplete versus corrupt/unsupported/I/O outcomes. A valid existing store cannot receive a new StoreInstanceId.

`legacy_open_refuses_native_graph_directory_before_any_mutation` in the feature-neutral integration test runs with and without graph-cypher. Guard is outside graph cfg and precedes ensure-directory(create), lock-file creation, WAL recovery/bootstrap and legacy cleanup. Existing graph-root marker, including unsupported version, forbids legacy adoption. If selector is absent but recognized native initialization artifacts exist, do not silently bootstrap legacy state; inspect via bounded read-only enumeration. Unknown ordinary legacy names retain their existing behavior. Test useful legacy open control alongside refusal.

No build can guarantee retroactively fixing already installed older binaries; evidence is this checkout's feature-disabled build.

### 3. Indeterminate commit and proved torn tails

`ze40_lost_ack_and_stopped_writer_resolve_on_reopen`

Use actual post-Full-sync/prepublication fault from ZE39, assert exactly one fire, indeterminate outcome and stopped new admissions, close/drop, reopen and observe the whole durable mixed batch. Exact structured retry returns the same IDs/original generations without WAL/artifact/serial/generation change; altered retry fails. Include append-before-write failure old-state case and a matching clean control. Do not label a returned error as rollback or claim modeled media loss from ordinary process behavior.

`ze40_incomplete_terminal_append_is_ignored_and_writable_reopen_rotates_before_append`

Produce a real valid prefix, then real partial append fault. Replay's existing classifier must prove the terminal append incomplete and expose no partial effects. Complete private validation/serial scan first. Writable recovery rotates a fully durable checkpoint/new WAL before admitting a subsequent append; it never appends after the old torn bytes. On rotation failure return failure without admitting a writer; preserved selector authority determines next reopen. Old log remains protected until the durable cutoff. Read-only counterpart is step 7 and changes zero bytes. Also test complete malformed frame followed by a torn Commit: it must fail, not be hidden as incomplete.

### 4. Missing/corrupt committed state and later-error atomicity

`ze40_committed_artifact_and_framing_damage_fail_without_partial_admission`

Use files from real commits. Corrupt a fully framed WAL checksum; remove/bit-flip canonical, native tree, sparse source/live-mask, catalog, or prepared-inventory artifacts; exercise checksum-valid wrong-store descriptor and later-envelope failure after an earlier valid envelope. Every required-live missing/corrupt participant fails open, with no partial handle, older-root fallback, repair or cleanup. Duplicate/omitted inventory and altered provenance/membership with repaired framing checksums must also fail semantic validation. Mutation controls are test copies only and restored/removed before final GREEN. Assert exact zero mutation on refusal and release of owned temporary charges.

### 5. Empty history, serial recovery and orphans

`ze40_empty_graph_preserves_ids_fences_replays_and_allocation_serials`

Create/edit/delete all entities through real apply, checkpoint where useful, close/reopen. Old retry must not cross a deletion fence or recreated incarnation; identical delete retry retains original generation; explicit recreate yields a fresh greater ID/revision. Node/rel/symbol high-waters survive net-empty state. Verify complete key provenance including original generation, absent/empty payload distinctions and no-op/replay no-write behavior after reopen.

`ze40_serial_scan_preserves_pre_wal_orphans_and_refuses_ambiguous_corruption`

Writable open streams exact recognized native object names and headers under its lifetime lock. Compute allocation fence from checked checkpoint/current/inventory descriptors AND fully validated recognized on-disk artifacts, including unpublished checkpoint roots and complete pre-WAL objects with higher serials; next allocation is strictly above maximum or fails on overflow. Do not overwrite/adopt a collision. Validate filename/full object identity/store and checked serials; full framed ambiguous corruption is loud. A genuinely partial unreferenced pre-WAL object is recorded/classified as interrupted uncommitted preparation, retained outside ordinary serial sweep, and never treated as a validated artifact or used to lower the maximum. Unknown names/families are untouched. No deletion here: complete orphan reconciliation and reclaim authority belong to ZE46. Test with injected real partial-create/precommit failures and a complete higher-serial orphan, plus explicit corruption/collision/overflow cases and clean control. Recording VFS proves streaming enumeration, zero list() calls, and zero unlink.

The scan must not be an unbounded in-memory name/descriptor set. Each entry, fixed header/object validation and per-entry temporary owner is released before advancing; committed union validation uses bounded cursors/descriptor storage. If classification cannot distinguish a partial unreferenced object from ambiguous corruption, fail instead of guessing.

### 6. Checkpoint cutoff and recovery backpressure

`ze40_checkpoint_cutoff_reopens_exactly_and_retains_required_inventories`

Drive actual 64-envelope threshold, explicit checkpoint, next mixed apply and reopen; separately exercise pending encoded-byte threshold with existing ZE39's real-byte mechanism. Verify exact WAL count/byte restoration after reopen (no reset to zero with a retained tail), no-op/replay early exit, all prepared-inventory refs before/after checkpoint, same graph/search/fence cutoff, and checkpoint-before-authority/log-retirement ordering. Keep an older lease while checkpoint advances and lazily read its valid original artifact. Inject failures around checkpoint object sync, selector replace and post-replace directory Full-sync, then reopen the actual surviving selected authority. Old WAL files may remain as conservative garbage; do not add reclaim/unlink to claim retirement. Required references leave uncheckpointed protection only after checked checkpoint durability.

For a schema-supported retained historical WAL prefix, locate the complete sequence boundary through existing checked scalar framing, bind its full state with `Replay::at_watermark`, and require exact declared first/applied sequence. Do not guess a byte watermark from generation or skip corrupt historical framing. Existing producer normally rotates to a cut stream; the retained-prefix test is explicitly a supported-schema admission case, not proof of a producer that does not exist.

No two-second performance assertion in unit tests. Record deterministic bounded tail counts/bytes/scanned entries/replayed envelopes; the five-process 20-open baseline and 2-second p95 target remain ZE118/qualification.

### 7. Read-only integration after shared-lock producer lands

`ze40_read_only_replay_preserves_tail_and_all_files`

Root integrates ZE-150 before ZE-40 execution begins. This step then uses that actual producer. The reviewed producer proposes `StoreLock::acquire_shared(directory: &Path) -> Result<Self, StoreLockError>`, gated to graph support, preserving `acquire` as exclusive. Use that API only after root verifies integration; no speculative stub. It opens existing writer.lock read-only and shares one same-process OS descriptor across counted guards. Native owner retains the shared guard in existing lifetime lock storage. ReadOnly open gets private validation/replay and final read bundle but no NativeWriter, append handle, checkpoint trigger, spill/intent write, truncation, rename, unlink or eager sweep. Explicit mutation/checkpoint/maintenance calls return StoreError::ReadOnly, including NoOp attempts if existing mode-safe admission rejects writes uniformly. Two read-only handles may coexist; cross-process matrix remains ZE106.

Open real complete and real incomplete-tail stores read-only, compare exact graph observations, all file hashes and recorded zero write/create/append/sync/rename/delete; close releases the guard. A read-only process must not need to create writer.lock. After closing read-only handles, writable open rotates the proved tail and can append normally.

Root-confirmed ownership boundary: ZE40 implements supported Mutation/checkpoint recovery and preserves tagged reclamation state/protection. ZE46 owns actual completed-mark/protected-root proof schemas, proof creation/validation and resumed unlink integration; ZE106 proves pending-intent read-only behavior against that real producer afterward. Currently those storage proof producers/parsers do not exist. Never accept unknown/unvalidated role-3/4/5 proof payloads, erase pending state, or checkpoint it away. Mandatory reclaim hooks fail closed until the actual supported parser is integrated, preserving all bytes and references. This does not waive valid known-schema recovery: invoke any real supported proof validator once present, retain exact pending state, and treat missing deletion targets as acceptable only under its fully validated committed intent. Do not invent fake proof payloads, default-success callbacks, or a circular dependency on ZE46 to start ordinary ZE40 recovery. Record this exact boundary in evidence and handoff, with original future reclaim obligations intact.

## Focused checks and runner integration

Each named acceptance test gets an intended behavioral RED before the corresponding production change and exact final GREEN. Capture run ID/command/result, fault fires and clean controls. For already implemented neighboring safeguards, one bounded deliberate omission/substitution in a test copy must fail and be restored; do not run an audit campaign. The first mixed observation's comparator must reject a deliberately missing reverse edge or incorrect generation, then accept the actual output.

Run the eight writable recovery cases, then all nine including read-only once its shared-lock prerequisite is integrated:

```sh
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(ze40_)'
```

The prefix is restricted to this new recovery test child; do not let it select unrelated future cases silently. Before first run enumerate the selection. Final expected count is nine: complete-mixed, refusal, lost-ack, incomplete-tail, committed-damage, empty-history, serial-scan, checkpoint-cutoff, and read-only.

```sh
cargo nextest run -p zeppelin-embed --test native_graph_open_admission -j 4 --retries 0
cargo nextest run -p zeppelin-embed --test native_graph_open_admission --features graph-cypher -j 4 --retries 0
cargo nextest run -p zeppelin-embed --test graph_wal --features graph-cypher -j 4 --retries 0 -E 'test(=every_prefix_exposes_only_complete_envelopes_and_requires_validation) | test(=complete_malformed_change_is_never_hidden_by_incomplete_commit) | test(=required_object_validation_binds_full_descriptor_role_and_checksum) | test(=checked_checkpoint_watermark_skips_only_complete_retired_history) | test(=cancellation_triggered_by_final_validation_cannot_publish_a_batch)'
cargo nextest run -p zeppelin-embed --lib --features graph-cypher -j 4 --retries 0 -E 'test(ze39_fresh_mixed_commit_is_durable_and_coherent) | test(ze39_checkpoint_thresholds_and_failure_preserve_acknowledged_state) | test(ze39_commit_attempt_errors_are_indeterminate_and_stop_admission)'
cargo nextest run -p zeppelin-embed --test wal_recovery -j 4 --retries 0 -E 'test(=real_durable_log_refuses_a_snapshot_ahead_of_replay) | test(=store_lock_rejects_a_second_writer_and_releases_on_drop)'
```

If the new enumerator needs one direct StdVfs/CountingVfs test, add one named bounded-control case in existing vfs tests and run just it. Path-backed recovery cases exercise its actual production use. Do not expand to all WAL/VFS suites.

Add `graph_recovery_test_support` under `cfg(all(graph-cypher,test-support))`, calling the same directed real-path case bodies and returning observed receipts/rows/state. New runner module uses existing independent graph identity/adjacency oracle, real IDs/generations/fence observations and a missing-edge/state negative control; expected values come from input history, never production decoder/classifier. Add coverage keys for complete reopen, refusal, lost ack, torn tail, corrupt/missing artifact, empty history, serial/orphan, checkpoint and read-only boundary. Mark faults only from actual fired receipts with corresponding clean controls, never manual assumed hits. Register the probe beside graph_publication. Compile it, do not execute runner campaigns.

Required compile controls, once final focused checks pass:

```sh
cargo check -p zeppelin-embed --lib -j 4
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,test-support
cargo check -p zeppelin-embed --lib -j 4 --features graph-cypher,allocation-audit,query-timing
cargo check -p zeppelin-embed --lib -j 4 --features allocation-audit,query-timing
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4 --features graph-cypher
cargo check -p zeppelin-embed-workspace-tests --test adversarial_tests -j 4 --features graph-result-test-support
```

Run scoped formatting on changed Rust files, `git diff --check`, inspect final scope, verify inherited hashes/symlinks, and commit. No repeated broad checks absent a new failure/change.

## Completion report

Evidence records actual production-path outcomes, exact source/base, host/toolchain, commands, first RED, final GREEN, fault fires/controls, file/hash refusal proof and preserved limits. State default/graph/hook compile results independently. Full public facade, process-platform lock qualification, actual reclamation integration, two-second recovery/large-history benchmarks and broad final qualification are not proved by these focused results. Return commit IDs, exact changed files, checks, current shared-lock/pending-proof limitations and any uncommitted ticket paths to root. Do not close ZE40 yourself or describe an unfinished listed recovery branch as accepted.
