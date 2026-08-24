//! The query-path allocation gate (FTS optimization plan P0.4).
//!
//! # Why a difference and not an absolute cap
//!
//! The property that matters is *shape*, not magnitude: a query may allocate
//! a constant number of working buffers, but it must never allocate once per
//! posting. An absolute cap would move whenever an unrelated buffer is added
//! and would say nothing about scaling. Comparing two corpora whose matching
//! posting count differs 16x isolates exactly the per-posting term.
//!
//! The allowance covers the geometric growth of the result vectors. A `Vec`
//! doubling from empty to `n` elements reallocates `ceil(log2(n)) + 1` times,
//! so growing from 64 to 1,024 entries costs at most four extra
//! reallocations per vector; the scorer keeps three such vectors, plus a
//! per-segment score array. Thirty-two is that arithmetic with slack, and it
//! is two orders of magnitude below a per-posting allocation.
//!
//! This is a deterministic counter, so it carries a zero flake budget and
//! needs no wall-clock headroom. It runs only under the `allocation-audit`
//! feature, whose global allocator wrapper is never in a shipped build.

use crate::fts::bm25::Bm25Params;
use crate::fts::index::{DEFAULT_FIELD, Document, FieldId, LexicalIndex, SegmentIndex};
use crate::fts::prune::{Strategy, search_pruned};
use crate::fts::search::{FieldWeights, TermQuery, search};
use crate::fts::tokenizer::{Analyzer, Profile};

/// Postings the small corpus supplies for the probe term.
const SMALL_MATCHES: usize = 64;

/// Postings the large corpus supplies for the probe term.
const LARGE_MATCHES: usize = 1_024;

/// Allocations attributable to geometric growth rather than to postings.
const GROWTH_ALLOWANCE: u64 = 32;

/// Builds a corpus in which `matches` documents carry the probe term.
///
/// Every document also carries a unique term so the dictionary grows with
/// the corpus, which keeps the two sizes structurally comparable rather than
/// letting the small one degenerate into a single posting list.
fn corpus(matches: usize) -> LexicalIndex {
    #[expect(
        clippy::expect_used,
        reason = "a fixed profile is valid; a failure here is a broken build, not bad input"
    )]
    let analyzer = Analyzer::new(Profile::Code.config()).expect("valid profile");
    let mut segment = SegmentIndex::new();
    for ordinal in 0..matches {
        let text = format!("alpha filler{ordinal} beta gamma delta");
        #[expect(
            clippy::expect_used,
            reason = "the fixture text is analyzable by construction"
        )]
        segment
            .push_document(&analyzer, &Document::with_text(&text))
            .expect("indexable fixture");
    }
    let mut index = LexicalIndex::new();
    index.push_segment(segment);
    index
}

/// Returns the allocator calls one exhaustive query makes.
fn exhaustive_allocations(matches: usize) -> u64 {
    let index = corpus(matches);
    let query = TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]);
    // Warm up outside the audit so one-time lazy initialization inside the
    // standard library is not charged to the measured call.
    drop(search(&index, &query, 10, Bm25Params::default()));
    let (result, report) = crate::allocation_audit::audit_engine_path(|| {
        search(&index, &query, 10, Bm25Params::default())
    });
    drop(result);
    report.allocations
}

#[test]
fn exhaustive_query_allocations_do_not_scale_with_postings() {
    let small = exhaustive_allocations(SMALL_MATCHES);
    let large = exhaustive_allocations(LARGE_MATCHES);
    assert!(
        large <= small.saturating_add(GROWTH_ALLOWANCE),
        "the exhaustive scorer allocates per posting: {small} allocations at \
         {SMALL_MATCHES} matches, {large} at {LARGE_MATCHES}"
    );
}

/// Returns the allocator calls one pruned query makes.
fn pruned_allocations(matches: usize) -> u64 {
    let index = corpus(matches);
    let query = TermQuery::flat(vec![b"alpha".to_vec()], &[DEFAULT_FIELD]);
    drop(search_pruned(
        &index,
        &query,
        10,
        Bm25Params::default(),
        Strategy::BlockMaxWand,
    ));
    let (result, report) = crate::allocation_audit::audit_engine_path(|| {
        search_pruned(
            &index,
            &query,
            10,
            Bm25Params::default(),
            Strategy::BlockMaxWand,
        )
    });
    drop(result);
    report.allocations
}

/// The two fields a multifield corpus indexes; the BEIR shape exactly.
const TITLE: FieldId = FieldId(0);
/// The body field of the multifield corpus.
const BODY: FieldId = FieldId(1);

/// Builds a corpus where the probe term occurs in BOTH fields of every row.
///
/// This is the shape every BEIR run uses — a title field and a body field,
/// flat-weighted — and it is the only shape that exercises the cross-field
/// union. A single-field corpus takes the linear fast path and says nothing
/// about it.
fn multifield_corpus(matches: usize) -> LexicalIndex {
    #[expect(
        clippy::expect_used,
        reason = "a fixed profile is valid; a failure here is a broken build, not bad input"
    )]
    let analyzer = Analyzer::new(Profile::Code.config()).expect("valid profile");
    let mut segment = SegmentIndex::new();
    for ordinal in 0..matches {
        let mut document = Document::new();
        document.set(TITLE, &format!("alpha title{ordinal}"));
        document.set(BODY, &format!("alpha filler{ordinal} beta gamma delta"));
        #[expect(
            clippy::expect_used,
            reason = "the fixture text is analyzable by construction"
        )]
        segment
            .push_document(&analyzer, &document)
            .expect("indexable fixture");
    }
    let mut index = LexicalIndex::new();
    index.push_segment(segment);
    index
}

/// Returns the allocator calls one flat two-field query makes.
fn multifield_allocations(matches: usize) -> u64 {
    let index = multifield_corpus(matches);
    let query = TermQuery {
        terms: vec![b"alpha".to_vec()],
        fields: FieldWeights::flat(&[TITLE, BODY]),
    };
    drop(search(&index, &query, 10, Bm25Params::default()));
    let (result, report) = crate::allocation_audit::audit_engine_path(|| {
        search(&index, &query, 10, Bm25Params::default())
    });
    drop(result);
    report.allocations
}

#[test]
fn cross_field_union_allocations_do_not_scale_with_postings() {
    // The cross-field union used an ordered map keyed by row, for the term
    // frequencies, and an ordered set of rows for the document frequency.
    // Both allocate a node per handful of entries, so both scale with the
    // posting count — on a corpus 16x larger they allocated 16x as often.
    // Both lists are already ascending by row, so the union is a linear
    // merge and the merge allocates once.
    let small = multifield_allocations(SMALL_MATCHES);
    let large = multifield_allocations(LARGE_MATCHES);
    assert!(
        large <= small.saturating_add(GROWTH_ALLOWANCE),
        "the cross-field union allocates per posting: {small} allocations at \
         {SMALL_MATCHES} matches, {large} at {LARGE_MATCHES}"
    );
}

#[test]
fn pruned_query_allocations_do_not_scale_with_postings() {
    let small = pruned_allocations(SMALL_MATCHES);
    let large = pruned_allocations(LARGE_MATCHES);
    assert!(
        large <= small.saturating_add(GROWTH_ALLOWANCE),
        "the pruned scorer allocates per posting: {small} allocations at \
         {SMALL_MATCHES} matches, {large} at {LARGE_MATCHES}"
    );
}
