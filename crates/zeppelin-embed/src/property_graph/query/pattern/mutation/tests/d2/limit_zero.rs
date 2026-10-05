//! ZE-52: `LIMIT 0` bounds the rows a statement returns, never its writes.
//!
//! `OffsetLimit` whose limit is zero emits no row. When a `Mutate` sits
//! below it, the child is still drained, so every write the statement
//! names is staged and published. When nothing below it writes, the child
//! is never pulled, exactly as before: a read-only `LIMIT 0` does no work.
//! These reuse slice D2's plan specifications, stores and public-path
//! observations.

use super::d3::node_id;
use super::*;

/// `<spine> -> Project(n, <projected>) -> OffsetLimit(0, 0) -> Collect`,
/// where `spine` ends with the `Mutate` and `n` is slot 0.
fn limit_zero(mut spine: Vec<(Vec<u32>, K)>, projected: u32, expressions: Vec<E>) -> Spec {
    let last = u32::try_from(spine.len() - 1).unwrap();
    spine.push((vec![last], K::Project(vec![(100, 0), (101, projected)])));
    spine.push((vec![last + 1], K::Limit(0, Some(0))));
    spine.push((vec![last + 2], K::Collect));
    Spec {
        operators: spine,
        expressions,
    }
}

/// `MATCH (n) SET n.p = n.p + 1 RETURN n, n.p LIMIT 0`, the review's probe:
/// no row is returned, yet all three increments publish and reopen.
#[test]
fn ze52_limit_zero_still_applies_a_mutate_write() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let before = store.generation();
    let spec = limit_zero(
        vec![
            (vec![], K::Unit),
            (vec![0], K::Scan(0)),
            (vec![1], K::Eager),
            (vec![2], K::Mutate(vec![M::Set(0, "p", 3)])),
        ],
        1,
        vec![
            E::Slot(0),
            E::Property(0, "p"),
            E::I64(1),
            E::Arithmetic(Arithmetic::Add, 1, 2),
        ],
    );

    let (rows, report) = committed(mutate(&store, &spec, IMAGES));
    assert_eq!(rows, vec![], "LIMIT 0 returns no row");
    assert_eq!(report.disposition, BatchDisposition::Changed);
    assert_eq!(report.admitted.get(), before);
    assert_eq!(report.changed.map(GraphGeneration::get), Some(before + 1));

    let store = store.reopen();
    assert_eq!(store.generation(), before + 1);
    assert_eq!(sorted(read(&store, &scan_p())), pairs(&nodes, &[2, 3, 4]));
    assert_eq!(revisions(&store, &nodes), vec![2, 2, 2]);
    store.store.close().expect("close limit-zero store");
}

/// `CREATE (n:A) SET n.p = 5 RETURN n, 5 LIMIT 0`: no row is returned, yet
/// the node exists after reopen with the next identity after the fence.
#[test]
fn ze52_limit_zero_still_applies_a_create() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let fence = nodes.iter().map(|node| node.get()).max().unwrap();
    let before = store.generation();
    let spec = limit_zero(
        vec![
            (vec![], K::Unit),
            (vec![0], K::Eager),
            (
                vec![1],
                K::Mutate(vec![M::CreateNode(0, &["A"]), M::Set(0, "p", 1)]),
            ),
        ],
        1,
        vec![E::Slot(0), E::I64(5)],
    );

    let (rows, report) = committed(mutate(&store, &spec, IMAGES));
    assert_eq!(rows, vec![], "LIMIT 0 returns no row");
    assert_eq!(report.disposition, BatchDisposition::Changed);
    assert_eq!(report.changed.map(GraphGeneration::get), Some(before + 1));

    let store = store.reopen();
    assert_eq!(store.generation(), before + 1);
    let created = fence + 1;
    assert_eq!(revisions(&store, &[node_id(created)]), vec![1]);
    let mut expected = pairs(&nodes, &[1, 2, 3]);
    expected.push((created, 5));
    assert_eq!(sorted(read(&store, &scan_p())), sorted(expected));
    store.store.close().expect("close limit-zero store");
}

