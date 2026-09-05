#![allow(clippy::expect_used, clippy::panic)]

use super::*;
use crate::fts::{index::DEFAULT_FIELD, preparation_observer, search::TermQuery};
use crate::fusion::HybridQuery;
use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest};
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

struct LexicalGateClock {
    base: Instant,
    entered: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    gated: AtomicBool,
    released: AtomicBool,
    expired: AtomicBool,
}

impl MonotonicClock for LexicalGateClock {
    fn now(&self) -> Instant {
        if std::thread::current().name() == Some("zeppelin-fts")
            && !self.gated.swap(true, Ordering::SeqCst)
        {
            self.entered.send(()).expect("lexical gate entered");
            self.release
                .lock()
                .expect("gate lock")
                .recv_timeout(Duration::from_secs(5))
                .expect("release lexical gate");
            self.released.store(true, Ordering::SeqCst);
        }
        self.base + Duration::from_secs(u64::from(self.expired.load(Ordering::SeqCst)) * 2)
    }
}

#[test]
fn astra_19_embed_failure_joins_lexical_work() {
    use crate::fusion::{FusionError, FusionLeg};
    for mode in ["error", "panic", "timeout", "close"] {
        let directory = tempfile::tempdir().expect("fixture");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let clock = Arc::new(LexicalGateClock {
            base: Instant::now(),
            entered: entered_tx,
            release: Mutex::new(release_rx),
            gated: AtomicBool::new(false),
            released: AtomicBool::new(false),
            expired: AtomicBool::new(false),
        });
        let mut store = Store::open(directory.path(), OpenOptions::default()).expect("open");
        store.clock = clock.clone();
        let store = Arc::new(store);
        store
            .ingest(IngestBatch::new(vec![document(1, "bronze zeppelin")]))
            .expect("ingest");
        let deadline = Deadline::after_with_test_clock(Duration::from_secs(1), clock.clone())
            .expect("deadline");
        let query_store = Arc::clone(&store);
        let query_clock = Arc::clone(&clock);
        let (prepared_tx, prepared_rx) = mpsc::channel();
        let (returned_tx, returned_rx) = mpsc::channel();
        let task = std::thread::spawn(move || {
            let query = TermQuery::flat(vec![b"bronze".to_vec()], &[DEFAULT_FIELD]);
            let result = query_store.search_hybrid_with_text_deferred(
                || -> Result<SearchRequest<'static>, &'static str> {
                    let admitted =
                        query_store.query_materialization_test_counters().admissions == 1;
                    if admitted {
                        entered_rx
                            .recv_timeout(Duration::from_secs(5))
                            .expect("lexical is pending");
                    }
                    prepared_tx.send(admitted).expect("embedding completion");
                    if mode == "timeout" {
                        query_clock.expired.store(true, Ordering::SeqCst);
                    }
                    if mode == "panic" {
                        panic!("fake embedding panic");
                    }
                    Err("fake embedding failure")
                },
                &query,
                &HybridQuery::new(1),
                SearchOptions::default(),
                QueryControl::Deadline(deadline),
                |_, _| panic!("must not materialize a failed query"),
            );
            returned_tx.send(()).expect("query returned");
            result
        });
        let admitted = prepared_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("embedding ran");
        let mut close = None;
        if admitted {
            assert!(
                returned_rx.try_recv().is_err(),
                "query returned with lexical gate held"
            );
            assert!(!clock.released.load(Ordering::SeqCst));
            if mode == "close" {
                let lease = store.snapshot().expect("observe close");
                let closing = Arc::clone(&store);
                close = Some(std::thread::spawn(move || closing.close()));
                lease
                    .wait_for_close_cancellation()
                    .expect("close cancelled admission");
            }
            release_tx.send(()).expect("allow lexical exit");
        }
        let result = task.join().expect("query thread joined");
        let receipt = store.take_hybrid_execution_receipt();
        if let Some(close) = close {
            close.join().expect("close thread").expect("close");
        } else {
            store.close().expect("close");
        }
        println!(
            "{mode}: admitted={admitted}, lexical released={}, result={result:?}",
            clock.released.load(Ordering::SeqCst)
        );
        assert!(
            admitted,
            "failure path must have admitted lexical work before embedding"
        );
        assert!(
            clock.released.load(Ordering::SeqCst),
            "lexical work escaped the query"
        );
        let receipt = receipt.expect("failure retains the existing execution receipt");
        assert!(receipt.vector_completed && receipt.lexical_completed);
        match mode {
            "error" => assert!(matches!(
                result,
                Err(HybridPreparationError::Preparation(
                    "fake embedding failure"
                ))
            )),
            "panic" => assert!(matches!(
                result,
                Err(HybridPreparationError::Search(FusionError::LegPanic {
                    leg: FusionLeg::Vector,
                    ..
                }))
            )),
            "timeout" => assert!(matches!(
                result,
                Err(HybridPreparationError::Search(FusionError::Timeout {
                    partial: false
                }))
            )),
            "close" => assert!(matches!(
                result,
                Err(HybridPreparationError::Search(FusionError::ReadCancelled {
                    partial: false
                }))
            )),
            _ => panic!("unknown test mode"),
        }
    }
}

