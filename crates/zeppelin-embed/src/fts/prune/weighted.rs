//! Combined structured top-k over the existing sealed posting cursors.
//!
//! Bounds and scores are folded in expansion order, including duplicates.
//! WAND pivot selection therefore never subtracts or reorders floating-point
//! contributions. A phrase predicate is consulted before a heap insertion.

use super::{TermCursor, TopK};
use crate::fts::bm25::{Df, DocLen, Tf};
use crate::fts::index::LexicalIndex;
use crate::fts::query::PreparedWeightedQuery;
use crate::fts::sealed::TermStream;
use crate::fts::search::{ControlledSearchError, GlobalDocId, SearchCounters};
use crate::meta::DocBitmap;
use std::{cmp::Reverse, collections::BinaryHeap};

#[cfg(test)]
thread_local! {
    static HEAD_SEEKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static HEAD_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static OMIT_BOUND_BOOST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// Order heads by row, then expansion slot. This also fixes score-addition
// order when several contributions share a row.
#[derive(Clone, Copy, Eq, PartialEq)]
struct CursorHead(Reverse<(u32, usize)>);
impl Ord for CursorHead {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        #[cfg(test)]
        HEAD_WORK.with(|count| count.set(count.get() + 1));
        self.0.cmp(&other.0)
    }
}
impl PartialOrd for CursorHead {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

struct WeightedCursor<'segment> {
    term: TermCursor<'segment>,
    boost: f64,
}

#[derive(Default)]
pub(crate) struct WeightedResult {
    pub(crate) hits: Vec<(GlobalDocId, f64)>,
    pub(crate) counters: SearchCounters,
}

impl WeightedCursor<'_> {
    fn bound(&self, value: f64) -> f64 {
        #[cfg(test)]
        if OMIT_BOUND_BOOST.with(std::cell::Cell::get) {
            return value;
        }
        // The BEIR scorer has at most eight rounded positive operations
        // after its frozen constants. Allow for both its bound and exact
        // evaluation, then the boost multiplication. 32 epsilon exceeds
        // (1+u)^8/(1-u)^8, where u=epsilon/2. Nonzero BEIR scores and the
        // u16-thousandths boosts are normal; overflow gives infinity and
        // disables pruning. Exact scores retain the original expression.
        if value == 0.0 || self.boost == 0.0 {
            0.0
        } else {
            (value * (1.0 + 32.0 * f64::EPSILON) * self.boost).next_up()
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::items_after_test_module,
    clippy::panic
)]
mod tests {
    use super::*;
    use crate::fts::index::{Document, FieldId, SegmentIndex};
    use crate::fts::query::{LexicalExpansion, LexicalMatchKind};
    use crate::fts::sealed::SealedSegment;
    use crate::fts::search::{FieldWeights, ScoredDoc};
    use crate::fts::tokenizer::{Analyzer, Profile};

    struct Fixture {
        index: LexicalIndex,
        alive: Vec<DocBitmap>,
        // Original term counts and lengths, independent of sealed scorers.
        rows: Vec<[[u32; 3]; 2]>,
    }

    fn fixture() -> Fixture {
        let analyzer = Analyzer::new(Profile::Code.config()).expect("analyzer");
        let mut fixture = Fixture {
            index: LexicalIndex::new(),
            alive: Vec::new(),
            rows: Vec::new(),
        };
        for ordinal in 0..3 {
            let mut segment = SegmentIndex::new();
            let alive = DocBitmap::from_ids((0..96).filter(|row| row % 11 != 0));
            for row in 0..96 {
                let mut counts = [[0; 3]; 2];
                let mut document = Document::new();
                for (field, slot) in counts.iter_mut().enumerate() {
                    let alpha = (row + ordinal * 3 + field as u32) % 9;
                    let beta = (row * 7 + field as u32) % 5;
                    let filler = 1 + (row * 3 + ordinal) % 19;
                    let mut words = vec!["alpha"; alpha as usize];
                    words.extend(vec!["beta"; beta as usize]);
                    words.extend(vec!["filler"; filler as usize]);
                    document.set(FieldId(field as u16), &words.join(" "));
                    *slot = [alpha, beta, alpha + beta + filler];
                }
                segment
                    .push_document(&analyzer, &document)
                    .expect("document");
                fixture.rows.push(counts);
            }
            fixture
                .index
                .push_sealed_with_live_rows(SealedSegment::seal(&segment).expect("sealed"), &alive)
                .expect("live index");
            fixture.alive.push(alive);
        }
        fixture
    }

