#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "test assertions and fixed fixture indices"
)]

use super::super::{NativePattern, PatternCapacity};
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::native_graph::NativeReadConsumer;
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use crate::property_graph::query::QueryValue;
use crate::property_graph::query::expression::ExpressionCapacity;
use crate::property_graph::query::plan::{
    AggregateExpression, Direction, ExprId, Expression, Literal, NodeFacts, Operator, OperatorKind,
    PatternId, PlanBacking, PlanDescription, PlanFootprint, PlanNodeId, Projection, RetainedRegion,
    SlotId, SortKey, VALIDATION_SCRATCH_BYTES,
};
use crate::property_graph::query::relational::StorageCapacity;
use crate::property_graph::query::resources::{
    QueryArena, QueryInputs, RetainedAllocation, RetentionInventory,
};
use crate::property_graph::query::runtime::{
    ArenaCapacity, Completion, Execution, ExecutionCapacity, FrozenOutput, NativeExecutionError,
    PreparedRows, RuntimeContext, RuntimeError, RuntimeFailure, RuntimeLimits, execute_in,
};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::{
    ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphName, GraphProperty,
    GraphRevision, NodeId, NodeRef, PropertyData, PropertyValue,
};
use std::mem::size_of;

use super::test_support::execute_relational_plan;

type OffsetTuple = (u128, u128, u128);

struct FreezeOffsetTuple;

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for FreezeOffsetTuple {
    type Output = ([Option<OffsetTuple>; 4], usize);

    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeExecutionError> {
        if rows.rows() > 4 || rows.columns() != 3 {
            return Err(RuntimeError::Batch.into());
        }
        let mut output = [None; 4];
        for (row, destination) in output.iter_mut().enumerate().take(rows.rows()) {
            let source = match rows.value(row, 0) {
                Some(QueryValue::NodeRef(value)) => value.id(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            let relationship = match rows.value(row, 1) {
                Some(QueryValue::RelRef(value)) => value.id(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            let target = match rows.value(row, 2) {
                Some(QueryValue::NodeRef(value)) => value.id(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            *destination = Some((source.get(), relationship.get(), target.get()));
        }
        FrozenOutput::new(
            (output, rows.rows()),
            rows.rows(),
            rows.rows() * size_of::<OffsetTuple>(),
            0,
        )
        .map_err(Into::into)
    }
}

#[derive(Debug)]
enum RelationalExecutionFailure {
    Build(NativeExecutionError),
    Run(Box<RuntimeFailure<NativeExecutionError>>),
}

impl std::fmt::Display for RelationalExecutionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Build(error) => write!(formatter, "build: {error}"),
            Self::Run(error) => write!(formatter, "run: {error}"),
        }
    }
}

struct OffsetScopeConsumer {
    source: NodeId,
    first_offset: u64,
    first_limit: Option<u64>,
    second_offset: u64,
    second_limit: Option<u64>,
}

impl
    NativeReadConsumer<
        Result<Execution<([Option<OffsetTuple>; 4], usize)>, RelationalExecutionFailure>,
    > for OffsetScopeConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<([Option<OffsetTuple>; 4], usize)>, RelationalExecutionFailure>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let lookup_inputs = [PlanNodeId(0)];
        let first_expand_inputs = [PlanNodeId(1)];
        let with_inputs = [PlanNodeId(2)];
        let offset_inputs = [PlanNodeId(3)];
        let chained_offset_inputs = [PlanNodeId(4)];
        let second_expand_inputs = [PlanNodeId(5)];
        let project_inputs = [PlanNodeId(6)];
        let collect_inputs = [PlanNodeId(7)];
        let with_projections = [Projection {
            slot: SlotId(1_000),
            expression: ExprId(0),
        }];
        let output_projections = [
            Projection {
                slot: SlotId(2_000),
                expression: ExprId(1),
            },
            Projection {
                slot: SlotId(2_001),
                expression: ExprId(2),
            },
            Projection {
                slot: SlotId(2_002),
                expression: ExprId(3),
            },
        ];
        let expressions = [
            Expression::Slot(SlotId(1)),
            Expression::Slot(SlotId(1_000)),
            Expression::Slot(SlotId(1_001)),
            Expression::Slot(SlotId(1_002)),
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_inputs,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.source,
                },
            },
            Operator {
                inputs: &first_expand_inputs,
                kind: OperatorKind::Expand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationship: SlotId(2),
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &with_inputs,
                kind: OperatorKind::With(&with_projections),
            },
            Operator {
                inputs: &offset_inputs,
                kind: OperatorKind::OffsetLimit {
                    offset: self.first_offset,
                    limit: self.first_limit,
                },
            },
            Operator {
                inputs: &chained_offset_inputs,
                kind: OperatorKind::OffsetLimit {
                    offset: self.second_offset,
                    limit: self.second_limit,
                },
            },
            Operator {
                inputs: &second_expand_inputs,
                kind: OperatorKind::Expand {
                    source: SlotId(1_000),
                    node: SlotId(1_002),
                    relationship: SlotId(1_001),
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(1),
                },
            },
            Operator {
                inputs: &project_inputs,
                kind: OperatorKind::Project(&output_projections),
            },
            Operator {
                inputs: &collect_inputs,
                kind: OperatorKind::Collect,
            },
        ];

        let memory = runtime.memory();
        let mut facts = QueryArena::new(memory, operators.len()).expect("fact arena");
        for _ in 0..operators.len() {
            facts.push(NodeFacts::default()).expect("fact slot");
        }
        let mut regions = vec![
            RetainedRegion::slice(&operators).unwrap(),
            RetainedRegion::slice(&expressions).unwrap(),
            RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes())
                .unwrap(),
            RetainedRegion::slice(&lookup_inputs).unwrap(),
            RetainedRegion::slice(&first_expand_inputs).unwrap(),
            RetainedRegion::slice(&with_inputs).unwrap(),
            RetainedRegion::slice(&offset_inputs).unwrap(),
            RetainedRegion::slice(&chained_offset_inputs).unwrap(),
            RetainedRegion::slice(&second_expand_inputs).unwrap(),
            RetainedRegion::slice(&project_inputs).unwrap(),
            RetainedRegion::slice(&collect_inputs).unwrap(),
            RetainedRegion::slice(&with_projections).unwrap(),
            RetainedRegion::slice(&output_projections).unwrap(),
        ];
        regions.sort();
        let retained_bytes = regions
            .iter()
            .try_fold(0usize, |total, region| {
                total.checked_add(region.end() - region.start())
            })
            .expect("retained plan bytes");
        let mut external = memory
            .reserve_external_capacity()
            .expect("external plan backing");
        external
            .reserve_additional(
                retained_bytes
                    + VALIDATION_SCRATCH_BYTES
                    + regions.capacity() * size_of::<RetainedRegion>()
                    + size_of::<PlanDescription<'_>>(),
            )
            .expect("plan validation backing");
        let description = PlanDescription {
            operators: &operators,
            expressions: &expressions,
            parameters: &[],
            root: PlanNodeId(8),
            eager_searches: &[],
        };
        let (plan, facts_owner) = facts
            .validate_plan(
                description,
                PlanFootprint::declared(memory.reserved_bytes()),
                PlanBacking::vector(&regions).unwrap(),
                runtime.values(),
            )
            .expect("validate offset pattern plan");
        let owners = vec![
            RetainedAllocation::array(&operators).unwrap(),
            RetainedAllocation::array(&expressions).unwrap(),
            facts_owner,
            RetainedAllocation::array(&lookup_inputs).unwrap(),
            RetainedAllocation::array(&first_expand_inputs).unwrap(),
            RetainedAllocation::array(&with_inputs).unwrap(),
            RetainedAllocation::array(&offset_inputs).unwrap(),
            RetainedAllocation::array(&chained_offset_inputs).unwrap(),
            RetainedAllocation::array(&second_expand_inputs).unwrap(),
            RetainedAllocation::array(&project_inputs).unwrap(),
            RetainedAllocation::array(&collect_inputs).unwrap(),
            RetainedAllocation::array(&with_projections).unwrap(),
            RetainedAllocation::array(&output_projections).unwrap(),
        ];
        let admitted = QueryInputs::reserve(
            memory,
            RetentionInventory::vector(&owners).unwrap(),
            runtime.values(),
        )
        .expect("retain offset pattern plan")
        .admit_plan(&plan, runtime.values())
        .expect("admit offset pattern plan");
        let mut source = match NativePattern::new(
            view,
            &admitted,
            PlanNodeId(8),
            &[],
            PatternCapacity {
                rows: StorageCapacity {
                    rows: 8,
                    max_rows: 8,
                    payload_bytes: 4096,
                    variable: ArenaCapacity::default(),
                },
                expression: ExpressionCapacity {
                    cells: 16,
                    string_bytes: 256,
                },
            },
            runtime,
        ) {
            Ok(source) => source,
            Err(error) => return Ok(Err(RelationalExecutionFailure::Build(error))),
        };
        Ok(execute_in(
            runtime,
            &admitted,
            &mut source,
            &mut FreezeOffsetTuple,
            ExecutionCapacity {
                batch_rows: 1,
                result_rows: 4,
                batch_payload_bytes: 4096,
                result_payload_bytes: 4096,
                batch: ArenaCapacity::default(),
                result: ArenaCapacity::default(),
            },
        )
        .map_err(|error| RelationalExecutionFailure::Run(Box::new(error))))
    }
}

#[test]
fn native_relational_offset_scope_then_match() {
    let directory = tempfile::tempdir().expect("native relational store");
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .expect("create native relational store");
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let node_a = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let node_b = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let node_c = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "relational", "a").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node_a)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "relational", "b").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node_b)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "relational", "c").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node_c)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "relational", "ab-1").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "relational", "ab-2").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "relational", "bc").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(1).unwrap()),
                    target: NodeRef::Local(refs.node(2).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            },
        ];
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("publish native relational fixture")
    });
    let node_a = match receipts[0].entity {
        EntityId::Node(id) => id,
        _ => panic!("node A receipt kind"),
    };
    let node_b = match receipts[1].entity {
        EntityId::Node(id) => id,
        _ => panic!("node B receipt kind"),
    };
    let node_c = match receipts[2].entity {
        EntityId::Node(id) => id,
        _ => panic!("node C receipt kind"),
    };
    let relationship_bc = match receipts[5].entity {
        EntityId::Relationship(id) => id,
        _ => panic!("B to C receipt kind"),
    };
    let execution = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            OffsetScopeConsumer {
                source: node_a,
                first_offset: 1,
                first_limit: Some(1),
                second_offset: 0,
                second_limit: None,
            },
        )
        .expect("admit native relational view");
    assert!(
        execution.is_ok(),
        "native OffsetLimit before a later MATCH must execute successfully: {}",
        execution.as_ref().err().unwrap()
    );
    let execution = execution.unwrap();
    assert_eq!(execution.output.1, 1);
    assert_eq!(
        execution.output.0[0],
        Some((node_b.get(), relationship_bc.get(), node_c.get()))
    );
    assert!(
        execution
            .counters
            .get(crate::property_graph::query::runtime::WorkKind::OperatorRows)
            > execution.output.1 as u64
    );
    for (consumer, label) in [
        (
            OffsetScopeConsumer {
                source: node_a,
                first_offset: 3,
                first_limit: None,
                second_offset: 0,
                second_limit: None,
            },
            "beyond-end",
        ),
        (
            OffsetScopeConsumer {
                source: node_a,
                first_offset: 0,
                first_limit: Some(0),
                second_offset: 0,
                second_limit: None,
            },
            "limit-zero",
        ),
    ] {
        let empty = store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                16 * 1024 * 1024,
                64,
                consumer,
            )
            .unwrap_or_else(|error| panic!("admit {label}: {error}"))
            .unwrap_or_else(|error| panic!("execute {label}: {error}"));
        assert_eq!(empty.output.1, 0, "{label}");
    }
    let chained = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            OffsetScopeConsumer {
                source: node_a,
                first_offset: 0,
                first_limit: Some(2),
                second_offset: 1,
                second_limit: Some(1),
            },
        )
        .expect("admit chained offset view")
        .expect("execute chained offset view");
    store.close().expect("close native relational store");
    assert_eq!(chained.output.1, 1);
    assert_eq!(
        chained.output.0[0],
        Some((node_b.get(), relationship_bc.get(), node_c.get()))
    );
}