fn document(revision: u64, text: &str) -> IngestDocument {
    IngestDocument::new(
        DocumentVersion::new(DocId::new(1), Revision::new(revision)),
        vec![1.0, 0.0],
    )
    .with_text(text)
}

#[test]
fn astra_19_lexical_starts_before_embedding_completion() {
    let directory = tempfile::tempdir().expect("fixture");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    store
        .ingest(IngestBatch::new(vec![document(1, "bronze zeppelin")]))
        .expect("ingest");
    let query = TermQuery::flat(vec![b"bronze".to_vec()], &[DEFAULT_FIELD]);
    preparation_observer::begin();
    let admitted_before_embedding = std::cell::Cell::new(false);
    let lexical_before_completion = std::cell::Cell::new(false);
    let result = store.search_hybrid_with_text_deferred(
        || {
            let admitted = store.query_materialization_test_counters().admissions == 1;
            admitted_before_embedding.set(admitted);
            // The serial RED has not admitted/submitted anything. Return the
            // fake embedding immediately so its failure is ordering, not a hang.
            if admitted {
                let watchdog = Instant::now() + Duration::from_secs(5);
                while preparation_observer::corpus_statistics_calls() == 0 {
                    assert!(
                        Instant::now() < watchdog,
                        "watchdog: lexical producer did not run"
                    );
                    std::thread::yield_now();
                }
                // This observation is made while embedding is still pending.
                lexical_before_completion.set(true);
            }
            Ok::<_, Infallible>(SearchRequest::new(&[1.0, 0.0]))
        },
        &query,
        &HybridQuery::new(1),
        SearchOptions::default(),
        QueryControl::Cancel(CancelToken::new()),
        |_, materializer| materializer.text(0),
    );
    let counts = preparation_observer::take();
    let (_, text) = result.expect("hybrid query");
    assert_eq!(text.expect("text").text, "bronze zeppelin");
    println!(
        "admitted_before_embedding={}, lexical_before_completion={}, scorer/frequency={counts:?}",
        admitted_before_embedding.get(),
        lexical_before_completion.get()
    );
    assert!(
        admitted_before_embedding.get(),
        "pin admission before embedding begins"
    );
    assert!(
        lexical_before_completion.get(),
        "lexical work must start before embedding completes"
    );
    store.close().expect("close");
}

#[test]
fn astra_19_overlap_uses_one_snapshot_despite_concurrent_ingest() {
    let directory = tempfile::tempdir().expect("fixture");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    let original = store
        .ingest(IngestBatch::new(vec![document(1, "bronze original")]))
        .expect("ingest original");
    let query = TermQuery::flat(vec![b"bronze".to_vec()], &[DEFAULT_FIELD]);
    let (_, row) = store
        .search_hybrid_with_text_deferred(
            || {
                store
                    .ingest(IngestBatch::new(vec![document(2, "bronze replacement")]))
                    .expect("mutation while fake embedding is pending");
                Ok::<_, Infallible>(SearchRequest::new(&[1.0, 0.0]))
            },
            &query,
            &HybridQuery::new(1),
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
            |outcome, materializer| (outcome.generation, materializer.text(0)),
        )
        .expect("query");
    let (generation, row) = row;
    let row = row.expect("pinned text");
    println!("original_ack={original:?}, generation={generation}, row={row:?}");
    assert_eq!(row.document.revision(), Revision::new(1));
    assert_eq!(row.text, "bronze original");
    assert_eq!(store.query_materialization_test_counters().admissions, 1);
    let (_, fresh) = store
        .search_hybrid_with_text(
            SearchRequest::new(&[1.0, 0.0]),
            &query,
            &HybridQuery::new(1),
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
            |_, materializer| materializer.text(0),
        )
        .expect("fresh query");
    assert_eq!(
        fresh.expect("fresh text").document.revision(),
        Revision::new(2)
    );
    store.close().expect("close");
}

