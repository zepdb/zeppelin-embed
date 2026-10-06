//! Nonshipping ZE-41 actual-path executor. Expectations belong to the independent runner.
#![allow(
    missing_docs,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use super::{GraphMaintenancePolicy, GraphStore, GraphStoreError, GraphWriteResult};
use crate::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower, Normalization};
use crate::lifecycle::native_graph::tests::publication::{FaultPoint, RecordingVfs};
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use crate::property_graph::catalog::{Symbol, SymbolKind};
use crate::property_graph::query::runtime::{RuntimeContext, RuntimeLimits};
use crate::property_graph::resources::GraphResources;
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::storage::adjacency::RelationshipRow;
use crate::property_graph::storage::search::Modality;
use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};
use crate::property_graph::storage::{
    CursorState, DirectionSelection, GraphReadView, RelationshipTypeSelection,
};
use crate::property_graph::{
    ApplicationKey, CanonicalContents, CanonicalEmbedding, EntityKind, GraphGeneration, GraphName,
    GraphProperty, GraphRevision, NodeId, NodeRef, PropertyData, PropertyValue, RelId,
    StoreInstanceId,
};
use crate::vfs::Vfs;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct Fixture {
    pub store: u128,
    pub rank: i64,
    pub text: String,
    pub coordinates: [f32; 2],
}

struct FixedEntropy(u128);
impl crate::property_graph::storage::allocation::EntropyProvider for FixedEntropy {
    fn fill_nonce(&mut self, output: &mut [u8; 16]) -> std::io::Result<()> {
        *output = self.0.to_le_bytes();
        self.0 = self.0.wrapping_add(1).max(1);
        Ok(())
    }
}

pub fn document() -> EmbeddingTower {
    EmbeddingTower {
        model_id: "ze41-document".into(),
        model_version: "1".into(),
        weights_digest: vec![0x41, 0xa5],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    }
}
fn options() -> OpenOptions {
    OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024)
}
fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchObservation {
    pub store: StoreInstanceId,
    pub generation: GraphGeneration,
    pub sequence: u64,
    pub node_count: u32,
    pub canonical_properties: [u64; 3],
    pub canonical_modalities: [[bool; 2]; 3],
    pub relationship_type_name: Vec<u8>,
    pub label_counts: [u64; 2],
    pub keys: [Vec<u8>; 3],
    pub namespaces: [Vec<u8>; 3],
    pub original_generations: [u64; 3],
    pub relationship_revision: u64,
    pub first_revision: u64,
    pub second_revision: u64,
    pub rank_property: Vec<u8>,
    pub text: Vec<u8>,
    pub vector_bits: Vec<u32>,
    pub sparse_vector_bits: Vec<u32>,
    pub text_count: u64,
    pub vector_count: u64,
    pub text_membership: [bool; 2],
    pub vector_membership: [bool; 2],
    pub relationship: RelationshipRow,
    pub outgoing: Vec<RelationshipRow>,
    pub incoming: Vec<RelationshipRow>,
}

struct ObserveRecoveredMixed {
    first: NodeId,
    second: NodeId,
    relationship: RelId,
}

