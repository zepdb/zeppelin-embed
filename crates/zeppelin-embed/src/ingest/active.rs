//! Exactly accounted in-RAM active-segment storage.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use crate::fts::index::{Document as LexicalDocument, SegmentIndex};
use crate::fts::tokenizer::Analyzer;
use crate::lifecycle::StoreError;
use crate::lifecycle::durability::{DurabilityPolicy, SyncRequirement};
use crate::lifecycle::stats::{Accounted, AccountedCounter, Accounting, AllocationComponent};
use crate::meta::AliveSet;
use crate::quant::{Bit4Factors, quantize_bit4};
use crate::vfs::Vfs;
use crate::wal::record::MIN_RECORD_LEN;
use crate::wal::{
    CleanWalReader, LogSeq, VisibleRecord, WalReadError, WalReader, WalWriter, encode_wal_image,
};

use super::lookup::{self, LookupSet};
use super::wal_payload::MutationPayload;
use super::{DocId, DocumentVersion, IngestDocument, IngestError, Revision, wal_payload};

/// Preserve the caller's control error while translating writer errors at the
/// active-segment boundary, without adding storage errors to the FTS API.
enum ActiveSealError<E> {
    Control(E),
    Sealed(crate::fts::sealed::SealedSegmentError),
}

impl<E> From<crate::fts::postings::PostingsError> for ActiveSealError<E> {
    fn from(error: crate::fts::postings::PostingsError) -> Self {
        Self::Sealed(crate::fts::sealed::SealedSegmentError::Postings(error))
    }
}

impl<E> From<crate::fts::sealed::SealedSegmentError> for ActiveSealError<E> {
    fn from(error: crate::fts::sealed::SealedSegmentError) -> Self {
        Self::Sealed(error)
    }
}

impl<E: From<StoreError>> ActiveSealError<E> {
    fn into_caller(self) -> E {
        match self {
            Self::Control(error) => error,
            Self::Sealed(error) => E::from(StoreError::Segment(
                crate::segment::SegmentError::Postings(error),
            )),
        }
    }
}

pub(crate) struct ActiveState {
    pub(crate) generation: u64,
    pub(crate) segment: Arc<ActiveSegment>,
}

pub(crate) struct RecoveredState {
    pub(crate) active: ActiveState,
    pub(crate) wal: Option<CleanWalReader>,
    pub(crate) tombstones: Vec<SealedTombstoneDemand>,
    #[cfg(feature = "graph-cypher")]
    pub(crate) graph: Vec<RecoveredGraphCommit>,
}

#[cfg(feature = "graph-cypher")]
pub(crate) struct RecoveredGraphCommit {
    pub(crate) seq: LogSeq,
    pub(crate) generation: u64,
    pub(crate) bytes: Vec<u8>,
}

#[derive(Clone, Copy)]
pub(crate) struct SealedTombstoneDemand {
    doc_id: DocId,
    below: Option<Revision>,
}

impl SealedTombstoneDemand {
    const fn delete(doc_id: DocId) -> Self {
        Self {
            doc_id,
            below: None,
        }
    }

    const fn upsert(version: DocumentVersion) -> Self {
        Self {
            doc_id: version.doc_id(),
            below: Some(version.revision()),
        }
    }

    pub(crate) const fn doc_id(self) -> DocId {
        self.doc_id
    }

    pub(crate) fn matches(self, version: DocumentVersion) -> bool {
        version.doc_id() == self.doc_id
            && self
                .below
                .is_none_or(|revision| version.revision() < revision)
    }
}

impl ActiveState {
    pub(crate) fn empty(generation: u64) -> Self {
        Self {
            generation,
            segment: Arc::new(ActiveSegment::empty()),
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "recovery routes two manifest watermarks"
    )]
    pub(crate) fn recover(
        vfs: &dyn Vfs,
        path: &Path,
        generation: u64,
        absorbed_through: u64,
        graph_absorbed_through: u64,
        #[cfg(feature = "graph-cypher")] graph_manifest: Option<&crate::manifest::GraphManifest>,
        #[cfg(feature = "graph-cypher")] graph_enabled: bool,
        accounting: &Arc<Accounting>,
        schema: &crate::meta::Schema,
        analyzer: &Analyzer,
        preparation: Option<&crate::lifecycle::namespace_batch::PrivatePreparation>,
    ) -> Result<RecoveredState, StoreError> {
        match vfs.open(path) {
            Ok(0) => Ok(RecoveredState {
                active: Self::empty(generation),
                wal: None,
                tombstones: Vec::new(),
                #[cfg(feature = "graph-cypher")]
                graph: Vec::new(),
            }),
            Ok(_) => {
                let reader = WalReader::open(vfs, path).map_err(StoreError::Wal)?;
                #[cfg(feature = "graph-cypher")]
                let clean = if graph_enabled {
                    reader.into_graph_prefix()
                } else {
                    reader.into_clean()
                }
                .map_err(StoreError::WalRecovery)?;
                #[cfg(not(feature = "graph-cypher"))]
                let clean = reader.into_clean().map_err(StoreError::WalRecovery)?;
                let decisions = crate::lifecycle::namespace_batch::transaction_decisions(
                    vfs,
                    path.parent().ok_or(StoreError::ActiveRowOverflow)?,
                    clean.records(),
                    absorbed_through.min(graph_absorbed_through),
                    preparation,
                )?;
                let mut recovered = Self::replay(
                    generation,
                    absorbed_through,
                    graph_absorbed_through,
                    #[cfg(feature = "graph-cypher")]
                    graph_manifest,
                    &clean,
                    #[cfg(feature = "graph-cypher")]
                    graph_enabled,
                    accounting,
                    schema,
                    analyzer,
                    &decisions,
                )?;
                let generation_watermark = absorbed_through.max(graph_absorbed_through);
                #[cfg(feature = "graph-cypher")]
                let explicit_namespace_generation = !graph_enabled;
                #[cfg(not(feature = "graph-cypher"))]
                let explicit_namespace_generation = true;
                if let Some(binding) = decisions.values().max_by_key(|binding| binding.last_seq)
                    && binding.last_seq > generation_watermark
                    && explicit_namespace_generation
                {
                    let later = super::atomic_batch::committed_batches(
                        clean.records(),
                        binding.last_seq,
                        |b| decisions.get(&b.transaction).copied(),
                    )?
                    .len() as u64;
                    recovered.active.generation = binding
                        .final_generation
                        .checked_add(later)
                        .ok_or(StoreError::GenerationOverflow)?;
                }
                {
                    recovered.wal = Some(clean);
                    Ok(recovered)
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(RecoveredState {
                active: Self::empty(generation),
                wal: None,
                tombstones: Vec::new(),
                #[cfg(feature = "graph-cypher")]
                graph: Vec::new(),
            }),
            Err(error) => Err(StoreError::Wal(WalReadError::Io(error))),
        }
    }

    /// Rebuilds the active segment from the unabsorbed WAL tail.
    ///
    /// The tail is decoded first so one working segment is sized for all of
    /// it, then every record is applied in place and the lexical index is
    /// built once at the end. Copying the segment per record, as live
    /// copy-on-write ingest does, made reopen quadratic in unsealed rows.
    #[allow(
        clippy::too_many_arguments,
        reason = "recovery routes two manifest watermarks"
    )]
    fn replay(
        mut generation: u64,
        absorbed_through: u64,
        graph_absorbed_through: u64,
        #[cfg(feature = "graph-cypher")] graph_manifest: Option<&crate::manifest::GraphManifest>,
        recovered: &CleanWalReader,
        #[cfg(feature = "graph-cypher")] graph_enabled: bool,
        accounting: &Arc<Accounting>,
        schema: &crate::meta::Schema,
        analyzer: &Analyzer,
        decisions: &std::collections::BTreeMap<u128, wal_payload::TransactionBinding>,
    ) -> Result<RecoveredState, StoreError> {
        #[cfg(feature = "graph-cypher")]
        if !graph_enabled
            && let Some(record) = recovered.records().iter().find(|record| {
                matches!(
                    record.op,
                    wal_payload::GRAPH_COMMIT_V1 | wal_payload::MIXED_BATCH_MEMBER_V1
                )
            })
        {
            return Err(StoreError::UnsupportedWalMutation {
                seq: record.seq,
                op: record.op,
            });
        }
        #[cfg(feature = "graph-cypher")]
        let mut graph = Vec::new();
        let mut documents = Vec::new();
        let mut steps = Vec::new();
        let mut deleted_ids = 0_usize;
        let batches = super::atomic_batch::committed_batches(
            recovered.records(),
            absorbed_through.min(graph_absorbed_through),
            |binding| decisions.get(&binding.transaction).copied(),
        )?;
        #[cfg(feature = "graph-cypher")]
        if let Some(boundary) = graph_manifest.and_then(|graph| graph.generation_absorbed_through) {
            // Manifest-only publishers record the physical durable end.
            // Abandoned runs contribute no generation, but their validated
            // records remain valid positions. Never split a committed batch.
            let retained_boundary = recovered
                .records()
                .iter()
                .any(|record| record.seq.get() == boundary)
                && !batches.iter().any(|batch| {
                    batch.members.first().is_some_and(|(first, _, _)| {
                        first.get() <= boundary && boundary < batch.last_seq.get()
                    })
                });
            // Physical purge can retire the whole covered prefix. Its
            // replacement WAL header pins that boundary independently.
            let retired_boundary = graph_manifest
                .is_some_and(|graph| !graph.generation_bumps.is_empty())
                && boundary.checked_add(1) == Some(recovered.retained_first_seq());
            if boundary > absorbed_through.max(graph_absorbed_through)
                && !(retained_boundary || retired_boundary)
            {
                return Err(StoreError::Manifest(
                    crate::manifest::ManifestError::Decode(
                        "generation boundary is not a committed batch boundary".to_owned(),
                    ),
                ));
            }
            // Reconstruct the generations of historical graph records too:
            // these batches still replay, but the manifest already counted them.
            let already_counted = batches
                .iter()
                .filter(|batch| {
                    batch.last_seq.get() > absorbed_through.max(graph_absorbed_through)
                        && batch.last_seq.get() <= boundary
                })
                .count();
            generation = generation
                .checked_sub(already_counted as u64)
                .ok_or(StoreError::GenerationOverflow)?;
        }
        #[cfg(feature = "graph-cypher")]
        // A cutoff alone accounts for WAL batches, not manifest-only changes.
        // Restore each such increment after the batches that preceded it so
        // historical graph envelopes keep their exact acknowledged generations.
        let mut bumps = graph_manifest
            .into_iter()
            .flat_map(|graph| graph.generation_bumps.iter().copied())
            .filter(|(sequence, _)| *sequence >= absorbed_through.max(graph_absorbed_through))
            .peekable();
        #[cfg(feature = "graph-cypher")]
        for (_, count) in bumps.clone() {
            generation = generation
                .checked_sub(count)
                .ok_or(StoreError::GenerationOverflow)?;
        }
        for batch in batches {
            #[cfg(feature = "graph-cypher")]
            while bumps
                .peek()
                .is_some_and(|(sequence, _)| *sequence < batch.last_seq.get())
            {
                let (_, count) = bumps.next().ok_or(StoreError::GenerationOverflow)?;
                generation = generation
                    .checked_add(count)
                    .ok_or(StoreError::GenerationOverflow)?;
            }
            #[cfg(feature = "graph-cypher")]
            if graph_enabled
                && let Some((first, _, _)) = batch.members.first()
                && [absorbed_through, graph_absorbed_through]
                    .into_iter()
                    .any(|mark| first.get() <= mark && mark < batch.last_seq.get())
            {
                return Err(StoreError::Manifest(
                    crate::manifest::ManifestError::Decode(
                        "watermark splits a committed batch".to_owned(),
                    ),
                ));
            }
            if batch.last_seq.get() > absorbed_through.max(graph_absorbed_through) {
                generation = generation
                    .checked_add(1)
                    .ok_or(StoreError::GenerationOverflow)?;
            }
            for (seq, op, mutation) in batch.members {
                #[cfg(feature = "graph-cypher")]
                if matches!(mutation, MutationPayload::GraphCommit(_)) && seq != batch.last_seq {
                    return Err(StoreError::WalMutation {
                        seq,
                        op,
                        source: wal_payload::PayloadError::GraphEnvelope(
                            crate::property_graph::wal::WalError::Sequence,
                        ),
                    });
                }
                let watermark = match &mutation {
                    #[cfg(feature = "graph-cypher")]
                    MutationPayload::GraphCommit(_) => graph_absorbed_through,
                    _ => absorbed_through,
                };
                if seq.get() <= watermark {
                    continue;
                }
                match mutation {
                    #[cfg(feature = "graph-cypher")]
                    MutationPayload::GraphCommit(bytes) => {
                        if !graph_enabled {
                            return Err(StoreError::UnsupportedWalMutation { seq, op });
                        }
                        graph.push(RecoveredGraphCommit {
                            seq,
                            generation,
                            bytes,
                        });
                    }
                    MutationPayload::Upsert(document) => {
                        super::validate_document_columns(schema, &document)
                            .map_err(|error| recovery_apply_error(seq, op, error))?;
                        steps.push((seq, op, ReplayStep::Upsert(documents.len())));
                        documents.push(document);
                    }
                    MutationPayload::Delete(doc_ids) => {
                        deleted_ids = deleted_ids
                            .checked_add(doc_ids.len())
                            .ok_or(StoreError::ActiveRowOverflow)?;
                        steps.push((seq, op, ReplayStep::Delete(doc_ids)));
                    }
                    MutationPayload::MetadataEdit(_)
                    | MutationPayload::BatchMember { .. }
                    | MutationPayload::MixedBatchMember { .. } => {
                        return Err(StoreError::UnsupportedWalMutation { seq, op });
                    }
                }
            }
        }
        #[cfg(feature = "graph-cypher")]
        for (_, count) in bumps {
            generation = generation
                .checked_add(count)
                .ok_or(StoreError::GenerationOverflow)?;
        }
        let mut segment = match (documents.first(), steps.first()) {
            (Some(first), Some((seq, op, _))) => ActiveSegment::empty()
                .copy_for_batch(first, &documents, accounting)
                .map_err(|error| recovery_apply_error(*seq, *op, error))?,
            _ => ActiveSegment::empty(),
        };
        segment.tombstones = copy_accounted(accounting, &segment.tombstones, deleted_ids)?;
        let mut rows = HashMap::<DocId, usize>::new();
        let mut sealed_tombstones = Vec::new();
        let last_record = steps.last().map(|(seq, op, _)| (*seq, *op));
        for (seq, op, step) in steps {
            match step {
                ReplayStep::Upsert(index) => {
                    let document = documents.get(index).ok_or(StoreError::ActiveRowOverflow)?;
                    let doc_id = document.version().doc_id();
                    let row = match rows.get(&doc_id).copied() {
                        Some(row) => {
                            let current =
                                segment.document(row).ok_or(StoreError::ActiveRowOverflow)?;
                            if document.version().revision() <= current.revision() {
                                return Err(StoreError::WalRevisionOrder {
                                    seq,
                                    doc_id,
                                    current: current.revision(),
                                    attempted: document.version().revision(),
                                });
                            }
                            segment
                                .replace_row_in_place(row, document)
                                .map_err(|error| recovery_apply_error(seq, op, error))?;
                            row
                        }
                        None => {
                            let row = segment
                                .insert_row_in_place(document)
                                .map_err(|error| recovery_apply_error(seq, op, error))?;
                            rows.insert(doc_id, row);
                            row
                        }
                    };
                    segment.set_sequence(row, seq)?;
                    sealed_tombstones.push(SealedTombstoneDemand::upsert(document.version()));
                }
                ReplayStep::Delete(doc_ids) => {
                    for doc_id in &doc_ids {
                        let Some(row) = rows.get(doc_id).copied() else {
                            continue;
                        };
                        let row_u32 =
                            u32::try_from(row).map_err(|_| StoreError::ActiveRowOverflow)?;
                        if !segment.tombstones.contains(&row_u32) {
                            segment.tombstones.push(row_u32)?;
                        }
                        segment.set_sequence(row, seq)?;
                    }
                    sealed_tombstones
                        .extend(doc_ids.iter().copied().map(SealedTombstoneDemand::delete));
                }
            }
        }
        if let Some((seq, op)) = last_record
            && segment.tracks_text()
        {
            segment.lexical = segment
                .rebuild_lexical(None, analyzer)
                .map_err(|error| recovery_apply_error(seq, op, error))?;
            segment
                .refresh_lexical_accounting(accounting)
                .map_err(|error| recovery_apply_error(seq, op, error))?;
        }
        Ok(RecoveredState {
            active: Self {
                generation,
                segment: Arc::new(segment),
            },
            wal: None,
            tombstones: sealed_tombstones,
            #[cfg(feature = "graph-cypher")]
            graph,
        })
    }
}

