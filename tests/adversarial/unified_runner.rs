//! The existing runner Store owns both document and graph mutations.
use super::*;
use zeppelin_embed::graph_recovery_test_support::{
    apply_unified_batch, configure_unified_runner, unified_graph_enabled,
};
use zeppelin_embed::property_graph::query::completed::{GraphQueryOptions, Value};
use zeppelin_embed::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use zeppelin_embed::property_graph::{
    ApplicationKey, CanonicalContents, EntityKind, GraphName, GraphProperty, GraphRevision,
    NodeRef, PropertyData, PropertyValue,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnifiedObservation {
    pub generation: u64,
    pub graph_enabled: bool,
    pub documents: Vec<(u32, u64, i64)>,
    pub document_nodes: Vec<u32>,
    pub document_edges: Vec<(u32, u32, u32)>,
    pub nodes: Vec<u32>,
    pub edges: Vec<(u32, u32, u32)>,
}

pub fn check_unified(model: &Model, observed: &UnifiedObservation) -> Result<(), String> {
    let nodes: Vec<_> = model.graph_nodes.iter().copied().collect();
    let document_nodes: Vec<_> = if model.graph_enabled {
        model.live_ids().into_iter().collect()
    } else {
        Vec::new()
    };
    let document_edges: Vec<_> = model
        .document_edges
        .iter()
        .map(|(key, (from, to))| (*key, *from, *to))
        .collect();
    let edges: Vec<_> = model
        .edges
        .iter()
        .map(|(key, (from, to))| (*key, *from, *to))
        .collect();
    if observed.generation != model.unified_generation
        || observed.graph_enabled != model.graph_enabled
        || observed.documents != model.live_documents()
        || observed.document_nodes != document_nodes
        || observed.document_edges != document_edges
        || observed.nodes != nodes
        || observed.edges != edges
    {
        return Err(format!(
            "unified state mismatch: expected generation={} enabled={} docs={:?} nodes={nodes:?} edges={edges:?}; observed={observed:?}",
            model.unified_generation,
            model.graph_enabled,
            model.live_documents()
        ));
    }
    Ok(())
}

#[derive(Debug)]
enum UnifiedError {
    Store(StoreError),
    Graph(Box<zeppelin_embed::property_graph::GraphStoreError>),
    Invalid(String),
}
impl std::fmt::Display for UnifiedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => e.fmt(f),
            Self::Graph(e) => e.fmt(f),
            Self::Invalid(e) => e.fmt(f),
        }
    }
}

impl RealEngine {
    pub(super) fn recover_unified_read_refusal(&mut self, model: &Model) -> Result<(), String> {
        self.vfs.set_operation(usize::MAX);
        self.reopen()?;
        check_unified(model, &self.observe_unified()?)
    }

