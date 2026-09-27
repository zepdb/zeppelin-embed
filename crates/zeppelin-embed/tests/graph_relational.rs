#![allow(clippy::expect_used, clippy::panic)]
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SnapshotLease, Store};
use zeppelin_embed::property_graph::query::plan::{PlanNodeId, SlotId};
use zeppelin_embed::property_graph::query::relational::*;
use zeppelin_embed::property_graph::query::resources::QueryMemory;
use zeppelin_embed::property_graph::query::runtime::*;
use zeppelin_embed::property_graph::query::{QueryError, QueryValue, QueryView};
use zeppelin_embed::property_graph::resources::GraphResources;
use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};

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
fn fixture(f: impl FnOnce(&mut RuntimeContext<'_, '_, '_>)) {
    let directory = tempfile::tempdir().expect("directory");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(64 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("resources");
    let memory = QueryMemory::new(&shared, 24 * 1024 * 1024).expect("memory");
    let view = View {
        token: QueryView::new(
            StoreInstanceId::new(1).expect("id"),
            GraphGeneration::new(0),
        ),
        lease: store.snapshot().expect("lease"),
    };
    let control = QueryControl::Cancel(CancelToken::new());
    let baseline = memory.reserved_bytes();
    {
        let mut context = RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default())
            .expect("context");
        f(&mut context);
    }
    assert_eq!(
        memory.reserved_bytes(),
        baseline,
        "all temporary owners released"
    );
    drop(view);
    store.close().expect("close");
}
fn capacity(rows: usize) -> StorageCapacity {
    StorageCapacity::new(
        rows,
        1024 * 1024,
        ArenaCapacity {
            string_bytes: 8192,
            list_cells: 4096,
            node_ids: 4096,
            relationship_ids: 64,
        },
    )
}

#[test]
fn sparse_slots_filter_project_and_limit_preserve_bags_across_batches() {
    fixture(|context| {
        let mut rows = Rows::new(context, &[SlotId(400), SlotId(7)], capacity(8)).expect("rows");
        for (value, predicate) in [
            (10, QueryValue::Null),
            (20, QueryValue::Bool(false)),
            (30, QueryValue::Bool(true)),
            (30, QueryValue::Bool(true)),
            (40, QueryValue::Bool(true)),
            (50, QueryValue::Bool(true)),
        ] {
            rows.push(&[QueryValue::I64(value), predicate], context)
                .expect("row");
        }
        let source = rows.into_source(PlanNodeId(1), context).expect("source");
        let mut operator = MapRows::new(
            context,
            PlanNodeId(2),
            source,
            &[SlotProjection {
                source: SlotId(400),
                output: SlotId(999),
            }],
            Some(SlotId(7)),
            1,
            Some(2),
            capacity(2),
        )
        .expect("operator");
        assert_eq!(operator.schema().slots(), &[SlotId(999)]);
        let mut output = RowBatch::new(context, 1, 1, 1024).expect("output");
        let mut observed = Vec::new();
        loop {
            output.clear();
            let state = operator.pull(context, &mut output).expect("pull");
            for row in 0..output.rows() {
                let Some(QueryValue::I64(value)) = output.value(row, 0) else {
                    panic!("integer")
                };
                observed.push(value);
            }
            if state == PullState::Done {
                break;
            }
        }
        assert_eq!(observed, [30, 40]);
        assert_eq!(
            context.counters().get(WorkKind::Expressions),
            0,
            "column reads are not expression evaluations"
        );
    });
}

#[test]
fn distinct_and_stable_sort_use_exact_recursive_query_equivalence() {
    use zeppelin_embed::property_graph::query::QueryList;
    fixture(|context| {
        let mut rows = Rows::new(context, &[SlotId(42)], capacity(16)).expect("rows");
        let list_a = [QueryValue::I64(1), QueryValue::Null];
        let list_b = [QueryValue::F64(1.0), QueryValue::Null];
        let a = QueryList::new(&list_a, context.values()).expect("list");
        let b = QueryList::new(&list_b, context.values()).expect("list");
        for value in [
            QueryValue::Null,
            QueryValue::Null,
            QueryValue::F64(f64::NAN),
            QueryValue::F64(f64::from_bits(0x7ff8000000000001)),
            QueryValue::I64(1),
            QueryValue::F64(1.0),
            QueryValue::I64(9007199254740993),
            QueryValue::F64(9007199254740992.0),
            QueryValue::List(a),
            QueryValue::List(b),
        ] {
            rows.push(&[value], context).expect("row");
        }
        let rows = rows
            .distinct(context)
            .expect("distinct")
            .sort(
                &[OrderKey {
                    slot: SlotId(42),
                    descending: false,
                }],
                context,
            )
            .expect("sort");
        assert_eq!(rows.len(), 6);
        assert!(matches!(rows.value(0, 0), Some(QueryValue::List(_))));
        assert!(matches!(rows.value(1, 0), Some(QueryValue::I64(1))));
        assert!(matches!(rows.value(2, 0), Some(QueryValue::F64(v)) if v == 9007199254740992.0));
        assert!(matches!(
            rows.value(3, 0),
            Some(QueryValue::I64(9007199254740993))
        ));
        assert!(matches!(rows.value(4, 0), Some(QueryValue::F64(v)) if v.is_nan()));
        assert!(matches!(rows.value(5, 0), Some(QueryValue::Null)));
        let mut tied = Rows::new(context, &[SlotId(99), SlotId(7)], capacity(5)).expect("rows");
        for (key, value) in [(2, 10), (1, 20), (2, 30), (1, 40), (2, 50)] {
            tied.push(&[QueryValue::I64(key), QueryValue::I64(value)], context)
                .expect("row");
        }
        let tied = tied
            .sort(
                &[OrderKey {
                    slot: SlotId(99),
                    descending: true,
                }],
                context,
            )
            .expect("stable");
        let order: Vec<_> = (0..tied.len())
            .map(|row| match tied.value(row, 1) {
                Some(QueryValue::I64(v)) => v,
                _ => panic!("integer"),
            })
            .collect();
        assert_eq!(order, [10, 30, 50, 20, 40]);
    });
}

#[test]
fn grouped_and_empty_global_aggregates_skip_null_and_keep_ordered_collect() {
    fixture(|context| {
        let specs = [
            AggregateColumn {
                output: SlotId(60),
                operation: Aggregate::CountAll,
            },
            AggregateColumn {
                output: SlotId(61),
                operation: Aggregate::Count {
                    slot: SlotId(9),
                    distinct: true,
                },
            },
            AggregateColumn {
                output: SlotId(62),
                operation: Aggregate::Collect {
                    slot: SlotId(9),
                    distinct: false,
                },
            },
            AggregateColumn {
                output: SlotId(63),
                operation: Aggregate::Collect {
                    slot: SlotId(9),
                    distinct: true,
                },
            },
        ];
        let keys = [SlotProjection {
            source: SlotId(8),
            output: SlotId(58),
        }];
        let mut rows = Rows::new(context, &[SlotId(8), SlotId(9)], capacity(8)).expect("rows");
        for (key, value) in [
            (QueryValue::Null, QueryValue::I64(3)),
            (QueryValue::Bool(true), QueryValue::Null),
            (QueryValue::Null, QueryValue::I64(1)),
            (QueryValue::Null, QueryValue::F64(1.0)),
            (QueryValue::Null, QueryValue::Null),
        ] {
            rows.push(&[key, value], context).expect("row");
        }
        let grouped = rows
            .aggregate(&keys, &specs, capacity(8), context)
            .expect("aggregate");
        assert_eq!(grouped.len(), 2);
        assert!(matches!(grouped.value(0, 1), Some(QueryValue::I64(4))));
        assert!(matches!(grouped.value(0, 2), Some(QueryValue::I64(2))));
        let Some(QueryValue::List(collected)) = grouped.value(0, 3) else {
            panic!("list")
        };
        assert_eq!(collected.len(), 3);
        assert!(matches!(collected.get(0), Some(QueryValue::I64(3))));
        assert!(matches!(collected.get(1), Some(QueryValue::I64(1))));
        assert!(matches!(collected.get(2), Some(QueryValue::F64(1.0))));
        let Some(QueryValue::List(distinct)) = grouped.value(0, 4) else {
            panic!("list")
        };
        assert_eq!(distinct.len(), 2);
        assert!(matches!(grouped.value(1, 1), Some(QueryValue::I64(1))));
        assert!(matches!(grouped.value(1, 2), Some(QueryValue::I64(0))));
        let empty = Rows::new(context, &[SlotId(8), SlotId(9)], capacity(0)).expect("empty");
        let global = empty
            .aggregate(&[], &specs, capacity(1), context)
            .expect("global");
        assert_eq!(global.len(), 1);
        assert!(matches!(global.value(0, 0), Some(QueryValue::I64(0))));
        assert!(matches!(global.value(0, 1), Some(QueryValue::I64(0))));
        assert!(matches!(global.value(0,2),Some(QueryValue::List(v)) if v.is_empty()));
        let empty = Rows::new(context, &[SlotId(8), SlotId(9)], capacity(0)).expect("empty");
        assert!(
            empty
                .aggregate(&keys, &specs, capacity(0), context)
                .expect("grouped empty")
                .is_empty()
        );
    });
}

#[test]
fn eligible_set_is_full_width_same_view_and_caps_materialized_ids_not_duplicates() {
    use zeppelin_embed::property_graph::NodeId;
    use zeppelin_embed::property_graph::query::eligibility::{Eligibility, EligibleNodeSet};
    fixture(|context| {
        let view = context.view();
        let low = NodeId::new(7).expect("id");
        let high = NodeId::new((1u128 << 100) | 7).expect("high id");
        let set = EligibleNodeSet::build(
            context,
            2,
            [view.node(high), view.node(low), view.node(high)],
        )
        .expect("set");
        assert_eq!(set.ids_for(view).expect("view"), &[low, high]);
        let foreign = QueryView::new(view.store(), view.generation());
        assert_eq!(
            set.ids_for(&foreign).expect_err("different token"),
            QueryError::ForeignView
        );
        assert!(EligibleNodeSet::build(context, 1, [foreign.node(low)]).is_err());
        let empty = EligibleNodeSet::build(context, 0, []).expect("empty set");
        assert!(matches!(Eligibility::Set(&empty), Eligibility::Set(_)));
        assert!(matches!(Eligibility::AllIndexed, Eligibility::AllIndexed));
        assert!(EligibleNodeSet::build(context, 1, [QueryValue::Null]).is_err());
        let before = context.counters().get(WorkKind::EligibilityEntries);
        let repeated =
            EligibleNodeSet::build(context, 1, std::iter::repeat_n(view.node(low), 524289))
                .expect("duplicates are work, not set cardinality");
        assert_eq!(repeated.ids_for(view).expect("view"), &[low]);
        assert_eq!(
            context.counters().get(WorkKind::EligibilityEntries) - before,
            524289
        );
        assert_eq!(repeated.capacity(), 1);
    });
}

#[test]
fn blocking_chain_sorts_then_collects_across_many_scheduling_batches() {
    fixture(|context| {
        let mut rows =
            Rows::new(context, &[SlotId(55)], capacity(600)).expect("blocking storage exceeds256");
        for value in (0..600).rev() {
            rows.push(&[QueryValue::I64(value)], context).expect("row");
        }
        let source = rows.into_source(PlanNodeId(1), context).expect("source");
        let sorted = BlockingRows::new(
            context,
            PlanNodeId(2),
            source,
            BlockingOperation::Sort(&[OrderKey {
                slot: SlotId(55),
                descending: false,
            }]),
            capacity(2),
            capacity(600),
            capacity(600),
        )
        .expect("sort stage");
        let aggregates = [AggregateColumn {
            output: SlotId(91),
            operation: Aggregate::Collect {
                slot: SlotId(55),
                distinct: false,
            },
        }];
        let mut collected = BlockingRows::new(
            context,
            PlanNodeId(3),
            sorted,
            BlockingOperation::Aggregate {
                keys: &[],
                columns: &aggregates,
            },
            capacity(3),
            capacity(600),
            capacity(1),
        )
        .expect("aggregate stage");
        let mut out =
            RowBatch::with_arenas(context, 1, 1, 1024 * 1024, capacity(1).variable).expect("out");
        assert_eq!(
            collected.pull(context, &mut out).expect("pull"),
            PullState::Done
        );
        let Some(QueryValue::List(values)) = out.value(0, 0) else {
            panic!("collect")
        };
        assert_eq!(values.len(), 600);
        assert!(matches!(values.get(0), Some(QueryValue::I64(0))));
        assert!(matches!(values.get(599), Some(QueryValue::I64(599))));
        assert_eq!(collected.schema().slots(), &[SlotId(91)]);
    });
}

#[test]
fn cancelled_empty_sort_and_source_preparation_fail_before_success() {
    let directory = tempfile::tempdir().expect("directory");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(64 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("resources");
    let memory = QueryMemory::new(&shared, 1024 * 1024).expect("memory");
    let view = View {
        token: QueryView::new(
            StoreInstanceId::new(1).expect("id"),
            GraphGeneration::new(0),
        ),
        lease: store.snapshot().expect("lease"),
    };
    let cancel = CancelToken::new();
    let control = QueryControl::Cancel(cancel.clone());
    let mut context =
        RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default()).expect("context");
    let rows = Rows::new(&context, &[], capacity(0)).expect("empty");
    let source_rows = Rows::new(&context, &[], capacity(0)).expect("empty");
    cancel.cancel();
    assert!(matches!(
        rows.sort(&[], &mut context),
        Err(RuntimeError::Value(QueryError::Cancelled))
    ));
    assert!(matches!(
        source_rows.into_source(PlanNodeId(1), &context),
        Err(RuntimeError::Value(QueryError::Cancelled))
    ));
}

#[test]
fn malformed_scopes_predicates_and_foreign_owners_fail_loudly() {
    fixture(|context| {
        assert!(Schema::new(context, &[SlotId(4), SlotId(4)]).is_err());
        assert!(
            Rows::new(
                context,
                &[],
                StorageCapacity::new(usize::MAX, 0, ArenaCapacity::default())
            )
            .is_err()
        );
        let mut rows = Rows::new(context, &[SlotId(4)], capacity(1)).expect("rows");
        assert!(rows.push(&[], context).is_err());
        rows.push(&[QueryValue::I64(1)], context).expect("row");
        assert!(rows.push(&[QueryValue::I64(2)], context).is_err());
        let source = rows.into_source(PlanNodeId(1), context).expect("source");
        let mut filter = MapRows::new(
            context,
            PlanNodeId(2),
            source,
            &[],
            Some(SlotId(4)),
            0,
            None,
            capacity(1),
        )
        .expect("filter");
        let mut output = RowBatch::new(context, 0, 1, 1024).expect("output");
        assert!(matches!(
            filter.pull(context, &mut output),
            Err(RuntimeError::Value(QueryError::Type))
        ));
        assert_eq!(output.rows(), 0);
        let source = Rows::new(context, &[SlotId(4)], capacity(0))
            .expect("rows")
            .into_source(PlanNodeId(1), context)
            .expect("source");
        assert!(
            MapRows::new(
                context,
                PlanNodeId(2),
                source,
                &[SlotProjection {
                    source: SlotId(999),
                    output: SlotId(1)
                }],
                None,
                0,
                None,
                capacity(1)
            )
            .is_err()
        );
        let rows = Rows::new(context, &[SlotId(4)], capacity(0)).expect("rows");
        assert!(
            rows.sort(
                &[OrderKey {
                    slot: SlotId(999),
                    descending: false
                }],
                context
            )
            .is_err()
        );
        let rows = Rows::new(context, &[SlotId(4)], capacity(0)).expect("rows");
        assert!(
            rows.aggregate(
                &[],
                &[AggregateColumn {
                    output: SlotId(3),
                    operation: Aggregate::Count {
                        slot: SlotId(999),
                        distinct: false
                    }
                }],
                capacity(1),
                context
            )
            .is_err()
        );
    });
}

#[test]
#[cfg(feature = "allocation-audit")]
fn allocation_failure_at_every_relational_owner_releases_real_capacity() {
    #[cfg(feature = "allocation-audit")]
    {
        use zeppelin_embed::adversarial_test_support::{
            audit_engine_path, fail_attributed_allocation,
        };
        fixture(|context| {
            let baseline = context.memory().reserved_bytes();
            let run = |context: &mut RuntimeContext<'_, '_, '_>| {
                let mut rows = Rows::new(context, &[SlotId(3)], capacity(8))?;
                for value in [3, 1, 3, 2] {
                    rows.push(&[QueryValue::I64(value)], context)?;
                }
                rows.distinct(context)?
                    .sort(
                        &[OrderKey {
                            slot: SlotId(3),
                            descending: false,
                        }],
                        context,
                    )?
                    .aggregate(
                        &[],
                        &[AggregateColumn {
                            output: SlotId(5),
                            operation: Aggregate::Collect {
                                slot: SlotId(3),
                                distinct: true,
                            },
                        }],
                        capacity(1),
                        context,
                    )
                    .map(drop)
            };
            let (result, audit) = audit_engine_path(|| run(context));
            result.expect("clean allocation path");
            assert_eq!(audit.unattributed_bytes, 0);
            assert!(audit.allocations > 10);
            eprintln!(
                "ZE125 relational allocation positions={} unattributed_bytes={}",
                audit.allocations, audit.unattributed_bytes
            );
            for nth in 1..=audit.allocations {
                let (result, fires) = fail_attributed_allocation(nth, || run(context));
                assert_eq!(fires, 1, "actual allocation {nth} can fire");
                assert!(
                    matches!(result, Err(RuntimeError::Memory(_))),
                    "allocation {nth}"
                );
                assert_eq!(context.memory().reserved_bytes(), baseline);
            }
            run(context).expect("same input clean control");
            assert_eq!(context.memory().reserved_bytes(), baseline);
        });
    }
}

