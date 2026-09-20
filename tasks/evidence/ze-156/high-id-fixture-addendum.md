# Genuine high-ID native fixture: ZE-156 / ZE-158 addendum

Read-only source correction, 2026-09-20. Inspected main `adc5968939e98a5093b5d21e90efc9bcbf7bb751`, the native creation producer, ZE-102 and canonical identity.md:30, and frozen ZE-155/158 plan requirements. No product edits, builds/tests, tracker mutation or moving-worker reads. Root reviews/records this exception and may assign its single implementation owner to ZE-156, then freeze the compiled helper for ZE-158.

## Finding and bounded decision

The assumed allocator-seed helper is absent. `lifecycle/native_graph/persistence.rs::create_native_graph_with_infrastructure` supplies VFS, clock and store/artifact entropy only. `empty_catalog` at line 491 writes logical node/relationship high-waters zero, and the initial CommitState at line 617 independently writes zeros. Entropy does not control logical IDs. `staging/structured.rs::allocate` performs checked high-water + 1; ordinary writes cannot cheaply reach high IDs today.

ZE-102 authorized a nonshipping, monotone allocator fixture seam with genuine writes and ordinary reopen/read consumers; it did not authorize caller-supplied public IDs or a fake admitted source. The canonical description envisages advancing an existing checkpoint, including same-low/different-high collisions. Implementing arbitrary reseeding of a live store would require extra coordinator/checkpoint mutation handling and is disproportionate for these two components. Use a **fresh-store-only seeded initial durable checkpoint** for their actual high-value witness. This is a narrow implementation interpretation for root approval, not a claim that the full existing-store reseeding/collision qualification is complete.

## Exact helper and implementation seam

Add only this crate-private, nonshipping constructor in `lifecycle/native_graph/persistence.rs`:

```rust
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn create_native_graph_with_allocator_seed_for_test(
    path: impl AsRef<Path>,
    options: OpenOptions,
    document: Option<EmbeddingTower>,
    first_node: crate::property_graph::NodeId,
    first_relationship: crate::property_graph::RelId,
) -> Result<Self, NativeGraphError>
```

The arguments mean the first IDs that the **ordinary allocator** will allocate, not already installed entities or public caller-selected write IDs. Their domain types exclude zero. Derive inclusive initial high-waters using checked `.get().checked_sub(1)` and the existing Invalid error on impossible invalid input. Use the ordinary StdVfs/SystemMonotonicClock/OsEntropy creation infrastructure. The high-ID witness does not need a new combined seed/fault/VFS option builder.

Factor the current `create` body into one module-private initializer accepting the two inclusive logical high-waters (a small private pair/struct is sufficient). The existing `create` signature and both current callers stay unchanged and delegate with zeros. Only the cfg-gated helper supplies nonzero values. There is still one initial-store creation implementation and no mutable global, environment override, alternate installer, post-create root patch or new capability exposed outside this crate.

Feed the same values to all existing real authorities:

1. `empty_catalog` passes them to the existing `catalog_payload`'s node_high_water/relationship_high_water arguments; symbol high-waters and empty dictionary stay zero.
2. The initial `CommitState.high_waters.node/relationship` uses that same pair; NativeCheckpoint encoding, checksum/framing, root selector and all current Full file/directory syncs are unchanged.
3. `NativeGraphBundleInput.high_waters` continues copying `state.high_waters`, as it already does. No separately edited in-memory counter.

Generation/sequence remain zero, graph roots remain empty, WAL first_sequence remains one, creation serials remain one/two and writer initialization remains serial two. Store identity still comes from the existing entropy provider. Preserve exclusive create, existing-path refusal, lock ownership and interrupted-create handling. There are no existing records/counters to move backward: this constructor only creates a fresh directory and reserves an unused logical prefix before exposing its first handle. Shipping create still starts at ID one.

No codec, catalog decoder, WAL/recovery validator, normal staging allocator, record contents, graph reader or completed/index representation changes are authorized. In particular recovery already compares catalog high-waters to checkpoint state at `recovery.rs:528`; both must agree. A failure there means fix the seed producer, never weaken recovery.

