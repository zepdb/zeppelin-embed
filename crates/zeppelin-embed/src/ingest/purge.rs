//! Verifiable physical purge tokens, artifact rewriting, and recovery intent.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xxhash_rust::xxh3::xxh3_64;

use crate::format::FormatFamily;
use crate::format::frame::{FormatError, decode_artifact, encode_artifact};
use crate::lifecycle::durability::{DurabilityPolicy, SyncRequirement};
use crate::lifecycle::stats::Accounting;
use crate::lifecycle::{PublishedSnapshot, Store, StoreError, StoreState};
use crate::manifest::Manifest;
use crate::manifest::io::{MANIFEST_FILE, commit_manifest, load_manifest};
use crate::meta::{
    AliveSet, Column, ColumnInput, ColumnStore, ColumnStoreBuilder, ColumnValue, TIMESTAMP_COLUMN,
};
use crate::quant::Bit4Factors;
use crate::segment::layout::Int8Factors;
use crate::segment::reader::SegmentReader;
use crate::segment::writer::{
    SegmentBuild, SegmentDocumentVersions, SegmentFactors, SegmentPayloads, SegmentPostings,
    SegmentStoredMetadata, SegmentStoredText, write_segment_with_documents_payloads,
};
use crate::segment::{ClusteringKeyRange, SegmentId, SegmentMeta};
use crate::vfs::Vfs;
use crate::wal::LogSeq;

use super::{ActiveState, DocId, IngestDocument, SealedTombstoneDemand, wal_payload};

const PURGE_INTENT_FILE: &str = "purge.ze";
const PURGE_INTENT_TEMP_FILE: &str = ".purge.ze.tmp";
const WAL_PURGE_TEMP_FILE: &str = ".wal.ze.purge.tmp";

type WalImageRecords = (Vec<(u16, Vec<u8>)>, Vec<bool>);

#[derive(Clone, Copy)]
pub(crate) struct SealedDocumentMatch {
    pub(crate) segment_index: usize,
    pub(crate) row: usize,
    pub(crate) version: super::DocumentVersion,
}

pub(crate) struct PreparedSealedTombstones {
    manifest: Manifest,
    snapshot: PublishedSnapshot,
    replaced_paths: Vec<PathBuf>,
    replacement_ids: Vec<SegmentId>,
}

impl PreparedSealedTombstones {
    pub(crate) fn abort(
        self,
        vfs: &dyn Vfs,
        directory: &Path,
        policy: DurabilityPolicy,
    ) -> Result<(), StoreError> {
        cleanup_replacement_segments(vfs, directory, &self.replacement_ids, policy)
    }

    pub(crate) fn commit(
        mut self,
        store: &Store,
        vfs: &dyn Vfs,
        directory: &Path,
        policy: DurabilityPolicy,
    ) -> Result<(PublishedSnapshot, Vec<PathBuf>), StoreError> {
        self.manifest.epochs = store.epoch_registry(&self.manifest.epochs);
        commit_manifest(vfs, directory, &self.manifest, policy).map_err(StoreError::Manifest)?;
        Ok((self.snapshot, self.replaced_paths))
    }
}

pub(crate) fn sealed_document_matches(
    snapshot: &PublishedSnapshot,
    ids: &[DocId],
) -> Result<Vec<SealedDocumentMatch>, StoreError> {
    sealed_document_matches_in(snapshot.segments(), ids)
}

fn all_sealed_document_matches(
    snapshot: &PublishedSnapshot,
    ids: &[DocId],
) -> Result<Vec<SealedDocumentMatch>, StoreError> {
    sealed_document_matches_in(snapshot.all_segments(), ids)
}

fn sealed_document_matches_in(
    segments: &[SegmentReader],
    ids: &[DocId],
) -> Result<Vec<SealedDocumentMatch>, StoreError> {
    let requested = ids.iter().copied().collect::<HashSet<_>>();
    let mut matches = Vec::new();
    for (segment_index, segment) in segments.iter().enumerate() {
        for row in 0..segment.meta().row_count as usize {
            if let Some(version) = segment.document_version(row).map_err(StoreError::Segment)?
                && requested.contains(&version.doc_id())
            {
                matches.push(SealedDocumentMatch {
                    segment_index,
                    row,
                    version,
                });
            }
        }
    }
    Ok(matches)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_sealed_tombstones(
    vfs: &dyn Vfs,
    directory: &Path,
    snapshot: &PublishedSnapshot,
    matches: &[SealedDocumentMatch],
    ids: &[DocId],
    durable_end: u64,
    generation: u64,
    nonce: u64,
    policy: DurabilityPolicy,
    accounting: &Arc<Accounting>,
) -> Result<Option<PreparedSealedTombstones>, StoreError> {
    if matches.is_empty() || ids.is_empty() {
        return Ok(None);
    }
    let requested = ids.iter().copied().collect::<HashSet<_>>();
    let mut rows_by_segment = vec![Vec::<usize>::new(); snapshot.segments().len()];
    for matched in matches {
        if requested.contains(&matched.version.doc_id()) {
            let rows = rows_by_segment
                .get_mut(matched.segment_index)
                .ok_or(StoreError::ActiveRowOverflow)?;
            if !rows.contains(&matched.row) {
                rows.push(matched.row);
            }
        }
    }
    let manifest_path = directory.join(MANIFEST_FILE);
    let mut manifest =
        load_manifest(vfs, &manifest_path, durable_end).map_err(StoreError::Manifest)?;
    let mut replaced_paths = Vec::new();
    let mut replacement_ids = Vec::new();
    let prepared = (|| {
        for (segment_index, rows) in rows_by_segment.iter_mut().enumerate() {
            if rows.is_empty() {
                continue;
            }
            let reader = snapshot
                .segments()
                .get(segment_index)
                .ok_or(StoreError::ActiveRowOverflow)?;
            let alive = reader.alive().map_err(StoreError::Segment)?;
            rows.retain(|row| {
                u32::try_from(*row)
                    .ok()
                    .is_some_and(|row| alive.is_alive(row))
            });
            if rows.is_empty() {
                continue;
            }
            let original = manifest
                .segments
                .iter()
                .find(|segment| segment.id == reader.meta().id)
                .cloned()
                .ok_or_else(|| {
                    StoreError::Segment(crate::segment::SegmentError::Geometry(format!(
                        "manifest lost sealed mutation segment {}",
                        reader.meta().id
                    )))
                })?;
            let replacement_id = replacement_segment_id(original.id, nonce, generation);
            replacement_ids.push(replacement_id);
            let all_rows = (0..reader.meta().row_count as usize).collect::<Vec<_>>();
            let mut replacement = rewrite_segment(
                vfs,
                directory,
                reader,
                &all_rows,
                rows,
                replacement_id,
                policy,
            )?;
            replacement.epoch_id = original.epoch_id;
            let target = manifest
                .segments
                .iter_mut()
                .find(|segment| segment.id == original.id)
                .ok_or_else(|| {
                    StoreError::Segment(crate::segment::SegmentError::Geometry(format!(
                        "manifest lost sealed mutation segment {}",
                        original.id
                    )))
                })?;
            *target = replacement;
            replaced_paths.push(directory.join(original.id.file_name()));
        }
        if replacement_ids.is_empty() {
            return Ok(None);
        }
        manifest.generation = generation;
        let remapped = PublishedSnapshot::from_manifest(vfs, directory, &manifest, accounting)?;
        Ok(Some(PreparedSealedTombstones {
            manifest,
            snapshot: remapped,
            replaced_paths,
            replacement_ids: replacement_ids.clone(),
        }))
    })();
    if prepared.is_err() {
        cleanup_replacement_segments(vfs, directory, &replacement_ids, policy)?;
    }
    prepared
}

/// Caller handle for one scheduled physical-purge guarantee.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PurgeToken {
    id: u64,
    generation: u64,
    unknown_ids: Vec<DocId>,
    no_op: bool,
}

