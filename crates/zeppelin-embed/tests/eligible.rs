#![allow(clippy::expect_used)]
mod test_support;

use tempfile::tempdir;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, QueryFilter, SearchOptions, SearchTier, Store,
};

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

#[test]
fn an_empty_set_returns_no_hits() {
    let dir = tempdir().expect("directory");
    let store = Store::open(dir.path(), OpenOptions::default()).expect("open");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(1), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text("amber"),
        ]))
        .expect("ingest");
    let filter = QueryFilter::eligible(store.schema(), &[]);
    let result = store
        .search(
            SearchRequest::new(&[1.0, 0.0]).with_filter(Some(&filter)),
            1,
            SearchOptions::default().with_tier(SearchTier::Exact),
            control(),
        )
        .expect("search");
    assert!(result.candidates.is_empty());
    store.close().expect("close");
}

fn document_id(value: u128) -> DocId {
    DocId::new(value * 257 + ((value % 3) << 96))
}

fn corpus() -> (tempfile::TempDir, Store) {
    let dir = tempdir().expect("directory");
    let store = Store::open(dir.path(), OpenOptions::default()).expect("open");
    for start in [1_u128, 9] {
        store
            .ingest(IngestBatch::new(
                (start..start + 8)
                    .map(|id| {
                        IngestDocument::new(
                            DocumentVersion::new(document_id(id), Revision::new(1)),
                            vec![id as f32, 1.0],
                        )
                        .with_text(if id % 2 == 0 {
                            "amber amber cedar"
                        } else {
                            "amber cedar cedar"
                        })
                    })
                    .collect(),
            ))
            .expect("ingest");
        if start == 1 {
            store.seal().expect("seal");
        }
    }
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(document_id(2), Revision::new(2)),
                vec![2.0, 1.0],
            )
            .with_text("amber amber cedar"),
        ]))
        .expect("replace sealed version");
    store
        .delete(zeppelin_embed::ingest::DeleteBatch::new(vec![document_id(
            3,
        )]))
        .expect("delete sealed row");
    (dir, store)
}

#[test]
fn filtered_top_k_equals_post_filtered_exhaustive_top_k() {
    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngSeed, TestRunner};
    use rand::RngCore;
    let (_dir, store) = corpus();
    let mut seed =
        test_support::seeded_rng("eligible::filtered_top_k_equals_post_filtered_exhaustive_top_k");
    let mut runner = TestRunner::new(Config {
        cases: 48,
        rng_seed: RngSeed::Fixed(seed.next_u64()),
        ..Config::default()
    });
    runner
        .run(
            &(
                any::<u16>(),
                1_usize..17,
                -20_f32..20_f32,
                prop::sample::select(vec![0_usize, 999, 1000, 1100]),
            ),
            |(mask, k, x, padding)| {
                let mut ids = (1_u128..=16)
                    .filter(|id| mask & (1 << (id - 1)) != 0)
                    .map(document_id)
                    .collect::<Vec<_>>();
                ids.extend((0..padding).map(|id| DocId::new(100_000 + id as u128)));
                ids.reverse();
                if let Some(id) = ids.first().copied() {
                    ids.push(id);
                }
                let vector = [x, 1.0];
                let options = SearchOptions::default().with_tier(SearchTier::Exact);
                let exhaustive = store
                    .search(SearchRequest::new(&vector), 16, options, control())
                    .expect("exhaustive");
                let expected = exhaustive
                    .candidates
                    .into_iter()
                    .filter(|hit| {
                        hit.document()
                            .is_some_and(|version| ids.contains(&version.doc_id()))
                    })
                    .take(k)
                    .collect::<Vec<_>>();
                let actual = store
                    .search(
                        SearchRequest::new(&vector).with_eligible(&ids),
                        k,
                        options,
                        control(),
                    )
                    .expect("eligible");
                prop_assert_eq!(actual.candidates, expected);
                let lexical = zeppelin_embed::fts::query::LexicalQuery::Term(
                    zeppelin_embed::fts::search::TermQuery::flat(
                        vec![b"amber".to_vec()],
                        &[zeppelin_embed::fts::index::DEFAULT_FIELD],
                    ),
                );
                let exhaustive = store
                    .search_lexical_structured(&lexical, 16, 32, control())
                    .expect("exhaustive lexical");
                let expected = exhaustive
                    .candidates
                    .into_iter()
                    .filter(|hit| ids.contains(&hit.document.doc_id()))
                    .take(k)
                    .collect::<Vec<_>>();
                let filter = QueryFilter::eligible(store.schema(), &ids);
                let actual = store
                    .search_lexical_structured_filtered(&lexical, k, 32, control(), Some(&filter))
                    .expect("eligible lexical");
                prop_assert_eq!(actual.candidates, expected);
                Ok(())
            },
        )
        .expect("property");
    store.close().expect("close");
}

#[test]
fn applies_to_both_hybrid_legs_before_fusion() {
    use zeppelin_embed::fts::index::DEFAULT_FIELD;
    use zeppelin_embed::fts::search::TermQuery;
    use zeppelin_embed::fusion::HybridQuery;
    let (_dir, store) = corpus();
    let ids = [document_id(16), document_id(7)];
    let lexical = TermQuery::flat(vec![b"amber".to_vec()], &[DEFAULT_FIELD]);
    // Both targets rank behind other documents in at least one unfiltered leg.
    // k=2 must still return both; post-filtering the unfiltered top-2 cannot.
    for alpha in [0.0, 0.5, 1.0] {
        let mut query = HybridQuery::new(2);
        query.alpha = Some(alpha);
        query.max_rounds = 0;
        let result = store
            .search_hybrid(
                SearchRequest::new(&[1.0, 1.0]).with_eligible(&ids),
                &lexical,
                &query,
                SearchOptions::default().with_tier(SearchTier::Exact),
                control(),
            )
            .expect("hybrid");
        assert_eq!(result.hits.len(), 2);
        assert!(result.hits.iter().all(|hit| ids.contains(&hit.key)));
    }
    let filter = QueryFilter::eligible(store.schema(), &[]);
    let result = store
        .search_hybrid(
            SearchRequest::new(&[1.0, 1.0]).with_filter(Some(&filter)),
            &lexical,
            &HybridQuery::new(2),
            SearchOptions::default(),
            control(),
        )
        .expect("empty hybrid");
    assert!(result.hits.is_empty());
    store.close().expect("close");
}
