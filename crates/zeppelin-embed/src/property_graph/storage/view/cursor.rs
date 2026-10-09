//! Opaque resumable high-level native read cursors.

use super::{NativeCatalog, NativeQuerySource, lookup_node_state, scan_live_nodes_after};
use crate::lifecycle::native_graph::NativeReadLease;
use crate::property_graph::catalog::{LabelId, RelTypeId};
use crate::property_graph::query::resources::{QueryArena, QueryMemory, QueryReservation};
use crate::property_graph::query::runtime::{
    RetainedView, RuntimeContext, RuntimeError, RuntimeInstanceId, WorkKind,
};
use crate::property_graph::storage::adjacency::{
    AdjacencyQuery, Direction, NativeGraphReader, RangeScratch, RelationshipRange, RelationshipRow,
    UpperBound,
};
use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};
use crate::property_graph::{NodeId, RelId};
use std::marker::PhantomData;

#[derive(Clone, Copy)]
pub(crate) enum RelationshipTypeSelection<'a> {
    All,
    Any(&'a [RelTypeId]),
}

#[derive(Clone, Copy)]
pub(crate) enum LabelSelection<'a> {
    All,
    AllOf(&'a [LabelId]),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CursorState {
    More,
    Done,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DirectionSelection {
    Out,
    In,
    Undirected,
}

pub(crate) struct RelCursor<'view, 'm, 'g> {
    view_token: u64,
    runtime: RuntimeInstanceId,
    memory: &'m QueryMemory<'g>,
    types: Option<QueryArena<'m, 'g, RelTypeId>>,
    after: Option<RelId>,
    exhausted: bool,
    failed: bool,
    _view: PhantomData<&'view ()>,
    _charge: QueryReservation<'m, 'g>,
}

pub(crate) struct NodeCursor<'view, 'm, 'g> {
    view_token: u64,
    runtime: RuntimeInstanceId,
    memory: &'m QueryMemory<'g>,
    labels: QueryArena<'m, 'g, LabelId>,
    documents: Option<crate::lifecycle::native_graph::documents::DocumentCursor>,
    incident_sources: bool,
    folder: Option<crate::lifecycle::native_graph::documents::FolderCandidates<'m, 'g>>,
    full_drain: bool,
    explicit_nodes: Option<QueryArena<'m, 'g, NodeId>>,
    graph_exhausted: bool,
    after: Option<NodeId>,
    exhausted: bool,
    failed: bool,
    _view: PhantomData<&'view ()>,
    _charge: QueryReservation<'m, 'g>,
}

