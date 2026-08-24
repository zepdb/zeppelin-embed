//! Task 13 R2: validate the evaluator before the gate can mean anything.
//!
//! Every expected value here is computed by hand in the comment above it.
//! If this file is wrong, a BEIR number from the gate is meaningless — it
//! would be measuring the evaluator, not the engine.
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::BTreeMap;
use std::path::Path;

use zeppelin_embed_bench::beir::eval::{
    dcg_at_k, flat_targets, mean_ndcg_at_k, multifield_targets, ndcg_at_k_for_query, GateRow,
    Qrels, Run, RunEntry,
};
use zeppelin_embed_bench::beir::loader::parse_qrels;

const TOLERANCE: f64 = 1e-9;

fn close(actual: f64, expected: f64, what: &str) {
    assert!(
        (actual - expected).abs() < TOLERANCE,
        "{what}: got {actual}, expected {expected}"
    );
}

fn ranked(ids: &[&str]) -> Vec<RunEntry> {
    ids.iter()
        .enumerate()
        .map(|(index, id)| RunEntry {
            doc_id: (*id).to_owned(),
            score: 100.0 - index as f64,
        })
        .collect()
}

fn judgements(pairs: &[(&str, u32)]) -> BTreeMap<String, u32> {
    pairs
        .iter()
        .map(|(id, grade)| ((*id).to_owned(), *grade))
        .collect()
}

#[test]
fn dcg_uses_log2_of_rank_plus_one_with_ranks_from_one() {
    // grades [3, 2, 3] at ranks 1, 2, 3:
    //   3/log2(2) + 2/log2(3) + 3/log2(4)
    // = 3/1 + 2/1.584962500721156 + 3/2
    // = 3 + 1.261859507142915 + 1.5 = 5.761859507142915
    close(
        dcg_at_k(&[3, 2, 3], 10),
        3.0 + 2.0 / 3.0_f64.log2() + 3.0 / 2.0,
        "dcg of [3,2,3]",
    );
    // A single grade at rank 1 is just the grade.
    close(dcg_at_k(&[1], 10), 1.0, "dcg of [1]");
    // Zero grades contribute nothing wherever they sit.
    close(dcg_at_k(&[0, 0, 0], 10), 0.0, "dcg of zeros");
}

#[test]
fn dcg_truncates_at_k() {
    // Only the first two of [1, 1, 1] count at k = 2:
    //   1/log2(2) + 1/log2(3) = 1 + 0.6309297535714574
    close(
        dcg_at_k(&[1, 1, 1], 2),
        1.0 + 1.0 / 3.0_f64.log2(),
        "dcg at k=2",
    );
    close(dcg_at_k(&[1, 1, 1], 0), 0.0, "dcg at k=0");
}

#[test]
fn a_perfect_ranking_scores_exactly_one() {
    let judged = judgements(&[("a", 2), ("b", 1)]);
    close(
        ndcg_at_k_for_query(&ranked(&["a", "b"]), &judged, 10).expect("judged"),
        1.0,
        "perfect ranking",
    );
}

#[test]
fn ndcg_at_10_matches_a_hand_computed_graded_example() {
    // Judgements: d1 = 2, d2 = 1, d3 = 1. Run: d3, d9, d1.
    //   DCG  = 1/log2(2) + 0/log2(3) + 2/log2(4) = 1 + 0 + 1 = 2
    //   IDCG = 2/log2(2) + 1/log2(3) + 1/log2(4)
    //        = 2 + 0.6309297535714574 + 0.5 = 3.1309297535714574
    //   nDCG = 2 / 3.1309297535714578 = 0.6387878864795979
    let judged = judgements(&[("d1", 2), ("d2", 1), ("d3", 1)]);
    let expected = 2.0 / (2.0 + 1.0 / 3.0_f64.log2() + 0.5);
    close(
        ndcg_at_k_for_query(&ranked(&["d3", "d9", "d1"]), &judged, 10).expect("judged"),
        expected,
        "graded example",
    );
    close(expected, 0.638_787_886_479_597_9, "the hand-computed value");
}

#[test]
fn retrieving_nothing_scores_zero_not_one() {
    let judged = judgements(&[("d1", 1)]);
    close(
        ndcg_at_k_for_query(&[], &judged, 10).expect("judged"),
        0.0,
        "empty run",
    );
}

#[test]
fn a_query_with_no_positive_judgement_is_skipped_not_scored_zero() {
    // This is the convention that most often produces a BEIR number several
    // points below everyone else's.
    assert_eq!(
        ndcg_at_k_for_query(&ranked(&["d1"]), &judgements(&[("d1", 0)]), 10),
        None
    );
    assert_eq!(
        ndcg_at_k_for_query(&ranked(&["d1"]), &BTreeMap::new(), 10),
        None
    );
}

