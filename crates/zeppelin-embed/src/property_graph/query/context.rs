//! Allocation-free value work and admitted-view identity seams.
use super::{QueryError, QueryValue};
use crate::lifecycle::{QueryControl, QueryError as ControlError};
use crate::property_graph::{GraphGeneration, NodeId, RelId, StoreInstanceId};

/// Maximum expression/value work per query; a caller may only tighten it.
pub const MAX_VALUE_WORK: u64 = 8_000_000;
/// Complete query construction ceiling; borrowed descriptors report their span.
pub const MAX_QUERY_BYTES: usize = 24 * 1024 * 1024;
/// One nonzero-sized view token owned by the eventual admission/lease.
/// Constructing it proves no entity exists and acquires no store lease.
#[derive(Debug)]
pub struct QueryView {
    store: StoreInstanceId,
    generation: GraphGeneration,
}
impl QueryView {
    /// Creates a distinct token even if another admission has identical metadata.
    pub const fn new(store: StoreInstanceId, generation: GraphGeneration) -> Self {
        Self { store, generation }
    }
    /// Binds a typed identity to this token; storage still proves liveness.
    pub const fn node(&self, id: NodeId) -> QueryValue<'_> {
        QueryValue::NodeRef(QueryNodeRef { id, view: self })
    }
    /// Binds a distinct relationship identity, never a node ID conversion.
    pub const fn relationship(&self, id: RelId) -> QueryValue<'_> {
        QueryValue::RelRef(QueryRelRef { id, view: self })
    }
    /// Store incarnation for admission bookkeeping.
    pub const fn store(&self) -> StoreInstanceId {
        self.store
    }
    /// Admitted generation metadata, not a substitute for token identity.
    pub const fn generation(&self) -> GraphGeneration {
        self.generation
    }
}
/// Mandatory caller control and cumulative work for value operations.
/// Owns no buffers; caller/lease backing and later runtime reservations remain
/// with their owners. It does not create a separate store memory allowance.
pub struct ValueContext<'a> {
    pub(super) view: &'a QueryView,
    control: &'a QueryControl,
    limit: u64,
    work: u64,
    produced_bytes: u64,
    retained: Option<&'a dyn super::runtime::RetainedView>,
}
impl<'a> ValueContext<'a> {
    /// The original caller control. This conveys no retained-view authority.
    /// Compiler preparation can share this exact control without constructing a
    /// second value context or a separate cancellation/deadline policy.
    pub const fn control(&self) -> &'a QueryControl {
        self.control
    }
    /// Original non-owning retained adapter, when supplied by runtime admission.
    /// Borrowing this adapter acquires no view or reservation and cannot extend
    /// its original lifetime. Compiler checkpoints use it before caller control.
    pub const fn retained_view(&self) -> Option<&'a dyn super::runtime::RetainedView> {
        self.retained
    }
    /// Uses the existing deadline/cancellation mechanism and a tightened cap.
    pub fn new(
        view: &'a QueryView,
        control: &'a QueryControl,
        limit: u64,
    ) -> Result<Self, QueryError> {
        if limit > MAX_VALUE_WORK {
            return Err(QueryError::WorkLimit);
        }
        let result = Self {
            view,
            control,
            limit,
            work: 0,
            produced_bytes: 0,
            retained: None,
        };
        result.checkpoint()?;
        Ok(result)
    }
    pub(super) fn retained(
        view: &'a dyn super::runtime::RetainedView,
        control: &'a QueryControl,
        limit: u64,
    ) -> Result<Self, QueryError> {
        view.check_active()?;
        let mut result = Self::new(view.query_view(), control, limit)?;
        result.retained = Some(view);
        result.checkpoint()?;
        Ok(result)
    }
    /// Actual value/descriptor/chunk units consumed, checked before each unit.
    pub const fn work(&self) -> u64 {
        self.work
    }
    /// Bytes actually written into reserved expression-output buffers. This is
    /// cumulative work, not peak owned capacity or completed-result accounting.
    pub const fn produced_bytes(&self) -> u64 {
        self.produced_bytes
    }
    pub(super) fn output(&mut self, bytes: u64) -> Result<(), QueryError> {
        self.produced_bytes = self
            .produced_bytes
            .checked_add(bytes)
            .ok_or(QueryError::WorkLimit)?;
        Ok(())
    }
    pub(super) fn step(&mut self) -> Result<(), QueryError> {
        self.checkpoint()?;
        if self.work >= self.limit {
            return Err(QueryError::WorkLimit);
        }
        self.work += 1;
        Ok(())
    }
    pub(super) fn checkpoint(&self) -> Result<(), QueryError> {
        if let Some(retained) = self.retained {
            retained.check_active()?;
            if !std::ptr::eq(retained.query_view(), self.view) {
                return Err(QueryError::ForeignView);
            }
        }
        self.control.checkpoint().map_err(|error| match error {
            ControlError::Timeout { .. } => QueryError::Timeout,
            ControlError::Cancelled { .. } => QueryError::Cancelled,
            _ => QueryError::Control,
        })
    }
}

/// Intermediate same-view node identity; no projected properties or offsets.
#[derive(Clone, Copy, Debug)]
pub struct QueryNodeRef<'a> {
    pub(super) id: NodeId,
    pub(super) view: &'a QueryView,
}
impl QueryNodeRef<'_> {
    /// Full store-local node identity.
    pub const fn id(self) -> NodeId {
        self.id
    }
}
/// Intermediate same-view relationship identity, distinct from node references.
#[derive(Clone, Copy, Debug)]
pub struct QueryRelRef<'a> {
    pub(super) id: RelId,
    pub(super) view: &'a QueryView,
}
impl QueryRelRef<'_> {
    /// Full store-local relationship identity.
    pub const fn id(self) -> RelId {
        self.id
    }
}
