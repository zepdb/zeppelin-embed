//! Store lifecycle and memory accounting.

mod budget;
mod cancel;
mod clock;
mod close;
pub mod durability;
pub(crate) mod graph_cache;
mod hybrid;
pub mod lock;
mod pool;
#[cfg(test)]
mod shared_bound_tests;
mod snapshot;
pub(crate) mod stats;

pub use cancel::{
    CancelToken, Deadline, DeadlineError, QueryCancellation, QueryControl, QueryError,
};
#[cfg(any(test, feature = "test-support"))]
pub use clock::ManualMonotonicClock;
pub use clock::{MonotonicClock, SystemMonotonicClock};
pub use snapshot::{
    InMemorySegment, InMemorySegmentFactors, PreparedSegment, PublishedSnapshot, SnapshotLease,
};
pub use stats::Stats;

use std::collections::BTreeMap;
use std::collections::HashSet;
#[cfg(any(test, feature = "test-support"))]
use std::io::IoSlice;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

use durability::{
    CommitTier, DurabilityMode, DurabilityPolicy, DurabilityPolicyError, SyncRequirement,
};
use lock::{StoreLock, StoreLockError};

use self::close::BackgroundThread;

/// Store-owned infrastructure injected only by deterministic tests.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone)]
pub struct StoreTestDependencies {
    vfs: Arc<dyn crate::vfs::Vfs>,
    clock: Arc<dyn MonotonicClock>,
    hybrid_leg_fault: Option<HybridLegTestFault>,
    storage_fault_controller: Option<StorageFaultController>,
    ingest_retention_fault_controller: Option<crate::ingest::IngestRetentionFaultController>,
    metadata_test_controller: Option<Arc<crate::planner::MetadataTestController>>,
    pub(crate) vector_fault_controller: Option<crate::scan::vector_fault::VectorFaultController>,
    pub(crate) kernel_fault_controller: Option<crate::kernels::vector_fault::KernelFaultController>,
    vector_seal_scheme: Option<crate::quant::QuantScheme>,
}

#[cfg(any(test, feature = "test-support"))]
impl StoreTestDependencies {
    /// Binds one filesystem and monotonic clock to every operation performed
    /// by a test store handle.
    #[must_use]
    pub fn new(vfs: Arc<dyn crate::vfs::Vfs>, clock: Arc<dyn MonotonicClock>) -> Self {
        Self {
            vfs,
            clock,
            hybrid_leg_fault: None,
            storage_fault_controller: None,
            ingest_retention_fault_controller: None,
            metadata_test_controller: None,
            vector_fault_controller: None,
            kernel_fault_controller: None,
            vector_seal_scheme: None,
        }
    }

    /// Arms one Store-owned hybrid fault; it is consumed exactly once.
    #[must_use]
    pub const fn with_hybrid_leg_fault(mut self, fault: HybridLegTestFault) -> Self {
        self.hybrid_leg_fault = Some(fault);
        self
    }

    /// Arms one storage-family fault controller at Store-owned production seams.
    #[must_use]
    pub fn with_storage_fault_controller(mut self, controller: StorageFaultController) -> Self {
        self.storage_fault_controller = Some(controller);
        self
    }

    /// Arms one ingest-retention fault at a Store-owned mutation checkpoint.
    #[must_use]
    pub fn with_ingest_retention_fault_controller(
        mut self,
        controller: crate::ingest::IngestRetentionFaultController,
    ) -> Self {
        self.ingest_retention_fault_controller = Some(controller);
        self
    }

    /// Binds one metadata/planner observation and fault controller to the
    /// public Store query path.
    #[must_use]
    pub fn with_metadata_test_controller(
        mut self,
        controller: Arc<crate::planner::MetadataTestController>,
    ) -> Self {
        self.metadata_test_controller = Some(controller);
        self
    }

    /// Arms one vector execution fault at the Store-owned query path.
    #[must_use]
    pub fn with_vector_fault_controller(
        mut self,
        controller: crate::scan::vector_fault::VectorFaultController,
    ) -> Self {
        self.vector_fault_controller = Some(controller);
        self
    }

    /// Forces and observes one concrete kernel backend through Store search.
    #[must_use]
    pub fn with_kernel_fault_controller(
        mut self,
        controller: crate::kernels::vector_fault::KernelFaultController,
    ) -> Self {
        self.kernel_fault_controller = Some(controller);
        self
    }

    /// Selects the immutable vector codec used by the next public Store seal.
    ///
    /// This test-support-only seam leaves public ingest and shipping defaults
    /// unchanged while exercising document-preserving Int8 publication.
    #[must_use]
    pub const fn with_vector_seal_scheme(mut self, scheme: crate::quant::QuantScheme) -> Self {
        self.vector_seal_scheme = Some(scheme);
        self
    }
}

/// Narrow storage-family faults available only through hidden test dependencies.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub enum StorageTestFault {
    TornWalHeader,
    TornWalBody,
    TornWalChecksum,
    PostCommitError,
    ManifestPreRename,
    ManifestPostRename,
    CorruptSegmentRegion,
    WrongManifestObject,
    WrongSegmentObject,
    ListOmission { file_name: String },
    DeleteOmission { file_name: String },
}

#[cfg(any(test, feature = "test-support"))]
impl StorageTestFault {
    const fn operation(&self) -> &'static str {
        match self {
            Self::TornWalHeader | Self::TornWalBody | Self::TornWalChecksum => "wal-prefix",
            Self::PostCommitError => "retry",
            Self::ManifestPreRename | Self::ManifestPostRename => "publication",
            Self::CorruptSegmentRegion | Self::WrongManifestObject | Self::WrongSegmentObject => {
                "format-check"
            }
            Self::ListOmission { .. } | Self::DeleteOmission { .. } => "orphan-cleanup",
        }
    }

    const fn key(&self) -> &'static str {
        match self {
            Self::TornWalHeader => "torn-wal-header",
            Self::TornWalBody => "torn-wal-body",
            Self::TornWalChecksum => "torn-wal-checksum",
            Self::PostCommitError => "post-commit-error",
            Self::ManifestPreRename => "manifest-pre-rename-crash",
            Self::ManifestPostRename => "manifest-post-rename-crash",
            Self::CorruptSegmentRegion => "corrupt-segment-region",
            Self::WrongManifestObject => "wrong-manifest-object",
            Self::WrongSegmentObject => "wrong-segment-object",
            Self::ListOmission { .. } | Self::DeleteOmission { .. } => "list-delete-omission",
        }
    }
}

/// Exact seed-derived target facts supplied before one storage operation runs.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub struct StorageFaultPlan {
    op_index: u32,
    artifact: String,
    offset: Option<u64>,
    segment: Option<crate::segment::SegmentId>,
    region_kind: Option<u16>,
    chunk: Option<u32>,
}

#[cfg(any(test, feature = "test-support"))]
impl StorageFaultPlan {
    /// Creates an exact artifact plan for one operation index.
    #[must_use]
    pub fn new(op_index: u32, artifact: impl Into<String>) -> Self {
        Self {
            op_index,
            artifact: artifact.into(),
            offset: None,
            segment: None,
            region_kind: None,
            chunk: None,
        }
    }

    /// Pins the exact byte offset selected by the primitive fixture.
    #[must_use]
    pub const fn with_offset(mut self, offset: u64) -> Self {
        self.offset = Some(offset);
        self
    }

    /// Pins the exact segment, region kind, and checksum chunk.
    #[must_use]
    pub const fn with_segment_region(
        mut self,
        segment: crate::segment::SegmentId,
        region_kind: u16,
        chunk: u32,
    ) -> Self {
        self.segment = Some(segment);
        self.region_kind = Some(region_kind);
        self.chunk = Some(chunk);
        self
    }

    /// Pins the exact newly published segment for a manifest checkpoint.
    #[must_use]
    pub const fn with_segment(mut self, segment: crate::segment::SegmentId) -> Self {
        self.segment = Some(segment);
        self
    }

    /// Exact operation index in the generated program.
    #[must_use]
    pub const fn op_index(&self) -> u32 {
        self.op_index
    }

    /// Normalized planned artifact.
    #[must_use]
    pub fn artifact(&self) -> &str {
        &self.artifact
    }

    /// Planned mutation/checkpoint byte offset when applicable.
    #[must_use]
    pub const fn offset(&self) -> Option<u64> {
        self.offset
    }

    /// Planned segment identity when applicable.
    #[must_use]
    pub const fn segment(&self) -> Option<crate::segment::SegmentId> {
        self.segment
    }

    /// Planned region kind when applicable.
    #[must_use]
    pub const fn region_kind(&self) -> Option<u16> {
        self.region_kind
    }

    /// Planned checksum chunk when applicable.
    #[must_use]
    pub const fn chunk(&self) -> Option<u32> {
        self.chunk
    }
}

/// Typed production checkpoint that consumed a storage fault.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub enum StorageReceiptSite {
    WalOpenHeaderValidation,
    WalOpenRecordValidation,
    WalOpenRecordChecksum,
    WalCommitAppendAfterInnerSuccess,
    ManifestCommitBeforeRename,
    ManifestCommitAfterRename,
    SegmentReadRegionChecksum,
    ManifestOpenFamilyValidation,
    SegmentOpenFamilyValidation,
    SegmentOpenObjectIdentity,
    OrphanCleanupList,
    OrphanCleanupDelete,
}

#[cfg(any(test, feature = "test-support"))]
impl StorageReceiptSite {
    const fn key(self) -> &'static str {
        match self {
            Self::WalOpenHeaderValidation => "WalOpen.HeaderValidation",
            Self::WalOpenRecordValidation => "WalOpen.RecordValidation",
            Self::WalOpenRecordChecksum => "WalOpen.RecordChecksum",
            Self::WalCommitAppendAfterInnerSuccess => "WalCommit.AppendAfterInnerSuccess",
            Self::ManifestCommitBeforeRename => "ManifestCommit.BeforeRename",
            Self::ManifestCommitAfterRename => "ManifestCommit.AfterRename",
            Self::SegmentReadRegionChecksum => "SegmentRead.RegionChecksum",
            Self::ManifestOpenFamilyValidation => "ManifestOpen.FamilyValidation",
            Self::SegmentOpenFamilyValidation => "SegmentOpen.FamilyValidation",
            Self::SegmentOpenObjectIdentity => "SegmentOpen.ObjectIdentity",
            Self::OrphanCleanupList => "OrphanCleanup.List",
            Self::OrphanCleanupDelete => "OrphanCleanup.Delete",
        }
    }
}

/// Exact facts observed at the production checkpoint.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub enum StorageReceiptObserved {
    WalHeader {
        artifact: String,
        reason: crate::wal::header::WalHeaderError,
    },
    WalRecord {
        artifact: String,
        offset: u64,
        reason: crate::wal::replay::CorruptionReason,
    },
    WalAppend {
        artifact: String,
        encoded_len: u64,
        first_seq: u64,
        last_seq: u64,
        inner_append_completed: bool,
        caller_saw_error: bool,
    },
    ManifestRename {
        temporary: String,
        committed: String,
        rename_performed: bool,
        new_segment_final: bool,
        directory_sync_returned: bool,
    },
    SegmentChecksum {
        artifact: String,
        segment: crate::segment::SegmentId,
        region_kind: u16,
        chunk: u32,
        expected_checksum: u64,
        actual_checksum: u64,
    },
    Format {
        artifact: String,
        check: crate::format::frame::FormatCheck,
        expected_family: Option<u16>,
        actual_family: Option<u16>,
        expected_id: Option<crate::segment::SegmentId>,
        actual_id: Option<crate::segment::SegmentId>,
    },
    Omission {
        artifact: String,
        deletion_observed: bool,
    },
}

/// One fact-only typed storage receipt emitted by product code.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub struct StorageFaultReceipt {
    operation: &'static str,
    fault: &'static str,
    site: StorageReceiptSite,
    plan: StorageFaultPlan,
    observed: StorageReceiptObserved,
    cardinality: u32,
}

#[cfg(any(test, feature = "test-support"))]
impl StorageFaultReceipt {
    #[must_use]
    pub const fn campaign(&self) -> &'static str {
        "storage-durability"
    }

    #[must_use]
    pub const fn operation(&self) -> &'static str {
        self.operation
    }

    #[must_use]
    pub const fn fault(&self) -> &'static str {
        self.fault
    }

    #[must_use]
    pub const fn site(&self) -> &'static str {
        self.site.key()
    }

    #[must_use]
    pub const fn typed_site(&self) -> StorageReceiptSite {
        self.site
    }

    #[must_use]
    pub const fn cardinality(&self) -> u32 {
        self.cardinality
    }

    #[must_use]
    pub const fn plan(&self) -> &StorageFaultPlan {
        &self.plan
    }

    #[must_use]
    pub const fn observed(&self) -> &StorageReceiptObserved {
        &self.observed
    }

    /// Decodes the exact receipt acknowledged by an aborting publication child.
    ///
    /// The child writes this only after `emit` has stored the typed production
    /// receipt. Parsing remains in product test support so an external harness
    /// cannot manufacture a generic receipt from partial fields.
    pub fn from_manifest_abort_acknowledgment(acknowledgment: &str) -> Result<Self, String> {
        let body = acknowledgment
            .strip_suffix('\n')
            .filter(|body| !body.contains('\n'))
            .ok_or_else(|| {
                "manifest abort acknowledgment must end in exactly one newline".to_owned()
            })?;
        let fields = body
            .split('|')
            .map(|field| {
                field.split_once('=').ok_or_else(|| {
                    format!("manifest abort acknowledgment field lacks '=': {field}")
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let expected_keys = [
            "campaign",
            "operation",
            "fault",
            "site",
            "op_index",
            "cardinality",
            "artifact",
            "temporary",
            "committed",
            "rename_performed",
            "new_segment_final",
            "directory_sync_returned",
        ];
        if fields.len() != expected_keys.len()
            || fields
                .iter()
                .map(|(key, _)| *key)
                .ne(expected_keys.iter().copied())
        {
            return Err("manifest abort acknowledgment field inventory differs".to_owned());
        }
        let mut values = fields.into_iter().map(|(_, value)| value);
        let campaign = values
            .next()
            .ok_or_else(|| "manifest abort acknowledgment lacks campaign".to_owned())?;
        let operation = values
            .next()
            .ok_or_else(|| "manifest abort acknowledgment lacks operation".to_owned())?;
        let fault = values
            .next()
            .ok_or_else(|| "manifest abort acknowledgment lacks fault".to_owned())?;
        let site = values
            .next()
            .ok_or_else(|| "manifest abort acknowledgment lacks site".to_owned())?;
        let op_index = values
            .next()
            .ok_or_else(|| "manifest abort acknowledgment lacks op index".to_owned())?
            .parse::<u32>()
            .map_err(|error| format!("manifest abort op index is invalid: {error}"))?;
        let cardinality = values
            .next()
            .ok_or_else(|| "manifest abort acknowledgment lacks cardinality".to_owned())?
            .parse::<u32>()
            .map_err(|error| format!("manifest abort cardinality is invalid: {error}"))?;
        let artifact = values
            .next()
            .ok_or_else(|| "manifest abort acknowledgment lacks artifact".to_owned())?;
        let temporary = values
            .next()
            .ok_or_else(|| "manifest abort acknowledgment lacks temporary path".to_owned())?;
        let committed = values
            .next()
            .ok_or_else(|| "manifest abort acknowledgment lacks committed path".to_owned())?;
        let parse_bool = |field: &str, value: &str| match value {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(format!("manifest abort {field} is not a bool: {value}")),
        };
        let rename_performed = parse_bool(
            "rename_performed",
            values
                .next()
                .ok_or_else(|| "manifest abort acknowledgment lacks rename result".to_owned())?,
        )?;
        let new_segment_final = parse_bool(
            "new_segment_final",
            values.next().ok_or_else(|| {
                "manifest abort acknowledgment lacks final-segment result".to_owned()
            })?,
        )?;
        let directory_sync_returned = parse_bool(
            "directory_sync_returned",
            values.next().ok_or_else(|| {
                "manifest abort acknowledgment lacks directory-sync result".to_owned()
            })?,
        )?;
        if campaign != "storage-durability"
            || operation != "publication"
            || cardinality != 1
            || artifact != crate::manifest::io::MANIFEST_FILE
            || temporary != crate::manifest::io::MANIFEST_TEMP_FILE
            || committed != crate::manifest::io::MANIFEST_FILE
            || !new_segment_final
            || directory_sync_returned
        {
            return Err("manifest abort acknowledgment fixed facts differ".to_owned());
        }
        let (fault, site, expected_rename) = match (fault, site) {
            ("manifest-pre-rename-crash", "ManifestCommit.BeforeRename") => (
                "manifest-pre-rename-crash",
                StorageReceiptSite::ManifestCommitBeforeRename,
                false,
            ),
            ("manifest-post-rename-crash", "ManifestCommit.AfterRename") => (
                "manifest-post-rename-crash",
                StorageReceiptSite::ManifestCommitAfterRename,
                true,
            ),
            _ => return Err("manifest abort fault and production site differ".to_owned()),
        };
        if rename_performed != expected_rename {
            return Err("manifest abort rename result differs from production site".to_owned());
        }
        Ok(Self {
            operation: "publication",
            fault,
            site,
            plan: StorageFaultPlan::new(op_index, artifact),
            observed: StorageReceiptObserved::ManifestRename {
                temporary: temporary.to_owned(),
                committed: committed.to_owned(),
                rename_performed,
                new_segment_final,
                directory_sync_returned,
            },
            cardinality,
        })
    }
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Debug)]
struct StorageFaultState {
    fault: StorageTestFault,
    plan: StorageFaultPlan,
    consumed: bool,
    receipt: Option<StorageFaultReceipt>,
    cleanup_report: Option<StorageCleanupReport>,
    #[cfg(unix)]
    child_abort_ack: Option<std::os::unix::net::UnixStream>,
}

/// Truthful internal result of Store-owned orphan cleanup.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub struct StorageCleanupReport {
    reclaimed_bytes: u64,
    deleted_paths: Vec<String>,
    retained_eligible_paths: Vec<String>,
    directory_synced: bool,
}

#[cfg(any(test, feature = "test-support"))]
impl StorageCleanupReport {
    /// Exact bytes removed from the filesystem.
    #[must_use]
    pub const fn reclaimed_bytes(&self) -> u64 {
        self.reclaimed_bytes
    }

    /// Normalized relative paths confirmed absent after deletion.
    #[must_use]
    pub fn deleted_paths(&self) -> &[String] {
        &self.deleted_paths
    }

    /// Eligible paths whose deletion was omitted by an armed test fault.
    #[must_use]
    pub fn retained_eligible_paths(&self) -> &[String] {
        &self.retained_eligible_paths
    }

    /// Whether the cleanup issued its required directory synchronization.
    #[must_use]
    pub const fn directory_synced(&self) -> bool {
        self.directory_synced
    }
}

/// Shared handle that arms one storage fault and receives its production receipt.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug)]
#[doc(hidden)]
pub struct StorageFaultController {
    state: Arc<Mutex<StorageFaultState>>,
}

#[cfg(any(test, feature = "test-support"))]
impl StorageFaultController {
    /// Arms one controller. A clone observes the same one-shot receipt.
    #[must_use]
    pub fn new(fault: StorageTestFault, plan: StorageFaultPlan) -> Self {
        Self {
            state: Arc::new(Mutex::new(StorageFaultState {
                fault,
                plan,
                consumed: false,
                receipt: None,
                cleanup_report: None,
                #[cfg(unix)]
                child_abort_ack: None,
            })),
        }
    }

    /// Supplies the inherited descriptor used to acknowledge a child abort.
    #[cfg(unix)]
    #[must_use]
    pub fn with_child_abort_ack(self, acknowledgment: std::os::unix::net::UnixStream) -> Self {
        if let Ok(mut state) = self.state.lock() {
            state.child_abort_ack = Some(acknowledgment);
        }
        self
    }

    /// Takes the single receipt emitted by the intended production operation.
    pub fn take_receipt(&self) -> Option<StorageFaultReceipt> {
        self.state
            .lock()
            .ok()
            .and_then(|mut state| state.receipt.take())
    }

    /// Takes the exact result of the most recent public-open cleanup pass.
    pub fn take_cleanup_report(&self) -> Option<StorageCleanupReport> {
        self.state
            .lock()
            .ok()
            .and_then(|mut state| state.cleanup_report.take())
    }

    fn record_cleanup_report(&self, report: crate::manifest::io::OrphanCleanupReport) {
        let normalize = |paths: Vec<PathBuf>| {
            paths
                .into_iter()
                .filter_map(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_owned)
                })
                .collect::<Vec<_>>()
        };
        if let Ok(mut state) = self.state.lock() {
            state.cleanup_report = Some(StorageCleanupReport {
                reclaimed_bytes: report.reclaimed_bytes,
                deleted_paths: normalize(report.deleted_paths),
                retained_eligible_paths: normalize(report.retained_eligible_paths),
                directory_synced: report.directory_synced,
            });
        }
    }

    fn armed_fault(&self) -> Option<StorageTestFault> {
        self.state
            .lock()
            .ok()
            .and_then(|state| (!state.consumed).then(|| state.fault.clone()))
    }

    fn planned_segment(&self) -> Option<crate::segment::SegmentId> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.plan.segment())
    }

    fn emit(&self, site: StorageReceiptSite, observed: StorageReceiptObserved) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.consumed {
            return false;
        }
        state.consumed = true;
        state.receipt = Some(StorageFaultReceipt {
            operation: state.fault.operation(),
            fault: state.fault.key(),
            site,
            plan: state.plan.clone(),
            observed,
            cardinality: 1,
        });
        true
    }

    fn abort_after_manifest_receipt(&self) -> ! {
        #[cfg(unix)]
        if let Ok(mut state) = self.state.lock() {
            let wire_receipt = state
                .receipt
                .as_ref()
                .and_then(storage_manifest_abort_wire_receipt);
            if let (Some(wire_receipt), Some(acknowledgment)) =
                (wire_receipt, state.child_abort_ack.as_mut())
            {
                use std::io::Write as _;
                let _ = acknowledgment.write_all(wire_receipt.as_bytes());
                let _ = acknowledgment.flush();
            }
        }
        std::process::abort()
    }
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static STORAGE_OPEN_CONTROLLER: std::cell::RefCell<
        Option<std::sync::Weak<Mutex<StorageFaultState>>>
    > = const { std::cell::RefCell::new(None) };
}

#[cfg(any(test, feature = "test-support"))]
fn with_storage_open_controller<R>(
    controller: Option<&StorageFaultController>,
    operation: impl FnOnce() -> R,
) -> R {
    STORAGE_OPEN_CONTROLLER.with(|slot| {
        let previous = slot.replace(controller.map(|controller| Arc::downgrade(&controller.state)));
        let result = operation();
        let _ = slot.replace(previous);
        result
    })
}

#[cfg(any(test, feature = "test-support"))]
fn storage_open_controller() -> Option<StorageFaultController> {
    STORAGE_OPEN_CONTROLLER.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .map(|state| StorageFaultController { state })
    })
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn record_storage_wal_recovery_fault(error: &crate::wal::WalRecoveryError) {
    let Some(controller) = storage_open_controller() else {
        return;
    };
    match (controller.armed_fault(), error) {
        (
            Some(StorageTestFault::TornWalHeader),
            crate::wal::WalRecoveryError::InvalidHeader(reason),
        ) => {
            let _ = controller.emit(
                StorageReceiptSite::WalOpenHeaderValidation,
                StorageReceiptObserved::WalHeader {
                    artifact: "wal.ze".to_owned(),
                    reason: *reason,
                },
            );
        }
        (
            Some(StorageTestFault::TornWalBody),
            crate::wal::WalRecoveryError::CorruptAt {
                offset,
                reason:
                    reason @ crate::wal::replay::CorruptionReason::Record {
                        error:
                            crate::wal::record::RecordError::HeaderTruncated { .. }
                            | crate::wal::record::RecordError::BodyTruncated { .. }
                            | crate::wal::record::RecordError::LengthOverflow { .. },
                        ..
                    },
            },
        ) => record_storage_wal_record_fault(
            &controller,
            StorageReceiptSite::WalOpenRecordValidation,
            *offset,
            *reason,
        ),
        (
            Some(StorageTestFault::TornWalChecksum),
            crate::wal::WalRecoveryError::CorruptAt {
                offset,
                reason:
                    reason @ crate::wal::replay::CorruptionReason::Record {
                        error: crate::wal::record::RecordError::ChecksumMismatch { .. },
                        ..
                    },
            },
        ) => record_storage_wal_record_fault(
            &controller,
            StorageReceiptSite::WalOpenRecordChecksum,
            *offset,
            *reason,
        ),
        _ => {}
    }
}

