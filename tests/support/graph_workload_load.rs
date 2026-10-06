//! Fixed-count shared-store workload. No sleep-based workload control.
use super::workload::*;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::{Arc, Barrier, Mutex};
use std::time::Instant;
use zeppelin_embed_bench::{
    graph_fixture::{self, FixtureState},
    harness_json::{Value, json},
};

pub fn load_requests(directory: &Path, name: &str) -> Result<Vec<Value>, String> {
    let mut rows = Vec::new();
    for case in 0..100 {
        let path = directory.join(format!("{case}-{name}.json"));
        let row: Value = zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(path).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        decode_request(&row["request"])?;
        rows.push(row);
    }
    Ok(rows)
}
pub fn scheduled_read(
    store: &zeppelin_embed::property_graph::GraphStore,
    job: &Value,
    frontend: &str,
    exact: bool,
    worker: usize,
    index: usize,
    warmup: bool,
) -> Result<Value, String> {
    let request = decode_request(&job["request"])?;
    let (result, elapsed) = timed_request(store, &request, frontend, exact)?;
    match result {
        Ok(result) => {
            let generation = result.metadata().generation.get();
            let counters = observed_counters(store, &result)?;
            let rows = observe(&result)?;
            let report = result
                .pools()
                .reports
                .iter()
                .map(|r| format!("{r:?}"))
                .collect::<Vec<_>>();
            let dispose = Instant::now();
            drop(result);
            Ok(
                json!({"participant":worker,"sample":index,"case":job["case"],"cohort":job["cohort"],"name":job["name"],"warmup":warmup,"elapsed_ns":elapsed,"disposal_ns":dispose.elapsed().as_nanos(),"generation":generation,"status":0,"rows":rows.iter().map(|r|r.iter().map(encode_cell).collect::<Vec<_>>()).collect::<Vec<_>>(),"counters":counters,"reports_raw":report}),
            )
        }
        Err(error) => Ok(
            json!({"participant":worker,"sample":index,"case":job["case"],"cohort":job["cohort"],"warmup":warmup,"elapsed_ns":elapsed,"status":1,"error":error}),
        ),
    }
}
pub fn read_schedule(
    store_path: &Path,
    directory: &Path,
    name: &str,
    frontend: &str,
    exact: bool,
    warmups: usize,
    samples: usize,
) -> Result<(), String> {
    if samples == 0 || samples > 10000 || warmups > 1000 {
        return Err("invalid counts".into());
    }
    let requests = load_requests(directory, name)?;
    let store = open(store_path)?;
    for i in 0..warmups + samples {
        let row = scheduled_read(
            &store,
            &requests[i % 100],
            frontend,
            exact,
            0,
            i.saturating_sub(warmups),
            i < warmups,
        )?;
        println!("{row}");
        if row["status"] != 0 {
            store.close().map_err(|e| e.to_string())?;
            return Err("failed public request retained; not taint".into());
        }
    }
    store.close().map_err(|e| e.to_string())
}
/// One recipe meeting: 27 nodes / 110 relationships / 20 supplied vectors.
/// Shared entities keep observed identities; all newly created keys are unique.
pub fn meeting_template(root: &Path) -> Result<Value, String> {
    let mut found = None;
    graph_fixture::visit_batches(root, FixtureState::A, &mut |row| {
        if found.is_none() && row["changes"].as_array().is_some_and(|v| v.len() == 137) {
            found = Some(row);
        }
        Ok(())
    })?;
    let row = found.ok_or("missing full meeting envelope")?;
    let changes = row["changes"].as_array().ok_or("changes")?;
    let nodes = changes
        .iter()
        .filter(|c| c["image"]["kind"] == "node")
        .count();
    let vectors = changes
        .iter()
        .filter(|c| c["image"]["vector"].is_object())
        .count();
    if nodes != 27 || changes.len() - nodes != 110 || vectors != 20 {
        return Err("meeting template is not 27 nodes/110 relationships/20 vectors".into());
    }
    Ok(row)
}
pub fn observed_ids(receipts: &Path) -> Result<Ids, String> {
    let mut ids = Ids::new();
    for line in BufReader::new(std::fs::File::open(receipts).map_err(|e| e.to_string())?).lines() {
        let row: Value =
            zeppelin_embed_bench::harness_json::from_str(&line.map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        if let Some(rows) = row["receipts"].as_array() {
            for r in rows {
                let kind = r["kind"].as_str().ok_or("kind")?;
                let namespace = r["key"]["namespace"].as_str().ok_or("namespace")?;
                let key = r["key"]["key"].as_str().ok_or("key")?;
                let id = r["id"]
                    .as_str()
                    .ok_or("full ID")?
                    .parse::<u128>()
                    .map_err(|e| e.to_string())?;
                ids.insert((kind.into(), namespace.into(), key.into()), id);
            }
        }
    }
    Ok(ids)
}
pub fn meeting(template: &Value, index: usize) -> Result<Value, String> {
    let mut row = template.clone();
    let nodes = template["changes"]
        .as_array()
        .ok_or("changes")?
        .iter()
        .filter(|c| c["image"]["kind"] == "node")
        .map(|c| c["image"]["key"].clone())
        .collect::<Vec<_>>();
    for change in row["changes"].as_array_mut().ok_or("changes")? {
        let image = &mut change["image"];
        let rename = |key: &mut Value| -> Result<(), String> {
            let value = key["key"].as_str().ok_or("key")?;
            key["key"] = json!(format!("ze77-import-{index}-{value}"));
            Ok(())
        };
        rename(&mut image["key"])?;
        if image["kind"] == "relationship" {
            for endpoint in ["source", "target"] {
                if nodes.contains(&image[endpoint]) {
                    rename(&mut image[endpoint])?;
                }
            }
        }
    }
    row["batch"] = json!(index);
    Ok(row)
}
/// Baseline lookup table belongs to the caller/harness, outside managed accounting.
/// Import journal is retained for offline state-at-admission correctness checking.
pub fn import_schedule(
    store_path: &Path,
    root: &Path,
    receipts: &Path,
    count: usize,
) -> Result<(), String> {
    let store = open(store_path)?;
    let template = meeting_template(root)?;
    let mut ids = observed_ids(receipts)?;
    for i in 0..count {
        let input = meeting(&template, i)?;
        let start = Instant::now();
        let result = apply_record(&store, root, &input, &mut ids);
        let elapsed = start.elapsed().as_nanos();
        match result {
            Ok(receipt) => println!(
                "{}",
                json!({"sample":i,"elapsed_ns":receipt["request_ns"],"harness_elapsed_ns":elapsed,"status":0,"input":input,"receipt":receipt,"counters":receipt["counters"],"ledger_before":receipt["ledger_before"],"ledger_after":receipt["ledger_after"],"disposal_ns":receipt["disposal_ns"]})
            ),
            Err(error) => {
                println!(
                    "{}",
                    json!({"sample":i,"elapsed_ns":elapsed,"status":1,"error":error})
                );
                return Err("durable import failed; retained raw outcome".into());
            }
        }
    }
    store.close().map_err(|e| e.to_string())
}
pub fn mixed_load(
    store_path: &Path,
    root: &Path,
    receipts: &Path,
    jobs: &Path,
    read_count: usize,
    write_count: usize,
) -> Result<(), String> {
    if read_count != 1000 || write_count != 200 {
        return Err("mixed load requires 1000 requests per reader and 200 writes".into());
    }
    let store = Arc::new(open(store_path)?);
    let barrier = Arc::new(Barrier::new(5));
    let rows = Arc::new(Mutex::new(Vec::new()));
    let template = meeting_template(root)?;
    let mut ids = observed_ids(receipts)?;
    let requests = Arc::new(load_requests(jobs, "alice-project-ranking")?);
    std::thread::scope(|scope| -> Result<(), String> {
        let mut handles = Vec::new();
        for reader in 0..4 {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let rows = Arc::clone(&rows);
            let requests = Arc::clone(&requests);
            handles.push(scope.spawn(move || -> Result<(), String> {
                barrier.wait();
                for i in 0..read_count {
                    let row = scheduled_read(
                        &store,
                        &requests[(i + reader * 17) % 100],
                        "structured",
                        false,
                        reader,
                        i,
                        false,
                    )?;
                    rows.lock()
                        .map_err(|_| "sample collector poisoned")?
                        .push(row);
                }
                Ok(())
            }));
        }
        let writer=scope.spawn(||->Result<(),String>{barrier.wait();for i in 0..write_count{let input=meeting(&template,i)?;let start=Instant::now();let result=apply_record(&store,root,&input,&mut ids);let elapsed=start.elapsed().as_nanos();let row=match result{Ok(receipt)=>json!({"participant":4,"sample":i,"elapsed_ns":receipt["request_ns"],"harness_elapsed_ns":elapsed,"status":0,"input":input,"receipt":receipt,"counters":receipt["counters"],"ledger_before":receipt["ledger_before"],"ledger_after":receipt["ledger_after"],"disposal_ns":receipt["disposal_ns"]}),Err(error)=>json!({"participant":4,"sample":i,"elapsed_ns":elapsed,"status":1,"error":error})};rows.lock().map_err(|_|"sample collector poisoned")?.push(row);}Ok(())});
        for handle in handles {
            handle.join().map_err(|_| "reader panicked")??;
        }
        writer.join().map_err(|_| "writer panicked")??;
        Ok(())
    })?;
    let observations = rows.lock().map_err(|_| "collector poisoned")?;
    let failed = observations.iter().any(|v| v["status"] != 0);
    for row in observations.iter() {
        println!("{row}");
    }
    store.close().map_err(|e| e.to_string())?;
    if observations.len() != read_count * 4 + write_count || failed {
        return Err("mixed load failed; partial/error outcomes retained, not taint".into());
    }
    Ok(())
}
/// Preserve the live corpus while growing durable deletion-key history.
/// No fence TTL, degree refusal, caller retry, or cap widening is introduced.
pub fn retention_churn(
    store_path: &Path,
    history_keys: u64,
    multiplier: u64,
) -> Result<(), String> {
    if ![1, 5, 10].contains(&multiplier) {
        return Err("retention multiplier must be 1/5/10".into());
    }
    let total = history_keys
        .checked_mul(multiplier)
        .ok_or("history overflow")?;
    let store = open(store_path)?;
    let mut ids = Ids::new();
    for start in (0..total).step_by(128) {
        let end = (start + 128).min(total);
        let create = json!({"batch":start,"changes":(start..end).map(|i|json!({"operation":"create","revision":1,"expected":"absent","image":{"kind":"node","key":{"namespace":"ze77-retention","key":i.to_string()},"labels":[],"properties":{},"text":null,"vector":null}})).collect::<Vec<_>>()});
        let began = Instant::now();
        let created = apply_record(&store, Path::new("."), &create, &mut ids)?;
        let create_ns = began.elapsed().as_nanos();
        let delete = json!({"batch":start,"changes":(start..end).map(|i|json!({"kind":"node","operation":"delete","detach":true,"revision":2,"key":{"namespace":"ze77-retention","key":i.to_string()},"image":null})).collect::<Vec<_>>()});
        let began = Instant::now();
        let deleted = apply_record(&store, Path::new("."), &delete, &mut ids)?;
        let delete_ns = began.elapsed().as_nanos();
        println!(
            "{}",
            json!({"history_keys":end,"create_ns":create_ns,"delete_ns":delete_ns,"created":created,"deleted":deleted,"elapsed_ns":create_ns+delete_ns,"disposal_ns":created["disposal_ns"].as_u64().ok_or("create disposal")?+deleted["disposal_ns"].as_u64().ok_or("delete disposal")?,"status":0,"ledger_before":created["ledger_before"],"ledger_after":deleted["ledger_after"],"counters":resource_counters(&store)?,"generation":deleted["receipts"][0]["generation"]})
        );
        ids.clear();
    }
    store.close().map_err(|e| e.to_string())
}
/// Logical DETACH must succeed independent of hub degree; subsequent public
/// edge-facing observations and offline oracle assert endpoint liveness.
pub fn detach(store_path: &Path, namespace: &str, key: &str, id: u128) -> Result<(), String> {
    let store = open(store_path)?;
    let mut ids = Ids::from([(("node".into(), namespace.into(), key.into()), id)]);
    let record = json!({"batch":0,"changes":[{"kind":"node","operation":"delete","detach":true,"revision":2,"key":{"namespace":namespace,"key":key},"image":null}]});
    let start = Instant::now();
    let result = apply_record(&store, Path::new("."), &record, &mut ids);
    let elapsed = start.elapsed().as_nanos();
    println!(
        "{}",
        json!({"elapsed_ns":elapsed,"operation":"logical-detach","outcome":result.as_ref().ok(),"error":result.as_ref().err()})
    );
    result?;
    store.close().map_err(|e| e.to_string())
}
/// One actual process open through first public admission; no OS-cold inference.
pub fn recover(path: &Path, read_only: bool) -> Result<(), String> {
    use zeppelin_embed::lifecycle::OpenOptions;
    use zeppelin_embed::property_graph::GraphStore;
    let start = Instant::now();
    let store = if read_only {
        GraphStore::open_read_only(path, OpenOptions::new(), Some(tower()))
    } else {
        GraphStore::open(path, OpenOptions::new(), Some(tower()))
    }
    .map_err(|e| e.to_string())?;
    let open_ns = start.elapsed().as_nanos();
    let result = cypher(&store, "MATCH (n) RETURN n LIMIT 1")?;
    let total_ns = start.elapsed().as_nanos();
    println!(
        "{}",
        json!({"process_cold":true,"os_cold":false,"read_only":read_only,"open_ns":open_ns,"through_first_admission_ns":total_ns,"generation":result.metadata().generation.get(),"counters":observed_counters(&store, &result)?,"missing_input":"ZE-76 observed 64-envelope / 16MiB checkpoint-tail and creation-serial inventory counters"})
    );
    store.close().map_err(|e| e.to_string())
}
/// Compare observed ANN-selected complete rows with independent exhaustive
/// full-domain scores; recall is against eligible exact top-k, not row count.
pub fn verify(
    root: &Path,
    state: FixtureState,
    receipts: &Path,
    jobs: &Path,
    samples: &Path,
) -> Result<(), String> {
    use std::collections::{BTreeMap, BTreeSet};
    use zeppelin_embed_adversarial_oracle::graph_fixture as oracle;
    let (snapshot, _) = primitive_snapshot(root, state, receipts)?;
    let mut truth_generation = None;
    for line in BufReader::new(std::fs::File::open(receipts).map_err(|e| e.to_string())?).lines() {
        let row: Value =
            zeppelin_embed_bench::harness_json::from_str(&line.map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        if let Some(g) = row["admitted_generation"].as_u64() {
            truth_generation = Some(g);
        }
    }
    let truth_generation = truth_generation
        .ok_or("missing independent public checkpoint/reopen generation receipt")?;
    let mut cache = BTreeMap::<(String, u64), (Vec<oracle::Row>, Vec<oracle::Row>)>::new();
    for line in BufReader::new(std::fs::File::open(samples).map_err(|e| e.to_string())?).lines() {
        let observed: Value =
            zeppelin_embed_bench::harness_json::from_str(&line.map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        if observed["warmup"] == true {
            continue;
        }
        if observed["status"] != 0 {
            return Err("failed public request, never removable taint".into());
        }
        let case = observed["case"]
            .as_u64()
            .or_else(|| observed["schedule_index"].as_u64())
            .ok_or("missing frozen request case")?;
        let name = observed["name"]
            .as_str()
            .ok_or("missing named request")?
            .to_owned();
        let key = (name.clone(), case);
        if !cache.contains_key(&key) {
            let job: Value = zeppelin_embed_bench::harness_json::from_slice(
                &std::fs::read(jobs.join(format!("{case}-{name}.json")))
                    .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            let request = decode_request(&job["request"])?;
            let exact = oracle::query(&snapshot, &request)?;
            let mut full = request.clone();
            let max = snapshot.nodes.len();
            match &mut full {
                oracle::Query::SemanticContext { k, .. }
                | oracle::Query::AliceProjectRanking { k, .. }
                | oracle::Query::LexicalEvidence { k, .. }
                | oracle::Query::HybridProjectEvidence { k, .. } => *k = max,
                _ => {}
            }
            let exhaustive = oracle::query(&snapshot, &full)?;
            cache.insert(key.clone(), (exact, exhaustive));
        }
        let (exact, full) = cache.get(&key).ok_or("truth cache")?;
        let actual = observed["rows"]
            .as_array()
            .ok_or("rows")?
            .iter()
            .map(|r| {
                r.as_array()
                    .ok_or("row")?
                    .iter()
                    .map(decode_cell)
                    .collect::<Result<Vec<_>, String>>()
            })
            .collect::<Result<Vec<_>, String>>()?;
        let searchable = name != "project-evidence" && !name.starts_with("bounded-evidence");
        let ids = |rows: &[oracle::Row]| {
            let mut seen = BTreeSet::new();
            rows.iter()
                .filter_map(|r| match r.first() {
                    Some(oracle::Cell::Node(id)) if seen.insert(*id) => Some(*id),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let truth_ids = if searchable { ids(exact) } else { vec![] };
        let hit_ids = if searchable { ids(&actual) } else { vec![] };
        let selected: BTreeSet<_> = hit_ids.iter().copied().collect();
        let mut expected = if searchable {
            full.iter()
                .filter(
                    |r| matches!(r.first(),Some(oracle::Cell::Node(id)) if selected.contains(id)),
                )
                .cloned()
                .collect::<Vec<_>>()
        } else {
            exact.clone()
        };
        // Sorting was requested in every frozen read. Full oracle order is stable;
        // bags and path-list multiplicity remain exact.
        if name.starts_with("bounded-evidence") {
            expected.sort();
        }
        let compared = oracle::compare_scored_rows(&expected, &actual, 1e-6, 1e-6);
        let recall = if searchable {
            zeppelin_embed_bench::graph_workload::recall_at_k(&truth_ids, &hit_ids, 20)?
        } else {
            1.0
        };
        println!(
            "{}",
            json!({"sample":observed["sample"],"case":case,"name":name,"admitted_generation":observed["generation"],"truth_generation":truth_generation,"expected_rows":expected.iter().map(|r|r.iter().map(encode_cell).collect::<Vec<_>>()).collect::<Vec<_>>(),"observed_rows":observed["rows"],"truth_ids":truth_ids.iter().map(u128::to_string).collect::<Vec<_>>(),"hit_ids":hit_ids.iter().map(u128::to_string).collect::<Vec<_>>(),"recall_at_20":recall,"correct":compared.is_ok(),"error":compared.err()})
        );
    }
    Ok(())
}
pub fn cypher_imports(path: &Path, list: &Path, count: usize) -> Result<(), String> {
    let paths = std::fs::read_to_string(list)
        .map_err(|e| e.to_string())?
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if paths.len() != count || count != 200 {
        return Err("requires 200 distinct metadata statements".into());
    }
    let store = open(path)?;
    for (index, path) in paths.iter().enumerate() {
        let source = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let start = Instant::now();
        let result = cypher(&store, &source);
        let elapsed = start.elapsed().as_nanos();
        match result {
            Ok(r) => {
                if r.metadata().rows != 0 {
                    return Err("metadata import must return zero rows".into());
                }
                let counters = observed_counters(&store, &r)?;
                let generation = r.metadata().generation.get();
                let receipts = r.pools().receipts.iter().enumerate().map(|(item, r)| {
                    let (kind, id) = match r.receipt.entity { zeppelin_embed::property_graph::EntityId::Node(n)=>("node",n.get()), zeppelin_embed::property_graph::EntityId::Relationship(e)=>("relationship",e.get()) };
                    json!({"item":item,"kind":kind,"id":id.to_string(),"revision":r.receipt.revision.get(),"generation":r.receipt.generation.get()})
                }).collect::<Vec<_>>();
                let disposal = Instant::now();
                drop(r);
                println!(
                    "{}",
                    json!({"sample":index,"elapsed_ns":elapsed,"disposal_ns":disposal.elapsed().as_nanos(),"status":0,"generation":generation,"receipts":receipts,"counters":counters})
                );
            }
            Err(error) => {
                println!(
                    "{}",
                    json!({"sample":index,"elapsed_ns":elapsed,"status":1,"error":error})
                );
                return Err("metadata import guard/product failure retained".into());
            }
        }
    }
    store.close().map_err(|e| e.to_string())
}
/// Apply primitive imported images independently, using actual public receipts
/// for allocated identity only. Never fetch product state as expected values.
fn import_truth(
    snapshot: &mut zeppelin_embed_adversarial_oracle::graph_fixture::Snapshot,
    ids: &mut Ids,
    row: &Value,
    root: &Path,
) -> Result<u64, String> {
    use std::collections::BTreeSet;
    use zeppelin_embed_adversarial_oracle::graph_fixture as oracle;
    let changes = row["input"]["changes"]
        .as_array()
        .ok_or("missing import input journal")?;
    let receipts = row["receipt"]["receipts"]
        .as_array()
        .ok_or("missing complete import receipts")?;
    if changes.len() != 137 || receipts.len() != 137 {
        return Err("partial meeting import".into());
    }
    for receipt in receipts {
        let kind = receipt["kind"].as_str().ok_or("receipt kind")?;
        let key = &receipt["key"];
        let id = receipt["id"]
            .as_str()
            .ok_or("full ID")?
            .parse::<u128>()
            .map_err(|e| e.to_string())?;
        let token = (
            kind.into(),
            key["namespace"].as_str().ok_or("namespace")?.into(),
            key["key"].as_str().ok_or("key")?.into(),
        );
        if ids.insert(token, id).is_some() {
            return Err("import reused existing application key".into());
        }
    }
    let mut generation = None;
    for (change, receipt) in changes.iter().zip(receipts) {
        let image = &change["image"];
        let id = receipt["id"]
            .as_str()
            .ok_or("ID")?
            .parse::<u128>()
            .map_err(|e| e.to_string())?;
        let g = receipt["generation"]
            .as_u64()
            .ok_or("committed generation")?;
        if generation.is_some_and(|old| old != g) {
            return Err("one atomic import returned inconsistent generations".into());
        }
        generation = Some(g);
        let properties = image["properties"]
            .as_object()
            .ok_or("properties")?
            .iter()
            .map(|(k, v)| Ok((k.clone(), scalar(v)?)))
            .collect::<Result<_, String>>()?;
        if image["kind"] == "node" {
            if snapshot.nodes.iter().any(|n| n.id == id) {
                return Err("import reused full node ID".into());
            }
            snapshot.nodes.push(oracle::Node {
                id,
                key: None,
                revision: 1,
                generation: g,
                labels: image["labels"]
                    .as_array()
                    .ok_or("labels")?
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned).ok_or("label".into()))
                    .collect::<Result<BTreeSet<_>, String>>()?,
                properties,
                text: image["text"].as_str().map(str::to_owned),
                vector: if image["vector"].is_object() {
                    Some(
                        vector(root, &image["vector"])?
                            .iter()
                            .map(|f| f.to_bits())
                            .collect(),
                    )
                } else {
                    None
                },
            });
        } else {
            let endpoint = |name: &str| -> Result<u128, String> {
                let v = &image[name];
                ids.get(&(
                    "node".into(),
                    v["namespace"].as_str().ok_or("namespace")?.into(),
                    v["key"].as_str().ok_or("key")?.into(),
                ))
                .copied()
                .ok_or("unknown endpoint ID".into())
            };
            snapshot.relationships.push(oracle::Relationship {
                id,
                key: None,
                revision: 1,
                generation: g,
                source: endpoint("source")?,
                target: endpoint("target")?,
                relationship_type: image["type"].as_str().ok_or("type")?.into(),
                properties,
            });
        }
    }
    generation.ok_or("empty import".into())
}
pub fn verify_mixed(
    root: &Path,
    state: FixtureState,
    receipts: &Path,
    jobs: &Path,
    samples: &Path,
) -> Result<(), String> {
    use zeppelin_embed_adversarial_oracle::graph_fixture as oracle;
    let (mut snapshot, mut ids) = primitive_snapshot(root, state, receipts)?;
    let mut baseline = None;
    for line in BufReader::new(std::fs::File::open(receipts).map_err(|e| e.to_string())?).lines() {
        let r: Value =
            zeppelin_embed_bench::harness_json::from_str(&line.map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        if let Some(g) = r["admitted_generation"].as_u64() {
            baseline = Some(g);
        }
    }
    let mut state_start = baseline.ok_or("missing baseline generation receipt")?;
    let rows = BufReader::new(std::fs::File::open(samples).map_err(|e| e.to_string())?)
        .lines()
        .map(|line| {
            zeppelin_embed_bench::harness_json::from_str::<Value>(&line.map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())
        })
        .collect::<Result<Vec<_>, String>>()?;
    if rows.iter().any(|r| r["status"] != 0) {
        return Err("mixed request error is failure, not taint".into());
    }
    let mut writes = rows
        .iter()
        .filter(|r| r["participant"] == 4)
        .collect::<Vec<_>>();
    writes.sort_by_key(|r| r["sample"].as_u64());
    if writes.len() != 200 {
        return Err("missing mixed writes".into());
    }
    let generation = |r: &Value| {
        r["receipt"]["receipts"]
            .as_array()
            .and_then(|rs| rs.first())
            .and_then(|r| r["generation"].as_u64())
            .ok_or("missing committed import generation")
    };
    let mut reads = rows
        .iter()
        .filter(|r| r["participant"] != 4)
        .collect::<Vec<_>>();
    reads.sort_by_key(|r| r["generation"].as_u64());
    if reads.len() != 4000 {
        return Err("missing mixed reads".into());
    }
    let mut next = 0;
    for read in reads {
        let admitted = read["generation"]
            .as_u64()
            .ok_or("missing admission generation")?;
        while let Some(write) = writes.get(next) {
            if generation(write)? > admitted {
                break;
            }
            state_start = import_truth(&mut snapshot, &mut ids, write, root)?;
            next += 1;
        }
        if admitted < state_start {
            return Err("read admitted before declared baseline".into());
        }
        let end = writes.get(next).map(|r| generation(r)).transpose()?;
        let case = read["case"].as_u64().ok_or("case")?;
        let name = read["name"].as_str().ok_or("name")?;
        let job: Value = zeppelin_embed_bench::harness_json::from_slice(
            &std::fs::read(jobs.join(format!("{case}-{name}.json"))).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        let request = decode_request(&job["request"])?;
        let expected = oracle::query(&snapshot, &request)?;
        println!(
            "{}",
            json!({"participant":read["participant"],"sample":read["sample"],"admitted_generation":admitted,"truth_generation_range":{"start":state_start,"end":end},"expected_rows":expected.iter().map(|r|r.iter().map(encode_cell).collect::<Vec<_>>()).collect::<Vec<_>>(),"observed_rows":read["rows"],"truth_ids":[],"hit_ids":[]})
        );
    }
    while let Some(write) = writes.get(next) {
        import_truth(&mut snapshot, &mut ids, write, root)?;
        next += 1;
    }
    Ok(())
}
pub fn recovery_series(list: &Path, read_only: bool) -> Result<(), String> {
    use zeppelin_embed::lifecycle::OpenOptions;
    use zeppelin_embed::property_graph::GraphStore;
    let paths = std::fs::read_to_string(list)
        .map_err(|e| e.to_string())?
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if paths.len() != 20 {
        return Err("bounded-tail recovery requires exactly twenty prepared stores".into());
    }
    for (index, path) in paths.iter().enumerate() {
        let start = Instant::now();
        let store = if read_only {
            GraphStore::open_read_only(path, OpenOptions::new(), Some(tower()))
        } else {
            GraphStore::open(path, OpenOptions::new(), Some(tower()))
        }
        .map_err(|e| e.to_string())?;
        let open_ns = start.elapsed().as_nanos();
        let result = cypher(&store, "MATCH (n) RETURN n LIMIT 1")?;
        let elapsed = start.elapsed().as_nanos();
        let generation = result.metadata().generation.get();
        let counters = observed_counters(&store, &result)?;
        let rows = observe(&result)?;
        let disposal = Instant::now();
        drop(result);
        let disposal = disposal.elapsed().as_nanos();
        println!(
            "{}",
            json!({"sample":index,"elapsed_ns":elapsed,"open_ns":open_ns,"through_first_admission_ns":elapsed,"disposal_ns":disposal,"generation":generation,"counters":counters,"status":0,"rows":rows.iter().map(|r|r.iter().map(encode_cell).collect::<Vec<_>>()).collect::<Vec<_>>(),"first_open_in_process":index==0,"os_cold":false,"read_only":read_only})
        );
        store.close().map_err(|e| e.to_string())?;
    }
    Ok(())
}
/// Untimed complete public entity reads for import/retention correctness.
/// Identity comes from receipts; all expected payload comes from frozen inputs.
pub fn inspect_entities(path: &Path, requests: &Path) -> Result<(), String> {
    use zeppelin_embed::property_graph::query::completed::{
        Pools, Span, Value as Cell, ValueIndex,
    };
    use zeppelin_embed::property_graph::{GraphGetOptions, NodeId, RelId};
    fn text(p: Pools<'_>, s: Span) -> Result<String, String> {
        let bytes = p
            .bytes
            .get(s.start as usize..(s.start + s.len) as usize)
            .ok_or("string range")?;
        String::from_utf8(bytes.to_vec()).map_err(|e| e.to_string())
    }
    fn value(p: Pools<'_>, index: ValueIndex) -> Result<Value, String> {
        Ok(match p.values.get(index.0 as usize).ok_or("value index")? {
            Cell::Null => json!({"null":true}),
            Cell::Bool(b) => json!({"bool":b}),
            Cell::I64(i) => json!({"i64":i}),
            Cell::F64(b) => json!({"f64_bits":format!("{b:016x}")}),
            Cell::String(s) => json!({"string":text(p,*s)?}),
            Cell::List { children, .. } => {
                let items = p
                    .children
                    .get(children.start as usize..(children.start + children.len) as usize)
                    .ok_or("children range")?;
                json!({"list":items.iter().map(|i|value(p,*i)).collect::<Result<Vec<_>,_>>()?})
            }
            _ => return Err("entity property type".into()),
        })
    }
    fn properties(p: Pools<'_>, s: Span) -> Result<Value, String> {
        let mut result = json!({});
        for prop in p
            .properties
            .get(s.start as usize..(s.start + s.len) as usize)
            .ok_or("properties range")?
        {
            result[text(p, prop.name)?] = value(p, prop.value)?;
        }
        Ok(result)
    }
    let store = open(path)?;
    for line in BufReader::new(std::fs::File::open(requests).map_err(|e| e.to_string())?).lines() {
        let request: Value =
            zeppelin_embed_bench::harness_json::from_str(&line.map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        let kind = request["kind"].as_str().ok_or("kind")?;
        let id = request["id"]
            .as_str()
            .ok_or("ID")?
            .parse::<u128>()
            .map_err(|e| e.to_string())?;
        let observed = if kind == "node" {
            let result = store
                .get_nodes(
                    &[NodeId::new(id).map_err(|e| e.to_string())?],
                    GraphGetOptions {
                        text: true,
                        vector: true,
                    },
                    &control(),
                )
                .map_err(|e| e.to_string())?;
            if let Some(n) = result.nodes().first().ok_or("node result")? {
                let p = result.pools();
                json!({"id":id.to_string(),"revision":n.revision.get(),"key":n.key.map(|k|Ok::<Value,String>(json!({"namespace":text(p,k.namespace)?,"key":text(p,k.value)?}))).transpose()?,"labels":result.labels(n).iter().map(|s|text(p,*s)).collect::<Result<Vec<_>,_>>()?,"properties":properties(p,n.properties)?,"text":n.text.map(|s|text(p,s)).transpose()?,"vector_bits":n.vector.map(|s|result.vector(s).to_vec())})
            } else {
                Value::Null
            }
        } else if kind == "relationship" {
            let result = store
                .get_relationships(&[RelId::new(id).map_err(|e| e.to_string())?], &control())
                .map_err(|e| e.to_string())?;
            if let Some(r) = result
                .relationships()
                .first()
                .ok_or("relationship result")?
            {
                json!({"id":id.to_string(),"revision":r.revision.get(),"key":r.key.map(|k|Ok::<Value,String>(json!({"namespace":text(result.pools(),k.namespace)?,"key":text(result.pools(),k.value)?}))).transpose()?,"source":r.source.get().to_string(),"target":r.target.get().to_string(),"type":text(result.pools(),r.relationship_type)?,"properties":properties(result.pools(),r.properties)?})
            } else {
                Value::Null
            }
        } else {
            return Err("unknown entity kind".into());
        };
        if let Some(expected) = request.get("expected")
            && expected != &observed
        {
            return Err(format!("complete baseline payload differs for {kind} {id}"));
        }
        println!("{}", json!({"observed":observed}));
    }
    store.close().map_err(|e| e.to_string())
}

/// Frozen baseline payload, independent of the retained store being checked.
pub fn expected_entities(root: &Path, state: FixtureState, receipts: &Path) -> Result<(), String> {
    use zeppelin_embed_adversarial_oracle::graph_fixture::{Property, Scalar};
    fn scalar(s: &Scalar) -> Value {
        match s {
            Scalar::Bool(v) => json!({"bool":v}),
            Scalar::I64(v) => json!({"i64":v}),
            Scalar::F64(v) => json!({"f64_bits":format!("{v:016x}")}),
            Scalar::String(v) => json!({"string":v}),
        }
    }
    fn properties(p: &std::collections::BTreeMap<String, Property>) -> Value {
        let mut result = json!({});
        for (k, v) in p {
            result[k] = match v {
                Property::Scalar(v) => scalar(v),
                Property::List(_, v) => json!({"list":v.iter().map(scalar).collect::<Vec<_>>()}),
            };
        }
        result
    }
    let (snapshot, _) = primitive_snapshot(root, state, receipts)?;
    for n in snapshot.nodes {
        println!(
            "{}",
            json!({"kind":"node","id":n.id.to_string(),"expected":{"id":n.id.to_string(),"revision":n.revision,"key":n.key.map(|k|json!({"namespace":k.namespace,"key":k.value})),"labels":n.labels.into_iter().collect::<Vec<_>>(),"properties":properties(&n.properties),"text":n.text,"vector_bits":n.vector}})
        );
    }
    for r in snapshot.relationships {
        println!(
            "{}",
            json!({"kind":"relationship","id":r.id.to_string(),"expected":{"id":r.id.to_string(),"revision":r.revision,"key":r.key.map(|k|json!({"namespace":k.namespace,"key":k.value})),"source":r.source.to_string(),"target":r.target.to_string(),"type":r.relationship_type,"properties":properties(&r.properties)}})
        );
    }
    Ok(())
}