struct SortScopeConsumer {
    source: NodeId,
    chunk_rows: usize,
}

impl
    NativeReadConsumer<
        Result<Execution<([Option<OffsetTuple>; 4], usize)>, RelationalExecutionFailure>,
    > for SortScopeConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<([Option<OffsetTuple>; 4], usize)>, RelationalExecutionFailure>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let ordinal = String::from("ordinal");
        let scratch = String::from("stable-tie");
        let lookup_inputs = [PlanNodeId(0)];
        let first_expand_inputs = [PlanNodeId(1)];
        let sort_inputs = [PlanNodeId(2)];
        let second_expand_inputs = [PlanNodeId(3)];
        let project_inputs = [PlanNodeId(4)];
        let collect_inputs = [PlanNodeId(5)];
        let sort_keys = [
            SortKey {
                expression: ExprId(1),
                descending: false,
            },
            SortKey {
                expression: ExprId(5),
                descending: true,
            },
        ];
        let projections = [
            Projection {
                slot: SlotId(2_000),
                expression: ExprId(2),
            },
            Projection {
                slot: SlotId(2_001),
                expression: ExprId(3),
            },
            Projection {
                slot: SlotId(2_002),
                expression: ExprId(4),
            },
        ];
        let expressions = [
            Expression::Slot(SlotId(1)),
            Expression::Property {
                entity: ExprId(0),
                name: GraphName::new(&ordinal).unwrap(),
            },
            Expression::Slot(SlotId(1)),
            Expression::Slot(SlotId(3)),
            Expression::Slot(SlotId(4)),
            Expression::Literal(Literal::String(&scratch)),
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_inputs,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.source,
                },
            },
            Operator {
                inputs: &first_expand_inputs,
                kind: OperatorKind::Expand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationship: SlotId(2),
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &sort_inputs,
                kind: OperatorKind::Sort(&sort_keys),
            },
            Operator {
                inputs: &second_expand_inputs,
                kind: OperatorKind::Expand {
                    source: SlotId(1),
                    node: SlotId(4),
                    relationship: SlotId(3),
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(1),
                },
            },
            Operator {
                inputs: &project_inputs,
                kind: OperatorKind::Project(&projections),
            },
            Operator {
                inputs: &collect_inputs,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_relational_plan!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&lookup_inputs).unwrap(),
                RetainedRegion::slice(&first_expand_inputs).unwrap(),
                RetainedRegion::slice(&sort_inputs).unwrap(),
                RetainedRegion::slice(&second_expand_inputs).unwrap(),
                RetainedRegion::slice(&project_inputs).unwrap(),
                RetainedRegion::slice(&collect_inputs).unwrap(),
                RetainedRegion::slice(&sort_keys).unwrap(),
                RetainedRegion::slice(&projections).unwrap(),
                RetainedRegion::declared(ordinal.as_ptr() as usize, ordinal.capacity()).unwrap(),
                RetainedRegion::declared(scratch.as_ptr() as usize, scratch.capacity()).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&lookup_inputs).unwrap(),
                RetainedAllocation::array(&first_expand_inputs).unwrap(),
                RetainedAllocation::array(&sort_inputs).unwrap(),
                RetainedAllocation::array(&second_expand_inputs).unwrap(),
                RetainedAllocation::array(&project_inputs).unwrap(),
                RetainedAllocation::array(&collect_inputs).unwrap(),
                RetainedAllocation::array(&sort_keys).unwrap(),
                RetainedAllocation::array(&projections).unwrap(),
                RetainedAllocation::string(&ordinal).unwrap(),
                RetainedAllocation::string(&scratch).unwrap(),
            ],
            &mut FreezeOffsetTuple,
            chunk_rows = self.chunk_rows
        ))
    }
}

struct FreezeNodeList;

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for FreezeNodeList {
    type Output = ([Option<u128>; 4], usize);

    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeExecutionError> {
        if rows.rows() != 1 || rows.columns() != 1 {
            return Err(RuntimeError::Batch.into());
        }
        let Some(QueryValue::List(list)) = rows.value(0, 0) else {
            return Err(RuntimeError::Batch.into());
        };
        if list.len() > 4 {
            return Err(RuntimeError::Batch.into());
        }
        let mut output = [None; 4];
        for (position, destination) in output.iter_mut().enumerate().take(list.len()) {
            *destination = match list.get(position) {
                Some(QueryValue::NodeRef(value)) => Some(value.id().get()),
                _ => return Err(RuntimeError::Batch.into()),
            };
        }
        FrozenOutput::new((output, list.len()), 1, list.len() * size_of::<u128>(), 0)
            .map_err(Into::into)
    }
}

struct SortCollectConsumer {
    source: NodeId,
}

impl NativeReadConsumer<Result<Execution<([Option<u128>; 4], usize)>, RelationalExecutionFailure>>
    for SortCollectConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<([Option<u128>; 4], usize)>, RelationalExecutionFailure>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let ordinal = String::from("ordinal");
        let scratch = String::from("stable-tie");
        let lookup_inputs = [PlanNodeId(0)];
        let expand_inputs = [PlanNodeId(1)];
        let sort_inputs = [PlanNodeId(2)];
        let aggregate_inputs = [PlanNodeId(3)];
        let collect_inputs = [PlanNodeId(4)];
        let sort_keys = [
            SortKey {
                expression: ExprId(1),
                descending: false,
            },
            SortKey {
                expression: ExprId(2),
                descending: true,
            },
        ];
        let aggregates = [Projection {
            slot: SlotId(10),
            expression: ExprId(3),
        }];
        let expressions = [
            Expression::Slot(SlotId(1)),
            Expression::Property {
                entity: ExprId(0),
                name: GraphName::new(&ordinal).unwrap(),
            },
            Expression::Literal(Literal::String(&scratch)),
            Expression::Aggregate {
                operation: AggregateExpression::Collect { distinct: false },
                operand: Some(ExprId(0)),
            },
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_inputs,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.source,
                },
            },
            Operator {
                inputs: &expand_inputs,
                kind: OperatorKind::Expand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationship: SlotId(2),
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &sort_inputs,
                kind: OperatorKind::Sort(&sort_keys),
            },
            Operator {
                inputs: &aggregate_inputs,
                kind: OperatorKind::Aggregate {
                    keys: &[],
                    aggregates: &aggregates,
                },
            },
            Operator {
                inputs: &collect_inputs,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_relational_plan!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&lookup_inputs).unwrap(),
                RetainedRegion::slice(&expand_inputs).unwrap(),
                RetainedRegion::slice(&sort_inputs).unwrap(),
                RetainedRegion::slice(&aggregate_inputs).unwrap(),
                RetainedRegion::slice(&collect_inputs).unwrap(),
                RetainedRegion::slice(&sort_keys).unwrap(),
                RetainedRegion::slice(&aggregates).unwrap(),
                RetainedRegion::declared(ordinal.as_ptr() as usize, ordinal.capacity()).unwrap(),
                RetainedRegion::declared(scratch.as_ptr() as usize, scratch.capacity()).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&lookup_inputs).unwrap(),
                RetainedAllocation::array(&expand_inputs).unwrap(),
                RetainedAllocation::array(&sort_inputs).unwrap(),
                RetainedAllocation::array(&aggregate_inputs).unwrap(),
                RetainedAllocation::array(&collect_inputs).unwrap(),
                RetainedAllocation::array(&sort_keys).unwrap(),
                RetainedAllocation::array(&aggregates).unwrap(),
                RetainedAllocation::string(&ordinal).unwrap(),
                RetainedAllocation::string(&scratch).unwrap(),
            ],
            &mut FreezeNodeList
        ))
    }
}

#[test]
fn native_relational_sort_keys_ordered_collect() {
    sort_chunk_fixture(16);
}

#[test]
fn native_relational_sort_spans_two_chunks() {
    distinct_chunk_fixture(1, false);
}

fn sort_chunk_fixture(chunk_rows: usize) {
    let directory = tempfile::tempdir().expect("native relational sort store");
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .expect("create native relational sort store");
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let node_a = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let mut first_properties = [GraphProperty::new(
            GraphName::new("ordinal").unwrap(),
            PropertyValue::new(PropertyData::I64(2)).unwrap(),
        )];
        let mut second_properties = [GraphProperty::new(
            GraphName::new("ordinal").unwrap(),
            PropertyValue::new(PropertyData::I64(1)).unwrap(),
        )];
        let node_b1 = CanonicalContents::node(&mut [], &mut first_properties, None, None).unwrap();
        let node_b2 = CanonicalContents::node(&mut [], &mut second_properties, None, None).unwrap();
        let node_c1 = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let node_c2 = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "sort", "a").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node_a)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "sort", "b1").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node_b1)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "sort", "b2").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node_b2)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "sort", "c1").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node_c1)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "sort", "c2").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node_c2)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "sort", "ab1").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "sort", "ab2").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(2).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "sort", "b1c1").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(1).unwrap()),
                    target: NodeRef::Local(refs.node(3).unwrap()),
                    relationship_type: GraphName::new("NEXT").unwrap(),
                    properties: &[],
                }),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "sort", "b2c2").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(2).unwrap()),
                    target: NodeRef::Local(refs.node(4).unwrap()),
                    relationship_type: GraphName::new("NEXT").unwrap(),
                    properties: &[],
                }),
            },
        ];
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("publish native relational sort fixture")
    });
    let node = |position: usize| match receipts[position].entity {
        EntityId::Node(id) => id,
        _ => panic!("sort node receipt kind"),
    };
    let relationship = |position: usize| match receipts[position].entity {
        EntityId::Relationship(id) => id,
        _ => panic!("sort relationship receipt kind"),
    };
    let execution = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            SortScopeConsumer {
                source: node(0),
                chunk_rows,
            },
        )
        .expect("admit native relational sort view");
    assert!(
        execution.is_ok(),
        "native Sort before a later MATCH must execute successfully: {}",
        execution.as_ref().err().unwrap()
    );
    let execution = execution.unwrap();
    assert_eq!(execution.output.1, 2);
    assert_eq!(
        &execution.output.0[..2],
        &[
            Some((node(2).get(), relationship(8).get(), node(4).get())),
            Some((node(1).get(), relationship(7).get(), node(3).get())),
        ]
    );
    let ordered = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            SortCollectConsumer { source: node(0) },
        )
        .expect("admit native sorted collect view")
        .expect("execute native sorted collect");
    store.close().expect("close native relational sort store");
    assert_eq!(ordered.output.1, 2);
    assert_eq!(
        &ordered.output.0[..2],
        &[Some(node(2).get()), Some(node(1).get())]
    );
}

