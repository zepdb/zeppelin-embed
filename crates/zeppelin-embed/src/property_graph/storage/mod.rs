//! Checked immutable graph containers. These codecs do not admit a GraphStore,
//! publish roots, replay a WAL, or authorize cleanup.

pub mod allocation;
pub mod artifact;
pub mod tree;

/// Bounded immutable adjacency codecs and range merging.
pub mod adjacency;
pub(crate) mod consolidation;

/// Bounded, role-checked logical streams over immutable physical chunks.
pub mod payload;

/// Retained logical windows and bounded field decoding over payload extents.
pub mod stream;

/// Lossless logical records and their required semantic validation hooks.
pub mod records;

mod view;
#[cfg(feature = "graph-cypher")]
#[allow(
    unused_imports,
    reason = "crate-private ZE-45 interface is consumed by ZE-50 after this dependency lands"
)]
pub(crate) use view::{
    CursorState, DirectionSelection, ExpandCursor, GraphPreparation, GraphReadView, LabelSelection,
    NativeArtifactWindow, NativeCatalog, NativePreparationCatalog, NativePreparationSource,
    NativeQuerySource, NativeReadCapability, NativeReadonlyMapping, NodeCursor, NodeView,
    PreparedGraphArtifacts, PreparedGraphFailure, RelView, RelationshipTypeSelection,
    TextPayloadReader,
};

/// Combined private storage capacity inside the authentic writer/store owner.
pub mod memory;

/// Packed private artifacts and explicit owned abort inventories.
pub mod prepared;

/// Persisted immutable allocation descriptors and reclamation state.
pub mod inventory;

/// Bounded protected-root, completed-mark, and reclaim-state proofs.
pub(crate) mod reclaim;

/// Native directory changes from one authentic normalized writer batch.
pub mod participant;

/// Sparse text/vector retrieval participants over native graph records.
#[cfg(feature = "graph-cypher")]
pub(crate) mod search;

/// Current durable recovery inventory bound. Native sources must be able to
/// address that inventory; 64 mapping slots imposed a smaller accidental store
/// limit. Descriptors are charged to the existing query/storage owner; payloads
/// remain immutable read-only mappings rather than copied graph rows.
pub(crate) const MAX_NATIVE_ARTIFACTS: usize = 8_192;

/// Locate a retained immutable mapping or an empty slot without allocating.
pub(crate) fn mapping_slot<T>(
    slots: &[std::cell::OnceCell<T>],
    artifact: artifact::ArtifactId,
    identity: impl Fn(&T) -> artifact::ArtifactId,
    mut step: impl FnMut() -> Result<(), tree::directory::TreeError>,
) -> Result<Option<&std::cell::OnceCell<T>>, tree::directory::TreeError> {
    let start = (xxhash_rust::xxh3::xxh3_64(&artifact.get().to_le_bytes()) as usize)
        .checked_rem(slots.len())
        .ok_or(tree::directory::TreeError::Memory)?;
    for index in (start..slots.len()).chain(0..start) {
        step()?;
        let slot = slots.get(index).ok_or(tree::directory::TreeError::Memory)?;
        if let Some(value) = slot.get() {
            if identity(value) == artifact {
                return Ok(Some(slot));
            }
        } else {
            return Ok(Some(slot));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod mapping_index_tests {
    #[test]
    #[allow(clippy::unwrap_used, reason = "test fixture failures are assertions")]
    fn ze257_mapping_lookup_work_does_not_scan_the_retained_inventory() {
        use super::{artifact::ArtifactId, mapping_slot};
        use std::cell::OnceCell;
        let slots: Vec<_> = (0..8192).map(|_| OnceCell::new()).collect();
        let mut probes = 0_u64;
        for index in 1..=4096 {
            let id = ArtifactId::new(index).unwrap();
            mapping_slot(
                &slots,
                id,
                |value| *value,
                || {
                    probes += 1;
                    Ok(())
                },
            )
            .unwrap()
            .unwrap()
            .set(id)
            .unwrap();
        }
        for index in (1..=4096).rev() {
            let id = ArtifactId::new(index).unwrap();
            let found = mapping_slot(
                &slots,
                id,
                |value| *value,
                || {
                    probes += 1;
                    Ok(())
                },
            )
            .unwrap()
            .unwrap()
            .get()
            .copied();
            assert_eq!(found, Some(id));
        }
        eprintln!("ZE257 mapping probes={probes} for 4096 inserts and reverse lookups");
        assert!(probes < 4096 * 8, "mapping table probes: {probes}");
    }
}
