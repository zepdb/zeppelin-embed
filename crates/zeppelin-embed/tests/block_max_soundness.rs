//! Does a persisted block bound stay an upper bound after statistics move?
//!
//! # The defect this file used to pin, and now gates against
//!
//! Version 1 stored a `u8 block_max` equal to `quantize(score / ceiling)`,
//! computed when a segment was sealed. The algebra:
//!
//! ```text
//! score          idf * tf * (k1 + 1) / (tf + k1 * (1 - b + b * len / avgdl))
//! ------- =      ---------------------------------------------------------
//! ceiling                    idf * (k1 + 1)
//!
//!          =     tf / (tf + k1 * (1 - b + b * len / avgdl))
//! ```
//!
//! `idf` cancels, so drift in `df` and `N` is harmless. What does NOT cancel
//! is `avgdl` and the pair `(k1, b)`: both were baked in at seal time and both
//! can differ when the block is later read.
//!
//! Direction, derived rather than assumed: as `avgdl` RISES, `len / avgdl`
//! falls, the denominator shrinks, and the fraction GROWS. So a bound sealed
//! when documents were short was too low once longer documents arrived. A
//! bound that is too low lets task 14 prune a document that belonged in the
//! top-k, which is a wrong answer rather than a slow one. Measured shortfalls
//! at the time the defect was pinned: **15.21%** on an `avgdl` rise from 10 to
//! 100, **19.56%** on a `beir()`-to-`anserini()` parameter change.
//!
//! # These tests were INVERTED when `POSTINGS_VERSION 2` landed
//!
//! `a_sealed_bound_is_violated_once_average_document_length_rises` and
//! `a_sealed_bound_is_violated_when_the_caller_changes_k1_and_b` were
//! CHARACTERIZATION tests: they asserted the bound *was* unsound, and they
//! passed because it was. Owner decision O1 authorised the remedy, so they are
//! replaced here by `a_sealed_bound_survives_*`, which assert the opposite
//! over the same two drifts and the same two fixtures.
//!
//! The remedy is an impact pair. Version 2 stores per block the largest term
//! frequency and the smallest document length it contains, and the reader
//! evaluates the bound at QUERY time from live statistics and the caller's
//! parameters. `term_score` is monotone increasing in `tf` and monotone
//! decreasing in `len`, so that pair dominates every posting in the block
//! under any statistics and any `(k1, b)`. The two extremes need not come from
//! the same document; pairing them is looser than the true maximum and is
//! therefore still an upper bound.
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use zeppelin_embed::fts::bm25::{Bm25Params, CorpusStats, Df, DocLen, Tf, term_score};
use zeppelin_embed::fts::postings::{
    BlockImpact, Posting, PostingList, PostingsReader, block_impacts, encode_v2,
};

/// Builds a one-block posting list with the given `(docid, tf)` pairs.
fn list_of(entries: &[(u32, u32)]) -> PostingList {
    let mut list = PostingList::new();
    for (docid, tf) in entries {
        list.push(Posting {
            docid: *docid,
            tf: *tf,
            positions: (0..*tf).collect(),
        })
        .expect("ascending fixture");
    }
    list
}

/// Seals a list and reads the impact pair back out of the persisted bytes.
///
/// Going through `encode_v2` and `PostingsReader` rather than through
/// `block_impacts` alone is the point: the claim under test is about what a
/// reader reconstructs from bytes, not about an in-memory helper.
fn sealed_impacts(list: &PostingList, lengths: &[u32]) -> Vec<BlockImpact> {
    let impacts = block_impacts(list, 64, lengths);
    let encoded = encode_v2(list, 64, &[], &impacts).expect("encodes");
    let reader = PostingsReader::open(encoded.as_bytes()).expect("opens");
    assert_eq!(reader.version(), 2, "encode_v2 must declare version 2");
    reader.blocks().iter().map(BlockImpact::from_meta).collect()
}

/// The bound a reader computes from a stored pair under `live` statistics.
fn stored_bound(impact: BlockImpact, df: Df, live: &CorpusStats, params: Bm25Params) -> f64 {
    term_score(
        Tf(impact.max_tf),
        df,
        DocLen(u32::from(impact.min_len)),
        live,
        params,
    )
}

