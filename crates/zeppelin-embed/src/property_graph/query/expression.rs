//! Crate-private scalar evaluation over one admitted native query owner.

#![allow(
    dead_code,
    reason = "ZE-145's crate-private evaluator is consumed by the later ZE-50/51 integration"
)]

use super::list::ListArena;
use super::plan::{
    BinaryExpression, ExprId, Expression, Literal, MAX_PLAN_DEPTH, ParameterBinding,
    PlanDescription, PlanError, UnaryExpression,
};
use super::relational::Schema;
use super::resources::{MemoryError, QueryArena, QueryMemory, RuntimePlan};
use super::runtime::{RowBatch, RuntimeContext, RuntimeError, WorkKind};
use super::{
    MAX_LIST_DEPTH, MAX_LIST_ELEMENTS, MAX_QUERY_BYTES, QueryList, QueryValue, QueryView, Truth,
};
use crate::property_graph::catalog::{Symbol, SymbolKind};
use crate::property_graph::staging::{
    BatchEntityRef, GraphBatchReadView, StageError, WriteControl,
};
use crate::property_graph::storage::payload::CHUNK_BYTES;
use crate::property_graph::storage::stream::{PayloadCursor, PayloadSlice};
use crate::property_graph::storage::tree::directory::{
    BlockSource, NativeReadEvent, TreeError, TreeResources,
};
use crate::property_graph::storage::{GraphReadView, TextPayloadReader};
use crate::property_graph::{
    GraphName, NodeId, NodeRef, PropertyData, PropertyValue, RelId, RelRef,
};
use std::marker::PhantomData;

#[cfg(any(test, feature = "test-seams"))]
#[derive(Default)]
struct TestPollControl {
    remaining: Option<usize>,
    cancel: Option<crate::lifecycle::CancelToken>,
}

#[cfg(not(any(test, feature = "test-seams")))]
#[derive(Default)]
struct TestPollControl {}

impl TestPollControl {
    #[cfg(any(test, feature = "test-seams"))]
    fn arm_cancel(&mut self, polls: usize, cancel: crate::lifecycle::CancelToken) {
        self.remaining = Some(polls);
        self.cancel = Some(cancel);
    }

    fn poll(&mut self) {
        #[cfg(any(test, feature = "test-seams"))]
        if let Some(remaining) = &mut self.remaining {
            if *remaining == 0 {
                if let Some(cancel) = self.cancel.take() {
                    cancel.cancel();
                }
                self.remaining = None;
            } else {
                *remaining -= 1;
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ExpressionCapacity {
    pub(crate) cells: usize,
    pub(crate) string_bytes: usize,
}

#[derive(Clone, Copy)]
enum ScratchCell {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    String {
        start: usize,
        len: usize,
    },
    Node(NodeId),
    Relationship(RelId),
    List {
        start: usize,
        len: usize,
        elements: usize,
        depth: u8,
        bytes: usize,
        entities: bool,
    },
}

#[derive(Debug)]
pub(crate) enum ExpressionFailure {
    Runtime(RuntimeError),
    Plan(PlanError),
    Tree(TreeError),
    Stage(StageError),
}

impl std::fmt::Display for ExpressionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Runtime(error) => error.fmt(formatter),
            Self::Plan(error) => error.fmt(formatter),
            Self::Tree(error) => error.fmt(formatter),
            Self::Stage(error) => error.fmt(formatter),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ExpressionError {
    pub(crate) expression: ExprId,
    pub(crate) failure: ExpressionFailure,
}

impl std::fmt::Display for ExpressionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "graph expression {} failed: {}",
            self.expression.0, self.failure
        )
    }
}

impl std::error::Error for ExpressionError {}

impl From<RuntimeError> for ExpressionFailure {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

impl From<PlanError> for ExpressionFailure {
    fn from(error: PlanError) -> Self {
        Self::Plan(error)
    }
}

impl From<TreeError> for ExpressionFailure {
    fn from(error: TreeError) -> Self {
        Self::Tree(error)
    }
}

impl From<StageError> for ExpressionFailure {
    fn from(error: StageError) -> Self {
        Self::Stage(error)
    }
}

/// The uncommitted writes an earlier clause of this same statement staged.
/// Reads consult it before the admitted base so a clause sees its predecessors.
pub(crate) struct ClauseOverlay<'o, 'c, 'a, 'batch> {
    batch: &'o mut GraphBatchReadView<'a, 'batch>,
    control: &'o mut WriteControl<'c>,
}

impl<'o, 'c, 'a, 'batch> ClauseOverlay<'o, 'c, 'a, 'batch> {
    /// Borrows one progressive write overlay and the checkpoint that bounds it.
    pub(crate) fn new(
        batch: &'o mut GraphBatchReadView<'a, 'batch>,
        control: &'o mut WriteControl<'c>,
    ) -> Self {
        Self { batch, control }
    }

    fn property(
        &mut self,
        target: BatchEntityRef<'batch>,
        name: GraphName<'_>,
    ) -> Result<Option<PropertyValue<'a>>, StageError> {
        self.batch.property(target, name, self.control)
    }

    fn stored_text(&mut self, node: NodeId) -> Result<Option<&'a str>, StageError> {
        self.batch
            .stored_text(NodeRef::Existing(node), self.control)
    }

    fn labels(&mut self, node: NodeId) -> Result<Option<&'a [GraphName<'a>]>, StageError> {
        self.batch
            .pending_labels(NodeRef::Existing(node), self.control)
    }

    fn relationship_type(
        &mut self,
        relationship: RelId,
    ) -> Result<Option<GraphName<'a>>, StageError> {
        self.batch
            .pending_relationship_type(RelRef::Existing(relationship), self.control)
    }
}

impl From<super::QueryError> for ExpressionFailure {
    fn from(error: super::QueryError) -> Self {
        RuntimeError::Value(error).into()
    }
}

impl From<MemoryError> for ExpressionFailure {
    fn from(error: MemoryError) -> Self {
        RuntimeError::Memory(error).into()
    }
}

struct ScratchArenas<'v, 'm, 'g> {
    view: &'v QueryView,
    cells: QueryArena<'m, 'g, ScratchCell>,
    bytes: QueryArena<'m, 'g, u8>,
}

impl std::fmt::Debug for ScratchArenas<'_, '_, '_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeExpressionScratch")
            .finish_non_exhaustive()
    }
}