#[cfg(any(test, feature = "test-support"))]
fn record_storage_wal_record_fault(
    controller: &StorageFaultController,
    site: StorageReceiptSite,
    offset: usize,
    reason: crate::wal::replay::CorruptionReason,
) {
    let Ok(offset) = u64::try_from(offset) else {
        return;
    };
    let _ = controller.emit(
        site,
        StorageReceiptObserved::WalRecord {
            artifact: "wal.ze".to_owned(),
            offset,
            reason,
        },
    );
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn record_storage_manifest_format_fault(
    error: &crate::format::frame::FormatError,
    actual_family: Option<u16>,
) {
    let Some(controller) = storage_open_controller() else {
        return;
    };
    if matches!(
        controller.armed_fault(),
        Some(StorageTestFault::WrongManifestObject)
    ) && error.check() == crate::format::frame::FormatCheck::Family
    {
        let _ = controller.emit(
            StorageReceiptSite::ManifestOpenFamilyValidation,
            StorageReceiptObserved::Format {
                artifact: storage_artifact_name(error.artifact()).to_owned(),
                check: error.check(),
                expected_family: Some(crate::format::FormatFamily::Manifest.id()),
                actual_family,
                expected_id: None,
                actual_id: None,
            },
        );
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn record_storage_segment_format_fault(
    error: &crate::format::frame::FormatError,
    actual_family: Option<u16>,
    expected_id: crate::segment::SegmentId,
) {
    let Some(controller) = storage_open_controller() else {
        return;
    };
    if matches!(
        controller.armed_fault(),
        Some(StorageTestFault::WrongSegmentObject)
    ) && error.check() == crate::format::frame::FormatCheck::Family
    {
        let _ = controller.emit(
            StorageReceiptSite::SegmentOpenFamilyValidation,
            StorageReceiptObserved::Format {
                artifact: storage_artifact_name(error.artifact()).to_owned(),
                check: error.check(),
                expected_family: Some(crate::format::FormatFamily::Segment.id()),
                actual_family,
                expected_id: Some(expected_id),
                actual_id: None,
            },
        );
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn record_storage_segment_identity_fault(
    artifact: &str,
    expected: crate::segment::SegmentId,
    actual: crate::segment::SegmentId,
) {
    let Some(controller) = storage_open_controller() else {
        return;
    };
    if matches!(
        controller.armed_fault(),
        Some(StorageTestFault::WrongSegmentObject)
    ) {
        let _ = controller.emit(
            StorageReceiptSite::SegmentOpenObjectIdentity,
            StorageReceiptObserved::Format {
                artifact: storage_artifact_name(artifact).to_owned(),
                check: crate::format::frame::FormatCheck::ObjectIdentity,
                expected_family: None,
                actual_family: None,
                expected_id: Some(expected),
                actual_id: Some(actual),
            },
        );
    }
}

#[cfg(any(test, feature = "test-support"))]
fn storage_manifest_abort_wire_receipt(receipt: &StorageFaultReceipt) -> Option<String> {
    let StorageReceiptObserved::ManifestRename {
        temporary,
        committed,
        rename_performed,
        new_segment_final,
        directory_sync_returned,
    } = &receipt.observed
    else {
        return None;
    };
    Some(format!(
        "campaign={}|operation={}|fault={}|site={}|op_index={}|cardinality={}|artifact={}|temporary={temporary}|committed={committed}|rename_performed={rename_performed}|new_segment_final={new_segment_final}|directory_sync_returned={directory_sync_returned}\n",
        receipt.campaign(),
        receipt.operation,
        receipt.fault,
        receipt.site.key(),
        receipt.plan.op_index,
        receipt.cardinality,
        receipt.plan.artifact,
    ))
}

#[cfg(any(test, feature = "test-support"))]
fn storage_artifact_name(artifact: &str) -> &str {
    Path::new(artifact)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(artifact)
}

#[cfg(any(test, feature = "test-support"))]
type StorageSegmentControllerRegistry = Mutex<
    Vec<(
        PathBuf,
        crate::segment::SegmentId,
        std::sync::Weak<Mutex<StorageFaultState>>,
    )>,
>;

#[cfg(any(test, feature = "test-support"))]
static STORAGE_SEGMENT_CONTROLLERS: std::sync::OnceLock<StorageSegmentControllerRegistry> =
    std::sync::OnceLock::new();

#[cfg(any(test, feature = "test-support"))]
fn register_storage_segment_controller(
    store_directory: &Path,
    segment: crate::segment::SegmentId,
    controller: &StorageFaultController,
) {
    let registry = STORAGE_SEGMENT_CONTROLLERS.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(mut entries) = registry.lock() {
        entries.retain(|(_, _, state)| state.strong_count() != 0);
        entries.push((
            store_directory.to_path_buf(),
            segment,
            Arc::downgrade(&controller.state),
        ));
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn record_storage_segment_checksum_fault(
    store_directory: &Path,
    segment: crate::segment::SegmentId,
    kind: crate::segment::layout::RegionKind,
    chunk: u32,
    expected: u64,
    actual: u64,
) {
    let Some(registry) = STORAGE_SEGMENT_CONTROLLERS.get() else {
        return;
    };
    let states = match registry.lock() {
        Ok(mut entries) => {
            entries.retain(|(_, _, state)| state.strong_count() != 0);
            entries
                .iter()
                .filter(|(directory, registered, _)| {
                    directory == store_directory && *registered == segment
                })
                .filter_map(|(_, _, state)| state.upgrade())
                .collect::<Vec<_>>()
        }
        Err(_) => return,
    };
    for state in states {
        let controller = StorageFaultController { state };
        if matches!(
            controller.armed_fault(),
            Some(StorageTestFault::CorruptSegmentRegion)
        ) {
            let _ = controller.emit(
                StorageReceiptSite::SegmentReadRegionChecksum,
                StorageReceiptObserved::SegmentChecksum {
                    artifact: segment.file_name(),
                    segment,
                    region_kind: kind.id(),
                    chunk,
                    expected_checksum: expected,
                    actual_checksum: actual,
                },
            );
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
struct StorageFaultVfs {
    inner: Arc<dyn crate::vfs::Vfs>,
    controller: StorageFaultController,
}

#[cfg(any(test, feature = "test-support"))]
struct StorageFaultFile {
    inner: Box<dyn crate::vfs::VfsFile>,
    path: PathBuf,
    controller: StorageFaultController,
}

#[cfg(any(test, feature = "test-support"))]
impl StorageFaultFile {
    fn after_append(&self, encoded_len: u64, first_seq: u64, last_seq: u64) -> std::io::Result<()> {
        if matches!(
            self.controller.armed_fault(),
            Some(StorageTestFault::PostCommitError)
        ) && self.path.file_name().is_some_and(|name| name == "wal.ze")
            && self.controller.emit(
                StorageReceiptSite::WalCommitAppendAfterInnerSuccess,
                StorageReceiptObserved::WalAppend {
                    artifact: "wal.ze".to_owned(),
                    encoded_len,
                    first_seq,
                    last_seq,
                    inner_append_completed: true,
                    caller_saw_error: true,
                },
            )
        {
            return Err(std::io::Error::other(
                "scheduled post-commit error after inner append",
            ));
        }
        Ok(())
    }
}

#[cfg(any(test, feature = "test-support"))]
impl crate::vfs::VfsFile for StorageFaultFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let record_facts = storage_wal_append_facts(bytes)?;
        self.inner.append(bytes)?;
        record_facts.map_or(Ok(()), |(encoded_len, first_seq, last_seq)| {
            self.after_append(encoded_len, first_seq, last_seq)
        })
    }

    fn append_vectored(&mut self, buffers: &mut [IoSlice<'_>]) -> std::io::Result<()> {
        let mut aggregate = None::<(u64, u64, u64)>;
        for bytes in buffers.iter() {
            if let Some((encoded_len, first_seq, last_seq)) = storage_wal_append_facts(bytes)? {
                aggregate = Some(match aggregate {
                    None => (encoded_len, first_seq, last_seq),
                    Some((total, first, _)) => (
                        total.checked_add(encoded_len).ok_or_else(|| {
                            std::io::Error::other("WAL record append length overflow")
                        })?,
                        first,
                        last_seq,
                    ),
                });
            }
        }
        self.inner.append_vectored(buffers)?;
        aggregate.map_or(Ok(()), |(encoded_len, first_seq, last_seq)| {
            self.after_append(encoded_len, first_seq, last_seq)
        })
    }

    fn sync(&self, kind: crate::vfs::SyncKind) -> std::io::Result<()> {
        self.inner.sync(kind)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl crate::vfs::Vfs for StorageFaultVfs {
    fn segment_data_read_counter(&self) -> Option<Arc<AtomicU64>> {
        self.inner.segment_data_read_counter()
    }

    fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
        self.inner.ensure_directory(path, create)
    }

    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.inner.open(path)
    }

    fn open_for_map(&self, path: &Path) -> std::io::Result<std::fs::File> {
        self.inner.open_for_map(path)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.inner.read(path)
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        self.inner.read_range(path, offset, length)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.write(path, bytes)
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn crate::vfs::VfsFile>> {
        Ok(Box::new(StorageFaultFile {
            inner: self.inner.open_append(path)?,
            path: path.to_path_buf(),
            controller: self.controller.clone(),
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        let is_manifest = from
            .file_name()
            .is_some_and(|name| name == crate::manifest::io::MANIFEST_TEMP_FILE)
            && to
                .file_name()
                .is_some_and(|name| name == crate::manifest::io::MANIFEST_FILE);
        if is_manifest
            && matches!(
                self.controller.armed_fault(),
                Some(StorageTestFault::ManifestPreRename)
            )
            && self.controller.emit(
                StorageReceiptSite::ManifestCommitBeforeRename,
                StorageReceiptObserved::ManifestRename {
                    temporary: crate::manifest::io::MANIFEST_TEMP_FILE.to_owned(),
                    committed: crate::manifest::io::MANIFEST_FILE.to_owned(),
                    rename_performed: false,
                    new_segment_final: storage_planned_segment_is_final(
                        &*self.inner,
                        from,
                        self.controller.planned_segment(),
                    ),
                    directory_sync_returned: false,
                },
            )
        {
            self.controller.abort_after_manifest_receipt();
        }
        self.inner.rename(from, to)?;
        if is_manifest
            && matches!(
                self.controller.armed_fault(),
                Some(StorageTestFault::ManifestPostRename)
            )
            && self.controller.emit(
                StorageReceiptSite::ManifestCommitAfterRename,
                StorageReceiptObserved::ManifestRename {
                    temporary: crate::manifest::io::MANIFEST_TEMP_FILE.to_owned(),
                    committed: crate::manifest::io::MANIFEST_FILE.to_owned(),
                    rename_performed: true,
                    new_segment_final: storage_planned_segment_is_final(
                        &*self.inner,
                        to,
                        self.controller.planned_segment(),
                    ),
                    directory_sync_returned: false,
                },
            )
        {
            self.controller.abort_after_manifest_receipt();
        }
        Ok(())
    }

    fn sync(&self, path: &Path, kind: crate::vfs::SyncKind) -> std::io::Result<()> {
        self.inner.sync(path, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        let mut paths = self.inner.list(directory)?;
        if let Some(StorageTestFault::ListOmission { file_name }) = self.controller.armed_fault()
            && let Some(position) = paths.iter().position(|path| {
                path.file_name()
                    .is_some_and(|name| name == file_name.as_str())
            })
        {
            let _ = paths.remove(position);
            let _ = self.controller.emit(
                StorageReceiptSite::OrphanCleanupList,
                StorageReceiptObserved::Omission {
                    artifact: file_name,
                    deletion_observed: false,
                },
            );
        }
        Ok(paths)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        if let Some(StorageTestFault::DeleteOmission { file_name }) = self.controller.armed_fault()
            && path
                .file_name()
                .is_some_and(|name| name == file_name.as_str())
            && self.controller.emit(
                StorageReceiptSite::OrphanCleanupDelete,
                StorageReceiptObserved::Omission {
                    artifact: file_name,
                    deletion_observed: false,
                },
            )
        {
            return Ok(());
        }
        self.inner.delete(path)
    }
}

#[cfg(any(test, feature = "test-support"))]
fn storage_wal_append_facts(bytes: &[u8]) -> std::io::Result<Option<(u64, u64, u64)>> {
    const RECORD_HEADER_LEN: usize = 14;
    const RECORD_CHECKSUM_LEN: usize = 8;
    let has_wal_header = bytes
        .get(..8)
        .is_some_and(|magic| magic == crate::format::frame::FILE_MAGIC)
        && bytes
            .get(8..10)
            .is_some_and(|family| family == crate::format::FormatFamily::Wal.id().to_le_bytes());
    let mut offset = if has_wal_header {
        crate::wal::header::WAL_HEADER_LEN
    } else {
        0
    };
    if offset == bytes.len() {
        return Ok(None);
    }
    let mut encoded_len = 0_u64;
    let mut first_seq = None;
    let mut last_seq = None;
    while offset < bytes.len() {
        let header_end = offset
            .checked_add(RECORD_HEADER_LEN)
            .ok_or_else(|| std::io::Error::other("WAL record header offset overflow"))?;
        let header = bytes
            .get(offset..header_end)
            .ok_or_else(|| std::io::Error::other("WAL append has a torn record header"))?;
        let payload_len = usize::try_from(u32::from_le_bytes(
            header
                .get(..4)
                .ok_or_else(|| std::io::Error::other("WAL payload length is absent"))?
                .try_into()
                .map_err(|_| std::io::Error::other("WAL payload length width changed"))?,
        ))
        .map_err(|_| std::io::Error::other("WAL payload length does not fit usize"))?;
        let seq = u64::from_le_bytes(
            header
                .get(4..12)
                .ok_or_else(|| std::io::Error::other("WAL sequence is absent"))?
                .try_into()
                .map_err(|_| std::io::Error::other("WAL sequence width changed"))?,
        );
        let record_len = RECORD_HEADER_LEN
            .checked_add(payload_len)
            .and_then(|length| length.checked_add(RECORD_CHECKSUM_LEN))
            .ok_or_else(|| std::io::Error::other("WAL record length overflow"))?;
        offset = offset
            .checked_add(record_len)
            .ok_or_else(|| std::io::Error::other("WAL record end overflow"))?;
        if offset > bytes.len() {
            return Err(std::io::Error::other("WAL append has a torn record body"));
        }
        encoded_len = encoded_len
            .checked_add(
                u64::try_from(record_len)
                    .map_err(|_| std::io::Error::other("WAL record length does not fit u64"))?,
            )
            .ok_or_else(|| std::io::Error::other("WAL record append length overflow"))?;
        first_seq.get_or_insert(seq);
        last_seq = Some(seq);
    }
    match (first_seq, last_seq) {
        (Some(first), Some(last)) => Ok(Some((encoded_len, first, last))),
        _ => Ok(None),
    }
}

#[cfg(any(test, feature = "test-support"))]
fn storage_planned_segment_is_final(
    vfs: &dyn crate::vfs::Vfs,
    artifact: &Path,
    planned: Option<crate::segment::SegmentId>,
) -> bool {
    let Some(file_name) = planned.map(crate::segment::SegmentId::file_name) else {
        return false;
    };
    artifact.parent().is_some_and(|directory| {
        vfs.list(directory).is_ok_and(|paths| {
            paths.iter().any(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == file_name)
            })
        })
    })
}

/// Narrow test-only fault at a Store-owned hybrid leg seam.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub enum HybridLegTestFault {
    /// Panics after the named leg starts and before it can publish output.
    Panic(crate::fusion::FusionLeg),
}

#[allow(clippy::panic)]
fn maybe_trigger_hybrid_leg_panic(armed: bool, detail: &'static str) {
    #[cfg(any(test, feature = "test-support"))]
    if armed {
        std::panic::panic_any(detail);
    }
    #[cfg(not(any(test, feature = "test-support")))]
    let _ = (armed, detail);
}

/// Facts emitted by one Store-owned parallel hybrid execution.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub struct HybridExecutionReceipt {
    /// Thread that executed vector work.
    pub vector_thread: std::thread::ThreadId,
    /// Name of the scoped lexical worker.
    pub lexical_thread_name: Option<String>,
    /// Vector leg reached a terminal result before return.
    pub vector_completed: bool,
    /// Lexical leg reached a terminal result before return.
    pub lexical_completed: bool,
}

/// Default grace period given to admitted readers before close cancellation.
pub const DEFAULT_READER_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

/// Explicit lifecycle state for one store handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreState {
    /// New calls may be admitted.
    Open,
    /// New calls are rejected while admitted work drains.
    Closing,
    /// Teardown completed and only idempotent close/state calls remain valid.
    Closed,
}

/// Store-open configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenOptions {
    access_mode: AccessMode,
    durability_mode: DurabilityMode,
    commit_tier: CommitTier,
    reader_drain_timeout: Duration,
    max_resident_bytes: u64,
    max_temp_bytes: u64,
    epoch: Option<crate::epoch::StoreEpoch>,
    tokenizer: Option<crate::fts::tokenizer::TokenizerConfig>,
    schema: Option<crate::meta::Schema>,
}

/// Filesystem authority requested for one store handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessMode {
    /// Own the store's one kernel-enforced writer slot.
    ReadWrite,
    /// Load only the last committed snapshot without filesystem mutation.
    ReadOnly,
}

impl OpenOptions {
    /// Returns the default read-write, derived-durability configuration.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            access_mode: AccessMode::ReadWrite,
            durability_mode: DurabilityMode::Derived,
            commit_tier: CommitTier::Ordered,
            reader_drain_timeout: DEFAULT_READER_DRAIN_TIMEOUT,
            max_resident_bytes: u64::MAX,
            max_temp_bytes: u64::MAX,
            epoch: None,
            tokenizer: None,
            schema: None,
        }
    }

    /// Returns a pure-read configuration that takes no writer lock.
    #[must_use]
    pub const fn read_only() -> Self {
        Self {
            access_mode: AccessMode::ReadOnly,
            durability_mode: DurabilityMode::Derived,
            commit_tier: CommitTier::Ordered,
            reader_drain_timeout: DEFAULT_READER_DRAIN_TIMEOUT,
            max_resident_bytes: u64::MAX,
            max_temp_bytes: u64::MAX,
            epoch: None,
            tokenizer: None,
            schema: None,
        }
    }

    /// Selects the existing durability-policy mode and per-commit tier.
    #[must_use]
    pub const fn with_durability(mut self, mode: DurabilityMode, tier: CommitTier) -> Self {
        self.durability_mode = mode;
        self.commit_tier = tier;
        self
    }

    /// Sets the grace period close gives admitted readers before cancellation.
    #[must_use]
    pub const fn with_reader_drain_timeout(mut self, timeout: Duration) -> Self {
        self.reader_drain_timeout = timeout;
        self
    }

    /// Sets the exact ceiling for live engine-owned anonymous bytes.
    #[must_use]
    pub const fn with_max_resident_bytes(mut self, bytes: u64) -> Self {
        self.max_resident_bytes = bytes;
        self
    }

    /// Declares the typed user-column schema when creating a store.
    ///
    /// On reopen, an optional declaration must exactly match the persisted schema.
    #[must_use]
    pub fn with_schema(mut self, schema: crate::meta::Schema) -> Self {
        self.schema = Some(schema);
        self
    }

    /// Sets the exact ceiling for live temporary anonymous bytes.
    #[must_use]
    pub const fn with_max_temp_bytes(mut self, bytes: u64) -> Self {
        self.max_temp_bytes = bytes;
        self
    }

    /// Declares the embedding and tokenizer identity used by this handle.
    ///
    /// The tokenizer configuration defaults to
    /// [`crate::fts::tokenizer::TokenizerConfig::text_default`].
    /// A non-default tokenizer epoch must be paired with [`Self::with_tokenizer`].
    #[must_use]
    pub fn with_epoch(mut self, epoch: crate::epoch::StoreEpoch) -> Self {
        self.epoch = Some(epoch);
        self
    }

    /// Selects the tokenizer configuration used for indexing and queries.
    ///
    /// Open rejects this configuration when its epoch differs from the
    /// declared or persisted store identity.
    #[must_use]
    pub fn with_tokenizer(mut self, tokenizer: crate::fts::tokenizer::TokenizerConfig) -> Self {
        self.tokenizer = Some(tokenizer);
        self
    }
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Traversal controls carried only when a caller explicitly selects the graph tier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphSearchOptions {
    profile: crate::graph::search::GraphSearchProfile,
    ef: Option<usize>,
    seed: u64,
    #[cfg(any(test, feature = "test-support"))]
    cancel_after_hops: Option<usize>,
}

impl GraphSearchOptions {
    /// Creates an adaptive-width graph request for one dataset-shape profile.
    #[must_use]
    pub const fn new(profile: crate::graph::search::GraphSearchProfile) -> Self {
        Self {
            profile,
            ef: None,
            seed: 0,
            #[cfg(any(test, feature = "test-support"))]
            cancel_after_hops: None,
        }
    }

    /// Overrides the profile's adaptive traversal width.
    #[must_use]
    pub const fn with_ef(mut self, ef: usize) -> Self {
        self.ef = Some(ef);
        self
    }

    /// Selects the deterministic Bit4 query-preparation seed.
    #[must_use]
    pub const fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// Arms deterministic in-traversal cancellation for integration tests.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub const fn with_cancel_after_hops(mut self, hops: usize) -> Self {
        self.cancel_after_hops = Some(hops);
        self
    }

    pub(crate) const fn profile(self) -> crate::graph::search::GraphSearchProfile {
        self.profile
    }

    pub(crate) const fn ef(self) -> Option<usize> {
        self.ef
    }

    pub(crate) const fn seed(self) -> u64 {
        self.seed
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) const fn cancel_after_hops(self) -> Option<usize> {
        self.cancel_after_hops
    }
}

impl Default for GraphSearchOptions {
    fn default() -> Self {
        Self::new(crate::graph::search::GraphSearchProfile::SiftClass)
    }
}

pub(crate) fn auto_graph_search_options(
    snapshot: &PublishedSnapshot,
) -> Result<GraphSearchOptions, QueryError> {
    snapshot
        .graph_profile()
        .map(|profile| GraphSearchOptions::new(profile.search_profile()))
        .map_err(crate::graph::search::GraphSearchError::Profile)
        .map_err(QueryError::Graph)
}

/// Per-query execution tier at the public store seam.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SearchTier {
    /// Select graph traversal per sealed segment when that graph is published.
    #[default]
    Auto,
    /// Exhaustively score full-precision vectors.
    Exact,
    /// Preserve the existing exhaustive sealed-segment scan.
    Scan,
    /// Traverse every sealed segment's Vamana graph.
    Graph(GraphSearchOptions),
}

/// Store-search controls, separating scan worker budget from tier selection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SearchOptions {
    scan: crate::scan::ScanOptions,
    tier: Option<SearchTier>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GraphBoundMode {
    Shared,
    #[cfg(test)]
    Independent,
}

impl SearchOptions {
    /// Creates options with no tier preference and the supplied scan worker budget.
    #[must_use]
    pub const fn new(scan: crate::scan::ScanOptions) -> Self {
        Self { scan, tier: None }
    }

    /// Explicitly selects one store-search tier.
    #[must_use]
    pub const fn with_tier(mut self, tier: SearchTier) -> Self {
        self.tier = Some(tier);
        self
    }

    /// Returns the scan worker controls used by scan-tier work.
    #[must_use]
    pub const fn scan(self) -> crate::scan::ScanOptions {
        self.scan
    }

    /// Returns the effective execution tier, using automatic selection when
    /// the caller expressed no preference.
    #[must_use]
    pub const fn tier(self) -> SearchTier {
        match self.tier {
            Some(tier) => tier,
            None => SearchTier::Auto,
        }
    }

    pub(crate) const fn explicit_tier(self) -> Option<SearchTier> {
        self.tier
    }
}

impl From<crate::scan::ScanOptions> for SearchOptions {
    fn from(scan: crate::scan::ScanOptions) -> Self {
        Self::new(scan)
    }
}

/// An open, lifecycle, or close operation was rejected.
#[derive(Debug)]
pub enum StoreError {
    /// A filesystem operation failed for the named store path.
    Io {
        /// Store path being opened.
        path: PathBuf,
        /// Underlying filesystem failure.
        source: std::io::Error,
    },
    /// The supplied path exists but is not a directory.
    NotDirectory {
        /// Invalid store path.
        path: PathBuf,
    },
    /// Another writer owns the store's process or kernel lock.
    StoreBusy {
        /// Busy store directory.
        path: PathBuf,
    },
    /// The writer lock could not be opened or acquired.
    Lock(StoreLockError),
    /// The selected durability mode/tier is unsupported.
    Durability(DurabilityPolicyError),
    /// Runtime kernel selection or an explicit override was invalid.
    Kernel(crate::kernels::KernelInitError),
    /// The committed manifest could not be loaded or validated.
    Manifest(crate::manifest::ManifestError),
    /// The selected tokenizer configuration could not be compiled.
    Tokenizer(crate::fts::tokenizer::TokenizerError),
    /// The caller's declared interpretation differs from the persisted one.
    EpochMismatch(crate::epoch::EpochMismatch),
    /// Persisted bytes name an epoch but the caller did not declare one.
    EpochUndeclared,
    /// A pre-epoch manifest cannot be retroactively assigned an identity.
    EpochUnstamped,
    /// A schema declaration on reopen differed from the committed schema.
    SchemaMismatch {
        /// Schema already committed in the manifest.
        persisted: crate::meta::Schema,
        /// Schema supplied by the opening caller.
        declared: crate::meta::Schema,
    },
    /// A referenced immutable segment could not be mapped or validated.
    Segment(crate::segment::SegmentError),
    /// The caller selected graph traversal for a sealed segment without a graph.
    GraphUnavailable {
        /// Immutable segment that cannot satisfy the explicit graph-tier request.
        segment_id: crate::segment::SegmentId,
    },
    /// The checked durable WAL prefix could not be opened.
    Wal(crate::wal::WalReadError),
    /// A non-empty WAL did not replay through a clean end boundary.
    WalRecovery(crate::wal::WalRecoveryError),
    /// A replayed record's retained encoded bytes could not be decoded.
    WalRecord {
        /// Sequence assigned to the invalid retained record.
        seq: crate::wal::LogSeq,
        /// Checked record-access failure.
        source: crate::wal::VisibleRecordError,
    },
    /// A checksummed WAL mutation payload violated its versioned contract.
    WalMutation {
        /// Sequence assigned to the invalid mutation.
        seq: crate::wal::LogSeq,
        /// Persisted operation identifier.
        op: u16,
        /// Typed mutation-payload failure.
        source: crate::ingest::wal_payload::PayloadError,
    },
    /// A recovered upsert would make one document's revision non-monotonic.
    WalRevisionOrder {
        /// Sequence assigned to the invalid upsert.
        seq: crate::wal::LogSeq,
        /// Document whose recovered history is invalid.
        doc_id: crate::ingest::DocId,
        /// Revision already rebuilt from the trusted prefix.
        current: crate::ingest::Revision,
        /// Revision carried by this record.
        attempted: crate::ingest::Revision,
    },
    /// A valid mutation kind has no active-segment application path yet.
    UnsupportedWalMutation {
        /// Sequence assigned to the unsupported mutation.
        seq: crate::wal::LogSeq,
        /// Persisted operation identifier.
        op: u16,
    },
    /// A recovered vector passed payload validation but failed quantization.
    WalVector {
        /// Sequence assigned to the invalid vector mutation.
        seq: crate::wal::LogSeq,
        /// Typed vector-contract failure.
        source: crate::quant::QuantError,
    },
    /// The store-owned WAL writer could not be created or advanced.
    WalWrite(crate::wal::WalWriteError),
    /// Retained WAL visibility could not be released through a committed seal.
    WalRetire(crate::wal::WalRetireError),
    /// A kernel probe needed for an exact statistics snapshot failed.
    Statistics {
        /// Counter or residency component that could not be read.
        component: &'static str,
        /// Underlying operating-system failure.
        source: std::io::Error,
    },
    /// An accounted allocation would exceed a configured ceiling.
    BudgetExceeded {
        /// Total live bytes that would be needed if the operation proceeded.
        needed: u64,
        /// Configured ceiling in bytes.
        budget: u64,
        /// Engine component requesting the allocation.
        component: &'static str,
    },
    /// The allocator rejected a checked reservation without aborting.
    AllocationFailed {
        /// Exact requested bytes.
        needed: u64,
        /// Engine component requesting the allocation.
        component: &'static str,
    },
    /// An active vector did not match the collection's established dimension.
    DimensionMismatch {
        /// Established active dimension.
        expected: usize,
        /// Supplied vector dimension.
        actual: usize,
    },
    /// The active segment exceeded its dense u32 row address space.
    ActiveRowOverflow,
    /// An explicit seal was requested without any active rows.
    EmptyActiveSegment,
    /// Caller cancellation stopped a seal before its manifest commit point.
    SealCancelled,
    /// A write-only lifecycle operation was requested from a read-only handle.
    ReadOnly,
    /// WAL replay requires sealed tombstone repair that a read-only open cannot publish.
    SealedTombstoneRecoveryRequired,
    /// A prepared segment was accounted to a different store handle.
    ForeignPreparedSegment,
    /// The current snapshot generation cannot be incremented.
    GenerationOverflow,
    /// The active generation changed while a serialized writer prepared a commit.
    ConcurrentActiveMutation {
        /// Generation snapshotted before preparing the mutation.
        expected_generation: u64,
        /// Generation found when the writer re-acquired the active state.
        actual_generation: u64,
    },
    /// Exact reclaimed-byte reporting overflowed its u64 contract.
    PartitionBytesOverflow,
    /// A durable physical-purge intent could not be resumed during open.
    PurgeRecovery {
        /// Typed purge failure rendered without discarding its actionable values.
        detail: String,
    },
    /// A new operation raced with close after admissions stopped.
    Closing,
    /// The handle has completed teardown.
    Closed,
    /// Close cancelled an admitted read after its drain grace period elapsed.
    ReadCancelled,
    /// The lifecycle background thread could not be created.
    BackgroundStart {
        /// Operating-system thread creation failure.
        source: std::io::Error,
    },
    /// The lifecycle background thread exited before its startup handshake.
    BackgroundHandshake,
    /// The lifecycle background thread panicked before close joined it.
    BackgroundThreadPanicked,
    /// A persistent query worker thread could not be created.
    QueryPoolStart {
        /// Operating-system thread creation failure.
        source: std::io::Error,
    },
    /// A persistent query worker exited before its startup handshake.
    QueryPoolHandshake,
    /// A persistent query worker panicked before close joined it.
    QueryPoolThreadPanicked,
    /// The operating system could not report a usable query-worker count.
    QueryPoolCapacity {
        /// CPU-topology failure text.
        source: String,
    },
    /// A lifecycle synchronization primitive was poisoned.
    Synchronization {
        /// Synchronization component that rejected the operation.
        component: &'static str,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(formatter, "store I/O {}: {source}", path.display())
            }
            Self::NotDirectory { path } => {
                write!(
                    formatter,
                    "store path is not a directory: {}",
                    path.display()
                )
            }
            Self::StoreBusy { path } => {
                write!(formatter, "store already has a writer: {}", path.display())
            }
            Self::Lock(error) => error.fmt(formatter),
            Self::Durability(error) => error.fmt(formatter),
            Self::Kernel(error) => error.fmt(formatter),
            Self::Manifest(error) => error.fmt(formatter),
            Self::Tokenizer(error) => write!(formatter, "store tokenizer: {error}"),
            Self::EpochMismatch(error) => error.fmt(formatter),
            Self::EpochUndeclared => {
                formatter.write_str("store epoch is persisted but the caller declared none")
            }
            Self::EpochUnstamped => formatter
                .write_str("store manifest predates epoch identity and cannot adopt a declaration"),
            Self::SchemaMismatch {
                persisted,
                declared,
            } => write!(
                formatter,
                "declared store schema {declared:?} does not match persisted schema {persisted:?}"
            ),
            Self::Segment(error) => error.fmt(formatter),
            Self::GraphUnavailable { segment_id } => {
                write!(formatter, "sealed segment {segment_id} has no graph region")
            }
            Self::Wal(error) => error.fmt(formatter),
            Self::WalRecovery(error) => error.fmt(formatter),
            Self::WalRecord { seq, source } => {
                write!(
                    formatter,
                    "WAL sequence {} retained record: {source}",
                    seq.get()
                )
            }
            Self::WalMutation { seq, op, source } => write!(
                formatter,
                "WAL sequence {} operation {op} payload: {source}",
                seq.get()
            ),
            Self::WalRevisionOrder {
                seq,
                doc_id,
                current,
                attempted,
            } => write!(
                formatter,
                "WAL sequence {} document {} revision {} does not follow {}",
                seq.get(),
                doc_id.get(),
                attempted.get(),
                current.get()
            ),
            Self::UnsupportedWalMutation { seq, op } => write!(
                formatter,
                "WAL sequence {} operation {op} has no active-segment recovery path",
                seq.get()
            ),
            Self::WalVector { seq, source } => {
                write!(formatter, "WAL sequence {} vector: {source}", seq.get())
            }
            Self::WalWrite(error) => error.fmt(formatter),
            Self::WalRetire(error) => error.fmt(formatter),
            Self::Statistics { component, source } => {
                write!(formatter, "store statistics {component}: {source}")
            }
            Self::BudgetExceeded {
                needed,
                budget,
                component,
            } => write!(
                formatter,
                "store {component} allocation needs {needed} bytes, budget is {budget} bytes"
            ),
            Self::AllocationFailed { needed, component } => write!(
                formatter,
                "store {component} allocator rejected {needed} bytes"
            ),
            Self::DimensionMismatch { expected, actual } => write!(
                formatter,
                "active vector dimension {actual} does not match {expected}"
            ),
            Self::ActiveRowOverflow => {
                formatter.write_str("active segment row or byte geometry overflow")
            }
            Self::EmptyActiveSegment => formatter.write_str("active segment is empty"),
            Self::SealCancelled => formatter.write_str("seal was cancelled before commit"),
            Self::ReadOnly => formatter.write_str("store handle is read-only"),
            Self::SealedTombstoneRecoveryRequired => formatter.write_str(
                "store requires writable sealed-tombstone recovery before reads are safe",
            ),
            Self::ForeignPreparedSegment => {
                formatter.write_str("prepared segment belongs to another store")
            }
            Self::GenerationOverflow => formatter.write_str("store snapshot generation overflow"),
            Self::ConcurrentActiveMutation {
                expected_generation,
                actual_generation,
            } => write!(
                formatter,
                "active generation changed from {expected_generation} to {actual_generation} while preparing a commit"
            ),
            Self::PartitionBytesOverflow => {
                formatter.write_str("partition reclaimed-byte count overflow")
            }
            Self::PurgeRecovery { detail } => {
                write!(formatter, "physical purge recovery failed: {detail}")
            }
            Self::Closing => formatter.write_str("store is closing"),
            Self::Closed => formatter.write_str("store is closed"),
            Self::ReadCancelled => formatter.write_str("store close cancelled the admitted read"),
            Self::BackgroundStart { source } => {
                write!(
                    formatter,
                    "store lifecycle thread could not start: {source}"
                )
            }
            Self::BackgroundHandshake => {
                formatter.write_str("store lifecycle thread startup handshake failed")
            }
            Self::BackgroundThreadPanicked => {
                formatter.write_str("store lifecycle thread panicked")
            }
            Self::QueryPoolStart { source } => {
                write!(formatter, "store query worker could not start: {source}")
            }
            Self::QueryPoolHandshake => {
                formatter.write_str("store query worker startup handshake failed")
            }
            Self::QueryPoolThreadPanicked => formatter.write_str("store query worker panicked"),
            Self::QueryPoolCapacity { source } => {
                write!(formatter, "store query worker capacity failed: {source}")
            }
            Self::Synchronization { component } => {
                write!(
                    formatter,
                    "store lifecycle synchronization poisoned: {component}"
                )
            }
        }
    }
}

/// Coarse, exhaustive classification of a [`StoreError`], stable for hosts
/// that map engine failures onto their own status codes. Every variant of
/// [`StoreError`] maps to exactly one kind; adding a variant is a compile
/// error here until it is classified.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StoreErrorKind {
    /// An operating-system or filesystem operation failed.
    Io,
    /// A caller-supplied path or schema was invalid.
    InvalidArgument,
    /// Another process or handle owns the store's writer lock.
    StoreBusy,
    /// The requested mode or operation is unsupported.
    Unsupported,
    /// Persisted data failed validation.
    Corrupt,
    /// A configured memory, disk, or work budget was exceeded.
    BudgetExceeded,
    /// A checked allocation could not be reserved.
    OutOfMemory,
    /// Vector dimensions disagreed.
    DimensionMismatch,
    /// The declared epoch differs from the persisted identity.
    EpochMismatch,
    /// The store requires an epoch declaration and none was supplied.
    EpochUndeclared,
    /// An epoch was declared for a store with no stamped identity.
    EpochUnstamped,
    /// An internal invariant failed.
    Internal,
    /// A batch or active segment was empty.
    EmptyBatch,
    /// Cooperative cancellation stopped the operation.
    Cancelled,
    /// The store is read-only.
    ReadOnly,
    /// The store is closing.
    Closing,
    /// The store is closed.
    Closed,
    /// A background or query-pool thread panicked.
    Panic,
    /// A synchronization primitive was poisoned or unavailable.
    Synchronization,
}

impl StoreError {
    /// Classifies this error; see [`StoreErrorKind`].
    #[must_use]
    pub fn kind(&self) -> StoreErrorKind {
        match self {
            Self::Io { .. }
            | Self::Lock(_)
            | Self::Statistics { .. }
            | Self::BackgroundStart { .. }
            | Self::QueryPoolStart { .. }
            | Self::WalWrite(_)
            | Self::WalRetire(_) => StoreErrorKind::Io,
            Self::NotDirectory { .. } | Self::Tokenizer(_) | Self::SchemaMismatch { .. } => {
                StoreErrorKind::InvalidArgument
            }
            Self::StoreBusy { .. } => StoreErrorKind::StoreBusy,
            Self::Durability(_)
            | Self::Kernel(_)
            | Self::GraphUnavailable { .. }
            | Self::UnsupportedWalMutation { .. } => StoreErrorKind::Unsupported,
            Self::Manifest(_)
            | Self::Segment(_)
            | Self::Wal(_)
            | Self::WalRecovery(_)
            | Self::WalRecord { .. }
            | Self::WalMutation { .. }
            | Self::WalRevisionOrder { .. }
            | Self::WalVector { .. }
            | Self::PurgeRecovery { .. } => StoreErrorKind::Corrupt,
            Self::BudgetExceeded { .. } => StoreErrorKind::BudgetExceeded,
            Self::AllocationFailed { .. } => StoreErrorKind::OutOfMemory,
            Self::DimensionMismatch { .. } => StoreErrorKind::DimensionMismatch,
            Self::EpochMismatch(_) => StoreErrorKind::EpochMismatch,
            Self::EpochUndeclared => StoreErrorKind::EpochUndeclared,
            Self::EpochUnstamped => StoreErrorKind::EpochUnstamped,
            Self::ActiveRowOverflow
            | Self::GenerationOverflow
            | Self::ConcurrentActiveMutation { .. }
            | Self::PartitionBytesOverflow
            | Self::ForeignPreparedSegment
            | Self::BackgroundHandshake
            | Self::QueryPoolHandshake
            | Self::QueryPoolCapacity { .. } => StoreErrorKind::Internal,
            Self::EmptyActiveSegment => StoreErrorKind::EmptyBatch,
            Self::SealCancelled | Self::ReadCancelled => StoreErrorKind::Cancelled,
            Self::ReadOnly | Self::SealedTombstoneRecoveryRequired => StoreErrorKind::ReadOnly,
            Self::Closing => StoreErrorKind::Closing,
            Self::Closed => StoreErrorKind::Closed,
            Self::BackgroundThreadPanicked | Self::QueryPoolThreadPanicked => StoreErrorKind::Panic,
            Self::Synchronization { .. } => StoreErrorKind::Synchronization,
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Lock(error) => Some(error),
            Self::Durability(error) => Some(error),
            Self::Kernel(error) => Some(error),
            Self::Manifest(error) => Some(error),
            Self::Tokenizer(error) => Some(error),
            Self::EpochMismatch(error) => Some(error),
            Self::Segment(error) => Some(error),
            Self::Wal(error) => Some(error),
            Self::WalRecovery(error) => Some(error),
            Self::WalRecord { source, .. } => Some(source),
            Self::WalMutation { source, .. } => Some(source),
            Self::WalVector { source, .. } => Some(source),
            Self::WalWrite(error) => Some(error),
            Self::WalRetire(error) => Some(error),
            Self::Statistics { source, .. } => Some(source),
            Self::BackgroundStart { source } => Some(source),
            Self::QueryPoolStart { source } => Some(source),
            Self::NotDirectory { .. }
            | Self::StoreBusy { .. }
            | Self::EpochUndeclared
            | Self::EpochUnstamped
            | Self::SchemaMismatch { .. }
            | Self::GraphUnavailable { .. }
            | Self::WalRevisionOrder { .. }
            | Self::UnsupportedWalMutation { .. }
            | Self::BudgetExceeded { .. }
            | Self::AllocationFailed { .. }
            | Self::DimensionMismatch { .. }
            | Self::ActiveRowOverflow
            | Self::EmptyActiveSegment
            | Self::SealCancelled
            | Self::ReadOnly
            | Self::SealedTombstoneRecoveryRequired
            | Self::ForeignPreparedSegment
            | Self::GenerationOverflow
            | Self::ConcurrentActiveMutation { .. }
            | Self::PartitionBytesOverflow
            | Self::PurgeRecovery { .. }
            | Self::Closing
            | Self::Closed
            | Self::ReadCancelled
            | Self::BackgroundHandshake
            | Self::BackgroundThreadPanicked
            | Self::QueryPoolHandshake
            | Self::QueryPoolThreadPanicked
            | Self::QueryPoolCapacity { .. }
            | Self::Synchronization { .. } => None,
        }
    }
}

/// One explicitly closeable embedded-store handle.
pub struct Store {
    pub(crate) directory: PathBuf,
    pub(crate) vfs: Arc<dyn crate::vfs::Vfs>,
    pub(crate) clock: Arc<dyn MonotonicClock>,
    pub(crate) state: Mutex<StoreState>,
    pub(crate) state_changed: Condvar,
    pub(crate) background: Mutex<Option<BackgroundThread>>,
    pub(crate) query_pool: Mutex<Option<Arc<pool::QueryPool>>>,
    pub(crate) lexical_worker: Mutex<Option<Arc<pool::LexicalWorker>>>,
    pub(crate) snapshot: RwLock<Option<Arc<PublishedSnapshot>>>,
    // Drop order is deliberate: immutable mappings, active buffers, and the
    // WAL descriptor all release before the kernel writer lock.
    pub(crate) active: Mutex<Option<crate::ingest::ActiveState>>,
    pub(crate) wal_writer: Mutex<Option<crate::ingest::StoreWal>>,
    pub(crate) writer_lock: Mutex<Option<StoreLock>>,
    pub(crate) maintenance: Mutex<()>,
    pub(crate) health_state: Mutex<crate::diag::HealthState>,
    pub(crate) durability_policy: DurabilityPolicy,
    pub(crate) reader_drain_timeout: Duration,
    pub(crate) accounting: Arc<stats::Accounting>,
    pub(crate) active_queries: AtomicU64,
    pub(crate) epoch: Option<crate::epoch::StoreEpoch>,
    pub(crate) epoch_alias: crate::epoch::EpochAliasCell,
    pub(crate) tokenizer: crate::fts::tokenizer::Analyzer,
    pub(crate) schema: crate::meta::Schema,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) ingest_retention_fault_controller:
        Option<crate::ingest::IngestRetentionFaultController>,
    #[cfg(any(test, feature = "test-support"))]
    hybrid_leg_fault: Mutex<Option<HybridLegTestFault>>,
    #[cfg(any(test, feature = "test-support"))]
    hybrid_execution_receipt: Mutex<Option<HybridExecutionReceipt>>,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) metadata_test_controller: Option<Arc<crate::planner::MetadataTestController>>,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) vector_fault_controller: Option<crate::scan::vector_fault::VectorFaultController>,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) kernel_fault_controller: Option<crate::kernels::vector_fault::KernelFaultController>,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) vector_seal_scheme: Option<crate::quant::QuantScheme>,
    #[cfg(test)]
    pub(crate) teardown_probe: Arc<close::TeardownProbe>,
}

