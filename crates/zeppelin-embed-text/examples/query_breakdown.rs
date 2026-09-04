#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use zeppelin_embed::epoch::{EmbeddingEpoch, Normalization, StoreEpoch};
use zeppelin_embed::fts::tokenizer::{Analyzer, TokenizerConfig};
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SearchOptions, Store};
use zeppelin_embed_text::bundle::Bundle;
use zeppelin_embed_text::runtime::ModelRuntime;
use zeppelin_embed_text::runtime::mlx::MlxRuntime;
use zeppelin_embed_text::tower::TowerRole;
use zeppelin_embed_text::{Legs, QueryOptions, TextStore};

const DOCUMENTS: usize = 171_332;
const K: usize = 10;
const QUERY: &str = "what causes pulmonary hypertension";

fn main() {
    let bundle_path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .expect("pair bundle path");
    let store_path = std::env::args_os()
        .nth(2)
        .map(PathBuf::from)
        .expect("new store path");
    assert!(!store_path.exists(), "measurement store already exists");

    let bundle = Arc::new(Bundle::open(&bundle_path).expect("open pair bundle"));
    let tokens = bundle.tokenize_query(QUERY).expect("tokenize query");
    let mut runtime =
        MlxRuntime::load(Arc::clone(&bundle), TowerRole::Query).expect("load query runtime");
    let mut query_vector = runtime
        .embed_batch(&tokens)
        .expect("embed query")
        .into_values();
    normalize(
        &mut query_vector,
        bundle.query_tower().embedding.normalization,
    );
    drop(runtime);

    let tokenizer = TokenizerConfig::text_default();
    let analyzer = Analyzer::new(tokenizer.clone()).expect("create analyzer");
    let document = bundle.document_tower().embedding.clone();
    let epoch = StoreEpoch {
        embedding: EmbeddingEpoch {
            document: document.clone(),
            query: document,
            alignment_digest: Vec::new(),
        },
        tokenizer: analyzer.epoch(),
    };
    let store = Store::open(
        &store_path,
        OpenOptions::default()
            .with_epoch(epoch.clone())
            .with_tokenizer(tokenizer.clone()),
    )
    .expect("create measurement store");
    let dims = query_vector.len();
    let mut other = vec![0.0_f32; dims];
    other[0] = 1.0;
    for batch_start in (0..DOCUMENTS).step_by(512) {
        let batch_end = batch_start.saturating_add(512).min(DOCUMENTS);
        let documents = (batch_start..batch_end)
            .map(|row| {
                let vector = if row >= DOCUMENTS - K {
                    query_vector.clone()
                } else {
                    other.clone()
                };
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(((row + 1) as u128) << 32), Revision::new(1)),
                    vector,
                )
                .with_text("synthetic stored-text accessor row")
            })
            .collect();
        store
            .ingest(IngestBatch::new(documents).with_epoch(epoch.identity()))
            .expect("ingest measurement rows");
    }
    store.seal().expect("seal measurement rows");
    store.close().expect("close measurement writer");
    drop(store);

    let text_store = TextStore::open(&store_path, &bundle_path, Default::default())
        .expect("open text measurement store");
    let full_ms = measure(|| {
        let hits = text_store
            .query_text(QUERY, QueryOptions::new(K).with_legs(Legs::Dense))
            .expect("full query_text");
        assert_eq!(hits.len(), K);
    });
    text_store.close().expect("close text measurement store");
    drop(text_store);

    let tokenize_ms = measure(|| {
        std::hint::black_box(bundle.tokenize_query(QUERY).expect("tokenize query"));
    });
    let tokens = bundle.tokenize_query(QUERY).expect("tokenize query");
    let mut runtime =
        MlxRuntime::load(Arc::clone(&bundle), TowerRole::Query).expect("load query runtime");
    let embed_ms = measure(|| {
        std::hint::black_box(runtime.embed_batch(&tokens).expect("embed query"));
    });
    let core = Store::open(
        &store_path,
        OpenOptions::read_only()
            .with_epoch(epoch)
            .with_tokenizer(tokenizer),
    )
    .expect("open core measurement store");
    let search_once = || {
        core.search(
            SearchRequest::new(&query_vector),
            K,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search core store")
    };
    let versions = search_once()
        .candidates
        .iter()
        .map(|candidate| candidate.document().expect("candidate identity"))
        .collect::<Vec<_>>();
    assert_eq!(versions.len(), K);
    let search_ms = measure(|| {
        std::hint::black_box(search_once());
    });
    let stored_text_ms = measure(|| {
        for version in &versions {
            std::hint::black_box(
                core.stored_text(*version)
                    .expect("stored-text read")
                    .expect("stored text"),
            );
        }
    });
    let without_stored_text_ms = measure(|| {
        direct_query(&bundle, &mut runtime, &core, false);
    });
    let with_stored_text_ms = measure(|| {
        direct_query(&bundle, &mut runtime, &core, true);
    });
    drop(runtime);
    core.close().expect("close core measurement store");

    println!(
        "{{\"documents\":{DOCUMENTS},\"k\":{K},\"tokenize_ms\":{tokenize_ms:.9},\"embed_ms\":{embed_ms:.9},\"search_ms\":{search_ms:.9},\"stored_text_ms\":{stored_text_ms:.9},\"query_text_ms\":{full_ms:.9},\"direct_without_stored_text_ms\":{without_stored_text_ms:.9},\"direct_with_stored_text_ms\":{with_stored_text_ms:.9}}}",
    );
}

fn direct_query(bundle: &Bundle, runtime: &mut MlxRuntime, store: &Store, include_text: bool) {
    let tokens = bundle.tokenize_query(QUERY).expect("tokenize direct query");
    let mut vector = runtime
        .embed_batch(&tokens)
        .expect("embed direct query")
        .into_values();
    normalize(&mut vector, bundle.query_tower().embedding.normalization);
    let outcome = store
        .search(
            SearchRequest::new(&vector),
            K,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("direct search");
    assert_eq!(outcome.candidates.len(), K);
    if include_text {
        for candidate in outcome.candidates {
            let version = candidate.document().expect("candidate identity");
            std::hint::black_box(
                store
                    .stored_text(version)
                    .expect("direct stored-text read")
                    .expect("direct stored text"),
            );
        }
    }
}

fn normalize(values: &mut [f32], normalization: Normalization) {
    if normalization != Normalization::L2 {
        return;
    }
    let norm = values
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    for value in values {
        *value = (f64::from(*value) / norm) as f32;
    }
}

fn measure(mut operation: impl FnMut()) -> f64 {
    let mut latencies = Vec::with_capacity(200);
    for iteration in 0..220 {
        let started = Instant::now();
        operation();
        if iteration >= 20 {
            latencies.push(started.elapsed().as_secs_f64() * 1_000.0);
        }
    }
    latencies.sort_by(f64::total_cmp);
    latencies[latencies.len() / 2]
}
