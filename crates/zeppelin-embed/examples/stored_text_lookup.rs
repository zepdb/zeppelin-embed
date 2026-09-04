//! Release-build micro-benchmark for `Store::stored_text`.
//!
//! Builds a store with 171,332 sealed rows spread across four segments, then
//! times ten `stored_text` lookups whose targets sit at the end of the last
//! segment (the worst case for a linear scan). Reports the p50 over 200
//! iterations after 20 warm-up iterations.
//!
//! Run with `cargo run --release -p zeppelin-embed --example stored_text_lookup`.

use std::time::{Duration, Instant};

use zeppelin_embed::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use zeppelin_embed::lifecycle::{OpenOptions, Store};

const TOTAL_ROWS: u128 = 171_332;
const SEGMENTS: u128 = 4;
const LOOKUPS: u128 = 10;
const WARM: usize = 20;
const ITERATIONS: usize = 200;

fn main() {
    let directory = tempfile::tempdir().expect("scratch directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");

    let per_segment = TOTAL_ROWS.div_ceil(SEGMENTS);
    let mut next = 0_u128;
    while next < TOTAL_ROWS {
        let end = (next + per_segment).min(TOTAL_ROWS);
        let documents = (next..end)
            .map(|doc_id| {
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(doc_id), Revision::new(1)),
                    vec![doc_id as f32, 1.0],
                )
                .with_text(format!("stored text for document {doc_id}"))
            })
            .collect();
        store
            .ingest(IngestBatch::new(documents))
            .expect("ingest segment rows");
        store.seal().expect("seal segment");
        next = end;
    }

    let targets: Vec<DocumentVersion> = (TOTAL_ROWS - LOOKUPS..TOTAL_ROWS)
        .map(|doc_id| DocumentVersion::new(DocId::new(doc_id), Revision::new(1)))
        .collect();
    let cold = Instant::now();
    for target in &targets {
        let expected = format!("stored text for document {}", target.doc_id().get());
        assert_eq!(
            store.stored_text(*target).expect("lookup"),
            Some(expected),
            "target text mismatch"
        );
    }
    let cold = cold.elapsed();

    let mut samples = Vec::with_capacity(ITERATIONS);
    for iteration in 0..WARM + ITERATIONS {
        let started = Instant::now();
        for target in &targets {
            let text = store.stored_text(*target).expect("lookup");
            std::hint::black_box(text);
        }
        let elapsed = started.elapsed();
        if iteration >= WARM {
            samples.push(elapsed);
        }
    }
    samples.sort();
    let p50 = samples[samples.len() / 2];
    let min = samples[0];
    let max = samples[samples.len() - 1];
    println!(
        "stored_text x{LOOKUPS} over {TOTAL_ROWS} sealed rows in {SEGMENTS} segments: p50 {:.3} ms, min {:.3} ms, max {:.3} ms (first cold pass {:.3} ms)",
        ms(p50),
        ms(min),
        ms(max),
        ms(cold)
    );
    store.close().expect("close store");
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}