fn acquire_writer_lock(
    path: &Path,
    access_mode: AccessMode,
) -> Result<Option<StoreLock>, StoreError> {
    Ok(match access_mode {
        AccessMode::ReadWrite => Some(StoreLock::acquire(path).map_err(|error| match error {
            StoreLockError::Io { path, source }
                if source.kind() == std::io::ErrorKind::WouldBlock =>
            {
                StoreError::StoreBusy { path }
            }
            StoreLockError::Io { path, source } => {
                StoreError::Lock(StoreLockError::Io { path, source })
            }
        })?),
        AccessMode::ReadOnly => None,
    })
}

fn resolve_open_schema(
    manifest_exists: bool,
    snapshot: &PublishedSnapshot,
    declared: Option<&crate::meta::Schema>,
) -> Result<crate::meta::Schema, StoreError> {
    if manifest_exists {
        let persisted = snapshot.schema().clone();
        if let Some(declared) = declared
            && declared != &persisted
        {
            return Err(StoreError::SchemaMismatch {
                persisted,
                declared: declared.clone(),
            });
        }
        Ok(snapshot.schema().clone())
    } else {
        Ok(declared
            .cloned()
            .unwrap_or_else(crate::meta::Schema::timestamp_only))
    }
}

fn validate_epoch_identity(
    persisted: Option<crate::epoch::EpochIdentity>,
    declared: Option<crate::epoch::EpochIdentity>,
    manifest_exists: bool,
    access_mode: AccessMode,
) -> Result<(), StoreError> {
    match (persisted, declared) {
        (Some(expected), Some(declared)) if expected != declared => {
            Err(StoreError::EpochMismatch(crate::epoch::EpochMismatch {
                expected,
                declared,
            }))
        }
        (Some(_), None) => Err(StoreError::EpochUndeclared),
        (None, Some(_)) if manifest_exists || access_mode == AccessMode::ReadOnly => {
            Err(StoreError::EpochUnstamped)
        }
        (Some(_), Some(_)) | (None, Some(_)) | (None, None) => Ok(()),
    }
}

fn validate_tokenizer_epoch(
    expected: Option<crate::epoch::EpochIdentity>,
    tokenizer: &crate::fts::tokenizer::Analyzer,
) -> Result<(), StoreError> {
    if let Some(expected) = expected
        && tokenizer.epoch() != expected.tokenizer
    {
        return Err(StoreError::EpochMismatch(crate::epoch::EpochMismatch {
            expected,
            declared: crate::epoch::EpochIdentity {
                embedding: expected.embedding,
                tokenizer: tokenizer.epoch(),
            },
        }));
    }
    Ok(())
}

fn cleanup_open_orphans(
    store: &Store,
) -> Result<crate::manifest::io::OrphanCleanupReport, StoreError> {
    let reachable_segments = store
        .snapshot
        .read()
        .map_err(|_| StoreError::Synchronization {
            component: "published snapshot",
        })?
        .as_ref()
        .map(|snapshot| {
            snapshot
                .all_segments()
                .iter()
                .map(|segment| store.directory.join(segment.meta().id.file_name()))
                .collect::<HashSet<_>>()
        })
        .ok_or(StoreError::Closed)?;
    crate::manifest::io::cleanup_store_orphans(
        store.vfs.as_ref(),
        &store.directory,
        &reachable_segments,
        store.durability_policy,
    )
    .map_err(StoreError::Manifest)
}

impl Store {
    /// Opens a store directory with the requested access and durability policy.
    pub fn open(path: impl AsRef<Path>, options: OpenOptions) -> Result<Self, StoreError> {
        Self::open_with_infrastructure(
            path,
            options,
            Arc::new(crate::vfs::StdVfs),
            Arc::new(SystemMonotonicClock),
            #[cfg(any(test, feature = "test-support"))]
            None,
            #[cfg(any(test, feature = "test-support"))]
            None,
            #[cfg(any(test, feature = "test-support"))]
            None,
            #[cfg(any(test, feature = "test-support"))]
            None,
            #[cfg(any(test, feature = "test-support"))]
            None,
            #[cfg(any(test, feature = "test-support"))]
            None,
            #[cfg(any(test, feature = "test-support"))]
            None,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "test-support controllers are explicit optional infrastructure dependencies"
    )]
    fn open_with_infrastructure(
        path: impl AsRef<Path>,
        options: OpenOptions,
        vfs: Arc<dyn crate::vfs::Vfs>,
        clock: Arc<dyn MonotonicClock>,
        #[cfg(any(test, feature = "test-support"))] hybrid_leg_fault: Option<HybridLegTestFault>,
        #[cfg(any(test, feature = "test-support"))] storage_fault_controller: Option<
            StorageFaultController,
        >,
        #[cfg(any(test, feature = "test-support"))] ingest_retention_fault_controller: Option<
            crate::ingest::IngestRetentionFaultController,
        >,
        #[cfg(any(test, feature = "test-support"))] metadata_test_controller: Option<
            Arc<crate::planner::MetadataTestController>,
        >,
        #[cfg(any(test, feature = "test-support"))] vector_fault_controller: Option<
            crate::scan::vector_fault::VectorFaultController,
        >,
        #[cfg(any(test, feature = "test-support"))] kernel_fault_controller: Option<
            crate::kernels::vector_fault::KernelFaultController,
        >,
        #[cfg(any(test, feature = "test-support"))] vector_seal_scheme: Option<
            crate::quant::QuantScheme,
        >,
    ) -> Result<Self, StoreError> {
        let path = path.as_ref();
        #[cfg(any(test, feature = "test-support"))]
        match kernel_fault_controller.as_ref() {
            Some(controller) => {
                controller
                    .initialize_for_store()
                    .map_err(StoreError::Kernel)?;
            }
            None => {
                crate::kernels::initialize().map_err(StoreError::Kernel)?;
            }
        }
        #[cfg(not(any(test, feature = "test-support")))]
        crate::kernels::initialize().map_err(StoreError::Kernel)?;
        #[cfg(any(test, feature = "test-support"))]
        let vfs = match storage_fault_controller.as_ref() {
            Some(controller) => Arc::new(StorageFaultVfs {
                inner: vfs,
                controller: controller.clone(),
            }) as Arc<dyn crate::vfs::Vfs>,
            None => vfs,
        };
        let durability_policy = DurabilityPolicy::new(options.durability_mode, options.commit_tier)
            .map_err(StoreError::Durability)?;
        let accounting = Arc::new(stats::Accounting::new(
            options.max_resident_bytes,
            options.max_temp_bytes,
        ));
        let is_directory = vfs
            .ensure_directory(path, options.access_mode == AccessMode::ReadWrite)
            .map_err(|source| StoreError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        if !is_directory {
            return Err(StoreError::NotDirectory {
                path: path.to_path_buf(),
            });
        }
        let writer_lock = acquire_writer_lock(path, options.access_mode)?;
        let manifest_path = path.join(crate::manifest::io::MANIFEST_FILE);
        let manifest_exists = match vfs.open(&manifest_path) {
            Ok(_) => true,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => false,
            Err(source) => {
                return Err(StoreError::Io {
                    path: manifest_path,
                    source,
                });
            }
        };
        // A persisted store gets one bounded header probe through the
        // Store-owned VFS before mmap becomes the query data plane. Internal
        // manifest-only remaps deliberately skip this probe so their exact
        // zero-segment-read accounting contracts remain intact.
        let snapshot = PublishedSnapshot::load_for_open_on_vfs(path, &accounting, vfs.as_ref())?;
        let schema = resolve_open_schema(manifest_exists, &snapshot, options.schema.as_ref())?;
        let persisted_epoch = snapshot.epoch_alias();
        let declared_epoch = options
            .epoch
            .as_ref()
            .map(crate::epoch::StoreEpoch::identity);
        validate_epoch_identity(
            persisted_epoch,
            declared_epoch,
            manifest_exists,
            options.access_mode,
        )?;
        let tokenizer = crate::fts::tokenizer::Analyzer::new(
            options
                .tokenizer
                .clone()
                .unwrap_or_else(crate::fts::tokenizer::TokenizerConfig::text_default),
        )
        .map_err(StoreError::Tokenizer)?;
        validate_tokenizer_epoch(persisted_epoch.or(declared_epoch), &tokenizer)?;
        let absorbed_through = snapshot.absorbed_through();
        let wal_path = path.join("wal.ze");
        let (active, recovered_wal, sealed_tombstones) = crate::ingest::ActiveState::recover(
            vfs.as_ref(),
            &wal_path,
            snapshot.generation(),
            absorbed_through,
            &accounting,
            &schema,
            &tokenizer,
        )?;
        if options.access_mode == AccessMode::ReadWrite && manifest_exists {
            // An adopted manifest may be the survivor of a commit interrupted
            // between rename and directory sync. Make its dirent durable before
            // any new generation is acknowledged on top of it.
            match durability_policy.directory_sync() {
                SyncRequirement::Skip => {}
                SyncRequirement::Sync(kind) => {
                    vfs.sync(path, kind).map_err(|source| StoreError::Io {
                        path: path.to_path_buf(),
                        source,
                    })?;
                }
            }
        }
        if options.access_mode == AccessMode::ReadWrite
            && !manifest_exists
            && (options.epoch.is_some() || options.schema.is_some())
        {
            crate::manifest::io::commit_manifest(
                vfs.as_ref(),
                path,
                &crate::manifest::Manifest {
                    generation: active.generation,
                    log_seq: 0,
                    segments: Vec::new(),
                    epochs: options
                        .epoch
                        .as_ref()
                        .cloned()
                        .map(crate::manifest::EpochMeta::from)
                        .into_iter()
                        .collect(),
                    epoch_alias: options
                        .epoch
                        .as_ref()
                        .map(crate::epoch::StoreEpoch::identity),
                    schema: schema.clone(),
                },
                durability_policy,
            )
            .map_err(StoreError::Manifest)?;
        }
        let wal_writer = match options.access_mode {
            AccessMode::ReadWrite => Some(match recovered_wal {
                Some(recovered) => crate::ingest::StoreWal::resume(
                    vfs.as_ref(),
                    &wal_path,
                    recovered,
                    durability_policy,
                    absorbed_through,
                    &accounting,
                )?,
                None => crate::ingest::StoreWal::create(
                    Arc::clone(&vfs),
                    path,
                    &wal_path,
                    durability_policy,
                    &accounting,
                )?,
            }),
            AccessMode::ReadOnly => {
                drop(recovered_wal);
                None
            }
        };
        let background = match options.access_mode {
            AccessMode::ReadWrite => Some(BackgroundThread::start()?),
            AccessMode::ReadOnly => None,
        };
        #[cfg(test)]
        let (snapshot, background, teardown_probe) = {
            let mut snapshot = snapshot;
            let mut background = background;
            let teardown_probe = Arc::new(close::TeardownProbe::new());
            snapshot.set_teardown_probe(Arc::clone(&teardown_probe));
            if let Some(background) = background.as_mut() {
                background.set_teardown_probe(Arc::clone(&teardown_probe));
            }
            (snapshot, background, teardown_probe)
        };
        let store = Self {
            directory: path.to_path_buf(),
            vfs,
            clock,
            state: Mutex::new(StoreState::Open),
            state_changed: Condvar::new(),
            background: Mutex::new(background),
            query_pool: Mutex::new(None),
            lexical_worker: Mutex::new(None),
            snapshot: RwLock::new(None),
            active: Mutex::new(Some(active)),
            wal_writer: Mutex::new(wal_writer),
            writer_lock: Mutex::new(writer_lock),
            maintenance: Mutex::new(()),
            health_state: Mutex::new(crate::diag::HealthState::default()),
            durability_policy,
            reader_drain_timeout: options.reader_drain_timeout,
            accounting,
            active_queries: AtomicU64::new(0),
            epoch_alias: crate::epoch::EpochAliasCell::new(persisted_epoch.or(declared_epoch)),
            epoch: options.epoch,
            tokenizer,
            schema,
            #[cfg(any(test, feature = "test-support"))]
            ingest_retention_fault_controller,
            #[cfg(any(test, feature = "test-support"))]
            hybrid_leg_fault: Mutex::new(hybrid_leg_fault),
            #[cfg(any(test, feature = "test-support"))]
            hybrid_execution_receipt: Mutex::new(None),
            #[cfg(any(test, feature = "test-support"))]
            metadata_test_controller,
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_controller,
            #[cfg(any(test, feature = "test-support"))]
            kernel_fault_controller,
            #[cfg(any(test, feature = "test-support"))]
            vector_seal_scheme,
            #[cfg(test)]
            teardown_probe,
        };
        store.publish_snapshot(snapshot)?;
        store
            .recover_pending_physical_purge()
            .map_err(|error| StoreError::PurgeRecovery {
                detail: error.to_string(),
            })?;
        store.recover_sealed_tombstones(&sealed_tombstones)?;
        if options.access_mode == AccessMode::ReadWrite {
            let cleanup_report = cleanup_open_orphans(&store)?;
            #[cfg(any(test, feature = "test-support"))]
            if let Some(controller) = storage_fault_controller.as_ref() {
                controller.record_cleanup_report(cleanup_report);
            }
            #[cfg(not(any(test, feature = "test-support")))]
            let _ = cleanup_report;
        }
        #[cfg(any(test, feature = "test-support"))]
        if let Some(controller) = storage_fault_controller.as_ref() {
            let published = store
                .snapshot
                .read()
                .map_err(|_| StoreError::Synchronization {
                    component: "published snapshot",
                })?;
            let snapshot = published.as_ref().ok_or(StoreError::Closed)?;
            for segment in snapshot.all_segments() {
                register_storage_segment_controller(
                    &store.directory,
                    segment.meta().id,
                    controller,
                );
            }
        }
        Ok(store)
    }

    /// Opens a store with deterministic infrastructure for adversarial tests.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn open_with_test_dependencies(
        path: impl AsRef<Path>,
        options: OpenOptions,
        dependencies: StoreTestDependencies,
    ) -> Result<Self, StoreError> {
        let controller = dependencies.storage_fault_controller.clone();
        with_storage_open_controller(controller.as_ref(), || {
            Self::open_with_infrastructure(
                path,
                options,
                dependencies.vfs,
                dependencies.clock,
                dependencies.hybrid_leg_fault,
                dependencies.storage_fault_controller,
                dependencies.ingest_retention_fault_controller,
                dependencies.metadata_test_controller,
                dependencies.vector_fault_controller,
                dependencies.kernel_fault_controller,
                dependencies.vector_seal_scheme,
            )
        })
    }