impl ListArena for ScratchArenas<'_, '_, '_> {
    fn value(&self, index: usize) -> Option<QueryValue<'_>> {
        self.cells
            .as_slice()
            .get(index)
            .copied()
            .and_then(|cell| self.value(cell))
    }
}

impl<'v, 'm, 'g> ScratchArenas<'v, 'm, 'g> {
    fn value<'a>(&'a self, cell: ScratchCell) -> Option<QueryValue<'a>> {
        Some(match cell {
            ScratchCell::Null => QueryValue::Null,
            ScratchCell::Bool(value) => QueryValue::Bool(value),
            ScratchCell::I64(value) => QueryValue::I64(value),
            ScratchCell::F64(value) => QueryValue::F64(value),
            ScratchCell::Node(id) => self.view.node(id),
            ScratchCell::Relationship(id) => self.view.relationship(id),
            ScratchCell::String { start, len } => {
                let bytes = self.bytes.as_slice().get(start..start.checked_add(len)?)?;
                // SAFETY: every String cell is installed only after copying an
                // already validated UTF-8 value or a canonical UTF-8 field.
                QueryValue::String(unsafe { std::str::from_utf8_unchecked(bytes) })
            }
            ScratchCell::List {
                start,
                len,
                elements,
                depth,
                bytes,
                entities,
            } => QueryValue::List(QueryList::arena(
                self,
                start,
                len,
                elements,
                depth,
                bytes,
                entities.then_some(self.view),
            )),
        })
    }

    fn truncate(&mut self, cells: usize, bytes: usize) {
        self.cells.truncate(cells);
        self.bytes.truncate(bytes);
    }

    fn copy_value(
        &mut self,
        value: QueryValue<'_>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        test_poll: &mut TestPollControl,
        depth: u8,
    ) -> Result<ScratchCell, ExpressionFailure> {
        context.values().step()?;
        value.validate(context.values())?;
        Ok(match value {
            QueryValue::Null => ScratchCell::Null,
            QueryValue::Bool(value) => {
                context.charge(WorkKind::CopiedBytes, 1)?;
                ScratchCell::Bool(value)
            }
            QueryValue::I64(value) => {
                context.charge(WorkKind::CopiedBytes, 8)?;
                ScratchCell::I64(value)
            }
            QueryValue::F64(value) => {
                context.charge(WorkKind::CopiedBytes, 8)?;
                ScratchCell::F64(value)
            }
            QueryValue::NodeRef(value) => {
                context.charge(WorkKind::CopiedBytes, 16)?;
                ScratchCell::Node(value.id())
            }
            QueryValue::RelRef(value) => {
                context.charge(WorkKind::CopiedBytes, 16)?;
                ScratchCell::Relationship(value.id())
            }
            QueryValue::String(value) => self.copy_bytes(value.as_bytes(), context, test_poll)?,
            QueryValue::List(list) => {
                if depth >= MAX_LIST_DEPTH {
                    return Err(super::QueryError::ListLimit.into());
                }
                let start = self.reserve_list(list.len(), || {
                    test_poll.poll();
                    context.values().step()?;
                    Ok(())
                })?;
                for index in 0..list.len() {
                    let child = self.copy_value(
                        list.get(index).ok_or(super::QueryError::ListLimit)?,
                        context,
                        test_poll,
                        depth + 1,
                    )?;
                    *self
                        .cells
                        .as_mut_slice()
                        .get_mut(start + index)
                        .ok_or(RuntimeError::Batch)? = child;
                }
                self.finish_list(start, list.len(), || {
                    test_poll.poll();
                    context.values().step()?;
                    Ok(())
                })?
            }
        })
    }

    fn copy_bytes(
        &mut self,
        value: &[u8],
        context: &mut RuntimeContext<'v, 'm, 'g>,
        test_poll: &mut TestPollControl,
    ) -> Result<ScratchCell, ExpressionFailure> {
        self.check_bytes(value.len())?;
        let start = self.bytes.len();
        for chunk in value.chunks(CHUNK_BYTES) {
            test_poll.poll();
            context.checkpoint()?;
            context.charge(WorkKind::CopiedBytes, chunk.len() as u64)?;
            self.bytes.extend_copy(chunk)?;
        }
        Ok(ScratchCell::String {
            start,
            len: value.len(),
        })
    }

    fn copy_native_bytes(
        &mut self,
        value: &[u8],
        resources: &mut TreeResources<'_>,
        test_poll: &mut TestPollControl,
    ) -> Result<ScratchCell, ExpressionFailure> {
        self.check_bytes(value.len())?;
        let start = self.bytes.len();
        for chunk in value.chunks(CHUNK_BYTES) {
            test_poll.poll();
            resources.step(1)?;
            resources.read_event(NativeReadEvent::CopiedBytes(chunk.len() as u64))?;
            self.bytes.extend_copy(chunk)?;
        }
        Ok(ScratchCell::String {
            start,
            len: value.len(),
        })
    }

    fn copy_payload<S: BlockSource>(
        &mut self,
        value: PayloadSlice<'_, S>,
        resources: &mut TreeResources<'_>,
        test_poll: &mut TestPollControl,
    ) -> Result<ScratchCell, ExpressionFailure> {
        let len = usize::try_from(value.len()).map_err(|_| RuntimeError::Batch)?;
        self.check_bytes(len)?;
        let start = self.bytes.len();
        let mut copied = 0_usize;
        let mut buffer = [0_u8; CHUNK_BYTES];
        while copied < len {
            test_poll.poll();
            let count = (len - copied).min(buffer.len());
            let output = buffer.get_mut(..count).ok_or(RuntimeError::Batch)?;
            if value.read_at(copied as u64, output, resources)? != count {
                return Err(TreeError::Invalid("short expression payload copy").into());
            }
            resources.read_event(NativeReadEvent::CopiedBytes(count as u64))?;
            self.bytes.extend_copy(output)?;
            copied += count;
        }
        Ok(ScratchCell::String { start, len })
    }

    fn copy_text_reader<S: BlockSource>(
        &mut self,
        value: &TextPayloadReader<'_, S>,
        resources: &mut TreeResources<'_>,
        test_poll: &mut TestPollControl,
    ) -> Result<ScratchCell, ExpressionFailure> {
        let len = usize::try_from(value.len()).map_err(|_| RuntimeError::Batch)?;
        self.check_bytes(len)?;
        let start = self.bytes.len();
        let mut copied = 0_usize;
        let mut buffer = [0_u8; CHUNK_BYTES];
        while copied < len {
            test_poll.poll();
            let count = (len - copied).min(buffer.len());
            let output = buffer.get_mut(..count).ok_or(RuntimeError::Batch)?;
            if value.read_at(copied as u64, output, resources)? != count {
                return Err(TreeError::Invalid("short stored-text expression copy").into());
            }
            resources.read_event(NativeReadEvent::CopiedBytes(count as u64))?;
            self.bytes.extend_copy(output)?;
            copied += count;
        }
        Ok(ScratchCell::String { start, len })
    }

    fn check_bytes(&self, additional: usize) -> Result<(), ExpressionFailure> {
        if self
            .bytes
            .len()
            .checked_add(additional)
            .is_none_or(|length| length > self.bytes.capacity())
        {
            return Err(RuntimeError::Batch.into());
        }
        Ok(())
    }

    fn reserve_list(
        &mut self,
        len: usize,
        mut step: impl FnMut() -> Result<(), ExpressionFailure>,
    ) -> Result<usize, ExpressionFailure> {
        if self
            .cells
            .len()
            .checked_add(len)
            .is_none_or(|length| length > self.cells.capacity())
        {
            return Err(RuntimeError::Batch.into());
        }
        let start = self.cells.len();
        for _ in 0..len {
            step()?;
            self.cells.push(ScratchCell::Null)?;
        }
        Ok(start)
    }

    fn finish_list(
        &self,
        start: usize,
        len: usize,
        mut step: impl FnMut() -> Result<(), ExpressionFailure>,
    ) -> Result<ScratchCell, ExpressionFailure> {
        let values = self
            .cells
            .as_slice()
            .get(start..start.checked_add(len).ok_or(RuntimeError::Batch)?)
            .ok_or(RuntimeError::Batch)?;
        let mut elements = len;
        let mut depth = 1_u8;
        let mut bytes = len
            .checked_mul(std::mem::size_of::<ScratchCell>())
            .ok_or(super::QueryError::ListLimit)?;
        let mut entities = false;
        for value in values {
            step()?;
            match *value {
                ScratchCell::String { len, .. } => {
                    bytes = bytes.checked_add(len).ok_or(super::QueryError::ListLimit)?;
                }
                ScratchCell::Node(_) | ScratchCell::Relationship(_) => entities = true,
                ScratchCell::List {
                    elements: nested_elements,
                    depth: nested_depth,
                    bytes: nested_bytes,
                    entities: nested_entities,
                    ..
                } => {
                    elements = elements
                        .checked_add(nested_elements)
                        .ok_or(super::QueryError::ListLimit)?;
                    depth = depth.max(
                        nested_depth
                            .checked_add(1)
                            .ok_or(super::QueryError::ListLimit)?,
                    );
                    bytes = bytes
                        .checked_add(nested_bytes)
                        .ok_or(super::QueryError::ListLimit)?;
                    entities |= nested_entities;
                }
                ScratchCell::Null
                | ScratchCell::Bool(_)
                | ScratchCell::I64(_)
                | ScratchCell::F64(_) => {}
            }
        }
        if elements > MAX_LIST_ELEMENTS || depth > MAX_LIST_DEPTH || bytes > MAX_QUERY_BYTES {
            return Err(super::QueryError::ListLimit.into());
        }
        Ok(ScratchCell::List {
            start,
            len,
            elements,
            depth,
            bytes,
            entities,
        })
    }

    fn adopt(&self, value: QueryValue<'_>) -> Result<ScratchCell, ExpressionFailure> {
        Ok(match value {
            QueryValue::Null => ScratchCell::Null,
            QueryValue::Bool(value) => ScratchCell::Bool(value),
            QueryValue::I64(value) => ScratchCell::I64(value),
            QueryValue::F64(value) => ScratchCell::F64(value),
            QueryValue::NodeRef(value) => ScratchCell::Node(value.id()),
            QueryValue::RelRef(value) => ScratchCell::Relationship(value.id()),
            QueryValue::String(value) => {
                let base = self.bytes.as_slice().as_ptr() as usize;
                let pointer = value.as_ptr() as usize;
                let start = pointer.checked_sub(base).ok_or(RuntimeError::Batch)?;
                if start
                    .checked_add(value.len())
                    .is_none_or(|end| end > self.bytes.len())
                {
                    return Err(RuntimeError::Batch.into());
                }
                ScratchCell::String {
                    start,
                    len: value.len(),
                }
            }
            QueryValue::List(list) => {
                let (start, len, elements, depth, bytes, entities) =
                    list.arena_descriptor(self).ok_or(RuntimeError::Batch)?;
                ScratchCell::List {
                    start,
                    len,
                    elements,
                    depth,
                    bytes,
                    entities,
                }
            }
        })
    }
}

