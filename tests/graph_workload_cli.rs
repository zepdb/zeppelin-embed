//! Public worker and offline oracle: deliberately separate process modes.
#[path = "support/graph_workload_load.rs"]
mod load;
#[path = "support/graph_workload_native.rs"]
mod native;
#[path = "tooling_seed.rs"]
mod test_support;
#[path = "support/graph_workload.rs"]
mod workload;
use rand::Rng;
use std::io::Write;
use std::path::Path;
use zeppelin_embed_adversarial_oracle::graph_fixture as oracle;
use zeppelin_embed_bench::{
    graph_fixture::{self, Config, FixtureState, Scale, WordStream},
    graph_workload,
    harness_json::{Value, json},
};
fn main() {
    if let Err(error) = run() {
        eprintln!("ZE-77: {error}");
        std::process::exit(1);
    }
}
fn state(s: &str) -> Result<FixtureState, String> {
    match s {
        "A" => Ok(FixtureState::A),
        "B" => Ok(FixtureState::B),
        _ => Err("state must be A or B".into()),
    }
}
fn self_test() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let base = std::env::var_os("ZE77_SMOKE_OUTPUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| dir.path().to_owned());
    std::fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    let root = base.join("fixture");
    let path = base.join("store");
    let config = Config::new(Scale::Small);
    let mut factory = |name: &str| {
        let mut rng = test_support::seeded_rng(name, config.seed);
        Box::new(move || rng.random()) as WordStream
    };
    graph_fixture::write_fixture(&root, config, "ze77-self-test", &mut factory)?;
    let receipts = base.join("receipts.jsonl");
    let mut file = std::fs::File::create(&receipts).map_err(|e| e.to_string())?; // Correctness smoke uses a process-checkpointed small fixture. It does
    // not claim the qualification consolidation schedule (prepare enforces it).
    let store = workload::create(&path)?;
    let mut smoke_ids = workload::Ids::new();
    for st in [FixtureState::A, FixtureState::B] {
        graph_fixture::visit_batches(&root, st, &mut |row| {
            let receipt = workload::apply_record(&store, &root, &row, &mut smoke_ids)?;
            writeln!(file, "{receipt}").map_err(|e| e.to_string())
        })?;
    }
    store
        .close()
        .map_err(|e| format!("smoke checkpoint: {e}"))?;
    drop(store);
    file.flush().map_err(|e| e.to_string())?;
    let (snapshot, ids) = workload::primitive_snapshot(&root, FixtureState::B, &receipts)?;
    if snapshot.nodes.len() != 131 || snapshot.relationships.len() != 440 {
        return Err("small fixture count mismatch".into());
    }
    let cases = workload::query_cases(&root)?;
    let store = workload::open(&path)?;
    let generation = workload::cypher(&store, "MATCH (n) RETURN n LIMIT 0")?
        .metadata()
        .generation
        .get();
    writeln!(
        file,
        "{}",
        json!({"admitted_generation":generation,"checkpoint_reopen":true})
    )
    .map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())?;
    let jobs = base.join("jobs");
    std::fs::create_dir_all(&jobs).map_err(|e| e.to_string())?;
    for name in graph_workload::READS {
        let q = workload::oracle_request(&root, &cases[0], &ids, name, 2)?;
        let expected = oracle::query(&snapshot, &q)?;
        let truth = json!({"request":workload::request_record(&q),"rows":expected.iter().map(|r|r.iter().map(workload::encode_cell).collect::<Vec<_>>()).collect::<Vec<_>>()});
        std::fs::write(jobs.join(format!("{name}.json")), truth.to_string())
            .map_err(|e| e.to_string())?;
        std::fs::write(
            jobs.join(format!("{name}.cypher")),
            workload::build_named_requests(&q, true),
        )
        .map_err(|e| e.to_string())?;
        let mut native_job = std::fs::File::create_new(jobs.join(format!("{name}.structured")))
            .map_err(|e| e.to_string())?;
        workload::with_plan(&q, true, |plan| native::write_plan(plan, &mut native_job))?;
        for frontend in ["structured", "cypher"] {
            let result = if frontend == "structured" {
                workload::structured(&store, &q, true)
                    .map_err(|e| format!("{name}/structured: {e}"))?
            } else {
                workload::cypher(&store, &workload::build_named_requests(&q, true))
                    .map_err(|e| format!("{name}/cypher: {e}"))?
            };
            let actual = workload::observe(&result)?;
            oracle::compare_scored_rows(&expected, &actual, 1e-6, 1e-6)
                .map_err(|e| format!("{name}/{frontend}: {e}"))?;
        }
    }
    store.close().map_err(|e| e.to_string())?;
    println!("ZE-77 small public Rust structured/Cypher fixture/reopen/offline oracle PASS");
    Ok(())
}
fn write_offline_truth(
    root: &Path,
    st: FixtureState,
    receipts: &Path,
    out: &Path,
    smoke: bool,
) -> Result<(), String> {
    let (snapshot, ids) = workload::primitive_snapshot(root, st, receipts)?;
    let mut file = std::fs::File::create_new(out).map_err(|e| e.to_string())?;
    let jobs = out.with_extension("jobs");
    std::fs::create_dir(&jobs).map_err(|e| e.to_string())?;
    for case in workload::query_cases(root)? {
        for name in graph_workload::READS {
            if smoke && name != "project-evidence" {
                continue;
            }
            let hops: Vec<_> = if name == "bounded-evidence" {
                vec![2, 1, 4, 8, 16]
            } else {
                vec![2]
            };
            for hop in hops {
                let q = workload::oracle_request(root, &case, &ids, name, hop)?;
                let label = if name == "bounded-evidence" && hop != 2 {
                    format!("{name}-h{hop}")
                } else {
                    name.to_owned()
                };
                let stem = format!("{}-{label}", case["case"]);
                let normal = match name {
                    "alice-project-ranking" => case["normal_timing"].clone(),
                    "lexical-evidence" => case["lexical"]["normal_timing"].clone(),
                    _ => json!(true),
                };
                let rows = oracle::query(&snapshot, &q)?;
                let job = json!({"name":label,"case":case["case"],"cohort":case["cohort"],"lexical_cohort":case["lexical"]["cohort"],"normal_timing":normal,"request":workload::request_record(&q)});
                std::fs::write(jobs.join(format!("{stem}.json")), job.to_string())
                    .map_err(|e| e.to_string())?;
                for exact in [false, true] {
                    let suffix = if exact { ".exact" } else { "" };
                    std::fs::write(
                        jobs.join(format!("{stem}{suffix}.cypher")),
                        workload::build_named_requests(&q, exact),
                    )
                    .map_err(|e| e.to_string())?;
                    let mut native_job =
                        std::fs::File::create_new(jobs.join(format!("{stem}{suffix}.structured")))
                            .map_err(|e| e.to_string())?;
                    workload::with_plan(&q, exact, |plan| {
                        native::write_plan(plan, &mut native_job)
                    })?;
                }
                if name == "bounded-evidence" && hop == 2 {
                    for suffix in [
                        "json",
                        "cypher",
                        "structured",
                        "exact.cypher",
                        "exact.structured",
                    ] {
                        std::fs::copy(
                            jobs.join(format!("{stem}.{suffix}")),
                            jobs.join(format!("{stem}-h2.{suffix}")),
                        )
                        .map_err(|e| e.to_string())?;
                    }
                }
                writeln!(file,"{}",json!({"state":if st==FixtureState::A{"A"}else{"B"},"case":case["case"],"cohort":case["cohort"],"name":label,"request":workload::request_record(&q),"rows":rows.iter().map(|r|r.iter().map(workload::encode_cell).collect::<Vec<_>>()).collect::<Vec<_>>(),"absolute_tolerance":1e-6,"relative_tolerance":1e-6})).map_err(|e|e.to_string())?;
            }
        }
    }
    Ok(())
}
fn run() -> Result<(), String> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.as_slice(){
        [cmd] if cmd=="self-test"=>self_test(),
        [cmd,root,st,receipts] if cmd=="expected-entities"=>load::expected_entities(Path::new(root),state(st)?,Path::new(receipts)),
        [cmd,path,requests] if cmd=="inspect-entities"=>load::inspect_entities(Path::new(path),Path::new(requests)),
        [cmd,path,jobs,name,front,mode,warmups,samples] if cmd=="read-schedule"=>load::read_schedule(Path::new(path),Path::new(jobs),name,front,mode=="exact",warmups.parse().map_err(|_|"warmups")?,samples.parse().map_err(|_|"samples")?),
        [cmd,root,st,receipts,jobs,samples] if cmd=="verify-mixed"=>load::verify_mixed(Path::new(root),state(st)?,Path::new(receipts),Path::new(jobs),Path::new(samples)),
        [cmd,root,st,receipts,jobs,samples] if cmd=="verify"=>load::verify(Path::new(root),state(st)?,Path::new(receipts),Path::new(jobs),Path::new(samples)),
        [cmd,path,list,count] if cmd=="cypher-imports"=>load::cypher_imports(Path::new(path),Path::new(list),count.parse().map_err(|_|"count")?),
        [cmd,path,keys,multiplier] if cmd=="retention"=>load::retention_churn(Path::new(path),keys.parse().map_err(|_|"keys")?,multiplier.parse().map_err(|_|"multiplier")?),
        [cmd,path,namespace,key,id] if cmd=="detach"=>load::detach(Path::new(path),namespace,key,id.parse().map_err(|_|"full ID")?),
        [cmd,list,mode] if cmd=="recovery-series"=>load::recovery_series(Path::new(list),mode=="read-only"),
        [cmd,path,mode] if cmd=="recovery"=>load::recover(Path::new(path),mode=="read-only"),
        [cmd,path,root,receipts,count] if cmd=="imports"=>load::import_schedule(Path::new(path),Path::new(root),Path::new(receipts),count.parse().map_err(|_|"count")?),
        [cmd,path,root,receipts,jobs] if cmd=="mixed"=>load::mixed_load(Path::new(path),Path::new(root),Path::new(receipts),Path::new(jobs),1000,200),
        [cmd,path,job,front,mode,warmups,samples] if cmd=="run"=>workload::run_cell(Path::new(path),Path::new(job),front,mode=="exact",warmups.parse().map_err(|_|"invalid warmups")?,samples.parse().map_err(|_|"invalid samples")?),
        [cmd,root,path,st,out] if cmd=="ingest"=>{let mut file=std::fs::File::create_new(out).map_err(|e|e.to_string())?;workload::ingest_fixture(Path::new(root),Path::new(path),state(st)?,&mut file)},
        [cmd,root,st,receipts,out] if cmd=="truth" || cmd=="truth-smoke"=>write_offline_truth(Path::new(root),state(st)?,Path::new(receipts),Path::new(out),cmd=="truth-smoke"),
        [cmd,manifest,input] if cmd=="report" || cmd=="report-smoke"=>{let manifest:Value=zeppelin_embed_bench::harness_json::from_slice(&std::fs::read(manifest).map_err(|e|e.to_string())?).map_err(|e|e.to_string())?;let records:Vec<Value>=std::fs::read_to_string(input).map_err(|e|e.to_string())?.lines().map(zeppelin_embed_bench::harness_json::from_str).collect::<Result<_,_>>().map_err(|e|e.to_string())?;if cmd=="report-smoke" { graph_workload::validate_smoke(&manifest,&records)?; } else { graph_workload::validate_matrix(&manifest,&records)?; }println!("{}",if cmd=="report-smoke" { graph_workload::summarize_smoke(&records)? } else { graph_workload::summarize(&records)? });Ok(())},
        _=>Err("usage: graph-workload self-test | ingest FIXTURE STORE A|B RECEIPTS | truth FIXTURE A|B RECEIPTS OUTPUT | report MANIFEST REPETITIONS".into())
    }
}
