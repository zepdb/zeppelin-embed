//! Tooling translation of bounded primitive fixture records to public requests.
#![allow(clippy::result_large_err)]
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;
use zeppelin_embed::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower, Normalization};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl};
use zeppelin_embed::property_graph::query::completed::{
    CompletedGraphResult, GraphQueryOptions, Value as Cell,
};
use zeppelin_embed::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use zeppelin_embed::property_graph::*;
use zeppelin_embed_adversarial_oracle::graph_fixture as oracle;
use zeppelin_embed_bench::{
    graph_fixture::{self, FixtureState},
    harness_json::{Value, json},
};
use zeppelin_embed_cypher::{CompileLimits, execute};

pub type Ids = BTreeMap<(String, String, String), u128>;
pub fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}
pub fn tower() -> EmbeddingTower {
    EmbeddingTower {
        model_id: "ze73-fixture".into(),
        model_version: "1".into(),
        weights_digest: vec![0x73],
        dims: 768,
        normalization: Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 512,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    }
}
pub fn create(path: &Path) -> Result<GraphStore, String> {
    GraphStore::create(
        path,
        OpenOptions::new().with_max_resident_bytes(256 << 20),
        Some(tower()),
    )
    .map_err(|e| e.to_string())
}
pub fn open(path: &Path) -> Result<GraphStore, String> {
    GraphStore::open(
        path,
        OpenOptions::new().with_max_resident_bytes(256 << 20),
        Some(tower()),
    )
    .map_err(|e| e.to_string())
}
fn s<'a>(v: &'a Value, k: &str) -> Result<&'a str, String> {
    v.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string {k}"))
}
fn u(v: &Value, k: &str) -> Result<u64, String> {
    v.get(k)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("missing integer {k}"))
}
fn key(v: &Value, kind: &str) -> Result<(String, String, String), String> {
    Ok((kind.into(), s(v, "namespace")?.into(), s(v, "key")?.into()))
}
pub fn vector(root: &Path, v: &Value) -> Result<Vec<f32>, String> {
    let file = s(v, "file")?;
    if !["vectors-a.f32le", "vectors-b.f32le", "query-vectors.f32le"].contains(&file) {
        return Err("unexpected vector file".into());
    }
    if u(v, "dimensions")? != 768 {
        return Err("fixture dimensions differ".into());
    }
    let mut f = File::open(root.join(file)).map_err(|e| e.to_string())?;
    f.seek(SeekFrom::Start(u(v, "offset")?))
        .map_err(|e| e.to_string())?;
    let mut bytes = vec![0; 768 * 4];
    f.read_exact(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}
fn property<'a>(v: &'a Value, list: &'a [&'a str]) -> Result<PropertyValue<'a>, String> {
    let data = if let Some(s) = v.get("string").and_then(Value::as_str) {
        PropertyData::String(s)
    } else if let Some(b) = v.get("bool").and_then(Value::as_bool) {
        PropertyData::Bool(b)
    } else if let Some(n) = v.get("i64").and_then(Value::as_i64) {
        PropertyData::I64(n)
    } else if let Some(b) = v.get("f64_bits").and_then(Value::as_str) {
        PropertyData::F64(f64::from_bits(
            u64::from_str_radix(b, 16).map_err(|e| e.to_string())?,
        ))
    } else if v.get("string_list").is_some() {
        PropertyData::Strings(list)
    } else {
        return Err("unsupported fixture property".into());
    };
    PropertyValue::new(data).map_err(|e| e.to_string())
}
/// Submit each recipe envelope once, preserving same-batch endpoint references.
/// A commit error is fatal (ZE-290), never a restart or reduced fixture.
pub fn apply_record(
    store: &GraphStore,
    root: &Path,
    row: &Value,
    ids: &mut Ids,
) -> Result<Value, String> {
    let changes = row["changes"].as_array().ok_or("missing changes")?;
    let mut lists = Vec::new();
    let mut labels = Vec::new();
    let mut coords = Vec::new();
    for change in changes {
        let image = &change["image"];
        let mut props = BTreeMap::new();
        if let Some(map) = image["properties"].as_object() {
            for (name, v) in map {
                let entries = v["string_list"]
                    .as_array()
                    .map(|values| {
                        values
                            .iter()
                            .map(|v| v.as_str().ok_or("invalid string list"))
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .transpose()?
                    .unwrap_or_default();
                props.insert(name.clone(), entries);
            }
        }
        lists.push(props);
        labels.push(
            image["labels"]
                .as_array()
                .map(|values| {
                    values
                        .iter()
                        .map(|v| {
                            GraphName::new(v.as_str().ok_or("invalid label")?)
                                .map_err(|e| e.to_string())
                        })
                        .collect::<Result<Vec<_>, String>>()
                })
                .transpose()?
                .unwrap_or_default(),
        );
        coords.push(if image["vector"].is_object() {
            Some(vector(root, &image["vector"])?)
        } else {
            None
        });
    }
    let mut props = Vec::new();
    for (change, ls) in changes.iter().zip(&lists) {
        let mut p = Vec::new();
        if let Some(map) = change["image"]["properties"].as_object() {
            for (name, v) in map {
                p.push(GraphProperty::new(
                    GraphName::new(name).map_err(|e| e.to_string())?,
                    property(v, ls.get(name).ok_or("missing list backing")?)?,
                ));
            }
        }
        props.push(p);
    }
    let relationship_props = props.clone();
    let document = tower();
    let mut contents = Vec::new();
    for (((change, ls), ps), cs) in changes.iter().zip(&mut labels).zip(&mut props).zip(&coords) {
        contents.push(if change["image"]["kind"] == "node" {
            Some(
                CanonicalContents::node(
                    ls,
                    ps,
                    change["image"]["text"].as_str(),
                    cs.as_ref()
                        .map(|v| CanonicalEmbedding::new(&document, v).map_err(|e| e.to_string()))
                        .transpose()?,
                )
                .map_err(|e| e.to_string())?,
            )
        } else {
            None
        });
    }
    let ledger_before = work_ledger(store)?;
    let (outcome, request_ns) = with_local_refs(|scope| -> Result<_, String> {
        let endpoint = |v: &Value| -> Result<NodeRef<'_>, String> {
            let k = key(v, "node")?;
            if let Some(id) = ids.get(&k) {
                return Ok(NodeRef::Existing(
                    NodeId::new(*id).map_err(|e| e.to_string())?,
                ));
            }
            let index = changes
                .iter()
                .position(|c| c["image"]["kind"] == "node" && c["image"]["key"] == *v)
                .ok_or("missing endpoint")?;
            Ok(NodeRef::Local(
                scope.node(index).map_err(|e| e.to_string())?,
            ))
        };
        let mut writes = Vec::new();
        for (index, change) in changes.iter().enumerate() {
            let image = &change["image"];
            let kind = if image.is_null() {
                change["kind"].as_str().unwrap_or("relationship")
            } else {
                s(image, "kind")?
            };
            let k = if image.is_null() {
                &change["key"]
            } else {
                &image["key"]
            };
            let entity = ids
                .get(&key(k, kind)?)
                .map(|id| -> Result<_, String> {
                    Ok(if kind == "node" {
                        EntityId::Node(NodeId::new(*id).map_err(|e| e.to_string())?)
                    } else {
                        EntityId::Relationship(RelId::new(*id).map_err(|e| e.to_string())?)
                    })
                })
                .transpose()?;
            let operation = match s(change, "operation")? {
                "create" => StructuredOperation::Create,
                "put" => StructuredOperation::Put(entity.ok_or("put missing ID")?),
                "delete" => StructuredOperation::Delete(
                    entity.ok_or("delete missing ID")?,
                    if change["detach"] == true {
                        GraphDeleteMode::Detach
                    } else {
                        GraphDeleteMode::Restrict
                    },
                ),
                "recreate" => StructuredOperation::Recreate(
                    GraphRevision::new(u(&change["expected"], "deletion_revision")?)
                        .map_err(|e| e.to_string())?,
                ),
                _ => return Err("unknown fixture operation".into()),
            };
            let image = if let Some(c) = contents.get(index).and_then(Option::as_ref) {
                Some(WriteImage::Node(c))
            } else if !image.is_null() {
                Some(WriteImage::Relationship {
                    source: endpoint(&image["source"])?,
                    target: endpoint(&image["target"])?,
                    relationship_type: GraphName::new(s(image, "type")?)
                        .map_err(|e| e.to_string())?,
                    properties: relationship_props
                        .get(index)
                        .ok_or("missing relationship properties")?,
                })
            } else {
                None
            };
            writes.push(StructuredWrite {
                key: ApplicationKey::new(
                    if kind == "node" {
                        EntityKind::Node
                    } else {
                        EntityKind::Relationship
                    },
                    s(k, "namespace")?,
                    s(k, "key")?,
                )
                .map_err(|e| e.to_string())?,
                revision: GraphRevision::new(u(change, "revision")?).map_err(|e| e.to_string())?,
                operation,
                image,
            });
        }
        let request_control = control();
        let start = std::time::Instant::now();
        let result = store.apply_batch(&writes, &request_control);
        let elapsed = start.elapsed().as_nanos();
        result.map(|result|(result,elapsed)).map_err(|e|format!("fixture batch {} failed; baseline uninterrupted ingestion requires ZE-290 if this is the known write-limit failure: {e}",row["batch"]))
    })?;
    if outcome.receipts().len() != changes.len() {
        return Err("partial fixture receipts".into());
    }
    let mut receipts = Vec::new();
    for (change, receipt) in changes.iter().zip(outcome.receipts()) {
        let (kind, id) = match receipt.entity {
            EntityId::Node(n) => ("node", n.get()),
            EntityId::Relationship(r) => ("relationship", r.get()),
        };
        let k = if change["image"].is_null() {
            &change["key"]
        } else {
            &change["image"]["key"]
        };
        ids.insert(key(k, kind)?, id);
        receipts.push(json!({"kind":kind,"key":k,"id":id.to_string(),"revision":receipt.revision.get(),"generation":receipt.generation.get()}));
    }
    let outcome_name = format!("{:?}", outcome.outcome());
    let disposal = std::time::Instant::now();
    drop(outcome);
    let disposal_ns = disposal.elapsed().as_nanos();
    Ok(
        json!({"disposal_ns":disposal_ns,"batch":row["batch"],"receipts":receipts,"request_ns":request_ns,"outcome":outcome_name,"counters":resource_counters(store)?,"ledger_before":ledger_before,"ledger_after":work_ledger(store)?}),
    )
}
pub fn ingest_fixture(
    root: &Path,
    path: &Path,
    state: FixtureState,
    output: &mut impl Write,
) -> Result<(), String> {
    graph_fixture::validate_fixture(root)?;
    let store = create(path)?;
    let mut ids = Ids::new();
    graph_fixture::visit_batches(root, FixtureState::A, &mut |row| {
        let observed = apply_record(&store, root, &row, &mut ids)?;
        writeln!(output, "{observed}").map_err(|e| e.to_string())
    })?;
    let report = store
        .maintain_cycle(&control())
        .map_err(|e| format!("fixture maintenance: {e}"))?;
    writeln!(
        output,
        "{}",
        json!({"maintenance":format!("{report:?}"),"generation":report.generation.get()})
    )
    .map_err(|e| e.to_string())?;
    if state == FixtureState::B {
        graph_fixture::visit_batches(root, state, &mut |row| {
            let observed = apply_record(&store, root, &row, &mut ids)?;
            writeln!(output, "{observed}").map_err(|e| e.to_string())?;
            if row["after"] == "checkpoint-and-consolidate" {
                let report = store
                    .maintain_cycle(&control())
                    .map_err(|e| format!("fixture maintenance: {e}"))?;
                writeln!(output,"{}",json!({"maintenance":format!("{report:?}"),"generation":report.generation.get()})).map_err(|e|e.to_string())?;
            }
            Ok(())
        })?;
    }
    if state == FixtureState::B {
        // Graceful close checkpoints the tail and would silently turn B into A.
        // Every completed batch is already durable. End this preparation-only
        // process after recording admission, leaving the final bounded WAL tail.
        let generation = cypher(&store, "MATCH (n) RETURN n LIMIT 0")?
            .metadata()
            .generation
            .get();
        writeln!(output, "{}", json!({"admitted_generation":generation,"state":"B","checkpoint_reopen":false,"uncheckpointed_tail_requested":true,"missing_input":"ZE-76 actual bounded WAL/index/adjacency tail capture"})).map_err(|e|e.to_string())?;
        output.flush().map_err(|e| e.to_string())?;
        std::process::exit(0);
    }
    store.close().map_err(|e| format!("fixture close: {e}"))?;
    drop(store);
    let reopened = open(path)?;
    let generation = cypher(&reopened, "MATCH (n) RETURN n LIMIT 0")?
        .metadata()
        .generation
        .get();
    writeln!(output,"{}",json!({"admitted_generation":generation,"state":if state==FixtureState::A{"A"}else{"B"},"checkpoint_reopen":true})).map_err(|e|e.to_string())?;
    reopened.close().map_err(|e| e.to_string())
}
pub fn scalar(v: &Value) -> Result<oracle::Property, String> {
    Ok(if let Some(a) = v["string_list"].as_array() {
        oracle::Property::List(
            oracle::Element::String,
            a.iter()
                .map(|x| {
                    x.as_str()
                        .map(|s| oracle::Scalar::String(s.into()))
                        .ok_or("string list".into())
                })
                .collect::<Result<_, String>>()?,
        )
    } else {
        oracle::Property::Scalar(if let Some(x) = v["string"].as_str() {
            oracle::Scalar::String(x.into())
        } else if let Some(x) = v["i64"].as_i64() {
            oracle::Scalar::I64(x)
        } else if let Some(x) = v["bool"].as_bool() {
            oracle::Scalar::Bool(x)
        } else {
            oracle::Scalar::F64(
                u64::from_str_radix(s(v, "f64_bits")?, 16).map_err(|e| e.to_string())?,
            )
        })
    })
}
/// Offline only: construct truth from primitive recipe plus observed full IDs.
/// Neither snapshot nor exhaustive oracle ever enters a timed worker.
pub fn primitive_snapshot(
    root: &Path,
    state: FixtureState,
    receipts: &Path,
) -> Result<(oracle::Snapshot, Ids), String> {
    graph_fixture::validate_fixture(root)?;
    let mut ids = Ids::new();
    let mut metadata = BTreeMap::new();
    for line in BufReader::new(File::open(receipts).map_err(|e| e.to_string())?).lines() {
        let v: Value =
            zeppelin_embed_bench::harness_json::from_str(&line.map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        if let Some(rows) = v["receipts"].as_array() {
            for r in rows {
                let k = key(&r["key"], s(r, "kind")?)?;
                let id = s(r, "id")?.parse::<u128>().map_err(|e| e.to_string())?;
                ids.insert(k.clone(), id);
                metadata.insert(k, (u(r, "revision")?, u(r, "generation")?));
            }
        }
    }
    let mut images = BTreeMap::new();
    let mut visit = |row: Value| {
        for c in row["changes"].as_array().ok_or("changes")? {
            let image = &c["image"];
            let k = if image.is_null() {
                key(&c["key"], "relationship")?
            } else {
                key(&image["key"], s(image, "kind")?)?
            };
            if image.is_null() {
                images.remove(&k);
            } else {
                images.insert(k, image.clone());
            }
        }
        Ok(())
    };
    graph_fixture::visit_batches(root, FixtureState::A, &mut visit)?;
    if state == FixtureState::B {
        graph_fixture::visit_batches(root, state, &mut visit)?;
    }
    let mut snapshot = oracle::Snapshot::default();
    for (k, image) in images {
        let id = *ids.get(&k).ok_or("missing observed full ID")?;
        let (revision, generation) = *metadata.get(&k).ok_or("missing receipt metadata")?;
        let properties = image["properties"]
            .as_object()
            .ok_or("properties")?
            .iter()
            .map(|(k, v)| Ok((k.clone(), scalar(v)?)))
            .collect::<Result<_, String>>()?;
        let app = Some(oracle::Key {
            kind: if k.0 == "node" {
                oracle::Kind::Node
            } else {
                oracle::Kind::Relationship
            },
            namespace: k.1,
            value: k.2,
        });
        if k.0 == "node" {
            snapshot.nodes.push(oracle::Node {
                id,
                key: app,
                revision,
                generation,
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
            snapshot.relationships.push(oracle::Relationship {
                id,
                key: app,
                revision,
                generation,
                source: *ids
                    .get(&key(&image["source"], "node")?)
                    .ok_or("source ID")?,
                target: *ids
                    .get(&key(&image["target"], "node")?)
                    .ok_or("target ID")?,
                relationship_type: s(&image, "type")?.into(),
                properties,
            });
        }
    }
    Ok((snapshot, ids))
}
pub fn query_cases(root: &Path) -> Result<Vec<Value>, String> {
    BufReader::new(File::open(root.join("queries.jsonl")).map_err(|e| e.to_string())?)
        .lines()
        .map(|line| {
            zeppelin_embed_bench::harness_json::from_str(&line.map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())
        })
        .collect()
}
pub fn oracle_request(
    root: &Path,
    case: &Value,
    ids: &Ids,
    name: &str,
    hops: u8,
) -> Result<oracle::Query, String> {
    let id = |k: &str| -> Result<u128, String> {
        ids.get(&key(&case[k], "node")?)
            .copied()
            .ok_or(format!("missing {k} ID"))
    };
    let terms = case["lexical"]["terms"]
        .as_array()
        .ok_or("terms")?
        .iter()
        .map(|v| v.as_str().map(str::to_owned).ok_or("term".into()))
        .collect::<Result<Vec<_>, String>>()?;
    let phrase = case["lexical"]["phrase"].as_bool().ok_or("phrase")?;
    Ok(match name {
        "project-evidence" => oracle::Query::ProjectEvidence {
            project: id("project")?,
            limit: 100,
        },
        "semantic-context" => oracle::Query::SemanticContext {
            vector: vector(root, &case["vector"])?,
            k: 20,
        },
        "alice-project-ranking" => oracle::Query::AliceProjectRanking {
            person: id("person")?,
            project: id("project")?,
            vector: vector(root, &case["vector"])?,
            k: 20,
        },
        "lexical-evidence" => oracle::Query::LexicalEvidence {
            terms,
            phrase,
            k: 20,
        },
        "hybrid-project-evidence" => oracle::Query::HybridProjectEvidence {
            project: id("project")?,
            vector: vector(root, &case["vector"])?,
            terms,
            phrase,
            k: 20,
            alpha: 0.5,
        },
        "bounded-evidence" => oracle::Query::BoundedEvidence {
            meeting: id("meeting")?,
            min: 1,
            max: hops,
        },
        _ => return Err("unknown named request".into()),
    })
}
pub fn encode_cell(cell: &oracle::Cell) -> Value {
    match cell {
        oracle::Cell::Null => json!({"null":true}),
        oracle::Cell::Bool(v) => json!({"bool":v}),
        oracle::Cell::I64(v) => json!({"i64":v}),
        oracle::Cell::String(v) => json!({"string":v}),
        oracle::Cell::Node(v) => json!({"node":v.to_string()}),
        oracle::Cell::Relationship(v) => json!({"relationship":v.to_string()}),
        oracle::Cell::Score(v) => json!({"f64_bits":format!("{v:016x}")}),
        oracle::Cell::List(v) => json!({"list":v.iter().map(encode_cell).collect::<Vec<_>>()}),
    }
}
pub fn observe(result: &CompletedGraphResult) -> Result<Vec<oracle::Row>, String> {
    fn value(r: &CompletedGraphResult, v: &Cell) -> Result<oracle::Cell, String> {
        let p = r.pools();
        Ok(match v {
            Cell::Null => oracle::Cell::Null,
            Cell::Bool(v) => oracle::Cell::Bool(*v),
            Cell::I64(v) => oracle::Cell::I64(*v),
            Cell::F64(v) => oracle::Cell::Score(*v),
            Cell::String(s) => oracle::Cell::String(r.string(*s).ok_or("string range")?.into()),
            Cell::Node(i) => {
                oracle::Cell::Node(p.nodes.get(*i as usize).ok_or("node pool")?.id.get())
            }
            Cell::Relationship(i) => oracle::Cell::Relationship(
                p.relationships
                    .get(*i as usize)
                    .ok_or("relationship pool")?
                    .id
                    .get(),
            ),
            Cell::List { children, .. } => {
                let range = children.start as usize..(children.start + children.len) as usize;
                oracle::Cell::List(
                    p.children
                        .get(range)
                        .ok_or("children range")?
                        .iter()
                        .map(|i| value(r, p.values.get(i.0 as usize).ok_or("value index")?))
                        .collect::<Result<_, String>>()?,
                )
            }
        })
    }
    (0..result.metadata().rows as usize)
        .map(|row| {
            (0..result.pools().columns.len())
                .map(|col| value(result, result.cell(row, col).ok_or("missing cell")?))
                .collect()
        })
        .collect()
}
pub fn cypher(store: &GraphStore, source: &str) -> Result<CompletedGraphResult, String> {
    execute(
        store.statement_store(),
        &control(),
        &GraphQueryOptions::default(),
        source,
        &[],
        CompileLimits::default(),
    )
    .map_err(|e| e.to_string())
}
/// Frozen jobs have the same selector, search mode and complete projections.
pub fn build_named_requests(request: &oracle::Query, exact: bool) -> String {
    let mode = if exact { "exact" } else { "auto" };
    let vector = |v: &[f32]| {
        v.iter()
            .map(|f| format!("{}", f64::from(*f)))
            .collect::<Vec<_>>()
            .join(",")
    };
    let lexical = |terms: &[String], phrase: bool| {
        if phrase {
            format!("\"{}\"", terms.join(" "))
        } else {
            terms.join(" ")
        }
    };
    let selector = |name: &str, id: u128| format!("ze.node_id({name}) = '{id:032x}'");
    match request {
        oracle::Query::ProjectEvidence { project, limit } => format!(
            "MATCH (item)-[:ABOUT]->(project:Project) WHERE {} MATCH (item)-[:SUPPORTED_BY]->(chunk) MATCH (meeting)-[:HAS_CHUNK]->(chunk) RETURN item,item.name,meeting,meeting.name,chunk,chunk.excerpt ORDER BY meeting.timestamp DESC,item,meeting,chunk LIMIT {limit}",
            selector("project", *project)
        ),
        oracle::Query::SemanticContext { vector: v, k } => format!(
            "CALL ze.vector_search([{}],{k},'{mode}') YIELD node AS chunk,distance MATCH (meeting)-[:HAS_CHUNK]->(chunk) MATCH (chunk)-[:MENTIONS]->(entity) RETURN chunk,distance,chunk.excerpt,meeting,meeting.name,entity,entity.name ORDER BY distance,chunk,meeting,entity",
            vector(v)
        ),
        oracle::Query::AliceProjectRanking {
            person,
            project,
            vector: v,
            k,
        } => format!(
            "MATCH (person:Person)-[:PARTICIPATED_IN]->(meeting)-[:FOR_PROJECT]->(project:Project) WHERE {} AND {} MATCH (meeting)-[:HAS_CHUNK]->(chunk) WITH collect(DISTINCT chunk) AS eligible CALL ze.vector_search([{}],{k},'{mode}',eligible) YIELD node AS chunk,distance MATCH (meeting)-[:HAS_CHUNK]->(chunk) RETURN chunk,distance,chunk.excerpt,meeting ORDER BY distance,chunk,meeting",
            selector("person", *person),
            selector("project", *project),
            vector(v)
        ),
        oracle::Query::LexicalEvidence { terms, phrase, k } => format!(
            "CALL ze.text_search('{}',{k}) YIELD node,score RETURN node,node.name,ze.stored_text(node) IS NOT NULL,ze.stored_text(node),score ORDER BY score DESC,node",
            lexical(terms, *phrase)
        ),
        oracle::Query::HybridProjectEvidence {
            project,
            vector: v,
            terms,
            phrase,
            k,
            alpha: _,
        } => format!(
            "MATCH (project:Project)<-[:FOR_PROJECT]-(meeting) WHERE {} MATCH (meeting)-[:HAS_CHUNK|HAS_ITEM*0..1]->(candidate) WITH collect(DISTINCT candidate) AS eligible CALL ze.hybrid_search([{}],'{}',{k},'{mode}',eligible) YIELD node,score,vector_distance,lexical_score RETURN node,node.name,score,vector_distance,lexical_score,vector_distance IS NOT NULL,lexical_score IS NOT NULL,ze.stored_text(node) ORDER BY score DESC,node",
            selector("project", *project),
            vector(v),
            lexical(terms, *phrase)
        ),
        oracle::Query::BoundedEvidence { meeting, min, max } => format!(
            "MATCH (meeting:Meeting) WHERE {} MATCH (meeting)-[relationships:HAS_CHUNK|MENTIONS*{min}..{max}]->(endpoint) RETURN endpoint,relationships ORDER BY endpoint,relationships",
            selector("meeting", *meeting)
        ),
    }
}

/// Build structured data directly; no compiler/prepared-plan interface involved.
pub fn structured(
    store: &GraphStore,
    request: &oracle::Query,
    exact: bool,
) -> Result<CompletedGraphResult, String> {
    with_plan(request, exact, |plan| {
        store
            .query(&control(), &GraphQueryOptions::default(), plan)
            .map_err(|e| e.to_string())
    })
}
pub fn with_plan<T>(
    request: &oracle::Query,
    exact: bool,
    call: impl FnOnce(&GraphQueryPlan<'_>) -> Result<T, String>,
) -> Result<T, String> {
    use zeppelin_embed::property_graph::query::plan::*;
    let names: Vec<String> = [
        "ABOUT",
        "SUPPORTED_BY",
        "HAS_CHUNK",
        "MENTIONS",
        "PARTICIPATED_IN",
        "FOR_PROJECT",
        "HAS_ITEM",
        "name",
        "excerpt",
        "timestamp",
    ]
    .iter()
    .map(|s| (*s).into())
    .collect();
    let n = |i: usize| GraphName::new(&names[i]).map_err(|e| e.to_string());
    let types: Vec<Vec<_>> = (0..7)
        .map(|i| Ok(vec![n(i)?]))
        .collect::<Result<_, String>>()?;
    let bounded = vec![n(2)?, n(3)?];
    let hybrid_types = vec![n(2)?, n(6)?];
    let expand = |source, node, relationship, t: usize, direction, pattern| OperatorKind::Expand {
        source: SlotId(source),
        node: SlotId(node),
        relationship: SlotId(relationship),
        relationship_types: &types[t],
        direction,
        pattern: PatternId(pattern),
    };
    let mut expressions = Vec::new();
    macro_rules! expr {
        ($e:expr) => {{
            let id = ExprId(expressions.len() as u32);
            expressions.push($e);
            id
        }};
    }
    let vector = match request {
        oracle::Query::SemanticContext { vector, .. }
        | oracle::Query::AliceProjectRanking { vector, .. }
        | oracle::Query::HybridProjectEvidence { vector, .. } => vector.clone(),
        _ => vec![],
    };
    let coordinates: Vec<_> = vector
        .iter()
        .map(|v| expr!(Expression::Literal(Literal::F64(f64::from(*v)))))
        .collect();
    let vector_expr = if coordinates.is_empty() {
        ExprId(0)
    } else {
        expr!(Expression::List(&coordinates))
    };
    let k_expr = if matches!(
        request,
        oracle::Query::ProjectEvidence { .. } | oracle::Query::BoundedEvidence { .. }
    ) {
        ExprId(0)
    } else {
        expr!(Expression::Literal(Literal::I64(20)))
    };
    let text = match request {
        oracle::Query::LexicalEvidence { terms, phrase, .. }
        | oracle::Query::HybridProjectEvidence { terms, phrase, .. } => {
            if *phrase {
                format!("\"{}\"", terms.join(" "))
            } else {
                terms.join(" ")
            }
        }
        _ => String::new(),
    };
    let text_expr = if matches!(
        request,
        oracle::Query::LexicalEvidence { .. } | oracle::Query::HybridProjectEvidence { .. }
    ) {
        expr!(Expression::Literal(Literal::String(&text)))
    } else {
        ExprId(0)
    };
    let mut kinds = vec![OperatorKind::Unit];
    let mut aggregates = Vec::new();
    let mut eager = Vec::new();
    let mut eligible = None;
    match request {
        oracle::Query::ProjectEvidence { project, .. } => {
            kinds.push(OperatorKind::LookupNode {
                output: SlotId(8),
                id: NodeId::new(*project).map_err(|e| e.to_string())?,
            });
            kinds.push(expand(8, 0, 9, 0, Direction::Incoming, 0));
            kinds.push(expand(0, 2, 10, 1, Direction::Outgoing, 1));
            kinds.push(expand(2, 1, 11, 2, Direction::Incoming, 2));
        }
        oracle::Query::AliceProjectRanking { person, .. } => {
            kinds.push(OperatorKind::LookupNode {
                output: SlotId(8),
                id: NodeId::new(*person).map_err(|e| e.to_string())?,
            });
            kinds.push(expand(8, 9, 10, 4, Direction::Outgoing, 0));
            kinds.push(expand(9, 11, 12, 5, Direction::Outgoing, 1));
            kinds.push(expand(9, 13, 14, 2, Direction::Outgoing, 2));
        }
        oracle::Query::HybridProjectEvidence { project, .. } => {
            kinds.push(OperatorKind::LookupNode {
                output: SlotId(11),
                id: NodeId::new(*project).map_err(|e| e.to_string())?,
            });
            kinds.push(expand(11, 9, 12, 5, Direction::Incoming, 0));
            kinds.push(OperatorKind::BoundedExpand {
                source: SlotId(9),
                node: SlotId(13),
                relationships: SlotId(14),
                min: 0,
                max: 1,
                direction: Direction::Outgoing,
                relationship_types: &hybrid_types,
                pattern: PatternId(1),
                edge_predicate: None,
                completed_edge_predicate: None,
            });
        }
        oracle::Query::BoundedEvidence { meeting, min, max } => {
            kinds.push(OperatorKind::LookupNode {
                output: SlotId(8),
                id: NodeId::new(*meeting).map_err(|e| e.to_string())?,
            });
            kinds.push(OperatorKind::BoundedExpand {
                source: SlotId(8),
                node: SlotId(2),
                relationships: SlotId(3),
                min: *min,
                max: *max,
                direction: Direction::Outgoing,
                relationship_types: &bounded,
                pattern: PatternId(0),
                edge_predicate: None,
                completed_edge_predicate: None,
            });
        }
        _ => {}
    }
    let project_selector = match request {
        oracle::Query::AliceProjectRanking { project, .. } => format!("{project:032x}"),
        _ => String::new(),
    };
    if matches!(request, oracle::Query::AliceProjectRanking { .. }) {
        let node = expr!(Expression::Slot(SlotId(11)));
        let actual = expr!(Expression::Unary {
            operation: UnaryExpression::NodeIdText,
            operand: node
        });
        let expected = expr!(Expression::Literal(Literal::String(&project_selector)));
        let predicate = expr!(Expression::Binary {
            operation: BinaryExpression::Comparison(
                zeppelin_embed::property_graph::query::Comparison::Equal
            ),
            left: actual,
            right: expected
        });
        kinds.push(OperatorKind::Filter(predicate));
    }
    if matches!(
        request,
        oracle::Query::AliceProjectRanking { .. } | oracle::Query::HybridProjectEvidence { .. }
    ) {
        let value = expr!(Expression::Slot(SlotId(13)));
        let aggregate = expr!(Expression::Aggregate {
            operation: AggregateExpression::Collect { distinct: true },
            operand: Some(value)
        });
        aggregates.push(Projection {
            slot: SlotId(15),
            expression: aggregate,
        });
        kinds.push(OperatorKind::Aggregate {
            keys: &[],
            aggregates: &aggregates,
        });
        eligible = Some(expr!(Expression::Slot(SlotId(15))));
    }
    if !matches!(
        request,
        oracle::Query::ProjectEvidence { .. } | oracle::Query::BoundedEvidence { .. }
    ) {
        let mode = if exact {
            SearchMode::Exact
        } else {
            SearchMode::Auto
        };
        let options = SearchOptions {
            alpha: if let oracle::Query::HybridProjectEvidence { alpha, .. } = request {
                Some(*alpha)
            } else {
                None
            },
            ..Default::default()
        };
        let (search, outputs) = match request {
            oracle::Query::LexicalEvidence { .. } => (
                SearchRequest::Text {
                    query: text_expr,
                    k: k_expr,
                    eligible: None,
                    options,
                },
                SearchOutputs {
                    node: Some(SlotId(2)),
                    score: Some(SlotId(3)),
                    ..Default::default()
                },
            ),
            oracle::Query::HybridProjectEvidence { .. } => (
                SearchRequest::Hybrid {
                    vector: vector_expr,
                    text: text_expr,
                    k: k_expr,
                    mode,
                    eligible,
                    options,
                },
                SearchOutputs {
                    node: Some(SlotId(2)),
                    score: Some(SlotId(3)),
                    vector_distance: Some(SlotId(4)),
                    lexical_score: Some(SlotId(5)),
                    ..Default::default()
                },
            ),
            _ => (
                SearchRequest::Vector {
                    vector: vector_expr,
                    k: k_expr,
                    mode,
                    eligible,
                    options,
                },
                SearchOutputs {
                    node: Some(SlotId(2)),
                    distance: Some(SlotId(3)),
                    ..Default::default()
                },
            ),
        };
        eager.push(PlanNodeId(kinds.len() as u32));
        kinds.push(OperatorKind::Search {
            call: SearchCallId(0),
            request: search,
            outputs,
        });
        if matches!(
            request,
            oracle::Query::SemanticContext { .. } | oracle::Query::AliceProjectRanking { .. }
        ) {
            kinds.push(expand(2, 1, 10, 2, Direction::Incoming, 3));
            if matches!(request, oracle::Query::SemanticContext { .. }) {
                kinds.push(expand(2, 4, 11, 3, Direction::Outgoing, 4));
            }
        }
    }
    let slot = |expressions: &mut Vec<Expression<'_>>, s| {
        let id = ExprId(expressions.len() as u32);
        expressions.push(Expression::Slot(SlotId(s)));
        id
    };
    let mut fields = Vec::new();
    let specs: Vec<(u32, Option<usize>)> = match request {
        oracle::Query::ProjectEvidence { .. } => vec![
            (0, None),
            (0, Some(7)),
            (1, None),
            (1, Some(7)),
            (2, None),
            (2, Some(8)),
        ],
        oracle::Query::SemanticContext { .. } => vec![
            (2, None),
            (3, None),
            (2, Some(8)),
            (1, None),
            (1, Some(7)),
            (4, None),
            (4, Some(7)),
        ],
        oracle::Query::AliceProjectRanking { .. } => {
            vec![(2, None), (3, None), (2, Some(8)), (1, None)]
        }
        oracle::Query::LexicalEvidence { .. } => vec![(2, None), (2, Some(7))],
        oracle::Query::HybridProjectEvidence { .. } => {
            vec![(2, None), (2, Some(7)), (3, None), (4, None), (5, None)]
        }
        oracle::Query::BoundedEvidence { .. } => vec![(2, None), (3, None)],
    };
    for (s, property) in specs {
        let mut e = slot(&mut expressions, s);
        if let Some(p) = property {
            e = expr!(Expression::Property {
                entity: e,
                name: n(p)?
            });
        }
        fields.push(e);
    }
    if matches!(request, oracle::Query::LexicalEvidence { .. }) {
        let node = slot(&mut expressions, 2);
        let text = expr!(Expression::Unary {
            operation: UnaryExpression::StoredText,
            operand: node
        });
        let present = expr!(Expression::Unary {
            operation: UnaryExpression::IsNotNull,
            operand: text
        });
        fields.extend([present, text, slot(&mut expressions, 3)]);
    }
    if matches!(request, oracle::Query::HybridProjectEvidence { .. }) {
        for s in [4, 5] {
            let value = slot(&mut expressions, s);
            fields.push(expr!(Expression::Unary {
                operation: UnaryExpression::IsNotNull,
                operand: value
            }));
        }
        let node = slot(&mut expressions, 2);
        fields.push(expr!(Expression::Unary {
            operation: UnaryExpression::StoredText,
            operand: node
        }));
    }
    let mut sort = Vec::new();
    if matches!(request, oracle::Query::ProjectEvidence { .. }) {
        let meeting = slot(&mut expressions, 1);
        sort.push(SortKey {
            expression: expr!(Expression::Property {
                entity: meeting,
                name: n(9)?
            }),
            descending: true,
        });
    }
    let slots: Vec<_> = match request {
        oracle::Query::ProjectEvidence { .. } => vec![0, 1, 2],
        oracle::Query::SemanticContext { .. } => vec![3, 2, 1, 4],
        oracle::Query::AliceProjectRanking { .. } => vec![3, 2, 1],
        oracle::Query::LexicalEvidence { .. } | oracle::Query::HybridProjectEvidence { .. } => {
            vec![3, 2]
        }
        oracle::Query::BoundedEvidence { .. } => vec![2, 3],
    };
    for (i, s) in slots.iter().enumerate() {
        sort.push(SortKey {
            expression: slot(&mut expressions, *s),
            descending: i == 0
                && matches!(
                    request,
                    oracle::Query::LexicalEvidence { .. }
                        | oracle::Query::HybridProjectEvidence { .. }
                ),
        });
    }
    kinds.push(OperatorKind::Sort(&sort));
    if let oracle::Query::ProjectEvidence { limit, .. } = request {
        kinds.push(OperatorKind::OffsetLimit {
            offset: 0,
            limit: Some(*limit as u64),
        });
    }
    let projections: Vec<_> = fields
        .iter()
        .enumerate()
        .map(|(i, e)| Projection {
            slot: SlotId(20 + i as u32),
            expression: *e,
        })
        .collect();
    kinds.push(OperatorKind::Project(&projections));
    let inputs: Vec<Vec<_>> = (0..kinds.len())
        .map(|i| {
            if i == 0 {
                vec![]
            } else {
                vec![PlanNodeId(i as u32 - 1)]
            }
        })
        .collect();
    let operators: Vec<_> = kinds
        .into_iter()
        .enumerate()
        .map(|(i, kind)| Operator {
            inputs: &inputs[i],
            kind,
        })
        .collect();
    let mut backing = GraphPlanBacking::default();
    for name in &names {
        backing.string(name).map_err(|e| e.to_string())?;
    }
    for t in &types {
        backing.vec(t).map_err(|e| e.to_string())?;
    }
    for input in &inputs {
        backing.vec(input).map_err(|e| e.to_string())?;
    }
    backing.string(&text).map_err(|e| e.to_string())?;
    backing
        .string(&project_selector)
        .map_err(|e| e.to_string())?;
    backing.vec(&bounded).map_err(|e| e.to_string())?;
    backing.vec(&hybrid_types).map_err(|e| e.to_string())?;
    backing.vec(&coordinates).map_err(|e| e.to_string())?;
    backing.vec(&aggregates).map_err(|e| e.to_string())?;
    backing.vec(&sort).map_err(|e| e.to_string())?;
    backing.vec(&projections).map_err(|e| e.to_string())?;
    let columns: Vec<_> = (0..fields.len()).map(|_| "value").collect();
    call(&GraphQueryPlan {
        operators: &operators,
        expressions: &expressions,
        parameters: &vec![],
        eager_searches: &eager,
        root: PlanNodeId(operators.len() as u32 - 1),
        backing: &backing,
        bindings: &[],
        columns: &columns,
    })
}
pub fn request_record(q: &oracle::Query) -> Value {
    match q {
        oracle::Query::ProjectEvidence { project, limit } => {
            json!({"name":"project-evidence","project":project.to_string(),"limit":limit})
        }
        oracle::Query::SemanticContext { vector, k } => {
            json!({"name":"semantic-context","vector":vector,"k":k})
        }
        oracle::Query::AliceProjectRanking {
            person,
            project,
            vector,
            k,
        } => {
            json!({"name":"alice-project-ranking","person":person.to_string(),"project":project.to_string(),"vector":vector,"k":k})
        }
        oracle::Query::LexicalEvidence { terms, phrase, k } => {
            json!({"name":"lexical-evidence","terms":terms,"phrase":phrase,"k":k})
        }
        oracle::Query::HybridProjectEvidence {
            project,
            vector,
            terms,
            phrase,
            k,
            alpha,
        } => {
            json!({"name":"hybrid-project-evidence","project":project.to_string(),"vector":vector,"terms":terms,"phrase":phrase,"k":k,"alpha":alpha})
        }
        oracle::Query::BoundedEvidence { meeting, min, max } => {
            json!({"name":"bounded-evidence","meeting":meeting.to_string(),"min":min,"max":max})
        }
    }
}
pub fn decode_request(v: &Value) -> Result<oracle::Query, String> {
    let id = |k| s(v, k)?.parse::<u128>().map_err(|e| e.to_string());
    let vector = || {
        v["vector"]
            .as_array()
            .ok_or("missing vector")?
            .iter()
            .map(|f| {
                f.as_f64()
                    .filter(|x| x.is_finite())
                    .map(|x| x as f32)
                    .ok_or("invalid vector".into())
            })
            .collect::<Result<Vec<_>, String>>()
    };
    let terms = || {
        v["terms"]
            .as_array()
            .ok_or("terms")?
            .iter()
            .map(|f| f.as_str().map(str::to_owned).ok_or("term".into()))
            .collect::<Result<Vec<_>, String>>()
    };
    let phrase = || v["phrase"].as_bool().ok_or("phrase".to_owned());
    let k = || u(v, "k").map(|k| k as usize);
    Ok(match s(v, "name")? {
        "project-evidence" => oracle::Query::ProjectEvidence {
            project: id("project")?,
            limit: u(v, "limit")? as usize,
        },
        "semantic-context" => oracle::Query::SemanticContext {
            vector: vector()?,
            k: k()?,
        },
        "alice-project-ranking" => oracle::Query::AliceProjectRanking {
            person: id("person")?,
            project: id("project")?,
            vector: vector()?,
            k: k()?,
        },
        "lexical-evidence" => oracle::Query::LexicalEvidence {
            terms: terms()?,
            phrase: phrase()?,
            k: k()?,
        },
        "hybrid-project-evidence" => oracle::Query::HybridProjectEvidence {
            project: id("project")?,
            vector: vector()?,
            terms: terms()?,
            phrase: phrase()?,
            k: k()?,
            alpha: v["alpha"].as_f64().ok_or("alpha")?,
        },
        "bounded-evidence" => oracle::Query::BoundedEvidence {
            meeting: id("meeting")?,
            min: u(v, "min")? as u8,
            max: u(v, "max")? as u8,
        },
        _ => return Err("unknown request".into()),
    })
}
pub fn resource_counters(store: &GraphStore) -> Result<Value, String> {
    let r = store
        .resources()
        .map_err(|e| e.to_string())?
        .snapshot()
        .map_err(|e| e.to_string())?;
    Ok(
        json!({"engine_bytes":r.engine_bytes,"engine_peak_bytes":r.engine_peak_bytes,
        "application_bytes":r.application_bytes,"application_peak_bytes":r.application_peak_bytes}),
    )
}
pub fn work_ledger(store: &GraphStore) -> Result<Value, String> {
    let w = store
        .resources()
        .map_err(|e| e.to_string())?
        .work_ledger()
        .map_err(|e| e.to_string())?;
    Ok(
        json!({"storage_lookups":w.storage_lookups,"storage_scans":w.storage_scans,"storage_adjacency_entries":w.storage_adjacency_entries,"storage_copied_bytes":w.storage_copied_bytes,"storage_pages_decoded":w.storage_pages_decoded,"storage_pages_copied":w.storage_pages_copied,"storage_property_values":w.storage_property_values,"storage_property_bytes":w.storage_property_bytes,"storage_adjacency_physical_entries":w.storage_adjacency_physical_entries,"storage_adjacency_merged_visits":w.storage_adjacency_merged_visits,"storage_adjacency_merge_runs":w.storage_adjacency_merge_runs,"canonical_comparison_bytes":w.canonical_comparison_bytes,"canonical_encoding_bytes":w.canonical_encoding_bytes,"wal_codec_units":w.wal_codec_units,"encoded_wal_bytes":w.encoded_wal_bytes,"artifact_bytes_written":w.artifact_bytes_written,"artifact_writes":w.artifact_writes,"wal_bytes_appended":w.wal_bytes_appended,"wal_appends":w.wal_appends,"full_sync_attempts":w.full_sync_attempts,"full_sync_successes":w.full_sync_successes,"directory_sync_attempts":w.directory_sync_attempts,"directory_sync_successes":w.directory_sync_successes}),
    )
}
pub fn observed_counters(store: &GraphStore, r: &CompletedGraphResult) -> Result<Value, String> {
    use zeppelin_embed::property_graph::query::runtime::WorkKind;
    let mut c = resource_counters(store)?;
    c["query_reservation_peak_bytes"] = json!(r.metadata().peak_query_bytes);
    for (name, kind) in [
        ("completed_rows", WorkKind::CompletedRows),
        ("operator_rows", WorkKind::OperatorRows),
        ("adjacency_entries", WorkKind::AdjacencyEntries),
        ("expressions", WorkKind::Expressions),
        ("hash_probes", WorkKind::HashProbes),
        ("result_bytes", WorkKind::CompletedBytes),
        ("prepared_payload_bytes", WorkKind::PreparedPayloadBytes),
        ("completed_abi_bytes", WorkKind::CompletedAbiBytes),
        ("vector_coordinates", WorkKind::VectorCoordinates),
        ("vector_payload_bytes", WorkKind::VectorBytes),
        ("postings", WorkKind::LexicalPostings),
        ("lexical_blocks", WorkKind::LexicalBlocks),
        ("search_calls", WorkKind::SearchInvocations),
        ("directory_lookups", WorkKind::Lookups),
        ("scans", WorkKind::Scans),
        ("paths", WorkKind::Paths),
        ("rows_in", WorkKind::RowsIn),
        ("rows_out", WorkKind::RowsOut),
        ("join_probes", WorkKind::JoinProbes),
        ("group_keys", WorkKind::GroupKeys),
        ("eligibility_entries", WorkKind::EligibilityEntries),
        ("copied_bytes", WorkKind::CopiedBytes),
        ("directory_pages_decoded", WorkKind::DirectoryPagesDecoded),
        ("directory_pages_copied", WorkKind::DirectoryPagesCopied),
        ("property_values", WorkKind::PropertyValues),
        ("property_bytes", WorkKind::PropertyBytes),
        (
            "adjacency_physical_entries",
            WorkKind::AdjacencyPhysicalEntries,
        ),
        ("adjacency_merged_visits", WorkKind::AdjacencyMergedVisits),
        ("adjacency_merge_runs", WorkKind::AdjacencyMergeRuns),
        (
            "eligibility_unique_entries",
            WorkKind::EligibilityUniqueEntries,
        ),
        ("candidate_window_peak", WorkKind::CandidateWindowPeak),
    ] {
        c[name] = json!(r.metadata().counters.get(kind));
    }
    Ok(c)
}
/// No corpus/oracle/model in this timed process: input is a bounded request file.
pub fn run_cell(
    path: &Path,
    job: &Path,
    frontend: &str,
    exact: bool,
    warmups: usize,
    samples: usize,
) -> Result<(), String> {
    if samples == 0 || samples > 10000 || warmups > 1000 {
        return Err("invalid sample counts".into());
    }
    let value: Value = zeppelin_embed_bench::harness_json::from_slice(
        &std::fs::read(job).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let request = decode_request(&value["request"])?;
    let store = open(path)?;
    let interval = store
        .resources()
        .map_err(|e| e.to_string())?
        .begin_allocation_interval()
        .map_err(|e| format!("reservation interval: {e:?}"))?;
    let mut failure = None;
    for i in 0..warmups + samples {
        let (result, elapsed) = timed_request(&store, &request, frontend, exact)?;
        match result {
            Ok(result) => {
                let generation = result.metadata().generation.get();
                let counters = observed_counters(&store, &result)?;
                let rows = observe(&result)?;
                let reports = result
                    .pools()
                    .reports
                    .iter()
                    .map(|r| format!("{r:?}"))
                    .collect::<Vec<_>>();
                let disposal = std::time::Instant::now();
                drop(result);
                let disposal = disposal.elapsed().as_nanos();
                if i >= warmups {
                    println!(
                        "{}",
                        json!({"sample":i-warmups,"case":value["case"],"name":value["name"],"elapsed_ns":elapsed,"disposal_ns":disposal,"generation":generation,"status":0,"rows":rows.iter().map(|r|r.iter().map(encode_cell).collect::<Vec<_>>()).collect::<Vec<_>>(),"counters":counters,"reports_raw":reports})
                    );
                }
            }
            Err(error) => {
                println!(
                    "{}",
                    json!({"sample":i.saturating_sub(warmups),"warmup":i<warmups,"elapsed_ns":elapsed,"error":error,"status":1})
                );
                failure = Some(error);
                break;
            }
        }
    }
    let observed = interval
        .finish()
        .map_err(|e| format!("interval finish {e:?}"))?;
    eprintln!(
        "ZE-77 shared reservation interval (engine lifetime capacity peak reported separately): {observed:?}"
    );
    store.close().map_err(|e| e.to_string())?;
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(())
}

pub fn decode_cell(v: &Value) -> Result<oracle::Cell, String> {
    let object = v.as_object().ok_or("typed cell")?;
    if object.len() != 1 {
        return Err("invalid typed cell".into());
    }
    let (tag, value) = object.iter().next().ok_or("cell")?;
    Ok(match tag.as_str() {
        "null" => {
            if value != true {
                return Err("invalid null".into());
            }
            oracle::Cell::Null
        }
        "bool" => oracle::Cell::Bool(value.as_bool().ok_or("bool")?),
        "i64" => oracle::Cell::I64(value.as_i64().ok_or("i64")?),
        "string" => oracle::Cell::String(value.as_str().ok_or("string")?.into()),
        "node" => oracle::Cell::Node(
            value
                .as_str()
                .ok_or("node ID")?
                .parse::<u128>()
                .map_err(|e| e.to_string())?,
        ),
        "relationship" => oracle::Cell::Relationship(
            value
                .as_str()
                .ok_or("relationship ID")?
                .parse::<u128>()
                .map_err(|e| e.to_string())?,
        ),
        "f64_bits" => oracle::Cell::Score(
            u64::from_str_radix(value.as_str().ok_or("score bits")?, 16)
                .map_err(|e| e.to_string())?,
        ),
        "list" => oracle::Cell::List(
            value
                .as_array()
                .ok_or("list")?
                .iter()
                .map(decode_cell)
                .collect::<Result<_, _>>()?,
        ),
        _ => return Err("unknown typed cell".into()),
    })
}

/// Identical public-entry timing seam for owned structured and compiler calls.
pub fn timed_request(
    store: &GraphStore,
    request: &oracle::Query,
    frontend: &str,
    exact: bool,
) -> Result<(Result<CompletedGraphResult, String>, u128), String> {
    let request_control = control();
    let options = GraphQueryOptions::default();
    match frontend {
        "structured" => with_plan(request, exact, |plan| {
            let start = std::time::Instant::now();
            let result = store
                .query(&request_control, &options, plan)
                .map_err(|e| e.to_string());
            Ok((result, start.elapsed().as_nanos()))
        }),
        "cypher" => {
            let source = build_named_requests(request, exact);
            let start = std::time::Instant::now();
            let result = execute(
                store.statement_store(),
                &request_control,
                &options,
                &source,
                &[],
                CompileLimits::default(),
            )
            .map_err(|e| e.to_string());
            Ok((result, start.elapsed().as_nanos()))
        }
        _ => Err("unknown frontend".into()),
    }
}
