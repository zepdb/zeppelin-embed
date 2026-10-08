use super::persistence::artifact_descriptor;
use super::{NativeGraphBundleInput, NativeGraphError};
use crate::epoch::EmbeddingTower;
use crate::lifecycle::{AccessMode, CancelToken, MonotonicClock, OpenOptions, QueryControl, Store};
use crate::property_graph::NodeId;
use crate::property_graph::catalog::{
    CatalogError, CatalogImage, GraphInterpretation, Symbol, SymbolEntry, SymbolHighWaters,
    SymbolKind,
};
use crate::property_graph::resources::{GraphReservation, GraphResources};
use crate::property_graph::staging::{BaseIdentity, WriteLimits, WriteMemory};
use crate::property_graph::storage::NativeReadonlyMapping;
use crate::property_graph::storage::adjacency::{
    Direction, RangeScratch, RelationshipRow, validate_range,
};
use crate::property_graph::storage::artifact::{
    self, ArtifactControlError, ArtifactId, ArtifactIdentity, BlockKind, ContainerKind,
    FramedBlock, MAX_ARTIFACT_BYTES, PhysicalRef, ValidatedArtifact,
};
use crate::property_graph::storage::inventory::verify_inventory_entry;
use crate::property_graph::storage::memory::{StorageBuffer, StorageMemory, StorageReservation};
use crate::property_graph::storage::payload::PayloadRef;
use crate::property_graph::storage::reclaim::TraceReferenceVisitor;
use crate::property_graph::storage::records::{
    FenceView, NativeDirectoryValues, NodeRecordState, RecordCatalog, RecordShape, RecordView,
    StoredKey, StoredProvenance, fence_window_reference, verify_fence_entry, verify_node_state,
    verify_record,
};
use crate::property_graph::storage::stream::PayloadSlice;
use crate::property_graph::storage::tree::{
    TreeKind,
    directory::{
        BlockSource, DirectoryCursor, FenceKey, GraphRoots, LeafValidator, TreeError,
        TreeResources, lookup_entry, lookup_fence_entry, verify_directory,
    },
};
use crate::property_graph::wal::FramedCaptureStep;
use crate::property_graph::wal::{
    ArtifactDescriptor, Change, ChangeReader, CommitState, InventoryChange, InventoryState,
    Mutation, ParticipantRole, ReclaimComplete, ReclaimIntent, Replay, ReplayStep, ReplayValidator,
    RequiredRef, RequiredRole, STACK_RESERVATION_BYTES, WalError, WalResources,
    validate_required_block,
};
use crate::property_graph::{
    ApplicationKey, CanonicalError, CanonicalFingerprint, CanonicalRecord, CurrentEntity,
    CypherEdit, EntityId, EntityShape, ExpectedGraphState, GraphOperation, KeyDecision, KeyRequest,
    KeyState, MAX_GRAPH_CHANGES, OperationProvenance, classify_cypher, classify_key,
};
use crate::vfs::Vfs;
use std::cell::{Cell, OnceCell, RefCell};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

const MAX_RECOVERED_DESCRIPTORS: usize = crate::property_graph::storage::MAX_NATIVE_ARTIFACTS;

#[cfg(any(test, feature = "test-seams"))]
thread_local! {
    /// Full captured-state traversals on this thread, counting both the
    /// producer's proof and the recovery retrace. ZE-163 gates the count.
    static STATE_TRACE_COUNT: Cell<u64> = const { Cell::new(0) };
    static SERIAL_PROBES: Cell<u64> = const { Cell::new(0) };
    static FENCE_CANDIDATES: Cell<u64> = const { Cell::new(0) };
    static OPEN_METRICS: Cell<(u64, usize, u64, u64)> = const { Cell::new((0, 0, 0, 0)) };
}

#[cfg(any(test, feature = "test-seams"))]
pub(crate) fn take_state_trace_count_for_test() -> u64 {
    STATE_TRACE_COUNT.with(|count| count.replace(0))
}

#[cfg(test)]
pub(crate) fn open_metrics_for_test() -> (u64, usize, u64, u64) {
    OPEN_METRICS.with(Cell::get)
}