    /// Takes the most recent Store-owned hybrid execution receipt.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn take_hybrid_execution_receipt(&self) -> Option<HybridExecutionReceipt> {
        self.hybrid_execution_receipt
            .lock()
            .ok()
            .and_then(|mut receipt| receipt.take())
    }

    /// Carries the committed epoch registry forward unchanged.
    ///
    /// A declared epoch is stamped once, during open, before any write is
    /// admitted, so every later commit only propagates what is already
    /// committed. Replacement-snapshot paths may not have a prior manifest
    /// value in hand, so a declared store reconstructs its already-committed
    /// singleton entry; an unstamped store still returns an empty registry.
    pub(crate) fn epoch_registry(
        &self,
        prior: &[crate::manifest::EpochMeta],
    ) -> Vec<crate::manifest::EpochMeta> {
        if prior.is_empty() {
            self.epoch
                .as_ref()
                .map(crate::manifest::EpochMeta::from)
                .into_iter()
                .collect()
        } else {
            prior.to_vec()
        }
    }

    /// Returns the published embedding and tokenizer identity, or `None`
    /// for a store that carries no stamped epoch.
    #[must_use]
    pub fn epoch_identity(&self) -> Option<crate::epoch::EpochIdentity> {
        self.epoch_alias.load()
    }

    /// Returns the current explicit lifecycle state.
    pub fn state(&self) -> Result<StoreState, StoreError> {
        self.state
            .lock()
            .map(|state| *state)
            .map_err(|_| StoreError::Synchronization { component: "state" })
    }

    /// Acquires the complete immutable snapshot admitted for one read.
    pub fn snapshot(&self) -> Result<SnapshotLease, StoreError> {
        let state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(StoreError::Closing),
            StoreState::Closed => return Err(StoreError::Closed),
        }
        let active = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        let generation = active.as_ref().ok_or(StoreError::Closed)?.generation;
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?
            .as_ref()
            .cloned()
            .ok_or(StoreError::Closed)?;
        drop(active);
        drop(state);
        Ok(SnapshotLease::new_at(snapshot, generation))
    }

    /// Reads one public result row's typed metadata through the owning Store boundary.
    ///
    /// This hidden test-support adapter lets independent persistence tests pair
    /// row identities returned by a public query with the values the reopened
    /// Store actually exposes. It is not compiled into the shipping API.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn test_metadata_row_values(
        &self,
        row_id: crate::ingest::GlobalRowId,
    ) -> Result<Vec<(crate::meta::ColumnId, Option<crate::meta::PredicateValue>)>, StoreError> {
        match row_id.source() {
            crate::ingest::RowSource::Active => {
                let row = usize::try_from(row_id.local_row())
                    .map_err(|_| StoreError::ActiveRowOverflow)?;
                let active = self
                    .active
                    .lock()
                    .map_err(|_| StoreError::Synchronization {
                        component: "active segment",
                    })?;
                let segment = &active.as_ref().ok_or(StoreError::Closed)?.segment;
                let timestamp = segment
                    .timestamps()
                    .get(row)
                    .copied()
                    .ok_or(StoreError::ActiveRowOverflow)?;
                let mut values = segment.column_values(row)?;
                values.push((
                    crate::meta::TIMESTAMP_COLUMN,
                    crate::meta::PredicateValue::I64(timestamp),
                ));
                self.schema
                    .columns()
                    .iter()
                    .map(|definition| {
                        let value = values
                            .iter()
                            .find(|(column, _)| *column == definition.id())
                            .map(|(_, value)| value.clone());
                        Ok((definition.id(), value))
                    })
                    .collect()
            }
            crate::ingest::RowSource::Sealed(segment_id) => {
                let snapshot = self.snapshot()?;
                let segment = snapshot
                    .segments()
                    .iter()
                    .find(|segment| segment.meta().id == segment_id)
                    .ok_or_else(|| {
                        StoreError::Segment(crate::segment::SegmentError::Geometry(format!(
                            "public metadata row names absent segment {segment_id}"
                        )))
                    })?;
                let columns = segment.columns().map_err(StoreError::Segment)?;
                if row_id.local_row() >= columns.row_count() {
                    return Err(StoreError::Segment(crate::segment::SegmentError::Geometry(
                        format!(
                            "public metadata row {} is outside {} rows",
                            row_id.local_row(),
                            columns.row_count()
                        ),
                    )));
                }
                columns
                    .schema()
                    .columns()
                    .iter()
                    .map(|definition| {
                        let column = columns.column(definition.id()).ok_or_else(|| {
                            StoreError::Segment(crate::segment::SegmentError::Geometry(format!(
                                "public metadata row is missing column {}",
                                definition.id().get()
                            )))
                        })?;
                        let value = match column {
                            crate::meta::Column::U64(column) => column
                                .get(row_id.local_row())
                                .map(crate::meta::PredicateValue::U64),
                            crate::meta::Column::I64(column) => column
                                .get(row_id.local_row())
                                .map(crate::meta::PredicateValue::I64),
                            crate::meta::Column::F64(column) => column
                                .get(row_id.local_row())
                                .map(crate::meta::PredicateValue::F64),
                            crate::meta::Column::Bool(column) => column
                                .get(row_id.local_row())
                                .map(crate::meta::PredicateValue::Bool),
                            crate::meta::Column::DictionaryString(column) => column
                                .get(row_id.local_row())
                                .map(|value| crate::meta::PredicateValue::String(value.to_owned())),
                            crate::meta::Column::RawString(column) => column
                                .get(row_id.local_row())
                                .map(|value| crate::meta::PredicateValue::String(value.to_owned())),
                        };
                        Ok((definition.id(), value))
                    })
                    .collect()
            }
        }
    }

    /// Evaluates one source's actual typed metadata state for independent tests.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn test_metadata_evaluate_source(
        &self,
        source: crate::ingest::RowSource,
        predicate: &crate::meta::Predicate,
    ) -> Result<(u32, Vec<u32>, Vec<u32>), StoreError> {
        let evaluate = |columns: &crate::meta::ColumnStore,
                        alive: &crate::meta::AliveSet|
         -> Result<(u32, Vec<u32>, Vec<u32>), StoreError> {
            let matches = crate::meta::evaluate(predicate, columns, alive).map_err(|error| {
                StoreError::Segment(crate::segment::SegmentError::Columns(format!(
                    "test metadata source evaluation: {error}"
                )))
            })?;
            Ok((
                columns.row_count(),
                alive.iter_alive().collect(),
                matches.iter().collect(),
            ))
        };
        match source {
            crate::ingest::RowSource::Active => {
                let active = self
                    .active
                    .lock()
                    .map_err(|_| StoreError::Synchronization {
                        component: "active segment",
                    })?;
                let segment = &active.as_ref().ok_or(StoreError::Closed)?.segment;
                let mut builder = crate::meta::ColumnStoreBuilder::new(self.schema.clone());
                for (row, timestamp) in segment.timestamps().iter().copied().enumerate() {
                    let values = segment.column_values(row)?;
                    builder
                        .push_row(timestamp, &crate::ingest::column_inputs(&values))
                        .map_err(|error| {
                            StoreError::Segment(crate::segment::SegmentError::Columns(
                                error.to_string(),
                            ))
                        })?;
                }
                let columns = builder.finish().map_err(|error| {
                    StoreError::Segment(crate::segment::SegmentError::Columns(error.to_string()))
                })?;
                let alive = segment.alive()?;
                evaluate(&columns, &alive)
            }
            crate::ingest::RowSource::Sealed(segment_id) => {
                let snapshot = self.snapshot()?;
                let segment = snapshot
                    .segments()
                    .iter()
                    .find(|segment| segment.meta().id == segment_id)
                    .ok_or_else(|| {
                        StoreError::Segment(crate::segment::SegmentError::Geometry(format!(
                            "metadata source evaluation names absent segment {segment_id}"
                        )))
                    })?;
                let columns = segment.columns().map_err(StoreError::Segment)?;
                let alive = segment.alive().map_err(StoreError::Segment)?;
                evaluate(&columns, &alive)
            }
        }
    }

    pub(crate) fn publish_snapshot(&self, snapshot: PublishedSnapshot) -> Result<(), StoreError> {
        let state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(StoreError::Closing),
            StoreState::Closed => return Err(StoreError::Closed),
        }
        let generation = snapshot.generation();
        let mut active = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        let active_generation = &mut active.as_mut().ok_or(StoreError::Closed)?.generation;
        *active_generation = (*active_generation).max(generation);
        let mut published = self
            .snapshot
            .write()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?;
        *published = Some(Arc::new(snapshot));
        drop(active);
        drop(state);
        Ok(())
    }

    /// Runs one exact parallel scan admitted against this store's snapshot.
    ///
    /// The query holds one snapshot lease for its complete execution and uses
    /// the store's lazily started persistent worker pool.
    pub fn top_k_with_options(
        &self,
        request: crate::scan::ScanRequest<'_>,
        k: usize,
        options: crate::scan::ScanOptions,
        control: QueryControl,
    ) -> Result<crate::scan::ScanOutcome, QueryError> {
        let control = control.with_clock(Arc::clone(&self.clock));
        let state = self
            .state
            .lock()
            .map_err(|_| QueryError::Store(StoreError::Synchronization { component: "state" }))?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(QueryError::Store(StoreError::Closing)),
            StoreState::Closed => return Err(QueryError::Store(StoreError::Closed)),
        }
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| {
                QueryError::Store(StoreError::Synchronization {
                    component: "published snapshot",
                })
            })?
            .as_ref()
            .cloned()
            .ok_or(QueryError::Store(StoreError::Closed))?;
        let mut pool_slot = self.query_pool.lock().map_err(|_| {
            QueryError::Store(StoreError::Synchronization {
                component: "query pool",
            })
        })?;
        if pool_slot.is_none() {
            let capacity = crate::scan::physical_thread_capacity().map_err(|error| {
                QueryError::Store(StoreError::QueryPoolCapacity {
                    source: error.to_string(),
                })
            })?;
            let pool =
                pool::QueryPool::start(capacity, &self.accounting).map_err(QueryError::Store)?;
            *pool_slot = Some(Arc::new(pool));
        }
        let pool = pool_slot.as_ref().cloned().ok_or({
            QueryError::Store(StoreError::Synchronization {
                component: "query pool initialization",
            })
        })?;
        self.active_queries.fetch_add(1, Ordering::Relaxed);
        let active = ActiveQuery {
            count: &self.active_queries,
        };
        drop(pool_slot);
        drop(state);

        let result = pool
            .execute(request, k, options, control, SnapshotLease::new(snapshot))
            .map(|mut outcome| {
                outcome.candidates.truncate(k);
                outcome
            });
        drop(active);
        result
    }

    /// Injects one persistent query-worker panic and joins the pool.
    ///
    /// This is test-support-only and exists so the lifecycle campaign reaches
    /// the real worker teardown/error path.
    #[cfg(any(test, feature = "test-support"))]
    pub fn panic_query_worker_for_test(&self) -> Result<(), StoreError> {
        let pool = self
            .query_pool
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "query pool",
            })?
            .as_ref()
            .cloned()
            .ok_or(StoreError::Synchronization {
                component: "query pool initialization",
            })?;
        pool.panic_one_and_join()
    }

    /// Searches the active segment plus every immutable segment in one pinned
    /// store generation, then merges one global top-k.
    ///
    /// Scan-tier segments use the persistent query pool. Explicit graph-tier
    /// segments traverse serially on the calling thread, while the active
    /// segment is still scanned. Both tiers share one cancellation state and
    /// `(RowSource, local_row)` addressing, so immutable row zero never collides
    /// across segments and manifest reordering cannot rename a sealed row.
    pub fn search(
        &self,
        request: crate::ingest::SearchRequest<'_>,
        k: usize,
        options: impl Into<SearchOptions>,
        control: QueryControl,
    ) -> Result<crate::ingest::SearchOutcome, QueryError> {
        self.search_with_graph_bound_mode(
            request,
            k,
            options.into(),
            control,
            GraphBoundMode::Shared,
        )
    }

    fn admit_lexical_query(&self) -> Result<AdmittedLexicalQuery<'_>, QueryError> {
        let state = self
            .state
            .lock()
            .map_err(|_| QueryError::Store(StoreError::Synchronization { component: "state" }))?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(QueryError::Store(StoreError::Closing)),
            StoreState::Closed => return Err(QueryError::Store(StoreError::Closed)),
        }
        let active_guard = self.active.lock().map_err(|_| {
            QueryError::Store(StoreError::Synchronization {
                component: "active segment",
            })
        })?;
        let active_state = active_guard
            .as_ref()
            .ok_or(QueryError::Store(StoreError::Closed))?;
        let generation = active_state.generation;
        let active = Arc::clone(&active_state.segment);
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| {
                QueryError::Store(StoreError::Synchronization {
                    component: "published snapshot",
                })
            })?
            .as_ref()
            .cloned()
            .ok_or(QueryError::Store(StoreError::Closed))?;
        self.active_queries.fetch_add(1, Ordering::Relaxed);
        let active_query = ActiveQuery {
            count: &self.active_queries,
        };
        drop(active_guard);
        drop(state);
        Ok(AdmittedLexicalQuery {
            generation,
            active,
            snapshot,
            active_query,
        })
    }

    /// Runs an exact structured lexical query over active and sealed text.
    pub fn search_lexical(
        &self,
        query: &crate::fts::search::TermQuery,
        k: usize,
        control: QueryControl,
    ) -> Result<crate::ingest::StoreLexicalSearchOutcome, crate::ingest::StoreLexicalError> {
        let control = control.with_clock(Arc::clone(&self.clock));
        let started = std::time::Instant::now();
        let AdmittedLexicalQuery {
            generation,
            active,
            snapshot,
            active_query,
        } = self.admit_lexical_query()?;

        let lease = SnapshotLease::new_at(Arc::clone(&snapshot), generation);
        let cancellation = QueryCancellation::new(&control, &lease);
        cancellation.check_graph().map_err(QueryError::Scan)?;
        let LexicalAssembly {
            index,
            alive_sets,
            sources,
        } = assemble_lexical_index(&snapshot, &active, &self.accounting, true, None)
            .map_err(map_store_lexical_assembly_error)?;
        let allow_lists = alive_sets
            .iter()
            .map(|alive| alive.alive_bitmap())
            .collect::<Vec<_>>();
        let lexical = crate::planner::search_lexical_filtered_refs(
            &index,
            query,
            k,
            crate::fts::bm25::Bm25Params::beir(),
            &allow_lists,
            None,
        )?;
        cancellation.check_graph().map_err(QueryError::Scan)?;
        let mut candidates = Vec::with_capacity(lexical.result.hits.len());
        for hit in &lexical.result.hits {
            let document = structured_lexical_document(&snapshot, &active, &sources, hit.doc, true)
                .map_err(map_store_lexical_document_error)?
                .ok_or(QueryError::Store(StoreError::ActiveRowOverflow))?;
            candidates.push(crate::ingest::LexicalCandidate {
                document,
                score: hit.score,
            });
        }
        let diagnostics = crate::diag::QueryDiagnostics::lexical(crate::diag::LexicalDiagnostics {
            snapshot_generation: generation,
            indexed_through_seq: active
                .indexed_through_seq()
                .max(crate::wal::LogSeq::new(snapshot.absorbed_through())),
            requested_k: k,
            returned: candidates.len(),
            counters: lexical.result.counters,
            elapsed: started.elapsed(),
        });
        drop(active_query);
        Ok(crate::ingest::StoreLexicalSearchOutcome {
            candidates,
            generation,
            diagnostics,
        })
    }

    /// Returns the stored UTF-8 body for one exact document revision.
    ///
    /// Active memory and immutable region 14 are searched without adding a
    /// second text format. `None` means either that the exact revision is not
    /// present or that its text field was absent.
    pub fn stored_text(
        &self,
        version: crate::ingest::DocumentVersion,
    ) -> Result<Option<String>, crate::ingest::StoreLexicalError> {
        let AdmittedLexicalQuery {
            active,
            snapshot,
            active_query,
            ..
        } = self.admit_lexical_query()?;
        if let Some((row, active_version, _)) = active.existing(version.doc_id())
            && active_version == version
        {
            let text = active
                .text(row)
                .map_err(crate::ingest::StoreLexicalError::from)?;
            drop(active_query);
            return Ok(text.map(str::to_owned));
        }
        for segment in snapshot.segments() {
            let Some(row) = segment
                .query_row_for_document_version(version)
                .map_err(crate::ingest::StoreLexicalError::from)?
            else {
                continue;
            };
            let text = segment
                .query_stored_text()
                .map_err(StoreError::Segment)
                .map_err(crate::ingest::StoreLexicalError::from)?
                .and_then(|rows| rows.row(row).flatten())
                .map(str::to_owned);
            drop(active_query);
            return Ok(text);
        }
        drop(active_query);
        Ok(None)
    }

    /// Runs a structured lexical query and returns provenance plus snippets
    /// copied from the exact row text pinned for this generation.
    pub fn search_lexical_structured(
        &self,
        query: &crate::fts::query::LexicalQuery,
        k: usize,
        snippet_bytes: usize,
        control: QueryControl,
    ) -> Result<crate::ingest::StoreStructuredLexicalSearchOutcome, crate::ingest::StoreLexicalError>
    {
        let control = control.with_clock(Arc::clone(&self.clock));
        let started = std::time::Instant::now();
        let AdmittedLexicalQuery {
            generation,
            active,
            snapshot,
            active_query,
        } = self.admit_lexical_query()?;

        let lease = SnapshotLease::new_at(Arc::clone(&snapshot), generation);
        let cancellation = QueryCancellation::new(&control, &lease);
        cancellation.check_graph().map_err(QueryError::Scan)?;
        let LexicalAssembly {
            index,
            alive_sets,
            sources,
        } = assemble_lexical_index(
            &snapshot,
            &active,
            &self.accounting,
            true,
            Some(&cancellation),
        )
        .map_err(map_store_lexical_assembly_error)?;
        let vocabulary = crate::fts::query::vocabulary(query, index.terms());
        let expansions = crate::fts::query::expand(query, &vocabulary)?;
        let allow_lists = alive_sets
            .iter()
            .map(|alive| alive.alive_bitmap())
            .collect::<Vec<_>>();
        let mut aggregate = BTreeMap::<
            crate::fts::search::GlobalDocId,
            (f64, Vec<crate::fts::query::LexicalExpansion>),
        >::new();
        let mut counters = crate::fts::search::SearchCounters::default();
        let fields = query.fields();
        let all_rows = usize::try_from(index.document_count()).unwrap_or(usize::MAX);
        for expansion in &expansions {
            cancellation.check_graph().map_err(QueryError::Scan)?;
            let term_query = crate::fts::search::TermQuery {
                terms: vec![expansion.term.clone()],
                fields: fields.clone(),
            };
            let scored = crate::fts::search::search_allow_list_driven_controlled(
                &index,
                &term_query,
                all_rows,
                crate::fts::bm25::Bm25Params::beir(),
                &allow_lists,
                || cancellation.check_graph(),
            )
            .map_err(map_store_controlled_lexical_error)?;
            accumulate_search_counters(&mut counters, &scored.counters);
            let boost = f64::from(expansion.boost_thousandths) / 1_000.0;
            for hit in scored.hits {
                let entry = aggregate.entry(hit.doc).or_default();
                entry.0 += hit.score * boost;
                entry.1.push(expansion.clone());
            }
        }
        let mut scored = Vec::with_capacity(aggregate.len());
        for (doc, (score, provenance)) in aggregate {
            if let Some((terms, slop)) = query.phrase_constraint() {
                let (text, _) = structured_lexical_row(&snapshot, &active, &sources, doc)?;
                if !crate::fts::query::phrase_matches(&self.tokenizer, text, terms, slop) {
                    continue;
                }
            }
            scored.push((doc, score, provenance));
        }
        scored.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(left.0.cmp(&right.0))
        });
        scored.truncate(k);
        let mut candidates = Vec::with_capacity(scored.len());
        for (doc, score, provenance) in scored {
            cancellation.check_graph().map_err(QueryError::Scan)?;
            let (text, document) = structured_lexical_row(&snapshot, &active, &sources, doc)?;
            let terms = provenance
                .iter()
                .map(|entry| entry.term.clone())
                .collect::<Vec<_>>();
            let snippet = crate::fts::snippet::best_window(
                &self.tokenizer,
                text,
                &terms,
                snippet_bytes,
                true,
            )?
            .ok_or(crate::ingest::StoreLexicalError::MissingSnippetMatch {
                segment: doc.segment,
                row: doc.row,
            })?;
            let snippet_text = snippet
                .text(text)
                .ok_or(QueryError::Store(StoreError::ActiveRowOverflow))?;
            candidates.push(crate::ingest::ExplainedLexicalCandidate {
                document,
                score,
                provenance,
                snippet: crate::fts::query::OwnedLexicalSnippet {
                    text: snippet_text.to_owned(),
                    source: snippet.window,
                    highlights: snippet.highlights,
                },
            });
        }
        cancellation.check_graph().map_err(QueryError::Scan)?;
        let diagnostics = crate::diag::QueryDiagnostics::lexical(crate::diag::LexicalDiagnostics {
            snapshot_generation: generation,
            indexed_through_seq: active
                .indexed_through_seq()
                .max(crate::wal::LogSeq::new(snapshot.absorbed_through())),
            requested_k: k,
            returned: candidates.len(),
            counters,
            elapsed: started.elapsed(),
        });
        drop(active_query);
        Ok(crate::ingest::StoreStructuredLexicalSearchOutcome {
            candidates,
            expansions,
            generation,
            diagnostics,
        })
    }

    /// Runs the store vector engine and exact lexical leg against one pinned
    /// generation, then delegates every blend decision to `crate::fusion`.
    pub fn search_hybrid(
        &self,
        vector_query: crate::ingest::SearchRequest<'_>,
        lexical_query: &crate::fts::search::TermQuery,
        hybrid_query: &crate::fusion::HybridQuery,
        options: impl Into<SearchOptions>,
        control: QueryControl,
    ) -> Result<crate::ingest::StoreHybridSearchOutcome, crate::fusion::FusionError> {
        self.search_hybrid_inner(
            vector_query,
            PinnedLexicalQuery::Term(lexical_query),
            hybrid_query,
            options.into(),
            control,
        )
    }

    /// Runs vector work with any structured lexical operator against the
    /// same pinned snapshot and generation.
    pub fn search_hybrid_structured(
        &self,
        vector_query: crate::ingest::SearchRequest<'_>,
        lexical_query: &crate::fts::query::LexicalQuery,
        hybrid_query: &crate::fusion::HybridQuery,
        options: impl Into<SearchOptions>,
        control: QueryControl,
    ) -> Result<crate::ingest::StoreHybridSearchOutcome, crate::fusion::FusionError> {
        self.search_hybrid_inner(
            vector_query,
            PinnedLexicalQuery::Structured(lexical_query),
            hybrid_query,
            options.into(),
            control,
        )
    }

    fn search_hybrid_inner(
        &self,
        vector_query: crate::ingest::SearchRequest<'_>,
        lexical_query: PinnedLexicalQuery<'_>,
        hybrid_query: &crate::fusion::HybridQuery,
        options: SearchOptions,
        control: QueryControl,
    ) -> Result<crate::ingest::StoreHybridSearchOutcome, crate::fusion::FusionError> {
        let control = control.with_clock(Arc::clone(&self.clock));
        let started = std::time::Instant::now();
        let mut options = options;
        let requested_tier = options.explicit_tier();
        // Hybrid once forced SearchTier::Exact unconditionally, because
        // fusion needs an anchor for unseen scores and only an exhaustive
        // scan could name the farthest alive row. The deterministic
        // ceiling now supplies that anchor on every tier, so hybrid
        // follows the caller's tier and reaches the graph like the dense
        // leg does.
        //
        // A segment with no published graph is different. There `Auto`
        // resolves to the Bit4 scan, whose scores are estimates, and
        // fusion may only fuse exactly rescored scores. Graph traversal
        // rescores its retained pool exactly, so it is safe; the Bit4
        // scan is not. When the caller stated no preference and no
        // segment has earned a graph yet, hybrid therefore still selects
        // Exact for itself. This is the adaptive ladder choosing the
        // tier that can honour the contract, not a fallback hiding one.
        let admitted = self
            .admit_vector_search(options)
            .map_err(crate::fusion::FusionError::from)?;
        let tier_resolution = if requested_tier.is_none() {
            if snapshot_has_graph(&admitted.snapshot) {
                Some(crate::diag::HybridTierResolution::Auto)
            } else {
                options = options.with_tier(SearchTier::Exact);
                Some(crate::diag::HybridTierResolution::Exact)
            }
        } else {
            None
        };
        let corpus_rows = hybrid::corpus_rows(&admitted.snapshot, &admitted.active_segment)?;
        // Round zero asks each producer for the window plus one row, so the
        // element just past the window is the stability bound's unseen value.
        // A caller that disabled widening asks for the corpus at once, which
        // is the complete-list fusion this path used to run unconditionally.
        let mut width = if hybrid_query.max_rounds == 0 {
            corpus_rows
        } else {
            hybrid::hybrid_window(hybrid_query.k, corpus_rows)?.width
        };
        let panic_vector = self.consume_hybrid_test_fault(crate::fusion::FusionLeg::Vector);
        let panic_lexical = self.consume_hybrid_test_fault(crate::fusion::FusionLeg::Lexical);
        let lexical_worker = self.ensure_lexical_worker()?;
        let submit_lexical_leg = |bound| {
            lexical_worker.submit(|| {
                let name = std::thread::current().name().map(str::to_owned);
                let result = (|| {
                    maybe_trigger_hybrid_leg_panic(
                        panic_lexical,
                        "injected lexical hybrid leg panic",
                    );
                    let lease =
                        SnapshotLease::new_at(Arc::clone(&admitted.snapshot), admitted.generation);
                    let cancellation = QueryCancellation::new(&control, &lease);
                    cancellation
                        .check_graph()
                        .map_err(QueryError::Scan)
                        .map_err(crate::fusion::FusionError::from)?;
                    match lexical_query {
                        PinnedLexicalQuery::Term(query) => exact_lexical_leg(
                            &admitted.snapshot,
                            &admitted.active_segment,
                            &self.accounting,
                            query,
                            bound,
                            &cancellation,
                        ),
                        PinnedLexicalQuery::Structured(query) => exact_structured_lexical_leg(
                            &admitted.snapshot,
                            &admitted.active_segment,
                            &self.accounting,
                            &self.tokenizer,
                            query,
                            bound,
                            &cancellation,
                        ),
                    }
                })();
                (name, result)
            })
        };
        let lexical_work = submit_lexical_leg(width.saturating_add(1))?;
        let caller_chose_tier = requested_tier.is_some();
        let run_vector_leg = |k: usize| {
            let graph_available = snapshot_has_graph(&admitted.snapshot);
            let scan_reason_override = if caller_chose_tier || !graph_available {
                None
            } else if k > corpus_rows {
                Some(crate::planner::ScanReason::FullMaterialization)
            } else if graph_round_is_worth_it(k, corpus_rows) {
                None
            } else {
                Some(crate::planner::ScanReason::WideningCap {
                    ef: graph_round_ef(k),
                    rows: corpus_rows,
                })
            };
            search_pinned(
                admitted.pool.as_deref(),
                &admitted.snapshot,
                &admitted.active_segment,
                &self.accounting,
                admitted.generation,
                self.epoch_identity(),
                vector_query,
                k,
                // A graph traversal only pays for itself while its
                // candidate pool is a small fraction of the segment. Once
                // a widening round would visit more than
                // `GRAPH_WIDENING_CAP_NUMERATOR/DENOMINATOR` of the rows,
                // the traversal is a scan wearing a graph costume: it
                // pays the graph's indirection and then reads most of the
                // segment anyway. Such a round runs as Exact instead,
                // which is both faster and exactly rescored.
                // Only when the caller stated no preference. An explicit
                // tier is a contract: honour it, and let fusion reject it
                // loudly if it cannot supply exact scores.
                if caller_chose_tier || graph_round_is_worth_it(k, corpus_rows) {
                    options
                } else {
                    options.with_tier(SearchTier::Exact)
                },
                control.clone(),
                GraphBoundMode::Shared,
                requested_tier,
                scan_reason_override,
                started,
                #[cfg(any(test, feature = "test-support"))]
                None,
            )
            .map_err(crate::fusion::FusionError::from)
        };
        let vector_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            maybe_trigger_hybrid_leg_panic(panic_vector, "injected vector hybrid leg panic");
            run_vector_leg(width.saturating_add(1))
        }))
        .unwrap_or(Err(crate::fusion::FusionError::LegPanic {
            leg: crate::fusion::FusionLeg::Vector,
            detail: "vector hybrid leg panicked",
        }));
        let (lexical_thread_name, lexical_result) = lexical_work
            .wait()
            .unwrap_or_else(|error| (Some("zeppelin-fts".to_owned()), Err(error)));
        #[cfg(any(test, feature = "test-support"))]
        if let Ok(mut receipt) = self.hybrid_execution_receipt.lock() {
            *receipt = Some(HybridExecutionReceipt {
                vector_thread: std::thread::current().id(),
                lexical_thread_name,
                vector_completed: true,
                lexical_completed: true,
            });
        }
        #[cfg(not(any(test, feature = "test-support")))]
        let _ = lexical_thread_name;
        let (
            mut vector_outcome,
            (mut lexical_hits, mut lexical_sources, mut lexical_counters, lexical_expansions),
        ) = resolve_hybrid_leg_results(vector_result, lexical_result)?;

        let mut rounds = 1_usize;
        let mut budget_exhausted = false;
        let (fused, report_window) = loop {
            let round = hybrid::build_round(
                &admitted.snapshot,
                &admitted.active_segment,
                &lexical_sources,
                vector_query.vector(),
                &vector_outcome.candidates,
                vector_outcome.vector_ceiling,
                &lexical_hits,
                width,
            )?;
            let fused = crate::fusion::fuse_bounded(
                hybrid_query,
                &round.vector,
                &round.lexical,
                round.bounds,
                |document: &Option<crate::ingest::DocId>| *document,
                |document: &Option<crate::ingest::DocId>| *document,
            )?;
            if fused.report.termination != crate::fusion::FusionTermination::WindowUnproven
                || width >= corpus_rows
            {
                break (
                    fused,
                    crate::diag::HybridReport {
                        window: width,
                        vector_returned: round.vector.len(),
                        lexical_returned: round.lexical.len(),
                        cross_filled_vector: round.cross_filled_vector,
                        cross_filled_lexical: round.cross_filled_lexical,
                    },
                );
            }
            width = if rounds >= hybrid_query.max_rounds {
                budget_exhausted = true;
                corpus_rows
            } else {
                width.saturating_mul(2).min(corpus_rows)
            };
            rounds = rounds.saturating_add(1);
            let lexical_work = submit_lexical_leg(width.saturating_add(1))?;
            let vector_result = run_vector_leg(width.saturating_add(1));
            let (_, lexical_result) = lexical_work
                .wait()
                .unwrap_or_else(|error| (Some("zeppelin-fts".to_owned()), Err(error)));
            let (next_vector, (next_hits, next_sources, next_counters, _)) =
                resolve_hybrid_leg_results(vector_result, lexical_result)?;
            vector_outcome = next_vector;
            lexical_hits = next_hits;
            lexical_sources = next_sources;
            lexical_counters = next_counters;
        };

        let crate::ingest::SearchOutcome {
            candidates: _,
            vector_ceiling: _,
            stats: scan,
            graph_stats: graph,
            generation,
            epoch,
            diagnostics: vector_diagnostics,
        } = vector_outcome;
        let mut report = fused.report;
        report.rounds = rounds;
        if budget_exhausted {
            report.budget_exhausted = true;
            report.termination = crate::fusion::FusionTermination::BudgetFullMaterialization;
        }
        let diagnostics = crate::diag::QueryDiagnostics::hybrid(crate::diag::HybridDiagnostics {
            snapshot_generation: vector_diagnostics.snapshot_generation,
            indexed_through_seq: vector_diagnostics.indexed_through_seq,
            plan: vector_diagnostics.plan,
            approximate: vector_diagnostics.approximate,
            exact_rescore: vector_diagnostics.exact_rescore,
            requested_k: hybrid_query.k,
            returned: fused.hits.len(),
            scan,
            graph,
            lexical: lexical_counters,
            report,
            hybrid: report_window,
            tier_resolution,
            epoch,
            elapsed: started.elapsed(),
        });
        Ok(crate::ingest::StoreHybridSearchOutcome {
            hits: fused.hits,
            lexical_expansions,
            generation,
            diagnostics,
        })
    }

    fn consume_hybrid_test_fault(&self, leg: crate::fusion::FusionLeg) -> bool {
        #[cfg(any(test, feature = "test-support"))]
        {
            let Ok(mut fault) = self.hybrid_leg_fault.lock() else {
                return false;
            };
            if matches!(*fault, Some(HybridLegTestFault::Panic(candidate)) if candidate == leg) {
                *fault = None;
                return true;
            }
        }
        #[cfg(not(any(test, feature = "test-support")))]
        let _ = leg;
        false
    }

    fn search_with_graph_bound_mode(
        &self,
        request: crate::ingest::SearchRequest<'_>,
        k: usize,
        options: SearchOptions,
        control: QueryControl,
        graph_bound_mode: GraphBoundMode,
    ) -> Result<crate::ingest::SearchOutcome, QueryError> {
        let control = control.with_clock(Arc::clone(&self.clock));
        let started = std::time::Instant::now();
        let admitted = self.admit_vector_search(options)?;
        let score = || {
            search_pinned(
                admitted.pool.as_deref(),
                &admitted.snapshot,
                &admitted.active_segment,
                &self.accounting,
                admitted.generation,
                self.epoch_identity(),
                request,
                k,
                options,
                control,
                graph_bound_mode,
                options.explicit_tier(),
                None,
                started,
                #[cfg(any(test, feature = "test-support"))]
                self.vector_fault_controller.as_ref(),
            )
        };
        #[cfg(any(test, feature = "test-support"))]
        let result = crate::kernels::vector_fault::run_store_scoring(
            self.kernel_fault_controller.as_ref(),
            score,
        );
        #[cfg(not(any(test, feature = "test-support")))]
        let result = score();
        #[cfg(any(test, feature = "test-support"))]
        {
            if let Some(controller) = self.vector_fault_controller.as_ref() {
                controller.finalize_search(result.is_ok());
            }
        }
        result
    }

    fn ensure_query_pool(&self) -> Result<Arc<pool::QueryPool>, QueryError> {
        let mut pool_slot = self.query_pool.lock().map_err(|_| {
            QueryError::Store(StoreError::Synchronization {
                component: "query pool",
            })
        })?;
        if pool_slot.is_none() {
            let capacity = crate::scan::physical_thread_capacity().map_err(|error| {
                QueryError::Store(StoreError::QueryPoolCapacity {
                    source: error.to_string(),
                })
            })?;
            let pool =
                pool::QueryPool::start(capacity, &self.accounting).map_err(QueryError::Store)?;
            *pool_slot = Some(Arc::new(pool));
        }
        let pool = pool_slot.as_ref().cloned().ok_or({
            QueryError::Store(StoreError::Synchronization {
                component: "query pool initialization",
            })
        })?;
        drop(pool_slot);
        Ok(pool)
    }

    fn ensure_lexical_worker(
        &self,
    ) -> Result<Arc<pool::LexicalWorker>, crate::fusion::FusionError> {
        let mut worker_slot =
            self.lexical_worker
                .lock()
                .map_err(|_| crate::fusion::FusionError::LegThreadStart {
                    leg: crate::fusion::FusionLeg::Lexical,
                    detail: "pooled lexical worker synchronization was poisoned".to_owned(),
                })?;
        if worker_slot.is_none() {
            *worker_slot = Some(Arc::new(pool::LexicalWorker::start()?));
        }
        let worker = worker_slot.as_ref().cloned().ok_or_else(|| {
            crate::fusion::FusionError::LegThreadStart {
                leg: crate::fusion::FusionLeg::Lexical,
                detail: "pooled lexical worker initialization failed".to_owned(),
            }
        })?;
        drop(worker_slot);
        Ok(worker)
    }

    fn admit_vector_search(
        &self,
        options: SearchOptions,
    ) -> Result<AdmittedVectorSearch<'_>, QueryError> {
        let state = self
            .state
            .lock()
            .map_err(|_| QueryError::Store(StoreError::Synchronization { component: "state" }))?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(QueryError::Store(StoreError::Closing)),
            StoreState::Closed => return Err(QueryError::Store(StoreError::Closed)),
        }
        let active_guard = self.active.lock().map_err(|_| {
            QueryError::Store(StoreError::Synchronization {
                component: "active segment",
            })
        })?;
        let active_state = active_guard
            .as_ref()
            .ok_or(QueryError::Store(StoreError::Closed))?;
        let generation = active_state.generation;
        let active_segment = Arc::clone(&active_state.segment);
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| {
                QueryError::Store(StoreError::Synchronization {
                    component: "published snapshot",
                })
            })?
            .as_ref()
            .cloned()
            .ok_or(QueryError::Store(StoreError::Closed))?;
        let needs_query_pool = !active_segment.is_empty()
            || match options.tier() {
                SearchTier::Auto => snapshot.segments().iter().any(|segment| {
                    !segment.directory().iter().any(|entry| {
                        entry.kind == crate::segment::layout::RegionKind::GraphNodeBlocks.id()
                    })
                }),
                SearchTier::Exact => false,
                SearchTier::Scan => true,
                SearchTier::Graph(_) => false,
            };
        let pool = if needs_query_pool {
            Some(self.ensure_query_pool()?)
        } else {
            None
        };
        self.active_queries.fetch_add(1, Ordering::Relaxed);
        let active_query = ActiveQuery {
            count: &self.active_queries,
        };
        drop(active_guard);
        drop(state);
        Ok(AdmittedVectorSearch {
            pool,
            snapshot,
            active_segment,
            generation,
            _active_query: active_query,
        })
    }

    #[cfg(test)]
    fn search_independent_for_test(
        &self,
        request: crate::ingest::SearchRequest<'_>,
        k: usize,
        options: SearchOptions,
        control: QueryControl,
    ) -> Result<crate::ingest::SearchOutcome, QueryError> {
        self.search_with_graph_bound_mode(request, k, options, control, GraphBoundMode::Independent)
    }
}