    fn apply_unified(&mut self, op: &Op) -> Result<Option<MutationAck>, UnifiedError> {
        let store = self.store().map_err(UnifiedError::Invalid)?;
        if matches!(op, Op::EnableGraph) {
            let enabled = unified_graph_enabled(store)
                .map_err(|error| UnifiedError::Graph(Box::new(error)))?;
            let generation = store.enable_graph().map_err(UnifiedError::Store)?;
            configure_unified_runner(store)
                .map_err(|error| UnifiedError::Graph(Box::new(error)))?;
            return Ok(Some(MutationAck {
                generation,
                changed: !enabled,
            }));
        }
        let graph_key = match *op {
            Op::GraphApply { graph_key } | Op::MixedBatch { graph_key, .. } => graph_key,
            _ => return Err(UnifiedError::Invalid("not a unified operation".into())),
        };
        let from = graph_key
            .checked_mul(2)
            .ok_or_else(|| UnifiedError::Invalid("graph key overflow".into()))?;
        let to = from
            .checked_add(1)
            .ok_or_else(|| UnifiedError::Invalid("graph key overflow".into()))?;
        let a_key = format!("node-{from}");
        let b_key = format!("node-{to}");
        let edge_key = format!("edge-{graph_key}");
        let mut a_labels = [GraphName::new("ZE358").unwrap()];
        let mut b_labels = [GraphName::new("ZE358").unwrap()];
        let mut a_props = [GraphProperty::new(
            GraphName::new("k").unwrap(),
            PropertyValue::new(PropertyData::I64(i64::from(from))).unwrap(),
        )];
        let mut b_props = [GraphProperty::new(
            GraphName::new("k").unwrap(),
            PropertyValue::new(PropertyData::I64(i64::from(to))).unwrap(),
        )];
        let edge_props = [GraphProperty::new(
            GraphName::new("k").unwrap(),
            PropertyValue::new(PropertyData::I64(i64::from(graph_key))).unwrap(),
        )];
        let a = CanonicalContents::node(&mut a_labels, &mut a_props, None, None).unwrap();
        let b = CanonicalContents::node(&mut b_labels, &mut b_props, None, None).unwrap();
        let documents = match *op {
            Op::MixedBatch {
                doc_id,
                revision,
                timestamp,
                ..
            } => Some(
                IngestBatch::new(vec![
                    IngestDocument::new(
                        DocumentVersion::new(
                            DocId::new(u128::from(doc_id)),
                            Revision::new(revision),
                        ),
                        program::vector(doc_id, revision).to_vec(),
                    )
                    .with_timestamp(timestamp)
                    .with_metadata(program::sentinel(doc_id))
                    .with_text(program::lexical_text(doc_id, revision))
                    .with_columns(adversarial_columns(doc_id)),
                ])
                .with_epoch(declared_identity()),
            ),
            _ => None,
        };
        zeppelin_embed::property_graph::with_local_refs(|refs| {
            let requests = [
                StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "ze358", &a_key).unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&a)),
                },
                StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "ze358", &b_key).unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&b)),
                },
                StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Relationship, "ze358", &edge_key).unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Relationship {
                        source: NodeRef::Local(refs.node(0).unwrap()),
                        target: NodeRef::Local(refs.node(1).unwrap()),
                        relationship_type: GraphName::new("ZE358_LINK").unwrap(),
                        properties: &edge_props,
                    }),
                },
            ];
            apply_unified_batch(
                store,
                documents.as_ref(),
                &requests,
                &QueryControl::Cancel(CancelToken::new()),
            )
            .map(|(generation, changed)| {
                Some(MutationAck {
                    generation,
                    changed,
                })
            })
            .map_err(|error| UnifiedError::Graph(Box::new(error)))
        })
    }

    pub(super) fn observe_unified(&mut self) -> Result<UnifiedObservation, String> {
        use zeppelin_embed::lifecycle::{DocumentFields, DocumentScanRequest};
        let store = self.store()?;
        let generation = store.snapshot().map_err(|e| e.to_string())?.generation();
        let mut documents = Vec::new();
        let mut cursor = None;
        loop {
            let mut request = DocumentScanRequest::new(
                1024,
                DocumentFields::NONE,
                QueryControl::Cancel(CancelToken::new()),
            );
            if let Some(cursor) = cursor {
                request = request.with_cursor(cursor);
            }
            let page = store.scan_documents(request).map_err(|e| e.to_string())?;
            if page.generation != generation {
                return Err("document scan crossed a generation".into());
            }
            for doc in page.documents {
                documents.push((
                    u32::try_from(doc.doc_id.get()).map_err(|e| e.to_string())?,
                    doc.revision.get(),
                    doc.timestamp,
                ));
            }
            cursor = page.continuation;
            if cursor.is_none() {
                break;
            }
        }
        documents.sort_unstable();
        let graph_enabled = unified_graph_enabled(store).map_err(|e| e.to_string())?;
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let mut document_nodes = Vec::new();
        let mut document_edges = Vec::new();
        if graph_enabled {
            for (query, relationship) in [
                ("MATCH (d:Document) RETURN d", false),
                (
                    "MATCH (a:Document)-[r:ZE358_DOC_LINK]->(b:Document) RETURN r.k,a,b",
                    true,
                ),
            ] {
                let result = zeppelin_embed_cypher::execute(
                    store,
                    &QueryControl::Cancel(CancelToken::new()),
                    &GraphQueryOptions::default(),
                    query,
                    &[],
                    zeppelin_embed_cypher::CompileLimits::default(),
                )
                .map_err(|e| e.to_string())?;
                let node_id = |row, column| -> Result<u32, String> {
                    let Some(Value::Node(index)) = result.cell(row, column) else {
                        return Err("document query returned a non-node".into());
                    };
                    u32::try_from(result.pools().nodes[*index as usize].id.get())
                        .map_err(|e| e.to_string())
                };
                for row in 0..result.metadata().rows as usize {
                    if relationship {
                        let Some(Value::I64(key)) = result.cell(row, 0) else {
                            return Err("document relationship returned a non-integer key".into());
                        };
                        document_edges.push((
                            u32::try_from(*key).map_err(|e| e.to_string())?,
                            node_id(row, 1)?,
                            node_id(row, 2)?,
                        ));
                    } else {
                        document_nodes.push(node_id(row, 0)?);
                    }
                }
            }
            for (query, columns) in [
                ("MATCH (n:ZE358) RETURN n.k", 1),
                (
                    "MATCH (a:ZE358)-[r:ZE358_LINK]->(b:ZE358) RETURN r.k,a.k,b.k",
                    3,
                ),
            ] {
                let result = zeppelin_embed_cypher::execute(
                    store,
                    &QueryControl::Cancel(CancelToken::new()),
                    &GraphQueryOptions::default(),
                    query,
                    &[],
                    zeppelin_embed_cypher::CompileLimits::default(),
                )
                .map_err(|e| e.to_string())?;
                for row in 0..result.metadata().rows {
                    let mut values = Vec::new();
                    for column in 0..columns {
                        let Some(Value::I64(value)) = result.cell(row as usize, column) else {
                            return Err("unified query returned a non-integer key".into());
                        };
                        values.push(u32::try_from(*value).map_err(|e| e.to_string())?);
                    }
                    if columns == 1 {
                        nodes.push(values[0]);
                    } else {
                        edges.push((values[0], values[1], values[2]));
                    }
                }
            }
        }
        nodes.sort_unstable();
        edges.sort_unstable();
        document_nodes.sort_unstable();
        document_edges.sort_unstable();
        Ok(UnifiedObservation {
            generation,
            graph_enabled,
            documents,
            document_nodes,
            document_edges,
            nodes,
            edges,
        })
    }

    pub(super) fn execute_unified(
        &mut self,
        model: &mut Model,
        op: &Op,
    ) -> Result<Option<MutationAck>, String> {
        let (candidate, changed) = model.unified_candidate(op)?;
        let ack = self
            .apply_unified(op)
            .map_err(|e| e.to_string())?
            .ok_or("unified mutation omitted acknowledgement")?;
        if ack.generation != candidate.unified_generation || ack.changed != changed {
            return Err(format!(
                "unified acknowledgement mismatch: expected {}/{changed}, observed {ack:?}",
                candidate.unified_generation
            ));
        }
        check_unified(&candidate, &self.observe_unified()?)?;
        *model = candidate;
        Ok(Some(ack))
    }

    pub(super) fn recover_unified(
        &mut self,
        model: &mut Model,
        op: &Op,
    ) -> Result<Option<MutationAck>, String> {
        // Inspect a complete pre/post state before deciding whether to retry.
        let (candidate, _) = model.unified_candidate(op)?;
        let observed = self.observe_unified()?;
        if check_unified(&candidate, &observed).is_ok() {
            *model = candidate;
            return Ok(Some(MutationAck {
                generation: observed.generation,
                changed: false,
            }));
        }
        check_unified(model, &observed)?;
        self.execute_unified(model, op)
    }
}