#[test]
fn maximum_materialized_eligibility_fits_one_packed_owner_and_rejects_one_over() {
    use zeppelin_embed::property_graph::{
        NodeId,
        query::{MAX_LIST_ELEMENTS, eligibility::EligibleNodeSet},
    };
    fixture(|context| {
        let view = context.view();
        let before = context.memory().reserved_bytes();
        let ids = (1..=MAX_LIST_ELEMENTS).map(|id| view.node(NodeId::new(id as u128).expect("id")));
        let set = EligibleNodeSet::build(context, MAX_LIST_ELEMENTS, ids)
            .expect("full permitted ordered set");
        assert_eq!(
            set.ids_for(view).expect("same view").len(),
            MAX_LIST_ELEMENTS
        );
        eprintln!(
            "ZE125 maximum eligible ids={} capacity={} live_delta={} peak_query_bytes={}",
            set.ids_for(view).expect("view").len(),
            set.capacity(),
            context.memory().reserved_bytes() - before,
            context.memory().peak_reserved_bytes()
        );
        assert_eq!(
            context.memory().reserved_bytes() - before,
            MAX_LIST_ELEMENTS * 16 + std::mem::size_of_val(&set)
        );
        drop(set);
        assert_eq!(context.memory().reserved_bytes(), before);
        assert!(matches!(
            EligibleNodeSet::build(context, MAX_LIST_ELEMENTS + 1, []),
            Err(RuntimeError::Value(QueryError::ListLimit))
        ));
        assert!(
            EligibleNodeSet::build(
                context,
                1,
                [
                    view.node(NodeId::new(1).expect("id")),
                    view.node(NodeId::new(2).expect("id"))
                ]
            )
            .is_err()
        );
    });
}

