#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::{TempDir, tempdir};
use zeppelin_embed::diag::SegmentHealth;
use zeppelin_embed::ingest::RowSource;
use zeppelin_embed_text::{
    IngestOptions, Legs, QueryOptions, TextDocument, TextError, TextHit, TextStore,
};

mod common;

const QUERY: &str = "bronze bronze zeppelin";

fn fixture() -> (TempDir, PathBuf) {
    let root = tempdir().expect("fixture directory");
    let model = root.path().join("fixture.zem");
    let mut tower = common::FixtureTower::document("merge-fixture");
    tower.word_vectors[2] = [0., 0.];
    tower.word_vectors[5] = [1., 0.];
    tower.word_vectors[6] = [0., 1.];
    common::write_fixture_bundle(&model, &[tower], &[]);
    (root, model)
}

fn options() -> IngestOptions {
    IngestOptions {
        // Use the same embedding batch shape in single-call and batch controls.
        embed_batch_size: 1,
        maintenance_wall_time: Duration::ZERO,
        ..Default::default()
    }
}

fn document(id: u128) -> TextDocument {
    let bronze = 1 + id % 4;
    let mut zeppelin = 1 + (id / 4) % 4;
    if bronze == zeppelin {
        zeppelin += 1;
    }
    TextDocument::new(
        id,
        1,
        format!(
            "{}{}note{id}",
            "bronze ".repeat(bronze as usize),
            "zeppelin ".repeat(zeppelin as usize)
        ),
    )
}

fn sealed(store: &TextStore) -> Vec<SegmentHealth> {
    store
        .health()
        .expect("health")
        .segments
        .into_iter()
        .filter(|segment| matches!(segment.source, RowSource::Sealed(_)))
        .collect()
}

fn pair(root: &Path, model: &Path) -> (TextStore, TextStore) {
    let singles = TextStore::open(root.join("singles"), model, Default::default())
        .expect("open single-call store");
    let control = TextStore::open(root.join("control"), model, Default::default())
        .expect("open batch control");
    let documents = (1..=100).map(document).collect::<Vec<_>>();
    for document in &documents {
        singles
            .ingest_text(std::slice::from_ref(document), options())
            .expect("single ingest");
    }
    control
        .ingest_text(&documents, options())
        .expect("one-batch ingest");
    (singles, control)
}

fn assert_hits_equal(expected: &[TextHit], actual: &[TextHit]) {
    assert_eq!(expected.len(), actual.len(), "hit count");
    let mut expected = expected.to_vec();
    let mut actual = actual.to_vec();
    // Lexical ties use physical segment/row order, which merging can change.
    // Preserve score-run order and every score bit; canonicalize only exact ties.
    for hits in [&mut expected, &mut actual] {
        for run in hits.chunk_by_mut(|left, right| left.score.to_bits() == right.score.to_bits()) {
            run.sort_by_key(|hit| hit.doc_id);
        }
    }
    for (expected, actual) in expected.iter().zip(&actual) {
        assert_eq!(
            (
                expected.doc_id,
                expected.revision,
                expected.chunk,
                &expected.text
            ),
            (actual.doc_id, actual.revision, actual.chunk, &actual.text),
        );
        assert_eq!(expected.score.to_bits(), actual.score.to_bits(), "score");
        assert_eq!(
            expected.vector_squared_l2.map(f64::to_bits),
            actual.vector_squared_l2.map(f64::to_bits),
            "dense score"
        );
        assert_eq!(
            expected.lexical_bm25.map(f64::to_bits),
            actual.lexical_bm25.map(f64::to_bits),
            "lexical score"
        );
    }
}

