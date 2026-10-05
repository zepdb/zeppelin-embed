//! PG18 actual native adjacency producer, finalized-file reopen and primitive
//! oracle comparison. This is a private participant fixture, not publication or
//! public-view qualification.
use super::coverage::CoverageRegistry;
use std::{
    cell::Cell,
    fs::File,
    io::{Read, Write},
    path::Path,
};
use zeppelin_embed::fts::tokenizer::{TokenizerConfig, TokenizerEpoch};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::property_graph::{
    catalog::*,
    resources::{GraphReservation, GraphResources},
    staging::*,
    storage::{
        adjacency::{
            AdjacencyQuery, AdjacencyRow as NativeAdjacencyRow, Direction as NativeDirection,
            NativeGraphBase, NativeGraphReader, RangeScratch, RelationshipRange as NativeRange,
            RelationshipRow as NativeRelationshipRow, UpperBound, prepare_native_graph,
            validate_range,
        },
        artifact::{
            ArtifactControlError, ArtifactId, ArtifactIdentity, BlockKind, ContainerKind,
            FramedBlock, PhysicalRef, decode_with_control,
        },
        memory::StorageMemory,
        participant::{DirectoryBase, PreparationCatalog},
        prepared::{PackLimits, PreparedObjects},
        records::{NodeRecordState, RecordCatalog, RecordShape, verify_node_state, verify_record},
        stream::PayloadSlice,
        tree::{Key, TreeKind, directory::*},
    },
    wal::{ArtifactDescriptor, CommitState, ReferenceList, RequiredRef, WalGraphRoots},
    *,
};
use zeppelin_embed_adversarial_oracle::graph_adjacency_store as oracle;
use zeppelin_embed_adversarial_oracle::storage_durability::xxh3_64;

pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.adjacency-store.native-history",
    "property-graph.adjacency-store.full-id",
    "property-graph.adjacency-store.same-batch",
    "property-graph.adjacency-store.self-parallel-sparse",
    "property-graph.adjacency-store.property-only",
    "property-graph.adjacency-store.detach-raw",
    "property-graph.adjacency-store.raw-delete",
    "property-graph.adjacency-store.plain-delete-refusal",
    "property-graph.adjacency-store.old-root",
    "property-graph.adjacency-store.reopen",
    "property-graph.adjacency-store.emitted-keys",
    "property-graph.adjacency-store.participant-selection.missing-reverse",
    "property-graph.adjacency-store.participant-selection.ignored-delete",
    "property-graph.adjacency-store.append.fire",
    "property-graph.adjacency-store.append.clean",
    "property-graph.adjacency-store.budget.fire",
    "property-graph.adjacency-store.budget.clean",
    "property-graph.adjacency-store.no-partial-candidate",
    "property-graph.adjacency-store.private-release",
];

#[derive(Debug, Default)]
pub struct Report {
    pub histories: Vec<String>,
    pub observations: Vec<oracle::Observation>,
    pub comparisons: usize,
    pub root_selection_fires: usize,
    pub emitted_files: usize,
    pub emitted_root_keys: usize,
    pub fault_fires: usize,
    pub clean_controls: usize,
    pub refusals: usize,
    pub cleanup_checks: usize,
    pub append_control_appends: usize,
    pub append_fault_appends: usize,
    pub append_abort_objects: usize,
    pub budget_control_work: u64,
    pub budget_fault_limit: u64,
    pub budget_fault_charged: u64,
    pub failed_candidate_root_keys: usize,
    pub storage_baseline_bytes: usize,
    pub storage_after_bytes: usize,
    pub retained_bytes_before_release: u64,
    pub shared_baseline_bytes: u64,
    pub shared_after_release_bytes: u64,
}

#[derive(Clone)]
struct FixtureEntry {
    provenance: OperationProvenance<'static>,
    shape: Option<EntityShape<'static>>,
    canonical: Option<Vec<u8>>,
    membership: Membership,
}

impl CanonicalSource for FixtureEntry {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> std::io::Result<usize> {
        let source = self
            .canonical
            .as_ref()
            .ok_or(std::io::ErrorKind::InvalidData)?;
        let source = source
            .get(usize::try_from(offset).map_err(|_| std::io::ErrorKind::InvalidInput)?..)
            .ok_or(std::io::ErrorKind::UnexpectedEof)?;
        let count = source.len().min(output.len());
        output
            .get_mut(..count)
            .ok_or(std::io::ErrorKind::InvalidInput)?
            .copy_from_slice(
                source
                    .get(..count)
                    .ok_or(std::io::ErrorKind::InvalidInput)?,
            );
        Ok(count)
    }
}

impl FixtureEntry {
    fn live(&self, view: BaseIdentity) -> Option<BaseEntity<'_>> {
        let bytes = self.canonical.as_ref()?;
        Some(BaseEntity {
            view,
            provenance: self.provenance,
            shape: self.shape?,
            fingerprint: CanonicalFingerprint::new(bytes.len() as u64, xxh3_64(bytes)).ok()?,
            source: self,
            membership: self.membership,
        })
    }
}

struct Fixture {
    identity: BaseIdentity,
    high: HighWaters,
    entries: Vec<FixtureEntry>,
    symbols: Vec<SymbolEntry<'static>>,
    ignore_incidents: Cell<bool>,
    _charge: GraphReservation,
}

impl Fixture {
    fn empty(shared: &GraphResources, seed: u64) -> Result<Self, String> {
        let high = HighWaters {
            node: (1_u128 << 120) | (u128::from(seed) << 32),
            relationship: (1_u128 << 124) | (u128::from(seed) << 48),
            symbols: SymbolHighWaters {
                relationship_type: (1_u64 << 60) | ((seed & 0xffff) << 16),
                ..SymbolHighWaters::default()
            },
        };
        Ok(Self {
            identity: BaseIdentity {
                store: StoreInstanceId::new((1_u128 << 112) | u128::from(seed) | 1)
                    .map_err(|error| error.to_string())?,
                generation: GraphGeneration::new(0),
                roots: None,
            },
            high,
            entries: Vec::new(),
            symbols: Vec::new(),
            ignore_incidents: Cell::new(false),
            _charge: shared.reserve(0).map_err(|error| error.to_string())?,
        })
    }