#[cfg(test)]
pub fn probe_unified_program(program: &Program) -> Result<(), String> {
    let root = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut engine = RealEngine::without_faults(root.path().to_owned());
    engine.open()?;
    let mut model = Model::default();
    let initial = DocMutation {
        doc_id: 1,
        revision: 1,
        timestamp: 10,
    };
    let ack = engine.ingest(&[initial])?;
    model.acknowledge(1, 1, 10);
    model.unified_generation = 1;
    if ack.generation != 1 {
        return Err("initial document generation".into());
    }
    let mut calls = 0;
    for op in &program.ops {
        match op {
            Op::EnableGraph | Op::GraphApply { .. } | Op::MixedBatch { .. } => {
                engine.execute_unified(&mut model, op)?;
                calls += 1;
            }
            Op::Seal if model.graph_enabled => {
                let before = model.unified_generation;
                let ack = engine.seal()?;
                if ack.changed {
                    model.unified_generation = before + 1;
                }
                model.seal();
                check_unified(&model, &engine.observe_unified()?)?;
            }
            Op::Reopen if model.graph_enabled => {
                engine.reopen()?;
                check_unified(&model, &engine.observe_unified()?)?;
            }
            _ => {}
        }
    }
    if calls != 15 {
        return Err(format!(
            "expected fifteen graph operations, observed {calls}"
        ));
    }
    if !root.path().join("wal.ze").exists()
        || !std::fs::read_dir(root.path())
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
            .into_iter()
            .any(|entry| entry.path().extension().is_some_and(|ext| ext == "zgraph"))
    {
        return Err("unified Store paths missing".into());
    }
    Ok(())
}

#[cfg(test)]
pub fn probe_unified_faults() -> Result<(), String> {
    exercise_unified_faults(256, &mut CoverageRegistry::default())
}