    fn prepared(fixture: &Fixture, boosts: [u16; 4]) -> PreparedWeightedQuery {
        let expansions = ["alpha", "beta", "alpha", "absent"]
            .into_iter()
            .zip(boosts)
            .map(|(term, boost_thousandths)| LexicalExpansion {
                term: term.as_bytes().to_vec(),
                boost_thousandths,
                kind: LexicalMatchKind::Term,
            })
            .collect();
        PreparedWeightedQuery::new(
            &fixture.index,
            expansions,
            FieldWeights::new(&[(FieldId(0), 500), (FieldId(1), 1500)]),
        )
        .expect("weighted preparation")
    }

    fn literal_score(fixture: &Fixture, row: usize, term: usize) -> f64 {
        if term > 1 {
            return 0.0;
        }
        let live = |position: usize| fixture.alive[position / 96].contains((position % 96) as u32);
        let n = fixture
            .rows
            .iter()
            .enumerate()
            .filter(|(position, _)| live(*position))
            .count() as f64;
        let tokens: u32 = fixture
            .rows
            .iter()
            .enumerate()
            .filter(|(position, _)| live(*position))
            .map(|(_, fields)| fields[0][2] + fields[1][2])
            .sum();
        let df = fixture
            .rows
            .iter()
            .enumerate()
            .filter(|(position, fields)| {
                live(*position) && fields.iter().any(|field| field[term] > 0)
            })
            .count() as f64;
        let fields = fixture.rows[row];
        let tf = (fields[0][term] * 500 + fields[1][term] * 1500) / 1000;
        let length = (fields[0][2] * 500 + fields[1][2] * 1500) / 1000;
        if tf == 0 {
            return 0.0;
        }
        let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
        let average = f64::from(tokens) / n;
        let tf = f64::from(tf);
        idf * (tf * 2.2) / (tf + 1.2 * (0.25 + 0.75 * f64::from(length) / average))
    }

    fn exhaustive_weighted(
        index: &LexicalIndex,
        query: &PreparedWeightedQuery,
        allowed: &[&DocBitmap],
        k: usize,
    ) -> Vec<(GlobalDocId, u64)> {
        let mut full = std::collections::BTreeMap::<GlobalDocId, f64>::new();
        for expansion in query.expansions() {
            let term = crate::fts::search::TermQuery {
                terms: vec![expansion.term.clone()],
                fields: query.scoring().query().fields.clone(),
            };
            let result = if allowed.is_empty() {
                crate::fts::search::search(
                    index,
                    &term,
                    index.document_count() as usize,
                    crate::fts::bm25::Bm25Params::beir(),
                )
            } else {
                crate::fts::search::search_allow_list_driven(
                    index,
                    &term,
                    index.document_count() as usize,
                    crate::fts::bm25::Bm25Params::beir(),
                    allowed,
                )
            }
            .expect("exhaustive term");
            for hit in result.hits {
                *full.entry(hit.doc).or_default() +=
                    hit.score * (f64::from(expansion.boost_thousandths) / 1000.0);
            }
        }
        let mut full = full.into_iter().collect::<Vec<_>>();
        full.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        full.truncate(k);
        full.into_iter()
            .map(|(doc, score)| (doc, score.to_bits()))
            .collect()
    }

