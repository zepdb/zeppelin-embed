use std::cell::{Cell, RefCell};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativePrepareStage {
    Quantize,
    QuantizeValidation,
    QueryValidation,
    Build,
    OutputValidation,
    CodeInitialization,
    ImageAllocation,
    ImageInitialization,
    Serialization,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NativePrepareEvent {
    pub(crate) stage: NativePrepareStage,
    pub(crate) units: u64,
    pub(crate) work_before: u64,
    pub(crate) reserved_before: usize,
    pub(crate) requested_bytes: usize,
}

struct Schedule {
    storage_limit: Option<usize>,
    work_limit: Option<u64>,
    observer: Box<dyn FnMut(NativePrepareEvent)>,
}

thread_local! {
    static SCHEDULE: RefCell<Option<Schedule>> = const { RefCell::new(None) };
    static VALIDATION_PHASE: Cell<Option<NativePrepareStage>> = const { Cell::new(None) };
}

pub(crate) struct NativeValidationScope(Option<NativePrepareStage>);

impl Drop for NativeValidationScope {
    fn drop(&mut self) {
        VALIDATION_PHASE.with(|phase| phase.set(self.0));
    }
}

pub(crate) fn validation_phase(stage: NativePrepareStage) -> NativeValidationScope {
    NativeValidationScope(VALIDATION_PHASE.with(|phase| phase.replace(Some(stage))))
}

#[cfg(feature = "allocation-audit")]
thread_local! {
    static BUILD_ALLOCATION_FAULT: Cell<Option<u64>> = const { Cell::new(None) };
    static BUILD_ALLOCATION_FIRES: Cell<u64> = const { Cell::new(0) };
}

#[cfg(feature = "allocation-audit")]
pub(crate) struct NativeBuildAllocationScope;

#[cfg(feature = "allocation-audit")]
impl NativeBuildAllocationScope {
    pub(crate) fn fires(&self) -> u64 {
        BUILD_ALLOCATION_FIRES.with(Cell::get)
    }
}

#[cfg(feature = "allocation-audit")]
impl Drop for NativeBuildAllocationScope {
    fn drop(&mut self) {
        BUILD_ALLOCATION_FAULT.with(|fault| fault.set(None));
        BUILD_ALLOCATION_FIRES.with(|fires| fires.set(0));
    }
}

#[cfg(feature = "allocation-audit")]
pub(crate) fn install_build_allocation_failure(ordinal: u64) -> NativeBuildAllocationScope {
    assert!(ordinal > 0);
    BUILD_ALLOCATION_FAULT.with(|fault| {
        assert!(
            fault.get().is_none(),
            "native build allocation schedule already installed"
        );
        fault.set(Some(ordinal));
    });
    BUILD_ALLOCATION_FIRES.with(|fires| fires.set(0));
    NativeBuildAllocationScope
}

#[cfg(feature = "allocation-audit")]
pub(crate) fn with_build_allocation_schedule<T>(build: impl FnOnce() -> T) -> T {
    let Some(ordinal) = BUILD_ALLOCATION_FAULT.with(Cell::get) else {
        return build();
    };
    let (result, fires) = crate::allocation_audit::fail_attributed_allocation(ordinal, build);
    BUILD_ALLOCATION_FIRES.with(|count| count.set(count.get() + fires));
    result
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PhysicalReadOrigin {
    Query,
    Preparation,
}

#[derive(Clone, Copy)]
struct PhysicalReadEvent {
    origin: PhysicalReadOrigin,
    reference: crate::property_graph::storage::artifact::PhysicalRef,
}

struct PhysicalReadState {
    events: Vec<PhysicalReadEvent>,
    paused: usize,
}

thread_local! {
    static PHYSICAL_READS: RefCell<Option<PhysicalReadState>> = const { RefCell::new(None) };
}

struct PhysicalReadScope;
impl PhysicalReadScope {
    fn start() -> Self {
        PHYSICAL_READS.with(|slot| {
            let mut slot = slot.borrow_mut();
            assert!(
                slot.is_none(),
                "physical read observation already installed"
            );
            *slot = Some(PhysicalReadState {
                events: Vec::new(),
                paused: 0,
            });
        });
        Self
    }
    fn finish(self) -> Vec<PhysicalReadEvent> {
        PHYSICAL_READS.with(|slot| {
            slot.borrow_mut()
                .take()
                .map_or_else(Vec::new, |state| state.events)
        })
    }
}
impl Drop for PhysicalReadScope {
    fn drop(&mut self) {
        PHYSICAL_READS.with(|slot| *slot.borrow_mut() = None);
    }
}

struct PhysicalReadPause(bool);
impl PhysicalReadPause {
    fn fixture_introspection() -> Self {
        Self(PHYSICAL_READS.with(|slot| {
            if let Some(state) = slot.borrow_mut().as_mut() {
                state.paused += 1;
                true
            } else {
                false
            }
        }))
    }
}
impl Drop for PhysicalReadPause {
    fn drop(&mut self) {
        if self.0 {
            PHYSICAL_READS.with(|slot| {
                if let Some(state) = slot.borrow_mut().as_mut() {
                    state.paused -= 1;
                }
            });
        }
    }
}

pub(crate) fn observe_physical_read(
    origin: PhysicalReadOrigin,
    reference: crate::property_graph::storage::artifact::PhysicalRef,
) {
    PHYSICAL_READS.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut()
            && state.paused == 0
        {
            state.events.push(PhysicalReadEvent { origin, reference });
        }
    });
}

/// Successful mapped-block resolutions for one independently bound source/index pair.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhysicalReadReceipt {
    /// Source-manifest reference admitted before the measured query.
    pub source: crate::property_graph::storage::artifact::PhysicalRef,
    /// Direct-index reference admitted before the measured query.
    pub index: crate::property_graph::storage::artifact::PhysicalRef,
    /// Actual successful source-manifest resolutions in the measured query.
    pub source_resolutions: usize,
    /// Actual successful direct-index resolutions in the measured query.
    pub index_resolutions: usize,
}

/// Exact physical-reference observations, excluding fixture descendant enumeration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhysicalReadReport {
    /// Independent source/index bindings paired with actual resolution counts.
    pub receipts: Vec<PhysicalReadReceipt>,
    /// All direct-index resolutions, including any unexpected index reference.
    pub index_resolutions: usize,
}

fn query_read_report(
    expected: &[(
        crate::property_graph::storage::artifact::PhysicalRef,
        crate::property_graph::storage::artifact::PhysicalRef,
    )],
    events: &[PhysicalReadEvent],
) -> Result<PhysicalReadReport, String> {
    use crate::property_graph::storage::artifact::BlockKind;
    let receipts = expected
        .iter()
        .map(|(source, index)| PhysicalReadReceipt {
            source: *source,
            index: *index,
            source_resolutions: events
                .iter()
                .filter(|event| {
                    event.origin == PhysicalReadOrigin::Query && event.reference == *source
                })
                .count(),
            index_resolutions: events
                .iter()
                .filter(|event| {
                    event.origin == PhysicalReadOrigin::Query && event.reference == *index
                })
                .count(),
        })
        .collect::<Vec<_>>();
    let index_resolutions = events
        .iter()
        .filter(|event| {
            event.origin == PhysicalReadOrigin::Query
                && event.reference.kind == BlockKind::RetrievalVectorIndex
        })
        .count();
    if expected.is_empty()
        || expected.iter().any(|(source, index)| {
            source.kind != BlockKind::CommitParticipant
                || index.kind != BlockKind::RetrievalVectorIndex
        })
        || expected
            .iter()
            .enumerate()
            .any(|(position, (source, index))| {
                expected
                    .iter()
                    .take(position)
                    .any(|(prior_source, prior_index)| {
                        prior_source == source || prior_index == index
                    })
            })
        || receipts
            .iter()
            .any(|receipt| receipt.source_resolutions != 1 || receipt.index_resolutions != 1)
        || index_resolutions != expected.len()
        || events
            .iter()
            .any(|event| event.origin != PhysicalReadOrigin::Query)
    {
        return Err(format!(
            "physical source/index resolution receipts mismatch: {receipts:?}; total direct-index resolutions {index_resolutions}"
        ));
    }
    Ok(PhysicalReadReport {
        receipts,
        index_resolutions,
    })
}

pub(crate) struct NativePrepareScope;

impl Drop for NativePrepareScope {
    fn drop(&mut self) {
        SCHEDULE.with(|slot| {
            *slot.borrow_mut() = None;
        });
    }
}

pub(crate) fn install(
    storage_limit: Option<usize>,
    work_limit: Option<u64>,
    observer: impl FnMut(NativePrepareEvent) + 'static,
) -> NativePrepareScope {
    SCHEDULE.with(|slot| {
        let mut slot = slot.borrow_mut();
        assert!(
            slot.is_none(),
            "native preparation schedule is already installed"
        );
        *slot = Some(Schedule {
            storage_limit,
            work_limit,
            observer: Box::new(observer),
        });
    });
    NativePrepareScope
}

pub(crate) fn limits(storage_default: usize, work_default: u64) -> (usize, u64) {
    SCHEDULE.with(|slot| {
        slot.borrow()
            .as_ref()
            .map_or((storage_default, work_default), |schedule| {
                (
                    schedule.storage_limit.map_or(storage_default, |limit| {
                        assert!(limit <= storage_default);
                        limit
                    }),
                    schedule.work_limit.map_or(work_default, |limit| {
                        assert!(limit <= work_default);
                        limit
                    }),
                )
            })
    })
}

pub(crate) fn observe(mut event: NativePrepareEvent) {
    if let Some(stage) = VALIDATION_PHASE.with(Cell::get) {
        event.stage = stage;
    }
    SCHEDULE.with(|slot| {
        if let Some(schedule) = slot.borrow_mut().as_mut() {
            (schedule.observer)(event);
        }
    });
}