#[test]
fn the_mean_skips_unjudged_queries_and_counts_missing_runs_as_zero() {
    let mut qrels: Qrels = BTreeMap::new();
    qrels.insert(String::from("q1"), judgements(&[("d1", 1)]));
    qrels.insert(String::from("q2"), judgements(&[("d2", 1)]));
    // q3 is judged but with no positive grade: skipped entirely.
    qrels.insert(String::from("q3"), judgements(&[("d3", 0)]));

    let mut run: Run = BTreeMap::new();
    run.insert(String::from("q1"), ranked(&["d1"]));
    // q2 returns nothing: it scores zero and IS counted.

    // Mean over q1 and q2 only: (1.0 + 0.0) / 2 = 0.5.
    close(mean_ndcg_at_k(&run, &qrels, 10), 0.5, "mean over two");
}

#[test]
fn the_mean_of_no_judged_queries_is_zero_rather_than_a_division_by_zero() {
    close(
        mean_ndcg_at_k(&Run::new(), &Qrels::new(), 10),
        0.0,
        "empty everything",
    );
}

#[test]
fn ranking_order_actually_matters() {
    let judged = judgements(&[("good", 2), ("poor", 1)]);
    let better = ndcg_at_k_for_query(&ranked(&["good", "poor"]), &judged, 10).expect("judged");
    let worse = ndcg_at_k_for_query(&ranked(&["poor", "good"]), &judged, 10).expect("judged");
    assert!(
        better > worse,
        "an evaluator that ignores order proves nothing: {better} vs {worse}"
    );
}

#[test]
fn the_gate_targets_are_the_flat_column_not_the_multifield_one() {
    // research/02a:267 flat/multifield pairs: TREC-COVID 0.595/0.656,
    // FiQA 0.236/0.236, NFCorpus 0.322/0.325, SciFact 0.679/0.665. The task
    // spec mixed the columns; these are the FLAT numbers.
    let flat = flat_targets();
    let lookup = |rows: &[GateRow], name: &str| -> f64 {
        rows.iter()
            .find(|row| row.corpus == name)
            .map(|row| row.target)
            .expect("corpus present")
    };
    close(lookup(&flat, "trec-covid"), 0.595, "trec-covid flat");
    close(lookup(&flat, "fiqa"), 0.236, "fiqa flat");
    close(lookup(&flat, "nfcorpus"), 0.322, "nfcorpus flat");
    close(lookup(&flat, "scifact"), 0.679, "scifact flat");

    let multifield = multifield_targets();
    close(lookup(&multifield, "trec-covid"), 0.656, "trec-covid multi");
    close(lookup(&multifield, "scifact"), 0.665, "scifact multi");
    close(lookup(&multifield, "nfcorpus"), 0.325, "nfcorpus multi");

    // The two columns must not be confusable: SciFact and NFCorpus differ.
    assert!(
        (lookup(&flat, "scifact") - lookup(&multifield, "scifact")).abs() > 1e-6,
        "SciFact flat and multifield must differ, or the correction is moot"
    );
}

#[test]
fn an_unmeasured_corpus_never_passes_the_gate() {
    let row = GateRow {
        corpus: String::from("scifact"),
        target: 0.679,
        measured: None,
    };
    assert!(!row.passes(0.02), "an absent number must not count as a pass");

    let hit = GateRow {
        measured: Some(0.670),
        ..row.clone()
    };
    assert!(hit.passes(0.02), "0.670 is within 2 points of 0.679");

    let miss = GateRow {
        measured: Some(0.600),
        ..row
    };
    assert!(!miss.passes(0.02), "0.600 is not within 2 points of 0.679");
}

#[test]
fn qrels_parse_with_and_without_the_header_row() {
    let path = Path::new("qrels/test.tsv");
    let with_header = "query-id\tcorpus-id\tscore\nq1\td1\t2\nq1\td2\t1\n";
    let without_header = "q1\td1\t2\nq1\td2\t1\n";
    let first = parse_qrels(with_header, path).expect("header variant parses");
    let second = parse_qrels(without_header, path).expect("headerless variant parses");
    assert_eq!(first, second, "the header row must not eat a judgement");
    assert_eq!(first["q1"]["d1"], 2);
    assert_eq!(first["q1"]["d2"], 1);
}

#[test]
fn a_negative_grade_is_clamped_to_non_relevant_not_rejected() {
    // TREC-COVID ships two `-1` judgements: "assessed, not relevant". TREC
    // convention gives them zero gain. Rejecting the row would drop the
    // whole corpus from the gate, which is a far worse failure than the
    // quirk it guards against.
    let path = Path::new("qrels/test.tsv");
    let parsed = parse_qrels("q1\td1\t-1\nq1\td2\t2\n", path).expect("negative grade parses");
    assert_eq!(parsed["q1"]["d1"], 0);
    assert_eq!(parsed["q1"]["d2"], 2);
    // And a clamped grade must not become an ideal-DCG contributor.
    assert_eq!(
        ndcg_at_k_for_query(&ranked(&["d1"]), &judgements(&[("d1", 0)]), 10),
        None
    );
}

#[test]
fn a_malformed_qrels_row_is_a_typed_error_not_a_silent_skip() {
    let path = Path::new("qrels/test.tsv");
    assert!(parse_qrels("q1\td1\t2\nq1\td2\tnope\n", path).is_err());
    assert!(parse_qrels("q1\td1\n", path).is_err());
}
