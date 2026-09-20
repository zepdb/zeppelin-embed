//! Private source-bound record leaves shared by native query components.
use super::{
    payload::PayloadRef,
    records::{NodeRecordState, RecordCatalog, verify_node_state},
    stream::PayloadSlice,
    tree::{
        TreeKind,
        directory::{BlockSource, GraphRoots, TreeError, TreeResources, lookup_entry},
    },
};
use crate::epoch::EmbeddingTower;
use crate::property_graph::NodeId;

/// Resolve and completely verify one node from caller-owned immutable source
/// bytes. The returned state and every payload view remain bound to `source`;
/// this leaf performs no admission, open, lease, or backing allocation.
pub(super) fn lookup_node_state<'a, S: BlockSource>(
    source: &'a S,
    roots: GraphRoots,
    node: NodeId,
    catalog: &impl RecordCatalog<S>,
    document: Option<&EmbeddingTower>,
    r: &mut TreeResources<'_>,
) -> Result<Option<NodeRecordState<'a, S>>, TreeError> {
    let root = roots.directory(TreeKind::Nodes)?;
    let Some(entry) = lookup_entry(source, root, &node.get().to_le_bytes(), r)? else {
        return Ok(None);
    };
    let payload = PayloadRef::decode(entry.value())?;
    verify_node_state(
        PayloadSlice::new(source, roots.store(), entry.creation_generation(), payload),
        node,
        catalog,
        document,
        r,
    )
    .map(Some)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

    use super::*;
    use crate::epoch::{ComputeUnits, EmbeddingRuntime, Normalization};
    use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, SnapshotLease, Store};
    use crate::property_graph::catalog::{Symbol, SymbolKind};
    use crate::property_graph::query::resources::QueryMemory;
    use crate::property_graph::query::runtime::{RetainedView, RuntimeContext, RuntimeLimits};
    use crate::property_graph::query::{QueryError, QueryView};
    use crate::property_graph::resources::GraphResources;
    use crate::property_graph::staging::{WriteLimits, WriteMemory};
    use crate::property_graph::storage::artifact::{
        self, ArtifactId, ArtifactIdentity, Block, BlockKind, ContainerKind, FramedBlock,
        PhysicalRef,
    };
    use crate::property_graph::storage::memory::StorageMemory;
    use crate::property_graph::storage::payload::prepare_payload;
    use crate::property_graph::storage::records::{
        RecordInput, RecordShape, prepare_node_tombstone, prepare_provenance, prepare_record,
    };
    use crate::property_graph::storage::tree::directory::{
        BlockSink, DirectoryRoot, TreeScratch, insert,
    };
    use crate::property_graph::{
        CanonicalContents, CanonicalEmbedding, EntityId, ExpectedGraphState, GraphDeleteMode,
        GraphGeneration, GraphOperation, GraphRevision, OperationFields, OperationProvenance,
        StoreInstanceId,
    };
    use std::collections::BTreeMap;

    struct Objects {
        store: StoreInstanceId,
        next: u128,
        objects: BTreeMap<u128, Vec<u8>>,
    }
    impl Objects {
        fn new() -> Self {
            Self {
                store: StoreInstanceId::new(1u128 << 100).expect("source store identity"),
                next: 1,
                objects: BTreeMap::new(),
            }
        }
    }
    impl BlockSource for Objects {
        fn resolve<'a>(
            &'a self,
            reference: PhysicalRef,
            resources: &mut TreeResources<'_>,
        ) -> Result<FramedBlock<'a>, TreeError> {
            resources.step(1)?;
            let bytes = self
                .objects
                .get(&reference.artifact.get())
                .ok_or(TreeError::Missing)?;
            let frame = artifact::decode(
                ContainerKind::Object,
                Some((self.store, reference.artifact)),
                bytes,
            )?;
            Ok(frame.framed_block(reference)?)
        }
    }
    impl BlockSink for Objects {
        fn append(
            &mut self,
            kind: BlockKind,
            generation: GraphGeneration,
            bytes: &[u8],
            resources: &mut TreeResources<'_>,
        ) -> Result<PhysicalRef, TreeError> {
            resources.step(1)?;
            let artifact = ArtifactId::new(self.next)?;
            let identity = ArtifactIdentity {
                store: self.store,
                artifact,
                generation,
                creation_serial: self.next as u64,
            };
            let blocks = [Block {
                kind,
                payload: bytes,
            }];
            let mut output = vec![0; artifact::encoded_len(ContainerKind::Object, &blocks)?];
            artifact::encode_into(ContainerKind::Object, identity, &blocks, &mut output)?;
            let reference =
                artifact::decode(ContainerKind::Object, Some((self.store, artifact)), &output)?
                    .reference(0)?;
            if self.objects.insert(self.next, output).is_some() {
                return Err(TreeError::Invalid("duplicate fixture artifact"));
            }
            self.next += 1;
            Ok(reference)
        }
    }

    struct EmptyCatalog;
    impl RecordCatalog<Objects> for EmptyCatalog {
        fn resolve(
            &self,
            _: SymbolKind,
            _: PayloadSlice<'_, Objects>,
            _: &mut TreeResources<'_>,
        ) -> Result<Symbol, TreeError> {
            Err(TreeError::Invalid("unexpected fixture catalog lookup"))
        }
    }

    struct View {
        token: QueryView,
        lease: SnapshotLease,
    }
    impl RetainedView for View {
        fn query_view(&self) -> &QueryView {
            &self.token
        }

        fn check_active(&self) -> Result<(), QueryError> {
            self.lease
                .check_active()
                .map_err(|_| QueryError::ReadCancelled)
        }
    }

    fn tower() -> EmbeddingTower {
        EmbeddingTower {
            model_id: "doc".into(),
            model_version: "1".into(),
            weights_digest: vec![0x42],
            dims: 2,
            normalization: Normalization::None,
            prompt_prefix: "".into(),
            max_tokens: 3,
            runtime: EmbeddingRuntime::CpuReference,
            compute_units: ComputeUnits::Cpu,
            os_build: None,
        }
    }

    fn retain_source_lifetime<'a, S: BlockSource>(
        _: &'a S,
        state: Option<NodeRecordState<'a, S>>,
    ) -> Option<NodeRecordState<'a, S>> {
        state
    }

    #[test]
    fn node_payload_leaf_preserves_full_identity_optional_payloads_and_state() {
        let directory = tempfile::tempdir().expect("store fixture");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("store");
        let shared = GraphResources::from_store(&store).expect("shared resources");
        let prepare_control = QueryControl::Cancel(CancelToken::new());
        let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("writer memory");
        let memory = StorageMemory::new(&writer, &prepare_control, 32 * 1024 * 1024)
            .expect("storage memory");
        let document = tower();
        let full = NodeId::new(u128::MAX - 17).expect("full-width node identity");
        let absent = NodeId::new((1u128 << 119) + 23).expect("absent-payload node identity");
        let tombstoned = NodeId::new((1u128 << 118) + 29).expect("tombstoned node identity");
        let missing = NodeId::new((1u128 << 117) + 31).expect("missing node identity");
        let generation = GraphGeneration::new(7);
        let mut objects = Objects::new();
        let source_store = objects.store;
        let roots = {
            let mut resources =
                TreeResources::for_prepare(&memory, 10_000_000).expect("prepare resources");
            let mut scratch = TreeScratch::for_prepare(&memory).expect("tree scratch");

            let embedded =
                CanonicalEmbedding::new(&document, &[1.25, -2.5]).expect("canonical embedding");
            let live = CanonicalContents::node(&mut [], &mut [], Some(""), Some(embedded))
                .expect("live canonical contents");
            let mut live_bytes = Vec::new();
            live.write_to(&mut live_bytes, &mut || Ok(()))
                .expect("encode live canonical contents");
            let live_canonical = prepare_payload(
                &mut objects,
                source_store,
                generation,
                BlockKind::CanonicalImage,
                &live_bytes,
                &mut resources,
            )
            .expect("prepare live canonical payload");
            let live_provenance = prepare_provenance(
                &mut objects,
                source_store,
                generation,
                OperationProvenance::from_fields(
                    Some(1),
                    OperationFields {
                        operation: GraphOperation::CypherEdit,
                        key: None,
                        requested_revision: GraphRevision::new(1).expect("live revision"),
                        installed_revision: GraphRevision::new(1).expect("live revision"),
                        expected: ExpectedGraphState::Absent,
                        incarnation: EntityId::Node(full),
                        delete_mode: None,
                        original_generation: GraphGeneration::new(1),
                    },
                )
                .expect("live provenance"),
                &memory,
                &mut resources,
            )
            .expect("prepare live provenance");
            let live_record = prepare_record(
                &mut objects,
                RecordInput {
                    store: source_store,
                    generation,
                    entity: EntityId::Node(full),
                    canonical: live_canonical,
                    provenance: live_provenance,
                },
                &EmptyCatalog,
                Some(&document),
                &memory,
                &mut resources,
            )
            .expect("prepare live record");

            let absent_contents = CanonicalContents::node(&mut [], &mut [], None, None)
                .expect("absent-payload canonical contents");
            let mut absent_bytes = Vec::new();
            absent_contents
                .write_to(&mut absent_bytes, &mut || Ok(()))
                .expect("encode absent-payload canonical contents");
            let absent_canonical = prepare_payload(
                &mut objects,
                source_store,
                generation,
                BlockKind::CanonicalImage,
                &absent_bytes,
                &mut resources,
            )
            .expect("prepare absent-payload canonical payload");
            let absent_provenance = prepare_provenance(
                &mut objects,
                source_store,
                generation,
                OperationProvenance::from_fields(
                    Some(1),
                    OperationFields {
                        operation: GraphOperation::CypherEdit,
                        key: None,
                        requested_revision: GraphRevision::new(2).expect("absent revision"),
                        installed_revision: GraphRevision::new(2).expect("absent revision"),
                        expected: ExpectedGraphState::Absent,
                        incarnation: EntityId::Node(absent),
                        delete_mode: None,
                        original_generation: GraphGeneration::new(2),
                    },
                )
                .expect("absent provenance"),
                &memory,
                &mut resources,
            )
            .expect("prepare absent provenance");
            let absent_record = prepare_record(
                &mut objects,
                RecordInput {
                    store: source_store,
                    generation,
                    entity: EntityId::Node(absent),
                    canonical: absent_canonical,
                    provenance: absent_provenance,
                },
                &EmptyCatalog,
                Some(&document),
                &memory,
                &mut resources,
            )
            .expect("prepare absent-payload record");

            let tombstone_provenance = prepare_provenance(
                &mut objects,
                source_store,
                generation,
                OperationProvenance::from_fields(
                    Some(1),
                    OperationFields {
                        operation: GraphOperation::CypherEdit,
                        key: None,
                        requested_revision: GraphRevision::new(3).expect("tombstone revision"),
                        installed_revision: GraphRevision::new(3).expect("tombstone revision"),
                        expected: ExpectedGraphState::Entity(EntityId::Node(tombstoned)),
                        incarnation: EntityId::Node(tombstoned),
                        delete_mode: Some(GraphDeleteMode::Detach),
                        original_generation: GraphGeneration::new(3),
                    },
                )
                .expect("tombstone provenance"),
                &memory,
                &mut resources,
            )
            .expect("prepare tombstone provenance");
            let tombstone_record = prepare_node_tombstone(
                &mut objects,
                source_store,
                generation,
                tombstoned,
                tombstone_provenance,
                &mut resources,
            )
            .expect("prepare tombstone record");

            let mut root =
                DirectoryRoot::empty(source_store, TreeKind::Nodes, GraphGeneration::new(0));
            for (node, record) in [
                (full, live_record),
                (absent, absent_record),
                (tombstoned, tombstone_record),
            ] {
                let mut value = [0; 48];
                record
                    .encode_into(&mut value)
                    .expect("encode record reference");
                root = insert(
                    &mut objects,
                    root,
                    &node.get().to_le_bytes(),
                    &value,
                    generation,
                    &mut scratch,
                    &mut resources,
                )
                .expect("insert node record");
            }
            let mut references = [None; 8];
            references[TreeKind::Nodes as usize - 1] = root.reference();
            GraphRoots::from_references(source_store, generation, references).expect("graph roots")
        };
        drop(memory);
        drop(writer);

        let query_memory = QueryMemory::new(&shared, 2 * 1024 * 1024).expect("query memory");
        let retained = View {
            token: QueryView::new(source_store, generation),
            lease: store.snapshot().expect("retained lease"),
        };
        let query_control = QueryControl::Cancel(CancelToken::new());
        let mut context = RuntimeContext::new(
            &retained,
            &query_control,
            &query_memory,
            RuntimeLimits::default(),
        )
        .expect("query runtime");
        let (full_state, absent_state, tombstone_state) = {
            let mut resources = TreeResources::for_query(&mut context).expect("query resources");
            let full_state = retain_source_lifetime(
                &objects,
                lookup_node_state(
                    &objects,
                    roots,
                    full,
                    &EmptyCatalog,
                    Some(&document),
                    &mut resources,
                )
                .expect("full node lookup"),
            )
            .expect("full node must be present");
            let absent_state = retain_source_lifetime(
                &objects,
                lookup_node_state(
                    &objects,
                    roots,
                    absent,
                    &EmptyCatalog,
                    Some(&document),
                    &mut resources,
                )
                .expect("absent-payload node lookup"),
            )
            .expect("absent-payload node must be present");
            let tombstone_state = retain_source_lifetime(
                &objects,
                lookup_node_state(
                    &objects,
                    roots,
                    tombstoned,
                    &EmptyCatalog,
                    Some(&document),
                    &mut resources,
                )
                .expect("tombstone lookup"),
            )
            .expect("tombstone must be present");
            assert!(
                lookup_node_state(
                    &objects,
                    roots,
                    missing,
                    &EmptyCatalog,
                    Some(&document),
                    &mut resources,
                )
                .expect("missing node lookup")
                .is_none(),
                "missing and tombstoned nodes must remain distinct"
            );
            (full_state, absent_state, tombstone_state)
        };

        let (text, vector) = match full_state {
            NodeRecordState::Live(record) => {
                assert_eq!(
                    record.shape(),
                    RecordShape::Node {
                        id: full,
                        labels: 0
                    }
                );
                let text = record
                    .canonical()
                    .stored_text()
                    .expect("present-empty text");
                assert!(text.is_empty());
                let vector = record.canonical().stored_vector().expect("present vector");
                assert_eq!(vector.dimensions(), 2);
                (text, vector)
            }
            NodeRecordState::Tombstone(_) => panic!("live full-width node became tombstone"),
        };
        match absent_state {
            NodeRecordState::Live(record) => {
                assert!(record.canonical().stored_text().is_none());
                assert!(record.canonical().stored_vector().is_none());
            }
            NodeRecordState::Tombstone(_) => panic!("live absent-payload node became tombstone"),
        }
        match tombstone_state {
            NodeRecordState::Tombstone(record) => {
                assert_eq!(record.node(), tombstoned);
                assert_eq!(record.revision(), GraphRevision::new(3).expect("revision"));
            }
            NodeRecordState::Live(_) => panic!("tombstone became live node"),
        }

        {
            let mut resources =
                TreeResources::for_query(&mut context).expect("reborrow query resources");
            let mut no_bytes = [];
            assert_eq!(
                text.read_at(0, &mut no_bytes, &mut resources)
                    .expect("read present-empty text"),
                0
            );
            assert_eq!(
                vector
                    .coordinate(0, &mut resources)
                    .expect("first borrowed coordinate"),
                1.25
            );
            assert_eq!(
                vector
                    .coordinate(1, &mut resources)
                    .expect("second borrowed coordinate"),
                -2.5
            );
        }

        drop(context);
        drop(retained);
        store.close().expect("close store");
    }
}