#[cfg(any(test, all(feature = "graph-cypher", feature = "test-seams")))]
mod kernel_probe {
    use super::{
        NativePrepareEvent, NativePrepareStage, PhysicalReadOrigin, PhysicalReadPause,
        PhysicalReadReport, PhysicalReadScope, query_read_report,
    };
    use crate::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower, Normalization};
    use crate::graph::search::{
        FilteredGraphSearchOutcome, GraphSearchRequest, GraphSearchScratch, GraphSearcher,
    };
    use crate::lifecycle::durability::{CommitTier, DurabilityMode};
    use crate::lifecycle::native_graph::NativeReadConsumer;
    use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
    use crate::meta::DocBitmap;
    use crate::property_graph::query::runtime::{RuntimeContext, RuntimeLimits};
    use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
    use crate::property_graph::storage::GraphReadView;
    use crate::property_graph::storage::artifact::PhysicalRef;
    use crate::property_graph::storage::payload::PayloadRef;
    use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};
    use crate::property_graph::{
        ApplicationKey, CanonicalContents, CanonicalEmbedding, EntityId, EntityKind, GraphRevision,
        NodeId,
    };
    use crate::quant::{est_dot_bit4, prepare_bit4_query};
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use xxhash_rust::xxh3::xxh3_64;

    const KERNEL_QUERY_SEED: u64 = 7;
    const KERNEL_REPORT_KEY: &str = "property-graph.native-vector-index.kernel";
    const SMALL_WRITES_QUERY_SEED: u64 = 0x158;
    const SMALL_WRITES_REPORT_KEY: &str = "property-graph.native-vector-index.small-writes";
    static NEXT_PROBE_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn document_tower() -> EmbeddingTower {
        EmbeddingTower {
            model_id: "ze158-document".into(),
            model_version: "1".into(),
            weights_digest: vec![0x15, 0x8a],
            dims: 2,
            normalization: Normalization::None,
            prompt_prefix: "doc: ".into(),
            max_tokens: 32,
            runtime: EmbeddingRuntime::CpuReference,
            compute_units: ComputeUnits::Cpu,
            os_build: None,
        }
    }

    pub(crate) fn native_options() -> OpenOptions {
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024)
    }

    pub(crate) fn apply_vector_checked(
        store: &Store,
        document: &EmbeddingTower,
        key: &str,
        revision: u64,
        operation: StructuredOperation,
        coordinates: &[f32; 2],
    ) -> Result<NodeId, String> {
        let embedding =
            CanonicalEmbedding::new(document, coordinates).map_err(|error| error.to_string())?;
        let node = CanonicalContents::node(&mut [], &mut [], None, Some(embedding))
            .map_err(|error| error.to_string())?;
        let request = [StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", key)
                .map_err(|error| error.to_string())?,
            revision: GraphRevision::new(revision).map_err(|error| error.to_string())?,
            operation,
            image: Some(WriteImage::Node(&node)),
        }];
        let receipts = store
            .apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new()))
            .map_err(|error| error.to_string())?;
        if receipts.len() != 1 {
            return Err("vector write receipt count".into());
        }
        let receipt = receipts.first().ok_or("missing vector write receipt")?;
        match receipt.entity {
            EntityId::Node(node) => Ok(node),
            EntityId::Relationship(_) => Err("vector write changed identity domain".into()),
        }
    }

    pub(crate) fn apply_repeated_vectors_checked(
        store: &Store,
        document: &EmbeddingTower,
        prefix: &str,
        count: usize,
        coordinates: &[f32],
    ) -> Result<Vec<NodeId>, String> {
        let embedding =
            CanonicalEmbedding::new(document, coordinates).map_err(|error| error.to_string())?;
        let node = CanonicalContents::node(&mut [], &mut [], None, Some(embedding))
            .map_err(|error| error.to_string())?;
        let keys = (0..count)
            .map(|index| format!("{prefix}-{index:04}"))
            .collect::<Vec<_>>();
        let requests = keys
            .iter()
            .map(|key| {
                Ok(StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "app", key)
                        .map_err(|error| error.to_string())?,
                    revision: GraphRevision::new(1).map_err(|error| error.to_string())?,
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&node)),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let receipts = store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .map_err(|error| error.to_string())?;
        if receipts.len() != count {
            return Err("vector batch receipt count".into());
        }
        receipts
            .iter()
            .map(|receipt| match receipt.entity {
                EntityId::Node(node) => Ok(node),
                EntityId::Relationship(_) => Err("vector batch changed identity domain".into()),
            })
            .collect()
    }

    pub(crate) fn kernel_coordinates() -> [f32; 2] {
        [f32::from_bits(0x3f80_0001), f32::from_bits(0x8000_0000)]
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) struct KernelObservation {
        pub(crate) observed_identity: u128,
        pub(crate) observed_coordinate_bits: [u32; 2],
        pub(crate) score_bits: u32,
        pub(crate) hit_identity: u128,
        pub(crate) hit_row: u32,
        pub(crate) distance_bits: u64,
        pub(crate) visited: usize,
    }

    struct RequireVectorIndex {
        node: NodeId,
        coordinates: [f32; 2],
    }

    impl NativeReadConsumer<KernelObservation> for RequireVectorIndex {
        fn consume<'s, 'lease, 'm, 'g>(
            &mut self,
            view: &GraphReadView<'s, 'lease, 'm, 'g>,
            runtime: &mut RuntimeContext<'lease, 'm, 'g>,
        ) -> Result<KernelObservation, TreeError> {
            let sparse = view.sparse_view(runtime)?;
            let mut resources = TreeResources::for_query(runtime)?;
            let mut sources =
                sparse.sources(super::super::super::Modality::Vector, &mut resources)?;
            let mut target = None;
            while let Some(source) = sources.next(&mut resources)? {
                let index = source
                    .vector_index(&mut resources)?
                    .ok_or(TreeError::Invalid("missing native vector index"))?;
                if index.row_count() == 1
                    && index.identity(0)? == (self.node, 1)
                    && target.replace(index).is_some()
                {
                    return Err(TreeError::Invalid("duplicate native vector target source"));
                }
            }
            let index = target.ok_or(TreeError::Invalid("missing native vector target source"))?;
            let (observed_node, observed_revision) = index.identity(0)?;
            let observed_coordinate_bits = [
                index.coordinate(0, 0)?.to_bits(),
                index.coordinate(0, 1)?.to_bits(),
            ];
            if (observed_node, observed_revision) != (self.node, 1)
                || observed_coordinate_bits
                    != [self.coordinates[0].to_bits(), self.coordinates[1].to_bits()]
            {
                return Err(TreeError::Invalid(
                    "native vector literal identity or coordinates",
                ));
            }
            let prepared = prepare_bit4_query(&self.coordinates, KERNEL_QUERY_SEED)
                .map_err(|_| TreeError::Invalid("native vector query quantization"))?;
            let coarse = est_dot_bit4(&prepared, index.code(0)?, index.factors(0)?)
                .map_err(|_| TreeError::Invalid("native vector Bit4 kernel"))?;
            if coarse.to_bits() != 0x3f80_0002 {
                return Err(TreeError::Invalid("native vector Bit4 literal score"));
            }
            let graph = index.graph()?;
            let mut scratch =
                GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
                    .map_err(|_| TreeError::Invalid("native vector graph scratch"))?;
            let mut searcher = GraphSearcher::new(graph, index.rescore(), &mut scratch)
                .map_err(|_| TreeError::Invalid("native vector graph binding"))?;
            let result = searcher
                .search(
                    GraphSearchRequest::new(&self.coordinates, 1, KERNEL_QUERY_SEED),
                    None,
                )
                .map_err(|_| TreeError::Invalid("native vector graph kernel"))?;
            let hit = result
                .candidates()
                .first()
                .ok_or(TreeError::Invalid("missing native vector graph hit"))?;
            let (hit_node, hit_revision) = index.identity(hit.row_id())?;
            if hit.row_id() != 0
                || (hit_node, hit_revision) != (self.node, 1)
                || hit.distance().to_bits() != 0.0_f64.to_bits()
                || result.counters().visited() != 1
            {
                return Err(TreeError::Invalid("native vector graph literal result"));
            }
            Ok(KernelObservation {
                observed_identity: observed_node.get(),
                observed_coordinate_bits,
                score_bits: coarse.to_bits(),
                hit_identity: hit_node.get(),
                hit_row: hit.row_id(),
                distance_bits: hit.distance().to_bits(),
                visited: result.counters().visited(),
            })
        }
    }

    pub(crate) fn read_kernel(store: &Store, node: NodeId) -> Result<KernelObservation, String> {
        store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                8 * 1024 * 1024,
                32,
                RequireVectorIndex {
                    node,
                    coordinates: kernel_coordinates(),
                },
            )
            .map_err(|error| error.to_string())
    }

    pub(crate) fn write_and_read_kernel(
        store: &Store,
        document: &EmbeddingTower,
    ) -> Result<(NodeId, KernelObservation, PhysicalReadReport), String> {
        let coordinates = kernel_coordinates();
        let embedding =
            CanonicalEmbedding::new(document, &coordinates).map_err(|error| error.to_string())?;
        let node = CanonicalContents::node(&mut [], &mut [], None, Some(embedding))
            .map_err(|error| error.to_string())?;
        let request = [StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "one")
                .map_err(|error| error.to_string())?,
            revision: GraphRevision::new(1).map_err(|error| error.to_string())?,
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&node)),
        }];
        let receipts = store
            .apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new()))
            .map_err(|error| error.to_string())?;
        if receipts.len() != 1 {
            return Err("native vector kernel write receipt count".into());
        }
        let receipt = receipts
            .first()
            .ok_or_else(|| "missing native vector kernel write receipt".to_owned())?;
        let node = match receipt.entity {
            EntityId::Node(node) => node,
            EntityId::Relationship(_) => {
                return Err("vector receipt changed identity domain".into());
            }
        };
        let descriptors = inspect_sources_checked(store, coordinates)?;
        if descriptors.len() != 1
            || descriptors
                .first()
                .is_none_or(|source| source.identities != [(node, 1)])
        {
            return Err("kernel independent source descriptor binding".into());
        }
        let expected = descriptors
            .iter()
            .map(|source| (source.source_reference, source.index_reference))
            .collect::<Vec<_>>();
        let reads = PhysicalReadScope::start();
        let observation = read_kernel(store, node);
        let events = reads.finish();
        let observation = observation?;
        let reads = query_read_report(&expected, &events)?;
        Ok((node, observation, reads))
    }

    pub(super) struct ProbeDirectory {
        path: PathBuf,
    }

    impl ProbeDirectory {
        pub(super) fn create(seed: u64) -> Result<Self, String> {
            let ordinal = NEXT_PROBE_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "zeppelin-native-vector-kernel-{}-{seed:016x}-{ordinal:016x}",
                std::process::id()
            ));
            std::fs::create_dir(&path).map_err(|error| {
                format!(
                    "create native vector kernel probe {}: {error}",
                    path.display()
                )
            })?;
            Ok(Self { path })
        }

        pub(super) fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for ProbeDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[derive(Clone, Debug)]
    pub(crate) struct SourceReport {
        pub(crate) source_reference: PhysicalRef,
        pub(crate) rows: u32,
        pub(crate) live_rows: usize,
        pub(crate) identities: Vec<(NodeId, u64)>,
        #[cfg(test)]
        pub(crate) live_identities: Vec<(NodeId, u64)>,
        pub(crate) fingerprint: u64,
        pub(crate) seed_count: usize,
        pub(crate) seed_rows: Vec<u32>,
        pub(crate) entry_points: Vec<u32>,
        pub(crate) visited: usize,
        pub(crate) filtered_rows: Vec<u32>,
        pub(crate) index_bytes: Vec<u8>,
        pub(crate) dimensions: u32,
        pub(crate) profile: u8,
        pub(crate) max_degree: u8,
        pub(crate) index_reference: PhysicalRef,
        pub(crate) index_payload: PayloadRef,
        pub(crate) index_physical_references: Vec<PhysicalRef>,
        pub(crate) index_catalog: crate::property_graph::wal::RequiredRef,
        pub(crate) index_catalog_block: PhysicalRef,
        pub(crate) coordinate_bits: Vec<u32>,
    }

    pub(crate) struct InspectVectorSources {
        pub(crate) query: [f32; 2],
    }

    impl NativeReadConsumer<Vec<SourceReport>> for InspectVectorSources {
        fn consume<'s, 'lease, 'm, 'g>(
            &mut self,
            view: &GraphReadView<'s, 'lease, 'm, 'g>,
            runtime: &mut RuntimeContext<'lease, 'm, 'g>,
        ) -> Result<Vec<SourceReport>, TreeError> {
            let sparse = view.sparse_view(runtime)?;
            let mut resources = TreeResources::for_query(runtime)?;
            let mut sources =
                sparse.sources(super::super::super::Modality::Vector, &mut resources)?;
            let mut reports = Vec::new();
            while let Some(source) = sources.next(&mut resources)? {
                let index = source
                    .vector_index(&mut resources)?
                    .ok_or(TreeError::Invalid("vector source lacks native index"))?;
                let graph = index.graph()?;
                let index_physical_references = {
                    // Descriptor introspection is outside the measured actual query work.
                    let _pause = PhysicalReadPause::fixture_introspection();
                    source.vector_index_physical_references_for_test(&mut resources)?
                };
                let mut identities = Vec::new();
                #[cfg(test)]
                let mut live_identities = Vec::new();
                let mut live_rows = 0;
                let mut seed_count = 0;
                let mut seed_rows = Vec::new();
                let mut has_edge = false;
                let mut coordinate_bits = Vec::new();
                for row in 0..index.row_count() {
                    let identity = index.identity(row)?;
                    identities.push(identity);
                    for dimension in 0..index.dimensions() {
                        coordinate_bits.push(index.coordinate(row, dimension)?.to_bits());
                    }
                    if source.is_live(row, &mut resources)? {
                        live_rows += 1;
                        #[cfg(test)]
                        live_identities.push(identity);
                    }
                    let block = graph
                        .block(row)
                        .map_err(|_| TreeError::Invalid("native report graph row"))?;
                    seed_count += usize::from(block.flags() & 1 != 0);
                    if block.flags() & 1 != 0 {
                        seed_rows.push(row);
                    }
                    let mut seen = Vec::new();
                    for neighbor in block.neighbors_padded().take(usize::from(block.degree())) {
                        if neighbor >= index.row_count() || seen.contains(&neighbor) {
                            return Err(TreeError::Invalid("native report graph neighbor"));
                        }
                        seen.push(neighbor);
                        has_edge = true;
                    }
                }
                let run_single =
                    index.row_count() == 1 && index.dimensions() as usize == self.query.len();
                let (visited, filtered_rows) = if index.row_count() >= 32 || run_single {
                    if index.row_count() >= 32 && !has_edge {
                        return Err(TreeError::Invalid("nontrivial native graph has no edges"));
                    }
                    let eligible = index.row_count() - 1;
                    let allow = DocBitmap::from_ids([eligible]);
                    let mut scratch =
                        GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
                            .map_err(|_| TreeError::Invalid("native report graph scratch"))?;
                    let mut searcher = GraphSearcher::new(graph, index.rescore(), &mut scratch)
                        .map_err(|_| TreeError::Invalid("native report graph binding"))?;
                    let outcome = searcher
                        .search_filtered(
                            GraphSearchRequest::new(&self.query, 1, 0x158)
                                .with_ef((index.row_count() as usize).min(32)),
                            &allow,
                            usize::MAX,
                            None,
                        )
                        .map_err(|_| TreeError::Invalid("native report filtered graph kernel"))?;
                    let FilteredGraphSearchOutcome::Traversed(result) = outcome else {
                        return Err(TreeError::Invalid("native report filtered budget"));
                    };
                    let rows = result
                        .candidates()
                        .iter()
                        .map(|candidate| candidate.row_id())
                        .collect::<Vec<_>>();
                    if rows.iter().any(|row| *row != eligible) {
                        return Err(TreeError::Invalid("native report leaked filtered row"));
                    }
                    (result.counters().visited(), rows)
                } else {
                    (0, Vec::new())
                };
                reports.push(SourceReport {
                    source_reference: source.source_reference_for_test(),
                    rows: index.row_count(),
                    live_rows,
                    identities,
                    #[cfg(test)]
                    live_identities,
                    fingerprint: xxh3_64(index.encoded_bytes()),
                    seed_count,
                    seed_rows,
                    entry_points: index.seed_row_ids().to_vec(),
                    visited,
                    filtered_rows,
                    index_bytes: index.encoded_bytes().to_vec(),
                    dimensions: index.dimensions(),
                    profile: index.profile_tag(),
                    max_degree: graph.layout().max_degree(),
                    index_reference: index.payload_reference().reference(),
                    index_payload: index.payload_reference(),
                    index_physical_references,
                    index_catalog: index.interpretation_catalog(),
                    index_catalog_block: index.interpretation_catalog().block,
                    coordinate_bits,
                });
            }
            Ok(reports)
        }
    }

    pub(crate) fn inspect_sources_checked(
        store: &Store,
        query: [f32; 2],
    ) -> Result<Vec<SourceReport>, String> {
        store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                8 * 1024 * 1024,
                32,
                InspectVectorSources { query },
            )
            .map_err(|error| error.to_string())
    }

    /// One actual source inspected by the small-write probe.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct SmallWriteSourceObservation {
        pub rows: u32,
        pub live_rows: usize,
        pub identities: Vec<(u128, u64)>,
        pub coordinate_bits: Vec<u32>,
        pub seed_count: usize,
        pub visited: usize,
        pub filtered_rows: Vec<u32>,
    }

    /// Checked receipts from separate writes and real filtered traversal.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct SmallWritesProbeReport {
        pub key: &'static str,
        pub seed: u64,
        pub query_seed: u64,
        pub separate_receipts: Vec<u128>,
        pub cohort_receipts: Vec<u128>,
        pub sources: Vec<SmallWriteSourceObservation>,
        pub prepared_images_per_apply: [usize; 5],
        pub preparation_index_resolutions: [usize; 5],
        pub query_reads: PhysicalReadReport,
        pub query_prepare_events: usize,
    }

    pub(crate) struct SmallWritesFixture {
        #[cfg(test)]
        pub(crate) separate: Vec<NodeId>,
        #[cfg(test)]
        pub(crate) before: Vec<SourceReport>,
        pub(crate) report: SmallWritesProbeReport,
    }

    pub(crate) fn prepare_small_writes_fixture(
        store: &Store,
        document: &EmbeddingTower,
        seed: u64,
    ) -> Result<SmallWritesFixture, String> {
        let coordinates = [0.25_f32, -0.5_f32];
        let events = Rc::new(RefCell::new(Vec::<NativePrepareEvent>::new()));
        let mut prepared_images_per_apply = [0_usize; 5];
        let mut preparation_index_resolutions = [0_usize; 5];
        let mut separate = Vec::new();
        for (index, image_count) in prepared_images_per_apply.iter_mut().take(4).enumerate() {
            events.borrow_mut().clear();
            let observed = Rc::clone(&events);
            let scope = super::install(None, None, move |event| observed.borrow_mut().push(event));
            let reads = PhysicalReadScope::start();
            let result = apply_vector_checked(
                store,
                document,
                &format!("separate-{index}"),
                1,
                StructuredOperation::Create,
                &coordinates,
            );
            drop(scope);
            let read_events = reads.finish();
            *preparation_index_resolutions
                .get_mut(index)
                .ok_or_else(|| "preparation slot missing".to_owned())? = read_events.iter().filter(|event| event.origin == PhysicalReadOrigin::Preparation && event.reference.kind == crate::property_graph::storage::artifact::BlockKind::RetrievalVectorIndex).count();
            separate.push(result?);
            *image_count = events
                .borrow()
                .iter()
                .filter(|event| event.stage == NativePrepareStage::ImageAllocation)
                .count();
        }
        events.borrow_mut().clear();
        let observed = Rc::clone(&events);
        let scope = super::install(None, None, move |event| observed.borrow_mut().push(event));
        let reads = PhysicalReadScope::start();
        let result = apply_repeated_vectors_checked(store, document, "cohort", 32, &coordinates);
        drop(scope);
        let read_events = reads.finish();
        *preparation_index_resolutions
            .last_mut()
            .ok_or("cohort read receipt")? = read_events
            .iter()
            .filter(|event| {
                event.origin == PhysicalReadOrigin::Preparation
                    && event.reference.kind
                        == crate::property_graph::storage::artifact::BlockKind::RetrievalVectorIndex
            })
            .count();
        let cohort = result?;
        *prepared_images_per_apply
            .last_mut()
            .ok_or("small-write image receipt")? = events
            .borrow()
            .iter()
            .filter(|event| event.stage == NativePrepareStage::ImageAllocation)
            .count();
        let descriptors = inspect_sources_checked(store, coordinates)?;
        let expected_references = descriptors
            .iter()
            .map(|source| (source.source_reference, source.index_reference))
            .collect::<Vec<_>>();
        events.borrow_mut().clear();
        let observed = Rc::clone(&events);
        let scope = super::install(None, None, move |event| observed.borrow_mut().push(event));
        let reads = PhysicalReadScope::start();
        let result = inspect_sources_checked(store, coordinates);
        drop(scope);
        let read_events = reads.finish();
        let before = result?;
        let query_reads = query_read_report(&expected_references, &read_events)?;
        let query_prepare_events = events.borrow().len();
        let mut expected = separate
            .iter()
            .chain(&cohort)
            .map(|node| (node.get(), 1))
            .collect::<Vec<_>>();
        expected.sort_unstable();
        let sources = before
            .iter()
            .map(|source| SmallWriteSourceObservation {
                rows: source.rows,
                live_rows: source.live_rows,
                identities: source
                    .identities
                    .iter()
                    .map(|(node, revision)| (node.get(), *revision))
                    .collect(),
                coordinate_bits: source.coordinate_bits.clone(),
                seed_count: source.seed_count,
                visited: source.visited,
                filtered_rows: source.filtered_rows.clone(),
            })
            .collect::<Vec<_>>();
        let mut actual = sources
            .iter()
            .flat_map(|source| source.identities.iter().copied())
            .collect::<Vec<_>>();
        actual.sort_unstable();
        if expected.len() != 36
            || expected.iter().copied().collect::<BTreeSet<_>>().len() != 36
            || actual != expected
            || sources.len() != 5
            || sources.iter().filter(|source| source.rows == 1).count() != 4
            || prepared_images_per_apply != [1; 5]
            || preparation_index_resolutions != [0; 5]
            || query_prepare_events != 0
        {
            return Err("small-write source partition or build receipt mismatch".into());
        }
        for source in &sources {
            let expected_bits = (0..source.rows)
                .flat_map(|_| [0x3e80_0000, 0xbf00_0000])
                .collect::<Vec<_>>();
            let expected_ids = if source.rows == 1 {
                if !separate
                    .iter()
                    .any(|node| source.identities == [(node.get(), 1)])
                {
                    return Err("small-write singleton identity mismatch".into());
                }
                source.identities.clone()
            } else {
                cohort.iter().map(|node| (node.get(), 1)).collect()
            };
            if source.live_rows != source.rows as usize
                || source.coordinate_bits != expected_bits
                || source.identities != expected_ids
                || source.seed_count != (source.rows as usize).min(4)
                || (source.rows == 1 && (source.visited != 1 || source.filtered_rows != [0]))
                || (source.rows != 1
                    && (source.rows != 32 || source.visited <= 4 || source.filtered_rows != [31]))
            {
                return Err("small-write real graph observation mismatch".into());
            }
        }
        Ok(SmallWritesFixture {
            #[cfg(test)]
            separate: separate.clone(),
            #[cfg(test)]
            before,
            report: SmallWritesProbeReport {
                key: SMALL_WRITES_REPORT_KEY,
                seed,
                query_seed: SMALL_WRITES_QUERY_SEED,
                separate_receipts: separate.iter().map(|node| node.get()).collect(),
                cohort_receipts: cohort.iter().map(|node| node.get()).collect(),
                sources,
                prepared_images_per_apply,
                preparation_index_resolutions,
                query_reads,
                query_prepare_events,
            },
        })
    }

    /// Runs only the shared small-write fixture in an owned scratch store.
    pub fn run_small_writes_probe(seed: u64) -> Result<SmallWritesProbeReport, String> {
        let directory = ProbeDirectory::create(seed)?;
        let document = document_tower();
        let store = Store::create_native_graph(
            directory.path().join("native"),
            native_options(),
            Some(document.clone()),
        )
        .map_err(|error| error.to_string())?;
        let result = prepare_small_writes_fixture(&store, &document, seed);
        let close = store.close().map_err(|error| error.to_string());
        let fixture = result?;
        close?;
        Ok(fixture.report)
    }

    /// Actual native vector kernel observation returned to the shared runner.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct KernelProbeReport {
        /// Stable shared-runner coverage key.
        pub key: &'static str,
        /// Caller-provided deterministic probe seed.
        pub seed: u64,
        /// Seed passed to real Bit4 preparation and graph search.
        pub query_seed: u64,
        /// Identity allocated and returned by the write receipt.
        pub expected_identity: u128,
        /// Identity decoded independently from the persisted index.
        pub observed_identity: u128,
        /// Literal coordinate bits provided to the write.
        pub expected_coordinate_bits: [u32; 2],
        /// Coordinate bits decoded independently from the persisted index.
        pub observed_coordinate_bits: [u32; 2],
        /// Actual Bit4 score bits.
        pub score_bits: u32,
        /// Identity resolved from the actual graph-search hit row.
        pub hit_identity: u128,
        /// Actual graph-search hit row.
        pub hit_row: u32,
        /// Actual graph-search distance bits.
        pub distance_bits: u64,
        /// Actual graph-search visited count.
        pub visited: usize,
        /// Actual successful source/index physical-reference resolutions.
        pub query_reads: PhysicalReadReport,
    }

    /// Runs the real single-write native vector kernel in an owned scratch store.
    pub fn run_kernel_probe(seed: u64) -> Result<KernelProbeReport, String> {
        let directory = ProbeDirectory::create(seed)?;
        let path = directory.path().join("native");
        let document = document_tower();
        let store = Store::create_native_graph(&path, native_options(), Some(document.clone()))
            .map_err(|error| error.to_string())?;
        let result = write_and_read_kernel(&store, &document);
        let close_result = store.close().map_err(|error| error.to_string());
        let (node, observed, query_reads) = result?;
        close_result?;
        let expected_coordinate_bits = [0x3f80_0001, 0x8000_0000];
        if node.get() != observed.observed_identity
            || node.get() != observed.hit_identity
            || expected_coordinate_bits != observed.observed_coordinate_bits
            || observed.score_bits != 0x3f80_0002
            || observed.hit_row != 0
            || observed.distance_bits != 0.0_f64.to_bits()
            || observed.visited != 1
        {
            return Err("native vector kernel probe observation mismatch".into());
        }
        Ok(KernelProbeReport {
            key: KERNEL_REPORT_KEY,
            seed,
            query_seed: KERNEL_QUERY_SEED,
            expected_identity: node.get(),
            observed_identity: observed.observed_identity,
            expected_coordinate_bits,
            observed_coordinate_bits: observed.observed_coordinate_bits,
            score_bits: observed.score_bits,
            hit_identity: observed.hit_identity,
            hit_row: observed.hit_row,
            distance_bits: observed.distance_bits,
            visited: observed.visited,
            query_reads,
        })
    }
}