struct DistinctScopeConsumer {
    source: NodeId,
    chunk_rows: usize,
    distinct: bool,
    later_pattern: PatternId,
}

impl
    NativeReadConsumer<
        Result<Execution<([Option<OffsetTuple>; 4], usize)>, RelationalExecutionFailure>,
    > for DistinctScopeConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<([Option<OffsetTuple>; 4], usize)>, RelationalExecutionFailure>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let lookup_inputs = [PlanNodeId(0)];
        let first_expand_inputs = [PlanNodeId(1)];
        let sort_inputs = [PlanNodeId(2)];
        let with_inputs = [PlanNodeId(3)];
        let distinct_inputs = [PlanNodeId(4)];
        let second_expand_inputs = [PlanNodeId(5)];
        let project_inputs = [PlanNodeId(6)];
        let collect_inputs = [PlanNodeId(7)];
        let sort_keys = [SortKey {
            expression: ExprId(0),
            descending: false,
        }];
        let with_projections = [
            Projection {
                slot: SlotId(1_000),
                expression: ExprId(1),
            },
            Projection {
                slot: SlotId(1_010),
                expression: ExprId(0),
            },
        ];
        let projections = [
            Projection {
                slot: SlotId(2_000),
                expression: ExprId(2),
            },
            Projection {
                slot: SlotId(2_001),
                expression: ExprId(3),
            },
            Projection {
                slot: SlotId(2_002),
                expression: ExprId(4),
            },
        ];
        let expressions = [
            Expression::Slot(SlotId(2)),
            Expression::Slot(SlotId(1)),
            Expression::Slot(SlotId(1_000)),
            Expression::Slot(SlotId(1_002)),
            Expression::Slot(SlotId(1_001)),
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_inputs,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.source,
                },
            },
            Operator {
                inputs: &first_expand_inputs,
                kind: OperatorKind::Expand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationship: SlotId(2),
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &sort_inputs,
                kind: OperatorKind::Sort(&sort_keys),
            },
            Operator {
                inputs: &with_inputs,
                kind: OperatorKind::With(
                    with_projections
                        .get(..if self.chunk_rows == 1 { 2 } else { 1 })
                        .unwrap(),
                ),
            },
            Operator {
                inputs: &distinct_inputs,
                kind: if self.distinct {
                    OperatorKind::Distinct
                } else {
                    OperatorKind::OffsetLimit {
                        offset: 0,
                        limit: None,
                    }
                },
            },
            Operator {
                inputs: &second_expand_inputs,
                kind: OperatorKind::Expand {
                    source: SlotId(1_000),
                    node: SlotId(1_001),
                    relationship: SlotId(1_002),
                    direction: Direction::Incoming,
                    relationship_types: &[],
                    pattern: self.later_pattern,
                },
            },
            Operator {
                inputs: &project_inputs,
                kind: OperatorKind::Project(&projections),
            },
            Operator {
                inputs: &collect_inputs,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_relational_plan!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&lookup_inputs).unwrap(),
                RetainedRegion::slice(&first_expand_inputs).unwrap(),
                RetainedRegion::slice(&sort_inputs).unwrap(),
                RetainedRegion::slice(&with_inputs).unwrap(),
                RetainedRegion::slice(&distinct_inputs).unwrap(),
                RetainedRegion::slice(&second_expand_inputs).unwrap(),
                RetainedRegion::slice(&project_inputs).unwrap(),
                RetainedRegion::slice(&collect_inputs).unwrap(),
                RetainedRegion::slice(&sort_keys).unwrap(),
                RetainedRegion::slice(&with_projections).unwrap(),
                RetainedRegion::slice(&projections).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&lookup_inputs).unwrap(),
                RetainedAllocation::array(&first_expand_inputs).unwrap(),
                RetainedAllocation::array(&sort_inputs).unwrap(),
                RetainedAllocation::array(&with_inputs).unwrap(),
                RetainedAllocation::array(&distinct_inputs).unwrap(),
                RetainedAllocation::array(&second_expand_inputs).unwrap(),
                RetainedAllocation::array(&project_inputs).unwrap(),
                RetainedAllocation::array(&collect_inputs).unwrap(),
                RetainedAllocation::array(&sort_keys).unwrap(),
                RetainedAllocation::array(&with_projections).unwrap(),
                RetainedAllocation::array(&projections).unwrap(),
            ],
            &mut FreezeOffsetTuple,
            chunk_rows = self.chunk_rows
        ))
    }
}

#[test]
fn native_relational_distinct_provenance_then_match() {
    distinct_chunk_fixture(16, true);
}

#[test]
fn native_relational_distinct_spans_two_chunks() {
    distinct_chunk_fixture(1, true);
}

fn distinct_chunk_fixture(chunk_rows: usize, distinct: bool) {
    let directory = tempfile::tempdir().expect("native relational distinct store");
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .expect("create native relational distinct store");
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let node_a = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let node_b = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "distinct", "a").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node_a)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "distinct", "b").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node_b)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "distinct", "ab1").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "distinct", "ab2").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            },
        ];
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("publish native relational distinct fixture")
    });
    let node = |position: usize| match receipts[position].entity {
        EntityId::Node(id) => id,
        _ => panic!("distinct node receipt kind"),
    };
    let relationship = |position: usize| match receipts[position].entity {
        EntityId::Relationship(id) => id,
        _ => panic!("distinct relationship receipt kind"),
    };
    let same_pattern = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            DistinctScopeConsumer {
                source: node(0),
                chunk_rows,
                distinct,
                later_pattern: PatternId(0),
            },
        )
        .expect("admit same-pattern distinct view");
    assert!(
        same_pattern.is_ok(),
        "native DISTINCT before a later MATCH must execute successfully: {}",
        same_pattern.as_ref().err().unwrap()
    );
    let same_pattern = same_pattern.unwrap();
    if chunk_rows == 1 {
        // Both distinct representatives survive; the second row must suppress
        // its own relationship, proving the second sidecar chunk is selected.
        assert_eq!(same_pattern.output.1, 2);
        assert_eq!(
            same_pattern.output.0[..2],
            [
                Some((node(1).get(), relationship(3).get(), node(0).get())),
                Some((node(1).get(), relationship(2).get(), node(0).get())),
            ]
        );
        store.close().unwrap();
        return;
    }

    assert_eq!(same_pattern.output.1, 1);
    assert_eq!(
        same_pattern.output.0[0],
        Some((node(1).get(), relationship(3).get(), node(0).get()))
    );

    let fresh_pattern = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            DistinctScopeConsumer {
                source: node(0),
                chunk_rows,
                distinct,
                later_pattern: PatternId(1),
            },
        )
        .expect("admit fresh-pattern distinct view");
    store
        .close()
        .expect("close native relational distinct store");
    assert!(fresh_pattern.is_ok());
    let fresh_pattern = fresh_pattern.unwrap();
    assert_eq!(fresh_pattern.output.1, 2);
    assert_eq!(
        &fresh_pattern.output.0[..2],
        &[
            Some((node(1).get(), relationship(2).get(), node(0).get())),
            Some((node(1).get(), relationship(3).get(), node(0).get())),
        ]
    );
}

struct FreezeI64Rows;

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for FreezeI64Rows {
    type Output = ([Option<i64>; 4], usize);

    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeExecutionError> {
        if rows.rows() > 4 || rows.columns() != 1 {
            return Err(RuntimeError::Batch.into());
        }
        let mut output = [None; 4];
        for (row, destination) in output.iter_mut().enumerate().take(rows.rows()) {
            *destination = match rows.value(row, 0) {
                Some(QueryValue::I64(value)) => Some(value),
                _ => return Err(RuntimeError::Batch.into()),
            };
        }
        FrozenOutput::new(
            (output, rows.rows()),
            rows.rows(),
            rows.rows() * size_of::<i64>(),
            0,
        )
        .map_err(Into::into)
    }
}

struct EmptyAggregateConsumer;

impl NativeReadConsumer<Result<Execution<([Option<i64>; 4], usize)>, RelationalExecutionFailure>>
    for EmptyAggregateConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<([Option<i64>; 4], usize)>, RelationalExecutionFailure>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let filter_inputs = [PlanNodeId(0)];
        let aggregate_inputs = [PlanNodeId(1)];
        let collect_inputs = [PlanNodeId(2)];
        let aggregates = [Projection {
            slot: SlotId(10),
            expression: ExprId(1),
        }];
        let expressions = [
            Expression::Literal(Literal::Bool(false)),
            Expression::Aggregate {
                operation: AggregateExpression::Count { distinct: false },
                operand: None,
            },
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &filter_inputs,
                kind: OperatorKind::Filter(ExprId(0)),
            },
            Operator {
                inputs: &aggregate_inputs,
                kind: OperatorKind::Aggregate {
                    keys: &[],
                    aggregates: &aggregates,
                },
            },
            Operator {
                inputs: &collect_inputs,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_relational_plan!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&filter_inputs).unwrap(),
                RetainedRegion::slice(&aggregate_inputs).unwrap(),
                RetainedRegion::slice(&collect_inputs).unwrap(),
                RetainedRegion::slice(&aggregates).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&filter_inputs).unwrap(),
                RetainedAllocation::array(&aggregate_inputs).unwrap(),
                RetainedAllocation::array(&collect_inputs).unwrap(),
                RetainedAllocation::array(&aggregates).unwrap(),
            ],
            &mut FreezeI64Rows
        ))
    }
}

struct FreezeEmptyGrouped;

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for FreezeEmptyGrouped {
    type Output = usize;

    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeExecutionError> {
        if rows.rows() != 0 || rows.columns() != 2 {
            return Err(RuntimeError::Batch.into());
        }
        FrozenOutput::new(0, 0, 0, 0).map_err(Into::into)
    }
}