#[cfg(test)]
#[derive(Clone, Debug, Default)]
pub(crate) struct ReopenProfile {
    pub phases: Vec<(&'static str, std::time::Duration)>,
    pub nested: Vec<(&'static str, std::time::Duration, usize)>,
}
#[cfg(test)]
thread_local! {
    static REOPEN_PROFILE: RefCell<ReopenProfile> = RefCell::new(ReopenProfile::default());
}
#[cfg(test)]
pub(crate) fn reopen_profile_for_test() -> ReopenProfile {
    REOPEN_PROFILE.with(|report| report.borrow().clone())
}

#[cfg(test)]
struct ReopenTimer(&'static str, std::time::Instant, usize);
#[cfg(test)]
impl ReopenTimer {
    fn new(name: &'static str, bytes: usize) -> Self {
        Self(name, std::time::Instant::now(), bytes)
    }
}
#[cfg(test)]
impl Drop for ReopenTimer {
    fn drop(&mut self) {
        REOPEN_PROFILE.with(|report| {
            report
                .borrow_mut()
                .nested
                .push((self.0, self.1.elapsed(), self.2))
        });
    }
}
macro_rules! reopen_validation {
    ($kind:expr, $identity:expr, $bytes:expr $(, $control:expr)? $(,)?) => {{
        #[cfg(test)]
        let _timer = ReopenTimer::new("artifact_validation", $bytes.len());
        artifact::decode($kind, $identity, $bytes)
    }};
}
macro_rules! reopen_controlled_validation {
    ($kind:expr, $identity:expr, $bytes:expr, $control:expr $(,)?) => {{
        #[cfg(test)]
        let _timer = ReopenTimer::new("artifact_validation", $bytes.len());
        #[cfg(all(test, feature = "graph-cypher"))]
        if let Some((_, artifact)) = $identity {
            crate::property_graph::storage::preparation_work_capture::artifact(
                artifact,
                $bytes.len() as u64,
            );
        }
        artifact::decode_with_control($kind, $identity, $bytes, $control)
    }};
}
#[cfg(test)]
fn reopen_phase(name: &'static str, start: &mut std::time::Instant) {
    let now = std::time::Instant::now();
    REOPEN_PROFILE.with(|report| {
        report
            .borrow_mut()
            .phases
            .push((name, now.duration_since(*start)))
    });
    *start = now;
}

#[cfg(test)]
pub(crate) fn serial_probes_for_test() -> u64 {
    SERIAL_PROBES.with(Cell::get)
}

fn canonical_payload<S: BlockSource>(
    source: &S,
    required: RequiredRef,
    resources: &mut TreeResources<'_>,
) -> Result<PayloadRef, TreeError> {
    let length = source.with_block(
        required.block,
        resources,
        |block, _resources| match required.block.kind {
            BlockKind::CanonicalImage => Ok(block.payload().len() as u64),
            BlockKind::ExtentList => {
                let payload = block.payload();
                if payload.get(..4) != Some(b"ZGEX".as_slice())
                    || payload.get(4..6) != Some(&1_u16.to_le_bytes())
                    || payload.get(6..8) != Some(&(BlockKind::CanonicalImage as u16).to_le_bytes())
                {
                    return Err(TreeError::Invalid("recovery canonical extent header"));
                }
                Ok(u64::from_le_bytes(
                    *payload
                        .get(8..16)
                        .and_then(|bytes| bytes.first_chunk::<8>())
                        .ok_or(TreeError::Invalid("recovery canonical extent length"))?,
                ))
            }
            _ => Err(TreeError::Invalid("recovery canonical role")),
        },
    )?;
    PayloadRef::new(BlockKind::CanonicalImage, length, required.block)
}

struct PreparedInventory<'a> {
    descriptors: &'a [u8],
    count: usize,
}

impl PreparedInventory<'_> {
    fn descriptor(&self, index: usize) -> Result<ArtifactDescriptor, TreeError> {
        let start = index.checked_mul(64).ok_or(TreeError::Memory)?;
        let row = self
            .descriptors
            .get(start..start + 64)
            .ok_or(TreeError::Invalid("prepared inventory descriptor extent"))?;
        let read_u16 = |offset| {
            row.get(offset..offset + 2)
                .and_then(|bytes| bytes.first_chunk::<2>())
                .copied()
                .map(u16::from_le_bytes)
                .ok_or(TreeError::Invalid("prepared inventory descriptor field"))
        };
        let read_u32 = |offset| {
            row.get(offset..offset + 4)
                .and_then(|bytes| bytes.first_chunk::<4>())
                .copied()
                .map(u32::from_le_bytes)
                .ok_or(TreeError::Invalid("prepared inventory descriptor field"))
        };
        let read_u64 = |offset| {
            row.get(offset..offset + 8)
                .and_then(|bytes| bytes.first_chunk::<8>())
                .copied()
                .map(u64::from_le_bytes)
                .ok_or(TreeError::Invalid("prepared inventory descriptor field"))
        };
        let read_u128 = |offset| {
            row.get(offset..offset + 16)
                .and_then(|bytes| bytes.first_chunk::<16>())
                .copied()
                .map(u128::from_le_bytes)
                .ok_or(TreeError::Invalid("prepared inventory descriptor field"))
        };
        Ok(ArtifactDescriptor {
            store: crate::property_graph::StoreInstanceId::new(read_u128(0)?)
                .map_err(|_| TreeError::Invalid("prepared inventory store"))?,
            artifact: ArtifactId::new(read_u128(16)?)
                .map_err(|_| TreeError::Invalid("prepared inventory artifact"))?,
            generation: crate::property_graph::GraphGeneration::new(read_u64(32)?),
            serial: read_u64(40)?,
            bytes: read_u32(48)?,
            family: read_u16(52)?,
            version: read_u16(54)?,
            checksum: read_u64(56)?,
        })
    }

    fn contains(
        &self,
        owner: ArtifactDescriptor,
        descriptor: ArtifactDescriptor,
        resources: &mut TreeResources<'_>,
    ) -> Result<bool, TreeError> {
        if owner == descriptor {
            return Ok(true);
        }
        for index in 0..self.count {
            resources.step(1)?;
            if self.descriptor(index)? == descriptor {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn prepared_inventory<'a>(
    source: &'a impl BlockSource,
    required: RequiredRef,
    state: CommitState<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<PreparedInventory<'a>, TreeError> {
    let block = source.resolve(required.block, resources)?;
    let identity = block.identity();
    if identity.store != required.object.store
        || identity.artifact != required.object.artifact
        || identity.generation != required.object.generation
        || identity.creation_serial != required.object.serial
        || block.reference() != required.block
        || block.file_length() != required.object.bytes as usize
        || block.file_checksum() != required.object.checksum
    {
        return Err(TreeError::Invalid("prepared inventory required descriptor"));
    }
    let payload = block.payload();
    if payload.get(..4) != Some(b"ZGCP".as_slice())
        || payload.get(4..6) != Some(&2_u16.to_le_bytes())
        || payload.get(6..8) != Some(&1_u16.to_le_bytes())
        || payload.get(12..16) != Some([0_u8; 4].as_slice())
    {
        return Err(TreeError::Invalid("prepared inventory role or reserved"));
    }
    let count = usize::try_from(u32::from_le_bytes(
        *payload
            .get(8..12)
            .and_then(|bytes| bytes.first_chunk::<4>())
            .ok_or(TreeError::Invalid("prepared inventory count"))?,
    ))
    .map_err(|_| TreeError::Memory)?;
    if count > MAX_RECOVERED_DESCRIPTORS
        || payload.len()
            != count
                .checked_mul(64)
                .and_then(|bytes| bytes.checked_add(16))
                .ok_or(TreeError::Memory)?
    {
        return Err(TreeError::Invalid("prepared inventory length"));
    }
    let inventory = PreparedInventory {
        descriptors: payload
            .get(16..)
            .ok_or(TreeError::Invalid("prepared inventory descriptors"))?,
        count,
    };
    for index in 0..count {
        resources.step(1)?;
        let descriptor = inventory.descriptor(index)?;
        if descriptor.store != state.store
            || descriptor.generation > state.generation
            || descriptor.serial == 0
            || descriptor.serial > state.high_waters.creation_serial
            || descriptor.bytes as usize > MAX_ARTIFACT_BYTES
            || descriptor.family != crate::format::FormatFamily::NativeGraphObject.id()
            || descriptor.version != 1
        {
            return Err(TreeError::Invalid("prepared inventory descriptor domain"));
        }
        for prior in 0..index {
            resources.step(1)?;
            let previous = inventory.descriptor(prior)?;
            if previous.artifact == descriptor.artifact || previous.serial == descriptor.serial {
                return Err(TreeError::Invalid(
                    "duplicate prepared inventory descriptor",
                ));
            }
        }
        if descriptor.artifact == required.object.artifact
            || descriptor.serial == required.object.serial
        {
            return Err(TreeError::Invalid(
                "prepared inventory owner descriptor collision",
            ));
        }
    }
    Ok(inventory)
}

fn next_inventory_row<S: BlockSource>(
    cursor: &mut DirectoryCursor<'_, '_, S>,
    source: &S,
    root: crate::property_graph::storage::tree::directory::DirectoryRoot,
    key: &mut [u8; 16],
    value: &mut [u8; 88],
    resources: &mut TreeResources<'_>,
) -> Result<Option<InventoryChange>, TreeError> {
    let Some((key_length, value_length)) = cursor.next(key, value, resources)? else {
        return Ok(None);
    };
    if key_length != key.len() || value_length != value.len() {
        return Err(TreeError::Invalid("recovery inventory row width"));
    }
    let entry = lookup_entry(source, root, key, resources)?
        .ok_or(TreeError::Invalid("recovery inventory row disappeared"))?;
    verify_inventory_entry(root, entry, resources).map(Some)
}

#[allow(clippy::too_many_arguments)]
fn validate_inventory_fold_transition(
    source: &RecoverySource<'_, '_>,
    base_root: crate::property_graph::storage::tree::directory::DirectoryRoot,
    target_root: crate::property_graph::storage::tree::directory::DirectoryRoot,
    selected: &[(RequiredRef, PreparedInventory<'_>)],
    retired: usize,
    base: CommitState<'_>,
    reclaim: &[InventoryChange],
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    let mut left = DirectoryCursor::seek(source, base_root, None, resources)?;
    let mut right = DirectoryCursor::seek(source, target_root, None, resources)?;
    let mut left_key = [0_u8; 16];
    let mut right_key = [0_u8; 16];
    let mut left_value = [0_u8; 88];
    let mut right_value = [0_u8; 88];
    let mut left_row = next_inventory_row(
        &mut left,
        source,
        base_root,
        &mut left_key,
        &mut left_value,
        resources,
    )?;
    let mut right_row = next_inventory_row(
        &mut right,
        source,
        target_root,
        &mut right_key,
        &mut right_value,
        resources,
    )?;
    let mut additions = 0_usize;
    let mut adoptions = 0_usize;
    while left_row.is_some() || right_row.is_some() {
        resources.step(1)?;
        match (left_row, right_row) {
            (Some(left_change), Some(right_change))
                if left_change.object.artifact == right_change.object.artifact =>
            {
                let reclaimed = reclaim.iter().any(|change| {
                    change.object == right_change.object
                        && change.state == right_change.state
                        && matches!(
                            (left_change.state, right_change.state),
                            (
                                InventoryState::Prepared | InventoryState::Retained,
                                InventoryState::ReclaimPending(_)
                            ) | (
                                InventoryState::ReclaimPending(_),
                                InventoryState::Reclaimed(_)
                            )
                        )
                });
                if left_change.object != right_change.object
                    || (left_change.state != right_change.state && !reclaimed)
                {
                    return Err(TreeError::Invalid(
                        "maintenance inventory changed existing row",
                    ));
                }
                left_row = next_inventory_row(
                    &mut left,
                    source,
                    base_root,
                    &mut left_key,
                    &mut left_value,
                    resources,
                )?;
                right_row = next_inventory_row(
                    &mut right,
                    source,
                    target_root,
                    &mut right_key,
                    &mut right_value,
                    resources,
                )?;
            }
            (Some(left_change), Some(right_change))
                if left_change.object.artifact < right_change.object.artifact =>
            {
                if !reclaim.iter().any(|change| {
                    change.object == left_change.object
                        && change.state == left_change.state
                        && matches!(left_change.state, InventoryState::Reclaimed(_))
                }) {
                    return Err(TreeError::Invalid("maintenance inventory omitted row"));
                }
                left_row = next_inventory_row(
                    &mut left,
                    source,
                    base_root,
                    &mut left_key,
                    &mut left_value,
                    resources,
                )?;
            }
            (_, Some(right_change)) => {
                let mut explained = false;
                for (owner, inventory) in selected {
                    if inventory.contains(owner.object, right_change.object, resources)? {
                        explained = true;
                        break;
                    }
                }
                if right_change.state != InventoryState::Retained {
                    return Err(TreeError::Invalid(
                        "maintenance inventory unexplained addition",
                    ));
                }
                if explained {
                    additions = additions.checked_add(1).ok_or(TreeError::Work)?;
                    if additions
                        > crate::property_graph::storage::inventory::INVENTORY_FOLD_ADDITION_LIMIT
                    {
                        return Err(TreeError::Invalid("maintenance inventory addition bound"));
                    }
                } else {
                    // An adopted complete orphan: a finalized object of this
                    // store that predates the base cutoffs. The row is
                    // bookkeeping only. It can never authorize an unlink; that
                    // takes a later completed mark and intent, validated on
                    // their own.
                    let object = right_change.object;
                    adoptions = adoptions.checked_add(1).ok_or(TreeError::Work)?;
                    if object.store != base.store
                        || object.generation > base.generation
                        || object.serial == 0
                        || object.serial > base.high_waters.creation_serial
                        || object.family != crate::format::FormatFamily::NativeGraphObject.id()
                        || object.version != 1
                        || adoptions
                            > crate::property_graph::storage::inventory::INVENTORY_ADOPTION_LIMIT
                    {
                        return Err(TreeError::Invalid(
                            "maintenance inventory unexplained addition",
                        ));
                    }
                }
                right_row = next_inventory_row(
                    &mut right,
                    source,
                    target_root,
                    &mut right_key,
                    &mut right_value,
                    resources,
                )?;
            }
            (Some(left_change), None) => {
                if !reclaim.iter().any(|change| {
                    change.object == left_change.object
                        && change.state == left_change.state
                        && matches!(left_change.state, InventoryState::Reclaimed(_))
                }) {
                    return Err(TreeError::Invalid("maintenance inventory omitted row"));
                }
                left_row = next_inventory_row(
                    &mut left,
                    source,
                    base_root,
                    &mut left_key,
                    &mut left_value,
                    resources,
                )?;
            }
            (None, None) => break,
        }
    }
    let retired_manifests = selected
        .get(..retired)
        .ok_or(TreeError::Invalid("retired inventory selection"))?;
    for (owner, inventory) in retired_manifests {
        for index in 0..inventory.count {
            let descriptor = inventory.descriptor(index)?;
            let entry = lookup_entry(
                source,
                target_root,
                &descriptor.artifact.get().to_le_bytes(),
                resources,
            )?
            .ok_or(TreeError::Invalid(
                "retired prepared inventory descriptor is absent",
            ))?;
            let actual = verify_inventory_entry(target_root, entry, resources)?;
            if actual.object != descriptor
                || !matches!(
                    actual.state,
                    InventoryState::Retained
                        | InventoryState::ReclaimPending(_)
                        | InventoryState::Reclaimed(_)
                )
            {
                return Err(TreeError::Invalid(
                    "retired prepared inventory descriptor changed",
                ));
            }
        }
        let entry = lookup_entry(
            source,
            target_root,
            &owner.object.artifact.get().to_le_bytes(),
            resources,
        )?
        .ok_or(TreeError::Invalid(
            "retired prepared inventory owner is absent",
        ))?;
        let actual = verify_inventory_entry(target_root, entry, resources)?;
        if actual.object != owner.object
            || !matches!(
                actual.state,
                InventoryState::Retained
                    | InventoryState::ReclaimPending(_)
                    | InventoryState::Reclaimed(_)
            )
        {
            return Err(TreeError::Invalid(
                "retired prepared inventory owner changed",
            ));
        }
    }
    Ok(())
}

/// The semantic validator only needs reads, mapping accounting and lexical
/// interpretation. Verification supplies these without opening a Store.
pub(super) trait RecoveryRead {
    fn vfs(&self) -> &dyn Vfs;
    fn publication(&self) -> &Arc<super::NativeGraphPublication>;
    fn lexical(&self) -> crate::fts::tokenizer::TokenizerEpoch;
    fn observe_mapping(&self, path: &Path);
}

impl RecoveryRead for Store {
    fn vfs(&self) -> &dyn Vfs {
        self.vfs.as_ref()
    }
    fn publication(&self) -> &Arc<super::NativeGraphPublication> {
        &self.native_graph
    }
    fn lexical(&self) -> crate::fts::tokenizer::TokenizerEpoch {
        self.tokenizer.epoch()
    }
    fn observe_mapping(&self, _: &Path) {}
}

fn map_file(
    store: &dyn RecoveryRead,
    path: &Path,
    maximum: usize,
) -> Result<NativeReadonlyMapping, NativeGraphError> {
    store.observe_mapping(path);
    #[cfg(test)]
    let timer = ReopenTimer::new("mapping", 0);
    let file = store
        .vfs()
        .open_for_map(path)
        .map_err(|source| NativeGraphError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    let result = NativeReadonlyMapping::open_recovery(file, path, store.publication(), maximum);
    #[cfg(test)]
    {
        let mut timer = timer;
        timer.2 = result
            .as_ref()
            .map_or(0, |mapping| mapping.as_bytes().len());
    }
    result
}

#[derive(Clone, Copy)]
pub(super) struct CapturedWalCutoff {
    pub(super) identity: u128,
    pub(super) first_sequence: u64,
    pub(super) bytes: usize,
}

#[allow(
    clippy::large_enum_variant,
    reason = "captured WAL state keeps failure handling allocation-free"
)]
pub(super) enum CapturedStateVisit<'a> {
    Checkpoint {
        wal_identity: u128,
        state: CommitState<'a>,
        complete_bytes: usize,
    },
    Envelope {
        kind: crate::property_graph::wal::EnvelopeKind,
        state: CommitState<'a>,
        changes: ChangeReader<'a>,
        complete_bytes: usize,
    },
}

/// Test oracle over the manifest checkpoint and the single outer WAL.
#[cfg(any(test, feature = "test-seams"))]
pub(super) fn visit_captured_state(
    store: &dyn RecoveryRead,
    directory: &Path,
    bundle: &super::NativeGraphBundle,
    target_sequence: u64,
    _exact_wal_cutoff: Option<CapturedWalCutoff>,
    control: &QueryControl,
    mut visit: impl FnMut(CapturedStateVisit<'_>, &mut WalResources<'_>) -> Result<(), NativeGraphError>,
) -> Result<(), NativeGraphError> {
    let reader = crate::wal::WalReader::open(store.vfs(), &directory.join("wal.ze"))
        .map_err(|error| NativeGraphError::Store(crate::lifecycle::StoreError::Wal(error)))?;
    use crate::manifest::io::DurableLog as _;
    let manifest = crate::manifest::io::load_manifest(
        store.vfs(),
        &directory.join("manifest.ze"),
        reader.durable_end(),
    )
    .map_err(|error| NativeGraphError::Store(crate::lifecycle::StoreError::Manifest(error)))?;
    let graph = manifest
        .graph
        .as_ref()
        .ok_or(NativeGraphError::Invalid("captured graph manifest"))?;
    let mut state = graph
        .state()
        .map_err(|error| NativeGraphError::Store(crate::lifecycle::StoreError::Manifest(error)))?;
    let mut cancelled = || control.checkpoint().is_err();
    let mut resources = WalResources::new(u64::MAX, STACK_RESERVATION_BYTES, &mut cancelled)?;
    if target_sequence < state.sequence {
        state = super::write::commit_state(bundle);
    }
    visit(
        CapturedStateVisit::Checkpoint {
            wal_identity: 0,
            state,
            complete_bytes: 0,
        },
        &mut resources,
    )?;
    if target_sequence == state.sequence {
        return Ok(());
    }
    for record in reader.records() {
        if record.seq.get() <= graph.graph_absorbed_through {
            continue;
        }
        let payload = record
            .payload()
            .map_err(|_| NativeGraphError::Invalid("captured outer WAL"))?;
        let decoded = crate::ingest::wal_payload::decode_mutation(record.op, payload)
            .map_err(|_| NativeGraphError::Invalid("captured outer WAL operation"))?;
        let bytes = match decoded {
            crate::ingest::wal_payload::MutationPayload::GraphCommit(_) => payload,
            crate::ingest::wal_payload::MutationPayload::MixedBatchMember { mutation, .. } => {
                match *mutation {
                    crate::ingest::wal_payload::MutationPayload::GraphCommit(_) => payload
                        .get(10..)
                        .ok_or(NativeGraphError::Invalid("captured mixed payload"))?,
                    _ => continue,
                }
            }
            _ => continue,
        };
        let mut replay = Replay::envelope(bytes, state);
        match replay.next_framed_capture(&mut resources)? {
            FramedCaptureStep::Envelope(envelope) => {
                let sequence = envelope.state.sequence;

                if sequence > target_sequence {
                    return Err(NativeGraphError::Wal(WalError::Sequence));
                }
                visit(
                    CapturedStateVisit::Envelope {
                        kind: envelope.kind,
                        state: envelope.state,
                        changes: envelope.changes(),
                        complete_bytes: envelope.complete_bytes,
                    },
                    &mut resources,
                )?;
                if sequence == target_sequence {
                    return Ok(());
                }
                state = envelope.state;
            }
            FramedCaptureStep::End(_) => return Err(NativeGraphError::Wal(WalError::Sequence)),
        }
    }
    Err(NativeGraphError::Wal(WalError::Sequence))
}

struct RecoveryMappedArtifact {
    artifact: ArtifactId,
    mapping: NativeReadonlyMapping,
    validation: ValidatedArtifact,
}

struct RecoveryArtifactWindow<'source, 'store, 'm> {
    source: &'source RecoverySource<'store, 'm>,
    artifact: ArtifactId,
    mapping: &'source NativeReadonlyMapping,
    validation: ValidatedArtifact,
}

struct RecoverySource<'a, 'm> {
    store: &'a dyn RecoveryRead,
    directory: &'a Path,
    expected_store: crate::property_graph::StoreInstanceId,
    generation: crate::property_graph::GraphGeneration,
    creation_serial: u64,
    memory: &'m StorageMemory<'m>,
    slots: StorageBuffer<'m, OnceCell<RecoveryMappedArtifact>>,
    scoped: Cell<bool>,
    /// At most one authenticated mapping retained for the current scoped
    /// traversal, keeping its open/authenticate cost at parity with the
    /// retained slot table without retaining one mapping per artifact.
    window: RefCell<Option<RecoveryMappedArtifact>>,
    retain_window: Cell<bool>,
    filled: Cell<usize>,
    #[cfg(any(test, feature = "test-seams"))]
    slot_observation: crate::property_graph::storage::mapping_slot_capture::Observation,
    source_error: RefCell<Option<NativeGraphError>>,
}

impl<'a, 'm> RecoverySource<'a, 'm> {
    fn new(
        store: &'a dyn RecoveryRead,
        directory: &'a Path,
        state: CommitState<'_>,
        memory: &'m StorageMemory<'m>,
        artifact_capacity: usize,
    ) -> Result<Self, TreeError> {
        Self::new_with_capacity(
            store,
            directory,
            state,
            memory,
            artifact_capacity.checked_mul(2).ok_or(TreeError::Memory)?,
            false,
        )
    }

    fn new_scoped(
        store: &'a dyn RecoveryRead,
        directory: &'a Path,
        state: CommitState<'_>,
        memory: &'m StorageMemory<'m>,
        capacity: usize,
    ) -> Result<Self, TreeError> {
        Self::new_with_capacity(store, directory, state, memory, capacity, true)
    }

    fn new_with_capacity(
        store: &'a dyn RecoveryRead,
        directory: &'a Path,
        state: CommitState<'_>,
        memory: &'m StorageMemory<'m>,
        capacity: usize,
        scoped: bool,
    ) -> Result<Self, TreeError> {
        if capacity == 0 {
            return Err(TreeError::Memory);
        }
        #[cfg(any(test, feature = "test-seams"))]
        let capacity = crate::property_graph::storage::mapping_slot_capture::capacity(capacity);
        let mut slots = StorageBuffer::new(memory, capacity)?;
        for _ in 0..capacity {
            slots.push(OnceCell::new())?;
        }
        Ok(Self {
            store,
            directory,
            expected_store: state.store,
            generation: state.generation,
            creation_serial: state.high_waters.creation_serial,
            memory,
            slots,
            scoped: Cell::new(scoped),
            window: RefCell::new(None),
            retain_window: Cell::new(scoped),
            filled: Cell::new(0),
            #[cfg(any(test, feature = "test-seams"))]
            slot_observation:
                crate::property_graph::storage::mapping_slot_capture::Observation::new(
                    crate::property_graph::storage::mapping_slot_capture::Kind::Recovery,
                    state.generation.get(),
                    capacity,
                ),
            source_error: RefCell::new(None),
        })
    }

    fn resources(&self, work: u64) -> Result<TreeResources<'m>, TreeError> {
        TreeResources::for_prepare(self.memory, work)
    }

    fn charged_path(
        &self,
        artifact: ArtifactId,
    ) -> Result<(PathBuf, StorageReservation<'m>), TreeError> {
        const NAME_BYTES: usize = b"graph-00000000000000000000000000000000.zgraph".len();
        let path_upper = self
            .directory
            .as_os_str()
            .len()
            .checked_add(1 + NAME_BYTES)
            .ok_or(TreeError::Memory)?;
        let mut charge = self.memory.reserve(
            path_upper
                .checked_add(NAME_BYTES)
                .ok_or(TreeError::Memory)?,
        )?;
        let mut name = String::new();
        name.try_reserve_exact(NAME_BYTES)
            .map_err(|_| TreeError::Memory)?;
        write!(&mut name, "graph-{:032x}.zgraph", artifact.get()).map_err(|_| TreeError::Memory)?;
        let mut raw = OsString::with_capacity(path_upper);
        raw.push(self.directory.as_os_str());
        let mut path = PathBuf::from(raw);
        path.push(&name);
        let raw = path.into_os_string();
        let path_capacity = raw.capacity();
        charge.resize(
            name.capacity()
                .checked_add(path_capacity)
                .ok_or(TreeError::Memory)?,
        )?;
        drop(name);
        charge.resize(path_capacity)?;
        Ok((PathBuf::from(raw), charge))
    }

    fn latch_source(&self, error: NativeGraphError) -> TreeError {
        let mut slot = self.source_error.borrow_mut();
        if slot.is_none() {
            *slot = Some(error);
        }
        TreeError::Invalid("native recovery artifact source")
    }

    fn take_source_error(&self) -> Option<NativeGraphError> {
        self.source_error.borrow_mut().take()
    }

    fn decode<'s>(
        &self,
        mapped: &'s RecoveryMappedArtifact,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'s>, TreeError> {
        resources.require_preparation(self.memory)?;
        resources.step(1)?;
        mapped
            .validation
            .framed_block(mapped.mapping.as_bytes(), reference)
            .map_err(TreeError::Format)
    }

    fn check_owner(&self, resources: &mut TreeResources<'_>) -> Result<(), TreeError> {
        resources.require_preparation(self.memory)?;
        resources.step(0)
    }

    fn admit_mapping(
        &self,
        mapping: &NativeReadonlyMapping,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<ValidatedArtifact, TreeError> {
        let frame = reopen_controlled_validation!(
            ContainerKind::Object,
            Some((self.expected_store, reference.artifact)),
            mapping.as_bytes(),
            &mut |bytes| resources.step(bytes as u64),
        )
        .map_err(|error| match error {
            ArtifactControlError::Format(error) => TreeError::Format(error),
            ArtifactControlError::Control(error) => error,
        })?;
        let identity = frame.identity();
        if identity.generation > self.generation
            || identity.creation_serial > self.creation_serial
            || frame
                .framed_block(reference)
                .map_err(TreeError::Format)?
                .reference()
                != reference
        {
            return Err(TreeError::Invalid(
                "scoped recovery artifact exceeds captured cutoff",
            ));
        }
        Ok(frame.validation())
    }

    fn with_artifact_window<R>(
        &self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
        callback: impl for<'window, 'r> FnOnce(
            &'window RecoveryArtifactWindow<'_, 'a, 'm>,
            &'r mut TreeResources<'_>,
        ) -> Result<R, TreeError>,
    ) -> Result<R, TreeError> {
        self.check_owner(resources)?;
        let slot = crate::property_graph::storage::mapping_slot(
            self.slots.as_slice(),
            reference.artifact,
            |mapped| mapped.artifact,
            || resources.step(1),
        )?;
        if !self.scoped_blocks()
            && let Some(cell) = slot
            && cell.get().is_none()
        {
            let _ = self.slot_block(reference, resources)?;
        }
        if let Some(mapped) = slot.and_then(OnceCell::get) {
            let window = RecoveryArtifactWindow {
                source: self,
                artifact: reference.artifact,
                mapping: &mapped.mapping,
                validation: mapped.validation,
            };
            let _ = window.resolve(reference, resources)?;
            return callback(&window, resources);
        }
        if let Ok(retained) = self.window.try_borrow()
            && let Some(mapped) = retained.as_ref()
            && mapped.artifact == reference.artifact
        {
            let window = RecoveryArtifactWindow {
                source: self,
                artifact: reference.artifact,
                mapping: &mapped.mapping,
                validation: mapped.validation,
            };
            let _ = window.resolve(reference, resources)?;
            return callback(&window, resources);
        }
        let (path, path_charge) = self.charged_path(reference.artifact)?;
        #[cfg(any(test, feature = "test-seams"))]
        self.slot_observation.open(true);
        let mapping = map_file(self.store, &path, MAX_ARTIFACT_BYTES)
            .map_err(|error| self.latch_source(error))?;
        let validation = self.admit_mapping(&mapping, reference, resources)?;
        let window = RecoveryArtifactWindow {
            source: self,
            artifact: reference.artifact,
            mapping: &mapping,
            validation,
        };
        let result = callback(&window, resources);
        drop(mapping);
        drop(path);
        drop(path_charge);
        result
    }

    fn validate_descriptor(
        &self,
        descriptor: ArtifactDescriptor,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        self.validate_descriptor_with_missing(descriptor, false, resources)
    }

    fn validate_descriptor_with_missing(
        &self,
        descriptor: ArtifactDescriptor,
        allow_missing: bool,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        if descriptor.store != self.expected_store
            || descriptor.generation > self.generation
            || descriptor.serial == 0
            || descriptor.serial > self.creation_serial
            || descriptor.bytes as usize > MAX_ARTIFACT_BYTES
            || descriptor.family != crate::format::FormatFamily::NativeGraphObject.id()
            || descriptor.version != 1
        {
            return Err(TreeError::Invalid("recovery artifact descriptor domain"));
        }
        let (path, path_charge) = self.charged_path(descriptor.artifact)?;
        #[cfg(any(test, feature = "test-seams"))]
        self.slot_observation.open(true);
        let mapping = match map_file(self.store, &path, MAX_ARTIFACT_BYTES) {
            Ok(mapping) => mapping,
            Err(NativeGraphError::Io { source, .. })
                if allow_missing && source.kind() == std::io::ErrorKind::NotFound =>
            {
                drop(path);
                drop(path_charge);
                return Ok(());
            }
            Err(error) => return Err(self.latch_source(error)),
        };
        drop(path);
        drop(path_charge);
        let frame = reopen_controlled_validation!(
            ContainerKind::Object,
            Some((descriptor.store, descriptor.artifact)),
            mapping.as_bytes(),
            &mut |bytes| resources.step(bytes as u64),
        )
        .map_err(|error| match error {
            ArtifactControlError::Format(error) => TreeError::Format(error),
            ArtifactControlError::Control(error) => error,
        })?;
        let identity = frame.identity();
        let checksum = u64::from_le_bytes(
            *mapping
                .as_bytes()
                .last_chunk::<8>()
                .ok_or(TreeError::Invalid("recovery artifact checksum trailer"))?,
        );
        if identity.generation != descriptor.generation
            || identity.creation_serial != descriptor.serial
            || mapping.as_bytes().len() != descriptor.bytes as usize
            || checksum != descriptor.checksum
        {
            return Err(TreeError::Invalid("recovery artifact descriptor mismatch"));
        }
        Ok(())
    }

    fn copy_required_payload(
        &self,
        reference: RequiredRef,
        output: &mut [u8],
        resources: &mut TreeResources<'_>,
    ) -> Result<usize, TreeError> {
        if reference.object.store != self.expected_store
            || reference.object.generation > self.generation
            || reference.object.serial == 0
            || reference.object.serial > self.creation_serial
            || reference.object.bytes as usize > MAX_ARTIFACT_BYTES
            || reference.object.family != crate::format::FormatFamily::NativeGraphObject.id()
            || reference.object.version != 1
            || reference.block.artifact != reference.object.artifact
        {
            return Err(TreeError::Invalid("recovery required descriptor domain"));
        }
        let (path, path_charge) = self.charged_path(reference.object.artifact)?;
        #[cfg(any(test, feature = "test-seams"))]
        self.slot_observation.open(true);
        let mapping = map_file(self.store, &path, MAX_ARTIFACT_BYTES)
            .map_err(|error| self.latch_source(error))?;
        drop(path);
        drop(path_charge);
        let frame = reopen_controlled_validation!(
            ContainerKind::Object,
            Some((reference.object.store, reference.object.artifact)),
            mapping.as_bytes(),
            &mut |bytes| resources.step(bytes as u64),
        )
        .map_err(|error| match error {
            ArtifactControlError::Format(error) => TreeError::Format(error),
            ArtifactControlError::Control(error) => error,
        })?;
        let identity = frame.identity();
        let checksum = u64::from_le_bytes(
            *mapping
                .as_bytes()
                .last_chunk::<8>()
                .ok_or(TreeError::Invalid("recovery required checksum trailer"))?,
        );
        if identity.generation != reference.object.generation
            || identity.creation_serial != reference.object.serial
            || mapping.as_bytes().len() != reference.object.bytes as usize
            || checksum != reference.object.checksum
        {
            return Err(TreeError::Invalid("recovery required descriptor mismatch"));
        }
        let block = frame
            .framed_block(reference.block)
            .map_err(TreeError::Format)?;
        if block.reference() != reference.block {
            return Err(TreeError::Invalid("recovery required block mismatch"));
        }
        let payload = block.payload();
        let destination = output.get_mut(..payload.len()).ok_or(TreeError::Memory)?;
        destination.copy_from_slice(payload);
        Ok(payload.len())
    }

    fn validate_required_reference_scoped(
        &self,
        reference: RequiredRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        self.validate_protected_reference(reference, false, resources)
    }

    fn validate_checkpoint_control_reference(
        &self,
        reference: RequiredRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        if reference.object.family != crate::format::FormatFamily::NativeGraphRoot.id()
            || reference.object.version != 1
            || reference.block.kind != BlockKind::CheckpointPayload
            || reference.block.version != 1
        {
            return Err(TreeError::Invalid("recovery checkpoint control reference"));
        }
        self.validate_protected_reference(reference, true, resources)
    }

    fn validate_protected_reference(
        &self,
        reference: RequiredRef,
        allow_checkpoint_serial: bool,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        self.validate_protected_reference_cached(
            reference,
            allow_checkpoint_serial,
            resources,
            None,
        )
    }

    fn validate_protected_reference_cached(
        &self,
        reference: RequiredRef,
        allow_checkpoint_serial: bool,
        resources: &mut TreeResources<'_>,
        mut cache: Option<&mut StorageBuffer<'_, (ArtifactDescriptor, RecoveryMappedArtifact)>>,
    ) -> Result<(), TreeError> {
        self.check_owner(resources)?;
        if reference.object.store != self.expected_store
            || reference.object.generation > self.generation
            || reference.object.serial == 0
            || (!allow_checkpoint_serial && reference.object.serial > self.creation_serial)
            || reference.object.bytes as usize > MAX_ARTIFACT_BYTES
            || reference.block.artifact != reference.object.artifact
        {
            return Err(TreeError::Invalid("recovery protected reference domain"));
        }
        let container = if reference.object.family
            == crate::format::FormatFamily::NativeGraphObject.id()
            && reference.object.version == 1
        {
            ContainerKind::Object
        } else if reference.object.family == crate::format::FormatFamily::NativeGraphRoot.id()
            && reference.object.version == 1
        {
            ContainerKind::RootEnvelope
        } else {
            return Err(TreeError::Invalid("recovery protected reference family"));
        };
        if let Some(entries) = cache.as_ref() {
            for (descriptor, mapped) in entries.as_slice() {
                resources.step(1)?;
                if descriptor.artifact == reference.object.artifact {
                    if *descriptor != reference.object {
                        return Err(TreeError::Invalid("recovery protected reference mismatch"));
                    }
                    let block = mapped
                        .validation
                        .framed_block(mapped.mapping.as_bytes(), reference.block)
                        .map_err(TreeError::Format)?;
                    if block.reference() != reference.block {
                        return Err(TreeError::Invalid("recovery protected reference mismatch"));
                    }
                    return Ok(());
                }
            }
        }
        let (path, path_charge) = self.charged_path(reference.object.artifact)?;
        #[cfg(any(test, feature = "test-seams"))]
        self.slot_observation.open(true);
        let mapping = map_file(self.store, &path, MAX_ARTIFACT_BYTES)
            .map_err(|error| self.latch_source(error))?;
        drop(path);
        drop(path_charge);
        let frame = reopen_controlled_validation!(
            container,
            Some((reference.object.store, reference.object.artifact)),
            mapping.as_bytes(),
            &mut |bytes| resources.step(bytes as u64),
        )
        .map_err(|error| match error {
            ArtifactControlError::Format(error) => TreeError::Format(error),
            ArtifactControlError::Control(error) => error,
        })?;
        let identity = frame.identity();
        let checksum = u64::from_le_bytes(
            *mapping
                .as_bytes()
                .last_chunk::<8>()
                .ok_or(TreeError::Invalid("recovery protected checksum trailer"))?,
        );
        if identity.generation != reference.object.generation
            || identity.creation_serial != reference.object.serial
            || mapping.as_bytes().len() != reference.object.bytes as usize
            || checksum != reference.object.checksum
            || frame
                .framed_block(reference.block)
                .map_err(TreeError::Format)?
                .reference()
                != reference.block
        {
            return Err(TreeError::Invalid("recovery protected reference mismatch"));
        }
        let validation = frame.validation();
        if let Some(entries) = cache.as_mut() {
            entries.push((
                reference.object,
                RecoveryMappedArtifact {
                    artifact: reference.object.artifact,
                    mapping,
                    validation,
                },
            ))?;
        }
        Ok(())
    }
}

struct RecoverySpillIo<'a, 'm> {
    source: &'a RecoverySource<'a, 'm>,
}

impl crate::property_graph::storage::reclaim::SpillIo for RecoverySpillIo<'_, '_> {
    fn append_page(
        &mut self,
        _: &[u8],
        _: &mut TreeResources<'_>,
    ) -> Result<RequiredRef, TreeError> {
        Err(TreeError::Invalid("read-only recovery spill append"))
    }

    fn read_page(
        &self,
        reference: RequiredRef,
        output: &mut [u8],
        resources: &mut TreeResources<'_>,
    ) -> Result<usize, TreeError> {
        self.source
            .copy_required_payload(reference, output, resources)
    }
}

/// Membership of one traced reference in the completed mark.
///
/// The descent reads one page per run level and does not re-authenticate the
/// run's count or digest; exactly one full authenticating walk per manifest
/// runs in `validate_reclaim_manifest_authority` before any caller reaches
/// here. The reader is caller-owned so one manifest's whole retrace shares a
/// single charged page buffer.
fn validate_mark_reference(
    mark: &mut crate::property_graph::storage::reclaim::DurableRunReader<'_>,
    io: &impl crate::property_graph::storage::reclaim::SpillIo,
    candidates: &[ArtifactDescriptor],
    expected_artifact: ArtifactId,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    if !mark.contains(expected_artifact, io, resources)? {
        return Err(TreeError::Invalid(
            "captured live reference absent from completed mark",
        ));
    }
    if candidates
        .iter()
        .any(|candidate| candidate.artifact == expected_artifact)
    {
        return Err(TreeError::Invalid("reclaim candidate is captured live"));
    }
    Ok(())
}

/// How much of one captured state a proof has to walk.
///
/// Recovery maps exactly the roots a commit state names before it replays
/// that envelope, so an intermediate state's own roots stay load-bearing.
/// Everything those roots reach transitively is already covered by the
/// checkpoint's complete trace, by the target's own bundle trace, and by the
/// envelope change references, so an intermediate state never needs the
/// traversal that dominates the cost.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CapturedTraceDepth {
    Roots,
    Complete,
}

#[allow(
    clippy::too_many_arguments,
    reason = "one exact captured state and completed mark authority"
)]
pub(super) fn trace_captured_state_references<'m, V>(
    store: &dyn RecoveryRead,
    directory: &Path,
    checkpoint: RequiredRef,
    state: CommitState<'_>,
    expected: GraphInterpretation<'_>,
    document: Option<&EmbeddingTower>,
    memory: &'m StorageMemory<'m>,
    depth: CapturedTraceDepth,
    resources: &mut TreeResources<'m>,
    visitor: &mut V,
) -> Result<(), NativeGraphError>
where
    V: TraceReferenceVisitor,
{
    #[cfg(any(test, feature = "test-seams"))]
    if depth == CapturedTraceDepth::Complete {
        STATE_TRACE_COUNT.with(|count| count.set(count.get().saturating_add(1)));
    }
    resources.require_preparation(memory)?;
    let trace_source = RecoverySource::new_with_capacity(
        store,
        directory,
        state,
        memory,
        crate::property_graph::storage::MAX_NATIVE_ARTIFACTS,
        false,
    )?;
    let catalog_source = (depth == CapturedTraceDepth::Complete)
        .then(|| RecoverySource::new_with_capacity(store, directory, state, memory, 1, false))
        .transpose()?;
    let result = (|| -> Result<(), TreeError> {
        visitor.visit(checkpoint.block, resources)?;
        for required in state.graph.slots.into_iter().flatten() {
            trace_source.validate_required_reference_scoped(required, resources)?;
            visitor.visit(required.block, resources)?;
        }
        trace_source.validate_required_reference_scoped(state.catalog, resources)?;
        visitor.visit(state.catalog.block, resources)?;
        for required in [state.text, state.vector, state.reclaim]
            .into_iter()
            .flatten()
        {
            trace_source.validate_required_reference_scoped(required, resources)?;
            visitor.visit(required.block, resources)?;
        }
        let mut no_cancel = || false;
        let mut list_resources = WalResources::new(
            u64::try_from(MAX_RECOVERED_DESCRIPTORS)
                .ok()
                .and_then(|count| count.checked_mul(256))
                .ok_or(TreeError::Work)?,
            STACK_RESERVATION_BYTES,
            &mut no_cancel,
        )
        .map_err(TreeError::WalMetadata)?;
        for index in 0..state
            .prepared_inventories
            .len()
            .map_err(TreeError::WalMetadata)?
        {
            let required = state
                .prepared_inventories
                .get(index, &mut list_resources)
                .map_err(TreeError::WalMetadata)?;
            trace_source.validate_required_reference_scoped(required, resources)?;
            visitor.visit(required.block, resources)?;
        }
        let Some(catalog_source) = catalog_source.as_ref() else {
            return Ok(());
        };
        let catalog = RecoveryCatalog::open(
            catalog_source,
            state.catalog,
            expected,
            state.high_waters,
            resources,
        )?;
        let roots = GraphRoots::from_references(
            state.store,
            state.generation,
            state
                .graph
                .slots
                .map(|required| required.map(|value| value.block)),
        )?;
        let mut range_scratch = RangeScratch::for_prepare(memory, resources)?;
        crate::property_graph::storage::reclaim::trace_graph_state(
            &trace_source,
            &catalog,
            roots,
            state.sequence,
            document,
            &mut range_scratch,
            visitor,
            resources,
        )?;
        if state.text.is_some() || state.vector.is_some() {
            let mut search = {
                // Sparse admission touches at most the text root, vector root and
                // historical interpretation catalog. Drop those borrowed mappings
                // before the value-owned trace begins its scoped read windows.
                let search_open_source =
                    RecoverySource::new_with_capacity(store, directory, state, memory, 3, false)?;
                crate::property_graph::storage::search::SearchTraceState::for_captured(
                    &search_open_source,
                    &catalog,
                    checkpoint,
                    state,
                    document,
                    store.lexical(),
                    memory,
                    resources,
                )?
            };
            let mut verify_row =
                |row_store: crate::property_graph::StoreInstanceId,
                 row_generation: crate::property_graph::GraphGeneration,
                 row_record: PayloadRef,
                 row_node: crate::property_graph::NodeId,
                 resources: &mut TreeResources<'m>| {
                    trace_source.with_artifact_window(
                        row_record.reference(),
                        resources,
                        |window, resources| {
                            crate::property_graph::storage::search::verify_sparse_trace_record(
                                PayloadSlice::new(window, row_store, row_generation, row_record),
                                row_node,
                                &catalog,
                                document,
                                resources,
                            )
                        },
                    )
                };
            let mut validate_vectors =
                |row_store: crate::property_graph::StoreInstanceId,
                 row_generation: crate::property_graph::GraphGeneration,
                 row_table: PayloadRef,
                 rows: u32,
                 index: &crate::property_graph::storage::search::NativeVectorIndex<'m>,
                 resources: &mut TreeResources<'m>| {
                    let mut validate_record =
                        |record: PayloadRef,
                         node: crate::property_graph::NodeId,
                         revision: u64,
                         ordinal: u32,
                         resources: &mut TreeResources<'m>| {
                            trace_source.with_artifact_window(
                            record.reference(),
                            resources,
                            |window, resources| {
                                crate::property_graph::storage::search::validate_vector_index_row(
                                    PayloadSlice::new(window, row_store, row_generation, record),
                                    node,
                                    revision,
                                    ordinal,
                                    &catalog,
                                    document,
                                    index,
                                    resources,
                                )
                            },
                        )
                        };
                    crate::property_graph::storage::search::validate_vector_index_rows_with(
                        &trace_source,
                        row_store,
                        row_generation,
                        row_table,
                        rows,
                        index,
                        &mut validate_record,
                        resources,
                    )
                };
            let mut output = [None; crate::property_graph::storage::reclaim::TRACE_OUTPUT_LIMIT];
            loop {
                let result = search.trace_captured(
                    &trace_source,
                    &catalog,
                    checkpoint,
                    state,
                    document,
                    store.lexical(),
                    memory,
                    &mut output,
                    &mut verify_row,
                    &mut validate_vectors,
                    resources,
                )?;
                for reference in output.iter().take(result.count).flatten().copied() {
                    visitor.visit(reference, resources)?;
                }
                if result.complete {
                    break;
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        if let Some(source) = trace_source.take_source_error().or_else(|| {
            catalog_source
                .as_ref()
                .and_then(RecoverySource::take_source_error)
        }) {
            return Err(source);
        }
        return Err(error.into());
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "one exact captured state and completed mark authority"
)]
fn validate_captured_state_reachability<'m>(
    store: &dyn RecoveryRead,
    directory: &Path,
    checkpoint: RequiredRef,
    state: CommitState<'_>,
    expected: GraphInterpretation<'_>,
    document: Option<&EmbeddingTower>,
    memory: &'m StorageMemory<'m>,
    depth: CapturedTraceDepth,
    mark: &mut crate::property_graph::storage::reclaim::DurableRunReader<'_>,
    io: &impl crate::property_graph::storage::reclaim::SpillIo,
    candidates: &[ArtifactDescriptor],
) -> Result<(), NativeGraphError> {
    #[cfg(test)]
    let _timer = ReopenTimer::new("validate_captured_state_reachability", 0);
    let mut resources = TreeResources::for_prepare(
        memory,
        crate::property_graph::storage::reclaim::MARK_WORK_LIMIT,
    )?;
    let mut visitor = |reference: PhysicalRef, resources: &mut TreeResources<'_>| {
        validate_mark_reference(mark, io, candidates, reference.artifact, resources)
    };
    trace_captured_state_references(
        store,
        directory,
        checkpoint,
        state,
        expected,
        document,
        memory,
        depth,
        &mut resources,
        &mut visitor,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "one complete framed change list and completed mark authority"
)]
pub(super) fn trace_captured_change_references<'m, V>(
    store: &dyn RecoveryRead,
    directory: &Path,
    state: CommitState<'_>,
    mut changes: ChangeReader<'_>,
    wal_resources: &mut WalResources<'_>,
    memory: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'m>,
    visitor: &mut V,
) -> Result<(), NativeGraphError>
where
    V: TraceReferenceVisitor,
{
    resources.require_preparation(memory)?;
    let source = RecoverySource::new_scoped(store, directory, state, memory, 1)?;
    #[cfg(all(test, feature = "graph-cypher"))]
    crate::property_graph::storage::preparation_work_capture::phase(
        "captured-changes-start",
        resources.work(),
    );
    // A mutation list commonly names many blocks in one immutable object.
    // Authenticate each artifact once, retaining the exact mapping and proof
    // only for this trace. Every reference still passes domain/descriptor and
    // framed-block validation, and all cache storage/work remains charged.
    let capacity = (changes.remaining_count() as usize)
        .checked_mul(2)
        .ok_or(TreeError::Memory)?;
    let mut authenticated = StorageBuffer::new(memory, capacity)?;
    let result = (|| -> Result<(), TreeError> {
        while let Some(change) = changes
            .next_change(wal_resources)
            .map_err(TreeError::WalMetadata)?
        {
            let required = match change {
                Change::Mutation(mutation) => mutation.canonical,
                Change::Inventory(_) => None,
                Change::ReclaimIntent(intent) => {
                    for required in [intent.protected_roots, intent.completed_mark] {
                        source.validate_protected_reference_cached(
                            required,
                            false,
                            resources,
                            Some(&mut authenticated),
                        )?;
                        visitor.visit(required.block, resources)?;
                    }
                    None
                }
                Change::ReclaimComplete(completion) => {
                    source.validate_protected_reference_cached(
                        completion.intent,
                        false,
                        resources,
                        Some(&mut authenticated),
                    )?;
                    visitor.visit(completion.intent.block, resources)?;
                    None
                }
            };
            if let Some(required) = required {
                source.validate_protected_reference_cached(
                    required,
                    false,
                    resources,
                    Some(&mut authenticated),
                )?;
                let payload = canonical_payload(&source, required, resources)?;
                crate::property_graph::storage::reclaim::trace_payload_references(
                    payload,
                    &source,
                    state.store,
                    state.generation,
                    visitor,
                    resources,
                )?;
            }
        }
        Ok(())
    })();
    #[cfg(all(test, feature = "graph-cypher"))]
    crate::property_graph::storage::preparation_work_capture::phase(
        "captured-changes-end",
        resources.work(),
    );
    if let Err(error) = result {
        if let Some(source) = source.take_source_error() {
            return Err(source);
        }
        return Err(error.into());
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "one complete framed change list and completed mark authority"
)]
fn validate_captured_change_references<'m>(
    store: &dyn RecoveryRead,
    directory: &Path,
    state: CommitState<'_>,
    changes: ChangeReader<'_>,
    wal_resources: &mut WalResources<'_>,
    memory: &'m StorageMemory<'m>,
    mark: &mut crate::property_graph::storage::reclaim::DurableRunReader<'_>,
    io: &impl crate::property_graph::storage::reclaim::SpillIo,
    candidates: &[ArtifactDescriptor],
) -> Result<(), NativeGraphError> {
    let mut resources = TreeResources::for_prepare(
        memory,
        crate::property_graph::storage::reclaim::MARK_WORK_LIMIT,
    )?;
    let mut visitor = |reference: PhysicalRef, resources: &mut TreeResources<'_>| {
        validate_mark_reference(mark, io, candidates, reference.artifact, resources)
    };
    trace_captured_change_references(
        store,
        directory,
        state,
        changes,
        wal_resources,
        memory,
        &mut resources,
        &mut visitor,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "one independently reconstructed reclaim authority"
)]
fn validate_complete_reclaim_authority(
    store: &dyn RecoveryRead,
    directory: &Path,
    current_state: CommitState<'_>,
    authentic_base: Option<CommitState<'_>>,
    expected: GraphInterpretation<'_>,
    document: Option<&EmbeddingTower>,
    control: &QueryControl,
    manifest: crate::property_graph::storage::reclaim::PendingIntentManifest,
    source: &RecoverySource<'_, '_>,
    memory: &StorageMemory<'_>,
    mark_reader: &mut crate::property_graph::storage::reclaim::DurableRunReader<'_>,
    resources: &mut TreeResources<'_>,
    candidates: &[ArtifactDescriptor],
) -> Result<(), TreeError> {
    #[cfg(test)]
    let _timer = ReopenTimer::new("validate_complete_reclaim_authority", 0);
    let io = RecoverySpillIo { source };
    let mut fold = None;
    let mut current = None;
    let mut prepared = None;
    // The authenticated mark is sorted unique and must contain every protected
    // artifact, so its count bounds this cache's distinct entries. Reader
    // closure records may name the same pack thousands of times; each block
    // and exact descriptor still validates. StorageBuffer checks byte overflow
    // and the shared 32 MiB allowance, returning TreeError::Memory if it cannot fit.
    let capacity = usize::try_from(manifest.mark.count).map_err(|_| TreeError::Memory)?;
    let mut authenticated = StorageBuffer::new(memory, capacity)?;
    crate::property_graph::storage::reclaim::validate_protected_stream(
        manifest.protected,
        &io,
        memory,
        resources,
        |record, resources| {
            match record.value {
                crate::property_graph::storage::reclaim::ProtectedValue::Required(required) => {
                    if required.object.family != 17 {
                        return Err(TreeError::Invalid("legacy checkpoint reference"));
                    }
                    source.validate_protected_reference_cached(
                        required,
                        false,
                        resources,
                        Some(&mut authenticated),
                    )?;
                }
                crate::property_graph::storage::reclaim::ProtectedValue::Descriptor(descriptor) => {
                    if descriptor.family != 17 {
                        return Err(TreeError::Invalid("legacy protected descriptor family"));
                    }
                    source.validate_descriptor(descriptor, resources)?;
                }
                crate::property_graph::storage::reclaim::ProtectedValue::FoldAuthority {
                    ..
                } => {
                    if fold.replace(record).is_some() {
                        return Err(TreeError::Invalid("duplicate protected fold authority"));
                    }
                }
                crate::property_graph::storage::reclaim::ProtectedValue::CapturedBase {
                    checkpoint,
                    sequence,
                } => {
                    source.validate_protected_reference_cached(
                        checkpoint,
                        false,
                        resources,
                        Some(&mut authenticated),
                    )?;
                    let slot = match record.class {
                        crate::property_graph::storage::reclaim::ProtectedClass::Current => {
                            &mut current
                        }
                        crate::property_graph::storage::reclaim::ProtectedClass::PreparedBase => {
                            &mut prepared
                        }
                        _ => return Err(TreeError::Invalid("captured base protected class")),
                    };
                    if slot.replace((checkpoint, sequence)).is_some() {
                        return Err(TreeError::Invalid("duplicate captured base"));
                    }
                }
            }
            if let Some(artifact) = record.artifact() {
                validate_mark_reference(mark_reader, &io, candidates, artifact, resources)?;
            }
            Ok(())
        },
    )?;
    let fold = fold.ok_or(TreeError::Invalid("protected fold authority is absent"))?;
    let current = current.ok_or(TreeError::Invalid("protected current base is absent"))?;
    if prepared != Some(current) {
        return Err(TreeError::Invalid(
            "protected prepared base differs from current",
        ));
    }
    if authentic_base.is_some()
        && (current_state.generation != manifest.binding.target_generation
            || current_state.sequence != manifest.binding.sequence.saturating_add(1))
    {
        return Err(TreeError::Invalid(
            "reclaim intent immediate base transition",
        ));
    }
    visit_captured_base(
        store,
        directory,
        current.0,
        current.1,
        manifest.binding,
        fold,
        control,
        |state, wal_resources| {
            validate_captured_state_reachability(
                store,
                directory,
                current.0,
                state,
                expected,
                document,
                memory,
                CapturedTraceDepth::Complete,
                mark_reader,
                &io,
                candidates,
            )?;
            if let Some(base) = authentic_base
                && !crate::property_graph::wal::same_commit_state(state, base, wal_resources)?
            {
                return Err(NativeGraphError::Invalid("captured current base changed"));
            }
            Ok(())
        },
    )
    .map_err(|error| source.latch_source(error))?;
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "one exact self-contained proof binding"
)]
fn visit_captured_base(
    store: &dyn RecoveryRead,
    directory: &Path,
    reference: RequiredRef,
    sequence: u64,
    binding: crate::property_graph::storage::reclaim::SpillBinding,
    authority: crate::property_graph::storage::reclaim::ProtectedRecord,
    control: &QueryControl,
    visit: impl FnOnce(CommitState<'_>, &mut WalResources<'_>) -> Result<(), NativeGraphError>,
) -> Result<(), NativeGraphError> {
    if reference.object.family != 17
        || reference.object.version != 1
        || reference.block.artifact != reference.object.artifact
        || reference.block.kind != BlockKind::CommitParticipant
        || reference.block.version != 1
        || reference.object.store != binding.store
        || reference.object.generation != binding.target_generation
    {
        return Err(NativeGraphError::Invalid("captured base reference binding"));
    }
    let path = crate::property_graph::storage::allocation::artifact_path(
        directory,
        reference.object.artifact,
    );
    let mapping = map_file(store, &path, MAX_ARTIFACT_BYTES)?;
    let descriptor = artifact_descriptor(
        ArtifactIdentity {
            store: reference.object.store,
            artifact: reference.object.artifact,
            generation: reference.object.generation,
            creation_serial: reference.object.serial,
        },
        ContainerKind::Object,
        mapping.as_bytes(),
    )?;
    if descriptor != reference.object {
        return Err(NativeGraphError::Invalid(
            "captured base descriptor mismatch",
        ));
    }
    let frame = reopen_validation!(
        ContainerKind::Object,
        Some((binding.store, reference.object.artifact)),
        mapping.as_bytes()
    )
    .map_err(|_| NativeGraphError::Invalid("corrupt captured base object"))?;
    let payload = frame
        .framed_block(reference.block)
        .map_err(|_| NativeGraphError::Invalid("captured base block mismatch"))?
        .payload();
    let mut cancelled = || control.checkpoint().is_err();
    let mut resources = WalResources::new(u64::MAX, STACK_RESERVATION_BYTES, &mut cancelled)?;
    let captured =
        crate::property_graph::storage::reclaim::decode_captured_base(payload, &mut resources)?;
    let crate::property_graph::storage::reclaim::ProtectedValue::FoldAuthority {
        manifest_generation,
        graph_absorbed_through,
        envelope_sequence,
        state_digest,
    } = authority.value
    else {
        return Err(NativeGraphError::Invalid("captured fold authority kind"));
    };
    if authority.class != crate::property_graph::storage::reclaim::ProtectedClass::Wal
        || captured.binding != binding
        || sequence != captured.state.sequence
        || manifest_generation != captured.fold.manifest_generation
        || graph_absorbed_through != captured.fold.graph_absorbed_through
        || envelope_sequence != captured.fold.envelope_sequence
        || state_digest != captured.state_digest
    {
        return Err(NativeGraphError::Invalid("captured fold authority binding"));
    }
    visit(captured.state, &mut resources)
}

