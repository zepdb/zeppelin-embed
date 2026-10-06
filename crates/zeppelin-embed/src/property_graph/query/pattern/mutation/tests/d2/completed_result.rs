//! ZE-53 slice S2: completed results for write statements.
//!
//! These reuse slice D2's plan specifications, stores and public-path
//! observations, and run every statement through the real writer admission,
//! `Store::with_native_mutation_settled`, into an owned `CompletedGraphResult`.
//! Each result is checked against an oracle that does not read the result
//! collector: the values the plan computes, the identities the store's
//! allocation fences predict, and the records the public read path returns
//! after the commit, usually after a reopen.

use super::d3::node_id;
use super::*;
use crate::property_graph::GraphRevision;
use crate::property_graph::query::completed::{
    CompletedError, CompletedGraphResult, Key, Node, Outcome, Relationship, SourceError, Span,
    UnsettledWriteResult, Value,
};
use crate::property_graph::staging::ItemReceipt;

/// Runs one `Spec` as a write statement that returns a completed result.
struct SpecResult<'a> {
    spec: &'a Spec,
    columns: &'a [&'static str],
    invocations: &'a Cell<usize>,
}

impl NativeMutationConsumer<UnsettledWriteResult, NativeMutationError> for SpecResult<'_> {
    fn consume<'lease, 'm, 'g, 'w, 'i>(
        &mut self,
        view: &'w GraphReadView<'w, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
        overlay: GraphBatchReadView<'w, 'static>,
        images: &'w StatementImages<'i>,
        _control: &mut WriteControl<'_>,
    ) -> Result<(UnsettledWriteResult, GraphBatchReadView<'w, 'static>), NativeMutationError> {
        self.invocations.set(self.invocations.get() + 1);
        let columns: Vec<GraphName<'_>> = self
            .columns
            .iter()
            .map(|column| GraphName::new(column).unwrap())
            .collect();
        run_spec!(self.spec, view, runtime, PATTERN_ROWS; result overlay, images, &columns)
    }
}

/// Runs one `Spec` read-only into a completed result.
struct SpecReadResult<'a> {
    spec: &'a Spec,
    columns: &'a [&'static str],
}

impl NativeReadConsumer<Result<CompletedGraphResult, String>> for SpecReadResult<'_> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Result<CompletedGraphResult, String>, TreeError> {
        let columns: Vec<GraphName<'_>> = self
            .columns
            .iter()
            .map(|column| GraphName::new(column).unwrap())
            .collect();
        let result = run_spec!(self.spec, view, runtime, PATTERN_ROWS; read_result &columns);
        Ok(result.map_err(|failure| failure.to_string()))
    }
}

type Written = Result<(CompletedGraphResult, NativeMutationReport), NativeMutationError>;

