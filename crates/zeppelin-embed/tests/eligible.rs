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
    check_hybrid_entry(HybridEntry::Plain);
}

#[derive(Clone, Copy, Debug)]
enum HybridEntry {
    Plain,
    Structured,
    Text,
    DeferredText,
    Snippets,
    StructuredSnippets,
}

fn check_hybrid_entry(entry: HybridEntry) {
    use std::num::NonZeroUsize;
    use zeppelin_embed::fts::{index::DEFAULT_FIELD, query::LexicalQuery, search::TermQuery};
    use zeppelin_embed::fusion::{FusedHit, HybridQuery};
    let (_dir, store) = corpus();
    let vector = [1.0, 1.0];
    let lexical = TermQuery::flat(vec![b"amber".to_vec()], &[DEFAULT_FIELD]);
    let structured = LexicalQuery::Term(lexical.clone());
    let options = SearchOptions::default().with_tier(SearchTier::Exact);
    let all_vector = store
        .search(SearchRequest::new(&vector), 16, options, control())
        .expect("exhaustive vector");
    let all_lexical = store
        .search_lexical(&lexical, 16, control())
        .expect("exhaustive lexical");
    for ids in [
        vec![document_id(16), document_id(7)],
        vec![document_id(7)],
        vec![],
    ] {
        for alpha in [0.0, 0.5, 1.0] {
            let mut query = HybridQuery::new(2);
            query.alpha = Some(alpha);
            query.max_rounds = 0;
            // Independent oracle: filter the two exhaustive leg lists, then fuse
            // their exact raw scores with the documented Store normalization bounds.
            let terms = all_lexical
                .candidates
                .iter()
                .filter(|hit| ids.contains(&hit.document.doc_id()))
                .collect::<Vec<_>>();
            let lexical_max = terms.first().map_or(0.0, |hit| hit.score);
            let ceiling = all_vector.vector_ceiling.expect("corpus ceiling");
            let mut expected = all_vector
                .candidates
                .iter()
                .filter_map(|hit| {
                    let key = hit.document()?.doc_id();
                    if !ids.contains(&key) {
                        return None;
                    }
                    let distance = -f64::from(hit.score());
                    let bm25 = terms
                        .iter()
                        .find(|hit| hit.document.doc_id() == key)
                        .expect("matching lexical score")
                        .score;
                    // Store normalization policy v1: fixed zero lower bounds,
                    // corpus vector ceiling, and best eligible lexical score.
                    Some(FusedHit {
                        key,
                        vector_squared_l2: Some(distance),
                        lexical_bm25: Some(bm25),
                        fused_score: alpha * (1.0 - distance / ceiling)
                            + (1.0 - alpha) * bm25 / lexical_max,
                    })
                })
                .collect::<Vec<_>>();
            expected.sort_by(|left, right| {
                right
                    .fused_score
                    .total_cmp(&left.fused_score)
                    .then(left.key.cmp(&right.key))
            });
            expected.truncate(query.k);
            let request = SearchRequest::new(&vector).with_eligible(&ids);
            let window = NonZeroUsize::new(32).expect("window");
            let actual = match entry {
                HybridEntry::Plain => store
                    .search_hybrid(request, &lexical, &query, options, control())
                    .expect("plain"),
                HybridEntry::Structured => store
                    .search_hybrid_structured(request, &structured, &query, options, control())
                    .expect("structured"),
                HybridEntry::Text => {
                    store
                        .search_hybrid_with_text(
                            request,
                            &lexical,
                            &query,
                            options,
                            control(),
                            |_, _| (),
                        )
                        .expect("text")
                        .0
                }
                HybridEntry::DeferredText => {
                    store
                        .search_hybrid_with_text_deferred(
                            || Ok::<_, std::convert::Infallible>(request),
                            &lexical,
                            &query,
                            options,
                            control(),
                            |_, _| (),
                        )
                        .expect("deferred text")
                        .0
                }
                HybridEntry::Snippets => {
                    store
                        .search_hybrid_with_snippets(
                            request,
                            &lexical,
                            &query,
                            options,
                            control(),
                            window,
                        )
                        .expect("snippets")
                        .0
                }
                HybridEntry::StructuredSnippets => {
                    store
                        .search_hybrid_structured_with_snippets(
                            request,
                            &structured,
                            &query,
                            options,
                            control(),
                            window,
                        )
                        .expect("structured snippets")
                        .0
                }
            };
            assert!(
                actual.hits.iter().all(|hit| ids.contains(&hit.key)),
                "{entry:?} leaked an ineligible hit"
            );
            assert_eq!(
                actual.hits.len(),
                expected.len(),
                "{entry:?}, alpha={alpha}"
            );
            for (actual, expected) in actual.hits.iter().zip(&expected) {
                assert_eq!(actual.key, expected.key, "{entry:?}, alpha={alpha}");
                assert_eq!(actual.vector_squared_l2, expected.vector_squared_l2);
                assert_eq!(actual.lexical_bm25, expected.lexical_bm25);
                assert!(
                    (actual.fused_score - expected.fused_score).abs() < 1e-12,
                    "{entry:?}, alpha={alpha}: {actual:?} != {expected:?}"
                );
            }
        }
    }
    store.close().expect("close");
}

#[test]
fn structured_hybrid_equals_filtered_exhaustive_fusion() {
    check_hybrid_entry(HybridEntry::Structured);
}
#[test]
fn hybrid_with_text_equals_filtered_exhaustive_fusion() {
    check_hybrid_entry(HybridEntry::Text);
}
#[test]
fn deferred_hybrid_with_text_equals_filtered_exhaustive_fusion() {
    check_hybrid_entry(HybridEntry::DeferredText);
}
#[test]
fn hybrid_with_snippets_equals_filtered_exhaustive_fusion() {
    check_hybrid_entry(HybridEntry::Snippets);
}
#[test]
fn structured_hybrid_with_snippets_equals_filtered_exhaustive_fusion() {
    check_hybrid_entry(HybridEntry::StructuredSnippets);
}

#[test]
fn vector_with_text_equals_post_filtered_exhaustive_search() {
    let (_dir, store) = corpus();
    let ids = [document_id(16), document_id(7)];
    let vector = [1.0, 1.0];
    let options = SearchOptions::default().with_tier(SearchTier::Exact);
    let all = store
        .search(SearchRequest::new(&vector), 16, options, control())
        .expect("exhaustive");
    let expected = all
        .candidates
        .into_iter()
        .filter(|hit| {
            hit.document()
                .is_some_and(|document| ids.contains(&document.doc_id()))
        })
        .take(2)
        .collect::<Vec<_>>();
    let actual = store
        .search_with_text(
            SearchRequest::new(&vector).with_eligible(&ids),
            2,
            options,
            control(),
            |_, _| (),
        )
        .expect("materialized vector")
        .0;
    assert_eq!(actual.candidates, expected);
    store.close().expect("close");
}
