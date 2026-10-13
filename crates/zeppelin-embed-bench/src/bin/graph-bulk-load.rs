//! Measure full durable graph-apply batches for an LDBC-like workload.

use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{Value, json};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::property_graph::resources::MAX_GRAPH_RESIDENT_BYTES;
use zeppelin_embed::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use zeppelin_embed::property_graph::{
    ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphName, GraphProperty,
    GraphRevision, NodeId, NodeRef, PropertyData, PropertyValue,
};

const DEFAULT_NODES: usize = 1_000_000;
const DEFAULT_EDGES_PER_NODE: usize = 2;
const LDBC_NAMESPACE: &str = "ldbc-snb";
const OPERATION_FRAMING_UPPER_BOUND: usize = 128;
const ENDPOINT_SEED: u64 = 0x534e_425f_5346_315f;

const MAX_BATCH_ITEMS: usize = zeppelin_embed::property_graph::MAX_GRAPH_CHANGES;
const MAX_BATCH_BYTES: usize = zeppelin_embed::property_graph::MAX_GRAPH_INPUT_BYTES;

type BenchResult<T> = Result<T, Box<dyn Error>>;

struct Config {
    store: PathBuf,
    nodes: usize,
    edges_per_node: usize,
    max_batch_items: usize,
    out: PathBuf,
}

#[derive(Clone, Copy)]
struct BatchMeasurement {
    phase: &'static str,
    batch_index: usize,
    items: usize,
    wall_ms: f64,
    store_nodes_before: usize,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("graph-bulk-load: {error}");
        std::process::exit(1);
    }
}

fn run() -> BenchResult<()> {
    let config = Config::parse()?;
    if config.store.try_exists()? {
        return Err(invalid("--store directory must not exist"));
    }
    let relationship_count = config
        .nodes
        .checked_mul(config.edges_per_node)
        .ok_or_else(|| invalid("node and edge counts overflow usize"))?;
    i64::try_from(config.nodes).map_err(|_| invalid("--nodes exceeds i64 property ids"))?;

    let options = OpenOptions::new().with_max_resident_bytes(MAX_GRAPH_RESIDENT_BYTES);
    let store = Store::create_graph(&config.store, options, None)?;
    let node_item_bytes = estimate_node_item_bytes()?;
    let mut node_batches = Vec::new();
    let mut node_ids = Vec::with_capacity(config.nodes);
    let mut node_start = 0_usize;
    while node_start < config.nodes {
        let items = batch_item_count(
            config.nodes - node_start,
            node_item_bytes,
            config.max_batch_items,
        )
        .ok_or_else(|| invalid("one node item exceeds the graph batch byte limit"))?;
        let batch_index = node_batches.len();
        let (mut ids, measurement) = apply_node_batch(&store, node_start, items, batch_index)?;
        node_ids.append(&mut ids);
        node_start = node_start
            .checked_add(items)
            .ok_or_else(|| invalid("node batch offset overflow"))?;
        node_batches.push(measurement);
        if node_batches.len() % 10 == 0 {
            print_progress(&node_batches);
        }
    }

    let relationship_item_bytes = estimate_relationship_item_bytes()?;
    let mut relationship_batches = Vec::new();
    let mut relationship_start = 0_usize;
    let mut endpoints = SplitMix64::new(ENDPOINT_SEED);
    while relationship_start < relationship_count {
        let items = batch_item_count(
            relationship_count - relationship_start,
            relationship_item_bytes,
            config.max_batch_items,
        )
        .ok_or_else(|| invalid("one relationship item exceeds the graph batch byte limit"))?;
        let batch_index = relationship_batches.len();
        let measurement = apply_relationship_batch(
            &store,
            &node_ids,
            relationship_start,
            items,
            batch_index,
            &mut endpoints,
        )?;
        relationship_start = relationship_start
            .checked_add(items)
            .ok_or_else(|| invalid("relationship batch offset overflow"))?;
        relationship_batches.push(measurement);
        if relationship_batches.len() % 10 == 0 {
            print_progress(&relationship_batches);
        }
    }

    store.close_graph()?;
    drop(store);
    let final_store_bytes = directory_bytes(&config.store)?;
    let peak_rss_bytes = peak_rss_bytes()?;
    let host = host_info()?;
    print_phase_summary(&node_batches);
    print_phase_summary(&relationship_batches);

    let batches = node_batches
        .iter()
        .chain(&relationship_batches)
        .map(batch_json)
        .collect::<Vec<_>>();
    let report = json!({
        "host": host,
        "config": {
            "nodes": config.nodes,
            "edges_per_node": config.edges_per_node,
            "relationships": relationship_count,
        },
        "batches": batches,
        "summaries": [
            phase_summary("nodes", &node_batches),
            phase_summary("relationships", &relationship_batches),
        ],
        "peak_rss_bytes": peak_rss_bytes,
        "final_store_bytes": final_store_bytes,
    });
    fs::write(&config.out, serde_json::to_vec_pretty(&report)?)?;
    Ok(())
}