impl crate::lifecycle::native_graph::NativeReadConsumer<BatchObservation>
    for ObserveRecoveredMixed
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<BatchObservation, TreeError> {
        let mut resources = TreeResources::for_query(runtime)?;
        let first = view
            .lookup_node(self.first, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered first node"))?;
        let second = view
            .lookup_node(self.second, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered second node"))?;
        let property = match view.expression_symbol(
            SymbolKind::Property,
            GraphName::new("rank").map_err(|_| TreeError::Invalid("property name"))?,
            &mut resources,
        )? {
            Some(Symbol::Property(property)) => property,
            _ => return Err(TreeError::Invalid("missing recovered property symbol")),
        };
        let rank = view
            .node_property(&first, property, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered property"))?;
        let mut rank_property =
            vec![0_u8; usize::try_from(rank.len()).map_err(|_| TreeError::Memory)?];
        let rank_bytes = rank_property.len();
        if rank.read_at(0, &mut rank_property, &mut resources)? != rank_bytes {
            return Err(TreeError::Invalid("short recovered property"));
        }
        let text = view
            .stored_text(self.first, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered text"))?;
        let mut text_bytes =
            vec![0_u8; usize::try_from(text.len()).map_err(|_| TreeError::Memory)?];
        let text_length = text_bytes.len();
        if text.read_at(0, &mut text_bytes, &mut resources)? != text_length {
            return Err(TreeError::Invalid("short recovered text"));
        }
        let vector = view
            .vector_payload(self.second, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered vector"))?;
        let mut vector_bits = Vec::new();
        for index in 0..vector.dimensions() {
            vector_bits.push(vector.coordinate(index, &mut resources)?.to_bits());
        }
        let rel = view
            .lookup_relationship(self.relationship, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered relationship"))?;
        let relationship = rel.row();
        let records = [first.record(), second.record(), rel.record()];
        let canonical_properties = records.map(|r| r.canonical().property_count());
        let canonical_modalities = records.map(|r| {
            [
                r.canonical().stored_text().is_some(),
                r.canonical().stored_vector().is_some(),
            ]
        });
        let relationship_type = match rel.record().canonical().shape() {
            crate::property_graph::storage::records::CanonicalShape::Relationship {
                relationship_type,
                ..
            } => *relationship_type,
            _ => return Err(TreeError::Invalid("relationship canonical shape")),
        };
        let mut relationship_type_name = vec![0; relationship_type.len() as usize];
        if relationship_type.read_at(0, &mut relationship_type_name, &mut resources)?
            != relationship_type_name.len()
        {
            return Err(TreeError::Invalid("short relationship type name"));
        }
        let mut label_counts = [0; 2];
        for (index, record) in records.iter().take(2).enumerate() {
            if let crate::property_graph::storage::records::CanonicalShape::Node { labels } =
                record.canonical().shape()
            {
                label_counts[index] = *labels;
            } else {
                return Err(TreeError::Invalid("node canonical shape"));
            }
        }
        let original_generations = records.map(|r| r.provenance().original_generation().get());
        let relationship_revision = rel.record().revision().get();
        let mut keys: [Vec<u8>; 3] = Default::default();
        let mut namespaces: [Vec<u8>; 3] = Default::default();
        for (index, record) in records.into_iter().enumerate() {
            let key = record
                .provenance()
                .key()
                .ok_or(TreeError::Invalid("missing key"))?;
            for (payload, output) in [
                (key.namespace(), &mut namespaces[index]),
                (key.key(), &mut keys[index]),
            ] {
                output.resize(payload.len() as usize, 0);
                if payload.read_at(0, output, &mut resources)? != output.len() {
                    return Err(TreeError::Invalid("short key"));
                }
            }
        }
        let first_revision = first.record().revision().get();
        let second_revision = second.record().revision().get();
        drop(resources);

        let sparse = view.sparse_view(runtime)?;
        let mut resources = TreeResources::for_query(runtime)?;
        let text_membership = [
            sparse
                .lookup(Modality::Text, self.first, &mut resources)?
                .is_some(),
            sparse
                .lookup(Modality::Text, self.second, &mut resources)?
                .is_some(),
        ];
        let first_vector = sparse
            .lookup(Modality::Vector, self.first, &mut resources)?
            .is_some();
        let vector_member = sparse
            .lookup(Modality::Vector, self.second, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered sparse vector"))?;
        let stored = vector_member.vector.ok_or(TreeError::Invalid(
            "missing recovered sparse vector payload",
        ))?;
        let mut sparse_vector_bits = Vec::new();
        for index in 0..stored.dimensions() {
            sparse_vector_bits.push(stored.coordinate(index, &mut resources)?.to_bits());
        }
        let text_count = sparse.text_count();
        let vector_count = sparse.vector_count();
        drop(resources);

        let mut outgoing = Vec::new();
        let mut incoming = Vec::new();
        for (node, direction, output) in [
            (self.first, DirectionSelection::Out, &mut outgoing),
            (self.second, DirectionSelection::In, &mut incoming),
        ] {
            let mut cursor =
                view.expansion_cursor(node, direction, RelationshipTypeSelection::All, runtime)?;
            let mut rows = [relationship; 2];
            loop {
                let (count, state) = view.expand(&mut cursor, &mut rows, runtime)?;
                output.extend_from_slice(
                    rows.get(..count)
                        .ok_or(TreeError::Invalid("recovered expansion extent"))?,
                );
                if state == CursorState::Done {
                    break;
                }
            }
        }
        Ok(BatchObservation {
            node_count: 2,
            canonical_properties,
            canonical_modalities,
            relationship_type_name,
            label_counts,
            keys,
            namespaces,
            original_generations,
            relationship_revision,
            store: view.store_instance_id(),
            generation: view.generation(),
            sequence: view.sequence(),
            first_revision,
            second_revision,
            rank_property,
            text: text_bytes,
            vector_bits,
            sparse_vector_bits,
            text_count,
            vector_count,
            text_membership,
            vector_membership: [first_vector, true],
            relationship,
            outgoing,
            incoming,
        })
    }
}

/// Owns the real public facade; private access is limited to faults and observations.
pub struct ProbeStore {
    graph: GraphStore,
    resources: GraphResources,
}
impl ProbeStore {
    /// Public facade for integrated qualification queries and writes.
    pub fn graph(&self) -> &GraphStore {
        &self.graph
    }
    pub fn create(path: &Path, fixture: &Fixture, vfs: Arc<dyn Vfs>) -> Self {
        let store = Store::create_native_graph_with_infrastructure(
            path,
            super::graph_options(options(), crate::lifecycle::AccessMode::ReadWrite),
            Some(document()),
            vfs,
            Arc::new(crate::lifecycle::SystemMonotonicClock),
            &mut FixedEntropy(fixture.store),
        )
        .expect("ZE41 create");
        let resources = GraphResources::from_store(&store).unwrap();
        let graph = GraphStore { store };
        graph
            .set_maintenance_policy(GraphMaintenancePolicy {
                automatic: false,
                ..GraphMaintenancePolicy::default()
            })
            .unwrap();
        Self { graph, resources }
    }
    pub fn open(path: &Path) -> Result<Self, GraphStoreError> {
        let graph = GraphStore::open(path, options(), Some(document()))?;
        let resources = GraphResources::from_store(&graph.store).unwrap();
        graph.set_maintenance_policy(GraphMaintenancePolicy {
            automatic: false,
            ..GraphMaintenancePolicy::default()
        })?;
        Ok(Self { graph, resources })
    }
    pub fn apply(&self, fixture: &Fixture) -> Result<GraphWriteResult, GraphStoreError> {
        crate::property_graph::with_local_refs(|refs| {
            let tower = document();
            let embedding = CanonicalEmbedding::new(&tower, &fixture.coordinates).unwrap();
            let mut properties = [GraphProperty::new(
                GraphName::new("rank").unwrap(),
                PropertyValue::new(PropertyData::I64(fixture.rank)).unwrap(),
            )];
            let first =
                CanonicalContents::node(&mut [], &mut properties, Some(&fixture.text), None)
                    .unwrap();
            let second = CanonicalContents::node(&mut [], &mut [], None, Some(embedding)).unwrap();
            self.graph.apply_batch(
                &[
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "ze41", "a").unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&first)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "ze41", "b").unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&second)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Relationship, "ze41", "ab").unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Local(refs.node(0).unwrap()),
                            target: NodeRef::Local(refs.node(1).unwrap()),
                            relationship_type: GraphName::new("LINKS").unwrap(),
                            properties: &[],
                        }),
                    },
                ],
                &control(),
            )
        })
    }
    pub fn observe(&self) -> Option<BatchObservation> {
        let lease = self
            .graph
            .store
            .admit_native_read()
            .expect("ZE41 admission");
        if lease.bundle().high_waters().node == 0 {
            assert_eq!(lease.bundle().high_waters().relationship, 0);
            assert_eq!(lease.bundle().base().generation.get(), 0);
            assert_eq!(lease.bundle().sequence(), 0);
            drop(lease);
            assert_eq!(self.query_node_count(), 0);
            return None;
        }
        drop(lease);
        let node_count = self.query_node_count();
        let mut observation = self
            .graph
            .store
            .with_native_read(
                &control(),
                RuntimeLimits::default(),
                8 * 1024 * 1024,
                32,
                ObserveRecoveredMixed {
                    first: NodeId::new(1).unwrap(),
                    second: NodeId::new(2).unwrap(),
                    relationship: RelId::new(1).unwrap(),
                },
            )
            .expect("ZE41 mixed observation");
        observation.node_count = node_count;
        Some(observation)
    }
    fn query_node_count(&self) -> u32 {
        use super::{GraphPlanBacking, GraphQueryPlan};
        use crate::property_graph::query::plan::{
            ExprId, Expression, Operator, OperatorKind, PlanNodeId, Projection, SlotId,
        };
        let unit = vec![PlanNodeId(0)];
        let scan = vec![PlanNodeId(1)];
        let projections = vec![Projection {
            slot: SlotId(10),
            expression: ExprId(0),
        }];
        let operators = vec![
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &unit,
                kind: OperatorKind::ScanNodes {
                    output: SlotId(0),
                    label: None,
                },
            },
            Operator {
                inputs: &scan,
                kind: OperatorKind::Project(&projections),
            },
        ];
        let expressions = vec![Expression::Slot(SlotId(0))];
        let parameters = Vec::new();
        let eager_searches = Vec::new();
        let mut backing = GraphPlanBacking::default();
        backing.vec(&unit).unwrap();
        backing.vec(&scan).unwrap();
        backing.vec(&projections).unwrap();
        let plan = GraphQueryPlan {
            operators: &operators,
            expressions: &expressions,
            parameters: &parameters,
            eager_searches: &eager_searches,
            root: PlanNodeId(2),
            backing: &backing,
            bindings: &[],
            columns: &["n"],
        };
        self.graph
            .query(
                &control(),
                &crate::property_graph::query::completed::GraphQueryOptions::default(),
                &plan,
            )
            .expect("ZE41 public graph query")
            .metadata()
            .rows
    }
    pub fn checkpoint(&self) -> Result<(), GraphStoreError> {
        self.graph
            .store
            .checkpoint_native_graph(&control())
            .map_err(GraphStoreError::graph)
    }
    pub fn maintain_retained(&self) -> BatchObservation {
        use crate::lifecycle::native_graph::NativeReadConsumer;
        use crate::property_graph::query::resources::QueryMemory;
        use crate::property_graph::storage::{
            NativeCatalog, NativeQuerySource, NativeReadCapability,
        };
        let lease = self.graph.store.admit_native_read().unwrap();
        let generation = lease.bundle().base().generation;
        self.graph
            .maintain(&control())
            .expect("ZE41 public maintenance with retained roots");
        assert_eq!(lease.bundle().base().generation, generation);
        let memory = QueryMemory::new(&self.resources, 8 * 1024 * 1024).unwrap();
        let control = control();
        let mut runtime =
            RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(&lease, &runtime).unwrap();
        let mut resources = TreeResources::for_query(&mut runtime).unwrap();
        let source = NativeQuerySource::new(capability, &resources, 32).unwrap();
        let catalog = NativeCatalog::open(&source, &mut resources).unwrap();
        drop(resources);
        let view = GraphReadView::new(&source, &catalog).unwrap();
        ObserveRecoveredMixed {
            first: NodeId::new(1).unwrap(),
            second: NodeId::new(2).unwrap(),
            relationship: RelId::new(1).unwrap(),
        }
        .consume(&view, &mut runtime)
        .expect("retained complete graph/search view")
    }
    pub fn reserved(&self) -> u64 {
        self.resources.reserved_bytes().unwrap()
    }
    pub fn publish_fault(&self) {
        crate::lifecycle::native_graph::tests::publication::arm_query_publication_fault(
            &self.graph.store,
        );
    }
    pub fn publication_fired(&self) -> bool {
        crate::lifecycle::native_graph::tests::publication::query_publication_fault_fired(
            &self.graph.store,
        )
    }
    /// Release mapped owners without running the facade's graceful checkpoint.
    /// Only after this returns may the runner materialize a modeled power cut.
    pub fn release(self) -> u64 {
        self.graph.store.close().expect("ZE41 abrupt-owner release");
        let resources = self.resources.clone();
        drop(self);
        resources.reserved_bytes().unwrap()
    }
    pub fn close(self) -> u64 {
        self.graph.close().expect("ZE41 public close");
        let resources = self.resources.clone();
        drop(self);
        resources.reserved_bytes().unwrap()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Boundary {
    ArtifactCreate,
    ArtifactWrite,
    ArtifactSync,
    DirectorySync,
    WalAppend,
    WalPartialAppend,
    WalSync,
    Publication,
    CheckpointReplace,
    CheckpointSync,
}
impl Boundary {
    pub fn key(self) -> &'static str {
        match self {
            Self::ArtifactCreate => "property-graph.recovery.commit.artifact-create",
            Self::ArtifactWrite => "property-graph.recovery.commit.artifact-write",
            Self::ArtifactSync => "property-graph.recovery.commit.artifact-sync",
            Self::DirectorySync => "property-graph.recovery.commit.directory-sync",
            Self::WalAppend => "property-graph.recovery.commit.wal-append",
            Self::WalPartialAppend => "property-graph.recovery.commit.wal-partial-append",
            Self::WalSync => "property-graph.recovery.commit.wal-sync",
            Self::Publication => "property-graph.recovery.commit.publication",
            Self::CheckpointReplace => "property-graph.recovery.commit.checkpoint-replace",
            Self::CheckpointSync => "property-graph.recovery.commit.checkpoint-sync",
        }
    }
    fn point(self) -> FaultPoint {
        match self {
            Self::ArtifactCreate => FaultPoint::Create,
            Self::ArtifactWrite => FaultPoint::PartialCreate,
            Self::ArtifactSync => FaultPoint::ObjectSync,
            Self::DirectorySync => FaultPoint::DirectorySync,
            Self::WalAppend => FaultPoint::Append,
            Self::WalPartialAppend => FaultPoint::PartialAppend,
            Self::WalSync => FaultPoint::WalSync,
            Self::Publication => FaultPoint::Publish,
            Self::CheckpointReplace => FaultPoint::Rename,
            Self::CheckpointSync => FaultPoint::SelectorSync,
        }
    }
}

