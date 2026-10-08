#![allow(clippy::indexing_slicing)]

use serde_json::Value;
use std::error::Error;
use std::process::Command;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn smoke(unified: bool) -> Result<Value> {
    let directory = tempfile::tempdir()?;
    let script = r#"
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { documentBatches, SPEC } from './bindings/node/bench/client-parity-generator.mjs';
import { perfQueries } from './bindings/node/bench/unified-perf.mjs';
import { createHash } from 'node:crypto';
const directory = process.argv[1];
const rows = documentBatches().next().value.slice(0, 300);
const stringify = v => JSON.stringify(v, (_, x) => typeof x === 'bigint' ? String(x) : x instanceof Float32Array ? Array.from(x) : x);
function artifact(name, value) { const path = join(directory, name); writeFileSync(path, value);
return {path, sha256: createHash('sha256').update(value).digest('hex')}; }
const queries = perfQueries();
writeFileSync(join(directory, 'workload.json'), JSON.stringify({schema:'zeppelin-unified-perf-v1',
seed:386, dimensions:8, attributes:SPEC.attributes, rowCount:300, segmentBoundaries:[300],
acceptance:false, epoch:{modelId:'zeppelin.vector-space',modelVersion:'1',normalization:'none',runtime:'cpuReference',
computeUnits:'cpu',maxTokens:0,weightsDigest:[],alignmentDigest:[]},
corpus:artifact('rows.jsonl', rows.map(stringify).join('\n')+'\n'),
queryFile:artifact('queries.json', JSON.stringify(queries)), queries, beir:[]}));
"#;
    let fixture = Command::new("node")
        .current_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .args(["--input-type=module", "-e", script])
        .arg(directory.path())
        .output()?;
    assert!(
        fixture.status.success(),
        "{}",
        String::from_utf8_lossy(&fixture.stderr)
    );
    let path = directory.path().join("counters.json");
    let mut command = Command::new(env!("CARGO_BIN_EXE_unified-perf-counters"));
    command.arg("--smoke");
    if unified {
        command.arg("--unified");
    }
    let output = command
        .arg("--manifest")
        .arg(directory.path().join("workload.json"))
        .arg("--out")
        .arg(&path)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

#[test]
fn counter_smoke_is_explicitly_non_acceptance_and_has_deterministic_work() -> Result<()> {
    let receipt = smoke(false)?;
    assert_eq!(receipt["acceptance"], false);
    assert_eq!(receipt["rowCount"], 300);
    assert_eq!(receipt["unified"], false);
    let queries = receipt["queries"].as_array().ok_or("missing queries")?;
    assert_eq!(queries.len(), 104);
    for query in queries {
        let samples = query["samples"].as_array().ok_or("missing samples")?;
        assert_eq!(samples.len(), 5);
        assert!(samples.iter().all(|s| s == &samples[0]));
        assert!(samples[0]["lexical"]["docs_evaluated"].is_u64());
        assert!(samples[0]["scan"]["dims_touched"].is_u64());
        assert!(samples[0].get("elapsed").is_none());
    }
    Ok(())
}

#[cfg(feature = "graph-cypher")]
#[test]
fn unified_store_counter_smoke_requires_real_graph_and_matching_hits() -> Result<()> {
    let receipt = smoke(true)?;
    assert_eq!(receipt["acceptance"], false);
    assert_eq!(receipt["unified"], true);
    assert_eq!(receipt["graph"]["documents"], 300);
    assert_eq!(receipt["graph"]["relationships"], 1);
    assert_eq!(receipt["queries"].as_array().ok_or("queries")?.len(), 106);
    for name in ["lexical-eligible", "hybrid-eligible"] {
        let q = receipt["queries"]
            .as_array()
            .ok_or("queries")?
            .iter()
            .find(|q| q["name"] == name)
            .ok_or("eligible query missing")?;
        assert_eq!(q["hits"], receipt["preGraphEligibleHits"][name]);
        assert!(!q["hits"].as_array().ok_or("hits")?.is_empty());
        for hit in q["hits"].as_array().ok_or("hits")? {
            let id: u64 = hit["id"].as_str().ok_or("id")?.parse()?;
            assert!((1..=100).contains(&id));
        }
        assert!(q["samples"][0]["lexical"]["postings_decoded"].is_u64());
    }
    assert!(
        receipt["eligibilityAccounting"]
            .as_str()
            .ok_or("accounting")?
            .contains("omit")
    );
    Ok(())
}

#[cfg(not(feature = "graph-cypher"))]
#[test]
fn unified_mode_without_graph_feature_is_refused() -> Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_unified-perf-counters"))
        .args(["--unified", "--smoke"])
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires graph-cypher"));
    Ok(())
}
