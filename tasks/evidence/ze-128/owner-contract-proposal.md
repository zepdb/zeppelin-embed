# ZE-128 compiled aligned response owner contract

Base: main 7028a42f2f4898bb7aa729dc935bd51b4033f2ba. The interfaces below
compile in the ZE-128 worktree; they are internal Rust components behind
`graph-cypher`, not C exports or graph admission/commit acceptance. The agreed
test seam is real prepare/expose/free ownership, allocation and outcome state.

## Compiled interface

`crates/zeppelin-embed-ffi/src/graph_result.rs` exports:

- `ResponseParts<'a>`: fourteen typed initialized C slices, no backing-ownership
  assertion. Arrays are values, children, bytes, nodes, relationships, properties,
  names, vectors, columns, cells, receipts, reports, diagnostics and work.
- `ResponseMetadata::new(row_count: usize, admitted_generation: Option<u64>)`,
  plus explicit `global_work: ZeGraphRange`. None differs from generation zero.
- `SuccessfulOutcome::{Read, Committed(NonZeroU64), Replayed, NoOp}`. This enum
  carries no authority to assert a durable commit; the coordinator supplies it.
- `empty_response() -> ZeGraphResponse`, the canonical unowned root.
- `OwnerError::{Limit, Allocation, Memory(MemoryError), Runtime(RuntimeError),
  Poisoned, Busy, RegistryFull, TokenExhausted, InvalidOwner, InvalidShape}`.

`graph_result/registration.rs` exports:

```rust
GraphResultRegistry::new(max_outstanding: usize) -> Self // const; use static
GraphResultRegistry::prepare<'m, 'g>(
    &'static self,
    context: &mut RuntimeContext<'_, 'm, 'g>,
    parts: ResponseParts<'_>,
    metadata: ResponseMetadata,
) -> Result<PreparedResponse<'m, 'g>, OwnerError>
GraphResultRegistry::free(&self, root: &mut ZeGraphResponse)
    -> Result<FreeReport, OwnerError>
PreparedResponse::descriptor(&self) -> ZeGraphResponse
PreparedResponse::arena_bytes(&self) -> usize
PreparedResponse::represented_bytes(&self) -> usize
PreparedResponse::allocation_bytes(&self) -> usize
PreparedResponse::reserved_bytes(&self) -> usize
PreparedResponse::expose(self, outcome: SuccessfulOutcome) -> ZeGraphResponse
```

`FreeReport` reports actual `examined_entries` and actual `released_bytes`.
The outcome module exports `OutcomeCell::{read,write,get,begin_attempt,
record_success,record_not_committed}` and `OperationOutcome::{NotCommitted,
Indeterminate,Success(SuccessfulOutcome)}`. Invalid transitions return
`OutcomeTransitionError`, including every attempt to overwrite known success.

## Allocation, control and the mandatory real-producer boundary

One fallibly allocated arena uses `Layout::extend` for every typed array and
`pad_to_align`; it never relies on `Vec<u8>` alignment. A separately fallibly
allocated stable node owns the arena and authoritative root. Full padded arena,
actual `size_of::<Node>()`, prepared-handle controls, layout, borrowed-parts and
metadata descriptors are reserved through the actual context's existing
`QueryExternalReservation` before allocation. The guard charges itself too.
No heap hash table, registry vector, logical slot estimate or hidden slab exists.
Zero-size arenas allocate nothing; empty responses still have a real node/token.

Source owners remain independently live and charged throughout copying.
**ZE-68 must prove whole native producer backing, all simultaneous conversions,
and actual core/ABI overlap.** A borrowed slice is not that proof. ZE-128 tests
use real owned typed fixtures, including a separately charged QueryArena, without
claiming a completed native producer or real graph admission exists here.

The actual RuntimeContext checks retained-view activity before caller control.
Copies reserve real CopiedBytes units immediately before consuming <=64 KiB
chunks; checks before/after preparation cleanly abort on cancellation. The
component enforces padded ABI capacity <=4 MiB and row/column geometry. It does
not revalidate native semantic values, ranges, identities, or execution reports:
ZE-68's real converter must supply valid pools. Its source descriptors cannot
escape preparation. No allocator, formatting or fallible callback runs in expose.

