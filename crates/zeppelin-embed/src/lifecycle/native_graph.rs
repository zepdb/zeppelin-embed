//! Writes-owned native graph admission and retained-root registration.

#![allow(
    dead_code,
    reason = "the admitted read seam is intentionally crate-private until ZE-50 consumes it"
)]

use super::{Store, StoreError, StoreState};
use crate::epoch::EmbeddingTower;
use crate::format::FormatFamily;
use crate::fts::tokenizer::TokenizerEpoch;
use crate::property_graph::query::QueryView;
use crate::property_graph::query::runtime::RetainedView;
use crate::property_graph::resources::{GraphReservation, GraphResources};
use crate::property_graph::staging::BaseIdentity;
use crate::property_graph::storage::artifact::{BlockKind, PhysicalRef};
use crate::property_graph::storage::tree::directory::GraphRoots;
use crate::property_graph::wal::{
    ArtifactDescriptor, HighWaters, InventoryChange, RequiredRef, WalGraphRoots,
};
use crate::vfs::Vfs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Instant;

mod base;
mod persistence;
mod recovery;
mod write;

const MAX_NATIVE_READ_LEASES: usize = 1024;
const MAX_NATIVE_READ_MAPPINGS: usize = MAX_NATIVE_READ_LEASES * 16;
const MAX_NATIVE_PREPARATIONS: usize = MAX_NATIVE_READ_LEASES;

/// Native lifecycle rejection, kept separate from tree/query data errors.
#[derive(Debug)]
pub(crate) enum NativeGraphError {
    Store(StoreError),
    Invalid(&'static str),
    NotInstalled,
    LeaseLimit,
    IdentityExhausted,
    Read(crate::property_graph::storage::tree::directory::TreeError),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Catalog(crate::property_graph::catalog::CatalogError),
    Wal(crate::property_graph::wal::WalError),
    Stage(crate::property_graph::staging::StageError),
    CommitIndeterminate {
        stage: &'static str,
        path: PathBuf,
        source: Option<std::io::Error>,
    },
    WritesStopped,
    ReadAdmissionsStopped,
    CheckpointRequired,
    StalePreparation,
    StoreInitializationIncomplete,
}

impl From<StoreError> for NativeGraphError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl std::fmt::Display for NativeGraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => error.fmt(f),
            Self::Invalid(reason) => write!(f, "invalid native graph bundle: {reason}"),
            Self::NotInstalled => f.write_str("native graph bundle is not installed"),
            Self::LeaseLimit => f.write_str("native graph read lease capacity exhausted"),
            Self::IdentityExhausted => f.write_str("native graph read identity exhausted"),
            Self::Read(error) => error.fmt(f),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Catalog(error) => error.fmt(f),
            Self::Wal(error) => error.fmt(f),
            Self::Stage(error) => error.fmt(f),
            Self::CommitIndeterminate {
                stage,
                path,
                source,
            } => {
                write!(
                    f,
                    "native graph commit is indeterminate at {stage}: {}",
                    path.display()
                )?;
                if let Some(source) = source {
                    write!(f, ": {source}")?;
                }
                Ok(())
            }
            Self::WritesStopped => f.write_str("native graph writes require recovery"),
            Self::ReadAdmissionsStopped => {
                f.write_str("native graph read admissions require recovery")
            }
            Self::CheckpointRequired => f.write_str("native graph checkpoint retry is required"),
            Self::StalePreparation => f.write_str("native graph maintenance preparation is stale"),
            Self::StoreInitializationIncomplete => {
                f.write_str("native graph store initialization is incomplete")
            }
        }
    }
}

impl std::error::Error for NativeGraphError {}

impl From<crate::property_graph::storage::tree::directory::TreeError> for NativeGraphError {
    fn from(error: crate::property_graph::storage::tree::directory::TreeError) -> Self {
        Self::Read(error)
    }
}

impl From<crate::property_graph::catalog::CatalogError> for NativeGraphError {
    fn from(error: crate::property_graph::catalog::CatalogError) -> Self {
        Self::Catalog(error)
    }
}

impl From<crate::property_graph::wal::WalError> for NativeGraphError {
    fn from(error: crate::property_graph::wal::WalError) -> Self {
        Self::Wal(error)
    }
}

impl From<crate::property_graph::staging::StageError> for NativeGraphError {
    fn from(error: crate::property_graph::staging::StageError) -> Self {
        Self::Stage(error)
    }
}

pub(crate) trait NativeReadConsumer<T> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut crate::property_graph::query::runtime::RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<T, crate::property_graph::storage::tree::directory::TreeError>;
}

/// Complete metadata transferred only by the writes/recovery owner. Tests use
/// the same value through the controlled installer; it has no publish method.
pub(crate) struct NativeGraphBundleInput {
    pub(crate) base: BaseIdentity,
    pub(crate) root_envelope: RequiredRef,
    pub(crate) roots: GraphRoots,
    pub(crate) wal_roots: WalGraphRoots,
    pub(crate) sequence: u64,
    pub(crate) catalog: RequiredRef,
    pub(crate) vector: Option<RequiredRef>,
    pub(crate) text: Option<RequiredRef>,
    pub(crate) reclaim: Option<RequiredRef>,
    pub(crate) high_waters: HighWaters,
    pub(crate) prepared_inventories: Vec<RequiredRef>,
    pub(crate) lexical: TokenizerEpoch,
    pub(crate) document: Option<EmbeddingTower>,
}

/// One immutable coherent native generation. It retains the exact VFS and root
/// directory selected during installation, so old lazy opens never consult the
/// replacement bundle.
pub(crate) struct NativeGraphBundle {
    base: BaseIdentity,
    root_envelope: RequiredRef,
    roots: GraphRoots,
    wal_roots: WalGraphRoots,
    sequence: u64,
    catalog: RequiredRef,
    vector: Option<RequiredRef>,
    text: Option<RequiredRef>,
    reclaim: Option<RequiredRef>,
    high_waters: HighWaters,
    prepared_inventories: Vec<RequiredRef>,
    lexical: TokenizerEpoch,
    document: Option<EmbeddingTower>,
    directory: PathBuf,
    vfs: Arc<dyn Vfs>,
    _charge: GraphReservation,
}

impl NativeGraphBundle {
    fn install(
        store: &Store,
        resources: &GraphResources,
        input: NativeGraphBundleInput,
    ) -> Result<Arc<Self>, NativeGraphError> {
        Self::install_checked(store, resources, input, false)
    }

    pub(super) fn install_recovered(
        store: &Store,
        resources: &GraphResources,
        input: NativeGraphBundleInput,
    ) -> Result<Arc<Self>, NativeGraphError> {
        Self::install_checked(store, resources, input, true)
    }

    fn install_checked(
        store: &Store,
        resources: &GraphResources,
        input: NativeGraphBundleInput,
        historical_checkpoint: bool,
    ) -> Result<Arc<Self>, NativeGraphError> {
        validate_bundle(&input, historical_checkpoint)?;
        let metadata_bytes = bundle_owned_bytes(&input, &store.directory)?;
        let charge = resources.reserve(metadata_bytes)?;
        let mut directory = PathBuf::new();
        directory
            .try_reserve(store.directory.as_os_str().len())
            .map_err(|_| {
                NativeGraphError::Store(StoreError::AllocationFailed {
                    needed: store.directory.as_os_str().len() as u64,
                    component: "native graph bundle path",
                })
            })?;
        directory.push(&store.directory);
        let NativeGraphBundleInput {
            base,
            root_envelope,
            roots,
            wal_roots,
            sequence,
            catalog,
            vector,
            text,
            reclaim,
            high_waters,
            prepared_inventories,
            lexical,
            document,
        } = input;
        Ok(Arc::new(Self {
            base,
            root_envelope,
            roots,
            wal_roots,
            sequence,
            catalog,
            vector,
            text,
            reclaim,
            high_waters,
            prepared_inventories,
            lexical,
            document,
            directory,
            vfs: Arc::clone(&store.vfs),
            _charge: charge,
        }))
    }

    fn assemble_committed(
        store: &Store,
        resources: &GraphResources,
        admitted: &Arc<Self>,
        input: NativeGraphBundleInput,
    ) -> Result<Arc<Self>, NativeGraphError> {
        let expected_generation = admitted
            .base
            .generation
            .get()
            .checked_add(1)
            .ok_or(NativeGraphError::IdentityExhausted)?;
        let expected_sequence = admitted
            .sequence
            .checked_add(1)
            .ok_or(NativeGraphError::IdentityExhausted)?;
        if input.base.store != admitted.base.store
            || input.base.generation.get() != expected_generation
            || input.base.roots != admitted.base.roots
            || input.root_envelope != admitted.root_envelope
            || input.roots.store() != input.base.store
            || input.roots.generation() != input.base.generation
            || input.sequence != expected_sequence
            || input.root_envelope.object.generation > input.base.generation
        {
            return Err(NativeGraphError::Invalid(
                "unproved native committed transition",
            ));
        }
        for (root, required) in input
            .roots
            .references()
            .into_iter()
            .zip(input.wal_roots.slots)
        {
            if root != required.map(|value| value.block) {
                return Err(NativeGraphError::Invalid(
                    "committed transition root mismatch",
                ));
            }
            if let Some(required) = required {
                validate_object_ref(input.base, required)?;
            }
        }
        for required in [Some(input.catalog), input.vector, input.text, input.reclaim]
            .into_iter()
            .flatten()
            .chain(input.prepared_inventories.iter().copied())
        {
            validate_object_ref(input.base, required)?;
        }
        let metadata_bytes = bundle_owned_bytes(&input, &store.directory)?;
        let charge = resources.reserve(metadata_bytes)?;
        let mut directory = PathBuf::new();
        directory
            .try_reserve(store.directory.as_os_str().len())
            .map_err(|_| {
                NativeGraphError::Store(StoreError::AllocationFailed {
                    needed: store.directory.as_os_str().len() as u64,
                    component: "native graph bundle path",
                })
            })?;
        directory.push(&store.directory);
        Ok(Arc::new(Self {
            base: input.base,
            root_envelope: input.root_envelope,
            roots: input.roots,
            wal_roots: input.wal_roots,
            sequence: input.sequence,
            catalog: input.catalog,
            vector: input.vector,
            text: input.text,
            reclaim: input.reclaim,
            high_waters: input.high_waters,
            prepared_inventories: input.prepared_inventories,
            lexical: input.lexical,
            document: input.document,
            directory,
            vfs: Arc::clone(&store.vfs),
            _charge: charge,
        }))
    }

    fn checkpoint_transition(
        store: &Store,
        resources: &GraphResources,
        admitted: &Arc<Self>,
        root_envelope: RequiredRef,
    ) -> Result<Arc<Self>, NativeGraphError> {
        if root_envelope.object.store != admitted.base.store
            || root_envelope.object.generation != admitted.base.generation
            || root_envelope.object.family != FormatFamily::NativeGraphRoot.id()
            || root_envelope.object.version != 1
            || root_envelope.block.artifact != root_envelope.object.artifact
            || root_envelope.block.kind != BlockKind::CheckpointPayload
            || root_envelope.block.version != 1
        {
            return Err(NativeGraphError::Invalid(
                "unproved native checkpoint transition",
            ));
        }
        Self::install(
            store,
            resources,
            NativeGraphBundleInput {
                base: BaseIdentity {
                    store: admitted.base.store,
                    generation: admitted.base.generation,
                    roots: Some(root_envelope.object.artifact),
                },
                root_envelope,
                roots: admitted.roots,
                wal_roots: admitted.wal_roots,
                sequence: admitted.sequence,
                catalog: admitted.catalog,
                vector: admitted.vector,
                text: admitted.text,
                reclaim: admitted.reclaim,
                high_waters: admitted.high_waters,
                prepared_inventories: admitted.prepared_inventories.clone(),
                lexical: admitted.lexical,
                document: admitted.document.clone(),
            },
        )
    }

    pub(crate) const fn base(&self) -> BaseIdentity {
        self.base
    }

    pub(crate) const fn root_envelope(&self) -> RequiredRef {
        self.root_envelope
    }

    pub(crate) const fn roots(&self) -> GraphRoots {
        self.roots
    }

    pub(crate) const fn wal_roots(&self) -> WalGraphRoots {
        self.wal_roots
    }

    pub(crate) const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub(crate) const fn catalog(&self) -> RequiredRef {
        self.catalog
    }

    pub(crate) const fn vector(&self) -> Option<RequiredRef> {
        self.vector
    }

    pub(crate) const fn text(&self) -> Option<RequiredRef> {
        self.text
    }

    pub(crate) const fn reclaim(&self) -> Option<RequiredRef> {
        self.reclaim
    }

    pub(crate) fn prepared_inventories(&self) -> &[RequiredRef] {
        &self.prepared_inventories
    }

    pub(crate) const fn high_waters(&self) -> HighWaters {
        self.high_waters
    }

    pub(crate) const fn lexical(&self) -> TokenizerEpoch {
        self.lexical
    }

    pub(crate) fn document(&self) -> Option<&EmbeddingTower> {
        self.document.as_ref()
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    pub(crate) fn vfs(&self) -> &dyn Vfs {
        self.vfs.as_ref()
    }

    fn contains(&self, reference: RequiredRef) -> bool {
        self.root_envelope == reference
            || self.catalog == reference
            || self.vector == Some(reference)
            || self.text == Some(reference)
            || self.reclaim == Some(reference)
            || self.wal_roots.slots.contains(&Some(reference))
            || self.prepared_inventories.contains(&reference)
    }

    pub(crate) fn required_object(&self, reference: PhysicalRef) -> Option<RequiredRef> {
        self.wal_roots
            .slots
            .into_iter()
            .flatten()
            .chain([self.catalog])
            .chain(self.vector)
            .chain(self.text)
            .chain(self.reclaim)
            .chain(self.prepared_inventories.iter().copied())
            .find(|required| required.block == reference)
    }
}

fn bundle_owned_bytes(
    input: &NativeGraphBundleInput,
    directory: &Path,
) -> Result<usize, NativeGraphError> {
    let prepared = input
        .prepared_inventories
        .capacity()
        .checked_mul(std::mem::size_of::<RequiredRef>())
        .ok_or(NativeGraphError::Invalid(
            "prepared inventory capacity overflow",
        ))?;
    let document = input.document.as_ref().map_or(0, |document| {
        document.model_id.capacity()
            + document.model_version.capacity()
            + document.weights_digest.capacity()
            + document.prompt_prefix.capacity()
            + document.os_build.as_ref().map_or(0, String::capacity)
    });
    std::mem::size_of::<NativeGraphBundle>()
        .checked_add(2 * std::mem::size_of::<usize>())
        .and_then(|bytes| bytes.checked_add(directory.as_os_str().len()))
        .and_then(|bytes| bytes.checked_add(prepared))
        .and_then(|bytes| bytes.checked_add(document))
        .ok_or(NativeGraphError::Invalid("bundle capacity overflow"))
}

fn validate_bundle(
    input: &NativeGraphBundleInput,
    historical_checkpoint: bool,
) -> Result<(), NativeGraphError> {
    let base = input.base;
    if base.store != input.roots.store()
        || base.generation != input.roots.generation()
        || base.roots != Some(input.root_envelope.object.artifact)
        || input.root_envelope.object.store != base.store
        || if historical_checkpoint {
            input.root_envelope.object.generation > base.generation
        } else {
            input.root_envelope.object.generation != base.generation
        }
        || input.root_envelope.object.family != FormatFamily::NativeGraphRoot.id()
        || input.root_envelope.object.version != 1
        || input.root_envelope.block.artifact != input.root_envelope.object.artifact
        || input.root_envelope.block.kind != BlockKind::CheckpointPayload
        || input.root_envelope.block.version != 1
    {
        return Err(NativeGraphError::Invalid("root envelope identity"));
    }
    for (root, required) in input
        .roots
        .references()
        .into_iter()
        .zip(input.wal_roots.slots)
    {
        if root != required.map(|required| required.block) {
            return Err(NativeGraphError::Invalid("WAL/native root mismatch"));
        }
        if let Some(required) = required {
            validate_object_ref(base, required)?;
            if required.block.kind != BlockKind::TreePage {
                return Err(NativeGraphError::Invalid("native root role"));
            }
        }
    }
    validate_object_ref(base, input.catalog)?;
    if input.catalog.block.kind != BlockKind::CommitParticipant {
        return Err(NativeGraphError::Invalid("catalog role"));
    }
    for reference in input
        .vector
        .into_iter()
        .chain(input.text)
        .chain(input.reclaim)
        .chain(input.prepared_inventories.iter().copied())
    {
        validate_object_ref(base, reference)?;
        if reference.block.kind != BlockKind::CommitParticipant {
            return Err(NativeGraphError::Invalid("participant role"));
        }
    }
    Ok(())
}

fn validate_object_ref(base: BaseIdentity, reference: RequiredRef) -> Result<(), NativeGraphError> {
    if reference.object.store != base.store
        || reference.object.generation > base.generation
        || reference.object.family != FormatFamily::NativeGraphObject.id()
        || reference.object.version != 1
        || reference.block.artifact != reference.object.artifact
        || reference.block.version != 1
    {
        return Err(NativeGraphError::Invalid("required object identity"));
    }
    Ok(())
}

struct RegistryEntry {
    token: u64,
    owner: Weak<NativeReadOwner>,
}

#[derive(Clone, Copy)]
struct NativeMappingEntry {
    token: u64,
    address: usize,
    length: usize,
}

#[derive(Clone, Copy)]
struct NativePreparationEntry {
    token: u64,
    address: usize,
    length: usize,
}

struct PublicationState {
    current: Option<Arc<NativeGraphBundle>>,
    leases: Vec<Option<RegistryEntry>>,
    mappings: Vec<Option<NativeMappingEntry>>,
    preparations: Vec<Option<NativePreparationEntry>>,
    next_token: u64,
    next_mapping_token: u64,
    next_preparation_token: u64,
    creation_serial_fence: u64,
    closing: bool,
    admissions_stopped: bool,
    #[cfg(any(test, feature = "test-support"))]
    admission_hook: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
    #[cfg(any(test, feature = "test-support"))]
    close_owner_hook: Option<(u64, Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
}

pub(crate) struct NativeGraphPublication {
    state: Mutex<PublicationState>,
    changed: Condvar,
    accounting: Arc<super::stats::Accounting>,
    _charge: super::stats::AccountedCounter,
    writer: Mutex<Option<write::NativeWriter>>,
    read_only: AtomicBool,
    #[cfg(any(test, feature = "test-support"))]
    fail_next_publication: AtomicBool,
    #[cfg(any(test, feature = "test-support"))]
    substitute_old_out: AtomicBool,
    #[cfg(all(feature = "allocation-audit", any(test, feature = "test-support")))]
    commit_allocations: std::sync::atomic::AtomicU64,
    #[cfg(all(feature = "allocation-audit", any(test, feature = "test-support")))]
    commit_allocation_denials: std::sync::atomic::AtomicU64,
}

impl NativeGraphPublication {
    pub(crate) fn new(accounting: &Arc<super::stats::Accounting>) -> Result<Arc<Self>, StoreError> {
        let mut charge = super::stats::AccountedCounter::new(
            accounting,
            super::stats::AllocationComponent::Temporary,
        )?;
        let lease_bytes = MAX_NATIVE_READ_LEASES
            .checked_mul(std::mem::size_of::<Option<RegistryEntry>>())
            .ok_or(StoreError::AllocationFailed {
                needed: u64::MAX,
                component: "native graph registry",
            })?;
        let mapping_bytes = MAX_NATIVE_READ_MAPPINGS
            .checked_mul(std::mem::size_of::<Option<NativeMappingEntry>>())
            .ok_or(StoreError::AllocationFailed {
                needed: u64::MAX,
                component: "native graph mapping registry",
            })?;
        let preparation_bytes = MAX_NATIVE_PREPARATIONS
            .checked_mul(std::mem::size_of::<Option<NativePreparationEntry>>())
            .ok_or(StoreError::AllocationFailed {
                needed: u64::MAX,
                component: "native graph preparation registry",
            })?;
        let bytes = std::mem::size_of::<Self>()
            .checked_add(2 * std::mem::size_of::<usize>())
            .and_then(|bytes| bytes.checked_add(lease_bytes))
            .and_then(|bytes| bytes.checked_add(2 * std::mem::size_of::<usize>()))
            .and_then(|bytes| bytes.checked_add(mapping_bytes))
            .and_then(|bytes| bytes.checked_add(2 * std::mem::size_of::<usize>()))
            .and_then(|bytes| bytes.checked_add(preparation_bytes))
            .ok_or(StoreError::AllocationFailed {
                needed: u64::MAX,
                component: "native graph registry",
            })?;
        charge.set(bytes)?;
        let mut leases = Vec::new();
        #[cfg(feature = "allocation-audit")]
        let reserved = crate::allocation_audit::attributed(|| {
            leases.try_reserve_exact(MAX_NATIVE_READ_LEASES)
        });
        #[cfg(not(feature = "allocation-audit"))]
        let reserved = leases.try_reserve_exact(MAX_NATIVE_READ_LEASES);
        reserved.map_err(|_| StoreError::AllocationFailed {
            needed: lease_bytes as u64,
            component: "native graph registry",
        })?;
        leases.resize_with(MAX_NATIVE_READ_LEASES, || None);
        let mut mappings = Vec::new();
        #[cfg(feature = "allocation-audit")]
        let reserved = crate::allocation_audit::attributed(|| {
            mappings.try_reserve_exact(MAX_NATIVE_READ_MAPPINGS)
        });
        #[cfg(not(feature = "allocation-audit"))]
        let reserved = mappings.try_reserve_exact(MAX_NATIVE_READ_MAPPINGS);
        reserved.map_err(|_| StoreError::AllocationFailed {
            needed: mapping_bytes as u64,
            component: "native graph mapping registry",
        })?;
        mappings.resize_with(MAX_NATIVE_READ_MAPPINGS, || None);
        let mut preparations = Vec::new();
        #[cfg(feature = "allocation-audit")]
        let reserved = crate::allocation_audit::attributed(|| {
            preparations.try_reserve_exact(MAX_NATIVE_PREPARATIONS)
        });
        #[cfg(not(feature = "allocation-audit"))]
        let reserved = preparations.try_reserve_exact(MAX_NATIVE_PREPARATIONS);
        reserved.map_err(|_| StoreError::AllocationFailed {
            needed: preparation_bytes as u64,
            component: "native graph preparation registry",
        })?;
        preparations.resize_with(MAX_NATIVE_PREPARATIONS, || None);
        Ok(Arc::new(Self {
            state: Mutex::new(PublicationState {
                current: None,
                leases,
                mappings,
                preparations,
                next_token: 1,
                next_mapping_token: 1,
                next_preparation_token: 1,
                creation_serial_fence: 0,
                closing: false,
                admissions_stopped: false,
                #[cfg(any(test, feature = "test-support"))]
                admission_hook: None,
                #[cfg(any(test, feature = "test-support"))]
                close_owner_hook: None,
            }),
            changed: Condvar::new(),
            accounting: Arc::clone(accounting),
            _charge: charge,
            writer: Mutex::new(None),
            read_only: AtomicBool::new(false),
            #[cfg(any(test, feature = "test-support"))]
            fail_next_publication: AtomicBool::new(false),
            #[cfg(any(test, feature = "test-support"))]
            substitute_old_out: AtomicBool::new(false),
            #[cfg(all(feature = "allocation-audit", any(test, feature = "test-support")))]
            commit_allocations: std::sync::atomic::AtomicU64::new(u64::MAX),
            #[cfg(all(feature = "allocation-audit", any(test, feature = "test-support")))]
            commit_allocation_denials: std::sync::atomic::AtomicU64::new(u64::MAX),
        }))
    }

    pub(super) fn mark_read_only(&self) {
        self.read_only.store(true, Ordering::Release);
    }

    pub(super) fn require_writable(&self) -> Result<(), NativeGraphError> {
        if self.read_only.load(Ordering::Acquire) {
            Err(NativeGraphError::Store(StoreError::ReadOnly))
        } else {
            Ok(())
        }
    }

    fn register_prepared(
        self: &Arc<Self>,
        inventory: &[InventoryChange],
    ) -> Result<NativePreparedRegistration, NativeGraphError> {
        let mut state = self.state.lock().map_err(|_| {
            NativeGraphError::Store(StoreError::Synchronization {
                component: "native graph publication",
            })
        })?;
        if state.closing {
            return Err(NativeGraphError::Store(StoreError::Closing));
        }
        let slot = state
            .preparations
            .iter()
            .position(Option::is_none)
            .ok_or(NativeGraphError::LeaseLimit)?;
        let token = state.next_preparation_token;
        state.next_preparation_token = state
            .next_preparation_token
            .checked_add(1)
            .ok_or(NativeGraphError::IdentityExhausted)?;
        *state
            .preparations
            .get_mut(slot)
            .ok_or(NativeGraphError::LeaseLimit)? = Some(NativePreparationEntry {
            token,
            address: inventory.as_ptr() as usize,
            length: inventory.len(),
        });
        Ok(NativePreparedRegistration {
            publication: Arc::downgrade(self),
            slot,
            token,
        })
    }