#[test]
fn sort_counts_actual_input_rows_and_does_not_inherit_completed_row_cap() {
    fixture(|context| {
        let mut rows = Rows::new(
            context,
            &[SlotId(2)],
            StorageCapacity::new(70000, 1024 * 1024, ArenaCapacity::default()),
        )
        .expect("intermediate rows");
        for value in 0..70000 {
            rows.push(&[QueryValue::I64(value)], context).expect("row");
        }
        let before = context.counters().get(WorkKind::OperatorRows);
        let rows = rows
            .sort(
                &[OrderKey {
                    slot: SlotId(2),
                    descending: true,
                }],
                context,
            )
            .expect("sort");
        assert_eq!(
            context.counters().get(WorkKind::OperatorRows) - before,
            70000
        );
        assert_eq!(rows.len(), 70000);
        assert!(matches!(rows.value(0, 0), Some(QueryValue::I64(69999))));
        assert!(matches!(rows.value(69999, 0), Some(QueryValue::I64(0))));
    });
}

#[test]
fn collected_entities_stay_packed_and_nested_depth_is_rechecked() {
    use zeppelin_embed::property_graph::{NodeId, query::QueryList};
    fn nested<'v, 'm, 'g>(
        depth: usize,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        visit: &mut dyn for<'a> FnMut(
            QueryValue<'a>,
            &mut RuntimeContext<'v, 'm, 'g>,
        ) -> Result<(), RuntimeError>,
    ) -> Result<(), RuntimeError> {
        if depth == 0 {
            return visit(QueryValue::I64(7), context);
        }
        nested(depth - 1, context, &mut |child, context| {
            let values = [child];
            let list = QueryList::new(&values, context.values())?;
            visit(QueryValue::List(list), context)
        })
    }
    fixture(|context| {
        let view = context.view();
        let mut rows = Rows::new(context, &[SlotId(3)], capacity(3)).expect("rows");
        rows.push(
            &[view.node(NodeId::new((1u128 << 100) | 7).expect("id"))],
            context,
        )
        .expect("node");
        rows.push(&[view.node(NodeId::new(7).expect("id"))], context)
            .expect("node");
        let rows = rows
            .aggregate(
                &[],
                &[AggregateColumn {
                    output: SlotId(4),
                    operation: Aggregate::Collect {
                        slot: SlotId(3),
                        distinct: false,
                    },
                }],
                capacity(1),
                context,
            )
            .expect("collect");
        let Some(QueryValue::List(list)) = rows.value(0, 0) else {
            panic!("list")
        };
        assert_eq!(list.borrowed_bytes(), 32);
        assert!(
            matches!(list.get(0),Some(QueryValue::NodeRef(node)) if node.id().get()==(1u128<<100)|7)
        );
        nested(16, context, &mut |value, context| {
            let mut rows = Rows::new(context, &[SlotId(3)], capacity(1))?;
            rows.push(&[value], context)?;
            assert!(matches!(
                rows.aggregate(
                    &[],
                    &[AggregateColumn {
                        output: SlotId(4),
                        operation: Aggregate::Collect {
                            slot: SlotId(3),
                            distinct: false
                        }
                    }],
                    capacity(1),
                    context
                ),
                Err(RuntimeError::Value(QueryError::ListLimit))
            ));
            Ok(())
        })
        .expect("nested boundary");
    });
}