#[derive(Clone, Copy)]
pub(crate) enum StructuredLexicalSource {
    Sealed(usize),
    Active,
}

struct LexicalAssembly {
    index: crate::fts::index::LexicalIndex,
    alive_sets: Vec<Arc<crate::meta::AliveSet>>,
    sources: Vec<StructuredLexicalSource>,
}

enum LexicalAssemblyError {
    Store(StoreError),
    Lexical(crate::fts::index::IndexError),
    MissingDocumentIdentity(crate::segment::SegmentId),
    Cancelled(crate::scan::ScanError),
}

fn assemble_lexical_index(
    snapshot: &PublishedSnapshot,
    active: &crate::ingest::ActiveSegment,
    accounting: &Arc<stats::Accounting>,
    require_document_identity: bool,
    cancellation: Option<&QueryCancellation<'_>>,
) -> Result<LexicalAssembly, LexicalAssemblyError> {
    let mut index = crate::fts::index::LexicalIndex::new();
    let mut alive_sets = Vec::new();
    let mut sources = Vec::new();
    for (ordinal, segment) in snapshot.segments().iter().enumerate() {
        if let Some(cancellation) = cancellation {
            cancellation
                .check_graph()
                .map_err(LexicalAssemblyError::Cancelled)?;
        }
        if let Some(postings) = segment
            .query_postings()
            .map_err(LexicalAssemblyError::Store)?
        {
            if require_document_identity
                && postings.row_count() != 0
                && segment
                    .document_version(0)
                    .map_err(StoreError::Segment)
                    .map_err(LexicalAssemblyError::Store)?
                    .is_none()
            {
                return Err(LexicalAssemblyError::MissingDocumentIdentity(
                    segment.meta().id,
                ));
            }
            let alive = segment.query_alive().map_err(LexicalAssemblyError::Store)?;
            index
                .push_shared_with_live_rows(postings, alive.alive_bitmap())
                .map_err(LexicalAssemblyError::Lexical)?;
            alive_sets.push(alive);
            sources.push(StructuredLexicalSource::Sealed(ordinal));
        }
    }
    if active.has_text() {
        let sealed = active
            .sealed_lexical(accounting)
            .map_err(LexicalAssemblyError::Store)?;
        let alive = Arc::new(active.alive().map_err(LexicalAssemblyError::Store)?);
        index
            .push_shared_with_live_rows(sealed, alive.alive_bitmap())
            .map_err(LexicalAssemblyError::Lexical)?;
        alive_sets.push(alive);
        sources.push(StructuredLexicalSource::Active);
    }
    Ok(LexicalAssembly {
        index,
        alive_sets,
        sources,
    })
}

fn map_store_lexical_assembly_error(
    error: LexicalAssemblyError,
) -> crate::ingest::StoreLexicalError {
    match error {
        LexicalAssemblyError::Store(error) => QueryError::Store(error).into(),
        LexicalAssemblyError::Lexical(error) => {
            crate::planner::LexicalFilterError::from(error).into()
        }
        LexicalAssemblyError::MissingDocumentIdentity(segment_id) => {
            crate::ingest::StoreLexicalError::MissingDocumentIdentity { segment_id }
        }
        LexicalAssemblyError::Cancelled(error) => QueryError::Scan(error).into(),
    }
}

fn map_fusion_lexical_assembly_error(error: LexicalAssemblyError) -> crate::fusion::FusionError {
    match error {
        LexicalAssemblyError::Store(error) => crate::fusion::FusionError::Leg {
            leg: crate::fusion::FusionLeg::Lexical,
            kind: crate::fusion::LegFailureKind::Store(error.kind()),
            detail: error.to_string(),
        },
        LexicalAssemblyError::Lexical(error) => crate::fusion::FusionError::Leg {
            leg: crate::fusion::FusionLeg::Lexical,
            kind: crate::fusion::LegFailureKind::Lexical,
            detail: error.to_string(),
        },
        LexicalAssemblyError::MissingDocumentIdentity(segment_id) => {
            crate::fusion::FusionError::Leg {
                leg: crate::fusion::FusionLeg::Lexical,
                kind: crate::fusion::LegFailureKind::Invariant,
                detail: format!("sealed lexical segment {segment_id} has no document identity"),
            }
        }
        LexicalAssemblyError::Cancelled(error) => {
            crate::fusion::FusionError::from(QueryError::Scan(error))
        }
    }
}

fn accumulate_search_counters(
    total: &mut crate::fts::search::SearchCounters,
    delta: &crate::fts::search::SearchCounters,
) {
    total.docs_evaluated = total.docs_evaluated.saturating_add(delta.docs_evaluated);
    total.postings_decoded = total
        .postings_decoded
        .saturating_add(delta.postings_decoded);
    total.blocks_decoded = total.blocks_decoded.saturating_add(delta.blocks_decoded);
    total.blocks_skipped = total.blocks_skipped.saturating_add(delta.blocks_skipped);
}

fn map_store_controlled_lexical_error(
    error: crate::fts::search::ControlledSearchError<crate::scan::ScanError>,
) -> crate::ingest::StoreLexicalError {
    match error {
        crate::fts::search::ControlledSearchError::Index(error) => {
            crate::planner::LexicalFilterError::from(error).into()
        }
        crate::fts::search::ControlledSearchError::Control(error) => QueryError::Scan(error).into(),
    }
}

fn map_fusion_controlled_lexical_error(
    error: crate::fts::search::ControlledSearchError<crate::scan::ScanError>,
) -> crate::fusion::FusionError {
    match error {
        crate::fts::search::ControlledSearchError::Index(error) => {
            crate::fusion::FusionError::Leg {
                leg: crate::fusion::FusionLeg::Lexical,
                kind: crate::fusion::LegFailureKind::Lexical,
                detail: error.to_string(),
            }
        }
        crate::fts::search::ControlledSearchError::Control(error) => {
            crate::fusion::FusionError::from(QueryError::Scan(error))
        }
    }
}

#[derive(Clone, Copy)]
enum PinnedLexicalQuery<'a> {
    Term(&'a crate::fts::search::TermQuery),
    Structured(&'a crate::fts::query::LexicalQuery),
}

enum LexicalDocumentError {
    SourceOrdinalOutOfRange,
    RowOverflow,
    SealedSourceAbsent,
    Segment(crate::segment::SegmentError),
    MissingDocumentIdentity(crate::segment::SegmentId),
    ActiveRowAbsent,
}

fn structured_lexical_document(
    snapshot: &PublishedSnapshot,
    active: &crate::ingest::ActiveSegment,
    sources: &[StructuredLexicalSource],
    doc: crate::fts::search::GlobalDocId,
    require_document_identity: bool,
) -> Result<Option<crate::ingest::DocumentVersion>, LexicalDocumentError> {
    let source = usize::try_from(doc.segment)
        .ok()
        .and_then(|slot| sources.get(slot))
        .ok_or(LexicalDocumentError::SourceOrdinalOutOfRange)?;
    let row = usize::try_from(doc.row).map_err(|_| LexicalDocumentError::RowOverflow)?;
    match source {
        StructuredLexicalSource::Sealed(ordinal) => {
            let segment = snapshot
                .segments()
                .get(*ordinal)
                .ok_or(LexicalDocumentError::SealedSourceAbsent)?;
            let document = segment
                .document_version(row)
                .map_err(LexicalDocumentError::Segment)?;
            if require_document_identity && document.is_none() {
                return Err(LexicalDocumentError::MissingDocumentIdentity(
                    segment.meta().id,
                ));
            }
            Ok(document)
        }
        StructuredLexicalSource::Active => {
            let document = active.document(row);
            if require_document_identity && document.is_none() {
                return Err(LexicalDocumentError::ActiveRowAbsent);
            }
            Ok(document)
        }
    }
}

fn map_store_lexical_document_error(
    error: LexicalDocumentError,
) -> crate::ingest::StoreLexicalError {
    match error {
        LexicalDocumentError::SourceOrdinalOutOfRange
        | LexicalDocumentError::RowOverflow
        | LexicalDocumentError::SealedSourceAbsent
        | LexicalDocumentError::ActiveRowAbsent => {
            QueryError::Store(StoreError::ActiveRowOverflow).into()
        }
        LexicalDocumentError::Segment(error) => {
            QueryError::Store(StoreError::Segment(error)).into()
        }
        LexicalDocumentError::MissingDocumentIdentity(segment_id) => {
            crate::ingest::StoreLexicalError::MissingDocumentIdentity { segment_id }
        }
    }
}

fn map_fusion_lexical_document_error(
    error: LexicalDocumentError,
    invariant_kind: crate::fusion::LegFailureKind,
    segment_kind: crate::fusion::LegFailureKind,
) -> crate::fusion::FusionError {
    let (kind, detail) = match error {
        LexicalDocumentError::SourceOrdinalOutOfRange => (
            invariant_kind,
            "lexical source ordinal is out of range".to_owned(),
        ),
        LexicalDocumentError::RowOverflow => {
            (invariant_kind, "lexical row exceeds usize".to_owned())
        }
        LexicalDocumentError::SealedSourceAbsent => {
            (invariant_kind, "lexical sealed source is absent".to_owned())
        }
        LexicalDocumentError::Segment(error) => (segment_kind, error.to_string()),
        LexicalDocumentError::MissingDocumentIdentity(segment_id) => (
            invariant_kind,
            format!("sealed lexical segment {segment_id} has no document identity"),
        ),
        LexicalDocumentError::ActiveRowAbsent => (
            invariant_kind,
            "lexical active source row is absent".to_owned(),
        ),
    };
    crate::fusion::FusionError::Leg {
        leg: crate::fusion::FusionLeg::Lexical,
        kind,
        detail,
    }
}

fn map_structured_leg_document_error(error: LexicalDocumentError) -> crate::fusion::FusionError {
    map_fusion_lexical_document_error(
        error,
        crate::fusion::LegFailureKind::Lexical,
        crate::fusion::LegFailureKind::Lexical,
    )
}

fn map_term_leg_document_error(error: LexicalDocumentError) -> crate::fusion::FusionError {
    map_fusion_lexical_document_error(
        error,
        crate::fusion::LegFailureKind::Invariant,
        crate::fusion::LegFailureKind::Segment,
    )
}

fn structured_lexical_row<'a>(
    snapshot: &'a PublishedSnapshot,
    active: &'a crate::ingest::ActiveSegment,
    sources: &[StructuredLexicalSource],
    doc: crate::fts::search::GlobalDocId,
) -> Result<(&'a str, crate::ingest::DocumentVersion), crate::ingest::StoreLexicalError> {
    let source = usize::try_from(doc.segment)
        .ok()
        .and_then(|slot| sources.get(slot))
        .ok_or(QueryError::Store(StoreError::ActiveRowOverflow))?;
    let row =
        usize::try_from(doc.row).map_err(|_| QueryError::Store(StoreError::ActiveRowOverflow))?;
    match source {
        StructuredLexicalSource::Sealed(ordinal) => {
            let segment = snapshot
                .segments()
                .get(*ordinal)
                .ok_or(QueryError::Store(StoreError::ActiveRowOverflow))?;
            let text = segment
                .query_stored_text()
                .map_err(StoreError::Segment)
                .map_err(QueryError::Store)?
                .and_then(|rows| rows.row(row).flatten())
                .ok_or(crate::ingest::StoreLexicalError::MissingStoredText {
                    segment_id: segment.meta().id,
                    row: doc.row,
                })?;
            let document = segment
                .document_version(row)
                .map_err(StoreError::Segment)
                .map_err(QueryError::Store)?
                .ok_or(crate::ingest::StoreLexicalError::MissingDocumentIdentity {
                    segment_id: segment.meta().id,
                })?;
            Ok((text, document))
        }
        StructuredLexicalSource::Active => {
            let text = active
                .text(row)
                .map_err(QueryError::Store)?
                .ok_or(QueryError::Store(StoreError::ActiveRowOverflow))?;
            let document = active
                .document(row)
                .ok_or(QueryError::Store(StoreError::ActiveRowOverflow))?;
            Ok((text, document))
        }
    }
}

