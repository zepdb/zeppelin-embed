#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

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
    Run(RuntimeFailure<NativeExecutionError>),
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
        .map_err(RelationalExecutionFailure::Run))
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
            &mut FreezeOffsetTuple
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
            SortScopeConsumer { source: node(0) },
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
        let with_projections = [Projection {
            slot: SlotId(1_000),
            expression: ExprId(1),
        }];
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
                kind: OperatorKind::With(&with_projections),
            },
            Operator {
                inputs: &distinct_inputs,
                kind: OperatorKind::Distinct,
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
            &mut FreezeOffsetTuple
        ))
    }
}

#[test]
fn native_relational_distinct_provenance_then_match() {
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
        let keys = [Projection {
            slot: SlotId(1_000),
            expression: ExprId(1),
        }];
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
                    keys: &keys,
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
            &mut FreezeAggregateRows
        ))
    }
}

#[test]
fn native_relational_aggregate_native_empty_and_groups() {
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
                later_pattern: PatternId(0),
            },
        )
        .expect("admit same-pattern aggregate view")
        .expect("execute same-pattern aggregate");
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
    const EXPECTED: [&str; 10] = [
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
    let report = super::test_support::run_actual_probe(0x5e15_4c01)
        .expect("run actual native relational control probe");
    let receipts = relational_probe_receipts(&report);
    assert_eq!(receipts.len(), 10);
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