/// Same submitted graph-only fixture for clean and faulted operation/path legs.
pub fn exercise_unified_faults(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    use fault_vfs::{FaultMode, FaultSite, Layer};
    let root = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mixed = Op::MixedBatch {
        doc_id: 91,
        revision: 2,
        timestamp: 20,
        graph_key: 9,
    };
    let mut cases = Vec::new();
    for mode in [FaultMode::Eio, FaultMode::PostCommitError] {
        for (op, site, path, prefix) in [
            (
                Op::EnableGraph,
                FaultSite::Write,
                ".zgraph",
                "storage-durability.graph-commit.enable",
            ),
            (
                Op::EnableGraph,
                FaultSite::Sync,
                ".zgraph",
                "storage-durability.graph-commit.enable",
            ),
            (
                Op::EnableGraph,
                FaultSite::Rename,
                "manifest.ze",
                "storage-durability.graph-commit.enable",
            ),
            (
                Op::GraphApply { graph_key: 9 },
                FaultSite::Write,
                ".zgraph",
                "storage-durability.graph-commit.artifact-write",
            ),
            (
                Op::GraphApply { graph_key: 9 },
                FaultSite::Sync,
                ".zgraph",
                "storage-durability.graph-commit.artifact-sync",
            ),
            (
                Op::GraphApply { graph_key: 9 },
                FaultSite::Append,
                "wal.ze",
                "storage-durability.graph-commit.wal-append",
            ),
            (
                Op::GraphApply { graph_key: 9 },
                FaultSite::Sync,
                "wal.ze",
                "storage-durability.graph-commit.wal-sync",
            ),
            (
                mixed.clone(),
                FaultSite::Sync,
                "wal.ze",
                "storage-durability.mixed-batch.sync",
            ),
        ] {
            cases.push((op, site, path, prefix, 1, mode));
        }
    }
    for nth in 1..=2 {
        cases.push((
            mixed.clone(),
            FaultSite::Append,
            "wal.ze",
            "storage-durability.mixed-batch.members",
            nth,
            FaultMode::Eio,
        ));
    }
    cases.push((
        mixed.clone(),
        FaultSite::Append,
        "wal.ze",
        "storage-durability.mixed-batch.members",
        2,
        FaultMode::PostCommitError,
    ));
    cases.push((
        mixed,
        FaultSite::Append,
        "wal.ze",
        "storage-durability.mixed-batch.torn-final",
        2,
        FaultMode::TornWrite,
    ));
    for (ordinal, (op, site, needle, prefix, nth, mode)) in cases.into_iter().enumerate() {
        let event = FaultEvent {
            id: format!("ze358-{seed}-{ordinal}"),
            op_index: 7,
            layer: Layer::Io,
            site,
            mode,
            nth_match: nth,
            expected_matches: None,
            deadline_budget_seconds: None,
            path_contains: Some(needle.into()),
            fired: false,
            fire_count: 0,
            path: None,
        };
        let (mut clean, before) = fault_fixture(
            root.path().join(format!("{ordinal}-clean")),
            FaultSchedule::default(),
            !matches!(op, Op::EnableGraph),
        )?;
        let (candidate, _) = before.unified_candidate(&op)?;
        let mut clean_model = before.clone();
        clean.execute_unified(&mut clean_model, &op)?;
        let clean_observed = clean.observe_unified()?;
        check_unified(&candidate, &clean_observed)?;
        let mut schedule = FaultSchedule::single(event.clone());
        if mode == FaultMode::TornWrite {
            // Stop before publication after the final member's short append.
            schedule.events.push(FaultEvent {
                id: format!("{}-sync", event.id),
                site: FaultSite::Sync,
                mode: FaultMode::Eio,
                nth_match: 1,
                ..event
            });
        }
        let (mut faulted, mut model) = fault_fixture(
            root.path().join(format!("{ordinal}-fault")),
            schedule,
            !matches!(op, Op::EnableGraph),
        )?;
        // Independent identical inputs, including document-only generation gaps.
        check_unified(&before, &faulted.observe_unified()?)?;
        faulted.vfs.set_operation(7);
        if faulted.apply_unified(&op).is_ok() {
            return Err(format!("{op:?}/{site:?}/{mode:?} acknowledged a fault"));
        }
        let events = faulted.vfs.events();
        if events.len() != if mode == FaultMode::TornWrite { 2 } else { 1 }
            || events.iter().any(|event| {
                event.op_index != 7
                    || event.fire_count != 1
                    || !event.fired
                    || !event
                        .path
                        .as_ref()
                        .is_some_and(|p| p.to_string_lossy().contains(needle))
            })
        {
            return Err(format!("wrong operation/path fire: {events:?}"));
        }
        if mode == FaultMode::TornWrite {
            use zeppelin_embed::wal::{header::WAL_HEADER_LEN, record::decode_record};
            let clean_wal =
                std::fs::read(clean.directory.join("wal.ze")).map_err(|e| e.to_string())?;
            let fault_wal =
                std::fs::read(faulted.directory.join("wal.ze")).map_err(|e| e.to_string())?;
            let mut offset = WAL_HEADER_LEN;
            let mut last = 0;
            while offset < clean_wal.len() {
                last = decode_record(&clean_wal[offset..])
                    .map_err(|e| e.to_string())?
                    .encoded_len;
                offset += last;
            }
            if fault_wal.len() != clean_wal.len() - last.div_ceil(2) {
                return Err(
                    "final mixed member did not retain the measured half-frame byte cut".into(),
                );
            }
        }
        faulted.vfs.set_operation(8);
        // Drop the fenced handle; do not checkpoint or blindly retry a mixed commit.
        faulted.store.take();
        faulted.open()?;
        let observed = faulted.observe_unified()?;
        let committed = match op {
            Op::EnableGraph => site == FaultSite::Rename && mode == FaultMode::PostCommitError,
            Op::MixedBatch { .. } => {
                site == FaultSite::Sync
                    || (site == FaultSite::Append && nth == 2 && mode == FaultMode::PostCommitError)
            }
            _ => {
                site == FaultSite::Sync && needle == "wal.ze"
                    || site == FaultSite::Append && mode == FaultMode::PostCommitError
            }
        };
        check_unified(if committed { &candidate } else { &before }, &observed)?;
        let wal_before =
            std::fs::read(faulted.directory.join("wal.ze")).map_err(|e| e.to_string())?;
        faulted.recover_unified(&mut model, &op)?;
        check_unified(&candidate, &faulted.observe_unified()?)?;
        if committed
            && std::fs::read(faulted.directory.join("wal.ze")).map_err(|e| e.to_string())?
                != wal_before
        {
            return Err("reconciliation retried a recovered complete commit".into());
        }
        coverage.hit(format!("op.{}", op.kind()));
        coverage.hit(prefix);
        coverage.hit(format!("{prefix}.fire"));
        coverage.hit(format!("{prefix}.clean"));
        let aggregate = match op {
            Op::MixedBatch { .. } => "storage-durability.mixed-batch",
            _ => "storage-durability.graph-commit",
        };
        coverage.hit(format!("{aggregate}.fire"));
        coverage.hit(format!("{aggregate}.clean"));
    }
    // A scheduled event alone earns nothing. Wrong operation and wrong path both run cleanly.
    for (index, needle) in [(6, "wal.ze"), (7, "manifest.ze")] {
        let event = FaultEvent {
            id: "ze358-wrong-target".into(),
            op_index: index,
            layer: Layer::Io,
            site: FaultSite::Append,
            mode: FaultMode::Eio,
            nth_match: 1,
            expected_matches: None,
            deadline_budget_seconds: None,
            path_contains: Some(needle.into()),
            fired: false,
            fire_count: 0,
            path: None,
        };
        let (mut engine, mut model) = fault_fixture(
            root.path().join(format!("wrong-{index}")),
            FaultSchedule::single(event),
            true,
        )?;
        engine.vfs.set_operation(7);
        engine.execute_unified(&mut model, &Op::GraphApply { graph_key: 9 })?;
        if engine
            .vfs
            .events()
            .iter()
            .any(|e| e.fired || e.fire_count != 0)
        {
            return Err("wrong target fired".into());
        }
    }
    Ok(())
}