impl PurgeToken {
    /// Returns the stable token identifier.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Returns the generation observed when the token was issued.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns requested identifiers absent from active and sealed state.
    #[must_use]
    pub fn unknown_ids(&self) -> &[DocId] {
        &self.unknown_ids
    }

    /// Returns whether this token represents no physical work.
    #[must_use]
    pub const fn is_no_op(&self) -> bool {
        self.no_op
    }
}

/// Completed physical-purge accounting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PurgeReport {
    generation: u64,
    segments_rewritten: usize,
    wal_rewritten: bool,
    unknown_ids: Vec<DocId>,
}

impl PurgeReport {
    /// Returns the generation at which the physical guarantee holds.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the number of sealed segments physically rewritten.
    #[must_use]
    pub const fn segments_rewritten(&self) -> usize {
        self.segments_rewritten
    }

    /// Returns whether the on-disk WAL was atomically replaced.
    #[must_use]
    pub const fn wal_rewritten(&self) -> bool {
        self.wal_rewritten
    }

    /// Returns requested identifiers absent from active and sealed state.
    #[must_use]
    pub fn unknown_ids(&self) -> &[DocId] {
        &self.unknown_ids
    }

    /// Returns whether no artifact needed mutation.
    #[must_use]
    pub const fn is_no_op(&self) -> bool {
        self.segments_rewritten == 0 && !self.wal_rewritten
    }
}

/// Typed physical-purge rejection.
#[derive(Debug)]
pub enum PurgeError {
    /// Store lifecycle, format, filesystem, or synchronization failure.
    Store(StoreError),
    /// Rewriting one immutable segment would exceed available temporary disk.
    InsufficientTempSpace {
        /// Exact immutable segment bytes that would be duplicated temporarily.
        segment_bytes: u64,
        /// Available filesystem bytes observed before any purge write.
        available_bytes: u64,
        /// Integer floor of the strict 120-percent threshold.
        required_bytes: u64,
    },
    /// One store already owns a scheduled purge that has not resolved.
    PurgeInProgress,
    /// A token was not issued by this store's current pending operation.
    UnknownToken {
        /// Rejected caller token identifier.
        token_id: u64,
    },
    /// The durable purge-intent frame failed checksum or header validation.
    IntentFormat(FormatError),
    /// The durable purge-intent payload violated its fixed-width contract.
    IntentDecode(String),
    /// Re-encoding the surviving active state violated the WAL payload contract.
    WalPayload(wal_payload::PayloadError),
    /// Rewriting the WAL would discard an acknowledged, unabsorbed mutation.
    WalRewriteWouldDropAcked {
        /// First retained sequence not covered by the surviving active state.
        seq: LogSeq,
    },
}

impl std::fmt::Display for PurgeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => error.fmt(formatter),
            Self::InsufficientTempSpace {
                segment_bytes,
                available_bytes,
                required_bytes,
            } => write!(
                formatter,
                "purge segment of {segment_bytes} bytes needs more than {required_bytes} temporary bytes, only {available_bytes} available"
            ),
            Self::PurgeInProgress => formatter.write_str("a physical purge is already pending"),
            Self::UnknownToken { token_id } => {
                write!(formatter, "purge token {token_id} is unknown")
            }
            Self::IntentFormat(error) => write!(formatter, "purge intent: {error}"),
            Self::IntentDecode(detail) => write!(formatter, "purge intent decode failed: {detail}"),
            Self::WalPayload(error) => write!(formatter, "purge WAL payload: {error}"),
            Self::WalRewriteWouldDropAcked { seq } => write!(
                formatter,
                "purge WAL rewrite would drop acknowledged WAL sequence {}",
                seq.get()
            ),
        }
    }
}

impl std::error::Error for PurgeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::IntentFormat(error) => Some(error),
            Self::WalPayload(error) => Some(error),
            Self::InsufficientTempSpace { .. }
            | Self::PurgeInProgress
            | Self::UnknownToken { .. }
            | Self::IntentDecode(_)
            | Self::WalRewriteWouldDropAcked { .. } => None,
        }
    }
}

impl From<StoreError> for PurgeError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

struct PurgeIntent {
    token_id: u64,
    ids: Vec<DocId>,
}