impl RecoverySource<'_, '_> {
    /// Open and authenticate one immutable artifact, releasing its path charge
    /// before the mapping is returned.
    fn open_mapping(
        &self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<RecoveryMappedArtifact, TreeError> {
        let (path, path_charge) = self.charged_path(reference.artifact)?;
        #[cfg(any(test, feature = "test-seams"))]
        self.slot_observation.open(self.scoped_blocks());
        let mapping = map_file(self.store, &path, MAX_ARTIFACT_BYTES)
            .map_err(|error| self.latch_source(error))?;
        drop(path);
        drop(path_charge);
        let validation = self.admit_mapping(&mapping, reference, resources)?;
        Ok(RecoveryMappedArtifact {
            artifact: reference.artifact,
            validation,
            mapping,
        })
    }

    /// Serve one reference from an already retained slot, without taking a free
    /// one. `None` means this artifact is not in the table.
    fn slot_hit<'a>(
        &'a self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<FramedBlock<'a>>, TreeError> {
        resources.require_preparation(self.memory)?;
        resources.step(0)?;
        let slot = crate::property_graph::storage::mapping_slot(
            self.slots.as_slice(),
            reference.artifact,
            |mapped| mapped.artifact,
            || resources.step(1),
        )?;
        if let Some(mapped) = slot.and_then(OnceCell::get) {
            return self.decode(mapped, reference, resources).map(Some);
        }
        Ok(None)
    }

    /// Serve one reference from the retained slot table, filling a free slot when
    /// the artifact is new. `None` means every slot is already taken by another
    /// artifact; the caller decides whether that is fatal or falls back.
    fn slot_block<'a>(
        &'a self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<FramedBlock<'a>>, TreeError> {
        if let Some(block) = self.slot_hit(reference, resources)? {
            return Ok(Some(block));
        }
        let Some(slot) = crate::property_graph::storage::mapping_slot(
            self.slots.as_slice(),
            reference.artifact,
            |mapped| mapped.artifact,
            || resources.step(1),
        )?
        else {
            return Ok(None);
        };
        slot.set(self.open_mapping(reference, resources)?)
            .map_err(|_| TreeError::Invalid("native recovery source slot initialized twice"))?;
        self.filled.set(self.filled.get().saturating_add(1));
        #[cfg(any(test, feature = "test-seams"))]
        self.slot_observation.filled(self.filled.get());
        self.decode(
            slot.get().ok_or(TreeError::Invalid(
                "native recovery source slot remained empty",
            ))?,
            reference,
            resources,
        )
        .map(Some)
    }

    /// Whether a scoped traversal must stop pinning. The reserve keeps `resolve`
    /// answerable for a consumer that already chose the pinning path.
    fn slots_exhausted(&self) -> bool {
        self.slots
            .as_slice()
            .len()
            .saturating_sub(self.filled.get())
            <= crate::property_graph::storage::tree::directory::RESERVED_PINNED_SLOTS
    }
}

impl BlockSource for RecoverySource<'_, '_> {
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        #[cfg(any(test, feature = "test-seams"))]
        self.slot_observation.resolve(self.slots_exhausted());
        self.slot_block(reference, resources)?
            .ok_or(TreeError::Memory)
    }

    fn with_block<R>(
        &self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
        callback: impl for<'a, 'r> FnOnce(
            FramedBlock<'a>,
            &'r mut TreeResources<'_>,
        ) -> Result<R, TreeError>,
    ) -> Result<R, TreeError> {
        if !self.scoped_blocks() {
            let block = self.resolve(reference, resources)?;
            return callback(block, resources);
        }
        self.check_owner(resources)?;
        if self.retain_window.get() {
            // Artifacts already pinned before the table filled stay free to read.
            // The reserve is never spent here; it belongs to `resolve`.
            if let Some(block) = self.slot_hit(reference, resources)? {
                return callback(block, resources);
            }
            if let Ok(window) = self.window.try_borrow()
                && let Some(mapped) = window.as_ref()
                && mapped.artifact == reference.artifact
            {
                let block = self.decode(mapped, reference, resources)?;
                return callback(block, resources);
            }
            if let Ok(mut window) = self.window.try_borrow_mut() {
                *window = Some(self.open_mapping(reference, resources)?);
                drop(window);
                let window = self
                    .window
                    .try_borrow()
                    .map_err(|_| TreeError::Invalid("scoped window is already borrowed"))?;
                let mapped = window
                    .as_ref()
                    .ok_or(TreeError::Invalid("scoped window remained empty"))?;
                let block = self.decode(mapped, reference, resources)?;
                return callback(block, resources);
            }
        }
        // A nested read naming another artifact keeps the retained window and
        // releases its own mapping before returning.
        let mapped = self.open_mapping(reference, resources)?;
        let block = self.decode(&mapped, reference, resources)?;
        let result = callback(block, resources);
        drop(mapped);
        result
    }

    /// A scoped traversal keeps pinning until its retained slot table is spent.
    /// Until then it reads exactly as an unscoped traversal does.
    fn scoped_blocks(&self) -> bool {
        self.scoped.get() || (self.retain_window.get() && self.slots_exhausted())
    }

    fn with_scoped_reads<R>(&self, body: impl FnOnce() -> R) -> R {
        let previous_retain = self.retain_window.replace(true);
        let result = body();
        self.retain_window.set(previous_retain);
        if !previous_retain && let Ok(mut window) = self.window.try_borrow_mut() {
            *window = None;
        }
        result
    }
}

impl BlockSource for RecoveryArtifactWindow<'_, '_, '_> {
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        self.source.check_owner(resources)?;
        if reference.artifact != self.artifact {
            return Err(TreeError::Invalid(
                "recovery artifact window reference owner",
            ));
        }
        resources.step(1)?;
        self.validation
            .framed_block(self.mapping.as_bytes(), reference)
            .map_err(TreeError::Format)
    }

    fn with_block<R>(
        &self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
        callback: impl for<'a, 'r> FnOnce(
            FramedBlock<'a>,
            &'r mut TreeResources<'_>,
        ) -> Result<R, TreeError>,
    ) -> Result<R, TreeError> {
        if reference.artifact == self.artifact {
            let block = self.resolve(reference, resources)?;
            callback(block, resources)
        } else {
            self.source.with_block(reference, resources, callback)
        }
    }

    fn scoped_blocks(&self) -> bool {
        true
    }
}

struct RecoveryCatalog<'source, 'm> {
    image: CatalogImage<'source>,
    _descriptors: StorageReservation<'m>,
}

impl<'source, 'm> RecoveryCatalog<'source, 'm> {
    fn open(
        source: &'source RecoverySource<'_, 'm>,
        required: RequiredRef,
        expected: GraphInterpretation<'_>,
        high: crate::property_graph::wal::HighWaters,
        resources: &mut TreeResources<'_>,
    ) -> Result<Self, TreeError> {
        let block = source.resolve(required.block, resources)?;
        let identity = block.identity();
        if identity.store != required.object.store
            || identity.artifact != required.object.artifact
            || identity.generation != required.object.generation
            || identity.creation_serial != required.object.serial
            || block.reference() != required.block
            || block.file_length() != required.object.bytes as usize
            || block.file_checksum() != required.object.checksum
        {
            return Err(TreeError::Invalid("recovery catalog descriptor mismatch"));
        }
        let payload = block.payload();
        if payload.get(..4) != Some(b"ZGCP".as_slice())
            || payload.get(4..6) != Some(&1_u16.to_le_bytes())
            || payload.get(6..8) != Some(&1_u16.to_le_bytes())
        {
            return Err(TreeError::Invalid("recovery catalog role or version"));
        }
        let encoded = payload
            .get(8..)
            .ok_or(TreeError::Invalid("recovery catalog payload"))?;
        let count = usize::try_from(u64::from_le_bytes(
            *encoded
                .get(104..112)
                .and_then(|bytes| bytes.first_chunk::<8>())
                .ok_or(TreeError::Invalid("recovery catalog symbol count"))?,
        ))
        .map_err(|_| TreeError::Memory)?;
        let allowance = count
            .checked_mul(std::mem::size_of::<SymbolEntry<'_>>())
            .ok_or(TreeError::Memory)?;
        let mut descriptors = source.memory.reserve(allowance)?;
        let mut callback_error = None;
        let image = CatalogImage::decode(encoded, allowance, &mut || {
            resources.step(1).map_err(|error| {
                if callback_error.is_none() {
                    callback_error = Some(error);
                }
                CatalogError::Cancelled
            })
        })
        .map_err(|error| match error {
            CatalogError::Cancelled => callback_error
                .take()
                .unwrap_or(TreeError::Invalid("recovery catalog cancelled")),
            CatalogError::Capacity | CatalogError::Allocation => TreeError::Memory,
            _ => TreeError::Invalid("invalid recovery catalog"),
        })?;
        descriptors.resize(image.symbols.allocated_bytes())?;
        let mut validation_error = None;
        image
            .declaration
            .validate_for(required.object.store, expected, &mut || {
                resources.step(1).map_err(|error| {
                    if validation_error.is_none() {
                        validation_error = Some(error);
                    }
                    CatalogError::Cancelled
                })
            })
            .map_err(|error| match error {
                CatalogError::Cancelled => validation_error
                    .take()
                    .unwrap_or(TreeError::Invalid("recovery catalog cancelled")),
                CatalogError::Capacity | CatalogError::Allocation => TreeError::Memory,
                _ => TreeError::Invalid("recovery catalog interpretation"),
            })?;
        let [label, relationship_type, property, namespace] = high.symbols;
        if image.declaration.node_high_water != high.node
            || image.declaration.relationship_high_water != high.relationship
            || image.symbols.high_waters()
                != (SymbolHighWaters {
                    label,
                    relationship_type,
                    property,
                    namespace,
                })
        {
            return Err(TreeError::Invalid("recovery catalog high-water mismatch"));
        }
        Ok(Self {
            image,
            _descriptors: descriptors,
        })
    }

    fn lookup_symbol(
        &self,
        kind: SymbolKind,
        name: crate::property_graph::GraphName<'_>,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<Symbol>, TreeError> {
        let mut callback_error = None;
        self.image
            .symbols
            .lookup(kind, name, &mut || {
                resources.step(1).map_err(|error| {
                    if callback_error.is_none() {
                        callback_error = Some(error);
                    }
                    CatalogError::Cancelled
                })
            })
            .map_err(|error| match error {
                CatalogError::Cancelled => callback_error
                    .take()
                    .unwrap_or(TreeError::Invalid("recovery catalog cancelled")),
                CatalogError::Capacity | CatalogError::Allocation => TreeError::Memory,
                _ => TreeError::Invalid("recovery catalog symbol lookup"),
            })
    }

    fn symbol_name(
        &self,
        symbol: Symbol,
        resources: &mut TreeResources<'_>,
    ) -> Result<crate::property_graph::GraphName<'source>, TreeError> {
        for entry in self.image.symbols.entries() {
            resources.step(1)?;
            if entry.symbol == symbol {
                return Ok(entry.name);
            }
        }
        Err(TreeError::Invalid(
            "recovery catalog symbol identity is absent",
        ))
    }

    fn validate_retains(
        &self,
        base: &RecoveryCatalog<'_, '_>,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        for entry in base.image.symbols.entries() {
            resources.step(1)?;
            if self.lookup_symbol(entry.symbol.kind(), entry.name, resources)? != Some(entry.symbol)
            {
                return Err(TreeError::Invalid(
                    "recovery catalog changed an existing symbol mapping",
                ));
            }
        }
        Ok(())
    }
}

impl<S: BlockSource> RecordCatalog<S> for RecoveryCatalog<'_, '_> {
    fn resolve(
        &self,
        kind: SymbolKind,
        name: PayloadSlice<'_, S>,
        resources: &mut TreeResources<'_>,
    ) -> Result<Symbol, TreeError> {
        name.validate_utf8(resources)?;
        for entry in self.image.symbols.entries() {
            resources.step(1)?;
            if entry.symbol.kind() == kind
                && name
                    .compare_bytes(entry.name.as_str().as_bytes(), resources)?
                    .is_eq()
            {
                return Ok(entry.symbol);
            }
        }
        Err(TreeError::Invalid(
            "record name is absent from recovery catalog",
        ))
    }
}

impl<'store, 'source_memory, 'catalog, 'catalog_memory>
    crate::property_graph::storage::reclaim::TraceEntrySource<
        RecoveryCatalog<'catalog, 'catalog_memory>,
    > for RecoverySource<'store, 'source_memory>
{
    fn trace_record_entry<V: crate::property_graph::storage::reclaim::TraceReferenceVisitor>(
        &self,
        catalog: &RecoveryCatalog<'catalog, 'catalog_memory>,
        kind: TreeKind,
        root: crate::property_graph::storage::tree::directory::DirectoryRoot,
        document: Option<&EmbeddingTower>,
        entry: crate::property_graph::storage::tree::directory::DirectoryEntry<'_>,
        visitor: &mut V,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        let record = PayloadRef::decode(entry.value())?;
        self.with_artifact_window(record.reference(), resources, |window, resources| {
            crate::property_graph::storage::reclaim::trace_record_entry_inner(
                window, catalog, kind, root, document, entry, visitor, resources,
            )
        })
    }

    fn trace_fence_entry<V: crate::property_graph::storage::reclaim::TraceReferenceVisitor>(
        &self,
        catalog: &RecoveryCatalog<'catalog, 'catalog_memory>,
        root: crate::property_graph::storage::tree::directory::DirectoryRoot,
        document: Option<&EmbeddingTower>,
        entry: crate::property_graph::storage::tree::directory::DirectoryEntry<'_>,
        visitor: &mut V,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        let fence = fence_window_reference(entry)?;
        self.with_artifact_window(fence.reference(), resources, |window, resources| {
            crate::property_graph::storage::reclaim::trace_fence_entry_inner(
                window, catalog, root, document, entry, visitor, resources,
            )
        })
    }
}

fn exact_relationship_membership(
    source: &RecoverySource<'_, '_>,
    roots: GraphRoots,
    sequence: u64,
    row: crate::property_graph::storage::adjacency::RelationshipRow,
    present: bool,
    memory: &StorageMemory<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    for (kind, node, direction, neighbor) in [
        (TreeKind::OutRanges, row.source, Direction::Out, row.target),
        (TreeKind::InRanges, row.target, Direction::In, row.source),
    ] {
        let root = roots.directory(kind)?;
        let mut scratch = RangeScratch::for_prepare(memory, resources)?;
        if !present {
            let mut probe = [0; 40];
            probe
                .get_mut(..16)
                .ok_or(TreeError::Memory)?
                .copy_from_slice(&node.get().to_le_bytes());
            probe
                .get_mut(16..24)
                .ok_or(TreeError::Memory)?
                .copy_from_slice(&row.relationship_type.get().to_le_bytes());
            probe
                .get_mut(24..)
                .ok_or(TreeError::Memory)?
                .copy_from_slice(&row.rel.get().to_le_bytes());
            if let Some(entry) =
                crate::property_graph::storage::tree::directory::lookup_predecessor(
                    source, root, &probe, resources,
                )?
            {
                let crate::property_graph::storage::tree::Key::Inline(key) = entry.key() else {
                    return Err(TreeError::Invalid("incident range key"));
                };
                let descriptor =
                    crate::property_graph::storage::adjacency::RangeDescriptor::decode(
                        kind,
                        key,
                        entry.value(),
                    )?;
                let key = descriptor.key();
                if key.node == node
                    && key.rel_type == row.relationship_type
                    && row.rel >= key.lower
                    && !matches!(key.upper, crate::property_graph::storage::adjacency::UpperBound::Exclusive(upper) if row.rel >= upper)
                {
                    let range =
                        validate_range(source, root, entry, sequence, &mut scratch, resources)?;
                    if range.edges().iter().any(|edge| edge.rel == row.rel) {
                        return Err(TreeError::Invalid(
                            "recovery relationship adjacency mismatch",
                        ));
                    }
                }
            }
            continue;
        }
        let mut count = 0_u64;
        verify_directory(source, root, resources, &mut |entry, resources| {
            let range = validate_range(source, root, entry, sequence, &mut scratch, resources)?;
            let key = range.descriptor().key();
            for edge in range.edges() {
                resources.step(1)?;
                if edge.rel == row.rel {
                    if key.node != node
                        || key.direction != direction
                        || key.rel_type != row.relationship_type
                        || edge.neighbor != neighbor
                    {
                        return Err(TreeError::Invalid(
                            "recovery relationship adjacency topology mismatch",
                        ));
                    }
                    count = count.checked_add(1).ok_or(TreeError::Work)?;
                }
            }
            Ok(())
        })?;
        if (present && count != 1) || (!present && count != 0) {
            return Err(TreeError::Invalid(
                "recovery relationship adjacency mismatch",
            ));
        }
    }
    Ok(())
}

fn authoritative_relationship(
    source: &RecoverySource<'_, '_>,
    catalog: &RecoveryCatalog<'_, '_>,
    document: Option<&EmbeddingTower>,
    roots: GraphRoots,
    relationship: crate::property_graph::RelId,
    resources: &mut TreeResources<'_>,
) -> Result<Option<RelationshipRow>, TreeError> {
    let root = roots.directory(TreeKind::Relationships)?;
    let Some(entry) = lookup_entry(source, root, &relationship.get().to_le_bytes(), resources)?
    else {
        return Ok(None);
    };
    let payload = PayloadRef::decode(entry.value())?;
    let record = verify_record(
        PayloadSlice::new(source, roots.store(), entry.creation_generation(), payload),
        crate::property_graph::EntityId::Relationship(relationship),
        catalog,
        document,
        resources,
    )?;
    let RecordShape::Relationship {
        id,
        source: relationship_source,
        target: relationship_target,
        relationship_type,
    } = record.shape()
    else {
        return Err(TreeError::Invalid("recovery relationship directory role"));
    };
    Ok(Some(RelationshipRow {
        rel: id,
        source: relationship_source,
        target: relationship_target,
        relationship_type,
    }))
}

fn fence_matches_record(
    fence: &FenceView<'_, RecoverySource<'_, '_>>,
    record: &RecordView<'_, RecoverySource<'_, '_>>,
    resources: &mut TreeResources<'_>,
) -> Result<bool, TreeError> {
    Ok(fence.incarnation() == record.incarnation()
        && fence.revision() == record.revision()
        && stored_provenance_equal(fence.provenance(), record.provenance(), resources)?
        && fence
            .canonical_bytes()
            .ok_or(TreeError::Invalid(
                "live recovery fence lacks canonical bytes",
            ))?
            .compare(record.canonical_bytes(), resources)?
            .is_eq())
}