fn fault_fixture(
    path: PathBuf,
    schedule: FaultSchedule,
    enabled: bool,
) -> Result<(RealEngine, Model), String> {
    let vfs = Arc::new(fault_vfs::simulated_scheduled(schedule));
    let mut engine = RealEngine::new(path, vfs, Arc::new(ManualMonotonicClock::new()));
    engine.open()?;
    let mut model = Model::default();
    engine.ingest(&[DocMutation {
        doc_id: 1,
        revision: 1,
        timestamp: 10,
    }])?;
    model.acknowledge(1, 1, 10);
    model.unified_generation = 1;
    engine.seal()?;
    model.seal();
    model.unified_generation = 2;
    if enabled {
        engine.execute_unified(&mut model, &Op::EnableGraph)?;
    }
    Ok((engine, model))
}

fn file_image(path: &Path) -> Result<BTreeMap<std::ffi::OsString, Vec<u8>>, String> {
    std::fs::read_dir(path)
        .map_err(|error| error.to_string())?
        .map(|entry| {
            let entry = entry.map_err(|error| error.to_string())?;
            Ok((
                entry.file_name(),
                std::fs::read(entry.path()).map_err(|error| error.to_string())?,
            ))
        })
        .collect()
}

impl RealEngine {
    fn document_relationship(
        &self,
        source: u32,
        target: u32,
    ) -> Result<(u64, bool), Box<zeppelin_embed::property_graph::GraphStoreError>> {
        use zeppelin_embed::property_graph::NodeId;
        let properties = [GraphProperty::new(
            GraphName::new("k").unwrap(),
            PropertyValue::new(PropertyData::I64(358)).unwrap(),
        )];
        apply_unified_batch(
            self.store.as_ref().unwrap(),
            None,
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "ze358", "document-link")
                    .unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Existing(NodeId::new(u128::from(source)).unwrap()),
                    target: NodeRef::Existing(NodeId::new(u128::from(target)).unwrap()),
                    relationship_type: GraphName::new("ZE358_DOC_LINK").unwrap(),
                    properties: &properties,
                }),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(Box::new)
    }

    fn execute_document_relationship(&mut self, model: &mut Model) -> Result<(), String> {
        let next = model.document_relationship_candidate(358, 1, 92)?;
        let ack = self
            .document_relationship(1, 92)
            .map_err(|error| error.to_string())?;
        if ack != (next.unified_generation, true) {
            return Err(format!(
                "document relationship acknowledgement mismatch: {ack:?}"
            ));
        }
        check_unified(&next, &self.observe_unified()?)?;
        *model = next;
        Ok(())
    }
}

