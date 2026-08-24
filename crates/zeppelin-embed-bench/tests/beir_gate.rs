//! Task 13's exit criterion: flat BM25 nDCG@10 within 2 points of Pyserini.
//!
//! # Status: MEASURED AND GREEN, 2026-08-23
//!
//! All four corpora are within 2 points of the published Pyserini flat
//! numbers. See `docs/13-beir.md` for the table and the competitor
//! comparison.
//!
//! This test stays `#[ignore]`d because it needs the corpora on disk, which
//! are ~280 MB and are not vendored. Do not read a passing `cargo test` as a
//! passing BEIR gate: it is skipped by default and REFUSES to report success
//! when the datasets are absent. Point it at a dataset directory to run it:
//!
//! ```text
//! ZE_BEIR_DIR=/path/to/beir \
//!   cargo test -p zeppelin-embed-bench --test beir_gate -- --ignored
//! ```
//!
//! Each corpus lives at `$ZE_BEIR_DIR/<name>/` with `corpus.jsonl`,
//! `queries.jsonl`, and `qrels/test.tsv`.
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use zeppelin_embed::fts::bm25::Bm25Params;
use zeppelin_embed::fts::index::{Document, FieldId, LexicalIndex, SegmentIndex};
use zeppelin_embed::fts::search::{search, TermQuery};
use zeppelin_embed::fts::tokenizer::{Analyzer, TokenizerConfig};
use zeppelin_embed_bench::beir::eval::{flat_targets, mean_ndcg_at_k, GateRow, Run, RunEntry};
use zeppelin_embed_bench::beir::loader::load_corpus;

/// Absolute tolerance, per the task 13 spec: within 2 points.
const TOLERANCE: f64 = 0.02;

/// Title and body are separate fields so the multifield run can weight them.
const TITLE: FieldId = FieldId(0);
const BODY: FieldId = FieldId(1);

fn dataset_root() -> Option<PathBuf> {
    std::env::var_os("ZE_BEIR_DIR").map(PathBuf::from)
}

/// Runs one corpus end to end and returns its mean nDCG@10.
fn measure(root: &std::path::Path, corpus_name: &str, flat: bool) -> Option<f64> {
    let corpus = match load_corpus(root, corpus_name) {
        Ok(corpus) => corpus,
        Err(error) => {
            eprintln!("corpus {corpus_name} unavailable: {error}");
            return None;
        }
    };
    let analyzer =
        Analyzer::new(TokenizerConfig::text_default()).expect("the text-default config is valid");
    let index_started = Instant::now();

    // One segment is enough for the gate; prop_engine_bm25_equals_model
    // already proves the score is independent of where segments were sealed.
    let mut segment = SegmentIndex::new();
    let mut row_to_doc: Vec<String> = Vec::with_capacity(corpus.documents.len());
    for document in &corpus.documents {
        let mut fields = Document::new();
        fields.set(TITLE, &document.title);
        fields.set(BODY, &document.text);
        segment
            .push_document(&analyzer, &fields)
            .expect("BEIR documents are indexable");
        row_to_doc.push(document.id.clone());
    }
    let mut index = LexicalIndex::new();
    index.push_segment(segment);
    let index_ms = index_started.elapsed().as_millis();

    let weights = if flat {
        zeppelin_embed::fts::search::FieldWeights::flat(&[TITLE, BODY])
    } else {
        // The multifield secondary target weights the title above the body.
        zeppelin_embed::fts::search::FieldWeights::new(&[(TITLE, 3_000), (BODY, 1_000)])
    };

    let mut run: Run = BTreeMap::new();
    let query_started = Instant::now();
    let mut executed = 0_usize;
    for query in &corpus.queries {
        // Only judged queries are timed and scored, matching the
        // competitors' harnesses exactly.
        if !corpus.qrels.contains_key(&query.id) {
            continue;
        }
        let mut terms: Vec<Vec<u8>> = Vec::new();
        for token in analyzer.analyze(&query.text) {
            let bytes = token.term.into_bytes();
            if !terms.contains(&bytes) {
                terms.push(bytes);
            }
        }
        if terms.is_empty() {
            continue;
        }
        let structured = TermQuery {
            terms,
            fields: weights.clone(),
        };
        let Ok(result) = search(&index, &structured, 10, Bm25Params::default()) else {
            continue;
        };
        executed += 1;
        run.insert(
            query.id.clone(),
            result
                .hits
                .into_iter()
                .filter_map(|hit| {
                    let row = usize::try_from(hit.doc.row).ok()?;
                    Some(RunEntry {
                        doc_id: row_to_doc.get(row)?.clone(),
                        score: hit.score,
                    })
                })
                .collect(),
        );
    }

    let query_ms = query_started.elapsed().as_millis();
    eprintln!(
        "TIMING corpus {corpus_name} docs {} queries {executed} index_ms {index_ms} query_ms {query_ms}",
        corpus.documents.len()
    );
    Some(mean_ndcg_at_k(&run, &corpus.qrels, 10))
}