    fn after(&self, batch: &StagedBatch<'_>, shared: &GraphResources) -> Result<Self, String> {
        let descriptor_count = self.entries.len() + batch.deltas().len();
        let symbol_count = self.symbols.len() + batch.symbols().len();
        let canonical_bytes = self
            .entries
            .iter()
            .filter_map(|entry| entry.canonical.as_ref())
            .map(Vec::len)
            .chain(
                batch
                    .deltas()
                    .iter()
                    .filter_map(NormalizedDelta::canonical)
                    .map(<[u8]>::len),
            )
            .sum::<usize>();
        let estimate = descriptor_count
            .checked_mul(std::mem::size_of::<FixtureEntry>())
            .and_then(|bytes| {
                bytes.checked_add(symbol_count * std::mem::size_of::<SymbolEntry<'static>>())
            })
            .and_then(|bytes| bytes.checked_add(canonical_bytes))
            .ok_or_else(|| "PG18 fixture reservation overflow".to_owned())?;
        let mut charge = shared
            .reserve(estimate)
            .map_err(|error| error.to_string())?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(descriptor_count)
            .map_err(|error| error.to_string())?;
        entries.extend(self.entries.iter().cloned());
        for delta in batch.deltas() {
            let fields = delta.provenance().fields();
            let provenance = OperationProvenance::from_fields(
                Some(1),
                OperationFields {
                    operation: fields.operation,
                    key: None,
                    requested_revision: fields.requested_revision,
                    installed_revision: fields.installed_revision,
                    expected: fields.expected,
                    incarnation: fields.incarnation,
                    delete_mode: fields.delete_mode,
                    original_generation: fields.original_generation,
                },
            )
            .map_err(|error| error.to_string())?;
            let shape = match delta.shape() {
                None => None,
                Some(EntityShape::Node) => Some(EntityShape::Node),
                Some(EntityShape::Relationship {
                    source,
                    target,
                    relationship_type,
                }) => Some(EntityShape::Relationship {
                    source,
                    target,
                    relationship_type: static_name(relationship_type.as_str())?,
                }),
            };
            let entry = FixtureEntry {
                provenance,
                shape,
                canonical: delta.canonical().map(<[u8]>::to_vec),
                membership: delta.membership().1,
            };
            if let Some(old) = entries
                .iter_mut()
                .find(|old| old.provenance.fields().incarnation == provenance.fields().incarnation)
            {
                *old = entry;
            } else {
                entries.push(entry);
            }
        }
        let mut symbols = Vec::new();
        symbols
            .try_reserve_exact(symbol_count)
            .map_err(|error| error.to_string())?;
        symbols.extend_from_slice(&self.symbols);
        for entry in batch.symbols() {
            symbols.push(SymbolEntry {
                symbol: entry.symbol,
                name: static_name(entry.name.as_str())?,
            });
        }
        let actual = entries.capacity() * std::mem::size_of::<FixtureEntry>()
            + symbols.capacity() * std::mem::size_of::<SymbolEntry<'static>>()
            + entries
                .iter()
                .filter_map(|entry| entry.canonical.as_ref())
                .map(Vec::capacity)
                .sum::<usize>();
        charge.resize(actual).map_err(|error| error.to_string())?;
        let generation = GraphGeneration::new(self.identity.generation.get() + 1);
        Ok(Self {
            identity: BaseIdentity {
                generation,
                roots: Some(
                    ArtifactId::new((1_u128 << 96) + u128::from(generation.get()))
                        .map_err(|error| error.to_string())?,
                ),
                ..self.identity
            },
            high: batch.high_waters(),
            entries,
            symbols,
            ignore_incidents: Cell::new(false),
            _charge: charge,
        })
    }

    fn shape(&self, entity: EntityId) -> Result<EntityShape<'static>, String> {
        self.entries
            .iter()
            .find(|entry| entry.provenance.fields().incarnation == entity)
            .and_then(|entry| entry.shape)
            .ok_or_else(|| format!("PG18 missing fixture shape {entity:?}"))
    }
}

impl AdmittedBase for Fixture {
    fn identity(&self) -> BaseIdentity {
        self.identity
    }
    fn high_waters(&self) -> HighWaters {
        self.high
    }
    fn interpretation(&self) -> GraphInterpretation<'_> {
        GraphInterpretation::new(TokenizerEpoch::of(&TokenizerConfig::text_default()), None)
            .expect("fixed PG18 interpretation")
    }
    fn key(
        &self,
        _: ApplicationKey<'_>,
        _: &mut WriteControl<'_>,
    ) -> Result<BaseKeyState<'_>, StageError> {
        Ok(BaseKeyState::NeverUsed)
    }
    fn entity(
        &self,
        id: EntityId,
        _: &mut WriteControl<'_>,
    ) -> Result<Option<BaseEntity<'_>>, StageError> {
        Ok(self
            .entries
            .iter()
            .find(|entry| entry.provenance.fields().incarnation == id)
            .and_then(|entry| entry.live(self.identity)))
    }
    fn has_live_incident(
        &self,
        node: NodeId,
        removed: &[RelId],
        _: &mut WriteControl<'_>,
    ) -> Result<bool, StageError> {
        if self.ignore_incidents.get() {
            return Ok(false);
        }
        let live = |id| {
            self.entries.iter().any(|entry| {
                entry.provenance.fields().incarnation == EntityId::Node(id)
                    && entry.canonical.is_some()
            })
        };
        Ok(self.entries.iter().any(|entry| {
            matches!(
                (entry.provenance.fields().incarnation, entry.shape),
                (
                    EntityId::Relationship(id),
                    Some(EntityShape::Relationship { source, target, .. })
                ) if !removed.contains(&id)
                    && live(source)
                    && live(target)
                    && (source == node || target == node)
            )
        }))
    }
    fn property(
        &self,
        _: EntityId,
        _: GraphName<'_>,
        _: &mut WriteControl<'_>,
    ) -> Result<Option<PropertyValue<'_>>, StageError> {
        Ok(None)
    }
    fn stored_text(&self, _: NodeId, _: &mut WriteControl<'_>) -> Result<Option<&str>, StageError> {
        Ok(None)
    }
    fn symbol(
        &self,
        kind: SymbolKind,
        name: GraphName<'_>,
        _: &mut WriteControl<'_>,
    ) -> Result<Option<Symbol>, StageError> {
        Ok(self
            .symbols
            .iter()
            .find(|entry| entry.symbol.kind() == kind && entry.name == name)
            .map(|entry| entry.symbol))
    }
}

impl<S: BlockSource> RecordCatalog<S> for Fixture {
    fn resolve(
        &self,
        kind: SymbolKind,
        name: PayloadSlice<'_, S>,
        r: &mut TreeResources<'_>,
    ) -> Result<Symbol, TreeError> {
        for entry in &self.symbols {
            r.step(1)?;
            if entry.symbol.kind() == kind
                && name
                    .compare_bytes(entry.name.as_str().as_bytes(), r)?
                    .is_eq()
            {
                return Ok(entry.symbol);
            }
        }
        Err(TreeError::Invalid("unknown PG18 catalog symbol"))
    }
}

impl<S: BlockSource> PreparationCatalog<S> for Fixture {
    fn namespace_id(
        &self,
        name: zeppelin_embed::property_graph::GraphName<'_>,
        r: &mut TreeResources<'_>,
    ) -> Result<zeppelin_embed::property_graph::catalog::NamespaceId, TreeError> {
        for entry in &self.symbols {
            r.step(1)?;
            if entry.name == name
                && let Symbol::Namespace(id) = entry.symbol
            {
                return Ok(id);
            }
        }
        Err(TreeError::Invalid("unknown fixture namespace"))
    }
    fn base_identity(&self) -> BaseIdentity {
        self.identity
    }
}

fn static_name(name: &str) -> Result<GraphName<'static>, String> {
    match name {
        "R0" => GraphName::new("R0"),
        "R1" => GraphName::new("R1"),
        "v" => GraphName::new("v"),
        _ => return Err(format!("PG18 unexpected borrowed name {name:?}")),
    }
    .map_err(|error| error.to_string())
}

struct FileImage {
    identity: ArtifactIdentity,
    bytes: Vec<u8>,
    _charge: GraphReservation,
}
struct Files {
    values: Vec<FileImage>,
    _charge: GraphReservation,
}

