//! Snippets for ranked text queries (ZE-215).
//!
//! Every snippet is computed by the store from the text pinned for the
//! query's own generation, with the store's own analyzer and the terms the
//! lexical leg scored, so a highlight here is a match there.
#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::num::NonZeroUsize;

use tempfile::tempdir;
use zeppelin_embed::fts::index::DEFAULT_FIELD;
use zeppelin_embed::fts::query::{LexicalQuery, OwnedLexicalSnippet};
use zeppelin_embed::fts::search::{FieldWeights, TermQuery};
use zeppelin_embed::fusion::HybridQuery;
use zeppelin_embed::ingest::SearchRequest;
use zeppelin_embed::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SearchOptions, Store};

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

fn window(bytes: usize) -> NonZeroUsize {
    NonZeroUsize::new(bytes).expect("nonzero window")
}

fn store_with(rows: &[(u128, [f32; 2], Option<&str>)]) -> (tempfile::TempDir, Store) {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let documents = rows
        .iter()
        .map(|(id, vector, text)| {
            let document = IngestDocument::new(
                DocumentVersion::new(DocId::new(*id), Revision::new(1)),
                vector.to_vec(),
            );
            match text {
                Some(text) => document.with_text(*text),
                None => document,
            }
        })
        .collect();
    store.ingest(IngestBatch::new(documents)).expect("ingest");
    (directory, store)
}

fn marked(snippet: &OwnedLexicalSnippet) -> Vec<&str> {
    snippet
        .highlights
        .iter()
        .map(|range| {
            let start = (range.start - snippet.source.start) as usize;
            let end = (range.end - snippet.source.start) as usize;
            &snippet.text[start..end]
        })
        .collect()
}

#[test]
fn term_snippets_mark_the_surface_forms_the_analyzer_matched() {
    let (_directory, store) = store_with(&[
        (1, [1.0, 0.0], Some("Boats leave the HARBOURS at dawn")),
        (2, [0.0, 1.0], Some("nothing relevant here")),
    ]);
    // "harbour" is the stem both "harbours" and "HARBOURS" analyse to.
    let query = TermQuery::flat(vec![b"harbour".to_vec()], &[DEFAULT_FIELD]);
    let (outcome, snippets) = store
        .search_lexical_with_snippets(&query, 5, window(64), control())
        .expect("lexical snippets");
    assert_eq!(outcome.candidates.len(), 1);
    assert_eq!(snippets.len(), outcome.candidates.len());
    assert_eq!(marked(&snippets[0]), vec!["HARBOURS"]);
    assert_eq!(snippets[0].text, "HARBOURS at dawn");
    assert_eq!(
        snippets[0].source_len,
        "Boats leave the HARBOURS at dawn".len()
    );

    // The default path returns the same ranking.
    let plain = store.search_lexical(&query, 5, control()).expect("plain");
    assert_eq!(plain.candidates, outcome.candidates);
    store.close().expect("close");
}

#[test]
fn hybrid_snippets_mark_lexical_hits_and_skip_rows_without_a_match() {
    let (_directory, store) = store_with(&[
        (1, [1.0, 0.0], Some("harbour lights at dusk")),
        (2, [0.9, 0.1], Some("quantized vectors")),
        (3, [0.8, 0.2], None),
    ]);
    let query = TermQuery::flat(vec![b"harbour".to_vec()], &[DEFAULT_FIELD]);
    let (outcome, snippets) = store
        .search_hybrid_with_snippets(
            SearchRequest::new(&[1.0, 0.0]),
            &query,
            &HybridQuery::new(3),
            SearchOptions::default(),
            control(),
            window(64),
        )
        .expect("hybrid snippets");
    let snippets = snippets.expect("materialized snippets");
    assert_eq!(snippets.len(), outcome.hits.len());
    for (hit, snippet) in outcome.hits.iter().zip(&snippets) {
        if hit.key == DocId::new(1) {
            let snippet = snippet.as_ref().expect("lexical hit has a snippet");
            assert_eq!(marked(snippet), vec!["harbour"]);
        } else {
            assert_eq!(snippet, &None, "hit {:?} has no matched term", hit.key);
        }
    }
    store.close().expect("close");
}

#[test]
fn prefix_snippets_mark_every_expanded_term() {
    let (_directory, store) = store_with(&[
        (1, [1.0, 0.0], Some("harbour and harbinger")),
        (2, [0.0, 1.0], Some("quantized vectors")),
    ]);
    let query = LexicalQuery::TermsWithPrefix {
        terms: Vec::new(),
        prefix: b"harb".to_vec(),
        fields: FieldWeights::flat(&[DEFAULT_FIELD]),
    };
    let (outcome, snippets) = store
        .search_hybrid_structured_with_snippets(
            SearchRequest::new(&[1.0, 0.0]),
            &query,
            &HybridQuery::new(2),
            SearchOptions::default(),
            control(),
            window(64),
        )
        .expect("structured hybrid snippets");
    let snippets = snippets.expect("materialized snippets");
    let first = outcome
        .hits
        .iter()
        .position(|hit| hit.key == DocId::new(1))
        .expect("prefix hit");
    let snippet = snippets[first].as_ref().expect("prefix snippet");
    assert_eq!(marked(snippet), vec!["harbour", "harbinger"]);
    store.close().expect("close");
}
