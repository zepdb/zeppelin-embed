//! ZE-191: real writer admission, commit and reopen for DETACH.
use super::*;

fn detach_node(id: NodeId, property: bool) -> Spec {
    Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::LookupNode(0, id)),
            (vec![1], K::Eager),
            (vec![2], K::Mutate(vec![M::DetachDelete(0)])),
            (vec![3], K::Project(vec![(100, 0), (101, 1)])),
            (vec![4], K::Collect),
        ],
        expressions: vec![
            E::Slot(0),
            if property {
                E::Property(0, "p")
            } else {
                E::I64(0)
            },
        ],
    }
}

#[test]
fn ze191_detach_committed_preserves_neighbours() {
    for target_is_b in [false, true] {
        let store = D2Store::create(None);
        let f = linked(&store);
        let (target, neighbour) = if target_is_b { (f.b, f.a) } else { (f.a, f.b) };
        let before = records(&store, &[neighbour, f.lonely], &[]);
        let generation = store.generation();
        let (rows, report) = committed(mutate(&store, &detach_node(target, false), IMAGES));
        assert_eq!(rows, vec![(target.get(), 0)]);
        assert_eq!(
            report.changed.map(GraphGeneration::get),
            Some(generation + 1)
        );
        assert_eq!(records(&store, &[neighbour, f.lonely], &[]), before);
        assert_eq!(read(&store, &scan_p()).len(), 2);
        assert!(relationship_rows(&store, f.r).is_empty());
        store.store.close().unwrap();
    }
}

#[test]
fn ze191_detach_hidden_edge_stays_hidden_after_reopen() {
    let store = D2Store::create(None);
    let f = linked(&store);
    committed(mutate(&store, &detach_node(f.a, false), IMAGES));
    let store = store.reopen();
    assert!(relationship_rows(&store, f.r).is_empty());
    assert_eq!(
        sorted(read(&store, &scan_p())),
        pairs(&[f.b, f.lonely], &[2, 3])
    );
    store.store.close().unwrap();
}

#[test]
fn ze191_detach_fresh_node_with_fresh_edge() {
    let store = D2Store::create(None);
    let spec = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::Eager),
            (
                vec![1],
                K::Mutate(vec![
                    M::CreateNode(0, &[]),
                    M::CreateNode(1, &[]),
                    M::Set(0, "p", 2),
                    M::Set(1, "p", 2),
                    M::CreateRelationship(2, 0, 1, "R"),
                    M::DetachDelete(0),
                ]),
            ),
            (vec![2], K::Project(vec![(100, 1), (101, 2)])),
            (vec![3], K::Collect),
        ],
        expressions: vec![E::Slot(0), E::Slot(1), E::I64(7)],
    };
    let (rows, report) = committed(mutate(&store, &spec, IMAGES));
    assert_eq!(report.disposition, BatchDisposition::Changed);
    let store = store.reopen();
    assert_eq!(read(&store, &scan_p()), rows);
    assert!(relationship_rows(&store, RelId::new(1).unwrap()).is_empty());
    store.store.close().unwrap();
}

#[test]
fn ze191_detach_with_explicit_incident_relationship_delete() {
    // Both plain DELETE r and DETACH DELETE r must stage Restrict for r.
    for relationship_item in [M::Delete(1), M::DetachDelete(1)] {
        let store = D2Store::create(None);
        let f = linked(&store);
        let before = records(&store, &[f.b, f.lonely], &[]);
        let spec = Spec {
            operators: vec![
                (vec![], K::Unit),
                (vec![0], K::LookupNode(0, f.a)),
                (vec![], K::Unit),
                (vec![2], K::LookupRelationship(1, f.r)),
                (vec![1, 3], K::Join),
                (vec![4], K::Eager),
                (
                    vec![5],
                    K::Mutate(vec![M::DetachDelete(0), relationship_item]),
                ),
                (vec![6], K::Project(vec![(100, 0), (101, 2)])),
                (vec![7], K::Collect),
            ],
            expressions: vec![E::Slot(0), E::Slot(1), E::I64(0)],
        };
        committed(mutate(&store, &spec, IMAGES));
        let store = store.reopen();
        assert!(relationship_rows(&store, f.r).is_empty());
        assert_eq!(records(&store, &[f.b, f.lonely], &[]), before);
        assert_eq!(read(&store, &scan_p()).len(), 2);
        store.store.close().unwrap();
    }
}

#[test]
fn ze191_detach_later_property_read_is_deleted_entity() {
    let store = D2Store::create(None);
    let f = linked(&store);
    let generation = store.generation();
    rejected(
        "later property read",
        mutate(&store, &detach_node(f.a, true), IMAGES),
        deleted_read,
    );
    assert_eq!(store.generation(), generation);
    let store = store.reopen();
    assert_eq!(
        sorted(read(&store, &scan_p())),
        pairs(&[f.a, f.b, f.lonely], &[1, 2, 3])
    );
    assert_eq!(relationship_rows(&store, f.r), vec![(f.r.get(), 0)]);
    store.store.close().unwrap();
}