/// Document endpoints run on the same adapter and comparator as Part A.
pub fn exercise_document_operation(
    operation: super::super::unified_graph::Operation,
    seed: u64,
    coverage: &mut CoverageRegistry,
) -> Result<(), String> {
    use super::super::unified_graph::Operation;
    use zeppelin_embed::property_graph::catalog::{OnDelete, RelationshipRule, RelationshipRules};
    let root = tempfile::tempdir().map_err(|error| error.to_string())?;
    let path = root.path().join(format!("documents-{seed}"));
    let (mut engine, mut model) = if matches!(operation, Operation::DocumentDelete) {
        let rules = [RelationshipRule {
            relationship_type: GraphName::new("ZE358_DOC_LINK").unwrap(),
            on_delete: OnDelete::Restrict,
        }];
        let store = zeppelin_embed::graph_recovery_test_support::create_with_relationship_rules(
            &path,
            RealEngine::options(ModelEpoch::A),
            Some(declared_store_epoch().embedding.document),
            RelationshipRules::new(&rules).map_err(|error| error.to_string())?,
        )?;
        store.close().map_err(|error| error.to_string())?;
        let mut engine = RealEngine::without_faults(path);
        engine.open()?;
        let mut model = Model::default();
        model.graph_enabled = true;
        model.unified_generation = 1;
        let ack = engine.ingest(&[DocMutation {
            doc_id: 1,
            revision: 1,
            timestamp: 10,
        }])?;
        model.acknowledge(1, 1, 10);
        model.unified_generation += 1;
        if ack.generation != model.unified_generation {
            return Err("Restrict fixture ingest generation mismatch".into());
        }
        (engine, model)
    } else {
        fault_fixture(path, FaultSchedule::default(), false)?
    };
    let ack = engine.ingest(&[
        DocMutation {
            doc_id: 92,
            revision: 1,
            timestamp: 20,
        },
        DocMutation {
            doc_id: 93,
            revision: 1,
            timestamp: 30,
        },
    ])?;
    model.acknowledge(92, 1, 20);
    model.acknowledge(93, 1, 30);
    model.unified_generation += 1;
    if ack.generation != model.unified_generation {
        return Err("document fixture ingest generation mismatch".into());
    }
    if !model.graph_enabled {
        engine.execute_unified(&mut model, &Op::EnableGraph)?;
    }
    check_unified(&model, &engine.observe_unified()?)?;
    match operation {
        Operation::DocumentRelationship => {
            engine.execute_document_relationship(&mut model)?;
            // Drop without a seal so this observes WAL recovery, not just a folded image.
            engine.store.take();
            engine.open()?;
            check_unified(&model, &engine.observe_unified()?)?;
            coverage.hit("op.graph_apply.document-endpoint");
        }
        Operation::DocumentDelete => {
            engine.execute_document_relationship(&mut model)?;
            {
                let doc_id = 92;
                if model.document_delete_candidate(doc_id).is_ok() {
                    return Err("model accepted Restrict document delete".into());
                }
                let before = file_image(&engine.directory)?;
                let error = engine
                    .store()?
                    .delete(DeleteBatch::new(vec![DocId::new(u128::from(doc_id))]));
                if !matches!(error, Err(IngestError::Graph(ref error)) if error.kind() == zeppelin_embed::property_graph::query::completed::GraphQueryErrorKind::Constraint)
                {
                    return Err(format!(
                        "Restrict did not return a typed constraint: {error:?}"
                    ));
                }
                if file_image(&engine.directory)? != before {
                    return Err("Restrict document delete changed durable bytes".into());
                }
                check_unified(&model, &engine.observe_unified()?)?;
            }
            coverage.hit("op.delete.document-restrict");
            // A successful isolated delete also proves the refused writes left admission usable.
            let next = model.document_delete_candidate(93)?;
            let ack = engine.delete(93)?;
            if ack.generation != next.unified_generation {
                return Err("document delete generation mismatch".into());
            }
            model = next;
            check_unified(&model, &engine.observe_unified()?)?;
            engine.store.take();
            engine.open()?;
            check_unified(&model, &engine.observe_unified()?)?;
            coverage.hit("op.delete.document-no-relationships");
        }
        Operation::MissingDocumentEndpoint => {
            for (source, target) in [(999_999, 1), (1, 999_999)] {
                if model
                    .document_relationship_candidate(358, source, target)
                    .is_ok()
                {
                    return Err("model accepted missing document endpoint".into());
                }
                let before = file_image(&engine.directory)?;
                let error = engine.document_relationship(source, target);
                if !matches!(error, Err(ref error) if error.kind() == zeppelin_embed::property_graph::GraphStoreErrorKind::Constraint)
                {
                    return Err(format!(
                        "missing document endpoint did not return a typed constraint: {error:?}"
                    ));
                }
                if file_image(&engine.directory)? != before {
                    return Err("missing document endpoint changed durable bytes".into());
                }
                check_unified(&model, &engine.observe_unified()?)?;
                engine.store.take();
                engine.open()?;
                check_unified(&model, &engine.observe_unified()?)?;
            }
            engine.execute_document_relationship(&mut model)?;
            coverage.hit("op.graph_apply.missing-document-endpoint");
        }
        _ => return Err("not a document endpoint operation".into()),
    }
    Ok(())
}

