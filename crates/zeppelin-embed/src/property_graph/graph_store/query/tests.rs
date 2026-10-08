//! ZE-66 S2: `Store::query`'s own exposure tests over the ZE-53 S3/S4
//! seam. These build the same tiny `MATCH (n) RETURN n, n.p` and
//! `MATCH (n) SET n.p = <assign> RETURN n, n.p` plans ZE-53 S3's
//! `entry_probe` fixture drives, but entirely through the public
//! `Store::query`/`GraphQueryPlan`/`GraphPlanBacking` surface, with no
//! access to `Store`, `RuntimeContext` or the raw builder seam.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests fail loudly on the first broken contract"
)]

use super::{GraphPlanBacking, GraphQueryPlan};
use crate::lifecycle::Store;
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl};
use crate::property_graph::query::completed::{CompletedGraphResult, GraphQueryOptions, Value};
use crate::property_graph::query::plan::{
    BinaryExpression, ExprId, Expression, Literal, Mutation, Operator, OperatorKind, Parameter,
    ParameterBinding, PlanNodeId, Projection, SlotId,
};
use crate::property_graph::query::{Arithmetic, QueryView};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::{
    ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphGeneration, GraphName,
    GraphProperty, GraphRevision, GraphStoreError, GraphStoreErrorKind, NodeId, PropertyData,
    PropertyValue, StoreInstanceId,
};

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

fn store_options() -> OpenOptions {
    OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024)
}

/// A graph store with three nodes whose `p` values are `base + 1`,
/// `base + 2` and `base + 3`, in node-ID order.
struct Fixture {
    _directory: tempfile::TempDir,
    store: Store,
    nodes: [NodeId; 3],
    values: [i64; 3],
}

impl Fixture {
    fn create(base: i64) -> Self {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = Store::create_graph(directory.path().join("native"), store_options(), None)
            .expect("graph store");
        let values = [base + 1, base + 2, base + 3];
        let p = GraphName::new("p").expect("property name");
        let mut properties = values.map(|value| {
            let value = PropertyValue::new(PropertyData::I64(value)).expect("property value");
            [GraphProperty::new(p, value)]
        });
        let mut images = Vec::new();
        for property in &mut properties {
            images
                .push(CanonicalContents::node(&mut [], property, None, None).expect("node image"));
        }
        let keys = ["one", "two", "three"];
        let mut requests = Vec::new();
        for (key, image) in keys.iter().zip(&images) {
            requests.push(StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze66-s2", key).expect("key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(image)),
            });
        }
        let result = store
            .graph_apply(&requests, &control())
            .expect("fixture batch");
        let mut nodes = Vec::new();
        for receipt in result.receipts() {
            match receipt.entity {
                EntityId::Node(node) => nodes.push(node),
                EntityId::Relationship(_) => panic!("fixture receipt names a relationship"),
            }
        }
        let nodes: [NodeId; 3] = nodes.try_into().expect("three fixture receipts");
        Self {
            _directory: directory,
            store,
            nodes,
            values,
        }
    }
}

/// `MATCH (n) RETURN n, n.p`, through `Store::query`.
fn read_p(store: &Store, control: &QueryControl) -> Result<CompletedGraphResult, GraphStoreError> {
    let p = String::from("p");
    let name = GraphName::new(&p).expect("name");
    let unit = vec![PlanNodeId(0)];
    let scan = vec![PlanNodeId(1)];
    let projections = vec![
        Projection {
            slot: SlotId(10),
            expression: ExprId(0),
        },
        Projection {
            slot: SlotId(11),
            expression: ExprId(1),
        },
    ];
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
    let expressions = vec![
        Expression::Slot(SlotId(0)),
        Expression::Property {
            entity: ExprId(0),
            name,
        },
    ];
    let eager: Vec<PlanNodeId> = Vec::new();
    let parameters: Vec<Parameter<'_>> = Vec::new();
    let mut backing = GraphPlanBacking::default();
    backing.string(&p).expect("backing string");
    backing.vec(&unit).expect("backing unit");
    backing.vec(&scan).expect("backing scan");
    backing.vec(&projections).expect("backing projections");
    let plan = GraphQueryPlan {
        operators: &operators,
        expressions: &expressions,
        parameters: &parameters,
        eager_searches: &eager,
        root: PlanNodeId(2),
        backing: &backing,
        bindings: &[],
        columns: &["n", "p"],
    };
    store.graph_query(control, &GraphQueryOptions::default(), &plan)
}