impl Store {
    pub(crate) fn recover_sealed_tombstones(
        &self,
        demands: &[SealedTombstoneDemand],
    ) -> Result<(), StoreError> {
        if demands.is_empty() {
            return Ok(());
        }
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?
            .as_ref()
            .cloned()
            .ok_or(StoreError::Closed)?;
        let requested = demands
            .iter()
            .map(|demand| demand.doc_id())
            .collect::<Vec<_>>();
        let sealed = sealed_document_matches(&snapshot, &requested)?;
        let mut surviving = Vec::new();
        for matched in sealed {
            if !demands.iter().any(|demand| demand.matches(matched.version)) {
                continue;
            }
            let segment = snapshot
                .segments()
                .get(matched.segment_index)
                .ok_or(StoreError::ActiveRowOverflow)?;
            let row = u32::try_from(matched.row).map_err(|_| StoreError::ActiveRowOverflow)?;
            if segment.alive().map_err(StoreError::Segment)?.is_alive(row) {
                surviving.push(matched);
            }
        }
        if surviving.is_empty() {
            return Ok(());
        }
        let state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(StoreError::Closing),
            StoreState::Closed => return Err(StoreError::Closed),
        }
        let writer_lock = self
            .writer_lock
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "writer lock",
            })?;
        if writer_lock.is_none() {
            return Err(StoreError::SealedTombstoneRecoveryRequired);
        }
        let mut wal = self
            .wal_writer
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "WAL writer",
            })?;
        let writer = wal
            .as_mut()
            .ok_or(StoreError::SealedTombstoneRecoveryRequired)?;
        let mut active = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        let current = active.as_ref().ok_or(StoreError::Closed)?;
        let generation = current
            .generation
            .checked_add(1)
            .ok_or(StoreError::GenerationOverflow)?;
        let active_segment = Arc::clone(&current.segment);
        let mut ids = surviving
            .iter()
            .map(|matched| matched.version.doc_id())
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        let prepared = prepare_sealed_tombstones(
            self.vfs.as_ref(),
            &self.directory,
            &snapshot,
            &surviving,
            &ids,
            writer.durable_end(),
            generation,
            writer.durable_end().saturating_add(1),
            self.durability_policy,
            &self.accounting,
        )?;
        let Some(prepared) = prepared else {
            return Ok(());
        };
        let (remapped, replaced_paths) = prepared.commit(
            self,
            self.vfs.as_ref(),
            &self.directory,
            self.durability_policy,
        )?;
        let mut published = self
            .snapshot
            .write()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?;
        let previous = published.replace(Arc::new(remapped));
        *active = Some(ActiveState {
            generation,
            segment: active_segment,
        });
        drop(published);
        drop(previous);
        unlink_replaced_segments(
            self.vfs.as_ref(),
            &self.directory,
            &replaced_paths,
            self.durability_policy,
        );
        drop(active);
        drop(wal);
        drop(writer_lock);
        drop(state);
        Ok(())
    }

    /// Schedules a physical purge and returns before artifact rewriting begins.
    pub fn purge(&self, ids: &[DocId]) -> Result<PurgeToken, PurgeError> {
        let available = available_disk_bytes(&self.directory).map_err(|source| {
            PurgeError::Store(StoreError::Io {
                path: self.directory.clone(),
                source,
            })
        })?;
        self.purge_inner(ids, available, self.vfs.as_ref())
    }

    /// Test seam that supplies a deterministic free-space observation.
    #[doc(hidden)]
    pub fn purge_with_available_space(
        &self,
        ids: &[DocId],
        available_bytes: u64,
    ) -> Result<PurgeToken, PurgeError> {
        self.purge_inner(ids, available_bytes, self.vfs.as_ref())
    }

    fn purge_inner(
        &self,
        ids: &[DocId],
        available_bytes: u64,
        vfs: &dyn Vfs,
    ) -> Result<PurgeToken, PurgeError> {
        let _maintenance = self
            .maintenance
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "physical purge",
            })?;
        let state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(StoreError::Closing.into()),
            StoreState::Closed => return Err(StoreError::Closed.into()),
        }
        let writer_lock = self
            .writer_lock
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "writer lock",
            })?;
        if writer_lock.is_none() {
            return Err(StoreError::ReadOnly.into());
        }
        let intent_path = self.directory.join(PURGE_INTENT_FILE);
        match vfs.open(&intent_path) {
            Ok(_) => return Err(PurgeError::PurgeInProgress),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(StoreError::Io {
                    path: intent_path,
                    source,
                }
                .into());
            }
        }
        let mut requested = ids.to_vec();
        requested.sort_unstable();
        requested.dedup();
        let active = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        let active_state = active.as_ref().ok_or(StoreError::Closed)?;
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?
            .as_ref()
            .cloned()
            .ok_or(StoreError::Closed)?;
        let sealed = all_sealed_document_matches(&snapshot, &requested)?;
        let mut known = Vec::new();
        let mut unknown = Vec::new();
        for id in requested {
            let in_active = active_state.segment.existing(id).is_some();
            let in_sealed = sealed.iter().any(|matched| matched.version.doc_id() == id);
            if in_active || in_sealed {
                known.push(id);
            } else {
                unknown.push(id);
            }
        }
        for (segment_index, segment) in snapshot.all_segments().iter().enumerate() {
            if sealed.iter().any(|matched| {
                matched.segment_index == segment_index && known.contains(&matched.version.doc_id())
            }) {
                enforce_temp_space(segment.meta().file_size, available_bytes)?;
            }
        }
        let token_id = purge_token_id(active_state.generation, &known);
        let no_op = known.is_empty();
        let token = PurgeToken {
            id: token_id,
            generation: active_state.generation,
            unknown_ids: unknown,
            no_op,
        };
        if no_op {
            return Ok(token);
        }
        #[cfg(any(test, feature = "test-support"))]
        let crash_target_ids = known.iter().map(|id| id.get()).collect::<Vec<_>>();
        write_intent(
            vfs,
            &self.directory,
            &PurgeIntent {
                token_id,
                ids: known,
            },
            self.durability_policy,
        )?;
        #[cfg(any(test, feature = "test-support"))]
        if let Some(controller) = self.ingest_retention_fault_controller.as_ref() {
            let plan = controller.take_purge_crash_boundary_plan().map_err(|_| {
                StoreError::Synchronization {
                    component: "ingest-retention controller",
                }
            })?;
            if let Some((invocation_id, receipt_sink)) = plan {
                let receipt = super::IngestRetentionFaultReceiptV1::purge_crash_boundary(
                    invocation_id,
                    crash_target_ids,
                    token_id,
                );
                receipt
                    .write_purge_crash_test_evidence(vfs, &receipt_sink)
                    .map_err(|source| StoreError::Io {
                        path: receipt_sink,
                        source,
                    })?;
                std::process::abort();
            }
        }
        drop(snapshot);
        drop(active);
        drop(writer_lock);
        drop(state);
        Ok(token)
    }

    /// Resolves only after rewritten artifacts are committed and old paths are unlinked.
    pub fn await_physical_purge(&self, token: PurgeToken) -> Result<PurgeReport, PurgeError> {
        self.await_physical_purge_on_vfs(token, self.vfs.as_ref())
    }

    /// Test seam that runs the purge protocol through an observable filesystem.
    #[doc(hidden)]
    pub fn await_physical_purge_on_vfs(
        &self,
        token: PurgeToken,
        vfs: &dyn Vfs,
    ) -> Result<PurgeReport, PurgeError> {
        if token.no_op {
            return Ok(PurgeReport {
                generation: token.generation,
                segments_rewritten: 0,
                wal_rewritten: false,
                unknown_ids: token.unknown_ids,
            });
        }
        let _maintenance = self
            .maintenance
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "physical purge",
            })?;
        let state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(StoreError::Closing.into()),
            StoreState::Closed => return Err(StoreError::Closed.into()),
        }
        let writer_lock = self
            .writer_lock
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "writer lock",
            })?;
        if writer_lock.is_none() {
            return Err(StoreError::ReadOnly.into());
        }
        let mut wal = self
            .wal_writer
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "WAL writer",
            })?;
        let writer = wal.as_mut().ok_or(StoreError::ReadOnly)?;
        let intent = read_intent(vfs, &self.directory)?;
        if intent.token_id != token.id {
            return Err(PurgeError::UnknownToken { token_id: token.id });
        }
        let mut active = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        let active_state = active.as_mut().ok_or(StoreError::Closed)?;
        let manifest_path = self.directory.join(MANIFEST_FILE);
        let mut manifest = match vfs.open(&manifest_path) {
            Ok(_) => load_manifest(vfs, &manifest_path, writer.durable_end())
                .map_err(StoreError::Manifest)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Manifest {
                generation: active_state.generation,
                log_seq: 0,
                segments: Vec::new(),
                epochs: Vec::new(),
                epoch_alias: None,
                schema: crate::meta::Schema::new(Vec::new()).map_err(|source| {
                    StoreError::Segment(crate::segment::SegmentError::Columns(source.to_string()))
                })?,
            },
            Err(source) => {
                return Err(StoreError::Io {
                    path: manifest_path,
                    source,
                }
                .into());
            }
        };
        let mut rewritten = 0_usize;
        let original_segments = manifest.segments.clone();
        for original in original_segments {
            let path = self.directory.join(original.id.file_name());
            let reader =
                SegmentReader::open(vfs, &path, original.id).map_err(StoreError::Segment)?;
            let survivors = survivor_rows(&reader, &intent.ids)?;
            if survivors.len() == original.row_count as usize {
                continue;
            }
            let generation = active_state
                .generation
                .max(manifest.generation)
                .checked_add(1)
                .ok_or(StoreError::GenerationOverflow)?;
            let replacement_id = replacement_segment_id(original.id, token.id, generation);
            let mut replacement = rewrite_segment(
                vfs,
                &self.directory,
                &reader,
                &survivors,
                &[],
                replacement_id,
                self.durability_policy,
            )?;
            replacement.epoch_id = original.epoch_id;
            drop(reader);
            let position = manifest
                .segments
                .iter()
                .position(|segment| segment.id == original.id)
                .ok_or_else(|| {
                    PurgeError::IntentDecode(format!("manifest lost purge segment {}", original.id))
                })?;
            let target = manifest.segments.get_mut(position).ok_or_else(|| {
                PurgeError::IntentDecode("purge segment position is invalid".to_owned())
            })?;
            *target = replacement;
            manifest.generation = generation;
            manifest.epochs = self.epoch_registry(&manifest.epochs);
            let remapped = PublishedSnapshot::from_manifest(
                vfs,
                &self.directory,
                &manifest,
                &self.accounting,
            )?;
            commit_manifest(vfs, &self.directory, &manifest, self.durability_policy)
                .map_err(StoreError::Manifest)?;
            let mut published = self
                .snapshot
                .write()
                .map_err(|_| StoreError::Synchronization {
                    component: "published snapshot",
                })?;
            let previous = published.replace(Arc::new(remapped));
            active_state.generation = generation;
            drop(published);
            drop(previous);
            if let Err(source) = vfs.delete(&path) {
                #[cfg(any(test, feature = "test-support"))]
                if source.kind() == std::io::ErrorKind::Other
                    && let Some(controller) = self.ingest_retention_fault_controller.as_ref()
                {
                    let invocation_id = controller
                        .purge_unlink_error_plan(*original.id.as_bytes())
                        .map_err(|_| StoreError::Synchronization {
                            component: "ingest-retention controller",
                        })?;
                    if let Some(invocation_id) = invocation_id {
                        let intent_present =
                            vfs.open(&self.directory.join(PURGE_INTENT_FILE)).is_ok();
                        let old_path_linked = vfs.open(&path).is_ok();
                        controller
                            .push_receipt(super::IngestRetentionFaultReceiptV1::purge_unlink_error(
                                invocation_id,
                                *original.id.as_bytes(),
                                *replacement_id.as_bytes(),
                                original.id.file_name(),
                                true,
                                intent_present,
                                old_path_linked,
                            ))
                            .map_err(|_| StoreError::Synchronization {
                                component: "ingest-retention controller",
                            })?;
                    }
                }
                return Err(StoreError::Io {
                    path: path.clone(),
                    source,
                }
                .into());
            }
            sync_directory(vfs, &self.directory, self.durability_policy)?;
            rewritten = rewritten.saturating_add(1);
        }
        sweep_purge_orphans(vfs, &self.directory, &manifest, self.durability_policy)?;

        let active_has_target = intent
            .ids
            .iter()
            .any(|id| active_state.segment.existing(*id).is_some());
        let mut next_active = if active_has_target {
            let (purged, removed) = active_state.segment.purge(&intent.ids, &self.accounting)?;
            if removed == 0 {
                return Err(PurgeError::IntentDecode(
                    "active purge target disappeared during admission".to_owned(),
                ));
            }
            active_state.generation = active_state
                .generation
                .checked_add(1)
                .ok_or(StoreError::GenerationOverflow)?;
            purged
        } else {
            active_state.segment.purge(&[], &self.accounting)?.0
        };
        let (records, tombstoned) = active_wal_records(&next_active)?;
        if manifest.generation < active_state.generation {
            manifest.generation = active_state.generation;
            manifest.epochs = self.epoch_registry(&manifest.epochs);
            let remapped = PublishedSnapshot::from_manifest(
                vfs,
                &self.directory,
                &manifest,
                &self.accounting,
            )?;
            commit_manifest(vfs, &self.directory, &manifest, self.durability_policy)
                .map_err(StoreError::Manifest)?;
            let mut published = self
                .snapshot
                .write()
                .map_err(|_| StoreError::Synchronization {
                    component: "published snapshot",
                })?;
            let previous = published.replace(Arc::new(remapped));
            drop(published);
            drop(previous);
        }
        let first_seq = LogSeq::new(
            manifest
                .log_seq
                .checked_add(1)
                .ok_or(StoreError::GenerationOverflow)?,
        );
        if writer.durable_end() >= first_seq.get() {
            ensure_wal_rewrite_covers_retained(
                vfs,
                &self.directory,
                first_seq,
                &intent.ids,
                &next_active,
            )?;
        }
        writer.rewrite(
            vfs,
            &self.directory,
            self.durability_policy,
            first_seq,
            &records,
        )?;
        assign_rewritten_sequences(&mut next_active, first_seq, &tombstoned)?;
        active_state.segment = Arc::new(next_active);
        remove_intent(vfs, &self.directory, self.durability_policy)?;
        let generation = active_state.generation;
        drop(active);
        drop(wal);
        drop(writer_lock);
        drop(state);
        Ok(PurgeReport {
            generation,
            segments_rewritten: rewritten,
            wal_rewritten: true,
            unknown_ids: token.unknown_ids,
        })
    }

    pub(crate) fn recover_pending_physical_purge(&self) -> Result<(), PurgeError> {
        let path = self.directory.join(PURGE_INTENT_FILE);
        match self.vfs.open(&path) {
            Ok(_) => {
                let intent = read_intent(self.vfs.as_ref(), &self.directory)?;
                let generation = self
                    .active
                    .lock()
                    .map_err(|_| StoreError::Synchronization {
                        component: "active segment",
                    })?
                    .as_ref()
                    .ok_or(StoreError::Closed)?
                    .generation;
                let token = PurgeToken {
                    id: intent.token_id,
                    generation,
                    unknown_ids: Vec::new(),
                    no_op: false,
                };
                self.await_physical_purge(token).map(|_| ())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StoreError::Io { path, source }.into()),
        }
    }
}

