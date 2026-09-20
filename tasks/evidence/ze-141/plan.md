# ZE-141 implementation plan: private native-to-C graph result conversion

Planning source: current main `e2d16b106593050711bc055d46d5673af0848bd7`.
Planning agent: mandatory GPT-6-Astra planning pass; implementation belongs to a fresh GPT-5.6-Sol/xhigh agent in a root-created isolated worktree. This file is a plan, not implementation, compiled-interface or test evidence. No product source, index, branch or worktree was changed by this planning pass. ZE-141 was unblocked and claimed before inspection and will be returned to `todo` after the tracker handoff.

The authoritative scope is the live ZE-141 description plus ZE-139's resolution. ZE-139 evidence SHA256 is `3220f319ef480249b0981c15695ba8e202a29b42734db119605701040c795110`. The separate `/tmp/ze-141-plan-source-sha256.json` freezes 17 inspected paths. Root must give both files to the executor. Recheck these inputs against the chosen implementation base; unrelated main progress is not permission to replace this interface with a guessed producer.

## 1. Scope and source authority

Implement one private module that synchronously accepts a `ResultSource`, creates the authentic `PreparedGraphResult`, converts directly into the final ZE-128 arena under the same `RuntimeContext`, returns a private `FrozenOutput`, and consumes the driver's final `Execution` to fill exactly 23 fixed metadata values. There is no temporary `Vec<ZeGraph*>`, arena ownership cast, independently prepared native input, new public ABI, native store producer, coordinator or public error mapping.

Source references below are repository-relative, line numbers at the planning pin:

- `crates/zeppelin-embed/src/property_graph/query/completed.rs:17-173`: typed spans, values, pools, source/outcome/error interface; `:197-215` prepared ownership; `:238-315` bounded native copy, exact view check, native represented accounting; `:317-368` pool and metadata access/consuming detach.
- `.../completed/records.rs:8-192`: receipt, list/key/property/entity/report fields; `.../completed/validate.rs:103-374`: complete validation, postorder list DAG, source-order report/receipt rules. Note: `Some(empty vector span)` is rejected, unlike present empty text/key.
- `.../query/runtime.rs:27-150`: 22 counters and ceilings; `:189-292`: one view/control/memory/counter authority; `.../runtime/driver.rs:125-181`: Completion/FrozenOutput/Execution; `:306-418`: actual `execute_in` drain and final charges/checkpoint.
- `.../query/resources.rs:72-183,204-280,314-350`: actual query/shared accounting, typed owner capacities, external reservation. `QueryMemory::reserve` is core-private; FFI must use `reserve_external_capacity`, not add a core escape hatch.
- `crates/zeppelin-embed-ffi/src/graph_result.rs:75-121`: 14 typed `Layout` entries and padded 4 MiB cap; `:127-258`: existing parts/metadata/outcome; `:276-330`: real allocator and bounded copy; `:332-392`: fixed pool initialization.
- `.../graph_result/registration.rs:113-207`: reserve/allocate/fill/admit/checkpoint sequence; `:211-316`: authoritative free and abort; `:337-410`: prepared owner, represented/capacity accounting and infallible expose. Preserve gate, unique tokens, poison semantics and root-alias checks.
- `.../graph_contracts.rs:36-112,175-294,579-604,1034-1353`: frozen ranges/value/list/entity/tier/search/result/report/work schema. No changes to these types or the generated header are needed.
- `crates/zeppelin-embed/tests/graph_compiled_context.rs:63-140,194-280,324-418`: actual retained plan/facts and borrowed driver fixture, cumulative work and final close/cancel controls. Port the useful fixture into FFI-private tests; do not import a compiler or implement a production Completion here.
- `tasks/evidence/ze-127-owned-results.md`, `tasks/evidence/ze-128/{README.md,runner/README.md}`, `tasks/evidence/ze-139/README.md`: existing ownership evidence and exact qualification exclusions.
- `docs/graph/plans/execution.md:79-102`, `bindings.md:61-81`, `parallel-contracts.md:58-66`: complete result copying before precommit, real source/core/ABI overlap, no partial result, lifecycle and outcome ownership. ZE-139/141 provide the later authorized scheduling refinement; they do not delete ZE-53/68/69 acceptance.

No external research is needed: the authoritative source was read locally. Historical Kuzu precedents are only those already recorded in the execution/bindings plans (result collector completion/multiplicity); this change adopts Zeppelin's stronger store-independent copied ownership and introduces no Kuzu dependency or conformance claim.

## 2. Production files and private interface

Allowed production edits:

1. `crates/zeppelin-embed-ffi/src/graph_result.rs`: declare `pub(crate) mod conversion` (not externally public); extract/reuse checked geometry and typed write machinery, preserving the old `ResponseParts` interface.
2. `crates/zeppelin-embed-ffi/src/graph_result/registration.rs`: factor one private preparation/admission implementation used by old parts and new native fill. At most a tightly scoped private accessor for the trusted global work slots is needed; do not expose arbitrary mutable registered roots.
3. New `crates/zeppelin-embed-ffi/src/graph_result/conversion.rs` (optional private `conversion/mapping.rs` if mapping length warrants it): complete native mappings, preparation/finalization wrappers.

No core production files, `graph_contracts.rs`, `outcome.rs`, C headers, export lists, Cargo dependency/feature manifests or legacy registry need modification. Existing normal graph-only builds must compile this module even though the production integration caller is future work. If unreachable private items trigger strict dead-code warnings, use one explicitly justified narrow module-level dead-code allowance naming ZE-68/69; do not make them public merely to silence lint or add a success-returning fake caller.

Private callable shape:

```rust
pub(crate) fn prepare_native<'v, 'm, 'g>(
    registry: &'static GraphResultRegistry,
    source: &impl ResultSource,
    context: &mut RuntimeContext<'v, 'm, 'g>,
) -> Result<FrozenOutput<PreparedNativeResponse<'m, 'g>>, ConversionError>;

pub(crate) fn finalize_native<'m, 'g>(
    execution: Execution<PreparedNativeResponse<'m, 'g>>,
) -> FinalizedNativeResponse<'m, 'g>;
```

`PreparedNativeResponse` has private fields: the genuine `PreparedResponse<'m,'g>`, a sealed handle to exactly the 23 initialized global work slots inside that response, and the mapped expected `SuccessfulOutcome`. It has no exposure/into-parts method. `FinalizedNativeResponse` holds that same owner and expected outcome and offers only a crate-private consuming `into_parts() -> (PreparedResponse<'m,'g>, SuccessfulOutcome)`. The finalizer discards its private slot handle before yielding the finalized wrapper. Do not implement Clone, Copy, Send or Sync for either wrapper or the slot handle. All payload ownership is the unique existing prepared response.

The slot handle can be a private `NonNull<[ZeGraphWorkCounter; 23]>` constructed only from checked, initialized `root.work` with `global_work={0,23}` while the registry node is still private. Its proof is the allocated layout, fixed work count and unique prepared owner, not caller pointers. It never dereferences after expose/abort and is never returned to consumers. A safer equivalent private owner method that consumes this capability is acceptable, provided no generalized root/pool mutation API is added.

`ConversionError` preserves three exact sources: `Completed(CompletedError)`, `Owner(OwnerError)`, `Runtime(RuntimeError)` (the last for `FrozenOutput::new`). Typed `SourceError::{Missing,Deleted,ForeignView,Storage}` must remain inspectable inside `Completed`; never collapse to `Batch`, `Shape`, null or empty success. No string formatting, public ZeErrorCode or diagnostic allocation is required. The future real Completion/public mapping remains ZE-53/68/69.

No callable function accepts `PreparedGraphResult` plus a context. A file-private mapping helper may borrow the locally created prepared owner only inside `prepare_native`. A second context sharing `QueryMemory` must never supply the work or view for a separately prepared owner. Do not add memory-identity-only authentication or a public prepared-plan/result handle.

`ResultSource::result_input()` is invoked once by `PreparedGraphResult::copy_from`. Every subsequent value, row, generation and outcome comes from that owned copy's stable metadata/pools, never a second source observation. The source retains its real backing through the synchronous call; ownership/accounting of that source remains its producer's obligation. This module does not infer capacity from source slice lengths.

## 3. Exact preparation and finalization order

