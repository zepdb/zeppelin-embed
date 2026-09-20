//! Opaque resumable high-level native read cursors.

use super::{NativeCatalog, NativeQuerySource, scan_live_nodes_after};
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
            labels,
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
        output: &mut [NodeId],
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
        let count = scan_live_nodes_after(
            source,
            lease.bundle().roots(),
            self.after,
            self.labels.as_slice(),
            catalog,
            lease.bundle().document(),
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
            .checked_mul(std::mem::size_of::<NodeId>())
            .ok_or(TreeError::Work)?;
        runtime
            .charge(WorkKind::CopiedBytes, bytes as u64)
            .map_err(TreeError::Runtime)?;
        output
            .get_mut(..count)
            .ok_or(TreeError::Memory)?
            .copy_from_slice(private.as_slice());
        if let Some(last) = private.as_slice().last() {
            self.after = Some(*last);
        }
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
    skipped: u64,
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
            skipped: 0,
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
            let (count, more) = reader.expand_after_skip(
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
                self.skipped,
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
            self.skipped = self
                .skipped
                .checked_add(count as u64)
                .ok_or(TreeError::Work)?;
            drop(scratch);
            drop(resources);
            if !more {
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
        self.skipped = 0;
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