#[allow(clippy::too_many_arguments)]
fn validate_fence_record_agreement(
    source: &RecoverySource<'_, '_>,
    catalog: &RecoveryCatalog<'_, '_>,
    document: Option<&EmbeddingTower>,
    roots: GraphRoots,
    fence: &FenceView<'_, RecoverySource<'_, '_>>,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    match fence.incarnation() {
        EntityId::Node(node) => {
            let root = roots.directory(TreeKind::Nodes)?;
            let Some(entry) = lookup_entry(source, root, &node.get().to_le_bytes(), resources)?
            else {
                if !fence.is_deleted() {
                    return Err(TreeError::Invalid("recovery live node fence is absent"));
                }
                require_no_physical_incident(source, roots, node, resources)?;
                return Ok(());
            };
            let payload = PayloadRef::decode(entry.value())?;
            match verify_node_state(
                PayloadSlice::new(source, roots.store(), entry.creation_generation(), payload),
                node,
                catalog,
                document,
                resources,
            )? {
                NodeRecordState::Live(record) => {
                    if fence.is_deleted() || !fence_matches_record(fence, &record, resources)? {
                        return Err(TreeError::Invalid("recovery live node fence mismatch"));
                    }
                }
                NodeRecordState::Tombstone(tombstone) => {
                    if !fence.is_deleted()
                        || fence.revision() != tombstone.revision()
                        || !stored_provenance_equal(
                            fence.provenance(),
                            tombstone.provenance(),
                            resources,
                        )?
                    {
                        return Err(TreeError::Invalid("recovery deleted node fence mismatch"));
                    }
                }
            }
        }
        EntityId::Relationship(relationship) => {
            let root = roots.directory(TreeKind::Relationships)?;
            let entry = lookup_entry(source, root, &relationship.get().to_le_bytes(), resources)?;
            match (fence.is_deleted(), entry) {
                (true, None) => {}
                (true, Some(_)) => {
                    return Err(TreeError::Invalid(
                        "recovery deleted relationship fence has a record",
                    ));
                }
                (false, None) => {
                    // Checkpoint validation below correlates every extant
                    // adjacency/type entry with Relationships once. That proves
                    // absence for all swept fences without per-fence tree walks.
                    fence.hidden_relationship(source, roots, catalog, document, resources)?;
                }
                (false, Some(entry)) => {
                    let payload = PayloadRef::decode(entry.value())?;
                    let record = verify_record(
                        PayloadSlice::new(
                            source,
                            roots.store(),
                            entry.creation_generation(),
                            payload,
                        ),
                        EntityId::Relationship(relationship),
                        catalog,
                        document,
                        resources,
                    )?;
                    if !fence_matches_record(fence, &record, resources)? {
                        return Err(TreeError::Invalid(
                            "recovery live relationship fence mismatch",
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn require_live_record_fence(
    source: &RecoverySource<'_, '_>,
    catalog: &RecoveryCatalog<'_, '_>,
    document: Option<&EmbeddingTower>,
    roots: GraphRoots,
    record: &RecordView<'_, RecoverySource<'_, '_>>,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    let Some(key) = record.provenance().key() else {
        return Ok(());
    };
    let fence = find_fence_by_key(source, catalog, document, roots, key, resources)?.ok_or(
        TreeError::Invalid("recovery keyed live record lacks a fence"),
    )?;
    if !fence_matches_record(&fence, record, resources)? {
        return Err(TreeError::Invalid(
            "recovery keyed live record differs from its fence",
        ));
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "independent resource owners and lifetimes are explicit at this private seam"
)]
fn validate_native_checkpoint(
    source: &RecoverySource<'_, '_>,
    catalog: &RecoveryCatalog<'_, '_>,
    document: Option<&EmbeddingTower>,
    memory: &StorageMemory<'_>,
    roots: GraphRoots,
    sequence: u64,
    high_waters: crate::property_graph::wal::HighWaters,
    reclaim: Option<(
        crate::property_graph::wal::BatchId,
        &[ArtifactDescriptor],
        bool,
    )>,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    for kind in [
        TreeKind::Nodes,
        TreeKind::Labels,
        TreeKind::RelationshipTypes,
    ] {
        let root = roots.directory(kind)?;
        let mut values = NativeDirectoryValues::new(catalog, document);
        verify_directory(source, root, resources, &mut |entry, resources| {
            if kind == TreeKind::Nodes {
                let crate::property_graph::storage::tree::Key::Inline(key) = entry.key() else {
                    return Err(TreeError::Invalid("recovery node key role"));
                };
                let id = u128::from_le_bytes(
                    key.try_into()
                        .map_err(|_| TreeError::Invalid("recovery node key width"))?,
                );
                if id == 0 || id > high_waters.node {
                    let payload = PayloadRef::decode(entry.value())?;
                    let record = verify_node_state(
                        PayloadSlice::new(
                            source,
                            roots.store(),
                            entry.creation_generation(),
                            payload,
                        ),
                        NodeId::from(crate::ingest::DocId::new(id)),
                        catalog,
                        document,
                        resources,
                    )?;
                    let bound = match record {
                        NodeRecordState::Live(record) => record.document_version(),
                        NodeRecordState::Tombstone(record) => record.document_version(),
                    };
                    if bound.is_none() {
                        return Err(TreeError::Invalid("recovery node high-water"));
                    }
                }
            }
            values.verify(source, root, entry, resources)
        })?;
    }

    let fence_root = roots.directory(TreeKind::KeyFences)?;
    verify_directory(source, fence_root, resources, &mut |entry, resources| {
        let fence = verify_fence_entry(source, fence_root, entry, catalog, document, resources)?;
        validate_fence_record_agreement(source, catalog, document, roots, &fence, resources)
    })?;

    let node_root = roots.directory(TreeKind::Nodes)?;
    let label_root = roots.directory(TreeKind::Labels)?;
    verify_directory(source, node_root, resources, &mut |entry, resources| {
        let crate::property_graph::storage::tree::Key::Inline(key) = entry.key() else {
            return Err(TreeError::Invalid("recovery node key role"));
        };
        let node = NodeId::from(crate::ingest::DocId::new(u128::from_le_bytes(
            key.try_into()
                .map_err(|_| TreeError::Invalid("recovery node key width"))?,
        )));
        let payload = PayloadRef::decode(entry.value())?;
        if let NodeRecordState::Live(record) = verify_node_state(
            PayloadSlice::new(source, roots.store(), entry.creation_generation(), payload),
            node,
            catalog,
            document,
            resources,
        )? {
            require_live_record_fence(source, catalog, document, roots, &record, resources)?;
            let RecordShape::Node { labels, .. } = record.shape() else {
                return Err(TreeError::Invalid("recovery node record role"));
            };
            for index in 0..labels {
                resources.step(1)?;
                let label = record.label(index, resources)?;
                let mut membership = [0_u8; 24];
                membership
                    .get_mut(..8)
                    .ok_or(TreeError::Memory)?
                    .copy_from_slice(&label.get().to_le_bytes());
                membership
                    .get_mut(8..)
                    .ok_or(TreeError::Memory)?
                    .copy_from_slice(&node.get().to_le_bytes());
                let member = lookup_entry(source, label_root, &membership, resources)?.ok_or(
                    TreeError::Invalid("recovery node label membership is absent"),
                )?;
                if !member.value().is_empty() {
                    return Err(TreeError::Invalid("recovery node label membership value"));
                }
            }
        }
        Ok(())
    })?;
    verify_directory(source, label_root, resources, &mut |entry, resources| {
        let crate::property_graph::storage::tree::Key::Inline(key) = entry.key() else {
            return Err(TreeError::Invalid("recovery label key role"));
        };
        let label = crate::property_graph::catalog::LabelId::new(u64::from_le_bytes(
            key.get(..8)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(TreeError::Invalid("recovery label key width"))?,
        ))
        .map_err(|_| TreeError::Invalid("recovery label identity"))?;
        let node = NodeId::from(crate::ingest::DocId::new(u128::from_le_bytes(
            key.get(8..)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(TreeError::Invalid("recovery label node width"))?,
        )));
        let node_entry = lookup_entry(source, node_root, &node.get().to_le_bytes(), resources)?
            .ok_or(TreeError::Invalid("recovery label node is absent"))?;
        let payload = PayloadRef::decode(node_entry.value())?;
        let NodeRecordState::Live(record) = verify_node_state(
            PayloadSlice::new(
                source,
                roots.store(),
                node_entry.creation_generation(),
                payload,
            ),
            node,
            catalog,
            document,
            resources,
        )?
        else {
            return Err(TreeError::Invalid("recovery label node is deleted"));
        };
        let RecordShape::Node { labels, .. } = record.shape() else {
            return Err(TreeError::Invalid("recovery label record role"));
        };
        let mut found = false;
        for index in 0..labels {
            resources.step(1)?;
            found |= record.label(index, resources)? == label;
        }
        if !found {
            return Err(TreeError::Invalid(
                "recovery label is absent from node record",
            ));
        }
        Ok(())
    })?;

    let relationship_root = roots.directory(TreeKind::Relationships)?;
    let mut values = NativeDirectoryValues::new(catalog, document);
    verify_directory(
        source,
        relationship_root,
        resources,
        &mut |entry, resources| {
            values.verify(source, relationship_root, entry, resources)?;
            let crate::property_graph::storage::tree::Key::Inline(key) = entry.key() else {
                return Err(TreeError::Invalid("recovery relationship key role"));
            };
            let id = u128::from_le_bytes(
                key.try_into()
                    .map_err(|_| TreeError::Invalid("recovery relationship key width"))?,
            );
            if id == 0 || id > high_waters.relationship {
                return Err(TreeError::Invalid("recovery relationship high-water"));
            }
            let relationship = crate::property_graph::RelId::new(id)
                .map_err(|_| TreeError::Invalid("recovery relationship identity"))?;
            let row = authoritative_relationship(
                source,
                catalog,
                document,
                roots,
                relationship,
                resources,
            )?
            .ok_or(TreeError::Invalid("recovery relationship disappeared"))?;
            let payload = PayloadRef::decode(entry.value())?;
            let record = verify_record(
                PayloadSlice::new(source, roots.store(), entry.creation_generation(), payload),
                EntityId::Relationship(relationship),
                catalog,
                document,
                resources,
            )?;
            require_live_record_fence(source, catalog, document, roots, &record, resources)?;
            exact_relationship_membership(source, roots, sequence, row, true, memory, resources)
        },
    )?;

    let type_root = roots.directory(TreeKind::RelationshipTypes)?;
    verify_directory(
        source,
        relationship_root,
        resources,
        &mut |entry, resources| {
            let crate::property_graph::storage::tree::Key::Inline(key) = entry.key() else {
                return Err(TreeError::Invalid("recovery relationship key role"));
            };
            let rel = crate::property_graph::RelId::new(u128::from_le_bytes(
                key.try_into()
                    .map_err(|_| TreeError::Invalid("recovery relationship key width"))?,
            ))
            .map_err(|_| TreeError::Invalid("recovery relationship identity"))?;
            let row = authoritative_relationship(source, catalog, document, roots, rel, resources)?
                .ok_or(TreeError::Invalid("recovery relationship disappeared"))?;
            let mut membership = [0_u8; 24];
            membership
                .get_mut(..8)
                .ok_or(TreeError::Memory)?
                .copy_from_slice(&row.relationship_type.get().to_le_bytes());
            membership
                .get_mut(8..)
                .ok_or(TreeError::Memory)?
                .copy_from_slice(&rel.get().to_le_bytes());
            let member = lookup_entry(source, type_root, &membership, resources)?.ok_or(
                TreeError::Invalid("recovery relationship type membership is absent"),
            )?;
            if !member.value().is_empty() {
                return Err(TreeError::Invalid(
                    "recovery relationship type membership value",
                ));
            }
            Ok(())
        },
    )?;
    verify_directory(source, type_root, resources, &mut |entry, resources| {
        let crate::property_graph::storage::tree::Key::Inline(key) = entry.key() else {
            return Err(TreeError::Invalid("recovery relationship type key role"));
        };
        let relationship_type = crate::property_graph::catalog::RelTypeId::new(u64::from_le_bytes(
            key.get(..8)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(TreeError::Invalid("recovery relationship type key width"))?,
        ))
        .map_err(|_| TreeError::Invalid("recovery relationship type identity"))?;
        let rel = crate::property_graph::RelId::new(u128::from_le_bytes(
            key.get(8..)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(TreeError::Invalid(
                    "recovery relationship type entity width",
                ))?,
        ))
        .map_err(|_| TreeError::Invalid("recovery relationship type entity"))?;
        let row = authoritative_relationship(source, catalog, document, roots, rel, resources)?
            .ok_or(TreeError::Invalid(
                "recovery relationship type entity is absent",
            ))?;
        if row.relationship_type != relationship_type {
            return Err(TreeError::Invalid(
                "recovery relationship type differs from record",
            ));
        }
        Ok(())
    })?;

    let mut scratch = RangeScratch::for_prepare(memory, resources)?;
    for kind in [TreeKind::OutRanges, TreeKind::InRanges] {
        let root = roots.directory(kind)?;
        verify_directory(source, root, resources, &mut |entry, resources| {
            let range = validate_range(source, root, entry, sequence, &mut scratch, resources)?;
            let key = range.descriptor().key();
            for edge in range.edges() {
                resources.step(1)?;
                let row = authoritative_relationship(
                    source, catalog, document, roots, edge.rel, resources,
                )?
                .ok_or(TreeError::Invalid(
                    "recovery adjacency relationship is absent",
                ))?;
                let matches = match key.direction {
                    Direction::Out => {
                        row.source == key.node
                            && row.target == edge.neighbor
                            && row.relationship_type == key.rel_type
                    }
                    Direction::In => {
                        row.target == key.node
                            && row.source == edge.neighbor
                            && row.relationship_type == key.rel_type
                    }
                };
                if !matches {
                    return Err(TreeError::Invalid("recovery adjacency topology mismatch"));
                }
            }
            Ok(())
        })?;
    }

    let inventory_root = roots.directory(TreeKind::ObjectInventory)?;
    verify_directory(
        source,
        inventory_root,
        resources,
        &mut |entry, resources| {
            let change = crate::property_graph::storage::inventory::verify_inventory_entry(
                inventory_root,
                entry,
                resources,
            )?;
            // An inventory row is a liveness claim of its own generation only.
            // A completed reclaim legitimately unlinks artifacts that older
            // replayed states still list as `Retained`, so file existence is
            // proved once against the state this open actually publishes; see
            // `validate_deferred_checkpoint_allocations`. The reclaim proof
            // binding below stays an exact per-state check.
            if let InventoryState::ReclaimPending(id) | InventoryState::Reclaimed(id) = change.state
            {
                let Some((expected_id, candidates, completed)) = reclaim else {
                    return Err(TreeError::Invalid("unsupported recovery inventory proof"));
                };
                if id != expected_id
                    || !candidates.contains(&change.object)
                    || (matches!(change.state, InventoryState::Reclaimed(_)) && !completed)
                {
                    return Err(TreeError::Invalid("recovery inventory proof mismatch"));
                }
            }
            Ok(())
        },
    )?;
    Ok(())
}

fn stored_keys_equal<S: BlockSource>(
    left: StoredKey<'_, S>,
    right: StoredKey<'_, S>,
    resources: &mut TreeResources<'_>,
) -> Result<bool, TreeError> {
    Ok(left.kind() == right.kind()
        && left
            .namespace()
            .compare(right.namespace(), resources)?
            .is_eq()
        && left.key().compare(right.key(), resources)?.is_eq())
}

fn stored_key_matches<S: BlockSource>(
    stored: StoredKey<'_, S>,
    key: ApplicationKey<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<bool, TreeError> {
    Ok(stored.kind() == key.kind()
        && stored
            .namespace()
            .compare_bytes(key.namespace().as_str().as_bytes(), resources)?
            .is_eq()
        && stored
            .key()
            .compare_bytes(key.key().as_str().as_bytes(), resources)?
            .is_eq())
}

fn stored_provenance_equal<S: BlockSource>(
    left: &StoredProvenance<'_, S>,
    right: &StoredProvenance<'_, S>,
    resources: &mut TreeResources<'_>,
) -> Result<bool, TreeError> {
    if left.operation() != right.operation()
        || left.requested_revision() != right.requested_revision()
        || left.installed_revision() != right.installed_revision()
        || left.expected() != right.expected()
        || left.incarnation() != right.incarnation()
        || left.delete_mode() != right.delete_mode()
        || left.original_generation() != right.original_generation()
    {
        return Ok(false);
    }
    match (left.key(), right.key()) {
        (None, None) => Ok(true),
        (Some(left), Some(right)) => stored_keys_equal(left, right, resources),
        _ => Ok(false),
    }
}

fn records_equal<S: BlockSource>(
    left: &RecordView<'_, S>,
    right: &RecordView<'_, S>,
    resources: &mut TreeResources<'_>,
) -> Result<bool, TreeError> {
    Ok(left.shape() == right.shape()
        && left.revision() == right.revision()
        && left.document_version() == right.document_version()
        && stored_provenance_equal(left.provenance(), right.provenance(), resources)?
        && left
            .canonical_bytes()
            .compare(right.canonical_bytes(), resources)?
            .is_eq())
}

#[allow(
    clippy::too_many_arguments,
    reason = "recovery compares one entity across two complete immutable roots"
)]
fn entity_logically_equal(
    source: &RecoverySource<'_, '_>,
    catalog: &RecoveryCatalog<'_, '_>,
    document: Option<&EmbeddingTower>,
    base: GraphRoots,
    target: GraphRoots,
    entity: EntityId,
    resources: &mut TreeResources<'_>,
) -> Result<bool, TreeError> {
    match entity {
        EntityId::Node(node) => {
            let load = |roots: GraphRoots, resources: &mut TreeResources<'_>| {
                let root = roots.directory(TreeKind::Nodes)?;
                let entry = lookup_entry(source, root, &node.get().to_le_bytes(), resources)?
                    .ok_or(TreeError::Invalid(
                        "recovery node transition entry is absent",
                    ))?;
                let payload = PayloadRef::decode(entry.value())?;
                verify_node_state(
                    PayloadSlice::new(source, roots.store(), entry.creation_generation(), payload),
                    node,
                    catalog,
                    document,
                    resources,
                )
            };
            let left = load(base, resources)?;
            let right = load(target, resources)?;
            match (left, right) {
                (NodeRecordState::Live(left), NodeRecordState::Live(right)) => {
                    records_equal(&left, &right, resources)
                }
                (NodeRecordState::Tombstone(left), NodeRecordState::Tombstone(right)) => Ok(left
                    .revision()
                    == right.revision()
                    && stored_provenance_equal(left.provenance(), right.provenance(), resources)?),
                _ => Ok(false),
            }
        }
        EntityId::Relationship(rel) => {
            let load = |roots: GraphRoots, resources: &mut TreeResources<'_>| {
                let root = roots.directory(TreeKind::Relationships)?;
                let entry = lookup_entry(source, root, &rel.get().to_le_bytes(), resources)?
                    .ok_or(TreeError::Invalid(
                        "recovery relationship transition entry is absent",
                    ))?;
                let payload = PayloadRef::decode(entry.value())?;
                verify_record(
                    PayloadSlice::new(source, roots.store(), entry.creation_generation(), payload),
                    entity,
                    catalog,
                    document,
                    resources,
                )
            };
            let left = load(base, resources)?;
            let right = load(target, resources)?;
            records_equal(&left, &right, resources)
        }
    }
}

fn mutation_count_for_entity(mutations: &[Mutation<'_>], entity: EntityId) -> usize {
    mutations
        .iter()
        .filter(|mutation| mutation.provenance.incarnation == entity)
        .count()
}

fn mutation_count_for_stored_key(
    mutations: &[Mutation<'_>],
    stored: StoredKey<'_, RecoverySource<'_, '_>>,
    resources: &mut TreeResources<'_>,
) -> Result<usize, TreeError> {
    let mut count = 0_usize;
    for mutation in mutations {
        resources.step(1)?;
        if let Some(key) = mutation.provenance.key
            && stored_key_matches(stored, key, resources)?
        {
            count = count.checked_add(1).ok_or(TreeError::Work)?;
        }
    }
    Ok(count)
}

#[allow(
    clippy::too_many_arguments,
    reason = "recovery streams exact base and target entity directories"
)]
fn reconcile_entity_directory(
    source: &RecoverySource<'_, '_>,
    catalog: &RecoveryCatalog<'_, '_>,
    document: Option<&EmbeddingTower>,
    base: GraphRoots,
    target: GraphRoots,
    kind: TreeKind,
    mutations: &[Mutation<'_>],
    base_high: u128,
    maintenance: bool,
    target_sequence: u64,
    memory: &StorageMemory<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    let base_root = base.directory(kind)?;
    let target_root = target.directory(kind)?;
    let mut left = DirectoryCursor::seek(source, base_root, None, resources)?;
    let mut right = DirectoryCursor::seek(source, target_root, None, resources)?;
    let mut left_key = [0_u8; 16];
    let mut right_key = [0_u8; 16];
    let mut left_value = [0_u8; 48];
    let mut right_value = [0_u8; 48];
    let mut left_row = left.next(&mut left_key, &mut left_value, resources)?;
    let mut right_row = right.next(&mut right_key, &mut right_value, resources)?;
    while left_row.is_some() || right_row.is_some() {
        resources.step(1)?;
        let left_id = left_row
            .map(|(key, value)| {
                if key != 16 || value != 48 {
                    return Err(TreeError::Invalid("recovery entity directory row width"));
                }
                Ok(u128::from_le_bytes(left_key))
            })
            .transpose()?;
        let right_id = right_row
            .map(|(key, value)| {
                if key != 16 || value != 48 {
                    return Err(TreeError::Invalid("recovery entity directory row width"));
                }
                Ok(u128::from_le_bytes(right_key))
            })
            .transpose()?;
        let (id, changed, advance_left, advance_right) = match (left_id, right_id) {
            (Some(left), Some(right)) if left < right => (left, true, true, false),
            (Some(left), Some(right)) if left > right => (right, true, false, true),
            (Some(id), Some(_)) => {
                let entity = match kind {
                    TreeKind::Nodes => EntityId::Node(NodeId::from(crate::ingest::DocId::new(id))),
                    TreeKind::Relationships => crate::property_graph::RelId::new(id)
                        .map(EntityId::Relationship)
                        .map_err(|_| TreeError::Invalid("recovery relationship identity"))?,
                    _ => return Err(TreeError::Invalid("recovery entity directory kind")),
                };
                (
                    id,
                    !entity_logically_equal(
                        source, catalog, document, base, target, entity, resources,
                    )?,
                    true,
                    true,
                )
            }
            (Some(left), None) => (left, true, true, false),
            (None, Some(right)) => (right, true, false, true),
            (None, None) => break,
        };
        let entity = match kind {
            TreeKind::Nodes => EntityId::Node(NodeId::from(crate::ingest::DocId::new(id))),
            TreeKind::Relationships => crate::property_graph::RelId::new(id)
                .map(EntityId::Relationship)
                .map_err(|_| TreeError::Invalid("recovery relationship identity"))?,
            _ => return Err(TreeError::Invalid("recovery entity directory kind")),
        };
        let count = mutation_count_for_entity(mutations, entity);
        let swept = maintenance && changed && advance_left && !advance_right;
        if swept {
            validate_maintenance_removal(
                source,
                catalog,
                document,
                base,
                target,
                target_sequence,
                entity,
                memory,
                resources,
            )?;
        }
        if (changed && !swept && count != 1) || ((!changed || swept) && count != 0) {
            return Err(TreeError::Invalid(
                "recovery entity transition differs from mutation set",
            ));
        }
        if advance_right && !advance_left && id <= base_high {
            let adoption = kind == TreeKind::Nodes
                && live_record(source, catalog, document, target, entity, resources)?
                    .is_some_and(|record| record.document_version().is_some());
            if !adoption {
                return Err(TreeError::Invalid(
                    "recovery fresh identity below high-water",
                ));
            }
        }
        if advance_left {
            left_row = left.next(&mut left_key, &mut left_value, resources)?;
        }
        if advance_right {
            right_row = right.next(&mut right_key, &mut right_value, resources)?;
        }
    }
    Ok(())
}

fn find_fence_by_key<'s, 'store, 'm, 'catalog_memory>(
    source: &'s RecoverySource<'store, 'm>,
    catalog: &RecoveryCatalog<'s, 'catalog_memory>,
    document: Option<&EmbeddingTower>,
    roots: GraphRoots,
    wanted: StoredKey<'s, RecoverySource<'store, 'm>>,
    resources: &mut TreeResources<'_>,
) -> Result<Option<FenceView<'s, RecoverySource<'store, 'm>>>, TreeError> {
    let root = roots.directory(TreeKind::KeyFences)?;
    let Symbol::Namespace(namespace) =
        catalog.resolve(SymbolKind::Namespace, wanted.namespace(), resources)?
    else {
        return Err(TreeError::Invalid("recovery fence namespace domain"));
    };
    // One bounded application key, charged to the recovery owner. Looking it
    // up in the existing key directory avoids scanning every fence per record.
    let length = usize::try_from(wanted.key().len()).map_err(|_| TreeError::Memory)?;
    let mut text = StorageBuffer::new(source.memory, length)?;
    for _ in 0..length {
        text.push(0_u8)?;
    }
    if wanted.key().read_at(0, text.as_mut_slice(), resources)? != length {
        return Err(TreeError::Invalid("short recovery fence key"));
    }
    let text = std::str::from_utf8(text.as_slice())
        .map_err(|_| TreeError::Invalid("recovery fence key UTF-8"))?;
    let probe = FenceKey::new(wanted.kind(), namespace, text)?;
    let Some(entry) = lookup_fence_entry(source, root, probe, resources)? else {
        return Ok(None);
    };
    #[cfg(test)]
    FENCE_CANDIDATES.with(|count| count.set(count.get() + 1));
    let fence = verify_fence_entry(source, root, entry, catalog, document, resources)?;
    let key = fence
        .provenance()
        .key()
        .ok_or(TreeError::Invalid("recovery fence is unkeyed"))?;
    if !stored_keys_equal(wanted, key, resources)? {
        return Err(TreeError::Invalid("recovery fence lookup mismatch"));
    }
    Ok(Some(fence))
}

fn fences_equal(
    left: &FenceView<'_, RecoverySource<'_, '_>>,
    right: &FenceView<'_, RecoverySource<'_, '_>>,
    resources: &mut TreeResources<'_>,
) -> Result<bool, TreeError> {
    if left.incarnation() != right.incarnation()
        || left.revision() != right.revision()
        || left.is_deleted() != right.is_deleted()
        || !stored_provenance_equal(left.provenance(), right.provenance(), resources)?
    {
        return Ok(false);
    }
    match (left.canonical_bytes(), right.canonical_bytes()) {
        (None, None) => Ok(true),
        (Some(left), Some(right)) => Ok(left.compare(right, resources)?.is_eq()),
        _ => Ok(false),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "recovery streams the exact permanent key ledger in both directions"
)]
fn reconcile_fence_directory(
    source: &RecoverySource<'_, '_>,
    catalog: &RecoveryCatalog<'_, '_>,
    document: Option<&EmbeddingTower>,
    base: GraphRoots,
    target: GraphRoots,
    mutations: &[Mutation<'_>],
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    for (outer_roots, inner_roots) in [(base, target), (target, base)] {
        let root = outer_roots.directory(TreeKind::KeyFences)?;
        let mut cursor = DirectoryCursor::seek(source, root, None, resources)?;
        while let Some(entry) = cursor.next_entry(resources)? {
            let fence = verify_fence_entry(source, root, entry, catalog, document, resources)?;
            let key = fence
                .provenance()
                .key()
                .ok_or(TreeError::Invalid("recovery fence is unkeyed"))?;
            let counterpart =
                find_fence_by_key(source, catalog, document, inner_roots, key, resources)?;
            let changed = match counterpart.as_ref() {
                None => true,
                Some(other) => !fences_equal(&fence, other, resources)?,
            };
            let count = mutation_count_for_stored_key(mutations, key, resources)?;
            if (changed && count != 1) || (!changed && count != 0) {
                return Err(TreeError::Invalid(
                    "recovery key-fence transition differs from mutation set",
                ));
            }
        }
    }
    Ok(())
}

struct RecoveryPayloadReader<'source, 'resources, 'store, 'source_memory, 'tree_memory> {
    slice: PayloadSlice<'source, RecoverySource<'store, 'source_memory>>,
    offset: u64,
    resources: &'resources RefCell<&'resources mut TreeResources<'tree_memory>>,
    first_error: &'resources Cell<Option<TreeError>>,
}

impl Read for RecoveryPayloadReader<'_, '_, '_, '_, '_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let mut resources = self
            .resources
            .try_borrow_mut()
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::WouldBlock))?;
        let remaining = usize::try_from(self.slice.len().saturating_sub(self.offset))
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
        let target = output
            .get_mut(..remaining.min(output.len()))
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        let read = self
            .slice
            .read_at(self.offset, target, &mut resources)
            .map_err(|error| {
                let previous = self.first_error.take();
                self.first_error.set(previous.or(Some(error)));
                std::io::Error::from(std::io::ErrorKind::Other)
            })?;
        self.offset = self
            .offset
            .checked_add(read as u64)
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
        Ok(read)
    }
}

fn streamed_fingerprint<S: BlockSource>(
    slice: PayloadSlice<'_, S>,
    resources: &mut TreeResources<'_>,
) -> Result<CanonicalFingerprint, TreeError> {
    let mut hash = Xxh3::new();
    let mut chunk = [0_u8; crate::property_graph::storage::payload::CHUNK_BYTES];
    let mut offset = 0_u64;
    while offset < slice.len() {
        let remaining = usize::try_from(slice.len() - offset).map_err(|_| TreeError::Memory)?;
        let chunk_length = chunk.len();
        let output = chunk
            .get_mut(..remaining.min(chunk_length))
            .ok_or(TreeError::Memory)?;
        let read = slice.read_at(offset, output, resources)?;
        if read != output.len() {
            return Err(TreeError::Invalid("short recovery canonical"));
        }
        hash.update(output);
        offset = offset.checked_add(read as u64).ok_or(TreeError::Memory)?;
    }
    CanonicalFingerprint::new(slice.len(), hash.digest())
        .map_err(|_| TreeError::Invalid("invalid recovery canonical fingerprint"))
}

fn recovery_entity_shape<'catalog>(
    shape: RecordShape,
    catalog: &'catalog RecoveryCatalog<'catalog, '_>,
    resources: &mut TreeResources<'_>,
) -> Result<EntityShape<'catalog>, TreeError> {
    match shape {
        RecordShape::Node { .. } => Ok(EntityShape::Node),
        RecordShape::Relationship {
            source,
            target,
            relationship_type,
            ..
        } => Ok(EntityShape::Relationship {
            source,
            target,
            relationship_type: catalog
                .symbol_name(Symbol::RelationshipType(relationship_type), resources)?,
        }),
    }
}

fn live_record<'source, 'store, 'm>(
    source: &'source RecoverySource<'store, 'm>,
    catalog: &RecoveryCatalog<'source, '_>,
    document: Option<&EmbeddingTower>,
    roots: GraphRoots,
    entity: EntityId,
    resources: &mut TreeResources<'_>,
) -> Result<Option<RecordView<'source, RecoverySource<'store, 'm>>>, TreeError> {
    match entity {
        EntityId::Node(node) => {
            let root = roots.directory(TreeKind::Nodes)?;
            let Some(entry) = lookup_entry(source, root, &node.get().to_le_bytes(), resources)?
            else {
                return Ok(None);
            };
            let payload = PayloadRef::decode(entry.value())?;
            match verify_node_state(
                PayloadSlice::new(source, roots.store(), entry.creation_generation(), payload),
                node,
                catalog,
                document,
                resources,
            )? {
                NodeRecordState::Live(record) => Ok(Some(record)),
                NodeRecordState::Tombstone(_) => Ok(None),
            }
        }
        EntityId::Relationship(rel) => {
            let root = roots.directory(TreeKind::Relationships)?;
            let Some(entry) = lookup_entry(source, root, &rel.get().to_le_bytes(), resources)?
            else {
                return Ok(None);
            };
            let payload = PayloadRef::decode(entry.value())?;
            verify_record(
                PayloadSlice::new(source, roots.store(), entry.creation_generation(), payload),
                entity,
                catalog,
                document,
                resources,
            )
            .map(Some)
        }
    }
}