fn enforce_temp_space(segment_bytes: u64, available_bytes: u64) -> Result<(), PurgeError> {
    let required_bytes = segment_bytes
        .checked_mul(6)
        .map(|value| value / 5)
        .unwrap_or(u64::MAX);
    if available_bytes > required_bytes {
        Ok(())
    } else {
        Err(PurgeError::InsufficientTempSpace {
            segment_bytes,
            available_bytes,
            required_bytes,
        })
    }
}

fn purge_token_id(generation: u64, ids: &[DocId]) -> u64 {
    let mut bytes = Vec::with_capacity(8_usize.saturating_add(ids.len().saturating_mul(16)));
    bytes.extend_from_slice(&generation.to_le_bytes());
    for id in ids {
        bytes.extend_from_slice(&id.get().to_le_bytes());
    }
    xxh3_64(&bytes)
}

fn write_intent(
    vfs: &dyn Vfs,
    directory: &Path,
    intent: &PurgeIntent,
    policy: DurabilityPolicy,
) -> Result<(), PurgeError> {
    let count = u32::try_from(intent.ids.len())
        .map_err(|_| PurgeError::IntentDecode("purge id count exceeds u32".to_owned()))?;
    let mut payload =
        Vec::with_capacity(16_usize.saturating_add(intent.ids.len().saturating_mul(16)));
    payload.extend_from_slice(&intent.token_id.to_le_bytes());
    payload.extend_from_slice(&count.to_le_bytes());
    payload.extend_from_slice(&0_u32.to_le_bytes());
    for id in &intent.ids {
        payload.extend_from_slice(&id.get().to_le_bytes());
    }
    let bytes = encode_artifact(FormatFamily::PurgeIntent, 0, &payload);
    let temporary = directory.join(PURGE_INTENT_TEMP_FILE);
    let committed = directory.join(PURGE_INTENT_FILE);
    vfs.write(&temporary, &bytes)
        .map_err(|source| StoreError::Io {
            path: temporary.clone(),
            source,
        })?;
    if let SyncRequirement::Sync(kind) = policy.data_file_sync() {
        vfs.sync(&temporary, kind)
            .map_err(|source| StoreError::Io {
                path: temporary.clone(),
                source,
            })?;
    }
    vfs.rename(&temporary, &committed)
        .map_err(|source| StoreError::Io {
            path: committed,
            source,
        })?;
    sync_directory(vfs, directory, policy).map_err(PurgeError::from)
}