impl Files {
    fn new(shared: &GraphResources) -> Result<Self, String> {
        const CAPACITY: usize = 64;
        let mut charge = shared
            .reserve(CAPACITY * std::mem::size_of::<FileImage>())
            .map_err(|error| error.to_string())?;
        let mut values = Vec::new();
        values
            .try_reserve_exact(CAPACITY)
            .map_err(|error| error.to_string())?;
        charge
            .resize(values.capacity() * std::mem::size_of::<FileImage>())
            .map_err(|error| error.to_string())?;
        Ok(Self {
            values,
            _charge: charge,
        })
    }
    fn admit(
        &mut self,
        path: &Path,
        identity: ArtifactIdentity,
        shared: &GraphResources,
        r: &mut TreeResources<'_>,
    ) -> Result<(), String> {
        if self.values.len() == self.values.capacity() {
            return Err("PG18 retained file descriptor capacity".to_owned());
        }
        let length = usize::try_from(
            std::fs::metadata(path)
                .map_err(|error| error.to_string())?
                .len(),
        )
        .map_err(|error| error.to_string())?;
        let mut charge = shared.reserve(length).map_err(|error| error.to_string())?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|error| error.to_string())?;
        charge
            .resize(bytes.capacity())
            .map_err(|error| error.to_string())?;
        bytes.resize(length, 0);
        let mut file = File::open(path).map_err(|error| error.to_string())?;
        for chunk in bytes.chunks_mut(65_536) {
            r.step(chunk.len() as u64)
                .map_err(|error| error.to_string())?;
            file.read_exact(chunk).map_err(|error| error.to_string())?;
        }
        let mut extra = [0; 1];
        if file.read(&mut extra).map_err(|error| error.to_string())? != 0 {
            return Err("PG18 reopened file grew during admission".to_owned());
        }
        controlled_frame(&bytes, identity, r)?;
        self.values.push(FileImage {
            identity,
            bytes,
            _charge: charge,
        });
        Ok(())
    }
    fn required(&self, block: PhysicalRef) -> Result<RequiredRef, String> {
        let file = self
            .values
            .iter()
            .find(|file| file.identity.artifact == block.artifact)
            .ok_or_else(|| format!("PG18 missing root object {:?}", block.artifact))?;
        Ok(RequiredRef {
            object: ArtifactDescriptor {
                store: file.identity.store,
                artifact: file.identity.artifact,
                generation: file.identity.generation,
                serial: file.identity.creation_serial,
                bytes: file
                    .bytes
                    .len()
                    .try_into()
                    .map_err(|error: std::num::TryFromIntError| error.to_string())?,
                family: 17,
                version: 1,
                checksum: u64::from_le_bytes(
                    file.bytes[file.bytes.len() - 8..]
                        .try_into()
                        .map_err(|_| "PG18 checksum trailer width".to_owned())?,
                ),
            },
            block,
        })
    }
}

impl BlockSource for Files {
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        r: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        let file = self
            .values
            .iter()
            .find(|file| file.identity.artifact == reference.artifact)
            .ok_or(TreeError::Missing)?;
        let frame = decode_with_control(
            ContainerKind::Object,
            Some((file.identity.store, file.identity.artifact)),
            &file.bytes,
            &mut |bytes| r.step(bytes as u64),
        )
        .map_err(|error| match error {
            ArtifactControlError::Format(error) => TreeError::Format(error),
            ArtifactControlError::Control(error) => error,
        })?;
        frame.framed_block(reference).map_err(TreeError::Format)
    }
}

fn controlled_frame(
    bytes: &[u8],
    identity: ArtifactIdentity,
    r: &mut TreeResources<'_>,
) -> Result<(), String> {
    let frame = decode_with_control(
        ContainerKind::Object,
        Some((identity.store, identity.artifact)),
        bytes,
        &mut |count| r.step(count as u64),
    )
    .map_err(|error| format!("PG18 outer artifact admission {error:?}"))?;
    if frame.identity() != identity {
        return Err("PG18 reopened object identity changed".to_owned());
    }
    Ok(())
}

fn bootstrap(fixture: &Fixture) -> CommitState<'static> {
    let artifact = ArtifactId::new(99).expect("fixed catalog identity");
    CommitState {
        store: fixture.identity.store,
        generation: fixture.identity.generation,
        sequence: 100,
        graph: WalGraphRoots::default(),
        catalog: RequiredRef {
            object: ArtifactDescriptor {
                store: fixture.identity.store,
                artifact,
                generation: fixture.identity.generation,
                serial: 1,
                bytes: 200,
                family: 17,
                version: 1,
                checksum: 0,
            },
            block: PhysicalRef {
                artifact,
                offset: 96,
                length: 24,
                kind: BlockKind::CommitParticipant,
                version: 1,
            },
        },
        vector: None,
        text: None,
        reclaim: None,
        high_waters: zeppelin_embed::property_graph::wal::HighWaters {
            node: fixture.high.node,
            relationship: fixture.high.relationship,
            symbols: [
                fixture.high.symbols.label,
                fixture.high.symbols.relationship_type,
                fixture.high.symbols.property,
                fixture.high.symbols.namespace,
            ],
            creation_serial: 1,
        },
        prepared_inventories: ReferenceList::Values(&[]),
    }
}

fn committed_after(
    previous: CommitState<'static>,
    roots: GraphRoots,
    sequence: u64,
    high: HighWaters,
    files: &Files,
) -> Result<CommitState<'static>, String> {
    let mut graph = WalGraphRoots::default();
    for (slot, reference) in roots.references().into_iter().enumerate() {
        graph.slots[slot] = reference.map(|block| files.required(block)).transpose()?;
    }
    Ok(CommitState {
        store: previous.store,
        generation: roots.generation(),
        sequence,
        graph,
        high_waters: zeppelin_embed::property_graph::wal::HighWaters {
            node: high.node,
            relationship: high.relationship,
            symbols: [
                high.symbols.label,
                high.symbols.relationship_type,
                high.symbols.property,
                high.symbols.namespace,
            ],
            creation_serial: files
                .values
                .iter()
                .map(|file| file.identity.creation_serial)
                .max()
                .unwrap_or(previous.high_waters.creation_serial),
        },
        ..previous
    })
}

fn packed<'a, 'b>(
    files: &'b Files,
    generation: u64,
    store: StoreInstanceId,
    memory: &'a StorageMemory<'a>,
    r: &mut TreeResources<'_>,
) -> Result<
    PreparedObjects<
        'a,
        'b,
        Files,
        impl FnMut() -> Result<ArtifactIdentity, TreeError> + use<'a, 'b>,
    >,
    String,
> {
    let mut serial = generation * 1000;
    PreparedObjects::new(
        files,
        move || {
            serial += 1;
            Ok(ArtifactIdentity {
                store,
                artifact: ArtifactId::new((1_u128 << 80) + u128::from(serial))?,
                generation: GraphGeneration::new(generation),
                creation_serial: serial,
            })
        },
        store,
        GraphGeneration::new(generation),
        PackLimits {
            artifact_bytes: 512 * 1024,
            blocks: 256,
            ..PackLimits::default()
        },
        memory,
        r,
    )
    .map_err(|error| error.to_string())
}

fn relation_name(value: u64, base: u64) -> Result<GraphName<'static>, String> {
    match value.checked_sub(base) {
        Some(1) => GraphName::new("R0"),
        Some(2) => GraphName::new("R1"),
        _ => return Err(format!("PG18 unexpected relationship type {value}")),
    }
    .map_err(|error| error.to_string())
}

fn operation_entity(operation: oracle::Operation) -> EntityId {
    match operation {
        oracle::Operation::CreateNode { id }
        | oracle::Operation::DeleteNode { id, .. }
        | oracle::Operation::PropertyOnly {
            entity_kind: oracle::EntityKind::Node,
            id,
        } => EntityId::Node(NodeId::new(id).expect("nonzero fixture node")),
        oracle::Operation::CreateRelationship { rel, .. }
        | oracle::Operation::DeleteRelationship { rel }
        | oracle::Operation::PropertyOnly {
            entity_kind: oracle::EntityKind::Relationship,
            id: rel,
        } => EntityId::Relationship(RelId::new(rel).expect("nonzero fixture relationship")),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PrepareFault {
    None,
    Append(usize),
}
struct FaultSink<'a, S> {
    inner: &'a mut S,
    fault: PrepareFault,
    appends: usize,
    fired: bool,
}
impl<S: BlockSource> BlockSource for FaultSink<'_, S> {
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        r: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        self.inner.resolve(reference, r)
    }
}
impl<S: BlockSink> BlockSink for FaultSink<'_, S> {
    fn append(
        &mut self,
        kind: BlockKind,
        generation: GraphGeneration,
        bytes: &[u8],
        r: &mut TreeResources<'_>,
    ) -> Result<PhysicalRef, TreeError> {
        self.appends += 1;
        if self.fault == PrepareFault::Append(self.appends) {
            self.fired = true;
            Err(TreeError::Missing)
        } else {
            self.inner.append(kind, generation, bytes, r)
        }
    }
}