struct EmptyGroupedAggregateConsumer;

impl NativeReadConsumer<Result<Execution<usize>, RelationalExecutionFailure>>
    for EmptyGroupedAggregateConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<usize>, RelationalExecutionFailure>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let project_inputs = [PlanNodeId(0)];
        let filter_inputs = [PlanNodeId(1)];
        let aggregate_inputs = [PlanNodeId(2)];
        let collect_inputs = [PlanNodeId(3)];
        let projections = [Projection {
            slot: SlotId(0),
            expression: ExprId(0),
        }];
        let keys = [Projection {
            slot: SlotId(10),
            expression: ExprId(2),
        }];
        let aggregates = [Projection {
            slot: SlotId(11),
            expression: ExprId(3),
        }];
        let expressions = [
            Expression::Literal(Literal::I64(1)),
            Expression::Literal(Literal::Bool(false)),
            Expression::Slot(SlotId(0)),
            Expression::Aggregate {
                operation: AggregateExpression::Count { distinct: false },
                operand: None,
            },
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &project_inputs,
                kind: OperatorKind::Project(&projections),
            },
            Operator {
                inputs: &filter_inputs,
                kind: OperatorKind::Filter(ExprId(1)),
            },
            Operator {
                inputs: &aggregate_inputs,
                kind: OperatorKind::Aggregate {
                    keys: &keys,
                    aggregates: &aggregates,
                },
            },
            Operator {
                inputs: &collect_inputs,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_relational_plan!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&project_inputs).unwrap(),
                RetainedRegion::slice(&filter_inputs).unwrap(),
                RetainedRegion::slice(&aggregate_inputs).unwrap(),
                RetainedRegion::slice(&collect_inputs).unwrap(),
                RetainedRegion::slice(&projections).unwrap(),
                RetainedRegion::slice(&keys).unwrap(),
                RetainedRegion::slice(&aggregates).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&project_inputs).unwrap(),
                RetainedAllocation::array(&filter_inputs).unwrap(),
                RetainedAllocation::array(&aggregate_inputs).unwrap(),
                RetainedAllocation::array(&collect_inputs).unwrap(),
                RetainedAllocation::array(&projections).unwrap(),
                RetainedAllocation::array(&keys).unwrap(),
                RetainedAllocation::array(&aggregates).unwrap(),
            ],
            &mut FreezeEmptyGrouped
        ))
    }
}

type AggregateDescriptorOutput = (i64, i64, [Option<u128>; 4], usize);

struct FreezeAggregateDescriptors;

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for FreezeAggregateDescriptors {
    type Output = AggregateDescriptorOutput;

    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeExecutionError> {
        if rows.rows() != 1 || rows.columns() != 4 {
            return Err(RuntimeError::Batch.into());
        }
        if !matches!(rows.value(0, 0), Some(QueryValue::Null)) {
            return Err(RuntimeError::Batch.into());
        }
        let count_all = match rows.value(0, 1) {
            Some(QueryValue::I64(value)) => value,
            _ => return Err(RuntimeError::Batch.into()),
        };
        let count_operand = match rows.value(0, 2) {
            Some(QueryValue::I64(value)) => value,
            _ => return Err(RuntimeError::Batch.into()),
        };
        let Some(QueryValue::List(list)) = rows.value(0, 3) else {
            return Err(RuntimeError::Batch.into());
        };
        if list.len() > 4 {
            return Err(RuntimeError::Batch.into());
        }
        let mut collected = [None; 4];
        for (position, destination) in collected.iter_mut().enumerate().take(list.len()) {
            *destination = match list.get(position) {
                Some(QueryValue::RelRef(value)) => Some(value.id().get()),
                _ => return Err(RuntimeError::Batch.into()),
            };
        }
        FrozenOutput::new(
            (count_all, count_operand, collected, list.len()),
            1,
            list.len() * size_of::<u128>() + 2 * size_of::<i64>(),
            0,
        )
        .map_err(Into::into)
    }
}

struct AggregateDescriptorConsumer {
    source: NodeId,
}

impl NativeReadConsumer<Result<Execution<AggregateDescriptorOutput>, RelationalExecutionFailure>>
    for AggregateDescriptorConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<AggregateDescriptorOutput>, RelationalExecutionFailure>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let missing = String::from("missing");
        let lookup_inputs = [PlanNodeId(0)];
        let expand_inputs = [PlanNodeId(1)];
        let aggregate_inputs = [PlanNodeId(2)];
        let collect_inputs = [PlanNodeId(3)];
        let keys = [Projection {
            slot: SlotId(10),
            expression: ExprId(1),
        }];
        let aggregates = [
            Projection {
                slot: SlotId(11),
                expression: ExprId(2),
            },
            Projection {
                slot: SlotId(12),
                expression: ExprId(3),
            },
            Projection {
                slot: SlotId(13),
                expression: ExprId(4),
            },
        ];
        let expressions = [
            Expression::Slot(SlotId(2)),
            Expression::Property {
                entity: ExprId(5),
                name: GraphName::new(&missing).unwrap(),
            },
            Expression::Aggregate {
                operation: AggregateExpression::Count { distinct: false },
                operand: None,
            },
            Expression::Aggregate {
                operation: AggregateExpression::Count { distinct: false },
                operand: Some(ExprId(1)),
            },
            Expression::Aggregate {
                operation: AggregateExpression::Collect { distinct: true },
                operand: Some(ExprId(0)),
            },
            Expression::Slot(SlotId(1)),
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_inputs,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.source,
                },
            },
            Operator {
                inputs: &expand_inputs,
                kind: OperatorKind::Expand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationship: SlotId(2),
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &aggregate_inputs,
                kind: OperatorKind::Aggregate {
                    keys: &keys,
                    aggregates: &aggregates,
                },
            },
            Operator {
                inputs: &collect_inputs,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_relational_plan!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&lookup_inputs).unwrap(),
                RetainedRegion::slice(&expand_inputs).unwrap(),
                RetainedRegion::slice(&aggregate_inputs).unwrap(),
                RetainedRegion::slice(&collect_inputs).unwrap(),
                RetainedRegion::slice(&keys).unwrap(),
                RetainedRegion::slice(&aggregates).unwrap(),
                RetainedRegion::declared(missing.as_ptr() as usize, missing.capacity()).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&lookup_inputs).unwrap(),
                RetainedAllocation::array(&expand_inputs).unwrap(),
                RetainedAllocation::array(&aggregate_inputs).unwrap(),
                RetainedAllocation::array(&collect_inputs).unwrap(),
                RetainedAllocation::array(&keys).unwrap(),
                RetainedAllocation::array(&aggregates).unwrap(),
                RetainedAllocation::string(&missing).unwrap(),
            ],
            &mut FreezeAggregateDescriptors
        ))
    }
}

type AggregateTuple = (u128, i64, u128, u128);

struct FreezeAggregateRows;

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for FreezeAggregateRows {
    type Output = ([Option<AggregateTuple>; 4], usize);

    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeExecutionError> {
        if rows.rows() > 4 || rows.columns() != 4 {
            return Err(RuntimeError::Batch.into());
        }
        let mut output = [None; 4];
        for (row, destination) in output.iter_mut().enumerate().take(rows.rows()) {
            let group = match rows.value(row, 0) {
                Some(QueryValue::NodeRef(value)) => value.id().get(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            let count = match rows.value(row, 1) {
                Some(QueryValue::I64(value)) => value,
                _ => return Err(RuntimeError::Batch.into()),
            };
            let relationship = match rows.value(row, 2) {
                Some(QueryValue::RelRef(value)) => value.id().get(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            let source = match rows.value(row, 3) {
                Some(QueryValue::NodeRef(value)) => value.id().get(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            *destination = Some((group, count, relationship, source));
        }
        FrozenOutput::new(
            (output, rows.rows()),
            rows.rows(),
            rows.rows() * size_of::<AggregateTuple>(),
            0,
        )
        .map_err(Into::into)
    }
}

struct AggregateScopeConsumer {
    source: NodeId,
    chunk_rows: usize,
    later_pattern: PatternId,
}

impl
    NativeReadConsumer<
        Result<Execution<([Option<AggregateTuple>; 4], usize)>, RelationalExecutionFailure>,
    > for AggregateScopeConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<([Option<AggregateTuple>; 4], usize)>, RelationalExecutionFailure>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let lookup_inputs = [PlanNodeId(0)];
        let first_expand_inputs = [PlanNodeId(1)];
        let sort_inputs = [PlanNodeId(2)];
        let aggregate_inputs = [PlanNodeId(3)];
        let second_expand_inputs = [PlanNodeId(4)];
        let project_inputs = [PlanNodeId(5)];
        let collect_inputs = [PlanNodeId(6)];
        let sort_keys = [SortKey {
            expression: ExprId(0),
            descending: false,
        }];
        let keys = [
            Projection {
                slot: SlotId(1_000),
                expression: ExprId(1),
            },
            Projection {
                slot: SlotId(1_010),
                expression: ExprId(0),
            },
        ];
        let aggregates = [Projection {
            slot: SlotId(1_001),
            expression: ExprId(2),
        }];
        let projections = [
            Projection {
                slot: SlotId(2_000),
                expression: ExprId(3),
            },
            Projection {
                slot: SlotId(2_001),
                expression: ExprId(4),
            },
            Projection {
                slot: SlotId(2_002),
                expression: ExprId(5),
            },
            Projection {
                slot: SlotId(2_003),
                expression: ExprId(6),
            },
        ];
        let expressions = [
            Expression::Slot(SlotId(2)),
            Expression::Slot(SlotId(1)),
            Expression::Aggregate {
                operation: AggregateExpression::Count { distinct: false },
                operand: None,
            },
            Expression::Slot(SlotId(1_000)),
            Expression::Slot(SlotId(1_001)),
            Expression::Slot(SlotId(1_002)),
            Expression::Slot(SlotId(1_003)),
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &lookup_inputs,
                kind: OperatorKind::LookupNode {
                    output: SlotId(0),
                    id: self.source,
                },
            },
            Operator {
                inputs: &first_expand_inputs,
                kind: OperatorKind::Expand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationship: SlotId(2),
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &sort_inputs,
                kind: OperatorKind::Sort(&sort_keys),
            },
            Operator {
                inputs: &aggregate_inputs,
                kind: OperatorKind::Aggregate {
                    keys: keys
                        .get(..if self.chunk_rows == 1 { 2 } else { 1 })
                        .unwrap(),
                    aggregates: &aggregates,
                },
            },
            Operator {
                inputs: &second_expand_inputs,
                kind: OperatorKind::Expand {
                    source: SlotId(1_000),
                    node: SlotId(1_003),
                    relationship: SlotId(1_002),
                    direction: Direction::Incoming,
                    relationship_types: &[],
                    pattern: self.later_pattern,
                },
            },
            Operator {
                inputs: &project_inputs,
                kind: OperatorKind::Project(&projections),
            },
            Operator {
                inputs: &collect_inputs,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_relational_plan!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&lookup_inputs).unwrap(),
                RetainedRegion::slice(&first_expand_inputs).unwrap(),
                RetainedRegion::slice(&sort_inputs).unwrap(),
                RetainedRegion::slice(&aggregate_inputs).unwrap(),
                RetainedRegion::slice(&second_expand_inputs).unwrap(),
                RetainedRegion::slice(&project_inputs).unwrap(),
                RetainedRegion::slice(&collect_inputs).unwrap(),
                RetainedRegion::slice(&sort_keys).unwrap(),
                RetainedRegion::slice(&keys).unwrap(),
                RetainedRegion::slice(&aggregates).unwrap(),
                RetainedRegion::slice(&projections).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&lookup_inputs).unwrap(),
                RetainedAllocation::array(&first_expand_inputs).unwrap(),
                RetainedAllocation::array(&sort_inputs).unwrap(),
                RetainedAllocation::array(&aggregate_inputs).unwrap(),
                RetainedAllocation::array(&second_expand_inputs).unwrap(),
                RetainedAllocation::array(&project_inputs).unwrap(),
                RetainedAllocation::array(&collect_inputs).unwrap(),
                RetainedAllocation::array(&sort_keys).unwrap(),
                RetainedAllocation::array(&keys).unwrap(),
                RetainedAllocation::array(&aggregates).unwrap(),
                RetainedAllocation::array(&projections).unwrap(),
            ],
            &mut FreezeAggregateRows,
            chunk_rows = self.chunk_rows
        ))
    }
}

