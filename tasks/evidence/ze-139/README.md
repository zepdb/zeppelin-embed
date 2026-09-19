# ZE-139: native-to-C graph result conversion readiness

## Verdict

**Ready as one independent production component.** The compiled native owner
and C owner expose enough real ownership to implement a complete, bounded,
lossless conversion for the fixed ZE-67 response contract before the missing
GraphStore producer and public C coordinator exist. The component must borrow
`PreparedGraphResult`, write each field directly into the final aligned C
arena, and pass through the real `GraphResultRegistry` private admission. It
must not build temporary `Vec<ZeGraph*>` pools or reinterpret native owners.

Checking only `QueryMemory` identity would be insufficient. A caller can create
a second `RuntimeContext` over the same memory with a different view token and
fresh counters, then attach that context's work to a prepared owner. The private
conversion entry must therefore accept `ResultSource` plus one context, call
`PreparedGraphResult::copy_from`, and convert that local owner synchronously in
the same call. No conversion entry accepts an independently prepared owner plus
an arbitrary context. The existing exact `input.view`/`context.view()` check
then binds source, native copy, C copy and counters without inventing admission.
The prepared owner otherwise exposes all twelve validated pools, stable
rows/generation/outcome, represented bytes, and real retained charges.
([completed.rs:197-214](../../../crates/zeppelin-embed/src/property_graph/query/completed.rs#L197-L214),
[completed.rs:249-258](../../../crates/zeppelin-embed/src/property_graph/query/completed.rs#L249-L258),
[runtime/batch.rs:386-389](../../../crates/zeppelin-embed/src/property_graph/query/runtime/batch.rs#L386-L389))

Audit base: `a3ecc9483723967f5a70fbbf47909b57d3399018`. This
source audit read the full canonical `bindings.md`, `execution.md` and
`parallel-contracts.md`, changed no product source and ran no product test.

## Why the component is substantive and complete

The native owner is not a proposed shape. It validates row geometry, a 4 MiB
represented cap, UTF-8, postorder list DAG/depth/descendants, value/entity
indices, ordered labels/properties, records, receipts and reports before copying
all twelve pools into charged `QueryArena`s. Copying is polled and charged in at
most 64 KiB pieces. No view or caller lifetime enters the pool elements.
([completed.rs:238-315](../../../crates/zeppelin-embed/src/property_graph/query/completed.rs#L238-L315),
[completed/validate.rs:103-215](../../../crates/zeppelin-embed/src/property_graph/query/completed/validate.rs#L103-L215),
[completed/validate.rs:277-374](../../../crates/zeppelin-embed/src/property_graph/query/completed/validate.rs#L277-L374))

The C owner is also production ownership, not a prepared byte fixture. Its
checked `Layout` covers fourteen typed arrays with a separate 4 MiB ABI cap; it
reserves padded arena, registry node and control capacity before allocation,
admits an actual private registry node, and exposes only after initialization.
Free authenticates the complete registered root/pool geometry and releases the
authoritative arena. Its current `ResponseParts` input is a slice adapter, not a
requirement that conversion first allocate fourteen temporary arrays.
([graph_result.rs:70-110](../../../crates/zeppelin-embed-ffi/src/graph_result.rs#L70-L110),
[graph_result.rs:113-166](../../../crates/zeppelin-embed-ffi/src/graph_result.rs#L113-L166),
[registration.rs:105-198](../../../crates/zeppelin-embed-ffi/src/graph_result/registration.rs#L105-L198),
[registration.rs:199-299](../../../crates/zeppelin-embed-ffi/src/graph_result/registration.rs#L199-L299))

The runtime already has the exact finalization sequence the adapter needs.
`Completion` returns both represented byte counts while its owners are live;
the driver charges `CompletedBytes` and `CompletedAbiBytes`, performs the final
checkpoint, and then returns the one authoritative `WorkCounters` and measured
query peak. Conversion reserves all 23 global work rows before the driver
charges the represented ABI bytes. After `execute_in` returns,
`finalize_native` infallibly assigns those fixed rows from the exact
`Execution.counters` and `Execution.peak_query_bytes`. This is the C analogue
of `PreparedGraphResult::detach` replacing unfinished metadata: it is not
another native-pool conversion and adds no `CopiedBytes`. No variable-size
work, validation, allocation or fallible operation remains after this internal
driver return; finalization is still before C exposure and write precommit. The
fixed slots are already included in `CompletedAbiBytes`, and the copied-bytes
row already contains all bounded native-to-C pool-copy charges.
([runtime/driver.rs:124-170](../../../crates/zeppelin-embed/src/property_graph/query/runtime/driver.rs#L124-L170),
[runtime/driver.rs:394-409](../../../crates/zeppelin-embed/src/property_graph/query/runtime/driver.rs#L394-L409),
[completed.rs:340-365](../../../crates/zeppelin-embed/src/property_graph/query/completed.rs#L340-L365),
[registration.rs:303-351](../../../crates/zeppelin-embed-ffi/src/graph_result/registration.rs#L303-L351))

This implements a real reusable conversion/ownership component: native pools
remain authentically charged while the final C arena and registry backing are
simultaneously reserved and initialized, every fallible allocation/copy occurs
before publication, and the returned prepared C owner is independent of the
native owner. It does not require a public function, store handle, commit, fake
coordinator or source stub.

## Exact private interface and files

Add `crates/zeppelin-embed-ffi/src/graph_result/conversion.rs` with crate-private
types and functions equivalent to:

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

`prepare_native` calls `PreparedGraphResult::copy_from(source, context)` and
borrows the resulting local owner for direct conversion before returning. This
sealed call shape is the ownership check: a prepared owner cannot be paired
with a second context that happens to share its `QueryMemory`. `ConversionError`
is private and preserves the existing `CompletedError` and `OwnerError`; ZE-53
and ZE-68 retain the actual `Completion` and public error mapping. The returned
`FrozenOutput` carries the native and C represented byte counts into the
driver's existing final charges. All typed validation finishes while both
owners are private and before that return, C exposure or write precommit.

`PreparedNativeResponse` owns the actual private `PreparedResponse`, reserved
global work slots and the explicitly mapped successful native outcome.
`finalize_native` consumes the driver's exact counters/peak and infallibly
writes only the 23 reserved fixed metadata rows. `FinalizedNativeResponse`
yields the private `PreparedResponse` plus the mapped expected
`SuccessfulOutcome` to ZE-68; it does not expose a C response or authenticate a
commit.

Refactor only private initialization/admission helpers in
`graph_result.rs` and `graph_result/registration.rs` so both existing
`ResponseParts` tests and `prepare_native` use the same `ArenaLayout`, capacity
reservation, allocator, token and intrusive registry admission. The native
path supplies exact counts and fixed typed writer functions rather than slices.
No core ownership accessor, C header/export, error number or public C symbol is
needed; every conversion item remains crate-private.
Focused tests belong in `graph_result/tests.rs` or a private
`graph_result/conversion/tests.rs`; changed-path runner coverage extends
`tests/adversarial/graph_response.rs` and its primitive oracle. No dependency
or ABI declaration is needed.

## Exact mapping

| Native fact | ZE-67 C representation |
| --- | --- |
| `Span { start, len }`, `ValueIndex(u32)` | Same full `u32` start/count or index; checked arithmetic already precedes conversion. Present empty spans keep their start and zero count. |
| `Null`, `Bool`, `I64`, `F64(u64)` | Explicit value tag; bool becomes exact 0/1; integer is unchanged; `f64::from_bits` preserves all IEEE bits, including signed zero, infinities and NaN payloads. All inactive fields are canonical zero. |
| String, Node, Relationship, List | Exact byte/child range or entity index. Map every `ListKind` by match because native and C discriminant order differs; never cast enums. Null, empty string, query empty list, each typed empty list and the untyped stored-empty sentinel remain distinct. |
| Bytes, children, cells | Typed bounded copies preserving embedded NUL, order, repeated child indices and bag duplicates. Zero count publishes a null pointer. |
| Columns | Exact name range and an explicitly assembled eight-bit kind mask using the native `ValueKinds` constants. Duplicate display names and order remain unchanged. |
| IDs and records | Split every nonzero `u128` into high/low `u64`; map revision and last-change generation without narrowing. Preserve node labels/properties and relationship source/target/type/properties exactly. |
| Optional key/text/vector | Presence flags are independent of ranges. Absent writes canonical zero ranges; present empty key/text retains `has_* = 1`. Vector `u32` bits become `f32::from_bits`, preserving every validated finite bit pattern. |
| Receipts | Preserve original item order, entity kind, full identity, deletion, revision and original generation. Per-item `replayed` selects Replayed versus Committed, including mixed committed batches. The inactive node/relationship ID is zero. |
| Search reports | Preserve source-order call, generation, kind, requested-tier presence, actual-tier presence, precision, coverage, both leg states, epoch presence (including `Some(0)`), alpha bits, versions, counts and completion flag. Every enum is matched explicitly. |
| Work | Reserve 23 global rows in ABI `ZeGraphWorkKind` order (22 runtime counters plus `PeakOwnedBytes`), then 22 rows per report. `global_work` names the global rows; each report names its own range. Final global values come only from the driver's `Execution`; report values come from each owned native report. |
| Outcome | `Read`, `Committed { changed }`, `Replayed`, `NoOp` map to the four `SuccessfulOutcome` variants. Committed generation is nonzero because native validation requires admitted generation plus one. The converter records this expected mapping but ZE-68 alone may reconcile it with the real outcome cell and expose it. |
| Diagnostics | Successful native completed values contain no diagnostic pool, so conversion emits canonical zero diagnostics. Error diagnostic construction remains ZE-69. |

The fixed C structs have the required fields for these mappings.
([graph_contracts.rs:36-112](../../../crates/zeppelin-embed-ffi/src/graph_contracts.rs#L36-L112),
[graph_contracts.rs:175-294](../../../crates/zeppelin-embed-ffi/src/graph_contracts.rs#L175-L294),
[graph_contracts.rs:1034-1116](../../../crates/zeppelin-embed-ffi/src/graph_contracts.rs#L1034-L1116),
[graph_contracts.rs:1118-1284](../../../crates/zeppelin-embed-ffi/src/graph_contracts.rs#L1118-L1284))

`SearchTier::Graph` contains request-side traversal options, while the frozen
response field intentionally reports only a `ZeGraphTier` tag. Therefore
`Graph(_) -> ZeGraphTierGraph` is lossless for the accepted response contract,
but it is not a reversible serialization of request options. No plan requires
the response to repeat those options. If that requirement changes, ZE-67 needs
an additive ABI decision before conversion; this component must not smuggle the
options into reserved fields.
([records.rs:151-192](../../../crates/zeppelin-embed/src/property_graph/query/completed/records.rs#L151-L192),
[graph_contracts.rs:1226-1284](../../../crates/zeppelin-embed-ffi/src/graph_contracts.rs#L1226-L1284))

## Complete component acceptance

Literal RED then GREEN should prove the following focused boundaries:

1. One all-pool native owner converts directly into the final aligned arena.
   An independent field oracle compares all value/list kinds, full high/low IDs,
   node/relationship metadata, ranges, bags, receipts and every legal report
   shape. Deliberate high-bit, IEEE-bit, presence, list-kind, receipt-disposition
   and report-range changes must fail and restore exact source.
2. `None`, present empty and nonempty are distinct for key/text/string/list;
   omitted requested tier differs from explicit Auto; absent epochs differ from
   `Some(0)`. F64 and vector bit round trips use `to_bits`, not float equality.
3. Global work contains all 22 exact final driver counters plus the measured
   peak, and each report's 22-counter range is disjoint and exact. No value is
   read from the unfinished prepared metadata, estimated from row counts or
   duplicated as a TCK side-effect counter.
4. A source bound to a different view fails before C allocation/admission, even
   when its context shares the same `QueryMemory`. The positive case measures
   simultaneous native capacities, final padded ABI arena, node and controls
   under the one context. Represented core and ABI limits remain independently
   4 MiB; native-valid input whose C descriptors exceed the ABI cap rejects.
5. Direct conversion performs only the final arena and registry-node system
   allocations already owned by ZE-128. Fail each allocation, every bounded
   copy/cancellation site, registry-full/busy/poison and final checkpoint; each
   path drops private backing, restores charges and exposes no descriptor.
6. Conversion checkpoints and charges exact output bytes in chunks no larger
   than 64 KiB. Finalization performs no allocation, further native-pool
   conversion, registry growth, formatting, validation or callback; its 23
   fixed metadata assignments preserve the driver's counter snapshot and peak.
   Private free rejects before exposure; normal, empty, stale, forged and
   second-free behavior remains the ZE-128 contract.
7. Port the real
   `borrowed_driver_keeps_prior_validation_value_and_operator_work`
   `Completion`/`execute_in` fixture in
   `crates/zeppelin-embed/tests/graph_compiled_context.rs` into the private FFI
   conversion tests. Its completion uses `context.view()`, real
   `QueryMemory`/native/C owners and returns the prepared C owner; the test
   finalizes from the returned `Execution` and compares all 22 counters plus
   peak. This proves the private sequence, not a public coordinator.
8. A seeded changed-path probe composes the actual native owner with the actual
   C registry, fires conversion work/cancel/allocation refusal, runs identical
   clean controls and uses a primitive expected-value comparator. It explicitly
   claims component composition, not GraphStore admission or commit.

Run only the focused native/conversion/owner tests, the changed-path probe,
strict affected-target Clippy and formatting for this component. Broad
workspace, coverage, sanitizer and release campaigns remain ZE-118.

## Actual producers still missing and retained gates

There is currently no non-test `ResultSource`, no production call to
`PreparedGraphResult::copy_from`, no native `GraphStore` execution facade, and
no public `ze_graph_*` result/free entry point. The only current callers of the
native builder are tests. That is missing integration implementation, not a
missing conversion input contract: `ResultSource` and `PreparedGraphResult`
already define the exact owner/borrow boundary the component consumes.
([completed.rs:109-138](../../../crates/zeppelin-embed/src/property_graph/query/completed.rs#L109-L138),
[execution.md:7-18](../../../docs/graph/plans/execution.md#L7-L18))

- **ZE-53 retains** the real admitted base/overlay entity producer, native
  `Completion`, read/write/search composition, error and final counter
  sequencing, close drain, and native results surviving real close/reopen and
  relocation.
- **ZE-68 retains** the real write-result preparation callback, use of this
  adapter inside that actual driver/coordinator path, outcome-cell transitions,
  actual global registry policy, core/ABI overlap proof, allocation denial
  across the real commit/delivery window, Busy/Poisoned handling and actual
  close/reopen C-owner lifetime.
- **ZE-69 retains** public handle/request validation, exports and matching free,
  public error/disposition/diagnostic behavior, unwind/delivery handling,
  malformed honest-pointer tests and C-path parity. ZE-67 layouts remain frozen;
  ZE-107 retains artifact/header/export packaging.

These boundaries are the accepted plan: conversion and registry preparation
must precede irreversible writes, but component evidence cannot claim a real
commit, public execution, lifecycle, ABI success or TCK coverage.
([bindings.md:61-81](../../../docs/graph/plans/bindings.md#L61-L81),
[bindings.md:130-140](../../../docs/graph/plans/bindings.md#L130-L140),
[parallel-contracts.md:58-66](../../../docs/graph/plans/parallel-contracts.md#L58-L66))