pub(crate) struct NativeExpressionEvaluator<'r, 'plan, 'v, 'm, 'g> {
    description: PlanDescription<'plan>,
    memory: *const QueryMemory<'g>,
    parameters: QueryArena<'m, 'g, ScratchCell>,
    scratch: ScratchArenas<'v, 'm, 'g>,
    parameter_cells: usize,
    parameter_bytes: usize,
    output: Option<ScratchCell>,
    test_poll: TestPollControl,
    _runtime_plan: PhantomData<&'r ()>,
}

impl<'r, 'plan, 'v, 'm, 'g> NativeExpressionEvaluator<'r, 'plan, 'v, 'm, 'g> {
    pub(crate) fn new<'p, 'facts, 'a>(
        plan: &'r RuntimePlan<'p, 'plan, 'facts, 'm, 'g, 'a>,
        bindings: &[ParameterBinding<'_>],
        capacity: ExpressionCapacity,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<Self, ExpressionFailure> {
        if !plan.belongs_to(context.memory()) {
            return Err(RuntimeError::Batch.into());
        }
        plan.validate_parameter_inputs(bindings, context.values())?;
        let description = plan.plan().description();
        let mut result = Self {
            description,
            memory: context.memory(),
            parameters: QueryArena::new(context.memory(), bindings.len())?,
            scratch: ScratchArenas {
                view: context.view(),
                cells: QueryArena::new(context.memory(), capacity.cells)?,
                bytes: QueryArena::new(context.memory(), capacity.string_bytes)?,
            },
            parameter_cells: 0,
            parameter_bytes: 0,
            output: None,
            test_poll: TestPollControl::default(),
            _runtime_plan: PhantomData,
        };
        for declaration in description.parameters {
            let binding = bindings
                .iter()
                .find(|binding| binding.name == declaration.name)
                .ok_or(PlanError::Parameter)?;
            let value =
                result
                    .scratch
                    .copy_value(binding.value, context, &mut result.test_poll, 0)?;
            result.parameters.push(value)?;
        }
        result.parameter_cells = result.scratch.cells.len();
        result.parameter_bytes = result.scratch.bytes.len();
        Ok(result)
    }

    #[cfg(any(test, feature = "test-seams"))]
    pub(crate) fn cancel_after_scratch_polls(
        &mut self,
        polls: usize,
        cancel: crate::lifecycle::CancelToken,
    ) {
        self.test_poll.arm_cancel(polls, cancel);
    }

    #[allow(
        clippy::result_large_err,
        reason = "keep typed graph errors allocation-free on failure paths"
    )]
    pub(crate) fn evaluate<'a>(
        &'a mut self,
        expression: ExprId,
        schema: &Schema<'_, '_>,
        input: &'a RowBatch<'v, 'm, 'g>,
        row: usize,
        view: &GraphReadView<'_, 'v, 'm, 'g>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<QueryValue<'a>, ExpressionError> {
        self.evaluate_over(expression, schema, input, row, view, None, context)
    }

    /// Evaluates against the writes an earlier clause of this same statement
    /// staged, falling back to `view` for every entity the overlay has not
    /// staged. Scalar evaluation is identical to `evaluate`.
    #[allow(
        clippy::too_many_arguments,
        reason = "the scalar boundary keeps every authentic owner explicit"
    )]
    #[allow(
        clippy::result_large_err,
        reason = "keep typed graph errors allocation-free on failure paths"
    )]
    pub(crate) fn evaluate_with_overlay<'a>(
        &'a mut self,
        expression: ExprId,
        schema: &Schema<'_, '_>,
        input: &'a RowBatch<'v, 'm, 'g>,
        row: usize,
        view: &GraphReadView<'_, 'v, 'm, 'g>,
        overlay: &mut ClauseOverlay<'_, '_, '_, '_>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<QueryValue<'a>, ExpressionError> {
        self.evaluate_over(expression, schema, input, row, view, Some(overlay), context)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the scalar boundary keeps every authentic owner explicit"
    )]
    #[allow(
        clippy::result_large_err,
        reason = "keep typed graph errors allocation-free on failure paths"
    )]
    fn evaluate_over<'a>(
        &'a mut self,
        expression: ExprId,
        schema: &Schema<'_, '_>,
        input: &'a RowBatch<'v, 'm, 'g>,
        row: usize,
        view: &GraphReadView<'_, 'v, 'm, 'g>,
        overlay: Option<&mut ClauseOverlay<'_, '_, '_, '_>>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<QueryValue<'a>, ExpressionError> {
        self.reset();
        let mut borrowed = None;
        let result: Result<ScratchCell, ExpressionFailure> = (|| {
            view.validate_expression_owner(context)?;
            if !std::ptr::eq(self.memory, context.memory())
                || !std::ptr::eq(self.scratch.view, context.view())
                || !input.belongs_to(context)
                || input.columns() != schema.slots().len()
                || row >= input.rows()
            {
                return Err(RuntimeError::Batch.into());
            }
            // A packed node list already has charged, immutable same-view backing.
            // Forward a root slot without expanding it into scratch descriptors.
            if let Some(Expression::Slot(slot)) =
                self.description.expressions.get(expression.0 as usize)
            {
                let value = input
                    .value(row, schema.column(*slot)?)
                    .ok_or(RuntimeError::Batch)?;
                if matches!(value, QueryValue::List(list) if list.node_ids().is_some()) {
                    context.charge(WorkKind::Expressions, 1)?;
                    context.values().step()?;
                    value.validate(context.values())?;
                    borrowed = Some(value);
                    return Ok(ScratchCell::Null);
                }
            }
            self.evaluate_inner(expression, schema, input, row, view, overlay, context, 0)
        })();
        match result {
            Ok(value) => {
                if let Some(value) = borrowed {
                    return Ok(value);
                }
                self.output = Some(value);
                self.scratch.value(value).ok_or(ExpressionError {
                    expression,
                    failure: ExpressionFailure::Runtime(RuntimeError::Batch),
                })
            }
            Err(failure) => {
                self.reset();
                Err(ExpressionError {
                    expression,
                    failure,
                })
            }
        }
    }

    fn reset(&mut self) {
        self.output = None;
        self.scratch
            .truncate(self.parameter_cells, self.parameter_bytes);
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the scalar boundary keeps every authentic owner explicit"
    )]
    fn evaluate_inner(
        &mut self,
        expression_id: ExprId,
        schema: &Schema<'_, '_>,
        input: &RowBatch<'v, 'm, 'g>,
        row: usize,
        view: &GraphReadView<'_, 'v, 'm, 'g>,
        mut overlay: Option<&mut ClauseOverlay<'_, '_, '_, '_>>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        depth: usize,
    ) -> Result<ScratchCell, ExpressionFailure> {
        if depth >= MAX_PLAN_DEPTH {
            return Err(PlanError::Limit.into());
        }
        context.charge(WorkKind::Expressions, 1)?;
        let expression = *self
            .description
            .expressions
            .get(expression_id.0 as usize)
            .ok_or(PlanError::Reference)?;
        Ok(match expression {
            Expression::Aggregate { .. } => return Err(PlanError::Aggregate.into()),
            Expression::Literal(Literal::Null) => ScratchCell::Null,
            Expression::Literal(Literal::Bool(value)) => ScratchCell::Bool(value),
            Expression::Literal(Literal::I64(value)) => ScratchCell::I64(value),
            Expression::Literal(Literal::F64(value)) => ScratchCell::F64(value),
            Expression::Literal(Literal::String(value)) => {
                self.scratch
                    .copy_bytes(value.as_bytes(), context, &mut self.test_poll)?
            }
            Expression::Parameter(id) => self
                .parameters
                .as_slice()
                .get(id.0 as usize)
                .copied()
                .ok_or(PlanError::Parameter)?,
            Expression::Slot(slot) => self.scratch.copy_value(
                input
                    .value(row, schema.column(slot)?)
                    .ok_or(RuntimeError::Batch)?,
                context,
                &mut self.test_poll,
                0,
            )?,
            Expression::List(items) => {
                let start = self.scratch.reserve_list(items.len(), || {
                    self.test_poll.poll();
                    context.values().step()?;
                    Ok(())
                })?;
                for (index, item) in items.iter().enumerate() {
                    let value = self.evaluate_inner(
                        *item,
                        schema,
                        input,
                        row,
                        view,
                        overlay.as_deref_mut(),
                        context,
                        depth + 1,
                    )?;
                    *self
                        .scratch
                        .cells
                        .as_mut_slice()
                        .get_mut(start + index)
                        .ok_or(RuntimeError::Batch)? = value;
                }
                self.scratch.finish_list(start, items.len(), || {
                    self.test_poll.poll();
                    context.values().step()?;
                    Ok(())
                })?
            }
            Expression::Property { entity, name } => {
                let receiver = self.evaluate_inner(
                    entity,
                    schema,
                    input,
                    row,
                    view,
                    overlay.as_deref_mut(),
                    context,
                    depth + 1,
                )?;
                self.property(receiver, name, view, overlay, context)?
            }
            Expression::HasLabel { entity, label } => {
                let receiver = self.evaluate_inner(
                    entity,
                    schema,
                    input,
                    row,
                    view,
                    overlay.as_deref_mut(),
                    context,
                    depth + 1,
                )?;
                self.has_label(receiver, label, view, overlay, context)?
            }
            Expression::Unary { operation, operand } => {
                let operand = self.evaluate_inner(
                    operand,
                    schema,
                    input,
                    row,
                    view,
                    overlay.as_deref_mut(),
                    context,
                    depth + 1,
                )?;
                self.unary(operation, operand, view, overlay, context)?
            }
            Expression::Binary {
                operation,
                left,
                right,
            } => {
                let left = self.evaluate_inner(
                    left,
                    schema,
                    input,
                    row,
                    view,
                    overlay.as_deref_mut(),
                    context,
                    depth + 1,
                )?;
                let right = self.evaluate_inner(
                    right,
                    schema,
                    input,
                    row,
                    view,
                    overlay,
                    context,
                    depth + 1,
                )?;
                self.binary(operation, left, right, context)?
            }
        })
    }

    fn unary(
        &mut self,
        operation: UnaryExpression,
        operand: ScratchCell,
        view: &GraphReadView<'_, 'v, 'm, 'g>,
        overlay: Option<&mut ClauseOverlay<'_, '_, '_, '_>>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<ScratchCell, ExpressionFailure> {
        let value = self.scratch.value(operand).ok_or(RuntimeError::Batch)?;
        Ok(match operation {
            UnaryExpression::Not => truth_cell(value.truth()?.not()),
            UnaryExpression::Positive => scalar_cell(value.positive()?)?,
            UnaryExpression::Negate => scalar_cell(value.negate()?)?,
            UnaryExpression::IsNull => ScratchCell::Bool(matches!(value, QueryValue::Null)),
            UnaryExpression::IsNotNull => ScratchCell::Bool(!matches!(value, QueryValue::Null)),
            UnaryExpression::Size => self.scratch.adopt(value.size(context.values())?)?,
            UnaryExpression::Labels => self.labels(operand, view, overlay, context)?,
            UnaryExpression::RelType => self.relationship_type(operand, view, overlay, context)?,
            UnaryExpression::StoredText => self.stored_text(operand, view, overlay, context)?,
            UnaryExpression::NodeIdText => {
                let mut output = [0_u8; 32];
                let value = value.node_id_text(&mut output, context.values())?;
                self.scratch
                    .copy_value(value, context, &mut self.test_poll, 0)?
            }
            UnaryExpression::RelIdText => {
                let mut output = [0_u8; 32];
                let value = value.relationship_id_text(&mut output, context.values())?;
                self.scratch
                    .copy_value(value, context, &mut self.test_poll, 0)?
            }
        })
    }

    fn binary(
        &self,
        operation: BinaryExpression,
        left: ScratchCell,
        right: ScratchCell,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<ScratchCell, ExpressionFailure> {
        let left = self.scratch.value(left).ok_or(RuntimeError::Batch)?;
        let right = self.scratch.value(right).ok_or(RuntimeError::Batch)?;
        Ok(match operation {
            BinaryExpression::And => truth_cell(left.truth()?.and(right.truth()?)),
            BinaryExpression::Or => truth_cell(left.truth()?.or(right.truth()?)),
            BinaryExpression::Xor => truth_cell(left.truth()?.xor(right.truth()?)),
            BinaryExpression::Comparison(operation) => {
                truth_cell(left.predicate(right, operation, context.values())?)
            }
            BinaryExpression::Arithmetic(operation) => {
                scalar_cell(left.arithmetic(right, operation)?)?
            }
            BinaryExpression::String(operation) => {
                truth_cell(left.string_predicate(right, operation, context.values())?)
            }
            BinaryExpression::In => truth_cell(left.in_list(right, context.values())?),
            BinaryExpression::Index => self.scratch.adopt(left.index(right, context.values())?)?,
        })
    }

    fn property(
        &mut self,
        receiver: ScratchCell,
        name: GraphName<'_>,
        view: &GraphReadView<'_, 'v, 'm, 'g>,
        overlay: Option<&mut ClauseOverlay<'_, '_, '_, '_>>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<ScratchCell, ExpressionFailure> {
        let receiver = self.scratch.value(receiver).ok_or(RuntimeError::Batch)?;
        receiver.validate(context.values())?;
        let entity = match receiver {
            QueryValue::Null => return Ok(ScratchCell::Null),
            QueryValue::NodeRef(node) => (Some(node.id()), None),
            QueryValue::RelRef(relationship) => (None, Some(relationship.id())),
            _ => return Err(super::QueryError::Type.into()),
        };
        if let Some(overlay) = overlay {
            let target = match entity {
                (Some(node), _) => BatchEntityRef::Node(NodeRef::Existing(node)),
                (_, Some(relationship)) => {
                    BatchEntityRef::Relationship(RelRef::Existing(relationship))
                }
                _ => return Err(super::QueryError::Type.into()),
            };
            return match overlay.property(target, name)? {
                None => Ok(ScratchCell::Null),
                Some(value) => self.copy_property(value, context),
            };
        }
        let mut resources = TreeResources::for_query(context)?;
        let payload = if let Some(node) = entity.0 {
            if let Some(value) = view.document_property(node, name.as_str())? {
                let data = match &value {
                    crate::meta::PredicateValue::I64(value) => PropertyData::I64(*value),
                    crate::meta::PredicateValue::U64(value) => {
                        PropertyData::I64(i64::try_from(*value).map_err(|_| {
                            TreeError::Invalid("document u64 property exceeds graph integer")
                        })?)
                    }
                    crate::meta::PredicateValue::F64(value) => PropertyData::F64(*value),
                    crate::meta::PredicateValue::Bool(value) => PropertyData::Bool(*value),
                    crate::meta::PredicateValue::String(value) => PropertyData::String(value),
                    crate::meta::PredicateValue::Id128(_) => {
                        return Err(TreeError::Invalid(
                            "document id128 property is not a graph scalar",
                        )
                        .into());
                    }
                };
                drop(resources);
                return self.copy_property(
                    PropertyValue::new(data)
                        .map_err(|_| TreeError::Invalid("document property"))?,
                    context,
                );
            }
            let Some(record) = view.lookup_node(node, &mut resources)? else {
                if view.document_version(node)?.is_some() {
                    return Ok(ScratchCell::Null);
                }
                return Err(TreeError::Invalid("expression node is absent or deleted").into());
            };
            let Some(Symbol::Property(key)) =
                view.expression_symbol(SymbolKind::Property, name, &mut resources)?
            else {
                return Ok(ScratchCell::Null);
            };
            view.node_property(&record, key, &mut resources)?
        } else {
            let relationship = entity.1.ok_or(super::QueryError::Type)?;
            let record = view
                .lookup_relationship(relationship, &mut resources)?
                .ok_or(TreeError::Invalid(
                    "expression relationship is absent, deleted, or hidden",
                ))?;
            let Some(Symbol::Property(key)) =
                view.expression_symbol(SymbolKind::Property, name, &mut resources)?
            else {
                return Ok(ScratchCell::Null);
            };
            view.relationship_property(&record, key, &mut resources)?
        };
        match payload {
            None => Ok(ScratchCell::Null),
            Some(payload) => self.decode_property(payload, &mut resources),
        }
    }

    fn has_label(
        &self,
        receiver: ScratchCell,
        label: GraphName<'_>,
        view: &GraphReadView<'_, 'v, 'm, 'g>,
        overlay: Option<&mut ClauseOverlay<'_, '_, '_, '_>>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<ScratchCell, ExpressionFailure> {
        let receiver = self.scratch.value(receiver).ok_or(RuntimeError::Batch)?;
        receiver.validate(context.values())?;
        let node = match receiver {
            QueryValue::Null => return Ok(ScratchCell::Null),
            QueryValue::NodeRef(node) => node.id(),
            _ => return Err(super::QueryError::Type.into()),
        };
        let document = view.document_version(node)?.is_some();
        if document && label.as_str() == "Document" {
            return Ok(ScratchCell::Bool(true));
        }
        if let Some(overlay) = overlay
            && let Some(staged) = overlay.labels(node)?
        {
            for staged in staged {
                if same_name(*staged, label, context)? {
                    return Ok(ScratchCell::Bool(true));
                }
            }
            return Ok(ScratchCell::Bool(false));
        }
        let mut resources = TreeResources::for_query(context)?;
        let Some(record) = view.lookup_node(node, &mut resources)? else {
            return if document {
                Ok(ScratchCell::Bool(false))
            } else {
                Err(TreeError::Invalid("expression node is absent or deleted").into())
            };
        };
        if label.as_str() == "Document" {
            let crate::property_graph::storage::records::RecordShape::Node { labels, .. } =
                record.record().shape()
            else {
                return Err(TreeError::Invalid("node expression record role").into());
            };
            for index in 0..labels {
                let name = view
                    .expression_symbol_name(
                        Symbol::Label(record.record().label(index, &mut resources)?),
                        &mut resources,
                    )?
                    .ok_or(TreeError::Invalid("native label symbol is unnamed"))?;
                if name.as_str() == "Document" {
                    return Ok(ScratchCell::Bool(true));
                }
            }
            return Ok(ScratchCell::Bool(false));
        }
        let Some(Symbol::Label(label)) =
            view.expression_symbol(SymbolKind::Label, label, &mut resources)?
        else {
            return Ok(ScratchCell::Bool(false));
        };
        let crate::property_graph::storage::records::RecordShape::Node { labels, .. } =
            record.record().shape()
        else {
            return Err(TreeError::Invalid("node expression record role").into());
        };
        for index in 0..labels {
            resources.step(1)?;
            if record.record().label(index, &mut resources)? == label {
                return Ok(ScratchCell::Bool(true));
            }
        }
        Ok(ScratchCell::Bool(false))
    }

    fn labels(
        &mut self,
        receiver: ScratchCell,
        view: &GraphReadView<'_, 'v, 'm, 'g>,
        overlay: Option<&mut ClauseOverlay<'_, '_, '_, '_>>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<ScratchCell, ExpressionFailure> {
        let receiver = self.scratch.value(receiver).ok_or(RuntimeError::Batch)?;
        receiver.validate(context.values())?;
        let node = match receiver {
            QueryValue::Null => return Ok(ScratchCell::Null),
            QueryValue::NodeRef(node) => node.id(),
            _ => return Err(super::QueryError::Type.into()),
        };
        let document = view.document_version(node)?.is_some();
        if let Some(overlay) = overlay
            && let Some(staged) = overlay.labels(node)?
        {
            return self.copy_names(staged, document, context);
        }
        let mut resources = TreeResources::for_query(context)?;
        let record = view.lookup_node(node, &mut resources)?;
        let labels = match record.as_ref().map(|node| node.record().shape()) {
            Some(crate::property_graph::storage::records::RecordShape::Node { labels, .. }) => {
                labels
            }
            None if document => 0,
            _ => return Err(TreeError::Invalid("expression node is absent or deleted").into()),
        };
        let mut explicit_document = false;
        if document && let Some(record) = &record {
            for index in 0..labels {
                let symbol = Symbol::Label(record.record().label(index, &mut resources)?);
                let name = view
                    .expression_symbol_name(symbol, &mut resources)?
                    .ok_or(TreeError::Invalid("native label symbol is unnamed"))?;
                explicit_document |= name.as_str() == "Document";
            }
        }
        let append_document = document && !explicit_document;
        let len = (labels as usize)
            .checked_add(usize::from(append_document))
            .ok_or(RuntimeError::Batch)?;
        let start = self.scratch.reserve_list(len, || {
            self.test_poll.poll();
            resources.step(1)?;
            Ok(())
        })?;
        for index in 0..labels {
            let label = record
                .as_ref()
                .ok_or(RuntimeError::Batch)?
                .record()
                .label(index, &mut resources)?;
            let name = view
                .expression_symbol_name(Symbol::Label(label), &mut resources)?
                .ok_or(TreeError::Invalid("native label symbol is unnamed"))?;
            let value = self.scratch.copy_native_bytes(
                name.as_str().as_bytes(),
                &mut resources,
                &mut self.test_poll,
            )?;
            *self
                .scratch
                .cells
                .as_mut_slice()
                .get_mut(start + index as usize)
                .ok_or(RuntimeError::Batch)? = value;
        }
        if append_document {
            let value =
                self.scratch
                    .copy_native_bytes(b"Document", &mut resources, &mut self.test_poll)?;
            *self
                .scratch
                .cells
                .as_mut_slice()
                .get_mut(start + labels as usize)
                .ok_or(RuntimeError::Batch)? = value;
        }
        self.scratch.finish_list(start, len, || {
            self.test_poll.poll();
            resources.step(1)?;
            Ok(())
        })
    }

    fn relationship_type(
        &mut self,
        receiver: ScratchCell,
        view: &GraphReadView<'_, 'v, 'm, 'g>,
        overlay: Option<&mut ClauseOverlay<'_, '_, '_, '_>>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<ScratchCell, ExpressionFailure> {
        let receiver = self.scratch.value(receiver).ok_or(RuntimeError::Batch)?;
        receiver.validate(context.values())?;
        let relationship = match receiver {
            QueryValue::Null => return Ok(ScratchCell::Null),
            QueryValue::RelRef(relationship) => relationship.id(),
            _ => return Err(super::QueryError::Type.into()),
        };
        if let Some(overlay) = overlay
            && let Some(staged) = overlay.relationship_type(relationship)?
        {
            return self.scratch.copy_bytes(
                staged.as_str().as_bytes(),
                context,
                &mut self.test_poll,
            );
        }
        let mut resources = TreeResources::for_query(context)?;
        let record = view
            .lookup_relationship(relationship, &mut resources)?
            .ok_or(TreeError::Invalid(
                "expression relationship is absent, deleted, or hidden",
            ))?;
        let name = view
            .expression_symbol_name(
                Symbol::RelationshipType(record.row().relationship_type),
                &mut resources,
            )?
            .ok_or(TreeError::Invalid(
                "native relationship type symbol is unnamed",
            ))?;
        self.scratch.copy_native_bytes(
            name.as_str().as_bytes(),
            &mut resources,
            &mut self.test_poll,
        )
    }

    fn stored_text(
        &mut self,
        receiver: ScratchCell,
        view: &GraphReadView<'_, 'v, 'm, 'g>,
        overlay: Option<&mut ClauseOverlay<'_, '_, '_, '_>>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<ScratchCell, ExpressionFailure> {
        let receiver = self.scratch.value(receiver).ok_or(RuntimeError::Batch)?;
        receiver.validate(context.values())?;
        let node = match receiver {
            QueryValue::Null => return Ok(ScratchCell::Null),
            QueryValue::NodeRef(node) => node.id(),
            _ => return Err(super::QueryError::Type.into()),
        };
        if let Some(overlay) = overlay {
            return match overlay.stored_text(node)? {
                None => Ok(ScratchCell::Null),
                Some(text) => {
                    self.scratch
                        .copy_bytes(text.as_bytes(), context, &mut self.test_poll)
                }
            };
        }
        let mut resources = TreeResources::for_query(context)?;
        match view.stored_text(node, &mut resources)? {
            None => Ok(ScratchCell::Null),
            Some(text) => self
                .scratch
                .copy_text_reader(&text, &mut resources, &mut self.test_poll),
        }
    }

    /// Copies one overlay-owned property into scratch. The pending image holds
    /// already validated values, so no canonical stream is decoded here.
    fn copy_property(
        &mut self,
        value: PropertyValue<'_>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<ScratchCell, ExpressionFailure> {
        Ok(match value.data() {
            PropertyData::String(value) => {
                self.scratch
                    .copy_bytes(value.as_bytes(), context, &mut self.test_poll)?
            }
            PropertyData::Bool(value) => ScratchCell::Bool(value),
            PropertyData::I64(value) => ScratchCell::I64(value),
            PropertyData::F64(value) => ScratchCell::F64(value),
            PropertyData::EmptyList { count } => {
                if count != 0 {
                    return Err(super::QueryError::Type.into());
                }
                self.copy_list(&[], context, |_, _, value: &(), _| {
                    let _ = value;
                    Err(RuntimeError::Batch.into())
                })?
            }
            PropertyData::Strings(values) => {
                self.copy_list(values, context, |scratch, poll, value, context| {
                    scratch.copy_bytes(value.as_bytes(), context, poll)
                })?
            }
            PropertyData::Bools(values) => self.copy_list(values, context, |_, _, value, _| {
                Ok(ScratchCell::Bool(*value))
            })?,
            PropertyData::Integers(values) => {
                self.copy_list(values, context, |_, _, value, _| {
                    Ok(ScratchCell::I64(*value))
                })?
            }
            PropertyData::Floats(values) => self.copy_list(values, context, |_, _, value, _| {
                Ok(ScratchCell::F64(*value))
            })?,
        })
    }

    fn copy_list<T>(
        &mut self,
        values: &[T],
        context: &mut RuntimeContext<'v, 'm, 'g>,
        mut cell: impl FnMut(
            &mut ScratchArenas<'v, 'm, 'g>,
            &mut TestPollControl,
            &T,
            &mut RuntimeContext<'v, 'm, 'g>,
        ) -> Result<ScratchCell, ExpressionFailure>,
    ) -> Result<ScratchCell, ExpressionFailure> {
        let len = values.len();
        if len > MAX_LIST_ELEMENTS {
            return Err(super::QueryError::ListLimit.into());
        }
        let start = self.scratch.reserve_list(len, || {
            self.test_poll.poll();
            context.values().step()?;
            Ok(())
        })?;
        for (index, value) in values.iter().enumerate() {
            context.checkpoint()?;
            let child = cell(&mut self.scratch, &mut self.test_poll, value, context)?;
            *self
                .scratch
                .cells
                .as_mut_slice()
                .get_mut(start + index)
                .ok_or(RuntimeError::Batch)? = child;
        }
        self.scratch.finish_list(start, len, || {
            self.test_poll.poll();
            context.values().step()?;
            Ok(())
        })
    }

    /// Copies staged label names into one scratch list.
    fn copy_names(
        &mut self,
        names: &[GraphName<'_>],
        document: bool,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<ScratchCell, ExpressionFailure> {
        let append = document && !names.iter().any(|name| name.as_str() == "Document");
        if !append {
            return self.copy_list(names, context, |scratch, poll, name, context| {
                scratch.copy_bytes(name.as_str().as_bytes(), context, poll)
            });
        }
        let len = names
            .len()
            .checked_add(usize::from(append))
            .ok_or(RuntimeError::Batch)?;
        let start = self.scratch.reserve_list(len, || {
            self.test_poll.poll();
            context.values().step()?;
            Ok(())
        })?;
        for (index, bytes) in names
            .iter()
            .map(|name| name.as_str().as_bytes())
            .chain(append.then_some(b"Document".as_slice()))
            .enumerate()
        {
            let value = self
                .scratch
                .copy_bytes(bytes, context, &mut self.test_poll)?;
            *self
                .scratch
                .cells
                .as_mut_slice()
                .get_mut(start + index)
                .ok_or(RuntimeError::Batch)? = value;
        }
        self.scratch.finish_list(start, len, || {
            self.test_poll.poll();
            context.values().step()?;
            Ok(())
        })
    }

    fn decode_property<S: BlockSource>(
        &mut self,
        value: PayloadSlice<'_, S>,
        resources: &mut TreeResources<'_>,
    ) -> Result<ScratchCell, ExpressionFailure> {
        let mut cursor = PayloadCursor::new(value);
        let tag = u8::from_le_bytes(cursor.read_array(resources)?);
        let result = match tag {
            1 => {
                let value = cursor.blob(resources)?;
                self.scratch
                    .copy_payload(value, resources, &mut self.test_poll)?
            }
            2 => match u8::from_le_bytes(cursor.read_array(resources)?) {
                0 => ScratchCell::Bool(false),
                1 => ScratchCell::Bool(true),
                _ => return Err(TreeError::Invalid("canonical expression boolean").into()),
            },
            3 => ScratchCell::I64(i64::from_le_bytes(cursor.read_array(resources)?)),
            4 => ScratchCell::F64(f64::from_bits(u64::from_le_bytes(
                cursor.read_array(resources)?,
            ))),
            5..=9 => {
                let count = u64::from_le_bytes(cursor.read_array(resources)?);
                let len = usize::try_from(count).map_err(|_| RuntimeError::Batch)?;
                if len > MAX_LIST_ELEMENTS || (tag == 5 && len != 0) {
                    return Err(TreeError::Invalid("canonical expression list count").into());
                }
                let start = self.scratch.reserve_list(len, || {
                    self.test_poll.poll();
                    resources.step(1)?;
                    Ok(())
                })?;
                for index in 0..len {
                    resources.step(1)?;
                    let child = match tag {
                        6 => {
                            let value = cursor.blob(resources)?;
                            self.scratch
                                .copy_payload(value, resources, &mut self.test_poll)?
                        }
                        7 => match u8::from_le_bytes(cursor.read_array(resources)?) {
                            0 => ScratchCell::Bool(false),
                            1 => ScratchCell::Bool(true),
                            _ => {
                                return Err(TreeError::Invalid(
                                    "canonical expression list boolean",
                                )
                                .into());
                            }
                        },
                        8 => ScratchCell::I64(i64::from_le_bytes(cursor.read_array(resources)?)),
                        9 => ScratchCell::F64(f64::from_bits(u64::from_le_bytes(
                            cursor.read_array(resources)?,
                        ))),
                        _ => {
                            return Err(
                                TreeError::Invalid("nonempty untyped expression list").into()
                            );
                        }
                    };
                    *self
                        .scratch
                        .cells
                        .as_mut_slice()
                        .get_mut(start + index)
                        .ok_or(RuntimeError::Batch)? = child;
                }
                self.scratch.finish_list(start, len, || {
                    self.test_poll.poll();
                    resources.step(1)?;
                    Ok(())
                })?
            }
            _ => return Err(TreeError::Invalid("canonical expression property tag").into()),
        };
        cursor.finish(resources)?;
        Ok(result)
    }
}

/// Compares two exact names in bounded chunks, checking work between them.
fn same_name(
    left: GraphName<'_>,
    right: GraphName<'_>,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<bool, ExpressionFailure> {
    let left = left.as_str().as_bytes();
    let right = right.as_str().as_bytes();
    if left.len() != right.len() {
        context.checkpoint()?;
        return Ok(false);
    }
    for (left, right) in left.chunks(CHUNK_BYTES).zip(right.chunks(CHUNK_BYTES)) {
        context.checkpoint()?;
        context.charge(WorkKind::CopiedBytes, left.len() as u64)?;
        if left != right {
            return Ok(false);
        }
    }
    context.checkpoint()?;
    Ok(true)
}

fn scalar_cell(value: QueryValue<'_>) -> Result<ScratchCell, ExpressionFailure> {
    Ok(match value {
        QueryValue::Null => ScratchCell::Null,
        QueryValue::Bool(value) => ScratchCell::Bool(value),
        QueryValue::I64(value) => ScratchCell::I64(value),
        QueryValue::F64(value) => ScratchCell::F64(value),
        QueryValue::String(_)
        | QueryValue::List(_)
        | QueryValue::NodeRef(_)
        | QueryValue::RelRef(_) => return Err(super::QueryError::Type.into()),
    })
}

const fn truth_cell(value: Truth) -> ScratchCell {
    match value {
        Truth::False => ScratchCell::Bool(false),
        Truth::Unknown => ScratchCell::Null,
        Truth::True => ScratchCell::Bool(true),
    }
}