/// `MATCH (n) WHERE id(n) = <first> DELETE n RETURN n, 0 LIMIT 0`: no row is
/// returned, yet the node is gone after reopen. A statement whose only
/// effect is a created-then-deleted node commits its allocator fence too.
#[test]
fn ze52_limit_zero_still_applies_a_delete() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let before = store.generation();
    let spec = limit_zero(
        vec![
            (vec![], K::Unit),
            (vec![0], K::LookupNode(0, nodes[0])),
            (vec![1], K::Eager),
            (vec![2], K::Mutate(vec![M::Delete(0)])),
        ],
        1,
        vec![E::Slot(0), E::I64(0)],
    );

    let (rows, report) = committed(mutate(&store, &spec, IMAGES));
    assert_eq!(rows, vec![], "LIMIT 0 returns no row");
    assert_eq!(report.disposition, BatchDisposition::Changed);
    assert_eq!(report.changed.map(GraphGeneration::get), Some(before + 1));

    let fence_only = limit_zero(
        vec![
            (vec![], K::Unit),
            (vec![0], K::Eager),
            (
                vec![1],
                K::Mutate(vec![M::CreateNode(0, &[]), M::Delete(0)]),
            ),
        ],
        1,
        vec![E::Slot(0), E::I64(0)],
    );
    let (rows, report) = committed(mutate(&store, &fence_only, IMAGES));
    assert!(rows.is_empty());
    assert_eq!(report.disposition, BatchDisposition::Changed);
    assert_eq!(report.changed.map(GraphGeneration::get), Some(before + 2));
    assert_eq!(store.generation(), before + 2);

    let store = store.reopen();
    assert_eq!(store.generation(), before + 2);
    assert_eq!(sorted(read(&store, &scan_p())), pairs(&nodes[1..], &[2, 3]));
    assert_eq!(
        super::d3::structured_node(&store, "after").get(),
        nodes[2].get() + 2
    );
    store.store.close().expect("close limit-zero store");
}

/// `MATCH (n) SET n.p = n.p + 1 WITH n WHERE (MAX - 3) + n.p > 0 RETURN n,
/// n.p LIMIT <limit>`. After the SET the rows carry 2, 3 and 4, and the
/// filter overflows only on the row carrying 4. The whole child of a
/// write-bearing limit runs, so the statement fails, and publishes nothing,
/// whatever the limit: a limit that stopped after the first matching row
/// would never evaluate the failing row. Without the failing row the same
/// statement commits.
#[test]
fn ze52_limit_zero_drains_the_whole_write_subtree() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let before = store.generation();
    let spec = |offset: i64, limit: u64| Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::Scan(0)),
            (vec![1], K::Eager),
            (vec![2], K::Mutate(vec![M::Set(0, "p", 3)])),
            (vec![3], K::Filter(7)),
            (vec![4], K::Project(vec![(100, 0), (101, 1)])),
            (vec![5], K::Limit(0, Some(limit))),
            (vec![6], K::Collect),
        ],
        expressions: vec![
            E::Slot(0),
            E::Property(0, "p"),
            E::I64(1),
            E::Arithmetic(Arithmetic::Add, 1, 2),
            E::I64(i64::MAX - offset),
            E::Arithmetic(Arithmetic::Add, 4, 1),
            E::I64(0),
            E::Comparison(Comparison::Greater, 5, 6),
        ],
    };
    for limit in [0, 1] {
        match mutate(&store, &spec(3, limit), IMAGES) {
            (Err(NativeMutationError::Execution(NativeExecutionError::Expression(_))), false) => {}
            (Err(error), refused) => {
                panic!("LIMIT {limit}: wrong rejection {error:?} (refused {refused})")
            }
            (Ok((_, report)), _) => panic!("LIMIT {limit}: committed {:?}", report.changed),
        }
        assert_eq!(store.generation(), before);
    }

    let (rows, report) = committed(mutate(&store, &spec(4, 0), IMAGES));
    assert_eq!(rows, vec![]);
    assert_eq!(report.changed.map(GraphGeneration::get), Some(before + 1));
    let store = store.reopen();
    assert_eq!(sorted(read(&store, &scan_p())), pairs(&nodes, &[2, 3, 4]));
    store.store.close().expect("close limit-zero store");
}