fn lifecycle_tree_error(
    first_error: &Cell<Option<TreeError>>,
    _: crate::property_graph::KeyLifecycleError,
) -> TreeError {
    first_error
        .take()
        .unwrap_or(TreeError::Invalid("recovery key lifecycle transition"))
}

fn recovery_checkpoint(
    resources: &RefCell<&mut TreeResources<'_>>,
    first_error: &Cell<Option<TreeError>>,
) -> Result<(), CanonicalError> {
    let mut resources = resources
        .try_borrow_mut()
        .map_err(|_| CanonicalError::Cancelled)?;
    resources.step(1).map_err(|error| {
        let previous = first_error.take();
        first_error.set(previous.or(Some(error)));
        CanonicalError::Cancelled
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "recovery replays exact base lifecycle against the persisted target"
)]
fn validate_lifecycle_transition<'source, 'store, 'source_memory, 'catalog_memory, 'tree_memory>(
    source: &'source RecoverySource<'store, 'source_memory>,
    catalog: &'source RecoveryCatalog<'source, 'catalog_memory>,
    document: Option<&EmbeddingTower>,
    base_roots: GraphRoots,
    target_roots: GraphRoots,
    base_high_waters: crate::property_graph::wal::HighWaters,
    mutation: Mutation<'source>,
    resources: &mut TreeResources<'tree_memory>,
) -> Result<(), TreeError> {
    let fields = mutation.provenance;
    if !matches!(mutation.provenance_version, 1 | 2)
        || fields.requested_revision != fields.installed_revision
        || fields.original_generation != target_roots.generation()
    {
        return Err(TreeError::Invalid("recovery mutation provenance domain"));
    }
    let fresh_high = match fields.incarnation {
        EntityId::Node(node) => (node.get(), base_high_waters.node),
        EntityId::Relationship(rel) => (rel.get(), base_high_waters.relationship),
    };
    if fields.operation == GraphOperation::CypherEdit
        && fields.expected == ExpectedGraphState::Absent
    {
        if fields.key.is_some()
            || !mutation.live
            || fields.delete_mode.is_some()
            || fields.requested_revision.get() != 1
            || (fresh_high.0 <= fresh_high.1 && !matches!(fields.incarnation, EntityId::Node(_)))
        {
            return Err(TreeError::Invalid("recovery fresh Cypher lifecycle"));
        }
        if fresh_high.0 <= fresh_high.1 {
            let record = live_record(
                source,
                catalog,
                document,
                target_roots,
                fields.incarnation,
                resources,
            )?
            .ok_or(TreeError::Invalid("recovery document adoption record"))?;
            if record.document_version().is_none()
                || live_record(
                    source,
                    catalog,
                    document,
                    base_roots,
                    fields.incarnation,
                    resources,
                )?
                .is_some()
            {
                return Err(TreeError::Invalid("recovery document adoption identity"));
            }
        }
        return Ok(());
    }
    if matches!(
        fields.operation,
        GraphOperation::StructuredCreate | GraphOperation::StructuredRecreate
    ) && fresh_high.0 <= fresh_high.1
    {
        return Err(TreeError::Invalid(
            "recovery fresh identity below high-water",
        ));
    }

    let target_record = if mutation.live {
        Some(
            live_record(
                source,
                catalog,
                document,
                target_roots,
                fields.incarnation,
                resources,
            )?
            .ok_or(TreeError::Invalid(
                "recovery target lifecycle record is absent",
            ))?,
        )
    } else {
        None
    };
    let mut base_provenance = None;
    let mut base_deleted = false;
    let base_record = if fields.operation == GraphOperation::CypherEdit {
        let ExpectedGraphState::Entity(expected) = fields.expected else {
            return Err(TreeError::Invalid("recovery Cypher base state"));
        };
        if expected != fields.incarnation {
            return Err(TreeError::Invalid("recovery Cypher incarnation"));
        }
        let record = live_record(source, catalog, document, base_roots, expected, resources)?
            .ok_or(TreeError::Invalid("recovery Cypher base record is absent"))?;
        base_provenance = Some(
            OperationProvenance::from_fields(
                Some(1),
                record.provenance().fields_with_key(fields.key, resources)?,
            )
            .map_err(|_| TreeError::Invalid("recovery Cypher base provenance"))?,
        );
        Some(record)
    } else {
        let key = fields
            .key
            .ok_or(TreeError::Invalid("recovery structured key is absent"))?;
        let namespace =
            match catalog.lookup_symbol(SymbolKind::Namespace, key.namespace(), resources)? {
                Some(Symbol::Namespace(namespace)) => namespace,
                _ => return Err(TreeError::Invalid("recovery base key namespace is absent")),
            };
        let root = base_roots.directory(TreeKind::KeyFences)?;
        let fence_key = FenceKey::new(key.kind(), namespace, key.key().as_str())?;
        let fence = lookup_fence_entry(source, root, fence_key, resources)?
            .map(|entry| verify_fence_entry(source, root, entry, catalog, document, resources))
            .transpose()?;
        match fence {
            None => None,
            Some(fence) => {
                base_deleted = fence.is_deleted();
                base_provenance = Some(
                    OperationProvenance::from_fields(
                        Some(1),
                        fence.provenance().fields_with_key(Some(key), resources)?,
                    )
                    .map_err(|_| TreeError::Invalid("recovery structured base provenance"))?,
                );
                if fence.is_deleted() {
                    None
                } else {
                    let record = live_record(
                        source,
                        catalog,
                        document,
                        base_roots,
                        fence.incarnation(),
                        resources,
                    )?;
                    let Some(record) = record else {
                        if mutation.live || fields.operation != GraphOperation::StructuredDelete {
                            return Err(TreeError::Invalid("recovery live fence record is absent"));
                        }
                        crate::property_graph::storage::records::swept_delete(
                            source, base_roots, fields, namespace, catalog, document, resources,
                        )?;
                        // The classifier below consumes this original canonical stream.
                        return validate_swept_lifecycle(
                            source,
                            catalog,
                            document,
                            base_roots,
                            target_roots,
                            mutation,
                            &fence,
                            resources,
                        );
                    };
                    if !record
                        .canonical_bytes()
                        .compare(
                            fence.canonical_bytes().ok_or(TreeError::Invalid(
                                "recovery live fence canonical is absent",
                            ))?,
                            resources,
                        )?
                        .is_eq()
                        || !stored_provenance_equal(
                            record.provenance(),
                            fence.provenance(),
                            resources,
                        )?
                    {
                        return Err(TreeError::Invalid("recovery live fence/entity mismatch"));
                    }
                    Some(record)
                }
            }
        }
    };

    let base_fingerprint = base_record
        .as_ref()
        .map(|record| streamed_fingerprint(record.canonical_bytes(), resources))
        .transpose()?;
    let target_fingerprint = target_record
        .as_ref()
        .map(|record| streamed_fingerprint(record.canonical_bytes(), resources))
        .transpose()?;
    let base_shape = base_record
        .as_ref()
        .map(|record| recovery_entity_shape(record.shape(), catalog, resources))
        .transpose()?;
    let target_shape = target_record
        .as_ref()
        .map(|record| recovery_entity_shape(record.shape(), catalog, resources))
        .transpose()?;

    if fields.operation == GraphOperation::CypherEdit
        && mutation.live
        && fields.delete_mode.is_none()
        && let (Some(before), Some(after)) = (&base_record, &target_record)
        && before.document_version() != after.document_version()
        && after.document_version().is_some()
        && before.shape() == after.shape()
        && before
            .canonical_bytes()
            .compare(after.canonical_bytes(), resources)?
            .is_eq()
    {
        if fields.installed_revision.get()
            != before
                .revision()
                .get()
                .checked_add(1)
                .ok_or(TreeError::Invalid("document binding revision overflow"))?
        {
            return Err(TreeError::Invalid("document binding revision transition"));
        }
        return Ok(());
    }

    let first_error = Cell::new(None);
    let resources_cell = RefCell::new(resources);
    let mut base_reader = base_record.as_ref().map(|record| RecoveryPayloadReader {
        slice: record.canonical_bytes(),
        offset: 0,
        resources: &resources_cell,
        first_error: &first_error,
    });
    let mut target_reader = target_record.as_ref().map(|record| RecoveryPayloadReader {
        slice: record.canonical_bytes(),
        offset: 0,
        resources: &resources_cell,
        first_error: &first_error,
    });
    let mut scratch = [0_u8; 4096];
    let mut checkpoint = || recovery_checkpoint(&resources_cell, &first_error);
    let decision = if fields.operation == GraphOperation::CypherEdit {
        let current = CurrentEntity {
            provenance: base_provenance
                .ok_or(TreeError::Invalid("recovery Cypher provenance is absent"))?,
            contents: CanonicalRecord::from_validated(
                base_shape.ok_or(TreeError::Invalid("recovery Cypher shape is absent"))?,
                base_fingerprint
                    .ok_or(TreeError::Invalid("recovery Cypher fingerprint is absent"))?,
                base_reader
                    .as_mut()
                    .ok_or(TreeError::Invalid("recovery Cypher source is absent"))?,
            ),
        };
        let edit = match (mutation.live, fields.delete_mode) {
            (true, None) => CypherEdit::Put(CanonicalRecord::from_validated(
                target_shape.ok_or(TreeError::Invalid("recovery Cypher target shape"))?,
                target_fingerprint
                    .ok_or(TreeError::Invalid("recovery Cypher target fingerprint"))?,
                target_reader
                    .as_mut()
                    .ok_or(TreeError::Invalid("recovery Cypher target source"))?,
            )),
            (false, Some(mode)) => CypherEdit::Delete(mode),
            _ => return Err(TreeError::Invalid("recovery Cypher target operation")),
        };
        classify_cypher(Some(current), edit, &mut scratch, &mut checkpoint)
            .map_err(|error| lifecycle_tree_error(&first_error, error))?
    } else {
        let key = fields
            .key
            .ok_or(TreeError::Invalid("recovery structured key is absent"))?;
        let state = match (base_provenance, base_deleted, base_reader.as_mut()) {
            (None, false, None) => KeyState::NeverUsed,
            (Some(provenance), true, None) => KeyState::Deleted(provenance),
            (Some(provenance), false, Some(reader)) => KeyState::Live(CurrentEntity {
                provenance,
                contents: CanonicalRecord::from_validated(
                    base_shape.ok_or(TreeError::Invalid("recovery structured base shape"))?,
                    base_fingerprint
                        .ok_or(TreeError::Invalid("recovery structured base fingerprint"))?,
                    reader,
                ),
            }),
            _ => return Err(TreeError::Invalid("recovery structured base state")),
        };
        let request = match fields.operation {
            GraphOperation::StructuredCreate => KeyRequest::Create {
                revision: fields.requested_revision,
                contents: CanonicalRecord::from_validated(
                    target_shape.ok_or(TreeError::Invalid("recovery structured target shape"))?,
                    target_fingerprint
                        .ok_or(TreeError::Invalid("recovery structured target fingerprint"))?,
                    target_reader
                        .as_mut()
                        .ok_or(TreeError::Invalid("recovery structured create source"))?,
                ),
            },
            GraphOperation::StructuredPut => KeyRequest::Put {
                revision: fields.requested_revision,
                expected: match fields.expected {
                    ExpectedGraphState::Entity(expected) => expected,
                    _ => return Err(TreeError::Invalid("recovery structured put precondition")),
                },
                contents: CanonicalRecord::from_validated(
                    target_shape.ok_or(TreeError::Invalid("recovery structured target shape"))?,
                    target_fingerprint
                        .ok_or(TreeError::Invalid("recovery structured target fingerprint"))?,
                    target_reader
                        .as_mut()
                        .ok_or(TreeError::Invalid("recovery structured put source"))?,
                ),
            },
            GraphOperation::StructuredDelete => KeyRequest::Delete {
                revision: fields.requested_revision,
                expected: match fields.expected {
                    ExpectedGraphState::Entity(expected) => expected,
                    _ => {
                        return Err(TreeError::Invalid(
                            "recovery structured delete precondition",
                        ));
                    }
                },
                mode: fields
                    .delete_mode
                    .ok_or(TreeError::Invalid("recovery structured delete mode"))?,
            },
            GraphOperation::StructuredRecreate => KeyRequest::Recreate {
                revision: fields.requested_revision,
                deleted_revision: match fields.expected {
                    ExpectedGraphState::Deletion(revision) => revision,
                    _ => {
                        return Err(TreeError::Invalid(
                            "recovery structured recreate precondition",
                        ));
                    }
                },
                contents: CanonicalRecord::from_validated(
                    target_shape.ok_or(TreeError::Invalid("recovery structured target shape"))?,
                    target_fingerprint
                        .ok_or(TreeError::Invalid("recovery structured target fingerprint"))?,
                    target_reader
                        .as_mut()
                        .ok_or(TreeError::Invalid("recovery structured recreate source"))?,
                ),
            },
            GraphOperation::CypherEdit => {
                return Err(TreeError::Invalid("recovery structured operation"));
            }
        };
        classify_key(key, state, request, &mut scratch, &mut checkpoint)
            .map_err(|error| lifecycle_tree_error(&first_error, error))?
    };
    let KeyDecision::Change(change) = decision else {
        return Err(TreeError::Invalid(
            "recovery mutation is not a lifecycle change",
        ));
    };
    let installed = change
        .install(
            fields.incarnation,
            target_roots.generation(),
            &mut checkpoint,
        )
        .map_err(|error| lifecycle_tree_error(&first_error, error))?;
    if installed.fields() != fields {
        return Err(TreeError::Invalid("recovery lifecycle provenance outcome"));
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "recovery mutation validation binds exact complete base and target owners"
)]
fn validate_mutation_state(
    source: &RecoverySource<'_, '_>,
    catalog: &RecoveryCatalog<'_, '_>,
    memory: &StorageMemory<'_>,
    document: Option<&EmbeddingTower>,
    base_roots: GraphRoots,
    target_roots: GraphRoots,
    base_high_waters: crate::property_graph::wal::HighWaters,
    base_sequence: u64,
    target_sequence: u64,
    mutation: Mutation<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    let fields = mutation.provenance;
    if !matches!(mutation.provenance_version, 1 | 2)
        || fields.requested_revision != fields.installed_revision
        || fields.original_generation != target_roots.generation()
        || target_sequence != base_sequence.checked_add(1).ok_or(TreeError::Work)?
    {
        return Err(TreeError::Invalid("recovery mutation provenance domain"));
    }
    validate_lifecycle_transition(
        source,
        catalog,
        document,
        base_roots,
        target_roots,
        base_high_waters,
        mutation,
        resources,
    )?;
    let canonical = mutation
        .canonical
        .map(|required| canonical_payload(source, required, resources))
        .transpose()?
        .map(|payload| {
            PayloadSlice::new(
                source,
                target_roots.store(),
                target_roots.generation(),
                payload,
            )
        });
    match fields.incarnation {
        crate::property_graph::EntityId::Node(node) => {
            let root = target_roots.directory(TreeKind::Nodes)?;
            let entry = lookup_entry(source, root, &node.get().to_le_bytes(), resources)?
                .ok_or(TreeError::Invalid("recovery target node is absent"))?;
            let payload = PayloadRef::decode(entry.value())?;
            let state = verify_node_state(
                PayloadSlice::new(
                    source,
                    target_roots.store(),
                    entry.creation_generation(),
                    payload,
                ),
                node,
                catalog,
                document,
                resources,
            )?;
            let bound = match &state {
                NodeRecordState::Live(record) => record.document_version(),
                NodeRecordState::Tombstone(record) => record.document_version(),
            };
            if bound.is_some() != (mutation.provenance_version == 2) {
                return Err(TreeError::Invalid("recovery document mutation version"));
            }
            match state {
                NodeRecordState::Live(record) => {
                    if record.revision() != fields.installed_revision
                        || record.provenance().fields_with_key(fields.key, resources)? != fields
                        || !mutation.live
                        || canonical.is_none()
                        || !record
                            .canonical_bytes()
                            .compare(
                                canonical
                                    .ok_or(TreeError::Invalid("recovery node canonical outcome"))?,
                                resources,
                            )?
                            .is_eq()
                    {
                        return Err(TreeError::Invalid("recovery node mutation outcome"));
                    }
                }
                NodeRecordState::Tombstone(tombstone) => {
                    if tombstone.revision() != fields.installed_revision
                        || tombstone
                            .provenance()
                            .fields_with_key(fields.key, resources)?
                            != fields
                        || mutation.live
                        || canonical.is_some()
                    {
                        return Err(TreeError::Invalid("recovery node tombstone outcome"));
                    }
                }
            }
        }
        crate::property_graph::EntityId::Relationship(rel) => {
            if mutation.live {
                let root = target_roots.directory(TreeKind::Relationships)?;
                let entry = lookup_entry(source, root, &rel.get().to_le_bytes(), resources)?
                    .ok_or(TreeError::Invalid("recovery target relationship is absent"))?;
                let payload = PayloadRef::decode(entry.value())?;
                let record = verify_record(
                    PayloadSlice::new(
                        source,
                        target_roots.store(),
                        entry.creation_generation(),
                        payload,
                    ),
                    crate::property_graph::EntityId::Relationship(rel),
                    catalog,
                    document,
                    resources,
                )?;
                if record.revision() != fields.installed_revision
                    || record.provenance().fields_with_key(fields.key, resources)? != fields
                    || canonical.is_none()
                    || !record
                        .canonical_bytes()
                        .compare(
                            canonical.ok_or(TreeError::Invalid(
                                "recovery relationship canonical outcome",
                            ))?,
                            resources,
                        )?
                        .is_eq()
                {
                    return Err(TreeError::Invalid("recovery relationship mutation outcome"));
                }
                exact_relationship_membership(
                    source,
                    target_roots,
                    target_sequence,
                    authoritative_relationship(
                        source,
                        catalog,
                        document,
                        target_roots,
                        rel,
                        resources,
                    )?
                    .ok_or(TreeError::Invalid("recovery relationship is absent"))?,
                    true,
                    memory,
                    resources,
                )?;
            } else {
                let target_root = target_roots.directory(TreeKind::Relationships)?;
                if lookup_entry(source, target_root, &rel.get().to_le_bytes(), resources)?.is_some()
                    || canonical.is_some()
                {
                    return Err(TreeError::Invalid("recovery deleted relationship is live"));
                }
                let base_root = base_roots.directory(TreeKind::Relationships)?;
                let row = if let Some(base_entry) =
                    lookup_entry(source, base_root, &rel.get().to_le_bytes(), resources)?
                {
                    let base_payload = PayloadRef::decode(base_entry.value())?;
                    let base_record = verify_record(
                        PayloadSlice::new(
                            source,
                            base_roots.store(),
                            base_entry.creation_generation(),
                            base_payload,
                        ),
                        crate::property_graph::EntityId::Relationship(rel),
                        catalog,
                        document,
                        resources,
                    )?;
                    let crate::property_graph::storage::records::RecordShape::Relationship {
                        id,
                        source: relationship_source,
                        target: relationship_target,
                        relationship_type,
                    } = base_record.shape()
                    else {
                        return Err(TreeError::Invalid("recovery relationship base role"));
                    };
                    crate::property_graph::storage::adjacency::RelationshipRow {
                        rel: id,
                        source: relationship_source,
                        target: relationship_target,
                        relationship_type,
                    }
                } else {
                    let key = fields
                        .key
                        .ok_or(TreeError::Invalid("swept Delete key absent"))?;
                    let Some(Symbol::Namespace(namespace)) =
                        catalog.lookup_symbol(SymbolKind::Namespace, key.namespace(), resources)?
                    else {
                        return Err(TreeError::Invalid("swept Delete namespace absent"));
                    };
                    crate::property_graph::storage::records::swept_delete(
                        source, base_roots, fields, namespace, catalog, document, resources,
                    )?
                };
                exact_relationship_membership(
                    source,
                    target_roots,
                    target_sequence,
                    row,
                    false,
                    memory,
                    resources,
                )?;
            }
        }
    }
    if let Some(key) = fields.key {
        let namespace =
            match catalog.lookup_symbol(SymbolKind::Namespace, key.namespace(), resources)? {
                Some(Symbol::Namespace(namespace)) => namespace,
                _ => return Err(TreeError::Invalid("recovery key namespace is absent")),
            };
        let root = target_roots.directory(TreeKind::KeyFences)?;
        let fence_key = FenceKey::new(key.kind(), namespace, key.key().as_str())?;
        let entry = lookup_fence_entry(source, root, fence_key, resources)?
            .ok_or(TreeError::Invalid("recovery key fence is absent"))?;
        let fence = verify_fence_entry(source, root, entry, catalog, document, resources)?;
        if fence.incarnation() != fields.incarnation
            || fence.revision() != fields.installed_revision
            || fence.provenance().fields_with_key(Some(key), resources)? != fields
            || fence.canonical_bytes().is_some() != mutation.live
        {
            return Err(TreeError::Invalid("recovery key fence outcome"));
        }
        if let (Some(stored), Some(expected)) = (fence.canonical_bytes(), canonical)
            && !stored.compare(expected, resources)?.is_eq()
        {
            return Err(TreeError::Invalid("recovery fence canonical outcome"));
        }
    }
    Ok(())
}

fn artifact_name(path: &Path) -> Option<ArtifactId> {
    let name = path.file_name()?.to_str()?;
    let digits = name.strip_prefix("graph-")?.strip_suffix(".zgraph")?;
    if digits.len() != 32 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    ArtifactId::new(u128::from_str_radix(digits, 16).ok()?).ok()
}

