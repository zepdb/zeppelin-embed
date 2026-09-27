//! Test-only bulk construction of fresh scalar-node fixtures. Normal staging,
//! record encoders, artifact ownership and durable publication remain in use.
//! Final pages are emitted once instead of retaining every insertion's old page.
#![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::type_complexity)]
use super::*;
use crate::property_graph::storage::tree::{Cell, Key, PAGE_BYTES, PageHeader, encode_page};

type Entry = ((u128, u128, Vec<u8>), Vec<u8>, Vec<u8>);

pub(super) fn create<S: BlockSink, C: RecordCatalog<S>>(
    state: &mut PrepareState<'_, '_, C>,
    sink: &mut S,
    batch: &StagedBatch<'_>,
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    if state.roots.references().iter().any(Option::is_some) {
        return Err(TreeError::Invalid(
            "capacity fixture requires empty directories",
        ));
    }
    let (store, generation) = (state.roots.store(), state.roots.generation());
    let (mut nodes, mut labels, mut fences) = (Vec::new(), Vec::new(), Vec::new());
    for delta in batch.deltas() {
        let fields = delta.provenance().fields();
        let EntityId::Node(id) = fields.incarnation else {
            return Err(TreeError::Invalid("capacity fixture requires nodes"));
        };
        if fields.expected != ExpectedGraphState::Absent {
            return Err(TreeError::Invalid("capacity fixture requires creates"));
        }
        let provenance =
            prepare_provenance(sink, store, generation, delta.provenance(), state.memory, r)?;
        let canonical = prepare_payload(
            sink,
            store,
            generation,
            BlockKind::CanonicalImage,
            delta
                .canonical()
                .ok_or(TreeError::Invalid("capacity fixture canonical image"))?,
            r,
        )?;
        let record = prepare_record(
            sink,
            RecordInput {
                store,
                generation,
                entity: fields.incarnation,
                canonical,
                provenance,
            },
            state.catalog,
            state.document,
            state.memory,
            r,
        )?;
        let snapshot = state.snapshot(
            sink,
            fields.incarnation,
            record,
            generation,
            state.memory,
            r,
        )?;
        let mut value = vec![0; 48];
        record.encode_into(&mut value)?;
        nodes.push((
            (id.get(), 0, vec![]),
            id.get().to_le_bytes().to_vec(),
            value,
        ));
        for label in snapshot.labels.as_slice() {
            let mut key = label.get().to_le_bytes().to_vec();
            key.extend_from_slice(&id.get().to_le_bytes());
            labels.push(((u128::from(label.get()), id.get(), vec![]), key, vec![]));
        }
        let key = fields
            .key
            .ok_or(TreeError::Invalid("capacity fixture requires keys"))?;
        let stored = verify_provenance(PayloadSlice::new(sink, store, generation, provenance), r)?;
        let stored_key = stored
            .key()
            .ok_or(TreeError::Invalid("capacity fixture stored key"))?;
        let Symbol::Namespace(namespace) =
            state
                .catalog
                .resolve(SymbolKind::Namespace, stored_key.namespace(), r)?
        else {
            return Err(TreeError::Invalid("capacity fixture namespace"));
        };
        let probe = FenceKey::new(key.kind(), namespace, key.key().as_str())?;
        let value = prepare_fence(
            sink,
            FenceInput {
                store,
                generation,
                key: probe,
                provenance,
                canonical: Some(canonical),
            },
            state.catalog,
            state.document,
            r,
        )?;
        let mut encoded = vec![1];
        encoded.extend_from_slice(&namespace.get().to_le_bytes());
        encoded.extend_from_slice(key.key().as_str().as_bytes());
        fences.push((
            (
                u128::from(namespace.get()),
                0,
                key.key().as_str().as_bytes().to_vec(),
            ),
            encoded,
            value.to_vec(),
        ));
    }
    for (kind, entries) in [
        (TreeKind::Nodes, nodes),
        (TreeKind::Labels, labels),
        (TreeKind::KeyFences, fences),
    ] {
        state
            .roots
            .replace(build(sink, kind, entries, store, generation, r)?)?;
    }
    Ok(())
}

fn build(
    sink: &mut impl BlockSink,
    kind: TreeKind,
    mut entries: Vec<Entry>,
    store: crate::property_graph::StoreInstanceId,
    generation: GraphGeneration,
    r: &mut TreeResources<'_>,
) -> Result<DirectoryRoot, TreeError> {
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(TreeError::Invalid("duplicate capacity fixture key"));
    }
    let mut page = [0; PAGE_BYTES];
    let mut children = Vec::new();
    for chunk in entries.chunks(64) {
        let cells: Vec<_> = chunk
            .iter()
            .map(|(_, key, value)| Cell::Leaf {
                key: Key::Inline(key),
                value,
            })
            .collect();
        encode_page(
            PageHeader {
                kind,
                level: 0,
                generation,
            },
            &cells,
            &mut page,
        )?;
        children.push((
            chunk[0].1.clone(),
            sink.append(BlockKind::TreePage, generation, &page, r)?,
        ));
    }
    let mut level = 1;
    while children.len() > 1 {
        let mut parents = Vec::new();
        for chunk in children.chunks(64) {
            let cells: Vec<_> = chunk
                .iter()
                .enumerate()
                .map(|(i, (_, child))| Cell::Branch {
                    upper: chunk.get(i + 1).map(|(key, _)| Key::Inline(key)),
                    child: *child,
                })
                .collect();
            encode_page(
                PageHeader {
                    kind,
                    level,
                    generation,
                },
                &cells,
                &mut page,
            )?;
            parents.push((
                chunk[0].0.clone(),
                sink.append(BlockKind::TreePage, generation, &page, r)?,
            ));
        }
        children = parents;
        level += 1;
    }
    DirectoryRoot::from_reference(
        store,
        kind,
        generation,
        children.first().map(|(_, reference)| *reference),
    )
}