type ExactLexicalLeg = (
    Vec<hybrid::LexicalHit>,
    Vec<StructuredLexicalSource>,
    crate::fts::search::SearchCounters,
    Vec<crate::fts::query::LexicalExpansion>,
);

fn resolve_hybrid_leg_results<Vector, Lexical>(
    vector: Result<Vector, crate::fusion::FusionError>,
    lexical: Result<Lexical, crate::fusion::FusionError>,
) -> Result<(Vector, Lexical), crate::fusion::FusionError> {
    fn control_priority(error: &crate::fusion::FusionError) -> Option<u8> {
        match error {
            crate::fusion::FusionError::ReadCancelled { .. } => Some(0),
            crate::fusion::FusionError::Timeout { .. } => Some(1),
            crate::fusion::FusionError::Cancelled { .. } => Some(2),
            _ => None,
        }
    }

    match (vector, lexical) {
        (Ok(vector), Ok(lexical)) => Ok((vector, lexical)),
        (Err(vector), Ok(_)) => Err(vector),
        (Ok(_), Err(lexical)) => Err(lexical),
        (Err(vector), Err(lexical)) => {
            match (control_priority(&vector), control_priority(&lexical)) {
                (Some(vector_priority), Some(lexical_priority)) => {
                    if vector_priority <= lexical_priority {
                        Err(vector)
                    } else {
                        Err(lexical)
                    }
                }
                (Some(_), None) => Err(vector),
                (None, Some(_)) => Err(lexical),
                (None, None) => Err(vector),
            }
        }
    }
}

fn exact_structured_lexical_leg(
    snapshot: &PublishedSnapshot,
    active: &crate::ingest::ActiveSegment,
    accounting: &Arc<stats::Accounting>,
    analyzer: &crate::fts::tokenizer::Analyzer,
    query: &crate::fts::query::LexicalQuery,
    bound: usize,
    cancellation: &QueryCancellation<'_>,
) -> Result<ExactLexicalLeg, crate::fusion::FusionError> {
    let lexical_error = |detail: String| crate::fusion::FusionError::Leg {
        leg: crate::fusion::FusionLeg::Lexical,
        kind: crate::fusion::LegFailureKind::Lexical,
        detail,
    };
    let LexicalAssembly {
        index,
        alive_sets,
        sources,
    } = assemble_lexical_index(snapshot, active, accounting, false, Some(cancellation))
        .map_err(map_fusion_lexical_assembly_error)?;
    let vocabulary = crate::fts::query::vocabulary(query, index.terms());
    let expansions = crate::fts::query::expand(query, &vocabulary)
        .map_err(|error| lexical_error(error.to_string()))?;
    if index.segments().is_empty() || expansions.is_empty() {
        return Ok((
            Vec::new(),
            sources,
            crate::fts::search::SearchCounters::default(),
            expansions,
        ));
    }
    let allow_lists = alive_sets
        .iter()
        .map(|alive| alive.alive_bitmap())
        .collect::<Vec<_>>();
    let fields = query.fields();
    let k = bound.min(usize::try_from(index.document_count()).unwrap_or(usize::MAX));
    let mut counters = crate::fts::search::SearchCounters::default();
    let mut aggregate = BTreeMap::<crate::fts::search::GlobalDocId, f64>::new();
    for expansion in &expansions {
        let term_query = crate::fts::search::TermQuery {
            terms: vec![expansion.term.clone()],
            fields: fields.clone(),
        };
        let result =
            exact_hybrid_lexical_search(&index, &term_query, k, &allow_lists, cancellation)?;
        accumulate_search_counters(&mut counters, &result.counters);
        let boost = f64::from(expansion.boost_thousandths) / 1_000.0;
        for hit in result.hits {
            *aggregate.entry(hit.doc).or_default() += hit.score * boost;
        }
    }
    if let Some((terms, slop)) = query.phrase_constraint() {
        let mut retained = BTreeMap::new();
        for (doc, score) in aggregate {
            let (text, _) = structured_lexical_row(snapshot, active, &sources, doc)
                .map_err(|error| lexical_error(error.to_string()))?;
            if crate::fts::query::phrase_matches(analyzer, text, terms, slop) {
                retained.insert(doc, score);
            }
        }
        aggregate = retained;
    }
    let mut scored = aggregate.into_iter().collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.0.cmp(&right.0))
    });
    scored.truncate(k);
    let mut joined = Vec::with_capacity(scored.len());
    for (doc, score) in scored {
        let document = structured_lexical_document(snapshot, active, &sources, doc, false)
            .map_err(map_structured_leg_document_error)?
            .map(|version| version.doc_id());
        joined.push(hybrid::LexicalHit {
            doc,
            document,
            bm25: score,
        });
    }
    Ok((joined, sources, counters, expansions))
}

fn exact_lexical_leg(
    snapshot: &PublishedSnapshot,
    active: &crate::ingest::ActiveSegment,
    accounting: &Arc<stats::Accounting>,
    query: &crate::fts::search::TermQuery,
    bound: usize,
    cancellation: &QueryCancellation<'_>,
) -> Result<ExactLexicalLeg, crate::fusion::FusionError> {
    let LexicalAssembly {
        index,
        alive_sets,
        sources,
    } = assemble_lexical_index(snapshot, active, accounting, false, Some(cancellation))
        .map_err(map_fusion_lexical_assembly_error)?;
    if index.segments().is_empty() {
        return Ok((
            Vec::new(),
            sources,
            crate::fts::search::SearchCounters::default(),
            query
                .terms
                .iter()
                .cloned()
                .map(|term| crate::fts::query::LexicalExpansion {
                    term,
                    boost_thousandths: 1_000,
                    kind: crate::fts::query::LexicalMatchKind::Term,
                })
                .collect(),
        ));
    }
    let k = bound.min(usize::try_from(index.document_count()).unwrap_or(usize::MAX));
    let allow_lists = alive_sets
        .iter()
        .map(|alive| alive.alive_bitmap())
        .collect::<Vec<_>>();
    let result = exact_hybrid_lexical_search(&index, query, k, &allow_lists, cancellation)?;
    let mut joined = Vec::with_capacity(result.hits.len());
    for hit in result.hits {
        let document = structured_lexical_document(snapshot, active, &sources, hit.doc, false)
            .map_err(map_term_leg_document_error)?
            .map(|version| version.doc_id());
        joined.push(hybrid::LexicalHit {
            doc: hit.doc,
            document,
            bm25: hit.score,
        });
    }
    Ok((
        joined,
        sources,
        result.counters,
        query
            .terms
            .iter()
            .cloned()
            .map(|term| crate::fts::query::LexicalExpansion {
                term,
                boost_thousandths: 1_000,
                kind: crate::fts::query::LexicalMatchKind::Term,
            })
            .collect(),
    ))
}

fn exact_hybrid_lexical_search(
    index: &crate::fts::index::LexicalIndex,
    query: &crate::fts::search::TermQuery,
    k: usize,
    allow_lists: &[&crate::meta::DocBitmap],
    cancellation: &QueryCancellation<'_>,
) -> Result<crate::fts::search::SearchResult, crate::fusion::FusionError> {
    let allowed = allow_lists
        .iter()
        .map(|allow_list| allow_list.cardinality())
        .fold(0_u64, u64::saturating_add);
    if allowed.saturating_mul(crate::planner::LEXICAL_ALLOW_LIST_DIVISOR) <= index.document_count()
    {
        return crate::fts::search::search_allow_list_driven_controlled(
            index,
            query,
            k,
            crate::fts::bm25::Bm25Params::beir(),
            allow_lists,
            || cancellation.check_graph(),
        )
        .map_err(map_fusion_controlled_lexical_error);
    }

    cancellation
        .check_graph()
        .map_err(QueryError::Scan)
        .map_err(crate::fusion::FusionError::from)?;
    let result = crate::fts::prune::search_pruned_filtered(
        index,
        query,
        k,
        crate::fts::bm25::Bm25Params::beir(),
        crate::fts::prune::select_strategy(query.terms.len(), k),
        allow_lists,
    )
    .map_err(|error| crate::fusion::FusionError::Leg {
        leg: crate::fusion::FusionLeg::Lexical,
        kind: crate::fusion::LegFailureKind::Lexical,
        detail: error.to_string(),
    })?;
    cancellation
        .check_graph()
        .map_err(QueryError::Scan)
        .map_err(crate::fusion::FusionError::from)?;
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn search_pinned(
    pool: Option<&pool::QueryPool>,
    snapshot: &Arc<PublishedSnapshot>,
    active: &crate::ingest::ActiveSegment,
    accounting: &Arc<stats::Accounting>,
    generation: u64,
    epoch: Option<crate::epoch::EpochIdentity>,
    request: crate::ingest::SearchRequest<'_>,
    k: usize,
    options: SearchOptions,
    control: QueryControl,
    graph_bound_mode: GraphBoundMode,
    requested_tier: Option<SearchTier>,
    hybrid_scan_reason: Option<crate::planner::ScanReason>,
    started: std::time::Instant,
    #[cfg(any(test, feature = "test-support"))] vector_fault_controller: Option<
        &crate::scan::vector_fault::VectorFaultController,
    >,
) -> Result<crate::ingest::SearchOutcome, QueryError> {
    use crate::ingest::{GraphSearchStats, RowSource, SearchOutcome};
    use crate::quant::{QuantError, prepare_bit4_query};
    use crate::scan::{ScanQuery, ScanRequest, ScanRows, ScanStats};

    let scan_options = options.scan();
    let query = request.vector();
    let quantized_query_validation = if query.is_empty() {
        Err(QuantError::EmptyVector)
    } else if query.len() > crate::kernels::MAX_DOT_I8_DIMENSION {
        Err(QuantError::DimensionTooLarge {
            actual: query.len(),
            maximum: crate::kernels::MAX_DOT_I8_DIMENSION,
        })
    } else if let Some((index, _)) = query
        .iter()
        .enumerate()
        .find(|(_, value)| !value.is_finite())
    {
        Err(QuantError::NonFinite { index })
    } else {
        Ok(())
    };
    quantized_query_validation
        .map_err(crate::scan::ScanError::Quant)
        .map_err(QueryError::Scan)?;
    let bit4_query = std::cell::OnceCell::new();
    let int8_query = std::cell::OnceCell::new();
    let mut candidates = Vec::new();
    let mut dims_touched = 0_u64;
    let mut bytes_read = 0_u64;
    let mut worst_squared_l2: Option<f64> = None;
    let mut worst_exhaustive = true;
    let mut worker_thread_ids = Vec::new();
    let mut graph_stats = GraphSearchStats::default();
    let mut plans = Vec::new();
    let auto_graph_options = if matches!(options.tier(), SearchTier::Auto)
        && snapshot.segments().iter().any(|segment| {
            segment
                .directory()
                .iter()
                .any(|entry| entry.kind == crate::segment::layout::RegionKind::GraphNodeBlocks.id())
        }) {
        Some(auto_graph_search_options(snapshot)?)
    } else {
        None
    };

    if matches!(options.tier(), SearchTier::Graph(_)) {
        for segment in snapshot.segments() {
            if !segment
                .directory()
                .iter()
                .any(|entry| entry.kind == crate::segment::layout::RegionKind::GraphNodeBlocks.id())
            {
                return Err(QueryError::Store(StoreError::GraphUnavailable {
                    segment_id: segment.meta().id,
                }));
            }
        }
    }
    let full_precision = matches!(options.tier(), SearchTier::Exact | SearchTier::Graph(_))
        || (matches!(options.tier(), SearchTier::Auto)
            && auto_uses_full_precision(snapshot, active));

    if !active.is_empty() {
        let alive = active.alive().map_err(QueryError::Store)?;
        let exact_score = full_precision;
        let outcome = match options.tier() {
            SearchTier::Auto if full_precision => {
                let lease = SnapshotLease::new_at(Arc::clone(snapshot), generation);
                let cancellation = QueryCancellation::new(&control, &lease);
                scan_active_squared_l2(
                    active,
                    &alive,
                    request.vector(),
                    k,
                    &cancellation,
                    #[cfg(any(test, feature = "test-support"))]
                    vector_fault_controller,
                    #[cfg(any(test, feature = "test-support"))]
                    vector_fault_tier(options.tier()),
                )?
            }
            SearchTier::Auto | SearchTier::Scan => {
                let query_pool = pool.ok_or(QueryError::Store(StoreError::Synchronization {
                    component: "active scan-tier query pool",
                }))?;
                let bit4_query = bit4_query
                    .get_or_init(|| prepare_bit4_query(request.vector(), 0))
                    .as_ref()
                    .map_err(|error| crate::scan::ScanError::Quant(error.clone()))
                    .map_err(QueryError::Scan)?;
                execute_store_scan(
                    query_pool,
                    ScanRequest {
                        query: ScanQuery::Bit4(bit4_query),
                        rows: ScanRows::Bit4RowMajor {
                            codes: active.codes(),
                            factors: active.factors(),
                        },
                        row_mask: Some(alive.scan_mask()),
                    },
                    k,
                    scan_options,
                    control.clone(),
                    SnapshotLease::new_at(Arc::clone(snapshot), generation),
                    #[cfg(any(test, feature = "test-support"))]
                    vector_fault_controller,
                    #[cfg(any(test, feature = "test-support"))]
                    crate::scan::vector_fault::VectorRowSource::Active,
                    #[cfg(any(test, feature = "test-support"))]
                    vector_fault_tier(options.tier()),
                )?
            }
            SearchTier::Exact | SearchTier::Graph(_) => {
                let lease = SnapshotLease::new_at(Arc::clone(snapshot), generation);
                let cancellation = QueryCancellation::new(&control, &lease);
                scan_active_squared_l2(
                    active,
                    &alive,
                    request.vector(),
                    k,
                    &cancellation,
                    #[cfg(any(test, feature = "test-support"))]
                    vector_fault_controller,
                    #[cfg(any(test, feature = "test-support"))]
                    vector_fault_tier(options.tier()),
                )?
            }
        };
        fold_worst_squared_l2(&mut worst_squared_l2, &mut worst_exhaustive, &outcome);
        merge_store_outcome(
            outcome,
            RowSource::Active,
            |row| Ok(active.document(row)),
            &mut candidates,
            &mut dims_touched,
            &mut bytes_read,
            &mut worker_thread_ids,
            exact_score,
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_controller,
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_tier(options.tier()),
        )?;
        plans.push(crate::planner::SegmentPlan::unfiltered_scan(
            RowSource::Active,
            crate::planner::SegmentTier::ActiveScan,
            alive.live_count(),
            crate::planner::ScanReason::ActiveSegment,
        ));
        if matches!(graph_bound_mode, GraphBoundMode::Shared) {
            retain_global_top_k(&mut candidates, k);
        }
    }

    let mut ordered_segments = snapshot.segments().iter().collect::<Vec<_>>();
    if matches!(graph_bound_mode, GraphBoundMode::Shared) {
        // Larger immutable segments have more opportunities to supply the
        // first competitive top-k. Row count is already in the manifest, so
        // this ordering tightens the bound without query-time artifact I/O.
        ordered_segments.sort_unstable_by(|left, right| {
            right
                .meta()
                .row_count
                .cmp(&left.meta().row_count)
                .then_with(|| left.meta().id.cmp(&right.meta().id))
        });
    }

    for segment in ordered_segments {
        let alive = segment.query_alive().map_err(QueryError::Store)?;
        let source = RowSource::Sealed(segment.meta().id);
        // Automatic tiering follows the artifact that is atomically published
        // now, not the tier policy's desired future state. A due-but-unbuilt
        // graph is therefore scanned silently and correctly: that is the
        // adaptive ladder working, not a fallback hiding a broken contract.
        let graph_options = match options.tier() {
            SearchTier::Graph(graph_options) => Some(graph_options),
            SearchTier::Auto
                if segment.directory().iter().any(|entry| {
                    entry.kind == crate::segment::layout::RegionKind::GraphNodeBlocks.id()
                }) =>
            {
                auto_graph_options
            }
            SearchTier::Auto | SearchTier::Exact | SearchTier::Scan => None,
        };
        if let Some(graph_options) = graph_options {
            worst_exhaustive = false;
            let competitive_distance = global_competitive_distance(&candidates, k);
            let MergedSegmentGraph {
                plan,
                traversed,
                _result,
                _scratch,
                _lease,
            } = traverse_segment_graph(
                segment,
                &alive,
                source,
                graph_options,
                graph_bound_mode,
                request,
                k,
                accounting,
                snapshot,
                generation,
                &control,
                competitive_distance,
                #[cfg(any(test, feature = "test-support"))]
                vector_fault_controller,
                options.tier(),
            )?
            .merge_into(
                segment,
                &alive,
                source,
                &mut candidates,
                &mut dims_touched,
                &mut bytes_read,
                &mut worker_thread_ids,
                &mut graph_stats,
                #[cfg(any(test, feature = "test-support"))]
                vector_fault_controller,
                options.tier(),
            )?;
            plans.push(plan);
            if traversed && matches!(graph_bound_mode, GraphBoundMode::Shared) {
                retain_global_top_k(&mut candidates, k);
            }
            continue;
        }

        let outcome = scan_sealed_segment(
            pool,
            snapshot,
            segment,
            &alive,
            generation,
            request,
            k,
            scan_options,
            &control,
            &bit4_query,
            &int8_query,
            full_precision,
            source,
            options.tier(),
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_controller,
        )?;
        let exact_score = full_precision || segment.meta().scheme == 0;
        fold_worst_squared_l2(&mut worst_squared_l2, &mut worst_exhaustive, &outcome);
        merge_store_outcome(
            outcome,
            source,
            |row| {
                segment
                    .document_version(row)
                    .map_err(StoreError::Segment)
                    .map_err(QueryError::Store)
            },
            &mut candidates,
            &mut dims_touched,
            &mut bytes_read,
            &mut worker_thread_ids,
            exact_score,
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_controller,
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_tier(options.tier()),
        )?;
        let tier = if segment
            .directory()
            .iter()
            .any(|entry| entry.kind == crate::segment::layout::RegionKind::GraphNodeBlocks.id())
        {
            crate::planner::SegmentTier::SealedGraph
        } else {
            crate::planner::SegmentTier::SealedScan
        };
        plans.push(crate::planner::SegmentPlan::unfiltered_scan(
            source,
            tier,
            alive.live_count(),
            sealed_scan_reason(segment, requested_tier, hybrid_scan_reason),
        ));
        if matches!(graph_bound_mode, GraphBoundMode::Shared) {
            retain_global_top_k(&mut candidates, k);
        }
    }

    retain_global_top_k(&mut candidates, k);
    let stats = ScanStats {
        dims_touched,
        bytes_read,
        threads_used: worker_thread_ids.len(),
        worker_thread_ids,
    };
    accounting.record_plans(&plans).map_err(QueryError::Store)?;
    let diagnostics = crate::diag::QueryDiagnostics::vector(crate::diag::VectorDiagnostics {
        snapshot_generation: generation,
        indexed_through_seq: active
            .indexed_through_seq()
            .max(crate::wal::LogSeq::new(snapshot.absorbed_through())),
        approximate: plans.iter().any(|plan| plan.approximate),
        exact_rescore: candidates.iter().all(|candidate| candidate.exact_score()),
        requested_k: k,
        returned: candidates.len(),
        budget_exhausted: false,
        plan: plans,
        scan: stats.clone(),
        graph: graph_stats,
        epoch,
        elapsed: started.elapsed(),
    });
    Ok(SearchOutcome {
        candidates,
        // Every tier reports the same deterministic ceiling. A graph
        // traversal cannot name the farthest alive row, and letting the
        // anchor depend on the tier would let the tier change ranking,
        // so the ceiling is the anchor everywhere.
        vector_ceiling: Some(exact_vector_ceiling(snapshot, active, query)?),
        stats,
        graph_stats,
        generation,
        epoch,
        diagnostics,
    })
}

/// Reports whether any sealed segment has a published graph region.
///
/// Only a published artifact counts. A graph that policy says is due but
/// that maintenance has not built yet is not usable by a query.
/// Widening rounds visit `ef` rows per segment, and `ef` grows with the
/// requested `k`. Past this share of the corpus a traversal reads most of
/// the segment anyway, so the round is cheaper and exact as a scan.
const GRAPH_WIDENING_CAP_NUMERATOR: usize = 1;
/// Denominator of [`GRAPH_WIDENING_CAP_NUMERATOR`]; the cap is 25%.
const GRAPH_WIDENING_CAP_DENOMINATOR: usize = 4;

/// Reports whether a widening round of width `k` should still use the graph.
///
/// `ef` follows `graph::search`: at least the SIFT floor, and otherwise a
/// small multiple of `k`. The angular multiple is used so the cap is
/// evaluated against the widest pool any profile would request, which
/// keeps the decision independent of the distance metric in play.
fn graph_round_is_worth_it(k: usize, corpus_rows: usize) -> bool {
    if corpus_rows == 0 {
        return true;
    }
    let ef = graph_round_ef(k);
    let cap =
        corpus_rows.saturating_mul(GRAPH_WIDENING_CAP_NUMERATOR) / GRAPH_WIDENING_CAP_DENOMINATOR;
    ef <= cap
}

fn graph_round_ef(k: usize) -> usize {
    k.saturating_mul(4).max(140)
}

fn sealed_scan_reason(
    segment: &crate::segment::reader::SegmentReader,
    requested_tier: Option<SearchTier>,
    hybrid_scan_reason: Option<crate::planner::ScanReason>,
) -> crate::planner::ScanReason {
    use crate::planner::{ExplicitScanTier, ScanReason};

    let has_graph = segment
        .directory()
        .iter()
        .any(|entry| entry.kind == crate::segment::layout::RegionKind::GraphNodeBlocks.id());
    if has_graph && let Some(reason) = hybrid_scan_reason {
        return reason;
    }
    match requested_tier {
        Some(SearchTier::Exact) => return ScanReason::ExplicitTier(ExplicitScanTier::Exact),
        Some(SearchTier::Scan) => return ScanReason::ExplicitTier(ExplicitScanTier::Scan),
        Some(SearchTier::Auto | SearchTier::Graph(_)) | None => {}
    }
    let rows = segment.meta().row_count;
    let min_rows = crate::tier::thresholds::for_bucket(segment.meta().dims, segment.meta().scheme)
        .graph_min_rows;
    if rows < min_rows {
        ScanReason::BelowGraphThreshold { rows, min_rows }
    } else {
        ScanReason::GraphPending
    }
}

fn snapshot_has_graph(snapshot: &PublishedSnapshot) -> bool {
    snapshot.segments().iter().any(|segment| {
        segment
            .directory()
            .iter()
            .any(|entry| entry.kind == crate::segment::layout::RegionKind::GraphNodeBlocks.id())
    })
}

fn exact_vector_ceiling(
    snapshot: &PublishedSnapshot,
    active: &crate::ingest::ActiveSegment,
    query: &[f32],
) -> Result<f64, QueryError> {
    fold_exact_vector_ceiling(
        snapshot,
        active,
        query,
        remembered_segment_vector_ceiling_norm_range,
    )
}

/// Returns a sealed segment's norm enclosure, walking its region only once.
///
/// The enclosure is the query-independent half of the ceiling: it depends on
/// the segment's immutable factor or rescore bytes alone. Every tier and every
/// hybrid widening round would otherwise rewalk every factor record of every
/// segment to reach the same two numbers.
fn remembered_segment_vector_ceiling_norm_range(
    segment: &crate::segment::reader::SegmentReader,
) -> Result<crate::graph::search::GraphSegmentNormRange, QueryError> {
    if let Some(range) = segment.cached_vector_ceiling_norm_range() {
        return Ok(range);
    }
    let range = compute_segment_vector_ceiling_norm_range(segment)?;
    segment.remember_vector_ceiling_norm_range(range);
    Ok(range)
}

/// Folds every segment's norm enclosure into the query-wide ceiling.
///
/// `segment_range` supplies one sealed segment's query-independent enclosure.
/// Production passes the remembering source; the differential test passes the
/// recomputing one, so both walk identical fold arithmetic.
fn fold_exact_vector_ceiling(
    snapshot: &PublishedSnapshot,
    active: &crate::ingest::ActiveSegment,
    query: &[f32],
    mut segment_range: impl FnMut(
        &crate::segment::reader::SegmentReader,
    )
        -> Result<crate::graph::search::GraphSegmentNormRange, QueryError>,
) -> Result<f64, QueryError> {
    let mut ceiling = 0.0_f64;
    if !active.is_empty() {
        // The active segment is the one mutable row source, so its enclosure
        // is recomputed on every query and is never remembered.
        ceiling = fold_vector_ceiling(
            ceiling,
            crate::graph::search::GraphSegmentNormRange::from_factors(active.factors()),
            query,
        )?;
    }
    for segment in snapshot.segments() {
        let range = segment_range(segment)?;
        ceiling = fold_vector_ceiling(ceiling, range, query)?;
    }
    Ok(ceiling)
}

/// Walks one sealed segment's factor or rescore region for its norm enclosure.
fn compute_segment_vector_ceiling_norm_range(
    segment: &crate::segment::reader::SegmentReader,
) -> Result<crate::graph::search::GraphSegmentNormRange, QueryError> {
    match segment.meta().scheme {
        4 => Ok(crate::graph::search::GraphSegmentNormRange::from_factors(
            segment
                .query_bit4_factors()
                .map_err(StoreError::Segment)
                .map_err(QueryError::Store)?,
        )),
        0 | 2 => Ok(
            crate::graph::search::GraphSegmentNormRange::from_exact_rows(
                segment
                    .query_rescore_f32()
                    .map_err(StoreError::Segment)
                    .map_err(QueryError::Store)?,
                segment.meta().dims as usize,
            ),
        ),
        scheme => Err(QueryError::Store(StoreError::Segment(
            crate::segment::SegmentError::Geometry(format!(
                "vector norm enclosure does not support scheme {scheme}"
            )),
        ))),
    }
}

fn fold_vector_ceiling(
    current: f64,
    range: crate::graph::search::GraphSegmentNormRange,
    query: &[f32],
) -> Result<f64, QueryError> {
    let ceiling = range.squared_l2_upper_bound(query);
    if !ceiling.is_finite() {
        return Err(QueryError::Store(StoreError::Segment(
            crate::segment::SegmentError::Geometry(
                "vector norm enclosure did not produce a finite squared-L2 ceiling".to_owned(),
            ),
        )));
    }
    Ok(current.max(ceiling))
}

/// Folds one segment's exhaustive worst score into the query-wide farthest
/// distance. A scan that returned candidates without a worst score did not
/// visit every allowed row, so the query-wide anchor is not available.
fn fold_worst_squared_l2(
    worst: &mut Option<f64>,
    exhaustive: &mut bool,
    outcome: &crate::scan::ScanOutcome,
) {
    match outcome.worst_score {
        Some(score) => {
            let squared_l2 = -f64::from(score);
            *worst = Some(worst.map_or(squared_l2, |current: f64| current.max(squared_l2)));
        }
        None => {
            if !outcome.candidates.is_empty() {
                *exhaustive = false;
            }
        }
    }
}

// Boxing the traversed variant would add an allocation to every graph segment.
#[allow(clippy::large_enum_variant)]
enum SegmentGraphResult<'a> {
    Pruned {
        plan: crate::planner::SegmentPlan,
        graph_stats: crate::ingest::GraphSearchStats,
        _lease: SnapshotLease,
    },
    Traversed {
        result: crate::graph::search::GraphSearchResult,
        plan: crate::planner::SegmentPlan,
        dims_touched: u64,
        bytes_read: u64,
        caller_thread: std::thread::ThreadId,
        graph_stats: crate::ingest::GraphSearchStats,
        _scratch: graph_cache::GraphScratchLease<'a>,
        _lease: SnapshotLease,
    },
}

