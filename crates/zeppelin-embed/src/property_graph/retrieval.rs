//! View-bound preparation and identity leaves for native graph retrieval.
#![allow(
    dead_code,
    reason = "the private retrieval adapter is consumed by downstream native search execution"
)]

use super::staging::NormalizedDelta;
use super::{EntityId, GraphRevision, NodeId};

pub(crate) mod rank;
use crate::ingest::{DocId, DocumentVersion, Revision};
use crate::lifecycle::materialize::{VersionMismatch, require_document_version};
use crate::lifecycle::prepared::{VectorValidationError, validate_vector_coordinates};
use crate::property_graph::catalog::GraphInterpretation;
use crate::property_graph::query::QueryError;
use crate::property_graph::query::eligibility::Eligibility;
use crate::property_graph::query::plan::SearchMode;
use crate::property_graph::query::resources::{QueryArena, QueryMemory, QueryReservation};
use crate::property_graph::query::runtime::{
    RuntimeContext, RuntimeError, RuntimeInstanceId, WorkKind,
};
use crate::property_graph::storage::records::StoredVector;
use crate::property_graph::storage::tree::directory::{NativeReadEvent, TreeError, TreeResources};
use crate::property_graph::storage::{GraphReadView, NativeQuerySource, NodeView};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdentityAdapterError {
    ZeroNode,
    ZeroRevision,
    Relationship,
}

pub(crate) const fn document_version_for_node(
    node: NodeId,
    revision: GraphRevision,
) -> DocumentVersion {
    DocumentVersion::new(DocId::new(node.get()), Revision::new(revision.get()))
}

pub(crate) fn node_version_from_document(
    version: DocumentVersion,
) -> Result<(NodeId, GraphRevision), IdentityAdapterError> {
    let node = NodeId::new(version.doc_id().get()).map_err(|_| IdentityAdapterError::ZeroNode)?;
    let revision = GraphRevision::new(version.revision().get())
        .map_err(|_| IdentityAdapterError::ZeroRevision)?;
    Ok((node, revision))
}

pub(crate) fn staged_node_version(
    delta: &NormalizedDelta<'_>,
) -> Result<DocumentVersion, IdentityAdapterError> {
    let fields = delta.provenance().fields();
    match fields.incarnation {
        EntityId::Node(node) => Ok(document_version_for_node(node, fields.installed_revision)),
        EntityId::Relationship(_) => Err(IdentityAdapterError::Relationship),
    }
}

#[derive(Debug)]
pub(crate) enum RetrievalError {
    Identity(IdentityAdapterError),
    Storage(TreeError),
    Control(RuntimeError),
    Eligibility(QueryError),
    NoVectorSpace,
    Dimension {
        expected: usize,
        actual: usize,
    },
    Vector(crate::quant::QuantError),
    MissingVersion(DocumentVersion),
    Version(VersionMismatch),
    Memory,
    /// The selected method must retain more candidates than the caller allowed.
    CandidateWindow {
        required: usize,
        window: usize,
    },
    /// A raw V1 vector source cannot serve a quantized or approximate route.
    UnindexedVectorSource,
    /// Existing graph kernel refusal, preserved without reinterpretation.
    Graph(crate::graph::search::GraphSearchError),
    /// A retrieval contract was violated by admitted state.
    Invariant(&'static str),
}

impl From<TreeError> for RetrievalError {
    fn from(error: TreeError) -> Self {
        Self::Storage(error)
    }
}

impl From<IdentityAdapterError> for RetrievalError {
    fn from(error: IdentityAdapterError) -> Self {
        Self::Identity(error)
    }
}

pub(crate) enum PreparedEligibility<'a> {
    AllIndexed,
    Set(&'a [NodeId]),
}

pub(crate) struct PreparedNativeVector<'q, 'e> {
    view: *const crate::property_graph::query::QueryView,
    coordinates: &'q [f32],
    mode: SearchMode,
    eligibility: PreparedEligibility<'e>,
}

impl<'q, 'e> PreparedNativeVector<'q, 'e> {
    pub(crate) const fn coordinates(&self) -> &'q [f32] {
        self.coordinates
    }