/// What a write statement assigns to every scanned node's `p`.
#[derive(Clone, Copy)]
enum Assign {
    /// `SET n.p = n.p + 1`.
    Increment,
    /// `SET n.p = 6 / (n.p - pole)`.
    DivideAround(i64),
}

/// `MATCH (n) SET n.p = <assign> RETURN n, n.p`, through `Store::query`.
fn write_p(
    store: &Store,
    control: &QueryControl,
    assign: Assign,
) -> Result<CompletedGraphResult, GraphStoreError> {
    let p = String::from("p");
    let name = GraphName::new(&p).expect("name");
    let unit = vec![PlanNodeId(0)];
    let scan = vec![PlanNodeId(1)];
    let eager_input = vec![PlanNodeId(2)];
    let mutate = vec![PlanNodeId(3)];
    let (value, mut expressions) = match assign {
        Assign::Increment => (
            ExprId(3),
            vec![
                Expression::Literal(Literal::I64(1)),
                Expression::Binary {
                    operation: BinaryExpression::Arithmetic(Arithmetic::Add),
                    left: ExprId(1),
                    right: ExprId(2),
                },
            ],
        ),
        Assign::DivideAround(pole) => (
            ExprId(5),
            vec![
                Expression::Literal(Literal::I64(pole)),
                Expression::Binary {
                    operation: BinaryExpression::Arithmetic(Arithmetic::Subtract),
                    left: ExprId(1),
                    right: ExprId(2),
                },
                Expression::Literal(Literal::I64(6)),
                Expression::Binary {
                    operation: BinaryExpression::Arithmetic(Arithmetic::Divide),
                    left: ExprId(4),
                    right: ExprId(3),
                },
            ],
        ),
    };
    expressions.splice(
        0..0,
        [
            Expression::Slot(SlotId(0)),
            Expression::Property {
                entity: ExprId(0),
                name,
            },
        ],
    );
    let mutations = vec![Mutation::SetProperty {
        entity: ExprId(0),
        name,
        value,
    }];
    let projections = vec![
        Projection {
            slot: SlotId(10),
            expression: ExprId(0),
        },
        Projection {
            slot: SlotId(11),
            expression: ExprId(1),
        },
    ];
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
            kind: OperatorKind::Eager,
        },
        Operator {
            inputs: &eager_input,
            kind: OperatorKind::Mutate(&mutations),
        },
        Operator {
            inputs: &mutate,
            kind: OperatorKind::Project(&projections),
        },
    ];
    let eager: Vec<PlanNodeId> = Vec::new();
    let parameters: Vec<Parameter<'_>> = Vec::new();
    let mut backing = GraphPlanBacking::default();
    backing.string(&p).expect("backing string");
    backing.vec(&unit).expect("backing unit");
    backing.vec(&scan).expect("backing scan");
    backing.vec(&eager_input).expect("backing eager input");
    backing.vec(&mutate).expect("backing mutate");
    backing.vec(&mutations).expect("backing mutations");
    backing.vec(&projections).expect("backing projections");
    let plan = GraphQueryPlan {
        operators: &operators,
        expressions: &expressions,
        parameters: &parameters,
        eager_searches: &eager,
        root: PlanNodeId(4),
        backing: &backing,
        bindings: &[],
        columns: &["n", "p"],
    };
    store.graph_query(control, &GraphQueryOptions::default(), &plan)
}

/// Unwraps a refusal. `CompletedGraphResult` is not `Debug` (it holds no
/// lease, but its pools are plain arrays with no cheap summary), so this
/// takes the place of `Result::expect_err`.
fn refused(
    result: Result<CompletedGraphResult, GraphStoreError>,
    context: &str,
) -> GraphStoreError {
    match result {
        Ok(_) => panic!("{context}: expected a refusal, observed a result"),
        Err(error) => error,
    }
}

/// `(node, p)` per row of a two-column `n, n.p` result, sorted by node.
fn node_values(result: &CompletedGraphResult) -> Vec<(u128, i64)> {
    let mut rows = Vec::new();
    for row in 0..result.metadata().rows as usize {
        let Some(Value::Node(index)) = result.cell(row, 0) else {
            panic!("row {row} has no node");
        };
        let Some(Value::I64(value)) = result.cell(row, 1) else {
            panic!("row {row} has no p");
        };
        let node = result
            .pools()
            .nodes
            .get(*index as usize)
            .expect("node record");
        rows.push((node.id.get(), *value));
    }
    rows.sort_unstable();
    rows
}