struct PreparedBatch {
    fixture: Fixture,
    roots: GraphRoots,
    state: CommitState<'static>,
    emitted: usize,
    root_keys: usize,
}

#[allow(clippy::too_many_arguments)]
fn apply_batch(
    operations: &[oracle::Operation],
    fixture: &Fixture,
    roots: GraphRoots,
    state: CommitState<'static>,
    files: &mut Files,
    shared: &GraphResources,
    writer: &WriteMemory<'_>,
    memory: &StorageMemory<'_>,
    directory: &Path,
    permit_stage_plain_delete: bool,
) -> Result<PreparedBatch, String> {
    let generation = fixture.identity.generation.get() + 1;
    let storage_baseline = memory.reserved_bytes();
    let mut r =
        TreeResources::for_prepare(memory, 400_000_000).map_err(|error| error.to_string())?;
    let empty_node =
        CanonicalContents::node(&mut [], &mut [], None, None).map_err(|error| error.to_string())?;
    let mut changed_properties = [GraphProperty::new(
        GraphName::new("v").map_err(|error| error.to_string())?,
        PropertyValue::new(PropertyData::F64(f64::from_bits(generation)))
            .map_err(|error| error.to_string())?,
    )];
    let changed_node = CanonicalContents::node(&mut [], &mut changed_properties, None, None)
        .map_err(|error| error.to_string())?;
    let relationship_properties = [GraphProperty::new(
        GraphName::new("v").map_err(|error| error.to_string())?,
        PropertyValue::new(PropertyData::F64(f64::from_bits(generation)))
            .map_err(|error| error.to_string())?,
    )];
    let result = with_local_refs(|refs| {
        let mut view = GraphBatchReadView::new(fixture, writer, operations.len(), &mut |_| Ok(()))
            .map_err(|error| error.to_string())?;
        let local_nodes: Vec<_> = operations
            .iter()
            .enumerate()
            .filter_map(|(index, operation)| match operation {
                oracle::Operation::CreateNode { id } => Some((*id, index)),
                _ => None,
            })
            .collect();
        for (index, operation) in operations.iter().copied().enumerate() {
            match operation {
                oracle::Operation::CreateNode { .. } => view
                    .create(
                        BatchEntityRef::Node(NodeRef::Local(
                            refs.node(index).map_err(|error| error.to_string())?,
                        )),
                        WriteImage::Node(&empty_node),
                        &mut |_| Ok(()),
                    )
                    .map_err(|error| error.to_string())?,
                oracle::Operation::CreateRelationship {
                    source,
                    target,
                    relationship_type,
                    ..
                } => {
                    let endpoint = |id| -> Result<NodeRef<'_>, String> {
                        if let Some((_, slot)) = local_nodes.iter().find(|(node, _)| *node == id) {
                            Ok(NodeRef::Local(
                                refs.node(*slot).map_err(|error| error.to_string())?,
                            ))
                        } else {
                            Ok(NodeRef::Existing(
                                NodeId::new(id).map_err(|error| error.to_string())?,
                            ))
                        }
                    };
                    view.create(
                        BatchEntityRef::Relationship(RelRef::Local(
                            refs.relationship(index)
                                .map_err(|error| error.to_string())?,
                        )),
                        WriteImage::Relationship {
                            source: endpoint(source)?,
                            target: endpoint(target)?,
                            relationship_type: relation_name(
                                relationship_type,
                                fixture.high.symbols.relationship_type,
                            )?,
                            properties: &[],
                        },
                        &mut |_| Ok(()),
                    )
                    .map_err(|error| error.to_string())?;
                }
                oracle::Operation::DeleteNode { id, detach } => view
                    .delete(
                        BatchEntityRef::Node(NodeRef::Existing(
                            NodeId::new(id).map_err(|error| error.to_string())?,
                        )),
                        if detach {
                            GraphDeleteMode::Detach
                        } else {
                            GraphDeleteMode::Restrict
                        },
                        &mut |_| Ok(()),
                    )
                    .map_err(|error| error.to_string())?,
                oracle::Operation::DeleteRelationship { rel } => view
                    .delete(
                        BatchEntityRef::Relationship(RelRef::Existing(
                            RelId::new(rel).map_err(|error| error.to_string())?,
                        )),
                        GraphDeleteMode::Restrict,
                        &mut |_| Ok(()),
                    )
                    .map_err(|error| error.to_string())?,
                oracle::Operation::PropertyOnly { entity_kind, id } => {
                    let entity =
                        operation_entity(oracle::Operation::PropertyOnly { entity_kind, id });
                    match (entity, fixture.shape(entity)?) {
                        (EntityId::Node(node), EntityShape::Node) => view
                            .replace(
                                BatchEntityRef::Node(NodeRef::Existing(node)),
                                WriteImage::Node(&changed_node),
                                &mut |_| Ok(()),
                            )
                            .map_err(|error| error.to_string())?,
                        (
                            EntityId::Relationship(rel),
                            EntityShape::Relationship {
                                source,
                                target,
                                relationship_type,
                            },
                        ) => view
                            .replace(
                                BatchEntityRef::Relationship(RelRef::Existing(rel)),
                                WriteImage::Relationship {
                                    source: NodeRef::Existing(source),
                                    target: NodeRef::Existing(target),
                                    relationship_type,
                                    properties: &relationship_properties,
                                },
                                &mut |_| Ok(()),
                            )
                            .map_err(|error| error.to_string())?,
                        _ => return Err("PG18 property-only kind/shape mismatch".to_owned()),
                    }
                }
            }
        }
        fixture.ignore_incidents.set(permit_stage_plain_delete);
        let batch = view
            .finish(&mut |_| Ok(()))
            .map_err(|error| error.to_string());
        fixture.ignore_incidents.set(false);
        let batch = batch?;
        if batch.receipts().len() != operations.len()
            || !batch
                .receipts()
                .iter()
                .zip(operations)
                .all(|(receipt, operation)| receipt.entity == operation_entity(*operation))
        {
            return Err("PG18 staged receipts differ from primitive identities".to_owned());
        }
        let next_fixture = fixture.after(&batch, shared)?;
        let before_files = files.values.len();
        let mut objects = packed(files, generation, fixture.identity.store, memory, &mut r)?;
        let candidate = prepare_native_graph(
            &mut objects,
            &batch,
            NativeGraphBase {
                directories: DirectoryBase {
                    identity: fixture.identity,
                    roots,
                },
                committed: state,
            },
            fixture,
            None,
            memory,
            &mut r,
        )
        .map_err(|error| format!("PG18 prepare generation{generation}: {error}"))?;
        let next_roots = candidate.roots();
        let sequence = candidate.sequence();
        drop(candidate);
        objects
            .finish(&mut r)
            .map_err(|error| format!("PG18 finish generation{generation}: {error}"))?;
        let object_count = objects.len();
        let mut paths = Vec::new();
        paths
            .try_reserve_exact(object_count)
            .map_err(|error| error.to_string())?;
        for index in 0..object_count {
            let object = objects.artifact(index).map_err(|error| error.to_string())?;
            let path = directory.join(format!("{}.graph", object.identity().artifact.get()));
            let mut file = File::create(&path).map_err(|error| error.to_string())?;
            for chunk in object.bytes().chunks(65_536) {
                file.write_all(chunk).map_err(|error| error.to_string())?;
            }
            file.sync_all().map_err(|error| error.to_string())?;
            paths.push((path, object.identity()));
        }
        drop(objects);
        for (path, identity) in paths {
            files
                .admit(&path, identity, shared, &mut r)
                .map_err(|error| format!("PG18 admit generation{generation}: {error}"))?;
        }
        let next_state = committed_after(state, next_roots, sequence, batch.high_waters(), files)
            .map_err(|error| format!("PG18 commit generation{generation}: {error}"))?;
        Ok(PreparedBatch {
            fixture: next_fixture,
            roots: next_roots,
            state: next_state,
            emitted: files.values.len() - before_files,
            root_keys: next_roots.references().into_iter().flatten().count(),
        })
    });
    drop(r);
    let storage_after = memory.reserved_bytes();
    if storage_after != storage_baseline {
        return Err(format!(
            "PG18 generation{generation} private release baseline={storage_baseline} after={storage_after}"
        ));
    }
    result
}