impl<'view, 'm, 'g> NodeCursor<'view, 'm, 'g> {
    pub(super) fn new<'lease>(
        lease: &NativeReadLease,
        selection: LabelSelection<'_>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Self, TreeError> {
        lease
            .check_active()
            .map_err(RuntimeError::Value)
            .map_err(TreeError::Runtime)?;
        runtime.checkpoint().map_err(TreeError::Runtime)?;
        if !std::ptr::eq(runtime.view(), lease.query_view()) {
            return Err(TreeError::Invalid("foreign native read view"));
        }
        let memory = runtime.memory();
        let charge = memory
            .reserve(std::mem::size_of::<Self>())
            .map_err(RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        let values = match selection {
            LabelSelection::All => &[][..],
            LabelSelection::AllOf(values) => values,
        };
        let mut labels = QueryArena::new(memory, values.len())
            .map_err(RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        for value in values {
            if !labels.as_slice().contains(value) {
                labels
                    .push(*value)
                    .map_err(RuntimeError::Memory)
                    .map_err(TreeError::Runtime)?;
            }
        }
        labels
            .as_mut_slice()
            .sort_unstable_by_key(|value| value.get());
        Ok(Self {
            view_token: lease.token(),
            runtime: runtime.identity(),
            memory,
            documents: (labels.is_empty()
                || labels
                    .as_slice()
                    .iter()
                    .all(|label| label.get() == u64::MAX))
            .then(crate::lifecycle::native_graph::documents::DocumentCursor::default),
            labels,
            incident_sources: false,
            folder: None,
            full_drain: false,
            explicit_nodes: None,
            graph_exhausted: false,
            after: None,
            exhausted: false,
            failed: false,
            _view: PhantomData,
            _charge: charge,
        })
    }

    pub(super) fn incident_sources<'lease>(
        lease: &NativeReadLease,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Self, TreeError> {
        let mut cursor = Self::new(lease, LabelSelection::All, runtime)?;
        cursor.incident_sources = true;
        cursor.documents = None;
        Ok(cursor)
    }

    /// Only callers that prove a read-only full drain may retain membership.
    pub(crate) fn enable_full_drain(&mut self) {
        self.full_drain = true;
    }

    fn retain_explicit_nodes<'lease>(
        &mut self,
        lease: &NativeReadLease,
        source: &NativeQuerySource<'lease, 'm, 'g>,
        catalog: &NativeCatalog<'_, 'm, 'g>,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        use super::super::payload::PayloadRef;
        use super::super::stream::PayloadSlice;
        use super::super::tree::{Key, TreeKind, directory::DirectoryCursor};

        let root = lease.bundle().roots().directory(TreeKind::Nodes)?;
        // The directory has no cardinality summary. Count its entries first so
        // the fixed-capacity arena is charged before allocation, including IDs
        // of tombstones. Both passes stay on this cursor's immutable admission.
        let mut count = 0_usize;
        {
            let mut cursor = DirectoryCursor::seek(source, root, None, resources)?;
            while cursor.next_entry(resources)?.is_some() {
                count = count.checked_add(1).ok_or(TreeError::Memory)?;
            }
        }
        let mut nodes = QueryArena::new(self.memory, count)
            .map_err(RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        let mut cursor = DirectoryCursor::seek(source, root, None, resources)?;
        while let Some(entry) = cursor.next_entry(resources)? {
            let Key::Inline(key) = entry.key() else {
                return Err(TreeError::Invalid("overflow node identity"));
            };
            let node = NodeId::from(crate::ingest::DocId::new(u128::from_le_bytes(
                key.try_into()
                    .map_err(|_| TreeError::Invalid("node identity width"))?,
            )));
            super::verify_node_state(
                PayloadSlice::new(
                    source,
                    root.store(),
                    entry.creation_generation(),
                    PayloadRef::decode(entry.value())?,
                ),
                node,
                catalog,
                lease.bundle().document(),
                resources,
            )?;
            nodes.push(node).map_err(|_| TreeError::Memory)?;
            resources.read_event(super::super::tree::directory::NativeReadEvent::CopiedBytes(
                std::mem::size_of::<NodeId>() as u64,
            ))?;
        }
        self.explicit_nodes = Some(nodes);
        Ok(())
    }

    pub(super) fn scan<'lease>(
        &mut self,
        lease: &NativeReadLease,
        source: &NativeQuerySource<'lease, 'm, 'g>,
        catalog: &NativeCatalog<'_, 'm, 'g>,
        output: &mut [NodeId],
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(usize, CursorState), TreeError> {
        let result = self.scan_inner(lease, source, catalog, output, runtime);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    pub(crate) fn select_folder(
        &mut self,
        candidates: crate::lifecycle::native_graph::documents::FolderCandidates<'m, 'g>,
    ) {
        self.folder = Some(candidates);
    }

    fn scan_inner<'lease>(
        &mut self,
        lease: &NativeReadLease,
        source: &NativeQuerySource<'lease, 'm, 'g>,
        catalog: &NativeCatalog<'_, 'm, 'g>,
        output: &mut [NodeId],
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(usize, CursorState), TreeError> {
        lease
            .check_active()
            .map_err(RuntimeError::Value)
            .map_err(TreeError::Runtime)?;
        if self.failed {
            return Err(TreeError::Invalid("node cursor previously failed"));
        }
        if self.view_token != lease.token()
            || self.runtime != runtime.identity()
            || !std::ptr::eq(self.memory, runtime.memory())
            || !std::ptr::eq(runtime.view(), lease.query_view())
        {
            return Err(TreeError::Invalid("node cursor owner mismatch"));
        }
        runtime.checkpoint().map_err(TreeError::Runtime)?;
        if output.is_empty() || output.len() > 256 {
            return Err(TreeError::Invalid("node scan capacity must be 1..=256"));
        }
        if self.exhausted {
            return Ok((0, CursorState::Done));
        }
        let mut private = QueryArena::new(self.memory, output.len())
            .map_err(RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        let mut resources = TreeResources::for_query(runtime)?;
        if !self.graph_exhausted {
            let count = if self.incident_sources {
                scan_incident_sources_after(
                    lease,
                    source,
                    catalog,
                    self.after,
                    &mut private,
                    &mut resources,
                )?
            } else {
                scan_live_nodes_after(
                    source,
                    lease.bundle().roots(),
                    self.after,
                    self.labels.as_slice(),
                    catalog,
                    lease.bundle().document(),
                    &mut private,
                    &mut resources,
                )?
            };
            self.graph_exhausted = count < output.len();
            if let Some(last) = private.as_slice().last() {
                self.after = Some(*last);
            }
        }
        // Graph records come first. The physical document cursor then supplies
        // only implicit nodes; adopted records and tombstones were handled above.
        if self.graph_exhausted {
            if self.full_drain && self.documents.is_some() && self.explicit_nodes.is_none() {
                self.retain_explicit_nodes(lease, source, catalog, &mut resources)?;
            }
            while private.len() < private.capacity() {
                let Some(cursor) = &mut self.documents else {
                    break;
                };
                let version = match &mut self.folder {
                    Some(folder) => folder.next(lease, &mut resources)?,
                    None => lease.next_document(cursor, &mut resources)?,
                };
                let Some(version) = version else {
                    self.documents = None;
                    break;
                };
                let node = NodeId::from(version.doc_id());
                let implicit = if let Some(nodes) = &self.explicit_nodes {
                    resources.step(1)?;
                    nodes
                        .as_slice()
                        .binary_search_by_key(&node.get(), |id| id.get())
                        .is_err()
                } else {
                    lookup_node_state(
                        source,
                        lease.bundle().roots(),
                        node,
                        catalog,
                        lease.bundle().document(),
                        &mut resources,
                    )?
                    .is_none()
                };
                if implicit {
                    private.push(node).map_err(|_| TreeError::Memory)?;
                }
            }
        }
        drop(resources);
        let count = private.len();
        let state = if self.graph_exhausted && self.documents.is_none() {
            self.exhausted = true;
            CursorState::Done
        } else {
            CursorState::More
        };
        let bytes = count
            .checked_mul(std::mem::size_of::<NodeId>())
            .ok_or(TreeError::Work)?;
        runtime
            .charge(WorkKind::CopiedBytes, bytes as u64)
            .map_err(TreeError::Runtime)?;
        output
            .get_mut(..count)
            .ok_or(TreeError::Memory)?
            .copy_from_slice(private.as_slice());
        Ok((count, state))
    }
}

pub(crate) struct ExpandCursor<'view, 'm, 'g> {
    view_token: u64,
    runtime: RuntimeInstanceId,
    memory: &'m QueryMemory<'g>,
    node: NodeId,
    direction: DirectionSelection,
    types: Option<QueryArena<'m, 'g, RelTypeId>>,
    type_index: usize,
    phase: Direction,
    resume: Option<super::super::adjacency::ExpansionResume>,
    exhausted: bool,
    failed: bool,
    _view: PhantomData<&'view ()>,
    _charge: QueryReservation<'m, 'g>,
}

impl<'view, 'm, 'g> ExpandCursor<'view, 'm, 'g> {
    pub(super) fn new<'lease>(
        lease: &NativeReadLease,
        node: NodeId,
        direction: DirectionSelection,
        selection: RelationshipTypeSelection<'_>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Self, TreeError> {
        lease
            .check_active()
            .map_err(RuntimeError::Value)
            .map_err(TreeError::Runtime)?;
        runtime.checkpoint().map_err(TreeError::Runtime)?;
        if !std::ptr::eq(runtime.view(), lease.query_view()) {
            return Err(TreeError::Invalid("foreign native read view"));
        }
        let memory = runtime.memory();
        let charge = memory
            .reserve(std::mem::size_of::<Self>())
            .map_err(RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        let types = retained_types(memory, selection)?;
        let exhausted = types.as_ref().is_some_and(|values| values.is_empty());
        Ok(Self {
            view_token: lease.token(),
            runtime: runtime.identity(),
            memory,
            node,
            direction,
            types,
            type_index: 0,
            phase: match direction {
                DirectionSelection::In => Direction::In,
                DirectionSelection::Out | DirectionSelection::Undirected => Direction::Out,
            },
            resume: None,
            exhausted,
            failed: false,
            _view: PhantomData,
            _charge: charge,
        })
    }

    pub(super) fn scan<'lease>(
        &mut self,
        lease: &NativeReadLease,
        source: &NativeQuerySource<'lease, 'm, 'g>,
        catalog: &NativeCatalog<'_, 'm, 'g>,
        output: &mut [RelationshipRow],
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(usize, CursorState), TreeError> {
        let result = self.scan_inner(lease, source, catalog, output, runtime);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn scan_inner<'lease>(
        &mut self,
        lease: &NativeReadLease,
        source: &NativeQuerySource<'lease, 'm, 'g>,
        catalog: &NativeCatalog<'_, 'm, 'g>,
        output: &mut [RelationshipRow],
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(usize, CursorState), TreeError> {
        lease
            .check_active()
            .map_err(RuntimeError::Value)
            .map_err(TreeError::Runtime)?;
        if self.failed {
            return Err(TreeError::Invalid("expansion cursor previously failed"));
        }
        if self.view_token != lease.token()
            || self.runtime != runtime.identity()
            || !std::ptr::eq(self.memory, runtime.memory())
            || !std::ptr::eq(runtime.view(), lease.query_view())
        {
            return Err(TreeError::Invalid("expansion cursor owner mismatch"));
        }
        runtime.checkpoint().map_err(TreeError::Runtime)?;
        if output.is_empty() || output.len() > 256 {
            return Err(TreeError::Invalid("expansion capacity must be 1..=256"));
        }
        if self.exhausted {
            return Ok((0, CursorState::Done));
        }
        let mut private = QueryArena::new(self.memory, output.len())
            .map_err(RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        while private.len() < output.len() && !self.exhausted {
            let available = output.len() - private.len();
            let mut adjacent = QueryArena::new(self.memory, available)
                .map_err(RuntimeError::Memory)
                .map_err(TreeError::Runtime)?;
            let mut resources = TreeResources::for_query(runtime)?;
            let mut scratch = RangeScratch::for_query(self.memory, &mut resources)?;
            let relationship_type = self
                .types
                .as_ref()
                .and_then(|types| types.as_slice().get(self.type_index))
                .copied();
            let reader = NativeGraphReader::new(
                source,
                lease.bundle().roots(),
                lease.bundle().sequence(),
                catalog,
                lease.bundle().document(),
            );
            let (_count, resume) = reader.expand_from(
                AdjacencyQuery {
                    node: self.node,
                    direction: self.phase,
                    relationship_type,
                    relationships: RelationshipRange {
                        lower: RelId::new(1)
                            .map_err(|_| TreeError::Invalid("minimum relationship identity"))?,
                        upper: UpperBound::Infinity,
                    },
                },
                self.resume,
                &mut adjacent,
                &mut scratch,
                &mut resources,
            )?;
            for row in adjacent.as_slice() {
                if self.phase == Direction::In
                    && self.direction == DirectionSelection::Undirected
                    && row.edge.neighbor == self.node
                {
                    continue;
                }
                let relationship = reader
                    .relationship(row.edge.rel, &mut resources)?
                    .ok_or(TreeError::Invalid("visible adjacency relationship missing"))?;
                private.push(relationship).map_err(|_| TreeError::Memory)?;
            }
            self.resume = resume;
            drop(scratch);
            drop(resources);
            if self.resume.is_none() {
                self.advance_query();
            }
        }
        let count = private.len();
        let bytes = count
            .checked_mul(std::mem::size_of::<RelationshipRow>())
            .ok_or(TreeError::Work)?;
        runtime
            .charge(WorkKind::CopiedBytes, bytes as u64)
            .map_err(TreeError::Runtime)?;
        output
            .get_mut(..count)
            .ok_or(TreeError::Memory)?
            .copy_from_slice(private.as_slice());
        Ok((
            count,
            if self.exhausted {
                CursorState::Done
            } else {
                CursorState::More
            },
        ))
    }

    fn advance_query(&mut self) {
        self.resume = None;
        if let Some(types) = self.types.as_ref()
            && self.type_index + 1 < types.len()
        {
            self.type_index += 1;
            return;
        }
        self.type_index = 0;
        if self.direction == DirectionSelection::Undirected && self.phase == Direction::Out {
            self.phase = Direction::In;
        } else {
            self.exhausted = true;
        }
    }
}

fn retained_types<'m, 'g>(
    memory: &'m QueryMemory<'g>,
    selection: RelationshipTypeSelection<'_>,
) -> Result<Option<QueryArena<'m, 'g, RelTypeId>>, TreeError> {
    match selection {
        RelationshipTypeSelection::All => Ok(None),
        RelationshipTypeSelection::Any(values) => {
            let mut retained = QueryArena::new(memory, values.len())
                .map_err(RuntimeError::Memory)
                .map_err(TreeError::Runtime)?;
            for value in values {
                if !retained.as_slice().contains(value) {
                    retained
                        .push(*value)
                        .map_err(RuntimeError::Memory)
                        .map_err(TreeError::Runtime)?;
                }
            }
            retained
                .as_mut_slice()
                .sort_unstable_by_key(|value| value.get());
            Ok(Some(retained))
        }
    }
}

impl<'view, 'm, 'g> RelCursor<'view, 'm, 'g> {
    pub(super) fn new<'lease>(
        lease: &NativeReadLease,
        selection: RelationshipTypeSelection<'_>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Self, TreeError> {
        lease
            .check_active()
            .map_err(RuntimeError::Value)
            .map_err(TreeError::Runtime)?;
        runtime.checkpoint().map_err(TreeError::Runtime)?;
        if !std::ptr::eq(runtime.view(), lease.query_view()) {
            return Err(TreeError::Invalid("foreign native read view"));
        }
        let memory = runtime.memory();
        let charge = memory
            .reserve(std::mem::size_of::<Self>())
            .map_err(RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        let types = retained_types(memory, selection)?;
        Ok(Self {
            view_token: lease.token(),
            runtime: runtime.identity(),
            memory,
            types,
            after: None,
            exhausted: false,
            failed: false,
            _view: PhantomData,
            _charge: charge,
        })
    }

    pub(super) fn scan<'lease>(
        &mut self,
        lease: &NativeReadLease,
        source: &NativeQuerySource<'lease, 'm, 'g>,
        catalog: &NativeCatalog<'_, 'm, 'g>,
        output: &mut [RelationshipRow],
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(usize, CursorState), TreeError> {
        let result = self.scan_inner(lease, source, catalog, output, runtime);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn scan_inner<'lease>(
        &mut self,
        lease: &NativeReadLease,
        source: &NativeQuerySource<'lease, 'm, 'g>,
        catalog: &NativeCatalog<'_, 'm, 'g>,
        output: &mut [RelationshipRow],
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(usize, CursorState), TreeError> {
        lease
            .check_active()
            .map_err(RuntimeError::Value)
            .map_err(TreeError::Runtime)?;
        if self.failed {
            return Err(TreeError::Invalid("relationship cursor previously failed"));
        }
        if self.view_token != lease.token()
            || self.runtime != runtime.identity()
            || !std::ptr::eq(self.memory, runtime.memory())
            || !std::ptr::eq(runtime.view(), lease.query_view())
        {
            return Err(TreeError::Invalid("relationship cursor owner mismatch"));
        }
        runtime.checkpoint().map_err(TreeError::Runtime)?;
        if output.is_empty() || output.len() > 256 {
            return Err(TreeError::Invalid(
                "relationship scan capacity must be 1..=256",
            ));
        }
        if self.exhausted {
            return Ok((0, CursorState::Done));
        }
        let mut private = QueryArena::new(self.memory, output.len())
            .map_err(RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        let mut resources = TreeResources::for_query(runtime)?;
        let reader = NativeGraphReader::new(
            source,
            lease.bundle().roots(),
            lease.bundle().sequence(),
            catalog,
            lease.bundle().document(),
        );
        let count = reader.scan_relationships_after(
            self.after,
            self.types.as_ref().map(QueryArena::as_slice),
            &mut private,
            &mut resources,
        )?;
        drop(resources);
        let state = if count < output.len() {
            self.exhausted = true;
            CursorState::Done
        } else {
            CursorState::More
        };
        let bytes = count
            .checked_mul(std::mem::size_of::<RelationshipRow>())
            .ok_or(TreeError::Work)?;
        runtime
            .charge(WorkKind::CopiedBytes, bytes as u64)
            .map_err(TreeError::Runtime)?;
        output
            .get_mut(..count)
            .ok_or(TreeError::Memory)?
            .copy_from_slice(private.as_slice());
        if let Some(last) = private.as_slice().last() {
            self.after = Some(last.rel);
        }
        Ok((count, state))
    }
}

/// OutRanges order is numeric source/type/lower. Seek after the entire source,
/// including all its types and split ranges; descriptors are candidates only.
fn scan_incident_sources_after<'lease, 'm, 'g>(
    lease: &NativeReadLease,
    source: &NativeQuerySource<'lease, 'm, 'g>,
    catalog: &NativeCatalog<'_, 'm, 'g>,
    mut after: Option<NodeId>,
    output: &mut QueryArena<'m, 'g, NodeId>,
    resources: &mut TreeResources<'_>,
) -> Result<usize, TreeError> {
    use crate::property_graph::storage::adjacency::{RANGE_DESCRIPTOR_BYTES, RangeDescriptor};
    use crate::property_graph::storage::records::NodeRecordState;
    use crate::property_graph::storage::tree::{Key, TreeKind, directory::DirectoryCursor};
    let roots = lease.bundle().roots();
    let root = roots.directory(TreeKind::OutRanges)?;
    while output.len() < output.capacity() {
        let lower = match after {
            Some(node) => {
                let Some(next) = node.get().checked_add(1) else {
                    break;
                };
                let mut key = [0_u8; 40];
                key.get_mut(..16)
                    .ok_or(TreeError::Memory)?
                    .copy_from_slice(&next.to_le_bytes());
                key.get_mut(16..24)
                    .ok_or(TreeError::Memory)?
                    .copy_from_slice(&1_u64.to_le_bytes());
                key.get_mut(24..)
                    .ok_or(TreeError::Memory)?
                    .copy_from_slice(&1_u128.to_le_bytes());
                Some(key)
            }
            None => None,
        };
        let mut cursor = DirectoryCursor::seek(
            source,
            root,
            lower.as_ref().map(<[u8; 40]>::as_slice),
            resources,
        )?;
        let Some(entry) = cursor.next_entry(resources)? else {
            break;
        };
        entry.require_root(root)?;
        let Key::Inline(key) = entry.key() else {
            return Err(TreeError::Invalid("overflow adjacency key"));
        };
        resources.step((40 + RANGE_DESCRIPTOR_BYTES) as u64)?;
        let node = RangeDescriptor::decode(root.kind(), key, entry.value())?
            .key()
            .node;
        after = Some(node);
        let state = lookup_node_state(
            source,
            roots,
            node,
            catalog,
            lease.bundle().document(),
            resources,
        )?
        .ok_or(TreeError::Missing)?;
        if matches!(state, NodeRecordState::Live(_)) {
            output.push(node).map_err(|_| TreeError::Memory)?;
        }
    }
    Ok(output.len())
}