    #[test]
    fn ze541_weighted_bound_terms_follow_the_pivot() {
        let analyzer = Analyzer::new(Profile::Code.config()).expect("analyzer");
        let mut segment = SegmentIndex::new();
        for row in 0..4096 {
            let mut words = vec!["filler".to_owned()];
            if row % 10 == 0 {
                words.push("anchor".to_owned());
            }
            for term in 0..32 {
                if row == term * 109 + 3 {
                    words.push(format!("sparse{term}"));
                }
            }
            segment
                .push_document(&analyzer, &Document::with_text(&words.join(" ")))
                .expect("document");
        }
        let mut index = LexicalIndex::new();
        index.push_segment(segment).expect("seal");
        let terms =
            std::iter::once("anchor".to_owned()).chain((0..32).map(|term| format!("sparse{term}")));
        let query = PreparedWeightedQuery::new(
            &index,
            terms
                .map(|term| LexicalExpansion {
                    term: term.into_bytes(),
                    boost_thousandths: 1000,
                    kind: LexicalMatchKind::Term,
                })
                .collect(),
            FieldWeights::flat(&[FieldId(0)]),
        )
        .expect("prepare");
        let expected = exhaustive_weighted(&index, &query, &[], 10);
        crate::fts::preparation_observer::begin();
        let actual = search(
            &index,
            &query,
            10,
            &[],
            || Ok::<_, ()>(()),
            |_| Ok(true),
            |_| Ok(()),
        )
        .unwrap_or_else(|_| panic!("weighted"));
        let bounds = crate::fts::preparation_observer::take_with_bounds().4;
        assert_eq!(
            actual
                .hits
                .iter()
                .map(|(doc, score)| (*doc, score.to_bits()))
                .collect::<Vec<_>>(),
            expected
        );
        const RED_ON_MAIN: usize = 12_210;
        println!("ZE541 weighted bounds={bounds} red_on_main={RED_ON_MAIN}");
        assert!(
            3 * bounds <= RED_ON_MAIN,
            "3 * bounds={} > main={RED_ON_MAIN}",
            3 * bounds
        );
    }

    #[test]
    fn ze541_weighted_matches_exhaustive_at_the_expansion_cap() {
        use rand::Rng;
        let mut rng = crate::test_support::seeded_rng(
            "fts::prune::weighted::tests::ze541_weighted_matches_exhaustive_at_the_expansion_cap",
        );
        let analyzer = Analyzer::new(Profile::Code.config()).expect("analyzer");
        let mut segment = SegmentIndex::new();
        for row in 0..4096 {
            let mut words = vec![
                "filler".to_owned();
                if row < 64 {
                    1
                } else {
                    rng.random_range(20..70)
                }
            ];
            for term in 0..32 {
                if row % (2 + term * 3) == 0 || rng.random_range(0..1000) < 3 {
                    let tf = if row < 64 { 12 } else { rng.random_range(1..4) };
                    words.extend(vec![format!("term{term}"); tf]);
                }
            }
            segment
                .push_document(&analyzer, &Document::with_text(&words.join(" ")))
                .expect("document");
        }
        let mut index = LexicalIndex::new();
        index.push_segment(segment).expect("seal");
        let allowed = DocBitmap::from_ids((0..4096).filter(|row| row % 7 != 0));
        for count in [33, 64] {
            let query = PreparedWeightedQuery::new(
                &index,
                (0..count)
                    .map(|slot| LexicalExpansion {
                        term: format!("term{}", slot % 32).into_bytes(),
                        boost_thousandths: [0, u16::MAX, 1, 1000][slot % 4],
                        kind: LexicalMatchKind::Term,
                    })
                    .collect(),
                FieldWeights::flat(&[FieldId(0)]),
            )
            .expect("prepare");
            for allow in [vec![], vec![&allowed]] {
                for k in [1, 10, 100] {
                    let expected = exhaustive_weighted(&index, &query, &allow, k);
                    let actual = search(
                        &index,
                        &query,
                        k,
                        &allow,
                        || Ok::<_, ()>(()),
                        |_| Ok(true),
                        |_| Ok(()),
                    )
                    .unwrap_or_else(|_| panic!("weighted"));
                    assert_eq!(
                        actual
                            .hits
                            .iter()
                            .map(|(doc, score)| (*doc, score.to_bits()))
                            .collect::<Vec<_>>(),
                        expected,
                        "expansions={count} k={k} filtered={}",
                        !allow.is_empty()
                    );
                }
            }
        }
    }