fn observe(
    source: &Files,
    roots: GraphRoots,
    cutoff: u64,
    catalog: &Fixture,
    plan: &oracle::ObservationPlan,
    node_ids: &[u128],
    memory: &StorageMemory<'_>,
) -> Result<oracle::Observation, String> {
    let mut r =
        TreeResources::for_prepare(memory, 1_000_000_000).map_err(|error| error.to_string())?;
    let mut nodes = Vec::new();
    let node_root = roots
        .directory(TreeKind::Nodes)
        .map_err(|error| error.to_string())?;
    let mut node_cursor = DirectoryCursor::seek(source, node_root, None, &mut r)
        .map_err(|error| error.to_string())?;
    while let Some(entry) = node_cursor
        .next_entry(&mut r)
        .map_err(|error| error.to_string())?
    {
        let Key::Inline(key) = entry.key() else {
            return Err("PG18 overflow node identity".to_owned());
        };
        let id = NodeId::new(u128::from_le_bytes(
            key.try_into()
                .map_err(|_| "PG18 node identity width".to_owned())?,
        ))
        .map_err(|error| error.to_string())?;
        let reference =
            zeppelin_embed::property_graph::storage::payload::PayloadRef::decode(entry.value())
                .map_err(|error| error.to_string())?;
        let live = matches!(
            verify_node_state(
                PayloadSlice::new(
                    source,
                    roots.store(),
                    entry.creation_generation(),
                    reference
                ),
                id,
                catalog,
                None,
                &mut r,
            )
            .map_err(|error| error.to_string())?,
            NodeRecordState::Live(_)
        );
        nodes.push(oracle::NodeLiveness { id: id.get(), live });
    }
    let mut raw_relationships = Vec::new();
    let relationship_root = roots
        .directory(TreeKind::Relationships)
        .map_err(|error| error.to_string())?;
    let mut relationship_cursor = DirectoryCursor::seek(source, relationship_root, None, &mut r)
        .map_err(|error| error.to_string())?;
    while let Some(entry) = relationship_cursor
        .next_entry(&mut r)
        .map_err(|error| error.to_string())?
    {
        let Key::Inline(key) = entry.key() else {
            return Err("PG18 overflow relationship identity".to_owned());
        };
        let rel = RelId::new(u128::from_le_bytes(
            key.try_into()
                .map_err(|_| "PG18 relationship identity width".to_owned())?,
        ))
        .map_err(|error| error.to_string())?;
        let reference =
            zeppelin_embed::property_graph::storage::payload::PayloadRef::decode(entry.value())
                .map_err(|error| error.to_string())?;
        let record = verify_record(
            PayloadSlice::new(
                source,
                roots.store(),
                entry.creation_generation(),
                reference,
            ),
            EntityId::Relationship(rel),
            catalog,
            None,
            &mut r,
        )
        .map_err(|error| error.to_string())?;
        let RecordShape::Relationship {
            id,
            source: from,
            target,
            relationship_type,
        } = record.shape()
        else {
            return Err("PG18 relationship directory role".to_owned());
        };
        raw_relationships.push(oracle::RelationshipRow {
            rel: id.get(),
            source: from.get(),
            target: target.get(),
            relationship_type: relationship_type.get(),
        });
    }
    let mut raw_outgoing = Vec::new();
    let mut raw_incoming = Vec::new();
    let mut scratch = RangeScratch::for_prepare(memory, &mut r).map_err(|e| e.to_string())?;
    for (kind, output) in [
        (TreeKind::OutRanges, &mut raw_outgoing),
        (TreeKind::InRanges, &mut raw_incoming),
    ] {
        let root = roots.directory(kind).map_err(|error| error.to_string())?;
        let mut cursor =
            DirectoryCursor::seek(source, root, None, &mut r).map_err(|e| e.to_string())?;
        while let Some(entry) = cursor.next_entry(&mut r).map_err(|e| e.to_string())? {
            let range = validate_range(source, root, entry, cutoff, &mut scratch, &mut r)
                .map_err(|error| error.to_string())?;
            for edge in range.edges() {
                output.push(oracle::AdjacencyRow {
                    bound_node: range.descriptor().key().node.get(),
                    relationship_type: range.descriptor().key().rel_type.get(),
                    rel: edge.rel.get(),
                    neighbor: edge.neighbor.get(),
                });
            }
        }
    }
    let reader = NativeGraphReader::new(source, roots, cutoff, catalog, None);
    let placeholder_relationship = NativeRelationshipRow {
        rel: RelId::new(1).expect("minimum rel"),
        source: NodeId::new(1).expect("minimum node"),
        target: NodeId::new(1).expect("minimum node"),
        relationship_type: RelTypeId::new(1).expect("minimum type"),
    };
    let mut visible_buffer = vec![placeholder_relationship; raw_relationships.len()];
    let visible_count = reader
        .scan_relationships(
            NativeRange {
                lower: RelId::new(1).expect("minimum rel"),
                upper: UpperBound::Infinity,
            },
            &mut visible_buffer,
            &mut r,
        )
        .map_err(|error| error.to_string())?;
    visible_buffer.truncate(visible_count);
    let visible_relationships = visible_buffer
        .iter()
        .map(native_relationship)
        .collect::<Vec<_>>();
    let mut visible_outgoing = Vec::new();
    let mut visible_incoming = Vec::new();
    for (direction, output) in [
        (NativeDirection::Out, &mut visible_outgoing),
        (NativeDirection::In, &mut visible_incoming),
    ] {
        for id in node_ids {
            let placeholder = NativeAdjacencyRow {
                relationship_type: RelTypeId::new(1).expect("minimum type"),
                edge: zeppelin_embed::property_graph::storage::adjacency::Edge {
                    rel: RelId::new(1).expect("minimum rel"),
                    neighbor: NodeId::new(1).expect("minimum node"),
                },
            };
            let mut rows = vec![placeholder; raw_relationships.len()];
            let count = reader
                .expand(
                    AdjacencyQuery {
                        node: NodeId::new(*id).map_err(|error| error.to_string())?,
                        direction,
                        relationship_type: None,
                        relationships: NativeRange {
                            lower: RelId::new(1).expect("minimum rel"),
                            upper: UpperBound::Infinity,
                        },
                    },
                    &mut rows,
                    &mut scratch,
                    &mut r,
                )
                .map_err(|error| error.to_string())?;
            rows.truncate(count);
            output.extend(rows.iter().map(|row| oracle::AdjacencyRow {
                bound_node: *id,
                relationship_type: row.relationship_type.get(),
                rel: row.edge.rel.get(),
                neighbor: row.edge.neighbor.get(),
            }));
        }
    }
    let mut degrees = Vec::new();
    for query in &plan.degrees {
        degrees.push(oracle::DegreeResult {
            query: *query,
            degree: reader
                .degree(
                    NodeId::new(query.node).map_err(|error| error.to_string())?,
                    native_direction(query.direction),
                    query
                        .relationship_type
                        .map(RelTypeId::new)
                        .transpose()
                        .map_err(|error| error.to_string())?,
                    &mut scratch,
                    &mut r,
                )
                .map_err(|error| error.to_string())?,
        });
    }
    let mut relationship_ranges = Vec::new();
    for query in &plan.relationship_ranges {
        let mut rows = vec![placeholder_relationship; query.capacity];
        let count = reader
            .scan_relationships(
                NativeRange {
                    lower: RelId::new(query.start).map_err(|error| error.to_string())?,
                    upper: upper(query.end)?,
                },
                &mut rows,
                &mut r,
            )
            .map_err(|error| error.to_string())?;
        rows.truncate(count);
        relationship_ranges.push(oracle::RelationshipRangeResult {
            query: *query,
            rows: rows.iter().map(native_relationship).collect(),
        });
    }
    let mut adjacency_ranges = Vec::new();
    for query in &plan.adjacency_ranges {
        if query.end.is_some_and(|end| {
            end.bound_node != query.start.bound_node
                || end.relationship_type != query.start.relationship_type
        }) {
            return Err("PG18 plan crosses a native adjacency group".to_owned());
        }
        let placeholder = NativeAdjacencyRow {
            relationship_type: RelTypeId::new(1).expect("minimum type"),
            edge: zeppelin_embed::property_graph::storage::adjacency::Edge {
                rel: RelId::new(1).expect("minimum rel"),
                neighbor: NodeId::new(1).expect("minimum node"),
            },
        };
        let mut rows = vec![placeholder; query.capacity];
        let count = reader
            .expand(
                AdjacencyQuery {
                    node: NodeId::new(query.start.bound_node).map_err(|error| error.to_string())?,
                    direction: native_direction(query.direction),
                    relationship_type: Some(
                        RelTypeId::new(query.start.relationship_type)
                            .map_err(|error| error.to_string())?,
                    ),
                    relationships: NativeRange {
                        lower: RelId::new(query.start.rel).map_err(|error| error.to_string())?,
                        upper: upper(query.end.map(|end| end.rel))?,
                    },
                },
                &mut rows,
                &mut scratch,
                &mut r,
            )
            .map_err(|error| error.to_string())?;
        rows.truncate(count);
        adjacency_ranges.push(oracle::AdjacencyRangeResult {
            query: *query,
            rows: rows
                .iter()
                .map(|row| oracle::AdjacencyRow {
                    bound_node: query.start.bound_node,
                    relationship_type: row.relationship_type.get(),
                    rel: row.edge.rel.get(),
                    neighbor: row.edge.neighbor.get(),
                })
                .collect(),
        });
    }
    Ok(oracle::Observation {
        generation: roots.generation().get(),
        nodes,
        raw_relationships,
        raw_outgoing,
        raw_incoming,
        visible_relationships,
        visible_outgoing,
        visible_incoming,
        visible_relationship_count: reader
            .relationship_count(&mut r)
            .map_err(|error| error.to_string())?,
        degrees,
        relationship_ranges,
        adjacency_ranges,
    })
}