fn read_intent(vfs: &dyn Vfs, directory: &Path) -> Result<PurgeIntent, PurgeError> {
    let path = directory.join(PURGE_INTENT_FILE);
    let bytes = vfs.read(&path).map_err(|source| StoreError::Io {
        path: path.clone(),
        source,
    })?;
    let decoded = decode_artifact(
        &path.display().to_string(),
        FormatFamily::PurgeIntent,
        &bytes,
    )
    .map_err(PurgeError::IntentFormat)?;
    let token_id = read_u64(decoded.payload, 0)?;
    let count = read_u32(decoded.payload, 8)? as usize;
    if read_u32(decoded.payload, 12)? != 0 {
        return Err(PurgeError::IntentDecode(
            "purge intent reserved field is non-zero".to_owned(),
        ));
    }
    let expected = 16_usize
        .checked_add(count.saturating_mul(16))
        .ok_or_else(|| PurgeError::IntentDecode("purge intent length overflow".to_owned()))?;
    if decoded.payload.len() != expected {
        return Err(PurgeError::IntentDecode(format!(
            "purge intent bytes {}, expected {expected}",
            decoded.payload.len()
        )));
    }
    let mut ids = Vec::with_capacity(count);
    for index in 0..count {
        let offset = 16_usize
            .checked_add(index.saturating_mul(16))
            .ok_or_else(|| PurgeError::IntentDecode("purge id offset overflow".to_owned()))?;
        let end = offset
            .checked_add(16)
            .ok_or_else(|| PurgeError::IntentDecode("purge id end overflow".to_owned()))?;
        let value = decoded
            .payload
            .get(offset..end)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u128::from_le_bytes)
            .ok_or_else(|| PurgeError::IntentDecode(format!("purge id {index} is truncated")))?;
        ids.push(DocId::new(value));
    }
    Ok(PurgeIntent { token_id, ids })
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, PurgeError> {
    bytes
        .get(offset..offset.saturating_add(4))
        .and_then(|value| value.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| PurgeError::IntentDecode(format!("u32 at {offset} is truncated")))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, PurgeError> {
    bytes
        .get(offset..offset.saturating_add(8))
        .and_then(|value| value.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| PurgeError::IntentDecode(format!("u64 at {offset} is truncated")))
}

fn remove_intent(
    vfs: &dyn Vfs,
    directory: &Path,
    policy: DurabilityPolicy,
) -> Result<(), PurgeError> {
    let path = directory.join(PURGE_INTENT_FILE);
    vfs.delete(&path)
        .map_err(|source| StoreError::Io { path, source })?;
    sync_directory(vfs, directory, policy).map_err(PurgeError::from)
}

fn sync_directory(
    vfs: &dyn Vfs,
    directory: &Path,
    policy: DurabilityPolicy,
) -> Result<(), StoreError> {
    if let SyncRequirement::Sync(kind) = policy.directory_sync() {
        vfs.sync(directory, kind).map_err(|source| StoreError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}

pub(crate) fn unlink_replaced_segments(
    vfs: &dyn Vfs,
    directory: &Path,
    replaced_paths: &[PathBuf],
    policy: DurabilityPolicy,
) {
    for path in replaced_paths {
        // The replacement manifest and snapshot are already published. A
        // failed unlink leaves an unreachable orphan for open-time cleanup;
        // it cannot roll back or deny the acknowledged mutation.
        let _ = vfs.delete(path);
    }
    if !replaced_paths.is_empty() {
        let _ = sync_directory(vfs, directory, policy);
    }
}

fn cleanup_replacement_segments(
    vfs: &dyn Vfs,
    directory: &Path,
    replacement_ids: &[SegmentId],
    policy: DurabilityPolicy,
) -> Result<(), StoreError> {
    let mut deleted = false;
    for id in replacement_ids {
        for path in [
            directory.join(id.file_name()),
            directory.join(format!(".{}.tmp", id.file_name())),
        ] {
            match vfs.open(&path) {
                Ok(_) => {
                    vfs.delete(&path)
                        .map_err(|source| StoreError::Io { path, source })?;
                    deleted = true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(StoreError::Io { path, source }),
            }
        }
    }
    if deleted {
        sync_directory(vfs, directory, policy)?;
    }
    Ok(())
}

fn survivor_rows(reader: &SegmentReader, ids: &[DocId]) -> Result<Vec<usize>, PurgeError> {
    let mut survivors = Vec::with_capacity(reader.meta().row_count as usize);
    for row in 0..reader.meta().row_count as usize {
        let version = reader.document_version(row).map_err(StoreError::Segment)?;
        if version.is_none_or(|version| !ids.contains(&version.doc_id())) {
            survivors.push(row);
        }
    }
    Ok(survivors)
}

enum OwnedFactors {
    Bit4(Vec<Bit4Factors>),
    Int8(Vec<Int8Factors>),
}

impl OwnedFactors {
    fn borrowed(&self) -> SegmentFactors<'_> {
        match self {
            Self::Bit4(values) => SegmentFactors::Bit4(values),
            Self::Int8(values) => SegmentFactors::Int8(values),
        }
    }
}

fn rewrite_segment(
    vfs: &dyn Vfs,
    directory: &Path,
    reader: &SegmentReader,
    survivors: &[usize],
    additional_tombstones: &[usize],
    replacement_id: SegmentId,
    policy: DurabilityPolicy,
) -> Result<SegmentMeta, StoreError> {
    let dims = reader.meta().dims as usize;
    let (codes, factors) = match reader.meta().scheme {
        4 => {
            let stride = dims.div_ceil(2);
            let source_codes = reader.bit4_codes().map_err(StoreError::Segment)?;
            let source_factors = reader.bit4_factors().map_err(StoreError::Segment)?;
            let mut codes = Vec::with_capacity(survivors.len().saturating_mul(stride));
            let mut factors = Vec::with_capacity(survivors.len());
            for row in survivors {
                let start = row
                    .checked_mul(stride)
                    .ok_or(StoreError::ActiveRowOverflow)?;
                let end = start
                    .checked_add(stride)
                    .ok_or(StoreError::ActiveRowOverflow)?;
                codes.extend_from_slice(
                    source_codes
                        .get(start..end)
                        .ok_or(StoreError::ActiveRowOverflow)?,
                );
                factors.push(
                    source_factors
                        .get(*row)
                        .copied()
                        .ok_or(StoreError::ActiveRowOverflow)?,
                );
            }
            (codes, OwnedFactors::Bit4(factors))
        }
        2 => {
            let stride = dims;
            let source_codes = reader.int8_codes().map_err(StoreError::Segment)?;
            let source_factors = reader.int8_factors().map_err(StoreError::Segment)?;
            let mut codes = Vec::with_capacity(survivors.len().saturating_mul(stride));
            let mut factors = Vec::with_capacity(survivors.len());
            for row in survivors {
                let start = row
                    .checked_mul(stride)
                    .ok_or(StoreError::ActiveRowOverflow)?;
                let end = start
                    .checked_add(stride)
                    .ok_or(StoreError::ActiveRowOverflow)?;
                for value in source_codes
                    .get(start..end)
                    .ok_or(StoreError::ActiveRowOverflow)?
                {
                    codes.push(value.to_ne_bytes()[0]);
                }
                factors.push(
                    source_factors
                        .get(*row)
                        .copied()
                        .ok_or(StoreError::ActiveRowOverflow)?,
                );
            }
            (codes, OwnedFactors::Int8(factors))
        }
        scheme => {
            return Err(StoreError::Segment(crate::segment::SegmentError::Geometry(
                format!("segment rewrite cannot encode scheme {scheme}"),
            )));
        }
    };
    let source_rescore = reader.rescore_f32().map_err(StoreError::Segment)?;
    let mut rescore = Vec::with_capacity(survivors.len().saturating_mul(dims));
    for row in survivors {
        let start = row.checked_mul(dims).ok_or(StoreError::ActiveRowOverflow)?;
        let end = start
            .checked_add(dims)
            .ok_or(StoreError::ActiveRowOverflow)?;
        rescore.extend_from_slice(
            source_rescore
                .get(start..end)
                .ok_or(StoreError::ActiveRowOverflow)?,
        );
    }
    let source_columns = reader.columns().map_err(StoreError::Segment)?;
    let columns = rewrite_columns(&source_columns, survivors)?;
    let source_alive = reader.alive().map_err(StoreError::Segment)?;
    let row_count = u32::try_from(survivors.len()).map_err(|_| StoreError::ActiveRowOverflow)?;
    let mut alive = AliveSet::new(row_count);
    for (new_row, old_row) in survivors.iter().enumerate() {
        let old_row_u32 = u32::try_from(*old_row).map_err(|_| StoreError::ActiveRowOverflow)?;
        if !source_alive.is_alive(old_row_u32) || additional_tombstones.contains(old_row) {
            alive
                .tombstone(u32::try_from(new_row).map_err(|_| StoreError::ActiveRowOverflow)?)
                .map_err(|_| StoreError::ActiveRowOverflow)?;
        }
    }
    let mut doc_ids = Vec::with_capacity(survivors.len());
    let mut revisions = Vec::with_capacity(survivors.len());
    for row in survivors {
        let version = reader
            .document_version(*row)
            .map_err(StoreError::Segment)?
            .ok_or_else(|| {
                StoreError::Segment(crate::segment::SegmentError::Geometry(
                    "purge target segment has no document-version row".to_owned(),
                ))
            })?;
        doc_ids.push(version.doc_id());
        revisions.push(version.revision());
    }
    let source_metadata = reader.stored_metadata().map_err(StoreError::Segment)?;
    let mut metadata_offsets = Vec::with_capacity(survivors.len());
    let mut metadata_bytes = Vec::new();
    if let Some(source) = source_metadata {
        for row in survivors {
            metadata_bytes.extend_from_slice(source.row(*row).ok_or_else(|| {
                StoreError::Segment(crate::segment::SegmentError::Geometry(format!(
                    "stored metadata row {row} is missing"
                )))
            })?);
            metadata_offsets.push(
                u64::try_from(metadata_bytes.len()).map_err(|_| StoreError::ActiveRowOverflow)?,
            );
        }
    }
    let source_text = reader.stored_text().map_err(StoreError::Segment)?;
    let mut text_present = Vec::with_capacity(survivors.len());
    let mut text_end_offsets = Vec::with_capacity(survivors.len());
    let mut text_bytes = Vec::new();
    if let Some(source) = source_text {
        for row in survivors {
            match source.row(*row).ok_or_else(|| {
                StoreError::Segment(crate::segment::SegmentError::Geometry(format!(
                    "stored text row {row} is missing"
                )))
            })? {
                Some(text) => {
                    text_present.push(1);
                    text_bytes.extend_from_slice(text.as_bytes());
                }
                None => text_present.push(0),
            }
            text_end_offsets
                .push(u64::try_from(text_bytes.len()).map_err(|_| StoreError::ActiveRowOverflow)?);
        }
    }
    let build = SegmentBuild {
        id: replacement_id,
        scheme: reader.meta().scheme,
        dims: reader.meta().dims,
        codes: &codes,
        factors: factors.borrowed(),
        rescore: &rescore,
        columns: &columns,
        alive: &alive,
    };
    let documents = SegmentDocumentVersions {
        doc_ids: &doc_ids,
        revisions: &revisions,
    };
    let postings = reader
        .postings()
        .map_err(StoreError::Segment)?
        .map(|postings| postings.retain_rows(survivors))
        .transpose()
        .map_err(crate::segment::SegmentError::from)
        .map_err(StoreError::Segment)?
        .map(|postings| postings.encode_region())
        .transpose()
        .map_err(crate::segment::SegmentError::from)
        .map_err(StoreError::Segment)?;
    let metadata = source_metadata.map(|_| SegmentStoredMetadata {
        end_offsets: &metadata_offsets,
        bytes: &metadata_bytes,
    });
    let text = source_text.map(|_| SegmentStoredText {
        present: &text_present,
        end_offsets: &text_end_offsets,
        bytes: &text_bytes,
    });
    let postings = postings.as_deref().map(|bytes| SegmentPostings { bytes });
    let written = write_segment_with_documents_payloads(
        vfs,
        directory,
        build,
        SegmentPayloads {
            documents,
            metadata,
            text,
            postings,
        },
        policy,
    )
    .map_err(StoreError::Segment)?;
    let mut written = written;
    written.clustering_key_range = clustering_range(&columns, &alive)?;
    Ok(written)
}

fn rewrite_columns(source: &ColumnStore, survivors: &[usize]) -> Result<ColumnStore, StoreError> {
    let mut builder = ColumnStoreBuilder::new(source.schema().clone());
    for row in survivors {
        let row_u32 = u32::try_from(*row).map_err(|_| StoreError::ActiveRowOverflow)?;
        let timestamp = source.timestamp(row_u32).ok_or_else(|| {
            StoreError::Segment(crate::segment::SegmentError::Columns(format!(
                "timestamp row {row} is missing"
            )))
        })?;
        let mut inputs = Vec::with_capacity(source.schema().user_column_count());
        for definition in source
            .schema()
            .columns()
            .iter()
            .filter(|definition| definition.id() != TIMESTAMP_COLUMN)
        {
            let column = source.column(definition.id()).ok_or_else(|| {
                StoreError::Segment(crate::segment::SegmentError::Columns(format!(
                    "column {} is missing",
                    definition.id().get()
                )))
            })?;
            let value = match column {
                Column::U64(values) => values.get(row_u32).map(ColumnValue::U64),
                Column::I64(values) => values.get(row_u32).map(ColumnValue::I64),
                Column::F64(values) => values.get(row_u32).map(ColumnValue::F64),
                Column::Bool(values) => values.get(row_u32).map(ColumnValue::Bool),
                Column::DictionaryString(values) => values.get(row_u32).map(ColumnValue::String),
                Column::RawString(values) => values.get(row_u32).map(ColumnValue::String),
            };
            if let Some(value) = value {
                inputs.push(ColumnInput {
                    column: definition.id(),
                    value,
                });
            }
        }
        builder.push_row(timestamp, &inputs).map_err(|source| {
            StoreError::Segment(crate::segment::SegmentError::Columns(source.to_string()))
        })?;
    }
    builder.finish().map_err(|source| {
        StoreError::Segment(crate::segment::SegmentError::Columns(source.to_string()))
    })
}

fn clustering_range(
    columns: &ColumnStore,
    alive: &AliveSet,
) -> Result<ClusteringKeyRange, StoreError> {
    let mut bounds: Option<(i64, i64)> = None;
    for row in alive.iter_alive() {
        let timestamp = columns
            .timestamp(row)
            .ok_or(StoreError::ActiveRowOverflow)?;
        bounds = Some(match bounds {
            None => (timestamp, timestamp),
            Some((minimum, maximum)) => (minimum.min(timestamp), maximum.max(timestamp)),
        });
    }
    Ok(match bounds {
        Some((min_ts, max_ts)) => ClusteringKeyRange::Bounded { min_ts, max_ts },
        None => ClusteringKeyRange::Empty,
    })
}

fn replacement_segment_id(original: SegmentId, token_id: u64, generation: u64) -> SegmentId {
    let mut bytes = *original.as_bytes();
    let first = generation.to_be_bytes();
    let second = token_id.to_be_bytes();
    for (target, value) in bytes.iter_mut().take(8).zip(first) {
        *target ^= value;
    }
    for (target, value) in bytes.iter_mut().skip(8).zip(second) {
        *target ^= value;
    }
    SegmentId::from_bytes(bytes)
}

fn sweep_purge_orphans(
    vfs: &dyn Vfs,
    directory: &Path,
    manifest: &Manifest,
    policy: DurabilityPolicy,
) -> Result<(), PurgeError> {
    let reachable = manifest
        .segments
        .iter()
        .map(|segment| directory.join(segment.id.file_name()))
        .collect::<HashSet<PathBuf>>();
    let mut deleted = false;
    for path in vfs.list(directory).map_err(|source| StoreError::Io {
        path: directory.to_path_buf(),
        source,
    })? {
        let is_segment = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("segment-") && name.ends_with(".zseg"));
        let is_segment_temp = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".segment-") && name.ends_with(".zseg.tmp"));
        let is_wal_temp = path
            .file_name()
            .is_some_and(|name| name == WAL_PURGE_TEMP_FILE);
        if (is_segment && !reachable.contains(&path)) || is_segment_temp || is_wal_temp {
            vfs.delete(&path)
                .map_err(|source| StoreError::Io { path, source })?;
            deleted = true;
        }
    }
    if deleted {
        sync_directory(vfs, directory, policy)?;
    }
    Ok(())
}