#[test]
fn graph_query_reads_nodes_and_properties_through_the_public_path() {
    let fixture = Fixture::create(100);
    let result = read_p(&fixture.store, &control()).expect("read plan");
    let expected: Vec<_> = fixture
        .nodes
        .iter()
        .zip(fixture.values)
        .map(|(node, value)| (node.get(), value))
        .collect();
    assert_eq!(node_values(&result), expected);
}

#[test]
fn graph_query_write_plan_commits_and_reads_back() {
    let fixture = Fixture::create(200);
    let written = write_p(&fixture.store, &control(), Assign::Increment).expect("write plan");
    let generation = match written.metadata().outcome {
        crate::property_graph::query::completed::Outcome::Committed { changed } => changed,
        other => panic!("expected Committed, observed {other:?}"),
    };
    // Manifest creation is generation 1, the fixture batch is 2, and SET is 3.
    assert_eq!(generation, GraphGeneration::new(3));
    let expected: Vec<_> = fixture
        .nodes
        .iter()
        .zip(fixture.values)
        .map(|(node, value)| (node.get(), value + 1))
        .collect();
    assert_eq!(node_values(&written), expected);

    // Committed by an independent read, not just by the writer's own result.
    let reread = read_p(&fixture.store, &control()).expect("read after write");
    assert_eq!(node_values(&reread), expected);
}

#[test]
fn graph_query_result_stays_valid_after_close() {
    let fixture = Fixture::create(300);
    let result = read_p(&fixture.store, &control()).expect("read plan");
    fixture.store.close_graph().expect("close graph store");
    // The owned result holds no lease or reservation on the closed store.
    let expected: Vec<_> = fixture
        .nodes
        .iter()
        .zip(fixture.values)
        .map(|(node, value)| (node.get(), value))
        .collect();
    assert_eq!(node_values(&result), expected);
}

#[test]
fn graph_query_refuses_a_division_by_zero_with_nothing_committed() {
    let fixture = Fixture::create(400);
    let before = read_p(&fixture.store, &control()).expect("read before");
    // One scanned row's p equals the pole, so its SET divides by zero.
    let error = refused(
        write_p(
            &fixture.store,
            &control(),
            Assign::DivideAround(fixture.values[0]),
        ),
        "division by zero",
    );
    assert_eq!(error.kind(), GraphStoreErrorKind::InvalidRequest);
    assert!(error.nothing_committed());
    let after = read_p(&fixture.store, &control()).expect("read after");
    assert_eq!(node_values(&after), node_values(&before));
}

#[test]
fn graph_query_refuses_an_already_cancelled_control() {
    let fixture = Fixture::create(500);
    let token = CancelToken::new();
    token.cancel();
    let error = refused(
        read_p(&fixture.store, &QueryControl::Cancel(token)),
        "already-cancelled control",
    );
    assert_eq!(error.kind(), GraphStoreErrorKind::Cancelled);
    assert!(error.nothing_committed());
}

#[test]
fn graph_query_write_on_read_only_store_reports_readonly() {
    // GraphStoreError::kind() reaches into GraphQueryError's own private
    // cause (GraphQueryCause::Graph(NativeGraphError::Store(..))) to tell
    // ReadOnly apart from other causes GraphQueryErrorKind folds into
    // Unavailable, the same precision apply_batch already has classifying
    // NativeGraphError directly. GraphQueryErrorKind itself is unchanged,
    // per the owner's decision; only GraphStoreError's mapping was widened.
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("native");
    let writer = Store::create_graph(&path, store_options(), None).expect("graph store");
    writer.close_graph().expect("close writer");
    drop(writer);

    let reader = Store::open_graph_read_only(&path, store_options(), None).expect("read-only open");
    let error = refused(
        write_p(&reader, &control(), Assign::Increment),
        "write on a read-only store",
    );
    assert_eq!(error.kind(), GraphStoreErrorKind::ReadOnly);
    assert!(error.nothing_committed());
    reader.close_graph().expect("close reader");
}