impl Config {
    fn parse() -> BenchResult<Self> {
        let mut store = None;
        let mut nodes = DEFAULT_NODES;
        let mut edges_per_node = DEFAULT_EDGES_PER_NODE;
        let mut max_batch_items = MAX_BATCH_ITEMS;
        let mut out = None;
        let mut arguments = std::env::args().skip(1);
        while let Some(flag) = arguments.next() {
            let value = arguments
                .next()
                .ok_or_else(|| invalid(&format!("{flag} needs a value")))?;
            match flag.as_str() {
                "--store" if store.is_none() => store = Some(PathBuf::from(value)),
                "--nodes" => {
                    nodes = parse_usize(&value, "--nodes")?;
                }
                "--edges-per-node" => {
                    edges_per_node = parse_usize(&value, "--edges-per-node")?;
                }
                "--max-batch-items" => {
                    max_batch_items = parse_usize(&value, "--max-batch-items")?;
                    if max_batch_items == 0 || max_batch_items > MAX_BATCH_ITEMS {
                        return Err(invalid("--max-batch-items must be 1..=MAX_GRAPH_CHANGES"));
                    }
                }
                "--out" if out.is_none() => out = Some(PathBuf::from(value)),
                "--store" | "--out" => return Err(invalid("duplicate path argument")),
                _ => return Err(invalid(&format!("unknown argument {flag}"))),
            }
        }
        Ok(Self {
            store: store.ok_or_else(|| invalid("--store is required"))?,
            nodes,
            edges_per_node,
            max_batch_items,
            out: out.ok_or_else(|| invalid("--out is required"))?,
        })
    }
}

fn parse_usize(value: &str, flag: &str) -> BenchResult<usize> {
    value
        .parse()
        .map_err(|error| invalid(&format!("invalid {flag} value {value:?}: {error}")))
}