/// Runs `spec` as one write statement and settles its result, counting how
/// often the statement ran and how often its result was settled.
#[allow(
    clippy::result_large_err,
    reason = "test keeps typed failure path allocation-free"
)]
fn write_counted(
    store: &D2Store,
    spec: &Spec,
    columns: &[&'static str],
    limits: RuntimeLimits,
    invocations: &Cell<usize>,
    settles: &Cell<usize>,
) -> Written {
    store.store.with_native_mutation_settled(
        &control(),
        limits,
        16 * 1024 * 1024,
        64,
        16,
        16,
        IMAGES,
        SpecResult {
            spec,
            columns,
            invocations,
        },
        |result: UnsettledWriteResult, receipts: &[ItemReceipt], changed| {
            settles.set(settles.get() + 1);
            result.settle(receipts, changed)
        },
    )
}

#[allow(
    clippy::result_large_err,
    reason = "test keeps typed failure path allocation-free"
)]
fn write(store: &D2Store, spec: &Spec, columns: &[&'static str]) -> Written {
    write_counted(
        store,
        spec,
        columns,
        RuntimeLimits::default(),
        &Cell::new(0),
        &Cell::new(0),
    )
}

fn written(outcome: Written) -> (CompletedGraphResult, NativeMutationReport) {
    outcome.unwrap_or_else(|error| panic!("write statement must commit: {error}"))
}

fn read_result(store: &D2Store, spec: &Spec, columns: &[&'static str]) -> CompletedGraphResult {
    store
        .store
        .with_native_read(
            &control(),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            SpecReadResult { spec, columns },
        )
        .expect("admit read result")
        .unwrap_or_else(|error| panic!("read result must complete: {error}"))
}

// ---------------------------------------------------------------------------
// Result observations, read only from the completed result's own pools
// ---------------------------------------------------------------------------

fn text(result: &CompletedGraphResult, span: Span) -> String {
    result.string(span).expect("valid result text").to_owned()
}

fn render(result: &CompletedGraphResult, value: Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(value) => format!("bool:{value}"),
        Value::I64(value) => format!("i64:{value}"),
        Value::F64(bits) => format!("f64:{bits:#x}"),
        Value::String(span) => format!("str:{}", text(result, span)),
        Value::Node(index) => format!("node:{index}"),
        Value::Relationship(index) => format!("rel:{index}"),
        Value::List { children, element } => {
            let pools = result.pools();
            let children =
                &pools.children[children.start as usize..(children.start + children.len) as usize];
            let items: Vec<String> = children
                .iter()
                .map(|child| render(result, pools.values[child.0 as usize]))
                .collect();
            format!("list<{element:?}>[{}]", items.join(","))
        }
    }
}

fn properties(result: &CompletedGraphResult, span: Span) -> Vec<(String, String)> {
    let pools = result.pools();
    pools.properties[span.start as usize..(span.start + span.len) as usize]
        .iter()
        .map(|property| {
            (
                text(result, property.name),
                render(result, pools.values[property.value.0 as usize]),
            )
        })
        .collect()
}

fn labels(result: &CompletedGraphResult, node: &Node) -> Vec<String> {
    result.pools().names[node.labels.start as usize..(node.labels.start + node.labels.len) as usize]
        .iter()
        .map(|span| text(result, *span))
        .collect()
}

fn key(result: &CompletedGraphResult, key: Option<Key>) -> Option<(String, String)> {
    key.map(|key| (text(result, key.namespace), text(result, key.value)))
}

fn node_at(result: &CompletedGraphResult, row: usize, column: usize) -> Node {
    match result.cell(row, column) {
        Some(Value::Node(index)) => result.pools().nodes[*index as usize],
        other => panic!("row {row} column {column} is not a node: {other:?}"),
    }
}

fn relationship_at(result: &CompletedGraphResult, row: usize, column: usize) -> Relationship {
    match result.cell(row, column) {
        Some(Value::Relationship(index)) => result.pools().relationships[*index as usize],
        other => panic!("row {row} column {column} is not a relationship: {other:?}"),
    }
}

fn i64_at(result: &CompletedGraphResult, row: usize, column: usize) -> i64 {
    match result.cell(row, column) {
        Some(Value::I64(value)) => *value,
        other => panic!("row {row} column {column} is not an i64: {other:?}"),
    }
}

fn committed_at(generation: u64) -> Outcome {
    Outcome::Committed {
        changed: GraphGeneration::new(generation),
    }
}

/// `(id, projected p, copied properties, revision, generation)` per row,
/// sorted by id.
type IncrementRow = (u128, i64, Vec<(String, String)>, u64, u64);

fn increment_rows(result: &CompletedGraphResult) -> Vec<IncrementRow> {
    let mut rows: Vec<_> = (0..result.metadata().rows as usize)
        .map(|row| {
            let node = node_at(result, row, 0);
            (
                node.id.get(),
                i64_at(result, row, 1),
                properties(result, node.properties),
                node.revision.get(),
                node.generation.get(),
            )
        })
        .collect();
    rows.sort();
    rows
}

fn p(value: i64) -> Vec<(String, String)> {
    vec![("p".into(), format!("i64:{value}"))]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `MATCH (n) SET n.p = n.p + 1 RETURN n, n.p`. Every returned node carries
/// the value this statement staged, not the admitted one, together with its
/// admitted key; its revision and generation are the ones the commit
/// published. The reopened store agrees with every field.
#[test]
fn ze53_slice_s2_return_after_set_carries_the_staged_values() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let before = store.generation();

    let (result, report) = written(write(&store, &increment_p(), &["n", "p"]));
    assert_eq!(report.disposition, BatchDisposition::Changed);
    assert_eq!(report.changed.map(GraphGeneration::get), Some(before + 1));
    assert_eq!(result.metadata().outcome, committed_at(before + 1));
    assert_eq!(result.metadata().generation.get(), before);
    assert_eq!(result.metadata().rows, 3);
    let expected: Vec<_> = nodes
        .iter()
        .zip([2, 3, 4])
        .map(|(node, value)| (node.get(), value, p(value), 2, before + 1))
        .collect();
    assert_eq!(increment_rows(&result), expected);
    let mut keys: Vec<_> = (0..3)
        .map(|row| key(&result, node_at(&result, row, 0).key))
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        ["one", "three", "two"].map(|key| Some(("d2".into(), key.into())))
    );

    let store = store.reopen();
    assert_eq!(store.generation(), before + 1);
    assert_eq!(sorted(read(&store, &scan_p())), pairs(&nodes, &[2, 3, 4]));
    assert_eq!(revisions(&store, &nodes), vec![2, 2, 2]);
    // The result is owned: it outlives the store it was read from.
    store.store.close().expect("close s2 store");
    assert_eq!(increment_rows(&result), expected);
}