#[cfg(test)]
pub fn probe_unified_refusals() -> Result<(), String> {
    let root = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (mut engine, mut model) =
        fault_fixture(root.path().join("refusal"), FaultSchedule::default(), false)?;
    let image = || file_image(&root.path().join("refusal"));
    let before = image()?;
    if !matches!(
        engine.apply_unified(&Op::GraphApply { graph_key: 1 }),
        Err(UnifiedError::Graph(_))
    ) {
        return Err("graph-free write did not return a typed graph refusal".into());
    }
    if image()? != before {
        return Err("definite refusal changed durable bytes".into());
    }
    check_unified(&model, &engine.observe_unified()?)?;
    engine.execute_unified(&mut model, &Op::EnableGraph)?;
    // Repeated enable is independently modelled as an unchanged generation.
    engine.execute_unified(&mut model, &Op::EnableGraph)?;
    engine.store.take();
    let before = image()?;
    let reader = Store::open(
        &engine.directory,
        OpenOptions::read_only()
            .with_schema(adversarial_schema())
            .with_epoch(declared_store_epoch()),
    )
    .map_err(|e| e.to_string())?;
    if !matches!(reader.enable_graph(), Err(StoreError::ReadOnly)) {
        return Err("read-only enable did not refuse with ReadOnly".into());
    }
    reader.close().map_err(|e| e.to_string())?;
    if image()? != before {
        return Err("read-only refusal changed durable bytes".into());
    }
    engine.open()?;
    engine.execute_unified(&mut model, &Op::GraphApply { graph_key: 1 })?;
    // Duplicate keyed graph writes retain the independently expected generation.
    engine.execute_unified(&mut model, &Op::GraphApply { graph_key: 1 })?;
    Ok(())
}

#[cfg(test)]
pub fn probe_mixed_replay() -> Result<(), String> {
    let root = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (mut engine, mut model) =
        fault_fixture(root.path().to_owned(), FaultSchedule::default(), true)?;
    let op = Op::MixedBatch {
        doc_id: 91,
        revision: 2,
        timestamp: 20,
        graph_key: 9,
    };
    engine.execute_unified(&mut model, &op)?;
    engine.execute_unified(&mut model, &op)?;
    if model.unified_generation != 4 {
        return Err("keyed mixed replay advanced generation".into());
    }
    Ok(())
}

#[cfg(test)]
pub fn probe_unified_read_refusal() -> Result<(), String> {
    use fault_vfs::{FaultMode, FaultSite, Layer};
    let root = tempfile::tempdir().map_err(|e| e.to_string())?;
    let event = FaultEvent {
        id: "ze358-reopen-read".into(),
        op_index: 7,
        layer: Layer::Content,
        site: FaultSite::Read,
        mode: FaultMode::BitFlip,
        nth_match: 1,
        expected_matches: None,
        deadline_budget_seconds: None,
        path_contains: Some("manifest.ze".into()),
        fired: false,
        fire_count: 0,
        path: None,
    };
    let (mut engine, mut model) =
        fault_fixture(root.path().to_owned(), FaultSchedule::single(event), true)?;
    engine.execute_unified(&mut model, &Op::GraphApply { graph_key: 1 })?;
    engine.vfs.set_operation(7);
    if engine.reopen().is_ok() || engine.store.is_some() || engine.vfs.events()[0].fire_count != 1 {
        return Err("transient manifest read did not refuse reopen exactly once".into());
    }
    engine.recover_unified_read_refusal(&model)?;
    engine.execute_unified(&mut model, &Op::GraphApply { graph_key: 2 })?;
    check_unified(&model, &engine.observe_unified()?)
}

#[cfg(test)]
mod ze410_tests {
    use super::*;