/// One decoded WAL record awaiting in-place replay.
enum ReplayStep {
    /// Index into the decoded upsert documents.
    Upsert(usize),
    Delete(Vec<DocId>),
}

fn recovery_apply_error(seq: LogSeq, op: u16, error: IngestError) -> StoreError {
    match error {
        IngestError::Store(error) => error,
        // Replay carries no revision conditions, so a conflict cannot arise.
        IngestError::EmptyBatch | IngestError::RevisionConflict { .. } => {
            StoreError::UnsupportedWalMutation { seq, op }
        }
        IngestError::EpochMismatch(error) => StoreError::EpochMismatch(error),
        IngestError::EpochUndeclared => StoreError::EpochUndeclared,
        IngestError::EpochUnstamped => StoreError::EpochUnstamped,
        IngestError::StaleRevision {
            doc_id,
            current,
            attempted,
        } => StoreError::WalRevisionOrder {
            seq,
            doc_id,
            current,
            attempted,
        },
        IngestError::Vector(source) => StoreError::WalVector { seq, source },
        IngestError::Lexical(source) => StoreError::WalMutation {
            seq,
            op,
            source: wal_payload::PayloadError::Lexical(source.to_string()),
        },
        IngestError::Tokenizer(source) => StoreError::WalMutation {
            seq,
            op,
            source: wal_payload::PayloadError::Lexical(source.to_string()),
        },
        IngestError::Columns(source) => StoreError::WalMutation {
            seq,
            op,
            source: wal_payload::PayloadError::Columns(source.to_string()),
        },
        IngestError::Payload(source) => StoreError::WalMutation { seq, op, source },
    }
}

pub(crate) struct ActiveSegment {
    dims: Option<usize>,
    doc_ids: Accounted<Vec<DocId>>,
    revisions: Accounted<Vec<Revision>>,
    sequences: Accounted<Vec<LogSeq>>,
    timestamps: Accounted<Vec<i64>>,
    metadata_end_offsets: Accounted<Vec<u64>>,
    metadata_bytes: Accounted<Vec<u8>>,
    text_present: Accounted<Vec<u8>>,
    text_end_offsets: Accounted<Vec<u64>>,
    text_bytes: Accounted<Vec<u8>>,
    column_end_offsets: Accounted<Vec<u64>>,
    column_bytes: Accounted<Vec<u8>>,
    lexical: SegmentIndex,
    lexical_bytes: Option<AccountedCounter>,
    sealed_lexical: OnceLock<CachedActiveLexical>,
    vectors: Accounted<Vec<f32>>,
    codes: Accounted<Vec<u8>>,
    factors: Accounted<Vec<Bit4Factors>>,
    tombstones: Accounted<Vec<u32>>,
}

struct CachedActiveLexical {
    value: Arc<crate::fts::sealed::SealedSegment>,
    accounting: AccountedCounter,
}

struct GrownCapacities {
    row: u32,
    row_stride: usize,
}

struct WorkingCapacities {
    row_count: usize,
    vector_count: usize,
    code_count: usize,
    metadata_capacity: usize,
    text_capacity: Option<usize>,
    column_capacity: Option<usize>,
}

struct ScalarRowBuffers {
    doc_ids: Accounted<Vec<DocId>>,
    revisions: Accounted<Vec<Revision>>,
    sequences: Accounted<Vec<LogSeq>>,
    timestamps: Accounted<Vec<i64>>,
}

struct RetainedCapacities {
    rows: usize,
    dims: usize,
    row_stride: usize,
    vector_capacity: usize,
    code_capacity: usize,
    metadata_capacity: usize,
    text_capacity: usize,
    column_capacity: usize,
    retained_tombstones: usize,
}

struct RetainedBuffers {
    doc_ids: Accounted<Vec<DocId>>,
    revisions: Accounted<Vec<Revision>>,
    sequences: Accounted<Vec<LogSeq>>,
    timestamps: Accounted<Vec<i64>>,
    metadata_end_offsets: Accounted<Vec<u64>>,
    metadata_bytes: Accounted<Vec<u8>>,
    text_present: Accounted<Vec<u8>>,
    text_end_offsets: Accounted<Vec<u64>>,
    text_bytes: Accounted<Vec<u8>>,
    column_end_offsets: Accounted<Vec<u64>>,
    column_bytes: Accounted<Vec<u8>>,
    vectors: Accounted<Vec<f32>>,
    codes: Accounted<Vec<u8>>,
    factors: Accounted<Vec<Bit4Factors>>,
    tombstones: Accounted<Vec<u32>>,
}

impl ActiveSegment {
    pub(super) fn empty() -> Self {
        Self {
            dims: None,
            doc_ids: Accounted::unaccounted_empty(),
            revisions: Accounted::unaccounted_empty(),
            sequences: Accounted::unaccounted_empty(),
            timestamps: Accounted::unaccounted_empty(),
            metadata_end_offsets: Accounted::unaccounted_empty(),
            metadata_bytes: Accounted::unaccounted_empty(),
            text_present: Accounted::unaccounted_empty(),
            text_end_offsets: Accounted::unaccounted_empty(),
            text_bytes: Accounted::unaccounted_empty(),
            column_end_offsets: Accounted::unaccounted_empty(),
            column_bytes: Accounted::unaccounted_empty(),
            lexical: SegmentIndex::new(),
            lexical_bytes: None,
            sealed_lexical: OnceLock::new(),
            vectors: Accounted::unaccounted_empty(),
            codes: Accounted::unaccounted_empty(),
            factors: Accounted::unaccounted_empty(),
            tombstones: Accounted::unaccounted_empty(),
        }
    }