struct MergedSegmentGraph<'a> {
    plan: crate::planner::SegmentPlan,
    traversed: bool,
    _result: Option<crate::graph::search::GraphSearchResult>,
    _scratch: Option<graph_cache::GraphScratchLease<'a>>,
    _lease: SnapshotLease,
}

impl<'a> SegmentGraphResult<'a> {
    #[allow(clippy::too_many_arguments)]
    fn merge_into(
        self,
        segment: &crate::segment::reader::SegmentReader,
        alive: &crate::meta::AliveSet,
        source: crate::ingest::RowSource,
        candidates: &mut Vec<crate::ingest::SearchCandidate>,
        dims_touched: &mut u64,
        bytes_read: &mut u64,
        worker_thread_ids: &mut Vec<std::thread::ThreadId>,
        graph_stats: &mut crate::ingest::GraphSearchStats,
        #[cfg(any(test, feature = "test-support"))] vector_fault_controller: Option<
            &crate::scan::vector_fault::VectorFaultController,
        >,
        tier: SearchTier,
    ) -> Result<MergedSegmentGraph<'a>, QueryError> {
        match self {
            Self::Pruned {
                plan,
                graph_stats: delta,
                _lease,
            } => {
                add_traversal_stats(graph_stats, delta)?;
                Ok(MergedSegmentGraph {
                    plan,
                    traversed: false,
                    _result: None,
                    _scratch: None,
                    _lease,
                })
            }
            Self::Traversed {
                result,
                plan,
                dims_touched: traversal_dims_touched,
                bytes_read: traversal_bytes_read,
                caller_thread,
                graph_stats: delta,
                _scratch,
                _lease,
            } => {
                *dims_touched = dims_touched
                    .checked_add(traversal_dims_touched)
                    .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
                *bytes_read = bytes_read
                    .checked_add(traversal_bytes_read)
                    .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
                if !worker_thread_ids.contains(&caller_thread) {
                    worker_thread_ids.push(caller_thread);
                }
                add_traversal_stats(graph_stats, delta)?;
                reserve_global_candidates(
                    candidates,
                    result
                        .candidates()
                        .iter()
                        .filter(|candidate| alive.is_alive(candidate.row_id()))
                        .count(),
                    #[cfg(any(test, feature = "test-support"))]
                    vector_fault_controller,
                    #[cfg(any(test, feature = "test-support"))]
                    vector_fault_tier(tier),
                )?;
                for candidate in result.candidates() {
                    if !alive.is_alive(candidate.row_id()) {
                        continue;
                    }
                    let score = -(candidate.distance() as f32);
                    if !score.is_finite() {
                        return Err(QueryError::Graph(
                            crate::graph::search::GraphSearchError::Geometry(format!(
                                "graph result distance for row {} is not representable as f32",
                                candidate.row_id()
                            )),
                        ));
                    }
                    candidates.push(crate::ingest::SearchCandidate::new(
                        crate::ingest::GlobalRowId::new(source, candidate.row_id()),
                        segment
                            .document_version(candidate.row_id() as usize)
                            .map_err(StoreError::Segment)
                            .map_err(QueryError::Store)?,
                        score,
                        true,
                    ));
                }
                Ok(MergedSegmentGraph {
                    plan,
                    traversed: true,
                    _result: Some(result),
                    _scratch: Some(_scratch),
                    _lease,
                })
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn traverse_segment_graph<'a>(
    segment: &'a crate::segment::reader::SegmentReader,
    alive: &crate::meta::AliveSet,
    source: crate::ingest::RowSource,
    graph_options: GraphSearchOptions,
    graph_bound_mode: GraphBoundMode,
    request: crate::ingest::SearchRequest<'_>,
    k: usize,
    accounting: &Arc<stats::Accounting>,
    snapshot: &Arc<PublishedSnapshot>,
    generation: u64,
    control: &QueryControl,
    competitive_distance: Option<f32>,
    #[cfg(any(test, feature = "test-support"))] vector_fault_controller: Option<
        &crate::scan::vector_fault::VectorFaultController,
    >,
    tier: SearchTier,
) -> Result<SegmentGraphResult<'a>, QueryError> {
    let lease = SnapshotLease::new_at(Arc::clone(snapshot), generation);
    let cancellation = QueryCancellation::new(control, &lease);
    cancellation.check_graph().map_err(map_scan_error)?;
    let (graph, graph_validated, prepared_entry_seed_discovered, norm_range) =
        match graph_bound_mode {
            GraphBoundMode::Shared => {
                let prepared = segment
                    .graph_search_cache
                    .prepare_shared(segment, &cancellation)
                    .map_err(map_graph_cache_error)?;
                (
                    prepared.graph,
                    prepared.graph_validated,
                    prepared.entry_seed_discovered,
                    Some(prepared.norm_range),
                )
            }
            #[cfg(test)]
            GraphBoundMode::Independent => {
                let (graph, graph_validated) = segment
                    .graph_search_cache
                    .bind_graph(segment)
                    .map_err(map_graph_cache_error)?;
                (graph, graph_validated, false, None)
            }
        };
    let rescore = query_rescore_rows_for_search(
        segment,
        #[cfg(any(test, feature = "test-support"))]
        vector_fault_controller,
        #[cfg(any(test, feature = "test-support"))]
        vector_fault_tier(tier),
    )?;
    let node_count = graph.node_count() as usize;
    let segment_k = k.min(node_count);
    let has_tombstones = alive.tombstone_count() != 0;
    let target_live_k = if has_tombstones {
        usize::try_from(alive.live_count())
            .map_err(|_| QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?
            .min(k)
    } else {
        segment_k
    };
    let maximum_candidate_k = if has_tombstones {
        let tombstone_count = usize::try_from(alive.tombstone_count())
            .map_err(|_| QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
        target_live_k
            .checked_add(tombstone_count)
            .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?
            .min(node_count)
            .max(segment_k)
    } else {
        segment_k
    };
    if let Some(ef) = graph_options.ef()
        && ef < segment_k
    {
        return Err(QueryError::Graph(
            crate::graph::search::GraphSearchError::AdaptiveEf(
                crate::graph::search::AdaptiveEfError::ExplicitBelowK { k: segment_k, ef },
            ),
        ));
    }
    let request_for = |candidate_k| {
        let graph_request = crate::graph::search::GraphSearchRequest::new(
            request.vector(),
            candidate_k,
            graph_options.seed(),
        )
        .with_profile(graph_options.profile());
        // An explicit ef controls the caller's live-candidate window.
        // Tombstones are engine state, so reserve only the additional
        // width needed to replace deleted rows instead of turning a
        // previously valid ef=k request into an internal below-k error.
        graph_options.ef().map_or(graph_request, |ef| {
            graph_request.with_ef(ef.max(candidate_k))
        })
    };
    let mut candidate_k = segment_k;
    let mut graph_request = request_for(candidate_k);
    let effective_ef = graph_request
        .effective_ef(graph.node_count() as usize)
        .map_err(crate::graph::search::GraphSearchError::AdaptiveEf)
        .map_err(QueryError::Graph)?;
    let requestable_max_k = maximum_candidate_k;
    let scratch_ef = if has_tombstones && requestable_max_k > candidate_k {
        request_for(requestable_max_k)
            .effective_ef(node_count)
            .map_err(crate::graph::search::GraphSearchError::AdaptiveEf)
            .map_err(QueryError::Graph)?
    } else {
        effective_ef
    };
    if let (Some(norm_range), Some(competitive_distance)) = (norm_range, competitive_distance)
        && norm_range.squared_l2_upper_bound(request.vector()) <= f64::from(f32::MAX)
        && (norm_range.squared_l2_lower_bound(request.vector()) as f32) > competitive_distance
    {
        cancellation.check_graph().map_err(map_scan_error)?;
        let graph_stats = crate::ingest::GraphSearchStats {
            graph_validations: usize::from(graph_validated),
            entry_seed_discoveries: usize::from(prepared_entry_seed_discovered),
            segments_pruned_by_bound: 1,
            ..crate::ingest::GraphSearchStats::default()
        };
        let plan = crate::planner::SegmentPlan::unfiltered_pruned(source, alive.live_count());
        return Ok(SegmentGraphResult::Pruned {
            plan,
            graph_stats,
            _lease: lease,
        });
    }
    let mut scratch = segment
        .graph_search_cache
        .checkout(graph, scratch_ef, accounting, &cancellation)
        .map_err(map_graph_cache_error)?;
    let entry_seed_discovered = prepared_entry_seed_discovered || scratch.entry_seed_discovered();
    let entries = scratch.entries();
    let mut searcher = crate::graph::search::GraphSearcher::with_entry_row_ids(
        graph,
        rescore,
        entries,
        scratch.scratch_mut().map_err(map_graph_error)?,
    )
    .map_err(map_graph_error)?
    .with_rescore_validator(segment);
    #[cfg(any(test, feature = "test-support"))]
    if let Some(after_hops) = graph_options.cancel_after_hops() {
        let token = cancellation.cancel_token_for_test().ok_or_else(|| {
            QueryError::Graph(crate::graph::search::GraphSearchError::Geometry(
                "test hop cancellation requires a Cancel query control".to_owned(),
            ))
        })?;
        let _observed_hops = searcher.cancel_after_hops(after_hops, token);
    }
    #[cfg(any(test, feature = "test-support"))]
    if let Some(controller) = vector_fault_controller {
        searcher = searcher.with_vector_fault_controller(
            controller,
            vector_fault_source(source),
            crate::scan::vector_fault::VectorSearchTier::Graph,
        );
    }
    let mut traversal_dims_touched = 0_u64;
    let mut traversal_bytes_read = 0_u64;
    let mut traversal_epoch_clears = 0_usize;
    let mut traversal_candidates_scored = 0_usize;
    let mut traversal_candidates_rescored = 0_usize;
    let result = loop {
        let result = searcher
            .search(graph_request, Some(&cancellation))
            .map_err(map_graph_error)?;
        let counters = result.counters();
        traversal_dims_touched = traversal_dims_touched
            .checked_add(counters.dims_touched())
            .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
        traversal_bytes_read = traversal_bytes_read
            .checked_add(counters.bytes_read())
            .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
        traversal_epoch_clears = traversal_epoch_clears
            .checked_add(usize::from(counters.visited_epoch_cleared()))
            .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
        traversal_candidates_scored = traversal_candidates_scored
            .checked_add(counters.candidates_scored())
            .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
        traversal_candidates_rescored = traversal_candidates_rescored
            .checked_add(counters.candidates_rescored())
            .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;

        if !has_tombstones {
            break result;
        }
        let retained_live = result
            .candidates()
            .iter()
            .filter(|candidate| alive.is_alive(candidate.row_id()))
            .count();
        if retained_live >= target_live_k {
            break result;
        }
        if candidate_k >= requestable_max_k {
            return Err(QueryError::Graph(
                crate::graph::search::GraphSearchError::Geometry(format!(
                    "graph traversal exhausted {candidate_k} candidates but retained {retained_live} live rows, expected {target_live_k}"
                )),
            ));
        }
        let widened_k = candidate_k
            .saturating_mul(2)
            .max(candidate_k.saturating_add(1))
            .min(requestable_max_k);
        if widened_k <= candidate_k {
            return Err(QueryError::Graph(
                crate::graph::search::GraphSearchError::Geometry(format!(
                    "graph traversal could not widen beyond {candidate_k} candidates"
                )),
            ));
        }
        candidate_k = widened_k;
        graph_request = request_for(candidate_k);
    };
    let graph_stats = crate::ingest::GraphSearchStats {
        segments_traversed: 1,
        graph_validations: usize::from(graph_validated),
        entry_seed_discoveries: usize::from(entry_seed_discovered),
        visited_epoch_clears: traversal_epoch_clears,
        candidates_scored: traversal_candidates_scored,
        candidates_rescored: traversal_candidates_rescored,
        segments_pruned_by_bound: 0,
    };
    let plan = crate::planner::SegmentPlan::unfiltered_graph(
        source,
        alive.live_count(),
        graph_options.ef(),
        effective_ef,
        graph_options.profile(),
    );
    let caller_thread = std::thread::current().id();
    drop(searcher);
    Ok(SegmentGraphResult::Traversed {
        result,
        plan,
        dims_touched: traversal_dims_touched,
        bytes_read: traversal_bytes_read,
        caller_thread,
        graph_stats,
        _scratch: scratch,
        _lease: lease,
    })
}

fn add_traversal_stats(
    stats: &mut crate::ingest::GraphSearchStats,
    delta: crate::ingest::GraphSearchStats,
) -> Result<(), QueryError> {
    stats.segments_traversed = stats
        .segments_traversed
        .checked_add(delta.segments_traversed)
        .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    stats.graph_validations = stats
        .graph_validations
        .checked_add(delta.graph_validations)
        .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    stats.entry_seed_discoveries = stats
        .entry_seed_discoveries
        .checked_add(delta.entry_seed_discoveries)
        .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    stats.visited_epoch_clears = stats
        .visited_epoch_clears
        .checked_add(delta.visited_epoch_clears)
        .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    stats.candidates_scored = stats
        .candidates_scored
        .checked_add(delta.candidates_scored)
        .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    stats.candidates_rescored = stats
        .candidates_rescored
        .checked_add(delta.candidates_rescored)
        .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    stats.segments_pruned_by_bound = stats
        .segments_pruned_by_bound
        .checked_add(delta.segments_pruned_by_bound)
        .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn scan_sealed_segment(
    pool: Option<&pool::QueryPool>,
    snapshot: &Arc<PublishedSnapshot>,
    segment: &crate::segment::reader::SegmentReader,
    alive: &crate::meta::AliveSet,
    generation: u64,
    request: crate::ingest::SearchRequest<'_>,
    k: usize,
    scan_options: crate::scan::ScanOptions,
    control: &QueryControl,
    bit4_query: &std::cell::OnceCell<Result<crate::quant::Bit4Query, crate::quant::QuantError>>,
    int8_query: &std::cell::OnceCell<Result<crate::quant::Int8Query, crate::quant::QuantError>>,
    full_precision: bool,
    source: crate::ingest::RowSource,
    tier: SearchTier,
    #[cfg(any(test, feature = "test-support"))] vector_fault_controller: Option<
        &crate::scan::vector_fault::VectorFaultController,
    >,
) -> Result<crate::scan::ScanOutcome, QueryError> {
    use crate::scan::{ScanQuery, ScanRequest, ScanRows};

    if full_precision {
        let lease = SnapshotLease::new_at(Arc::clone(snapshot), generation);
        let cancellation = QueryCancellation::new(control, &lease);
        let vectors = exact_rescore_rows_for_search(
            segment,
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_controller,
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_tier(tier),
        )?;
        return scan_squared_l2(
            vectors,
            segment.meta().row_count as usize,
            alive,
            request.vector(),
            k,
            &cancellation,
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_controller,
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_source(source),
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_tier(tier),
        );
    }

    let query_pool = pool.ok_or(QueryError::Store(StoreError::Synchronization {
        component: "sealed scan-tier query pool",
    }))?;
    match segment.meta().scheme {
        0 => execute_store_scan(
            query_pool,
            ScanRequest {
                query: ScanQuery::F32(request.vector()),
                rows: ScanRows::F32BorrowedRowMajor(
                    segment
                        .query_f32_codes()
                        .map_err(StoreError::Segment)
                        .map_err(QueryError::Store)?,
                ),
                row_mask: Some(alive.scan_mask()),
            },
            k,
            scan_options,
            control.clone(),
            SnapshotLease::new_at(Arc::clone(snapshot), generation),
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_controller,
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_source(source),
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_tier(tier),
        ),
        4 => execute_store_scan(
            query_pool,
            ScanRequest {
                query: ScanQuery::Bit4(
                    bit4_query
                        .get_or_init(|| crate::quant::prepare_bit4_query(request.vector(), 0))
                        .as_ref()
                        .map_err(|error| crate::scan::ScanError::Quant(error.clone()))
                        .map_err(QueryError::Scan)?,
                ),
                rows: ScanRows::Bit4RowMajor {
                    codes: segment
                        .query_bit4_codes()
                        .map_err(StoreError::Segment)
                        .map_err(QueryError::Store)?,
                    factors: segment
                        .query_bit4_factors()
                        .map_err(StoreError::Segment)
                        .map_err(QueryError::Store)?,
                },
                row_mask: Some(alive.scan_mask()),
            },
            k,
            scan_options,
            control.clone(),
            SnapshotLease::new_at(Arc::clone(snapshot), generation),
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_controller,
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_source(source),
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_tier(tier),
        ),
        2 => {
            let factors = segment.query_int8_factors().map_err(QueryError::Store)?;
            execute_store_scan(
                query_pool,
                ScanRequest {
                    query: ScanQuery::Int8(
                        int8_query
                            .get_or_init(|| crate::quant::prepare_int8_query(request.vector()))
                            .as_ref()
                            .map_err(|error| crate::scan::ScanError::Quant(error.clone()))
                            .map_err(QueryError::Scan)?,
                    ),
                    rows: ScanRows::Int8RowMajor {
                        codes: segment
                            .query_int8_codes()
                            .map_err(StoreError::Segment)
                            .map_err(QueryError::Store)?,
                        factors: factors.as_slice(),
                    },
                    row_mask: Some(alive.scan_mask()),
                },
                k,
                scan_options,
                control.clone(),
                SnapshotLease::new_at(Arc::clone(snapshot), generation),
                #[cfg(any(test, feature = "test-support"))]
                vector_fault_controller,
                #[cfg(any(test, feature = "test-support"))]
                vector_fault_source(source),
                #[cfg(any(test, feature = "test-support"))]
                vector_fault_tier(tier),
            )
        }
        scheme => Err(QueryError::Store(StoreError::Segment(
            crate::segment::SegmentError::Geometry(format!(
                "store search does not support sealed scheme {scheme}"
            )),
        ))),
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_store_scan(
    pool: &pool::QueryPool,
    request: crate::scan::ScanRequest<'_>,
    k: usize,
    options: crate::scan::ScanOptions,
    control: QueryControl,
    lease: SnapshotLease,
    #[cfg(any(test, feature = "test-support"))] controller: Option<
        &crate::scan::vector_fault::VectorFaultController,
    >,
    #[cfg(any(test, feature = "test-support"))] source: crate::scan::vector_fault::VectorRowSource,
    #[cfg(any(test, feature = "test-support"))] tier: crate::scan::vector_fault::VectorSearchTier,
) -> Result<crate::scan::ScanOutcome, QueryError> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(controller) = controller {
        let geometry = crate::scan::scan_geometry(request).map_err(QueryError::Scan)?;
        let cancellation = QueryCancellation::new(&control, &lease);
        let partition = crate::scan::scan_partition_with_vector_faults(
            request,
            k,
            0..geometry.row_count,
            Some(&cancellation),
            controller,
            source,
            tier,
        )
        .map_err(map_scan_error)?;
        return Ok(crate::scan::ScanOutcome {
            candidates: partition.candidates,
            worst_score: None,
            stats: crate::scan::ScanStats {
                dims_touched: partition.dims_touched,
                bytes_read: partition.bytes_read,
                threads_used: 1,
                worker_thread_ids: vec![partition.worker_thread_id],
            },
        });
    }
    pool.execute(request, k, options, control, lease)
}

#[cfg(any(test, feature = "test-support"))]
fn vector_fault_tier(tier: SearchTier) -> crate::scan::vector_fault::VectorSearchTier {
    match tier {
        SearchTier::Exact => crate::scan::vector_fault::VectorSearchTier::Exact,
        SearchTier::Scan => crate::scan::vector_fault::VectorSearchTier::Scan,
        SearchTier::Graph(_) => crate::scan::vector_fault::VectorSearchTier::Graph,
        SearchTier::Auto => crate::scan::vector_fault::VectorSearchTier::Auto,
    }
}

#[cfg(any(test, feature = "test-support"))]
fn vector_fault_source(
    source: crate::ingest::RowSource,
) -> crate::scan::vector_fault::VectorRowSource {
    match source {
        crate::ingest::RowSource::Active => crate::scan::vector_fault::VectorRowSource::Active,
        crate::ingest::RowSource::Sealed(segment) => {
            crate::scan::vector_fault::VectorRowSource::Sealed(*segment.as_bytes())
        }
    }
}

pub(crate) fn auto_uses_full_precision(
    snapshot: &PublishedSnapshot,
    active: &crate::ingest::ActiveSegment,
) -> bool {
    let mut has_exact = false;
    let mut has_estimated = !active.is_empty();
    for segment in snapshot.segments() {
        if segment
            .directory()
            .iter()
            .any(|entry| entry.kind == crate::segment::layout::RegionKind::GraphNodeBlocks.id())
        {
            return true;
        }
        if segment.meta().row_count == 0 {
            continue;
        }
        match segment.meta().scheme {
            0 => has_exact = true,
            2 | 4 => has_estimated = true,
            _ => {}
        }
    }
    has_exact && has_estimated
}

/// Returns the f32 rescore rows for a query-time exact score.
///
/// This is the query path, so it uses the validate-once accessor. The
/// verifying `rescore_f32` re-hashes the whole rescore region on every
/// call, and hybrid cross-fill calls this once per cross-filled
/// document: 50 calls over a 181 MB region cost about 219 ms of a
/// 221 ms hybrid query on 58,980 FiQA rows, while both legs together
/// cost under 3 ms. Uncached `rescore_f32` remains for the validation
/// and diagnostic callers.
pub(crate) fn exact_rescore_rows(
    segment: &crate::segment::reader::SegmentReader,
) -> Result<&[f32], QueryError> {
    segment
        .query_rescore_f32()
        .map_err(|source| QueryError::Store(StoreError::Segment(source)))
}

fn exact_rescore_rows_for_search<'a>(
    segment: &'a crate::segment::reader::SegmentReader,
    #[cfg(any(test, feature = "test-support"))] controller: Option<
        &crate::scan::vector_fault::VectorFaultController,
    >,
    #[cfg(any(test, feature = "test-support"))] tier: crate::scan::vector_fault::VectorSearchTier,
) -> Result<&'a [f32], QueryError> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(available_rows) = controller.and_then(|controller| {
        controller.missing_rescore_rows(
            crate::scan::vector_fault::VectorRowSource::Sealed(*segment.meta().id.as_bytes()),
            tier,
            crate::scan::vector_fault::MissingRescoreSite::ExactRescoreRows,
            segment.meta().row_count,
        )
    }) {
        return Err(missing_rescore_rows_error(segment, available_rows));
    }
    exact_rescore_rows(segment)
}

pub(crate) fn query_rescore_rows(
    segment: &crate::segment::reader::SegmentReader,
) -> Result<&[f32], QueryError> {
    segment
        .query_rescore_f32()
        .map_err(|source| QueryError::Store(StoreError::Segment(source)))
}

fn query_rescore_rows_for_search<'a>(
    segment: &'a crate::segment::reader::SegmentReader,
    #[cfg(any(test, feature = "test-support"))] controller: Option<
        &crate::scan::vector_fault::VectorFaultController,
    >,
    #[cfg(any(test, feature = "test-support"))] tier: crate::scan::vector_fault::VectorSearchTier,
) -> Result<&'a [f32], QueryError> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(available_rows) = controller.and_then(|controller| {
        controller.missing_rescore_rows(
            crate::scan::vector_fault::VectorRowSource::Sealed(*segment.meta().id.as_bytes()),
            tier,
            crate::scan::vector_fault::MissingRescoreSite::QueryRescoreRows,
            segment.meta().row_count,
        )
    }) {
        return Err(missing_rescore_rows_error(segment, available_rows));
    }
    query_rescore_rows(segment)
}

#[cfg(any(test, feature = "test-support"))]
fn missing_rescore_rows_error(
    segment: &crate::segment::reader::SegmentReader,
    available_rows: u32,
) -> QueryError {
    QueryError::Store(StoreError::Segment(crate::segment::SegmentError::Geometry(
        format!(
            "exact scores unavailable for segment {}: expected {} rows, got {available_rows}",
            segment.meta().id,
            segment.meta().row_count,
        ),
    )))
}

fn retain_global_top_k(candidates: &mut Vec<crate::ingest::SearchCandidate>, k: usize) {
    candidates.sort_unstable_by(crate::ingest::compare_search_candidates);
    candidates.truncate(k);
}

