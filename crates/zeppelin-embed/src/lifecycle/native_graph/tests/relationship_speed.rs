//! Relationship-speed regressions at the admitted structured-query seam.
use super::*;
use crate::property_graph::query::completed::{CompletedGraphResult, GraphQueryOptions, Value};
use crate::property_graph::query::entry_probe::{Backing, run_plan};
use crate::property_graph::query::plan::{
    AggregateExpression, Direction, ExprId, Expression, Operator, OperatorKind, PatternId,
    PlanNodeId, Projection, SlotId,
};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::storage::preparation_work_capture;
use crate::property_graph::{
    ApplicationKey, CanonicalContents, EntityKind, GraphName, GraphRevision, NodeRef,
};

fn source_pairs(count: usize) -> (tempfile::TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), OpenOptions::new()).unwrap();
    store.enable_graph().unwrap();
    let node_keys: Vec<_> = (0..count * 2).map(|i| format!("node-{i}")).collect();
    let edge_keys: Vec<_> = (0..count).map(|i| format!("edge-{i}")).collect();
    crate::property_graph::with_local_refs(|refs| {
        let mut labels = [GraphName::new("Anchor").unwrap()];
        let source = CanonicalContents::node(&mut labels, &mut [], None, None).unwrap();
        let target = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let mut writes: Vec<_> = node_keys
            .iter()
            .enumerate()
            .map(|(i, key)| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "rel-speed", key).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(if i % 2 == 0 { &source } else { &target })),
            })
            .collect();
        writes.extend(
            edge_keys
                .iter()
                .enumerate()
                .map(|(i, key)| StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Relationship, "rel-speed", key).unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Relationship {
                        source: NodeRef::Local(refs.node(i * 2).unwrap()),
                        target: NodeRef::Local(refs.node(i * 2 + 1).unwrap()),
                        relationship_type: GraphName::new("R").unwrap(),
                        properties: &[],
                    }),
                }),
        );
        store
            .graph_apply(&writes, &QueryControl::Cancel(CancelToken::new()))
            .unwrap();
    });
    (directory, store)
}

fn counted_expansion(store: &Store, anchors: u64) -> CompletedGraphResult {
    store
        .execute_graph_statement(
            &QueryControl::Cancel(CancelToken::new()),
            &GraphQueryOptions::default(),
            |runtime, executor| {
                let label = String::from("Anchor");
                let unit = vec![PlanNodeId(0)];
                let scan = vec![PlanNodeId(1)];
                let limited = vec![PlanNodeId(2)];
                let expanded = vec![PlanNodeId(3)];
                let aggregates = vec![Projection {
                    slot: SlotId(3),
                    expression: ExprId(0),
                }];
                let expressions = vec![Expression::Aggregate {
                    operation: AggregateExpression::Count { distinct: false },
                    operand: None,
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
                            label: Some(GraphName::new(&label).unwrap()),
                        },
                    },
                    Operator {
                        inputs: &scan,
                        kind: OperatorKind::OffsetLimit {
                            offset: 0,
                            limit: Some(anchors),
                        },
                    },
                    Operator {
                        inputs: &limited,
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
                        inputs: &expanded,
                        kind: OperatorKind::Aggregate {
                            keys: &[],
                            aggregates: &aggregates,
                        },
                    },
                ];
                let mut backing = Backing::default();
                backing.string(&label)?;
                for values in [&unit, &scan, &limited, &expanded] {
                    backing.vec(values)?;
                }
                backing.vec(&aggregates)?;
                run_plan(
                    runtime,
                    executor,
                    &operators,
                    &expressions,
                    &Vec::new(),
                    &backing,
                    &["count"],
                )
            },
        )
        .unwrap()
}

#[test]
fn expand_scratch_is_reserved_once_per_operator() {
    let (_directory, store) = source_pairs(32);
    let mut peaks = Vec::new();
    for anchors in [0, 16, 32] {
        preparation_work_capture::start();
        let result = counted_expansion(&store, anchors);
        let report = preparation_work_capture::take();
        assert_eq!(result.cell(0, 0), Some(&Value::I64(anchors as i64)));
        let work: u64 = report.query_scratch.iter().map(|(work, _)| work).sum();
        eprintln!(
            "S2 anchors={anchors} scratch={:?} peak={}",
            report.query_scratch,
            result.metadata().peak_query_bytes
        );
        if anchors == 0 {
            assert!(report.query_scratch.is_empty(), "LIMIT 0 must stay lazy");
        } else {
            assert_eq!(
                report.query_scratch.len(),
                1,
                "one buffer for this Expand operator"
            );
            assert_eq!(work, 196_608, "charge the fixed merge backing once");
            peaks.push(result.metadata().peak_query_bytes);
        }
    }
    assert_eq!(
        peaks[0], peaks[1],
        "doubling anchors must not grow scratch or query peak"
    );
    store.close().unwrap();
}
