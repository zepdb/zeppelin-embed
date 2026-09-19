//! Streaming input files. JSON is a tooling interchange, never an engine format.
use super::*;
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::Command;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileManifest {
    pub nodes: u64,
    pub edges: u64,
    pub vectors: u64,
    pub initial_batches: u64,
    pub state_b_nodes: u64,
    pub state_b_relationships: u64,
    pub queries: u64,
}
fn io(error: impl std::fmt::Display) -> Error {
    error.to_string()
}
fn create(root: &Path, name: &str) -> Result<BufWriter<File>, Error> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join(name))
        .map(BufWriter::new)
        .map_err(io)
}
fn row(out: &mut impl Write, value: &Value) -> Result<(), Error> {
    serde_json::to_writer(&mut *out, value).map_err(io)?;
    out.write_all(b"\n").map_err(io)
}
fn bits(out: &mut impl Write, values: &[u32]) -> Result<(), Error> {
    for value in values {
        out.write_all(&value.to_le_bytes()).map_err(io)?;
    }
    Ok(())
}
fn named(key: NodeKey) -> Value {
    json!({"namespace":key.namespace(),"key":key.index.to_string()})
}
fn fixed(prefix: String, length: usize) -> String {
    let mut value = prefix;
    value.extend(std::iter::repeat_n('x', length - value.len()));
    value
}
fn fixture_text(length: usize, rare: bool, corrected: bool) -> String {
    let mut text = if rare { "quartz cedar" } else { "amber cedar" }.to_owned();
    let pair = if corrected {
        "velvet delta"
    } else {
        "cobalt delta"
    };
    while text.len() + 1 + pair.len() <= length {
        text.push(' ');
        text.push_str(pair);
    }
    text.extend(std::iter::repeat_n(' ', length - text.len()));
    text
}
fn node_payload(node: NodeRecord, vector: Option<Value>, corrected: bool) -> Value {
    let key = node.key;
    let name = fixed(
        format!(
            "{}:{}:{}",
            key.kind.name(),
            key.index,
            if corrected { 'b' } else { 'a' }
        ),
        48,
    );
    let mut props = serde_json::Map::from_iter([
        ("name".into(), json!({"string":name})),
        ("active".into(), json!({"bool":true})),
        ("quality".into(), json!({"f64_bits":"3fe0000000000000"})),
    ]);
    if node.ordinal.is_multiple_of(10) {
        props.insert("aliases".into(),json!({"string_list":if node.ordinal.is_multiple_of(20){Vec::<&str>::new()}else{vec!["alpha","beta"]}}));
    }
    if key.kind == Kind::Meeting {
        props.insert(
            "timestamp".into(),
            json!({"i64":1_700_000_000_i64+17_280*key.index as i64}),
        );
    }
    if key.kind == Kind::Chunk {
        props.insert("ordinal".into(), json!({"i64":key.index}));
        props.insert("topic".into(), json!({"i64":node.topic}));
        props.insert(
            "excerpt".into(),
            json!({"string":fixed(format!("evidence:{}:",key.index),256)}),
        );
    }
    let shared = matches!(key.kind, Kind::Person | Kind::Project | Kind::Topic);
    let mut labels = vec![key.kind.label()];
    if shared && node.ordinal.is_multiple_of(7) {
        labels.push("Shared");
    }
    let length = match key.kind {
        Kind::Chunk => 2048,
        Kind::Meeting => 512,
        Kind::Decision | Kind::Action => 128,
        _ => 48,
    };
    let text = if (shared && node.ordinal % 2 == 1)
        || (key.kind == Kind::Chunk && key.index.is_multiple_of(100))
    {
        None
    } else if key.kind == Kind::Chunk && key.index % 100 == 1 {
        Some(String::new())
    } else if key.kind == Kind::Chunk && key.index % 100 == 2 {
        Some(" ".repeat(32))
    } else {
        Some(fixture_text(
            length,
            node.ordinal.is_multiple_of(997),
            corrected,
        ))
    };
    json!({"kind":"node","key":named(key),"ordinal":node.ordinal,"labels":labels,"properties":props,"text":text,"vector":vector})
}
fn edge_payload(edge: EdgeRecord, corrected: bool) -> Value {
    let props = if corrected || edge.ordinal.is_multiple_of(3) {
        json!({"ordinal":{"i64":edge.ordinal},"weight":{"f64_bits":if corrected{"3ff8000000000000"}else{"3fe0000000000000"}}})
    } else {
        json!({})
    };
    json!({"kind":"relationship",
        "key":{"namespace":"fixture-v1/relationship",
        "key":edge.ordinal.to_string()},
        "ordinal":edge.ordinal,
        "source":named(edge.source),
        "target":named(edge.target),
        "type":edge.kind,
        "properties":props})
}
fn permutation(count: u64, selected: u64, words: &mut WordStream) -> Vec<u64> {
    let mut all = (0..count).collect::<Vec<_>>();
    for end in (1..all.len()).rev() {
        let next = topology::choose(words, (end + 1) as u64) as usize;
        all.swap(end, next);
    }
    all.truncate(selected as usize);
    all
}
fn node_hub(config: Config, key: NodeKey) -> bool {
    match key.kind {
        Kind::Project => key.index == 0,
        Kind::Meeting => project(config, key.index) == 0,
        Kind::Chunk => project(config, key.index / 20) == 0,
        Kind::Decision | Kind::Action => project(config, key.index / 6) == 0,
        _ => false,
    }
}
fn summary_json(summary: &Summary) -> Value {
    json!({"nodes":summary.nodes,
        "edges":summary.edges,
        "vectors":summary.vectors,
        "vector_bytes":summary.vector_bytes,
        "node_kinds":summary.node_kinds,
        "edge_kinds":summary.edge_kinds,
        "indegree":summary.indegree,
        "outdegree":summary.outdegree,
        "degree":summary.degree,
        "project_hub_degree":summary.project_hub_degree,
        "text":{"absent":summary.absent_text,
        "empty":summary.empty_text,
        "whitespace":summary.whitespace_text,
        "indexed":summary.indexed_text},
        "populations":{"both":summary.both,
        "vector_only":summary.vector_only,
        "text_only":summary.text_only,
        "neither":summary.neither},
        "initial_batches":summary.batches,
        "max_initial_batch_changes":summary.max_batch_changes})
}
/// Uses the explicitly recorded platform build tool, without a new dependency.
/// SHA-256 is outside the timed product path. Missing/failed tool is fatal.
fn digest(path: &Path) -> Result<String, Error> {
    let output = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .map_err(io)?;
    if !output.status.success() {
        return Err(format!(
            "shasum failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let text = String::from_utf8(output.stdout).map_err(io)?;
    let hash = text.split_whitespace().next().ok_or("missing SHA-256")?;
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid SHA-256 tool output".into());
    }
    Ok(hash.to_owned())
}
/// Writes a new fixture directory. Refuses to overwrite any input file.
/// Retains degree counters and selected correction records, never the corpus or
/// vector matrix. These are separately reported tooling allocations.
pub fn write_fixture(
    root: &Path,
    config: Config,
    source_pin: &str,
    factory: &mut dyn FnMut(&str) -> WordStream,
) -> Result<FileManifest, Error> {
    if source_pin.is_empty() {
        return Err("source pin is required".into());
    }
    std::fs::create_dir_all(root).map_err(io)?;
    let summary = inventory(config, &mut factory("graph-fixture-v1/topics"))?;
    let mut selected_nodes = permutation(
        config.node_count(),
        config.node_count() / 100,
        &mut factory("graph-fixture-v1/state-b-nodes"),
    );
    let mut node_order = Vec::with_capacity(selected_nodes.len());
    for row in selected_nodes {
        node_order.push((!node_hub(config, node_key(config, row)?), row));
    }
    node_order.sort_by_key(|(ordinary, _)| *ordinary);
    selected_nodes = node_order.into_iter().map(|(_, row)| row).collect();
    let mut selected_edges = permutation(
        config.edge_count(),
        config.edge_count() / 50,
        &mut factory("graph-fixture-v1/state-b-relationships"),
    );
    selected_edges.sort_by_key(|row| project(config, *row / 110) != 0);
    let node_set = selected_nodes
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    let edge_set = selected_edges
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    let mut corrections_nodes = BTreeMap::new();
    let mut corrections_edges = BTreeMap::new();
    let mut inputs = create(root, "batches-a.jsonl")?;
    let mut vectors = create(root, "vectors-a.f32le")?;
    let mut vector_generator = VectorGenerator::new(factory)?;
    let mut initial_batches = 0;
    let mut indexed_tokens = 0_u64;
    let mut declared_text_bytes = 0_u64;
    let mut indexed_documents = 0_u64;
    let mut term_documents = BTreeMap::<String, u64>::new();
    visit(
        config,
        &mut factory("graph-fixture-v1/topics"),
        &mut |batch| {
            let mut changes = Vec::with_capacity(batch.nodes.len() + batch.edges.len());
            for node in batch.nodes {
                let vector = if let Some(topic) = node.topic {
                    bits(&mut vectors, &vector_generator.chunk(topic, false)?)?;
                    Some(
                        json!({"file":"vectors-a.f32le","offset":node.key.index*DIMS as u64*4,"dimensions":DIMS}),
                    )
                } else {
                    None
                };
                let payload = node_payload(node, vector, false);
                if let Some(text) = payload["text"].as_str() {
                    declared_text_bytes += text.len() as u64;
                    let terms = text.split_ascii_whitespace().count() as u64;
                    indexed_tokens += terms;
                    indexed_documents += u64::from(terms > 0);
                    for term in text
                        .split_ascii_whitespace()
                        .collect::<std::collections::BTreeSet<_>>()
                    {
                        *term_documents.entry(term.into()).or_default() += 1;
                    }
                }
                changes.push(
                    json!({"operation":"create","revision":1,"expected":"absent","image":payload}),
                );
                if node_set.contains(&node.ordinal) {
                    corrections_nodes.insert(node.ordinal, node);
                }
            }
            for edge in batch.edges {
                changes.push(json!({"operation":"create","revision":1,"expected":"absent","image":edge_payload(edge,false)}));
                if edge_set.contains(&edge.ordinal) {
                    corrections_edges.insert(edge.ordinal, edge);
                }
            }
            row(
                &mut inputs,
                &json!({"batch":initial_batches,"state":"A",
                    "expected_disposition":"changed","mutation_ordinal":initial_batches+1,
                    "expected_generation":{"relative_to":"admitted","increment":1},
                    "changes":changes}),
            )?;
            initial_batches += 1;
            Ok(())
        },
    )?;
    if indexed_documents != summary.indexed_text {
        return Err("actual serialized text population differs from topology inventory".into());
    }
    inputs.flush().map_err(io)?;
    vectors.flush().map_err(io)?;
    drop(inputs);
    drop(vectors);
    let mut correction_vectors = create(root, "vectors-b.f32le")?;
    let mut original_vectors = File::open(root.join("vectors-a.f32le")).map_err(io)?;
    let mut vector_offset = 0_u64;
    let total_batches = selected_nodes.len().div_ceil(128)
        + selected_edges.len().div_ceil(128)
        + (selected_edges.len() / 2).div_ceil(128);
    let mut schedule = create(root, "batches-b.jsonl")?;
    let mut batch_index = 0;
    let mut final_tail = 0;
    let mut emit = |changes: Vec<Value>| -> Result<(), Error> {
        if changes.is_empty() {
            return Ok(());
        }
        if changes.len() > 128 {
            return Err("state B batch exceeds128 changes".into());
        }
        final_tail = changes.len();
        row(
            &mut schedule,
            &json!({"batch":batch_index,
                "state":"B",
            "expected_disposition":"changed",
            "mutation_ordinal":initial_batches+batch_index as u64+1,
            "expected_generation":{"relative_to":"admitted","increment":1},
                "changes":changes,
                "after":if batch_index+1==total_batches{"retain-active-tail"}else{"checkpoint-and-consolidate"}}),
        )?;
        batch_index += 1;
        Ok(())
    };
    let mut group = Vec::with_capacity(128);
    for ordinal in &selected_nodes {
        let node = *corrections_nodes
            .get(ordinal)
            .ok_or("selected node was not generated")?;
        let vector = if let Some(topic) = node.topic {
            let value = vector_generator.chunk(topic, true)?;
            original_vectors
                .seek(SeekFrom::Start(node.key.index * DIMS as u64 * 4))
                .map_err(io)?;
            let mut old = vec![0; DIMS * 4];
            original_vectors.read_exact(&mut old).map_err(io)?;
            let new = value
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>();
            if old == new {
                return Err("state B vector did not change".into());
            }
            bits(&mut correction_vectors, &value)?;
            let reference =
                json!({"file":"vectors-b.f32le","offset":vector_offset,"dimensions":DIMS});
            vector_offset += DIMS as u64 * 4;
            Some(reference)
        } else {
            None
        };
        group.push(json!({"operation":"put","revision":2,"expected":{"current_incarnation_of":named(node.key)},"image":node_payload(node,vector,true)}));
        if group.len() == 128 {
            emit(std::mem::take(&mut group))?;
        }
    }
    if !group.is_empty() {
        emit(std::mem::take(&mut group))?;
    }
    for (position, ordinal) in selected_edges.iter().enumerate() {
        let edge = *corrections_edges
            .get(ordinal)
            .ok_or("selected relationship was not generated")?;
        if position % 2 == 0 {
            group.push(json!({"operation":"put",
                "revision":2,
                "expected":{"current_incarnation_of":{"namespace":"fixture-v1/relationship",
                "key":ordinal.to_string()}},
                "image":edge_payload(edge,
                true)}));
        } else {
            group.push(json!({"operation":"delete",
                "revision":2,
                "expected":{"current_incarnation_of":{"namespace":"fixture-v1/relationship",
                "key":ordinal.to_string()}},
                "key":{"namespace":"fixture-v1/relationship",
                "key":ordinal.to_string()}}));
        }
        if group.len() == 128 {
            emit(std::mem::take(&mut group))?;
        }
    }
    emit(std::mem::take(&mut group))?;
    for ordinal in selected_edges.iter().skip(1).step_by(2) {
        let edge = *corrections_edges
            .get(ordinal)
            .ok_or("selected relationship was not generated")?;
        group.push(json!({"operation":"recreate","revision":3,"expected":{"deletion_revision":2},"image":edge_payload(edge,true)}));
        if group.len() == 128 {
            emit(std::mem::take(&mut group))?;
        }
    }
    emit(group)?;
    if batch_index != total_batches {
        return Err("state B schedule batch count mismatch".into());
    }
    schedule.flush().map_err(io)?;
    correction_vectors.flush().map_err(io)?;
    drop(schedule);
    drop(correction_vectors);
    let query_count = write_queries(root, config, &mut vector_generator, &term_documents)?;
    let files = [
        "batches-a.jsonl",
        "vectors-a.f32le",
        "batches-b.jsonl",
        "vectors-b.f32le",
        "queries.jsonl",
        "query-vectors.f32le",
    ];
    let mut inventory = Vec::new();
    for name in files {
        let path = root.join(name);
        inventory.push(json!({"name":name,"bytes":std::fs::metadata(&path).map_err(io)?.len(),"sha256":digest(&path)?}));
    }
    let result = FileManifest {
        nodes: summary.nodes,
        edges: summary.edges,
        vectors: summary.vectors,
        initial_batches,
        state_b_nodes: selected_nodes.len() as u64,
        state_b_relationships: selected_edges.len() as u64,
        queries: query_count,
    };
    let manifest = json!({"version":VERSION,
        "generator_version":1,
        "scale":match config.scale{Scale::Baseline=>"baseline",
        Scale::Stress=>"stress-10x",
        Scale::Small=>"small-correctness"},
        "seed":format!("{:016x}",
        config.seed),
        "source_pin":source_pin,
        "build":{"rustc":tool_version("rustc",
        &["--version"] )?,
        "host":std::env::consts::OS,
        "architecture":std::env::consts::ARCH},
        "prng":"rand-0.9.5/rand_chacha-0.9.0/ChaCha8Rng; tests/tooling_seed.rs derivation-v1",
        "analyzer":{"name":"TokenizerConfig::text_default",
        "epoch":1035315901113778624_u64,
        "reference_subset":"lowercase ASCII whitespace",
        "vocabulary":["amber",
        "cedar",
        "cobalt",
        "delta",
        "quartz",
        "velvet",
        "zephyr"]},
        "text_inventory":{"indexed_documents":indexed_documents,
        "indexed_tokens":indexed_tokens,
        "declared_bytes":declared_text_bytes,
        "document_frequencies":term_documents},
        "embedding":{"dimensions":DIMS,
        "metric":"squared-l2",
        "normalization":"ordered-f64-low11-mixture-single-f32-cast-v1",
        "vector_bytes":summary.vector_bytes,
        "corpus_coefficients":[0.875,
        0.125],
        "query_coefficients":[0.96875,
        0.03125],
        "centroids":64},
        "inventory":summary_json(&summary),
        "generation_contract":{"mutation":"admitted+1","maintenance":"record-returned-generation",
        "mutation_ordinals":"exclude-maintenance"},
        "state_a":{"after_ingest":"checkpoint-and-consolidate","mutation_batches":initial_batches,
        "generation":"record-after-barrier"},
        "state_b":{"nodes":selected_nodes,
        "relationships":selected_edges,
        "batches":total_batches,
        "final_active_tail_changes":final_tail,
        "checkpoint_trigger_envelopes":64,
        "checkpoint_trigger_bytes":16777216,
        "barriers":"requested actions only; actual engine run counts recorded by product driver"},
        "query_cases":query_count,
        "policy":{"version":1,
        "alpha":0.75,
        "rule_shifts":false,
        "anchors":"full-live vector norm enclosure and full-live lexical maximum"},
        "files":inventory,
        "tooling_memory":{"degree_counter_bytes":config.node_count()*16,
        "largest_permutation_bytes":config.edge_count()*8,
        "vector_matrix_retained":false,
        "engine_memory_claim":false}});
    let mut out = create(root, "manifest.json")?;
    serde_json::to_writer_pretty(&mut out, &manifest).map_err(io)?;
    out.flush().map_err(io)?;
    Ok(result)
}
fn tool_version(tool: &str, args: &[&str]) -> Result<String, Error> {
    let out = Command::new(tool).args(args).output().map_err(io)?;
    if !out.status.success() {
        return Err(format!("{tool} version failed"));
    }
    String::from_utf8(out.stdout)
        .map(|v| v.trim().to_owned())
        .map_err(io)
}
fn write_queries(
    root: &Path,
    config: Config,
    generator: &mut VectorGenerator,
    term_documents: &BTreeMap<String, u64>,
) -> Result<u64, Error> {
    let mut queries = create(root, "queries.jsonl")?;
    let mut vectors = create(root, "query-vectors.f32le")?;
    for i in 0..100_u64 {
        let requested = match i % 10 {
            0 => "hub",
            1 => "empty",
            2 => "sparse",
            _ => "ordinary",
        };
        let project_id = if i % 10 == 0 {
            0
        } else {
            1 + (17 * i) % (config.projects() - 1)
        };
        let meetings = (0..config.meetings())
            .filter(|m| project(config, *m) == project_id)
            .collect::<Vec<_>>();
        let mut frequency = BTreeMap::<u64, u64>::new();
        for m in &meetings {
            for person in participants(config, *m) {
                *frequency.entry(person).or_default() += 1;
            }
        }
        let person = match i % 10 {
            0 => 0,
            1 => (0..config.people())
                .find(|p| !frequency.contains_key(p))
                .ok_or("empty person cohort unavailable")?,
            2 => frequency
                .iter()
                .min_by_key(|(p, n)| (**n, **p))
                .map_or(0, |(p, _)| *p),
            _ => meetings.first().map_or(0, |m| participants(config, *m)[0]),
        };
        let eligible = frequency.get(&person).copied().unwrap_or(0) * 20;
        let centroid = i % CENTROIDS as u64;
        bits(&mut vectors, &generator.query(centroid)?)?;
        let (terms, phrase, lexical_cohort) = match i % 4 {
            0 => (vec!["quartz"], false, "rare"),
            1 => (vec!["amber"], false, "common"),
            2 => (vec!["amber", "cedar"], true, "phrase"),
            _ => (vec!["zephyr"], false, "no-match"),
        };
        row(
            &mut queries,
            &json!({"case":i,
                "cohort":requested,
                "normal_timing":eligible>0&&requested!="empty",
                "project":named(NodeKey{kind:Kind::Project,
                index:project_id}),
                "person":named(NodeKey{kind:Kind::Person,
                index:person}),
                "topic":centroid,
                "vector":{"file":"query-vectors.f32le",
                "offset":i*DIMS as u64*4,
                "dimensions":DIMS},
                "lexical":{"terms":terms,
                "phrase":phrase,
                "cohort":lexical_cohort,
                "normal_timing":lexical_cohort!="no-match"},
                "generated_topology":{"project_meetings":meetings.len(),
                "eligible_chunks":eligible,
                "project_evidence_prelimit":meetings.len()*6,
                "project_evidence_rows":(meetings.len()*6).min(100),
                "semantic_context_rows":60,
                "alice_ranking_rows":eligible.min(20),
                "bounded_evidence_rows":80,
                "lexical_evidence_rows":term_documents.get(terms[0]).copied().unwrap_or(0).min(20),
                "hybrid_project_evidence_rows":if meetings.is_empty(){0}else{20}},
                "meeting":meetings.first().map(|m|named(NodeKey{kind:Kind::Meeting,
                index:*m})),
                "queries":[{"name":"project-evidence",
                "limit":100,
                "projection":["item.id",
                "item.name",
                "meeting.id",
                "meeting.name",
                "chunk.id",
                "chunk.excerpt"],
                "order":"timestamp DESC, item.id, meeting.id, chunk.id"},
                {"name":"semantic-context",
                "k":20,
                "projection":["seed.id",
                "seed.distance",
                "chunk.excerpt",
                "meeting.id",
                "meeting.name",
                "entity.id",
                "entity.name"]},
                {"name":"alice-project-ranking",
                "k":20,
                "projection":["chunk.id",
                "distance",
                "chunk.excerpt",
                "meeting.id"]},
                {"name":"lexical-evidence",
                "k":20,
                "projection":["node.id",
                "node.name",
                "text_present",
                "text",
                "bm25"]},
                {"name":"hybrid-project-evidence",
                "k":20,
                "projection":["node.id",
                "node.name",
                "fused",
                "distance",
                "bm25",
                "vector_present",
                "indexed_text_present",
                "text"]},
                {"name":"bounded-evidence",
                "min":1,
                "max":2,
                "types":["HAS_CHUNK",
                "MENTIONS"],
                "projection":["endpoint.id",
                "relationship_ids"]}]}),
        )?;
    }
    queries.flush().map_err(io)?;
    vectors.flush().map_err(io)?;
    Ok(100)
}
/// Validates the immutable input inventory and critical dimensions/counts before
/// a downstream benchmark may consume it. This is not product qualification.
pub fn validate_fixture(root: &Path) -> Result<FileManifest, Error> {
    let manifest: Value =
        serde_json::from_reader(File::open(root.join("manifest.json")).map_err(io)?).map_err(io)?;
    if manifest["version"] != VERSION
        || manifest["generator_version"] != 1
        || manifest["embedding"]["dimensions"] != DIMS
    {
        return Err("unsupported fixture version/dimensions".into());
    }
    let config = Config::new(match manifest["scale"].as_str() {
        Some("baseline") => Scale::Baseline,
        Some("stress-10x") => Scale::Stress,
        Some("small-correctness") => Scale::Small,
        _ => return Err("invalid fixture scale".into()),
    });
    let number = |value: &Value| {
        value
            .as_u64()
            .ok_or_else(|| "missing numeric manifest field".to_owned())
    };
    let inv = &manifest["inventory"];
    if number(&inv["nodes"])? != config.node_count()
        || number(&inv["edges"])? != config.edge_count()
        || number(&inv["vectors"])? != config.meetings() * 20
        || number(&inv["vector_bytes"])? != config.meetings() * 20 * DIMS as u64 * 4
    {
        return Err("fixture counts differ from versioned scale".into());
    }
    let required = std::collections::BTreeSet::from([
        "batches-a.jsonl",
        "vectors-a.f32le",
        "batches-b.jsonl",
        "vectors-b.f32le",
        "queries.jsonl",
        "query-vectors.f32le",
    ]);
    let mut found = std::collections::BTreeSet::new();
    for file in manifest["files"]
        .as_array()
        .ok_or("missing file inventory")?
    {
        let name = file["name"].as_str().ok_or("missing file name")?;
        if !required.contains(name) || !found.insert(name) {
            return Err("unknown/duplicate fixture file".into());
        }
        let path = root.join(name);
        if std::fs::metadata(&path).map_err(io)?.len() != number(&file["bytes"])?
            || digest(&path)? != file["sha256"].as_str().ok_or("missing file hash")?
        {
            return Err(format!("fixture file differs: {name}"));
        }
    }
    if required != found {
        return Err("incomplete fixture file inventory".into());
    }
    if std::fs::metadata(root.join("vectors-a.f32le"))
        .map_err(io)?
        .len()
        != number(&inv["vector_bytes"])?
    {
        return Err("vector file geometry differs".into());
    }
    let count_lines = |name| -> Result<u64, Error> {
        let mut count = 0;
        for line in BufReader::new(File::open(root.join(name)).map_err(io)?).lines() {
            line.map_err(io)?;
            count += 1;
        }
        Ok(count)
    };
    let initial_batches = count_lines("batches-a.jsonl")?;
    let queries = count_lines("queries.jsonl")?;
    if initial_batches != number(&inv["initial_batches"])?
        || queries != 100
        || number(&manifest["query_cases"])? != 100
    {
        return Err("batch/query counts differ".into());
    }
    if manifest["analyzer"]["epoch"] != 1035315901113778624_u64
        || manifest["policy"]["version"] != 1
        || manifest["policy"]["alpha"] != 0.75
        || manifest["policy"]["rule_shifts"] != false
    {
        return Err("fixture analyzer or score policy differs".into());
    }
    if manifest["generation_contract"]["mutation"] != "admitted+1"
        || manifest["generation_contract"]["maintenance"] != "record-returned-generation"
        || manifest["generation_contract"]["mutation_ordinals"] != "exclude-maintenance"
        || manifest["state_a"]["after_ingest"] != "checkpoint-and-consolidate"
        || manifest["state_a"]["mutation_batches"] != initial_batches
        || manifest["state_a"]["generation"] != "record-after-barrier"
    {
        return Err("fixture generation contract differs".into());
    }
    let mut mutation_ordinal = 0_u64;
    visit_batches(root, FixtureState::A, &mut |row| {
        mutation_ordinal += 1;
        validate_generation(&row, mutation_ordinal)
    })?;
    let selected_nodes = manifest["state_b"]["nodes"]
        .as_array()
        .ok_or("missing selected nodes")?;
    let selected_edges = manifest["state_b"]["relationships"]
        .as_array()
        .ok_or("missing selected edges")?;
    for (selected, total, divisor) in [
        (selected_nodes, config.node_count(), 100),
        (selected_edges, config.edge_count(), 50),
    ] {
        let mut unique = std::collections::BTreeSet::new();
        if selected.len() as u64 != total / divisor {
            return Err("state B selected count differs".into());
        }
        for id in selected {
            let id = number(id)?;
            if id >= total || !unique.insert(id) {
                return Err("state B invalid or repeated selection".into());
            }
        }
    }
    if std::fs::metadata(root.join("query-vectors.f32le"))
        .map_err(io)?
        .len()
        != 100 * DIMS as u64 * 4
    {
        return Err("query vector file geometry differs".into());
    }
    let mut actual_batches = 0;
    let mut changes = 0;
    let mut tail = 0;
    visit_batches(root, FixtureState::B, &mut |row| {
        mutation_ordinal += 1;
        validate_generation(&row, mutation_ordinal)?;
        let batch = row["changes"].as_array().ok_or("state B changes missing")?;
        if batch.is_empty() || batch.len() > 128 || row["batch"] != actual_batches {
            return Err("invalid state B batch".into());
        }
        tail = batch.len();
        changes += batch.len();
        actual_batches += 1;
        let expected = if actual_batches == number(&manifest["state_b"]["batches"])? {
            "retain-active-tail"
        } else {
            "checkpoint-and-consolidate"
        };
        if row["after"] != expected {
            return Err("state B barrier mismatch".into());
        }
        Ok(())
    })?;
    if actual_batches != number(&manifest["state_b"]["batches"])?
        || changes != selected_nodes.len() + selected_edges.len() + selected_edges.len() / 2
        || tail as u64 != number(&manifest["state_b"]["final_active_tail_changes"])?
    {
        return Err("state B serialized schedule differs".into());
    }
    Ok(FileManifest {
        nodes: config.node_count(),
        edges: config.edge_count(),
        vectors: config.meetings() * 20,
        initial_batches,
        state_b_nodes: manifest["state_b"]["nodes"]
            .as_array()
            .ok_or("missing selected nodes")?
            .len() as u64,
        state_b_relationships: manifest["state_b"]["relationships"]
            .as_array()
            .ok_or("missing selected edges")?
            .len() as u64,
        queries,
    })
}

fn validate_generation(row: &Value, mutation_ordinal: u64) -> Result<(), Error> {
    if row["mutation_ordinal"] != mutation_ordinal
        || row["expected_disposition"] != "changed"
        || row["expected_generation"]["relative_to"] != "admitted"
        || row["expected_generation"]["increment"] != 1
    {
        return Err("batch generation expectation differs".into());
    }
    Ok(())
}

/// Raw versioned tooling data for adapters. Validation is mandatory before a
/// benchmark starts; adapters preserve these primitive values, not engine IDs.
pub fn read_manifest(root: &Path) -> Result<Value, Error> {
    serde_json::from_reader(File::open(root.join("manifest.json")).map_err(io)?).map_err(io)
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FixtureState {
    A,
    B,
}
/// Visits one serialized logical batch at a time. Same-batch node endpoints are
/// namespaced keys; the product adapter binds them as local references and saves
/// returned full IDs. Decimal fixture indices are never caller-selected IDs.
pub fn visit_batches(
    root: &Path,
    state: FixtureState,
    sink: &mut dyn FnMut(Value) -> Result<(), Error>,
) -> Result<(), Error> {
    let file = match state {
        FixtureState::A => "batches-a.jsonl",
        FixtureState::B => "batches-b.jsonl",
    };
    for line in BufReader::new(File::open(root.join(file)).map_err(io)?).lines() {
        let value = serde_json::from_str(&line.map_err(io)?).map_err(io)?;
        sink(value)?;
    }
    Ok(())
}