pub struct BoundaryReport {
    pub key: &'static str,
    pub fires: u64,
    pub clean_controls: u64,
    pub observation: Option<BatchObservation>,
    pub retry: GraphWriteResult,
    pub reservation_before: u64,
    pub reservation_after: u64,
    pub remaining_ownership: u64,
    pub post_sync_ordinal: usize,
    pub error_kind: Option<String>,
    pub raw_error: Option<String>,
    pub fresh_identity: (u128, u64, u64, bool),
    pub nothing_committed: bool,
    pub stopped_error: Option<String>,
    pub durable_image: std::collections::BTreeMap<String, Vec<u8>>,
    pub protected_before: std::collections::BTreeMap<String, Vec<u8>>,
    pub protected_after: std::collections::BTreeMap<String, Vec<u8>>,
}

pub fn run_boundary(
    path: &Path,
    fixture: &Fixture,
    boundary: Boundary,
    fault: bool,
) -> BoundaryReport {
    #[cfg(feature = "test-seams")]
    {
        with_qualification_nonces(fixture.store as u64, || {
            run_boundary_inner(path, fixture, boundary, fault)
        })
    }
    #[cfg(not(feature = "test-seams"))]
    run_boundary_inner(path, fixture, boundary, fault)
}
fn run_boundary_inner(
    path: &Path,
    fixture: &Fixture,
    boundary: Boundary,
    fault: bool,
) -> BoundaryReport {
    let vfs = Arc::new(RecordingVfs::default());
    let store = ProbeStore::create(path, fixture, vfs.clone());
    let checkpoint = matches!(
        boundary,
        Boundary::CheckpointReplace | Boundary::CheckpointSync
    );
    if checkpoint {
        store.apply(fixture).unwrap();
    }
    let protected_before = image(path);
    vfs.clear_events();
    let before = store.reserved();
    if fault {
        if boundary == Boundary::Publication {
            store.publish_fault();
        } else {
            vfs.arm_fault(boundary.point());
        }
    }
    let result = if checkpoint {
        store
            .graph
            .store
            .checkpoint_native_graph(&control())
            .map(|_| ())
            .map_err(GraphStoreError::graph)
    } else {
        store.apply(fixture).map(|_| ())
    };
    let error_kind = result.as_ref().err().map(|e| format!("{:?}", e.kind()));
    let raw_error = result
        .as_ref()
        .err()
        .map(|e| format!("{e:?}").replace(path.to_string_lossy().as_ref(), "<store>"));
    let nothing_committed = result.as_ref().err().is_some_and(|e| e.nothing_committed());
    if fault {
        let error = result.expect_err("ZE41 fault must fail");
        if !checkpoint {
            assert_eq!(
                error.nothing_committed(),
                matches!(
                    boundary,
                    Boundary::ArtifactCreate
                        | Boundary::ArtifactWrite
                        | Boundary::ArtifactSync
                        | Boundary::DirectorySync
                )
            );
        }
        if boundary == Boundary::Publication {
            assert!(store.publication_fired());
        } else {
            vfs.assert_fired_once();
        }
    } else {
        result.expect("same-seed clean control");
    }
    let post_sync_ordinal =
        if !fault && matches!(boundary, Boundary::DirectorySync | Boundary::CheckpointSync) {
            vfs.directory_sync_ordinal(checkpoint)
        } else {
            0
        };
    let after = store.reserved();
    let stopped_error = if fault && !checkpoint && !nothing_committed {
        Some(format!(
            "{:?}",
            store
                .apply(fixture)
                .expect_err("stopped writer must refuse")
                .kind()
        ))
    } else {
        None
    };
    let protected_after = image(path);
    let durable_image = protected_after.clone();
    // A create collision deliberately belongs to someone else. It is not a
    // valid graph artifact; remove only this test's foreign-owner sentinel.
    if fault && boundary == Boundary::ArtifactCreate {
        for file in std::fs::read_dir(path).unwrap() {
            let path = file.unwrap().path();
            if path.is_file() && std::fs::read(&path).unwrap() == b"foreign-owner" {
                std::fs::remove_file(path).unwrap();
            }
        }
    }
    let mut remaining = store.release();
    let reopened = ProbeStore::open(path).expect("ZE41 permitted complete recovery");
    let observation = reopened.observe();
    let retry = reopened
        .apply(fixture)
        .expect("ZE41 exact retry after recovery");
    let fresh_image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let fresh = reopened
        .graph
        .apply_batch(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze41", "fresh").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&fresh_image)),
            }],
            &control(),
        )
        .expect("fresh allocation after retry");
    let receipt = fresh.receipts().first().unwrap();
    let fresh_identity = match receipt.entity {
        crate::property_graph::EntityId::Node(id) => (
            id.get(),
            receipt.revision.get(),
            receipt.generation.get(),
            receipt.replayed,
        ),
        _ => panic!("fresh node returned relationship"),
    };
    drop(fresh);
    remaining += reopened.close();
    BoundaryReport {
        key: boundary.key(),
        fires: u64::from(fault),
        clean_controls: u64::from(!fault),
        observation,
        retry,
        reservation_before: before,
        reservation_after: after,
        remaining_ownership: remaining,
        post_sync_ordinal,
        error_kind,
        raw_error,
        fresh_identity,
        nothing_committed,
        stopped_error,
        durable_image,
        protected_before,
        protected_after,
    }
}