    pub(crate) fn register_mapping(
        self: &Arc<Self>,
        range: &[u8],
    ) -> Result<NativeMappingOwnership, NativeGraphError> {
        let bytes = u64::try_from(range.len())
            .map_err(|_| NativeGraphError::Invalid("native graph mapping length exceeds u64"))?;
        let reservation = self.accounting.track_mapping(bytes)?;
        let mut state = self.state.lock().map_err(|_| {
            NativeGraphError::Store(StoreError::Synchronization {
                component: "native graph publication",
            })
        })?;
        let slot = state
            .mappings
            .iter()
            .position(Option::is_none)
            .ok_or(NativeGraphError::LeaseLimit)?;
        let token = state.next_mapping_token;
        state.next_mapping_token = state
            .next_mapping_token
            .checked_add(1)
            .ok_or(NativeGraphError::IdentityExhausted)?;
        *state
            .mappings
            .get_mut(slot)
            .ok_or(NativeGraphError::LeaseLimit)? = Some(NativeMappingEntry {
            token,
            address: range.as_ptr() as usize,
            length: range.len(),
        });
        Ok(NativeMappingOwnership {
            _registration: NativeMappingRegistration {
                publication: Arc::downgrade(self),
                slot,
                token,
            },
            _reservation: reservation,
        })
    }

    pub(crate) fn mapping_stats(&self) -> Result<(u64, u64), StoreError> {
        let state = self.state.lock().map_err(|_| StoreError::Synchronization {
            component: "native graph publication",
        })?;
        let mut count = 0_u64;
        let mut resident = 0_u64;
        for entry in state.mappings.iter().flatten() {
            count = count.checked_add(1).ok_or_else(|| StoreError::Statistics {
                component: "native graph mapped file count",
                source: std::io::Error::other("mapped file count overflow"),
            })?;
            // SAFETY: a mapping registration is removed before its sole mapping
            // owner unmaps this exact range. Holding the publication mutex keeps
            // that removal and unmap from racing this read-only residency probe.
            let range =
                unsafe { std::slice::from_raw_parts(entry.address as *const u8, entry.length) };
            #[cfg(unix)]
            let bytes = crate::sys::memory::mincore_resident_bytes(range);
            #[cfg(windows)]
            let bytes = crate::sys::windows::resident_bytes(range);
            resident = resident
                .checked_add(bytes.map_err(|source| StoreError::Statistics {
                    component: "native graph mapped resident bytes",
                    source,
                })?)
                .ok_or_else(|| StoreError::Statistics {
                    component: "native graph mapped resident bytes",
                    source: std::io::Error::other("mapped resident byte count overflow"),
                })?;
        }
        Ok((count, resident))
    }

    pub(crate) fn drain_writer_for_close(&self) -> Result<(), StoreError> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "native graph writer",
            })?;
        if let Some(writer) = writer.as_mut() {
            writer.stopped = true;
        }
        drop(writer.take());
        Ok(())
    }

    fn install(&self, bundle: Arc<NativeGraphBundle>) -> Result<(), NativeGraphError> {
        let mut state = self.state.lock().map_err(|_| {
            NativeGraphError::Store(StoreError::Synchronization {
                component: "native graph publication",
            })
        })?;
        if state.closing {
            return Err(NativeGraphError::Store(StoreError::Closing));
        }
        state.current = Some(bundle);
        Ok(())
    }

    fn is_current_at_serial(
        &self,
        bundle: &Arc<NativeGraphBundle>,
        serial_fence: u64,
    ) -> Result<bool, NativeGraphError> {
        let state = self.state.lock().map_err(|_| {
            NativeGraphError::Store(StoreError::Synchronization {
                component: "native graph publication",
            })
        })?;
        Ok(state.creation_serial_fence == serial_fence
            && state
                .current
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, bundle)))
    }

    fn burn_creation_serial(&self) -> Result<u64, NativeGraphError> {
        let mut state = self.state.lock().map_err(|_| {
            NativeGraphError::Store(StoreError::Synchronization {
                component: "native graph publication",
            })
        })?;
        state.creation_serial_fence = state
            .creation_serial_fence
            .checked_add(1)
            .ok_or(NativeGraphError::IdentityExhausted)?;
        Ok(state.creation_serial_fence)
    }

    fn serial_fence(&self) -> Result<u64, NativeGraphError> {
        let state = self.state.lock().map_err(|_| {
            NativeGraphError::Store(StoreError::Synchronization {
                component: "native graph publication",
            })
        })?;
        Ok(state.creation_serial_fence)
    }

    fn publish_transition(
        &self,
        admitted: &Arc<NativeGraphBundle>,
        next: Arc<NativeGraphBundle>,
    ) -> Result<(), NativeGraphError> {
        #[cfg(any(test, feature = "test-support"))]
        if self.fail_next_publication.swap(false, Ordering::AcqRel) {
            return Err(NativeGraphError::Invalid(
                "scheduled native graph publication failure",
            ));
        }
        let mut state = self.state.lock().map_err(|_| {
            NativeGraphError::Store(StoreError::Synchronization {
                component: "native graph publication",
            })
        })?;
        let current = state
            .current
            .as_ref()
            .ok_or(NativeGraphError::NotInstalled)?;
        if !Arc::ptr_eq(current, admitted) {
            return Err(NativeGraphError::Invalid(
                "stale native committed transition",
            ));
        }
        state.current = Some(next);
        Ok(())
    }

    fn publish_committed_transition(
        &self,
        transition: write::NativeCommittedTransition<'_>,
    ) -> Result<(), NativeGraphError> {
        let (admitted, next) = transition.into_publication();
        self.publish_transition(&admitted, next)
    }

    fn stop_admissions(&self) -> Result<(), NativeGraphError> {
        let mut state = self.state.lock().map_err(|_| {
            NativeGraphError::Store(StoreError::Synchronization {
                component: "native graph publication",
            })
        })?;
        state.admissions_stopped = true;
        Ok(())
    }

    fn admit(
        self: &Arc<Self>,
        charge: GraphReservation,
    ) -> Result<NativeReadLease, NativeGraphError> {
        let mut state = self.state.lock().map_err(|_| {
            NativeGraphError::Store(StoreError::Synchronization {
                component: "native graph publication",
            })
        })?;
        if state.closing {
            return Err(NativeGraphError::Store(StoreError::Closing));
        }
        if state.admissions_stopped {
            return Err(NativeGraphError::ReadAdmissionsStopped);
        }
        let bundle = Arc::clone(
            state
                .current
                .as_ref()
                .ok_or(NativeGraphError::NotInstalled)?,
        );
        #[cfg(any(test, feature = "test-support"))]
        if let Some((entered, release)) = state.admission_hook.take() {
            entered.wait();
            release.wait();
        }
        let slot = state
            .leases
            .iter()
            .position(Option::is_none)
            .ok_or(NativeGraphError::LeaseLimit)?;
        let token = state.next_token;
        state.next_token = state
            .next_token
            .checked_add(1)
            .ok_or(NativeGraphError::IdentityExhausted)?;
        let owner = Arc::new(NativeReadOwner {
            view: QueryView::new(bundle.base.store, bundle.base.generation),
            bundle,
            _charge: charge,
            cancelled: AtomicBool::new(false),
            registration: NativeReadRegistration {
                publication: Arc::downgrade(self),
                slot,
                token,
            },
        });
        *state
            .leases
            .get_mut(slot)
            .ok_or(NativeGraphError::LeaseLimit)? = Some(RegistryEntry {
            token,
            owner: Arc::downgrade(&owner),
        });
        Ok(NativeReadLease { owner })
    }

    fn capture(
        &self,
        resources: &GraphResources,
    ) -> Result<NativeProtectedRoots, NativeGraphError> {
        let writer = self.writer.lock().map_err(|_| {
            NativeGraphError::Store(StoreError::Synchronization {
                component: "native graph writer",
            })
        })?;
        let state = self.state.lock().map_err(|_| {
            NativeGraphError::Store(StoreError::Synchronization {
                component: "native graph publication",
            })
        })?;
        let capacity = 1_usize
            .checked_add(state.leases.iter().filter(|entry| entry.is_some()).count())
            .ok_or(NativeGraphError::LeaseLimit)?;
        let prepared_capacity = state
            .preparations
            .iter()
            .flatten()
            .try_fold(0_usize, |total, entry| total.checked_add(entry.length))
            .and_then(|total| {
                writer
                    .as_ref()
                    .and_then(|writer| total.checked_add(writer.protected.len()))
                    .or_else(|| writer.is_none().then_some(total))
            })
            .ok_or(NativeGraphError::LeaseLimit)?;
        let bytes = capacity
            .checked_mul(std::mem::size_of::<Arc<NativeGraphBundle>>())
            .and_then(|bytes| {
                prepared_capacity
                    .checked_mul(std::mem::size_of::<ArtifactDescriptor>())
                    .and_then(|prepared| bytes.checked_add(prepared))
            })
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<NativeProtectedRoots>()))
            .ok_or(NativeGraphError::LeaseLimit)?;
        let mut charge = resources.reserve(bytes)?;
        let mut bundles = Vec::new();
        #[cfg(feature = "allocation-audit")]
        let reserved = crate::allocation_audit::attributed(|| bundles.try_reserve_exact(capacity));
        #[cfg(not(feature = "allocation-audit"))]
        let reserved = bundles.try_reserve_exact(capacity);
        reserved.map_err(|_| {
            NativeGraphError::Store(StoreError::AllocationFailed {
                needed: bytes as u64,
                component: "native graph protected roots",
            })
        })?;
        let mut prepared = Vec::new();
        #[cfg(feature = "allocation-audit")]
        let reserved =
            crate::allocation_audit::attributed(|| prepared.try_reserve_exact(prepared_capacity));
        #[cfg(not(feature = "allocation-audit"))]
        let reserved = prepared.try_reserve_exact(prepared_capacity);
        reserved.map_err(|_| {
            NativeGraphError::Store(StoreError::AllocationFailed {
                needed: bytes as u64,
                component: "native graph prepared roots",
            })
        })?;
        if let Some(current) = state.current.as_ref() {
            bundles.push(Arc::clone(current));
        }
        for entry in state.leases.iter().flatten() {
            if let Some(owner) = entry.owner.upgrade()
                && !bundles
                    .iter()
                    .any(|bundle| Arc::ptr_eq(bundle, &owner.bundle))
            {
                bundles.push(Arc::clone(&owner.bundle));
            }
        }
        for entry in state.preparations.iter().flatten() {
            // SAFETY: registration is removed while holding this mutex before
            // its owning charged inventory is dropped. Capture holds the same
            // mutex while copying the immutable finalized descriptors.
            let changes = unsafe {
                std::slice::from_raw_parts(entry.address as *const InventoryChange, entry.length)
            };
            prepared.extend(changes.iter().map(|change| change.object));
        }
        if let Some(writer) = writer.as_ref() {
            prepared.extend(writer.protected.iter().copied());
        }
        let actual = bundles
            .capacity()
            .checked_mul(std::mem::size_of::<Arc<NativeGraphBundle>>())
            .and_then(|bytes| {
                prepared
                    .capacity()
                    .checked_mul(std::mem::size_of::<ArtifactDescriptor>())
                    .and_then(|prepared| bytes.checked_add(prepared))
            })
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<NativeProtectedRoots>()))
            .ok_or(NativeGraphError::LeaseLimit)?;
        charge.resize(actual)?;
        Ok(NativeProtectedRoots {
            bundles,
            prepared,
            wal: writer.as_ref().map(|writer| NativeProtectedWal {
                identity: writer.wal.identity,
                first_sequence: writer.wal.first_sequence,
                bytes: writer.wal.bytes,
            }),
            serial_fence: state.creation_serial_fence,
            _charge: charge,
        })
    }

    pub(crate) fn drain_and_clear(&self, deadline: Option<Instant>) -> Result<(), StoreError> {
        let mut state = self.state.lock().map_err(|_| StoreError::Synchronization {
            component: "native graph publication",
        })?;
        state.closing = true;
        self.changed.notify_all();
        while state.leases.iter().any(Option::is_some) {
            let Some(deadline) = deadline else {
                break;
            };
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let waited = self
                .changed
                .wait_timeout(state, deadline.saturating_duration_since(now))
                .map_err(|_| StoreError::Synchronization {
                    component: "native graph publication",
                })?;
            state = waited.0;
        }
        if state.leases.iter().any(Option::is_some) {
            let slot_count = state.leases.len();
            for index in 0..slot_count {
                let Some((_token, owner)) = state
                    .leases
                    .get(index)
                    .and_then(Option::as_ref)
                    .and_then(|entry| entry.owner.upgrade().map(|owner| (entry.token, owner)))
                else {
                    continue;
                };
                owner.cancelled.store(true, Ordering::Release);
                #[cfg(any(test, feature = "test-support"))]
                if state
                    .close_owner_hook
                    .as_ref()
                    .is_some_and(|(target, _, _)| *target == _token)
                    && let Some((_, entered, release)) = state.close_owner_hook.take()
                {
                    entered.wait();
                    release.wait();
                }
                drop(state);
                drop(owner);
                state = self.state.lock().map_err(|_| StoreError::Synchronization {
                    component: "native graph publication",
                })?;
            }
            self.changed.notify_all();
        }
        while state.leases.iter().any(Option::is_some) {
            state = self
                .changed
                .wait(state)
                .map_err(|_| StoreError::Synchronization {
                    component: "native graph publication",
                })?;
        }
        drop(state.current.take());
        Ok(())
    }

    pub(crate) fn cancel_and_clear_best_effort(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closing = true;
        let slot_count = state.leases.len();
        for index in 0..slot_count {
            let Some((_token, owner)) = state
                .leases
                .get(index)
                .and_then(Option::as_ref)
                .and_then(|entry| entry.owner.upgrade().map(|owner| (entry.token, owner)))
            else {
                continue;
            };
            owner.cancelled.store(true, Ordering::Release);
            #[cfg(any(test, feature = "test-support"))]
            if state
                .close_owner_hook
                .as_ref()
                .is_some_and(|(target, _, _)| *target == _token)
                && let Some((_, entered, release)) = state.close_owner_hook.take()
            {
                entered.wait();
                release.wait();
            }
            drop(state);
            drop(owner);
            state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        drop(state.current.take());
        self.changed.notify_all();
    }
}

struct NativeReadRegistration {
    publication: Weak<NativeGraphPublication>,
    slot: usize,
    token: u64,
}

struct NativeMappingRegistration {
    publication: Weak<NativeGraphPublication>,
    slot: usize,
    token: u64,
}

pub(crate) struct NativePreparedRegistration {
    publication: Weak<NativeGraphPublication>,
    slot: usize,
    token: u64,
}

impl Drop for NativePreparedRegistration {
    fn drop(&mut self) {
        let Some(publication) = self.publication.upgrade() else {
            return;
        };
        let mut state = publication
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .preparations
            .get(self.slot)
            .and_then(Option::as_ref)
            .is_some_and(|entry| entry.token == self.token)
            && let Some(slot) = state.preparations.get_mut(self.slot)
        {
            *slot = None;
        }
    }
}

impl Drop for NativeMappingRegistration {
    fn drop(&mut self) {
        let Some(publication) = self.publication.upgrade() else {
            return;
        };
        let mut state = publication
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .mappings
            .get(self.slot)
            .and_then(Option::as_ref)
            .is_some_and(|entry| entry.token == self.token)
            && let Some(slot) = state.mappings.get_mut(self.slot)
        {
            *slot = None;
        }
    }
}

/// Owns both exact mapped-byte accounting and the live residency probe. Field
/// order removes the probe before releasing mapped bytes; the mapping itself is
/// declared after this owner by `NativeReadonlyMapping` and unmaps last.
pub(crate) struct NativeMappingOwnership {
    _registration: NativeMappingRegistration,
    _reservation: super::stats::MappingReservation,
}

impl Drop for NativeReadRegistration {
    fn drop(&mut self) {
        let Some(publication) = self.publication.upgrade() else {
            return;
        };
        let mut state = publication
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .leases
            .get(self.slot)
            .and_then(Option::as_ref)
            .is_some_and(|entry| entry.token == self.token)
            && let Some(slot) = state.leases.get_mut(self.slot)
        {
            *slot = None;
        }
        drop(state);
        publication.changed.notify_all();
    }
}

struct NativeReadOwner {
    // Drop order matters: protected bytes and query identity disappear before
    // the final registration removal wakes a close waiter.
    bundle: Arc<NativeGraphBundle>,
    view: QueryView,
    _charge: GraphReservation,
    cancelled: AtomicBool,
    registration: NativeReadRegistration,
}

pub(crate) struct NativeReadLease {
    owner: Arc<NativeReadOwner>,
}

impl Clone for NativeReadLease {
    fn clone(&self) -> Self {
        Self {
            owner: Arc::clone(&self.owner),
        }
    }
}

impl NativeReadLease {
    pub(crate) fn bundle(&self) -> &Arc<NativeGraphBundle> {
        &self.owner.bundle
    }

    pub(crate) fn token(&self) -> u64 {
        self.owner.registration.token
    }

    pub(crate) fn belongs_to(
        &self,
        resources: &crate::property_graph::resources::GraphResources,
    ) -> bool {
        self.owner._charge.belongs_to(resources)
    }

    pub(crate) fn check_active(&self) -> Result<(), crate::property_graph::query::QueryError> {
        if self.owner.cancelled.load(Ordering::Acquire) {
            Err(crate::property_graph::query::QueryError::ReadCancelled)
        } else {
            Ok(())
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn wait_until_cancelled_for_test(&self) -> Result<(), NativeGraphError> {
        let publication = self
            .owner
            .registration
            .publication
            .upgrade()
            .ok_or(NativeGraphError::Invalid(
                "native graph publication no longer owns read lease",
            ))?;
        let mut state = publication
            .state
            .lock()
            .map_err(|_| NativeGraphError::Invalid("native graph publication poisoned"))?;
        while self.check_active().is_ok() {
            state = publication
                .changed
                .wait(state)
                .map_err(|_| NativeGraphError::Invalid("native graph cancellation wait poisoned"))?;
        }
        Ok(())
    }

    pub(crate) fn track_mapping(
        &self,
        range: &[u8],
    ) -> Result<NativeMappingOwnership, NativeGraphError> {
        let publication =
            self.owner
                .registration
                .publication
                .upgrade()
                .ok_or(NativeGraphError::Invalid(
                    "native graph publication no longer owns mapping",
                ))?;
        publication.register_mapping(range)
    }

    pub(crate) fn register_prepared(
        &self,
        inventory: &[InventoryChange],
    ) -> Result<NativePreparedRegistration, NativeGraphError> {
        let publication =
            self.owner
                .registration
                .publication
                .upgrade()
                .ok_or(NativeGraphError::Invalid(
                    "native graph publication no longer owns preparation",
                ))?;
        publication.register_prepared(inventory)
    }
}

impl RetainedView for NativeReadLease {
    fn query_view(&self) -> &QueryView {
        &self.owner.view
    }

    fn check_active(&self) -> Result<(), crate::property_graph::query::QueryError> {
        Self::check_active(self)
    }
}

pub(crate) struct NativeProtectedRoots {
    bundles: Vec<Arc<NativeGraphBundle>>,
    prepared: Vec<ArtifactDescriptor>,
    wal: Option<NativeProtectedWal>,
    serial_fence: u64,
    _charge: GraphReservation,
}

pub(crate) struct NativeMaintenanceAdmission {
    lease: NativeReadLease,
    serial_fence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NativeProtectedWal {
    identity: u128,
    first_sequence: u64,
    bytes: usize,
}

impl NativeProtectedRoots {
    fn contains(&self, reference: RequiredRef) -> bool {
        self.bundles.iter().any(|bundle| bundle.contains(reference))
    }

    fn bundle_count(&self) -> usize {
        self.bundles.len()
    }

    fn contains_prepared(&self, object: ArtifactDescriptor) -> bool {
        self.prepared.contains(&object)
    }

    fn wal(&self) -> Option<NativeProtectedWal> {
        self.wal
    }

    fn serial_fence(&self) -> u64 {
        self.serial_fence
    }
}

impl Store {
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn fail_next_native_graph_publication_for_test(&self) {
        self.native_graph
            .fail_next_publication
            .store(true, Ordering::Release);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn install_native_graph_for_test(
        &self,
        input: NativeGraphBundleInput,
    ) -> Result<(), NativeGraphError> {
        let resources = GraphResources::from_store(self)?;
        let bundle = NativeGraphBundle::install(self, &resources, input)?;
        let state = self.state.lock().map_err(|_| {
            NativeGraphError::Store(StoreError::Synchronization { component: "state" })
        })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(NativeGraphError::Store(StoreError::Closing)),
            StoreState::Closed => return Err(NativeGraphError::Store(StoreError::Closed)),
        }
        self.native_graph.install(bundle)
    }

    pub(crate) fn admit_native_read(&self) -> Result<NativeReadLease, NativeGraphError> {
        let state = self.state.lock().map_err(|_| {
            NativeGraphError::Store(StoreError::Synchronization { component: "state" })
        })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(NativeGraphError::Store(StoreError::Closing)),
            StoreState::Closed => return Err(NativeGraphError::Store(StoreError::Closed)),
        }
        drop(state);
        let resources = GraphResources::from_store(self)?;
        let charge = resources
            .reserve(std::mem::size_of::<NativeReadOwner>() + 2 * std::mem::size_of::<usize>())?;
        self.native_graph.admit(charge)
    }

    pub(crate) fn capture_native_read_roots(
        &self,
    ) -> Result<NativeProtectedRoots, NativeGraphError> {
        let state = self.state.lock().map_err(|_| {
            NativeGraphError::Store(StoreError::Synchronization { component: "state" })
        })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(NativeGraphError::Store(StoreError::Closing)),
            StoreState::Closed => return Err(NativeGraphError::Store(StoreError::Closed)),
        }
        drop(state);
        let resources = GraphResources::from_store(self)?;
        self.native_graph.capture(&resources)
    }

    pub(crate) fn with_native_read<T, C: NativeReadConsumer<T>>(
        &self,
        control: &crate::lifecycle::QueryControl,
        limits: crate::property_graph::query::runtime::RuntimeLimits,
        memory_limit: usize,
        source_slots: usize,
        mut consumer: C,
    ) -> Result<T, NativeGraphError> {
        use crate::property_graph::query::resources::QueryMemory;
        use crate::property_graph::query::runtime::{RuntimeContext, RuntimeError};
        use crate::property_graph::storage::tree::directory::TreeResources;
        use crate::property_graph::storage::{
            GraphReadView, NativeCatalog, NativeQuerySource, NativeReadCapability,
        };

        let lease = self.admit_native_read()?;
        self.active_queries.fetch_add(1, Ordering::Relaxed);
        let _active_query = super::ActiveQuery {
            count: &self.active_queries,
        };
        let shared = GraphResources::from_store(self)?;
        let memory = QueryMemory::new(&shared, memory_limit)
            .map_err(RuntimeError::Memory)
            .map_err(crate::property_graph::storage::tree::directory::TreeError::Runtime)?;
        let mut runtime = RuntimeContext::new(&lease, control, &memory, limits)
            .map_err(crate::property_graph::storage::tree::directory::TreeError::Runtime)?;
        let capability = NativeReadCapability::admit(&lease, &runtime)?;
        let mut resources = TreeResources::for_query(&mut runtime)?;
        let source = NativeQuerySource::new(capability, &resources, source_slots)?;
        let catalog = NativeCatalog::open(&source, &mut resources)?;
        drop(resources);
        let view = GraphReadView::new(&source, &catalog)?;
        let result = consumer.consume(&view, &mut runtime)?;
        runtime
            .checkpoint()
            .map_err(crate::property_graph::storage::tree::directory::TreeError::Runtime)?;
        Ok(result)
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) mod tests {
    #![allow(
        dead_code,
        clippy::drop_non_drop,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )]

    mod close_owner;
    mod expression_tests;
    pub(crate) mod publication;
    mod recovery;
    mod retrieval;
    mod sparse;

    #[cfg(feature = "test-support")]
    pub(crate) fn run_recovery_probe(
        seed: u64,
    ) -> crate::graph_recovery_test_support::RecoveryProbeReport {
        recovery::run_actual_probe(seed)
    }

    mod tempfile {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(1);

        pub(super) struct TempDir(PathBuf);

        impl TempDir {
            pub(super) fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        pub(super) fn tempdir() -> std::io::Result<TempDir> {
            let ordinal = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("zeppelin-ze45-{}-{ordinal}", std::process::id()));
            std::fs::create_dir(&path)?;
            Ok(TempDir(path))
        }
    }