fn assert_top_k_matches_all(all: &[TextHit], top: &[TextHit], k: usize) {
    assert_eq!(top.len(), k.min(all.len()), "top-k hit count");
    assert!(
        top.windows(2)
            .all(|pair| pair[0].score.total_cmp(&pair[1].score).is_ge()),
        "descending scores"
    );
    let Some(boundary) = all.iter().take(k).next_back() else {
        return;
    };
    assert_eq!(
        top.last().map(|hit| hit.score.to_bits()),
        Some(boundary.score.to_bits()),
        "k-th score bits"
    );
    let above = |hits: &[TextHit]| {
        hits.iter()
            .filter(|hit| hit.score.total_cmp(&boundary.score).is_gt())
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_hits_equal(&above(all), &above(top));
    let boundary_ids = all
        .iter()
        .filter(|hit| hit.score.to_bits() == boundary.score.to_bits())
        .map(|hit| hit.doc_id)
        .collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();
    for hit in top {
        assert!(seen.insert(hit.doc_id), "duplicate top-k id");
        if hit.score.to_bits() == boundary.score.to_bits() {
            assert!(
                boundary_ids.contains(&hit.doc_id),
                "id outside the complete boundary tie"
            );
        } else {
            assert!(
                hit.score.total_cmp(&boundary.score).is_gt(),
                "hit below k-th score"
            );
        }
        let expected = all
            .iter()
            .find(|expected| expected.doc_id == hit.doc_id)
            .expect("top-k id in complete results");
        assert_hits_equal(std::slice::from_ref(expected), std::slice::from_ref(hit));
    }
}

fn assert_queries_equal(singles: &TextStore, control: &TextStore, phase: &str) {
    for legs in [Legs::Dense, Legs::Lexical, Legs::Hybrid] {
        eprintln!("{phase}: complete {legs:?} results");
        let all_options = QueryOptions::new(100).with_legs(legs);
        let expected_all = control
            .query_text(QUERY, all_options)
            .expect("complete control query");
        let actual_all = singles
            .query_text(QUERY, all_options)
            .expect("complete merged query");
        assert_hits_equal(&expected_all, &actual_all);
        for k in [10, 100] {
            eprintln!("{phase}: {legs:?} k={k}");
            let query = QueryOptions::new(k).with_legs(legs);
            let expected = control.query_text(QUERY, query).expect("control query");
            let actual = singles.query_text(QUERY, query).expect("merged query");
            // Check the entire strict prefix and both boundary subsets against
            // complete, bit-equal results, even when k splits a physical-row tie.
            assert_top_k_matches_all(&expected_all, &expected, k);
            assert_top_k_matches_all(&expected_all, &actual, k);
        }
    }
}

#[test]
fn single_document_ingests_keep_sealed_segments_bounded() {
    let (root, model) = fixture();
    let store =
        TextStore::open(root.path().join("store"), model, Default::default()).expect("open store");
    let mut seen = BTreeSet::new();
    let mut rewritten_rows = 0;
    let mut max_sealed = 0;
    let mut previous_generation = store.health().expect("initial health").generation;
    let mut ordinary_delta = None;
    let mut merging_delta = None;
    for id in 1..=1_000 {
        let report = store
            .ingest_text(&[document(id)], options())
            .expect("ingest");
        assert_eq!(report.seals, 1);
        let segments = sealed(&store);
        max_sealed = max_sealed.max(segments.len());
        assert!(
            segments.len() <= 16,
            "{} sealed segments at call {id}",
            segments.len()
        );
        for segment in segments {
            if seen.insert(segment.source) && segment.rows >= 2 {
                rewritten_rows += segment.rows;
            }
        }
        let delta = report.generation - previous_generation;
        if id == 2 {
            ordinary_delta = Some(delta);
        }
        if id == 16 {
            merging_delta = Some(delta);
        }
        previous_generation = report.generation;
    }
    assert!(rewritten_rows <= 33_500, "rewrote {rewritten_rows} rows");
    assert_eq!(merging_delta, ordinary_delta.map(|delta| delta + 1));
    eprintln!(
        "max_sealed={max_sealed}, merge_rewritten_rows={rewritten_rows}, ordinary_generation_delta={ordinary_delta:?}, merging_generation_delta={merging_delta:?}"
    );
    store.close().expect("close");
}

#[test]
fn merged_store_answers_like_a_one_batch_control() {
    let (root, model) = fixture();
    let (singles, control) = pair(root.path(), &model);
    assert!(
        sealed(&singles).len() <= 16,
        "sealed = {}",
        sealed(&singles).len()
    );
    let dense = control
        .query_text(QUERY, QueryOptions::new(100).with_legs(Legs::Dense))
        .expect("control dense scores");
    assert!(
        dense
            .iter()
            .map(|hit| hit.score.to_bits())
            .collect::<BTreeSet<_>>()
            .len()
            >= 2
    );
    assert_queries_equal(&singles, &control, "initial");
    singles.close().expect("close singles");
    control.close().expect("close control");
}

#[test]
fn delete_and_update_of_merged_documents_match_control_across_reopen() {
    let (root, model) = fixture();
    let (singles, control) = pair(root.path(), &model);
    assert!(
        sealed(&singles).len() <= 16,
        "sealed = {}",
        sealed(&singles).len()
    );
    for store in [&singles, &control] {
        store.delete_text(&[2, 17, 90]).expect("delete merged ids");
        store
            .ingest_text(
                &[TextDocument::new(
                    31,
                    2,
                    "bronze bronze zeppelin note31 revised",
                )],
                options(),
            )
            .expect("update merged id");
    }
    assert_queries_equal(&singles, &control, "after delete and update");
    let hits = singles
        .query_text(QUERY, QueryOptions::new(100).with_legs(Legs::Dense))
        .expect("live documents");
    assert_eq!(hits.len(), 97);
    assert!(hits.iter().all(|hit| ![2, 17, 90].contains(&hit.doc_id)));
    assert!(hits.iter().any(|hit| hit.doc_id == 31 && hit.revision == 2));
    assert!(sealed(&singles).len() <= 16);
    singles.close().expect("close singles");
    control.close().expect("close control");
    drop((singles, control));
    let singles = TextStore::open(root.path().join("singles"), &model, Default::default())
        .expect("reopen singles");
    let control = TextStore::open(root.path().join("control"), &model, Default::default())
        .expect("reopen control");
    assert_queries_equal(&singles, &control, "after reopen");
    assert!(sealed(&singles).len() <= 16);
    singles.close().expect("close reopened singles");
    control.close().expect("close reopened control");
}

#[test]
fn open_merges_segments_left_by_earlier_sessions() {
    let (root, model) = fixture();
    let path = root.path().join("store");
    for id in 1..=20 {
        let store = TextStore::open(&path, &model, Default::default()).expect("open session");
        store
            .ingest_text(&[document(id)], options())
            .expect("one session ingest");
        store.close().expect("close session");
    }
    let store = TextStore::open(&path, &model, Default::default()).expect("final open");
    assert_eq!(sealed(&store).len(), 1, "sealed after final open");
    let hits = store
        .query_text(QUERY, QueryOptions::new(20).with_legs(Legs::Lexical))
        .expect("all session documents");
    assert_eq!(
        hits.iter().map(|hit| hit.doc_id).collect::<BTreeSet<_>>(),
        (1..=20).collect()
    );
    store.close().expect("close final session");
}

#[test]
fn a_failed_merge_fails_the_ingest_before_writing() {
    for serialized in [false, true] {
        let (root, model) = fixture();
        let path = root.path().join("store");
        let store = TextStore::open(&path, &model, Default::default()).expect("open store");
        let ingest = |document: &TextDocument| {
            if serialized {
                store.ingest_text_serialized(std::slice::from_ref(document), options())
            } else {
                store.ingest_text(std::slice::from_ref(document), options())
            }
        };
        for id in 1..=15 {
            ingest(&document(id)).expect("ingest before merge");
        }
        // The analyzer splits note16 into terms including the shared "note".
        // Request every fixture hit and probe the refused document's identity.
        let query = QueryOptions::new(100).with_legs(Legs::Lexical);
        let before = store
            .query_text("note16", query)
            .expect("query before failure");
        let refused = document(16);
        let generation = store.health().expect("health before failure").generation;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o555))
            .expect("deny publication");
        let result = ingest(&refused);
        // Restore before assertions so even a RED failure can clean up its store.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("restore publication");
        let hits = store
            .query_text("note16", query)
            .expect("query failed call");
        assert!(
            matches!(
                result,
                Err(TextError::Pipeline {
                    stage: "segment merge",
                    ..
                })
            ),
            "serialized={serialized}, result={result:?}, note16 hits={hits:?}"
        );
        assert!(
            hits.iter()
                .all(|hit| hit.doc_id != refused.id && hit.text != refused.text),
            "failed call wrote document 16"
        );
        assert_hits_equal(&before, &hits);
        assert_eq!(
            store.health().expect("health after failure").generation,
            generation
        );
        ingest(&refused).expect("retry merge and ingest");
        assert_eq!(sealed(&store).len(), 2);
        let hits = store.query_text("note16", query).expect("query retry");
        let written = hits
            .iter()
            .filter(|hit| hit.doc_id == refused.id)
            .collect::<Vec<_>>();
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].text, refused.text);
        store.close().expect("close");
    }
}