/// `CREATE (n:B:A) SET n.p = 5, n.q = n.p + 1 RETURN n, n.q`. The created
/// node exists only in the statement's staged image, yet the result carries
/// it whole: the identity after the store's fence, its sorted labels and
/// both properties, no key, revision one and the published generation.
#[test]
fn ze53_slice_s2_return_of_a_created_node_carries_its_published_identity() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let fence = nodes.iter().map(|node| node.get()).max().unwrap();
    let before = store.generation();
    let spec = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::Eager),
            (
                vec![1],
                K::Mutate(vec![
                    M::CreateNode(0, &["B", "A"]),
                    M::Set(0, "p", 1),
                    M::Set(0, "q", 4),
                ]),
            ),
            (vec![2], K::Project(vec![(100, 0), (101, 5)])),
            (vec![3], K::Collect),
        ],
        expressions: vec![
            E::Slot(0),
            E::I64(5),
            E::Property(0, "p"),
            E::I64(1),
            E::Arithmetic(Arithmetic::Add, 2, 3),
            E::Property(0, "q"),
        ],
    };

    let (result, report) = written(write(&store, &spec, &["n", "q"]));
    let created = fence + 1;
    assert_eq!(report.changed.map(GraphGeneration::get), Some(before + 1));
    assert_eq!(result.metadata().outcome, committed_at(before + 1));
    assert_eq!(result.metadata().rows, 1);
    let node = node_at(&result, 0, 0);
    assert_eq!(node.id.get(), created);
    assert_eq!(labels(&result, &node), ["A", "B"]);
    assert_eq!(
        properties(&result, node.properties),
        vec![
            ("p".to_owned(), "i64:5".to_owned()),
            ("q".to_owned(), "i64:6".to_owned())
        ]
    );
    assert_eq!(key(&result, node.key), None);
    assert_eq!(node.revision.get(), 1);
    assert_eq!(node.generation.get(), before + 1);
    assert_eq!(i64_at(&result, 0, 1), 6);

    let store = store.reopen();
    assert_eq!(revisions(&store, &[node_id(created)]), vec![1]);
    let mut expected = pairs(&nodes, &[1, 2, 3]);
    expected.push((created, 5));
    assert_eq!(sorted(read(&store, &scan_p())), sorted(expected));
    store.store.close().expect("close s2 store");
}

