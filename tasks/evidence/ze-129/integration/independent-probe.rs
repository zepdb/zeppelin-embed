#[path = "/tmp/ze-129-sol-review.90XCrN/source.rs"]
mod oracle;

use oracle::{EntityKind, Model, ObservationPlan, Operation};

fn main() {
    let a = (1_u128 << 100) | 11;
    let b = (1_u128 << 100) | 12;
    let rel = (1_u128 << 120) | 21;
    let mut model = Model::new();
    model
        .apply(
            1,
            &[
                Operation::CreateNode { id: a },
                Operation::CreateNode { id: b },
                Operation::CreateRelationship {
                    rel,
                    source: a,
                    target: b,
                    relationship_type: 7,
                },
            ],
        )
        .unwrap();

    let before_rejection = model
        .snapshot()
        .observation(&ObservationPlan::default())
        .unwrap();
    assert!(model
        .apply(
            2,
            &[
                Operation::DeleteNode {
                    id: a,
                    detach: false,
                },
                Operation::DeleteNode {
                    id: b,
                    detach: false,
                },
            ],
        )
        .is_err());
    assert_eq!(
        before_rejection,
        model
            .snapshot()
            .observation(&ObservationPlan::default())
            .unwrap()
    );

    model
        .apply(
            2,
            &[
                Operation::DeleteRelationship { rel },
                Operation::DeleteNode {
                    id: a,
                    detach: false,
                },
                Operation::DeleteNode {
                    id: b,
                    detach: false,
                },
            ],
        )
        .unwrap();
    let deleted = model
        .snapshot()
        .observation(&ObservationPlan::default())
        .unwrap();
    assert!(deleted.nodes.iter().all(|node| !node.live));
    assert!(deleted.raw_relationships.is_empty());

    assert!(model.apply(3, &[Operation::CreateNode { id: a }]).is_err());
    assert!(model
        .apply(
            3,
            &[Operation::CreateRelationship {
                rel,
                source: a,
                target: b,
                relationship_type: 7,
            }],
        )
        .is_err());

    let c = u128::MAX - 1;
    let d = u128::MAX;
    let rel2 = u128::MAX;
    model
        .apply(
            3,
            &[
                Operation::CreateNode { id: c },
                Operation::CreateNode { id: d },
                Operation::CreateRelationship {
                    rel: rel2,
                    source: c,
                    target: d,
                    relationship_type: u64::MAX,
                },
            ],
        )
        .unwrap();
    model
        .apply(
            4,
            &[
                Operation::PropertyOnly {
                    entity_kind: EntityKind::Relationship,
                    id: rel2,
                },
                Operation::DeleteNode {
                    id: c,
                    detach: true,
                },
            ],
        )
        .unwrap();
    let detached = model
        .snapshot()
        .observation(&ObservationPlan::default())
        .unwrap();
    assert_eq!(detached.raw_relationships.len(), 1);
    assert!(detached.visible_relationships.is_empty());
    model
        .apply(5, &[Operation::DeleteRelationship { rel: rel2 }])
        .unwrap();
}
