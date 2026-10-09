use std::collections::BTreeMap;
use zeppelin_embed_adversarial_oracle::graph_fixture::*;
use zeppelin_embed_adversarial_oracle::graph_lifecycle::*;

fn batch() -> Vec<Mutation> {
    vec![
        Mutation {
            key: Key {
                kind: Kind::Node,
                namespace: "fixture".into(),
                value: "a".into(),
            },
            operation: Operation::Create,
            revision: 1,
            expected: Expectation::Absent,
            detach: false,
            image: Some(Image::Node {
                labels: Default::default(),
                properties: Default::default(),
                text: Some("amber".into()),
                vector: None,
            }),
        },
        Mutation {
            key: Key {
                kind: Kind::Node,
                namespace: "fixture".into(),
                value: "b".into(),
            },
            operation: Operation::Create,
            revision: 1,
            expected: Expectation::Absent,
            detach: false,
            image: Some(Image::Node {
                labels: Default::default(),
                properties: Default::default(),
                text: None,
                vector: Some(vec![1, 2]),
            }),
        },
    ]
}
#[test]
fn plants_trip_named_comparators() {
    let batches = vec![batch()];
    let mut graph = Graph::default();
    graph.apply(&batches[0]).unwrap();
    let good = graph.snapshot();
    compare_complete_prefix(0, &batches, &[1], &good).unwrap();
    let mut partial = good.clone();
    partial.nodes.pop();
    assert!(
        compare_complete_prefix(0, &batches, &[0, 1], &partial)
            .unwrap_err()
            .starts_with(COMPLETE_PREFIX)
    );
    let ids = vec![
        Identity {
            relationship: false,
            key: "a".into(),
            id: 1,
            revision: 1,
            generation: 1,
            replayed: false,
        },
        Identity {
            relationship: false,
            key: "b".into(),
            id: 2,
            revision: 1,
            generation: 1,
            replayed: false,
        },
    ];
    compare_identity_history(&ids, &ids).unwrap();
    let mut reused = ids.clone();
    reused[1].id = 1;
    assert!(
        compare_identity_history(&ids, &reused)
            .unwrap_err()
            .starts_with(IDENTITY_HISTORY)
    );
    let protected = BTreeMap::from([("reader-root".into(), vec![4, 5])]);
    compare_protected_artifacts(&protected, &protected).unwrap();
    assert!(
        compare_protected_artifacts(&protected, &BTreeMap::new())
            .unwrap_err()
            .starts_with(PROTECTED_ARTIFACTS)
    );
    let expected = Outcome {
        error: Some("Io".into()),
        nothing_committed: false,
        stopped_error: Some("Stopped".into()),
    };
    compare_outcome(&expected, &expected).unwrap();
    let mut lying = expected.clone();
    lying.nothing_committed = true;
    assert!(
        compare_outcome(&expected, &lying)
            .unwrap_err()
            .starts_with(OUTCOME)
    );
}