**One completed-counter authority:** this owner charges copying and enforces
geometry. Existing driver/coordinator charges CompletedAbiBytes exactly once
from `represented_bytes()`; it must not double-charge each conversion chunk.
Actual padded capacity remains separately charged through `arena_bytes()`.

## Synchronization and linearization

Observed RED on this macOS: the first standard Mutex lock for a registry made
an infallible 64-byte allocation; real allocator denial terminated with SIGABRT
before arena allocation (logs 10/11). The owner now uses an **embedded atomic
gate**, with no platform mutex backing or hidden heap allocation.

The gate is AtomicBool held + AtomicBool poisoned + UnsafeCell<List>.
UnsafeCell's Sync proof is exclusive CAS Acquire / Release RAII ownership; all
list accesses require its guard. Normal prepare/free try once and return typed
Busy on contention. No normal call spins or waits. A guard records whether it
entered during unwinding and marks poison only for a new unwind within the
critical section. Normal operations never clear or recover poison as success.

Abort cleanup alone retries acquisition with thread::yield_now, ignores only
the poison check, unlinks its unique private node, releases the gate, destroys
backing/node, then releases its real reservation. Poison remains set. No callback,
allocator or deallocator runs in a gate section, so no recursive acquisition
path exists. Sections examine at most max_outstanding nodes. Scheduling can
starve cleanup: this is not a wait-free or fairness guarantee. Cleanup cannot
return early or release charges while its backing remains live.

**ZE-68/69 must explicitly map Busy/Poisoned and retain the original descriptor
and ownership for retry.** This component invents no public ABI free status,
automatic retry policy or default global outstanding-result limit. The shipping
registry bound and wrapper policy remain their integration obligation.

Nodes use one process-wide, monotone, nonreused token source: independent
registries cannot confuse even all-empty roots. Exhaustion rejects preparation
before allocation or exposure. Every private node rejects every free, even a
guessed/copied exact descriptor. A unique, nonclone prepared handle owns its
abort/publication right; safe Rust cannot expose and abort it concurrently.

Expose disarms the prepared destructor with ManuallyDrop, moves out its guard,
applies only fixed successful-outcome metadata, copies the root locally, then
Release-publishes the node. **The Release store is the linearization point.**
No node dereference follows it: a matching concurrent free may immediately
unlink/deallocate the node. The moved guard releases temporary query/shared
accounting once. Actual published bytes stay registry/application-owned.

The existing core accounting guard Drop acquires its infallible cleanup mutex
(`lifecycle/stats.rs`, Reservation::drop). Expose therefore takes no registry or
fallible lock; it is **not lock-free**. This existing cleanup lock does not
allocate and does not report a fallible result. No new accounting bypass exists.

Free Acquire-observes publication and compares all 42 root/pool scalar,
pointer/count and range fields against the registered immutable root. It checks
that the caller's root storage does not overlap either authoritative arena or
node extent, preventing clear-after-free UAF. Caller pointer values are never
dereferenced to discover ownership. Successful free unlinks once under the gate,
then releases the arena/node outside it using their original Layouts and clears
the caller root. Repeated canonical-empty free succeeds. Stale, private, foreign
and altered descriptors cannot consume another owner. Nested lists are not
traversed during destruction.

Lookup is O(outstanding results), capped by explicit registry admission count;
FreeReport exposes actual examined entries. Destruction itself is two bounded
deallocations (one for an empty arena). These are distinct guarantees.

## Outcomes and integration acceptance retained

Keep OutcomeCell outside catch_unwind. Reads remain NotApplicable; a potential
write becomes Indeterminate before the possibly durable attempt. Only the real
coordinator establishes Committed(generation), Replayed, NoOp or a definite
NotCommitted result. Known success is terminal and survives delivery errors,
cancellation and unwind. No cell contains fresh IDs. Indeterminate cannot be
passed to PreparedResponse::expose; discard private success payloads and deliver
separate error metadata. The component neither authenticates nor recovers commits.

ZE-53/127 supply real completed native owners. ZE-68 must implement actual typed
conversion, the actual staging receipt hook/aligned allocation adaptation,
source-capacity proof, whole overlap/accounting transfer, real coordinator
outcome faults, true commit-window allocator denial and genuine graph close/
reopen independence. ZE-69/107 retain public exports, request marshalling,
lifecycle/error/busy policy, feature/header/artifact gates and packaging.
Standalone C-shaped fixture tests do not discharge those tickets.
