//! Does a seal-time block maximum stay an upper bound after statistics move?
//!
//! The persisted `u8 block_max` is `quantize(score / ceiling)` computed when a
//! segment is sealed. The algebra:
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
//! is `avgdl` and the pair `(k1, b)`: both are baked in at seal time and both
//! can differ when the block is later read.
//!
//! Direction, derived rather than assumed: as `avgdl` RISES, `len / avgdl`
//! falls, the denominator shrinks, and the fraction GROWS. So a bound sealed
//! when documents were short is too low once longer documents arrive. A bound
//! that is too low lets task 14 prune a document that belonged in the top-k,
//! which is a wrong answer rather than a slow one.
//!
//! These tests probe the PERSISTED bound path (`block_maxima` +
//! `dequantize_block_max`). They deliberately do not go through
//! `search_pruned`, which currently recomputes bounds from live statistics at
//! query time and is therefore sound today — the defect is latent in the
//! format and would bite whichever task first reads the stored byte.
//!
//! # These are CHARACTERIZATION tests, and two of them must be inverted
//!
//! `a_sealed_bound_is_violated_*` assert that the bound IS currently unsound.
//! They pass today because the defect is real, and they exist to pin its
//! existence and its magnitude rather than to gate against it — a failing
//! test cannot sit in this suite while the remedy needs a format decision the
//! owner has not taken.
//!
//! When the impact-pair remedy lands, those two tests are deleted, and
//! `the_impact_pair_bound_survives_both_kinds_of_drift` becomes the standing
//! gate. If either violation test ever starts FAILING before that work, the
//! stored-bound path changed underneath this file and someone must work out
//! why. Measured magnitudes at the time of writing: 15.21% shortfall on an
//! avgdl rise from 10 to 100, 19.56% on a beir-to-anserini parameter change.
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::BTreeMap;

use zeppelin_embed::fts::bm25::{
    Bm25Params, CorpusStats, Df, DocLen, Tf, term_score, term_score_ceiling,
};
use zeppelin_embed::fts::postings::{Posting, PostingList, block_maxima, dequantize_block_max};

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

fn lengths_of(pairs: &[(u32, u32)]) -> BTreeMap<u32, u32> {
    pairs.iter().copied().collect()
}

/// The bound a reader reconstructs from the stored byte, under `live` stats.
fn stored_bound(sealed_byte: u8, df: Df, live: &CorpusStats, params: Bm25Params) -> f64 {
    dequantize_block_max(sealed_byte, term_score_ceiling(df, live, params))
}

#[test]
fn a_sealed_bound_holds_while_statistics_are_unchanged() {
    // The control. Whatever the other tests show, the bound must at least be
    // sound against the statistics it was computed with, or `block_maxima` is
    // simply broken.
    let list = list_of(&[(0, 3), (1, 5), (2, 1)]);
    let lengths = lengths_of(&[(0, 10), (1, 10), (2, 10)]);
    let stats = CorpusStats::new(100, 1_000).expect("avgdl 10");
    let params = Bm25Params::default();
    let df = Df(list.document_frequency());
    let scale = term_score_ceiling(df, &stats, params);

    let sealed = block_maxima(&list, 64, &lengths, &stats, params, scale);
    let bound = stored_bound(sealed[0], df, &stats, params);

    for posting in list.postings() {
        let truth = term_score(
            Tf(posting.tf),
            df,
            DocLen(lengths[&posting.docid]),
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
fn a_sealed_bound_is_violated_once_average_document_length_rises() {
    // Seal a segment while documents are short: avgdl = 10.
    let list = list_of(&[(0, 4)]);
    let lengths = lengths_of(&[(0, 10)]);
    let sealed_stats = CorpusStats::new(100, 1_000).expect("avgdl 10");
    let params = Bm25Params::default();
    let df = Df(1);
    let sealed_scale = term_score_ceiling(df, &sealed_stats, params);
    let sealed = block_maxima(&list, 64, &lengths, &sealed_stats, params, sealed_scale);

    // Later segments arrive full of long documents; avgdl rises to 100.
    let live_stats = CorpusStats::new(1_000, 100_000).expect("avgdl 100");
    let bound = stored_bound(sealed[0], df, &live_stats, params);
    let truth = term_score(Tf(4), df, DocLen(10), &live_stats, params);

    assert!(
        bound < truth,
        "expected the sealed bound to go stale: bound {bound}, true score {truth}"
    );
    let shortfall = (truth - bound) / truth * 100.0;
    println!("avgdl 10 -> 100: bound {bound:.6}, truth {truth:.6}, short by {shortfall:.2}%");
}

#[test]
fn a_sealed_bound_is_violated_when_the_caller_changes_k1_and_b() {
    // Sealed under the BEIR defaults the index was built with.
    let list = list_of(&[(0, 4)]);
    let lengths = lengths_of(&[(0, 30)]);
    let stats = CorpusStats::new(100, 1_000).expect("avgdl 10");
    let sealed_params = Bm25Params::beir();
    let df = Df(1);
    let sealed_scale = term_score_ceiling(df, &stats, sealed_params);
    let sealed = block_maxima(&list, 64, &lengths, &stats, sealed_params, sealed_scale);

    // The caller queries with Anserini's parameters, which the public API
    // exposes and documents as a supported alternative.
    let caller_params = Bm25Params::anserini();
    let bound = stored_bound(sealed[0], df, &stats, caller_params);
    let truth = term_score(Tf(4), df, DocLen(30), &stats, caller_params);

    assert!(
        bound < truth,
        "expected the sealed bound to go stale under different (k1, b): \
         bound {bound}, true score {truth}"
    );
    let shortfall = (truth - bound) / truth * 100.0;
    println!("beir -> anserini: bound {bound:.6}, truth {truth:.6}, short by {shortfall:.2}%");
}

#[test]
fn the_impact_pair_bound_survives_both_kinds_of_drift() {
    // The proposed remedy: store `(max_tf, min_len)` and evaluate the bound
    // at query time from LIVE stats and the CALLER's parameters. `term_score`
    // is monotone increasing in tf and decreasing in len, so the pair
    // dominates every posting in the block under any statistics.
    let entries = [(0_u32, 4_u32), (1, 2), (2, 7), (3, 1)];
    let lengths = lengths_of(&[(0, 30), (1, 12), (2, 45), (3, 9)]);
    let max_tf = entries.iter().map(|(_, tf)| *tf).max().expect("non-empty");
    let min_len = entries
        .iter()
        .map(|(docid, _)| lengths[docid])
        .min()
        .expect("non-empty");
    let df = Df(4);

    // Every combination of drifted statistics and caller parameters.
    for (docs, tokens) in [
        (100_u64, 1_000_u64),
        (1_000, 100_000),
        (10, 50),
        (5_000, 5_000),
    ] {
        let live = CorpusStats::new(docs, tokens).expect("valid stats");
        for params in [Bm25Params::beir(), Bm25Params::anserini()] {
            let bound = term_score(Tf(max_tf), df, DocLen(min_len), &live, params);
            for (docid, tf) in entries {
                let truth = term_score(Tf(tf), df, DocLen(lengths[&docid]), &live, params);
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