fn report(rows: &[GateRow], label: &str) {
    eprintln!("\n{label} nDCG@10, tolerance {TOLERANCE:.3} absolute");
    eprintln!("{:<12} {:>8} {:>10} {:>8}", "corpus", "target", "measured", "delta");
    for row in rows {
        match row.measured {
            Some(value) => eprintln!(
                "{:<12} {:>8.3} {:>10.4} {:>+8.4}",
                row.corpus,
                row.target,
                value,
                value - row.target
            ),
            None => eprintln!(
                "{:<12} {:>8.3} {:>10} {:>8}",
                row.corpus, row.target, "ABSENT", "-"
            ),
        }
    }
}

#[test]
#[ignore = "needs the BEIR corpora; set ZE_BEIR_DIR to run"]
fn flat_bm25_ndcg_at_10_is_within_two_points_of_pyserini_on_all_four_corpora() {
    let Some(root) = dataset_root() else {
        panic!(
            "ZE_BEIR_DIR is not set. This gate has NOT been measured. \
             Set it to a directory holding trec-covid/, fiqa/, nfcorpus/, \
             and scifact/, each with corpus.jsonl, queries.jsonl, and \
             qrels/test.tsv."
        );
    };

    let mut rows = flat_targets();
    for row in &mut rows {
        row.measured = measure(&root, &row.corpus, true);
    }
    report(&rows, "flat BM25");

    let failures: Vec<&GateRow> = rows.iter().filter(|row| !row.passes(TOLERANCE)).collect();
    assert!(
        failures.is_empty(),
        "BEIR gate failed on {} corpora: {:?}",
        failures.len(),
        failures
            .iter()
            .map(|row| (row.corpus.as_str(), row.target, row.measured))
            .collect::<Vec<_>>()
    );
}

#[test]
#[ignore = "needs the BEIR corpora; set ZE_BEIR_DIR to run"]
fn multifield_bm25_reproduces_the_multifield_shape_on_trec_covid() {
    let Some(root) = dataset_root() else {
        panic!("ZE_BEIR_DIR is not set; the multifield target has NOT been measured");
    };
    let measured = measure(&root, "trec-covid", false);
    let row = GateRow {
        corpus: String::from("trec-covid"),
        target: 0.656,
        measured,
    };
    report(std::slice::from_ref(&row), "multifield BM25 (secondary)");
    assert!(
        row.passes(TOLERANCE),
        "multifield TREC-COVID missed 0.656: {measured:?}"
    );
}

/// Guards the claim this file makes about itself.
///
/// It runs by default, unlike the gates above, so that a reader running
/// `cargo test` is told the gate is unmeasured rather than being allowed to
/// assume a green suite covered it.
#[test]
fn the_beir_gate_is_unmeasured_unless_a_dataset_directory_is_supplied() {
    if dataset_root().is_none() {
        eprintln!(
            "NOTE: the BEIR gate is NOT YET MEASURED in this environment. \
             The harness, loader, and evaluator are implemented and tested, \
             but no corpus has been scored. Set ZE_BEIR_DIR and run the \
             ignored tests in this file to produce real numbers."
        );
    }
    // The targets themselves must always be the flat column.
    let rows = flat_targets();
    assert_eq!(rows.len(), 4);
    assert!(
        rows.iter().all(|row| row.measured.is_none()),
        "a target row must not ship with a measurement baked in"
    );
}