/// Expressions for `WHERE 9223372036854775807 + n.p > 0`, whose evaluation
/// overflows on every fixture row, so any pulled row fails the statement.
fn overflowing() -> Vec<E> {
    vec![
        E::Slot(0),
        E::Property(0, "p"),
        E::I64(i64::MAX),
        E::Arithmetic(Arithmetic::Add, 2, 1),
        E::I64(0),
        E::Comparison(Comparison::Greater, 3, 4),
    ]
}

#[allow(
    clippy::result_large_err,
    reason = "test keeps typed failure path allocation-free"
)]
fn read_result(store: &D2Store, spec: &Spec) -> Result<Vec<(u128, i64)>, EagerExecutionFailure> {
    store
        .store
        .with_native_read(
            &control(),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            SpecRead { spec },
        )
        .expect("admit limit-zero read")
        .map(|rows| rows.0.iter().take(rows.1).flatten().copied().collect())
}

/// A `LIMIT 0` with no write below it still never pulls its child: a filter
/// that fails on every row it evaluates is not reached, with or without a
/// plain `Eager` below the limit, and in a mutation statement whose `LIMIT 0`
/// sits below the `Mutate`. Each control with `LIMIT 1` fails, which proves
/// the probe fires whenever a row is pulled.
#[test]
fn ze52_limit_zero_without_a_write_below_does_not_pull() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let before = store.generation();
    let read_spec = |eager: bool, limit: u64| {
        let mut operators = vec![
            (vec![], K::Unit),
            (vec![0], K::Scan(0)),
            (vec![1], K::Filter(5)),
        ];
        if eager {
            operators.push((vec![2], K::Eager));
        }
        let last = u32::try_from(operators.len() - 1).unwrap();
        operators.push((vec![last], K::Project(vec![(100, 0), (101, 1)])));
        operators.push((vec![last + 1], K::Limit(0, Some(limit))));
        operators.push((vec![last + 2], K::Collect));
        Spec {
            operators,
            expressions: overflowing(),
        }
    };
    for eager in [false, true] {
        assert_eq!(
            read_result(&store, &read_spec(eager, 0)).unwrap(),
            vec![],
            "read-only LIMIT 0 (eager {eager}) must not pull its child"
        );
        assert!(
            read_result(&store, &read_spec(eager, 1)).is_err(),
            "the LIMIT 1 control (eager {eager}) must reach the failing filter"
        );
    }

    // MATCH (n) WHERE <overflow> WITH n LIMIT <limit> SET n.p = 0 RETURN n, n.p
    let write_spec = |limit: u64| Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::Scan(0)),
            (vec![1], K::Filter(5)),
            (vec![2], K::Limit(0, Some(limit))),
            (vec![3], K::Eager),
            (vec![4], K::Mutate(vec![M::Set(0, "p", 4)])),
            (vec![5], K::Project(vec![(100, 0), (101, 1)])),
            (vec![6], K::Collect),
        ],
        expressions: overflowing(),
    };
    let (rows, report) = committed(mutate(&store, &write_spec(0), IMAGES));
    assert_eq!(rows, vec![]);
    assert_eq!(report.disposition, BatchDisposition::NoOp);
    assert!(
        mutate(&store, &write_spec(1), IMAGES).0.is_err(),
        "the LIMIT 1 control must reach the failing filter"
    );
    assert_eq!(store.generation(), before);
    assert_eq!(sorted(read(&store, &scan_p())), pairs(&nodes, &[1, 2, 3]));
    store.store.close().expect("close limit-zero store");
}