## Exact ownership exception

- Production/test helper: only `crates/zeppelin-embed/src/lifecycle/native_graph/persistence.rs`, with the refactor and cfg helper above. Root coordinates this narrow fresh-create hunk with ZE-46; no whole-file replacement and no write/recovery/maintenance edits.
- ZE-156: use its already owned completed/native test/helper subtree, inside `native_result_lists_bits_and_entity_identity`, for the real high-ID fixture and seed/reopen assertions. No new test framework, public export, Cargo dependency, ABI symbol or extra acceptance group.
- ZE-158: after root freezes the implementation commit, consume the same helper in its already owned `native_vector_index_identity_space_and_geometry` group. Do not copy an unfinished constructor or implement a second seed hook.
- Root records the explicit source-gap correction and pending same-low collision qualification against original ZE-53/62. Accepted plan artifacts remain frozen; link this addendum rather than rewriting their historical claims.

## One compact actual full-ID witness

Use `H = 1_u128 << 80`, first_node `NodeId::new(H - 1)`, and first_relationship `RelId::new((1_u128 << 96) + 7)`. The initial node high-water is H-2. Two ordinary node creates yield H-1 and H: adjacent IDs whose high 64-bit halves differ, without a large population. One ordinary relationship create links those two real node receipts through existing batch-local endpoint references and yields exactly the requested first relationship ID. Use nontrivial keys/properties already needed by the component; ZE-158 attaches its genuine supplied vectors to those ordinary node writes.

ZE-156 observes the actual NativePattern rows and completed entity pools/cells: exact full u128 node and relationship IDs, preserved directed endpoint IDs, full-ID sort/dedup and repeated reference multiplicity. Compare to literal arithmetic expectations as well as the actual apply receipts. Do not build QueryValue entity rows by hand or fabricate an installed RecordView. Existing close/drop lifetime assertions remain. ZE-158 independently observes its genuine sparse source/index row identity table and real kernel results for these full IDs, preserving row/revision mapping and existing reopen evidence.

This crosses a high-half boundary and detects ID truncation, but **does not prove two same-kind IDs with identical low64**. Do not label it that way. Cross-domain node/relationship equality is not a replacement for that stronger collision case.

## Necessary constructor correctness and durability evidence

Within the same small fixture, close immediately after seeded creation and reopen through ordinary `Store::open_native_graph` **before creating any entity**. Assert initial admitted high-waters H-2 and `(1<<96)+6`, zero generation/sequence, and empty graph; successful normal recovery authenticates matching catalog/checkpoint counters. This catches a seed stored only in RAM or only one durable authority, which an intervening normal write could otherwise conceal by rewriting the catalog.

Then perform the ordinary three-entity mixed write and verify the exact IDs/endpoints above through the component's real reader. Use its existing close/reopen phase (or one small additional reopen in that same group) to assert persisted high-waters H and `(1<<96)+7`; an ordinary subsequent node create must return H+1, with no ID reuse. No reseeding on open. Retain the existing ordinary-zero creation regression demonstrating first ID one; do not add a separate default-creation test matrix.

Observe an honest intended RED by routing the new helper through the existing zero-initialized creation before the seed wiring, so the required durable seeded-high-water/first-ID assertion fails, then wire the one initializer and obtain GREEN. A missing method compile error is not the behavioral RED. Run only the existing named component group with `cargo nextest ... -j 4 --retries 0`, plus its already required narrow feature/consumer compilation proving cfg absence in ordinary builds. No new broad runner/test suite or budget increase is needed.

The durable producer correction is small enough to implement now. Defer the larger live-store monotone reseeding mechanism, same-low64 collision matrix, portable digest-pinned C/Swift fixtures and relocation/parity qualification to their original ZE-53/62/identity/binding acceptance owners (root may record one flat E12 follow-up). That deferral does not waive actual high-u128 evidence in either component, nor the eventual stronger collision requirements.