    use super::*;
    use crate::epoch::{ComputeUnits, EmbeddingRuntime, Normalization};
    use crate::fts::tokenizer::{TokenizerConfig, TokenizerEpoch};
    use crate::lifecycle::{
        CancelToken, Deadline, ManualMonotonicClock, OpenOptions, QueryControl, Store,
        StoreTestDependencies,
    };
    use crate::property_graph::catalog::{
        CatalogDeclaration, CatalogImage, GraphInterpretation, LabelId, NamespaceId, PropertyKeyId,
        RelTypeId, Symbol, SymbolCatalog, SymbolEntry, SymbolHighWaters,
    };
    use crate::property_graph::query::resources::QueryMemory;
    use crate::property_graph::query::runtime::{RuntimeContext, RuntimeLimits};
    use crate::property_graph::resources::GraphResources;
    use crate::property_graph::staging::BaseIdentity;
    use crate::property_graph::staging::{
        AdmittedBase, BaseEntity, BaseKeyState, HighWaters as StageHighWaters, StageError,
        StructuredOperation, StructuredWrite, WriteControl, WriteImage, WriteLimits, WriteMemory,
        stage_structured,
    };
    use crate::property_graph::storage::adjacency::{NativeGraphBase, prepare_native_graph};
    use crate::property_graph::storage::allocation::artifact_path;
    use crate::property_graph::storage::artifact::{
        self, ArtifactId, ArtifactIdentity, Block, BlockKind, ContainerKind, PhysicalRef,
    };
    use crate::property_graph::storage::memory::StorageMemory;
    use crate::property_graph::storage::participant::{DirectoryBase, PreparationCatalog};
    use crate::property_graph::storage::prepared::{PackLimits, PreparedObjects};
    use crate::property_graph::storage::records::RecordCatalog;
    use crate::property_graph::storage::stream::PayloadSlice;
    use crate::property_graph::storage::tree::directory::GraphRoots;
    use crate::property_graph::storage::tree::directory::{BlockSource, TreeError, TreeResources};
    use crate::property_graph::storage::{
        CursorState, DirectionSelection, GraphReadView, LabelSelection, NativeCatalog,
        NativeQuerySource, NativeReadCapability, NodeCursor, PreparedGraphArtifacts,
        PreparedGraphFailure, RelationshipTypeSelection,
    };
    use crate::property_graph::wal::{ArtifactDescriptor, HighWaters, RequiredRef, WalGraphRoots};
    use crate::property_graph::{
        ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphGeneration, GraphName,
        GraphProperty, GraphRevision, NodeId, NodeRef, PropertyData, PropertyValue,
        StoreInstanceId, with_local_refs,
    };
    use crate::vfs::{CountingVfs, StdVfs, SyncKind, Vfs, VfsFile};
    use std::fs::File;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    struct ScheduledMapVfs {
        calls: AtomicU64,
        fail_at: AtomicU64,
        fires: AtomicU64,
    }

    impl ScheduledMapVfs {
        fn new() -> Self {
            Self {
                calls: AtomicU64::new(0),
                fail_at: AtomicU64::new(0),
                fires: AtomicU64::new(0),
            }
        }

        fn arm_next(&self) -> u64 {
            let scheduled = self.calls.load(Ordering::Relaxed) + 1;
            self.fail_at.store(scheduled, Ordering::Relaxed);
            scheduled
        }

        fn disarm(&self) {
            self.fail_at.store(0, Ordering::Relaxed);
        }
    }

    impl Vfs for ScheduledMapVfs {
        fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
            StdVfs.ensure_directory(path, create)
        }

        fn open(&self, path: &Path) -> std::io::Result<u64> {
            StdVfs.open(path)
        }

