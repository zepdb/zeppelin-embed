//! Directed graph retrieval checks against the Store's document segments.
use super::coverage::CoverageRegistry;
use zeppelin_embed::epoch::*;
use zeppelin_embed::ingest::*;
use zeppelin_embed::lifecycle::*;
use zeppelin_embed::property_graph::query::completed::{
    CompletedGraphResult, GraphQueryOptions, Value,
};
use zeppelin_embed::property_graph::query::runtime::{RuntimeLimits, WorkKind};
use zeppelin_embed_cypher::{CompileLimits, execute};

pub const REGISTERED_COVERAGE: &[&str] = &[
    "property-graph.native-segment-search.exact",
    "property-graph.native-segment-search.eligible",
    "property-graph.native-segment-search.identity",
    "property-graph.native-segment-search.limit.fire",
    "property-graph.native-segment-search.control.fire",
    "property-graph.native-segment-search.reopen",
    "property-graph.native-segment-search.sealed",
    "property-graph.native-segment-search.oracle.can-fire",
];
#[derive(Debug)]
pub struct ProbeReport {
    pub comparisons: usize,
    pub refusals: usize,
}
fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}
fn observe(result: &CompletedGraphResult) -> Vec<(u128, u64)> {
    (0..result.metadata().rows as usize)
        .map(|row| {
            let Some(Value::Node(index)) = result.cell(row, 0) else {
                panic!("node")
            };
            let Some(Value::F64(score)) = result.cell(row, 1) else {
                panic!("score")
            };
            (result.pools().nodes[*index as usize].id.get(), *score)
        })
        .collect()
}
fn run(store: &Store, source: &str) -> Result<Vec<(u128, u64)>, String> {
    execute(
        store,
        &control(),
        &GraphQueryOptions::default(),
        source,
        &[],
        CompileLimits::default(),
    )
    .map(|result| observe(&result))
    .map_err(|error| error.to_string())
}
pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<ProbeReport, String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let tower = EmbeddingTower {
        model_id: "ze366".into(),
        model_version: "1".into(),
        weights_digest: seed.to_le_bytes().to_vec(),
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 32,
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
    let options = OpenOptions::new()
        .with_epoch(epoch)
        .with_max_resident_bytes(128 * 1024 * 1024);
    let store =
        Store::open(directory.path(), options.clone()).map_err(|error| error.to_string())?;
    let high = (1_u128 << 100) + 7;
    store
        .ingest(
            IngestBatch::new(vec![
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(high), Revision::new(1)),
                    vec![0.0, 0.0],
                )
                .with_text("amber")
                .with_timestamp(1),
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(7), Revision::new(1)),
                    vec![2.0, 0.0],
                )
                .with_text("amber birch"),
            ])
            .with_epoch(store.epoch_identity().unwrap()),
        )
        .map_err(|error| error.to_string())?;
    store.enable_graph().map_err(|error| error.to_string())?;
    let source = "CALL ze.vector_search([0,0],2,'exact') YIELD node,distance RETURN node,distance";
    let expected = store
        .search(
            SearchRequest::new(&[0.0, 0.0]),
            2,
            SearchOptions::default().with_tier(SearchTier::Exact),
            control(),
        )
        .map_err(|error| error.to_string())?
        .candidates
        .iter()
        .map(|hit| {
            (
                hit.document().unwrap().doc_id().get(),
                (-(hit.score() as f64)).to_bits(),
            )
        })
        .collect::<Vec<_>>();
    let actual = run(&store, source)?;
    if actual != expected {
        return Err("Store/Cypher exact ranking mismatch".into());
    }
    coverage.hit(REGISTERED_COVERAGE[0]);
    let eligible = run(
        &store,
        "MATCH (d:Document) WHERE d.ts = 1 WITH collect(DISTINCT d) AS e CALL ze.hybrid_search([0,0],'amber',2,'exact',e) YIELD node,score RETURN node,score",
    )?;
    if eligible.len() != 1 || eligible[0].0 != high {
        return Err("hybrid restriction lost".into());
    }
    coverage.hit(REGISTERED_COVERAGE[1]);
    if actual.iter().map(|row| row.0).collect::<Vec<_>>() != [high, 7] {
        return Err("full-width identity lost".into());
    }
    coverage.hit(REGISTERED_COVERAGE[2]);
    let limited = GraphQueryOptions::default()
        .with_limits(
            24 * 1024 * 1024,
            RuntimeLimits::default()
                .with_limit(WorkKind::VectorCoordinates, 0)
                .unwrap(),
        )
        .unwrap();
    let error = execute(
        &store,
        &control(),
        &limited,
        source,
        &[],
        CompileLimits::default(),
    )
    .err()
    .ok_or("zero coordinate limit accepted")?;
    if !error.to_string().contains("VectorCoordinates") {
        return Err(error.to_string());
    }
    coverage.hit(REGISTERED_COVERAGE[3]);
    let cancelled = CancelToken::new();
    cancelled.cancel();
    if execute(
        &store,
        &QueryControl::Cancel(cancelled),
        &GraphQueryOptions::default(),
        source,
        &[],
        CompileLimits::default(),
    )
    .is_ok()
    {
        return Err("cancel accepted".into());
    }
    coverage.hit(REGISTERED_COVERAGE[4]);
    store.seal().map_err(|error| error.to_string())?;
    if run(&store, source)? != expected {
        return Err("sealed ranking changed".into());
    }
    coverage.hit(REGISTERED_COVERAGE[6]);
    store.close().map_err(|error| error.to_string())?;
    let store = Store::open(directory.path(), options).map_err(|error| error.to_string())?;
    if run(&store, source)? != expected {
        return Err("reopened ranking changed".into());
    }
    coverage.hit(REGISTERED_COVERAGE[5]);
    let mut mutated = expected.clone();
    mutated[0].0 = 7;
    if run(&store, source)? == mutated {
        return Err("identity comparator cannot fire".into());
    }
    coverage.hit(REGISTERED_COVERAGE[7]);
    Ok(ProbeReport {
        comparisons: 5,
        refusals: 2,
    })
}