#[cfg(test)]
pub(crate) use kernel_probe::{
    InspectVectorSources, SourceReport, apply_vector_checked, inspect_sources_checked,
    kernel_coordinates, prepare_small_writes_fixture, read_kernel, write_and_read_kernel,
};
#[cfg(any(test, all(feature = "graph-cypher", feature = "test-seams")))]
pub use kernel_probe::{
    KernelProbeReport, SmallWriteSourceObservation, SmallWritesProbeReport, run_kernel_probe,
    run_small_writes_probe,
};
#[cfg(any(test, all(feature = "graph-cypher", feature = "test-seams")))]
pub(crate) use kernel_probe::{apply_repeated_vectors_checked, document_tower, native_options};

#[cfg(any(test, all(feature = "graph-cypher", feature = "test-seams")))]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod actual_cases {
    use super::install;
    use super::kernel_probe::{
        ProbeDirectory, SourceReport, apply_vector_checked, inspect_sources_checked,
    };
    use super::{document_tower, native_options};
    use crate::epoch::EmbeddingTower;
    use crate::lifecycle::{CancelToken, QueryControl, Store};
    use crate::property_graph::resources::GraphResources;
    use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
    use crate::property_graph::storage::artifact::{BlockKind, PhysicalRef};
    use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};
    use crate::property_graph::{
        ApplicationKey, EntityId, EntityKind, GraphName, GraphRevision, NodeId, NodeRef, RelId,
    };
    use crate::property_graph::{CanonicalContents, CanonicalEmbedding};
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    fn apply_vector(
        store: &Store,
        document: &EmbeddingTower,
        key: &str,
        revision: u64,
        operation: StructuredOperation,
        coordinates: &[f32; 2],
    ) -> NodeId {
        apply_vector_checked(store, document, key, revision, operation, coordinates)
            .expect("vector write")
    }
    fn inspect_sources(store: &Store, query: [f32; 2]) -> Vec<SourceReport> {
        inspect_sources_checked(store, query).expect("inspect vector sources")
    }

    /// Full-width identities and real source-local search observations.
    type IdentitySourceObservation = (Vec<(u128, u64)>, Vec<u32>, Vec<u32>, usize);

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct IdentityProbeReport {
        pub key: &'static str,
        pub seed: u64,
        pub write_identities: [u128; 3],
        pub relationship: u128,
        pub sources: Vec<IdentitySourceObservation>,
    }

    pub fn run_identity_probe(seed: u64) -> IdentityProbeReport {
        let directory = ProbeDirectory::create(seed).expect("temporary high-id store");
        let path = directory.path().join("native");
        let document = document_tower();
        let high = 1_u128 << 80;
        let first_node = NodeId::new(high - 1).expect("high first node");
        let first_relationship = RelId::new((1_u128 << 96) + 7).expect("high relationship");
        let store = Store::create_native_graph_with_allocator_seed_for_test(
            &path,
            native_options(),
            Some(document.clone()),
            first_node,
            first_relationship,
        )
        .expect("seeded native graph");
        let initial = store.admit_native_read().expect("initial seeded admission");
        assert_eq!(initial.bundle().high_waters().node, high - 2);
        assert_eq!(
            initial.bundle().high_waters().relationship,
            (1_u128 << 96) + 6
        );
        assert_eq!(initial.bundle().base().generation.get(), 0);
        assert_eq!(initial.bundle().sequence(), 0);
        drop(initial);
        store.close().expect("close seeded empty graph");
        drop(store);

        let store = Store::open_native_graph(&path, native_options(), Some(document.clone()))
            .expect("reopen seeded empty graph");
        let empty = store
            .admit_native_read()
            .expect("reopened seeded admission");
        assert_eq!(empty.bundle().high_waters().node, high - 2);
        assert_eq!(
            empty.bundle().high_waters().relationship,
            (1_u128 << 96) + 6
        );
        drop(empty);
        let signed_zero = [f32::from_bits(0x8000_0000), 0.5_f32];
        let second_vector = [0.25_f32, -0.75_f32];
        let left = apply_vector(
            &store,
            &document,
            "high-left",
            1,
            StructuredOperation::Create,
            &signed_zero,
        );
        let right = apply_vector(
            &store,
            &document,
            "high-right",
            1,
            StructuredOperation::Create,
            &second_vector,
        );
        assert_eq!((left.get(), right.get()), (high - 1, high));
        assert_ne!(left.get() >> 64, right.get() >> 64);
        let relationship = [StructuredWrite {
            key: ApplicationKey::new(EntityKind::Relationship, "app", "high-edge")
                .expect("relationship key"),
            revision: GraphRevision::new(1).expect("relationship revision"),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Relationship {
                source: NodeRef::Existing(left),
                target: NodeRef::Existing(right),
                relationship_type: GraphName::new("HIGH_EDGE").expect("relationship type"),
                properties: &[],
            }),
        }];
        let relationship = match store
            .apply_native_graph(&relationship, &QueryControl::Cancel(CancelToken::new()))
            .expect("high relationship write")[0]
            .entity
        {
            EntityId::Relationship(value) => value,
            EntityId::Node(_) => panic!("relationship changed identity domain"),
        };
        assert_eq!(relationship, first_relationship);
        let reports = inspect_sources(&store, signed_zero);
        let left_report = reports
            .iter()
            .find(|source| {
                source.identities == [(left, 1)]
                    && source.identities.first().is_some_and(|_| source.rows == 1)
            })
            .expect("H-1 kernel report");
        assert_eq!(
            left_report.coordinate_bits,
            signed_zero
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>()
        );
        assert!(left_report.visited > 0);
        let right_report = reports
            .iter()
            .find(|source| source.identities == [(right, 1)])
            .expect("H kernel report");
        assert_eq!(
            right_report.coordinate_bits,
            second_vector
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>()
        );
        assert!(right_report.visited > 0);
        let root = store.admit_native_read().expect("outer version admission");
        assert_eq!(root.bundle().root_envelope().object.version, 1);
        assert_eq!(root.bundle().root_envelope().block.version, 1);
        assert!(root
            .bundle()
            .vector()
            .is_some_and(|required| required.object.version == 1 && required.block.version == 1));
        drop(root);

        store.close().expect("close high-id graph");
        drop(store);
        let reopened = Store::open_native_graph(&path, native_options(), Some(document.clone()))
            .expect("reopen high-id graph");
        let state = reopened.admit_native_read().expect("high-id state");
        assert_eq!(state.bundle().high_waters().node, high);
        assert_eq!(
            state.bundle().high_waters().relationship,
            first_relationship.get()
        );
        drop(state);
        let next_coordinates = [0.5_f32, 0.5_f32];
        let next = apply_vector(
            &reopened,
            &document,
            "high-next",
            1,
            StructuredOperation::Create,
            &next_coordinates,
        );
        assert_eq!(next.get(), high + 1);
        assert!(
            inspect_sources(&reopened, next_coordinates)
                .iter()
                .any(|source| source.identities == [(next, 1)]
                    && source.coordinate_bits
                        == next_coordinates
                            .iter()
                            .map(|value| value.to_bits())
                            .collect::<Vec<_>>())
        );
        let final_sources = inspect_sources(&reopened, next_coordinates);
        reopened.close().expect("close reopened high-id graph");
        IdentityProbeReport {
            key: "property-graph.native-vector-index.identity",
            seed,
            write_identities: [left.get(), right.get(), next.get()],
            relationship: relationship.get(),
            sources: final_sources
                .iter()
                .map(|source| {
                    (
                        source
                            .identities
                            .iter()
                            .map(|(node, revision)| (node.get(), *revision))
                            .collect(),
                        source.coordinate_bits.clone(),
                        source.filtered_rows.clone(),
                        source.visited,
                    )
                })
                .collect(),
        }
    }
    /// Actual observations of the shared successful preparation control.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct CleanPreparationObservation {
        pub completed: bool,
        pub event_count: usize,
        pub max_chunk_units: u64,
        pub max_work_observed: u64,
        pub prepared_images: usize,
        pub allocation_reserved_bytes: usize,
        pub allocation_requested_bytes: usize,
        pub closed_reserved_bytes: u64,
    }
    /// A scheduled native failure with before/after state and measured ownership.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct NativeFailureObservation {
        pub fired: bool,
        pub kind: &'static str,
        pub generation_before: u64,
        pub generation_after: u64,
        pub sequence_before: u64,
        pub sequence_after: u64,
        pub baseline_reserved_bytes: u64,
        pub released_reserved_bytes: u64,
        pub closed_reserved_bytes: u64,
    }
    /// Close compares settled ownership against an identically closed clean store.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct CloseFailureObservation {
        pub closing_observed: bool,
        pub kind: &'static str,
        pub generation_before: u64,
        pub generation_after: u64,
        pub sequence_before: u64,
        pub sequence_after: u64,
        pub closed_reserved_bytes: u64,
    }
    /// Work and memory failures derived from actual clean preparation events.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct LimitProbeReport {
        pub key: &'static str,
        pub seed: u64,
        pub clean: CleanPreparationObservation,
        pub work_limit: u64,
        pub memory_limit: usize,
        pub work: NativeFailureObservation,
        pub memory: NativeFailureObservation,
    }
    /// Caller cancellation, real deadline, and acknowledged Closing observations.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct ControlProbeReport {
        pub key: &'static str,
        pub seed: u64,
        pub clean: CleanPreparationObservation,
        pub cancel: NativeFailureObservation,
        pub deadline: NativeFailureObservation,
        pub close: CloseFailureObservation,
    }

    fn observed_failure(
        result: &Result<(), crate::lifecycle::native_graph::NativeGraphError>,
    ) -> &'static str {
        use crate::lifecycle::QueryError;
        use crate::lifecycle::native_graph::NativeGraphError;
        match result {
            Err(NativeGraphError::Read(TreeError::Work)) => "work",
            Err(NativeGraphError::Read(TreeError::Memory)) => "memory",
            Err(NativeGraphError::Read(TreeError::Control(QueryError::Cancelled {
                partial: false,
            }))) => "cancelled",
            Err(NativeGraphError::Read(TreeError::Control(QueryError::Timeout {
                partial: false,
            }))) => "timeout",
            Err(NativeGraphError::Read(TreeError::Control(QueryError::ReadCancelled {
                partial: false,
            }))) => "read-cancelled",
            other => panic!("unexpected native preparation result: {other:?}"),
        }
    }

    pub(crate) fn try_apply_repeated_vectors(
        store: &Store,
        document: &EmbeddingTower,
        prefix: &str,
        count: usize,
        coordinates: &[f32],
        control: &QueryControl,
    ) -> Result<(), crate::lifecycle::native_graph::NativeGraphError> {
        let embedding = CanonicalEmbedding::new(document, coordinates).expect("embedding");
        let node = CanonicalContents::node(&mut [], &mut [], None, Some(embedding)).expect("node");
        let keys = (0..count)
            .map(|index| format!("{prefix}-{index:04}"))
            .collect::<Vec<_>>();
        let requests = keys
            .iter()
            .map(|key| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", key).expect("key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node)),
            })
            .collect::<Vec<_>>();
        store.apply_native_graph(&requests, control).map(|_| ())
    }

    /// Runs the existing native schedules once, sharing only their clean setup.
    pub fn run_preparation_schedule_probe(seed: u64) -> (LimitProbeReport, ControlProbeReport) {
        use crate::lifecycle::native_graph::NativeGraphError;
        use crate::lifecycle::{Deadline, ManualMonotonicClock, QueryError, StoreState};
        use crate::property_graph::storage::search::vector_index::test_support::{
            NativePrepareEvent, NativePrepareStage, install,
        };
        use std::sync::{Arc, Mutex, atomic::AtomicBool, atomic::Ordering, mpsc};
        use std::time::Duration;

        let odd_dimensions = 513_usize;
        let odd_row = (0..odd_dimensions)
            .map(|dimension| (dimension as f32 - 256.0) / 257.0)
            .collect::<Vec<_>>();
        let mut scheduled_document = document_tower();
        scheduled_document.model_id = "ze158-controlled-preparation".into();
        scheduled_document.dims = odd_dimensions as u32;
        let create_store = |name: &str| {
            let directory = ProbeDirectory::create(seed).expect("scheduled temporary store");
            let store = Store::create_native_graph(
                directory.path().join(name),
                native_options(),
                Some(scheduled_document.clone()),
            )
            .expect("scheduled fresh store");
            (directory, store)
        };
        let state = |store: &Store| {
            let lease = store
                .admit_native_read()
                .expect("scheduled state admission");
            let value = (lease.bundle().base(), lease.bundle().sequence());
            drop(lease);
            value
        };

        let (_clean_directory, clean_store) = create_store("clean");
        let clean_shared =
            GraphResources::from_store(&clean_store).expect("clean shared resources");
        let clean_events = Arc::new(Mutex::new(Vec::<NativePrepareEvent>::new()));
        let clean_sink = Arc::clone(&clean_events);
        let clean_scope = install(None, None, move |event| {
            clean_sink.lock().expect("clean event lock").push(event);
        });
        let clean_result = try_apply_repeated_vectors(
            &clean_store,
            &scheduled_document,
            "scheduled",
            4,
            &odd_row,
            &QueryControl::Cancel(CancelToken::new()),
        );
        let clean_completed = clean_result.is_ok();
        clean_result.expect("clean scheduled vector preparation");
        drop(clean_scope);
        let clean_events = clean_events.lock().expect("clean events").clone();
        for stage in [
            NativePrepareStage::Quantize,
            NativePrepareStage::QuantizeValidation,
            NativePrepareStage::QueryValidation,
            NativePrepareStage::Build,
            NativePrepareStage::OutputValidation,
            NativePrepareStage::CodeInitialization,
            NativePrepareStage::ImageAllocation,
            NativePrepareStage::ImageInitialization,
            NativePrepareStage::Serialization,
        ] {
            assert!(clean_events.iter().any(|event| event.stage == stage));
        }
        assert!(clean_events.iter().all(|event| event.units <= 256));
        let work_limit = clean_events
            .iter()
            .find(|event| event.stage == NativePrepareStage::Serialization)
            .expect("clean serialization event")
            .work_before;
        let image_allocation = clean_events
            .iter()
            .find(|event| event.stage == NativePrepareStage::ImageAllocation)
            .expect("clean image allocation event");
        assert!(image_allocation.reserved_before > 0);
        let memory_limit = image_allocation
            .reserved_before
            .checked_add(image_allocation.requested_bytes)
            .and_then(|limit| limit.checked_sub(1))
            .expect("later allocation limit");
        clean_store.close().expect("close clean scheduled store");
        let clean_closed = clean_shared
            .reserved_bytes()
            .expect("clean closed accounting");

        let (_cancel_directory, cancel_store) = create_store("cancel");
        let cancel_shared =
            GraphResources::from_store(&cancel_store).expect("cancel shared resources");
        let cancel_baseline = cancel_shared.reserved_bytes().expect("cancel baseline");
        let cancel_state = state(&cancel_store);
        let cancel_token = CancelToken::new();
        let scheduled_cancel = cancel_token.clone();
        let cancel_fired = Arc::new(AtomicBool::new(false));
        let cancel_receipt = Arc::clone(&cancel_fired);
        let cancel_callbacks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let scheduled_cancel_callbacks = Arc::clone(&cancel_callbacks);
        let cancel_scope = install(None, None, move |event| {
            if event.stage == NativePrepareStage::QuantizeValidation
                && scheduled_cancel_callbacks.fetch_add(1, Ordering::SeqCst) == 1
            {
                cancel_receipt.store(true, Ordering::SeqCst);
                scheduled_cancel.cancel();
            }
        });
        let cancelled = try_apply_repeated_vectors(
            &cancel_store,
            &scheduled_document,
            "scheduled",
            4,
            &odd_row,
            &QueryControl::Cancel(cancel_token),
        );
        drop(cancel_scope);
        assert!(matches!(
            cancelled,
            Err(NativeGraphError::Read(TreeError::Control(
                QueryError::Cancelled { partial: false }
            )))
        ));
        assert!(cancel_fired.load(Ordering::SeqCst));
        assert_eq!(cancel_callbacks.load(Ordering::SeqCst), 2);
        assert_eq!(state(&cancel_store), cancel_state);
        assert_eq!(
            cancel_shared.reserved_bytes().expect("cancel release"),
            cancel_baseline
        );
        let cancel_after = state(&cancel_store);
        let cancel_released = cancel_shared
            .reserved_bytes()
            .expect("cancel reported release");
        let cancel_failure = observed_failure(&cancelled);
        cancel_store.close().expect("close cancelled store");
        assert_eq!(
            cancel_shared.reserved_bytes().expect("cancel closed"),
            clean_closed
        );

        let (_deadline_directory, deadline_store) = create_store("deadline");
        let deadline_shared =
            GraphResources::from_store(&deadline_store).expect("deadline shared resources");
        let deadline_baseline = deadline_shared.reserved_bytes().expect("deadline baseline");
        let deadline_state = state(&deadline_store);
        let deadline_clock = Arc::new(ManualMonotonicClock::new());
        let deadline = Deadline::after_with_test_clock(
            Duration::from_secs(1),
            Arc::clone(&deadline_clock) as Arc<dyn crate::lifecycle::MonotonicClock>,
        )
        .expect("scheduled deadline");
        let advance_clock = Arc::clone(&deadline_clock);
        let deadline_fired = Arc::new(AtomicBool::new(false));
        let deadline_receipt = Arc::clone(&deadline_fired);
        let deadline_callbacks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let scheduled_deadline_callbacks = Arc::clone(&deadline_callbacks);
        let deadline_scope = install(None, None, move |event| {
            if event.stage == NativePrepareStage::QueryValidation
                && scheduled_deadline_callbacks.fetch_add(1, Ordering::SeqCst) == 1
            {
                deadline_receipt.store(true, Ordering::SeqCst);
                advance_clock.advance(Duration::from_secs(2));
            }
        });
        let timed_out = try_apply_repeated_vectors(
            &deadline_store,
            &scheduled_document,
            "scheduled",
            4,
            &odd_row,
            &QueryControl::Deadline(deadline),
        );
        drop(deadline_scope);
        assert!(matches!(
            timed_out,
            Err(NativeGraphError::Read(TreeError::Control(
                QueryError::Timeout { partial: false }
            )))
        ));
        assert!(deadline_fired.load(Ordering::SeqCst));
        assert_eq!(deadline_callbacks.load(Ordering::SeqCst), 2);
        assert_eq!(state(&deadline_store), deadline_state);
        assert_eq!(
            deadline_shared.reserved_bytes().expect("deadline release"),
            deadline_baseline
        );
        let deadline_after = state(&deadline_store);
        let deadline_released = deadline_shared
            .reserved_bytes()
            .expect("deadline reported release");
        let deadline_failure = observed_failure(&timed_out);
        deadline_store.close().expect("close deadline store");
        assert_eq!(
            deadline_shared.reserved_bytes().expect("deadline closed"),
            clean_closed
        );

        let (_work_directory, work_store) = create_store("work");
        let work_shared = GraphResources::from_store(&work_store).expect("work shared resources");
        let work_baseline = work_shared.reserved_bytes().expect("work baseline");
        let work_state = state(&work_store);
        let work_fired = Arc::new(AtomicBool::new(false));
        let work_receipt = Arc::clone(&work_fired);
        let work_scope = install(None, Some(work_limit), move |event| {
            if event.stage == NativePrepareStage::Serialization {
                work_receipt.store(true, Ordering::SeqCst);
            }
        });
        let work_failed = try_apply_repeated_vectors(
            &work_store,
            &scheduled_document,
            "scheduled",
            4,
            &odd_row,
            &QueryControl::Cancel(CancelToken::new()),
        );
        drop(work_scope);
        assert!(matches!(
            work_failed,
            Err(NativeGraphError::Read(TreeError::Work))
        ));
        assert!(work_fired.load(Ordering::SeqCst));
        assert_eq!(state(&work_store), work_state);
        assert_eq!(
            work_shared.reserved_bytes().expect("work release"),
            work_baseline
        );
        let work_after = state(&work_store);
        let work_released = work_shared.reserved_bytes().expect("work reported release");
        let work_failure = observed_failure(&work_failed);
        work_store.close().expect("close work store");
        assert_eq!(
            work_shared.reserved_bytes().expect("work closed"),
            clean_closed
        );

        let (_memory_directory, memory_store) = create_store("memory");
        let memory_shared =
            GraphResources::from_store(&memory_store).expect("memory shared resources");
        let memory_baseline = memory_shared.reserved_bytes().expect("memory baseline");
        let memory_state = state(&memory_store);
        let memory_fired = Arc::new(AtomicBool::new(false));
        let memory_receipt = Arc::clone(&memory_fired);
        let memory_scope = install(Some(memory_limit), None, move |event| {
            if event.stage == NativePrepareStage::ImageAllocation {
                memory_receipt.store(true, Ordering::SeqCst);
            }
        });
        let memory_failed = try_apply_repeated_vectors(
            &memory_store,
            &scheduled_document,
            "scheduled",
            4,
            &odd_row,
            &QueryControl::Cancel(CancelToken::new()),
        );
        drop(memory_scope);
        assert!(matches!(
            memory_failed,
            Err(NativeGraphError::Read(TreeError::Memory))
        ));
        assert!(memory_fired.load(Ordering::SeqCst));
        assert_eq!(state(&memory_store), memory_state);
        assert_eq!(
            memory_shared.reserved_bytes().expect("memory release"),
            memory_baseline
        );
        let memory_after = state(&memory_store);
        let memory_released = memory_shared
            .reserved_bytes()
            .expect("memory reported release");
        let memory_failure = observed_failure(&memory_failed);
        memory_store.close().expect("close memory store");
        assert_eq!(
            memory_shared.reserved_bytes().expect("memory closed"),
            clean_closed
        );

        let (_close_directory, close_store) = create_store("close");
        let close_store = Arc::new(close_store);
        let close_shared =
            GraphResources::from_store(&close_store).expect("close shared resources");
        let close_before = state(&close_store);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer_store = Arc::clone(&close_store);
        let writer_document = scheduled_document.clone();
        let writer_row = odd_row.clone();
        let writer = std::thread::spawn(move || {
            let mut entered = false;
            let scope = install(None, None, move |event| {
                if event.stage == NativePrepareStage::Build && !entered {
                    entered = true;
                    entered_tx.send(()).expect("report build barrier");
                    release_rx.recv().expect("release build barrier");
                }
            });
            let result = try_apply_repeated_vectors(
                &writer_store,
                &writer_document,
                "scheduled",
                4,
                &writer_row,
                &QueryControl::Cancel(CancelToken::new()),
            );
            drop(scope);
            result
        });
        entered_rx.recv().expect("build barrier entered");
        let closing_store = Arc::clone(&close_store);
        let closer = std::thread::spawn(move || closing_store.close());
        let mut closing_observed = false;
        for _ in 0..1_000_000 {
            if close_store.state().expect("close state") == StoreState::Closing {
                closing_observed = true;
                break;
            }
            std::thread::yield_now();
        }
        assert!(closing_observed, "close did not acknowledge Closing");
        release_tx.send(()).expect("release close barrier");
        let close_failed = writer.join().expect("join closing writer");
        assert!(matches!(
            close_failed,
            Err(NativeGraphError::Read(TreeError::Control(
                QueryError::ReadCancelled { partial: false }
            )))
        ));
        closer
            .join()
            .expect("join close thread")
            .expect("close during preparation");
        assert_eq!(
            close_store.state().expect("closed state"),
            StoreState::Closed
        );
        assert_eq!(
            close_shared
                .reserved_bytes()
                .expect("close settled accounting"),
            clean_closed
        );
        let reopened = Store::open_native_graph(
            _close_directory.path().join("close"),
            native_options(),
            Some(scheduled_document),
        )
        .expect("reopen close-aborted store");
        let close_after = state(&reopened);
        assert_eq!(close_after, close_before);
        reopened.close().expect("close reopened store");
        let clean = CleanPreparationObservation {
            completed: clean_completed,
            event_count: clean_events.len(),
            max_chunk_units: clean_events
                .iter()
                .map(|event| event.units)
                .max()
                .unwrap_or(0),
            max_work_observed: clean_events
                .iter()
                .map(|event| event.work_before)
                .max()
                .unwrap_or(0),
            prepared_images: clean_events
                .iter()
                .filter(|event| event.stage == NativePrepareStage::ImageAllocation)
                .count(),
            allocation_reserved_bytes: image_allocation.reserved_before,
            allocation_requested_bytes: image_allocation.requested_bytes,
            closed_reserved_bytes: clean_closed,
        };
        let limits = LimitProbeReport {
            key: "property-graph.native-vector-index.limit.fire",
            seed,
            clean: clean.clone(),
            work_limit,
            memory_limit,
            work: NativeFailureObservation {
                fired: work_fired.load(Ordering::SeqCst),
                kind: work_failure,
                generation_before: work_state.0.generation.get(),
                generation_after: work_after.0.generation.get(),
                sequence_before: work_state.1,
                sequence_after: work_after.1,
                baseline_reserved_bytes: work_baseline,
                released_reserved_bytes: work_released,
                closed_reserved_bytes: work_shared.reserved_bytes().expect("work reported closed"),
            },
            memory: NativeFailureObservation {
                fired: memory_fired.load(Ordering::SeqCst),
                kind: memory_failure,
                generation_before: memory_state.0.generation.get(),
                generation_after: memory_after.0.generation.get(),
                sequence_before: memory_state.1,
                sequence_after: memory_after.1,
                baseline_reserved_bytes: memory_baseline,
                released_reserved_bytes: memory_released,
                closed_reserved_bytes: memory_shared
                    .reserved_bytes()
                    .expect("memory reported closed"),
            },
        };
        let controls = ControlProbeReport {
            key: "property-graph.native-vector-index.control.fire",
            seed,
            clean,
            cancel: NativeFailureObservation {
                fired: cancel_fired.load(Ordering::SeqCst),
                kind: cancel_failure,
                generation_before: cancel_state.0.generation.get(),
                generation_after: cancel_after.0.generation.get(),
                sequence_before: cancel_state.1,
                sequence_after: cancel_after.1,
                baseline_reserved_bytes: cancel_baseline,
                released_reserved_bytes: cancel_released,
                closed_reserved_bytes: cancel_shared
                    .reserved_bytes()
                    .expect("cancel reported closed"),
            },
            deadline: NativeFailureObservation {
                fired: deadline_fired.load(Ordering::SeqCst),
                kind: deadline_failure,
                generation_before: deadline_state.0.generation.get(),
                generation_after: deadline_after.0.generation.get(),
                sequence_before: deadline_state.1,
                sequence_after: deadline_after.1,
                baseline_reserved_bytes: deadline_baseline,
                released_reserved_bytes: deadline_released,
                closed_reserved_bytes: deadline_shared
                    .reserved_bytes()
                    .expect("deadline reported closed"),
            },
            close: CloseFailureObservation {
                closing_observed,
                kind: observed_failure(&close_failed),
                generation_before: close_before.0.generation.get(),
                generation_after: close_after.0.generation.get(),
                sequence_before: close_before.1,
                sequence_after: close_after.1,
                closed_reserved_bytes: close_shared
                    .reserved_bytes()
                    .expect("close reported closed"),
            },
        };
        (limits, controls)
    }

    /// Exact persisted index and source observations before and after reopen.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct ReopenIndexObservation {
        pub index_bytes: Vec<u8>,
        pub identities: Vec<(u128, u64)>,
        pub coordinate_bits: Vec<u32>,
        pub rows: u32,
        pub live_rows: usize,
        pub dimensions: u32,
        pub profile: u8,
        pub max_degree: u8,
        pub seed_count: usize,
        pub catalog: crate::property_graph::wal::RequiredRef,
        pub index_reference: crate::property_graph::storage::artifact::PhysicalRef,
        pub physical_references: Vec<crate::property_graph::storage::artifact::PhysicalRef>,
    }
    impl From<&SourceReport> for ReopenIndexObservation {
        fn from(source: &SourceReport) -> Self {
            Self {
                index_bytes: source.index_bytes.clone(),
                identities: source
                    .identities
                    .iter()
                    .map(|(node, revision)| (node.get(), *revision))
                    .collect(),
                coordinate_bits: source.coordinate_bits.clone(),
                rows: source.rows,
                live_rows: source.live_rows,
                dimensions: source.dimensions,
                profile: source.profile,
                max_degree: source.max_degree,
                seed_count: source.seed_count,
                catalog: source.index_catalog,
                index_reference: source.index_reference,
                physical_references: source.index_physical_references.clone(),
            }
        }
    }
    /// WAL replay and explicit-checkpoint reopen with observed zero construction.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct ReopenProbeReport {
        pub key: &'static str,
        pub seed: u64,
        pub wal_write_receipts: Vec<u128>,
        pub checkpoint_write_receipts: Vec<u128>,
        pub expected_row_bits: Vec<u32>,
        pub wal_before: ReopenIndexObservation,
        pub wal_after: ReopenIndexObservation,
        pub checkpoint_before: ReopenIndexObservation,
        pub checkpoint_after: ReopenIndexObservation,
        pub wal_prepare_events: usize,
        pub checkpoint_prepare_events: usize,
    }
    pub(crate) struct ReopenFixture {
        _directory: ProbeDirectory,
        seed: u64,
        wal_nodes: Vec<NodeId>,
        checkpoint_nodes: Vec<NodeId>,
        pub(crate) document: EmbeddingTower,
        pub(crate) coordinates: Vec<f32>,
        pub(crate) query: [f32; 2],
        pub(crate) wal_path: std::path::PathBuf,
        pub(crate) checkpoint_path: std::path::PathBuf,
        pub(crate) wal_before: SourceReport,
        pub(crate) checkpoint_before: SourceReport,
    }
    #[cfg(test)]
    impl ReopenFixture {
        pub(crate) fn path(&self) -> &std::path::Path {
            self._directory.path()
        }
    }
    fn assert_same_index(expected: &SourceReport, actual: &SourceReport) {
        assert_eq!(actual.index_bytes, expected.index_bytes);
        assert_eq!(actual.identities, expected.identities);
        assert_eq!(actual.coordinate_bits, expected.coordinate_bits);
        assert_eq!(actual.rows, expected.rows);
        assert_eq!(actual.dimensions, expected.dimensions);
        assert_eq!(actual.profile, expected.profile);
        assert_eq!(actual.max_degree, expected.max_degree);
        assert_eq!(actual.seed_count, expected.seed_count);
        assert_eq!(actual.index_catalog, expected.index_catalog);
        assert_eq!(actual.index_reference, expected.index_reference);
        assert_eq!(
            actual.index_physical_references,
            expected.index_physical_references
        );
    }

    fn reopen_without_build(
        path: &Path,
        document: &EmbeddingTower,
        query: [f32; 2],
    ) -> (SourceReport, usize) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&events);
        let scope = install(None, None, move |event| {
            observed.lock().expect("reopen build events").push(event);
        });
        let store = Store::open_native_graph(path, native_options(), Some(document.clone()))
            .expect("reopen native vector fixture");
        let mut reports = inspect_sources(&store, query);
        assert_eq!(reports.len(), 1);
        store.close().expect("close reopened vector fixture");
        drop(store);
        drop(scope);
        assert!(
            events.lock().expect("reopen build events").is_empty(),
            "replay/open rebuilt a native vector index"
        );
        let preparation_events = events.lock().expect("reopen reported events").len();
        (reports.remove(0), preparation_events)
    }

    pub(crate) fn prepare_reopen_fixture(seed: u64) -> ReopenFixture {
        let directory = ProbeDirectory::create(seed).expect("temporary reopen store");
        let mut document = document_tower();
        document.model_id = "ze158-reopen-document".into();
        document.dims = 512;
        let mut coordinates = vec![0.0_f32; 512];
        coordinates[0] = 0.375;
        coordinates[1] = -0.875;
        let query = [coordinates[0], coordinates[1]];

        let wal_path = directory.path().join("wal-original");
        let wal_store =
            Store::create_native_graph(&wal_path, native_options(), Some(document.clone()))
                .expect("fresh WAL replay store");
        let wal_nodes =
            super::apply_repeated_vectors_checked(&wal_store, &document, "wal", 24, &coordinates)
                .expect("WAL vector batch");
        let mut wal_before = inspect_sources(&wal_store, query);
        assert_eq!(wal_before.len(), 1);
        let wal_before = wal_before.remove(0);
        assert_eq!(
            wal_before.identities,
            wal_nodes
                .iter()
                .copied()
                .map(|node| (node, 1))
                .collect::<Vec<_>>()
        );
        assert_eq!(wal_before.index_reference.kind, BlockKind::ExtentList);
        assert!(wal_before.index_physical_references.len() >= 2);
        wal_store.close().expect("close WAL replay store");
        drop(wal_store);

        let checkpoint_path = directory.path().join("checkpoint-original");
        let checkpoint_store =
            Store::create_native_graph(&checkpoint_path, native_options(), Some(document.clone()))
                .expect("fresh checkpoint reopen store");
        let checkpoint_nodes = super::apply_repeated_vectors_checked(
            &checkpoint_store,
            &document,
            "checkpoint",
            24,
            &coordinates,
        )
        .expect("checkpoint vector batch");
        let mut checkpoint_before = inspect_sources(&checkpoint_store, query);
        assert_eq!(checkpoint_before.len(), 1);
        let checkpoint_before = checkpoint_before.remove(0);
        assert_eq!(
            checkpoint_before.identities,
            checkpoint_nodes
                .iter()
                .copied()
                .map(|node| (node, 1))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            checkpoint_before.index_reference.kind,
            BlockKind::ExtentList
        );
        assert!(checkpoint_before.index_physical_references.len() >= 2);
        checkpoint_store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .expect("checkpoint indexed graph");
        let checkpointed = inspect_sources(&checkpoint_store, query);
        assert_eq!(checkpointed.len(), 1);
        assert_same_index(&checkpoint_before, &checkpointed[0]);
        checkpoint_store
            .close()
            .expect("close explicitly checkpointed graph");
        drop(checkpoint_store);

        ReopenFixture {
            _directory: directory,
            seed,
            wal_nodes,
            checkpoint_nodes,
            document,
            coordinates,
            query,
            wal_path,
            checkpoint_path,
            wal_before,
            checkpoint_before,
        }
    }

    pub(crate) fn verify_reopen_fixture(fixture: &ReopenFixture) -> ReopenProbeReport {
        let (wal_after, wal_prepare_events) =
            reopen_without_build(&fixture.wal_path, &fixture.document, fixture.query);
        assert_same_index(&fixture.wal_before, &wal_after);
        let (checkpoint_after, checkpoint_prepare_events) =
            reopen_without_build(&fixture.checkpoint_path, &fixture.document, fixture.query);
        assert_same_index(&fixture.checkpoint_before, &checkpoint_after);
        let report = ReopenProbeReport {
            key: "property-graph.native-vector-index.reopen",
            seed: fixture.seed,
            wal_write_receipts: fixture.wal_nodes.iter().map(|node| node.get()).collect(),
            checkpoint_write_receipts: fixture
                .checkpoint_nodes
                .iter()
                .map(|node| node.get())
                .collect(),
            expected_row_bits: fixture
                .coordinates
                .iter()
                .map(|value| value.to_bits())
                .collect(),
            wal_before: (&fixture.wal_before).into(),
            wal_after: (&wal_after).into(),
            checkpoint_before: (&fixture.checkpoint_before).into(),
            checkpoint_after: (&checkpoint_after).into(),
            wal_prepare_events,
            checkpoint_prepare_events,
        };
        assert_eq!(report.wal_before, report.wal_after);
        assert_eq!(report.checkpoint_before, report.checkpoint_after);
        report
    }

    /// Uses the same setup and comparisons as the focused reopen qualification.
    pub fn run_reopen_probe(seed: u64) -> ReopenProbeReport {
        let fixture = prepare_reopen_fixture(seed);
        verify_reopen_fixture(&fixture)
    }

    /// Actual completion and batching receipts from SearchTraceCursor.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct TraceBatchObservation {
        pub references: Vec<PhysicalRef>,
        pub complete: bool,
        pub batches: usize,
        pub max_batch: usize,
    }
    /// Source identities, values, and persisted seeds admitted before tracing.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct TraceSourceObservation {
        pub rows: u32,
        pub identities: Vec<(u128, u64)>,
        pub coordinate_bits: Vec<u32>,
        pub seed_rows: Vec<u32>,
        pub entry_points: Vec<u32>,
        pub catalog: PhysicalRef,
    }
    /// Required descendant closure and measured trace-owner release.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct TraceProbeReport {
        pub key: &'static str,
        pub seed: u64,
        pub write_receipts: Vec<Vec<u128>>,
        pub expected_row_bits: Vec<u32>,
        pub sources: Vec<TraceSourceObservation>,
        pub required_references: Vec<PhysicalRef>,
        pub small: TraceBatchObservation,
        pub full: TraceBatchObservation,
        pub baseline_reserved_bytes: u64,
        pub released_reserved_bytes: u64,
    }
    pub(crate) struct TraceFixture {
        _directory: ProbeDirectory,
        seed: u64,
        coordinates: Vec<f32>,
        write_receipts: Vec<Vec<NodeId>>,
        pub(crate) store: Store,
        pub(crate) reports: Vec<SourceReport>,
    }
    pub(crate) fn prepare_trace_fixture(seed: u64) -> TraceFixture {
        let directory = ProbeDirectory::create(seed).expect("temporary trace store");
        let path = directory.path().join("native");
        let mut document = document_tower();
        document.model_id = "ze158-trace-document".into();
        document.dims = 512;
        let store = Store::create_native_graph(&path, native_options(), Some(document.clone()))
            .expect("fresh trace store");
        let mut coordinates = vec![0.0_f32; 512];
        coordinates[0] = 0.625;
        coordinates[1] = -0.125;
        let old = super::apply_repeated_vectors_checked(
            &store,
            &document,
            "old-catalog",
            1,
            &coordinates,
        )
        .expect("old-catalog write");
        let extent_nodes =
            super::apply_repeated_vectors_checked(&store, &document, "trace", 24, &coordinates)
                .expect("extent write");
        let reports = inspect_sources(&store, [coordinates[0], coordinates[1]]);
        assert_eq!(reports.len(), 2);
        let extent = reports
            .iter()
            .find(|report| report.rows == 24)
            .expect("extent-backed native vector index");
        assert_eq!(extent.index_reference.kind, BlockKind::ExtentList);
        assert_ne!(
            reports[0].index_catalog_block,
            reports[1].index_catalog_block
        );

        TraceFixture {
            _directory: directory,
            seed,
            coordinates,
            write_receipts: vec![old, extent_nodes],
            store,
            reports,
        }
    }
    fn collect_native_vector_trace<'s, 'lease, 'm>(
        source: &'s crate::property_graph::storage::NativePreparationSource<'lease, 'm>,
        catalog: &'s crate::property_graph::storage::NativePreparationCatalog<'s, 'lease, 'm>,
        resources: &mut TreeResources<'m>,
        capacity: usize,
    ) -> TraceBatchObservation {
        let mut cursor =
            crate::property_graph::storage::search::SearchTraceCursor::for_preparation(
                source, catalog, resources,
            )
            .expect("native vector trace cursor");
        let mut references = Vec::new();
        let mut batches = 0;
        let mut max_batch = 0;
        loop {
            let mut batch = vec![None; capacity];
            let result = cursor
                .trace(&mut batch, resources)
                .expect("native vector trace batch");
            batches += 1;
            max_batch = max_batch.max(result.count);
            assert!(result.count <= capacity);
            references.extend(
                batch
                    .into_iter()
                    .take(result.count)
                    .map(|reference| reference.expect("reported trace reference")),
            );
            if result.complete {
                return TraceBatchObservation {
                    references,
                    complete: result.complete,
                    batches,
                    max_batch,
                };
            }
        }
    }

    pub(crate) fn verify_trace_fixture(fixture: &TraceFixture) -> TraceProbeReport {
        use crate::property_graph::staging::{WriteLimits, WriteMemory};
        use crate::property_graph::storage::memory::StorageMemory;
        use crate::property_graph::storage::{NativePreparationCatalog, NativePreparationSource};
        let store = &fixture.store;
        let shared = GraphResources::from_store(store).expect("trace shared resources");
        let baseline_reserved_bytes = shared.reserved_bytes().expect("trace baseline");
        let (small, full, required_references) = {
            let lease = store.admit_native_read().expect("trace admission");
            let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("trace writer");
            let control = QueryControl::Cancel(CancelToken::new());
            let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024)
                .expect("trace storage memory");
            let source = NativePreparationSource::new(&lease, &memory, 128)
                .expect("trace preparation source");
            let mut resources = source.resources(u64::MAX).expect("trace resources");
            let catalog = NativePreparationCatalog::open(&source, &mut resources)
                .expect("trace preparation catalog");
            let one = collect_native_vector_trace(&source, &catalog, &mut resources, 1);
            let full = collect_native_vector_trace(&source, &catalog, &mut resources, 256);
            assert_eq!(one.references, full.references);
            let mut required_references = Vec::new();
            for report in &fixture.reports {
                required_references.push(report.index_catalog_block);
                assert!(full.references.contains(&report.index_catalog_block));
                let mut ordinal = 0;
                loop {
                    let reference = report
                        .index_payload
                        .physical_reference_at(
                            &source,
                            lease.bundle().roots().store(),
                            lease.bundle().base().generation,
                            ordinal,
                            &mut resources,
                        )
                        .expect("index physical trace closure");
                    let Some(reference) = reference else {
                        break;
                    };
                    required_references.push(reference);
                    assert!(full.references.contains(&reference));
                    ordinal += 1;
                }
                assert!(ordinal >= 1);
            }

            (one, full, required_references)
        };
        let released_reserved_bytes = shared.reserved_bytes().expect("released trace resources");
        assert_eq!(released_reserved_bytes, baseline_reserved_bytes);
        let sources = fixture
            .reports
            .iter()
            .map(|source| TraceSourceObservation {
                rows: source.rows,
                identities: source
                    .identities
                    .iter()
                    .map(|(node, revision)| (node.get(), *revision))
                    .collect(),
                coordinate_bits: source.coordinate_bits.clone(),
                seed_rows: source.seed_rows.clone(),
                entry_points: source.entry_points.clone(),
                catalog: source.index_catalog_block,
            })
            .collect::<Vec<_>>();
        let mut expected_ids = fixture
            .write_receipts
            .iter()
            .flatten()
            .map(|node| (node.get(), 1))
            .collect::<Vec<_>>();
        expected_ids.sort_unstable();
        let mut actual_ids = sources
            .iter()
            .flat_map(|source| source.identities.iter().copied())
            .collect::<Vec<_>>();
        actual_ids.sort_unstable();
        assert_eq!(actual_ids, expected_ids);
        let expected_row_bits = fixture
            .coordinates
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>();
        for source in &sources {
            assert_eq!(
                source.coordinate_bits,
                expected_row_bits.repeat(source.rows as usize)
            );
            let mut entries = source.entry_points.clone();
            entries.sort_unstable();
            entries.dedup();
            assert_eq!(entries, source.seed_rows);
            assert_eq!(entries.len(), (source.rows as usize).min(4));
        }
        TraceProbeReport {
            key: "property-graph.native-vector-index.trace",
            seed: fixture.seed,
            write_receipts: fixture
                .write_receipts
                .iter()
                .map(|nodes| nodes.iter().map(|node| node.get()).collect())
                .collect(),
            expected_row_bits,
            sources,
            required_references,
            small,
            full,
            baseline_reserved_bytes,
            released_reserved_bytes,
        }
    }
    /// Runs the same positive extent/catalog trace body used by the focused test.
    pub fn run_trace_probe(seed: u64) -> TraceProbeReport {
        let fixture = prepare_trace_fixture(seed);
        let report = verify_trace_fixture(&fixture);
        fixture.store.close().expect("close trace store");
        report
    }

    /// One deliberate mutation rejected by the real comparator and restored.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct OracleControlObservation {
        pub name: &'static str,
        pub rejected: bool,
        pub restored: bool,
    }
    /// Actual oracle controls, persisted observations, and reference closure.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct OracleProbeReport {
        pub key: &'static str,
        pub seed: u64,
        pub query_seed: u64,
        pub expected_identities: Vec<(u128, u64)>,
        pub observed_identities: Vec<(u128, u64)>,
        pub expected_coordinate_bits: Vec<u32>,
        pub observed_coordinate_bits: Vec<u32>,
        pub declared_seed_rows: Vec<u32>,
        pub observed_seed_rows: Vec<u32>,
        pub observed_entry_points: Vec<u32>,
        pub required_references: Vec<(PhysicalRef, usize)>,
        pub observed_references: Vec<PhysicalRef>,
        pub restored_references: Vec<PhysicalRef>,
        pub initial_complete: bool,
        pub restored_complete: bool,
        pub initial_clean: bool,
        pub same_seed_clean: bool,
        pub controls: Vec<OracleControlObservation>,
        pub fires: usize,
        pub restored_checks: usize,
        pub release_checks: usize,
        pub traced_reference_count: usize,
        pub trace_batches: usize,
        pub preparation_events: usize,
        pub traversal_visits: usize,
        pub baseline_reserved_bytes: [u64; 2],
        pub released_reserved_bytes: [u64; 2],
    }
    /// Runs the original directed trace/traversal and comparator controls.
    pub fn run_oracle_probe(seed: u64) -> OracleProbeReport {
        use super::install;
        use crate::graph::search::{GraphSearchRequest, GraphSearchScratch, GraphSearcher};
        use crate::lifecycle::native_graph::NativeReadConsumer;
        use crate::property_graph::query::runtime::{RuntimeContext, RuntimeLimits};
        use crate::property_graph::staging::{WriteLimits, WriteMemory};
        use crate::property_graph::storage::GraphReadView;
        use crate::property_graph::storage::memory::StorageMemory;
        use crate::property_graph::storage::{NativePreparationCatalog, NativePreparationSource};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Clone)]
        struct TraceObservation {
            references: Vec<PhysicalRef>,
            complete: bool,
            batches: usize,
            released: bool,
            baseline_reserved_bytes: u64,
            released_reserved_bytes: u64,
        }

        fn actual_trace(store: &Store, shared: &GraphResources) -> TraceObservation {
            let baseline = shared.reserved_bytes().expect("directed trace baseline");
            let lease = store.admit_native_read().expect("directed trace admission");
            let writer =
                WriteMemory::new(shared, WriteLimits::default()).expect("directed trace writer");
            let control = QueryControl::Cancel(CancelToken::new());
            let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024)
                .expect("directed trace memory");
            let source =
                NativePreparationSource::new(&lease, &memory, 128).expect("directed trace source");
            let mut resources = source
                .resources(u64::MAX)
                .expect("directed trace resources");
            let catalog = NativePreparationCatalog::open(&source, &mut resources)
                .expect("directed trace catalog");
            let mut cursor =
                crate::property_graph::storage::search::SearchTraceCursor::for_preparation(
                    &source,
                    &catalog,
                    &mut resources,
                )
                .expect("directed trace cursor");
            let mut references = Vec::new();
            let mut batches = 0;
            let complete = loop {
                let mut batch = [None; 3];
                let result = cursor
                    .trace(&mut batch, &mut resources)
                    .expect("directed trace batch");
                batches += 1;
                references.extend(
                    batch
                        .into_iter()
                        .take(result.count)
                        .map(|reference| reference.expect("directed traced reference")),
                );
                if result.complete {
                    break true;
                }
            };
            drop(cursor);
            drop(catalog);
            drop(resources);
            drop(source);
            drop(memory);
            drop(control);
            drop(lease);
            let released_reserved_bytes = shared.reserved_bytes().expect("directed released bytes");
            let released = released_reserved_bytes == baseline;
            TraceObservation {
                references,
                complete,
                batches,
                released,
                baseline_reserved_bytes: baseline,
                released_reserved_bytes,
            }
        }

        struct MeasureTraversal {
            query: Vec<f32>,
        }

        impl NativeReadConsumer<usize> for MeasureTraversal {
            fn consume<'s, 'lease, 'm, 'g>(
                &mut self,
                view: &GraphReadView<'s, 'lease, 'm, 'g>,
                runtime: &mut RuntimeContext<'lease, 'm, 'g>,
            ) -> Result<usize, TreeError> {
                let sparse = view.sparse_view(runtime)?;
                let mut resources = TreeResources::for_query(runtime)?;
                let mut sources = sparse.sources(
                    crate::property_graph::storage::search::Modality::Vector,
                    &mut resources,
                )?;
                let source = sources
                    .next(&mut resources)?
                    .ok_or(TreeError::Invalid("directed vector source missing"))?;
                let index = source
                    .vector_index(&mut resources)?
                    .ok_or(TreeError::Invalid("directed native index missing"))?;
                if sources.next(&mut resources)?.is_some() {
                    return Err(TreeError::Invalid("directed multiple vector sources"));
                }
                let graph = index.graph()?;
                let mut scratch =
                    GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
                        .map_err(|_| TreeError::Invalid("directed traversal scratch"))?;
                let mut searcher = GraphSearcher::new(graph, index.rescore(), &mut scratch)
                    .map_err(|_| TreeError::Invalid("directed traversal binding"))?;
                let result = searcher
                    .search(
                        GraphSearchRequest::new(&self.query, 1, 0x158e)
                            .with_ef(index.row_count() as usize),
                        None,
                    )
                    .map_err(|_| TreeError::Invalid("directed traversal"))?;
                Ok(result.counters().visited())
            }
        }

        fn accepts(
            expected_receipts: &[NodeId],
            expected_values: &[u32],
            expected_seed_receipts: &[u32],
            required_references: &[(PhysicalRef, usize)],
            observed_receipts: &[NodeId],
            observed: &SourceReport,
            trace: &TraceObservation,
        ) -> bool {
            let expected_identities = expected_receipts
                .iter()
                .copied()
                .map(|node| (node, 1))
                .collect::<Vec<_>>();
            let mut expected_seeds = expected_seed_receipts.to_vec();
            expected_seeds.sort_unstable();
            let mut observed_seeds = observed.seed_rows.clone();
            observed_seeds.sort_unstable();
            observed_receipts == expected_receipts
                && observed.identities == expected_identities
                && observed.coordinate_bits == expected_values
                && observed.entry_points == expected_seed_receipts
                && observed_seeds == expected_seeds
                && observed.seed_count == expected_seeds.len()
                && trace.complete
                && required_references
                    .iter()
                    .enumerate()
                    .all(|(index, (reference, _))| {
                        !required_references[..index]
                            .iter()
                            .any(|(prior, _)| prior == reference)
                    })
                && required_references
                    .iter()
                    .all(|(reference, expected_count)| {
                        trace
                            .references
                            .iter()
                            .filter(|observed| *observed == reference)
                            .count()
                            == *expected_count
                    })
        }

        let directory = ProbeDirectory::create(seed).expect("temporary directed-probe store");
        let path = directory.path().join("native");
        let mut document = document_tower();
        document.model_id = "ze158-directed-document".into();
        document.dims = 512;
        let store = Store::create_native_graph(&path, native_options(), Some(document.clone()))
            .expect("fresh directed-probe store");
        let mut coordinates = vec![0.0_f32; 512];
        coordinates[0] = f32::from_bits(0x3f00_0001);
        coordinates[1] = f32::from_bits(0xbf40_0001);
        let build_events = Arc::new(AtomicUsize::new(0));
        let build_receipt = Arc::clone(&build_events);
        let scope = install(None, None, move |_| {
            build_receipt.fetch_add(1, Ordering::SeqCst);
        });
        let receipts =
            super::apply_repeated_vectors_checked(&store, &document, "directed", 24, &coordinates)
                .expect("directed vector batch");
        drop(scope);
        let expected_values = (0..receipts.len())
            .flat_map(|_| coordinates.iter().map(|value| value.to_bits()))
            .collect::<Vec<_>>();
        let reports = inspect_sources(&store, [coordinates[0], coordinates[1]]);
        assert_eq!(reports.len(), 1);
        let clean = &reports[0];
        assert_eq!(clean.index_reference.kind, BlockKind::ExtentList);
        assert!(clean.index_physical_references.len() > 2);
        let mut required_references = clean
            .index_physical_references
            .iter()
            .copied()
            .map(|reference| (reference, 1))
            .collect::<Vec<_>>();
        required_references.push((clean.index_catalog_block, 3));
        let shared = GraphResources::from_store(&store).expect("directed shared resources");
        let trace = actual_trace(&store, &shared);
        let traversal_visits = store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                crate::property_graph::query::MAX_QUERY_BYTES,
                32,
                MeasureTraversal {
                    query: coordinates.clone(),
                },
            )
            .expect("directed actual traversal");
        let expected_seed_receipts = clean.entry_points.clone();
        let initial_clean = accepts(
            &receipts,
            &expected_values,
            &expected_seed_receipts,
            &required_references,
            &receipts,
            clean,
            &trace,
        );
        assert!(initial_clean);

        let mut controls = Vec::new();
        let mut restored_checks = 0_usize;
        let mut missing_reference = trace.clone();
        let missing = required_references[1].0;
        let position = missing_reference
            .references
            .iter()
            .position(|reference| *reference == missing)
            .expect("actual required trace reference");
        missing_reference.references.remove(position);
        let rejected = !accepts(
            &receipts,
            &expected_values,
            &expected_seed_receipts,
            &required_references,
            &receipts,
            clean,
            &missing_reference,
        );
        assert!(rejected);
        controls.push(OracleControlObservation {
            name: "missing-required-reference",
            rejected,
            restored: false,
        });
        let restored = accepts(
            &receipts,
            &expected_values,
            &expected_seed_receipts,
            &required_references,
            &receipts,
            clean,
            &trace,
        );
        assert!(restored);
        restored_checks += usize::from(restored);
        controls
            .last_mut()
            .expect("negative control before restoration")
            .restored = restored;

        let mut corrupt_row = clean.clone();
        corrupt_row.identities[0].1 += 1;
        let rejected = !accepts(
            &receipts,
            &expected_values,
            &expected_seed_receipts,
            &required_references,
            &receipts,
            &corrupt_row,
            &trace,
        );
        assert!(rejected);
        controls.push(OracleControlObservation {
            name: "changed-revision",
            rejected,
            restored: false,
        });
        let restored = accepts(
            &receipts,
            &expected_values,
            &expected_seed_receipts,
            &required_references,
            &receipts,
            clean,
            &trace,
        );
        assert!(restored);
        restored_checks += usize::from(restored);
        controls
            .last_mut()
            .expect("negative control before restoration")
            .restored = restored;

        let mut corrupt_value = clean.clone();
        corrupt_value.coordinate_bits[0] ^= 1;
        let rejected = !accepts(
            &receipts,
            &expected_values,
            &expected_seed_receipts,
            &required_references,
            &receipts,
            &corrupt_value,
            &trace,
        );
        assert!(rejected);
        controls.push(OracleControlObservation {
            name: "changed-value",
            rejected,
            restored: false,
        });
        let restored = accepts(
            &receipts,
            &expected_values,
            &expected_seed_receipts,
            &required_references,
            &receipts,
            clean,
            &trace,
        );
        assert!(restored);
        restored_checks += usize::from(restored);
        controls
            .last_mut()
            .expect("negative control before restoration")
            .restored = restored;

        let mut corrupt_seed = clean.clone();
        corrupt_seed.seed_rows[0] = clean.rows;
        let rejected = !accepts(
            &receipts,
            &expected_values,
            &expected_seed_receipts,
            &required_references,
            &receipts,
            &corrupt_seed,
            &trace,
        );
        assert!(rejected);
        controls.push(OracleControlObservation {
            name: "changed-seed",
            rejected,
            restored: false,
        });
        let restored = accepts(
            &receipts,
            &expected_values,
            &expected_seed_receipts,
            &required_references,
            &receipts,
            clean,
            &trace,
        );
        assert!(restored);
        restored_checks += usize::from(restored);
        controls
            .last_mut()
            .expect("negative control before restoration")
            .restored = restored;

        let mut missing_receipt = receipts.clone();
        missing_receipt.remove(0);
        let rejected = !accepts(
            &receipts,
            &expected_values,
            &expected_seed_receipts,
            &required_references,
            &missing_receipt,
            clean,
            &trace,
        );
        assert!(rejected);
        controls.push(OracleControlObservation {
            name: "missing-write-receipt",
            rejected,
            restored: false,
        });
        let mut duplicate_receipt = receipts.clone();
        duplicate_receipt.push(receipts[0]);
        let rejected = !accepts(
            &receipts,
            &expected_values,
            &expected_seed_receipts,
            &required_references,
            &duplicate_receipt,
            clean,
            &trace,
        );
        assert!(rejected);
        controls.push(OracleControlObservation {
            name: "duplicate-write-receipt",
            rejected,
            restored: false,
        });

        let same_seed = inspect_sources(&store, [coordinates[0], coordinates[1]]);
        let same_trace = actual_trace(&store, &shared);
        assert_eq!(same_seed[0].fingerprint, clean.fingerprint);
        let same_seed_clean = accepts(
            &receipts,
            &expected_values,
            &expected_seed_receipts,
            &required_references,
            &receipts,
            &same_seed[0],
            &same_trace,
        );
        assert!(same_seed_clean);
        restored_checks += usize::from(same_seed_clean);
        for observation in controls.iter_mut().skip(4) {
            observation.restored = same_seed_clean;
        }
        let release_checks = usize::from(trace.released) + usize::from(same_trace.released);
        let traced_reference_count = trace.references.len() + same_trace.references.len();
        let trace_batches = trace.batches + same_trace.batches;
        let fires = controls
            .iter()
            .filter(|observation| observation.rejected)
            .count();
        assert_eq!(fires, 6);
        assert!(controls.iter().all(|observation| observation.restored));
        assert_eq!(release_checks, 2);
        assert!(traced_reference_count > 0);
        assert!(build_events.load(Ordering::SeqCst) > 0);
        assert!(traversal_visits > 0);
        assert!(trace_batches > 0);
        store.close().expect("close directed-probe store");
        OracleProbeReport {
            key: "property-graph.native-vector-index.oracle.can-fire",
            seed,
            query_seed: 0x158e,
            expected_identities: receipts.iter().map(|node| (node.get(), 1)).collect(),
            observed_identities: clean
                .identities
                .iter()
                .map(|(node, revision)| (node.get(), *revision))
                .collect(),
            expected_coordinate_bits: expected_values,
            observed_coordinate_bits: clean.coordinate_bits.clone(),
            declared_seed_rows: expected_seed_receipts,
            observed_seed_rows: clean.seed_rows.clone(),
            observed_entry_points: clean.entry_points.clone(),
            required_references,
            observed_references: trace.references,
            restored_references: same_trace.references,
            initial_complete: trace.complete,
            restored_complete: same_trace.complete,
            initial_clean,
            same_seed_clean,
            controls,
            fires,
            restored_checks,
            release_checks,
            traced_reference_count,
            trace_batches,
            preparation_events: build_events.load(Ordering::SeqCst),
            traversal_visits,
            baseline_reserved_bytes: [
                trace.baseline_reserved_bytes,
                same_trace.baseline_reserved_bytes,
            ],
            released_reserved_bytes: [
                trace.released_reserved_bytes,
                same_trace.released_reserved_bytes,
            ],
        }
    }
}