/// `MATCH (a) CREATE (a)-[r:LINKS]->(m:M) SET r.w = 7, r.a = -1 RETURN r, m`.
/// The relationship's staged properties arrive in SET order, `w` before `a`;
/// the result orders them by name, as every copied property map is. Both
/// created entities carry the published revision and generation, and the
/// relationship its bound endpoints. The reopened record is exactly the
/// returned relationship.
#[test]
fn ze53_slice_s2_return_of_created_relationship_and_endpoint() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let fence = nodes.iter().map(|node| node.get()).max().unwrap();
    let before = store.generation();
    let spec = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::LookupNode(0, nodes[0])),
            (vec![1], K::Eager),
            (
                vec![2],
                K::Mutate(vec![
                    M::CreateNode(1, &["M"]),
                    M::CreateRelationship(2, 0, 1, "LINKS"),
                    M::Set(2, "w", 3),
                    M::Set(2, "a", 4),
                ]),
            ),
            (vec![3], K::Project(vec![(100, 2), (101, 1)])),
            (vec![4], K::Collect),
        ],
        expressions: vec![E::Slot(0), E::Slot(1), E::Slot(2), E::I64(7), E::I64(-1)],
    };

    let (result, _) = written(write(&store, &spec, &["r", "m"]));
    assert_eq!(result.metadata().outcome, committed_at(before + 1));
    let relationship = relationship_at(&result, 0, 0);
    let endpoint = node_at(&result, 0, 1);
    assert_eq!(endpoint.id.get(), fence + 1);
    assert_eq!(labels(&result, &endpoint), ["M"]);
    assert_eq!(properties(&result, endpoint.properties), vec![]);
    assert_eq!(
        (endpoint.revision.get(), endpoint.generation.get()),
        (1, before + 1)
    );
    assert_eq!(relationship.source, nodes[0]);
    assert_eq!(relationship.target, endpoint.id);
    assert_eq!(text(&result, relationship.relationship_type), "LINKS");
    assert_eq!(
        properties(&result, relationship.properties),
        vec![
            ("a".to_owned(), "i64:-1".to_owned()),
            ("w".to_owned(), "i64:7".to_owned())
        ]
    );
    assert_eq!(key(&result, relationship.key), None);
    assert_eq!(
        (relationship.revision.get(), relationship.generation.get()),
        (1, before + 1)
    );

    let store = store.reopen();
    let mut published = [
        GraphProperty::new(
            GraphName::new("w").unwrap(),
            PropertyValue::new(PropertyData::I64(7)).unwrap(),
        ),
        GraphProperty::new(
            GraphName::new("a").unwrap(),
            PropertyValue::new(PropertyData::I64(-1)).unwrap(),
        ),
    ];
    let image = CanonicalContents::relationship(
        nodes[0],
        endpoint.id,
        GraphName::new("LINKS").unwrap(),
        &mut published,
    )
    .unwrap();
    let mut bytes = Vec::new();
    image.write_to(&mut bytes, &mut || Ok(())).unwrap();
    assert_eq!(records(&store, &[], &[relationship.id]), vec![(1, bytes)]);
    store.store.close().expect("close s2 store");
}