        fn open_for_map(&self, path: &Path) -> std::io::Result<File> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
            if self.fail_at.load(Ordering::Relaxed) == call {
                self.fires.fetch_add(1, Ordering::Relaxed);
                return Err(std::io::Error::other("scheduled ZE-45 map-open fault"));
            }
            StdVfs.open_for_map(path)
        }

        fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            StdVfs.read(path)
        }

        fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
            StdVfs.read_range(path, offset, length)
        }

        fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
            StdVfs.write(path, bytes)
        }

        fn create_new(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
            StdVfs.create_new(path, bytes)
        }

        fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
            StdVfs.open_append(path)
        }

        fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            StdVfs.rename(from, to)
        }

        fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
            StdVfs.sync(path, kind)
        }

        fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
            StdVfs.list(directory)
        }

        fn for_each_direct_child(
            &self,
            directory: &Path,
            visitor: &mut dyn FnMut(&Path) -> std::io::Result<()>,
        ) -> std::io::Result<()> {
            StdVfs.for_each_direct_child(directory, visitor)
        }

        fn delete(&self, path: &Path) -> std::io::Result<()> {
            StdVfs.delete(path)
        }
    }

    fn required(
        store: StoreInstanceId,
        generation: GraphGeneration,
        artifact: u128,
        kind: BlockKind,
        family: ContainerKind,
    ) -> RequiredRef {
        let artifact = ArtifactId::new(artifact).unwrap();
        RequiredRef {
            object: ArtifactDescriptor {
                store,
                artifact,
                generation,
                serial: artifact.get() as u64,
                bytes: 128,
                family: match family {
                    ContainerKind::Object => 17,
                    ContainerKind::RootEnvelope => 18,
                },
                version: 1,
                checksum: artifact.get() as u64,
            },
            block: PhysicalRef {
                artifact,
                offset: 96,
                length: 32,
                kind,
                version: 1,
            },
        }
    }

    fn bundle(store: StoreInstanceId, generation: u64, artifact: u128) -> NativeGraphBundleInput {
        let generation = GraphGeneration::new(generation);
        let root_envelope = required(
            store,
            generation,
            artifact,
            BlockKind::CheckpointPayload,
            ContainerKind::RootEnvelope,
        );
        let catalog = required(
            store,
            generation,
            artifact + 1,
            BlockKind::CommitParticipant,
            ContainerKind::Object,
        );
        NativeGraphBundleInput {
            base: BaseIdentity {
                store,
                generation,
                roots: Some(root_envelope.object.artifact),
            },
            root_envelope,
            roots: GraphRoots::from_references(store, generation, [None; 8]).unwrap(),
            wal_roots: WalGraphRoots::default(),
            sequence: generation.get(),
            catalog,
            vector: None,
            text: None,
            reclaim: None,
            high_waters: HighWaters::default(),
            prepared_inventories: Vec::new(),
            lexical: TokenizerEpoch::of(&TokenizerConfig::text_default()),
            document: None,
        }
    }

    fn install_catalog_file(
        directory: &Path,
        store: StoreInstanceId,
        generation: GraphGeneration,
        artifact_value: u128,
    ) -> RequiredRef {
        let mut checkpoint = || Ok(());
        let symbols =
            SymbolCatalog::reconstruct(&[], SymbolHighWaters::default(), 0, 0, &mut checkpoint)
                .unwrap();
        let image = CatalogImage {
            declaration: CatalogDeclaration {
                store,
                node_high_water: 0,
                relationship_high_water: 0,
                interpretation: GraphInterpretation::new(
                    TokenizerEpoch::of(&TokenizerConfig::text_default()),
                    None,
                )
                .unwrap(),
            },
            symbols,
        };
        let inner_length = image.encoded_len(&mut checkpoint).unwrap();
        let mut participant = vec![0; 8 + inner_length];
        participant[..4].copy_from_slice(b"ZGCP");
        participant[4..6].copy_from_slice(&1_u16.to_le_bytes());
        participant[6..8].copy_from_slice(&1_u16.to_le_bytes());
        image
            .encode_into(&mut participant[8..], &mut checkpoint)
            .unwrap();
        let artifact = ArtifactId::new(artifact_value).unwrap();
        let blocks = [Block {
            kind: BlockKind::CommitParticipant,
            payload: &participant,
        }];
        let mut bytes = vec![0; artifact::encoded_len(ContainerKind::Object, &blocks).unwrap()];
        artifact::encode_into(
            ContainerKind::Object,
            ArtifactIdentity {
                store,
                artifact,
                generation,
                creation_serial: 9,
            },
            &blocks,
            &mut bytes,
        )
        .unwrap();
        let frame =
            artifact::decode(ContainerKind::Object, Some((store, artifact)), &bytes).unwrap();
        let block = frame.reference(0).unwrap();
        let checksum = u64::from_le_bytes(*bytes[bytes.len() - 8..].first_chunk::<8>().unwrap());
        let required = RequiredRef {
            object: ArtifactDescriptor {
                store,
                artifact,
                generation,
                serial: 9,
                bytes: bytes.len() as u32,
                family: 17,
                version: 1,
                checksum,
            },
            block,
        };
        std::fs::write(artifact_path(directory, artifact), bytes).unwrap();
        required
    }

    struct EmptyProducerBase {
        identity: BaseIdentity,
        high_waters: StageHighWaters,
        document: Option<EmbeddingTower>,
    }

    struct IncrementalProducerBase {
        identity: BaseIdentity,
        high_waters: StageHighWaters,
        node_a: crate::property_graph::NodeId,
        node_b: crate::property_graph::NodeId,
        canonical: Vec<u8>,
        fingerprint: crate::property_graph::CanonicalFingerprint,
        provenance_a: crate::property_graph::OperationProvenance<'static>,
        provenance_b: crate::property_graph::OperationProvenance<'static>,
        membership: crate::property_graph::staging::Membership,
        document: Option<EmbeddingTower>,
        text: Option<String>,
    }

    impl crate::property_graph::staging::CanonicalSource for IncrementalProducerBase {
        fn read_at(&self, offset: u64, output: &mut [u8]) -> std::io::Result<usize> {
            let offset = usize::try_from(offset).map_err(|_| std::io::ErrorKind::InvalidInput)?;
            let bytes = self
                .canonical
                .get(offset..)
                .ok_or(std::io::ErrorKind::UnexpectedEof)?;
            let count = bytes.len().min(output.len());
            output
                .get_mut(..count)
                .ok_or(std::io::ErrorKind::InvalidInput)?
                .copy_from_slice(bytes.get(..count).ok_or(std::io::ErrorKind::InvalidInput)?);
            Ok(count)
        }
    }

    impl IncrementalProducerBase {
        fn new(input: &NativeGraphBundleInput) -> Self {
            let contents = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
            Self::new_with_contents(
                input,
                &contents,
                crate::property_graph::staging::Membership::default(),
                GraphRevision::new(1).unwrap(),
                None,
            )
        }

        fn new_with_contents(
            input: &NativeGraphBundleInput,
            contents: &CanonicalContents<'_>,
            membership: crate::property_graph::staging::Membership,
            revision: GraphRevision,
            text: Option<&str>,
        ) -> Self {
            use crate::property_graph::{
                CanonicalFingerprint, EntityShape, ExpectedGraphState, GraphOperation,
                OperationFields, OperationProvenance,
            };
            let node_a = crate::property_graph::NodeId::new((1_u128 << 100) + 1).unwrap();
            let node_b = crate::property_graph::NodeId::new((1_u128 << 100) + 2).unwrap();
            let mut canonical = Vec::new();
            contents.write_to(&mut canonical, &mut || Ok(())).unwrap();
            let fingerprint = CanonicalFingerprint::new(
                canonical.len() as u64,
                xxhash_rust::xxh3::xxh3_64(&canonical),
            )
            .unwrap();
            let provenance = |id, key| {
                OperationProvenance::from_fields(
                    Some(1),
                    OperationFields {
                        operation: GraphOperation::StructuredCreate,
                        key: Some(ApplicationKey::new(EntityKind::Node, "app", key).unwrap()),
                        requested_revision: revision,
                        installed_revision: revision,
                        expected: ExpectedGraphState::Absent,
                        incarnation: EntityId::Node(id),
                        delete_mode: None,
                        original_generation: input.base.generation,
                    },
                )
                .unwrap()
            };
            let high_waters = StageHighWaters {
                node: input.high_waters.node,
                relationship: input.high_waters.relationship,
                symbols: crate::property_graph::catalog::SymbolHighWaters {
                    label: input.high_waters.symbols[0],
                    relationship_type: input.high_waters.symbols[1],
                    property: input.high_waters.symbols[2],
                    namespace: input.high_waters.symbols[3],
                },
            };
            let _ = EntityShape::Node;
            Self {
                identity: input.base,
                high_waters,
                node_a,
                node_b,
                canonical,
                fingerprint,
                provenance_a: provenance(node_a, "a"),
                provenance_b: provenance(node_b, "b"),
                membership,
                document: input.document.clone(),
                text: text.map(ToOwned::to_owned),
            }
        }

        fn entity_value(
            &self,
            id: crate::property_graph::NodeId,
        ) -> Option<crate::property_graph::staging::BaseEntity<'_>> {
            let provenance = if id == self.node_a {
                self.provenance_a
            } else if id == self.node_b {
                self.provenance_b
            } else {
                return None;
            };
            Some(crate::property_graph::staging::BaseEntity {
                view: self.identity,
                provenance,
                shape: crate::property_graph::EntityShape::Node,
                fingerprint: self.fingerprint,
                source: self,
                membership: self.membership,
            })
        }
    }

    impl AdmittedBase for IncrementalProducerBase {
        fn identity(&self) -> BaseIdentity {
            self.identity
        }

        fn high_waters(&self) -> StageHighWaters {
            self.high_waters
        }

        fn interpretation(&self) -> GraphInterpretation<'_> {
            GraphInterpretation::new(
                TokenizerEpoch::of(&TokenizerConfig::text_default()),
                self.document.as_ref(),
            )
            .unwrap()
        }

        fn key(
            &self,
            key: ApplicationKey<'_>,
            _: &mut WriteControl<'_>,
        ) -> Result<BaseKeyState<'_>, StageError> {
            if key.namespace().as_str() != "app" {
                return Ok(BaseKeyState::NeverUsed);
            }
            let entity = if key.key().as_str() == "a" {
                self.entity_value(self.node_a)
            } else if key.key().as_str() == "b" {
                self.entity_value(self.node_b)
            } else {
                None
            };
            Ok(entity.map_or(BaseKeyState::NeverUsed, BaseKeyState::Live))
        }

        fn entity(
            &self,
            id: EntityId,
            _: &mut WriteControl<'_>,
        ) -> Result<Option<BaseEntity<'_>>, StageError> {
            Ok(match id {
                EntityId::Node(id) => self.entity_value(id),
                EntityId::Relationship(_) => None,
            })
        }

        fn has_live_incident(
            &self,
            _: crate::property_graph::NodeId,
            _: &[crate::property_graph::RelId],
            _: &mut WriteControl<'_>,
        ) -> Result<bool, StageError> {
            Ok(false)
        }

        fn property(
            &self,
            _: EntityId,
            _: GraphName<'_>,
            _: &mut WriteControl<'_>,
        ) -> Result<Option<PropertyValue<'_>>, StageError> {
            Ok(None)
        }

        fn stored_text(
            &self,
            _: crate::property_graph::NodeId,
            _: &mut WriteControl<'_>,
        ) -> Result<Option<&str>, StageError> {
            Ok(self.text.as_deref())
        }

        fn symbol(
            &self,
            kind: crate::property_graph::catalog::SymbolKind,
            name: GraphName<'_>,
            _: &mut WriteControl<'_>,
        ) -> Result<Option<crate::property_graph::catalog::Symbol>, StageError> {
            use crate::property_graph::catalog::{
                LabelId, NamespaceId, PropertyKeyId, RelTypeId, Symbol, SymbolKind,
            };
            Ok(match (kind, name.as_str()) {
                (SymbolKind::Label, "Label") => Some(Symbol::Label(LabelId::new(1).unwrap())),
                (SymbolKind::RelationshipType, "R") => {
                    Some(Symbol::RelationshipType(RelTypeId::new(1).unwrap()))
                }
                (SymbolKind::RelationshipType, "S") => {
                    Some(Symbol::RelationshipType(RelTypeId::new(2).unwrap()))
                }
                (SymbolKind::Property, "weight") => {
                    Some(Symbol::Property(PropertyKeyId::new(1).unwrap()))
                }
                (SymbolKind::Namespace, "app") => {
                    Some(Symbol::Namespace(NamespaceId::new(1).unwrap()))
                }
                _ => None,
            })
        }
    }

    impl AdmittedBase for EmptyProducerBase {
        fn identity(&self) -> BaseIdentity {
            self.identity
        }

        fn high_waters(&self) -> StageHighWaters {
            self.high_waters
        }

        fn interpretation(&self) -> GraphInterpretation<'_> {
            GraphInterpretation::new(
                TokenizerEpoch::of(&TokenizerConfig::text_default()),
                self.document.as_ref(),
            )
            .unwrap()
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
            _: EntityId,
            _: &mut WriteControl<'_>,
        ) -> Result<Option<BaseEntity<'_>>, StageError> {
            Ok(None)
        }

        fn has_live_incident(
            &self,
            _: crate::property_graph::NodeId,
            _: &[crate::property_graph::RelId],
            _: &mut WriteControl<'_>,
        ) -> Result<bool, StageError> {
            Ok(false)
        }

        fn property(
            &self,
            _: EntityId,
            _: GraphName<'_>,
            _: &mut WriteControl<'_>,
        ) -> Result<Option<PropertyValue<'_>>, StageError> {
            Ok(None)
        }

        fn stored_text(
            &self,
            _: crate::property_graph::NodeId,
            _: &mut WriteControl<'_>,
        ) -> Result<Option<&str>, StageError> {
            Ok(None)
        }

        fn symbol(
            &self,
            _: crate::property_graph::catalog::SymbolKind,
            _: GraphName<'_>,
            _: &mut WriteControl<'_>,
        ) -> Result<Option<crate::property_graph::catalog::Symbol>, StageError> {
            Ok(None)
        }
    }

    struct MissingProducerSource;
    impl BlockSource for MissingProducerSource {
        fn resolve<'a>(
            &'a self,
            _: PhysicalRef,
            _: &mut TreeResources<'_>,
        ) -> Result<
            artifact::FramedBlock<'a>,
            crate::property_graph::storage::tree::directory::TreeError,
        > {
            Err(crate::property_graph::storage::tree::directory::TreeError::Missing)
        }
    }

    struct FixtureFileSource {
        store: StoreInstanceId,
        objects: Vec<(ArtifactId, Vec<u8>)>,
    }

    impl FixtureFileSource {
        fn one(directory: &Path, required: RequiredRef) -> Self {
            Self {
                store: required.object.store,
                objects: vec![(
                    required.object.artifact,
                    std::fs::read(artifact_path(directory, required.object.artifact)).unwrap(),
                )],
            }
        }
    }

    impl BlockSource for FixtureFileSource {
        fn resolve<'a>(
            &'a self,
            reference: PhysicalRef,
            resources: &mut TreeResources<'_>,
        ) -> Result<
            artifact::FramedBlock<'a>,
            crate::property_graph::storage::tree::directory::TreeError,
        > {
            resources.step(1)?;
            let bytes = self
                .objects
                .iter()
                .find(|(artifact, _)| *artifact == reference.artifact)
                .map(|(_, bytes)| bytes.as_slice())
                .ok_or(crate::property_graph::storage::tree::directory::TreeError::Missing)?;
            let frame = artifact::decode(
                ContainerKind::Object,
                Some((self.store, reference.artifact)),
                bytes,
            )?;
            frame.framed_block(reference).map_err(Into::into)
        }
    }

    struct EmptyPreparationCatalog(BaseIdentity);
    impl<S: BlockSource> RecordCatalog<S> for EmptyPreparationCatalog {
        fn resolve(
            &self,
            _: crate::property_graph::catalog::SymbolKind,
            _: PayloadSlice<'_, S>,
            _: &mut TreeResources<'_>,
        ) -> Result<
            crate::property_graph::catalog::Symbol,
            crate::property_graph::storage::tree::directory::TreeError,
        > {
            Err(
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "unexpected base catalog lookup",
                ),
            )
        }
    }
    impl<S: BlockSource> PreparationCatalog<S> for EmptyPreparationCatalog {
        fn base_identity(&self) -> BaseIdentity {
            self.0
        }
    }

    fn write_framed_file(
        directory: &Path,
        kind: ContainerKind,
        identity: ArtifactIdentity,
        blocks: &[Block<'_>],
    ) -> RequiredRef {
        let mut bytes = vec![0; artifact::encoded_len(kind, blocks).unwrap()];
        artifact::encode_into(kind, identity, blocks, &mut bytes).unwrap();
        let frame =
            artifact::decode(kind, Some((identity.store, identity.artifact)), &bytes).unwrap();
        let block = frame.reference(0).unwrap();
        let checksum = u64::from_le_bytes(*bytes[bytes.len() - 8..].first_chunk::<8>().unwrap());
        let required = RequiredRef {
            object: ArtifactDescriptor {
                store: identity.store,
                artifact: identity.artifact,
                generation: identity.generation,
                serial: identity.creation_serial,
                bytes: bytes.len() as u32,
                family: match kind {
                    ContainerKind::Object => 17,
                    ContainerKind::RootEnvelope => 18,
                },
                version: 1,
                checksum,
            },
            block,
        };
        std::fs::write(artifact_path(directory, identity.artifact), bytes).unwrap();
        required
    }

    fn write_catalog_with_symbols(
        directory: &Path,
        identity: ArtifactIdentity,
        staged: &crate::property_graph::staging::StagedBatch<'_>,
        document: Option<&EmbeddingTower>,
    ) -> RequiredRef {
        write_catalog_with_symbols_and_high_waters(
            directory,
            identity,
            staged,
            document,
            staged.high_waters().node,
            staged.high_waters().relationship,
        )
    }

    fn write_catalog_with_symbols_and_high_waters(
        directory: &Path,
        identity: ArtifactIdentity,
        staged: &crate::property_graph::staging::StagedBatch<'_>,
        document: Option<&EmbeddingTower>,
        node_high_water: u128,
        relationship_high_water: u128,
    ) -> RequiredRef {
        let mut checkpoint = || Ok(());
        let fixture_symbols = [
            SymbolEntry {
                symbol: Symbol::Label(LabelId::new(1).unwrap()),
                name: GraphName::new("Label").unwrap(),
            },
            SymbolEntry {
                symbol: Symbol::RelationshipType(RelTypeId::new(1).unwrap()),
                name: GraphName::new("R").unwrap(),
            },
            SymbolEntry {
                symbol: Symbol::RelationshipType(RelTypeId::new(2).unwrap()),
                name: GraphName::new("S").unwrap(),
            },
            SymbolEntry {
                symbol: Symbol::Property(PropertyKeyId::new(1).unwrap()),
                name: GraphName::new("weight").unwrap(),
            },
            SymbolEntry {
                symbol: Symbol::Namespace(NamespaceId::new(1).unwrap()),
                name: GraphName::new("app").unwrap(),
            },
        ];
        let entries = if staged.symbols().is_empty() {
            fixture_symbols.as_slice()
        } else {
            staged.symbols()
        };
        let allowance = std::mem::size_of_val(entries);
        let symbols = SymbolCatalog::reconstruct(
            entries,
            staged.high_waters().symbols,
            entries.len(),
            allowance,
            &mut checkpoint,
        )
        .unwrap();
        let image = CatalogImage {
            declaration: CatalogDeclaration {
                store: identity.store,
                node_high_water,
                relationship_high_water,
                interpretation: GraphInterpretation::new(
                    TokenizerEpoch::of(&TokenizerConfig::text_default()),
                    document,
                )
                .unwrap(),
            },
            symbols,
        };
        let inner_length = image.encoded_len(&mut checkpoint).unwrap();
        let mut participant = vec![0; 8 + inner_length];
        participant[..4].copy_from_slice(b"ZGCP");
        participant[4..6].copy_from_slice(&1_u16.to_le_bytes());
        participant[6..8].copy_from_slice(&1_u16.to_le_bytes());
        image
            .encode_into(&mut participant[8..], &mut checkpoint)
            .unwrap();
        write_framed_file(
            directory,
            ContainerKind::Object,
            identity,
            &[Block {
                kind: BlockKind::CommitParticipant,
                payload: &participant,
            }],
        )
    }

    fn write_complete_sparse_catalog(
        directory: &Path,
        identity: ArtifactIdentity,
        high: StageHighWaters,
        entries: &[SymbolEntry<'_>],
        document: Option<&EmbeddingTower>,
    ) -> RequiredRef {
        let mut checkpoint = || Ok(());
        let allowance = std::mem::size_of_val(entries);
        let symbols = SymbolCatalog::reconstruct(
            entries,
            high.symbols,
            entries.len(),
            allowance,
            &mut checkpoint,
        )
        .unwrap();
        let image = CatalogImage {
            declaration: CatalogDeclaration {
                store: identity.store,
                node_high_water: high.node,
                relationship_high_water: high.relationship,
                interpretation: GraphInterpretation::new(
                    TokenizerEpoch::of(&TokenizerConfig::text_default()),
                    document,
                )
                .unwrap(),
            },
            symbols,
        };
        let inner_length = image.encoded_len(&mut checkpoint).unwrap();
        let mut participant = vec![0; 8 + inner_length];
        participant.get_mut(..4).unwrap().copy_from_slice(b"ZGCP");
        participant
            .get_mut(4..6)
            .unwrap()
            .copy_from_slice(&1_u16.to_le_bytes());
        participant
            .get_mut(6..8)
            .unwrap()
            .copy_from_slice(&1_u16.to_le_bytes());
        image
            .encode_into(participant.get_mut(8..).unwrap(), &mut checkpoint)
            .unwrap();
        write_framed_file(
            directory,
            ContainerKind::Object,
            identity,
            &[Block {
                kind: BlockKind::CommitParticipant,
                payload: &participant,
            }],
        )
    }

    fn actual_producer_bundle_with_dimensions(
        store: &Store,
        directory: &Path,
        identity: StoreInstanceId,
        extra_nodes: usize,
        with_vector: bool,
        extra_relationships: usize,
        max_ids: bool,
        dimensions: usize,
    ) -> NativeGraphBundleInput {
        use crate::property_graph::storage::{GraphPreparation, NativePreparationSource};

        let document = with_vector.then(|| EmbeddingTower {
            model_id: "ze45-document".into(),
            model_version: "1".into(),
            weights_digest: vec![0x45, 0xa5],
            dims: u32::try_from(dimensions).unwrap(),
            normalization: Normalization::None,
            prompt_prefix: "doc: ".into(),
            max_tokens: 32,
            runtime: EmbeddingRuntime::CpuReference,
            compute_units: ComputeUnits::Cpu,
            os_build: None,
        });
        let node_count = 2_u128 + extra_nodes as u128;
        let relationship_count = 3_u128 + extra_relationships as u128;
        let initial_generation = GraphGeneration::new(0);
        let initial_high_waters = StageHighWaters {
            node: if max_ids {
                u128::MAX - node_count
            } else {
                1_u128 << 100
            },
            relationship: if max_ids {
                u128::MAX - relationship_count
            } else {
                1_u128 << 110
            },
            ..StageHighWaters::default()
        };
        let initial_root_identity = ArtifactIdentity {
            store: identity,
            artifact: ArtifactId::new(998).unwrap(),
            generation: initial_generation,
            creation_serial: 1,
        };
        let initial_root = write_framed_file(
            directory,
            ContainerKind::RootEnvelope,
            initial_root_identity,
            &[Block {
                kind: BlockKind::CheckpointPayload,
                payload: b"controlled-fixture-initial-root",
            }],
        );
        let initial_catalog = write_complete_sparse_catalog(
            directory,
            ArtifactIdentity {
                store: identity,
                artifact: ArtifactId::new(999).unwrap(),
                generation: initial_generation,
                creation_serial: 2,
            },
            initial_high_waters,
            &[],
            document.as_ref(),
        );
        store
            .install_native_graph_for_test(NativeGraphBundleInput {
                base: BaseIdentity {
                    store: identity,
                    generation: initial_generation,
                    roots: Some(initial_root_identity.artifact),
                },
                root_envelope: initial_root,
                roots: GraphRoots::from_references(identity, initial_generation, [None; 8])
                    .unwrap(),
                wal_roots: WalGraphRoots::default(),
                sequence: 40,
                catalog: initial_catalog,
                vector: None,
                text: None,
                reclaim: None,
                high_waters: HighWaters {
                    node: initial_high_waters.node,
                    relationship: initial_high_waters.relationship,
                    symbols: [0; 4],
                    creation_serial: 2,
                },
                prepared_inventories: Vec::new(),
                lexical: TokenizerEpoch::of(&TokenizerConfig::text_default()),
                document: document.clone(),
            })
            .unwrap();
        let lease = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(store).unwrap();
        let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let storage = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
        let admitted = EmptyProducerBase {
            identity: lease.bundle().base(),
            high_waters: initial_high_waters,
            document: document.clone(),
        };
        let label = GraphName::new("Label").unwrap();
        let rel_type = GraphName::new("R").unwrap();
        let alternate_rel_type = GraphName::new("S").unwrap();
        let namespace = "app";
        let extra_keys = (0..extra_nodes)
            .map(|index| format!("bulk-{index:04}"))
            .collect::<Vec<_>>();
        let extra_relationship_keys = (0..extra_relationships)
            .map(|index| format!("bulk-rel-{index:04}"))
            .collect::<Vec<_>>();
        with_local_refs(|refs| {
            let mut labels_a = [label];
            let relationship_properties = [GraphProperty::new(
                GraphName::new("weight").unwrap(),
                PropertyValue::new(PropertyData::I64(-17)).unwrap(),
            )];
            let mut coordinates = vec![0.0_f32; dimensions];
            if let Some(first) = coordinates.first_mut() {
                *first = f32::from_bits(0x3f80_0001);
            }
            if let Some(second) = coordinates.get_mut(1) {
                *second = f32::from_bits(0x8000_0000);
            }
            let embedding = document.as_ref().map(|document| {
                crate::property_graph::CanonicalEmbedding::new(document, &coordinates).unwrap()
            });
            let node_a = CanonicalContents::node(&mut labels_a, &mut [], Some(""), None).unwrap();
            let node_b = CanonicalContents::node(&mut [], &mut [], None, embedding).unwrap();
            let mut requests = Vec::with_capacity(5 + extra_nodes + extra_relationships);
            requests.push(StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, namespace, "a").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node_a)),
            });
            requests.push(StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, namespace, "b").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node_b)),
            });
            for key in &extra_keys {
                requests.push(StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, namespace, key).unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&node_b)),
                });
            }
            requests.push(StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, namespace, "ab1").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: rel_type,
                    properties: &[],
                }),
            });
            for key in &extra_relationship_keys {
                requests.push(StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Relationship, namespace, key).unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Relationship {
                        source: NodeRef::Local(refs.node(0).unwrap()),
                        target: NodeRef::Local(refs.node(1).unwrap()),
                        relationship_type: rel_type,
                        properties: &[],
                    }),
                });
            }
            requests.push(StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, namespace, "self").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(0).unwrap()),
                    relationship_type: alternate_rel_type,
                    properties: &[],
                }),
            });
            requests.push(StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, namespace, "ab2").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: rel_type,
                    properties: &relationship_properties,
                }),
            });
            let staged = stage_structured(&admitted, &requests, &writer, &mut |_| Ok(())).unwrap();
            let target_generation = GraphGeneration::new(1);
            let mut next_artifact = 1_000_u128;
            let mut next_serial = 3_u64;
            let source = NativePreparationSource::new(&lease, &storage, 256).unwrap();
            let mut resources = source.resources(u64::MAX).unwrap();
            let preparation = GraphPreparation::new(
                &source,
                target_generation,
                || {
                    let artifact = ArtifactId::new(next_artifact)?;
                    let creation_serial = next_serial;
                    next_artifact += 1;
                    next_serial += 1;
                    Ok(ArtifactIdentity {
                        store: identity,
                        artifact,
                        generation: target_generation,
                        creation_serial,
                    })
                },
                PackLimits {
                    artifact_bytes: crate::property_graph::storage::artifact::MAX_ARTIFACT_BYTES,
                    blocks: 8_192,
                },
                &store.tokenizer,
                &mut resources,
            )
            .unwrap();
            let artifacts =
                preparation
                    .prepare(&staged, &mut resources)
                    .unwrap_or_else(|failure| {
                        panic!(
                            "actual producer preparation failed: {}; current={} peak={}",
                            failure.error(),
                            storage.reserved_bytes(),
                            storage.peak_reserved_bytes()
                        )
                    });
            let roots = artifacts.candidate().roots();
            let sequence = artifacts.candidate().sequence();
            let sparse_roots = artifacts.sparse_roots();
            let descriptors = artifacts
                .inventory()
                .iter()
                .map(|change| change.object)
                .collect::<Vec<_>>();
            for index in 0..artifacts.objects().len() {
                let object = artifacts.objects().artifact(index).unwrap();
                std::fs::write(
                    artifact_path(directory, object.identity().artifact),
                    object.bytes(),
                )
                .unwrap();
            }
            let mut wal_roots = WalGraphRoots::default();
            for (slot, reference) in roots.references().into_iter().enumerate() {
                if let Some(block) = reference {
                    let descriptor = *descriptors
                        .iter()
                        .find(|descriptor| descriptor.artifact == block.artifact)
                        .unwrap();
                    wal_roots.slots[slot] = Some(RequiredRef {
                        object: descriptor,
                        block,
                    });
                }
            }
            let max_serial = descriptors
                .iter()
                .map(|descriptor| descriptor.serial)
                .max()
                .unwrap_or(2);
            let catalog_identity = ArtifactIdentity {
                store: identity,
                artifact: ArtifactId::new(8_001).unwrap(),
                generation: target_generation,
                creation_serial: max_serial + 1,
            };
            let catalog =
                write_catalog_with_symbols(directory, catalog_identity, &staged, document.as_ref());
            let root_identity = ArtifactIdentity {
                store: identity,
                artifact: ArtifactId::new(8_002).unwrap(),
                generation: target_generation,
                creation_serial: max_serial + 2,
            };
            let root_envelope = write_framed_file(
                directory,
                ContainerKind::RootEnvelope,
                root_identity,
                &[Block {
                    kind: BlockKind::CheckpointPayload,
                    payload: b"controlled-fixture-root",
                }],
            );
            NativeGraphBundleInput {
                base: BaseIdentity {
                    store: identity,
                    generation: target_generation,
                    roots: Some(root_identity.artifact),
                },
                root_envelope,
                roots,
                wal_roots,
                sequence,
                catalog,
                vector: sparse_roots.vector,
                text: sparse_roots.text,
                reclaim: None,
                high_waters: HighWaters {
                    node: staged.high_waters().node,
                    relationship: staged.high_waters().relationship,
                    symbols: [
                        staged.high_waters().symbols.label,
                        staged.high_waters().symbols.relationship_type,
                        staged.high_waters().symbols.property,
                        staged.high_waters().symbols.namespace,
                    ],
                    creation_serial: root_identity.creation_serial,
                },
                prepared_inventories: Vec::new(),
                lexical: TokenizerEpoch::of(&TokenizerConfig::text_default()),
                document: document.clone(),
            }
        })
    }

    fn actual_producer_bundle_with_extra_nodes(
        store: &Store,
        directory: &Path,
        identity: StoreInstanceId,
        extra_nodes: usize,
        with_vector: bool,
        extra_relationships: usize,
        max_ids: bool,
    ) -> NativeGraphBundleInput {
        actual_producer_bundle_with_dimensions(
            store,
            directory,
            identity,
            extra_nodes,
            with_vector,
            extra_relationships,
            max_ids,
            2,
        )
    }

    fn actual_producer_bundle(
        store: &Store,
        directory: &Path,
        identity: StoreInstanceId,
    ) -> NativeGraphBundleInput {
        actual_producer_bundle_with_extra_nodes(store, directory, identity, 0, false, 0, false)
    }

    fn current_native_input(store: &Store) -> NativeGraphBundleInput {
        let lease = store.admit_native_read().unwrap();
        let bundle = lease.bundle();
        NativeGraphBundleInput {
            base: bundle.base(),
            root_envelope: bundle.root_envelope(),
            roots: bundle.roots(),
            wal_roots: bundle.wal_roots(),
            sequence: bundle.sequence(),
            catalog: bundle.catalog(),
            vector: bundle.vector(),
            text: bundle.text(),
            reclaim: bundle.reclaim(),
            high_waters: bundle.high_waters(),
            prepared_inventories: bundle.prepared_inventories().to_vec(),
            lexical: bundle.lexical(),
            document: bundle.document().cloned(),
        }
    }

    fn append_actual_relationship_generation(
        store: &Store,
        directory: &Path,
        admitted: &NativeGraphBundleInput,
        count: usize,
    ) -> NativeGraphBundleInput {
        use crate::property_graph::storage::{GraphPreparation, NativePreparationSource};

        let lease = store.admit_native_read().unwrap();
        assert_eq!(lease.bundle().base(), admitted.base);
        let shared = GraphResources::from_store(store).unwrap();
        let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let storage = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
        let base = IncrementalProducerBase::new(admitted);
        let generation_number = admitted.base.generation.get() + 1;
        let keys = (0..count)
            .map(|index| format!("append-g{generation_number}-rel-{index:04}"))
            .collect::<Vec<_>>();
        let node_a = crate::property_graph::NodeId::new((1_u128 << 100) + 1).unwrap();
        let node_b = crate::property_graph::NodeId::new((1_u128 << 100) + 2).unwrap();
        let relationship_type = GraphName::new("R").unwrap();
        let requests = keys
            .iter()
            .map(|key| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "app", key).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Existing(node_a),
                    target: NodeRef::Existing(node_b),
                    relationship_type,
                    properties: &[],
                }),
            })
            .collect::<Vec<_>>();
        let staged = stage_structured(&base, &requests, &writer, &mut |_| Ok(())).unwrap();
        assert!(
            staged.symbols().is_empty(),
            "relationship-only successor introduced new symbols"
        );
        let source = NativePreparationSource::new(&lease, &storage, 256).unwrap();
        let mut resources = source.resources(u64::MAX).unwrap();
        let generation = GraphGeneration::new(admitted.base.generation.get() + 1);
        let mut next_artifact = 50_000_u128 + generation_number as u128 * 100_000;
        let mut next_serial = admitted.high_waters.creation_serial + 1;
        let preparation = GraphPreparation::new(
            &source,
            generation,
            || {
                let identity = ArtifactIdentity {
                    store: admitted.base.store,
                    artifact: ArtifactId::new(next_artifact)?,
                    generation,
                    creation_serial: next_serial,
                };
                next_artifact += 1;
                next_serial += 1;
                Ok(identity)
            },
            PackLimits {
                artifact_bytes: crate::property_graph::storage::artifact::MAX_ARTIFACT_BYTES,
                blocks: 8_192,
            },
            &store.tokenizer,
            &mut resources,
        )
        .unwrap();
        let artifacts = preparation
            .prepare(&staged, &mut resources)
            .unwrap_or_else(|failure| {
                panic!(
                    "incremental preparation failed: {}; current={} peak={}",
                    failure.error(),
                    storage.reserved_bytes(),
                    storage.peak_reserved_bytes()
                )
            });
        assert_eq!(artifacts.membership_changes().len(), count);
        assert!(
            artifacts
                .membership_changes()
                .iter()
                .all(|change| change.node.is_none() && change.membership.is_none()),
            "relationship-only successor acquired sparse membership"
        );
        let mut sparse_sources = 0_usize;
        for object_index in 0..artifacts.objects().len() {
            let object = artifacts.objects().artifact(object_index).unwrap();
            let frame = artifact::decode(ContainerKind::Object, None, object.bytes()).unwrap();
            let mut block_index = 0_usize;
            while let Ok(reference) = frame.reference(block_index) {
                block_index += 1;
                if reference.kind != BlockKind::CommitParticipant {
                    continue;
                }
                let payload = frame.framed_block(reference).unwrap().payload();
                if payload.get(4..6) == Some(6_u16.to_le_bytes().as_slice())
                    && payload.get(8).copied() == Some(2)
                {
                    sparse_sources += 1;
                }
            }
        }
        assert_eq!(
            sparse_sources, 0,
            "relationship-only successor created a sparse source"
        );
        let roots = artifacts.candidate().roots();
        let sequence = artifacts.candidate().sequence();
        let sparse_roots = artifacts.sparse_roots();
        let descriptors = artifacts
            .inventory()
            .iter()
            .map(|change| change.object)
            .collect::<Vec<_>>();
        for index in 0..artifacts.objects().len() {
            let object = artifacts.objects().artifact(index).unwrap();
            std::fs::write(
                artifact_path(directory, object.identity().artifact),
                object.bytes(),
            )
            .unwrap();
        }
        let mut wal_roots = WalGraphRoots::default();
        for (slot, reference) in roots.references().into_iter().enumerate() {
            let Some(block) = reference else {
                continue;
            };
            if let Some(object) = descriptors
                .iter()
                .find(|descriptor| descriptor.artifact == block.artifact)
                .copied()
            {
                wal_roots.slots[slot] = Some(RequiredRef { object, block });
            } else {
                let prior = admitted.wal_roots.slots[slot]
                    .filter(|required| required.block == block)
                    .unwrap_or_else(|| panic!("missing inherited root descriptor for slot {slot}"));
                wal_roots.slots[slot] = Some(prior);
            }
        }
        let max_serial = descriptors
            .iter()
            .map(|descriptor| descriptor.serial)
            .max()
            .unwrap_or(admitted.high_waters.creation_serial);
        let catalog_identity = ArtifactIdentity {
            store: admitted.base.store,
            artifact: ArtifactId::new(50_000_u128 + generation_number as u128 * 100_000 + 99_998)
                .unwrap(),
            generation,
            creation_serial: max_serial + 1,
        };
        let catalog = write_catalog_with_symbols(
            directory,
            catalog_identity,
            &staged,
            admitted.document.as_ref(),
        );
        let root_identity = ArtifactIdentity {
            store: admitted.base.store,
            artifact: ArtifactId::new(50_000_u128 + generation_number as u128 * 100_000 + 99_999)
                .unwrap(),
            generation,
            creation_serial: max_serial + 2,
        };
        let root_envelope = write_framed_file(
            directory,
            ContainerKind::RootEnvelope,
            root_identity,
            &[Block {
                kind: BlockKind::CheckpointPayload,
                payload: b"controlled-incremental-root",
            }],
        );
        let result = NativeGraphBundleInput {
            base: BaseIdentity {
                store: admitted.base.store,
                generation,
                roots: Some(root_identity.artifact),
            },
            root_envelope,
            roots,
            wal_roots,
            sequence,
            catalog,
            vector: sparse_roots.vector,
            text: sparse_roots.text,
            reclaim: admitted.reclaim,
            high_waters: HighWaters {
                node: staged.high_waters().node,
                relationship: staged.high_waters().relationship,
                symbols: [
                    staged.high_waters().symbols.label,
                    staged.high_waters().symbols.relationship_type,
                    staged.high_waters().symbols.property,
                    staged.high_waters().symbols.namespace,
                ],
                creation_serial: root_identity.creation_serial,
            },
            prepared_inventories: admitted.prepared_inventories.clone(),
            lexical: admitted.lexical,
            document: admitted.document.clone(),
        };
        let (candidate, _sparse_roots, _sparse, objects, retained) = artifacts.into_parts();
        drop(candidate);
        drop(_sparse);
        drop(objects);
        drop(retained);
        drop(resources);
        drop(source);
        drop(staged);
        drop(storage);
        drop(writer);
        drop(lease);
        result
    }

    fn tombstoned_endpoint_bundle(
        store: &Store,
        directory: &Path,
        admitted: &NativeGraphBundleInput,
        node: crate::property_graph::NodeId,
    ) -> NativeGraphBundleInput {
        use crate::property_graph::storage::records::{prepare_node_tombstone, prepare_provenance};
        use crate::property_graph::storage::tree::TreeKind;
        use crate::property_graph::storage::tree::directory::{TreeScratch, insert};
        use crate::property_graph::{
            ExpectedGraphState, GraphDeleteMode, GraphOperation, OperationFields,
            OperationProvenance,
        };

        let source = FixtureFileSource::one(
            directory,
            admitted.wal_roots.slots[TreeKind::Nodes as usize - 1].unwrap(),
        );
        let shared = GraphResources::from_store(store).unwrap();
        let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let storage = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
        let target_generation = GraphGeneration::new(admitted.base.generation.get() + 1);
        let mut next_artifact = 9_000_u128;
        let mut next_serial = admitted.high_waters.creation_serial + 1;
        let mut resources = TreeResources::for_prepare(&storage, u64::MAX).unwrap();
        let mut objects = PreparedObjects::new(
            &source,
            || {
                let identity = ArtifactIdentity {
                    store: admitted.base.store,
                    artifact: ArtifactId::new(next_artifact)?,
                    generation: target_generation,
                    creation_serial: next_serial,
                };
                next_artifact += 1;
                next_serial += 1;
                Ok(identity)
            },
            admitted.base.store,
            target_generation,
            PackLimits::default(),
            &storage,
            &mut resources,
        )
        .unwrap();
        let provenance = prepare_provenance(
            &mut objects,
            admitted.base.store,
            target_generation,
            OperationProvenance::from_fields(
                Some(1),
                OperationFields {
                    operation: GraphOperation::CypherEdit,
                    key: None,
                    requested_revision: GraphRevision::new(2).unwrap(),
                    installed_revision: GraphRevision::new(2).unwrap(),
                    expected: ExpectedGraphState::Entity(EntityId::Node(node)),
                    incarnation: EntityId::Node(node),
                    delete_mode: Some(GraphDeleteMode::Detach),
                    original_generation: admitted.base.generation,
                },
            )
            .unwrap(),
            &storage,
            &mut resources,
        )
        .unwrap();
        let tombstone = prepare_node_tombstone(
            &mut objects,
            admitted.base.store,
            target_generation,
            node,
            provenance,
            &mut resources,
        )
        .unwrap();
        let mut value = [0_u8; 48];
        tombstone.encode_into(&mut value).unwrap();
        let mut scratch = TreeScratch::for_prepare(&storage).unwrap();
        let mut roots = admitted.roots.for_generation(target_generation).unwrap();
        let nodes = insert(
            &mut objects,
            roots.directory(TreeKind::Nodes).unwrap(),
            &node.get().to_le_bytes(),
            &value,
            target_generation,
            &mut scratch,
            &mut resources,
        )
        .unwrap();
        roots.replace(nodes).unwrap();
        objects.finish(&mut resources).unwrap();
        let mut descriptors = Vec::new();
        for index in 0..objects.len() {
            let object = objects.artifact(index).unwrap();
            let bytes = object.bytes();
            let checksum =
                u64::from_le_bytes(*bytes[bytes.len() - 8..].first_chunk::<8>().unwrap());
            descriptors.push(ArtifactDescriptor {
                store: admitted.base.store,
                artifact: object.identity().artifact,
                generation: target_generation,
                serial: object.identity().creation_serial,
                bytes: bytes.len() as u32,
                family: 17,
                version: 1,
                checksum,
            });
            std::fs::write(artifact_path(directory, object.identity().artifact), bytes).unwrap();
        }
        let mut wal_roots = admitted.wal_roots;
        let node_root = roots.references()[TreeKind::Nodes as usize - 1].unwrap();
        wal_roots.slots[TreeKind::Nodes as usize - 1] = Some(RequiredRef {
            object: *descriptors
                .iter()
                .find(|descriptor| descriptor.artifact == node_root.artifact)
                .unwrap(),
            block: node_root,
        });
        let root_identity = ArtifactIdentity {
            store: admitted.base.store,
            artifact: ArtifactId::new(9_999).unwrap(),
            generation: target_generation,
            creation_serial: 9_999,
        };
        let root_envelope = write_framed_file(
            directory,
            ContainerKind::RootEnvelope,
            root_identity,
            &[Block {
                kind: BlockKind::CheckpointPayload,
                payload: b"controlled-tombstone-root",
            }],
        );
        NativeGraphBundleInput {
            base: BaseIdentity {
                store: admitted.base.store,
                generation: target_generation,
                roots: Some(root_identity.artifact),
            },
            root_envelope,
            roots,
            wal_roots,
            sequence: admitted.sequence + 1,
            catalog: admitted.catalog,
            vector: admitted.vector,
            text: admitted.text,
            reclaim: admitted.reclaim,
            high_waters: HighWaters {
                creation_serial: 9_999,
                ..admitted.high_waters
            },
            prepared_inventories: admitted.prepared_inventories.clone(),
            lexical: admitted.lexical,
            document: admitted.document.clone(),
        }
    }

    struct NativeNodePull<'view, 'source, 'lease, 'm, 'g> {
        view: &'view GraphReadView<'source, 'lease, 'm, 'g>,
        cursor: Option<NodeCursor<'source, 'm, 'g>>,
        nodes: Option<[crate::property_graph::NodeId; 2]>,
        emitted: usize,
        materialized: bool,
    }

    impl<'view, 'source, 'lease, 'm, 'g>
        crate::property_graph::query::runtime::PullOperator<'lease, 'm, 'g>
        for NativeNodePull<'view, 'source, 'lease, 'm, 'g>
    {
        fn node(&self) -> crate::property_graph::query::plan::PlanNodeId {
            crate::property_graph::query::plan::PlanNodeId(1)
        }

        fn prepare_search(
            &mut self,
            _: crate::property_graph::query::plan::PlanNodeId,
            _: &mut RuntimeContext<'lease, 'm, 'g>,
        ) -> Result<(), crate::property_graph::query::runtime::RuntimeError> {
            Err(crate::property_graph::query::runtime::RuntimeError::Batch)
        }

        fn pull(
            &mut self,
            context: &mut RuntimeContext<'lease, 'm, 'g>,
            output: &mut crate::property_graph::query::runtime::RowBatch<'lease, 'm, 'g>,
        ) -> Result<
            crate::property_graph::query::runtime::PullState,
            crate::property_graph::query::runtime::RuntimeError,
        > {
            use crate::property_graph::query::runtime::{PullState, RuntimeError, WorkKind};
            let placeholder =
                crate::property_graph::NodeId::new(1).map_err(|_| RuntimeError::Batch)?;
            if self.nodes.is_none() {
                let mut nodes = [placeholder; 2];
                let cursor = self.cursor.as_mut().ok_or(RuntimeError::Batch)?;
                let (count, state) = self
                    .view
                    .scan_nodes(cursor, &mut nodes, context)
                    .map_err(|_| RuntimeError::Batch)?;
                if count != nodes.len() || state != CursorState::More {
                    return Err(RuntimeError::Batch);
                }
                self.nodes = Some(nodes);
            }
            if self.emitted > 0 && !self.materialized {
                let mut tail = [placeholder];
                let cursor = self.cursor.as_mut().ok_or(RuntimeError::Batch)?;
                let (count, state) = self
                    .view
                    .scan_nodes(cursor, &mut tail, context)
                    .map_err(|_| RuntimeError::Batch)?;
                if count != 0 || state != CursorState::Done {
                    return Err(RuntimeError::Batch);
                }
                self.cursor = None;
                let first = self
                    .nodes
                    .as_ref()
                    .and_then(|nodes| nodes.first())
                    .copied()
                    .ok_or(RuntimeError::Batch)?;
                let mut resources =
                    TreeResources::for_query(context).map_err(|_| RuntimeError::Batch)?;
                if self
                    .view
                    .lookup_node(first, &mut resources)
                    .map_err(|_| RuntimeError::Batch)?
                    .is_none()
                {
                    return Err(RuntimeError::Batch);
                }
                let text = self
                    .view
                    .stored_text(first, &mut resources)
                    .map_err(|_| RuntimeError::Batch)?
                    .ok_or(RuntimeError::Batch)?;
                if text
                    .read_at(0, &mut [], &mut resources)
                    .map_err(|_| RuntimeError::Batch)?
                    != 0
                {
                    return Err(RuntimeError::Batch);
                }
                self.materialized = true;
            }
            let count = output.capacity().min(257 - self.emitted);
            for index in 0..count {
                let node = self
                    .nodes
                    .as_ref()
                    .and_then(|nodes| nodes.get((self.emitted + index) % nodes.len()))
                    .copied()
                    .ok_or(RuntimeError::Batch)?;
                context.charge(WorkKind::OperatorRows, 1)?;
                output.push_row(&[context.view().node(node)], context)?;
            }
            self.emitted += count;
            Ok(if self.emitted == 257 {
                PullState::Done
            } else {
                PullState::More
            })
        }
    }

    struct FreezeNodeSummary;

    impl<'m, 'g> crate::property_graph::query::runtime::Completion<'m, 'g> for FreezeNodeSummary {
        type Output = (
            usize,
            crate::property_graph::NodeId,
            crate::property_graph::NodeId,
        );

        fn complete<'v>(
            &mut self,
            rows: &crate::property_graph::query::runtime::PreparedRows<'v, 'm, 'g>,
            _: &mut RuntimeContext<'v, 'm, 'g>,
        ) -> Result<
            crate::property_graph::query::runtime::FrozenOutput<Self::Output>,
            crate::property_graph::query::runtime::RuntimeError,
        > {
            use crate::property_graph::query::QueryValue;
            use crate::property_graph::query::runtime::{FrozenOutput, RuntimeError};
            let first = match rows.value(0, 0) {
                Some(QueryValue::NodeRef(node)) => node.id(),
                _ => return Err(RuntimeError::Batch),
            };
            let last = match rows.value(rows.rows().saturating_sub(1), 0) {
                Some(QueryValue::NodeRef(node)) => node.id(),
                _ => return Err(RuntimeError::Batch),
            };
            FrozenOutput::new(
                (rows.rows(), first, last),
                rows.rows(),
                2 * std::mem::size_of::<crate::property_graph::NodeId>(),
                0,
            )
        }
    }

    #[cfg_attr(test, test)]
    fn native_read_admission_registers_before_replacement_capture() {
        let directory = tempfile::tempdir().expect("store directory");
        let store = Arc::new(
            Store::open(
                directory.path(),
                OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
            )
            .expect("open store"),
        );
        let identity = StoreInstanceId::new(1_u128 << 96).unwrap();
        let old = bundle(identity, 1, 11);
        let old_root = old.root_envelope;
        store.install_native_graph_for_test(old).unwrap();

        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        store.native_graph.state.lock().unwrap().admission_hook =
            Some((Arc::clone(&entered), Arc::clone(&release)));
        let read_store = Arc::clone(&store);
        let admitted = std::thread::spawn(move || read_store.admit_native_read().unwrap());
        entered.wait();

        let replace_store = Arc::clone(&store);
        let replacement = std::thread::spawn(move || {
            replace_store
                .install_native_graph_for_test(bundle(identity, 2, 21))
                .unwrap();
            replace_store.capture_native_read_roots().unwrap()
        });
        release.wait();
        let lease = admitted.join().unwrap();
        let captured = replacement.join().unwrap();

        assert!(captured.contains(old_root));
        assert_eq!(captured.bundle_count(), 2);
        drop(lease);
        drop(captured);
        store.close().unwrap();
    }

    #[cfg_attr(test, test)]
    fn native_read_clone_retains_one_registry_entry_until_final_drop() {
        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 95).unwrap();
        store
            .install_native_graph_for_test(bundle(identity, 1, 31))
            .unwrap();
        let resources = GraphResources::from_store(&store).unwrap();
        let installed = resources.reserved_bytes().unwrap();

        let first = store.admit_native_read().unwrap();
        let first_clone = first.clone();
        let second = store.admit_native_read().unwrap();
        assert_eq!(first.token(), first_clone.token());
        assert_ne!(first.token(), second.token());
        assert_eq!(
            store
                .native_graph
                .state
                .lock()
                .unwrap()
                .leases
                .iter()
                .flatten()
                .count(),
            2
        );
        let admitted = resources.reserved_bytes().unwrap();
        assert!(admitted > installed);

        drop(first);
        assert_eq!(
            store
                .native_graph
                .state
                .lock()
                .unwrap()
                .leases
                .iter()
                .flatten()
                .count(),
            2
        );
        drop(first_clone);
        assert_eq!(
            store
                .native_graph
                .state
                .lock()
                .unwrap()
                .leases
                .iter()
                .flatten()
                .count(),
            1
        );
        drop(second);
        assert_eq!(resources.reserved_bytes().unwrap(), installed);
        store.close().unwrap();
    }

    #[cfg_attr(test, test)]
    fn native_read_old_view_lazily_opens_unmapped_artifact_after_replacement() {
        native_read_every_edge_path_checks_both_endpoint_states();
    }

    #[cfg_attr(test, test)]
    fn native_read_missing_or_corrupt_lazy_file_is_not_absence() {
        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 87).unwrap();
        let generation = GraphGeneration::new(1);
        let artifact_id = ArtifactId::new((1_u128 << 81) + 45).unwrap();
        let blocks = [Block {
            kind: BlockKind::PayloadChunk,
            payload: b"required-lazy-file",
        }];
        let mut bytes = vec![0; artifact::encoded_len(ContainerKind::Object, &blocks).unwrap()];
        artifact::encode_into(
            ContainerKind::Object,
            ArtifactIdentity {
                store: identity,
                artifact: artifact_id,
                generation,
                creation_serial: 7,
            },
            &blocks,
            &mut bytes,
        )
        .unwrap();
        let reference =
            artifact::decode(ContainerKind::Object, Some((identity, artifact_id)), &bytes)
                .unwrap()
                .reference(0)
                .unwrap();
        let path = artifact_path(directory.path(), artifact_id);
        std::fs::write(&path, &bytes).unwrap();
        let mut installed = bundle(identity, 1, 161);
        installed.high_waters.creation_serial = 7;
        store.install_native_graph_for_test(installed).unwrap();
        let lease = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let memory = QueryMemory::new(&shared, 2 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut runtime =
            RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(&lease, &runtime).unwrap();
        let mut resources = TreeResources::for_query(&mut runtime).unwrap();
        let source = NativeQuerySource::new(capability, &resources, 2).unwrap();

        std::fs::remove_file(&path).unwrap();
        assert!(matches!(
            source.resolve(reference, &mut resources),
            Err(crate::property_graph::storage::tree::directory::TreeError::Io(_))
        ));
        let last = bytes.len() - 1;
        bytes[last] ^= 0x80;
        std::fs::write(&path, bytes).unwrap();
        assert!(matches!(
            source.resolve(reference, &mut resources),
            Err(crate::property_graph::storage::tree::directory::TreeError::Format(_))
        ));
        drop(source);
        drop(resources);
        drop(runtime);
        drop(lease);
        store.close().unwrap();
    }

    #[cfg_attr(test, test)]
    fn native_read_drop_cancels_without_destroying_borrowed_mapping() {
        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 85).unwrap();
        let generation = GraphGeneration::new(1);
        let artifact_id = ArtifactId::new((1_u128 << 82) + 45).unwrap();
        let blocks = [Block {
            kind: BlockKind::PayloadChunk,
            payload: b"mapping-survives-store-drop",
        }];
        let mut bytes = vec![0; artifact::encoded_len(ContainerKind::Object, &blocks).unwrap()];
        artifact::encode_into(
            ContainerKind::Object,
            ArtifactIdentity {
                store: identity,
                artifact: artifact_id,
                generation,
                creation_serial: 7,
            },
            &blocks,
            &mut bytes,
        )
        .unwrap();
        let reference =
            artifact::decode(ContainerKind::Object, Some((identity, artifact_id)), &bytes)
                .unwrap()
                .reference(0)
                .unwrap();
        std::fs::write(artifact_path(directory.path(), artifact_id), bytes).unwrap();
        let mut installed = bundle(identity, 1, 201);
        installed.high_waters.creation_serial = 7;
        store.install_native_graph_for_test(installed).unwrap();
        let lease = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let memory = QueryMemory::new(&shared, 2 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut runtime =
            RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(&lease, &runtime).unwrap();
        let mut resources = TreeResources::for_query(&mut runtime).unwrap();
        let source = NativeQuerySource::new(capability, &resources, 2).unwrap();
        let borrowed = source.resolve(reference, &mut resources).unwrap();
        drop(store);
        assert_eq!(borrowed.payload(), b"mapping-survives-store-drop");
        assert!(matches!(
            source.resolve(reference, &mut resources),
            Err(
                crate::property_graph::storage::tree::directory::TreeError::Runtime(
                    crate::property_graph::query::runtime::RuntimeError::Value(
                        crate::property_graph::query::QueryError::ReadCancelled
                    )
                )
            )
        ));
        drop(source);
        drop(resources);
        drop(runtime);
        drop(lease);
    }

    #[cfg_attr(test, test)]
    fn native_read_catalog_and_required_refs_cannot_be_substituted() {
        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 86).unwrap();
        let mut wrong_store = bundle(identity, 1, 171);
        wrong_store.catalog.object.store = StoreInstanceId::new(identity.get() + 1).unwrap();
        assert!(matches!(
            store.install_native_graph_for_test(wrong_store),
            Err(NativeGraphError::Invalid("required object identity"))
        ));
        let mut wrong_role = bundle(identity, 1, 181);
        wrong_role.root_envelope.block.kind = BlockKind::TreePage;
        assert!(matches!(
            store.install_native_graph_for_test(wrong_role),
            Err(NativeGraphError::Invalid("root envelope identity"))
        ));
        let mut wrong_root = bundle(identity, 1, 191);
        wrong_root.wal_roots.slots[0] = Some(wrong_root.catalog);
        assert!(matches!(
            store.install_native_graph_for_test(wrong_root),
            Err(NativeGraphError::Invalid("WAL/native root mismatch"))
        ));

        let mut wrong_required = actual_producer_bundle(&store, directory.path(), identity);
        let required = wrong_required
            .wal_roots
            .slots
            .iter_mut()
            .flatten()
            .next()
            .expect("actual producer root");
        required.object.checksum ^= 1;
        store.install_native_graph_for_test(wrong_required).unwrap();
        let lease = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let memory = QueryMemory::new(&shared, 4 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut runtime =
            RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(&lease, &runtime).unwrap();
        let mut resources = TreeResources::for_query(&mut runtime).unwrap();
        let source = NativeQuerySource::new(capability, &resources, 8).unwrap();
        let catalog = NativeCatalog::open(&source, &mut resources).unwrap();
        let view = GraphReadView::new(&source, &catalog).unwrap();
        let node = crate::property_graph::NodeId::new((1_u128 << 100) + 1).unwrap();
        assert!(matches!(
            view.lookup_node(node, &mut resources),
            Err(
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "native graph required descriptor mismatch"
                )
            )
        ));
        drop(view);
        drop(catalog);
        drop(source);
        drop(resources);
        drop(runtime);
        drop(lease);
        store.close().unwrap();
    }

    #[cfg_attr(test, test)]
    fn native_read_close_cancels_and_drains_current_and_retired_leases() {
        let directory = tempfile::tempdir().expect("store directory");
        let store = Arc::new(
            Store::open(
                directory.path(),
                OpenOptions::new()
                    .with_max_resident_bytes(256 * 1024 * 1024)
                    .with_reader_drain_timeout(Duration::ZERO),
            )
            .expect("open store"),
        );
        let identity = StoreInstanceId::new(1_u128 << 93).unwrap();
        store
            .install_native_graph_for_test(bundle(identity, 1, 61))
            .unwrap();
        let retired = store.admit_native_read().unwrap();
        store
            .install_native_graph_for_test(bundle(identity, 2, 71))
            .unwrap();
        let current = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let cancellation_memory = QueryMemory::new(&shared, 2 * 1024 * 1024).unwrap();
        let deadline_memory = QueryMemory::new(&shared, 2 * 1024 * 1024).unwrap();
        let publication = Arc::clone(&store.native_graph);
        let closing_store = Arc::clone(&store);
        let (done_tx, done_rx) = mpsc::channel();
        let closer = std::thread::spawn(move || {
            let result = closing_store.close();
            done_tx.send(result).expect("report close result");
        });

        let mut state = publication.state.lock().unwrap();
        while retired.check_active().is_ok() || current.check_active().is_ok() {
            state = publication.changed.wait(state).unwrap();
        }
        drop(state);
        assert!(
            done_rx.try_recv().is_err(),
            "close returned before leases drained"
        );
        assert!(matches!(
            store.admit_native_read(),
            Err(NativeGraphError::Store(
                StoreError::Closing | StoreError::Closed
            ))
        ));
        let caller_cancel = CancelToken::new();
        caller_cancel.cancel();
        assert!(matches!(
            RuntimeContext::new(
                &retired,
                &QueryControl::Cancel(caller_cancel),
                &cancellation_memory,
                RuntimeLimits::default()
            ),
            Err(crate::property_graph::query::runtime::RuntimeError::Value(
                crate::property_graph::query::QueryError::ReadCancelled
            ))
        ));
        let deadline_clock = Arc::new(ManualMonotonicClock::new());
        let expired = Deadline::after_with_test_clock(Duration::ZERO, deadline_clock).unwrap();
        assert!(matches!(
            RuntimeContext::new(
                &retired,
                &QueryControl::Deadline(expired),
                &deadline_memory,
                RuntimeLimits::default()
            ),
            Err(crate::property_graph::query::runtime::RuntimeError::Value(
                crate::property_graph::query::QueryError::ReadCancelled
            ))
        ));
        drop(retired);
        assert!(
            done_rx.try_recv().is_err(),
            "one retired lease remained live"
        );
        drop(current);
        done_rx.recv().expect("close result").unwrap();
        closer.join().unwrap();
    }

    #[cfg_attr(test, test)]
    fn native_preparation_coordinator_owns_admitted_source_and_finalization() {
        use crate::property_graph::storage::{GraphPreparation, NativePreparationSource};
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .unwrap();
        let identity = StoreInstanceId::new(45001).unwrap();
        let mut input = bundle(identity, 0, 45001);
        input.catalog =
            install_catalog_file(directory.path(), identity, GraphGeneration::new(0), 45002);
        input.root_envelope = write_framed_file(
            directory.path(),
            ContainerKind::RootEnvelope,
            ArtifactIdentity {
                store: identity,
                artifact: ArtifactId::new(45001).unwrap(),
                generation: GraphGeneration::new(0),
                creation_serial: 10,
            },
            &[Block {
                kind: BlockKind::CheckpointPayload,
                payload: b"coordinator-base",
            }],
        );
        input.high_waters.creation_serial = 10;
        store.install_native_graph_for_test(input).unwrap();
        let lease = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let storage = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
        let baseline = storage.reserved_bytes();
        let admitted = EmptyProducerBase {
            identity: lease.bundle().base(),
            high_waters: StageHighWaters::default(),
            document: None,
        };
        let contents =
            CanonicalContents::node(&mut [], &mut [], Some("coordinator"), None).unwrap();
        let requests = [StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "new", "created").unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&contents)),
        }];
        let staged = stage_structured(&admitted, &requests, &writer, &mut |_| Ok(())).unwrap();
        {
            let source = NativePreparationSource::new(&lease, &storage, 8).unwrap();
            let mut resources = source.resources(1_000_000).unwrap();
            let mut next = 45100;
            let prepare = GraphPreparation::new(
                &source,
                GraphGeneration::new(1),
                || {
                    next += 1;
                    Ok(ArtifactIdentity {
                        store: identity,
                        artifact: ArtifactId::new(next)?,
                        generation: GraphGeneration::new(1),
                        creation_serial: next as u64,
                    })
                },
                PackLimits::default(),
                &store.tokenizer,
                &mut resources,
            )
            .unwrap();
            let artifacts = prepare
                .prepare(&staged, &mut resources)
                .unwrap_or_else(|_| panic!("coordinator preparation failed"));
            assert!(artifacts.objects().is_finished());
            assert!(!artifacts.inventory().is_empty());
            assert!(artifacts.sparse_roots().text.is_some());
            assert!(artifacts.sparse_roots().vector.is_none());
            assert_eq!(artifacts.membership_changes().len(), 1);
            assert_eq!(artifacts.membership_changes()[0].ordinal, 0);
            assert_eq!(
                artifacts.membership_changes()[0].node,
                Some(match staged.receipts()[0].entity {
                    EntityId::Node(node) => node,
                    EntityId::Relationship(_) => panic!("created node returned relationship"),
                })
            );
            assert_eq!(
                artifacts.membership_changes()[0].membership,
                Some(crate::property_graph::wal::Membership {
                    text_before: false,
                    text_after: true,
                    vector_before: false,
                    vector_after: false,
                })
            );
            assert_eq!(
                artifacts.expected_root_envelope(),
                lease.bundle().root_envelope()
            );
            assert!(artifacts.matches_base(&lease));
            drop(artifacts);

            let failed = GraphPreparation::new(
                &source,
                GraphGeneration::new(1),
                || {
                    Err(TreeError::Io(std::io::Error::other(
                        "injected coordinator create",
                    )))
                },
                PackLimits::default(),
                &store.tokenizer,
                &mut resources,
            )
            .unwrap();
            let failure = match failed.prepare(&staged, &mut resources) {
                Ok(_) => panic!("injected coordinator creation failure was ignored"),
                Err(failure) => failure,
            };
            assert!(matches!(failure.error(), TreeError::Io(_)));
            let (error, objects, retained) = failure.into_parts();
            assert!(matches!(error, TreeError::Io(_)));
            assert_eq!(objects.abort_inventory().count(), 0);
            assert_eq!(retained.token(), lease.token());
            drop(retained);
            drop(objects);
        }
        assert_eq!(storage.reserved_bytes(), baseline);

        let foreign_directory = tempfile::tempdir().unwrap();
        let foreign_store = Store::open(
            foreign_directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .unwrap();
        let mut foreign_input = bundle(identity, 0, 45201);
        foreign_input.catalog = install_catalog_file(
            foreign_directory.path(),
            identity,
            GraphGeneration::new(0),
            45202,
        );
        foreign_store
            .install_native_graph_for_test(foreign_input)
            .unwrap();
        let foreign_lease = foreign_store.admit_native_read().unwrap();
        assert!(matches!(
            NativePreparationSource::new(&foreign_lease, &storage, 8),
            Err(TreeError::Invalid(
                "native preparation source accounting owner mismatch"
            ))
        ));
        drop(foreign_lease);
        foreign_store.close().unwrap();
        drop(staged);
        drop(storage);
        drop(writer);
        drop(lease);
        store.close().unwrap();
    }

    fn prepared_artifact_contract() {
        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 88).unwrap();
        let installed = bundle(identity, 0, 141);
        let base_identity = installed.base;
        let base_roots = installed.roots;
        let base_sequence = installed.sequence;
        let base_wal_roots = installed.wal_roots;
        let base_catalog = installed.catalog;
        store.install_native_graph_for_test(installed).unwrap();
        let lease = store.admit_native_read().unwrap();
        let foreign = store.admit_native_read().unwrap();
        assert_ne!(lease.token(), foreign.token());

        let shared = GraphResources::from_store(&store).unwrap();
        let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let storage = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
        let admitted = EmptyProducerBase {
            identity: base_identity,
            high_waters: StageHighWaters::default(),
            document: None,
        };
        let mut labels = [];
        let mut properties = [];
        let contents =
            CanonicalContents::node(&mut labels, &mut properties, Some("prepared"), None).unwrap();
        let requests = [StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "prepared").unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&contents)),
        }];
        let staged = stage_structured(&admitted, &requests, &writer, &mut |_| Ok(())).unwrap();
        let mut resources = TreeResources::for_prepare(&storage, 1_000_000).unwrap();
        let mut next = 9_000_u128;
        let mut objects = PreparedObjects::new(
            &MissingProducerSource,
            || {
                let artifact = ArtifactId::new(next)?;
                next += 1;
                Ok(ArtifactIdentity {
                    store: identity,
                    artifact,
                    generation: GraphGeneration::new(1),
                    creation_serial: next as u64,
                })
            },
            identity,
            GraphGeneration::new(1),
            PackLimits::default(),
            &storage,
            &mut resources,
        )
        .unwrap();
        let committed = crate::property_graph::wal::CommitState {
            store: identity,
            generation: GraphGeneration::new(0),
            sequence: base_sequence,
            graph: base_wal_roots,
            catalog: base_catalog,
            vector: None,
            text: None,
            reclaim: None,
            high_waters: HighWaters::default(),
            prepared_inventories: crate::property_graph::wal::ReferenceList::Values(&[]),
        };
        let candidate = prepare_native_graph(
            &mut objects,
            &staged,
            NativeGraphBase {
                directories: DirectoryBase {
                    identity: base_identity,
                    roots: base_roots,
                },
                committed,
            },
            &EmptyPreparationCatalog(base_identity),
            None,
            &storage,
            &mut resources,
        )
        .unwrap();
        let batch_catalog = crate::property_graph::storage::participant::BatchCatalog {
            base: &EmptyPreparationCatalog(base_identity),
            additions: staged.symbols(),
        };
        let sparse = crate::property_graph::storage::search::prepare_sparse(
            &mut objects,
            &staged,
            &candidate,
            &batch_catalog,
            &store.tokenizer,
            &lease,
            &storage,
            &mut resources,
        )
        .unwrap();
        objects.finish(&mut resources).unwrap();
        let artifacts =
            PreparedGraphArtifacts::new(candidate, sparse, objects, &lease, lease.clone())
                .unwrap_or_else(|_| panic!("exact retained base must be accepted"));
        store
            .install_native_graph_for_test(bundle(identity, 2, 151))
            .unwrap();
        assert_eq!(artifacts.candidate().expected_base(), base_identity);
        assert_eq!(artifacts.candidate().expected_sequence(), base_sequence);
        assert_eq!(artifacts.candidate().expected_roots(), base_wal_roots);
        assert!(
            artifacts
                .candidate()
                .roots()
                .references()
                .iter()
                .any(Option::is_some)
        );
        assert!(artifacts.candidate().sequence() > base_sequence);
        assert!(!artifacts.objects().is_empty());
        assert_eq!(
            artifacts.abort_inventory().count(),
            artifacts.objects().len()
        );
        assert_eq!(artifacts.inventory().len(), artifacts.objects().len());
        let protected = store.capture_native_read_roots().unwrap();
        for change in artifacts.inventory() {
            assert_eq!(
                change.state,
                crate::property_graph::wal::InventoryState::Prepared
            );
            assert!(protected.contains_prepared(change.object));
        }
        drop(protected);
        let (_candidate, _sparse_roots, _sparse, _objects, retained) = artifacts.into_parts();
        assert_eq!(retained.bundle().base(), base_identity);
        drop(retained);

        let mut next_foreign = 9_100_u128;
        let mut foreign_objects = PreparedObjects::new(
            &MissingProducerSource,
            || {
                let artifact = ArtifactId::new(next_foreign)?;
                next_foreign += 1;
                Ok(ArtifactIdentity {
                    store: identity,
                    artifact,
                    generation: GraphGeneration::new(1),
                    creation_serial: next_foreign as u64,
                })
            },
            identity,
            GraphGeneration::new(1),
            PackLimits::default(),
            &storage,
            &mut resources,
        )
        .unwrap();
        let foreign_candidate = prepare_native_graph(
            &mut foreign_objects,
            &staged,
            NativeGraphBase {
                directories: DirectoryBase {
                    identity: base_identity,
                    roots: base_roots,
                },
                committed,
            },
            &EmptyPreparationCatalog(base_identity),
            None,
            &storage,
            &mut resources,
        )
        .unwrap();
        let foreign_batch_catalog = crate::property_graph::storage::participant::BatchCatalog {
            base: &EmptyPreparationCatalog(base_identity),
            additions: staged.symbols(),
        };
        let foreign_sparse = crate::property_graph::storage::search::prepare_sparse(
            &mut foreign_objects,
            &staged,
            &foreign_candidate,
            &foreign_batch_catalog,
            &store.tokenizer,
            &foreign,
            &storage,
            &mut resources,
        )
        .unwrap();
        foreign_objects.finish(&mut resources).unwrap();
        let failure = match PreparedGraphArtifacts::new(
            foreign_candidate,
            foreign_sparse,
            foreign_objects,
            &lease,
            lease.clone(),
        ) {
            Ok(_) => panic!("foreign sparse preparation owner was accepted"),
            Err(failure) => failure,
        };
        assert!(matches!(
            failure.error(),
            crate::property_graph::storage::tree::directory::TreeError::Invalid(
                "prepared native graph base mismatch"
            )
        ));
        let (_error, failed_objects, retained) = failure.into_parts();
        assert!(!failed_objects.is_empty());
        assert_eq!(
            failed_objects.abort_inventory().count(),
            failed_objects.len()
        );
        assert!(failed_objects.is_finished());
        assert_eq!(retained.token(), lease.token());
        assert_eq!(retained.bundle().base().generation, GraphGeneration::new(0));
        drop(retained);
        drop(foreign);
        drop(lease);
        store.close().unwrap();
    }

    fn prepared_failure_ownership_contract() {
        use crate::property_graph::storage::tree::directory::{BlockSink, TreeError};

        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 87).unwrap();
        let generation = GraphGeneration::new(1);
        store
            .install_native_graph_for_test(bundle(identity, 1, 281))
            .unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let storage = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
        let baseline = storage.reserved_bytes();

        {
            let mut resources = TreeResources::for_prepare(&storage, u64::MAX).unwrap();
            let mut objects = PreparedObjects::new(
                &MissingProducerSource,
                || {
                    Err(TreeError::Io(std::io::Error::other(
                        "injected pack creation",
                    )))
                },
                identity,
                generation,
                PackLimits::default(),
                &storage,
                &mut resources,
            )
            .unwrap();
            assert!(matches!(
                objects.append(
                    BlockKind::PayloadChunk,
                    generation,
                    b"creation",
                    &mut resources
                ),
                Err(TreeError::Io(_))
            ));
            assert_eq!(objects.abort_inventory().count(), 0);
            assert!(
                objects.artifact(0).is_err(),
                "no candidate artifact escaped"
            );
        }
        assert_eq!(storage.reserved_bytes(), baseline);

        {
            let mut next = 20_000_u128;
            let mut setup = TreeResources::for_prepare(&storage, u64::MAX).unwrap();
            let mut objects = PreparedObjects::new(
                &MissingProducerSource,
                || {
                    let artifact = ArtifactId::new(next)?;
                    next += 1;
                    Ok(ArtifactIdentity {
                        store: identity,
                        artifact,
                        generation,
                        creation_serial: next as u64,
                    })
                },
                identity,
                generation,
                PackLimits::default(),
                &storage,
                &mut setup,
            )
            .unwrap();
            objects
                .append(BlockKind::PayloadChunk, generation, b"seed", &mut setup)
                .unwrap();
            drop(setup);
            let mut injected = TreeResources::for_prepare(&storage, 1).unwrap();
            assert!(matches!(
                objects.append(BlockKind::PayloadChunk, generation, b"x", &mut injected),
                Err(TreeError::Work)
            ));
            assert_eq!(objects.abort_inventory().count(), 1);
            assert!(
                objects.artifact(0).is_err(),
                "append failure exposed no candidate"
            );
        }
        assert_eq!(storage.reserved_bytes(), baseline);

        {
            let mut next = 21_000_u128;
            let mut setup = TreeResources::for_prepare(&storage, u64::MAX).unwrap();
            let mut objects = PreparedObjects::new(
                &MissingProducerSource,
                || {
                    let artifact = ArtifactId::new(next)?;
                    next += 1;
                    Ok(ArtifactIdentity {
                        store: identity,
                        artifact,
                        generation,
                        creation_serial: next as u64,
                    })
                },
                identity,
                generation,
                PackLimits::default(),
                &storage,
                &mut setup,
            )
            .unwrap();
            objects
                .append(BlockKind::PayloadChunk, generation, b"seal", &mut setup)
                .unwrap();
            drop(setup);
            let mut injected = TreeResources::for_prepare(&storage, 0).unwrap();
            let error = objects.finish(&mut injected).unwrap_err();
            assert!(matches!(error, TreeError::Work));
            assert_eq!(objects.abort_inventory().count(), 1);
            assert!(
                objects.artifact(0).is_err(),
                "seal failure exposed no candidate"
            );
            let failure = PreparedGraphFailure::from_preparation(
                error,
                objects,
                store.admit_native_read().unwrap(),
            );
            let (error, objects, base) = failure.into_parts();
            assert!(matches!(error, TreeError::Work));
            assert_eq!(objects.abort_inventory().count(), 1);
            assert_eq!(base.bundle().base().generation, generation);
        }
        assert_eq!(storage.reserved_bytes(), baseline);
        drop(storage);
        drop(writer);
        store.close().unwrap();
    }

    #[cfg_attr(test, test)]
    fn native_prepared_artifacts_retain_exact_base_source_and_abort_owners() {
        native_preparation_coordinator_owns_admitted_source_and_finalization();
        prepared_artifact_contract();
        prepared_failure_ownership_contract();
    }

    #[cfg_attr(test, test)]
    fn native_prepared_artifacts_reject_foreign_or_stale_base() {
        native_preparation_coordinator_owns_admitted_source_and_finalization();
        prepared_artifact_contract();
    }

    fn sparse_relationship_successor_preserves_populations() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .unwrap();
        let identity = StoreInstanceId::new((1_u128 << 87) + 61).unwrap();
        let initial = actual_producer_bundle_with_extra_nodes(
            &store,
            directory.path(),
            identity,
            0,
            true,
            0,
            false,
        );
        assert_eq!(initial.base.generation, GraphGeneration::new(1));
        assert_eq!(initial.sequence, 41);
        store.install_native_graph_for_test(initial).unwrap();
        let admitted = current_native_input(&store);
        let successor =
            append_actual_relationship_generation(&store, directory.path(), &admitted, 1);
        assert_eq!(successor.base.generation, GraphGeneration::new(2));
        assert_eq!(successor.sequence, 42);
        store.install_native_graph_for_test(successor).unwrap();

        let lease = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut runtime =
            RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(&lease, &runtime).unwrap();
        let mut initial_resources = TreeResources::for_query(&mut runtime).unwrap();
        let source = NativeQuerySource::new(capability, &initial_resources, 32).unwrap();
        let catalog = NativeCatalog::open(&source, &mut initial_resources).unwrap();
        drop(initial_resources);
        let view = GraphReadView::new(&source, &catalog).unwrap();
        let sparse = view.sparse_view(&mut runtime).unwrap();
        assert_eq!(sparse.generation(), GraphGeneration::new(2));
        assert_eq!(sparse.sequence(), 42);
        assert_eq!(sparse.text_count(), 0);
        assert_eq!(sparse.vector_count(), 1);
        let node_b = NodeId::new((1_u128 << 100) + 2).unwrap();
        let mut resources = TreeResources::for_query(&mut runtime).unwrap();
        let member = sparse
            .lookup(
                crate::property_graph::storage::search::Modality::Vector,
                node_b,
                &mut resources,
            )
            .unwrap()
            .unwrap();
        let vector = member.vector.unwrap();
        assert_eq!(
            vector.coordinate(0, &mut resources).unwrap().to_bits(),
            0x3f80_0001
        );
        assert_eq!(
            vector.coordinate(1, &mut resources).unwrap().to_bits(),
            0x8000_0000
        );
        drop(resources);
        drop(sparse);
        drop(view);
        drop(catalog);
        drop(source);
        drop(runtime);
        drop(lease);
        store.close().unwrap();
    }

    #[cfg_attr(test, test)]
    fn ze61_complete_search_handoff_precedes_pack_finish() {
        native_preparation_coordinator_owns_admitted_source_and_finalization();
        prepared_artifact_contract();
        sparse_relationship_successor_preserves_populations();
    }

    struct ReplaceDuringScopedRead<'a> {
        store: &'a Store,
        replacement: Option<NativeGraphBundleInput>,
    }

    impl NativeReadConsumer<u64> for ReplaceDuringScopedRead<'_> {
        fn consume<'s, 'lease, 'm, 'g>(
            &mut self,
            view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
            _runtime: &mut RuntimeContext<'lease, 'm, 'g>,
        ) -> Result<u64, crate::property_graph::storage::tree::directory::TreeError> {
            let before = view.sequence();
            let replacement = self.replacement.take().ok_or(
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "missing replacement bundle",
                ),
            )?;
            self.store
                .install_native_graph_for_test(replacement)
                .map_err(|_| {
                    crate::property_graph::storage::tree::directory::TreeError::Invalid(
                        "controlled replacement failed",
                    )
                })?;
            if view.sequence() != before {
                return Err(
                    crate::property_graph::storage::tree::directory::TreeError::Invalid(
                        "scoped read changed bundle",
                    ),
                );
            }
            Ok(before)
        }
    }

    #[cfg_attr(test, test)]
    fn native_read_scoped_consumer_retains_one_catalog_and_bundle() {
        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 92).unwrap();
        let mut old = bundle(identity, 1, 81);
        old.catalog = install_catalog_file(directory.path(), identity, GraphGeneration::new(1), 91);
        old.high_waters.creation_serial = 9;
        store.install_native_graph_for_test(old).unwrap();
        let mut replacement = bundle(identity, 2, 101);
        replacement.catalog =
            install_catalog_file(directory.path(), identity, GraphGeneration::new(2), 111);
        replacement.high_waters.creation_serial = 9;
        let control = QueryControl::Cancel(CancelToken::new());
        let observed = store
            .with_native_read(
                &control,
                RuntimeLimits::default(),
                2 * 1024 * 1024,
                4,
                ReplaceDuringScopedRead {
                    store: &store,
                    replacement: Some(replacement),
                },
            )
            .unwrap();
        assert_eq!(observed, 1);
        store.close().unwrap();
    }

    #[cfg_attr(test, test)]
    fn native_read_cursor_rejects_same_view_memory_different_runtime() {
        use crate::property_graph::catalog::RelTypeId;
        use crate::property_graph::storage::adjacency::RelationshipRow;
        use crate::property_graph::{NodeId, RelId};

        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 91).unwrap();
        let mut installed = bundle(identity, 1, 121);
        installed.catalog =
            install_catalog_file(directory.path(), identity, GraphGeneration::new(1), 131);
        installed.high_waters.creation_serial = 9;
        store.install_native_graph_for_test(installed).unwrap();
        let lease = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let memory = QueryMemory::new(&shared, 2 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut runtime_a =
            RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(&lease, &runtime_a).unwrap();
        let mut initial = TreeResources::for_query(&mut runtime_a).unwrap();
        let source = NativeQuerySource::new(capability, &initial, 4).unwrap();
        let catalog = NativeCatalog::open(&source, &mut initial).unwrap();
        drop(initial);
        let view = GraphReadView::new(&source, &catalog).unwrap();
        let mut foreign = view
            .relationship_cursor(RelationshipTypeSelection::All, &mut runtime_a)
            .unwrap();
        let before = runtime_a.counters();
        let mut runtime_b =
            RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
        let dummy = RelationshipRow {
            rel: RelId::new(1).unwrap(),
            source: NodeId::new(1).unwrap(),
            target: NodeId::new(1).unwrap(),
            relationship_type: RelTypeId::new(1).unwrap(),
        };
        let mut output = [dummy];
        assert!(matches!(
            view.scan_relationships(&mut foreign, &mut output, &mut runtime_b),
            Err(
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "relationship cursor owner mismatch"
                )
            )
        ));
        assert_eq!(runtime_a.counters(), before);

        let mut fresh = view
            .relationship_cursor(RelationshipTypeSelection::Any(&[]), &mut runtime_a)
            .unwrap();
        assert_eq!(
            view.scan_relationships(&mut fresh, &mut output, &mut runtime_a)
                .unwrap(),
            (0, CursorState::Done)
        );
        drop(fresh);
        drop(foreign);
        drop(view);
        drop(catalog);
        drop(source);
        drop(runtime_b);
        drop(runtime_a);
        drop(lease);
        store.close().unwrap();
    }

    #[cfg_attr(test, test)]
    fn native_read_capability_rejects_foreign_runtime_before_source_construction() {
        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 79).unwrap();
        store
            .install_native_graph_for_test(actual_producer_bundle(
                &store,
                directory.path(),
                identity,
            ))
            .unwrap();
        let first = store.admit_native_read().unwrap();
        let second = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let memory = QueryMemory::new(&shared, 2 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let runtime =
            RuntimeContext::new(&second, &control, &memory, RuntimeLimits::default()).unwrap();
        let before = runtime.counters();
        assert!(matches!(
            NativeReadCapability::admit(&first, &runtime),
            Err(TreeError::Invalid("foreign native read capability"))
        ));
        assert_eq!(runtime.counters(), before);
        drop(runtime);
        drop(second);
        drop(first);
        store.close().unwrap();
    }

    #[cfg_attr(test, test)]
    fn native_read_cursor_rejects_same_metadata_foreign_admission() {
        use crate::property_graph::catalog::RelTypeId;
        use crate::property_graph::storage::adjacency::RelationshipRow;
        use crate::property_graph::{NodeId, RelId};

        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 84).unwrap();
        let mut installed = bundle(identity, 1, 211);
        installed.catalog =
            install_catalog_file(directory.path(), identity, GraphGeneration::new(1), 221);
        installed.high_waters.creation_serial = 9;
        store.install_native_graph_for_test(installed).unwrap();
        let first = store.admit_native_read().unwrap();
        let second = store.admit_native_read().unwrap();
        assert_ne!(first.token(), second.token());
        let shared = GraphResources::from_store(&store).unwrap();
        let memory = QueryMemory::new(&shared, 4 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut runtime_a =
            RuntimeContext::new(&first, &control, &memory, RuntimeLimits::default()).unwrap();
        let source_a_capability = NativeReadCapability::admit(&first, &runtime_a).unwrap();
        let mut initial_a = TreeResources::for_query(&mut runtime_a).unwrap();
        let source_a = NativeQuerySource::new(source_a_capability, &initial_a, 4).unwrap();
        let catalog_a = NativeCatalog::open(&source_a, &mut initial_a).unwrap();
        drop(initial_a);
        let view_a = GraphReadView::new(&source_a, &catalog_a).unwrap();
        let mut cursor = view_a
            .relationship_cursor(RelationshipTypeSelection::All, &mut runtime_a)
            .unwrap();

        let mut runtime_b =
            RuntimeContext::new(&second, &control, &memory, RuntimeLimits::default()).unwrap();
        let source_b_capability = NativeReadCapability::admit(&second, &runtime_b).unwrap();
        let mut initial_b = TreeResources::for_query(&mut runtime_b).unwrap();
        let source_b = NativeQuerySource::new(source_b_capability, &initial_b, 4).unwrap();
        let catalog_b = NativeCatalog::open(&source_b, &mut initial_b).unwrap();
        drop(initial_b);
        let view_b = GraphReadView::new(&source_b, &catalog_b).unwrap();
        let before = runtime_a.counters();
        let dummy = RelationshipRow {
            rel: RelId::new(1).unwrap(),
            source: NodeId::new(1).unwrap(),
            target: NodeId::new(1).unwrap(),
            relationship_type: RelTypeId::new(1).unwrap(),
        };
        let mut output = [dummy];
        assert!(matches!(
            view_b.scan_relationships(&mut cursor, &mut output, &mut runtime_a),
            Err(
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "relationship cursor owner mismatch"
                )
            )
        ));
        assert_eq!(runtime_a.counters(), before);
        drop(cursor);
        drop(view_b);
        drop(catalog_b);
        drop(source_b);
        drop(view_a);
        drop(catalog_a);
        drop(source_a);
        drop(runtime_b);
        drop(runtime_a);
        drop(second);
        drop(first);
        store.close().unwrap();
    }

    fn actual_producer_read_contract() {
        use crate::property_graph::catalog::{LabelId, RelTypeId};
        use crate::property_graph::storage::adjacency::RelationshipRow;
        use crate::property_graph::storage::records::RecordShape;
        use crate::property_graph::{NodeId, RelId};

        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 90).unwrap();
        let installed = actual_producer_bundle(&store, directory.path(), identity);
        assert_eq!(installed.sequence, 41);
        store.install_native_graph_for_test(installed).unwrap();
        let lease = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut runtime =
            RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(&lease, &runtime).unwrap();
        let mut initial = TreeResources::for_query(&mut runtime).unwrap();
        let source = NativeQuerySource::new(capability, &initial, 8).unwrap();
        let catalog = NativeCatalog::open(&source, &mut initial).unwrap();
        drop(initial);
        let view = GraphReadView::new(&source, &catalog).unwrap();
        store
            .install_native_graph_for_test(bundle(identity, 2, 231))
            .unwrap();
        assert_eq!(view.sequence(), 41);
        let node_a = NodeId::new((1_u128 << 100) + 1).unwrap();
        let node_b = NodeId::new((1_u128 << 100) + 2).unwrap();
        let rel_base = 1_u128 << 110;
        {
            let mut resources = TreeResources::for_query(&mut runtime).unwrap();
            let first = view.lookup_node(node_a, &mut resources).unwrap().unwrap();
            assert_eq!(
                first.record().shape(),
                RecordShape::Node {
                    id: node_a,
                    labels: 1,
                }
            );
            assert!(first.record().canonical().stored_text().unwrap().is_empty());
            assert!(first.record().canonical().stored_vector().is_none());
            let second = view.lookup_node(node_b, &mut resources).unwrap().unwrap();
            assert!(second.record().canonical().stored_text().is_none());
            assert!(second.record().canonical().stored_vector().is_none());
            let empty_text = view.stored_text(node_a, &mut resources).unwrap().unwrap();
            assert_eq!(empty_text.len(), 0);
            assert_eq!(empty_text.read_at(0, &mut [], &mut resources).unwrap(), 0);
            assert!(view.stored_text(node_b, &mut resources).unwrap().is_none());
            assert!(
                view.vector_payload(node_a, &mut resources)
                    .unwrap()
                    .is_none()
            );
            assert!(
                view.lookup_node(NodeId::new(u128::MAX).unwrap(), &mut resources)
                    .unwrap()
                    .is_none()
            );
            assert!(matches!(
                view.stored_text(NodeId::new(u128::MAX).unwrap(), &mut resources),
                Err(
                    crate::property_graph::storage::tree::directory::TreeError::Invalid(
                        "stored text entity is absent or deleted"
                    )
                )
            ));
        }

        let label = LabelId::new(1).unwrap();
        let mut nodes = view
            .node_cursor(LabelSelection::AllOf(&[label, label]), &mut runtime)
            .unwrap();
        let mut node_rows = Vec::new();
        loop {
            let mut output = [node_b];
            let (count, state) = view
                .scan_nodes(&mut nodes, &mut output, &mut runtime)
                .unwrap();
            node_rows.extend_from_slice(&output[..count]);
            if state == CursorState::Done {
                break;
            }
        }
        assert_eq!(node_rows, vec![node_a]);
        drop(nodes);

        let dummy = RelationshipRow {
            rel: RelId::new(1).unwrap(),
            source: node_a,
            target: node_b,
            relationship_type: RelTypeId::new(1).unwrap(),
        };
        let expected = vec![
            RelationshipRow {
                rel: RelId::new(rel_base + 1).unwrap(),
                source: node_a,
                target: node_b,
                relationship_type: RelTypeId::new(1).unwrap(),
            },
            RelationshipRow {
                rel: RelId::new(rel_base + 2).unwrap(),
                source: node_a,
                target: node_a,
                relationship_type: RelTypeId::new(2).unwrap(),
            },
            RelationshipRow {
                rel: RelId::new(rel_base + 3).unwrap(),
                source: node_a,
                target: node_b,
                relationship_type: RelTypeId::new(1).unwrap(),
            },
        ];
        for capacity in [1_usize, 2, 256] {
            let mut cursor = view
                .relationship_cursor(
                    RelationshipTypeSelection::Any(&[
                        RelTypeId::new(2).unwrap(),
                        RelTypeId::new(1).unwrap(),
                        RelTypeId::new(2).unwrap(),
                    ]),
                    &mut runtime,
                )
                .unwrap();
            let mut observed = Vec::new();
            loop {
                let mut output = vec![dummy; capacity];
                let (count, state) = view
                    .scan_relationships(&mut cursor, &mut output, &mut runtime)
                    .unwrap();
                observed.extend_from_slice(&output[..count]);
                if state == CursorState::Done {
                    break;
                }
            }
            assert_eq!(observed, expected);
            drop(cursor);
        }
        drop(view);
        drop(catalog);
        drop(source);
        drop(runtime);
        drop(lease);
        store.close().unwrap();
    }

    #[test]
    fn native_read_label_scan_uses_selected_membership_index() {
        use crate::property_graph::NodeId;
        use crate::property_graph::catalog::LabelId;
        use crate::property_graph::query::runtime::WorkKind;

        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .unwrap();
        let identity = StoreInstanceId::new((1_u128 << 90) + 45).unwrap();
        store
            .install_native_graph_for_test(actual_producer_bundle_with_extra_nodes(
                &store,
                directory.path(),
                identity,
                64,
                false,
                0,
                false,
            ))
            .unwrap();
        let lease = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut runtime =
            RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(&lease, &runtime).unwrap();
        let mut initial = TreeResources::for_query(&mut runtime).unwrap();
        let source = NativeQuerySource::new(capability, &initial, 16).unwrap();
        let catalog = NativeCatalog::open(&source, &mut initial).unwrap();
        drop(initial);
        let view = GraphReadView::new(&source, &catalog).unwrap();
        let label = LabelId::new(1).unwrap();
        let before = runtime.counters().get(WorkKind::Scans);
        let mut cursor = view
            .node_cursor(LabelSelection::AllOf(&[label, label]), &mut runtime)
            .unwrap();
        let sentinel = NodeId::new(1).unwrap();
        let mut rows = [sentinel; 2];
        let (count, state) = view
            .scan_nodes(&mut cursor, &mut rows, &mut runtime)
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(state, CursorState::Done);
        assert_eq!(rows[0], NodeId::new((1_u128 << 100) + 1).unwrap());
        let scans = runtime.counters().get(WorkKind::Scans) - before;
        assert!(
            scans <= 16,
            "selected label scanned {scans} physical entries"
        );
        drop(cursor);

        let unknown = LabelId::new(99).unwrap();
        let mut cursor = view
            .node_cursor(LabelSelection::AllOf(&[unknown]), &mut runtime)
            .unwrap();
        let (count, state) = view
            .scan_nodes(&mut cursor, &mut rows, &mut runtime)
            .unwrap();
        assert_eq!((count, state), (0, CursorState::Done));

        drop(cursor);
        drop(view);
        drop(catalog);
        drop(source);
        drop(runtime);
        drop(lease);
        store.close().unwrap();
    }

    #[test]
    fn native_read_path_accounting_releases_transient_capacity() {
        use crate::property_graph::query::resources::MemoryError;
        use crate::property_graph::query::runtime::RuntimeError;

        let directory = tempfile::tempdir().unwrap();
        let vfs = Arc::new(CountingVfs::new(StdVfs));
        let store = Store::open_with_test_dependencies(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
            StoreTestDependencies::new(vfs.clone(), Arc::new(ManualMonotonicClock::new())),
        )
        .unwrap();
        let identity = StoreInstanceId::new((1_u128 << 90) + 450).unwrap();
        let second = write_framed_file(
            directory.path(),
            ContainerKind::Object,
            ArtifactIdentity {
                store: identity,
                artifact: ArtifactId::new((1_u128 << 119) + 45).unwrap(),
                generation: GraphGeneration::new(1),
                creation_serial: 1,
            },
            &[Block {
                kind: BlockKind::CommitParticipant,
                payload: b"second-accounted-lazy-open",
            }],
        );
        store
            .install_native_graph_for_test(actual_producer_bundle(
                &store,
                directory.path(),
                identity,
            ))
            .unwrap();
        let lease = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
        let mut runtime =
            RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(&lease, &runtime).unwrap();
        let mut initial = TreeResources::for_query(&mut runtime).unwrap();
        let source = NativeQuerySource::new(capability, &initial, 16).unwrap();
        let source_steady = memory.reserved_bytes();
        let catalog = NativeCatalog::open(&source, &mut initial).unwrap();
        drop(initial);
        let view = GraphReadView::new(&source, &catalog).unwrap();
        let steady = memory.reserved_bytes();
        let opens_before = vfs.open_for_map_calls();
        let node = NodeId::new((1_u128 << 100) + 1).unwrap();

        let mut resources = TreeResources::for_query(&mut runtime).unwrap();
        assert!(view.lookup_node(node, &mut resources).unwrap().is_some());
        assert_eq!(
            source
                .resolve(second.block, &mut resources)
                .unwrap()
                .payload(),
            b"second-accounted-lazy-open"
        );
        drop(resources);
        assert_eq!(memory.reserved_bytes(), steady);
        assert!(vfs.open_for_map_calls() >= opens_before + 2);
        let peak_after_open = memory.peak_reserved_bytes();

        let mut resources = TreeResources::for_query(&mut runtime).unwrap();
        assert!(view.lookup_node(node, &mut resources).unwrap().is_some());
        drop(resources);
        assert_eq!(memory.reserved_bytes(), steady);
        assert_eq!(memory.peak_reserved_bytes(), peak_after_open);

        let missing = PhysicalRef {
            artifact: ArtifactId::new((1_u128 << 120) + 45).unwrap(),
            offset: 96,
            length: 25,
            kind: BlockKind::NodeRecord,
            version: 1,
        };
        let mut resources = TreeResources::for_query(&mut runtime).unwrap();
        assert!(matches!(
            source.resolve(missing, &mut resources),
            Err(TreeError::Io(_))
        ));
        drop(resources);
        assert_eq!(memory.reserved_bytes(), steady);
        assert!(memory.peak_reserved_bytes() >= steady);

        drop(view);
        drop(catalog);
        drop(source);
        drop(runtime);
        assert_eq!(
            memory.reserved_bytes(),
            std::mem::size_of::<QueryMemory<'_>>()
        );

        let tight = QueryMemory::new(&shared, source_steady + 1).unwrap();
        let mut tight_runtime =
            RuntimeContext::new(&lease, &control, &tight, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(&lease, &tight_runtime).unwrap();
        let mut resources = TreeResources::for_query(&mut tight_runtime).unwrap();
        let tight_source = NativeQuerySource::new(capability, &resources, 16).unwrap();
        let tight_steady = tight.reserved_bytes();
        assert!(matches!(
            tight_source.resolve(missing, &mut resources),
            Err(TreeError::Runtime(RuntimeError::Memory(MemoryError::Limit)))
        ));
        assert_eq!(tight.reserved_bytes(), tight_steady);
        drop(tight_source);
        drop(resources);
        drop(tight_runtime);
        drop(lease);
        store.close().unwrap();
    }

    #[test]
    fn expansion_resume_crosses_real_split_and_type_boundary_with_bounded_work() {
        use crate::property_graph::catalog::RelTypeId;
        use crate::property_graph::query::runtime::WorkKind;
        use crate::property_graph::storage::adjacency::{
            MAX_MERGED_ENTRIES, RangeDescriptor, RelationshipRow,
        };
        use crate::property_graph::storage::tree::directory::DirectoryCursor;
        use crate::property_graph::storage::tree::{Key, TreeKind};
        use crate::property_graph::{NodeId, RelId};

        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .unwrap();
        let identity = StoreInstanceId::new((1_u128 << 90) + 451).unwrap();
        let first = actual_producer_bundle_with_extra_nodes(
            &store,
            directory.path(),
            identity,
            0,
            false,
            380,
            false,
        );
        store.install_native_graph_for_test(first).unwrap();
        for generation_index in 0..27 {
            let admitted = current_native_input(&store);
            let next =
                append_actual_relationship_generation(&store, directory.path(), &admitted, 186);
            store.install_native_graph_for_test(next).unwrap();
            eprintln!(
                "ZE45 split fixture generation {} installed",
                generation_index + 2
            );
        }

        let lease = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let memory = QueryMemory::new(&shared, 24 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut runtime =
            RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(&lease, &runtime).unwrap();
        let mut initial = TreeResources::for_query(&mut runtime).unwrap();
        let source = NativeQuerySource::new(capability, &initial, 256).unwrap();
        let catalog = NativeCatalog::open(&source, &mut initial).unwrap();
        drop(initial);
        let view = GraphReadView::new(&source, &catalog).unwrap();
        let node = NodeId::new((1_u128 << 100) + 1).unwrap();
        let type_r = RelTypeId::new(1).unwrap();
        let type_s = RelTypeId::new(2).unwrap();

        let mut resources = TreeResources::for_query(&mut runtime).unwrap();
        let root = lease
            .bundle()
            .roots()
            .directory(TreeKind::OutRanges)
            .unwrap();
        let mut ranges = DirectoryCursor::seek(&source, root, None, &mut resources).unwrap();
        let mut same_type_ranges = 0_usize;
        let mut first_boundary = None;
        while let Some(entry) = ranges.next_entry(&mut resources).unwrap() {
            let Key::Inline(key) = entry.key() else {
                panic!("overflow adjacency range key");
            };
            let descriptor = RangeDescriptor::decode(root.kind(), key, entry.value()).unwrap();
            if descriptor.key().node == node && descriptor.key().rel_type == type_r {
                same_type_ranges += 1;
                if first_boundary.is_none()
                    && let crate::property_graph::storage::adjacency::UpperBound::Exclusive(rel) =
                        descriptor.key().upper
                {
                    first_boundary = Some(rel);
                }
            }
        }
        assert!(
            same_type_ranges >= 2,
            "same type did not persist a physical split"
        );
        drop(ranges);
        drop(resources);

        let rel_base = 1_u128 << 110;
        let node_b = NodeId::new((1_u128 << 100) + 2).unwrap();
        let mut expected = Vec::with_capacity(5_405);
        for offset in 1..=5_405_u128 {
            if offset != 382 {
                expected.push(RelationshipRow {
                    rel: RelId::new(rel_base + offset).unwrap(),
                    source: node,
                    target: node_b,
                    relationship_type: type_r,
                });
            }
        }
        expected.push(RelationshipRow {
            rel: RelId::new(rel_base + 382).unwrap(),
            source: node,
            target: node,
            relationship_type: type_s,
        });
        let dummy = expected[0];
        let boundary = first_boundary.unwrap();
        let boundary_index = expected.iter().position(|row| row.rel == boundary).unwrap();
        let mut cursor = view
            .expansion_cursor(
                node,
                DirectionSelection::Out,
                RelationshipTypeSelection::All,
                &mut runtime,
            )
            .unwrap();
        let mut observed = Vec::with_capacity(expected.len());
        let mut exercised = [false; 3];
        let mut pulls = 0_usize;
        let mut bounded_remerges = 0_usize;
        let work_start = runtime.counters().get(WorkKind::AdjacencyEntries);
        loop {
            pulls += 1;
            assert!(pulls <= 64, "expansion failed to make bounded progress");
            let capacity = if observed.len() < boundary_index.saturating_sub(1) {
                256.min(boundary_index - 1 - observed.len())
            } else if observed.len() < boundary_index + 1 {
                1
            } else if observed.len() < boundary_index + 3 {
                2
            } else {
                256
            };
            match capacity {
                1 => exercised[0] = true,
                2 => exercised[1] = true,
                256 => exercised[2] = true,
                _ => {}
            }
            let before = runtime.counters().get(WorkKind::AdjacencyEntries);
            let mut output = vec![dummy; capacity];
            let (count, state) = view.expand(&mut cursor, &mut output, &mut runtime).unwrap();
            let work = runtime.counters().get(WorkKind::AdjacencyEntries) - before;
            if work > capacity as u64 + 8 {
                bounded_remerges += 1;
                assert!(
                    work <= (2 * MAX_MERGED_ENTRIES + capacity + 8) as u64,
                    "one pull exceeded bounded remerge work: {work} for capacity {capacity}"
                );
            }
            observed.extend_from_slice(&output[..count]);
            if state == CursorState::Done {
                break;
            }
        }
        assert_eq!(exercised, [true; 3]);
        assert_eq!(observed, expected);
        assert!(
            bounded_remerges > 0,
            "physical ranges never exercised a remerge"
        );
        let total_work = runtime.counters().get(WorkKind::AdjacencyEntries) - work_start;
        assert!(
            total_work <= (expected.len() + pulls * 2 * MAX_MERGED_ENTRIES + pulls * 8) as u64,
            "expansion replayed adjacency work across pulls: {total_work}"
        );
        eprintln!("ZE45 split fixture completed in {pulls} pulls");
        drop(cursor);

        drop(view);
        drop(catalog);
        drop(source);
        drop(runtime);
        drop(lease);
        store.close().unwrap();
    }

    fn actual_vector_contract() {
        use crate::property_graph::NodeId;

        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 85).unwrap();
        let installed = actual_producer_bundle_with_extra_nodes(
            &store,
            directory.path(),
            identity,
            0,
            true,
            0,
            false,
        );
        assert!(
            installed.vector.is_some(),
            "prepared vector participant is absent"
        );
        store.install_native_graph_for_test(installed).unwrap();
        let lease = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut runtime =
            RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(&lease, &runtime).unwrap();
        let mut initial = TreeResources::for_query(&mut runtime).unwrap();
        let source = NativeQuerySource::new(capability, &initial, 8).unwrap();
        let catalog = NativeCatalog::open(&source, &mut initial).unwrap();
        drop(initial);
        let view = GraphReadView::new(&source, &catalog).unwrap();
        let node_a = NodeId::new((1_u128 << 100) + 1).unwrap();
        let node_b = NodeId::new((1_u128 << 100) + 2).unwrap();
        let mut resources = TreeResources::for_query(&mut runtime).unwrap();
        assert!(
            view.vector_payload(node_a, &mut resources)
                .unwrap()
                .is_none()
        );
        let vector = view
            .vector_payload(node_b, &mut resources)
            .unwrap()
            .expect("declared vector");
        assert_eq!(vector.dimensions(), 2);
        assert_eq!(
            vector.coordinate(0, &mut resources).unwrap().to_bits(),
            0x3f80_0001
        );
        assert_eq!(
            vector.coordinate(1, &mut resources).unwrap().to_bits(),
            0x8000_0000
        );
        drop(resources);
        let sparse = view.sparse_view(&mut runtime).unwrap();
        assert_eq!(sparse.text_count(), 0);
        assert_eq!(sparse.vector_count(), 1);
        let mut resources = TreeResources::for_query(&mut runtime).unwrap();
        assert_eq!(
            sparse
                .validate_all(
                    crate::property_graph::storage::search::Modality::Vector,
                    &mut resources,
                )
                .unwrap(),
            1
        );
        drop(resources);
        drop(sparse);
        drop(view);
        drop(catalog);
        drop(source);
        drop(runtime);
        drop(lease);
        store.close().unwrap();
    }

    #[cfg_attr(test, test)]
    fn actual_max_identity_and_split_contract() {
        use crate::property_graph::catalog::RelTypeId;
        use crate::property_graph::storage::adjacency::exact_split_fixture;
        use crate::property_graph::{NodeId, RelId};

        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 84).unwrap();
        let installed = actual_producer_bundle_with_extra_nodes(
            &store,
            directory.path(),
            identity,
            0,
            false,
            0,
            true,
        );
        store.install_native_graph_for_test(installed).unwrap();
        let lease = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let memory = QueryMemory::new(&shared, 24 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut runtime =
            RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(&lease, &runtime).unwrap();
        let mut initial = TreeResources::for_query(&mut runtime).unwrap();
        let source = NativeQuerySource::new(capability, &initial, 16).unwrap();
        let catalog = NativeCatalog::open(&source, &mut initial).unwrap();
        drop(initial);
        let view = GraphReadView::new(&source, &catalog).unwrap();
        let node_a = NodeId::new(u128::MAX - 1).unwrap();
        let node_b = NodeId::new(u128::MAX).unwrap();
        let max_relationship = RelId::new(u128::MAX).unwrap();
        let relationship_type = RelTypeId::new(1).unwrap();
        let mut resources = TreeResources::for_query(&mut runtime).unwrap();

        assert!(view.lookup_node(node_a, &mut resources).unwrap().is_some());
        assert!(view.lookup_node(node_b, &mut resources).unwrap().is_some());
        let row = view
            .lookup_relationship(max_relationship, &mut resources)
            .unwrap()
            .expect("u128::MAX relationship is live");
        assert_eq!(
            row.record().incarnation(),
            EntityId::Relationship(max_relationship)
        );
        assert_eq!(row.record().revision(), GraphRevision::new(1).unwrap());
        let crate::property_graph::storage::records::RecordShape::Relationship {
            id,
            source: source_node,
            target: target_node,
            relationship_type: actual_type,
        } = row.record().shape()
        else {
            panic!("relationship lookup returned a node record");
        };
        assert_eq!(id, max_relationship);
        assert_eq!(source_node, node_a);
        assert_eq!(target_node, node_b);
        assert_eq!(actual_type, relationship_type);
        let property = view
            .relationship_property(
                &row,
                crate::property_graph::catalog::PropertyKeyId::new(1).unwrap(),
                &mut resources,
            )
            .unwrap()
            .expect("relationship property is present");
        let mut encoded = [0_u8; 9];
        assert_eq!(
            property.read_at(0, &mut encoded, &mut resources).unwrap(),
            9
        );
        assert_eq!(encoded[0], 3);
        assert_eq!(i64::from_le_bytes(encoded[1..].try_into().unwrap()), -17);
        drop(resources);

        let dummy = crate::property_graph::storage::adjacency::RelationshipRow {
            rel: RelId::new(1).unwrap(),
            source: NodeId::new(1).unwrap(),
            target: NodeId::new(1).unwrap(),
            relationship_type,
        };
        let expected = [
            RelId::new(u128::MAX - 2).unwrap(),
            RelId::new(u128::MAX - 1).unwrap(),
            max_relationship,
        ];
        for capacity in [1_usize, 2, 256] {
            let mut cursor = view
                .relationship_cursor(RelationshipTypeSelection::All, &mut runtime)
                .unwrap();
            let mut observed = Vec::new();
            loop {
                let mut output = vec![dummy; capacity];
                let (count, state) = view
                    .scan_relationships(&mut cursor, &mut output, &mut runtime)
                    .unwrap();
                observed.extend(output[..count].iter().map(|row| row.rel));
                if state == CursorState::Done {
                    break;
                }
            }
            assert_eq!(observed, expected);
            drop(cursor);
        }
        exact_split_fixture();

        drop(view);
        drop(catalog);
        drop(source);
        drop(runtime);
        drop(lease);
        store.close().unwrap();
    }

    fn actual_directory_depth_contract() {
        use crate::property_graph::storage::tree::{TreeKind, decode_page};

        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 83).unwrap();
        let installed = actual_producer_bundle_with_extra_nodes(
            &store,
            directory.path(),
            identity,
            320,
            false,
            0,
            false,
        );
        store.install_native_graph_for_test(installed).unwrap();
        let lease = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut runtime =
            RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(&lease, &runtime).unwrap();
        let mut resources = TreeResources::for_query(&mut runtime).unwrap();
        let source = NativeQuerySource::new(capability, &resources, 16).unwrap();
        let root = lease.bundle().roots().directory(TreeKind::Nodes).unwrap();
        let block = source
            .resolve(
                root.reference().expect("nonempty node root"),
                &mut resources,
            )
            .unwrap();
        assert!(
            decode_page(TreeKind::Nodes, block.payload())
                .unwrap()
                .header()
                .level
                > 0,
            "actual node directory must exceed one leaf"
        );
        drop(resources);
        drop(source);
        drop(runtime);
        drop(lease);
        store.close().unwrap();
    }

    #[cfg_attr(test, test)]
    fn native_read_all_operations_use_one_admitted_bundle() {
        actual_producer_read_contract();
    }

    #[cfg_attr(test, test)]
    fn native_read_graph_only_optional_payloads_and_full_width_ids() {
        actual_producer_read_contract();
        actual_vector_contract();
        actual_max_identity_and_split_contract();
    }

    #[cfg_attr(test, test)]
    fn native_read_scan_resume_preserves_interleaved_types_and_max_ids() {
        actual_producer_read_contract();
        actual_max_identity_and_split_contract();
        actual_directory_depth_contract();
    }

    #[cfg_attr(test, test)]
    fn native_read_pull_consumer_resumes_and_materializes_same_view() {
        use crate::property_graph::NodeId;
        use crate::property_graph::query::plan::{
            NodeFacts, Operator, OperatorKind, PlanBacking, PlanDescription, PlanFootprint,
            PlanNodeId, RetainedRegion, SlotId, VALIDATION_SCRATCH_BYTES,
        };
        use crate::property_graph::query::resources::{
            QueryArena, QueryInputs, RetainedAllocation, RetentionInventory,
        };
        use crate::property_graph::query::runtime::{ArenaCapacity, ExecutionCapacity, execute_in};

        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 86).unwrap();
        store
            .install_native_graph_for_test(actual_producer_bundle(
                &store,
                directory.path(),
                identity,
            ))
            .unwrap();
        let lease = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let memory = QueryMemory::new(&shared, 16 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut runtime =
            RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(&lease, &runtime).unwrap();
        let mut initial = TreeResources::for_query(&mut runtime).unwrap();
        let source = NativeQuerySource::new(capability, &initial, 16).unwrap();
        let catalog = NativeCatalog::open(&source, &mut initial).unwrap();
        drop(initial);
        let view = GraphReadView::new(&source, &catalog).unwrap();

        let mut plan_scratch = memory.reserve_external_capacity().unwrap();
        plan_scratch
            .reserve_additional(
                VALIDATION_SCRATCH_BYTES
                    + std::mem::size_of::<[RetainedRegion; 3]>()
                    + std::mem::size_of::<PlanDescription<'_>>(),
            )
            .unwrap();
        let mut operators = QueryArena::new(&memory, 2).unwrap();
        let scan_inputs = [PlanNodeId(0)];
        operators
            .push(Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            })
            .unwrap();
        operators
            .push(Operator {
                inputs: &scan_inputs,
                kind: OperatorKind::ScanNodes {
                    output: SlotId(7),
                    label: None,
                },
            })
            .unwrap();
        let mut facts = QueryArena::new(&memory, 16).unwrap();
        facts.push(NodeFacts::default()).unwrap();
        facts.push(NodeFacts::default()).unwrap();
        let fact_bytes = facts.heap_bytes();
        let mut regions = [
            RetainedRegion::declared(
                operators.as_slice().as_ptr() as usize,
                operators.heap_bytes(),
            )
            .unwrap(),
            RetainedRegion::declared(facts.as_slice().as_ptr() as usize, fact_bytes).unwrap(),
            RetainedRegion::slice(&scan_inputs).unwrap(),
        ];
        regions.sort();
        let (plan, facts_owner) = facts
            .validate_plan(
                PlanDescription {
                    operators: operators.as_slice(),
                    expressions: &[],
                    parameters: &[],
                    root: PlanNodeId(1),
                    eager_searches: &[],
                },
                PlanFootprint::declared(memory.reserved_bytes()),
                PlanBacking::new(&regions, std::mem::size_of_val(&regions)).unwrap(),
                runtime.values(),
            )
            .unwrap();
        let owners = [
            RetainedAllocation::arena(&operators).unwrap(),
            facts_owner,
            RetainedAllocation::array(&scan_inputs).unwrap(),
        ];
        let admitted = QueryInputs::reserve(
            &memory,
            RetentionInventory::array(&owners),
            runtime.values(),
        )
        .unwrap()
        .admit_plan(&plan, runtime.values())
        .unwrap();

        let cursor = view.node_cursor(LabelSelection::All, &mut runtime).unwrap();
        let mut pull = NativeNodePull {
            view: &view,
            cursor: Some(cursor),
            nodes: None,
            emitted: 0,
            materialized: false,
        };
        let result = execute_in(
            &mut runtime,
            &admitted,
            &mut pull,
            &mut FreezeNodeSummary,
            ExecutionCapacity {
                batch_rows: 256,
                result_rows: 257,
                batch_payload_bytes: 256 * 16,
                result_payload_bytes: 257 * 16,
                batch: ArenaCapacity::default(),
                result: ArenaCapacity::default(),
            },
        )
        .unwrap();
        assert_eq!(
            result.output,
            (
                257,
                NodeId::new((1_u128 << 100) + 1).unwrap(),
                NodeId::new((1_u128 << 100) + 1).unwrap(),
            )
        );
        assert!(
            result
                .counters
                .get(crate::property_graph::query::runtime::WorkKind::OperatorRows)
                > 256
        );

        drop(pull);
        drop(admitted);
        drop(plan);
        drop(facts);
        drop(operators);
        drop(plan_scratch);
        drop(view);
        drop(catalog);
        drop(source);
        drop(runtime);
        drop(lease);
        store.close().unwrap();
    }

    #[cfg_attr(test, test)]
    fn native_read_every_edge_path_checks_both_endpoint_states() {
        use crate::property_graph::catalog::RelTypeId;
        use crate::property_graph::storage::adjacency::RelationshipRow;
        use crate::property_graph::{NodeId, RelId};

        let directory = tempfile::tempdir().expect("store directory");
        let vfs = Arc::new(CountingVfs::new(StdVfs));
        let store = Store::open_with_test_dependencies(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
            StoreTestDependencies::new(vfs.clone(), Arc::new(ManualMonotonicClock::new())),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 88).unwrap();
        let node_a = NodeId::new((1_u128 << 100) + 1).unwrap();
        let node_b = NodeId::new((1_u128 << 100) + 2).unwrap();
        let installed = actual_producer_bundle(&store, directory.path(), identity);
        let tombstoned = tombstoned_endpoint_bundle(&store, directory.path(), &installed, node_b);
        let protected_objects = installed
            .wal_roots
            .slots
            .iter()
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        store.install_native_graph_for_test(installed).unwrap();
        let mut missing_endpoint = current_native_input(&store);
        let old = store.admit_native_read().unwrap();

        let shared = GraphResources::from_store(&store).unwrap();
        let old_memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut old_runtime =
            RuntimeContext::new(&old, &control, &old_memory, RuntimeLimits::default()).unwrap();
        let old_source_capability = NativeReadCapability::admit(&old, &old_runtime).unwrap();
        let mut old_initial = TreeResources::for_query(&mut old_runtime).unwrap();
        let old_source = NativeQuerySource::new(old_source_capability, &old_initial, 8).unwrap();
        let old_catalog = NativeCatalog::open(&old_source, &mut old_initial).unwrap();
        drop(old_initial);
        let old_view = GraphReadView::new(&old_source, &old_catalog).unwrap();
        let opens_before_old_lookup = vfs.open_for_map_calls();

        store.install_native_graph_for_test(tombstoned).unwrap();
        let protected = store.capture_native_read_roots().unwrap();
        assert!(
            protected_objects
                .iter()
                .all(|required| protected.contains(*required)),
            "retained old admission must protect every actual producer root"
        );
        drop(protected);

        let rel = RelId::new((1_u128 << 110) + 1).unwrap();
        let expected = RelationshipRow {
            rel,
            source: node_a,
            target: node_b,
            relationship_type: RelTypeId::new(1).unwrap(),
        };
        let mut old_resources = TreeResources::for_query(&mut old_runtime).unwrap();
        assert_eq!(
            old_view
                .lookup_relationship(rel, &mut old_resources)
                .unwrap()
                .map(|relationship| relationship.row()),
            Some(expected)
        );
        assert!(
            vfs.open_for_map_calls() > opens_before_old_lookup,
            "old GraphReadView must lazily map an actual producer descendant after replacement"
        );
        assert!(
            old_view
                .lookup_node(node_a, &mut old_resources)
                .unwrap()
                .is_some()
        );
        let old_text = old_view
            .stored_text(node_a, &mut old_resources)
            .unwrap()
            .unwrap();
        assert_eq!(old_text.len(), 0);
        drop(old_resources);

        let tombstone_lease = store.admit_native_read().unwrap();
        let tombstone_memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
        let mut tombstone_runtime = RuntimeContext::new(
            &tombstone_lease,
            &control,
            &tombstone_memory,
            RuntimeLimits::default(),
        )
        .unwrap();
        let tombstone_capability =
            NativeReadCapability::admit(&tombstone_lease, &tombstone_runtime).unwrap();
        let mut tombstone_initial = TreeResources::for_query(&mut tombstone_runtime).unwrap();
        let tombstone_source =
            NativeQuerySource::new(tombstone_capability, &tombstone_initial, 12).unwrap();
        let tombstone_catalog =
            NativeCatalog::open(&tombstone_source, &mut tombstone_initial).unwrap();
        drop(tombstone_initial);
        let tombstone_view = GraphReadView::new(&tombstone_source, &tombstone_catalog).unwrap();
        let mut tombstone_resources = TreeResources::for_query(&mut tombstone_runtime).unwrap();
        assert!(
            tombstone_view
                .lookup_relationship(rel, &mut tombstone_resources)
                .unwrap()
                .is_none(),
            "tombstoned far endpoint hides rather than corrupts the relationship",
        );
        drop(tombstone_resources);
        let sentinel = RelationshipRow {
            rel: RelId::new(1).unwrap(),
            source: NodeId::new(1).unwrap(),
            target: NodeId::new(1).unwrap(),
            relationship_type: RelTypeId::new(1).unwrap(),
        };
        let self_row = RelationshipRow {
            rel: RelId::new((1_u128 << 110) + 2).unwrap(),
            source: node_a,
            target: node_a,
            relationship_type: RelTypeId::new(2).unwrap(),
        };
        let mut scan = tombstone_view
            .relationship_cursor(RelationshipTypeSelection::All, &mut tombstone_runtime)
            .unwrap();
        let mut tombstone_output = [sentinel; 4];
        let (count, state) = tombstone_view
            .scan_relationships(&mut scan, &mut tombstone_output, &mut tombstone_runtime)
            .unwrap();
        assert_eq!((count, state), (1, CursorState::Done));
        assert_eq!(&tombstone_output[..count], &[self_row]);
        drop(scan);
        let mut expansion = tombstone_view
            .expansion_cursor(
                node_a,
                DirectionSelection::Out,
                RelationshipTypeSelection::All,
                &mut tombstone_runtime,
            )
            .unwrap();
        let (count, state) = tombstone_view
            .expand(
                &mut expansion,
                &mut tombstone_output,
                &mut tombstone_runtime,
            )
            .unwrap();
        assert_eq!((count, state), (1, CursorState::Done));
        assert_eq!(&tombstone_output[..count], &[self_row]);
        drop(expansion);
        drop(tombstone_view);
        drop(tombstone_catalog);
        drop(tombstone_source);
        drop(tombstone_runtime);
        drop(tombstone_lease);

        let mut references = missing_endpoint.roots.references();
        references[0] = None;
        missing_endpoint.roots =
            GraphRoots::from_references(identity, missing_endpoint.base.generation, references)
                .unwrap();
        missing_endpoint.wal_roots.slots[0] = None;
        store
            .install_native_graph_for_test(missing_endpoint)
            .unwrap();

        let current = store.admit_native_read().unwrap();
        let current_memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
        let mut current_runtime = RuntimeContext::new(
            &current,
            &control,
            &current_memory,
            RuntimeLimits::default(),
        )
        .unwrap();
        let current_capability = NativeReadCapability::admit(&current, &current_runtime).unwrap();
        let mut current_initial = TreeResources::for_query(&mut current_runtime).unwrap();
        let current_source =
            NativeQuerySource::new(current_capability, &current_initial, 8).unwrap();
        let current_catalog = NativeCatalog::open(&current_source, &mut current_initial).unwrap();
        drop(current_initial);
        let current_view = GraphReadView::new(&current_source, &current_catalog).unwrap();
        let mut current_resources = TreeResources::for_query(&mut current_runtime).unwrap();
        assert!(matches!(
            current_view.lookup_relationship(rel, &mut current_resources),
            Err(crate::property_graph::storage::tree::directory::TreeError::Missing)
        ));
        drop(current_resources);

        let mut scan = current_view
            .relationship_cursor(RelationshipTypeSelection::All, &mut current_runtime)
            .unwrap();
        let mut output = [sentinel];
        assert!(matches!(
            current_view.scan_relationships(&mut scan, &mut output, &mut current_runtime),
            Err(crate::property_graph::storage::tree::directory::TreeError::Missing)
        ));
        assert_eq!(output, [sentinel]);
        drop(scan);

        let mut expansion = current_view
            .expansion_cursor(
                node_a,
                DirectionSelection::Out,
                RelationshipTypeSelection::All,
                &mut current_runtime,
            )
            .unwrap();
        assert!(matches!(
            current_view.expand(&mut expansion, &mut output, &mut current_runtime),
            Err(crate::property_graph::storage::tree::directory::TreeError::Missing)
        ));
        assert_eq!(output, [sentinel]);

        drop(expansion);
        drop(current_view);
        drop(current_catalog);
        drop(current_source);
        drop(current_runtime);
        drop(current);
        drop(old_view);
        drop(old_catalog);
        drop(old_source);
        drop(old_runtime);
        drop(old);
        store.close().unwrap();
    }

    fn native_read_limits_are_cumulative_and_refused_batch_is_private_probe() -> (u64, u64, u64, u64)
    {
        use crate::property_graph::catalog::RelTypeId;
        use crate::property_graph::query::runtime::{RuntimeError, WorkKind};
        use crate::property_graph::storage::adjacency::RelationshipRow;
        use crate::property_graph::storage::tree::directory::TreeError;
        use crate::property_graph::{NodeId, RelId};

        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 87).unwrap();
        store
            .install_native_graph_for_test(actual_producer_bundle(
                &store,
                directory.path(),
                identity,
            ))
            .unwrap();
        let lease = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let node = NodeId::new((1_u128 << 100) + 1).unwrap();

        let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
        let mut runtime =
            RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(&lease, &runtime).unwrap();
        let mut initial = TreeResources::for_query(&mut runtime).unwrap();
        let source = NativeQuerySource::new(capability, &initial, 8).unwrap();
        let catalog = NativeCatalog::open(&source, &mut initial).unwrap();
        drop(initial);
        let view = GraphReadView::new(&source, &catalog).unwrap();
        let before = runtime.counters().get(WorkKind::Lookups);
        let mut resources = TreeResources::for_query(&mut runtime).unwrap();
        assert!(view.lookup_node(node, &mut resources).unwrap().is_some());
        drop(resources);
        let after_one = runtime.counters().get(WorkKind::Lookups);
        let mut resources = TreeResources::for_query(&mut runtime).unwrap();
        assert!(view.lookup_node(node, &mut resources).unwrap().is_some());
        drop(resources);
        let after_two = runtime.counters().get(WorkKind::Lookups);
        assert!(after_one > before);
        assert_eq!(after_two - after_one, after_one - before);
        let work_clean =
            u64::from(after_one > before && after_two - after_one == after_one - before);
        drop(view);
        drop(catalog);
        drop(source);
        drop(runtime);
        assert_eq!(
            memory.reserved_bytes(),
            std::mem::size_of::<QueryMemory<'_>>()
        );

        let limited_memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
        let limits = RuntimeLimits::default()
            .with_limit(WorkKind::CopiedBytes, 0)
            .unwrap();
        let mut limited = RuntimeContext::new(&lease, &control, &limited_memory, limits).unwrap();
        let limited_source_capability = NativeReadCapability::admit(&lease, &limited).unwrap();
        let mut initial = TreeResources::for_query(&mut limited).unwrap();
        let limited_source =
            NativeQuerySource::new(limited_source_capability, &initial, 8).unwrap();
        let limited_catalog = NativeCatalog::open(&limited_source, &mut initial).unwrap();
        drop(initial);
        let limited_view = GraphReadView::new(&limited_source, &limited_catalog).unwrap();
        let mut cursor = limited_view
            .relationship_cursor(RelationshipTypeSelection::All, &mut limited)
            .unwrap();
        let sentinel = RelationshipRow {
            rel: RelId::new(1).unwrap(),
            source: NodeId::new(1).unwrap(),
            target: NodeId::new(1).unwrap(),
            relationship_type: RelTypeId::new(1).unwrap(),
        };
        let mut output = [sentinel];
        let limited_result =
            limited_view.scan_relationships(&mut cursor, &mut output, &mut limited);
        let work_fire = u64::from(matches!(
            limited_result,
            Err(TreeError::Runtime(RuntimeError::Limit(
                WorkKind::CopiedBytes
            )))
        ));
        assert_eq!(work_fire, 1);
        assert_eq!(output, [sentinel]);
        assert_eq!(limited.counters().get(WorkKind::CopiedBytes), 0);
        drop(cursor);
        drop(limited_view);
        drop(limited_catalog);
        drop(limited_source);
        drop(limited);
        assert_eq!(
            limited_memory.reserved_bytes(),
            std::mem::size_of::<QueryMemory<'_>>()
        );

        let refused = QueryMemory::new(&shared, std::mem::size_of::<QueryMemory<'_>>()).unwrap();
        let baseline = refused.reserved_bytes();
        let refused_result =
            RuntimeContext::new(&lease, &control, &refused, RuntimeLimits::default());
        let memory_fire = u64::from(matches!(
            refused_result,
            Err(RuntimeError::Memory(
                crate::property_graph::query::resources::MemoryError::Limit
            ))
        ));
        assert_eq!(memory_fire, 1);
        assert_eq!(refused.reserved_bytes(), baseline);
        let memory_clean = u64::from(refused.reserved_bytes() == baseline);

        drop(lease);
        store.close().unwrap();
        (memory_fire, memory_clean, work_fire, work_clean)
    }

    #[test]
    fn native_read_limits_are_cumulative_and_refused_batch_is_private() {
        assert_eq!(
            native_read_limits_are_cumulative_and_refused_batch_is_private_probe(),
            (1, 1, 1, 1)
        );
    }

    fn native_read_undirected_self_loop_and_parallel_edges_are_exact_probe()
    -> Vec<crate::property_graph::storage::adjacency::RelationshipRow> {
        use crate::property_graph::catalog::RelTypeId;
        use crate::property_graph::query::runtime::WorkKind;
        use crate::property_graph::storage::adjacency::{RangeDescriptor, RelationshipRow};
        use crate::property_graph::storage::tree::directory::DirectoryCursor;
        use crate::property_graph::storage::tree::{Key, TreeKind};
        use crate::property_graph::{NodeId, RelId};

        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 89).unwrap();
        let installed = actual_producer_bundle(&store, directory.path(), identity);
        store.install_native_graph_for_test(installed).unwrap();
        let lease = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut runtime =
            RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(&lease, &runtime).unwrap();
        let mut initial = TreeResources::for_query(&mut runtime).unwrap();
        let source = NativeQuerySource::new(capability, &initial, 8).unwrap();
        let catalog = NativeCatalog::open(&source, &mut initial).unwrap();
        drop(initial);
        let view = GraphReadView::new(&source, &catalog).unwrap();
        let node_a = NodeId::new((1_u128 << 100) + 1).unwrap();
        let node_b = NodeId::new((1_u128 << 100) + 2).unwrap();
        let rel_base = 1_u128 << 110;
        let root = lease
            .bundle()
            .roots()
            .directory(TreeKind::OutRanges)
            .unwrap();
        let mut resources = TreeResources::for_query(&mut runtime).unwrap();
        let mut ranges = DirectoryCursor::seek(&source, root, None, &mut resources).unwrap();
        let mut node_descriptors = Vec::new();
        while let Some(entry) = ranges.next_entry(&mut resources).unwrap() {
            let Key::Inline(key) = entry.key() else {
                panic!("persisted adjacency descriptor key must be inline");
            };
            let descriptor = RangeDescriptor::decode(root.kind(), key, entry.value()).unwrap();
            if descriptor.key().node == node_a {
                node_descriptors.push(descriptor.key());
            }
        }
        assert_eq!(node_descriptors.len(), 2);
        assert_ne!(
            node_descriptors.first().unwrap().rel_type,
            node_descriptors.last().unwrap().rel_type
        );
        drop(ranges);
        drop(resources);
        let dummy = RelationshipRow {
            rel: RelId::new(1).unwrap(),
            source: node_a,
            target: node_b,
            relationship_type: RelTypeId::new(1).unwrap(),
        };
        let mut cursor = view
            .expansion_cursor(
                node_a,
                DirectionSelection::Undirected,
                RelationshipTypeSelection::All,
                &mut runtime,
            )
            .unwrap();
        let mut observed = Vec::new();
        loop {
            let before = runtime.counters().get(WorkKind::AdjacencyEntries);
            let mut output = [dummy];
            let (count, state) = view.expand(&mut cursor, &mut output, &mut runtime).unwrap();
            let adjacency_work = runtime.counters().get(WorkKind::AdjacencyEntries) - before;
            assert!(
                adjacency_work <= 8,
                "one pull replayed {adjacency_work} physical rows"
            );
            observed.extend_from_slice(&output[..count]);
            if state == CursorState::Done {
                break;
            }
        }
        assert_eq!(
            observed,
            vec![
                RelationshipRow {
                    rel: RelId::new(rel_base + 1).unwrap(),
                    source: node_a,
                    target: node_b,
                    relationship_type: RelTypeId::new(1).unwrap(),
                },
                RelationshipRow {
                    rel: RelId::new(rel_base + 3).unwrap(),
                    source: node_a,
                    target: node_b,
                    relationship_type: RelTypeId::new(1).unwrap(),
                },
                RelationshipRow {
                    rel: RelId::new(rel_base + 2).unwrap(),
                    source: node_a,
                    target: node_a,
                    relationship_type: RelTypeId::new(2).unwrap(),
                },
            ]
        );
        drop(cursor);
        let mut empty = view
            .expansion_cursor(
                node_a,
                DirectionSelection::Undirected,
                RelationshipTypeSelection::Any(&[]),
                &mut runtime,
            )
            .unwrap();
        let mut untouched = [dummy];
        assert_eq!(
            view.expand(&mut empty, &mut untouched, &mut runtime)
                .unwrap(),
            (0, CursorState::Done)
        );
        assert_eq!(untouched, [dummy]);
        drop(empty);
        drop(view);
        drop(catalog);
        drop(source);
        drop(runtime);
        drop(lease);
        store.close().unwrap();
        observed
    }

    #[test]
    fn native_read_undirected_self_loop_and_parallel_edges_are_exact() {
        let observed = native_read_undirected_self_loop_and_parallel_edges_are_exact_probe();
        assert_eq!(observed.len(), 3);
    }

    fn caller_cancel_and_clean_control_contract() -> (u64, u64) {
        use crate::property_graph::NodeId;
        use crate::property_graph::query::QueryError;
        use crate::property_graph::query::runtime::RuntimeError;
        use crate::property_graph::storage::tree::directory::TreeError;

        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 82).unwrap();
        store
            .install_native_graph_for_test(actual_producer_bundle(
                &store,
                directory.path(),
                identity,
            ))
            .unwrap();
        let lease = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
        let cancel = CancelToken::new();
        let control = QueryControl::Cancel(cancel.clone());
        let mut runtime =
            RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(&lease, &runtime).unwrap();
        let mut initial = TreeResources::for_query(&mut runtime).unwrap();
        let source = NativeQuerySource::new(capability, &initial, 8).unwrap();
        let catalog = NativeCatalog::open(&source, &mut initial).unwrap();
        drop(initial);
        let view = GraphReadView::new(&source, &catalog).unwrap();
        cancel.cancel();
        let cancel_fire = {
            let cancelled = TreeResources::for_query(&mut runtime);
            u64::from(matches!(
                cancelled,
                Err(TreeError::Runtime(RuntimeError::Value(
                    QueryError::Cancelled
                )))
            ))
        };
        assert_eq!(cancel_fire, 1);
        drop(view);
        drop(catalog);
        drop(source);
        drop(runtime);

        let clean_memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
        let clean_control = QueryControl::Cancel(CancelToken::new());
        let mut clean = RuntimeContext::new(
            &lease,
            &clean_control,
            &clean_memory,
            RuntimeLimits::default(),
        )
        .unwrap();
        let capability = NativeReadCapability::admit(&lease, &clean).unwrap();
        let mut initial = TreeResources::for_query(&mut clean).unwrap();
        let source = NativeQuerySource::new(capability, &initial, 8).unwrap();
        let catalog = NativeCatalog::open(&source, &mut initial).unwrap();
        drop(initial);
        let view = GraphReadView::new(&source, &catalog).unwrap();
        let mut resources = TreeResources::for_query(&mut clean).unwrap();
        let clean_control = u64::from(
            view.lookup_node(NodeId::new((1_u128 << 100) + 1).unwrap(), &mut resources)
                .unwrap()
                .is_some(),
        );
        assert_eq!(clean_control, 1);
        drop(resources);
        drop(view);
        drop(catalog);
        drop(source);
        drop(clean);
        drop(lease);
        store.close().unwrap();
        (cancel_fire, clean_control)
    }

    fn scheduled_map_io_fire_and_clean_control_contract() -> (u64, u64) {
        use crate::property_graph::NodeId;
        use crate::property_graph::storage::tree::directory::TreeError;

        let directory = tempfile::tempdir().expect("store directory");
        let vfs = Arc::new(ScheduledMapVfs::new());
        let store = Store::open_with_test_dependencies(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
            StoreTestDependencies::new(vfs.clone(), Arc::new(ManualMonotonicClock::new())),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 81).unwrap();
        store
            .install_native_graph_for_test(actual_producer_bundle(
                &store,
                directory.path(),
                identity,
            ))
            .unwrap();
        let lease = store.admit_native_read().unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut runtime =
            RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(&lease, &runtime).unwrap();
        let mut initial = TreeResources::for_query(&mut runtime).unwrap();
        let source = NativeQuerySource::new(capability, &initial, 8).unwrap();
        let catalog = NativeCatalog::open(&source, &mut initial).unwrap();
        drop(initial);
        let view = GraphReadView::new(&source, &catalog).unwrap();
        let scheduled = vfs.arm_next();
        let node = NodeId::new((1_u128 << 100) + 1).unwrap();
        assert!(matches!(
            view.lookup_node(
                node,
                &mut TreeResources::for_query(&mut runtime).unwrap()
            ),
            Err(TreeError::Io(error)) if error.kind() == std::io::ErrorKind::Other
        ));
        assert_eq!(vfs.calls.load(Ordering::Relaxed), scheduled);
        assert_eq!(vfs.fires.load(Ordering::Relaxed), 1);

        vfs.disarm();
        let mut clean = TreeResources::for_query(&mut runtime).unwrap();
        assert!(view.lookup_node(node, &mut clean).unwrap().is_some());
        drop(clean);
        assert_eq!(vfs.fires.load(Ordering::Relaxed), 1);
        drop(view);
        drop(catalog);
        drop(source);
        drop(runtime);
        assert_eq!(
            memory.reserved_bytes(),
            std::mem::size_of::<QueryMemory<'_>>()
        );
        drop(lease);
        store.close().unwrap();
        (vfs.fires.load(Ordering::Relaxed), 1)
    }

    struct ObserveNativeReadStats<'a> {
        store: &'a Store,
        baseline_mapped: u64,
        baseline_resident: u64,
        baseline_queries: u64,
    }

    impl NativeReadConsumer<(u64, u64)> for ObserveNativeReadStats<'_> {
        fn consume<'s, 'lease, 'm, 'g>(
            &mut self,
            view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
            runtime: &mut RuntimeContext<'lease, 'm, 'g>,
        ) -> Result<(u64, u64), TreeError> {
            let mut resources = TreeResources::for_query(runtime)?;
            let node = NodeId::new((1_u128 << 100) + 1)
                .map_err(|_| TreeError::Invalid("stats fixture node identity"))?;
            if view.lookup_node(node, &mut resources)?.is_none() {
                return Err(TreeError::Invalid("stats fixture node missing"));
            }
            drop(resources);
            let live = self
                .store
                .stats()
                .map_err(|_| TreeError::Invalid("stats unavailable"))?;
            assert_eq!(live.active_queries, self.baseline_queries + 1);
            assert!(live.mapped_bytes > self.baseline_mapped);
            assert!(live.mapped_resident_bytes > self.baseline_resident);
            Ok((live.mapped_bytes, live.mapped_resident_bytes))
        }
    }

    #[cfg_attr(test, test)]
    fn native_read_stats_track_live_mapping_residency_and_active_query() {
        let directory = tempfile::tempdir().expect("store directory");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("open store");
        let identity = StoreInstanceId::new(1_u128 << 80).unwrap();
        store
            .install_native_graph_for_test(actual_producer_bundle(
                &store,
                directory.path(),
                identity,
            ))
            .unwrap();
        let baseline = store.stats().unwrap();
        let live = store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                8 * 1024 * 1024,
                8,
                ObserveNativeReadStats {
                    store: &store,
                    baseline_mapped: baseline.mapped_bytes,
                    baseline_resident: baseline.mapped_resident_bytes,
                    baseline_queries: baseline.active_queries,
                },
            )
            .unwrap();
        assert!(live.0 > baseline.mapped_bytes);
        assert!(live.1 > baseline.mapped_resident_bytes);
        let after = store.stats().unwrap();
        assert_eq!(after.mapped_bytes, baseline.mapped_bytes);
        assert_eq!(after.mapped_resident_bytes, baseline.mapped_resident_bytes);
        assert_eq!(after.active_queries, baseline.active_queries);
        store.close().unwrap();
    }

    pub(crate) fn run_adversarial_probe(
        seed: u64,
    ) -> crate::graph_read_view_test_support::ActualProbeReport {
        use crate::graph_read_view_test_support::{
            ActualProbeReport, ObservedRelationship, PathReceipt,
        };
        let mut receipts = Vec::new();
        let mut receipt = |key, fires, clean_controls| {
            receipts.push(PathReceipt {
                key,
                fires,
                clean_controls,
            });
        };
        if seed & 1 == 0 {
            native_read_admission_registers_before_replacement_capture();
            native_read_clone_retains_one_registry_entry_until_final_drop();
        } else {
            native_read_clone_retains_one_registry_entry_until_final_drop();
            native_read_admission_registers_before_replacement_capture();
        }
        receipt("property-graph.read-view.admission-capture", 1, 1);
        native_read_old_view_lazily_opens_unmapped_artifact_after_replacement();
        receipt("property-graph.read-view.old-lazy-open", 1, 1);
        native_read_all_operations_use_one_admitted_bundle();
        receipt("property-graph.read-view.coherent-reads", 0, 1);
        native_read_cursor_rejects_same_view_memory_different_runtime();
        native_read_cursor_rejects_same_metadata_foreign_admission();
        receipt("property-graph.read-view.cursor-mismatch", 2, 0);
        native_read_catalog_and_required_refs_cannot_be_substituted();
        native_read_missing_or_corrupt_lazy_file_is_not_absence();
        receipt("property-graph.read-view.source-identity-format", 2, 1);
        let (io_fires, io_clean) = scheduled_map_io_fire_and_clean_control_contract();
        receipt("property-graph.read-view.io.fire", io_fires, 0);
        receipt("property-graph.read-view.io.clean", 0, io_clean);
        let (memory_fire, memory_clean, work_fire, work_clean) =
            native_read_limits_are_cumulative_and_refused_batch_is_private_probe();
        receipt("property-graph.read-view.memory.fire", memory_fire, 0);
        receipt("property-graph.read-view.memory.clean", 0, memory_clean);
        receipt("property-graph.read-view.work.fire", work_fire, 0);
        receipt("property-graph.read-view.work.clean", 0, work_clean);
        let (cancel_fire, cancel_clean) = caller_cancel_and_clean_control_contract();
        receipt(
            "property-graph.read-view.caller-cancel.fire",
            cancel_fire,
            0,
        );
        receipt(
            "property-graph.read-view.caller-cancel.clean",
            0,
            cancel_clean,
        );
        native_read_close_cancels_and_drains_current_and_retired_leases();
        receipt("property-graph.read-view.close-first-drain", 1, 1);
        close_owner::native_close_drain_releases_last_temporary_owner();
        receipt("property-graph.read-view.close-drain-last-owner", 0, 1);
        close_owner::native_close_best_effort_releases_last_temporary_owner();
        receipt("property-graph.read-view.close-drop-last-owner", 0, 1);
        native_read_drop_cancels_without_destroying_borrowed_mapping();
        receipt("property-graph.read-view.release", 1, 1);
        native_prepared_artifacts_retain_exact_base_source_and_abort_owners();
        receipt("property-graph.read-view.preparation-abort", 1, 1);
        native_read_every_edge_path_checks_both_endpoint_states();
        let relationships = native_read_undirected_self_loop_and_parallel_edges_are_exact_probe()
            .into_iter()
            .map(|row| ObservedRelationship {
                rel: row.rel.get(),
                source: row.source.get(),
                target: row.target.get(),
                relationship_type: row.relationship_type.get(),
            })
            .collect();
        ActualProbeReport {
            receipts,
            relationships,
        }
    }
}