    pub(crate) const fn mode(&self) -> SearchMode {
        self.mode
    }

    pub(crate) const fn eligibility(&self) -> &PreparedEligibility<'e> {
        &self.eligibility
    }
}

pub(crate) struct ResolvedNativeNode<
    'a,
    S: crate::property_graph::storage::tree::directory::BlockSource,
> {
    version: DocumentVersion,
    view: *const crate::property_graph::query::QueryView,
    node: NodeView<'a, S>,
}

impl<'a, S: crate::property_graph::storage::tree::directory::BlockSource>
    ResolvedNativeNode<'a, S>
{
    pub(crate) const fn version(&self) -> DocumentVersion {
        self.version
    }

    pub(crate) const fn node(&self) -> &NodeView<'a, S> {
        &self.node
    }
}

pub(crate) struct NativeRetrievalContext<'view, 's, 'lease, 'm, 'g> {
    view: &'view GraphReadView<'s, 'lease, 'm, 'g>,
    query_view: &'view crate::property_graph::query::QueryView,
    interpretation: GraphInterpretation<'view>,
    runtime: RuntimeInstanceId,
    memory: &'m QueryMemory<'g>,
    _charge: QueryReservation<'m, 'g>,
}

impl<'view, 's, 'lease, 'm, 'g> NativeRetrievalContext<'view, 's, 'lease, 'm, 'g> {
    pub(crate) fn new(
        view: &'view GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Self, RetrievalError> {
        let binding = view.retrieval_binding(runtime)?;
        let memory = runtime.memory();
        let charge = memory
            .reserve(std::mem::size_of::<Self>())
            .map_err(RuntimeError::Memory)
            .map_err(RetrievalError::Control)?;
        Ok(Self {
            view,
            query_view: binding.view,
            interpretation: binding.interpretation,
            runtime: runtime.identity(),
            memory,
            _charge: charge,
        })
    }

    fn validate_runtime(
        &self,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), RetrievalError> {
        let binding = self.view.retrieval_binding(runtime)?;
        if !std::ptr::eq(binding.view, self.query_view)
            || binding.interpretation != self.interpretation
            || runtime.identity() != self.runtime
            || !std::ptr::eq(runtime.memory(), self.memory)
        {
            return Err(RetrievalError::Storage(TreeError::Invalid(
                "foreign native retrieval context",
            )));
        }
        Ok(())
    }

    pub(crate) fn prepare_vector<'q, 'e, 'v, 'em, 'eg>(
        &self,
        coordinates: &'q [f32],
        mode: SearchMode,
        eligibility: Eligibility<'e, 'v, 'em, 'eg>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<PreparedNativeVector<'q, 'e>, RetrievalError> {
        self.validate_runtime(runtime)?;
        let eligibility = match eligibility {
            Eligibility::AllIndexed => PreparedEligibility::AllIndexed,
            Eligibility::Set(set) => PreparedEligibility::Set(
                set.ids_for(self.query_view)
                    .map_err(RetrievalError::Eligibility)?,
            ),
        };
        let declaration = self
            .interpretation
            .embedding()
            .ok_or(RetrievalError::NoVectorSpace)?;
        let expected = declaration.dimensions() as usize;
        if coordinates.len() != expected {
            return Err(RetrievalError::Dimension {
                expected,
                actual: coordinates.len(),
            });
        }
        validate_vector_coordinates(
            coordinates,
            crate::property_graph::MAX_GRAPH_INPUT_BYTES / std::mem::size_of::<f32>(),
            |_| {
                runtime.check_work(WorkKind::VectorCoordinates, 1)?;
                runtime.check_work(WorkKind::VectorBytes, 4)?;
                runtime.charge(WorkKind::VectorCoordinates, 1)?;
                runtime.charge(WorkKind::VectorBytes, 4)
            },
        )
        .map_err(|error| match error {
            VectorValidationError::Data(error) => RetrievalError::Vector(error),
            VectorValidationError::Control(error) => RetrievalError::Control(error),
        })?;
        Ok(PreparedNativeVector {
            view: self.query_view,
            coordinates,
            mode,
            eligibility,
        })
    }