    pub(crate) fn copy_for_batch(
        &self,
        first_document: &IngestDocument,
        remaining: &[IngestDocument],
        accounting: &Arc<Accounting>,
    ) -> Result<Self, IngestError> {
        #[cfg(feature = "allocation-audit")]
        crate::allocation_audit::record_full_segment_clone();
        let capacities = self.working_capacities(first_document, remaining)?;
        let doc_ids = copy_accounted(accounting, &self.doc_ids, capacities.row_count)?;
        let revisions = copy_accounted(accounting, &self.revisions, capacities.row_count)?;
        let sequences = copy_accounted(accounting, &self.sequences, capacities.row_count)?;
        let timestamps = copy_accounted(accounting, &self.timestamps, capacities.row_count)?;
        let metadata_end_offsets =
            copy_accounted(accounting, &self.metadata_end_offsets, capacities.row_count)?;
        let metadata_bytes = copy_accounted(
            accounting,
            &self.metadata_bytes,
            capacities.metadata_capacity,
        )?;
        let (text_present, text_end_offsets, text_bytes) =
            if let Some(text_capacity) = capacities.text_capacity {
                (
                    copy_accounted(accounting, &self.text_present, capacities.row_count)?,
                    copy_accounted(accounting, &self.text_end_offsets, capacities.row_count)?,
                    copy_accounted(accounting, &self.text_bytes, text_capacity)?,
                )
            } else {
                (
                    Accounted::unaccounted_empty(),
                    Accounted::unaccounted_empty(),
                    Accounted::unaccounted_empty(),
                )
            };
        let (column_end_offsets, column_bytes) =
            if let Some(column_capacity) = capacities.column_capacity {
                (
                    copy_accounted(accounting, &self.column_end_offsets, capacities.row_count)?,
                    copy_accounted(accounting, &self.column_bytes, column_capacity)?,
                )
            } else {
                (
                    Accounted::unaccounted_empty(),
                    Accounted::unaccounted_empty(),
                )
            };
        let vectors = copy_accounted(accounting, &self.vectors, capacities.vector_count)?;
        let codes = copy_accounted(accounting, &self.codes, capacities.code_count)?;
        let factors = copy_accounted(accounting, &self.factors, capacities.row_count)?;
        let tombstones = copy_accounted(accounting, &self.tombstones, self.tombstones.len())?;
        let lexical = self.lexical.clone();
        let lexical_bytes = self
            .lexical_bytes
            .as_ref()
            .map(|_| account_lexical(accounting, &lexical))
            .transpose()?;
        Ok(Self {
            dims: self.dims,
            doc_ids,
            revisions,
            sequences,
            timestamps,
            metadata_end_offsets,
            metadata_bytes,
            text_present,
            text_end_offsets,
            text_bytes,
            column_end_offsets,
            column_bytes,
            lexical,
            lexical_bytes,
            sealed_lexical: OnceLock::new(),
            vectors,
            codes,
            factors,
            tombstones,
        })
    }

    pub(crate) fn insert_in_place(
        &mut self,
        document: &IngestDocument,
        accounting: &Arc<Accounting>,
        analyzer: &Analyzer,
    ) -> Result<usize, IngestError> {
        self.refuse_existing(document)?;
        let tracked_text = self.tracks_text();
        let row = self.insert_row_in_place(document)?;
        if tracked_text || document.text().is_some() {
            if !tracked_text {
                for _ in 0..row {
                    self.lexical
                        .push_document(analyzer, &LexicalDocument::new())
                        .map_err(IngestError::Lexical)?;
                }
            }
            let lexical_document = document
                .text()
                .map_or_else(LexicalDocument::new, LexicalDocument::with_text);
            self.lexical
                .push_document(analyzer, &lexical_document)
                .map_err(IngestError::Lexical)?;
            self.refresh_lexical_accounting(accounting)?;
        }
        Ok(row)
    }

    /// Appends every column of one row but leaves the lexical index stale;
    /// the caller updates or rebuilds it, and has already refused an id the
    /// segment holds.
    fn insert_row_in_place(&mut self, document: &IngestDocument) -> Result<usize, IngestError> {
        let capacities = self.appended_row_capacities(document)?;
        let row = usize::try_from(capacities.row)
            .map_err(|_| IngestError::Store(StoreError::ActiveRowOverflow))?;
        let dims = document.vector().len();
        let row_stride = capacities.row_stride;

        self.doc_ids.push(document.version().doc_id())?;
        self.revisions.push(document.version().revision())?;
        self.sequences.push(LogSeq::new(0))?;
        self.timestamps.push(document.timestamp())?;
        self.metadata_bytes.extend_from_slice(document.metadata())?;
        self.metadata_end_offsets.push(
            u64::try_from(self.metadata_bytes.len())
                .map_err(|_| IngestError::Store(StoreError::ActiveRowOverflow))?,
        )?;
        self.append_text_in_place(row, document.text())?;
        self.append_columns_in_place(row, document.has_columns(), document.columns())?;
        self.vectors.extend_from_slice(document.vector())?;
        for _ in 0..row_stride {
            self.codes.push(0)?;
        }
        let start = row
            .checked_mul(row_stride)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let encoded = self
            .codes
            .as_mut_slice()
            .get_mut(start..)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let factor = quantize_bit4(document.vector(), encoded).map_err(IngestError::Vector)?;
        self.factors.push(factor)?;
        self.dims = Some(dims);
        Ok(row)
    }

    pub(crate) fn replace_in_place(
        &mut self,
        row: usize,
        document: &IngestDocument,
        accounting: &Arc<Accounting>,
        analyzer: &Analyzer,
    ) -> Result<(), IngestError> {
        let tracked_text = self.tracks_text();
        if !tracked_text && document.text().is_none() {
            return self.replace_row_in_place(row, document);
        }
        // The lexical index holds exactly the stored text, so the row's
        // current text names every posting the replacement must remove.
        let previous = if tracked_text {
            self.text(row)
                .map_err(IngestError::Store)?
                .map_or_else(LexicalDocument::new, LexicalDocument::with_text)
        } else {
            LexicalDocument::new()
        };
        self.replace_row_in_place(row, document)?;
        if !tracked_text {
            for _ in 0..self.row_count() {
                self.lexical
                    .push_document(analyzer, &LexicalDocument::new())
                    .map_err(IngestError::Lexical)?;
            }
        }
        let replacement = document
            .text()
            .map_or_else(LexicalDocument::new, LexicalDocument::with_text);
        let lexical_row =
            u32::try_from(row).map_err(|_| IngestError::Store(StoreError::ActiveRowOverflow))?;
        self.lexical
            .replace_document(analyzer, lexical_row, &previous, &replacement)
            .map_err(IngestError::Lexical)?;
        self.refresh_lexical_accounting(accounting)
    }