fn active_wal_records(segment: &super::ActiveSegment) -> Result<WalImageRecords, PurgeError> {
    let dims = segment.dims().unwrap_or(0);
    let mut records = Vec::with_capacity(segment.row_count().saturating_add(1));
    let mut tombstoned = Vec::with_capacity(segment.row_count());
    let mut deleted = Vec::new();
    for row in 0..segment.row_count() {
        let start = row.checked_mul(dims).ok_or(StoreError::ActiveRowOverflow)?;
        let end = start
            .checked_add(dims)
            .ok_or(StoreError::ActiveRowOverflow)?;
        let version = segment.document(row).ok_or(StoreError::ActiveRowOverflow)?;
        let mut document = IngestDocument::new(
            version,
            segment
                .vectors()
                .get(start..end)
                .ok_or(StoreError::ActiveRowOverflow)?
                .to_vec(),
        )
        .with_timestamp(
            segment
                .timestamps()
                .get(row)
                .copied()
                .ok_or(StoreError::ActiveRowOverflow)?,
        )
        .with_metadata(
            segment
                .metadata(row)
                .ok_or(StoreError::ActiveRowOverflow)?
                .to_vec(),
        );
        if let Some(text) = segment.text(row)? {
            document = document.with_text(text);
        }
        document = document.with_columns(segment.column_values(row)?);
        records.push(super::encode_persisted_upsert(&document).map_err(purge_ingest_error)?);
        let is_tombstoned = segment.is_tombstoned(row);
        tombstoned.push(is_tombstoned);
        if is_tombstoned {
            deleted.push(version.doc_id());
        }
    }
    if !deleted.is_empty() {
        records.push((
            wal_payload::DELETE_V1,
            wal_payload::encode_delete(&deleted).map_err(PurgeError::WalPayload)?,
        ));
    }
    Ok((records, tombstoned))
}