#[test]
fn native_relational_aggregate_native_empty_and_groups() {
    aggregate_chunk_fixture(16);
}

#[test]
fn native_relational_aggregate_spans_two_chunks() {
    aggregate_chunk_fixture(1);
}

fn aggregate_chunk_fixture(chunk_rows: usize) {
    let directory = tempfile::tempdir().expect("native relational aggregate store");
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .expect("create native relational aggregate store");
    let empty = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            EmptyAggregateConsumer,
        )
        .expect("admit empty aggregate view");
    assert!(
        empty.is_ok(),
        "native Aggregate must emit an empty global result: {}",
        empty.as_ref().err().unwrap()
    );
    assert_eq!(empty.unwrap().output, ([Some(0), None, None, None], 1));
    let empty_grouped = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            EmptyGroupedAggregateConsumer,
        )
        .expect("admit empty grouped aggregate view")
        .expect("execute empty grouped aggregate");
    assert_eq!(empty_grouped.output, 0);

    let receipts = crate::property_graph::with_local_refs(|refs| {
        let node_a = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let node_b = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        store
            .apply_native_graph(
                &[
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "aggregate", "a").unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&node_a)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "aggregate", "b").unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&node_b)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Relationship, "aggregate", "ab1")
                            .unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Local(refs.node(0).unwrap()),
                            target: NodeRef::Local(refs.node(1).unwrap()),
                            relationship_type: GraphName::new("LINKS").unwrap(),
                            properties: &[],
                        }),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Relationship, "aggregate", "ab2")
                            .unwrap(),
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
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("publish native relational aggregate fixture")
    });
    let node = |position: usize| match receipts[position].entity {
        EntityId::Node(id) => id,
        _ => panic!("aggregate node receipt kind"),
    };
    let relationship = |position: usize| match receipts[position].entity {
        EntityId::Relationship(id) => id,
        _ => panic!("aggregate relationship receipt kind"),
    };
    let same = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            AggregateScopeConsumer {
                source: node(0),
                chunk_rows,
                later_pattern: PatternId(0),
            },
        )
        .expect("admit same-pattern aggregate view")
        .expect("execute same-pattern aggregate");
    if chunk_rows == 1 {
        // Separate groups retain both representatives and their own uses.
        assert_eq!(same.output.1, 2);
        assert_eq!(
            same.output.0[..2],
            [
                Some((node(1).get(), 1, relationship(3).get(), node(0).get())),
                Some((node(1).get(), 1, relationship(2).get(), node(0).get())),
            ]
        );
        store.close().unwrap();
        return;
    }

    assert_eq!(same.output.1, 1);
    assert_eq!(
        same.output.0[0],
        Some((node(1).get(), 2, relationship(3).get(), node(0).get()))
    );
    let fresh = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            AggregateScopeConsumer {
                source: node(0),
                chunk_rows,
                later_pattern: PatternId(1),
            },
        )
        .expect("admit fresh-pattern aggregate view")
        .expect("execute fresh-pattern aggregate");
    assert_eq!(fresh.output.1, 2);
    assert_eq!(
        &fresh.output.0[..2],
        &[
            Some((node(1).get(), 2, relationship(2).get(), node(0).get())),
            Some((node(1).get(), 2, relationship(3).get(), node(0).get())),
        ]
    );
    let descriptors = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            AggregateDescriptorConsumer { source: node(0) },
        )
        .expect("admit aggregate descriptor view")
        .expect("execute aggregate descriptor view");
    assert_eq!(descriptors.output.0, 2);
    assert_eq!(descriptors.output.1, 0);
    assert_eq!(descriptors.output.3, 2);
    assert_eq!(
        &descriptors.output.2[..2],
        &[Some(relationship(2).get()), Some(relationship(3).get())]
    );
    store
        .close()
        .expect("close native relational aggregate store");
}

#[test]
fn native_relational_eligibility_singleton_domains() {
    qualify_native_eligibility_boundaries();
    let directory = tempfile::tempdir().expect("native relational eligibility store");
    let store = std::sync::Arc::new(
        Store::create_native_graph(
            directory.path().join("native"),
            OpenOptions::new()
                .with_durability(DurabilityMode::Durable, CommitTier::Durable)
                .with_max_resident_bytes(256 * 1024 * 1024),
            None,
        )
        .expect("create native relational eligibility store"),
    );
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let first = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let second = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        store
            .apply_native_graph(
                &[
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "eligibility", "first").unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&first)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "eligibility", "second")
                            .unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&second)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(
                            EntityKind::Relationship,
                            "eligibility",
                            "first-edge",
                        )
                        .unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Local(refs.node(0).unwrap()),
                            target: NodeRef::Local(refs.node(1).unwrap()),
                            relationship_type: GraphName::new("LINKS").unwrap(),
                            properties: &[],
                        }),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(
                            EntityKind::Relationship,
                            "eligibility",
                            "second-edge",
                        )
                        .unwrap(),
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
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("publish native relational eligibility fixture")
    });
    let mut expected: Vec<_> = receipts
        .iter()
        .take(2)
        .map(|receipt| match receipt.entity {
            EntityId::Node(id) => id.get(),
            _ => panic!("eligibility node receipt kind"),
        })
        .collect();
    expected.sort_unstable();
    let examined = super::test_support::run_actual_eligibility_probe(&store, &expected)
        .expect("prepare singleton eligibility");
    let source = match receipts[0].entity {
        EntityId::Node(id) => id,
        _ => panic!("eligibility source receipt kind"),
    };
    let target = match receipts[1].entity {
        EntityId::Node(id) => id,
        _ => panic!("eligibility target receipt kind"),
    };
    super::test_support::run_actual_eligibility_controls(
        &store,
        &expected,
        source,
        target,
        std::sync::Arc::clone(&store),
    )
    .expect("check singleton eligibility controls");
    store
        .close()
        .expect("close native relational eligibility store");
    assert_eq!(examined, 2);
}

struct FreezeOptionalMarker;

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for FreezeOptionalMarker {
    type Output = ([Option<(u128, Option<i64>)>; 4], usize);

    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeExecutionError> {
        if rows.rows() > 4 || rows.columns() != 3 {
            return Err(RuntimeError::Batch.into());
        }
        let mut output = [None; 4];
        for (row, destination) in output.iter_mut().enumerate().take(rows.rows()) {
            let node = match rows.value(row, 1) {
                Some(QueryValue::NodeRef(value)) => value.id().get(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            let marker = match rows.value(row, 2) {
                Some(QueryValue::Null) => None,
                Some(QueryValue::I64(value)) => Some(value),
                _ => return Err(RuntimeError::Batch.into()),
            };
            *destination = Some((node, marker));
        }
        FrozenOutput::new(
            (output, rows.rows()),
            rows.rows(),
            rows.rows() * size_of::<(u128, Option<i64>)>(),
            0,
        )
        .map_err(Into::into)
    }
}

struct AggregateOptionalConsumer;

impl
    NativeReadConsumer<
        Result<Execution<([Option<(u128, Option<i64>)>; 4], usize)>, RelationalExecutionFailure>,
    > for AggregateOptionalConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<([Option<(u128, Option<i64>)>; 4], usize)>, RelationalExecutionFailure>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let scan_inputs = [PlanNodeId(0)];
        let project_inputs = [PlanNodeId(1)];
        let aggregate_inputs = [PlanNodeId(2)];
        let optional_inputs = [PlanNodeId(2), PlanNodeId(3)];
        let sort_inputs = [PlanNodeId(4)];
        let collect_inputs = [PlanNodeId(5)];
        let left_projections = [
            Projection {
                slot: SlotId(0),
                expression: ExprId(2),
            },
            Projection {
                slot: SlotId(5),
                expression: ExprId(3),
            },
        ];
        let aggregates = [
            Projection {
                slot: SlotId(0),
                expression: ExprId(0),
            },
            Projection {
                slot: SlotId(1),
                expression: ExprId(1),
            },
        ];
        let sort_keys = [SortKey {
            expression: ExprId(4),
            descending: false,
        }];
        let expressions = [
            Expression::Aggregate {
                operation: AggregateExpression::Count { distinct: false },
                operand: None,
            },
            Expression::Aggregate {
                operation: AggregateExpression::Count { distinct: false },
                operand: None,
            },
            Expression::Literal(Literal::I64(99)),
            Expression::Slot(SlotId(5)),
            Expression::Slot(SlotId(5)),
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &scan_inputs,
                kind: OperatorKind::ScanNodes {
                    output: SlotId(5),
                    label: None,
                },
            },
            Operator {
                inputs: &project_inputs,
                kind: OperatorKind::Project(&left_projections),
            },
            Operator {
                inputs: &aggregate_inputs,
                kind: OperatorKind::Aggregate {
                    keys: &[],
                    aggregates: &aggregates,
                },
            },
            Operator {
                inputs: &optional_inputs,
                kind: OperatorKind::OptionalApply { predicate: None },
            },
            Operator {
                inputs: &sort_inputs,
                kind: OperatorKind::Sort(&sort_keys),
            },
            Operator {
                inputs: &collect_inputs,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_relational_plan!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&scan_inputs).unwrap(),
                RetainedRegion::slice(&project_inputs).unwrap(),
                RetainedRegion::slice(&aggregate_inputs).unwrap(),
                RetainedRegion::slice(&optional_inputs).unwrap(),
                RetainedRegion::slice(&sort_inputs).unwrap(),
                RetainedRegion::slice(&collect_inputs).unwrap(),
                RetainedRegion::slice(&aggregates).unwrap(),
                RetainedRegion::slice(&left_projections).unwrap(),
                RetainedRegion::slice(&sort_keys).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&scan_inputs).unwrap(),
                RetainedAllocation::array(&project_inputs).unwrap(),
                RetainedAllocation::array(&aggregate_inputs).unwrap(),
                RetainedAllocation::array(&optional_inputs).unwrap(),
                RetainedAllocation::array(&sort_inputs).unwrap(),
                RetainedAllocation::array(&collect_inputs).unwrap(),
                RetainedAllocation::array(&aggregates).unwrap(),
                RetainedAllocation::array(&left_projections).unwrap(),
                RetainedAllocation::array(&sort_keys).unwrap(),
            ],
            &mut FreezeOptionalMarker
        ))
    }
}