/// `MATCH ()-[r]->() SET r.w = r.w + 10 RETURN r, r.w` over a keyed,
/// committed relationship. The returned relationship keeps its admitted key,
/// type and endpoints, carries the staged `w` beside the untouched `tag`, and
/// advances to revision two at the published generation.
#[test]
fn ze53_slice_s2_return_after_set_on_a_keyed_relationship() {
    let store = D2Store::create(None);
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let first = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let second = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let properties = [
            GraphProperty::new(
                GraphName::new("w").unwrap(),
                PropertyValue::new(PropertyData::I64(1)).unwrap(),
            ),
            GraphProperty::new(
                GraphName::new("tag").unwrap(),
                PropertyValue::new(PropertyData::String("t")).unwrap(),
            ),
        ];
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "s2", "a").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&first)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "s2", "b").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&second)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "s2", "ab").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &properties,
                }),
            },
        ];
        store
            .store
            .apply_native_graph(&requests, &control())
            .expect("publish relationship fixture")
    });
    let (source, target) = (node(&receipts[0]), node(&receipts[1]));
    let EntityId::Relationship(id) = receipts[2].entity else {
        panic!("relationship receipt");
    };
    let before = store.generation();
    let spec = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::LookupRelationship(0, id)),
            (vec![1], K::Eager),
            (vec![2], K::Mutate(vec![M::Set(0, "w", 3)])),
            (vec![3], K::Project(vec![(100, 0), (101, 1)])),
            (vec![4], K::Collect),
        ],
        expressions: vec![
            E::Slot(0),
            E::Property(0, "w"),
            E::I64(10),
            E::Arithmetic(Arithmetic::Add, 1, 2),
        ],
    };

    let (result, _) = written(write(&store, &spec, &["r", "w"]));
    assert_eq!(result.metadata().outcome, committed_at(before + 1));
    let relationship = relationship_at(&result, 0, 0);
    assert_eq!(relationship.id, id);
    assert_eq!((relationship.source, relationship.target), (source, target));
    assert_eq!(text(&result, relationship.relationship_type), "LINKS");
    assert_eq!(
        key(&result, relationship.key),
        Some(("s2".to_owned(), "ab".to_owned()))
    );
    assert_eq!(
        properties(&result, relationship.properties),
        vec![
            ("tag".to_owned(), "str:t".to_owned()),
            ("w".to_owned(), "i64:11".to_owned())
        ]
    );
    assert_eq!(
        (relationship.revision.get(), relationship.generation.get()),
        (2, before + 1)
    );
    assert_eq!(i64_at(&result, 0, 1), 11);

    let store = store.reopen();
    assert_eq!(records(&store, &[], &[id])[0].0, 2);
    assert_eq!(store.generation(), before + 1);
    store.store.close().expect("close s2 store");
}

/// `MATCH (n) DELETE n RETURN id(n)` returns the deleted node's full-width
/// identity text and commits. `MATCH (n) DELETE n RETURN n` asks for the
/// deleted node's contents, which no longer exist: the statement is refused
/// with a typed `Deleted` naming it, nothing commits, and the node is still
/// exactly as it was.
#[test]
fn ze53_slice_s2_deleted_reference_returns_but_its_contents_refuse() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let before = store.generation();
    let delete = |target: NodeId, projected: u32| Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::LookupNode(0, target)),
            (vec![1], K::Eager),
            (vec![2], K::Mutate(vec![M::Delete(0)])),
            (vec![3], K::Project(vec![(100, projected)])),
            (vec![4], K::Collect),
        ],
        // Every validated expression must be reachable, so the identity
        // text exists only in the plan that projects it.
        expressions: if projected == 0 {
            vec![E::Slot(0)]
        } else {
            vec![E::Slot(0), E::NodeIdText(0)]
        },
    };

    match write(&store, &delete(nodes[1], 0), &["n"]) {
        Err(NativeMutationError::Completed(CompletedError::Source(SourceError::Deleted(
            EntityId::Node(refused),
        )))) => assert_eq!(refused, nodes[1]),
        Err(error) => panic!("expected a typed Deleted, got {error}"),
        Ok((_, report)) => panic!("returning a deleted node committed {:?}", report.changed),
    }
    assert_eq!(store.generation(), before);
    assert_eq!(sorted(read(&store, &scan_p())), pairs(&nodes, &[1, 2, 3]));

    let (result, report) = written(write(&store, &delete(nodes[0], 1), &["id"]));
    assert_eq!(report.changed.map(GraphGeneration::get), Some(before + 1));
    assert_eq!(result.metadata().outcome, committed_at(before + 1));
    match result.cell(0, 0) {
        Some(Value::String(span)) => {
            assert_eq!(text(&result, *span), format!("{:032x}", nodes[0].get()));
        }
        other => panic!("expected identity text, got {other:?}"),
    }
    assert!(result.pools().nodes.is_empty());

    let store = store.reopen();
    assert_eq!(store.generation(), before + 1);
    assert_eq!(sorted(read(&store, &scan_p())), pairs(&nodes[1..], &[2, 3]));
    store.store.close().expect("close s2 store");
}

