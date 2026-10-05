//! Public paused-query proof shared by directed qualification and the runner.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::result_large_err
)]
use super::graph_search::*;
use zeppelin_embed_adversarial_oracle::graph_fixture as oracle;
pub fn retained(seed: u64) {
    let _ = retained_observation(seed);
}
pub fn retained_observation(seed: u64) -> zeppelin_embed_bench::harness_json::Value {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    use std::time::{Duration, Instant};
    use zeppelin_embed::lifecycle::{Deadline, MonotonicClock, QueryControl};
    use zeppelin_embed::property_graph::staging::*;
    use zeppelin_embed::property_graph::*;
    struct Gate {
        polls: AtomicUsize,
        nth: usize,
        reached: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
        base: Instant,
    }
    impl MonotonicClock for Gate {
        fn now(&self) -> Instant {
            if self.polls.fetch_add(1, Ordering::SeqCst) == self.nth {
                self.reached.send(()).unwrap();
                self.resume
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(20))
                    .unwrap();
            }
            self.base
        }
    }
    let c = Corpus::new();
    let q = "CALL ze.vector_search([0,0],20,'exact') YIELD node,distance MATCH (meeting)-[:HAS_CHUNK]->(node) RETURN node,distance,node.excerpt,ze.stored_text(node),meeting,meeting.name ORDER BY distance,node,meeting";
    let old = c.run(q);
    let snapshot = c.snapshot();
    let expected: Vec<_> = oracle::query(
        &snapshot,
        &oracle::Query::AliceProjectRanking {
            person: 2,
            project: 1,
            vector: vec![0.0; 2],
            k: 20,
        },
    )
    .unwrap()
    .into_iter()
    .map(|r| {
        let oracle::Cell::Node(id) = r[0] else {
            panic!("node truth")
        };
        let text = snapshot
            .nodes
            .iter()
            .find(|n| n.id == id)
            .unwrap()
            .text
            .clone()
            .map_or(oracle::Cell::Null, oracle::Cell::String);
        vec![
            r[0].clone(),
            r[1].clone(),
            r[2].clone(),
            text,
            r[3].clone(),
            oracle::Cell::String("meeting".into()),
        ]
    })
    .collect();
    oracle::compare_scored_rows(&expected, &observe(&old), ABSOLUTE, RELATIVE).unwrap();
    let (reached, receive) = mpsc::channel();
    let (_send, resume) = mpsc::channel();
    let counter = Arc::new(Gate {
        polls: AtomicUsize::new(0),
        nth: usize::MAX,
        reached,
        resume: Mutex::new(resume),
        base: Instant::now(),
    });
    let ctrl = QueryControl::Deadline(
        Deadline::after_with_test_clock(Duration::from_secs(60), counter.clone()).unwrap(),
    );
    counter.polls.store(0, Ordering::SeqCst);
    let r = zeppelin_embed_cypher::execute(
        c.graph().statement_store(),
        &ctrl,
        &Default::default(),
        q,
        &[],
        Default::default(),
    )
    .unwrap();
    assert_eq!(observe(&r), expected);
    drop(r);
    let polls = counter.polls.load(Ordering::SeqCst);
    drop(receive);
    let (reached, receive) = mpsc::channel();
    let (send, resume) = mpsc::channel();
    let nth = polls / 2;
    let gate = Arc::new(Gate {
        polls: AtomicUsize::new(0),
        nth,
        reached,
        resume: Mutex::new(resume),
        base: Instant::now(),
    });
    let ctrl = QueryControl::Deadline(
        Deadline::after_with_test_clock(Duration::from_secs(60), gate.clone()).unwrap(),
    );
    gate.polls.store(0, Ordering::SeqCst);
    let write = |corpus: &Corpus| {
        let tower = zeppelin_embed::graph_commit_recovery_test_support::document();
        let coords = [if seed & 1 == 0 { 0.125 } else { 0.25 }, 0.0];
        let mut labels = [GraphName::new("Eligible").unwrap()];
        let mut props = [GraphProperty::new(
            GraphName::new("excerpt").unwrap(),
            PropertyValue::new(PropertyData::String("changed")).unwrap(),
        )];
        let content = CanonicalContents::node(
            &mut labels,
            &mut props,
            Some("quartz"),
            Some(CanonicalEmbedding::new(&tower, &coords).unwrap()),
        )
        .unwrap();
        corpus
            .graph()
            .apply_batch(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "ze65", "b").unwrap(),
                    revision: GraphRevision::new(2).unwrap(),
                    operation: StructuredOperation::Put(EntityId::Node(NodeId::new(7).unwrap())),
                    image: Some(WriteImage::Node(&content)),
                }],
                &control(),
            )
            .unwrap();
        corpus
            .graph()
            .apply_batch(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Relationship, "ze65", "retained-added")
                        .unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Relationship {
                        source: NodeRef::Existing(NodeId::new(3).unwrap()),
                        target: NodeRef::Existing(NodeId::new(7).unwrap()),
                        relationship_type: GraphName::new("HAS_CHUNK").unwrap(),
                        properties: &[],
                    }),
                }],
                &control(),
            )
            .unwrap();
        corpus.graph().maintain_cycle(&control()).unwrap();
    };
    // Writer/maintenance bookkeeping control uses the same publications.
    let clean = Corpus::new();
    write(&clean);
    let baseline = clean.store.as_ref().unwrap().reserved();
    let result = std::thread::scope(|scope| {
        let task = scope.spawn(|| {
            zeppelin_embed_cypher::execute(
                c.graph().statement_store(),
                &ctrl,
                &Default::default(),
                q,
                &[],
                Default::default(),
            )
            .unwrap()
        });
        receive.recv_timeout(Duration::from_secs(20)).unwrap();
        write(&c);
        send.send(()).unwrap();
        task.join().unwrap()
    });
    assert_eq!(observe(&result), expected);
    assert_eq!(result.metadata().generation, old.metadata().generation);
    assert_eq!(result.pools().reports, old.pools().reports);
    for node in result.pools().nodes {
        let expected = snapshot
            .nodes
            .iter()
            .find(|n| n.id == node.id.get())
            .unwrap();
        assert_eq!(node.revision.get(), expected.revision);
        assert_eq!(node.generation.get(), expected.generation);
    }
    let later = c.run(q);
    let replaced = later
        .pools()
        .nodes
        .iter()
        .find(|n| n.id.get() == 7)
        .unwrap();
    assert_eq!(replaced.revision.get(), 2);
    assert!(later.metadata().generation > result.metadata().generation);
    assert_eq!(
        observe(&later)
            .iter()
            .filter(|row| row[0] == oracle::Cell::Node(7))
            .count(),
        2
    );
    assert_ne!(observe(&later), expected);
    assert!(
        observe(&later)
            .iter()
            .any(|r| r[2] == oracle::Cell::String("changed".into())
                && r[3] == oracle::Cell::String("quartz".into()))
    );
    // Poll ordinals vary across processes; retain them in the measured log.
    // Replay compares admitted generations and primitive rows at the pause.
    let observation = zeppelin_embed_bench::harness_json::json!({
        "site": "retained-reader", "fires": 1, "controls": 1,
        "old_generation": result.metadata().generation.get(), "new_generation": later.metadata().generation.get(),
        "old_rows": format!("{:?}", observe(&result)), "new_rows": format!("{:?}", observe(&later)),
        "pause": "measured-mid-query", "reservation_baseline": baseline,
    });
    drop(later);
    drop(result);
    drop(old);
    assert_eq!(c.store.as_ref().unwrap().reserved(), baseline);
    println!(
        "ZE65 seed={seed} retained boundary={nth}/{polls} writer-control-reservations={baseline}"
    );
    observation
}
