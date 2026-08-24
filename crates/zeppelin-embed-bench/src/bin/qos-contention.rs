//! Measures what requesting a QoS class buys a query thread under load.
//!
//! P5.1. Apple silicon has no thread-affinity API, so the QoS class is the
//! only supported way to influence P-core against E-core placement. The
//! engine has always OBSERVED a class it never asked for; `sys::darwin::
//! request_qos` supplies the mechanism, and this supplies the number the
//! owner needs to decide whether the engine should use it (O5).
//!
//! # Why it makes the machine loud on purpose
//!
//! A quiesced run measures nothing here: with idle P-cores every thread
//! gets one regardless of class, so both arms are identical. Demotion only
//! bites under contention. The harness therefore creates its own load and
//! measures both arms against it, which also makes the comparison
//! self-controlled rather than dependent on a quiet machine.
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use zeppelin_embed::fts::bm25::Bm25Params;
use zeppelin_embed::fts::index::{DEFAULT_FIELD, Document, LexicalIndex, SegmentIndex};
use zeppelin_embed::fts::prune::{Strategy, search_pruned};
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fts::tokenizer::{Analyzer, Profile};
use zeppelin_embed::sys::darwin::{QosClass, observed_qos, request_qos};

/// Background threads competing for cores. The machine is 12 P plus 4 E.
const SPINNERS: usize = 16;
/// Queries per arm per round.
const SAMPLES: usize = 150;
/// Alternating rounds, so drift affects both arms equally.
const ROUNDS: usize = 3;

fn zipf_corpus(documents: usize, terms: usize) -> Vec<String> {
    (0..documents)
        .map(|doc| {
            let mut words: Vec<String> = Vec::new();
            for term in 0..terms {
                if doc % (term + 1) == 0 {
                    words.push(format!("t{term}"));
                }
            }
            if words.is_empty() {
                words.push(String::from("filler"));
            }
            words.join(" ")
        })
        .collect()
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = (fraction * (sorted.len() - 1) as f64).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

fn main() {
    let analyzer = Analyzer::new(Profile::Code.config()).expect("valid");
    let texts = zipf_corpus(100_000, 12);
    let mut segment = SegmentIndex::new();
    for text in &texts {
        segment
            .push_document(&analyzer, &Document::with_text(text))
            .expect("indexable");
    }
    let mut index = LexicalIndex::new();
    index.push_segment(segment).expect("seals");
    let params = Bm25Params::default();
    let terms: Vec<Vec<u8>> = (0..6).map(|i| format!("t{i}").into_bytes()).collect();
    let query = TermQuery::flat(terms, &[DEFAULT_FIELD]);

    let index = &index;
    let query = &query;

    let running = AtomicBool::new(true);
    let burned = AtomicU64::new(0);
    let mut baseline: Vec<f64> = Vec::new();
    let mut promoted: Vec<f64> = Vec::new();

    std::thread::scope(|scope| {
        // Synthetic host load at the unannotated default class: the shape a
        // host application's own worker threads have.
        for _ in 0..SPINNERS {
            scope.spawn(|| {
                let mut accumulator = 0_u64;
                while running.load(Ordering::Relaxed) {
                    for step in 0..10_000_u64 {
                        accumulator = accumulator
                            .wrapping_mul(6_364_136_223_846_793_005)
                            .wrapping_add(step);
                    }
                }
                burned.fetch_add(accumulator | 1, Ordering::Relaxed);
            });
        }

        for round in 0..ROUNDS {
            for (label, requested) in [
                ("baseline", None),
                ("interactive", Some(QosClass::UserInteractive)),
            ] {
                // A fresh thread per arm: a QoS class holds for the life of
                // the thread that asked, so the arms cannot share one.
                let samples = scope
                    .spawn(move || {
                        if let Some(class) = requested {
                            request_qos(class).expect("Darwin accepts the request");
                        }
                        let (observed, _) = observed_qos().expect("readable");
                        let mut samples: Vec<f64> = Vec::with_capacity(SAMPLES);
                        for _ in 0..SAMPLES {
                            let started = Instant::now();
                            let result =
                                search_pruned(index, query, 10, params, Strategy::BlockMaxMaxscore)
                                    .expect("scores");
                            samples.push(started.elapsed().as_nanos() as f64 / 1000.0);
                            std::hint::black_box(&result.hits);
                        }
                        (observed, samples)
                    })
                    .join()
                    .expect("the measurement thread must not panic");
                let (observed, samples) = samples;
                if round == 0 {
                    println!("ARM {label} observed_qos={observed:?}");
                }
                if requested.is_some() {
                    promoted.extend_from_slice(&samples);
                } else {
                    baseline.extend_from_slice(&samples);
                }
            }
        }
        running.store(false, Ordering::Relaxed);
    });

    for (label, mut samples) in [("baseline", baseline), ("interactive", promoted)] {
        samples.sort_by(f64::total_cmp);
        println!(
            "QOS {label} n={} p50={:.0}us p99={:.0}us",
            samples.len(),
            percentile(&samples, 0.50),
            percentile(&samples, 0.99),
        );
    }
    println!("spinners={SPINNERS} rounds={ROUNDS} samples_per_arm_per_round={SAMPLES}");
}