fn apply_node_batch(
    store: &Store,
    start: usize,
    items: usize,
    batch_index: usize,
) -> BenchResult<(Vec<NodeId>, BatchMeasurement)> {
    let end = start
        .checked_add(items)
        .ok_or_else(|| invalid("node batch range overflow"))?;
    let first_names = (start..end)
        .map(|id| format!("FirstName{:09}", id % 1_000_000_000))
        .collect::<Vec<_>>();
    let last_names = (start..end)
        .map(|id| format!("LastName{:08}", id % 100_000_000))
        .collect::<Vec<_>>();
    let person = GraphName::new("Person")?;
    let id_name = GraphName::new("id")?;
    let first_name = GraphName::new("firstName")?;
    let last_name = GraphName::new("lastName")?;
    let birthday_name = GraphName::new("birthday")?;
    let mut labels = vec![[person]; items];
    let mut properties = Vec::with_capacity(items);
    for (offset, (first, last)) in first_names.iter().zip(&last_names).enumerate() {
        let id = start
            .checked_add(offset)
            .ok_or_else(|| invalid("node id overflow"))?;
        properties.push([
            GraphProperty::new(
                id_name,
                PropertyValue::new(PropertyData::I64(i64::try_from(id)?))?,
            ),
            GraphProperty::new(
                first_name,
                PropertyValue::new(PropertyData::String(first.as_str()))?,
            ),
            GraphProperty::new(
                last_name,
                PropertyValue::new(PropertyData::String(last.as_str()))?,
            ),
            GraphProperty::new(
                birthday_name,
                PropertyValue::new(PropertyData::I64(i64::try_from(id % 36_500)?))?,
            ),
        ]);
    }
    let images = labels
        .iter_mut()
        .zip(properties.iter_mut())
        .map(|(labels, properties)| CanonicalContents::node(labels, properties, None, None))
        .collect::<Result<Vec<_>, _>>()?;
    let keys = (start..end)
        .map(|id| format!("person-{id:020}"))
        .collect::<Vec<_>>();
    let revision = GraphRevision::new(1)?;
    let mut writes = Vec::with_capacity(items);
    for (key, image) in keys.iter().zip(&images) {
        writes.push(StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, LDBC_NAMESPACE, key)?,
            revision,
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(image)),
        });
    }

    let started = Instant::now();
    let result = store.graph_apply(&writes, &control())?;
    let wall_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let mut ids = Vec::with_capacity(items);
    for receipt in result.receipts() {
        match receipt.entity {
            EntityId::Node(id) => ids.push(id),
            EntityId::Relationship(_) => return Err(invalid("node batch returned edge receipt")),
        }
    }
    if ids.len() != items {
        return Err(invalid("node batch returned an incomplete receipt list"));
    }
    Ok((
        ids,
        BatchMeasurement {
            phase: "nodes",
            batch_index,
            items,
            wall_ms,
            store_nodes_before: start,
        },
    ))
}

fn apply_relationship_batch(
    store: &Store,
    nodes: &[NodeId],
    start: usize,
    items: usize,
    batch_index: usize,
    endpoints_rng: &mut SplitMix64,
) -> BenchResult<BatchMeasurement> {
    if nodes.is_empty() {
        return Err(invalid("relationships require at least one node"));
    }
    let end = start
        .checked_add(items)
        .ok_or_else(|| invalid("relationship batch range overflow"))?;
    let node_count = u64::try_from(nodes.len())?;
    let endpoints = (0..items)
        .map(|_| {
            let source_index = usize::try_from(endpoints_rng.next_u64() % node_count)?;
            let target_index = usize::try_from(endpoints_rng.next_u64() % node_count)?;
            let source = nodes
                .get(source_index)
                .copied()
                .ok_or_else(|| invalid("generated source node is out of range"))?;
            let target = nodes
                .get(target_index)
                .copied()
                .ok_or_else(|| invalid("generated target node is out of range"))?;
            Ok((source, target))
        })
        .collect::<BenchResult<Vec<_>>>()?;
    let keys = (start..end)
        .map(|id| format!("knows-{id:020}"))
        .collect::<Vec<_>>();
    let weight_name = GraphName::new("weight")?;
    let relationship_type = GraphName::new("KNOWS")?;
    let revision = GraphRevision::new(1)?;
    let mut properties = Vec::with_capacity(items);
    for id in start..end {
        properties.push([GraphProperty::new(
            weight_name,
            PropertyValue::new(PropertyData::I64(i64::try_from(id % 10_000)?))?,
        )]);
    }
    let mut writes = Vec::with_capacity(items);
    for ((key, (source, target)), properties) in keys.iter().zip(&endpoints).zip(&properties) {
        writes.push(StructuredWrite {
            key: ApplicationKey::new(EntityKind::Relationship, LDBC_NAMESPACE, key)?,
            revision,
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Relationship {
                source: NodeRef::Existing(*source),
                target: NodeRef::Existing(*target),
                relationship_type,
                properties,
            }),
        });
    }

    let started = Instant::now();
    let result = store.graph_apply(&writes, &control())?;
    let wall_ms = started.elapsed().as_secs_f64() * 1_000.0;
    if result.receipts().len() != items
        || result
            .receipts()
            .iter()
            .any(|receipt| !matches!(receipt.entity, EntityId::Relationship(_)))
    {
        return Err(invalid("relationship batch returned invalid receipts"));
    }
    Ok(BatchMeasurement {
        phase: "relationships",
        batch_index,
        items,
        wall_ms,
        store_nodes_before: nodes.len(),
    })
}