struct FreezeBarrierReset;

impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for FreezeBarrierReset {
    type Output = ([Option<(u128, i64)>; 4], usize);

    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeExecutionError> {
        if rows.rows() > 4 || rows.columns() != 3 {
            return Err(RuntimeError::Batch.into());
        }
        let mut output = [None; 4];
        for (row, destination) in output.iter_mut().enumerate().take(rows.rows()) {
            let node = match rows.value(row, 0) {
                Some(QueryValue::NodeRef(value)) => value.id().get(),
                _ => return Err(RuntimeError::Batch.into()),
            };
            let marker = match rows.value(row, 1) {
                Some(QueryValue::I64(value)) => value,
                _ => return Err(RuntimeError::Batch.into()),
            };
            if !matches!(rows.value(row, 2), Some(QueryValue::I64(1))) {
                return Err(RuntimeError::Batch.into());
            }
            *destination = Some((node, marker));
        }
        FrozenOutput::new(
            (output, rows.rows()),
            rows.rows(),
            rows.rows() * size_of::<(u128, i64)>(),
            0,
        )
        .map_err(Into::into)
    }
}

struct BarrierResetConsumer;

impl
    NativeReadConsumer<
        Result<Execution<([Option<(u128, i64)>; 4], usize)>, RelationalExecutionFailure>,
    > for BarrierResetConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        Result<Execution<([Option<(u128, i64)>; 4], usize)>, RelationalExecutionFailure>,
        crate::property_graph::storage::tree::directory::TreeError,
    > {
        let ordinal = String::from("ordinal");
        let scan_inputs = [PlanNodeId(0)];
        let project_inputs = [PlanNodeId(1)];
        let offset_inputs = [PlanNodeId(2)];
        let sort_inputs = [PlanNodeId(3)];
        let distinct_inputs = [PlanNodeId(4)];
        let aggregate_inputs = [PlanNodeId(5)];
        let join_inputs = [PlanNodeId(6), PlanNodeId(0)];
        let optional_inputs = [PlanNodeId(1), PlanNodeId(7)];
        let output_sort_inputs = [PlanNodeId(8)];
        let collect_inputs = [PlanNodeId(9)];
        let projections = [
            Projection {
                slot: SlotId(5),
                expression: ExprId(0),
            },
            Projection {
                slot: SlotId(6),
                expression: ExprId(1),
            },
        ];
        let sort_keys = [SortKey {
            expression: ExprId(2),
            descending: true,
        }];
        let output_sort_keys = [SortKey {
            expression: ExprId(2),
            descending: false,
        }];
        let keys = [Projection {
            slot: SlotId(6),
            expression: ExprId(2),
        }];
        let aggregates = [Projection {
            slot: SlotId(7),
            expression: ExprId(3),
        }];
        let expressions = [
            Expression::Slot(SlotId(5)),
            Expression::Property {
                entity: ExprId(0),
                name: GraphName::new(&ordinal).unwrap(),
            },
            Expression::Slot(SlotId(6)),
            Expression::Aggregate {
                operation: AggregateExpression::Count { distinct: false },
                operand: None,
            },
        ];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &scan_inputs,
                kind: OperatorKind::ScanNodes {
                    output: SlotId(5),
                    label: None,
                },
            },
            Operator {
                inputs: &project_inputs,
                kind: OperatorKind::Project(&projections),
            },
            Operator {
                inputs: &offset_inputs,
                kind: OperatorKind::OffsetLimit {
                    offset: 0,
                    limit: Some(1),
                },
            },
            Operator {
                inputs: &sort_inputs,
                kind: OperatorKind::Sort(&sort_keys),
            },
            Operator {
                inputs: &distinct_inputs,
                kind: OperatorKind::Distinct,
            },
            Operator {
                inputs: &aggregate_inputs,
                kind: OperatorKind::Aggregate {
                    keys: &keys,
                    aggregates: &aggregates,
                },
            },
            Operator {
                inputs: &join_inputs,
                kind: OperatorKind::Join { predicate: None },
            },
            Operator {
                inputs: &optional_inputs,
                kind: OperatorKind::OptionalApply { predicate: None },
            },
            Operator {
                inputs: &output_sort_inputs,
                kind: OperatorKind::Sort(&output_sort_keys),
            },
            Operator {
                inputs: &collect_inputs,
                kind: OperatorKind::Collect,
            },
        ];
        Ok(execute_relational_plan!(
            view,
            runtime,
            operators,
            expressions,
            vec![
                RetainedRegion::slice(&scan_inputs).unwrap(),
                RetainedRegion::slice(&project_inputs).unwrap(),
                RetainedRegion::slice(&offset_inputs).unwrap(),
                RetainedRegion::slice(&sort_inputs).unwrap(),
                RetainedRegion::slice(&distinct_inputs).unwrap(),
                RetainedRegion::slice(&aggregate_inputs).unwrap(),
                RetainedRegion::slice(&join_inputs).unwrap(),
                RetainedRegion::slice(&optional_inputs).unwrap(),
                RetainedRegion::slice(&output_sort_inputs).unwrap(),
                RetainedRegion::slice(&collect_inputs).unwrap(),
                RetainedRegion::slice(&projections).unwrap(),
                RetainedRegion::slice(&sort_keys).unwrap(),
                RetainedRegion::slice(&output_sort_keys).unwrap(),
                RetainedRegion::slice(&keys).unwrap(),
                RetainedRegion::slice(&aggregates).unwrap(),
                RetainedRegion::declared(ordinal.as_ptr() as usize, ordinal.capacity()).unwrap(),
            ],
            vec![
                RetainedAllocation::array(&scan_inputs).unwrap(),
                RetainedAllocation::array(&project_inputs).unwrap(),
                RetainedAllocation::array(&offset_inputs).unwrap(),
                RetainedAllocation::array(&sort_inputs).unwrap(),
                RetainedAllocation::array(&distinct_inputs).unwrap(),
                RetainedAllocation::array(&aggregate_inputs).unwrap(),
                RetainedAllocation::array(&join_inputs).unwrap(),
                RetainedAllocation::array(&optional_inputs).unwrap(),
                RetainedAllocation::array(&output_sort_inputs).unwrap(),
                RetainedAllocation::array(&collect_inputs).unwrap(),
                RetainedAllocation::array(&projections).unwrap(),
                RetainedAllocation::array(&sort_keys).unwrap(),
                RetainedAllocation::array(&output_sort_keys).unwrap(),
                RetainedAllocation::array(&keys).unwrap(),
                RetainedAllocation::array(&aggregates).unwrap(),
                RetainedAllocation::string(&ordinal).unwrap(),
            ],
            &mut FreezeBarrierReset
        ))
    }
}

#[test]
fn native_relational_barriers_join_optional_reset() {
    let directory = tempfile::tempdir().expect("native relational optional reset store");
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .expect("create native relational optional reset store");
    let receipts = crate::property_graph::with_local_refs(|_| {
        let mut first_properties = [GraphProperty::new(
            GraphName::new("ordinal").unwrap(),
            PropertyValue::new(PropertyData::I64(7)).unwrap(),
        )];
        let mut second_properties = [GraphProperty::new(
            GraphName::new("ordinal").unwrap(),
            PropertyValue::new(PropertyData::I64(8)).unwrap(),
        )];
        let first = CanonicalContents::node(&mut [], &mut first_properties, None, None).unwrap();
        let second = CanonicalContents::node(&mut [], &mut second_properties, None, None).unwrap();
        store
            .apply_native_graph(
                &[
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "reset", "first").unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&first)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "reset", "second").unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&second)),
                    },
                ],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("publish native relational optional reset fixture")
    });
    let mut expected: Vec<_> = receipts
        .iter()
        .map(|receipt| match receipt.entity {
            EntityId::Node(id) => Some((id.get(), None)),
            _ => panic!("optional reset node receipt kind"),
        })
        .collect();
    expected.sort_unstable();
    let execution = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            AggregateOptionalConsumer,
        )
        .expect("admit native relational optional reset view");
    assert!(
        execution.is_ok(),
        "aggregate right subtree must reset and preserve shared-slot equality: {}",
        execution.as_ref().err().unwrap()
    );
    let execution = execution.unwrap();
    assert_eq!(execution.output.1, 2);
    assert_eq!(&execution.output.0[..2], expected.as_slice());
    let reset = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            BarrierResetConsumer,
        )
        .expect("admit native relational barrier reset view")
        .expect("execute native relational barrier reset view");
    store
        .close()
        .expect("close native relational optional reset store");
    assert_eq!(reset.output.1, 2);
    assert_eq!(
        &reset.output.0[..2],
        &[
            Some((expected[0].unwrap().0, 7)),
            Some((expected[1].unwrap().0, 8)),
        ]
    );
}

fn relational_probe_receipts(
    report: &super::test_support::NativeRelationalProbeReport,
) -> std::collections::BTreeMap<&'static str, u64> {
    report.receipts.iter().copied().collect()
}