    #[test]
    fn ze263_cursor_merge_work_scales_with_postings() {
        let analyzer = Analyzer::new(Profile::Code.config()).expect("analyzer");
        let mut segment = SegmentIndex::new();
        for row in 0..2048 {
            let mut document = Document::new();
            document.set(FieldId(0), &format!("topic{}", row % 256));
            segment
                .push_document(&analyzer, &document)
                .expect("document");
        }
        let mut index = LexicalIndex::new();
        index.push_sealed(SealedSegment::seal(&segment).expect("seal"));
        let expansions = (0..256)
            .map(|term| LexicalExpansion {
                term: format!("topic{term}").into_bytes(),
                boost_thousandths: 1000,
                kind: LexicalMatchKind::Term,
            })
            .collect();
        let query =
            PreparedWeightedQuery::new(&index, expansions, FieldWeights::flat(&[FieldId(0)]))
                .expect("prepare");
        HEAD_WORK.with(|count| count.set(0));
        let result = search(
            &index,
            &query,
            2048,
            &[],
            || Ok::<_, ()>(()),
            |_| Ok(true),
            |_| Ok(()),
        )
        .unwrap_or_else(|_| panic!("search"));
        let work = HEAD_WORK.with(std::cell::Cell::get);
        let mut full = std::collections::BTreeMap::<GlobalDocId, f64>::new();
        for expansion in query.expansions() {
            let term =
                crate::fts::search::TermQuery::flat(vec![expansion.term.clone()], &[FieldId(0)]);
            for hit in crate::fts::search::search(
                &index,
                &term,
                2048,
                crate::fts::bm25::Bm25Params::beir(),
            )
            .expect("exhaustive")
            .hits
            {
                *full.entry(hit.doc).or_default() += hit.score;
            }
        }
        let mut full = full.into_iter().collect::<Vec<_>>();
        full.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        assert_eq!(result.hits, full);
        println!("head scheduling inspections/comparisons={work}");
        assert!(work < 2048 * 32, "head scheduling work={work}");
    }

    #[test]
    fn astra_10_weighted_bounds_dominate_combined_score() {
        OMIT_BOUND_BOOST
            .with(|plant| plant.set(std::env::var_os("ZE_ASTRA10_BOUND_PLANT").is_some()));
        let fixture = fixture();
        let query = prepared(&fixture, [u16::MAX, 1, 250, 0]);
        for (ordinal, segment) in fixture.index.segments().iter().enumerate() {
            let mut cursors = Vec::new();
            for (slot, expansion) in query.expansions().iter().enumerate().take(3) {
                let scorer = query.scoring().scorer(slot).expect("scorer");
                let stream =
                    TermStream::open(segment, &expansion.term, &query.scoring().query().fields)
                        .expect("stream");
                let upper_bound = stream.upper_bound(&scorer);
                cursors.push(WeightedCursor {
                    term: TermCursor {
                        stream,
                        df: Df(0),
                        scorer,
                        upper_bound,
                    },
                    boost: f64::from(expansion.boost_thousandths) / 1000.0,
                });
            }
            for row in 0..96 {
                let mut exact = 0.0;
                let mut bounds = Vec::new();
                for (slot, cursor) in cursors.iter().enumerate() {
                    let raw = literal_score(&fixture, ordinal * 96 + row, slot % 2);
                    exact += raw * cursor.boost;
                    let bound = cursor.bound(
                        cursor
                            .term
                            .stream
                            .bound_for(row as u32, &cursor.term.scorer),
                    );
                    assert!(
                        bound >= raw * cursor.boost,
                        "weighted bound omitted a contribution: {bound} < {}",
                        raw * cursor.boost
                    );
                    bounds.push(bound);
                }
                assert!(
                    sum_bounds(bounds.into_iter()) >= exact,
                    "combined bound is conservative without a tolerance"
                );
            }
        }
        OMIT_BOUND_BOOST.with(|plant| plant.set(false));
    }

