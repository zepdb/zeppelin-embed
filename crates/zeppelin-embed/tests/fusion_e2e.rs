#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod lifecycle_support;

use zeppelin_embed::fts::bm25::Bm25Params;
use zeppelin_embed::fts::index::{DEFAULT_FIELD, Document, LexicalIndex, SegmentIndex};
use zeppelin_embed::fts::search::{TermQuery, search};
use zeppelin_embed::fts::tokenizer::{Analyzer, Profile};
use zeppelin_embed::fusion::{
    FusionError, FusionLeg, HybridQuery, LexicalCandidate, VectorCandidate, execute_hybrid, fuse,
};
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, Deadline, GraphSearchOptions, OpenOptions, QueryControl, SearchOptions,
    SearchTier, Store,
};

#[test]
fn a_segment_without_document_identity_is_a_typed_error_for_hybrid_queries() {
    let _guard = lifecycle_support::test_guard();
    let directory = lifecycle_support::published_store(1);
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open task-07 store");
    let outcome = store
        .search(
            SearchRequest::new(&[0.0, 0.0]),
            1,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("task-07 vector leg");
    assert_eq!(outcome.candidates.len(), 1);
    let vector = outcome
        .candidates
        .iter()
        .map(|candidate| VectorCandidate::exact(candidate.document(), 0.0))
        .collect::<Vec<_>>();
    let lexical = vec![
        LexicalCandidate::new(1_u32, 2.0),
        LexicalCandidate::new(2_u32, 1.0),
    ];
    let error = fuse(
        &HybridQuery::new(2),
        &vector,
        &lexical,
        |identity: &Option<DocumentVersion>| (*identity).map(|version| version.doc_id()),
        |row| Some(zeppelin_embed::ingest::DocId::new(u128::from(*row))),
    )
    .expect_err("legacy segment identity must fail loudly");
    assert_eq!(
        error,
        FusionError::MissingDocumentIdentity {
            leg: FusionLeg::Vector,
            rank: 0,
        }
    );
    store.close().expect("close task-07 store");
}

#[test]
fn fused_e2e_goldens_on_a_corpus_where_the_legs_disagree_on_purpose() {
    let _guard = lifecycle_support::test_guard();
    let directory = tempfile::tempdir().expect("hybrid store");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open hybrid store");
    let document_ids = [DocId::new(101), DocId::new(102), DocId::new(103)];
    let vectors = [[0.0_f32, 0.0_f32], [1.0, 0.0], [2.0, 0.0]];
    let documents = document_ids
        .iter()
        .zip(vectors)
        .map(|(doc_id, vector)| {
            IngestDocument::new(
                DocumentVersion::new(*doc_id, Revision::new(1)),
                vector.to_vec(),
            )
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents))
        .expect("ingest vector corpus");
    let vector_result = store
        .search(
            SearchRequest::new(&[0.0, 0.0]),
            3,
            SearchOptions::default().with_tier(SearchTier::Graph(GraphSearchOptions::default())),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("exact active vector search");
    let vector = vector_result
        .candidates
        .iter()
        .map(|candidate| {
            VectorCandidate::exact(
                candidate.document().expect("public ingest identity"),
                -f64::from(candidate.score()),
            )
        })
        .collect::<Vec<_>>();

    let analyzer = Analyzer::new(Profile::TextDefault.config()).expect("text analyzer");
    let mut segment = SegmentIndex::new();
    for text in [
        "hybrid filler filler filler filler filler filler filler filler filler filler filler filler filler filler filler filler filler filler filler filler",
        "hybrid hybrid hybrid",
        "hybrid",
    ] {
        segment
            .push_document(&analyzer, &Document::with_text(text))
            .expect("index lexical document");
    }
    let mut lexical_index = LexicalIndex::new();
    lexical_index
        .push_segment(segment)
        .expect("seal lexical corpus");
    let lexical_result = search(
        &lexical_index,
        &TermQuery::flat(vec![b"hybrid".to_vec()], &[DEFAULT_FIELD]),
        3,
        Bm25Params::default(),
    )
    .expect("lexical search");
    let lexical = lexical_result
        .hits
        .iter()
        .map(|hit| LexicalCandidate::new(hit.doc, hit.score))
        .collect::<Vec<_>>();
    let fused = fuse(
        &HybridQuery::new(3).with_alpha(0.5),
        &vector,
        &lexical,
        |version| Some(version.doc_id()),
        |global| {
            usize::try_from(global.row)
                .ok()
                .and_then(|row| document_ids.get(row).copied())
        },
    )
    .expect("hybrid fusion");

    let vector_order = vector
        .iter()
        .map(|hit| hit.id().doc_id())
        .collect::<Vec<_>>();
    let lexical_order = lexical
        .iter()
        .filter_map(|hit| {
            usize::try_from(hit.id().row)
                .ok()
                .and_then(|row| document_ids.get(row).copied())
        })
        .collect::<Vec<_>>();
    let fused_order = fused.hits.iter().map(|hit| hit.key).collect::<Vec<_>>();
    assert_eq!(vector_order, document_ids);
    assert_eq!(
        lexical_order,
        [document_ids[1], document_ids[2], document_ids[0]]
    );
    assert_eq!(
        fused_order,
        [document_ids[1], document_ids[0], document_ids[2]]
    );
    assert_ne!(fused_order, vector_order);
    assert_ne!(fused_order, lexical_order);
    assert_eq!(
        fused
            .hits
            .iter()
            .map(|hit| hit.vector_squared_l2)
            .collect::<Vec<_>>(),
        [Some(1.0), Some(0.0), Some(4.0)]
    );
    store.close().expect("close hybrid store");
}

#[test]
fn fusion_under_a_tight_deadline_returns_the_typed_timeout_with_partial_false() {
    let _guard = lifecycle_support::test_guard();
    let directory = lifecycle_support::published_store(1);
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open deadline store");
    let result = execute_hybrid(
        &HybridQuery::new(1),
        || {
            let outcome = store
                .search(
                    SearchRequest::new(&[0.0, 0.0]),
                    1,
                    SearchOptions::default(),
                    QueryControl::Deadline(
                        Deadline::after(std::time::Duration::ZERO).expect("representable deadline"),
                    ),
                )
                .map_err(FusionError::from)?;
            Ok(outcome
                .candidates
                .iter()
                .map(|candidate| VectorCandidate::exact(candidate.document(), 0.0))
                .collect::<Vec<_>>())
        },
        || Ok(vec![LexicalCandidate::new(1_u32, 1.0)]),
        |identity: &Option<DocumentVersion>| (*identity).map(|version| version.doc_id()),
        |row| Some(DocId::new(u128::from(*row))),
    );
    assert_eq!(result, Err(FusionError::Timeout { partial: false }));
    store.close().expect("close deadline store");
}

#[test]
fn the_vector_window_entering_fusion_is_exactly_rescored() {
    let _guard = lifecycle_support::test_guard();
    let directory = tempfile::tempdir().expect("rescore store");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open rescore store");
    let inputs = [
        (DocId::new(201), [0.0_f32, 0.0_f32]),
        (DocId::new(202), [1.0_f32, 0.0_f32]),
        (DocId::new(203), [3.0_f32, 0.0_f32]),
    ];
    store
        .ingest(IngestBatch::new(
            inputs
                .iter()
                .map(|(doc_id, vector)| {
                    IngestDocument::new(
                        DocumentVersion::new(*doc_id, Revision::new(1)),
                        vector.to_vec(),
                    )
                })
                .collect(),
        ))
        .expect("ingest Bit4 fixture");
    let estimated_outcome = store
        .search(
            SearchRequest::new(&[0.0, 0.0]),
            3,
            SearchOptions::default().with_tier(SearchTier::Scan),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("estimated Bit4 scan");
    let estimated = estimated_outcome
        .candidates
        .iter()
        .map(|candidate| VectorCandidate::estimated(candidate.document().expect("identity"), 0.0))
        .collect::<Vec<_>>();
    let lexical = vec![
        LexicalCandidate::new(inputs[0].0, 3.0),
        LexicalCandidate::new(inputs[1].0, 2.0),
        LexicalCandidate::new(inputs[2].0, 1.0),
    ];
    let rejected = fuse(
        &HybridQuery::new(3),
        &estimated,
        &lexical,
        |version| Some(version.doc_id()),
        |doc_id| Some(*doc_id),
    );
    assert_eq!(rejected, Err(FusionError::EstimatedVectorScore { rank: 0 }));

    let exact_outcome = store
        .search(
            SearchRequest::new(&[0.0, 0.0]),
            3,
            SearchOptions::default().with_tier(SearchTier::Graph(GraphSearchOptions::default())),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("exact full-precision active scan");
    let exact = exact_outcome
        .candidates
        .iter()
        .map(|candidate| {
            VectorCandidate::exact(
                candidate.document().expect("identity"),
                -f64::from(candidate.score()),
            )
        })
        .collect::<Vec<_>>();
    let fused = fuse(
        &HybridQuery::new(3),
        &exact,
        &lexical,
        |version| Some(version.doc_id()),
        |doc_id| Some(*doc_id),
    )
    .expect("exact scores fuse");
    for hit in &fused.hits {
        let expected = inputs
            .iter()
            .find(|(doc_id, _)| *doc_id == hit.key)
            .map(|(_, vector)| {
                vector
                    .iter()
                    .map(|coordinate| {
                        let delta = f64::from(*coordinate);
                        delta * delta
                    })
                    .sum::<f64>()
            })
            .expect("fused id belongs to fixture");
        assert_eq!(hit.vector_squared_l2, Some(expected));
    }
    store.close().expect("close rescore store");
}