#[test]
fn a_sealed_bound_holds_while_statistics_are_unchanged() {
    // The control. Whatever the other tests show, the bound must at least be
    // sound against the statistics it was computed with.
    let list = list_of(&[(0, 3), (1, 5), (2, 1)]);
    let lengths = vec![10_u32, 10, 10];
    let stats = CorpusStats::new(100, 1_000).expect("avgdl 10");
    let params = Bm25Params::default();
    let df = Df(list.document_frequency());

    let impacts = sealed_impacts(&list, &lengths);
    let bound = stored_bound(impacts[0], df, &stats, params);

    for posting in list.postings() {
        let truth = term_score(
            Tf(posting.tf),
            df,
            DocLen(lengths[posting.docid as usize]),
            &stats,
            params,
        );
        assert!(
            bound >= truth - 1e-12,
            "bound {bound} below a true score {truth} under UNCHANGED stats"
        );
    }
}

#[test]
fn a_sealed_bound_survives_a_rise_in_average_document_length() {
    // The inverted characterization test. Seal while documents are short —
    // avgdl 10 — which is the drift direction that used to break the bound.
    let list = list_of(&[(0, 4)]);
    let lengths = vec![10_u32];
    let params = Bm25Params::default();
    let df = Df(1);
    let impacts = sealed_impacts(&list, &lengths);

    // Later segments arrive full of long documents; avgdl rises to 100.
    let live_stats = CorpusStats::new(1_000, 100_000).expect("avgdl 100");
    let bound = stored_bound(impacts[0], df, &live_stats, params);
    let truth = term_score(Tf(4), df, DocLen(10), &live_stats, params);

    assert!(
        bound >= truth - 1e-12,
        "the sealed bound went stale on an avgdl rise: bound {bound}, \
         true score {truth}"
    );
}

#[test]
fn a_sealed_bound_survives_a_change_of_k1_and_b() {
    // Sealed under the BEIR defaults the index was built with.
    let list = list_of(&[(0, 4)]);
    let lengths = vec![30_u32];
    let stats = CorpusStats::new(100, 1_000).expect("avgdl 10");
    let df = Df(1);
    let impacts = sealed_impacts(&list, &lengths);

    // The caller queries with Anserini's parameters, which the public API
    // exposes and documents as a supported alternative.
    let caller_params = Bm25Params::anserini();
    let bound = stored_bound(impacts[0], df, &stats, caller_params);
    let truth = term_score(Tf(4), df, DocLen(30), &stats, caller_params);

    assert!(
        bound >= truth - 1e-12,
        "the sealed bound went stale under different (k1, b): bound {bound}, \
         true score {truth}"
    );
}

#[test]
fn the_impact_pair_bound_survives_both_kinds_of_drift() {
    // The standing gate: every combination of drifted statistics and caller
    // parameters, over a block whose largest term frequency and shortest
    // document belong to DIFFERENT documents.
    let entries = [(0_u32, 4_u32), (1, 2), (2, 7), (3, 1)];
    let lengths = vec![30_u32, 12, 45, 9];
    let list = list_of(&entries);
    let df = Df(4);
    let impacts = sealed_impacts(&list, &lengths);
    assert_eq!(impacts[0].max_tf, 7, "the largest tf is document 2's");
    assert_eq!(impacts[0].min_len, 9, "the shortest document is 3");

    for (docs, tokens) in [
        (100_u64, 1_000_u64),
        (1_000, 100_000),
        (10, 50),
        (5_000, 5_000),
    ] {
        let live = CorpusStats::new(docs, tokens).expect("valid stats");
        for params in [Bm25Params::beir(), Bm25Params::anserini()] {
            let bound = stored_bound(impacts[0], df, &live, params);
            for (docid, tf) in entries {
                let truth = term_score(Tf(tf), df, DocLen(lengths[docid as usize]), &live, params);
                assert!(
                    bound >= truth - 1e-12,
                    "impact-pair bound {bound} below true score {truth} \
                     at avgdl {:.1}, k1 {}, b {}",
                    live.average_document_length(),
                    params.k1,
                    params.b
                );
            }
        }
    }
}

#[test]
fn a_document_length_beyond_the_u16_slot_saturates_downward() {
    // `min_len` is a u16. A block whose shortest document is longer than the
    // slot can hold must saturate DOWN: a shorter length scores higher, so
    // storing u16::MAX keeps the bound above every true score.
    let list = list_of(&[(0, 3)]);
    let lengths = vec![200_000_u32];
    let stats = CorpusStats::new(100, 20_000_000).expect("valid stats");
    let params = Bm25Params::default();
    let df = Df(1);

    let impacts = sealed_impacts(&list, &lengths);
    assert_eq!(impacts[0].min_len, u16::MAX, "must saturate, not wrap");
    let bound = stored_bound(impacts[0], df, &stats, params);
    let truth = term_score(Tf(3), df, DocLen(200_000), &stats, params);
    assert!(
        bound >= truth - 1e-12,
        "a saturated length produced a bound {bound} below {truth}"
    );
}
