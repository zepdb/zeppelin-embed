//! Read-only ZE-417 timing and work-counter driver for the prepared 150k store.
use std::time::Instant;
use zeppelin_embed::epoch::*;
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::meta::{ColumnDefinition, ColumnId, ColumnType, Schema};
use zeppelin_embed::property_graph::query::completed::GraphQueryOptions;
use zeppelin_embed::property_graph::query::runtime::WorkKind;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("prepared store path required")?;
    let head = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()?;
    if !head.status.success() {
        return Err("cannot read HEAD".into());
    }
    println!(
        "HEAD {} (worktree code)",
        String::from_utf8(head.stdout)?.trim()
    );
    let columns = ["note", "folder", "speaker", "stream", "startMs", "endMs"]
        .iter()
        .enumerate()
        .map(|(i, name)| {
            ColumnDefinition::new(ColumnId::new(i as u32 + 1), *name, ColumnType::U64, false)
        })
        .collect();
    let tower = EmbeddingTower {
        model_id: "zeppelin.vector-space".into(),
        model_version: "1".into(),
        weights_digest: vec![],
        dims: 8,
        normalization: Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 0,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    let epoch = StoreEpoch {
        embedding: EmbeddingEpoch {
            query: tower.clone(),
            document: tower,
            alignment_digest: vec![],
        },
        tokenizer: zeppelin_embed::fts::tokenizer::TokenizerConfig::text_default().epoch(),
    };
    let store = Store::open(
        path,
        OpenOptions::read_only()
            .with_schema(Schema::new(columns)?)
            .with_epoch(epoch),
    )?;
    let options = GraphQueryOptions::default().with_result_row_limit(65_536)?;
    let queries = [
        "MATCH (n) RETURN count(n)",
        "MATCH (n:Document) RETURN count(n)",
        "MATCH ()-[r]->() RETURN count(r)",
        "MATCH ()-[r:PERF_LINK]->() RETURN count(r)",
        "MATCH (n) WHERE ze.node_id(n) = '00000000000000000000000000000001' RETURN n",
        "MATCH (a)-[r:PERF_LINK]->(b) RETURN a,b LIMIT 10",
        "MATCH (a)-[r:PERF_LINK]->(b) RETURN a,b",
        "MATCH (a)-[r:PERF_LINK]->(b)-[s:PERF_LINK]->(c) RETURN count(c)",
        "MATCH (n) WHERE n.folder = 3 RETURN count(n)",
        "MATCH (a)<-[r:PERF_LINK]-(b) RETURN count(r)",
        "MATCH (a)-[r:PERF_LINK]-(b) RETURN count(r)",
        "MATCH (a:Document)-[r:PERF_LINK]->(b) RETURN count(r)",
        "MATCH (a)-[r]->(b) WHERE ze.node_id(a) = '00000000000000000000000000000001' RETURN a,r,b",
    ];
    for query in queries {
        for sample in 0..3 {
            let start = Instant::now();
            let result = zeppelin_embed_cypher::execute(
                &store,
                &QueryControl::Cancel(CancelToken::new()),
                &options,
                query,
                &[],
                Default::default(),
            )?;
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            let metadata = result.metadata();
            println!(
                "{elapsed:.3} ms sample={sample} rows={} first={:?} lookups={} scans={} pages={} peak={} {query}",
                metadata.rows,
                result.cell(0, 0),
                metadata.counters.get(WorkKind::Lookups),
                metadata.counters.get(WorkKind::Scans),
                metadata.counters.get(WorkKind::DirectoryPagesDecoded),
                metadata.peak_query_bytes
            );
        }
    }
    Ok(())
}