    pub(crate) fn resolve(
        &self,
        expected: DocumentVersion,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<ResolvedNativeNode<'s, NativeQuerySource<'lease, 'm, 'g>>, RetrievalError> {
        self.validate_runtime(runtime)?;
        let (node, _) = node_version_from_document(expected)?;
        let mut resources = TreeResources::for_query(runtime)?;
        let resolved = self
            .view
            .lookup_node(node, &mut resources)?
            .ok_or(RetrievalError::MissingVersion(expected))?;
        let record = resolved.record();
        let EntityId::Node(actual_node) = record.incarnation() else {
            return Err(RetrievalError::Identity(IdentityAdapterError::Relationship));
        };
        let actual = document_version_for_node(actual_node, record.revision());
        require_document_version(expected, Some(actual)).map_err(RetrievalError::Version)?;
        Ok(ResolvedNativeNode {
            version: actual,
            view: self.query_view,
            node: resolved,
        })
    }

    fn validate_resolved<S: crate::property_graph::storage::tree::directory::BlockSource>(
        &self,
        resolved: &ResolvedNativeNode<'_, S>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), RetrievalError> {
        self.validate_runtime(runtime)?;
        if !std::ptr::eq(resolved.view, self.query_view) {
            return Err(RetrievalError::Storage(TreeError::Invalid(
                "foreign resolved native node",
            )));
        }
        Ok(())
    }

    pub(crate) fn copy_text(
        &self,
        resolved: &ResolvedNativeNode<'s, NativeQuerySource<'lease, 'm, 'g>>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Option<QueryArena<'m, 'g, u8>>, RetrievalError> {
        self.validate_resolved(resolved, runtime)?;
        let Some(payload) = resolved.node.record().canonical().stored_text() else {
            return Ok(None);
        };
        let length = usize::try_from(payload.len()).map_err(|_| RetrievalError::Memory)?;
        let mut output = QueryArena::new(self.memory, length)
            .map_err(RuntimeError::Memory)
            .map_err(RetrievalError::Control)?;
        let mut resources = TreeResources::for_query(runtime)?;
        let mut offset = 0_u64;
        while offset < payload.len() {
            let bytes = payload.span_at(offset, &mut resources)?;
            if bytes.is_empty() {
                return Err(RetrievalError::Storage(TreeError::Invalid(
                    "short native text payload",
                )));
            }
            resources.read_event(NativeReadEvent::CopiedBytes(bytes.len() as u64))?;
            output
                .extend_copy(bytes)
                .map_err(RuntimeError::Memory)
                .map_err(RetrievalError::Control)?;
            offset = offset
                .checked_add(bytes.len() as u64)
                .ok_or(RetrievalError::Memory)?;
        }
        Ok(Some(output))
    }

    pub(crate) fn copy_vector(
        &self,
        resolved: &ResolvedNativeNode<'s, NativeQuerySource<'lease, 'm, 'g>>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Option<QueryArena<'m, 'g, f32>>, RetrievalError> {
        self.validate_resolved(resolved, runtime)?;
        let Some(vector): Option<StoredVector<'_, _>> =
            resolved.node.record().canonical().stored_vector()
        else {
            return Ok(None);
        };
        let dimensions = vector.dimensions() as usize;
        let mut output = QueryArena::new(self.memory, dimensions)
            .map_err(RuntimeError::Memory)
            .map_err(RetrievalError::Control)?;
        let mut resources = TreeResources::for_query(runtime)?;
        for index in 0..vector.dimensions() {
            let coordinate = vector.coordinate(index, &mut resources)?;
            resources.read_event(NativeReadEvent::CopiedBytes(4))?;
            output
                .push(coordinate)
                .map_err(RuntimeError::Memory)
                .map_err(RetrievalError::Control)?;
        }
        Ok(Some(output))
    }
}