#[test]
fn astra_19_overlapped_hybrid_matches_serial_scores_and_text() {
    let directory = tempfile::tempdir().expect("fixture");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    let documents = (0..400)
        .map(|row| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(row + 1), Revision::new(1)),
                vec![1.0, row as f32 * 0.0025],
            )
            .with_text(if row < 200 {
                "copper".to_owned()
            } else {
                vec!["zeppelin"; 1 + (400 - row as usize) % 5].join(" ")
            })
        })
        .collect();
    store
        .ingest(IngestBatch::new(documents))
        .expect("ingest disjoint windows");
    let lexical = TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]);
    let options = SearchOptions::default().with_tier(SearchTier::Exact);
    let run_serial = || {
        store
            .search_hybrid_with_text(
                SearchRequest::new(&[1.0, 0.0]),
                &lexical,
                &HybridQuery::new(1),
                options,
                QueryControl::Cancel(CancelToken::new()),
                |outcome, rows| {
                    (0..outcome.hits.len())
                        .map(|rank| rows.text(rank))
                        .collect::<Result<Vec<_>, _>>()
                },
            )
            .expect("vector prepared before retrieval")
    };
    let _ = run_serial(); // Match cache state on both measured work controls.
    let (serial, serial_text) = run_serial();
    let calls = std::cell::Cell::new(0);
    let (overlapped, overlap_text) = store
        .search_hybrid_with_text_deferred(
            || {
                calls.set(calls.get() + 1);
                Ok::<_, Infallible>(SearchRequest::new(&[1.0, 0.0]))
            },
            &lexical,
            &HybridQuery::new(1),
            options,
            QueryControl::Cancel(CancelToken::new()),
            |outcome, rows| {
                (0..outcome.hits.len())
                    .map(|rank| rows.text(rank))
                    .collect::<Result<Vec<_>, _>>()
            },
        )
        .expect("overlapped query");
    assert_eq!(calls.get(), 1, "widening must not repeat embedding");
    assert_eq!(overlapped.generation, serial.generation);
    assert_eq!(overlapped.hits, serial.hits);
    for (actual, expected) in overlapped.hits.iter().zip(&serial.hits) {
        assert_eq!(actual.fused_score.to_bits(), expected.fused_score.to_bits());
        assert_eq!(
            actual.vector_squared_l2.map(f64::to_bits),
            expected.vector_squared_l2.map(f64::to_bits)
        );
        assert_eq!(
            actual.lexical_bm25.map(f64::to_bits),
            expected.lexical_bm25.map(f64::to_bits)
        );
    }
    assert_eq!(
        overlap_text.expect("overlapped text"),
        serial_text.expect("serial text")
    );
    assert_eq!(overlapped.diagnostics.counters, serial.diagnostics.counters);
    assert_eq!(overlapped.diagnostics.fusion, serial.diagnostics.fusion);
    assert_eq!(
        overlapped
            .diagnostics
            .fusion
            .as_ref()
            .expect("fusion")
            .rounds,
        3
    );
    assert_eq!(overlapped.diagnostics.counters.scan.dims_touched, 2_800);
    println!("three rounds, one embedding, exact score/text/generation/work parity");
    store.close().expect("close");
}

#[test]
fn astra_19_preparation_cancel_preserves_control_error() {
    let directory = tempfile::tempdir().expect("fixture");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    store
        .ingest(IngestBatch::new(vec![document(1, "bronze zeppelin")]))
        .expect("ingest");
    let token = CancelToken::new();
    let result = store.search_hybrid_with_text_deferred(
        || -> Result<SearchRequest<'static>, &'static str> {
            token.cancel();
            Err("late embedding failure")
        },
        &TermQuery::flat(vec![b"bronze".to_vec()], &[DEFAULT_FIELD]),
        &HybridQuery::new(1),
        SearchOptions::default(),
        QueryControl::Cancel(token.clone()),
        |_, _| panic!("cancelled query cannot materialize"),
    );
    assert!(matches!(
        result,
        Err(HybridPreparationError::Search(
            crate::fusion::FusionError::Cancelled { partial: false }
        ))
    ));
    store.close().expect("close");
}