    /// Overwrites every column of one row but leaves the lexical index
    /// stale; the caller rebuilds it.
    fn replace_row_in_place(
        &mut self,
        row: usize,
        document: &IngestDocument,
    ) -> Result<(), IngestError> {
        let dims = self
            .dims
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        if document.vector().len() != dims {
            return Err(IngestError::Store(StoreError::DimensionMismatch {
                expected: dims,
                actual: document.vector().len(),
            }));
        }
        *self
            .doc_ids
            .as_mut_slice()
            .get_mut(row)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))? =
            document.version().doc_id();
        *self
            .revisions
            .as_mut_slice()
            .get_mut(row)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))? =
            document.version().revision();
        *self
            .sequences
            .as_mut_slice()
            .get_mut(row)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))? = LogSeq::new(0);
        *self
            .timestamps
            .as_mut_slice()
            .get_mut(row)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))? = document.timestamp();
        replace_row_bytes(
            row,
            &mut self.metadata_end_offsets,
            &mut self.metadata_bytes,
            document.metadata(),
        )?;
        self.replace_text_in_place(row, document.text())?;
        self.replace_columns_in_place(row, document.has_columns(), document.columns())?;
        let vector_start = row
            .checked_mul(dims)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let vector_end = vector_start
            .checked_add(dims)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        self.vectors
            .as_mut_slice()
            .get_mut(vector_start..vector_end)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?
            .copy_from_slice(document.vector());
        let row_stride = dims.div_ceil(2);
        let code_start = row
            .checked_mul(row_stride)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let code_end = code_start
            .checked_add(row_stride)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let code = self
            .codes
            .as_mut_slice()
            .get_mut(code_start..code_end)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let factor = quantize_bit4(document.vector(), code).map_err(IngestError::Vector)?;
        *self
            .factors
            .as_mut_slice()
            .get_mut(row)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))? = factor;
        let row_u32 =
            u32::try_from(row).map_err(|_| IngestError::Store(StoreError::ActiveRowOverflow))?;
        if let Some(position) = self
            .tombstones
            .iter()
            .position(|candidate| *candidate == row_u32)
        {
            let end = position
                .checked_add(1)
                .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
            self.tombstones.replace_range(position..end, &[])?;
        }
        Ok(())
    }

    pub(crate) fn set_sequence(&mut self, row: usize, seq: LogSeq) -> Result<(), StoreError> {
        let target = self
            .sequences
            .as_mut_slice()
            .get_mut(row)
            .ok_or(StoreError::ActiveRowOverflow)?;
        *target = seq;
        Ok(())
    }

    pub(crate) fn indexed_through_seq(&self) -> LogSeq {
        self.sequences
            .iter()
            .copied()
            .max()
            .unwrap_or(LogSeq::new(0))
    }

    pub(crate) fn tombstone(
        &self,
        doc_ids: &[DocId],
        accounting: &Arc<Accounting>,
    ) -> Result<(Self, Vec<usize>), IngestError> {
        let ScalarRowBuffers {
            doc_ids: doc_ids_buffer,
            revisions,
            sequences,
            timestamps,
        } = self.copy_scalar_rows(accounting, 0)?;
        let metadata_end_offsets = copy_accounted(
            accounting,
            &self.metadata_end_offsets,
            self.metadata_end_offsets.len(),
        )?;
        let metadata_bytes =
            copy_accounted(accounting, &self.metadata_bytes, self.metadata_bytes.len())?;
        let text_present = copy_accounted(accounting, &self.text_present, self.text_present.len())?;
        let text_end_offsets = copy_accounted(
            accounting,
            &self.text_end_offsets,
            self.text_end_offsets.len(),
        )?;
        let text_bytes = copy_accounted(accounting, &self.text_bytes, self.text_bytes.len())?;
        let column_end_offsets = copy_accounted(
            accounting,
            &self.column_end_offsets,
            self.column_end_offsets.len(),
        )?;
        let column_bytes = copy_accounted(accounting, &self.column_bytes, self.column_bytes.len())?;
        let vectors = copy_accounted(accounting, &self.vectors, self.vectors.len())?;
        let codes = copy_accounted(accounting, &self.codes, self.codes.len())?;
        let factors = copy_accounted(accounting, &self.factors, self.factors.len())?;
        // Keep the first row for each id and preserve request order, as before.
        let mut first_rows = HashMap::with_capacity(self.row_count());
        for (row, id) in self.doc_ids.iter().enumerate() {
            lookup::work(1);
            first_rows.entry(*id).or_insert(row);
        }
        let mut rows = Vec::new();
        for doc_id in doc_ids {
            lookup::work(1);
            if let Some(row) = first_rows.remove(doc_id) {
                rows.push(row);
            }
        }
        let tombstone_capacity = self
            .tombstones
            .len()
            .checked_add(rows.len())
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let mut tombstones = copy_accounted(accounting, &self.tombstones, tombstone_capacity)?;
        let mut seen = lookup::set(&self.tombstones);
        for row in &rows {
            let row = u32::try_from(*row)
                .map_err(|_| IngestError::Store(StoreError::ActiveRowOverflow))?;
            lookup::work(1);
            if seen.insert(row) {
                tombstones.push(row)?;
            }
        }
        let lexical = self.lexical.clone();
        let lexical_bytes = self
            .lexical_bytes
            .as_ref()
            .map(|_| account_lexical(accounting, &lexical))
            .transpose()?;
        Ok((
            Self {
                dims: self.dims,
                doc_ids: doc_ids_buffer,
                revisions,
                sequences,
                timestamps,
                metadata_end_offsets,
                metadata_bytes,
                text_present,
                text_end_offsets,
                text_bytes,
                column_end_offsets,
                column_bytes,
                lexical,
                lexical_bytes,
                sealed_lexical: OnceLock::new(),
                vectors,
                codes,
                factors,
                tombstones,
            },
            rows,
        ))
    }

    pub(crate) fn purge(
        &self,
        doc_ids: &[DocId],
        accounting: &Arc<Accounting>,
        analyzer: &Analyzer,
    ) -> Result<(Self, usize), StoreError> {
        let doc_ids = lookup::set(doc_ids);
        let old_tombstones = lookup::set(&self.tombstones);
        let removed = self
            .doc_ids
            .iter()
            .filter(|doc_id| lookup::contains(&doc_ids, doc_id))
            .count();
        if removed == 0 {
            return Ok((self.copy(accounting)?, 0));
        }
        let capacities = self.retained_capacities(&doc_ids, removed)?;
        let rows = capacities.rows;
        let dims = capacities.dims;
        let row_stride = capacities.row_stride;
        let tracks_text = self.tracks_text();
        let tracks_columns = self.tracks_columns();
        let RetainedBuffers {
            doc_ids: mut doc_id_rows,
            mut revisions,
            mut sequences,
            mut timestamps,
            mut metadata_end_offsets,
            mut metadata_bytes,
            mut text_present,
            mut text_end_offsets,
            mut text_bytes,
            mut column_end_offsets,
            mut column_bytes,
            mut vectors,
            mut codes,
            mut factors,
            mut tombstones,
        } = allocate_retained_buffers(accounting, &capacities, tracks_text, tracks_columns)?;
        let mut lexical = SegmentIndex::new();
        let mut next_row = 0_u32;
        for row in 0..self.row_count() {
            let doc_id = self
                .doc_ids
                .get(row)
                .copied()
                .ok_or(StoreError::ActiveRowOverflow)?;
            if lookup::contains(&doc_ids, &doc_id) {
                continue;
            }
            doc_id_rows.push(doc_id)?;
            revisions.push(
                self.revisions
                    .get(row)
                    .copied()
                    .ok_or(StoreError::ActiveRowOverflow)?,
            )?;
            sequences.push(
                self.sequences
                    .get(row)
                    .copied()
                    .ok_or(StoreError::ActiveRowOverflow)?,
            )?;
            timestamps.push(
                self.timestamps
                    .get(row)
                    .copied()
                    .ok_or(StoreError::ActiveRowOverflow)?,
            )?;
            for byte in self.metadata(row).ok_or(StoreError::ActiveRowOverflow)? {
                metadata_bytes.push(*byte)?;
            }
            metadata_end_offsets.push(
                u64::try_from(metadata_bytes.len()).map_err(|_| StoreError::ActiveRowOverflow)?,
            )?;
            if tracks_text {
                let text = self.text(row)?;
                text_present.push(u8::from(text.is_some()))?;
                if let Some(text) = text {
                    for byte in text.as_bytes() {
                        text_bytes.push(*byte)?;
                    }
                }
                text_end_offsets.push(
                    u64::try_from(text_bytes.len()).map_err(|_| StoreError::ActiveRowOverflow)?,
                )?;
                let lexical_document =
                    text.map_or_else(LexicalDocument::new, LexicalDocument::with_text);
                lexical
                    .push_document(analyzer, &lexical_document)
                    .map_err(|error| StoreError::WalMutation {
                        seq: LogSeq::new(0),
                        op: wal_payload::UPSERT_V2,
                        source: wal_payload::PayloadError::Lexical(error.to_string()),
                    })?;
            }
            if tracks_columns {
                for byte in self
                    .column_bytes_at(row)
                    .ok_or(StoreError::ActiveRowOverflow)?
                {
                    column_bytes.push(*byte)?;
                }
                column_end_offsets.push(
                    u64::try_from(column_bytes.len()).map_err(|_| StoreError::ActiveRowOverflow)?,
                )?;
            }
            let vector_start = row.checked_mul(dims).ok_or(StoreError::ActiveRowOverflow)?;
            let vector_end = vector_start
                .checked_add(dims)
                .ok_or(StoreError::ActiveRowOverflow)?;
            for value in self
                .vectors
                .get(vector_start..vector_end)
                .ok_or(StoreError::ActiveRowOverflow)?
            {
                vectors.push(*value)?;
            }
            let code_start = row
                .checked_mul(row_stride)
                .ok_or(StoreError::ActiveRowOverflow)?;
            let code_end = code_start
                .checked_add(row_stride)
                .ok_or(StoreError::ActiveRowOverflow)?;
            for byte in self
                .codes
                .get(code_start..code_end)
                .ok_or(StoreError::ActiveRowOverflow)?
            {
                codes.push(*byte)?;
            }
            factors.push(
                self.factors
                    .get(row)
                    .copied()
                    .ok_or(StoreError::ActiveRowOverflow)?,
            )?;
            let old_row = u32::try_from(row).map_err(|_| StoreError::ActiveRowOverflow)?;
            if lookup::contains(&old_tombstones, &old_row) {
                tombstones.push(next_row)?;
            }
            next_row = next_row
                .checked_add(1)
                .ok_or(StoreError::ActiveRowOverflow)?;
        }
        let lexical_bytes = (tracks_text && rows != 0)
            .then(|| account_lexical(accounting, &lexical))
            .transpose()?;
        Ok((
            Self {
                dims: (rows != 0).then_some(dims),
                doc_ids: doc_id_rows,
                revisions,
                sequences,
                timestamps,
                metadata_end_offsets,
                metadata_bytes,
                text_present,
                text_end_offsets,
                text_bytes,
                column_end_offsets,
                column_bytes,
                lexical,
                lexical_bytes,
                sealed_lexical: OnceLock::new(),
                vectors,
                codes,
                factors,
                tombstones,
            },
            removed,
        ))
    }

    fn working_capacities(
        &self,
        first_document: &IngestDocument,
        remaining: &[IngestDocument],
    ) -> Result<WorkingCapacities, IngestError> {
        let dims = self.dims.unwrap_or(first_document.vector().len());
        if dims == 0 {
            return Err(IngestError::Vector(crate::quant::QuantError::EmptyVector));
        }
        if first_document.vector().len() != dims {
            return Err(IngestError::Store(StoreError::DimensionMismatch {
                expected: dims,
                actual: first_document.vector().len(),
            }));
        }
        let row_count = self
            .row_count()
            .checked_add(remaining.len())
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let vector_count = row_count
            .checked_mul(dims)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let code_count = row_count
            .checked_mul(dims.div_ceil(2))
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let metadata_capacity =
            remaining
                .iter()
                .try_fold(self.metadata_bytes.len(), |capacity, document| {
                    capacity
                        .checked_add(document.metadata().len())
                        .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))
                })?;
        let tracks_text =
            self.tracks_text() || remaining.iter().any(|document| document.text().is_some());
        let text_capacity = tracks_text
            .then(|| {
                remaining
                    .iter()
                    .try_fold(self.text_bytes.len(), |capacity, document| {
                        capacity
                            .checked_add(document.text().map_or(0, str::len))
                            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))
                    })
            })
            .transpose()?;
        let tracks_columns =
            self.tracks_columns() || remaining.iter().any(IngestDocument::has_columns);
        let column_capacity = if tracks_columns {
            let mut capacity = if self.tracks_columns() {
                self.column_bytes.len()
            } else {
                let empty = wal_payload::encode_column_values(&[]).map_err(IngestError::Payload)?;
                self.row_count()
                    .checked_mul(empty.len())
                    .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?
            };
            for document in remaining {
                let encoded = wal_payload::encode_column_values(document.columns())
                    .map_err(IngestError::Payload)?;
                capacity = capacity
                    .checked_add(encoded.len())
                    .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
            }
            Some(capacity)
        } else {
            None
        };
        Ok(WorkingCapacities {
            row_count,
            vector_count,
            code_count,
            metadata_capacity,
            text_capacity,
            column_capacity,
        })
    }

    /// Refuses to append a row for an id the segment already holds. The scan
    /// is linear, so WAL replay, which tracks its ids itself, skips it.
    fn refuse_existing(&self, document: &IngestDocument) -> Result<(), IngestError> {
        if self.existing(document.version().doc_id()).is_some() {
            return Err(IngestError::Store(StoreError::Synchronization {
                component: "revision handling not yet admitted",
            }));
        }
        Ok(())
    }

    fn appended_row_capacities(
        &self,
        document: &IngestDocument,
    ) -> Result<GrownCapacities, IngestError> {
        let dims = document.vector().len();
        if let Some(expected) = self.dims
            && expected != dims
        {
            return Err(IngestError::Store(StoreError::DimensionMismatch {
                expected,
                actual: dims,
            }));
        }
        if dims == 0 {
            return Err(IngestError::Vector(crate::quant::QuantError::EmptyVector));
        }
        let row = u32::try_from(self.doc_ids.len())
            .map_err(|_| IngestError::Store(StoreError::ActiveRowOverflow))?;
        Ok(GrownCapacities {
            row,
            row_stride: dims.div_ceil(2),
        })
    }

    fn copy_scalar_rows(
        &self,
        accounting: &Arc<Accounting>,
        extra_rows: usize,
    ) -> Result<ScalarRowBuffers, IngestError> {
        let doc_id_capacity = self
            .doc_ids
            .len()
            .checked_add(extra_rows)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let doc_ids = copy_accounted(accounting, &self.doc_ids, doc_id_capacity)?;
        let revision_capacity = self
            .revisions
            .len()
            .checked_add(extra_rows)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let revisions = copy_accounted(accounting, &self.revisions, revision_capacity)?;
        let sequence_capacity = self
            .sequences
            .len()
            .checked_add(extra_rows)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let sequences = copy_accounted(accounting, &self.sequences, sequence_capacity)?;
        let timestamp_capacity = self
            .timestamps
            .len()
            .checked_add(extra_rows)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let timestamps = copy_accounted(accounting, &self.timestamps, timestamp_capacity)?;
        Ok(ScalarRowBuffers {
            doc_ids,
            revisions,
            sequences,
            timestamps,
        })
    }

    fn retained_capacities(
        &self,
        doc_ids: &LookupSet<DocId>,
        removed: usize,
    ) -> Result<RetainedCapacities, StoreError> {
        let rows = self.row_count().saturating_sub(removed);
        let dims = self.dims.unwrap_or(0);
        let row_stride = dims.div_ceil(2);
        let vector_capacity = rows
            .checked_mul(dims)
            .ok_or(StoreError::ActiveRowOverflow)?;
        let code_capacity = rows
            .checked_mul(row_stride)
            .ok_or(StoreError::ActiveRowOverflow)?;
        let metadata_capacity = (0..self.row_count()).try_fold(0_usize, |total, row| {
            let keep = self
                .doc_ids
                .get(row)
                .is_some_and(|doc_id| !lookup::contains(doc_ids, doc_id));
            if !keep {
                return Ok(total);
            }
            let length = self
                .metadata(row)
                .ok_or(StoreError::ActiveRowOverflow)?
                .len();
            total
                .checked_add(length)
                .ok_or(StoreError::ActiveRowOverflow)
        })?;
        let text_capacity = if self.tracks_text() {
            self.retained_row_bytes_capacity(doc_ids, &self.text_end_offsets, &self.text_bytes)?
        } else {
            0
        };
        let column_capacity = if self.tracks_columns() {
            self.retained_row_bytes_capacity(doc_ids, &self.column_end_offsets, &self.column_bytes)?
        } else {
            0
        };
        let retained_tombstones = self
            .tombstones
            .iter()
            .filter(|old_row| {
                usize::try_from(**old_row)
                    .ok()
                    .and_then(|row| self.doc_ids.get(row))
                    .is_some_and(|doc_id| !lookup::contains(doc_ids, doc_id))
            })
            .count();
        Ok(RetainedCapacities {
            rows,
            dims,
            row_stride,
            vector_capacity,
            code_capacity,
            metadata_capacity,
            text_capacity,
            column_capacity,
            retained_tombstones,
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.doc_ids.is_empty()
    }

    pub(crate) fn row_count(&self) -> usize {
        self.doc_ids.len()
    }

    pub(crate) const fn dims(&self) -> Option<usize> {
        self.dims
    }

    pub(crate) fn codes(&self) -> &[u8] {
        &self.codes
    }

    pub(crate) fn factors(&self) -> &[Bit4Factors] {
        &self.factors
    }

    pub(crate) fn doc_ids(&self) -> &[DocId] {
        &self.doc_ids
    }

    pub(crate) fn revisions(&self) -> &[Revision] {
        &self.revisions
    }

    pub(crate) fn timestamps(&self) -> &[i64] {
        &self.timestamps
    }

    pub(crate) fn vectors(&self) -> &[f32] {
        &self.vectors
    }

    pub(crate) fn metadata_end_offsets(&self) -> &[u64] {
        &self.metadata_end_offsets
    }

    pub(crate) fn metadata_bytes(&self) -> &[u8] {
        &self.metadata_bytes
    }

    pub(crate) fn text_present(&self) -> &[u8] {
        &self.text_present
    }

    pub(crate) fn text_end_offsets(&self) -> &[u64] {
        &self.text_end_offsets
    }

    pub(crate) fn text_bytes(&self) -> &[u8] {
        &self.text_bytes
    }

    pub(crate) fn metadata(&self, row: usize) -> Option<&[u8]> {
        row_bytes(&self.metadata_end_offsets, &self.metadata_bytes, row)
    }

    pub(crate) fn text(&self, row: usize) -> Result<Option<&str>, StoreError> {
        if row >= self.row_count() {
            return Err(StoreError::ActiveRowOverflow);
        }
        if !self.tracks_text() {
            return Ok(None);
        }
        let present = self
            .text_present
            .get(row)
            .copied()
            .ok_or(StoreError::ActiveRowOverflow)?;
        if present > 1 {
            return Err(StoreError::ActiveRowOverflow);
        }
        let bytes = self
            .text_bytes_at(row)
            .ok_or(StoreError::ActiveRowOverflow)?;
        if present == 0 {
            return Ok(None);
        }
        std::str::from_utf8(bytes)
            .map(Some)
            .map_err(|_| StoreError::ActiveRowOverflow)
    }

    pub(crate) fn has_text(&self) -> bool {
        self.text_present.contains(&1)
    }

    pub(crate) fn has_text_controlled<E>(
        &self,
        work: &mut crate::fts::control::WorkCheck<impl FnMut() -> Result<(), E>>,
    ) -> Result<bool, E> {
        work.check_now()?;
        for present in self.text_present.iter() {
            work.step()?;
            if *present == 1 {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn tracks_text(&self) -> bool {
        !self.text_present.is_empty()
    }

    fn tracks_columns(&self) -> bool {
        !self.column_end_offsets.is_empty()
    }

    pub(crate) const fn lexical(&self) -> &SegmentIndex {
        &self.lexical
    }

    pub(crate) fn sealed_lexical(
        &self,
        accounting: &Arc<Accounting>,
    ) -> Result<Arc<crate::fts::sealed::SealedSegment>, StoreError> {
        self.sealed_lexical_controlled(
            accounting,
            &mut crate::fts::control::WorkCheck::new(|| Ok::<(), StoreError>(())),
        )
    }

    pub(crate) fn sealed_lexical_controlled<E: From<StoreError>>(
        &self,
        accounting: &Arc<Accounting>,
        work: &mut crate::fts::control::WorkCheck<impl FnMut() -> Result<(), E>>,
    ) -> Result<Arc<crate::fts::sealed::SealedSegment>, E> {
        work.check_now()?;
        if let Some(cached) = self.sealed_lexical.get() {
            return Ok(Arc::clone(&cached.value));
        }
        let mut sealing = crate::fts::control::WorkCheck::new(|| {
            work.check_now().map_err(ActiveSealError::Control)
        });
        let sealed =
            crate::fts::sealed::SealedSegment::seal_controlled(&self.lexical, &mut sealing)
                .map_err(ActiveSealError::into_caller)?;
        let resident_bytes = sealed
            .resident_bytes_controlled(&mut sealing)
            .map_err(ActiveSealError::into_caller)?
            .checked_add(std::mem::size_of::<crate::fts::sealed::SealedSegment>())
            .and_then(|bytes| bytes.checked_add(2 * std::mem::size_of::<usize>()))
            .ok_or(StoreError::BudgetExceeded {
                needed: u64::MAX,
                budget: u64::MAX,
                component: "active segment",
            })?;
        #[cfg(any(test, feature = "test-seams"))]
        crate::segment::reader::account_active_postings_decode(resident_bytes);
        let mut cache_accounting = AccountedCounter::new(accounting, AllocationComponent::Active)?;
        cache_accounting.set(resident_bytes)?;
        let cached = CachedActiveLexical {
            value: Arc::new(sealed),
            accounting: cache_accounting,
        };
        work.check_now()?;
        if self.sealed_lexical.set(cached).is_err() {
            // A concurrent initializer won. Its value and reservation are authoritative.
        }
        self.sealed_lexical
            .get()
            .map(|cached| Arc::clone(&cached.value))
            .ok_or(StoreError::Synchronization {
                component: "active lexical query cache",
            })
            .map_err(E::from)
    }

    pub(crate) fn column_values(
        &self,
        row: usize,
    ) -> Result<Vec<(crate::meta::ColumnId, crate::meta::PredicateValue)>, StoreError> {
        if row >= self.row_count() {
            return Err(StoreError::ActiveRowOverflow);
        }
        if !self.tracks_columns() {
            return Ok(Vec::new());
        }
        let bytes = self
            .column_bytes_at(row)
            .ok_or(StoreError::ActiveRowOverflow)?;
        wal_payload::decode_column_values(bytes).map_err(|source| StoreError::WalMutation {
            seq: self.sequences.get(row).copied().unwrap_or(LogSeq::new(0)),
            op: wal_payload::UPSERT_V2,
            source,
        })
    }

    fn text_bytes_at(&self, row: usize) -> Option<&[u8]> {
        row_bytes(&self.text_end_offsets, &self.text_bytes, row)
    }

    fn column_bytes_at(&self, row: usize) -> Option<&[u8]> {
        row_bytes(&self.column_end_offsets, &self.column_bytes, row)
    }

    pub(crate) fn is_tombstoned(&self, row: usize) -> bool {
        u32::try_from(row)
            .ok()
            .is_some_and(|row| self.tombstones.contains(&row))
    }

    pub(crate) fn document(&self, row: usize) -> Option<DocumentVersion> {
        self.doc_ids
            .get(row)
            .copied()
            .zip(self.revisions.get(row).copied())
            .map(|(doc_id, revision)| DocumentVersion::new(doc_id, revision))
    }

    pub(crate) fn existing(&self, doc_id: DocId) -> Option<(usize, DocumentVersion, LogSeq)> {
        let row = self
            .doc_ids
            .iter()
            .position(|candidate| *candidate == doc_id)?;
        self.document(row)
            .zip(self.sequences.get(row).copied())
            .map(|(version, seq)| (row, version, seq))
    }

    pub(crate) fn alive(&self) -> Result<AliveSet, StoreError> {
        self.alive_controlled(&mut crate::fts::control::WorkCheck::new(|| {
            Ok::<(), StoreError>(())
        }))
    }

    pub(crate) fn alive_controlled<E: From<StoreError>>(
        &self,
        work: &mut crate::fts::control::WorkCheck<impl FnMut() -> Result<(), E>>,
    ) -> Result<AliveSet, E> {
        work.check_now()?;
        let row_count =
            u32::try_from(self.doc_ids.len()).map_err(|_| StoreError::ActiveRowOverflow)?;
        let mut alive = AliveSet::new(row_count);
        for row in self.tombstones.iter().copied() {
            work.step()?;
            alive
                .tombstone(row)
                .map_err(|_| StoreError::ActiveRowOverflow)?;
        }
        work.check_now()?;
        Ok(alive)
    }

    pub(crate) fn resident_bytes(&self) -> u64 {
        self.doc_ids
            .resident_bytes()
            .saturating_add(self.revisions.resident_bytes())
            .saturating_add(self.sequences.resident_bytes())
            .saturating_add(self.timestamps.resident_bytes())
            .saturating_add(self.metadata_end_offsets.resident_bytes())
            .saturating_add(self.metadata_bytes.resident_bytes())
            .saturating_add(self.text_present.resident_bytes())
            .saturating_add(self.text_end_offsets.resident_bytes())
            .saturating_add(self.text_bytes.resident_bytes())
            .saturating_add(self.column_end_offsets.resident_bytes())
            .saturating_add(self.column_bytes.resident_bytes())
            .saturating_add(
                self.lexical_bytes
                    .as_ref()
                    .map_or(0, AccountedCounter::bytes),
            )
            .saturating_add(
                self.sealed_lexical
                    .get()
                    .map_or(0, |cached| cached.accounting.bytes()),
            )
            .saturating_add(self.vectors.resident_bytes())
            .saturating_add(self.codes.resident_bytes())
            .saturating_add(self.factors.resident_bytes())
            .saturating_add(self.tombstones.resident_bytes())
    }

    pub(crate) fn tombstone_count(&self) -> u64 {
        u64::try_from(self.tombstones.len()).unwrap_or(u64::MAX)
    }

    pub(crate) fn tombstone_bytes(&self) -> u64 {
        self.tombstones.resident_bytes()
    }

    fn append_text_in_place(
        &mut self,
        previous_rows: usize,
        text: Option<&str>,
    ) -> Result<(), IngestError> {
        if !self.tracks_text() && text.is_none() {
            return Ok(());
        }
        if !self.tracks_text() {
            for _ in 0..previous_rows {
                self.text_present.push(0)?;
                self.text_end_offsets.push(0)?;
            }
        }
        self.text_present.push(u8::from(text.is_some()))?;
        self.text_bytes
            .extend_from_slice(text.unwrap_or("").as_bytes())?;
        self.text_end_offsets.push(
            u64::try_from(self.text_bytes.len())
                .map_err(|_| IngestError::Store(StoreError::ActiveRowOverflow))?,
        )?;
        Ok(())
    }

    fn replace_text_in_place(
        &mut self,
        row: usize,
        replacement: Option<&str>,
    ) -> Result<(), IngestError> {
        if row >= self.row_count() {
            return Err(IngestError::Store(StoreError::ActiveRowOverflow));
        }
        if !self.tracks_text() && replacement.is_none() {
            return Ok(());
        }
        if !self.tracks_text() {
            for _ in 0..self.row_count() {
                self.text_present.push(0)?;
                self.text_end_offsets.push(0)?;
            }
        }
        replace_row_bytes(
            row,
            &mut self.text_end_offsets,
            &mut self.text_bytes,
            replacement.unwrap_or("").as_bytes(),
        )?;
        *self
            .text_present
            .as_mut_slice()
            .get_mut(row)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))? =
            u8::from(replacement.is_some());
        Ok(())
    }

    fn append_columns_in_place(
        &mut self,
        previous_rows: usize,
        columns_present: bool,
        columns: &[(crate::meta::ColumnId, crate::meta::PredicateValue)],
    ) -> Result<(), IngestError> {
        if !self.tracks_columns() && !columns_present {
            return Ok(());
        }
        let encoded = wal_payload::encode_column_values(columns).map_err(IngestError::Payload)?;
        if !self.tracks_columns() {
            let empty = wal_payload::encode_column_values(&[]).map_err(IngestError::Payload)?;
            for _ in 0..previous_rows {
                self.column_bytes.extend_from_slice(&empty)?;
                self.column_end_offsets.push(
                    u64::try_from(self.column_bytes.len())
                        .map_err(|_| IngestError::Store(StoreError::ActiveRowOverflow))?,
                )?;
            }
        }
        self.column_bytes.extend_from_slice(&encoded)?;
        self.column_end_offsets.push(
            u64::try_from(self.column_bytes.len())
                .map_err(|_| IngestError::Store(StoreError::ActiveRowOverflow))?,
        )?;
        Ok(())
    }

    fn replace_columns_in_place(
        &mut self,
        row: usize,
        columns_present: bool,
        columns: &[(crate::meta::ColumnId, crate::meta::PredicateValue)],
    ) -> Result<(), IngestError> {
        if row >= self.row_count() {
            return Err(IngestError::Store(StoreError::ActiveRowOverflow));
        }
        if !self.tracks_columns() && !columns_present {
            return Ok(());
        }
        let replacement =
            wal_payload::encode_column_values(columns).map_err(IngestError::Payload)?;
        if !self.tracks_columns() {
            let empty = wal_payload::encode_column_values(&[]).map_err(IngestError::Payload)?;
            for _ in 0..self.row_count() {
                self.column_bytes.extend_from_slice(&empty)?;
                self.column_end_offsets.push(
                    u64::try_from(self.column_bytes.len())
                        .map_err(|_| IngestError::Store(StoreError::ActiveRowOverflow))?,
                )?;
            }
        }
        replace_row_bytes(
            row,
            &mut self.column_end_offsets,
            &mut self.column_bytes,
            &replacement,
        )
    }

    fn refresh_lexical_accounting(
        &mut self,
        accounting: &Arc<Accounting>,
    ) -> Result<(), IngestError> {
        self.sealed_lexical = OnceLock::new();
        if let Some(counter) = self.lexical_bytes.as_mut() {
            counter.set(self.lexical.resident_bytes())?;
        } else {
            self.lexical_bytes = Some(account_lexical(accounting, &self.lexical)?);
        }
        Ok(())
    }

    fn rebuild_lexical(
        &self,
        replacement: Option<(usize, Option<&str>)>,
        analyzer: &Analyzer,
    ) -> Result<SegmentIndex, IngestError> {
        let mut lexical = SegmentIndex::new();
        for row in 0..self.row_count() {
            let text = match replacement {
                Some((target, text)) if target == row => text,
                _ => self.text(row).map_err(IngestError::Store)?,
            };
            let document = text.map_or_else(LexicalDocument::new, LexicalDocument::with_text);
            lexical
                .push_document(analyzer, &document)
                .map_err(IngestError::Lexical)?;
        }
        Ok(lexical)
    }

    fn retained_row_bytes_capacity(
        &self,
        removed_ids: &LookupSet<DocId>,
        offsets: &[u64],
        bytes: &[u8],
    ) -> Result<usize, StoreError> {
        (0..self.row_count()).try_fold(0_usize, |total, row| {
            let keep = self
                .doc_ids
                .get(row)
                .is_some_and(|doc_id| !lookup::contains(removed_ids, doc_id));
            if !keep {
                return Ok(total);
            }
            let length = row_bytes(offsets, bytes, row)
                .ok_or(StoreError::ActiveRowOverflow)?
                .len();
            total
                .checked_add(length)
                .ok_or(StoreError::ActiveRowOverflow)
        })
    }

    fn copy(&self, accounting: &Arc<Accounting>) -> Result<Self, StoreError> {
        let lexical = self.lexical.clone();
        let lexical_bytes = self
            .lexical_bytes
            .as_ref()
            .map(|_| account_lexical(accounting, &lexical))
            .transpose()?;
        Ok(Self {
            dims: self.dims,
            doc_ids: copy_accounted(accounting, &self.doc_ids, self.doc_ids.len())?,
            revisions: copy_accounted(accounting, &self.revisions, self.revisions.len())?,
            sequences: copy_accounted(accounting, &self.sequences, self.sequences.len())?,
            timestamps: copy_accounted(accounting, &self.timestamps, self.timestamps.len())?,
            metadata_end_offsets: copy_accounted(
                accounting,
                &self.metadata_end_offsets,
                self.metadata_end_offsets.len(),
            )?,
            metadata_bytes: copy_accounted(
                accounting,
                &self.metadata_bytes,
                self.metadata_bytes.len(),
            )?,
            text_present: copy_accounted(accounting, &self.text_present, self.text_present.len())?,
            text_end_offsets: copy_accounted(
                accounting,
                &self.text_end_offsets,
                self.text_end_offsets.len(),
            )?,
            text_bytes: copy_accounted(accounting, &self.text_bytes, self.text_bytes.len())?,
            column_end_offsets: copy_accounted(
                accounting,
                &self.column_end_offsets,
                self.column_end_offsets.len(),
            )?,
            column_bytes: copy_accounted(accounting, &self.column_bytes, self.column_bytes.len())?,
            lexical,
            lexical_bytes,
            sealed_lexical: OnceLock::new(),
            vectors: copy_accounted(accounting, &self.vectors, self.vectors.len())?,
            codes: copy_accounted(accounting, &self.codes, self.codes.len())?,
            factors: copy_accounted(accounting, &self.factors, self.factors.len())?,
            tombstones: copy_accounted(accounting, &self.tombstones, self.tombstones.len())?,
        })
    }
}

fn replace_row_bytes(
    row: usize,
    offsets: &mut Accounted<Vec<u64>>,
    bytes: &mut Accounted<Vec<u8>>,
    replacement: &[u8],
) -> Result<(), IngestError> {
    let end = offsets
        .get(row)
        .copied()
        .and_then(|offset| usize::try_from(offset).ok())
        .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
    let start = if row == 0 {
        0
    } else {
        offsets
            .get(row.saturating_sub(1))
            .copied()
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?
    };
    let current_len = end
        .checked_sub(start)
        .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
    bytes.replace_range(start..end, replacement)?;
    let grows = replacement.len() >= current_len;
    let difference = replacement.len().abs_diff(current_len);
    let difference =
        u64::try_from(difference).map_err(|_| IngestError::Store(StoreError::ActiveRowOverflow))?;
    let following = offsets
        .as_mut_slice()
        .get_mut(row..)
        .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
    for offset in following {
        *offset = if grows {
            offset
                .checked_add(difference)
                .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?
        } else {
            offset
                .checked_sub(difference)
                .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?
        };
    }
    Ok(())
}

fn row_bytes<'a>(offsets: &[u64], bytes: &'a [u8], row: usize) -> Option<&'a [u8]> {
    let end = offsets
        .get(row)
        .copied()
        .and_then(|value| usize::try_from(value).ok())?;
    let start = if row == 0 {
        0
    } else {
        offsets
            .get(row.saturating_sub(1))
            .copied()
            .and_then(|value| usize::try_from(value).ok())?
    };
    bytes.get(start..end)
}