#[test]
#[cfg(feature = "allocation-audit")]
fn eligibility_allocation_failures_release_id_and_hash_scratch_owners() {
    #[cfg(feature = "allocation-audit")]
    {
        use zeppelin_embed::{
            adversarial_test_support::{audit_engine_path, fail_attributed_allocation},
            property_graph::{NodeId, query::eligibility::EligibleNodeSet},
        };
        fixture(|context| {
            let view = context.view();
            let before = context.memory().reserved_bytes();
            let run = |context: &mut RuntimeContext<'_, '_, '_>| {
                EligibleNodeSet::build(
                    context,
                    4,
                    [
                        view.node(NodeId::new(2).expect("id")),
                        view.node(NodeId::new(1).expect("id")),
                    ],
                )
                .map(drop)
            };
            let (result, audit) = audit_engine_path(|| run(context));
            result.expect("clean");
            assert_eq!(audit.allocations, 2);
            eprintln!(
                "ZE125 eligibility allocation positions={} unattributed_bytes={}",
                audit.allocations, audit.unattributed_bytes
            );
            assert_eq!(audit.unattributed_bytes, 0);
            for nth in 1..=audit.allocations {
                let (result, fires) = fail_attributed_allocation(nth, || run(context));
                assert_eq!(fires, 1);
                assert!(matches!(result, Err(RuntimeError::Memory(_))));
                assert_eq!(context.memory().reserved_bytes(), before);
            }
            run(context).expect("same input clean control");
        });
    }
}