fn native_relationship(row: &NativeRelationshipRow) -> oracle::RelationshipRow {
    oracle::RelationshipRow {
        rel: row.rel.get(),
        source: row.source.get(),
        target: row.target.get(),
        relationship_type: row.relationship_type.get(),
    }
}
const fn native_direction(direction: oracle::Direction) -> NativeDirection {
    match direction {
        oracle::Direction::Outgoing => NativeDirection::Out,
        oracle::Direction::Incoming => NativeDirection::In,
    }
}
fn upper(value: Option<u128>) -> Result<UpperBound, String> {
    value.map_or(Ok(UpperBound::Infinity), |value| {
        RelId::new(value)
            .map(UpperBound::Exclusive)
            .map_err(|error| error.to_string())
    })
}
fn changed_roots(
    current: GraphRoots,
    previous: GraphRoots,
    slots: &[usize],
) -> Result<GraphRoots, String> {
    // This is an observer/participant-selection control built after a clean
    // production candidate. Actual producer mutants are separate RED receipts.
    let mut references = current.references();
    let old = previous.references();
    for slot in slots {
        references[*slot] = old[*slot];
    }
    GraphRoots::from_references(current.store(), current.generation(), references)
        .map_err(|error| error.to_string())
}

#[derive(Debug)]
struct FaultReceipt {
    success: bool,
    fired: bool,
    appends: usize,
    abort_objects: usize,
    root_keys: usize,
    work: u64,
    baseline_bytes: usize,
    remaining_bytes: usize,
    error: Option<String>,
}