1. Call the existing `PreparedGraphResult::copy_from(source, context)` immediately. Its close-first check and exact `ptr::eq(input.view, context.view())` precede native allocations/copy. All native validation and its scratch/capacity reservations remain intact.
2. Read native rows/admitted generation/outcome/pools once from the prepared owner. Map outcome before C allocation. `Committed.changed` must become checked `NonZeroU64`; native validation already requires checked admitted+1. Unexpected zero is typed `CompletedError::Shape`, never unchecked construction or a fallback.
3. Build checked final 14-pool counts and work ranges; perform every fallible count/range/conversion/layout check now. Compute exact control reservation and wrapper/initializer overhead as described below. Reserve actual C arena, registry node and control capacity while the full native owner and source owners are live.
4. The common ZE-128 preparation implementation obtains the nonreused token, checks gate/full state, reserves, allocates one aligned arena, and invokes the fixed native initializer directly into final typed slots. Every array is initialized, including valid zero-valued global work descriptors. Fill creates the canonical root with admitted generation present (including zero), no changed generation yet, global range `{0,23}` and zero diagnostics.
5. Preserve ZE-128's pre-node checkpoint, second real allocation for the node, immediate OwnedNode RAII initialization, bounded second registry admission check, and final post-registration checkpoint. No private descriptor escapes on any failure. Early failure drops arena; late failure drops/unlinks the registered node before refunding charge.
6. Obtain the sealed 23-slot handle only after trusted initialization has succeeded. Compute C represented bytes through the existing owner measurement. Capture native represented bytes and rows. Drop the local native owner only after all C mapping is complete and the C private owner is held; both capacities overlap during conversion and the memory peak records this. There is no native detach or retained native copy after this point.
7. Return `FrozenOutput::new(prepared_wrapper, rows, native_represented, c_represented)`. The actual driver alone charges `CompletedBytes` and `CompletedAbiBytes`, checks final close/cancel/deadline, and snapshots `Execution.counters` and `memory.peak_reserved_bytes()`. If FrozenOutput creation, either final charge, shape comparison or final checkpoint fails, dropping the wrapper aborts the real registry owner. No completed descriptor is returned.
8. `finalize_native` consumes that `Execution`, takes the exact snapshot and performs only the 23 fixed scalar metadata assignments into the already initialized global rows: 22 counters in explicit C order and one peak. No checkpoint, work charge, source read, native-pool access, pool conversion, allocation, fallible function, formatting, registry operation or callback. No `Result` return. The existing descriptor headers/kinds/reserved fields are already correct; only `.value` fields need change. The fixed assignments are completion metadata (like ZE-128 expose's disposition assignment), not a new payload-copy pass. They cannot change the counter snapshot they describe.
9. ZE-68 consumes finalized parts, reconciles expected successful outcome with the real coordinator/outcome cell and only then uses existing expose. This module does not call `OutcomeCell::begin_attempt`, establish commit, choose registry policy, expose an uncertain result or authenticate an arbitrary manually constructed Execution. Its private caller contract requires the actual driver snapshot before real precommit/exposure.

The current driver is read/preparation-only. The test may exercise actual `execute_in`; it must not claim the absent write callback/commit window is implemented. The 23 metadata assignments must precede eventual real precommit. All later known-outcome/delivery rules remain on ZE-68/69.

## 4. Shared owner refactor and checked write geometry

Refactor `GraphResultRegistry::prepare` into a thin unchanged public Rust parts adapter over one private generic preparation helper. A suitable private helper takes `counts:[usize;14]`, `ResponseMetadata`, accounted initializer/control bytes, and a bounded initializer closure with `(&AlignedArena, &ArenaLayout, &mut RuntimeContext) -> Result<ZeGraphResponse,OwnerError>`. Only the two fixed internal initializers call it. It owns the entire existing token/reserve/allocate/admit/cleanup sequence. There must not be two registries, two implementations of admission, or a conversion-only bypass around final checkpoints.

Keep `ArenaLayout::new` as the single source of allocation geometry, using actual `Layout::new::<T>()`, checked size multiplication, `Layout::extend`, `pad_to_align` and the existing padded arena <=4 MiB restriction. Extract root shape validation to use counts and metadata so both callers receive exactly the same row/cell/global-range checks. Preserve the old parts path's 14 arrays, diagnostics, metadata and owner/free behavior unchanged.

For direct mapping, a private typed writer can accept a checked offset, a native slice and an infallible scalar mapper; it returns the final `*const Out`. At each chunk, check/charge `CopiedBytes` for exactly `chunk.len()*size_of::<Out>()` before writing, then immediately perform infallible fixed mappings and `ptr::write`. Chunk elements are `max(1,65536/size_of::<Out>())`; all used output types have nonzero size <=65536. No fallible operation goes between successful charge and those writes, and none of these mappers allocates. Output bytes may differ from native bytes. Bytes can use existing bounded copy. Work rows can use a fixed bounded generation loop with the same reservation/charging policy. No intermediate output Vec, Box, String, collected iterator or typed alias of Rust enums is allowed.

The private helper's unsafe contract is narrow: `T` matches the fixed typed pool whose checked offset/count were computed. Prove offset+count*size <= arena layout size using checked operations before allocation/write; ensure base+offset alignment for T. `Layout` establishes Rust's allocation/isize extent restrictions. For zero count return null and perform no pointer arithmetic, raw slice construction or dereference (null plus length zero is not a Rust slice). Do not allocate zero-sized arenas through alloc; the native conversion always has 23 work rows, so even logically empty native results have a nonempty C arena.

Every count stored in an internal u32 range/index must use a checked conversion. Compute report work count `23 + 22 * reports.len()` with checked mul/add and checked u32 conversion; reports <=8, total work <=199. Report ordinal i has start `23+22*i`, count22; global range starts0,count23. All ranges are disjoint/contained; reports retain original order. Native `Span` maps start unchanged, len to count unchanged. Never normalize present-empty start to zero. Unused/absent ranges are independently all zero. Native validation already checked all source ranges, row product, index/type/UTF-8/list/order invariants; the converter must neither weaken it nor reimplement a conflicting validator.

All actual C descriptors get `abi_size = size_of::<ThatExactType>()` and every reserved/inactive field canonical zero. Use explicit typed constructors or zero-initialize only the known all-scalar/raw-pointer C structs, with a safety comment. Never zero an enum-bearing native struct. Do not read/compare struct padding as initialized bytes in tests; assert fields/float bits and typed extents.

## 5. Complete pool and value mapping

C allocation order is frozen by the existing layout, not native declaration order:

| C pool index | C output | Native input | Exact rule |
|---|---|---|---|
|0|ZeGraphValue|values|Map all variants below, preserve order|
|1|u32 children|children:ValueIndex|`.0`, exact repeated indices/order|
|2|u8 bytes|bytes|Byte-identical bounded copy, including NUL and UTF-8 boundary bytes|
|3|ZeGraphNode|nodes|Complete mapping below; preserve full-ID order|
|4|ZeGraphRelationship|relationships|Complete mapping below, original directed endpoints|
|5|ZeGraphProperty|properties|header, name Span, `.value.0`, reserved0|
|6|ZeGraphRange names|names|start/count exactly|
|7|f32 vectors|vectors:u32|`f32::from_bits`, no arithmetic/conversion|
|8|ZeGraphColumn|columns|header, name, explicit kind mask, reserved0; preserve duplicate names/order|
|9|u32 cells|cells:ValueIndex|`.0`, preserve row-major order and all duplicate rows|
|10|ZeGraphReceipt|receipts|Original item, identity/revision/generation/deleted/replayed mapping|
|11|ZeGraphSearchReport|reports|Every field below plus distinct work range|
|12|ZeGraphDiagnostic|none|count0, null pointer; successful native input has no diagnostic pool|
|13|ZeGraphWorkCounter|synthesized metadata|23 global placeholders plus22 per native report|

For every value start from canonical inactive zeros and exact header:

- Null -> tag0; Bool -> tag1 and boolean exactly0/1; I64 -> tag2 and exact signed64; F64 bits -> tag3 and `f64::from_bits` (signed zero, subnormal, infinities, quiet/signaling NaN payloads retained with no arithmetic).
- String -> tag4, exact byte range; Node -> tag5 and exact native node index; Relationship -> tag6 and exact native rel index; List -> tag7, exact child range and explicit list-kind mapping.
- ListKind mapping by exhaustive match: Query0, Bool1, I642, F643, String4, Empty5. Native enum ordering is Query,Empty,Bool,I64,F64,String; an integer cast is wrong. Empty stored sentinel requires zero children, already validated. Query-empty, each typed-empty and stored Empty remain distinct. Mixed/nested/null query lists and duplicate child references preserve multiplicity, depth and postorder indices.
- Column `ValueKinds`: explicitly OR native `.contains(NULL/BOOL/I64/F64/STRING/NODE/REL/LIST)` into C bits1,2,4,8,16,32,64,128. Do not expose or transmute the native private u16 field. ANY maps255 and each individual/union test uses literal expected bits.
- `u128` identity -> high `(id.get() >> 64) as u64`, low `id.get() as u64`, for node IDs, rel IDs, both endpoints and receipts. Use the actual native typed ID accessor; never signed i64, f64, DocId, low-only equality or formatting/parsing.

Node maps: id; revision.get(); generation.get() -> last_change_generation; properties and labels exact spans; key None -> has_key0, namespace/key0; Some -> has_key1 and both exact spans even when empty/NUL; text None -> has_text0,text0; Some -> has_text1 and exact span including empty; vector None -> has_vector0,vector0; Some -> has_vector1 and exact nonempty validated span. Do not infer presence from length. Preserve finite f32 bits including negative zero/subnormals. Do not synthesize text/vector from other fields. Zero-length present vector is a typed native Shape error and must not be accepted by the converter.

Relationship maps id, source, target, revision, last_change_generation, key presence/ranges, properties and relationship_type exactly; no traversal reversal, endpoint lookup or recursive node duplication. Node/relationship properties are already stored-value valid, ordered and unique; retain them unchanged, including typed-list/Empty values and exact property names.

Receipt maps `item_index -> item`; `EntityId::Node` -> kind0/node exact/relationship zero, `EntityId::Relationship` -> kind1/rel exact/node zero; `replayed` true -> disposition3(Replayed), false ->2(Committed), deleted ->0/1; exact original revision/generation. Preserve mixed committed/replayed batch receipts and deletion incarnations. Root outcome mapping never rewrites per-item generations or dispositions.

Outcome maps exhaustive `Read -> SuccessfulOutcome::Read`, `Committed{changed}->Committed(nonzero changed)`, `Replayed->Replayed`, `NoOp->NoOp`. Expected C root dispositions after existing expose are0,2,3,4; only Committed has changed generation. Admitted generation is present for all valid inputs, including0. There is no conversion success for NotCommitted or Indeterminate: those are coordinator/error states, absent from native Outcome. No diagnostic construction or last-error mutation here.

## 6. Every search report field and work row

Map report fields exhaustively, preserving native validated values:

- call.0 -> call_id; generation.get(); kind Vector->ZeGraphSearchVector(0), Lexical->ZeGraphSearchText(1), Hybrid->ZeGraphSearchHybrid(2).
- requested_tier None -> has0,tag0; Some Auto -> has1,tag0; Exact ->1,1; Scan ->1,2; Graph(_) ->1,3. Graph request options are intentionally not repeated by the frozen response schema; do not put them in reserved fields or claim reversible request serialization.
- actual_tier None -> has0,tag0; Some Exact/Scan/Graph -> has1,tag1/2/3. Actual Auto is impossible. Requested route does not certify actual route or coverage.
- precision NotApplicable/Original/Quantized/Mixed ->0/1/2/3. Coverage Exact/Approximate ->0/1. Both legs independently map NotRequested/Nonempty/NoIndexedPopulation/NoEligibleMembers/NoQueryMatches ->0/1/2/3/4.
- document/query/tokenizer epochs: separate has flag and value (`None => 0,0`, `Some(0) => 1,0`). Do not derive presence from value.
- effective_alpha_bits -> f64::from_bits; normalization_version, rules_version, candidate_count, cross_scored_count, fallback_count unchanged; cross_score_complete ->0/1; work exact assigned per-report range. Do not recompute precision/coverage/empty legs/fallback from count or infer missing scores as0.
- Nullable hybrid component yields are native `Value::Null` versus scalar `F64(0)` cells. Include both in the mapping fixture even though reports themselves contain no component-score fields.

One static exhaustive array of pairs maps the runtime counter to C kind; never transmute enum discriminants. Order and IDs are:

`0 OperatorRows, 1 AdjacencyEntries, 2 Expressions, 3 HashProbes, 4 CompletedRows, 5 CompletedBytes, 6 PreparedPayloadBytes, 7 CompletedAbiBytes, 8 VectorCoordinates, 9 VectorBytes, 10 LexicalPostings, 11 LexicalBlocks, 12 SearchInvocations, 13 Lookups, 14 Scans, 15 Paths, 16 RowsIn, 17 RowsOut, 18 JoinProbes, 19 GroupKeys, 20 EligibilityEntries, 21 CopiedBytes, 22 PeakOwnedBytes`.

Each work descriptor has exact header, kind, reserved0 and value. Global rows0..21 are initialized zero in preparation, finalized from `execution.counters.get(runtime_kind)`, and row22 receives `execution.peak_query_bytes` as u64 (supported platform usize is64-bit). Per-report22 rows use that report's native `work.get(kind)` during preparation and are never touched by finalization. No per-report peak is fabricated, no summing reports replaces global counters, no row-derived estimates or TCK side-effect counters are added. Use a separate literal expected table in tests so changing production mappings cannot update the oracle accidentally.

## 7. Exact memory/work ledger and limits

Maintain three distinct quantities:

1. Native represented bytes `R_native = size_of::<CompletedGraphResult>() + sum(size_of_val(each of12 initialized native pools))`, measured by `PreparedGraphResult::represented_bytes()`. It must be <=4MiB. Its native copied-byte work is only the sum of pool bytes, excluding the descriptor.
2. C represented bytes `R_c = sum(count_i*size_i for all14 C arrays)`, measured by prepared owner's `represented_bytes()`. Includes all global/per-report work rows, excludes padding, root/node/control descriptors. It must be <=4MiB. `ArenaLayout` independently enforces padded arena `A_c<=4MiB`.
3. Actual simultaneous charged capacity: preexisting caller/source/plan/driver owners + native actual Vec capacities and their12 QueryArena controls + native PreparedGraphResult control (and validation scratch while live) + `A_c + size_of::<registration::Node>() + all explicit C preparation/wrapper controls`. This is nested in the one QueryMemory<=24MiB and the same GraphResources aggregate<=256MiB; no new independent allowance. Real writer64MiB ownership/adoption is an integration obligation, not something to simulate here.

Common owner control reservation must include the existing `PreparedResponse` excluding its already independently charged external-guard descriptor, `ArenaLayout`, `ResponseMetadata`, counts/initializer descriptor, and native wrapper's additional slot/outcome state. Define a named fixed initializer descriptor if needed so it can be charged by `size_of`, rather than a hidden unmeasured closure capture. A coherent formula is:

`total C retained reservation = size_of<QueryExternalReservation> + A_c + size_of<Node> + common_controls + initializer_controls + wrapper_extra`.

`common_controls` includes `size_of<PreparedResponse>-size_of<QueryExternalReservation> + size_of<ArenaLayout> + size_of<ResponseMetadata>`; `initializer_controls` includes exact fixed counts/source descriptor/capture sizes used by that path (old path includes ResponseParts; native path includes Pools and fixed conversion geometry); `wrapper_extra` covers the largest live moved-result envelope (`FrozenOutput<PreparedNativeResponse>`, `Execution<PreparedNativeResponse>`, prepared/finalized wrapper), minus the already counted PreparedResponse, plus any fixed slot table/map scratch retained on stack. Document conservative control over-reservation separately from actual heap; do not call it an allocator byte measurement. Do not charge static read-only tables as per-query heap. Document the final compiled formula and measure exact resulting sizes; count one actual external guard exactly once. Preserve or conservatively extend existing old-path accounting, never subtract controls to make a test pass. A separate external control guard is permitted only if it truly owns separate live state and drops after that state/backing; avoid a redundant heap allocation.

Reserve these bytes before either C allocation. No native owned lifetime or charge may be erased via transmute/ManuallyDrop/casting/adopting mere slice lengths. The existing source producer is responsible for its authentic source capacity; test the composition using QueryArena-backed source pools with deliberate spare capacity under the same memory. Ordinary literal static fixtures do not prove source capacity accounting. Peak is the actual monotone reservation peak, not `R_native+R_c`, not allocator live bytes alone.

Work accounting:

- Native validation continues using its existing cumulative ValueContext steps; native copy charges its real pool bytes once.
- C direct writers charge exactly R_c in CopiedBytes once (including initializing global work descriptors), in chunks<=65536 bytes. Do not charge padding or registry/control bytes as copied payload. Fixed final metadata updates do not add a second payload pass.
- The converter does not charge CompletedRows/CompletedBytes/CompletedAbiBytes/PreparedPayloadBytes; the driver owns them at existing sites. `FrozenOutput` advertises exact native/C represented bytes only.
- Under actual execute_in the final expected CopiedBytes is prior/prepared-row copy work + native pool copy bytes + R_c. The expected final CompletedBytes and CompletedAbiBytes add R_native/R_c exactly once to prior counters. Failed unconsumed charge requests do not appear. Native-valid input with expanded C descriptors over the C cap fails typed OwnerError::Limit and cleans native capacities before any C arena allocation.

Bound tests must cover checked counts/product overflow, native/core limit, padded ABI limit, query limit, aggregate refusal, work limit, cancellation/deadline/close, registry full/busy/poison and actual allocator failures. Do not relax any limit or add best-effort partial conversion.

## 8. Focused tests and literal RED -> GREEN sequence

New private tests: `crates/zeppelin-embed-ffi/src/graph_result/conversion/tests.rs`, optionally `conversion/fixtures.rs` under cfg(test). Reuse existing `graph_result/audit.rs` thread-local actual System allocator; do not introduce a competing global allocator or enable core allocation-audit in the same FFI lib test binary. Existing `graph_result/tests.rs` and registration tests are mandatory regressions. Any new test-only inspection in registration stays under cfg(test).

Use test names below with `graph_result_native_` prefix. Establish each named test or small group first, run and save intended RED, then implement the smallest complete corresponding behavior and rerun terminal GREEN. Initial missing-module/API compile RED is legitimate but label it separately; it is not evidence for field correctness. After compiling skeletons exist, require runtime assertion REDs or deliberate production mutations for semantic/resource properties. Do not leave stubs, skipped tests or intentional failing assertions in final source.

A. `graph_result_native_all_pools_match_literal_field_oracle`: one complete read fixture with all value variants, all list kinds (empty and nonempty), nested repeated query children, original IEEE scalar bit patterns, UTF8/NUL strings, duplicate cells/rows/column names, typed properties, two nodes whose high halves differ but low halves agree, relationship IDs/endpoints/revisions/generations, keyed/unkeyed records, text absent/empty/nonempty, original f32 signed-zero/subnormal vector. Directly inspect every initialized C field, all inactive/reserved zeros and all14 pointer/count pairs against independent literal expected values, not conversion helpers. Include empty native input and nonzero zero-column rows. The diagnostics pointer/count is always null/0; global work is still present. A valid `Some(empty vector)` must not be manufactured; test its actual Shape rejection separately.

B. `graph_result_native_reports_receipts_outcomes_are_lossless`: separate read/report, committed mixed receipts, replayed receipts and NoOp fixtures honoring native validation. Include node/rel/deleted receipts, original generations, no changed generation for replay/NoOp/read, committed admitted+1. Table-driven valid reports cover every enum mapping, None versus Auto, all explicit tiers, all precision/coverage/leg states, each epoch None/Some(0)/Some(nonzero), nontrivial versions/counts/alpha bits and dropped-score zero-row result. Do not combine contradictory report modes merely to exercise tags; rejection remains typed. Include eight-report maximum and report22-row disjoint ranges, null component versus numeric zero cells. Native source errors/malformed ranges/UTF8/list cycles/outcome mismatch/report contradiction must be preserved with zero owner exposure.

C. `graph_result_native_context_identity_and_source_snapshot_are_single`: different QueryView pointer with identical store/generation and same QueryMemory fails exact ForeignView before C allocation; close takes precedence over simultaneous cancellation; source's input call count is exactly1 and a source that would change second observation cannot affect output. Positive case carries prior runtime/value work through preparation. The only native conversion entry takes source+context; do not test or implement a forbidden prepared-owner overload.

D. `graph_result_native_driver_finalizes_all_23_exact_counters`: port actual with_plan/execute_in fixture above using real QueryArena/fact ownership and one zero-column row (plus separate nonempty table fixture if useful). Its test Completion builds a ResultInput from `context.view()`, calls private prepare_native and returns the FrozenOutput. A test-only diagnostic carrier may retain ConversionError when translating unexpected Completion failure to RuntimeError; do not add a production lossy mapping. Seed all22 prior work kinds with distinct known legal amounts; add actual value validation/operator/copy/final work. Record the returned Execution snapshot, finalize it, then compare literal C kind order, all22 exact values and peak; compare a separately calculated known-delta subset for CopiedBytes/CompletedBytes/CompletedAbiBytes/CompletedRows so copying an already wrong snapshot is not the only oracle. Per-report counters must stay independent. Deny all System allocations during finalization/into_parts/expose/free and assert zero attempts, no context counter/peak delta, same arena pointers and real charge release. Compile an ordinary graph-only build; do not let cfg(test) be the sole compiled consumer.

E. `graph_result_native_geometry_limits_and_real_overlap_reject`: independently calculate layout align-up offsets/sizes for all14 pools, check actual base+offset pointers, disjoint extents and null-at-zero. Keep existing layout overflow tests. Build a native-valid large values pool (choose N = floor(4MiB/size_of<ZeGraphValue>)+1; independently assert native represented bytes remain <4MiB) to force C expansion over cap before C allocation. Use same-query QueryArena source backing with spare capacity, measure prior/native/C/control and peak overlap, exact query and shared refunds. Tighten one byte below measured required query reservation to force genuine overlap refusal. Separately saturate actual aggregate with a held unrelated graph reservation and verify actual shared refusal, followed by release and clean success. Never replace these by manually incremented fake byte counters.

F. `graph_result_native_every_allocation_and_copy_checkpoint_cleans`: after fixture setup audit a clean full conversion to establish actual native-validation/native-pool/C-arena/node allocation inventory. Fail each observed System allocation ordinal in separate fresh cases; require exact typed Memory/Allocation error, no descriptor, no live-byte delta and baseline query/shared charges. Separately use existing AllocationFaultScope for the two C sites and require receipt matching_sites=1/2,fires=1. Sweep every actual checkpoint ordinal from clean retained-view polling (at least one >64KiB byte input and one >64KiB descriptor output), inject cancel exactly there, and check exact performed CopiedBytes prefixes and complete abort. Identify native versus C versus post-registration/final-driver sites in evidence. Include work tight ceilings at native/C chunk seams and each final Completed byte charge. Expired deadline and final driver close-first/cancel checks must drop already registered owner. Use real control APIs and captured typed errors, no sleeps or random scheduling as deterministic evidence.

G. `graph_result_native_private_drop_and_source_independence_are_heap_flat`: private descriptor free rejected; dropping unfinalized/finalized owner aborts, then stale descriptor rejected; normal finalized expose/free, forged pool/count/token, stale copy and second free retain existing contracts. Read copied payload after all source/native/context/view fixtures are dropped (no claim of actual GraphStore close). At least32 full-fixture expose/free and32 prepare/abort loops have exact allocator allocations=frees and zero net live bytes. Preserve the existing all42-field and concurrency owner tests rather than duplicating their whole implementation.

No coverage percentage is invented. Scoped tests are required; crate-wide>=90% and full sanitizer/size/feature qualification remain tracked on ZE-118 with the actual integrated commit.

## 9. Mandatory adversarial route and independent controls

Run existing PG15/PG16 direct probe and actual runner tests before changing production. This refactor changes materialization failure/order paths, so directed runner extension is mandatory now, not deferred.

Extend existing PG16, not a new uncoordinated PG number. Add a cfg(graph-result-test-support)-only test bridge under `crates/zeppelin-embed-ffi/src/graph_result/test_support.rs` with child `test_support/conversion.rs`. It invokes the actual private converter; it must not reimplement mappings, return success placeholders, expose the converter in ordinary graph/default builds, add a C symbol, or make an independently prepared native result a public test shortcut. It owns only an explicitly test-only fixture, not a production ResultSource. It returns primitive observations/fault receipts after actual finalize/expose/free, never mutable production owners.

The bridge interface is `pub fn run_native_conversion_case(context: &mut RuntimeContext<'_, '_, '_>, case: NativeConversionCase<'_>) -> Result<NativeConversionObservation, NativeConversionFailure>` under that opt-in module only. `NativeConversionCase` carries primitive full-u128 node/relationship IDs, scalar u64 IEEE bits, borrowed UTF-8 payload and outcome/report fixture selector. The caller constructs the one real context with selected memory/work/cancel/deadline controls and separately arms existing AllocationFaultScope; the bridge constructs charged native fixture pools, an actual charged Unit plan/facts and test-only Completion, invokes execute_in -> prepare_native -> finalize_native -> into_parts -> expose -> primitive observation -> free. Source cells may be empty with one zero-column row while the complete node/list/report pools remain present; driver rows and ResultInput rows must agree. Source fixture buffers use QueryArena under context.memory(), including deliberate spare byte capacity for overlap proof. The source's view is context.view(). Static strings or unaccounted fixture Vecs cannot stand in for the overlap fixture.

`NativeConversionFailure` is test-only and preserves `ConversionError` (stored in the Completion on failure before returning RuntimeError::Batch through the fixed Completion trait) or the actual `RuntimeFailure` when the driver fails after successful completion. Its public test-facing classification is a primitive refusal enum and stage; callers check C-stage preconditions/actual allocation receipts rather than credit earlier native refusal as a C fire. Do not expose private ConversionError as a public type: the bridge owns it internally and reports exact primitive classification plus observed work/charge data. Record stage only after the actual successful boundary, using cfg(feature) observation state if needed; no success simulation. Keep primitive observation allocation outside audited/denied windows and free the root on every observation-error exit using a test-only RAII guard. Real field comparison stays outside the FFI helper in the oracle below; helper success flags alone are not evidence.

Allowed tooling edits:

- `tests/adversarial/graph_response.rs`: additive native conversion probe, seeded through `super::test_support::seeded_rng("property_graph::native_response_probe", seed)`, actual source/core/C owners and real fault/control pairs. Keep every old PG16 case.
- `tests/adversarial-oracle/src/graph_response.rs`: add primitive observation structs/checker for high/low IDs, IEEE bits, list/presence tags, report/global work and refusal receipts. It imports no engine/C/native conversion/layout helpers. Expected values come from primitive fixture inputs and literal schema integers. Negative controls modify observed high bits, float bits, presence/list tag, per-report range, final global count, owner refund and missing fire; each must fail.
- `tests/adversarial/coverage.rs`: add exactly these eight feature-gated keys: `property-graph.response.native-map`, `.native-context`, `.native-final-counters`, `.native-overlap`, `.native-allocation.fire`, `.native-cancel-work.fire`, `.native-same-seed-control`, `.native-oracle.can-fire` (all with the same full prefix).
- `tests/adversarial_tests.rs`: focused `property_graph_native_response_probe_checks_conversion_and_paired_faults` and `one_runner_episode_reaches_required_native_response_contracts`; add native keys to PG16's required inventory; exact feature key count12 becomes20 with all original12 retained. Preserve exact count semantics, not >=/contains-only replacement. If a different final additive key set is necessary, document it with root before shared integration.

The existing `tests/adversarial/runner.rs:2916` already calls graph_response::probe under the hook. Call the additive native probe from there indirectly by extending that existing probe; no runner.rs edit is necessary. Old required key names/feature gates remain. Coordinate shared coverage/tests file handoff with root; no blanket replacements over concurrent work.

Four directed seeds `[0,1,141,u64::MAX]` must cover actual C arena and node failures after native copy, in-conversion cancel, copied-work limit, genuine source/native/C memory refusal, matching clean controls and exact primitive mapping. Faults must fire in the real intended C stage (native preparation must have succeeded before an intended C fault), with receipts/positive preconditions to prevent false credit for an earlier unrelated refusal. Every fault has byte-identical seed/source clean control, real baseline refund and exact mismatch-sensitive oracle. Add actual canonical runner seed141 and verify all old+new keys, zero violations. This proves component composition only.

## 10. Deliberate production mutations and restoration protocol

After each relevant GREEN, plant one bounded defect, run the named test that should kill it, restore exact source bytes in `finally`, then rerun terminal GREEN. A compiler failure from a malformed mutant is not semantic can-fire evidence. If a redundant guard survives a mutation, record it as surviving/redundant rather than claim kill; target the actual property without weakening a separate guard in final code.

Required mutation matrix (each needs exact before/mutant/restored SHA256, replacement, command, expected assertion and exit status in `tasks/evidence/ze-141/mutations.json`):

1. ID high half set0 -> all-pool exact high-ID assertion fails.
2. F64 negative zero/NaN payload normalized -> `to_bits` assertion fails.
3. present empty text/Some(0) epoch presence cleared -> presence assertion fails (cover both separately if one mutant cannot exercise both).
4. ListKind::Empty mapped Query or direct native discriminant -> list-tag assertion fails.
5. replayed receipt forced Committed -> per-item disposition assertion fails.
6. report work start uses22*i or aliases global0 -> exact range/counter assertion fails.
7. final global CompletedAbiBytes read from unfinished metadata/zero or peak replaced0 -> actual-driver final snapshot assertion fails (both counter and peak require controls).
8. one C chunk CopiedBytes charge removed -> known-byte-delta and C-work refusal assertion fails.
9. native owner released/charge suppressed before C capacity overlap (use a narrowly controlled test-support mutation if Rust borrowing rejects premature real drop) -> genuine overlap/charge ledger assertion fails. Never commit an unsafe ownership cast to make a mutant compile. If legal production premature-drop mutant cannot be made, an accounting mutation in owner reservation that removes known C capacity while leaving live allocation is the executable control, and report its precise narrower property.
10. final post-registration checkpoint omitted -> scheduled last-checkpoint cancellation test returns success incorrectly and fails.
11. introduce `Vec::<u8>::with_capacity(1)` retained through finalization -> actual deny-all allocator abort/failure. Run isolated child, restore exactly. Do not call this a real commit-window test.
12. remove additive native probe invocation -> actual canonical runner required-key assertion fails; suppress one actual C allocation-fire receipt -> primitive fault checker fails. Restore each separately.

Before mutations save the complete final owned source inventory and raw file bytes outside the worktree. Script must assert exact occurrence counts before replacement and refuse source drift. Restoration hash must equal pre-mutation hash for every changed file, followed by `git diff --check` and terminal focused GREEN. Use proper shell quoting/structured files; do not interpolate source text through unescaped shell code. Planning-pin hashes are recorded in `/tmp/ze-141-plan-source-sha256.json`; postimplementation mutation originals cannot be known in this plan and must be measured, not fabricated.

## 11. Exact focused commands and evidence

Execute from the isolated worktree. Every nextest command uses `--profile default -j 4 --retries 0`; `.config/nextest.toml` already supplies `run-extra-args=["--test-threads=1"]` and process isolation. Preserve that file. Run only named targets/filters; no broad workspace/adversarial campaign or benchmark. Save stdout/stderr plus exact exit status without losing failures through a pipeline. `ZE_TEST_SEED=141` fixes seed where tests use it; explicit multi-seed fixtures still run their declared seeds.

Baseline before edits and terminal regressions after restoration:

```sh
ZE_TEST_SEED=141 cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed-ffi --features graph-cypher --lib -E 'test(graph_result_)'
ZE_TEST_SEED=141 cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed --features graph-cypher --test graph_completed_results --test graph_compiled_context
ZE_TEST_SEED=141 cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed-workspace-tests --features graph-result-test-support --test adversarial_tests -E 'test(completed_owner_probe_preserves_bits_bags_and_real_fault_controls) | test(one_runner_episode_reaches_completed_owner_controls) | test(property_graph_response_probe_checks_real_owners_and_paired_faults) | test(one_runner_episode_reaches_required_graph_response_contracts)'
```

New red/green groups run the exact test being introduced with `test(=fully_qualified_test_name)` (obtain it via `cargo nextest list`; do not accept zero matches). Terminal new suite and route:

```sh
ZE_TEST_SEED=141 cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed-ffi --features graph-cypher --lib -E 'test(graph_result_native_)'
ZE_TEST_SEED=141 cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed-workspace-tests --features graph-result-test-support --test adversarial_tests -E 'test(property_graph_native_response_probe_checks_conversion_and_paired_faults) | test(one_runner_episode_reaches_required_native_response_contracts) | test(graph_response_runner_keys_are_active_with_test_hook)'
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed-adversarial-oracle --lib -E 'test(graph_response)'
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed-workspace-tests --features graph-cypher --test adversarial_tests -E 'test(graph_response_runner_keys_are_absent_without_test_hook)'
```

Focused ABI regression because owner construction was refactored (no regenerated schema expected):

```sh
cargo nextest run --profile default -j 4 --retries 0 -p zeppelin-embed-ffi --features graph-cypher --test ffi_graph_contract --test ffi_graph_layout --test ffi_graph_header
cargo clippy -p zeppelin-embed-ffi --features graph-result-test-support --lib --tests --no-deps -- -D warnings
cargo clippy -p zeppelin-embed-workspace-tests --features graph-result-test-support --test adversarial_tests --no-deps -- -D warnings
cargo clippy -p zeppelin-embed-adversarial-oracle --lib --tests --no-deps -- -D warnings
cargo check -p zeppelin-embed-ffi --no-default-features --lib
cargo check -p zeppelin-embed-ffi --no-default-features --features graph-cypher --lib
cargo fmt --package zeppelin-embed-ffi --package zeppelin-embed-workspace-tests --package zeppelin-embed-adversarial-oracle --check
git diff --check
```

Run the FFI native tests additionally with graph-result-test-support if the C allocation-site controls are feature-gated; the ordinary graph-only run remains required for actual allocator audit. The baseline/terminal PG16 suite now transitively executes the additive probe; do not rerun an identical broad selection without changed source or unresolved evidence. No cargo test libtest-wide concurrency fallback.

Evidence directory `tasks/evidence/ze-141/` must contain README, exact plan (copied without changing its original hash), source-before/final SHA256 manifests, source/parent/worktree/feature/host/tool provenance, exact test commands/results/counts/skips, raw log inventory with hashes, mutation script and mutation manifest, actual allocation/byte/counter/work/peak/refund measurements, four-seed direct probe and actual runner receipts. Capture host model, RAM, uname/sw_vers, rustc/cargo/nextest versions. All claims name synthetic bounded fixtures and distinguish actual system allocator audit from explicit C allocation-site injection. Preserve harness mistakes/compile RED separately from intended behavioral RED. Include literal number of23 global/22 per-call rows, measured sizes, reservation formula and output/allocation inventories.

Do not claim planning tests passed: this plan ran none. Record implementation limitations: no real GraphStore producer/admission, no public graph C export or free entry, no actual write commit/outcome reconciliation/delivery window, no real graph close/reopen/relocation, no executed TCK, broad campaign, crate-wide coverage, performance benchmark, minimum-OS/Windows/Intel/sanitizer/shipping acceptance. Native query sources/test leases and actual private owners prove only this seam.

Commit only owned implementation/tooling/evidence paths with a `ZE-141:` title and wrapped body naming observed RED tests/mutations and terminal GREEN commands/results. Root reviews and integrates the individual commit; no push. Broad followups are ZE-118 and must be bound to the actual integrated commit. Executor must update ticket progress when state changes; root controls final closure/integration.

## 12. Retained ownership and acceptance checklist

- ZE-53: authentic admitted base/allowed-overlay entity ResultSource, native Completion, real read/write/search execution, full typed error assembly, counter sequence, close-drain and actual native result lifetime through close/reopen/relocation.
- ZE-68: consumption of this compiled private converter in the actual coordinator/precommit callback, actual source/core/C ownership and writer/query/shared overlap, global registry admission policy, Busy/Poisoned ownership/retry policy, authenticated outcome reconciliation and true allocation-denied commit/delivery window, actual C-owner lifetime after graph close/reopen.
- ZE-69: public handle/request validation/marshalling, exports/matching free, error/disposition/diagnostic/unwind delivery and public parity; ZE-67 layouts remain frozen; ZE-107 retains shipping artifacts/header/export/Swift/minimum-platform work.
- ZE-64 retains ranking truth and source reports; converter preserves but cannot establish exactness/coverage. ZE-118 retains broad final qualification.

The implementation is ready for review only when complete mapping, actual same-context driver finalization, checked aligned geometry, exact accounting/fault cleanup, every mandatory production mutation/restoration and canonical changed-path runner route all have concrete evidence. Success in a synthetic fixture never marks ZE-53/68/69 acceptance complete.