    #[test]
    fn ze263_heap_merge_preserves_seeks_and_score_order() {
        HEAD_SEEKS.with(|count| count.set(0));
        let fixture = fixture();
        for boosts in [[0, 0, 0, 0], [1, 65535, 250, 0], [1000, 500, 1000, 1]] {
            let query = prepared(&fixture, boosts);
            let mut oracle = Vec::new();
            for (position, _) in fixture.rows.iter().enumerate() {
                if !fixture.alive[position / 96].contains((position % 96) as u32) {
                    continue;
                }
                let alpha = literal_score(&fixture, position, 0);
                let beta = literal_score(&fixture, position, 1);
                if alpha == 0.0 && beta == 0.0 {
                    continue;
                }
                let score = ((alpha * f64::from(boosts[0]) / 1000.0)
                    + (beta * f64::from(boosts[1]) / 1000.0))
                    + (alpha * f64::from(boosts[2]) / 1000.0);
                oracle.push(ScoredDoc {
                    doc: GlobalDocId {
                        segment: (position / 96) as u32,
                        row: (position % 96) as u32,
                    },
                    score,
                });
            }
            oracle.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc.cmp(&b.doc)));
            for k in [0, 1, 10, 100, 300] {
                let result = search(
                    &fixture.index,
                    &query,
                    k,
                    &fixture.alive.iter().collect::<Vec<_>>(),
                    || Ok::<_, ()>(()),
                    |_| Ok(true),
                    |_| Ok(()),
                )
                .unwrap_or_else(|_| panic!("weighted search"));
                assert_eq!(result.hits.len(), k.min(oracle.len()));
                for (hit, expected) in result.hits.iter().zip(&oracle) {
                    assert_eq!(hit.0, expected.doc, "k={k} boosts={boosts:?}");
                    assert!(
                        (hit.1 - expected.score).abs() < 1e-12,
                        "independent weighted formula"
                    );
                }
                // The existing exhaustive term search is a second, bit-exact
                // control; the literal formula above is the independent oracle.
                let mut full = std::collections::BTreeMap::<GlobalDocId, f64>::new();
                for expansion in query.expansions() {
                    let term = crate::fts::search::TermQuery {
                        terms: vec![expansion.term.clone()],
                        fields: query.scoring().query().fields.clone(),
                    };
                    let result = crate::fts::search::search_allow_list_driven(
                        &fixture.index,
                        &term,
                        300,
                        crate::fts::bm25::Bm25Params::beir(),
                        &fixture.alive.iter().collect::<Vec<_>>(),
                    )
                    .expect("exhaustive term control");
                    for hit in result.hits {
                        *full.entry(hit.doc).or_default() +=
                            hit.score * (f64::from(expansion.boost_thousandths) / 1000.0);
                    }
                }
                let mut full = full
                    .into_iter()
                    .map(|(doc, score)| ScoredDoc { doc, score })
                    .collect::<Vec<_>>();
                full.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc.cmp(&b.doc)));
                full.truncate(k);
                assert_eq!(
                    result.hits,
                    full.iter()
                        .map(|hit| (hit.doc, hit.score))
                        .collect::<Vec<_>>(),
                    "exact score bits and ranking"
                );
            }
        }
        assert!(
            HEAD_SEEKS.with(std::cell::Cell::get) > 0,
            "oracle must exercise pruning seeks"
        );
    }
}

// Monotonic IEEE addition in the SAME order as scoring is the aggregate
// proof. Outward rounding also encloses each sum; equality is never pruned.
#[cfg(test)]
fn sum_bounds(values: impl Iterator<Item = f64>) -> f64 {
    match sum_bounds_controlled(values.map(Ok::<f64, std::convert::Infallible>)) {
        Ok(sum) => sum,
        Err(never) => match never {},
    }
}

fn sum_bounds_controlled<E>(mut values: impl Iterator<Item = Result<f64, E>>) -> Result<f64, E> {
    values.try_fold(0.0, |sum, value| {
        let value = value?;
        Ok(if value == 0.0 {
            sum
        } else {
            (sum + value).next_up()
        })
    })
}