/// A limit that fires in the result's final copy, the last copied byte, drops
/// the whole statement: nothing commits, the generation and every value stay
/// as they were, and every query and writer reservation is released. The
/// same statement then commits unchanged.
#[test]
fn ze53_slice_s2_final_copy_failure_commits_nothing() {
    let clean = D2Store::create(None);
    three_nodes(&clean);
    let (measured, _) = written(write(&clean, &increment_p(), &["n", "p"]));
    let copied = measured.metadata().counters.get(WorkKind::CopiedBytes);
    assert!(copied > 0);
    clean.store.close().expect("close clean s2 store");

    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let before = store.generation();
    let shared = GraphResources::from_store(&store.store).expect("graph resources");
    let baseline = shared.reserved_bytes().expect("baseline reservation");
    let limited = RuntimeLimits::default()
        .with_limit(WorkKind::CopiedBytes, copied - 1)
        .unwrap();
    match write_counted(
        &store,
        &increment_p(),
        &["n", "p"],
        limited,
        &Cell::new(0),
        &Cell::new(0),
    ) {
        Err(NativeMutationError::Completed(CompletedError::Runtime(RuntimeError::Limit(
            WorkKind::CopiedBytes,
        )))) => {}
        Err(error) => panic!("expected the final copy's byte limit, got {error}"),
        Ok((_, report)) => panic!("a failed final copy committed {:?}", report.changed),
    }
    assert_eq!(store.generation(), before);
    assert_eq!(
        shared.reserved_bytes().expect("reservation after"),
        baseline
    );
    assert_eq!(sorted(read(&store, &scan_p())), pairs(&nodes, &[1, 2, 3]));
    assert_eq!(revisions(&store, &nodes), vec![1, 1, 1]);

    let (result, _) = written(write(&store, &increment_p(), &["n", "p"]));
    assert_eq!(
        result.metadata().counters.get(WorkKind::CopiedBytes),
        copied
    );
    assert_eq!(result.metadata().outcome, committed_at(before + 1));
    let store = store.reopen();
    assert_eq!(sorted(read(&store, &scan_p())), pairs(&nodes, &[2, 3, 4]));
    store.store.close().expect("close s2 store");
}

/// `MATCH (n) SET n.p = n.p RETURN n, n.p` changes nothing. The result
/// reports `NoOp`, and every node keeps the revision and generation it was
/// committed with.
#[test]
fn ze53_slice_s2_unchanged_statement_reports_noop_and_admitted_metadata() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let before = store.generation();
    let spec = scan_mutate(
        vec![M::Set(0, "p", 1)],
        1,
        vec![E::Slot(0), E::Property(0, "p")],
    );

    let (result, report) = written(write(&store, &spec, &["n", "p"]));
    assert_eq!(report.disposition, BatchDisposition::NoOp);
    assert_eq!(report.changed, None);
    assert_eq!(result.metadata().outcome, Outcome::NoOp);
    let expected: Vec<_> = nodes
        .iter()
        .zip([1, 2, 3])
        .map(|(node, value)| (node.get(), value, p(value), 1, before))
        .collect();
    assert_eq!(increment_rows(&result), expected);
    assert_eq!(store.generation(), before);
    store.store.close().expect("close s2 store");
}

/// `MATCH (n) SET n.p = 2 RETURN n, n.p` over `p` = 1, 2 and 3 changes two
/// nodes. Those two carry revision two at the published generation; the
/// node whose value was already 2 is unchanged by the statement and keeps
/// the revision and generation it was committed with.
#[test]
fn ze53_slice_s2_unchanged_rows_keep_their_metadata_beside_changed_ones() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let before = store.generation();
    let spec = scan_mutate(
        vec![M::Set(0, "p", 2)],
        1,
        vec![E::Slot(0), E::Property(0, "p"), E::I64(2)],
    );

    let (result, report) = written(write(&store, &spec, &["n", "p"]));
    assert_eq!(report.changed.map(GraphGeneration::get), Some(before + 1));
    assert_eq!(result.metadata().outcome, committed_at(before + 1));
    let expected: Vec<_> = nodes
        .iter()
        .zip([(2, before + 1), (1, before), (2, before + 1)])
        .map(|(node, (revision, generation))| (node.get(), 2, p(2), revision, generation))
        .collect();
    assert_eq!(increment_rows(&result), expected);

    let store = store.reopen();
    assert_eq!(revisions(&store, &nodes), vec![2, 1, 2]);
    store.store.close().expect("close s2 store");
}