#[cfg(test)]
pub(crate) use actual_cases::try_apply_repeated_vectors;
#[cfg(any(test, all(feature = "graph-cypher", feature = "test-seams")))]
pub use actual_cases::{
    ControlProbeReport, IdentityProbeReport, LimitProbeReport, run_identity_probe,
    run_preparation_schedule_probe,
};

#[cfg(any(test, all(feature = "graph-cypher", feature = "test-seams")))]
pub use actual_cases::{ReopenProbeReport, run_reopen_probe};
#[cfg(test)]
pub(crate) use actual_cases::{prepare_reopen_fixture, verify_reopen_fixture};

#[cfg(any(test, all(feature = "graph-cypher", feature = "test-seams")))]
pub use actual_cases::{TraceProbeReport, run_trace_probe};
#[cfg(test)]
pub(crate) use actual_cases::{prepare_trace_fixture, verify_trace_fixture};

#[cfg(any(test, all(feature = "graph-cypher", feature = "test-seams")))]
pub use actual_cases::{OracleProbeReport, run_oracle_probe};

#[cfg(any(test, all(feature = "graph-cypher", feature = "test-seams")))]
pub use actual_cases::{
    CleanPreparationObservation, CloseFailureObservation, NativeFailureObservation,
    OracleControlObservation, ReopenIndexObservation, TraceBatchObservation,
    TraceSourceObservation,
};

/// The eight independently observed native vector-index receipt groups.
#[cfg(any(test, all(feature = "graph-cypher", feature = "test-seams")))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActualProbeReport {
    pub kernel: KernelProbeReport,
    pub small_writes: SmallWritesProbeReport,
    pub identity: IdentityProbeReport,
    pub limit: LimitProbeReport,
    pub control: ControlProbeReport,
    pub reopen: ReopenProbeReport,
    pub trace: TraceProbeReport,
    pub oracle: OracleProbeReport,
}

/// Runs the shared actual-path leaves; the paired schedules share one clean setup.
#[cfg(any(test, all(feature = "graph-cypher", feature = "test-seams")))]
pub fn run_actual_probe(seed: u64) -> Result<ActualProbeReport, String> {
    let kernel = run_kernel_probe(seed)?;
    let small_writes = run_small_writes_probe(seed)?;
    let identity = run_identity_probe(seed);
    let (limit, control) = run_preparation_schedule_probe(seed);
    let reopen = run_reopen_probe(seed);
    let trace = run_trace_probe(seed);
    let oracle = run_oracle_probe(seed);
    Ok(ActualProbeReport {
        kernel,
        small_writes,
        identity,
        limit,
        control,
        reopen,
        trace,
        oracle,
    })
}