fn account_lexical(
    accounting: &Arc<Accounting>,
    lexical: &SegmentIndex,
) -> Result<AccountedCounter, StoreError> {
    let mut counter = AccountedCounter::new(accounting, AllocationComponent::Active)?;
    counter.set(lexical.resident_bytes())?;
    Ok(counter)
}

fn copy_accounted<T: Copy>(
    accounting: &Arc<Accounting>,
    source: &[T],
    capacity: usize,
) -> Result<Accounted<Vec<T>>, StoreError> {
    let mut destination =
        Accounted::try_with_capacity(accounting, capacity, AllocationComponent::Active)?;
    destination.extend_from_slice(source)?;
    Ok(destination)
}

fn allocate_retained_buffers(
    accounting: &Arc<Accounting>,
    capacities: &RetainedCapacities,
    tracks_text: bool,
    tracks_columns: bool,
) -> Result<RetainedBuffers, StoreError> {
    let rows = capacities.rows;
    let doc_ids = Accounted::try_with_capacity(accounting, rows, AllocationComponent::Active)?;
    let revisions = Accounted::try_with_capacity(accounting, rows, AllocationComponent::Active)?;
    let sequences = Accounted::try_with_capacity(accounting, rows, AllocationComponent::Active)?;
    let timestamps = Accounted::try_with_capacity(accounting, rows, AllocationComponent::Active)?;
    let metadata_end_offsets =
        Accounted::try_with_capacity(accounting, rows, AllocationComponent::Active)?;
    let metadata_bytes = Accounted::try_with_capacity(
        accounting,
        capacities.metadata_capacity,
        AllocationComponent::Active,
    )?;
    let text_present = if tracks_text {
        Accounted::try_with_capacity(accounting, rows, AllocationComponent::Active)?
    } else {
        Accounted::unaccounted_empty()
    };
    let text_end_offsets = if tracks_text {
        Accounted::try_with_capacity(accounting, rows, AllocationComponent::Active)?
    } else {
        Accounted::unaccounted_empty()
    };
    let text_bytes = if tracks_text {
        Accounted::try_with_capacity(
            accounting,
            capacities.text_capacity,
            AllocationComponent::Active,
        )?
    } else {
        Accounted::unaccounted_empty()
    };
    let column_end_offsets = if tracks_columns {
        Accounted::try_with_capacity(accounting, rows, AllocationComponent::Active)?
    } else {
        Accounted::unaccounted_empty()
    };
    let column_bytes = if tracks_columns {
        Accounted::try_with_capacity(
            accounting,
            capacities.column_capacity,
            AllocationComponent::Active,
        )?
    } else {
        Accounted::unaccounted_empty()
    };
    let vectors = Accounted::try_with_capacity(
        accounting,
        capacities.vector_capacity,
        AllocationComponent::Active,
    )?;
    let codes = Accounted::try_with_capacity(
        accounting,
        capacities.code_capacity,
        AllocationComponent::Active,
    )?;
    let factors = Accounted::try_with_capacity(accounting, rows, AllocationComponent::Active)?;
    let tombstones = Accounted::try_with_capacity(
        accounting,
        capacities.retained_tombstones,
        AllocationComponent::Active,
    )?;
    Ok(RetainedBuffers {
        doc_ids,
        revisions,
        sequences,
        timestamps,
        metadata_end_offsets,
        metadata_bytes,
        text_present,
        text_end_offsets,
        text_bytes,
        column_end_offsets,
        column_bytes,
        vectors,
        codes,
        factors,
        tombstones,
    })
}