fn relational_receipt_oracle(receipts: &[(&'static str, u64)]) -> bool {
    const EXPECTED: [&str; 14] = [
        "pipeline",
        "representative",
        "group",
        "eligibility",
        "limit",
        "cancel",
        "late-error",
        "release",
        "same-seed",
        "oracle",
        "chunk-reservation",
        "variable-reservation",
        "row-cap",
        "streaming-retention",
    ];
    receipts.len() == EXPECTED.len()
        && receipts
            .iter()
            .zip(EXPECTED)
            .all(|((actual, value), expected)| *actual == expected && *value > 0)
}

fn relational_probe_oracle(
    observations: &[(u128, i64, u128, u128)],
    expected: &[(u128, i64, u128, u128)],
) -> bool {
    let observed = observations.iter().copied().fold(
        std::collections::BTreeMap::<(u128, i64, u128, u128), usize>::new(),
        |mut bag, row| {
            *bag.entry(row).or_default() += 1;
            bag
        },
    );
    let expected = expected.iter().copied().fold(
        std::collections::BTreeMap::<(u128, i64, u128, u128), usize>::new(),
        |mut bag, row| {
            *bag.entry(row).or_default() += 1;
            bag
        },
    );
    observed == expected && expected.values().all(|count| *count == 1)
}

#[test]
fn native_relational_limits_controls_errors_release() {
    use rand::RngCore;
    let mut rng = crate::test_support::seeded_rng("ze161-native-operator-failures");
    for _ in 0..3 {
        let seed = rng.next_u64();
        qualify_native_operator_failures(seed);
        eprintln!("ZE161 native failure fixture seed={seed:#x}");
        let report = super::test_support::run_actual_probe(seed)
            .unwrap_or_else(|error| panic!("seed={seed:#x}: {error}"));
        assert!(relational_probe_oracle(
            &report.observations,
            &report.expected
        ));
        assert!(relational_receipt_oracle(&report.receipts));
    }
    let report = super::test_support::run_actual_probe(0x5e15_4c01)
        .expect("run actual native relational control probe");
    let receipts = relational_probe_receipts(&report);
    assert_eq!(receipts.len(), 14);
    for name in ["limit", "cancel", "late-error", "release", "same-seed"] {
        assert!(
            receipts.get(name).copied().unwrap_or(0) > 0,
            "actual native relational control did not fire: {name}"
        );
    }
}

#[test]
fn native_relational_directed_probe_can_fire() {
    let report = super::test_support::run_actual_probe(0x5e15_4c01)
        .expect("run actual native relational directed probe");
    assert!(relational_probe_oracle(
        &report.observations,
        &report.expected
    ));
    let mut missing = report.observations.clone();
    let _ = missing.pop();
    assert!(!relational_probe_oracle(&missing, &report.expected));
    let mut duplicate = report.observations.clone();
    duplicate.extend(report.observations.iter().copied());
    assert!(!relational_probe_oracle(&duplicate, &report.expected));
    let mut wrong_representative = report.observations.clone();
    wrong_representative[0].2 ^= 1_u128 << 96;
    assert!(!relational_probe_oracle(
        &wrong_representative,
        &report.expected
    ));
    let receipts = relational_probe_receipts(&report);
    for name in [
        "pipeline",
        "representative",
        "group",
        "eligibility",
        "oracle",
    ] {
        assert!(
            receipts.get(name).copied().unwrap_or(0) > 0,
            "actual native relational evidence missing: {name}"
        );
    }
    assert!(relational_receipt_oracle(&report.receipts));
    for name in ["chunk-reservation", "variable-reservation", "row-cap"] {
        let mut absent = report.receipts.clone();
        absent.iter_mut().find(|(key, _)| *key == name).unwrap().1 = 0;
        assert!(
            !relational_receipt_oracle(&absent),
            "accepted non-firing {name}"
        );
    }

    let mut missing_receipt = report.receipts.clone();
    let _ = missing_receipt.pop();
    assert!(!relational_receipt_oracle(&missing_receipt));
    let mut duplicate_receipt = report.receipts.clone();
    duplicate_receipt.push(report.receipts[0]);
    assert!(!relational_receipt_oracle(&duplicate_receipt));
    let mut wrong_receipt = report.receipts.clone();
    wrong_receipt[1] = ("representative", 0);
    assert!(!relational_receipt_oracle(&wrong_receipt));
    let repeated = super::test_support::run_actual_probe(0x5e15_4c01)
        .expect("repeat actual native relational directed probe");
    assert_eq!(repeated.observations, report.observations);
    assert_eq!(repeated.expected, report.expected);
    assert_eq!(repeated.receipts, report.receipts);
}

#[test]
fn ze51_blocking_growth_stops_at_the_query_memory_cap_with_no_rows() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .unwrap();
    super::test_support::seed_capacity_store(&store, 2, true).unwrap();
    super::test_support::blocking_capacity_probe(&store).unwrap();
    store.close().unwrap();
}

#[test]
fn ze51_default_memory_blocking_capacity_measurement() {
    struct Measure(usize);
    impl NativeReadConsumer<(usize, usize, usize)> for Measure {
        fn consume<'s, 'lease, 'm, 'g>(
            &mut self,
            _: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
            runtime: &mut RuntimeContext<'lease, 'm, 'g>,
        ) -> Result<(usize, usize, usize), crate::property_graph::storage::tree::directory::TreeError>
        {
            use crate::property_graph::query::completed::GraphQueryOptions;
            use crate::property_graph::query::relational::Rows;
            use crate::property_graph::query::resources::MemoryError;
            let capacity = GraphQueryOptions::default().pattern.rows;
            let before = runtime.memory().reserved_bytes();
            let chunk = crate::property_graph::query::runtime::RowBatch::storage(
                runtime,
                1,
                capacity.rows,
                capacity.payload_bytes,
                capacity.variable,
            )
            .unwrap();
            let chunk_bytes = runtime.memory().reserved_bytes() - before;
            drop(chunk);
            let mut rows: Vec<_> = (0..self.0)
                .map(|_| Rows::new(runtime, &[SlotId(0)], capacity).unwrap())
                .collect();
            let mut uses = super::RowUses::new(capacity, runtime).unwrap();
            let initial = runtime.memory().reserved_bytes();
            let mut count = 0;
            loop {
                let result = (|| {
                    for row in &mut rows {
                        row.push(&[QueryValue::I64(count as i64)], runtime)?;
                    }
                    uses.push(&[], runtime)
                })();
                match result {
                    Ok(()) => count += 1,
                    Err(RuntimeError::Memory(MemoryError::Limit) | RuntimeError::BatchCapacity) => {
                        break;
                    }
                    other => panic!("unexpected capacity result: {other:?}"),
                }
            }
            eprintln!(
                "ZE51 stores={} chunk_bytes={chunk_bytes} rows_per_chunk={} initial_bytes={initial} retained_rows={count} final_bytes={} sidecar_descriptor_bytes={}",
                self.0,
                capacity.rows,
                runtime.memory().reserved_bytes(),
                size_of::<QueryArena<'_, '_, super::RelationshipUse>>()
            );
            Ok((count, chunk_bytes, initial))
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .unwrap();
    for stores in [1, 2] {
        let (count, _, _) = store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                crate::property_graph::query::MAX_QUERY_BYTES,
                64,
                Measure(stores),
            )
            .unwrap();
        assert!(
            count <= 65_536,
            "on-demand backing stops at query memory or the existing row cap"
        );
    }
    store.close().unwrap();
}

struct EligibilityDistributionConsumer {
    ids: Vec<NodeId>,
    full: bool,
}

impl NativeReadConsumer<()> for EligibilityDistributionConsumer {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        _: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), crate::property_graph::storage::tree::directory::TreeError> {
        use crate::property_graph::query::{QueryList, runtime::WorkKind};
        let before = runtime.memory().reserved_bytes();
        {
            let mut owner = runtime.memory().reserve_external_capacity().unwrap();
            owner
                .reserve_additional(self.ids.capacity() * size_of::<NodeId>())
                .unwrap();
            let view = runtime.view();
            let list = QueryList::copied_nodes(view, &self.ids);
            let run = |runtime: &mut RuntimeContext<'lease, 'm, 'g>| {
                super::eligibility::eligible_set(QueryValue::List(list), ExprId(0), runtime)
            };
            let owned = runtime.memory().reserved_bytes();
            let result = run(runtime);
            if self.full {
                assert!(matches!(
                    result,
                    Err(NativeExecutionError::Runtime(RuntimeError::Memory(
                        crate::property_graph::query::resources::MemoryError::Limit
                    )))
                ));
            } else {
                let set = result.unwrap();
                let mut expected = self.ids.clone();
                expected.sort_unstable();
                expected.dedup();
                assert_eq!(set.ids_for(view).unwrap(), expected);
                assert_eq!(
                    runtime.counters().get(WorkKind::EligibilityEntries),
                    self.ids.len() as u64
                );
                let probes = runtime.counters().get(WorkKind::HashProbes);
                assert!(probes >= self.ids.len() as u64 && probes < self.ids.len() as u64 * 20);
                drop(set);
                #[cfg(feature = "allocation-audit")]
                if self.ids.len() <= 16 {
                    use crate::adversarial_test_support::{
                        audit_engine_path, fail_attributed_allocation,
                    };
                    let (result, audit) = audit_engine_path(|| run(runtime).map(drop));
                    result.unwrap();
                    assert!(audit.allocations > 0);
                    assert_eq!(audit.unattributed_bytes, 0);
                    eprintln!(
                        "ZE161 native eligibility owner positions={}",
                        audit.allocations
                    );
                    for ordinal in 1..=audit.allocations {
                        let (result, fires) =
                            fail_attributed_allocation(ordinal, || run(runtime).map(drop));
                        assert_eq!(fires, 1);
                        assert!(matches!(
                            result,
                            Err(NativeExecutionError::Runtime(RuntimeError::Memory(_)))
                        ));
                        assert_eq!(runtime.memory().reserved_bytes(), owned);
                    }
                    run(runtime).unwrap();
                }
            }
            assert_eq!(runtime.memory().reserved_bytes(), owned);
        }
        assert_eq!(runtime.memory().reserved_bytes(), before);
        Ok(())
    }
}

fn qualify_native_eligibility_boundaries() {
    use rand::{RngCore, seq::SliceRandom};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .unwrap();
    let mut rng = crate::test_support::seeded_rng("ze161-native-eligibility-distributions");
    for distribution in 0..4 {
        let base: Vec<_> = (1..=3500u128)
            .map(|i| {
                NodeId::new(match distribution {
                    0 => (i << 64) | 7,
                    1 => (i << if i % 2 == 0 { 96 } else { 64 }) | 7,
                    2 => i,
                    _ => ((rng.next_u64() as u128) << 64) | i,
                })
                .unwrap()
            })
            .collect();
        for order in 0..3 {
            for duplicates in [false, true] {
                let mut ids = base.clone();
                if duplicates {
                    ids.extend_from_slice(&base);
                }
                match order {
                    1 => ids.reverse(),
                    2 => ids.shuffle(&mut rng),
                    _ => {}
                }
                store
                    .with_native_read(
                        &QueryControl::Cancel(CancelToken::new()),
                        RuntimeLimits::default(),
                        24 * 1024 * 1024,
                        64,
                        EligibilityDistributionConsumer { ids, full: false },
                    )
                    .unwrap();
            }
        }
    }
    store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            24 * 1024 * 1024,
            64,
            EligibilityDistributionConsumer {
                ids: vec![
                    NodeId::new(2).unwrap(),
                    NodeId::new(1).unwrap(),
                    NodeId::new(2).unwrap(),
                ],
                full: false,
            },
        )
        .unwrap();
    let ids = (1..=524_288).map(|i| NodeId::new(i).unwrap()).collect();
    store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            24 * 1024 * 1024,
            64,
            EligibilityDistributionConsumer { ids, full: true },
        )
        .unwrap();
    store.close().unwrap();
}

