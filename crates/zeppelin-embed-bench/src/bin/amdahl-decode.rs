//! Measures decode's share of query time, the P4 Amdahl guard.
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::time::Instant;

use zeppelin_embed::fts::bm25::Bm25Params;
use zeppelin_embed::fts::index::{DEFAULT_FIELD, Document, LexicalIndex, SegmentIndex};
use zeppelin_embed::fts::prune::{Strategy, search_pruned};
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fts::tokenizer::{Analyzer, Profile};
use zeppelin_embed::kernels::postings::{prefix_sum, unpack, unpack_scalar};

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

fn pack(values: &[u32], bits: u8) -> Vec<u8> {
    let mut out = vec![0_u8; (values.len() * usize::from(bits)).div_ceil(8) + 16];
    let mut bit = 0_usize;
    for value in values {
        for step in 0..usize::from(bits) {
            if value >> step & 1 == 1 {
                out[(bit + step) / 8] |= 1 << ((bit + step) % 8);
            }
        }
        bit += usize::from(bits);
    }
    out
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
    let cases: [(&str, Vec<Vec<u8>>, Strategy); 2] = [
        (
            "2term_wand",
            vec![b"t0".to_vec(), b"t5".to_vec()],
            Strategy::BlockMaxWand,
        ),
        (
            "6term_maxscore",
            (0..6).map(|i| format!("t{i}").into_bytes()).collect(),
            Strategy::BlockMaxMaxscore,
        ),
    ];

    for (label, terms, strategy) in cases {
        let query = TermQuery::flat(terms, &[DEFAULT_FIELD]);
        // Warm.
        for _ in 0..3 {
            search_pruned(&index, &query, 10, params, strategy).expect("scores");
        }
        // Per-query samples, not a mean: a mean over a run hides exactly
        // the tail that scheduling and allocation problems live in.
        let iterations = 200_usize;
        let mut samples: Vec<f64> = Vec::with_capacity(iterations);
        let mut blocks = 0_u64;
        for _ in 0..iterations {
            let started = Instant::now();
            let result = search_pruned(&index, &query, 10, params, strategy).expect("scores");
            samples.push(started.elapsed().as_nanos() as f64);
            blocks = result.counters.blocks_decoded;
            std::hint::black_box(&result.hits);
        }
        samples.sort_by(f64::total_cmp);
        let percentile = |fraction: f64| -> f64 {
            let rank = (fraction * (samples.len() - 1) as f64).round() as usize;
            samples[rank.min(samples.len() - 1)]
        };
        let per_query_ns = percentile(0.50);
        println!(
            "QUERY {label} p50_us={:.1} p99_us={:.1} blocks_decoded={blocks}",
            per_query_ns / 1000.0,
            percentile(0.99) / 1000.0
        );

        // Decode cost of exactly that many blocks, at the measured widths:
        // a 64-value docid stream near 2 bits and a 64-value tf stream at 1.
        // `mode` 0 = narrow unpack + prefix sum, 1 = scalar unpack + prefix
        // sum, 2 = prefix sum alone. The third isolates K4's target.
        for (kernel, mode) in [("narrow", 0_u8), ("scalar", 1), ("prefixsum", 2)] {
            let mut total = 0_u128;
            for (bits, span) in [(2_u8, 4_u32), (1_u8, 2_u32)] {
                let values: Vec<u32> = (0..64_u32).map(|i| i % span).collect();
                let packed = pack(&values, bits);
                let mut out = vec![0_u32; 64];
                let reps = 20_000_u32;
                let started = Instant::now();
                for _ in 0..reps {
                    match mode {
                        0 => {
                            unpack(&packed, bits, 64, &mut out).expect("decodes");
                        }
                        1 => {
                            unpack_scalar(&packed, bits, 64, &mut out).expect("decodes");
                        }
                        _ => {}
                    }
                    prefix_sum(&mut out, 0);
                    std::hint::black_box(&out);
                }
                total += started.elapsed().as_nanos() / u128::from(reps);
            }
            let decode_us = total as f64 * blocks as f64 / 1000.0;
            println!(
                "  DECODE {kernel} ns_per_block={total} \
                 total_us={decode_us:.1} share={:.1}%",
                decode_us * 1000.0 / per_query_ns * 100.0
            );
        }
    }
}