    #[test]
    fn unified_storage_mutations_preserve_acknowledged_documents_and_graph() {
        for seed in 0..4 {
            let root = tempfile::tempdir().unwrap();
            let mut engine = RealEngine::without_faults(root.path().to_owned());
            let mut model = Model::default();
            for (index, op) in Program::generate_for(CampaignKind::StorageDurability, seed)
                .ops
                .iter()
                .enumerate()
                .take(46)
            {
                let ack = match *op {
                    Op::Open => {
                        engine.open().unwrap();
                        None
                    }
                    Op::Ingest {
                        first_id,
                        count,
                        revision,
                        timestamp,
                    } => {
                        let docs = (first_id..first_id + count)
                            .map(|doc_id| DocMutation {
                                doc_id,
                                revision,
                                timestamp,
                            })
                            .collect::<Vec<_>>();
                        let ack = engine.ingest(&docs).unwrap();
                        for doc in docs {
                            model.acknowledge(doc.doc_id, doc.revision, doc.timestamp);
                        }
                        Some(ack)
                    }
                    Op::EnableGraph | Op::GraphApply { .. } | Op::MixedBatch { .. } => {
                        engine.execute_unified(&mut model, op).unwrap()
                    }
                    Op::Seal => {
                        let ack = engine.seal().unwrap();
                        model.seal();
                        Some(ack)
                    }
                    Op::Maintain { bytes } => Some(engine.maintain(bytes).unwrap()),
                    Op::Revise {
                        doc_id,
                        revision,
                        timestamp,
                    } => {
                        let ack = engine
                            .ingest(&[DocMutation {
                                doc_id,
                                revision,
                                timestamp,
                            }])
                            .unwrap();
                        model.acknowledge(doc_id, revision, timestamp);
                        Some(ack)
                    }
                    Op::Upsert {
                        doc_id,
                        revision,
                        timestamp,
                    } => {
                        let ack = engine
                            .ingest(&[DocMutation {
                                doc_id,
                                revision,
                                timestamp,
                            }])
                            .unwrap();
                        model.acknowledge(doc_id, revision, timestamp);
                        Some(ack)
                    }
                    Op::Delete { doc_id } => {
                        let ack = engine.delete(doc_id).unwrap();
                        model.delete(doc_id);
                        Some(ack)
                    }
                    Op::Purge { doc_id } => {
                        let ack = engine.purge(doc_id).unwrap();
                        model.purge(doc_id);
                        Some(ack)
                    }
                    Op::Close => {
                        engine.close().unwrap();
                        None
                    }
                    Op::Reopen => {
                        engine.reopen().unwrap();
                        None
                    }
                    _ => continue,
                };
                if let Some(ack) = ack {
                    model.unified_generation = ack.generation;
                }
                if matches!(op, Op::Close) {
                    continue;
                }
                check_unified(&model, &engine.observe_unified().unwrap())
                    .unwrap_or_else(|e| panic!("op {index} {op:?}: {e}"));
                let found = engine
                    .search(&program::query(3), usize::MAX, SearchKind::Scan, 0)
                    .unwrap()
                    .hits
                    .into_iter()
                    .map(|h| h.doc_id)
                    .collect::<BTreeSet<_>>();
                assert_eq!(found, model.live_ids(), "op {index} {op:?}");
            }
        }
    }

    #[test]
    fn unified_reopen_counts_internal_snapshot_owner_and_rejects_leaks() {
        let root = tempfile::tempdir().unwrap();
        let mut engine = RealEngine::without_faults(root.path().to_owned());
        engine.open().unwrap();
        let mut model = Model::default();
        let stats = engine.stats().unwrap();
        assert_eq!(stats.active_snapshot_leases, 0);
        assert!(
            lifecycle_stats_violation(
                1,
                FaultProfile::None,
                0,
                model.quiescent_snapshot_owners(),
                stats
            )
            .is_none()
        );
        engine
            .execute_unified(&mut model, &Op::EnableGraph)
            .unwrap();
        engine.reopen().unwrap();
        check_unified(&model, &engine.observe_unified().unwrap()).unwrap();
        let stats = engine.stats().unwrap();
        assert_eq!(stats.active_snapshot_leases, 1);
        assert!(
            lifecycle_stats_violation(
                1,
                FaultProfile::None,
                1,
                model.quiescent_snapshot_owners(),
                stats
            )
            .is_none()
        );
        let leaked = engine.store().unwrap().snapshot().unwrap();
        let stats = engine.stats().unwrap();
        assert_eq!(stats.active_snapshot_leases, 2);
        assert!(
            lifecycle_stats_violation(
                1,
                FaultProfile::None,
                2,
                model.quiescent_snapshot_owners(),
                stats
            )
            .is_some()
        );
        drop(leaked);
        assert_eq!(engine.stats().unwrap().active_snapshot_leases, 1);
    }
}
