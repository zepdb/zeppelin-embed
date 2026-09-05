//! Dense API stage attribution and direct CoreML controls. Outputs must be fresh.
use serde_json::{Value, json};
use std::{error::Error, fs, path::PathBuf, time::Instant};
use zeppelin_embed_text::{Legs, QueryOptions, SearchTier, TextStore, bundle::Bundle};

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    let mode = args.get(1).ok_or("query|tower")?;
    let bundle_path = PathBuf::from(args.get(2).ok_or("bundle")?);
    let fixture: Value = serde_json::from_slice(&fs::read(args.get(3).ok_or("fixture")?)?)?;
    let output = PathBuf::from(args.get(4).ok_or("fresh output directory")?);
    fs::create_dir(&output)?;
    let queries = fixture["queries"].as_array().ok_or("queries")?;
    if mode == "tower" {
        use zeppelin_embed_text::runtime::{
            ModelRuntime,
            coreml::{ComputeUnits, CoreMlRuntime},
        };
        let bundle = Bundle::open(&bundle_path)?;
        let model = PathBuf::from(args.get(5).ok_or("model")?);
        let sequence: usize = args.get(6).ok_or("sequence")?.parse()?;
        let units = match args.get(7).map(String::as_str) {
            Some("cpu") => ComputeUnits::Cpu,
            Some("ane") => ComputeUnits::CpuAndNeuralEngine,
            _ => return Err("cpu|ane".into()),
        };
        let max_tokens = std::env::var("ZE_BUDGET_QUERY_MAX_TOKENS")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()?
            .unwrap_or(sequence);
        let mut batches = Vec::new();
        for query in queries {
            let tokens = bundle.tokenize_query(query["text"].as_str().ok_or("text")?)?;
            if tokens.tokens_per_row <= max_tokens && tokens.tokens_per_row <= sequence {
                batches.push((query["id"].clone(), tokens.padded_to(sequence)?));
            }
        }
        if batches.is_empty() {
            return Err("no fitting queries".into());
        }
        let mut runtime = CoreMlRuntime::load(
            &model,
            sequence,
            bundle.query_tower().embedding.dims as usize,
            units,
        )?;
        for (_, tokens) in batches.iter().cycle().take(20) {
            runtime.embed_batch(tokens)?;
        }
        let mut samples = Vec::new();
        for (id, tokens) in &batches {
            let started = Instant::now();
            let embedding = runtime.embed_batch(tokens)?;
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            if embedding.values().iter().any(|x| !x.is_finite()) {
                return Err("nonfinite embedding".into());
            }
            samples.push(json!({"query":id,"ms":ms,"embedding":embedding.values(),
                "bits":embedding.values().iter().map(|x|x.to_bits()).collect::<Vec<_>>() }));
        }
        fs::write(
            output.join("results.json"),
            serde_json::to_vec(&json!({
                "mode":mode,"sequence":sequence,"model":model,"units":format!("{units:?}"),
                "warmups":20,"max_tokens":max_tokens,"samples":samples,
            }))?,
        )?;
        return Ok(());
    }
    if mode != "query" {
        return Err("query|tower".into());
    }
    let store_path = PathBuf::from(args.get(5).ok_or("store")?);
    let api = args.get(6).map(String::as_str).unwrap_or("dense");
    let (legs, tier) = match api {
        "dense" => (Legs::Dense, None),
        "exact" => (Legs::Dense, Some(SearchTier::Exact)),
        "lexical" => (Legs::Lexical, None),
        "hybrid" => (Legs::Hybrid, None),
        _ => return Err("dense|exact|lexical|hybrid".into()),
    };
    let store = TextStore::open(&store_path, &bundle_path, Default::default())?;
    let health = store.health()?;
    if health.segments.iter().map(|s| s.rows).sum::<u64>()
        != fixture["expected_chunks"]
            .as_u64()
            .ok_or("expected chunks")?
        || health.segments.iter().any(|s| s.tombstones != 0)
    {
        return Err(format!("corpus geometry mismatch: {health:?}").into());
    }
    let expected_graph: f64 = std::env::var("ZE_BUDGET_GRAPH_COVERAGE")
        .unwrap_or_else(|_| "0".to_owned())
        .parse()?;
    if health.graph_coverage != expected_graph {
        return Err(format!("unexpected graph coverage: {health:?}").into());
    }
    let options = QueryOptions::new(10)
        .with_legs(legs)
        .with_optional_tier(tier);
    for query in queries.iter().cycle().take(20) {
        store.query_text(query["text"].as_str().ok_or("text")?, options)?;
    }
    let mut samples = Vec::new();
    for query in queries {
        let started = Instant::now();
        let answer =
            store.query_text_with_diagnostics(query["text"].as_str().ok_or("text")?, options)?;
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let diag = answer.diagnostics.as_ref().ok_or("missing diagnostics")?;
        if diag.budget_exhausted || answer.embedding_calls != usize::from(legs != Legs::Lexical) {
            return Err("query incomplete or wrong embedding calls".into());
        }
        let stages = answer.timings.map(|t| {
            let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
            let sum = t.tokenization + t.lexical_analysis + t.embedding_queue
                + t.embedding_evaluation + t.embedding_normalization + t.retrieval
                + t.materialization;
            json!({"tokenization":ms(t.tokenization),"lexical_analysis":ms(t.lexical_analysis),
                "embedding_queue":ms(t.embedding_queue),"embedding_evaluation":ms(t.embedding_evaluation),
                "embedding_normalization":ms(t.embedding_normalization),"retrieval":ms(t.retrieval),
                "materialization":ms(t.materialization),"end_to_end":ms(t.end_to_end),
                "unattributed":ms(t.end_to_end.saturating_sub(sum)),"stage_sum":ms(sum)})
        });
        samples.push(json!({"query":query["id"],"ms":ms,"stages_ms":stages,
            "query_tokens":answer.query_tokens,"embedding_calls":answer.embedding_calls,
            "hits":answer.hits.iter().map(|h|json!({"id":h.doc_id.to_string(),"revision":h.revision,
                "score":h.score,"score_bits":h.score.to_bits(),"chunk":h.chunk,"text_bytes":h.text.len()})).collect::<Vec<_>>(),
            "plan":format!("{:?}",diag.plan),"counters":format!("{:?}",diag.counters),
            "core_stages":format!("{:?}",diag.timings),"exact_rescore":diag.exact_rescore,
            "approximate":diag.approximate }));
    }
    let result = json!({"api":api,"warmups":20,"bundle":bundle_path,"store":store_path,
        "backend":format!("{:?}",store.query_backend()),"epoch":format!("{:?}",store.epoch()),
        "health":format!("{health:?}"),"samples":samples });
    store.close()?;
    fs::write(output.join("results.json"), serde_json::to_vec(&result)?)?;
    Ok(())
}