fn bound_through_controlled<E>(
    cursors: &[WeightedCursor<'_>],
    row: u32,
    work: &mut crate::fts::control::WorkCheck<impl FnMut() -> Result<(), E>>,
) -> Result<f64, E> {
    sum_bounds_controlled(cursors.iter().map(|cursor| {
        work.step()?;
        Ok(if cursor.term.current().is_some_and(|head| head <= row) {
            cursor.bound(cursor.term.upper_bound)
        } else {
            0.0
        })
    }))
}

// Every live cursor owns exactly one heap entry. Seeks replace only entries
// below the target, preserving expansion slots for bit-exact score folding.
fn seek_heads<E: From<crate::fts::index::IndexError>>(
    heads: &mut BinaryHeap<CursorHead>,
    cursors: &mut [WeightedCursor<'_>],
    target: u32,
    work: &mut crate::fts::control::WorkCheck<impl FnMut() -> Result<(), E>>,
) -> Result<(), E> {
    while heads.peek().is_some_and(|head| (head.0).0.0 < target) {
        work.step()?;
        let Some(CursorHead(Reverse((_, slot)))) = heads.pop() else {
            break;
        };
        let cursor = cursors
            .get_mut(slot)
            .ok_or(crate::fts::index::IndexError::CursorSlotMissing { slot })?;
        #[cfg(test)]
        HEAD_SEEKS.with(|count| count.set(count.get() + 1));
        cursor.term.stream.seek_controlled(target, work)?;
        if let Some(row) = cursor.term.current() {
            heads.push(CursorHead(Reverse((row, slot))));
        }
    }
    Ok(())
}

/// One bounded producer for both structured Store seams. `allow_lists` belongs
/// to the same validated, pinned assembly as `prepared`; eligibility may fail
/// with the caller's typed text/cancellation error and returns no partial hits.
pub(crate) fn search<Control>(
    index: &LexicalIndex,
    prepared: &PreparedWeightedQuery,
    k: usize,
    allow_lists: &[&DocBitmap],
    mut checkpoint: impl FnMut() -> Result<(), Control>,
    mut eligible: impl FnMut(GlobalDocId) -> Result<bool, Control>,
    mut reserve: impl FnMut(Option<usize>) -> Result<(), Control>,
) -> Result<WeightedResult, ControlledSearchError<Control>> {
    let mut work = crate::fts::control::WorkCheck::new(|| {
        checkpoint().map_err(ControlledSearchError::Control)
    });
    work.check_now()?;
    let mut counters = SearchCounters::default();
    let capacity = k.min(usize::try_from(index.document_count()).unwrap_or(usize::MAX));
    if capacity == 0 {
        return Ok(WeightedResult::default());
    }
    let heap_bytes = capacity
        .checked_add(1)
        .and_then(|capacity| capacity.checked_mul(std::mem::size_of::<(GlobalDocId, f64)>()));
    reserve(heap_bytes).map_err(ControlledSearchError::Control)?;
    let mut heap = TopK::new(capacity);
    let scoring = prepared.scoring();
    let mut bound_terms = 0_usize;
    let mut peak_rows = 0_usize;
    for (ordinal, segment) in index.segments().iter().enumerate() {
        work.check_now()?;
        let segment_id = u32::try_from(ordinal).unwrap_or(u32::MAX);
        let allowed = allow_lists.get(ordinal).copied();
        let fields = &scoring.query().fields;
        let mut field_iter = fields.iter();
        let lengths = match (field_iter.next(), field_iter.next()) {
            (Some((field, 1000)), None) => segment.field_lengths(field),
            _ if fields.is_unit() && segment.fields().all(|field| fields.weight(field) == 1000) => {
                Some(segment.total_lengths())
            }
            _ => None,
        };
        let mut scratch_bytes = heap_bytes.and_then(|bytes| {
            prepared
                .expansions()
                .len()
                .checked_mul(
                    std::mem::size_of::<WeightedCursor<'_>>()
                        + 2 * std::mem::size_of::<Reverse<(u32, usize)>>()
                        + std::mem::size_of::<u32>(),
                )
                .and_then(|scratch| bytes.checked_add(scratch))
        });
        reserve(scratch_bytes).map_err(ControlledSearchError::Control)?;
        let mut cursors = Vec::with_capacity(prepared.expansions().len());
        let mut live = Vec::with_capacity(prepared.expansions().len());
        let mut at = Vec::with_capacity(prepared.expansions().len());
        let mut heads = BinaryHeap::with_capacity(prepared.expansions().len());
        for (slot, expansion) in prepared.expansions().iter().enumerate() {
            if slot.is_multiple_of(64) {
                work.check_now()?;
            }
            let Some(scorer) = scoring.scorer(slot) else {
                continue;
            };
            let df = Df(scoring.frequencies().get(slot).copied().unwrap_or(0));
            if df.0 == 0 {
                continue;
            }
            let previous_bytes = scratch_bytes;
            scratch_bytes = scratch_bytes.and_then(|bytes| {
                TermStream::allocation_bytes(segment, &expansion.term, fields)
                    .and_then(|stream| bytes.checked_add(stream))
            });
            reserve(scratch_bytes).map_err(ControlledSearchError::Control)?;
            let Some(stream) =
                TermStream::open_sized_controlled(segment, &expansion.term, fields, &mut work)?
            else {
                scratch_bytes = previous_bytes;
                reserve(scratch_bytes).map_err(ControlledSearchError::Control)?;
                continue;
            };
            if stream.exhausted() {
                drop(stream);
                scratch_bytes = previous_bytes;
                reserve(scratch_bytes).map_err(ControlledSearchError::Control)?;
                continue;
            }
            let upper_bound = stream.upper_bound_controlled(&scorer, &mut work)?;
            if let Some(row) = stream.current_row() {
                heads.push(CursorHead(Reverse((row, cursors.len()))));
            }
            cursors.push(WeightedCursor {
                term: TermCursor {
                    stream,
                    df,
                    scorer,
                    upper_bound,
                },
                boost: f64::from(expansion.boost_thousandths) / 1_000.0,
            });
        }
        loop {
            work.check_now()?;
            #[cfg(test)]
            HEAD_WORK.with(|count| count.set(count.get() + 1));
            let Some(&CursorHead(Reverse((first, _)))) = heads.peek() else {
                break;
            };
            // Pop the equal-row prefix in expansion order. Cursors above
            // it contribute skipped zeroes to both bounds at this pivot.
            at.clear();
            while heads.peek().is_some_and(|head| (head.0).0.0 == first) {
                work.step()?;
                #[cfg(test)]
                HEAD_WORK.with(|count| count.set(count.get() + 1));
                if let Some(CursorHead(Reverse((_, slot)))) = heads.pop() {
                    at.push(slot);
                }
            }
            let threshold = heap.threshold();
            let mut pivot = first;
            if heap.is_full() {
                bound_terms = bound_terms.saturating_add(at.len());
                let bound = sum_bounds_controlled(at.iter().map(|slot| {
                    work.step()?;
                    let cursor = cursors.get(*slot).ok_or(ControlledSearchError::Index(
                        crate::fts::index::IndexError::CursorSlotMissing { slot: *slot },
                    ))?;
                    Ok::<f64, ControlledSearchError<Control>>(cursor.bound(cursor.term.upper_bound))
                }))?;
                if bound < threshold {
                    let Some(&CursorHead(Reverse((next, _)))) = heads.peek() else {
                        break;
                    };
                    bound_terms = bound_terms.saturating_add(cursors.len());
                    if bound_through_controlled(&cursors, next, &mut work)? >= threshold {
                        pivot = next;
                    } else {
                        live.clear();
                        live.push(first);
                        for head in heads.iter() {
                            work.step()?;
                            live.push((head.0).0.0);
                        }
                        live.sort_unstable();
                        live.dedup();
                        let mut failure = None;
                        let position = live.partition_point(|row| {
                            if failure.is_some() {
                                return false;
                            }
                            bound_terms = bound_terms.saturating_add(cursors.len());
                            match bound_through_controlled(&cursors, *row, &mut work) {
                                Ok(bound) => bound < threshold,
                                Err(error) => {
                                    failure = Some(error);
                                    false
                                }
                            }
                        });
                        if let Some(error) = failure {
                            return Err(error);
                        }
                        let Some(&row) = live.get(position) else {
                            break;
                        };
                        pivot = row;
                    }
                }
            }
            let mut target = (first < pivot).then_some(pivot);
            if target.is_none() && heap.is_full() {
                // The next head bounds the zero contribution of all cursors
                // above the pivot; only the popped cursors need block reads.
                let mut edge = heads.peek().map(|head| (head.0).0.0.saturating_sub(1));
                let bound = sum_bounds_controlled(at.iter().map(|slot| {
                    work.step()?;
                    let cursor = cursors.get(*slot).ok_or(ControlledSearchError::Index(
                        crate::fts::index::IndexError::CursorSlotMissing { slot: *slot },
                    ))?;
                    let stream = &cursor.term.stream;
                    if let Some(horizon) = stream.bound_horizon_for_controlled(pivot, &mut work)? {
                        edge = Some(edge.map_or(horizon, |held| held.min(horizon)));
                    }
                    Ok::<f64, ControlledSearchError<Control>>(cursor.bound(
                        stream.bound_for_controlled(pivot, &cursor.term.scorer, &mut work)?,
                    ))
                }))?;
                bound_terms = bound_terms.saturating_add(at.len());
                if bound < threshold {
                    let Some(next) = edge.and_then(|row| row.checked_add(1)) else {
                        break;
                    };
                    target = Some(next);
                }
            }
            if let Some(target) = target {
                for slot in &at {
                    work.step()?;
                    let cursor = cursors.get_mut(*slot).ok_or(ControlledSearchError::Index(
                        crate::fts::index::IndexError::CursorSlotMissing { slot: *slot },
                    ))?;
                    #[cfg(test)]
                    HEAD_SEEKS.with(|count| count.set(count.get() + 1));
                    cursor.term.stream.seek_controlled(target, &mut work)?;
                    if let Some(row) = cursor.term.current() {
                        heads.push(CursorHead(Reverse((row, *slot))));
                    }
                }
                seek_heads(&mut heads, &mut cursors, target, &mut work)?;
                continue;
            }
            let length = match lengths
                .and_then(|lengths| lengths.get(pivot as usize))
                .copied()
            {
                Some(length) => length,
                None => {
                    crate::fts::search::row_length_controlled(segment, pivot, fields, &mut work)?
                }
            };
            let mut score = 0.0;
            for slot in at.iter().copied() {
                work.step()?;
                let cursor = cursors.get_mut(slot).ok_or(ControlledSearchError::Index(
                    crate::fts::index::IndexError::CursorSlotMissing { slot },
                ))?;
                counters.postings_decoded = counters.postings_decoded.saturating_add(1);
                let tf = Tf(cursor
                    .term
                    .stream
                    .current_tf_controlled(&mut work)?
                    .unwrap_or(0));
                score += cursor.term.scorer.score(tf, DocLen(length)) * cursor.boost;
                cursor.term.stream.advance_controlled(&mut work)?;
                if let Some(row) = cursor.term.current() {
                    heads.push(CursorHead(Reverse((row, slot))));
                }
            }
            counters.docs_evaluated = counters.docs_evaluated.saturating_add(1);
            let doc = GlobalDocId {
                segment: segment_id,
                row: pivot,
            };
            if allowed.is_none_or(|allowed| allowed.contains(pivot))
                && eligible(doc).map_err(ControlledSearchError::Control)?
            {
                heap.offer_observed(doc, score, |rows| peak_rows = peak_rows.max(rows));
            }
        }
        for cursor in &cursors {
            counters.blocks_decoded = counters
                .blocks_decoded
                .saturating_add(cursor.term.stream.blocks_decoded());
            counters.blocks_skipped = counters
                .blocks_skipped
                .saturating_add(cursor.term.stream.blocks_skipped());
        }
    }
    reserve(heap_bytes).map_err(ControlledSearchError::Control)?;
    work.check_now()?;
    #[cfg(any(test, feature = "test-seams"))]
    {
        crate::fts::preparation_observer::structured_collection(peak_rows);
        crate::fts::preparation_observer::structured_bounds(bound_terms);
    }
    #[cfg(not(any(test, feature = "test-seams")))]
    let _ = (peak_rows, bound_terms);
    Ok(WeightedResult {
        hits: heap.entries,
        counters,
    })
}