/// A statement with no write, run through the writer admission, copies
/// exactly what the read path copies: every pool is identical. Only the
/// outcome differs, `NoOp` from the writer and `Read` from the reader.
#[test]
fn ze53_slice_s2_read_only_statement_matches_the_read_path() {
    let store = D2Store::create(Some(document()));
    let rich = Rich::original();
    rich.commit(&store);
    three_nodes(&store);

    let read = read_result(&store, &scan_p(), &["n", "p"]);
    let (written, report) = written(write(&store, &scan_p(), &["n", "p"]));
    assert_eq!(report.disposition, BatchDisposition::NoOp);
    assert_eq!(read.metadata().outcome, Outcome::Read);
    assert_eq!(written.metadata().outcome, Outcome::NoOp);
    assert_eq!(read.metadata().generation, written.metadata().generation);
    assert_eq!(read.metadata().rows, 4);
    let (r, w) = (read.pools(), written.pools());
    assert_eq!(r.values, w.values);
    assert_eq!(r.bytes, w.bytes);
    assert_eq!(r.columns, w.columns);
    assert_eq!(r.cells, w.cells);
    assert_eq!(r.children, w.children);
    assert_eq!(r.names, w.names);
    assert_eq!(r.properties, w.properties);
    assert_eq!(r.nodes, w.nodes);
    assert_eq!(r.relationships, w.relationships);
    store.store.close().expect("close s2 store");
}

/// The commit tail may checkpoint instead of committing. The whole attempt,
/// its copied result included, is then discarded and rebuilt against the
/// checkpointed generation, and only the attempt that commits is settled:
/// its result carries exactly what that commit published.
#[test]
fn ze53_slice_s2_checkpointed_attempt_is_rebuilt_and_settled_once() {
    let store = D2Store::create(None);

    store
        .store
        .set_native_graph_maintenance_policy(crate::property_graph::GraphMaintenancePolicy {
            automatic: false,
            ..crate::property_graph::GraphMaintenancePolicy::default()
        })
        .unwrap();
    let nodes = three_nodes(&store);
    // The fixture is one complete envelope; 63 more make the next commit
    // checkpoint first.
    for _ in 0..63 {
        committed(mutate(&store, &increment_p(), IMAGES));
    }
    let before = store.generation();
    assert_eq!(before, 64);

    let (invocations, settles) = (Cell::new(0), Cell::new(0));
    let (result, report) = written(write_counted(
        &store,
        &increment_p(),
        &["n", "p"],
        RuntimeLimits::default(),
        &invocations,
        &settles,
    ));
    assert_eq!(invocations.get(), 2);
    assert_eq!(settles.get(), 1);
    assert_eq!(report.admitted.get(), before);
    assert_eq!(report.changed.map(GraphGeneration::get), Some(before + 1));
    assert_eq!(result.metadata().outcome, committed_at(before + 1));
    let expected: Vec<_> = nodes
        .iter()
        .zip([65, 66, 67])
        .map(|(node, value)| (node.get(), value, p(value), 65, before + 1))
        .collect();
    assert_eq!(increment_rows(&result), expected);

    let store = store.reopen();
    assert_eq!(revisions(&store, &nodes), vec![65, 65, 65]);
    assert_eq!(
        sorted(read(&store, &scan_p())),
        pairs(&nodes, &[65, 66, 67])
    );
    store.store.close().expect("close s2 store");
}