fn artifact_inventory_capacity(
    store: &dyn RecoveryRead,
    directory: &Path,
) -> Result<usize, NativeGraphError> {
    let mut count = 0_usize;
    store
        .vfs()
        .for_each_direct_child(directory, &mut |path| {
            if artifact_name(path).is_some() {
                count = count
                    .checked_add(1)
                    .ok_or_else(|| std::io::Error::other("native artifact count overflow"))?;
            }
            Ok(())
        })
        .map_err(|source| NativeGraphError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
    Ok(count.max(MAX_RECOVERED_DESCRIPTORS))
}

fn scan_creation_serials(
    store: &dyn RecoveryRead,
    resources: &GraphResources,
    directory: &Path,
    expected_store: crate::property_graph::StoreInstanceId,
    initial: u64,
    control: &QueryControl,
) -> Result<u64, NativeGraphError> {
    #[cfg(test)]
    SERIAL_PROBES.with(|count| count.set(0));
    let capacity = artifact_inventory_capacity(store, directory)?
        .checked_mul(2)
        .ok_or(NativeGraphError::Invalid("recovery serial scan capacity"))?;
    let mut maximum = initial;
    let mut first_error = None;
    let _serial_charge = resources.reserve(
        capacity
            .checked_mul(std::mem::size_of::<OnceCell<ArtifactId>>())
            .ok_or(NativeGraphError::Invalid("recovery serial scan capacity"))?,
    )?;
    let mut serials = Vec::new();
    serials
        .try_reserve_exact(capacity)
        .map_err(|_| NativeGraphError::Invalid("recovery serial scan allocation"))?;
    serials.resize_with(capacity, OnceCell::new);
    store
        .vfs()
        .for_each_direct_child(directory, &mut |path| {
            if first_error.is_some() {
                return Ok(());
            }
            let result = (|| {
                control
                    .checkpoint()
                    .map_err(crate::property_graph::storage::tree::directory::TreeError::Control)
                    .map_err(NativeGraphError::Read)?;
                let Some(artifact) = artifact_name(path) else {
                    return Ok(());
                };
                let length = store
                    .vfs()
                    .open(path)
                    .map_err(|source| NativeGraphError::Io {
                        path: path.to_path_buf(),
                        source,
                    })?;
                if length < artifact::HEADER_BYTES as u64 {
                    return Ok(());
                }
                let header = store
                    .vfs()
                    .read_range(path, 0, artifact::HEADER_BYTES)
                    .map_err(|source| NativeGraphError::Io {
                        path: path.to_path_buf(),
                        source,
                    })?;
                let field = |range: std::ops::Range<usize>| {
                    header.get(range).ok_or(NativeGraphError::Invalid(
                        "truncated native artifact header",
                    ))
                };
                let family = u16::from_le_bytes(
                    field(8..10)?
                        .try_into()
                        .map_err(|_| NativeGraphError::Invalid("native artifact family"))?,
                );
                let declared = u64::from_le_bytes(
                    field(24..32)?
                        .try_into()
                        .map_err(|_| NativeGraphError::Invalid("native artifact length"))?,
                );
                let header_store = u128::from_le_bytes(
                    field(32..48)?
                        .try_into()
                        .map_err(|_| NativeGraphError::Invalid("native artifact store"))?,
                );
                let header_artifact = u128::from_le_bytes(
                    field(48..64)?
                        .try_into()
                        .map_err(|_| NativeGraphError::Invalid("native artifact identity"))?,
                );
                let serial = u64::from_le_bytes(
                    field(88..96)?
                        .try_into()
                        .map_err(|_| NativeGraphError::Invalid("native artifact serial"))?,
                );
                let header_length = u64::from_le_bytes(
                    field(16..24)?
                        .try_into()
                        .map_err(|_| NativeGraphError::Invalid("native artifact header length"))?,
                );
                // An uncommitted preparation may use any store-assigned future
                // generation. Recovery still validates its header and complete
                // bytes; the existing reachability rules retain cleanup authority.
                if field(0..8)? != b"ZEPEMBED"
                    || family != 17
                    || field(10..12)? != 1_u16.to_le_bytes()
                    || field(12..16)? != [0_u8; 4]
                    || header_length != artifact::HEADER_BYTES as u64
                    || header_store != expected_store.get()
                    || header_artifact != artifact.get()
                    || serial == 0
                    || declared < (artifact::HEADER_BYTES + 8) as u64
                    || declared > MAX_ARTIFACT_BYTES as u64
                {
                    return Err(NativeGraphError::Invalid(
                        "corrupt recognized native artifact header",
                    ));
                }
                let serial_key = ArtifactId::new(u128::from(serial) + 1)
                    .map_err(|_| NativeGraphError::Invalid("recovery creation serial key"))?;
                let slot = crate::property_graph::storage::mapping_slot(
                    &serials,
                    serial_key,
                    |value| *value,
                    || {
                        #[cfg(test)]
                        SERIAL_PROBES.with(|count| count.set(count.get() + 1));
                        control.checkpoint().map_err(TreeError::Control)
                    },
                )?
                .ok_or(NativeGraphError::Invalid("recovery serial scan capacity"))?;
                slot.set(serial_key).map_err(|_| {
                    NativeGraphError::Invalid("duplicate native artifact creation serial")
                })?;
                if length < declared {
                    maximum = maximum.max(serial);
                    return Ok(());
                }
                if length != declared {
                    return Err(NativeGraphError::Invalid(
                        "ambiguous recognized native artifact extent",
                    ));
                }
                let mapping = map_file(store, path, MAX_ARTIFACT_BYTES)?;
                let container = if family == crate::format::FormatFamily::NativeGraphObject.id() {
                    ContainerKind::Object
                } else {
                    ContainerKind::RootEnvelope
                };
                let frame = reopen_controlled_validation!(
                    container,
                    Some((expected_store, artifact)),
                    mapping.as_bytes(),
                    &mut |_| {
                        control.checkpoint().map_err(
                            crate::property_graph::storage::tree::directory::TreeError::Control,
                        )
                    },
                )
                .map_err(|error| match error {
                    ArtifactControlError::Format(_) => {
                        NativeGraphError::Invalid("corrupt recognized native artifact")
                    }
                    ArtifactControlError::Control(error) => NativeGraphError::Read(error),
                })?;
                let identity = frame.identity();
                if identity.store != expected_store || identity.artifact != artifact {
                    return Err(NativeGraphError::Invalid(
                        "recognized native artifact identity mismatch",
                    ));
                }
                maximum = maximum.max(identity.creation_serial);
                Ok(())
            })();
            if let Err(error) = result {
                first_error = Some(error);
            }
            Ok(())
        })
        .map_err(|source| NativeGraphError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
    if let Some(error) = first_error {
        return Err(error);
    }
    if maximum == u64::MAX {
        return Err(NativeGraphError::IdentityExhausted);
    }
    Ok(maximum)
}

#[derive(Clone, Copy)]
struct CatalogWaters {
    node: u128,
    relationship: u128,
    symbols: SymbolHighWaters,
}

#[derive(Clone, Copy)]
struct PendingWalIntent {
    id: crate::property_graph::wal::BatchId,
    capture_generation: crate::property_graph::GraphGeneration,
    capture_sequence: u64,
    serial_fence: u64,
    protected_roots: RequiredRef,
    protected_digest: u64,
    completed_mark: RequiredRef,
    mark_digest: u64,
}

#[derive(Clone, Copy)]
struct PendingWalCompletion {
    id: crate::property_graph::wal::BatchId,
    intent: RequiredRef,
    completed_count: usize,
}

struct SemanticReplay<'a, 'm> {
    store: &'a dyn RecoveryRead,
    directory: &'a Path,
    expected: GraphInterpretation<'a>,
    document: Option<&'a EmbeddingTower>,
    control: &'a QueryControl,
    artifact_capacity: usize,
    resources: &'a GraphResources,
    memory: &'m StorageMemory<'m>,
    protected: Vec<ArtifactDescriptor>,
    checkpoint_allocations: Vec<ArtifactDescriptor>,
    reclaim_inventory: Vec<InventoryChange>,
    reclaim_candidates: Vec<ArtifactDescriptor>,
    reclaim_remaining: Vec<ArtifactDescriptor>,
    pending_intent: Option<PendingWalIntent>,
    pending_completion: Option<PendingWalCompletion>,
    active_reclaim: Option<crate::property_graph::storage::reclaim::PendingIntentManifest>,
    active_intent_ref: Option<RequiredRef>,
    reclaim_completed: bool,
    inventory_start: usize,
    _charge: GraphReservation,
    catalog: Option<CatalogWaters>,
    first_error: Option<NativeGraphError>,
}

impl<'a, 'm> SemanticReplay<'a, 'm> {
    #[allow(
        clippy::too_many_arguments,
        reason = "independent resource owners and lifetimes are explicit at this private seam"
    )]
    fn new(
        store: &'a dyn RecoveryRead,
        directory: &'a Path,
        expected: GraphInterpretation<'a>,
        document: Option<&'a EmbeddingTower>,
        control: &'a QueryControl,
        artifact_capacity: usize,
        resources: &'a GraphResources,
        memory: &'m StorageMemory<'m>,
    ) -> Result<Self, NativeGraphError> {
        let descriptor_slots = artifact_capacity
            .checked_mul(2)
            .and_then(|slots| {
                slots.checked_add(crate::property_graph::storage::reclaim::MAX_CANDIDATES * 3)
            })
            .ok_or(NativeGraphError::Invalid("recovery descriptor capacity"))?;
        let bytes = descriptor_slots
            .checked_mul(std::mem::size_of::<ArtifactDescriptor>())
            .ok_or(NativeGraphError::Invalid("recovery descriptor capacity"))?;
        let charge = resources.reserve(bytes)?;
        let mut protected = Vec::new();
        protected
            .try_reserve_exact(artifact_capacity)
            .map_err(|_| NativeGraphError::Invalid("recovery descriptor allocation"))?;
        let mut checkpoint_allocations = Vec::new();
        checkpoint_allocations
            .try_reserve_exact(artifact_capacity)
            .map_err(|_| NativeGraphError::Invalid("recovery descriptor allocation"))?;
        let mut reclaim_inventory = Vec::new();
        reclaim_inventory
            .try_reserve_exact(crate::property_graph::storage::reclaim::MAX_CANDIDATES)
            .map_err(|_| NativeGraphError::Invalid("recovery descriptor allocation"))?;
        let mut reclaim_candidates = Vec::new();
        reclaim_candidates
            .try_reserve_exact(crate::property_graph::storage::reclaim::MAX_CANDIDATES)
            .map_err(|_| NativeGraphError::Invalid("recovery descriptor allocation"))?;
        let mut reclaim_remaining = Vec::new();
        reclaim_remaining
            .try_reserve_exact(crate::property_graph::storage::reclaim::MAX_CANDIDATES)
            .map_err(|_| NativeGraphError::Invalid("recovery descriptor allocation"))?;
        Ok(Self {
            store,
            directory,
            expected,
            document,
            control,
            artifact_capacity,
            resources,
            memory,
            protected,
            checkpoint_allocations,
            reclaim_inventory,
            reclaim_candidates,
            reclaim_remaining,
            pending_intent: None,
            pending_completion: None,
            active_reclaim: None,
            active_intent_ref: None,
            reclaim_completed: false,
            inventory_start: 0,
            _charge: charge,
            catalog: None,
            first_error: None,
        })
    }

    fn fail(&mut self, error: NativeGraphError, reported: WalError) -> WalError {
        if self.first_error.is_none() {
            self.first_error = Some(error);
        }
        reported
    }

    fn frame(&mut self, reference: RequiredRef) -> Result<NativeReadonlyMapping, WalError> {
        let path = crate::property_graph::storage::allocation::artifact_path(
            self.directory,
            reference.object.artifact,
        );
        match map_file(self.store, &path, MAX_ARTIFACT_BYTES) {
            Ok(mapping) => Ok(mapping),
            Err(error) => Err(self.fail(error, WalError::MissingArtifact)),
        }
    }

    fn validate_reference(
        &mut self,
        reference: RequiredRef,
        role: RequiredRole,
        resources: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        let mapping = self.frame(reference)?;
        let frame = reopen_controlled_validation!(
            ContainerKind::Object,
            Some((reference.object.store, reference.object.artifact)),
            mapping.as_bytes(),
            &mut |bytes| resources.charge(bytes as u64),
        )
        .map_err(|error| match error {
            artifact::ArtifactControlError::Format(_) => WalError::Malformed,
            artifact::ArtifactControlError::Control(error) => error,
        })?;
        let payload = validate_required_block(reference, role, &frame, resources)?;
        if role == RequiredRole::Participant(ParticipantRole::Catalog) {
            let count = usize::try_from(
                payload
                    .get(104..112)
                    .and_then(|bytes| bytes.first_chunk::<8>())
                    .copied()
                    .map(u64::from_le_bytes)
                    .ok_or(WalError::Malformed)?,
            )
            .map_err(|_| WalError::Capacity)?;
            let allowance = count
                .checked_mul(std::mem::size_of::<SymbolEntry<'_>>())
                .ok_or(WalError::Capacity)?;
            let descriptor_charge = self
                .resources
                .reserve(allowance)
                .map_err(|error| self.fail(NativeGraphError::Store(error), WalError::Capacity))?;
            let image = CatalogImage::decode(payload, allowance, &mut || {
                resources.charge(1).map_err(|_| CatalogError::Cancelled)
            })
            .map_err(|error| match error {
                CatalogError::Cancelled => WalError::Cancelled,
                CatalogError::Capacity | CatalogError::Allocation => WalError::Capacity,
                _ => WalError::Participant,
            })?;
            image
                .declaration
                .validate_for(reference.object.store, self.expected, &mut || {
                    resources.charge(1).map_err(|_| CatalogError::Cancelled)
                })
                .map_err(|error| match error {
                    CatalogError::Cancelled => WalError::Cancelled,
                    _ => WalError::Participant,
                })?;
            self.catalog = Some(CatalogWaters {
                node: image.declaration.node_high_water,
                relationship: image.declaration.relationship_high_water,
                symbols: image.symbols.high_waters(),
            });
            drop(image);
            drop(descriptor_charge);
        }
        Ok(())
    }

    fn validate_pending_reclaim(
        &mut self,
        source: &RecoverySource<'_, 'm>,
        state: CommitState<'_>,
        authentic_base: Option<CommitState<'_>>,
        intent_ref: RequiredRef,
        expected: Option<PendingWalIntent>,
        resources: &mut TreeResources<'_>,
    ) -> Result<crate::property_graph::storage::reclaim::PendingIntentManifest, TreeError> {
        let capacity = intent_ref.block.length as usize;
        let mut payload = StorageBuffer::new(self.memory, capacity)?;
        for _ in 0..capacity {
            payload.push(0)?;
        }
        let length = source.copy_required_payload(intent_ref, payload.as_mut_slice(), resources)?;
        let bytes = payload
            .as_slice()
            .get(..length)
            .ok_or(TreeError::Invalid("recovery reclaim intent extent"))?;
        let manifest =
            crate::property_graph::storage::reclaim::decode_pending_intent_manifest(bytes)?;
        if manifest.binding.store != state.store
            || manifest.binding.target_generation != intent_ref.object.generation
            || manifest.binding.target_generation > state.generation
            || manifest.binding.capture_generation >= manifest.binding.target_generation
            || manifest.binding.serial_fence > state.high_waters.creation_serial
            || manifest
                .candidate_count
                .checked_add(manifest.partial_count)
                .is_none_or(|rows| {
                    rows == 0 || rows > crate::property_graph::storage::reclaim::MAX_CANDIDATES
                })
        {
            return Err(TreeError::Invalid("recovery reclaim intent binding"));
        }
        if let Some(expected) = expected
            && (expected.id.get() != manifest.binding.session.get()
                || expected.capture_generation != manifest.binding.capture_generation
                || expected.capture_sequence != manifest.binding.sequence
                || expected.serial_fence != manifest.binding.serial_fence
                || expected.protected_roots != manifest.protected.head
                || expected.protected_digest != manifest.protected.digest
                || expected.completed_mark != manifest.mark.root
                || expected.mark_digest != manifest.mark.digest
                || self.reclaim_candidates.len() != manifest.candidate_count)
        {
            return Err(TreeError::Invalid("recovery WAL reclaim intent mismatch"));
        }
        let mut encoded_candidates = StorageBuffer::new(self.memory, manifest.candidate_count)?;
        for index in 0..manifest.candidate_count {
            let candidate =
                crate::property_graph::storage::reclaim::pending_intent_candidate_at(bytes, index)?;
            if expected.is_some() && self.reclaim_candidates.get(index).copied() != Some(candidate)
            {
                return Err(TreeError::Invalid("recovery reclaim candidate mismatch"));
            }
            encoded_candidates.push(candidate)?;
        }
        let io = RecoverySpillIo { source };
        // The one authenticating walk of this manifest's mark. It proves the
        // run's count, digest, sorted-unique order and binding, and it proves
        // no candidate is marked live. Every membership question below is
        // then a bounded root-to-leaf descent over the same authenticated
        // run, so this walk must stay ahead of them.
        let mut authenticating = crate::property_graph::storage::reclaim::DurableRunReader::new(
            manifest.mark,
            self.memory,
        )?;
        // The tagged partial partition is validated separately: it is not an
        // object, so nothing above reaches it. Its domain, ordering and
        // disjointness from the candidates were proved by the decoder; this
        // walk proves the one remaining fact, that the mark never named it.
        let mut encoded_partials = StorageBuffer::new(self.memory, manifest.partial_count)?;
        for index in 0..manifest.partial_count {
            encoded_partials.push(
                crate::property_graph::storage::reclaim::pending_intent_partial_at(bytes, index)?,
            )?;
        }
        let mut candidate_index = 0_usize;
        let mut partial_index = 0_usize;
        while let Some(live) = authenticating.next(&io, resources)? {
            while encoded_candidates
                .as_slice()
                .get(candidate_index)
                .is_some_and(|candidate| candidate.artifact < live)
            {
                candidate_index += 1;
            }
            if encoded_candidates
                .as_slice()
                .get(candidate_index)
                .is_some_and(|candidate| candidate.artifact == live)
            {
                return Err(TreeError::Invalid(
                    "recovery reclaim candidate is marked live",
                ));
            }
            while encoded_partials
                .as_slice()
                .get(partial_index)
                .is_some_and(|target| target.artifact < live)
            {
                partial_index += 1;
            }
            if encoded_partials
                .as_slice()
                .get(partial_index)
                .is_some_and(|target| target.artifact == live)
            {
                return Err(TreeError::Invalid(
                    "recovery reclaim partial target is marked live",
                ));
            }
        }
        drop(authenticating);
        let mut mark_reader = crate::property_graph::storage::reclaim::DurableRunReader::new(
            manifest.mark,
            self.memory,
        )?;
        validate_complete_reclaim_authority(
            self.store,
            self.directory,
            state,
            authentic_base,
            self.expected,
            self.document,
            self.control,
            manifest,
            source,
            self.memory,
            &mut mark_reader,
            resources,
            encoded_candidates.as_slice(),
        )?;
        self.reclaim_candidates.clear();
        self.reclaim_candidates
            .extend_from_slice(encoded_candidates.as_slice());
        Ok(manifest)
    }

    fn validate_reclaim_inventory(
        &self,
        id: crate::property_graph::wal::BatchId,
        candidates: &[ArtifactDescriptor],
        state: InventoryState,
    ) -> Result<(), TreeError> {
        if self.reclaim_inventory.len() != candidates.iter().filter(|v| v.family == 17).count() {
            return Err(TreeError::Invalid("recovery reclaim inventory count"));
        }
        for candidate in candidates.iter().filter(|v| v.family == 17) {
            let expected_state = match state {
                InventoryState::ReclaimPending(_) => InventoryState::ReclaimPending(id),
                InventoryState::Reclaimed(_) => InventoryState::Reclaimed(id),
                _ => return Err(TreeError::Invalid("recovery reclaim inventory role")),
            };
            if !self
                .reclaim_inventory
                .iter()
                .any(|change| change.object == *candidate && change.state == expected_state)
            {
                return Err(TreeError::Invalid("recovery reclaim inventory mismatch"));
            }
        }
        Ok(())
    }

    fn validate_completion_reclaim(
        &mut self,
        source: &RecoverySource<'_, 'm>,
        state: CommitState<'_>,
        completion_ref: RequiredRef,
        expected: Option<PendingWalCompletion>,
        resources: &mut TreeResources<'_>,
    ) -> Result<
        (
            crate::property_graph::storage::reclaim::PendingIntentManifest,
            RequiredRef,
        ),
        TreeError,
    > {
        let capacity = completion_ref.block.length as usize;
        let mut payload = StorageBuffer::new(self.memory, capacity)?;
        for _ in 0..capacity {
            payload.push(0)?;
        }
        let length =
            source.copy_required_payload(completion_ref, payload.as_mut_slice(), resources)?;
        let bytes = payload
            .as_slice()
            .get(..length)
            .ok_or(TreeError::Invalid("recovery reclaim completion extent"))?;
        let completion =
            crate::property_graph::storage::reclaim::decode_completed_intent_manifest(bytes)?;
        let total = completion
            .completed_count
            .checked_add(completion.remaining_count)
            .ok_or(TreeError::Memory)?;
        if completion.binding.store != state.store
            || completion.binding.target_generation >= completion_ref.object.generation
            || completion_ref.object.generation > state.generation
            || total
                .checked_add(completion.partial_count)
                .is_none_or(|rows| {
                    rows == 0 || rows > crate::property_graph::storage::reclaim::MAX_CANDIDATES
                })
        {
            return Err(TreeError::Invalid("recovery reclaim completion binding"));
        }
        if let Some(expected) = expected
            && (expected.id.get() != completion.binding.session.get()
                || expected.intent != completion.intent
                || expected.completed_count != completion.completed_count
                || self.reclaim_candidates.len() != completion.completed_count
                || self.reclaim_remaining.len() != completion.remaining_count)
        {
            return Err(TreeError::Invalid(
                "recovery WAL reclaim completion mismatch",
            ));
        }
        let mut completed = StorageBuffer::new(self.memory, completion.completed_count)?;
        let mut remaining = StorageBuffer::new(self.memory, completion.remaining_count)?;
        for index in 0..total {
            let candidate = crate::property_graph::storage::reclaim::completed_intent_candidate_at(
                bytes, index,
            )?;
            if index < completion.completed_count {
                if expected.is_some()
                    && self.reclaim_candidates.get(index).copied() != Some(candidate)
                {
                    return Err(TreeError::Invalid("recovery completed candidate mismatch"));
                }
                completed.push(candidate)?;
            } else {
                let remaining_index = index - completion.completed_count;
                if expected.is_some()
                    && self.reclaim_remaining.get(remaining_index).copied() != Some(candidate)
                {
                    return Err(TreeError::Invalid("recovery remaining candidate mismatch"));
                }
                remaining.push(candidate)?;
            }
        }
        let intent =
            self.validate_pending_reclaim(source, state, None, completion.intent, None, resources)?;
        if intent.binding != completion.binding
            || intent.candidate_count != total
            || intent.partial_count != completion.partial_count
        {
            return Err(TreeError::Invalid(
                "recovery completion original intent mismatch",
            ));
        }
        for candidate in &self.reclaim_candidates {
            let in_completed = completed
                .as_slice()
                .binary_search_by_key(&candidate.artifact, |value| value.artifact)
                .is_ok();
            let in_remaining = remaining
                .as_slice()
                .binary_search_by_key(&candidate.artifact, |value| value.artifact)
                .is_ok();
            if in_completed == in_remaining {
                return Err(TreeError::Invalid("recovery completion partition"));
            }
        }
        let id = crate::property_graph::wal::BatchId::new(completion.binding.session.get())
            .map_err(|_| TreeError::Invalid("recovery completion intent identity"))?;
        if expected.is_some() {
            self.validate_reclaim_inventory(
                id,
                completed.as_slice(),
                InventoryState::Reclaimed(id),
            )?;
        }
        Ok((intent, completion.intent))
    }

    fn validate_state_reclaim(
        &mut self,
        source: &RecoverySource<'_, 'm>,
        state: CommitState<'_>,
        authentic_base: Option<CommitState<'_>>,
        reclaim: RequiredRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        let capacity = reclaim.block.length as usize;
        let mut payload = StorageBuffer::new(self.memory, capacity)?;
        for _ in 0..capacity {
            payload.push(0)?;
        }
        let length = source.copy_required_payload(reclaim, payload.as_mut_slice(), resources)?;
        let bytes = payload
            .as_slice()
            .get(..length)
            .ok_or(TreeError::Invalid("recovery reclaim state extent"))?;
        let subtype = u16::from_le_bytes(
            *bytes
                .get(8..10)
                .and_then(|value| value.first_chunk::<2>())
                .ok_or(TreeError::Invalid("recovery reclaim state subtype"))?,
        );
        match subtype {
            2 => {
                let expected = self.pending_intent;
                let manifest = self.validate_pending_reclaim(
                    source,
                    state,
                    authentic_base,
                    reclaim,
                    expected,
                    resources,
                )?;
                if let Some(intent) = expected {
                    self.validate_reclaim_inventory(
                        intent.id,
                        &self.reclaim_candidates,
                        InventoryState::ReclaimPending(intent.id),
                    )?;
                }
                self.active_reclaim = Some(manifest);
                self.active_intent_ref = Some(reclaim);
                self.reclaim_completed = false;
            }
            3 => {
                let expected = self.pending_completion;
                let (manifest, intent) =
                    self.validate_completion_reclaim(source, state, reclaim, expected, resources)?;
                self.active_reclaim = Some(manifest);
                self.active_intent_ref = Some(intent);
                self.reclaim_completed = true;
            }
            _ => return Err(TreeError::Invalid("recovery reclaim state subtype")),
        }
        Ok(())
    }

    fn validate_state(
        &mut self,
        state: CommitState<'_>,
        resources: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        for (root, tree) in state.graph.slots.into_iter().zip([
            TreeKind::Nodes,
            TreeKind::Relationships,
            TreeKind::KeyFences,
            TreeKind::Labels,
            TreeKind::RelationshipTypes,
            TreeKind::OutRanges,
            TreeKind::InRanges,
            TreeKind::ObjectInventory,
        ]) {
            if let Some(root) = root {
                self.validate_reference(root, RequiredRole::Tree(tree), resources)?;
            }
        }
        self.validate_reference(
            state.catalog,
            RequiredRole::Participant(ParticipantRole::Catalog),
            resources,
        )?;
        for root in [state.vector, state.text].into_iter().flatten() {
            self.validate_reference(
                root,
                RequiredRole::Participant(ParticipantRole::RetrievalState),
                resources,
            )?;
        }
        if let Some(reclaim) = state.reclaim {
            self.validate_reference(
                reclaim,
                RequiredRole::Participant(ParticipantRole::ReclaimState),
                resources,
            )?;
        }
        for index in 0..state.prepared_inventories.len()? {
            let root = state.prepared_inventories.get(index, resources)?;
            self.validate_reference(
                root,
                RequiredRole::Participant(ParticipantRole::PreparedInventory),
                resources,
            )?;
        }
        let catalog = self.catalog.ok_or(WalError::Participant)?;
        let [label, relationship_type, property, namespace] = state.high_waters.symbols;
        if catalog.node != state.high_waters.node
            || catalog.relationship != state.high_waters.relationship
            || catalog.symbols
                != (SymbolHighWaters {
                    label,
                    relationship_type,
                    property,
                    namespace,
                })
        {
            return Err(WalError::HighWater);
        }
        Ok(())
    }

    fn validate_checkpoint_state(
        &mut self,
        state: CommitState<'_>,
        resources: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        self.validate_state(state, resources)?;
        let source = RecoverySource::new(
            self.store,
            self.directory,
            state,
            self.memory,
            self.artifact_capacity,
        )
        .map_err(|error| self.fail(NativeGraphError::Read(error), WalError::Capacity))?;
        let mut tree = source
            .resources(resources.remaining())
            .map_err(|error| self.fail(NativeGraphError::Read(error), WalError::Participant))?;
        let result = (|| {
            if let Some(reclaim) = state.reclaim {
                self.validate_state_reclaim(&source, state, None, reclaim, &mut tree)?;
            } else {
                self.active_reclaim = None;
                self.active_intent_ref = None;
                self.reclaim_completed = false;
                self.reclaim_candidates.clear();
            }
            let catalog = RecoveryCatalog::open(
                &source,
                state.catalog,
                self.expected,
                state.high_waters,
                &mut tree,
            )?;
            let roots = GraphRoots::from_references(
                state.store,
                state.generation,
                state.graph.slots.map(|root| root.map(|value| value.block)),
            )?;
            let reclaim =
                self.active_reclaim
                    .map(|manifest| {
                        crate::property_graph::wal::BatchId::new(manifest.binding.session.get())
                            .map(|id| {
                                (
                                    id,
                                    self.reclaim_candidates.as_slice(),
                                    self.reclaim_completed,
                                )
                            })
                    })
                    .transpose()
                    .map_err(|_| TreeError::Invalid("checkpoint reclaim identity"))?;
            validate_native_checkpoint(
                &source,
                &catalog,
                self.document,
                self.memory,
                roots,
                state.sequence,
                state.high_waters,
                reclaim,
                &mut tree,
            )?;
            if state.sequence == 0 {
                if state.text.is_some() || state.vector.is_some() {
                    return Err(TreeError::Invalid("initial sparse checkpoint roots"));
                }
            } else {
                crate::property_graph::storage::search::validate_checkpoint(
                    &source,
                    crate::property_graph::storage::search::SparseCheckpoint {
                        cutoff: state.sequence,
                        roots: crate::property_graph::storage::search::SparseRoots {
                            text: state.text,
                            vector: state.vector,
                        },
                    },
                    roots,
                    state.catalog,
                    &catalog,
                    self.document,
                    self.store.lexical(),
                    self.memory,
                    &mut tree,
                )?;
            }
            let mut checkpoint_descriptors =
                StorageBuffer::<ArtifactDescriptor>::new(self.memory, self.artifact_capacity)?;
            let inventory_count = state.prepared_inventories.len().map_err(|error| {
                self.fail(NativeGraphError::Wal(error), error);
                TreeError::Invalid("checkpoint prepared inventory list")
            })?;
            for index in 0..inventory_count {
                let required =
                    state
                        .prepared_inventories
                        .get(index, resources)
                        .map_err(|error| {
                            self.fail(NativeGraphError::Wal(error), error);
                            TreeError::Invalid("checkpoint prepared inventory reference")
                        })?;
                let inventory = prepared_inventory(&source, required, state, &mut tree)?;
                for position in 0..inventory.count {
                    tree.step(1)?;
                    let descriptor = inventory.descriptor(position)?;
                    let duplicate = checkpoint_descriptors
                        .as_slice()
                        .iter()
                        .find(|previous| previous.artifact == descriptor.artifact);
                    if duplicate.is_some_and(|previous| *previous != descriptor)
                        || checkpoint_descriptors.as_slice().iter().any(|previous| {
                            previous.serial == descriptor.serial
                                && previous.artifact != descriptor.artifact
                        })
                    {
                        return Err(TreeError::Invalid(
                            "duplicate checkpoint prepared descriptor",
                        ));
                    }
                    if duplicate.is_some() {
                        continue;
                    }
                    checkpoint_descriptors.push(descriptor)?;
                }
                let duplicate = checkpoint_descriptors
                    .as_slice()
                    .iter()
                    .find(|previous| previous.artifact == required.object.artifact);
                if duplicate.is_some_and(|previous| *previous != required.object)
                    || checkpoint_descriptors.as_slice().iter().any(|previous| {
                        previous.serial == required.object.serial
                            && previous.artifact != required.object.artifact
                    })
                {
                    return Err(TreeError::Invalid(
                        "duplicate checkpoint prepared inventory",
                    ));
                }
                if duplicate.is_none() {
                    checkpoint_descriptors.push(required.object)?;
                }
            }
            checkpoint_descriptors
                .as_mut_slice()
                .sort_unstable_by_key(|descriptor| descriptor.artifact);
            self.checkpoint_allocations.clear();
            self.checkpoint_allocations
                .extend_from_slice(checkpoint_descriptors.as_slice());
            let inventory_root = roots.directory(TreeKind::ObjectInventory)?;
            let mut cursor = DirectoryCursor::seek(&source, inventory_root, None, &mut tree)?;
            while let Some(entry) = cursor.next_entry(&mut tree)? {
                let rooted = verify_inventory_entry(inventory_root, entry, &mut tree)?;
                if let InventoryState::ReclaimPending(id) | InventoryState::Reclaimed(id) =
                    rooted.state
                {
                    let manifest = self.active_reclaim.ok_or(TreeError::Invalid(
                        "checkpoint inventory proof state is absent",
                    ))?;
                    if id.get() != manifest.binding.session.get()
                        || !self.reclaim_candidates.contains(&rooted.object)
                        || (matches!(rooted.state, InventoryState::Reclaimed(_))
                            && !self.reclaim_completed)
                    {
                        return Err(TreeError::Invalid(
                            "checkpoint inventory proof state mismatch",
                        ));
                    }
                }
                if let Ok(index) = checkpoint_descriptors
                    .as_slice()
                    .binary_search_by_key(&rooted.object.artifact, |descriptor| descriptor.artifact)
                    && checkpoint_descriptors
                        .as_slice()
                        .get(index)
                        .is_some_and(|descriptor| *descriptor != rooted.object)
                {
                    return Err(TreeError::Invalid(
                        "checkpoint rooted/prepared descriptor contradiction",
                    ));
                }
                if !self
                    .checkpoint_allocations
                    .iter()
                    .any(|descriptor| descriptor.artifact == rooted.object.artifact)
                {
                    if self.checkpoint_allocations.len() == self.artifact_capacity {
                        return Err(TreeError::Memory);
                    }
                    self.checkpoint_allocations.push(rooted.object);
                }
                for descriptor in checkpoint_descriptors.as_slice() {
                    tree.step(1)?;
                    if descriptor.serial == rooted.object.serial
                        && descriptor.artifact != rooted.object.artifact
                    {
                        return Err(TreeError::Invalid(
                            "checkpoint rooted/prepared serial alias",
                        ));
                    }
                }
            }
            Ok::<(), TreeError>(())
        })();
        resources.charge(tree.work())?;
        if let Err(error) = result {
            if let Some(source_error) = source.take_source_error() {
                return Err(self.fail(source_error, WalError::Participant));
            }
            if let Some(source_error) = self.first_error.take() {
                return Err(self.fail(source_error, WalError::Participant));
            }
            return Err(self.fail(NativeGraphError::Read(error), WalError::Participant));
        }
        self.inventory_start = self.protected.len();
        Ok(())
    }

    /// True when the reclaim proof live at `state` names `descriptor` as one
    /// of its candidates, which is the only way an inventoried artifact may be
    /// absent from the directory.
    fn reclaim_allows_missing(
        &self,
        state: CommitState<'_>,
        descriptor: ArtifactDescriptor,
    ) -> bool {
        self.active_reclaim.is_some_and(|manifest| {
            manifest.binding.store == descriptor.store
                && manifest.binding.target_generation <= state.generation
        }) && self.reclaim_candidates.contains(&descriptor)
    }

    /// Proves every inventoried artifact exists exactly once, against the
    /// state this open publishes rather than against superseded history.
    ///
    /// The checkpoint's own allocations are collected during checkpoint
    /// validation and checked here, because a completed reclaim between the
    /// checkpoint and `state` legitimately unlinks artifacts the checkpoint
    /// still lists. Replayed envelopes carry the same superseded claim, so
    /// their inventories are proved here too, through `state`'s inventory:
    /// rows only leave it through a validated reclaim retirement. Its exact
    /// candidates discharge older checkpoint descriptors even if the trailing
    /// checkpoint is interrupted.
    fn validate_deferred_checkpoint_allocations(
        &mut self,
        state: CommitState<'_>,
        wal_resources: &mut WalResources<'_>,
    ) -> Result<(), NativeGraphError> {
        let source = RecoverySource::new(
            self.store,
            self.directory,
            state,
            self.memory,
            self.artifact_capacity,
        )?;
        let mut resources = source.resources(wal_resources.remaining())?;
        let roots = GraphRoots::from_references(
            state.store,
            state.generation,
            state.graph.slots.map(|root| root.map(|value| value.block)),
        )
        .map_err(NativeGraphError::Read)?;
        let rooted = (|| -> Result<(), TreeError> {
            let inventory_root = roots.directory(TreeKind::ObjectInventory)?;
            let mut cursor = DirectoryCursor::seek(&source, inventory_root, None, &mut resources)?;
            while let Some(entry) = cursor.next_entry(&mut resources)? {
                let change = verify_inventory_entry(inventory_root, entry, &mut resources)?;
                let allow_missing = self.reclaim_allows_missing(state, change.object);
                source.validate_descriptor_with_missing(
                    change.object,
                    allow_missing,
                    &mut resources,
                )?;
            }
            Ok(())
        })();
        if let Err(error) = rooted {
            if let Some(source_error) = source.take_source_error() {
                return Err(source_error);
            }
            return Err(NativeGraphError::Read(error));
        }
        for descriptor in &self.checkpoint_allocations {
            let allow_missing = self.reclaim_allows_missing(state, *descriptor);
            if let Err(error) =
                source.validate_descriptor_with_missing(*descriptor, allow_missing, &mut resources)
            {
                if let Some(source_error) = source.take_source_error() {
                    return Err(source_error);
                }
                return Err(NativeGraphError::Read(error));
            }
        }
        wal_resources.charge(resources.work())?;
        Ok(())
    }
}

impl ReplayValidator for SemanticReplay<'_, '_> {
    fn required(
        &mut self,
        reference: RequiredRef,
        role: RequiredRole,
        resources: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        self.validate_reference(reference, role, resources)
    }

    fn mutation(
        &mut self,
        mutation: crate::property_graph::wal::Mutation<'_>,
        resources: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        resources.charge(1)?;
        if mutation.live != mutation.canonical.is_some() {
            return Err(WalError::Participant);
        }
        Ok(())
    }

    fn inventory(
        &mut self,
        inventory: InventoryChange,
        resources: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        resources.charge(1)?;
        match inventory.state {
            InventoryState::Prepared | InventoryState::Retained => {
                if self.protected.len() == self.protected.capacity() {
                    return Err(WalError::Capacity);
                }
                if self
                    .protected
                    .get(self.inventory_start..)
                    .ok_or(WalError::Participant)?
                    .iter()
                    .any(|previous| {
                        previous.artifact == inventory.object.artifact
                            || previous.serial == inventory.object.serial
                    })
                {
                    return Err(WalError::Participant);
                }
                self.protected.push(inventory.object);
                Ok(())
            }
            InventoryState::ReclaimPending(_) | InventoryState::Reclaimed(_) => {
                if self.reclaim_inventory.len()
                    == crate::property_graph::storage::reclaim::MAX_CANDIDATES
                {
                    return Err(WalError::Capacity);
                }
                if self.reclaim_inventory.iter().any(|previous| {
                    previous.object.artifact == inventory.object.artifact
                        || previous.object.serial == inventory.object.serial
                }) {
                    return Err(WalError::Participant);
                }
                self.reclaim_inventory.push(inventory);
                Ok(())
            }
        }
    }

    fn reclaim_intent(
        &mut self,
        intent: ReclaimIntent<'_>,
        resources: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        if self.pending_intent.is_some() || self.pending_completion.is_some() {
            return Err(WalError::Participant);
        }
        // An empty list is legal: the intent may name only interrupted
        // creations, which are not objects and never enter this list.
        // `validate_pending_reclaim` reads the role-5 record's tagged partial
        // partition and rejects an intent that names nothing at all.
        let count = intent.candidates.len()?;
        if count > crate::property_graph::storage::reclaim::MAX_CANDIDATES {
            return Err(WalError::Capacity);
        }
        self.reclaim_candidates.clear();
        for index in 0..count {
            self.reclaim_candidates
                .push(intent.candidates.get(index, resources)?);
        }
        self.pending_intent = Some(PendingWalIntent {
            id: intent.id,
            capture_generation: intent.capture_generation,
            capture_sequence: intent.capture_sequence,
            serial_fence: intent.serial_fence,
            protected_roots: intent.protected_roots,
            protected_digest: intent.protected_digest,
            completed_mark: intent.completed_mark,
            mark_digest: intent.mark_digest,
        });
        Ok(())
    }

    fn reclaim_complete(
        &mut self,
        complete: ReclaimComplete<'_>,
        resources: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        if self.pending_intent.is_some() || self.pending_completion.is_some() {
            return Err(WalError::Participant);
        }
        let completed_count = complete.completed.len()?;
        let remaining_count = complete.remaining.len()?;
        if completed_count
            .checked_add(remaining_count)
            .is_none_or(|count| count > crate::property_graph::storage::reclaim::MAX_CANDIDATES)
        {
            return Err(WalError::Capacity);
        }
        self.reclaim_candidates.clear();
        self.reclaim_remaining.clear();
        for index in 0..completed_count {
            self.reclaim_candidates
                .push(complete.completed.get(index, resources)?);
        }
        for index in 0..remaining_count {
            self.reclaim_remaining
                .push(complete.remaining.get(index, resources)?);
        }
        self.pending_completion = Some(PendingWalCompletion {
            id: complete.id,
            intent: complete.intent,
            completed_count,
        });
        Ok(())
    }

    fn state(
        &mut self,
        kind: crate::property_graph::wal::EnvelopeKind,
        base: CommitState<'_>,
        target: CommitState<'_>,
        changes: ChangeReader<'_>,
        resources: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        let source = RecoverySource::new(
            self.store,
            self.directory,
            target,
            self.memory,
            self.artifact_capacity,
        )
        .map_err(|error| self.fail(NativeGraphError::Read(error), WalError::Capacity))?;
        let mut replay_error = None;
        let mut tree = source
            .resources(resources.remaining())
            .map_err(|error| self.fail(NativeGraphError::Read(error), WalError::Participant))?;
        let clearing_completed = self.pending_intent.is_none()
            && self.pending_completion.is_none()
            && self.reclaim_completed
            && base.reclaim.is_some()
            && target.reclaim.is_none();
        let result = (|| {
            match (self.pending_intent, self.pending_completion) {
                (Some(_), None) if base.reclaim.is_none() && target.reclaim.is_some() => {}
                (None, Some(complete))
                    if base.reclaim == Some(complete.intent) && target.reclaim.is_some() => {}
                (None, None) if clearing_completed => {}
                (None, None) if target.reclaim == base.reclaim => {}
                _ => return Err(TreeError::Invalid("recovery reclaim transition")),
            }
            match target.reclaim {
                Some(reclaim) => {
                    let authentic_base = self.pending_intent.is_some().then_some(base);
                    self.validate_state_reclaim(
                        &source,
                        target,
                        authentic_base,
                        reclaim,
                        &mut tree,
                    )?;
                }
                None => {
                    if (base.reclaim.is_some() && !clearing_completed)
                        || self.pending_intent.is_some()
                        || self.pending_completion.is_some()
                        || !self.reclaim_inventory.is_empty()
                    {
                        return Err(TreeError::Invalid("recovery reclaim state disappeared"));
                    }
                    if clearing_completed {
                        let manifest = self.active_reclaim.ok_or(TreeError::Invalid(
                            "completed reclaim retirement proof is absent",
                        ))?;
                        let id = crate::property_graph::wal::BatchId::new(
                            manifest.binding.session.get(),
                        )
                        .map_err(|_| TreeError::Invalid("completed reclaim retirement identity"))?;
                        self.reclaim_inventory.clear();
                        for index in 0..self.reclaim_candidates.len() {
                            let object = self.reclaim_candidates.get(index).copied().ok_or(
                                TreeError::Invalid("completed reclaim retirement candidate"),
                            )?;
                            if object.family != 17 {
                                continue;
                            }
                            self.reclaim_inventory.push(InventoryChange {
                                object,
                                state: InventoryState::Reclaimed(id),
                            });
                        }
                    }
                    self.active_reclaim = None;
                    self.active_intent_ref = None;
                    self.reclaim_completed = false;
                    if !clearing_completed {
                        self.reclaim_candidates.clear();
                    }
                }
            }
            let catalog = RecoveryCatalog::open(
                &source,
                target.catalog,
                self.expected,
                target.high_waters,
                &mut tree,
            )?;
            let base_catalog = RecoveryCatalog::open(
                &source,
                base.catalog,
                self.expected,
                base.high_waters,
                &mut tree,
            )?;
            catalog.validate_retains(&base_catalog, &mut tree)?;
            let base_native = GraphRoots::from_references(
                base.store,
                base.generation,
                base.graph.slots.map(|root| root.map(|value| value.block)),
            )?;
            let target_native = GraphRoots::from_references(
                target.store,
                target.generation,
                target.graph.slots.map(|root| root.map(|value| value.block)),
            )?;
            let reclaim =
                self.active_reclaim
                    .map(|manifest| {
                        crate::property_graph::wal::BatchId::new(manifest.binding.session.get())
                            .map(|id| {
                                (
                                    id,
                                    self.reclaim_candidates.as_slice(),
                                    self.reclaim_completed,
                                )
                            })
                    })
                    .transpose()
                    .map_err(|_| TreeError::Invalid("replay reclaim identity"))?;
            validate_native_checkpoint(
                &source,
                &catalog,
                self.document,
                self.memory,
                target_native,
                target.sequence,
                target.high_waters,
                reclaim,
                &mut tree,
            )?;
            let mut retained_mutations = StorageBuffer::new(self.memory, MAX_GRAPH_CHANGES)?;
            let mut semantic_changes = changes;
            while let Some(change) = semantic_changes.next_change(resources).map_err(|error| {
                if replay_error.is_none() {
                    replay_error = Some(error);
                }
                TreeError::Invalid("invalid recovery mutation stream")
            })? {
                match change {
                    Change::Mutation(mutation) => retained_mutations.push(mutation)?,
                    Change::Inventory(_) => {}
                    Change::ReclaimIntent(_) | Change::ReclaimComplete(_) => {}
                }
            }
            if kind == crate::property_graph::wal::EnvelopeKind::Maintenance
                && (!retained_mutations.as_slice().is_empty()
                    || target.catalog != base.catalog
                    || target.high_waters.node != base.high_waters.node
                    || target.high_waters.relationship != base.high_waters.relationship
                    || target.high_waters.symbols != base.high_waters.symbols)
            {
                return Err(TreeError::Invalid(
                    "maintenance changed logical commit state",
                ));
            }
            for (index, mutation) in retained_mutations.as_slice().iter().enumerate() {
                tree.step(1)?;
                for previous in retained_mutations
                    .as_slice()
                    .get(..index)
                    .ok_or(TreeError::Invalid("recovery mutation prefix"))?
                {
                    tree.step(1)?;
                    let duplicate_key = mutation
                        .provenance
                        .key
                        .zip(previous.provenance.key)
                        .is_some_and(|(left, right)| left == right);
                    if mutation.provenance.incarnation == previous.provenance.incarnation
                        || duplicate_key
                    {
                        return Err(TreeError::Invalid("duplicate recovery mutation target"));
                    }
                }
                validate_mutation_state(
                    &source,
                    &catalog,
                    self.memory,
                    self.document,
                    base_native,
                    target_native,
                    base.high_waters,
                    base.sequence,
                    target.sequence,
                    *mutation,
                    &mut tree,
                )?;
            }
            reconcile_entity_directory(
                &source,
                &catalog,
                self.document,
                base_native,
                target_native,
                TreeKind::Nodes,
                retained_mutations.as_slice(),
                base.high_waters.node,
                kind == crate::property_graph::wal::EnvelopeKind::Maintenance,
                target.sequence,
                self.memory,
                &mut tree,
            )?;
            reconcile_entity_directory(
                &source,
                &catalog,
                self.document,
                base_native,
                target_native,
                TreeKind::Relationships,
                retained_mutations.as_slice(),
                base.high_waters.relationship,
                kind == crate::property_graph::wal::EnvelopeKind::Maintenance,
                target.sequence,
                self.memory,
                &mut tree,
            )?;
            reconcile_fence_directory(
                &source,
                &catalog,
                self.document,
                base_native,
                target_native,
                retained_mutations.as_slice(),
                &mut tree,
            )?;
            let base_count = base.prepared_inventories.len().map_err(|error| {
                if replay_error.is_none() {
                    replay_error = Some(error);
                }
                TreeError::Invalid("invalid base prepared inventory list")
            })?;
            let target_count = target.prepared_inventories.len().map_err(|error| {
                if replay_error.is_none() {
                    replay_error = Some(error);
                }
                TreeError::Invalid("invalid target prepared inventory list")
            })?;
            match kind {
                crate::property_graph::wal::EnvelopeKind::Mutation => {
                    if target_count < base_count || target_count - base_count > 1 {
                        return Err(TreeError::Invalid("prepared inventory history order"));
                    }
                    for index in 0..base_count {
                        let old =
                            base.prepared_inventories
                                .get(index, resources)
                                .map_err(|error| {
                                    if replay_error.is_none() {
                                        replay_error = Some(error);
                                    }
                                    TreeError::Invalid("invalid base prepared inventory reference")
                                })?;
                        let retained =
                            target
                                .prepared_inventories
                                .get(index, resources)
                                .map_err(|error| {
                                    if replay_error.is_none() {
                                        replay_error = Some(error);
                                    }
                                    TreeError::Invalid(
                                        "invalid retained prepared inventory reference",
                                    )
                                })?;
                        if old != retained {
                            return Err(TreeError::Invalid("prepared inventory history changed"));
                        }
                    }
                }
                crate::property_graph::wal::EnvelopeKind::Maintenance => {
                    // The target keeps a suffix of the base list and appends
                    // its own manifest; the missing prefix was retired.
                    let retired = base_count
                        .checked_add(1)
                        .and_then(|count| count.checked_sub(target_count))
                        .filter(|retired| {
                            target_count > 0
                                && *retired
                                    <= crate::property_graph::storage::inventory::INVENTORY_FOLD_MANIFEST_LIMIT
                        })
                        .ok_or(TreeError::Invalid(
                            "maintenance prepared inventory replacement count",
                        ))?;
                    let history_count = target_count - 1;
                    let base_start = retired;
                    for index in 0..history_count {
                        let old = base
                            .prepared_inventories
                            .get(base_start + index, resources)
                            .map_err(|error| {
                                if replay_error.is_none() {
                                    replay_error = Some(error);
                                }
                                TreeError::Invalid("invalid base maintenance inventory reference")
                            })?;
                        let retained =
                            target
                                .prepared_inventories
                                .get(index, resources)
                                .map_err(|error| {
                                    if replay_error.is_none() {
                                        replay_error = Some(error);
                                    }
                                    TreeError::Invalid(
                                        "invalid retained maintenance inventory reference",
                                    )
                                })?;
                        if old != retained {
                            return Err(TreeError::Invalid(
                                "maintenance prepared inventory history changed",
                            ));
                        }
                    }
                    // Additions may come from any retired manifest or from
                    // the first retained one, which the fold may have started.
                    let selected_count = retired
                        .checked_add(1)
                        .ok_or(TreeError::Work)?
                        .min(base_count)
                        .min(
                            crate::property_graph::storage::inventory::INVENTORY_FOLD_MANIFEST_LIMIT,
                        );
                    let mut selected = Vec::new();
                    selected
                        .try_reserve_exact(selected_count)
                        .map_err(|_| TreeError::Memory)?;
                    for index in 0..selected_count {
                        let required =
                            base.prepared_inventories
                                .get(index, resources)
                                .map_err(|error| {
                                    if replay_error.is_none() {
                                        replay_error = Some(error);
                                    }
                                    TreeError::Invalid(
                                        "invalid selected maintenance inventory reference",
                                    )
                                })?;
                        selected.push((
                            required,
                            prepared_inventory(&source, required, base, &mut tree)?,
                        ));
                    }
                    validate_inventory_fold_transition(
                        &source,
                        base_native.directory(TreeKind::ObjectInventory)?,
                        target_native.directory(TreeKind::ObjectInventory)?,
                        &selected,
                        retired,
                        base,
                        &self.reclaim_inventory,
                        &mut tree,
                    )?;
                }
            }
            for index in 0..target_count {
                let required =
                    target
                        .prepared_inventories
                        .get(index, resources)
                        .map_err(|error| {
                            if replay_error.is_none() {
                                replay_error = Some(error);
                            }
                            TreeError::Invalid("invalid prepared inventory reference")
                        })?;
                let inventory = prepared_inventory(&source, required, target, &mut tree)?;
                let is_new = match kind {
                    crate::property_graph::wal::EnvelopeKind::Mutation => {
                        index == target_count.saturating_sub(1) && target_count > base_count
                    }
                    crate::property_graph::wal::EnvelopeKind::Maintenance => {
                        index == target_count.saturating_sub(1)
                    }
                };
                if is_new {
                    let current = self
                        .protected
                        .get(self.inventory_start..)
                        .ok_or(TreeError::Invalid("prepared inventory change range"))?;
                    if current.len() != inventory.count + 1
                        || current.last().copied() != Some(required.object)
                    {
                        return Err(TreeError::Invalid("prepared inventory envelope coverage"));
                    }
                    for (position, descriptor) in current
                        .get(..inventory.count)
                        .ok_or(TreeError::Invalid("prepared inventory coverage extent"))?
                        .iter()
                        .enumerate()
                    {
                        if inventory.descriptor(position)? != *descriptor {
                            return Err(TreeError::Invalid(
                                "prepared inventory envelope descriptor",
                            ));
                        }
                    }
                }
            }
            if kind == crate::property_graph::wal::EnvelopeKind::Mutation
                && target_count == base_count
                && self.protected.len() != self.inventory_start
            {
                return Err(TreeError::Invalid("unrooted prepared inventory changes"));
            }

            let base_sparse = crate::property_graph::storage::search::SparseCheckpoint {
                cutoff: base.sequence,
                roots: crate::property_graph::storage::search::SparseRoots {
                    text: base.text,
                    vector: base.vector,
                },
            };
            let target_sparse = crate::property_graph::storage::search::SparseRoots {
                text: target.text,
                vector: target.vector,
            };
            let empty_graph_noop = base.graph.slots.iter().all(Option::is_none)
                && target.graph.slots.iter().all(Option::is_none)
                && base_sparse.roots
                    == crate::property_graph::storage::search::SparseRoots::default()
                && target_sparse == crate::property_graph::storage::search::SparseRoots::default()
                && retained_mutations.as_slice().is_empty();
            if !empty_graph_noop {
                match kind {
                    crate::property_graph::wal::EnvelopeKind::Mutation => {
                        crate::property_graph::storage::search::validate_persisted_replay_transition(
                        &source,
                        base_sparse,
                        base_native,
                        target_sparse,
                        target_native,
                        target.catalog,
                        &catalog,
                        self.document,
                        self.store.lexical(),
                        changes,
                        resources,
                        &mut replay_error,
                        self.memory,
                        &mut tree,
                    )?;
                    }
                    crate::property_graph::wal::EnvelopeKind::Maintenance => {
                        crate::property_graph::storage::search::validate_persisted_maintenance_transition(
                        &source,
                        base_sparse,
                        base_native,
                        target_sparse,
                        target_native,
                        target.catalog,
                        &catalog,
                        self.document,
                        self.store.lexical(),
                        self.memory,
                        &mut tree,
                    )?;
                    }
                }
            }

            Ok::<(), TreeError>(())
        })();
        resources.charge(tree.work())?;
        if let Err(error) = result {
            if let Some(error) = replay_error {
                return Err(error);
            }
            let source = source
                .take_source_error()
                .unwrap_or(NativeGraphError::Read(error));
            return Err(self.fail(source, WalError::Participant));
        }
        self.inventory_start = self.protected.len();
        self.validate_state(target, resources)?;
        if clearing_completed {
            self.checkpoint_allocations
                .retain(|descriptor| !self.reclaim_candidates.contains(descriptor));
            self.reclaim_candidates.clear();
        }
        self.reclaim_inventory.clear();
        self.reclaim_remaining.clear();
        self.pending_intent = None;
        self.pending_completion = None;
        Ok(())
    }
}

pub(super) fn open(
    path: &Path,
    options: OpenOptions,
    document: Option<EmbeddingTower>,
    vfs: Arc<dyn Vfs>,
    clock: Arc<dyn MonotonicClock>,
) -> Result<Store, NativeGraphError> {
    let store = Store::open_graph_with_infrastructure(path, options, document, vfs, clock)?;
    if store
        .native_graph
        .state
        .lock()
        .map_err(|_| crate::lifecycle::StoreError::Synchronization {
            component: "native graph publication",
        })?
        .current
        .is_none()
    {
        return Err(NativeGraphError::StoreInitializationIncomplete);
    }
    Ok(store)
}

fn require_type_absent(
    source: &RecoverySource<'_, '_>,
    roots: GraphRoots,
    row: RelationshipRow,
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    let mut key = [0_u8; 24];
    key.get_mut(..8)
        .ok_or(TreeError::Memory)?
        .copy_from_slice(&row.relationship_type.get().to_le_bytes());
    key.get_mut(8..)
        .ok_or(TreeError::Memory)?
        .copy_from_slice(&row.rel.get().to_le_bytes());
    if lookup_entry(
        source,
        roots.directory(TreeKind::RelationshipTypes)?,
        &key,
        r,
    )?
    .is_some()
    {
        return Err(TreeError::Invalid(
            "swept relationship retains type membership",
        ));
    }
    Ok(())
}

fn require_no_physical_incident(
    source: &RecoverySource<'_, '_>,
    roots: GraphRoots,
    node: crate::property_graph::NodeId,
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    // The checkpoint's ordinary relationship/adjacency correlation checks
    // establish both copies once. A swept node must have no own range: seek
    // its prefix, rather than rechecking the entire graph for every old fence.
    let mut probe = [0_u8; 40];
    probe
        .get_mut(16..24)
        .ok_or(TreeError::Memory)?
        .copy_from_slice(&1_u64.to_le_bytes());
    probe
        .get_mut(24..40)
        .ok_or(TreeError::Memory)?
        .copy_from_slice(&1_u128.to_le_bytes());
    probe
        .get_mut(..16)
        .ok_or(TreeError::Memory)?
        .copy_from_slice(&node.get().to_le_bytes());
    for kind in [TreeKind::OutRanges, TreeKind::InRanges] {
        #[cfg(test)]
        crate::property_graph::storage::consolidation::count_sweep_work(2);
        let root = roots.directory(kind)?;
        let mut cursor = DirectoryCursor::seek(source, root, Some(&probe), r)?;
        if let Some(entry) = cursor.next_entry(r)? {
            let crate::property_graph::storage::tree::Key::Inline(key) = entry.key() else {
                return Err(TreeError::Invalid("incident adjacency key"));
            };
            if key.get(..16) == Some(node.get().to_le_bytes().as_slice()) {
                return Err(TreeError::Invalid("swept node retains incident adjacency"));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_maintenance_removal(
    source: &RecoverySource<'_, '_>,
    catalog: &RecoveryCatalog<'_, '_>,
    document: Option<&EmbeddingTower>,
    base: GraphRoots,
    target: GraphRoots,
    sequence: u64,
    entity: EntityId,
    memory: &StorageMemory<'_>,
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    let (kind, id) = match entity {
        EntityId::Node(id) => (TreeKind::Nodes, id.get()),
        EntityId::Relationship(id) => (TreeKind::Relationships, id.get()),
    };
    let entry = lookup_entry(source, base.directory(kind)?, &id.to_le_bytes(), r)?
        .ok_or(TreeError::Missing)?;
    let slice = PayloadSlice::new(
        source,
        base.store(),
        entry.creation_generation(),
        PayloadRef::decode(entry.value())?,
    );
    let (provenance, canonical) = match entity {
        EntityId::Node(node) => {
            let NodeRecordState::Tombstone(tombstone) =
                verify_node_state(slice, node, catalog, document, r)?
            else {
                return Err(TreeError::Invalid("maintenance removed live node"));
            };
            require_no_physical_incident(source, target, node, r)?;
            (
                crate::property_graph::storage::records::verify_provenance(
                    PayloadSlice::new(
                        source,
                        base.store(),
                        entry.creation_generation(),
                        tombstone.provenance_ref(),
                    ),
                    r,
                )?,
                None,
            )
        }
        EntityId::Relationship(rel) => {
            let record = verify_record(slice, entity, catalog, document, r)?;
            let row = authoritative_relationship(source, catalog, document, base, rel, r)?
                .ok_or(TreeError::Missing)?;
            let a = crate::property_graph::storage::records::endpoint_present(
                source, base, row.source, catalog, document, r,
            )?;
            let b = crate::property_graph::storage::records::endpoint_present(
                source, base, row.target, catalog, document, r,
            )?;
            if a && b {
                return Err(TreeError::Invalid(
                    "maintenance removed visible relationship",
                ));
            }
            exact_relationship_membership(source, target, sequence, row, false, memory, r)?;
            require_type_absent(source, target, row, r)?;
            (
                crate::property_graph::storage::records::verify_provenance(
                    PayloadSlice::new(
                        source,
                        base.store(),
                        entry.creation_generation(),
                        *record
                            .required_payloads()
                            .get(1)
                            .ok_or(TreeError::Missing)?,
                    ),
                    r,
                )?,
                Some(record.canonical_bytes()),
            )
        }
    };
    if let Some(key) = provenance.key() {
        let old = find_fence_by_key(source, catalog, document, base, key, r)?
            .ok_or(TreeError::Missing)?;
        let new = find_fence_by_key(source, catalog, document, target, key, r)?
            .ok_or(TreeError::Missing)?;
        if old.incarnation() != entity
            || new.incarnation() != entity
            || old.revision() != new.revision()
            || old.is_deleted() != new.is_deleted()
            || !stored_provenance_equal(&provenance, old.provenance(), r)?
            || !stored_provenance_equal(old.provenance(), new.provenance(), r)?
        {
            return Err(TreeError::Invalid(
                "maintenance sweep changed fence evidence",
            ));
        }
        match (canonical, old.canonical_bytes(), new.canonical_bytes()) {
            (None, None, None) => {}
            (Some(record), Some(old), Some(new))
                if record.compare(old, r)?.is_eq() && old.compare(new, r)?.is_eq() => {}
            _ => {
                return Err(TreeError::Invalid(
                    "maintenance sweep changed canonical evidence",
                ));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_swept_lifecycle(
    source: &RecoverySource<'_, '_>,
    catalog: &RecoveryCatalog<'_, '_>,
    document: Option<&EmbeddingTower>,
    base: GraphRoots,
    target: GraphRoots,
    mutation: Mutation<'_>,
    fence: &FenceView<'_, RecoverySource<'_, '_>>,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    let fields = mutation.provenance;
    let key = fields.key.ok_or(TreeError::Missing)?;
    let row = fence.hidden_relationship(source, base, catalog, document, resources)?;
    let shape = recovery_entity_shape(
        RecordShape::Relationship {
            id: row.rel,
            source: row.source,
            target: row.target,
            relationship_type: row.relationship_type,
        },
        catalog,
        resources,
    )?;
    let bytes = fence.canonical_bytes().ok_or(TreeError::Missing)?;
    let fingerprint = streamed_fingerprint(bytes, resources)?;
    let prior = OperationProvenance::from_fields(
        Some(1),
        fence.provenance().fields_with_key(Some(key), resources)?,
    )
    .map_err(|_| TreeError::Invalid("swept lifecycle provenance"))?;
    let first_error = Cell::new(None);
    let resources_cell = RefCell::new(resources);
    let mut reader = RecoveryPayloadReader {
        slice: bytes,
        offset: 0,
        resources: &resources_cell,
        first_error: &first_error,
    };
    let mut scratch = [0; 4096];
    let decision = classify_key(
        key,
        KeyState::Live(CurrentEntity {
            provenance: prior,
            contents: CanonicalRecord::from_validated(shape, fingerprint, &mut reader),
        }),
        KeyRequest::Delete {
            revision: fields.requested_revision,
            expected: fields.incarnation,
            mode: fields.delete_mode.ok_or(TreeError::Missing)?,
        },
        &mut scratch,
        &mut || recovery_checkpoint(&resources_cell, &first_error),
    )
    .map_err(|e| lifecycle_tree_error(&first_error, e))?;
    let KeyDecision::Change(change) = decision else {
        return Err(TreeError::Invalid("swept lifecycle is not a change"));
    };
    let installed = change
        .install(fields.incarnation, target.generation(), &mut || {
            recovery_checkpoint(&resources_cell, &first_error)
        })
        .map_err(|e| lifecycle_tree_error(&first_error, e))?;
    if installed.fields() != fields || mutation.live {
        return Err(TreeError::Invalid("swept lifecycle transition differs"));
    }
    Ok(())
}

#[cfg(any(test, feature = "test-seams"))]
pub(super) fn validate_reclaim_proof_for_test(
    store: &Store,
    bundle: &super::NativeGraphBundle,
    control: &QueryControl,
) -> Result<(), NativeGraphError> {
    let shared = GraphResources::from_store(store)?;
    let write = WriteMemory::new(&shared, WriteLimits::default())?;
    let memory = StorageMemory::new(&write, control, 32 * 1024 * 1024)?;
    let expected = GraphInterpretation::new(bundle.lexical(), bundle.document())?;
    let mut replay = SemanticReplay::new(
        store,
        bundle.directory(),
        expected,
        bundle.document(),
        control,
        crate::property_graph::storage::MAX_NATIVE_ARTIFACTS,
        &shared,
        &memory,
    )?;
    let state = super::write::commit_state(bundle);
    let source = RecoverySource::new(
        store,
        bundle.directory(),
        state,
        &memory,
        crate::property_graph::storage::MAX_NATIVE_ARTIFACTS,
    )?;
    let mut resources =
        source.resources(crate::property_graph::storage::reclaim::MARK_WORK_LIMIT)?;
    replay
        .validate_state_reclaim(
            &source,
            state,
            None,
            bundle
                .reclaim()
                .ok_or(NativeGraphError::Invalid("test reclaim root"))?,
            &mut resources,
        )
        .map_err(|error| {
            replay
                .first_error
                .take()
                .unwrap_or(NativeGraphError::Read(error))
        })
}

/// Admits the manifest checkpoint and every graph envelope routed by the single
/// document WAL scan. All participant checks finish before publication.
pub(crate) fn install_unified(
    store: &Store,
    graph: &crate::manifest::GraphManifest,
    manifest_generation: u64,
    commits: &[crate::ingest::RecoveredGraphCommit],
    access: AccessMode,
    document: Option<EmbeddingTower>,
) -> Result<(), crate::lifecycle::StoreError> {
    install_unified_inner(store, graph, manifest_generation, commits, access, document).map_err(
        |error| match error {
            NativeGraphError::Store(error) => error,
            NativeGraphError::Io { path, source } => {
                crate::lifecycle::StoreError::Io { path, source }
            }
            error => crate::lifecycle::StoreError::Manifest(
                crate::manifest::ManifestError::Decode(error.to_string()),
            ),
        },
    )
}

fn install_unified_inner(
    store: &Store,
    graph: &crate::manifest::GraphManifest,
    manifest_generation: u64,
    commits: &[crate::ingest::RecoveredGraphCommit],
    access: AccessMode,
    document: Option<EmbeddingTower>,
) -> Result<(), NativeGraphError> {
    #[cfg(test)]
    REOPEN_PROFILE.with(|report| *report.borrow_mut() = ReopenProfile::default());
    #[cfg(test)]
    let mut phase_start = std::time::Instant::now();
    #[cfg(test)]
    FENCE_CANDIDATES.with(|count| count.set(0));
    store.accounting.enable_graph_ceiling()?;
    let checkpoint = graph
        .state()
        .map_err(|e| NativeGraphError::Store(crate::lifecycle::StoreError::Manifest(e)))?;
    if checkpoint.generation.get() > manifest_generation {
        return Err(NativeGraphError::Invalid(
            "checkpoint generation ahead of manifest",
        ));
    }
    let shared = GraphResources::from_store(store)?;
    let control = QueryControl::Cancel(CancelToken::new());
    let write = WriteMemory::new(&shared, WriteLimits::default())?;
    let storage = StorageMemory::new(&write, &control, 32 * 1024 * 1024)?;
    let capacity = artifact_inventory_capacity(store, &store.directory)?
        .checked_add(crate::property_graph::storage::reclaim::MAX_CANDIDATES)
        .ok_or(NativeGraphError::IdentityExhausted)?;
    let document = document.or_else(|| {
        store
            .epoch
            .as_ref()
            .map(|epoch| epoch.embedding.document.clone())
    });
    let expected = GraphInterpretation::new(store.tokenizer.epoch(), document.as_ref())?;
    let (validator, final_state, (_replay_work, replay_remaining)) = replay_unified(
        store,
        &store.directory,
        checkpoint,
        commits,
        expected,
        document.as_ref(),
        &control,
        capacity,
        &shared,
        &storage,
    )?;
    let mut cancelled = || control.checkpoint().is_err();
    let mut resources =
        WalResources::new(replay_remaining, STACK_RESERVATION_BYTES, &mut cancelled)?;
    let mut prepared = Vec::new();
    for index in 0..final_state.prepared_inventories.len()? {
        prepared.push(
            final_state
                .prepared_inventories
                .get(index, &mut resources)?,
        );
    }
    let roots = GraphRoots::from_references(
        final_state.store,
        final_state.generation,
        final_state
            .graph
            .slots
            .map(|root| root.map(|value| value.block)),
    )?;
    #[cfg(test)]
    reopen_phase("deferred_allocations_and_roots", &mut phase_start);
    let bundle = super::NativeGraphBundle::install_recovered(
        store,
        &shared,
        NativeGraphBundleInput {
            base: BaseIdentity {
                store: final_state.store,
                generation: final_state.generation,
                fold: crate::property_graph::staging::FoldMark {
                    manifest_generation,
                    graph_absorbed_through: graph.graph_absorbed_through,
                    envelope_sequence: checkpoint.sequence,
                },
                roots: None,
            },
            root_envelope: None,
            roots,
            wal_roots: final_state.graph,
            sequence: final_state.sequence,
            catalog: final_state.catalog,
            vector: final_state.vector,
            text: final_state.text,
            reclaim: final_state.reclaim,
            high_waters: final_state.high_waters,
            prepared_inventories: prepared,
            lexical: store.tokenizer.epoch(),
            document: document.clone(),
        },
    )?;
    store.native_graph.install(bundle)?;
    if access == AccessMode::ReadOnly {
        store.native_graph.mark_read_only();
    } else {
        let serial_fence = scan_creation_serials(
            store,
            &shared,
            &store.directory,
            final_state.store,
            final_state.high_waters.creation_serial,
            &control,
        )?;
        #[cfg(test)]
        reopen_phase("creation_serial_scan", &mut phase_start);
        let last_graph_seq = commits
            .last()
            .map_or(graph.graph_absorbed_through, |commit| commit.seq.get());
        let count =
            u64::try_from(commits.len()).map_err(|_| NativeGraphError::IdentityExhausted)?;
        let mut writer = super::write::NativeWriter::resume(
            last_graph_seq,
            &shared,
            count,
            &validator.protected,
        )?;
        writer.envelope_bytes = commits
            .iter()
            .try_fold(0_usize, |sum, commit| sum.checked_add(commit.bytes.len()))
            .ok_or(NativeGraphError::IdentityExhausted)?;
        store.native_graph.initialize_writer(writer, serial_fence)?;
        if final_state.sequence != 0 {
            // Retained history cannot gain a fresh allowance merely by reopening.
            store.native_graph.commits_since_reclaim.store(
                super::automatic::RECLAIM_AFTER_COMMITS,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
    }
    #[cfg(test)]
    let resident_peak = shared.peak_reserved_bytes()?;
    #[cfg(test)]
    OPEN_METRICS.with(|metrics| {
        metrics.set((
            _replay_work.saturating_add(resources.consumed()),
            storage.peak_reserved_bytes(),
            FENCE_CANDIDATES.with(Cell::get),
            resident_peak,
        ))
    });
    #[cfg(test)]
    reopen_phase("bundle_writer_installation", &mut phase_start);
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "read dependencies and borrowed replay/resource lifetimes are explicit"
)]
fn replay_unified<'a, 'm>(
    store: &'a dyn RecoveryRead,
    directory: &'a Path,
    checkpoint: CommitState<'a>,
    commits: &'a [crate::ingest::RecoveredGraphCommit],
    expected: GraphInterpretation<'a>,
    document: Option<&'a EmbeddingTower>,
    control: &'a QueryControl,
    capacity: usize,
    shared: &'a GraphResources,
    storage: &'m StorageMemory<'m>,
) -> Result<(SemanticReplay<'a, 'm>, CommitState<'a>, (u64, u64)), NativeGraphError> {
    #[cfg(test)]
    let mut phase_start = std::time::Instant::now();
    let mut validator = SemanticReplay::new(
        store, directory, expected, document, control, capacity, shared, storage,
    )?;
    let mut cancelled = || control.checkpoint().is_err();
    let replay_work = u64::try_from(capacity)
        .ok()
        .and_then(|count| count.checked_mul(MAX_ARTIFACT_BYTES as u64))
        .and_then(|bytes| bytes.checked_mul(8))
        .and_then(|work| {
            commits.iter().try_fold(work, |work, commit| {
                u64::try_from(commit.bytes.len())
                    .ok()
                    .and_then(|bytes| bytes.checked_mul(128))
                    .and_then(|bytes| work.checked_add(bytes))
            })
        })
        .ok_or(NativeGraphError::Invalid("unified recovery work bound"))?;
    let mut resources = WalResources::new(replay_work, STACK_RESERVATION_BYTES, &mut cancelled)?;
    #[cfg(test)]
    reopen_phase("manifest_checkpoint_setup", &mut phase_start);
    validator
        .validate_checkpoint_state(checkpoint, &mut resources)
        .map_err(|error| {
            validator
                .first_error
                .take()
                .unwrap_or(NativeGraphError::Wal(error))
        })?;
    #[cfg(test)]
    reopen_phase("checkpoint_validation", &mut phase_start);
    let mut final_state = checkpoint;
    for commit in commits {
        let mut replay = Replay::envelope(&commit.bytes, final_state);
        match replay
            .next_envelope(&mut validator, &mut resources)
            .map_err(|error| {
                validator
                    .first_error
                    .take()
                    .unwrap_or(NativeGraphError::Wal(error))
            })? {
            ReplayStep::Envelope(envelope) => {
                if envelope.state.generation.get() != commit.generation {
                    return Err(NativeGraphError::Store(
                        crate::lifecycle::StoreError::WalMutation {
                            seq: commit.seq,
                            op: crate::ingest::wal_payload::GRAPH_COMMIT_V1,
                            source: crate::ingest::wal_payload::PayloadError::GraphEnvelope(
                                WalError::Sequence,
                            ),
                        },
                    ));
                }
                final_state = envelope.state;
            }
            ReplayStep::End(_) => {
                return Err(NativeGraphError::Invalid(
                    "incomplete unified graph envelope",
                ));
            }
        }
    }
    #[cfg(test)]
    reopen_phase("wal_watermark_replay", &mut phase_start);
    validator.validate_deferred_checkpoint_allocations(final_state, &mut resources)?;
    Ok((
        validator,
        final_state,
        (resources.consumed(), resources.remaining()),
    ))
}

/// Census authority is the decoded manifest and complete outer WAL runs.
/// This shares the envelope framer with recovery; it grants no replay or unlink
/// authority. Semantic admission still runs separately, including reclaim proofs.
pub(crate) fn snapshot_graph_objects(
    vfs: &dyn Vfs,
    directory: &Path,
    manifest: &crate::manifest::Manifest,
    records: &[crate::wal::VisibleRecord],
) -> Result<Vec<crate::manifest::GraphObject>, NativeGraphError> {
    use crate::ingest::{committed_batches, wal_payload::MutationPayload};
    use std::collections::BTreeMap;
    let Some(graph) = &manifest.graph else {
        return Ok(Vec::new());
    };
    let mut state = graph
        .state()
        .map_err(|error| NativeGraphError::Store(crate::lifecycle::StoreError::Manifest(error)))?;
    let store = state.store;
    let mut objects: BTreeMap<_, _> = graph
        .objects
        .iter()
        .map(|object| (object.artifact, *object))
        .collect();
    let mut descriptors = BTreeMap::new();
    let mut offer = |descriptor: ArtifactDescriptor| -> Result<(), NativeGraphError> {
        if descriptor.store != store
            || descriptors
                .get(&descriptor.artifact)
                .is_some_and(|prior| *prior != descriptor)
        {
            return Err(NativeGraphError::Invalid(
                "snapshot descriptor identity contradiction",
            ));
        }
        let object = crate::manifest::GraphObject {
            artifact: descriptor.artifact,
            length: u64::from(descriptor.bytes),
            checksum: descriptor.checksum,
        };
        if objects
            .get(&object.artifact)
            .is_some_and(|prior| *prior != object)
        {
            return Err(NativeGraphError::Invalid(
                "snapshot inventory descriptor contradiction",
            ));
        }
        if objects.len() >= crate::property_graph::storage::MAX_NATIVE_ARTIFACTS
            && !objects.contains_key(&object.artifact)
        {
            return Err(NativeGraphError::Invalid("snapshot graph census capacity"));
        }
        descriptors.insert(descriptor.artifact, descriptor);
        objects.insert(object.artifact, object);
        Ok(())
    };
    let boundary = manifest.log_seq.min(graph.graph_absorbed_through);
    let decisions = crate::lifecycle::namespace_batch::transaction_decisions(
        vfs, directory, records, boundary, None,
    )?;
    let batches = committed_batches(records, boundary, |binding| {
        decisions.get(&binding.transaction).copied()
    })?;
    let mut cancelled = || false;
    let bytes = records.iter().try_fold(0_u64, |sum, record| {
        let bytes = record.encoded().map_err(|source| {
            NativeGraphError::Store(crate::lifecycle::StoreError::WalRecord {
                seq: record.seq,
                source,
            })
        })?;
        sum.checked_add(bytes.len() as u64)
            .ok_or(NativeGraphError::Invalid(
                "snapshot graph census work bound",
            ))
    })?;
    let work = bytes
        .checked_mul(128)
        .and_then(|bytes| bytes.checked_add(1024 * 1024))
        .ok_or(NativeGraphError::Invalid(
            "snapshot graph census work bound",
        ))?;
    let mut resources = WalResources::new(work, STACK_RESERVATION_BYTES, &mut cancelled)?;
    for batch in &batches {
        for (seq, _, mutation) in &batch.members {
            if seq.get() <= graph.graph_absorbed_through {
                continue;
            }
            let MutationPayload::GraphCommit(bytes) = mutation else {
                continue;
            };
            let mut replay = Replay::envelope(bytes, state);
            let FramedCaptureStep::Envelope(envelope) =
                replay.next_framed_capture(&mut resources)?
            else {
                return Err(NativeGraphError::Invalid(
                    "incomplete snapshot graph envelope",
                ));
            };
            let _kind = envelope.kind;
            if envelope.complete_bytes != bytes.len() {
                return Err(NativeGraphError::Invalid("snapshot graph envelope extent"));
            }
            state = envelope.state;
            for reference in state
                .graph
                .slots
                .into_iter()
                .flatten()
                .chain(Some(state.catalog))
                .chain(state.text)
                .chain(state.vector)
                .chain(state.reclaim)
            {
                offer(reference.object)?;
            }
            for index in 0..state.prepared_inventories.len()? {
                offer(
                    state
                        .prepared_inventories
                        .get(index, &mut resources)?
                        .object,
                )?;
            }
            let mut changes = envelope.changes();
            while let Some(change) = changes.next_change(&mut resources)? {
                match change {
                    Change::Mutation(mutation) => {
                        if let Some(reference) = mutation.canonical {
                            offer(reference.object)?;
                        }
                    }
                    Change::Inventory(change)
                        if matches!(
                            change.state,
                            InventoryState::Prepared | InventoryState::Retained
                        ) =>
                    {
                        offer(change.object)?
                    }
                    Change::ReclaimIntent(intent) => {
                        offer(intent.protected_roots.object)?;
                        offer(intent.completed_mark.object)?;
                    }
                    Change::ReclaimComplete(complete) => offer(complete.intent.object)?,
                    Change::Inventory(_) => {}
                }
            }
        }
    }
    Ok(objects.into_values().collect())
}

pub(crate) struct GraphVerificationError {
    pub(crate) source: Box<NativeGraphError>,
    pub(crate) file: Option<PathBuf>,
}

pub(crate) fn verify_unified(
    vfs: &dyn Vfs,
    directory: &Path,
    graph: &crate::manifest::GraphManifest,
    manifest_generation: u64,
    commits: &[crate::ingest::RecoveredGraphCommit],
) -> Result<(), GraphVerificationError> {
    struct Reads<'a> {
        vfs: &'a dyn Vfs,
        publication: Arc<super::NativeGraphPublication>,
        lexical: crate::fts::tokenizer::TokenizerEpoch,
        last_file: RefCell<Option<PathBuf>>,
    }
    impl RecoveryRead for Reads<'_> {
        fn vfs(&self) -> &dyn Vfs {
            self.vfs
        }
        fn publication(&self) -> &Arc<super::NativeGraphPublication> {
            &self.publication
        }
        fn lexical(&self) -> crate::fts::tokenizer::TokenizerEpoch {
            self.lexical
        }
        fn observe_mapping(&self, path: &Path) {
            *self.last_file.borrow_mut() = Some(path.to_path_buf());
        }
    }
    let mut last_file = None;
    let result = (|| -> Result<(), NativeGraphError> {
        let checkpoint = graph.state().map_err(|error| {
            NativeGraphError::Store(crate::lifecycle::StoreError::Manifest(error))
        })?;
        if checkpoint.generation.get() > manifest_generation {
            return Err(NativeGraphError::Invalid(
                "checkpoint generation ahead of manifest",
            ));
        }
        let accounting = Arc::new(crate::lifecycle::stats::Accounting::new(
            crate::property_graph::resources::MAX_GRAPH_RESIDENT_BYTES,
            u64::MAX,
        ));
        let publication = super::NativeGraphPublication::new(&accounting, true)?;
        let catalog_path = crate::property_graph::storage::allocation::artifact_path(
            directory,
            checkpoint.catalog.object.artifact,
        );
        last_file = Some(catalog_path.clone());
        let catalog_file =
            vfs.open_for_map(&catalog_path)
                .map_err(|source| NativeGraphError::Io {
                    path: catalog_path.clone(),
                    source,
                })?;
        let mapping = NativeReadonlyMapping::open_recovery(
            catalog_file,
            &catalog_path,
            &publication,
            MAX_ARTIFACT_BYTES,
        )?;
        let frame = artifact::decode(
            ContainerKind::Object,
            Some((checkpoint.store, checkpoint.catalog.object.artifact)),
            mapping.as_bytes(),
        )
        .map_err(TreeError::Format)?;
        let payload = frame
            .resolve_framed_block(checkpoint.catalog.block)
            .map_err(TreeError::Format)?;
        let encoded = payload
            .get(8..)
            .ok_or(NativeGraphError::Invalid("catalog participant header"))?;
        let shared = GraphResources::from_accounting(&accounting)?;
        let count = encoded
            .get(104..112)
            .and_then(|bytes| bytes.first_chunk::<8>())
            .copied()
            .map(u64::from_le_bytes)
            .and_then(|count| usize::try_from(count).ok())
            .ok_or(NativeGraphError::Invalid(
                "verification catalog symbol count",
            ))?;
        let allowance = count
            .checked_mul(std::mem::size_of::<SymbolEntry<'_>>())
            .ok_or(NativeGraphError::IdentityExhausted)?;
        let mut descriptors = shared.reserve(allowance)?;
        let image = CatalogImage::decode(encoded, allowance, &mut || Ok(()))?;
        descriptors.resize(image.symbols.allocated_bytes())?;
        let expected = image.declaration.interpretation;
        // The tower strings are bounded by the encoded catalog bytes.
        let _document_charge = shared.reserve(encoded.len())?;
        let document = expected
            .embedding()
            .map(|declaration| declaration.to_tower());
        let reads = Reads {
            vfs,
            publication,
            lexical: expected.lexical(),
            last_file: RefCell::new(None),
        };
        let control = QueryControl::Cancel(CancelToken::new());
        let write = WriteMemory::new(&shared, WriteLimits::default())?;
        let storage = StorageMemory::new(&write, &control, 32 * 1024 * 1024)?;
        let capacity = artifact_inventory_capacity(&reads, directory)?
            .checked_add(crate::property_graph::storage::reclaim::MAX_CANDIDATES)
            .ok_or(NativeGraphError::IdentityExhausted)?;
        let replay = replay_unified(
            &reads,
            directory,
            checkpoint,
            commits,
            expected,
            document.as_ref(),
            &control,
            capacity,
            &shared,
            &storage,
        );
        last_file = reads.last_file.borrow().clone();
        replay.map(|_| ())
    })();
    result.map_err(|source| GraphVerificationError {
        source: Box::new(source),
        file: last_file,
    })
}

/// Live inventory at a fold; prepared and retained packs remain immutable.
pub(super) fn manifest_inventory(
    store: &Store,
    bundle: &super::NativeGraphBundle,
    memory: &StorageMemory<'_>,
) -> Result<Vec<crate::manifest::GraphObject>, NativeGraphError> {
    use crate::property_graph::storage::reclaim::{
        DurableRunReader, ProtectedValue, SpillIo, validate_protected_stream,
    };
    let state = super::write::commit_state(bundle);
    let source = RecoverySource::new_scoped(store, bundle.directory(), state, memory, 1)?;
    let mut tree = source.resources(crate::property_graph::storage::reclaim::MARK_WORK_LIMIT)?;
    let descriptors = RefCell::new(StorageBuffer::<ArtifactDescriptor>::new(
        memory,
        crate::property_graph::storage::MAX_NATIVE_ARTIFACTS,
    )?);
    let offer = |descriptor: ArtifactDescriptor| -> Result<(), TreeError> {
        let mut values = descriptors
            .try_borrow_mut()
            .map_err(|_| TreeError::Memory)?;
        if let Some(previous) = values
            .as_slice()
            .iter()
            .find(|value| value.artifact == descriptor.artifact)
        {
            if *previous != descriptor {
                return Err(TreeError::Invalid(
                    "fold inventory descriptor contradiction",
                ));
            }
        } else {
            values.push(descriptor)?;
        }
        Ok(())
    };
    for required in state
        .graph
        .slots
        .into_iter()
        .flatten()
        .chain(Some(state.catalog))
        .chain(state.text)
        .chain(state.vector)
        .chain(state.reclaim)
        .chain(bundle.prepared_inventories().iter().copied())
    {
        offer(required.object)?;
    }
    for required in [state.text, state.vector].into_iter().flatten() {
        let catalog =
            crate::property_graph::storage::search::root_catalog(&source, required, &mut tree)?;
        offer(catalog.object)?;
    }
    let root = bundle.roots().directory(TreeKind::ObjectInventory)?;
    let mut cursor =
        crate::property_graph::storage::tree::directory::DirectoryTraceState::new(root, &mut tree)?;
    loop {
        use crate::property_graph::storage::tree::directory::DirectoryTraceEvent;
        match cursor.next(&source, &mut tree)? {
            DirectoryTraceEvent::Reference(_) => {}
            DirectoryTraceEvent::Leaf(leaf) => {
                cursor.with_leaf(leaf, &mut tree, |entry, tree| {
                    let change = verify_inventory_entry(root, entry, tree)?;
                    if matches!(
                        change.state,
                        InventoryState::Prepared | InventoryState::Retained
                    ) {
                        offer(change.object)?;
                    }
                    Ok(())
                })?;
            }
            DirectoryTraceEvent::Done => break,
        }
    }
    for required in bundle.prepared_inventories().iter().copied() {
        source.with_artifact_window(required.block, &mut tree, |window, tree| {
            let inventory = prepared_inventory(window, required, state, tree)?;
            for index in 0..inventory.count {
                offer(inventory.descriptor(index)?)?;
            }
            Ok(())
        })?;
    }
    if let Some(mut reclaim) = state.reclaim {
        let mut bytes = StorageBuffer::new(memory, reclaim.block.length as usize)?;
        for _ in 0..reclaim.block.length {
            bytes.push(0)?;
        }
        let mut length = source.copy_required_payload(reclaim, bytes.as_mut_slice(), &mut tree)?;
        let payload = bytes.as_slice().get(..length).ok_or(TreeError::Memory)?;
        if payload.get(8..10) == Some(&3_u16.to_le_bytes()) {
            reclaim =
                crate::property_graph::storage::reclaim::decode_completed_intent_manifest(payload)?
                    .intent;
            offer(reclaim.object)?;
            drop(bytes);
            let mut intent_bytes = StorageBuffer::new(memory, reclaim.block.length as usize)?;
            for _ in 0..reclaim.block.length {
                intent_bytes.push(0)?;
            }
            bytes = intent_bytes;
            length = source.copy_required_payload(reclaim, bytes.as_mut_slice(), &mut tree)?;
        }
        let manifest = crate::property_graph::storage::reclaim::decode_pending_intent_manifest(
            bytes.as_slice().get(..length).ok_or(TreeError::Memory)?,
        )?;
        struct ProofIo<'a, 's, 'm, F> {
            source: &'a RecoverySource<'s, 'm>,
            offer: F,
        }
        impl<F: Fn(ArtifactDescriptor) -> Result<(), TreeError>> SpillIo for ProofIo<'_, '_, '_, F> {
            fn append_page(
                &mut self,
                _: &[u8],
                _: &mut TreeResources<'_>,
            ) -> Result<RequiredRef, TreeError> {
                Err(TreeError::Invalid("fold proof is read only"))
            }
            fn read_page(
                &self,
                reference: RequiredRef,
                output: &mut [u8],
                resources: &mut TreeResources<'_>,
            ) -> Result<usize, TreeError> {
                (self.offer)(reference.object)?;
                self.source
                    .copy_required_payload(reference, output, resources)
            }
        }
        let io = ProofIo {
            source: &source,
            offer: &offer,
        };
        validate_protected_stream(manifest.protected, &io, memory, &mut tree, |record, _| {
            match record.value {
                ProtectedValue::Required(required)
                | ProtectedValue::CapturedBase {
                    checkpoint: required,
                    ..
                } => offer(required.object)?,
                ProtectedValue::Descriptor(descriptor)
                    if record.class
                        == crate::property_graph::storage::reclaim::ProtectedClass::Reader =>
                {
                    offer(descriptor)?
                }
                ProtectedValue::Descriptor(_) | ProtectedValue::FoldAuthority { .. } => {}
            }
            Ok(())
        })?;
        let mut mark = DurableRunReader::new(manifest.mark, memory)?;
        while mark.next(&io, &mut tree)?.is_some() {}
    }
    let descriptors = descriptors.into_inner();
    let mut objects = Vec::new();
    objects
        .try_reserve_exact(descriptors.as_slice().len())
        .map_err(|_| NativeGraphError::Invalid("fold inventory allocation"))?;
    for descriptor in descriptors.as_slice() {
        if descriptor.family != 17 {
            return Err(NativeGraphError::Invalid("legacy fold object"));
        }
        source.validate_descriptor(*descriptor, &mut tree)?;
        objects.push(crate::manifest::GraphObject {
            artifact: descriptor.artifact,
            length: u64::from(descriptor.bytes),
            checksum: descriptor.checksum,
        });
    }
    objects.sort_unstable_by_key(|object| object.artifact);
    Ok(objects)
}

impl super::NativeGraphPublication {
    pub(crate) fn has_pending_reclaim(&self) -> Result<bool, crate::lifecycle::StoreError> {
        let state =
            self.state
                .lock()
                .map_err(|_| crate::lifecycle::StoreError::Synchronization {
                    component: "native graph publication",
                })?;
        let Some(bundle) = state.current.as_ref() else {
            return Ok(false);
        };
        let Some(reclaim) = bundle.reclaim() else {
            return Ok(false);
        };
        let payload = bundle
            .vfs()
            .read_range(
                &crate::property_graph::storage::allocation::artifact_path(
                    bundle.directory(),
                    reclaim.object.artifact,
                ),
                reclaim
                    .block
                    .offset
                    .checked_add(24)
                    .ok_or(crate::lifecycle::StoreError::ActiveRowOverflow)?,
                10,
            )
            .map_err(|source| crate::lifecycle::StoreError::Io {
                path: bundle.directory().to_path_buf(),
                source,
            })?;
        if payload.get(..4) != Some(b"ZGCP".as_slice())
            || payload.get(4..6) != Some(&5_u16.to_le_bytes())
            || payload.get(6..8) != Some(&1_u16.to_le_bytes())
        {
            return Err(crate::lifecycle::StoreError::Manifest(
                crate::manifest::ManifestError::Decode("reclaim phase header".into()),
            ));
        }
        match payload.get(8..10) {
            Some(bytes) if bytes == 2_u16.to_le_bytes() => Ok(true),
            Some(bytes) if bytes == 3_u16.to_le_bytes() => Ok(false),
            _ => Err(crate::lifecycle::StoreError::Manifest(
                crate::manifest::ManifestError::Decode("reclaim phase role".into()),
            )),
        }
    }
}