fn global_competitive_distance(
    candidates: &[crate::ingest::SearchCandidate],
    k: usize,
) -> Option<f32> {
    if k == 0 || candidates.len() < k {
        return None;
    }
    candidates
        .get(k.saturating_sub(1))
        .map(|candidate| -candidate.score())
        .filter(|distance| distance.is_finite() && *distance >= 0.0)
}

fn map_graph_cache_error(error: graph_cache::GraphCacheError) -> QueryError {
    match error {
        graph_cache::GraphCacheError::Store(error) => QueryError::Store(error),
        graph_cache::GraphCacheError::Search(error) => map_graph_error(error),
    }
}

fn map_graph_error(error: crate::graph::search::GraphSearchError) -> QueryError {
    match error {
        crate::graph::search::GraphSearchError::ExactRescoreUnavailable(detail) => {
            QueryError::Store(StoreError::Segment(crate::segment::SegmentError::Geometry(
                detail,
            )))
        }
        crate::graph::search::GraphSearchError::Cancelled { partial } => {
            QueryError::Cancelled { partial }
        }
        crate::graph::search::GraphSearchError::Timeout { partial } => {
            QueryError::Timeout { partial }
        }
        crate::graph::search::GraphSearchError::ReadCancelled { partial } => {
            QueryError::ReadCancelled { partial }
        }
        crate::graph::search::GraphSearchError::Scan(error) => QueryError::Scan(error),
        error => QueryError::Graph(error),
    }
}

fn scan_active_squared_l2(
    active: &crate::ingest::ActiveSegment,
    alive: &crate::meta::AliveSet,
    query: &[f32],
    k: usize,
    cancellation: &QueryCancellation<'_>,
    #[cfg(any(test, feature = "test-support"))] controller: Option<
        &crate::scan::vector_fault::VectorFaultController,
    >,
    #[cfg(any(test, feature = "test-support"))] tier: crate::scan::vector_fault::VectorSearchTier,
) -> Result<crate::scan::ScanOutcome, QueryError> {
    scan_squared_l2(
        active.vectors(),
        active.row_count(),
        alive,
        query,
        k,
        cancellation,
        #[cfg(any(test, feature = "test-support"))]
        controller,
        #[cfg(any(test, feature = "test-support"))]
        crate::scan::vector_fault::VectorRowSource::Active,
        #[cfg(any(test, feature = "test-support"))]
        tier,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "test-support vector faults require the actual source and tier at the scoring boundary"
)]
fn scan_squared_l2(
    vectors: &[f32],
    row_count: usize,
    alive: &crate::meta::AliveSet,
    query: &[f32],
    k: usize,
    cancellation: &QueryCancellation<'_>,
    #[cfg(any(test, feature = "test-support"))] controller: Option<
        &crate::scan::vector_fault::VectorFaultController,
    >,
    #[cfg(any(test, feature = "test-support"))] source: crate::scan::vector_fault::VectorRowSource,
    #[cfg(any(test, feature = "test-support"))] tier: crate::scan::vector_fault::VectorSearchTier,
) -> Result<crate::scan::ScanOutcome, QueryError> {
    if query.is_empty() {
        return Err(QueryError::Scan(crate::scan::ScanError::ZeroDimension));
    }
    let expected = row_count
        .checked_mul(query.len())
        .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    if vectors.len() != expected {
        return Err(QueryError::Scan(crate::scan::ScanError::RowDataLength {
            dimension: query.len(),
            actual: vectors.len(),
        }));
    }
    let mut row_indices = Vec::new();
    for row in 0..row_count {
        if row.is_multiple_of(64) {
            cancellation.check_graph().map_err(map_scan_error)?;
        }
        let local_row = u32::try_from(row)
            .map_err(|_| QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
        if !alive.is_alive(local_row) {
            continue;
        }
        row_indices.push(local_row);
    }
    cancellation.check_graph().map_err(map_scan_error)?;
    if row_indices.is_empty() {
        return Ok(crate::scan::ScanOutcome {
            candidates: Vec::new(),
            worst_score: None,
            stats: crate::scan::ScanStats {
                dims_touched: 0,
                bytes_read: 0,
                threads_used: 1,
                worker_thread_ids: vec![std::thread::current().id()],
            },
        });
    }

    let coarse_scores = vec![0.0_f32; row_indices.len()];
    let pool = crate::quant::RescorePool::retained(
        &row_indices,
        &coarse_scores,
        crate::quant::RescoreMetric::SquaredL2,
        0,
        0,
    )
    .with_prefetch(true);
    let rescored = crate::quant::rescore_top_k_with_check(
        query,
        vectors,
        query.len(),
        pool,
        row_indices.len(),
        |row, is_checkpoint| {
            if is_checkpoint {
                cancellation.check_graph().map_err(map_scan_error)?;
            }
            #[cfg(any(test, feature = "test-support"))]
            if controller.is_some_and(|controller| controller.after_eligible_row(source, tier, row))
            {
                return Err(QueryError::Cancelled { partial: false });
            }
            Ok(())
        },
    )
    .map_err(|error| match error {
        crate::quant::RescoreCheckError::Rescore(error) => map_l2_rescore_error(error),
        crate::quant::RescoreCheckError::Check(error) => error,
    })?;
    cancellation.check_graph().map_err(map_scan_error)?;

    let mut candidates = Vec::with_capacity(rescored.hits.len());
    for hit in rescored.hits {
        let score = hit.score as f32;
        if !score.is_finite() {
            return Err(QueryError::Scan(crate::scan::ScanError::NonFiniteScore {
                row_id: hit.row_index,
            }));
        }
        candidates.push(crate::scan::ScanCandidate {
            row_id: hit.row_index,
            score,
        });
    }
    candidates.sort_unstable_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.row_id.cmp(&right.row_id))
    });
    // Every alive row was scored exactly, so the tail of the complete order
    // is the farthest row: the hybrid normalization anchor, taken before the
    // top-k cut throws it away.
    let worst_score = candidates.last().map(|candidate| candidate.score);
    crate::scan::truncate_to_k_with_score_ties(&mut candidates, k, |candidate| candidate.score);
    let dims = u64::try_from(query.len())
        .map_err(|_| QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    let scored_rows = u64::try_from(rescored.candidates_rescored)
        .map_err(|_| QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    let dims_touched = scored_rows
        .checked_mul(dims)
        .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    let bytes_read = dims_touched
        .checked_mul(std::mem::size_of::<f32>() as u64)
        .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    Ok(crate::scan::ScanOutcome {
        candidates,
        worst_score,
        stats: crate::scan::ScanStats {
            dims_touched,
            bytes_read,
            threads_used: 1,
            worker_thread_ids: vec![std::thread::current().id()],
        },
    })
}

fn map_l2_rescore_error(error: crate::quant::RescoreError) -> QueryError {
    let error = match error {
        crate::quant::RescoreError::ZeroDimension => crate::scan::ScanError::ZeroDimension,
        crate::quant::RescoreError::QueryDimension { expected, actual }
        | crate::quant::RescoreError::CoarseScoreCount { expected, actual }
        | crate::quant::RescoreError::CandidateRowCount { expected, actual } => {
            crate::scan::ScanError::RowDataLength {
                dimension: expected,
                actual,
            }
        }
        crate::quant::RescoreError::RowDataLength { dimension, actual } => {
            crate::scan::ScanError::RowDataLength { dimension, actual }
        }
        crate::quant::RescoreError::CandidateRowOutOfRange {
            row_index,
            row_count,
            ..
        } => crate::scan::ScanError::RowDataLength {
            dimension: row_count,
            actual: row_index,
        },
        crate::quant::RescoreError::NonFiniteCoarseScore { index }
        | crate::quant::RescoreError::NonFiniteExactScore { row_index: index } => {
            crate::scan::ScanError::NonFiniteScore { row_id: index }
        }
        crate::quant::RescoreError::ZeroK
        | crate::quant::RescoreError::ZeroOversample
        | crate::quant::RescoreError::InsufficientCandidates { .. }
        | crate::quant::RescoreError::ArithmeticOverflow => {
            crate::scan::ScanError::ArithmeticOverflow
        }
    };
    QueryError::Scan(error)
}

fn map_scan_error(error: crate::scan::ScanError) -> QueryError {
    match error {
        crate::scan::ScanError::Cancelled { partial } => QueryError::Cancelled { partial },
        crate::scan::ScanError::Timeout { partial } => QueryError::Timeout { partial },
        crate::scan::ScanError::ReadCancelled { partial } => QueryError::ReadCancelled { partial },
        error => QueryError::Scan(error),
    }
}

#[allow(clippy::too_many_arguments)]
fn merge_store_outcome(
    outcome: crate::scan::ScanOutcome,
    source: crate::ingest::RowSource,
    document: impl Fn(usize) -> Result<Option<crate::ingest::DocumentVersion>, QueryError>,
    candidates: &mut Vec<crate::ingest::SearchCandidate>,
    dims_touched: &mut u64,
    bytes_read: &mut u64,
    worker_thread_ids: &mut Vec<std::thread::ThreadId>,
    exact_score: bool,
    #[cfg(any(test, feature = "test-support"))] vector_fault_controller: Option<
        &crate::scan::vector_fault::VectorFaultController,
    >,
    #[cfg(any(test, feature = "test-support"))]
    vector_fault_tier: crate::scan::vector_fault::VectorSearchTier,
) -> Result<(), QueryError> {
    *dims_touched = dims_touched
        .checked_add(outcome.stats.dims_touched)
        .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    *bytes_read = bytes_read
        .checked_add(outcome.stats.bytes_read)
        .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    for worker in outcome.stats.worker_thread_ids {
        if !worker_thread_ids.contains(&worker) {
            worker_thread_ids.push(worker);
        }
    }
    reserve_global_candidates(
        candidates,
        outcome.candidates.len(),
        #[cfg(any(test, feature = "test-support"))]
        vector_fault_controller,
        #[cfg(any(test, feature = "test-support"))]
        vector_fault_tier,
    )?;
    for candidate in outcome.candidates {
        let local_row = u32::try_from(candidate.row_id)
            .map_err(|_| QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
        candidates.push(crate::ingest::SearchCandidate::new(
            crate::ingest::GlobalRowId::new(source, local_row),
            document(candidate.row_id)?,
            candidate.score,
            exact_score,
        ));
    }
    Ok(())
}

pub(crate) fn reserve_global_candidates(
    candidates: &mut Vec<crate::ingest::SearchCandidate>,
    additional: usize,
    #[cfg(any(test, feature = "test-support"))] vector_fault_controller: Option<
        &crate::scan::vector_fault::VectorFaultController,
    >,
    #[cfg(any(test, feature = "test-support"))]
    vector_fault_tier: crate::scan::vector_fault::VectorSearchTier,
) -> Result<(), QueryError> {
    if additional == 0 {
        return Ok(());
    }
    let items = u64::try_from(additional)
        .map_err(|_| QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    let candidate_bytes = u64::try_from(std::mem::size_of::<crate::ingest::SearchCandidate>())
        .map_err(|_| QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    let needed = items
        .checked_mul(candidate_bytes)
        .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    #[cfg(any(test, feature = "test-support"))]
    if vector_fault_controller.is_some_and(|controller| {
        controller.deny_global_candidate_allocation(vector_fault_tier, items, needed)
    }) {
        return Err(QueryError::Store(StoreError::AllocationFailed {
            needed,
            component: "vector search global candidates",
        }));
    }
    candidates.try_reserve_exact(additional).map_err(|_| {
        QueryError::Store(StoreError::AllocationFailed {
            needed,
            component: "vector search global candidates",
        })
    })
}

struct AdmittedLexicalQuery<'a> {
    generation: u64,
    active: Arc<crate::ingest::ActiveSegment>,
    snapshot: Arc<PublishedSnapshot>,
    active_query: ActiveQuery<'a>,
}

struct AdmittedVectorSearch<'a> {
    pool: Option<Arc<pool::QueryPool>>,
    snapshot: Arc<PublishedSnapshot>,
    active_segment: Arc<crate::ingest::ActiveSegment>,
    generation: u64,
    _active_query: ActiveQuery<'a>,
}

struct ActiveQuery<'a> {
    count: &'a AtomicU64,
}

impl Drop for ActiveQuery<'_> {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        self.close_best_effort();
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use std::error::Error;
    use std::sync::Arc;

    use tempfile::tempdir;

    use crate::lifecycle::durability::DurabilityPolicy;
    use crate::manifest::io::commit_manifest;
    use crate::manifest::{Manifest, ManifestError};
    use crate::meta::Schema;
    use crate::vfs::StdVfs;
    use crate::vfs::crash::MemoryVfs;

    use super::durability::{CommitTier, DurabilityMode, DurabilityPolicyError};
    use super::{
        CancelToken, GraphSearchOptions, ManualMonotonicClock, OpenOptions, QueryControl,
        SearchOptions, SearchTier, Store, StoreError, StoreTestDependencies,
        resolve_hybrid_leg_results,
    };

    /// Vectors whose norms differ enough that the ceiling is not degenerate.
    const CEILING_ROWS: [[f32; 4]; 3] = [
        [0.5, -0.25, 0.75, 0.125],
        [3.0, 1.5, -2.5, 0.25],
        [0.0, 0.0, 0.0, 0.0],
    ];

    /// Publishes a scheme-0 segment that no seal path can produce.
    fn publish_f32_segment(directory: &std::path::Path) {
        let created = Store::open(directory, OpenOptions::default()).expect("create f32 store");
        created.close().expect("close f32 store");

        let schema = Schema::timestamp_only();
        let mut columns = crate::meta::ColumnStoreBuilder::new(schema.clone());
        for row in 0..CEILING_ROWS.len() {
            columns
                .push_row(i64::try_from(row).expect("row timestamp"), &[])
                .expect("timestamp row");
        }
        let columns = columns.finish().expect("columns");
        let alive =
            crate::meta::AliveSet::new(u32::try_from(CEILING_ROWS.len()).expect("row count"));
        let rescore = CEILING_ROWS.iter().flatten().copied().collect::<Vec<f32>>();
        let codes = rescore
            .iter()
            .flat_map(|value| value.to_bits().to_le_bytes())
            .collect::<Vec<u8>>();
        let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
            .expect("f32 fixture durability");
        let meta = crate::segment::writer::write_segment(
            &StdVfs,
            directory,
            crate::segment::writer::SegmentBuild {
                id: crate::segment::SegmentId::new(11, [7; 10]),
                scheme: 0,
                dims: 4,
                codes: &codes,
                factors: crate::segment::writer::SegmentFactors::F32,
                rescore: &rescore,
                columns: &columns,
                alive: &alive,
            },
            policy,
        )
        .expect("write scheme-0 segment");
        commit_manifest(
            &StdVfs,
            directory,
            &Manifest {
                generation: 1,
                log_seq: 0,
                segments: vec![meta],
                epochs: Vec::new(),
                epoch_alias: None,
                schema,
            },
            policy,
        )
        .expect("publish scheme-0 segment");
    }

    /// Ingests `CEILING_ROWS`, sealing every row except an optional tail that
    /// stays in the mutable active segment.
    fn ingest_ceiling_rows(store: &Store, keep_active: usize, one_segment_per_row: bool) {
        use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};

        let sealed = CEILING_ROWS.len() - keep_active;
        for (index, row) in CEILING_ROWS.iter().enumerate() {
            let version = DocumentVersion::new(
                DocId::new(u128::try_from(index).expect("document id")),
                Revision::new(1),
            );
            store
                .ingest(IngestBatch::new(vec![IngestDocument::new(
                    version,
                    row.to_vec(),
                )]))
                .expect("ingest ceiling row");
            let seal_here = if one_segment_per_row {
                index < sealed
            } else {
                index + 1 == sealed
            };
            if seal_here {
                store.seal().expect("seal ceiling rows");
            }
        }
    }

    /// Proves the remembered per-segment enclosure reproduces the ceiling
    /// exactly, for one store, over several queries.
    fn assert_remembered_ceiling_is_exact(store: &Store, expected_segments: usize) {
        let queries: [Vec<f32>; 3] = [
            vec![0.0, 0.0, 0.0, 0.0],
            vec![1.0, -1.0, 0.5, 2.0],
            vec![7.5, 0.25, -3.5, 0.125],
        ];
        let admitted = store
            .admit_vector_search(SearchOptions::default())
            .expect("admit ceiling query");
        let snapshot = &admitted.snapshot;
        let active = &admitted.active_segment;
        assert_eq!(snapshot.segments().len(), expected_segments);
        for segment in snapshot.segments() {
            assert!(
                segment.cached_vector_ceiling_norm_range().is_none(),
                "a segment remembered an enclosure before any query walked it"
            );
        }

        for query in &queries {
            let cold = super::exact_vector_ceiling(snapshot, active, query).expect("cold ceiling");
            for segment in snapshot.segments() {
                let remembered = segment
                    .cached_vector_ceiling_norm_range()
                    .expect("a walked segment must remember its enclosure");
                let recomputed = super::compute_segment_vector_ceiling_norm_range(segment)
                    .expect("recomputed enclosure");
                assert_eq!(
                    remembered.endpoint_bits(),
                    recomputed.endpoint_bits(),
                    "remembered enclosure differs from the recomputed one"
                );
            }
            let warm = super::exact_vector_ceiling(snapshot, active, query).expect("warm ceiling");
            let recomputed = super::fold_exact_vector_ceiling(
                snapshot,
                active,
                query,
                super::compute_segment_vector_ceiling_norm_range,
            )
            .expect("recomputed ceiling");
            assert_eq!(
                cold.to_bits(),
                warm.to_bits(),
                "the remembered ceiling changed between queries"
            );
            assert_eq!(
                cold.to_bits(),
                recomputed.to_bits(),
                "the remembered ceiling is not the recomputed ceiling"
            );
        }
    }

    #[test]
    fn the_remembered_vector_ceiling_is_the_recomputed_ceiling_for_every_scheme() {
        let bit4_directory = tempdir().expect("Bit4 ceiling store directory");
        let bit4 = Store::open(bit4_directory.path(), OpenOptions::default())
            .expect("open Bit4 ceiling store");
        ingest_ceiling_rows(&bit4, 0, false);
        assert_remembered_ceiling_is_exact(&bit4, 1);
        bit4.close().expect("close Bit4 ceiling store");

        let split_directory = tempdir().expect("multi-segment ceiling store directory");
        let split = Store::open(split_directory.path(), OpenOptions::default())
            .expect("open multi-segment ceiling store");
        ingest_ceiling_rows(&split, 0, true);
        assert_remembered_ceiling_is_exact(&split, CEILING_ROWS.len());
        split.close().expect("close multi-segment ceiling store");

        let int8_directory = tempdir().expect("Int8 ceiling store directory");
        let int8 = Store::open_with_test_dependencies(
            int8_directory.path(),
            OpenOptions::default(),
            StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(super::SystemMonotonicClock))
                .with_vector_seal_scheme(crate::quant::QuantScheme::Int8),
        )
        .expect("open Int8 ceiling store");
        ingest_ceiling_rows(&int8, 0, false);
        assert_remembered_ceiling_is_exact(&int8, 1);
        int8.close().expect("close Int8 ceiling store");

        let f32_directory = tempdir().expect("F32 ceiling store directory");
        publish_f32_segment(f32_directory.path());
        let f32_store = Store::open(f32_directory.path(), OpenOptions::default())
            .expect("open F32 ceiling store");
        assert_remembered_ceiling_is_exact(&f32_store, 1);
        f32_store.close().expect("close F32 ceiling store");

        let active_directory = tempdir().expect("active ceiling store directory");
        let active = Store::open(active_directory.path(), OpenOptions::default())
            .expect("open active ceiling store");
        ingest_ceiling_rows(&active, 2, false);
        assert!(
            !active
                .admit_vector_search(SearchOptions::default())
                .expect("admit active ceiling query")
                .active_segment
                .is_empty(),
            "the active-segment case sealed every row"
        );
        assert_remembered_ceiling_is_exact(&active, 1);
        active.close().expect("close active ceiling store");
    }

    #[test]
    fn exact_and_graph_tiers_preserve_quantizer_validation_errors() {
        let directory = tempdir().expect("store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
        let query = vec![0.0_f32; crate::kernels::MAX_DOT_I8_DIMENSION + 1];

        for tier in [
            SearchTier::Exact,
            SearchTier::Graph(GraphSearchOptions::default()),
        ] {
            let error = store
                .search(
                    crate::ingest::SearchRequest::new(&query),
                    1,
                    SearchOptions::default().with_tier(tier),
                    QueryControl::Cancel(CancelToken::new()),
                )
                .expect_err("oversized quantized query must retain its error");
            assert!(matches!(
                error,
                super::QueryError::Scan(crate::scan::ScanError::Quant(
                    crate::quant::QuantError::DimensionTooLarge { actual, maximum }
                )) if actual == query.len()
                    && maximum == crate::kernels::MAX_DOT_I8_DIMENSION
            ));
        }
    }

    #[cfg(feature = "allocation-audit")]
    #[test]
    fn exact_tier_query_does_not_prepare_quantized_forms() {
        let directory = tempdir().expect("store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
        let query = [0.25_f32; 128];
        let options = SearchOptions::default().with_tier(SearchTier::Exact);
        let warm = store
            .search(
                crate::ingest::SearchRequest::new(&query),
                1,
                options,
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("warm exact-tier search");
        assert!(warm.candidates.is_empty());
        let control = QueryControl::Cancel(CancelToken::new());

        let (outcome, report) = crate::allocation_audit::audit_engine_path(|| {
            store.search(
                crate::ingest::SearchRequest::new(&query),
                1,
                options,
                control,
            )
        });
        let outcome = outcome.expect("audited exact-tier search");
        assert!(outcome.candidates.is_empty());
        assert_eq!(report.allocations, 0, "Exact tier prepared quantized forms");
        assert_eq!(report.attributed_bytes, 0);
        assert_eq!(report.unattributed_bytes, 0);
    }

    #[test]
    fn hybrid_dual_failure_precedence_is_independent_of_leg_completion_order() {
        use crate::fusion::{FusionError, FusionLeg, LegFailureKind};

        let vector_failure = FusionError::Leg {
            leg: FusionLeg::Vector,
            kind: LegFailureKind::Scan,
            detail: "vector failure".to_owned(),
        };
        let lexical_failure = FusionError::Leg {
            leg: FusionLeg::Lexical,
            kind: LegFailureKind::Lexical,
            detail: "lexical failure".to_owned(),
        };
        assert_eq!(
            resolve_hybrid_leg_results::<(), ()>(
                Err(vector_failure.clone()),
                Err(lexical_failure.clone()),
            ),
            Err(vector_failure.clone()),
        );
        assert_eq!(
            resolve_hybrid_leg_results::<(), ()>(
                Err(vector_failure.clone()),
                Err(FusionError::Cancelled { partial: false }),
            ),
            Err(FusionError::Cancelled { partial: false }),
        );
        assert_eq!(
            resolve_hybrid_leg_results::<(), ()>(
                Err(FusionError::Timeout { partial: false }),
                Err(FusionError::ReadCancelled { partial: false }),
            ),
            Err(FusionError::ReadCancelled { partial: false }),
        );
        assert_eq!(
            resolve_hybrid_leg_results::<(), ()>(
                Err(FusionError::Cancelled { partial: false }),
                Err(FusionError::Timeout { partial: false }),
            ),
            Err(FusionError::Timeout { partial: false }),
        );
        assert_eq!(
            resolve_hybrid_leg_results::<(), ()>(
                Err(FusionError::ReadCancelled { partial: false }),
                Err(lexical_failure),
            ),
            Err(FusionError::ReadCancelled { partial: false }),
        );
    }

    #[test]
    fn accounting_errors_report_the_typed_context() {
        let statistics = StoreError::Statistics {
            component: "mapped resident bytes",
            source: std::io::Error::other("mincore failed"),
        };
        assert_eq!(
            statistics.to_string(),
            "store statistics mapped resident bytes: mincore failed"
        );
        assert_eq!(
            statistics.source().map(ToString::to_string),
            Some("mincore failed".to_owned())
        );

        let budget = StoreError::BudgetExceeded {
            needed: 65,
            budget: 64,
            component: "wal",
        };
        assert_eq!(
            budget.to_string(),
            "store wal allocation needs 65 bytes, budget is 64 bytes"
        );
        assert!(budget.source().is_none());

        let allocation = StoreError::AllocationFailed {
            needed: 23,
            component: "snapshot",
        };
        assert_eq!(
            allocation.to_string(),
            "store snapshot allocator rejected 23 bytes"
        );
        assert!(allocation.source().is_none());
    }

    #[test]
    fn open_options_resolve_durability_before_filesystem_mutation() {
        let parent = tempdir().expect("parent directory");
        let store_path = parent.path().join("not-created");
        let options =
            OpenOptions::new().with_durability(DurabilityMode::Attached, CommitTier::Durable);

        let error = match Store::open(&store_path, options) {
            Ok(store) => {
                drop(store);
                panic!("attached durability unexpectedly opened")
            }
            Err(error) => error,
        };

        assert!(matches!(
            error,
            StoreError::Durability(DurabilityPolicyError::AttachedNotYetSupported)
        ));
        assert!(!store_path.exists(), "rejected open created store files");
    }

    #[test]
    fn memory_vfs_store_open_does_not_create_host_directory() {
        let parent = tempdir().expect("parent directory");
        let store_path = parent.path().join("memory-only-store");
        let dependencies = StoreTestDependencies::new(
            Arc::new(MemoryVfs::new()),
            Arc::new(ManualMonotonicClock::new()),
        );

        let store =
            Store::open_with_test_dependencies(&store_path, OpenOptions::read_only(), dependencies)
                .expect("MemoryVfs-backed store open");
        store.close().expect("MemoryVfs-backed store close");

        assert!(
            !store_path.exists(),
            "MemoryVfs-backed Store touched the host filesystem"
        );
    }

    #[test]
    fn open_refuses_a_manifest_ahead_of_the_checked_wal() {
        let directory = tempdir().expect("store directory");
        let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::Ordered)
            .expect("derived policy");
        commit_manifest(
            &StdVfs,
            directory.path(),
            &Manifest {
                generation: 1,
                log_seq: 1,
                segments: Vec::new(),
                epochs: Vec::new(),
                epoch_alias: None,
                schema: Schema::new(Vec::new()).expect("schema"),
            },
            policy,
        )
        .expect("manifest");

        let error = match Store::open(directory.path(), OpenOptions::default()) {
            Ok(store) => {
                drop(store);
                panic!("manifest ahead of missing WAL unexpectedly opened")
            }
            Err(error) => error,
        };

        assert!(matches!(
            error,
            StoreError::Manifest(ManifestError::AheadOfLog {
                snapshot: 1,
                durable: 0
            })
        ));
    }
}