#[test]
fn eligible_hashes_mix_high_identity_bits_before_bucket_selection() {
    use zeppelin_embed::property_graph::{NodeId, query::eligibility::EligibleNodeSet};
    fixture(|context| {
        let view = context.view();
        let set = EligibleNodeSet::build(
            context,
            7000,
            (1..=7000).map(|high| view.node(NodeId::new(((high as u128) << 64) | 7).expect("id"))),
        )
        .expect("high bits participate in bucket hash");
        assert_eq!(set.ids_for(view).expect("view").len(), 7000);
        assert!(
            context.counters().get(WorkKind::HashProbes) < 140000,
            "known non-adversarial fixture stays well below shared probe budget"
        );
    });
}

struct RepeatedRows<'m, 'g> {
    schema: Schema<'m, 'g>,
    remaining: usize,
}
impl<'v, 'm, 'g> RowOperator<'v, 'm, 'g> for RepeatedRows<'m, 'g> {
    fn schema(&self) -> &Schema<'m, 'g> {
        &self.schema
    }
}
impl<'v, 'm, 'g> PullOperator<'v, 'm, 'g> for RepeatedRows<'m, 'g> {
    fn node(&self) -> PlanNodeId {
        PlanNodeId(0)
    }
    fn prepare_search(
        &mut self,
        _: PlanNodeId,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::Batch)
    }
    fn pull(
        &mut self,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        output: &mut RowBatch<'v, 'm, 'g>,
    ) -> Result<PullState, RuntimeError> {
        while self.remaining > 0 && output.rows() < output.capacity() {
            output.push_row(&[QueryValue::I64((self.remaining % 7) as i64)], context)?;
            self.remaining -= 1;
        }
        Ok(if self.remaining == 0 {
            PullState::Done
        } else {
            PullState::More
        })
    }
}
fn ze255_bounded<'a>(operation: impl Fn() -> BlockingOperation<'a>, expected_rows: usize) {
    let mut peaks = Vec::new();
    for input_rows in [128, 200_000] {
        fixture(|context| {
            let baseline = context.memory().reserved_bytes();
            let child = RepeatedRows {
                schema: Schema::new(context, &[SlotId(0)]).expect("schema"),
                remaining: input_rows,
            };
            let small = StorageCapacity::new(32, 4096, ArenaCapacity::default());
            let mut operator = BlockingRows::new(
                context,
                PlanNodeId(1),
                child,
                operation(),
                small,
                small,
                small,
            )
            .expect("operator");
            let mut output =
                RowBatch::new(context, operator.schema().slots().len(), 32, 4096).expect("batch");
            let mut seen = 0;
            let mut total = 0;
            loop {
                output.clear();
                let state = operator
                    .pull(context, &mut output)
                    .expect("bounded streaming input");
                seen += output.rows();
                for row in 0..output.rows() {
                    if output.columns() == 2 {
                        let Some(QueryValue::I64(count)) = output.value(row, 1) else {
                            panic!("count")
                        };
                        total += count;
                    }
                }
                if state == PullState::Done {
                    break;
                }
            }
            assert_eq!(seen, expected_rows);
            if output.columns() == 2 {
                assert_eq!(total, input_rows as i64);
            }
            assert!(
                context.memory().peak_reserved_bytes() - baseline < 256 * 1024,
                "actual charged peak must depend on seven groups, not 200000 rows"
            );
            peaks.push(context.memory().peak_reserved_bytes() - baseline);
        });
    }
    assert_eq!(
        peaks[0], peaks[1],
        "retained allocation is independent of input row count"
    );
}
#[test]
fn ze255_streaming_groups_memory_bound() {
    ze255_bounded(
        || BlockingOperation::Aggregate {
            keys: &[SlotProjection {
                source: SlotId(0),
                output: SlotId(1),
            }],
            columns: &[AggregateColumn {
                output: SlotId(2),
                operation: Aggregate::CountAll,
            }],
        },
        7,
    );
}
#[test]
fn ze255_distinct_memory_bound() {
    ze255_bounded(|| BlockingOperation::Distinct, 7);
}