fn fault_attempt(seed: u64, fault: PrepareFault, work: u64) -> Result<FaultReceipt, String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .map_err(|error| error.to_string())?;
    let shared = GraphResources::from_store(&store).map_err(|error| error.to_string())?;
    let writer =
        WriteMemory::new(&shared, WriteLimits::default()).map_err(|error| error.to_string())?;
    let control = QueryControl::Cancel(CancelToken::new());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024)
        .map_err(|error| error.to_string())?;
    let files = Files::new(&shared)?;
    let fixture = Fixture::empty(&shared, seed)?;
    let roots = GraphRoots::from_references(
        fixture.identity.store,
        fixture.identity.generation,
        [None; 8],
    )
    .map_err(|error| error.to_string())?;
    let state = bootstrap(&fixture);
    let mut r = TreeResources::for_prepare(&memory, work).map_err(|error| error.to_string())?;
    let baseline = memory.reserved_bytes();
    let image =
        CanonicalContents::node(&mut [], &mut [], None, None).map_err(|error| error.to_string())?;
    with_local_refs(|refs| {
        let mut view = GraphBatchReadView::new(&fixture, &writer, 3, &mut |_| Ok(()))
            .map_err(|error| error.to_string())?;
        for slot in 0..2 {
            view.create(
                BatchEntityRef::Node(NodeRef::Local(
                    refs.node(slot).map_err(|error| error.to_string())?,
                )),
                WriteImage::Node(&image),
                &mut |_| Ok(()),
            )
            .map_err(|error| error.to_string())?;
        }
        view.create(
            BatchEntityRef::Relationship(RelRef::Local(
                refs.relationship(2).map_err(|error| error.to_string())?,
            )),
            WriteImage::Relationship {
                source: NodeRef::Local(refs.node(0).map_err(|error| error.to_string())?),
                target: NodeRef::Local(refs.node(1).map_err(|error| error.to_string())?),
                relationship_type: GraphName::new("R0").map_err(|error| error.to_string())?,
                properties: &[],
            },
            &mut |_| Ok(()),
        )
        .map_err(|error| error.to_string())?;
        let batch = view
            .finish(&mut |_| Ok(()))
            .map_err(|error| error.to_string())?;
        let mut objects = packed(&files, 1, fixture.identity.store, &memory, &mut r)?;
        let (success, fired, appends, root_keys, error) = {
            let mut sink = FaultSink {
                inner: &mut objects,
                fault,
                appends: 0,
                fired: false,
            };
            let result = prepare_native_graph(
                &mut sink,
                &batch,
                NativeGraphBase {
                    directories: DirectoryBase {
                        identity: fixture.identity,
                        roots,
                    },
                    committed: state,
                },
                &fixture,
                None,
                &memory,
                &mut r,
            );
            let root_keys = result.as_ref().map_or(0, |candidate| {
                candidate.roots().references().into_iter().flatten().count()
            });
            let success = result.is_ok();
            let error = result.as_ref().err().map(ToString::to_string);
            drop(result);
            let fired = sink.fired || error.as_deref() == Some("graph directory work exhausted");
            (success, fired, sink.appends, root_keys, error)
        };
        let abort_objects = objects.abort_inventory().count();
        drop(objects);
        let remaining_bytes = memory.reserved_bytes();
        Ok(FaultReceipt {
            success,
            fired,
            appends,
            abort_objects,
            root_keys,
            work: r.work(),
            baseline_bytes: baseline,
            remaining_bytes,
            error,
        })
    })
}

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<Report, String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let store = Store::open(
        directory.path().join("store"),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .map_err(|error| error.to_string())?;
    let shared = GraphResources::from_store(&store).map_err(|error| error.to_string())?;
    let writer =
        WriteMemory::new(&shared, WriteLimits::default()).map_err(|error| error.to_string())?;
    let control = QueryControl::Cancel(CancelToken::new());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024)
        .map_err(|error| error.to_string())?;
    let storage_baseline = memory.reserved_bytes();
    let shared_baseline = shared.reserved_bytes().map_err(|error| error.to_string())?;
    let mut files = Files::new(&shared)?;
    let mut fixture = Fixture::empty(&shared, seed)?;
    let mut roots = GraphRoots::from_references(
        fixture.identity.store,
        fixture.identity.generation,
        [None; 8],
    )
    .map_err(|error| error.to_string())?;
    let mut state = bootstrap(&fixture);
    let node = fixture.high.node;
    let rel = fixture.high.relationship;
    let rel_type = fixture.high.symbols.relationship_type;
    let nodes = [node + 1, node + 2, node + 3, node + 4, node + 5];
    let relationships = [rel + 1, rel + 2, rel + 3, rel + 4, rel + 5];
    let initial = [
        oracle::Operation::CreateNode { id: nodes[0] },
        oracle::Operation::CreateNode { id: nodes[1] },
        oracle::Operation::CreateNode { id: nodes[2] },
        oracle::Operation::CreateNode { id: nodes[3] },
        oracle::Operation::CreateNode { id: nodes[4] },
        oracle::Operation::CreateRelationship {
            rel: relationships[0],
            source: nodes[0],
            target: nodes[1],
            relationship_type: rel_type + 1,
        },
        oracle::Operation::CreateRelationship {
            rel: relationships[1],
            source: nodes[0],
            target: nodes[1],
            relationship_type: rel_type + 1,
        },
        oracle::Operation::CreateRelationship {
            rel: relationships[2],
            source: nodes[0],
            target: nodes[0],
            relationship_type: rel_type + 1,
        },
        oracle::Operation::CreateRelationship {
            rel: relationships[3],
            source: nodes[4],
            target: nodes[0],
            relationship_type: rel_type + 2,
        },
        oracle::Operation::CreateRelationship {
            rel: relationships[4],
            source: nodes[1],
            target: nodes[4],
            relationship_type: rel_type + 2,
        },
    ];
    let plan = oracle::ObservationPlan {
        relationship_ranges: vec![oracle::RelationshipRange {
            start: relationships[0],
            end: Some(relationships[4]),
            capacity: 2,
        }],
        adjacency_ranges: vec![oracle::AdjacencyRange {
            direction: oracle::Direction::Outgoing,
            start: oracle::AdjacencyRow {
                bound_node: nodes[0],
                relationship_type: rel_type + 1,
                rel: relationships[0],
                neighbor: 0,
            },
            end: Some(oracle::AdjacencyRow {
                bound_node: nodes[0],
                relationship_type: rel_type + 1,
                rel: relationships[3],
                neighbor: 0,
            }),
            capacity: 2,
        }],
        degrees: vec![
            oracle::DegreeQuery {
                node: nodes[0],
                direction: oracle::Direction::Outgoing,
                relationship_type: None,
            },
            oracle::DegreeQuery {
                node: nodes[1],
                direction: oracle::Direction::Incoming,
                relationship_type: Some(rel_type + 1),
            },
        ],
    };
    let mut model = oracle::Model::new();
    let mut report = Report::default();
    let previous_roots = roots;
    let first = apply_batch(
        &initial,
        &fixture,
        roots,
        state,
        &mut files,
        &shared,
        &writer,
        &memory,
        directory.path(),
        false,
    )?;
    model
        .apply(1, &initial)
        .map_err(|error| format!("{error:?}"))?;
    fixture = first.fixture;
    roots = first.roots;
    state = first.state;
    report.emitted_files += first.emitted;
    report.emitted_root_keys += first.root_keys;
    report.cleanup_checks += 1;
    let observed = observe(
        &files,
        roots,
        state.sequence,
        &fixture,
        &plan,
        &nodes,
        &memory,
    )?;
    model
        .snapshot()
        .check(&plan, &observed)
        .map_err(|difference| format!("PG18 generation1 {difference:?}"))?;
    report.comparisons += 1;
    report.observations.push(observed.clone());
    report.histories.push(format!("{:?}", initial));

    for key in &REQUIRED_COVERAGE[..4] {
        coverage.hit(*key);
    }
    coverage.hit(REQUIRED_COVERAGE[9]);
    coverage.hit(REQUIRED_COVERAGE[10]);
    let missing_reverse = changed_roots(roots, previous_roots, &[6])?;
    let malformed = observe(
        &files,
        missing_reverse,
        state.sequence,
        &fixture,
        &plan,
        &nodes,
        &memory,
    )?;
    if model.snapshot().check(&plan, &malformed).is_ok() {
        return Err("PG18 oracle accepted root selection missing IN".to_owned());
    }
    report.root_selection_fires += 1;
    coverage.hit(REQUIRED_COVERAGE[11]);

    let property_only = [
        oracle::Operation::PropertyOnly {
            entity_kind: oracle::EntityKind::Node,
            id: nodes[2],
        },
        oracle::Operation::PropertyOnly {
            entity_kind: oracle::EntityKind::Relationship,
            id: relationships[1],
        },
    ];
    let second = apply_batch(
        &property_only,
        &fixture,
        roots,
        state,
        &mut files,
        &shared,
        &writer,
        &memory,
        directory.path(),
        false,
    )?;
    model
        .apply(2, &property_only)
        .map_err(|error| format!("{error:?}"))?;
    fixture = second.fixture;
    roots = second.roots;
    state = second.state;
    report.emitted_files += second.emitted;
    report.emitted_root_keys += second.root_keys;
    report.cleanup_checks += 1;
    let retained_roots = roots;
    let retained_snapshot = model.snapshot();
    let observed = observe(
        &files,
        roots,
        state.sequence,
        &fixture,
        &plan,
        &nodes,
        &memory,
    )?;
    model
        .snapshot()
        .check(&plan, &observed)
        .map_err(|difference| format!("PG18 property-only {difference:?}"))?;
    report.comparisons += 1;
    report.observations.push(observed.clone());
    report.histories.push(format!("{:?}", property_only));

    coverage.hit(REQUIRED_COVERAGE[4]);

    let plain_delete = [oracle::Operation::DeleteNode {
        id: nodes[0],
        detach: false,
    }];
    let refusal = apply_batch(
        &plain_delete,
        &fixture,
        roots,
        state,
        &mut files,
        &shared,
        &writer,
        &memory,
        directory.path(),
        true,
    );
    if !refusal
        .as_ref()
        .is_err_and(|error| error.contains("plain node deletion retains a live incident"))
    {
        return Err("PG18 plain DELETE did not reach typed native refusal".to_owned());
    }
    if model.apply(3, &plain_delete) != Err(oracle::Error::State("incident_relationship")) {
        return Err("PG18 primitive model did not refuse plain DELETE".to_owned());
    }
    report.refusals += 1;
    report.cleanup_checks += 1;
    coverage.hit(REQUIRED_COVERAGE[7]);

    let detach = [oracle::Operation::DeleteNode {
        id: nodes[1],
        detach: true,
    }];
    let third = apply_batch(
        &detach,
        &fixture,
        roots,
        state,
        &mut files,
        &shared,
        &writer,
        &memory,
        directory.path(),
        false,
    )?;
    model
        .apply(3, &detach)
        .map_err(|error| format!("{error:?}"))?;
    fixture = third.fixture;
    roots = third.roots;
    state = third.state;
    report.emitted_files += third.emitted;
    report.emitted_root_keys += third.root_keys;
    report.cleanup_checks += 1;
    let observed = observe(
        &files,
        roots,
        state.sequence,
        &fixture,
        &plan,
        &nodes,
        &memory,
    )?;
    model
        .snapshot()
        .check(&plan, &observed)
        .map_err(|difference| format!("PG18 detach {difference:?}"))?;
    report.comparisons += 1;
    report.observations.push(observed.clone());
    report.histories.push(format!("{:?}", detach));

    coverage.hit(REQUIRED_COVERAGE[5]);

    let before_delete = roots;
    let delete = [oracle::Operation::DeleteRelationship {
        rel: relationships[0],
    }];
    let fourth = apply_batch(
        &delete,
        &fixture,
        roots,
        state,
        &mut files,
        &shared,
        &writer,
        &memory,
        directory.path(),
        false,
    )?;
    model
        .apply(4, &delete)
        .map_err(|error| format!("{error:?}"))?;
    fixture = fourth.fixture;
    roots = fourth.roots;
    state = fourth.state;
    report.emitted_files += fourth.emitted;
    report.emitted_root_keys += fourth.root_keys;
    report.cleanup_checks += 1;
    let observed = observe(
        &files,
        roots,
        state.sequence,
        &fixture,
        &plan,
        &nodes,
        &memory,
    )?;
    model
        .snapshot()
        .check(&plan, &observed)
        .map_err(|difference| format!("PG18 raw delete {difference:?}"))?;
    report.comparisons += 1;
    report.observations.push(observed.clone());
    report.histories.push(format!("{:?}", delete));

    coverage.hit(REQUIRED_COVERAGE[6]);
    let ignored_delete = changed_roots(roots, before_delete, &[1, 5, 6])?;
    let malformed = observe(
        &files,
        ignored_delete,
        state.sequence,
        &fixture,
        &plan,
        &nodes,
        &memory,
    )?;
    if model.snapshot().check(&plan, &malformed).is_ok() {
        return Err("PG18 oracle accepted root selection before delete".to_owned());
    }
    report.root_selection_fires += 1;
    coverage.hit(REQUIRED_COVERAGE[12]);
    let retained = observe(
        &files,
        retained_roots,
        102,
        &fixture,
        &plan,
        &nodes,
        &memory,
    )?;
    retained_snapshot
        .check(&plan, &retained)
        .map_err(|difference| format!("PG18 retained old roots {difference:?}"))?;
    report.comparisons += 1;
    report.observations.push(retained.clone());

    coverage.hit(REQUIRED_COVERAGE[8]);
    if report.emitted_files == 0 || report.emitted_root_keys < 8 {
        return Err(format!("PG18 missing emitted receipts {report:?}"));
    }
    if memory.peak_reserved_bytes() > 32 * 1024 * 1024 {
        return Err("PG18 exceeded authentic storage reservation".to_owned());
    }
    let append_control = fault_attempt(seed, PrepareFault::None, 400_000_000)?;
    if !append_control.success
        || append_control.appends < 2
        || append_control.root_keys < 5
        || append_control.remaining_bytes != append_control.baseline_bytes
    {
        return Err(format!("PG18 append clean control {append_control:?}"));
    }
    report.clean_controls += 1;
    report.cleanup_checks += 1;
    report.append_control_appends = append_control.appends;
    coverage.hit(REQUIRED_COVERAGE[14]);
    let stop = append_control.appends / 2;
    let append_fault = fault_attempt(seed, PrepareFault::Append(stop), 400_000_000)?;
    if append_fault.success
        || !append_fault.fired
        || append_fault.appends != stop
        || append_fault.error.as_deref() != Some("missing graph directory artifact")
        || append_fault.abort_objects == 0
        || append_fault.remaining_bytes != append_fault.baseline_bytes
    {
        return Err(format!("PG18 append fault receipt {append_fault:?}"));
    }
    report.fault_fires += 1;
    report.cleanup_checks += 1;
    report.append_fault_appends = append_fault.appends;
    report.append_abort_objects = append_fault.abort_objects;
    report.failed_candidate_root_keys += append_fault.root_keys;
    coverage.hit(REQUIRED_COVERAGE[13]);
    coverage.hit(REQUIRED_COVERAGE[17]);

    let budget_control = fault_attempt(seed, PrepareFault::None, 400_000_000)?;
    if !budget_control.success
        || budget_control.work == 0
        || budget_control.remaining_bytes != budget_control.baseline_bytes
    {
        return Err(format!("PG18 budget clean control {budget_control:?}"));
    }
    report.clean_controls += 1;
    report.cleanup_checks += 1;
    report.budget_control_work = budget_control.work;
    coverage.hit(REQUIRED_COVERAGE[16]);
    let budget_fault = fault_attempt(
        seed,
        PrepareFault::None,
        budget_control.work.saturating_sub(1),
    )?;
    if budget_fault.success
        || !budget_fault.fired
        || budget_fault.error.as_deref() != Some("graph directory work exhausted")
        || budget_fault.remaining_bytes != budget_fault.baseline_bytes
    {
        return Err(format!("PG18 budget fault receipt {budget_fault:?}"));
    }
    report.fault_fires += 1;
    report.cleanup_checks += 1;
    report.budget_fault_limit = budget_control.work.saturating_sub(1);
    report.budget_fault_charged = budget_fault.work;
    report.failed_candidate_root_keys += budget_fault.root_keys;
    coverage.hit(REQUIRED_COVERAGE[15]);
    coverage.hit(REQUIRED_COVERAGE[17]);
    report.storage_baseline_bytes = storage_baseline;
    report.storage_after_bytes = memory.reserved_bytes();
    if report.storage_after_bytes != storage_baseline {
        return Err(format!(
            "PG18 terminal storage release baseline={storage_baseline} after={}",
            report.storage_after_bytes
        ));
    }
    report.retained_bytes_before_release =
        shared.reserved_bytes().map_err(|error| error.to_string())?;
    report.shared_baseline_bytes = shared_baseline;
    if report.retained_bytes_before_release <= shared_baseline {
        return Err("PG18 retained fixture/file owners did not reserve bytes".to_owned());
    }
    drop(files);
    drop(fixture);
    report.shared_after_release_bytes =
        shared.reserved_bytes().map_err(|error| error.to_string())?;
    if report.shared_after_release_bytes != shared_baseline {
        return Err(format!(
            "PG18 shared release baseline={shared_baseline} after={}",
            report.shared_after_release_bytes
        ));
    }
    report.cleanup_checks += 1;
    coverage.hit(REQUIRED_COVERAGE[18]);
    Ok(report)
}