pub(crate) struct StoreWal {
    writer: WalWriter,
    retained: AccountedCounter,
}

/// Fences every shared writer if publication fails after changing WAL or
/// generation state.
#[must_use]
pub(crate) struct ManifestPublication<'a> {
    wal: &'a StoreWal,
    armed: bool,
}

impl ManifestPublication<'_> {
    pub(crate) fn after_mutation(mut self) -> Self {
        self.armed = true;
        self
    }

    pub(crate) fn commit_manifest(
        &mut self,
        vfs: &dyn Vfs,
        directory: &Path,
        manifest: &crate::manifest::Manifest,
        policy: DurabilityPolicy,
    ) -> Result<(), crate::manifest::ManifestError> {
        // A failed temp write/sync is safe to retry for manifest-only work.
        // Once rename succeeds, a later error can leave durable state ahead.
        crate::manifest::io::commit_manifest_with_rename(vfs, directory, manifest, policy, || {
            self.armed = true;
        })
    }

    pub(crate) fn complete(mut self) {
        self.armed = false;
    }
}

impl Drop for ManifestPublication<'_> {
    fn drop(&mut self) {
        if self.armed {
            // A poisoned writer-state mutex already refuses every commit, so
            // failure to acquire it here cannot leave a usable stale writer.
            let _ = self
                .wal
                .poison_after_manifest_failure("manifest publication did not complete");
        }
    }
}