fn ensure_wal_rewrite_covers_retained(
    vfs: &dyn Vfs,
    directory: &Path,
    first_seq: LogSeq,
    purged_ids: &[DocId],
    next_active: &super::ActiveSegment,
) -> Result<(), PurgeError> {
    let wal_path = directory.join("wal.ze");
    let reader = crate::wal::WalReader::open(vfs, &wal_path).map_err(StoreError::Wal)?;
    let clean = reader.into_clean().map_err(StoreError::WalRecovery)?;
    for record in clean
        .records()
        .iter()
        .filter(|record| record.seq >= first_seq)
    {
        let payload = record.payload().map_err(|source| StoreError::WalRecord {
            seq: record.seq,
            source,
        })?;
        let mutation = wal_payload::decode_mutation(record.op, payload).map_err(|source| {
            StoreError::WalMutation {
                seq: record.seq,
                op: record.op,
                source,
            }
        })?;
        let covered = match mutation {
            super::wal_payload::MutationPayload::Upsert(document) => {
                let version = document.version();
                purged_ids.contains(&version.doc_id())
                    || next_active
                        .existing(version.doc_id())
                        .is_some_and(|(_, existing, _)| existing.revision() >= version.revision())
            }
            super::wal_payload::MutationPayload::Delete(ids) => ids.iter().all(|id| {
                purged_ids.contains(id)
                    || next_active
                        .existing(*id)
                        .is_some_and(|(row, _, _)| next_active.is_tombstoned(row))
            }),
            super::wal_payload::MutationPayload::MetadataEdit(_) => false,
        };
        if !covered {
            return Err(PurgeError::WalRewriteWouldDropAcked { seq: record.seq });
        }
    }
    Ok(())
}

fn purge_ingest_error(error: super::IngestError) -> PurgeError {
    match error {
        super::IngestError::Store(error) => PurgeError::Store(error),
        super::IngestError::EpochMismatch(error) => {
            PurgeError::Store(StoreError::EpochMismatch(error))
        }
        super::IngestError::EpochUndeclared => PurgeError::Store(StoreError::EpochUndeclared),
        super::IngestError::EpochUnstamped => PurgeError::Store(StoreError::EpochUnstamped),
        super::IngestError::Payload(error) => PurgeError::WalPayload(error),
        super::IngestError::EmptyBatch => {
            PurgeError::IntentDecode("active WAL rewrite produced an empty batch error".to_owned())
        }
        super::IngestError::StaleRevision { .. } => PurgeError::IntentDecode(
            "active WAL rewrite produced a stale revision error".to_owned(),
        ),
        super::IngestError::Vector(error) => PurgeError::IntentDecode(format!(
            "active WAL rewrite rejected a persisted vector: {error}"
        )),
        super::IngestError::Lexical(error) => PurgeError::IntentDecode(format!(
            "active WAL rewrite rejected persisted text: {error}"
        )),
        super::IngestError::Tokenizer(error) => PurgeError::IntentDecode(format!(
            "active WAL rewrite rejected the frozen tokenizer: {error}"
        )),
        super::IngestError::Columns(error) => PurgeError::IntentDecode(format!(
            "active WAL rewrite rejected persisted columns: {error}"
        )),
    }
}