fn estimate_node_item_bytes() -> BenchResult<usize> {
    let mut labels = [GraphName::new("Person")?];
    let mut properties = [
        GraphProperty::new(
            GraphName::new("id")?,
            PropertyValue::new(PropertyData::I64(1))?,
        ),
        GraphProperty::new(
            GraphName::new("firstName")?,
            PropertyValue::new(PropertyData::String("FirstName000000001"))?,
        ),
        GraphProperty::new(
            GraphName::new("lastName")?,
            PropertyValue::new(PropertyData::String("LastName00000001"))?,
        ),
        GraphProperty::new(
            GraphName::new("birthday")?,
            PropertyValue::new(PropertyData::I64(1))?,
        ),
    ];
    let image = CanonicalContents::node(&mut labels, &mut properties, None, None)?;
    estimated_item_bytes(image.encoded_len(), "person-00000000000000000000".len())
}

fn estimate_relationship_item_bytes() -> BenchResult<usize> {
    let mut properties = [GraphProperty::new(
        GraphName::new("weight")?,
        PropertyValue::new(PropertyData::I64(1))?,
    )];
    let image = CanonicalContents::relationship(
        NodeId::new(1)?,
        NodeId::new(2)?,
        GraphName::new("KNOWS")?,
        &mut properties,
    )?;
    estimated_item_bytes(image.encoded_len(), "knows-00000000000000000000".len())
}

fn estimated_item_bytes(image_bytes: u64, key_bytes: usize) -> BenchResult<usize> {
    usize::try_from(image_bytes)?
        .checked_add(LDBC_NAMESPACE.len())
        .and_then(|bytes| bytes.checked_add(key_bytes))
        .and_then(|bytes| bytes.checked_add(OPERATION_FRAMING_UPPER_BOUND))
        .ok_or_else(|| invalid("estimated graph item size overflow"))
}

fn batch_item_count(
    remaining: usize,
    estimated_item_bytes: usize,
    max_items: usize,
) -> Option<usize> {
    if remaining == 0 {
        return Some(0);
    }
    if estimated_item_bytes == 0 {
        return None;
    }
    let byte_limited = MAX_BATCH_BYTES / estimated_item_bytes;
    let items = remaining.min(max_items).min(byte_limited);
    (items > 0).then_some(items)
}

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

fn batch_json(batch: &BatchMeasurement) -> Value {
    json!({
        "phase": batch.phase,
        "batch_index": batch.batch_index,
        "items": batch.items,
        "wall_ms": batch.wall_ms,
        "store_nodes_before": batch.store_nodes_before,
    })
}

fn phase_summary(phase: &str, batches: &[BatchMeasurement]) -> Value {
    let total_items = batches.iter().map(|batch| batch.items).sum::<usize>();
    let total_wall_ms = batches.iter().map(|batch| batch.wall_ms).sum::<f64>();
    let per_item = batches
        .iter()
        .filter(|batch| batch.items > 0)
        .map(|batch| batch.wall_ms / batch.items as f64)
        .collect::<Vec<_>>();
    let median_ms_per_item = median(per_item);
    json!({
        "phase": phase,
        "batch_count": batches.len(),
        "total_items": total_items,
        "first_batch_ms_per_item": batches.first().map(|batch| batch.wall_ms / batch.items as f64),
        "last_batch_ms_per_item": batches.last().map(|batch| batch.wall_ms / batch.items as f64),
        "median_ms_per_item": median_ms_per_item,
        "total_wall_ms": total_wall_ms,
    })
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        values.get(middle).copied()
    } else {
        Some((*values.get(middle.checked_sub(1)?)? + *values.get(middle)?) / 2.0)
    }
}