impl StoreWal {
    pub(crate) fn manifest_publication(&self) -> Result<ManifestPublication<'_>, StoreError> {
        // Writers hold the store WAL mutex: no pending group can be in flight.
        // This checks the existing shared failure state without issuing I/O.
        self.writer.flush().map_err(StoreError::WalWrite)?;
        Ok(ManifestPublication {
            wal: self,
            armed: false,
        })
    }

    pub(crate) fn poison_after_manifest_failure(&self, detail: &str) -> Result<(), StoreError> {
        self.writer
            .poison(&std::io::Error::other(detail.to_owned()))
            .map_err(StoreError::WalWrite)
    }

    #[cfg(feature = "graph-cypher")]
    pub(crate) fn io_work(&self) -> [u64; 4] {
        self.writer.io_work()
    }

    pub(crate) fn create(
        vfs: Arc<dyn Vfs>,
        directory: &Path,
        path: &Path,
        policy: DurabilityPolicy,
        accounting: &Arc<Accounting>,
    ) -> Result<Self, StoreError> {
        let writer = WalWriter::create_store_wal(vfs, directory, path, LogSeq::new(1), policy)
            .map_err(StoreError::WalWrite)?;
        Ok(Self {
            writer,
            retained: AccountedCounter::new(accounting, AllocationComponent::Wal)?,
        })
    }

    pub(crate) fn resume(
        vfs: &dyn Vfs,
        path: &Path,
        recovered: CleanWalReader,
        policy: DurabilityPolicy,
        absorbed_through: u64,
        accounting: &Arc<Accounting>,
    ) -> Result<Self, StoreError> {
        let writer =
            WalWriter::resume(vfs, path, recovered, policy).map_err(StoreError::WalWrite)?;
        if absorbed_through != 0 {
            writer
                .retire_visible_through(LogSeq::new(absorbed_through))
                .map_err(StoreError::WalRetire)?;
        }
        let mut retained = AccountedCounter::new(accounting, AllocationComponent::Wal)?;
        retained.set(writer.stats().map_err(StoreError::WalWrite)?.retained_bytes)?;
        Ok(Self { writer, retained })
    }

    pub(crate) fn abort_namespace_suffix(
        &mut self,
        vfs: &dyn Vfs,
        path: &Path,
        length: u64,
        policy: DurabilityPolicy,
        absorbed: u64,
        accounting: &Arc<Accounting>,
    ) -> Result<(), StoreError> {
        vfs.truncate(path, length)
            .map_err(|source| StoreError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        vfs.sync(path, crate::vfs::SyncKind::Full)
            .map_err(|source| StoreError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        let recovered = WalReader::open(vfs, path)
            .map_err(StoreError::Wal)?
            .into_clean()
            .map_err(StoreError::WalRecovery)?;
        *self = Self::resume(vfs, path, recovered, policy, absorbed, accounting)?;
        Ok(())
    }

    pub(crate) fn durable_end(&self) -> u64 {
        use crate::manifest::io::DurableLog as _;

        self.writer.durable_end()
    }

    /// Every retained record after `absorbed_through`, in sequence order:
    /// the WAL tail a store snapshot carries (ZE-220).
    ///
    /// The caller holds the store's WAL mutex, so no commit is in flight and
    /// every retained record is acknowledged and applied. The records share
    /// the writer's encoded allocations; nothing is copied.
    pub(crate) fn unabsorbed_records(
        &self,
        absorbed_through: u64,
    ) -> Result<Vec<VisibleRecord>, StoreError> {
        let stats = self.writer.stats().map_err(StoreError::WalWrite)?;
        let visible_end = stats.visible_end.map_or(0, LogSeq::get);
        if stats.pending_records != 0 || visible_end > stats.durable_end.map_or(0, LogSeq::get) {
            return Err(StoreError::SnapshotPin {
                detail: "the WAL holds records whose flush has not returned",
            });
        }
        let first = absorbed_through
            .checked_add(1)
            .ok_or(StoreError::SnapshotPin {
                detail: "the absorbed WAL prefix has no successor sequence",
            })?;
        let records = self
            .writer
            .visible_records(LogSeq::new(first), stats.retained_records)
            .map_err(StoreError::WalWrite)?;
        let contiguous = records
            .iter()
            .zip(first..)
            .all(|(record, expected)| record.seq.get() == expected);
        let complete = records
            .last()
            .map_or(visible_end <= absorbed_through, |last| {
                last.seq.get() == visible_end
            });
        if !contiguous || !complete {
            return Err(StoreError::SnapshotPin {
                detail: "the retained WAL tail does not continue the absorbed prefix",
            });
        }
        Ok(records)
    }

    pub(crate) fn retire_visible_through(
        &mut self,
        absorbed_through: LogSeq,
    ) -> Result<(), StoreError> {
        let retirement = self
            .writer
            .retire_visible_through(absorbed_through)
            .map_err(StoreError::WalRetire)?;
        self.retained.set(retirement.retained_bytes)?;
        Ok(())
    }

    pub(crate) fn commit(&mut self, op: u16, payload: &[u8]) -> Result<LogSeq, StoreError> {
        let current = self
            .writer
            .stats()
            .map_err(StoreError::WalWrite)?
            .retained_bytes;
        let expected = current
            .checked_add(MIN_RECORD_LEN)
            .and_then(|bytes| bytes.checked_add(payload.len()))
            .ok_or(StoreError::BudgetExceeded {
                needed: u64::MAX,
                budget: u64::MAX,
                component: "wal",
            })?;
        self.retained.set(expected)?;
        let result = self
            .writer
            .commit(op, payload)
            .map_err(StoreError::WalWrite);
        let actual = self
            .writer
            .stats()
            .map_err(StoreError::WalWrite)?
            .retained_bytes;
        self.retained.set(actual)?;
        result
    }

    pub(crate) fn commit_many(
        &mut self,
        records: &[(u16, &[u8])],
    ) -> Result<std::ops::Range<LogSeq>, StoreError> {
        let current = self
            .writer
            .stats()
            .map_err(StoreError::WalWrite)?
            .retained_bytes;
        let additional = records.iter().try_fold(0_usize, |total, (_, payload)| {
            total
                .checked_add(MIN_RECORD_LEN)
                .and_then(|bytes| bytes.checked_add(payload.len()))
                .ok_or(StoreError::BudgetExceeded {
                    needed: u64::MAX,
                    budget: u64::MAX,
                    component: "wal",
                })
        })?;
        let expected = current
            .checked_add(additional)
            .ok_or(StoreError::BudgetExceeded {
                needed: u64::MAX,
                budget: u64::MAX,
                component: "wal",
            })?;
        self.retained.set(expected)?;
        let result = self
            .writer
            .commit_many(records)
            .map_err(StoreError::WalWrite);
        let actual = self
            .writer
            .stats()
            .map_err(StoreError::WalWrite)?
            .retained_bytes;
        self.retained.set(actual)?;
        result
    }

    #[cfg(any(test, feature = "test-seams"))]
    pub(crate) fn restore_after_failed_append(
        &mut self,
        vfs: &dyn Vfs,
        path: &Path,
        policy: DurabilityPolicy,
        clean_bytes: &[u8],
    ) -> Result<(), StoreError> {
        vfs.write(path, clean_bytes)
            .map_err(|source| StoreError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        if let SyncRequirement::Sync(kind) = policy.data_file_sync() {
            vfs.sync(path, kind).map_err(|source| StoreError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let reader = WalReader::open(vfs, path).map_err(StoreError::Wal)?;
        let recovered = reader.into_clean().map_err(StoreError::WalRecovery)?;
        let replacement =
            WalWriter::resume(vfs, path, recovered, policy).map_err(StoreError::WalWrite)?;
        let retained = replacement
            .stats()
            .map_err(StoreError::WalWrite)?
            .retained_bytes;
        self.retained.set(retained)?;
        self.writer = replacement;
        Ok(())
    }

    pub(crate) fn rewrite(
        &mut self,
        vfs: &dyn Vfs,
        directory: &Path,
        policy: DurabilityPolicy,
        first_seq: LogSeq,
        records: &[(u16, Vec<u8>)],
    ) -> Result<(), StoreError> {
        let path = directory.join("wal.ze");
        let temporary = directory.join(".wal.ze.purge.tmp");
        let bytes = encode_wal_image(first_seq, records).map_err(StoreError::WalWrite)?;
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
        // Validate the image before the rename. Once the rename replaces the
        // old log, the old writer would append to a file recovery never reads,
        // so it is swapped out, or poisoned when the reopen fails, before any
        // later step can return.
        let reader = WalReader::open(vfs, &temporary).map_err(StoreError::Wal)?;
        let recovered = reader.into_clean().map_err(StoreError::WalRecovery)?;
        vfs.rename(&temporary, &path)
            .map_err(|source| StoreError::Io {
                path: path.clone(),
                source,
            })?;
        let replacement = match WalWriter::resume(vfs, &path, recovered, policy) {
            Ok(replacement) => replacement,
            Err(error) => {
                self.writer
                    .poison(&std::io::Error::other(format!(
                        "wal.ze was replaced but its writer did not reopen: {error}"
                    )))
                    .map_err(StoreError::WalWrite)?;
                return Err(StoreError::WalWrite(error));
            }
        };
        self.writer = replacement;
        self.retained.set(
            self.writer
                .stats()
                .map_err(StoreError::WalWrite)?
                .retained_bytes,
        )?;
        if let SyncRequirement::Sync(kind) = policy.directory_sync()
            && let Err(source) = vfs.sync(directory, kind)
        {
            self.writer.poison(&source).map_err(StoreError::WalWrite)?;
            return Err(StoreError::Io {
                path: directory.to_path_buf(),
                source,
            });
        }
        Ok(())
    }

    /// Replaces `wal.ze` with an empty log that continues the sequence after
    /// `absorbed_through`, once a durable manifest has absorbed every record.
    ///
    /// Recovery accepts either file at every crash point: the old log replays
    /// nothing at or below the manifest boundary, and the new log's header
    /// resumes numbering after it. A record not yet absorbed fails loudly
    /// instead of being dropped.
    pub(crate) fn truncate_absorbed(
        &mut self,
        vfs: &dyn Vfs,
        directory: &Path,
        policy: DurabilityPolicy,
        absorbed_through: LogSeq,
    ) -> Result<(), StoreError> {
        let stats = self.writer.stats().map_err(StoreError::WalWrite)?;
        if stats.retained_records != 0 {
            return Err(StoreError::WalRetire(
                crate::wal::WalRetireError::BeyondDurable {
                    requested: stats.visible_end.unwrap_or(absorbed_through),
                    durable_end: Some(absorbed_through),
                },
            ));
        }
        let first_seq = absorbed_through
            .get()
            .checked_add(1)
            .ok_or(StoreError::WalWrite(
                crate::wal::WalWriteError::SequenceExhausted,
            ))?;
        self.rewrite(vfs, directory, policy, LogSeq::new(first_seq), &[])
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::fts::bm25::Bm25Params;
    use crate::fts::index::{DEFAULT_FIELD, LexicalIndex};
    use crate::fts::search::{TermQuery, search};
    use crate::fts::tokenizer::TokenizerConfig;
    use crate::meta::DocBitmap;

    fn lookup_fixture(rows: usize) -> (ActiveSegment, Arc<Accounting>, Analyzer) {
        let accounting = Arc::new(Accounting::new(u64::MAX, u64::MAX));
        let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("analyzer");
        let documents: Vec<_> = (0..rows)
            .map(|id| texted(id as u128, 1, Some("alpha")))
            .collect();
        let mut segment = ActiveSegment::empty()
            .copy_for_batch(documents.first().expect("first"), &documents, &accounting)
            .expect("allocate");
        for document in &documents {
            segment
                .insert_in_place(document, &accounting, &analyzer)
                .expect("insert");
        }
        (segment, accounting, analyzer)
    }

    #[test]
    fn active_delete_lookup_work_is_linear() {
        for n in [128, 512, 2048] {
            let (segment, accounting, _) = lookup_fixture(n);
            let mut ids: Vec<_> = (0..n)
                .step_by(2)
                .rev()
                .map(|id| DocId::new(id as u128))
                .collect();
            ids.extend([DocId::new(0), DocId::new(u128::MAX)]);
            lookup::take_work();
            let (deleted, rows) = segment.tombstone(&ids, &accounting).expect("delete");
            let work = lookup::take_work();
            assert_eq!(rows, (0..n).step_by(2).rev().collect::<Vec<_>>());
            assert_eq!(deleted.tombstones.len(), n / 2);
            assert!(work <= 8 * (n + ids.len()), "{n} rows: {work} lookup work");
            lookup::take_work();
            let (again, _) = deleted.tombstone(&ids, &accounting).expect("repeat");
            assert_eq!(again.tombstones.len(), n / 2);
            assert!(lookup::take_work() <= 8 * (n + ids.len()));
        }
    }

    #[test]
    fn active_purge_lookup_work_is_linear() {
        for n in [128, 512, 2048] {
            let (segment, accounting, analyzer) = lookup_fixture(n);
            let deleted: Vec<_> = (0..n).step_by(2).map(|id| DocId::new(id as u128)).collect();
            let (segment, _) = segment.tombstone(&deleted, &accounting).expect("delete");
            let mut ids: Vec<_> = (0..n)
                .step_by(3)
                .rev()
                .map(|id| DocId::new(id as u128))
                .collect();
            ids.extend([DocId::new(0), DocId::new(u128::MAX)]);
            lookup::take_work();
            let (purged, removed) = segment.purge(&ids, &accounting, &analyzer).expect("purge");
            let work = lookup::take_work();
            let survivors: Vec<_> = (0..n).filter(|id| id % 3 != 0).collect();
            assert_eq!(removed, n - survivors.len());
            assert_eq!(
                purged.doc_ids(),
                survivors
                    .iter()
                    .map(|id| DocId::new(*id as u128))
                    .collect::<Vec<_>>()
            );
            for (row, id) in survivors.iter().enumerate() {
                assert_eq!(purged.is_tombstoned(row), id % 2 == 0);
                assert_eq!(purged.text(row).expect("text"), Some("alpha"));
            }
            assert!(work <= 8 * (n + ids.len()), "{n} rows: {work} lookup work");
        }
    }

    fn matching_rows(segment: Arc<crate::fts::sealed::SealedSegment>, term: &[u8]) -> Vec<u32> {
        let live_rows = DocBitmap::full(segment.row_count());
        let mut index = LexicalIndex::new();
        index
            .push_shared_with_live_rows(segment, &live_rows)
            .expect("append active lexical view");
        search(
            &index,
            &TermQuery::flat(vec![term.to_vec()], &[DEFAULT_FIELD]),
            10,
            Bm25Params::beir(),
        )
        .expect("search active lexical view")
        .hits
        .into_iter()
        .map(|hit| hit.doc.row)
        .collect()
    }

    #[test]
    fn in_place_text_mutation_invalidates_the_sealed_lexical_cache() {
        let accounting = Arc::new(Accounting::new(u64::MAX, u64::MAX));
        let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("valid analyzer");
        let first = IngestDocument::new(
            DocumentVersion::new(DocId::new(1), Revision::new(1)),
            vec![1.0, 0.0],
        )
        .with_text("alpha");
        let replacement = IngestDocument::new(
            DocumentVersion::new(DocId::new(1), Revision::new(2)),
            vec![0.0, 1.0],
        )
        .with_text("beta");
        let mut segment = ActiveSegment::empty()
            .copy_for_batch(&first, std::slice::from_ref(&first), &accounting)
            .expect("size seed segment");
        segment
            .insert_in_place(&first, &accounting, &analyzer)
            .expect("seed text row");
        let mut working = segment
            .copy_for_batch(
                &replacement,
                std::slice::from_ref(&replacement),
                &accounting,
            )
            .expect("copy batch working segment");
        let warmed = working
            .sealed_lexical(&accounting)
            .expect("warm sealed lexical cache");
        assert_eq!(matching_rows(warmed, b"alpha"), vec![0]);

        working
            .replace_in_place(0, &replacement, &accounting, &analyzer)
            .expect("replace text in place");
        let refreshed = working
            .sealed_lexical(&accounting)
            .expect("read refreshed sealed lexical cache");

        assert_eq!(matching_rows(refreshed, b"beta"), vec![0]);
    }

    fn texted(doc_id: u128, revision: u64, text: Option<&str>) -> IngestDocument {
        let document = IngestDocument::new(
            DocumentVersion::new(DocId::new(doc_id), Revision::new(revision)),
            vec![1.0, 0.5],
        );
        match text {
            Some(text) => document.with_text(text),
            None => document,
        }
    }

    /// Asserts the incremental index answers exactly as a rebuild does:
    /// sorted postings, every row's lengths, token totals, and the corpus
    /// statistics and BM25 hits of every vocabulary term over live rows.
    fn assert_index_matches_rebuild(segment: &ActiveSegment, analyzer: &Analyzer, step: usize) {
        let incremental = segment.lexical();
        let rebuilt = segment
            .rebuild_lexical(None, analyzer)
            .expect("rebuild oracle");
        assert_eq!(incremental.row_count(), rebuilt.row_count(), "step {step}");
        let ours: Vec<_> = incremental.postings().collect();
        let oracle: Vec<_> = rebuilt.postings().collect();
        assert_eq!(ours, oracle, "postings diverged at step {step}");
        for row in 0..rebuilt.row_count() {
            assert_eq!(
                incremental.field_length(row, DEFAULT_FIELD),
                rebuilt.field_length(row, DEFAULT_FIELD),
                "length of row {row} diverged at step {step}"
            );
            assert_eq!(
                incremental.document_length(row),
                rebuilt.document_length(row),
                "step {step}"
            );
        }
        assert_eq!(incremental.total_tokens(), rebuilt.total_tokens());
        let alive = segment.alive().expect("alive rows");
        let scored = |index: &SegmentIndex| {
            let sealed = crate::fts::sealed::SealedSegment::seal(index).expect("seal");
            let mut lexical = LexicalIndex::new();
            lexical
                .push_sealed_with_live_rows(sealed, alive.alive_bitmap())
                .expect("live rows");
            let hits: Vec<_> = VOCABULARY
                .iter()
                .map(|term| {
                    search(
                        &lexical,
                        &TermQuery::flat(vec![term.as_bytes().to_vec()], &[DEFAULT_FIELD]),
                        64,
                        Bm25Params::beir(),
                    )
                    .map(|result| result.hits)
                })
                .collect();
            (lexical.corpus_stats(), hits)
        };
        assert_eq!(scored(incremental), scored(&rebuilt), "step {step}");
    }

    const VOCABULARY: [&str; 10] = [
        "alpha", "beta", "gamma", "delta", "note", "meeting", "fox", "river", "stone", "cloud",
    ];

    #[test]
    #[allow(clippy::indexing_slicing)]
    fn incremental_text_index_matches_a_rebuild_after_every_step() {
        use rand::Rng;

        let accounting = Arc::new(Accounting::new(u64::MAX, u64::MAX));
        let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("valid analyzer");
        let mut rng = crate::test_support::seeded_rng("incremental_text_index_matches_a_rebuild");
        fn random_text(rng: &mut rand_chacha::ChaCha8Rng) -> Option<String> {
            match rng.random_range(0..10) {
                0 => None,
                1 => Some(String::new()),
                _ => Some(
                    (0..rng.random_range(1..12))
                        .map(|_| VOCABULARY[rng.random_range(0..VOCABULARY.len())])
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
            }
        }
        let mut segment = ActiveSegment::empty();
        let mut next_id = 0_u128;
        let mut revision = 1_u64;
        for step in 0..400 {
            revision += 1;
            let rows = segment.row_count();
            let operation = if rows == 0 {
                0
            } else {
                rng.random_range(0..10)
            };
            let text = random_text(&mut rng);
            if operation < 4 {
                next_id += 1;
                let document = texted(next_id, revision, text.as_deref());
                segment = segment
                    .copy_for_batch(&document, std::slice::from_ref(&document), &accounting)
                    .expect("copy for insert");
                segment
                    .insert_in_place(&document, &accounting, &analyzer)
                    .expect("insert");
            } else if operation < 8 {
                let row = rng.random_range(0..rows);
                let doc_id = segment.doc_ids()[row].get();
                let document = texted(doc_id, revision, text.as_deref());
                segment = segment
                    .copy_for_batch(&document, std::slice::from_ref(&document), &accounting)
                    .expect("copy for replace");
                segment
                    .replace_in_place(row, &document, &accounting, &analyzer)
                    .expect("replace");
            } else {
                let row = rng.random_range(0..rows);
                let doc_id = segment.doc_ids()[row];
                segment = segment
                    .tombstone(&[doc_id], &accounting)
                    .expect("tombstone")
                    .0;
            }
            if segment.has_text() {
                assert_index_matches_rebuild(&segment, &analyzer, step);
            }
        }
    }

    #[test]
    fn replacing_one_active_row_indexes_only_that_row() {
        fn tokens_for_one_replace(rows: u128) -> u64 {
            let directory = tempfile::tempdir().expect("store directory");
            let store = crate::lifecycle::Store::open(
                directory.path(),
                crate::lifecycle::OpenOptions::default(),
            )
            .expect("open store");
            for doc_id in 1..=rows {
                let text = format!("alpha beta gamma note {doc_id}");
                store
                    .ingest(super::super::IngestBatch::new(vec![texted(
                        doc_id,
                        1,
                        Some(&text),
                    )]))
                    .expect("append");
            }
            let before = crate::fts::index::tokens_indexed();
            store
                .ingest(super::super::IngestBatch::new(vec![texted(
                    rows / 2,
                    2,
                    Some("delta epsilon replaced"),
                )]))
                .expect("replace one active row");
            let tokens = crate::fts::index::tokens_indexed() - before;
            store.close().expect("close");
            tokens
        }

        let small = tokens_for_one_replace(8);
        let large = tokens_for_one_replace(1_000);
        assert_eq!(
            small, large,
            "one replace indexed {small} tokens at 8 rows and {large} at 1000"
        );
        assert!(
            small > 0 && small <= 10,
            "one replace indexed {small} tokens"
        );
    }
}