fn assign_rewritten_sequences(
    segment: &mut super::ActiveSegment,
    first_seq: LogSeq,
    tombstoned: &[bool],
) -> Result<(), PurgeError> {
    let delete_seq = (!tombstoned.is_empty() && tombstoned.iter().any(|value| *value))
        .then(|| LogSeq::new(first_seq.get().saturating_add(segment.row_count() as u64)));
    for row in 0..segment.row_count() {
        let row_seq = LogSeq::new(first_seq.get().saturating_add(row as u64));
        let sequence = if tombstoned.get(row).copied().unwrap_or(false) {
            delete_seq.unwrap_or(row_seq)
        } else {
            row_seq
        };
        segment.set_sequence(row, sequence)?;
    }
    Ok(())
}

#[cfg(unix)]
fn available_disk_bytes(path: &Path) -> std::io::Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let encoded = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "store path contains a NUL byte",
        )
    })?;
    let mut output = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `encoded` is NUL-terminated and `output` points to writable storage
    // for exactly one `statvfs` result initialized by a successful call.
    let result = unsafe { libc::statvfs(encoded.as_ptr(), output.as_mut_ptr()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the successful `statvfs` call initialized every field in `output`.
    let output = unsafe { output.assume_init() };
    let blocks = u128::from(output.f_bavail);
    let fragment = u128::from(output.f_frsize);
    u64::try_from(blocks.saturating_mul(fragment)).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "available filesystem bytes exceed u64",
        )
    })
}

#[cfg(not(unix))]
fn available_disk_bytes(_path: &Path) -> std::io::Result<u64> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "physical purge free-space probing requires statvfs",
    ))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{
        PURGE_INTENT_FILE, PurgeError, PurgeIntent, PurgeReport, PurgeToken, enforce_temp_space,
        purge_token_id, read_intent, read_u32, read_u64, remove_intent, write_intent,
    };
    use crate::format::FormatFamily;
    use crate::format::frame::{decode_artifact, encode_artifact};
    use crate::ingest::{DocId, wal_payload};
    use crate::lifecycle::StoreError;
    use crate::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
    use crate::vfs::StdVfs;
    use crate::wal::LogSeq;

    #[test]
    fn purge_tokens_reports_and_errors_preserve_their_contract_values() {
        let unknown = DocId::new(19);
        let token = PurgeToken {
            id: 41,
            generation: 42,
            unknown_ids: vec![unknown],
            no_op: false,
        };
        assert_eq!(token.id(), 41);
        assert_eq!(token.generation(), 42);
        assert_eq!(token.unknown_ids(), &[unknown]);
        assert!(!token.is_no_op());

        let report = PurgeReport {
            generation: 43,
            segments_rewritten: 2,
            wal_rewritten: true,
            unknown_ids: vec![unknown],
        };
        assert_eq!(report.generation(), 43);
        assert_eq!(report.segments_rewritten(), 2);
        assert!(report.wal_rewritten());
        assert_eq!(report.unknown_ids(), &[unknown]);
        assert!(!report.is_no_op());

        let format = decode_artifact("purge", FormatFamily::PurgeIntent, &[])
            .expect_err("empty intent is invalid");
        let errors = [
            PurgeError::Store(StoreError::ReadOnly),
            PurgeError::InsufficientTempSpace {
                segment_bytes: 100,
                available_bytes: 120,
                required_bytes: 120,
            },
            PurgeError::PurgeInProgress,
            PurgeError::UnknownToken { token_id: 44 },
            PurgeError::IntentFormat(format),
            PurgeError::IntentDecode("bad payload".to_owned()),
            PurgeError::WalPayload(wal_payload::PayloadError::Truncated),
            PurgeError::WalRewriteWouldDropAcked {
                seq: LogSeq::new(45),
            },
        ];
        for error in errors {
            assert!(!error.to_string().is_empty());
            let sourced = std::error::Error::source(&error).is_some();
            assert_eq!(
                sourced,
                matches!(
                    error,
                    PurgeError::Store(_) | PurgeError::IntentFormat(_) | PurgeError::WalPayload(_)
                )
            );
        }
        let converted = PurgeError::from(StoreError::Closed);
        assert!(matches!(converted, PurgeError::Store(StoreError::Closed)));

        let ids = [DocId::new(3), DocId::new(5)];
        assert_eq!(purge_token_id(7, &ids), purge_token_id(7, &ids));
        assert_ne!(purge_token_id(7, &ids), purge_token_id(8, &ids));
        assert!(enforce_temp_space(100, 121).is_ok());
        assert!(matches!(
            enforce_temp_space(100, 120),
            Err(PurgeError::InsufficientTempSpace {
                required_bytes: 120,
                ..
            })
        ));
    }

    #[test]
    fn purge_intent_round_trip_rejects_reserved_and_length_corruption() {
        let directory = tempfile::tempdir().expect("purge intent test directory");
        let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
            .expect("derived durability");
        let intent = PurgeIntent {
            token_id: 0x0102_0304_0506_0708,
            ids: vec![DocId::new(11), DocId::new(12)],
        };
        write_intent(&StdVfs, directory.path(), &intent, policy).expect("write valid intent");
        let decoded = read_intent(&StdVfs, directory.path()).expect("read valid intent");
        assert_eq!(decoded.token_id, intent.token_id);
        assert_eq!(decoded.ids, intent.ids);

        let path = directory.path().join(PURGE_INTENT_FILE);
        let bytes = std::fs::read(&path).expect("read framed intent");
        let payload = decode_artifact("purge", FormatFamily::PurgeIntent, &bytes)
            .expect("decode framed intent")
            .payload
            .to_vec();

        let mut reserved = payload.clone();
        let field = reserved.get_mut(12).expect("reserved byte exists");
        *field = 1;
        std::fs::write(
            &path,
            encode_artifact(FormatFamily::PurgeIntent, 0, &reserved),
        )
        .expect("write reserved corruption");
        assert!(matches!(
            read_intent(&StdVfs, directory.path()),
            Err(PurgeError::IntentDecode(detail)) if detail.contains("reserved")
        ));

        let truncated = payload.get(..payload.len() - 1).expect("truncate payload");
        std::fs::write(
            &path,
            encode_artifact(FormatFamily::PurgeIntent, 0, truncated),
        )
        .expect("write length corruption");
        assert!(matches!(
            read_intent(&StdVfs, directory.path()),
            Err(PurgeError::IntentDecode(detail)) if detail.contains("expected")
        ));
        assert!(read_u32(&[], 0).is_err());
        assert!(read_u64(&[], 0).is_err());

        std::fs::write(&path, bytes).expect("restore valid intent");
        remove_intent(&StdVfs, directory.path(), policy).expect("remove valid intent");
        assert!(!path.exists());
    }
}