struct OperatorFailureConsumer {
    cancel: Option<CancelToken>,
    barrier: usize,
    rows: usize,
    payload: usize,
    audit: bool,
    #[cfg_attr(not(feature = "allocation-audit"), allow(dead_code))]
    sweep: bool,
    work: Option<crate::property_graph::query::runtime::WorkKind>,
}
struct FreezeOperatorFailure {
    called: bool,
    aggregate: bool,
}
impl<'m, 'g> Completion<'m, 'g, NativeExecutionError> for FreezeOperatorFailure {
    type Output = usize;
    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<usize>, NativeExecutionError> {
        self.called = true;
        assert_eq!(
            (rows.rows(), rows.columns()),
            (1, if self.aggregate { 1 } else { 4 })
        );
        assert!(
            matches!(rows.value(0,0), Some(QueryValue::I64(v)) if v == if self.aggregate { 1 } else { 7 })
        );
        FrozenOutput::new(1, 1, size_of::<usize>(), 0).map_err(Into::into)
    }
}
impl NativeReadConsumer<()> for OperatorFailureConsumer {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), crate::property_graph::storage::tree::directory::TreeError> {
        let text = String::from("poll");
        let inputs = [[PlanNodeId(0)], [PlanNodeId(1)], [PlanNodeId(2)]];
        let mut projection = vec![
            Projection {
                slot: SlotId(0),
                expression: ExprId(0),
            },
            Projection {
                slot: SlotId(1),
                expression: ExprId(1),
            },
        ];
        if self.barrier != 3 {
            projection.push(Projection {
                slot: SlotId(2),
                expression: ExprId(2),
            });
        }
        projection.push(Projection {
            slot: SlotId(3),
            expression: ExprId(3),
        });
        let keys = [SortKey {
            expression: ExprId(1),
            descending: false,
        }];
        let aggregates = [Projection {
            slot: SlotId(0),
            expression: ExprId(2),
        }];
        let expressions = vec![
            Expression::Literal(Literal::I64(7)),
            Expression::Literal(Literal::I64(7)),
            if self.barrier == 3 {
                Expression::Aggregate {
                    operation: AggregateExpression::Count { distinct: false },
                    operand: None,
                }
            } else {
                Expression::Literal(Literal::I64(7))
            },
            Expression::Literal(Literal::String(&text)),
        ];
        let operators = vec![
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &inputs[0],
                kind: OperatorKind::Project(&projection),
            },
            Operator {
                inputs: &inputs[1],
                kind: match self.barrier {
                    0 => OperatorKind::OffsetLimit {
                        offset: 0,
                        limit: Some(1),
                    },
                    1 => OperatorKind::Sort(&keys),
                    2 => OperatorKind::Distinct,
                    _ => OperatorKind::Aggregate {
                        keys: &[],
                        aggregates: &aggregates,
                    },
                },
            },
            Operator {
                inputs: &inputs[2],
                kind: OperatorKind::Collect,
            },
        ];
        let variable = ArenaCapacity {
            string_bytes: 4,
            list_cells: 0,
            node_ids: 0,
            relationship_ids: 0,
        };
        let pattern_capacity = PatternCapacity {
            rows: StorageCapacity {
                rows: 1,
                max_rows: 1,
                payload_bytes: 8192,
                variable,
            },
            expression: ExpressionCapacity {
                cells: 8,
                string_bytes: 4,
            },
        };
        let execution_capacity = ExecutionCapacity {
            batch_rows: 1,
            result_rows: self.rows,
            batch_payload_bytes: 8192,
            result_payload_bytes: self.payload,
            batch: variable,
            result: variable,
        };
        let before = runtime.memory().reserved_bytes();
        execute_relational_plan!(@admit runtime, operators, expressions,
        vec![RetainedRegion::slice(&inputs).unwrap(), RetainedRegion::vector(&projection).unwrap(),
            RetainedRegion::slice(&keys).unwrap(), RetainedRegion::slice(&aggregates).unwrap(),
        RetainedRegion::declared(text.as_ptr() as usize, text.capacity()).unwrap()],
        vec![RetainedAllocation::array(&inputs).unwrap(), RetainedAllocation::vector(&projection).unwrap(),
            RetainedAllocation::array(&keys).unwrap(), RetainedAllocation::array(&aggregates).unwrap(),
        RetainedAllocation::string(&text).unwrap()],
        vector, (admitted, root) => {
            let held = runtime.memory().reserved_bytes();
            let run = |runtime: &mut RuntimeContext<'lease, 'm, 'g>| {
                let mut completion = FreezeOperatorFailure { called: false, aggregate: self.barrier == 3 };
                let result = match NativePattern::new(view, &admitted, root, &[], pattern_capacity, runtime) {
                    Err(error) => Err(RelationalExecutionFailure::Build(error)),
                    Ok(mut source) => {
                        if let Some(cancel) = &self.cancel { source.cancel_after_expression_polls(0, cancel.clone()); }
                        execute_in(runtime, &admitted, &mut source, &mut completion,
                            execution_capacity).map_err(|error| RelationalExecutionFailure::Run(Box::new(error)))
                    },
                };
                assert_eq!(completion.called, result.is_ok(), "no completion after failure");
                result
            };
            if self.audit {
                #[cfg(feature = "allocation-audit")]
                if self.sweep {
                    use crate::adversarial_test_support::{audit_engine_path, fail_attributed_allocation};
                    let (result, audit) = audit_engine_path(|| run(runtime));
                    result.unwrap();
                    assert!(audit.allocations > 0);
                    assert_eq!(audit.unattributed_bytes, 0);
                    eprintln!("ZE161 barrier={} allocation positions={}", self.barrier, audit.allocations);
                    for ordinal in 1..=audit.allocations {
                        let (result, fires) = fail_attributed_allocation(ordinal, || run(runtime));
                        assert_eq!(fires, 1, "barrier={} ordinal={ordinal}", self.barrier);
                        let error = match result {
                            Err(RelationalExecutionFailure::Build(error)) => error,
                            Err(RelationalExecutionFailure::Run(failure)) => failure.error,
                            Ok(_) => panic!("allocation failure returned success"),
                        };
                        assert!(matches!(error, NativeExecutionError::Runtime(RuntimeError::Memory(_))
                            | NativeExecutionError::Expression(crate::property_graph::query::expression::ExpressionError {
                                failure: crate::property_graph::query::expression::ExpressionFailure::Runtime(RuntimeError::Memory(_)), .. })),
                            "barrier={} ordinal={ordinal}: {error:?}", self.barrier);
                        assert_eq!(runtime.memory().reserved_bytes(), held);
                    }
                }
                run(runtime).unwrap();
            } else if let Some(cancel) = &self.cancel {
                let result = run(runtime);
                assert!(cancel.is_cancelled(), "expression poll cancellation actually fired");
                assert!(matches!(result, Err(RelationalExecutionFailure::Run(ref failure))
                    if matches!(failure.error, NativeExecutionError::Expression(ref error)
                        if matches!(error.failure, crate::property_graph::query::expression::ExpressionFailure::Runtime(
                            RuntimeError::Value(crate::property_graph::query::QueryError::Cancelled))))
                        || matches!(failure.error, NativeExecutionError::Runtime(RuntimeError::Value(
                            crate::property_graph::query::QueryError::Cancelled)))));
            } else {
                let result = run(runtime);
                let failure = match result {
                    Err(RelationalExecutionFailure::Run(failure)) => failure,
                    Err(RelationalExecutionFailure::Build(error)) => panic!("unexpected build refusal: {error:?}"),
                            Ok(_) => panic!("execution refusal returned success"),
                };
                if let Some(expected) = self.work {
                    assert!(matches!(failure.error, NativeExecutionError::Runtime(RuntimeError::Limit(actual)) if actual == expected)
                        || matches!(failure.error, NativeExecutionError::Expression(ref error)
                            if matches!(error.failure, crate::property_graph::query::expression::ExpressionFailure::Runtime(RuntimeError::Limit(actual)) if actual == expected)),
                        "wrong work refusal: {:?}", failure.error);
                } else {
                    assert!(matches!(failure.error, NativeExecutionError::Runtime(RuntimeError::BatchCapacity)),
                        "wrong capacity refusal: {:?}", failure.error);
                }
            }
            assert_eq!(runtime.memory().reserved_bytes(), held);
        });
        assert_eq!(runtime.memory().reserved_bytes(), before);
        Ok(())
    }
}

fn qualify_native_operator_failures(seed: u64) {
    use crate::property_graph::query::runtime::WorkKind;
    let directory = tempfile::tempdir().unwrap();
    let store = Store::create_native_graph(
        directory.path().join("native"),
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .unwrap();
    for index in 0..4 {
        let barrier = (index + seed as usize % 4) % 4;
        for (rows, payload, work) in [
            (0, 8192, None),
            (1, 0, None),
            (1, 8192, Some(WorkKind::OperatorRows)),
            (1, 8192, Some(WorkKind::Expressions)),
            (1, 8192, Some(WorkKind::CopiedBytes)),
        ] {
            let limits = if let Some(work) = work {
                RuntimeLimits::default().with_limit(work, 0).unwrap()
            } else {
                RuntimeLimits::default()
            };
            store
                .with_native_read(
                    &QueryControl::Cancel(CancelToken::new()),
                    limits,
                    24 * 1024 * 1024,
                    64,
                    OperatorFailureConsumer {
                        cancel: None,
                        barrier,
                        rows,
                        payload,
                        audit: false,
                        sweep: false,
                        work,
                    },
                )
                .unwrap();
            store
                .with_native_read(
                    &QueryControl::Cancel(CancelToken::new()),
                    RuntimeLimits::default(),
                    24 * 1024 * 1024,
                    64,
                    OperatorFailureConsumer {
                        cancel: None,
                        barrier,
                        rows: 1,
                        payload: 8192,
                        audit: true,
                        sweep: false,
                        work: None,
                    },
                )
                .unwrap();
        }
    }
    for barrier in 0..4 {
        let cancel = CancelToken::new();
        let cancelled = store.with_native_read(
            &QueryControl::Cancel(cancel.clone()),
            RuntimeLimits::default(),
            24 * 1024 * 1024,
            64,
            OperatorFailureConsumer {
                cancel: Some(cancel),
                barrier,
                rows: 1,
                payload: 8192,
                audit: false,
                sweep: false,
                work: None,
            },
        );
        assert!(matches!(
            cancelled,
            Err(crate::lifecycle::native_graph::NativeGraphError::Read(
                crate::property_graph::storage::tree::directory::TreeError::Runtime(
                    RuntimeError::Value(crate::property_graph::query::QueryError::Cancelled)
                )
            ))
        ));
        store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                24 * 1024 * 1024,
                64,
                OperatorFailureConsumer {
                    cancel: None,
                    barrier,
                    rows: 1,
                    payload: 8192,
                    audit: true,
                    sweep: true,
                    work: None,
                },
            )
            .unwrap();
    }
    store.close().unwrap();
}