#[cfg(feature = "test-seams")]
pub fn run_reclaim_boundaries() -> Vec<crate::graph_read_view_test_support::PathReceipt> {
    crate::lifecycle::native_graph::tests::consolidation::run_ze41_reclaim_boundaries()
}

fn image(path: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_str().unwrap().to_owned(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

#[cfg(feature = "test-seams")]
pub fn with_qualification_nonces<T>(seed: u64, run: impl FnOnce() -> T) -> T {
    crate::property_graph::storage::allocation::with_qualification_nonces(seed, run)
}

#[cfg(any(test, feature = "test-seams"))]
pub type ProvenanceEvidence = std::collections::BTreeMap<Vec<u8>, (Vec<u8>, Option<Vec<u8>>)>;
#[cfg(any(test, feature = "test-seams"))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReclaimEvidence {
    pub cell: String,
    pub provenance_phases: Vec<ProvenanceEvidence>,
    pub inventory_pending: String,
    pub inventory_replayed: String,
    pub pending_proof: String,
    pub replayed_proof: String,
    pub completion: String,
    pub retry_receipts: String,
    pub error: Option<String>,
    pub input_image: std::collections::BTreeMap<String, Vec<u8>>,
    pub durable_image: std::collections::BTreeMap<String, Vec<u8>>,
    pub fires: u64,
    pub controls: u64,
}
#[cfg(feature = "test-seams")]
pub fn run_ze75_reclaim_evidence(seed: u64) -> Vec<ReclaimEvidence> {
    crate::lifecycle::native_graph::tests::consolidation::run_ze75_reclaim_evidence(seed)
}
