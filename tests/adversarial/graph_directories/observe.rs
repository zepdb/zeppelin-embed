//! Decode observed production bytes in their emitted order. Never normalize the
//! observation to make it agree with the independent logical model.
use super::*;
use zeppelin_embed::property_graph::storage::payload::PayloadRef;
use zeppelin_embed::property_graph::storage::tree::Key;
fn bytes<S: BlockSource>(
    source: PayloadSlice<'_, S>,
    r: &mut TreeResources<'_>,
) -> Result<Vec<u8>, TreeError> {
    let mut out = vec![0; source.len() as usize];
    if source.read_at(0, &mut out, r)? != out.len() {
        return Err(TreeError::Invalid("short observed bytes"));
    }
    Ok(out)
}
fn entity(entity: EntityId) -> model::Entity {
    match entity {
        EntityId::Node(id) => model::Entity {
            kind: model::Kind::Node,
            id: id.get(),
        },
        EntityId::Relationship(id) => model::Entity {
            kind: model::Kind::Relationship,
            id: id.get(),
        },
    }
}
fn provenance<S: BlockSource>(
    stored: &StoredProvenance<'_, S>,
    r: &mut TreeResources<'_>,
) -> Result<model::Provenance, TreeError> {
    Ok(model::Provenance {
        version: 1, // The complete ZGOP decoder accepts exactly version1.
        operation: match stored.operation() {
            GraphOperation::StructuredCreate => model::OperationKind::Create,
            GraphOperation::StructuredPut => model::OperationKind::Put,
            GraphOperation::StructuredDelete => model::OperationKind::Delete,
            GraphOperation::StructuredRecreate => model::OperationKind::Recreate,
            GraphOperation::CypherEdit => model::OperationKind::Cypher,
        },
        key: stored
            .key()
            .map(|key| {
                Ok::<_, TreeError>(model::Key {
                    kind: match key.kind() {
                        EntityKind::Node => model::Kind::Node,
                        EntityKind::Relationship => model::Kind::Relationship,
                    },
                    namespace: bytes(key.namespace(), r)?,
                    key: bytes(key.key(), r)?,
                })
            })
            .transpose()?,
        requested_revision: stored.requested_revision().get(),
        installed_revision: stored.installed_revision().get(),
        expected: match stored.expected() {
            ExpectedGraphState::Absent => model::Expected::Absent,
            ExpectedGraphState::Entity(id) => model::Expected::Entity(entity(id)),
            ExpectedGraphState::Deletion(rev) => model::Expected::Deletion(rev.get()),
        },
        incarnation: entity(stored.incarnation()),
        delete_mode: stored.delete_mode().map(|mode| match mode {
            GraphDeleteMode::Restrict => model::DeleteMode::Restrict,
            GraphDeleteMode::Detach => model::DeleteMode::Detach,
        }),
        original_generation: stored.original_generation().get(),
    })
}
fn record<S: BlockSource>(
    view: RecordView<'_, S>,
    r: &mut TreeResources<'_>,
) -> Result<model::Record, TreeError> {
    let shape = match view.shape() {
        RecordShape::Node { labels, .. } => {
            let mut ids = Vec::new();
            for index in 0..labels {
                ids.push(view.label(index, r)?.get());
            }
            model::Shape::Node { labels: ids }
        }
        RecordShape::Relationship {
            source,
            target,
            relationship_type,
            ..
        } => model::Shape::Relationship {
            source: source.get(),
            target: target.get(),
            rel_type: relationship_type.get(),
        },
    };
    Ok(model::Record {
        image: model::Image {
            canonical: bytes(view.canonical_bytes(), r)?,
            shape,
        },
        provenance: provenance(view.provenance(), r)?,
    })
}
fn id(entry: DirectoryEntry<'_>) -> u128 {
    let Key::Inline(key) = entry.key() else {
        panic!("native numeric overflow");
    };
    u128::from_le_bytes(key.try_into().unwrap())
}
pub fn all<S: BlockSource>(
    source: &S,
    roots: GraphRoots,
    catalog: &fixture::Base,
    r: &mut TreeResources<'_>,
) -> Result<model::Observation, TreeError> {
    let mut out = model::Observation {
        generation: roots.generation().get(),
        nodes: Vec::new(),
        relationships: Vec::new(),
        fences: Vec::new(),
        labels: Vec::new(),
        types: Vec::new(),
        node_tombstones: Vec::new(),
    };
    for kind in [
        TreeKind::Nodes,
        TreeKind::Relationships,
        TreeKind::KeyFences,
        TreeKind::Labels,
        TreeKind::RelationshipTypes,
    ] {
        let root = roots.directory(kind)?;
        let mut cursor = DirectoryCursor::seek(source, root, None, r)?;
        while let Some(entry) = cursor.next_entry(r)? {
            match kind {
                TreeKind::Nodes | TreeKind::Relationships => {
                    let slice = PayloadSlice::new(
                        source,
                        roots.store(),
                        entry.creation_generation(),
                        PayloadRef::decode(entry.value())?,
                    );
                    if kind == TreeKind::Nodes {
                        match verify_node_state(
                            slice,
                            NodeId::new(id(entry)).unwrap(),
                            catalog,
                            None,
                            r,
                        )? {
                            NodeRecordState::Live(view) => out.nodes.push(record(view, r)?),
                            NodeRecordState::Tombstone(view) => {
                                out.node_tombstones.push(provenance(view.provenance(), r)?)
                            }
                        }
                    } else {
                        out.relationships.push(record(
                            verify_record(
                                slice,
                                EntityId::Relationship(RelId::new(id(entry)).unwrap()),
                                catalog,
                                None,
                                r,
                            )?,
                            r,
                        )?);
                    }
                }
                TreeKind::KeyFences => {
                    let view = verify_fence_entry(source, root, entry, catalog, None, r)?;
                    out.fences.push(model::Fence {
                        provenance: provenance(view.provenance(), r)?,
                        canonical: view
                            .canonical_bytes()
                            .map(|slice| bytes(slice, r))
                            .transpose()?,
                    });
                }
                TreeKind::Labels | TreeKind::RelationshipTypes => {
                    assert!(entry.value().is_empty());
                    let Key::Inline(key) = entry.key() else {
                        panic!("membership overflow");
                    };
                    let pair = (
                        u64::from_le_bytes(key[..8].try_into().unwrap()),
                        u128::from_le_bytes(key[8..].try_into().unwrap()),
                    );
                    if kind == TreeKind::Labels {
                        out.labels.push(pair);
                    } else {
                        out.types.push(pair);
                    }
                }
                _ => unreachable!(),
            }
        }
    }
    Ok(out)
}
pub fn ranges<S: BlockSource>(
    source: &S,
    roots: GraphRoots,
    catalog: &fixture::Base,
    expected: &model::Snapshot,
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    for start in [
        0,
        fixture::NODE_HIGH + 1,
        fixture::NODE_HIGH + 2,
        fixture::NODE_HIGH + 3,
    ] {
        let mut cursor = DirectoryCursor::seek(
            source,
            roots.directory(TreeKind::Nodes)?,
            Some(&start.max(1).to_le_bytes()),
            r,
        )?;
        let mut ids = Vec::new();
        while ids.len() < 2 {
            let Some(entry) = cursor.next_entry(r)? else {
                break;
            };
            let slice = PayloadSlice::new(
                source,
                roots.store(),
                entry.creation_generation(),
                PayloadRef::decode(entry.value())?,
            );
            if matches!(
                verify_node_state(slice, NodeId::new(id(entry)).unwrap(), catalog, None, r)?,
                NodeRecordState::Live(_)
            ) {
                ids.push(id(entry));
            }
        }
        let wanted: Vec<_> = expected
            .node_range(start, None, 2)
            .unwrap()
            .iter()
            .map(|record| record.provenance.incarnation.id)
            .collect();
        assert_eq!(ids, wanted, "PG8 actual bounded node seek");
    }
    for fence in expected.observation().fences {
        let key = fence.provenance.key.as_ref().unwrap();
        // One fixed namespace deliberately gives identical physical/logical order;
        // symbol-ID ordering itself is separately covered by the numeric tree tests.
        assert_eq!(key.namespace, fixture::NS.as_bytes());
        let probe = FenceKey::new(
            match key.kind {
                model::Kind::Node => EntityKind::Node,
                model::Kind::Relationship => EntityKind::Relationship,
            },
            NamespaceId::new(1).unwrap(),
            std::str::from_utf8(&key.key).unwrap(),
        )?;
        let root = roots.directory(TreeKind::KeyFences)?;
        let entry = lookup_fence_entry(source, root, probe, r)?.ok_or(TreeError::Missing)?;
        let view = verify_fence_entry(source, root, entry, catalog, None, r)?;
        assert_eq!(provenance(view.provenance(), r)?, fence.provenance);
    }
    Ok(())
}
