//! Step 15: standalone lexical setup and live assembly through public APIs.
//!
//! For attribution, build this source as an external binary with a core path
//! dependency and no features: the bench crate otherwise enables test-support.
use serde_json::json;
use std::error::Error;
use std::hint::black_box;
use std::time::Instant;
use zeppelin_embed::fts::{
    bm25::Bm25Params,
    index::{DEFAULT_FIELD, Document, LexicalIndex, SegmentIndex},
    sealed::SealedSegment,
    search::TermQuery,
    tokenizer::{Analyzer, Profile},
};
use zeppelin_embed::meta::DocBitmap;
use zeppelin_embed::planner::{LexicalBranch, search_lexical_filtered};

fn main() -> Result<(), Box<dyn Error>> {
    let warmups = 20;
    let samples = 128;
    let analyzer = Analyzer::new(Profile::Code.config())?;
    let absent = TermQuery::flat(vec![b"absent".to_vec()], &[DEFAULT_FIELD]);
    let present = TermQuery::flat(vec![b"present".to_vec()], &[DEFAULT_FIELD]);
    let mut cases = Vec::new();
    for rows in [58_980, 131_072] {
        let mut active = SegmentIndex::new();
        for _ in 0..rows {
            active.push_document(&analyzer, &Document::with_text("present pair"))?;
        }
        let sealed = SealedSegment::seal(&active)?;
        let mut index = LexicalIndex::new();
        index.push_sealed(sealed.clone());
        for (density, allowed) in [
            ("dense", DocBitmap::full(rows)),
            ("sparse", DocBitmap::from_ids((0..rows).step_by(97))),
        ] {
            let allowed = [allowed];
            let documents = allowed[0].cardinality();
            let mut setup_ns = Vec::new();
            let mut assembly_ns = Vec::new();
            for i in 0..warmups + samples {
                let start = Instant::now();
                let result = search_lexical_filtered(
                    black_box(&index),
                    black_box(&absent),
                    10,
                    Bm25Params::default(),
                    black_box(&allowed),
                    Some(LexicalBranch::PostCheck),
                )?;
                let elapsed = start.elapsed().as_nanos() as u64;
                assert!(result.result.hits.is_empty());
                if i >= warmups {
                    setup_ns.push(elapsed);
                }
            }
            for i in 0..warmups + samples {
                // Exclude copying the owned segment and index destruction.
                let owned = sealed.clone();
                let mut assembled = LexicalIndex::new();
                let start = Instant::now();
                assembled.push_sealed_with_live_rows(black_box(owned), &allowed[0])?;
                let elapsed = start.elapsed().as_nanos() as u64;
                assert_eq!(assembled.document_count(), documents);
                assert_eq!(assembled.total_tokens(), documents * 2);
                if i >= warmups {
                    assembly_ns.push(elapsed);
                }
            }
            // Retrieval and its exact-score control are outside setup timing.
            let control = search_lexical_filtered(
                &index,
                &present,
                10,
                Bm25Params::default(),
                &allowed,
                Some(LexicalBranch::PostCheck),
            )?;
            cases.push(json!({
                "rows": rows, "density": density, "allowed": documents,
                "tokens": documents * 2, "setup_ns": setup_ns,
                "assembly_ns": assembly_ns,
                "control": control.result.hits.iter().map(|hit| json!({
                    "doc": format!("{:?}", hit.doc), "score_bits": hit.score.to_bits(),
                })).collect::<Vec<_>>(),
            }));
        }
    }
    println!(
        "{}",
        json!({
            "fixture": "one sealed segment; identical present-pair documents; sparse stride 97",
            "warmups": warmups, "samples": samples, "k": 10,
            "bitmap_inline_bytes": std::mem::size_of::<DocBitmap>(),
            "lexical_index_inline_bytes": std::mem::size_of::<LexicalIndex>(),
            "cases": cases,
        })
    );
    Ok(())
}