fn print_progress(batches: &[BatchMeasurement]) {
    let Some(last) = batches.last() else {
        return;
    };
    let total_items = batches.iter().map(|batch| batch.items).sum::<usize>();
    let total_wall_ms = batches.iter().map(|batch| batch.wall_ms).sum::<f64>();
    let ms_per_item = total_wall_ms / total_items as f64;
    eprintln!(
        "{} progress: batches={} items={} graph_apply_ms={:.3} ms_per_item={:.6}",
        last.phase,
        batches.len(),
        total_items,
        total_wall_ms,
        ms_per_item,
    );
}

fn print_phase_summary(batches: &[BatchMeasurement]) {
    let Some(first) = batches.first() else {
        return;
    };
    let summary = phase_summary(first.phase, batches);
    eprintln!(
        "{} summary: first_ms_per_item={} last_ms_per_item={} median_ms_per_item={} total_wall_ms={}",
        first.phase,
        display_optional(summary.get("first_batch_ms_per_item")),
        display_optional(summary.get("last_batch_ms_per_item")),
        display_optional(summary.get("median_ms_per_item")),
        summary
            .get("total_wall_ms")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
    );
}

fn display_optional(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_f64)
        .map(|number| format!("{number:.6}"))
        .unwrap_or_else(|| "n/a".to_owned())
}

fn host_info() -> BenchResult<Value> {
    let logical_cpus = std::thread::available_parallelism()?.get();
    Ok(json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "family": std::env::consts::FAMILY,
        "logical_cpus": logical_cpus,
    }))
}

fn directory_bytes(root: &Path) -> io::Result<u64> {
    let mut pending = vec![root.to_path_buf()];
    let mut bytes = 0_u64;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() || kind.is_symlink() {
                bytes = bytes
                    .checked_add(entry.metadata()?.len())
                    .ok_or_else(|| io::Error::other("store byte count overflow"))?;
            } else {
                return Err(io::Error::other("unexpected file type in store directory"));
            }
        }
    }
    Ok(bytes)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn peak_rss_bytes() -> BenchResult<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage writes the rusage value when it returns success.
    let status = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if status != 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: the successful getrusage call initialized the output value.
    let usage = unsafe { usage.assume_init() };
    let raw = i128::from(usage.ru_maxrss);
    if raw < 0 {
        return Err(invalid("getrusage returned negative peak RSS"));
    }
    let bytes = u64::try_from(raw)?;
    #[cfg(target_os = "macos")]
    return Ok(bytes);
    #[cfg(target_os = "linux")]
    bytes
        .checked_mul(1_024)
        .ok_or_else(|| invalid("peak RSS byte count overflow"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn peak_rss_bytes() -> BenchResult<u64> {
    Err(invalid(
        "peak RSS byte conversion is supported on macOS and Linux",
    ))
}

struct SplitMix64(u64);

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
}

fn invalid(message: &str) -> Box<dyn Error> {
    io::Error::other(message).into()
}

#[cfg(test)]
mod tests {
    use super::{MAX_BATCH_ITEMS, batch_item_count};

    #[test]
    fn batch_item_count_respects_item_and_byte_caps() {
        assert_eq!(batch_item_count(20_000, 1, MAX_BATCH_ITEMS), Some(16_384));
        assert_eq!(
            batch_item_count(20_000, 1_024, MAX_BATCH_ITEMS),
            Some(8_192)
        );
    }
}