#[test]
fn graph_query_refuses_a_foreign_view_parameter_binding() {
    // A QueryView is a pure descriptor ("constructing it proves no entity
    // exists and acquires no store lease"), so any caller can build one
    // that does not belong to this call's real admission and bind a node
    // through it. ValueContext::checkpoint compares the bound value's view
    // against the admission's own view by pointer identity and refuses a
    // mismatch as QueryError::ForeignView, classified GraphQueryErrorKind::
    // InvalidPlan -> GraphStoreErrorKind::InvalidRequest.
    let fixture = Fixture::create(600);
    let p = String::from("p");
    let name = GraphName::new(&p).expect("name");
    let unit = vec![PlanNodeId(0)];
    let scan = vec![PlanNodeId(1)];
    let projections = vec![
        Projection {
            slot: SlotId(10),
            expression: ExprId(0),
        },
        Projection {
            slot: SlotId(11),
            expression: ExprId(1),
        },
    ];
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
    let expressions = vec![
        Expression::Slot(SlotId(0)),
        Expression::Property {
            entity: ExprId(0),
            name,
        },
    ];
    let eager: Vec<PlanNodeId> = Vec::new();
    let parameters: Vec<Parameter<'_>> = Vec::new();

    let foreign_view = QueryView::new(
        StoreInstanceId::new(1).expect("id"),
        GraphGeneration::new(1),
    );
    let binding_name = String::from("unused");
    let bindings = vec![ParameterBinding {
        name: &binding_name,
        value: foreign_view.node(fixture.nodes[0]),
    }];

    let mut backing = GraphPlanBacking::default();
    backing.string(&p).expect("backing string");
    backing.vec(&unit).expect("backing unit");
    backing.vec(&scan).expect("backing scan");
    backing.vec(&projections).expect("backing projections");
    backing.string(&binding_name).expect("backing binding name");
    backing.vec(&bindings).expect("backing bindings");

    let plan = GraphQueryPlan {
        operators: &operators,
        expressions: &expressions,
        parameters: &parameters,
        eager_searches: &eager,
        root: PlanNodeId(2),
        backing: &backing,
        bindings: &bindings,
        columns: &["n", "p"],
    };
    let error = refused(
        fixture
            .store
            .graph_query(&control(), &GraphQueryOptions::default(), &plan),
        "foreign-view parameter binding",
    );
    assert_eq!(error.kind(), GraphStoreErrorKind::InvalidRequest);
    assert!(error.nothing_committed());
    // `InvalidRequest` alone does not distinguish ForeignView from every
    // other cause folded into the same group (an unproved retained span, a
    // malformed plan, ...); the message pins the specific QueryError.
    let message = error.to_string();
    assert!(
        message.contains("another query view"),
        "expected a ForeignView rejection, observed: {message}"
    );
}

mod search;

#[test]
fn ze316_query_mutations_trigger_count_reclaim() {
    use crate::property_graph::GraphMaintenancePolicy;
    use crate::property_graph::storage::preparation_work_capture as capture;
    use std::sync::atomic::Ordering;
    let fixture = Fixture::create(0);
    fixture
        .store
        .set_graph_maintenance_policy(GraphMaintenancePolicy {
            automatic: true,
            reclaim_after_bytes: u64::MAX,
        })
        .unwrap();
    let native = fixture.store.store_for_test();
    // Fixture creation is one batch publication.
    assert_eq!(
        native
            .native_graph
            .commits_since_reclaim
            .load(Ordering::Relaxed),
        1
    );
    for _ in 0..31 {
        write_p(&fixture.store, &control(), Assign::Increment).unwrap();
    }
    assert_eq!(
        native
            .native_graph
            .commits_since_reclaim
            .load(Ordering::Relaxed),
        32
    );
    capture::start();
    write_p(&fixture.store, &control(), Assign::Increment).unwrap();
    let report = capture::take();
    assert!(
        report
            .phases
            .iter()
            .any(|(phase, _)| *phase == "maintenance-start")
    );
    let debt = native
        .native_graph
        .commits_since_reclaim
        .load(Ordering::Relaxed);
    if report
        .phases
        .iter()
        .any(|(phase, _)| *phase == "maintenance-retired")
    {
        assert_eq!(debt, 1);
    } else {
        assert!(debt >= 32, "fold-only query maintenance preserves debt");
    }
}
