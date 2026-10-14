//! Allocation-free admission for preparation attributable to a structured batch.
//! Existing records/catalogs and copied old tree paths still use StorageMemory;
//! admission does not promise that arbitrary existing-store work will fit.
use crate::property_graph::canonical::{Encoder, encode_staging_value};
use crate::property_graph::staging::{
    StageError, StructuredOperation, StructuredWrite, WriteImage,
};
use crate::property_graph::storage::artifact::MAX_ARTIFACT_BYTES;
use crate::property_graph::storage::payload::CHUNK_BYTES;
use crate::property_graph::storage::tree::directory::{DirectoryBatch, DirectoryOp};
use crate::property_graph::storage::tree::{Cell, PAGE_BYTES};
use crate::property_graph::{EntityId, ExpectedGraphState, NodeRef};
use std::mem::size_of;

/// Upper-bound the submitted images' record/index, directory edit and private
/// pack backing. Saturating arithmetic makes overflow an admission refusal.
/// No ID, symbol, reservation, file or staged image is created here.
#[cfg(test)]
pub(super) fn estimate_preparation_bytes(
    writes: &[StructuredWrite<'_, '_>],
) -> Result<usize, StageError> {
    Ok(estimate_batch_bytes(writes)?.0)
}

/// Storage preparation overlaps the retained normalized batch and receipts.
/// The storage-only 32 MiB admission does not include these writer owners.
pub(super) fn estimate_write_bytes(
    writes: &[StructuredWrite<'_, '_>],
) -> Result<(usize, usize), StageError> {
    let (storage, input, symbols) = estimate_batch_bytes(writes)?;
    let rows = writes.len().saturating_mul(
        size_of::<crate::property_graph::staging::NormalizedDelta<'_>>()
            + 2 * size_of::<crate::property_graph::staging::ItemReceipt>(),
    );
    let temporary = storage
        .saturating_add(input)
        .saturating_add(rows)
        .saturating_add(
            symbols.saturating_mul(size_of::<crate::property_graph::catalog::SymbolEntry<'_>>()),
        );
    Ok((storage, temporary))
}

fn estimate_batch_bytes(
    writes: &[StructuredWrite<'_, '_>],
) -> Result<(usize, usize, usize), StageError> {
    let edit = DirectoryBatch::admission_descriptor_bytes();
    let mut packs = Packs::default();
    let (mut nodes, mut rels, mut labels, mut key_bytes, mut cache_rows) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    let mut groups = [0usize; 2];
    let mut previous = [None; 2];
    let mut input = 0usize;
    let mut record_scratch = 0usize;
    let mut caller_nodes = 0usize;
    let mut symbols = 0usize;
    for write in writes {
        caller_nodes = caller_nodes.saturating_add(usize::from(matches!(
            write.operation,
            StructuredOperation::CreateWithId(_)
        )));
        let expected = match write.operation {
            StructuredOperation::Create | StructuredOperation::CreateWithId(_) => {
                ExpectedGraphState::Absent
            }
            StructuredOperation::Put(id) | StructuredOperation::Delete(id, _) => {
                ExpectedGraphState::Entity(id)
            }
            StructuredOperation::Recreate(revision) => ExpectedGraphState::Deletion(revision),
        };
        let framing = crate::property_graph::provenance::measure_operation_framing(
            Some(write.key),
            expected,
            &mut || Ok(()),
        )? as usize;
        let (image_bytes, label_count, properties, relationship) = match write.image {
            Some(WriteImage::Node(image)) => {
                let (names, properties, _, _) =
                    image.staging_node_parts().ok_or(StageError::InvalidInput)?;
                (
                    image.encoded_len() as usize,
                    names.len(),
                    properties.len(),
                    false,
                )
            }
            Some(WriteImage::Relationship {
                source,
                target,
                relationship_type,
                properties,
            }) => {
                // Runs in submission order upper-bound distinct endpoint/type
                // groups after sorting, without allocating a deduplication set.
                for ((groups, previous), endpoint) in
                    groups.iter_mut().zip(&mut previous).zip([source, target])
                {
                    let group = (endpoint, relationship_type);
                    if *previous != Some(group) {
                        *groups = groups.saturating_add(1);
                        *previous = Some(group);
                    }
                }
                cache_rows = cache_rows.saturating_add(
                    usize::from(matches!(source, NodeRef::Existing(_)))
                        + usize::from(matches!(target, NodeRef::Existing(_))),
                );
                // Same canonical tags and value encoder; endpoint IDs are fixed
                // width even when their batch-local identities are unresolved.
                let mut sink = std::io::sink();
                let mut poll = || Ok(());
                let mut e = Encoder::new(&mut sink, &mut poll);
                e.emit(b"ZGCI")?;
                e.emit(&1u16.to_le_bytes())?;
                e.byte(2)?;
                e.emit(&[0; 32])?;
                e.blob(relationship_type.as_str().as_bytes())?;
                e.count(properties.len())?;
                for property in properties {
                    e.blob(property.name().as_str().as_bytes())?;
                    encode_staging_value(&mut e, property.value().data())?;
                }
                e.optional_blob(None)?;
                e.byte(0)?;
                (e.stats().bytes as usize, 0, properties.len(), true)
            }
            None => (
                0,
                0,
                0,
                matches!(
                    write.key.kind(),
                    crate::property_graph::EntityKind::Relationship
                ),
            ),
        };
        cache_rows = cache_rows.saturating_add(usize::from(!matches!(
            write.operation,
            StructuredOperation::Create
        )));
        symbols = symbols
            .saturating_add(label_count)
            .saturating_add(properties)
            .saturating_add(2);
        input = input.saturating_add(framing).saturating_add(image_bytes);
        if input > crate::property_graph::MAX_GRAPH_INPUT_BYTES {
            return Err(StageError::Limit);
        }
        key_bytes = key_bytes
            .saturating_add(9)
            .saturating_add(write.key.key().as_str().len());
        if relationship {
            rels = rels.saturating_add(1);
            if !matches!(
                write.operation,
                StructuredOperation::Create | StructuredOperation::CreateWithId(_)
            ) {
                // Existing ranges/removals depend on the store, not just the
                // submitted endpoints. Allow two directory edits per direction.
                groups = [rels.saturating_mul(2); 2];
                previous = [None; 2];
            }
        } else {
            nodes = nodes.saturating_add(1);
        }
        labels = labels.saturating_add(label_count);
        // records/prepare.rs: node header 40 (relationship 80), index
        // header 8, references up to 120, labels 8, IndexRow 24.
        let record = (if relationship { 80usize } else { 40 }) + 8 + 120;
        let index = label_count
            .saturating_mul(8)
            .saturating_add(properties.saturating_mul(24));
        record_scratch = record_scratch.max(index.saturating_mul(2));
        packs.stream(framing);
        if image_bytes > 0 {
            packs.stream(image_bytes);
        }
        packs.stream(record.saturating_add(index));
    }
    // Mapping slots (8192 * 160), TreeResources' 256 KiB workspace,
    // page/record scratch and bounded candidate/inventory control objects.
    let fixed = 2_usize * 1024 * 1024;
    let cache =
        cache_rows.saturating_mul(crate::lifecycle::native_graph::admission_cache_row_bytes());
    // Linked documents retain one version per caller ID (base.rs), in
    // addition to their cache rows and temporary wanted-entity descriptors.
    let versions = caller_nodes.saturating_mul(size_of::<crate::ingest::DocumentVersion>());
    // Wanted entities are reserved at three rows/request, then dropped before
    // directories and packs exist (base.rs). Do not sum disjoint phases.
    let preload = fixed
        .saturating_add(cache)
        .saturating_add(versions)
        .saturating_add(
            writes
                .len()
                .saturating_mul(3)
                .saturating_add(caller_nodes)
                .saturating_mul(size_of::<EntityId>()),
        );
    let capacity = |count: usize| {
        if count == 0 {
            0
        } else {
            count.checked_next_power_of_two().unwrap_or(usize::MAX)
        }
    };
    let label_capacity = capacity(labels);
    let type_capacity = capacity(rels);
    // Node/relationship: 16-byte ID + 48-byte PayloadRef. Fence: encoded
    // key + 144-byte value. Memberships: 24-byte key, empty value.
    let entity_charge = writes.len().saturating_mul(edit + 64);
    let key_charge = writes
        .len()
        .saturating_mul(edit + 144)
        .saturating_add(key_bytes);
    let label_charge = label_capacity
        .saturating_mul(edit)
        .saturating_add(labels.saturating_mul(24));
    let type_charge = type_capacity
        .saturating_mul(edit)
        .saturating_add(rels.saturating_mul(24));
    let member_charge = label_charge.saturating_add(type_charge);
    let mut pending = entity_charge
        .saturating_add(key_charge)
        .saturating_add(member_charge);
    let common = fixed
        .saturating_add(cache)
        .saturating_add(versions)
        .saturating_add(record_scratch);
    // Descriptor doubling admits old + replacement; old backing disappears
    // before sorting/flush scratch is allocated.
    let growth = (label_capacity.max(type_capacity) / 2).saturating_mul(edit);
    let mut peak = preload.max(
        common
            .saturating_add(packs.peak)
            .saturating_add(pending)
            .saturating_add(growth),
    );
    for (count, bytes, max_cell, charge) in [
        (
            nodes,
            nodes.saturating_mul(92),
            92,
            nodes.saturating_mul(edit + 64),
        ),
        (
            rels,
            rels.saturating_mul(92),
            92,
            rels.saturating_mul(edit + 64),
        ),
        (labels, labels.saturating_mul(52), 52, label_charge),
        (rels, rels.saturating_mul(52), 52, type_charge),
        (
            writes.len(),
            writes.len().saturating_mul(172).saturating_add(key_bytes),
            684,
            key_charge,
        ),
    ] {
        let pages = directory_pages(bytes, max_cell);
        for _ in 0..pages {
            packs.block(PAGE_BYTES);
        }
        // directory.rs: old_count + 2*ops Cell descriptors, cuts per
        // cell, DirectoryOp per edit, plus emitted page separators.
        let bulk = count
            .saturating_mul(
                2 * size_of::<Cell<'_>>() + 2 * size_of::<usize>() + size_of::<DirectoryOp<'_>>(),
            )
            .saturating_add(pages.saturating_mul(2 * (512 + 64)));
        peak = peak.max(
            common
                .saturating_add(packs.peak)
                .saturating_add(pending)
                .saturating_add(bulk),
        );
        pending = pending.saturating_sub(charge);
        packs.peak = packs.backing();
    }
    if rels > 0 {
        use crate::property_graph::storage::adjacency::{
            DeltaEntry, Edge, HEADER_BYTES, MAX_BASE_ENTRIES, MAX_MERGED_ENTRIES,
            MAX_PENDING_ENTRIES, MERGE_STATE_BYTES, RANGE_DESCRIPTOR_BYTES, RangeDescriptor,
        };
        // ranges::apply reserves 2*rels in BOTH edit buffers and four
        // RangeDescriptor rows per relationship. Changes remain live throughout.
        let mut retained = rels.saturating_mul(4 * (edit + size_of::<RangeDescriptor>()) + 2 * 96);
        // A group can split every MAX_BASE_ENTRIES edges. Counting submission
        // runs may overcount sorted groups, but cannot omit a submitted range.
        let ranges = groups.map(|groups| {
            groups
                .saturating_add(rels / MAX_BASE_ENTRIES)
                .min(rels.saturating_mul(2))
        });
        // At most an empty base plus a delta per new range, with up to 40
        // encoded bytes per directed edge. Extra edge streams conservatively
        // include their framing and private-pack replacement overlap.
        for _ in 0..ranges.into_iter().fold(0usize, usize::saturating_add) {
            packs.block(HEADER_BYTES);
            packs.block(HEADER_BYTES);
        }
        packs.stream(rels.saturating_mul(2 * 40));
        let mut range_edits = ranges
            .into_iter()
            .fold(0usize, usize::saturating_add)
            .saturating_mul(40 + RANGE_DESCRIPTOR_BYTES);
        // ranges::Workspace plus RangeScratch: merged/old/output edges,
        // pending deltas, encoded base, merge state and scoped input spans.
        let workspace = (2 * MAX_MERGED_ENTRIES + MAX_BASE_ENTRIES + 1) * size_of::<Edge>()
            + MAX_PENDING_ENTRIES * size_of::<DeltaEntry>()
            + HEADER_BYTES
            + MAX_BASE_ENTRIES * 32
            + MERGE_STATE_BYTES
            + 2 * CHUNK_BYTES
            + PAGE_BYTES;
        // Native record edits are gone before adjacency starts. Range
        // payloads and pages grow the retained packs; each directional flush
        // has its own bulk scratch, while both edit buffers still exist.
        peak = peak.max(
            common
                .saturating_add(packs.peak)
                .saturating_add(retained)
                .saturating_add(range_edits)
                .saturating_add(workspace),
        );
        packs.peak = packs.backing();
        for count in ranges {
            let pages = directory_pages(count.saturating_mul(396), 396);
            for _ in 0..pages {
                packs.block(PAGE_BYTES);
            }
            let bulk = count
                .saturating_mul(
                    2 * size_of::<Cell<'_>>()
                        + 2 * size_of::<usize>()
                        + size_of::<DirectoryOp<'_>>(),
                )
                .saturating_add(pages.saturating_mul(2 * (512 + 64)));
            peak = peak.max(
                common
                    .saturating_add(packs.peak)
                    .saturating_add(retained)
                    .saturating_add(range_edits)
                    .saturating_add(workspace)
                    .saturating_add(bulk),
            );
            retained = retained.saturating_sub(rels.saturating_mul(2 * edit));
            range_edits =
                range_edits.saturating_sub(count.saturating_mul(40 + RANGE_DESCRIPTOR_BYTES));
            packs.peak = packs.backing();
        }
    }
    // Copied keys/overflow streams, symbol names and catalog framing can
    // retain another input-sized owner. Text/vector retrieval also duplicates
    // submitted payload; the input allowance remains a separate upper bound.
    Ok((
        peak.max(common.saturating_add(packs.backing()).saturating_add(input)),
        input,
        symbols,
    ))
}

fn directory_pages(bytes: usize, max_cell: usize) -> usize {
    if bytes == 0 {
        return 0;
    }
    // emit_many greedily packs cells. Every nonfinal leaf uses more than
    // PAGE_BYTES - 64 - largest_cell bytes. Branch separators are <=684.
    let mut level = bytes.div_ceil(PAGE_BYTES - 64 - max_cell);
    let mut total = level;
    while level > 1 {
        level = level.div_ceil((PAGE_BYTES - 64) / 684);
        total = total.saturating_add(level);
    }
    total
}

/// Scalar bound on PrivateArtifact backing, including simultaneous old/new
/// buffers. Mirrors only its allocation geometry: 512 KiB byte steps,
/// doubled VerifiedEntry capacity, 24-byte frame + 24-byte directory entry.
#[derive(Default)]
struct Packs {
    sealed: usize,
    body: usize,
    entries: usize,
    byte_capacity: usize,
    entry_capacity: usize,
    peak: usize,
}
impl Packs {
    fn backing(&self) -> usize {
        self.sealed
            .saturating_add(self.byte_capacity)
            .saturating_add(self.entry_capacity.saturating_mul(48))
    }
    fn stream(&mut self, bytes: usize) {
        for chunk in 0..bytes.div_ceil(CHUNK_BYTES).max(1) {
            self.block(bytes.saturating_sub(chunk * CHUNK_BYTES).min(CHUNK_BYTES));
        }
        if bytes > CHUNK_BYTES {
            self.block(24 + bytes.div_ceil(CHUNK_BYTES) * 32);
        }
    }
    fn block(&mut self, payload: usize) {
        if self.body == 0 {
            self.body = 144;
            self.byte_capacity = 144;
        }
        let needed = self
            .body
            .saturating_add(payload)
            .saturating_add(24)
            .saturating_add((self.entries + 1) * 24 + 8);
        if needed > MAX_ARTIFACT_BYTES || self.entries == 32_768 {
            self.sealed = self.backing();
            self.body = 144;
            self.entries = 0;
            self.byte_capacity = 144;
            self.entry_capacity = 0;
        }
        let needed = self
            .body
            .saturating_add(payload)
            .saturating_add(24)
            .saturating_add((self.entries + 1) * 24 + 8);
        let next_bytes = needed
            .div_ceil(512 * 1024)
            .saturating_mul(512 * 1024)
            .min(MAX_ARTIFACT_BYTES);
        let next_entries = (self.entries + 1)
            .checked_next_power_of_two()
            .unwrap_or(usize::MAX)
            .clamp(1024, 32_768);
        let overlap = if next_bytes > self.byte_capacity {
            next_bytes
        } else {
            0
        };
        let overlap = overlap.saturating_add(if next_entries > self.entry_capacity {
            next_entries.saturating_mul(48)
        } else {
            0
        });
        self.peak = self.peak.max(self.backing().saturating_add(overlap));
        self.byte_capacity = self.byte_capacity.max(next_bytes);
        self.entry_capacity = self.entry_capacity.max(next_entries);
        self.body = self.body.saturating_add(payload).saturating_add(24);
        self.entries = self.entries.saturating_add(1);
    }
}
