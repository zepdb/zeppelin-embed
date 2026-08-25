//! Exactly accounted in-RAM active-segment storage.

use std::path::Path;
use std::sync::Arc;

use crate::lifecycle::StoreError;
use crate::lifecycle::durability::{DurabilityPolicy, SyncRequirement};
use crate::lifecycle::stats::{Accounted, AccountedCounter, Accounting, AllocationComponent};
use crate::meta::AliveSet;
use crate::quant::{Bit4Factors, quantize_bit4};
use crate::vfs::{StdVfs, Vfs};
use crate::wal::record::MIN_RECORD_LEN;
use crate::wal::{CleanWalReader, LogSeq, WalReadError, WalReader, WalWriter, encode_wal_image};

use super::wal_payload::MutationPayload;
use super::{DocId, DocumentVersion, IngestDocument, IngestError, Revision, wal_payload};

type AccountedMetadata = (Accounted<Vec<u64>>, Accounted<Vec<u8>>);

pub(crate) struct ActiveState {
    pub(crate) generation: u64,
    pub(crate) segment: Arc<ActiveSegment>,
}

impl ActiveState {
    pub(crate) fn empty(generation: u64) -> Self {
        Self {
            generation,
            segment: Arc::new(ActiveSegment::empty()),
        }
    }

    pub(crate) fn recover(
        path: &Path,
        generation: u64,
        absorbed_through: u64,
        accounting: &Arc<Accounting>,
    ) -> Result<(Self, Option<CleanWalReader>), StoreError> {
        match StdVfs.open(path) {
            Ok(0) => Ok((Self::empty(generation), None)),
            Ok(_) => {
                let reader = WalReader::open(&StdVfs, path).map_err(StoreError::Wal)?;
                let clean = reader.into_clean().map_err(StoreError::WalRecovery)?;
                let active = Self::replay(generation, absorbed_through, &clean, accounting)?;
                Ok((active, Some(clean)))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok((Self::empty(generation), None))
            }
            Err(error) => Err(StoreError::Wal(WalReadError::Io(error))),
        }
    }

    fn replay(
        mut generation: u64,
        absorbed_through: u64,
        recovered: &CleanWalReader,
        accounting: &Arc<Accounting>,
    ) -> Result<Self, StoreError> {
        let mut segment = ActiveSegment::empty();
        for record in recovered.records() {
            if record.seq.get() <= absorbed_through {
                continue;
            }
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
            segment = match mutation {
                MutationPayload::Upsert(document) => {
                    apply_recovered_upsert(&segment, &document, record.seq, record.op, accounting)?
                }
                MutationPayload::Delete(doc_ids) => {
                    let (mut next, rows) = segment
                        .tombstone(&doc_ids, accounting)
                        .map_err(|error| recovery_apply_error(record.seq, record.op, error))?;
                    for row in rows {
                        next.set_sequence(row, record.seq)?;
                    }
                    next
                }
                MutationPayload::MetadataEdit(_) => {
                    return Err(StoreError::UnsupportedWalMutation {
                        seq: record.seq,
                        op: record.op,
                    });
                }
            };
            generation = generation
                .checked_add(1)
                .ok_or(StoreError::GenerationOverflow)?;
        }
        Ok(Self {
            generation,
            segment: Arc::new(segment),
        })
    }
}

fn apply_recovered_upsert(
    segment: &ActiveSegment,
    document: &IngestDocument,
    seq: LogSeq,
    op: u16,
    accounting: &Arc<Accounting>,
) -> Result<ActiveSegment, StoreError> {
    let (mut next, row) = match segment.existing(document.version().doc_id()) {
        Some((row, current, _)) => {
            if document.version().revision() <= current.revision() {
                return Err(StoreError::WalRevisionOrder {
                    seq,
                    doc_id: document.version().doc_id(),
                    current: current.revision(),
                    attempted: document.version().revision(),
                });
            }
            (
                segment
                    .replace(row, document, accounting)
                    .map_err(|error| recovery_apply_error(seq, op, error))?,
                row,
            )
        }
        None => {
            let row = segment.row_count();
            (
                segment
                    .insert(document, accounting)
                    .map_err(|error| recovery_apply_error(seq, op, error))?,
                row,
            )
        }
    };
    next.set_sequence(row, seq)?;
    Ok(next)
}

fn recovery_apply_error(seq: LogSeq, op: u16, error: IngestError) -> StoreError {
    match error {
        IngestError::Store(error) => error,
        IngestError::EmptyBatch => StoreError::UnsupportedWalMutation { seq, op },
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
    vectors: Accounted<Vec<f32>>,
    codes: Accounted<Vec<u8>>,
    factors: Accounted<Vec<Bit4Factors>>,
    tombstones: Accounted<Vec<u32>>,
}

impl ActiveSegment {
    fn empty() -> Self {
        Self {
            dims: None,
            doc_ids: Accounted::unaccounted_empty(),
            revisions: Accounted::unaccounted_empty(),
            sequences: Accounted::unaccounted_empty(),
            timestamps: Accounted::unaccounted_empty(),
            metadata_end_offsets: Accounted::unaccounted_empty(),
            metadata_bytes: Accounted::unaccounted_empty(),
            vectors: Accounted::unaccounted_empty(),
            codes: Accounted::unaccounted_empty(),
            factors: Accounted::unaccounted_empty(),
            tombstones: Accounted::unaccounted_empty(),
        }
    }

    pub(crate) fn insert(
        &self,
        document: &IngestDocument,
        accounting: &Arc<Accounting>,
    ) -> Result<Self, IngestError> {
        if self.existing(document.version().doc_id()).is_some() {
            return Err(IngestError::Store(StoreError::Synchronization {
                component: "revision handling not yet admitted",
            }));
        }
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
        let row_count = self
            .doc_ids
            .len()
            .checked_add(1)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let vector_count = row_count
            .checked_mul(dims)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let row_stride = dims.div_ceil(2);
        let code_count = row_count
            .checked_mul(row_stride)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let mut doc_ids = copy_accounted(accounting, &self.doc_ids, row_count)?;
        let mut revisions = copy_accounted(accounting, &self.revisions, row_count)?;
        let mut sequences = copy_accounted(accounting, &self.sequences, row_count)?;
        let mut timestamps = copy_accounted(accounting, &self.timestamps, row_count)?;
        let mut metadata_end_offsets =
            copy_accounted(accounting, &self.metadata_end_offsets, row_count)?;
        let metadata_capacity = self
            .metadata_bytes
            .len()
            .checked_add(document.metadata().len())
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let mut metadata_bytes =
            copy_accounted(accounting, &self.metadata_bytes, metadata_capacity)?;
        let mut vectors = copy_accounted(accounting, &self.vectors, vector_count)?;
        let mut codes = copy_accounted(accounting, &self.codes, code_count)?;
        let mut factors = copy_accounted(accounting, &self.factors, row_count)?;
        let tombstones = copy_accounted(accounting, &self.tombstones, self.tombstones.len())?;

        doc_ids.push(document.version().doc_id())?;
        revisions.push(document.version().revision())?;
        sequences.push(LogSeq::new(0))?;
        timestamps.push(document.timestamp())?;
        for byte in document.metadata() {
            metadata_bytes.push(*byte)?;
        }
        metadata_end_offsets.push(
            u64::try_from(metadata_bytes.len())
                .map_err(|_| IngestError::Store(StoreError::ActiveRowOverflow))?,
        )?;
        for value in document.vector() {
            vectors.push(*value)?;
        }
        for _ in 0..row_stride {
            codes.push(0)?;
        }
        let start = usize::try_from(row)
            .ok()
            .and_then(|value| value.checked_mul(row_stride))
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let encoded = codes
            .as_mut_slice()
            .get_mut(start..)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let factor = quantize_bit4(document.vector(), encoded).map_err(IngestError::Vector)?;
        factors.push(factor)?;
        Ok(Self {
            dims: Some(dims),
            doc_ids,
            revisions,
            sequences,
            timestamps,
            metadata_end_offsets,
            metadata_bytes,
            vectors,
            codes,
            factors,
            tombstones,
        })
    }

    pub(crate) fn replace(
        &self,
        row: usize,
        document: &IngestDocument,
        accounting: &Arc<Accounting>,
    ) -> Result<Self, IngestError> {
        let dims = self
            .dims
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        if document.vector().len() != dims {
            return Err(IngestError::Store(StoreError::DimensionMismatch {
                expected: dims,
                actual: document.vector().len(),
            }));
        }
        let row_stride = dims.div_ceil(2);
        let mut doc_ids = copy_accounted(accounting, &self.doc_ids, self.doc_ids.len())?;
        let mut revisions = copy_accounted(accounting, &self.revisions, self.revisions.len())?;
        let mut sequences = copy_accounted(accounting, &self.sequences, self.sequences.len())?;
        let mut timestamps = copy_accounted(accounting, &self.timestamps, self.timestamps.len())?;
        let (metadata_end_offsets, metadata_bytes) =
            self.replaced_metadata(row, document.metadata(), accounting)?;
        let mut vectors = copy_accounted(accounting, &self.vectors, self.vectors.len())?;
        let mut codes = copy_accounted(accounting, &self.codes, self.codes.len())?;
        let mut factors = copy_accounted(accounting, &self.factors, self.factors.len())?;
        let row_u32 =
            u32::try_from(row).map_err(|_| IngestError::Store(StoreError::ActiveRowOverflow))?;
        let tombstone_capacity = self
            .tombstones
            .len()
            .saturating_sub(usize::from(self.tombstones.contains(&row_u32)));
        let mut tombstones = Accounted::try_with_capacity(
            accounting,
            tombstone_capacity,
            AllocationComponent::Active,
        )?;
        for tombstone in self.tombstones.iter().copied() {
            if tombstone != row_u32 {
                tombstones.push(tombstone)?;
            }
        }

        *doc_ids
            .as_mut_slice()
            .get_mut(row)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))? =
            document.version().doc_id();
        *revisions
            .as_mut_slice()
            .get_mut(row)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))? =
            document.version().revision();
        *sequences
            .as_mut_slice()
            .get_mut(row)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))? = LogSeq::new(0);
        *timestamps
            .as_mut_slice()
            .get_mut(row)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))? = document.timestamp();
        let vector_start = row
            .checked_mul(dims)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let vector_end = vector_start
            .checked_add(dims)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        vectors
            .as_mut_slice()
            .get_mut(vector_start..vector_end)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?
            .copy_from_slice(document.vector());
        let code_start = row
            .checked_mul(row_stride)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let code_end = code_start
            .checked_add(row_stride)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let code = codes
            .as_mut_slice()
            .get_mut(code_start..code_end)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let factor = quantize_bit4(document.vector(), code).map_err(IngestError::Vector)?;
        *factors
            .as_mut_slice()
            .get_mut(row)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))? = factor;
        Ok(Self {
            dims: self.dims,
            doc_ids,
            revisions,
            sequences,
            timestamps,
            metadata_end_offsets,
            metadata_bytes,
            vectors,
            codes,
            factors,
            tombstones,
        })
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
        let doc_ids_buffer = copy_accounted(accounting, &self.doc_ids, self.doc_ids.len())?;
        let revisions = copy_accounted(accounting, &self.revisions, self.revisions.len())?;
        let sequences = copy_accounted(accounting, &self.sequences, self.sequences.len())?;
        let timestamps = copy_accounted(accounting, &self.timestamps, self.timestamps.len())?;
        let metadata_end_offsets = copy_accounted(
            accounting,
            &self.metadata_end_offsets,
            self.metadata_end_offsets.len(),
        )?;
        let metadata_bytes =
            copy_accounted(accounting, &self.metadata_bytes, self.metadata_bytes.len())?;
        let vectors = copy_accounted(accounting, &self.vectors, self.vectors.len())?;
        let codes = copy_accounted(accounting, &self.codes, self.codes.len())?;
        let factors = copy_accounted(accounting, &self.factors, self.factors.len())?;
        let mut rows = Vec::new();
        for doc_id in doc_ids {
            if let Some(row) = self
                .doc_ids
                .iter()
                .position(|candidate| candidate == doc_id)
                && !rows.contains(&row)
            {
                rows.push(row);
            }
        }
        let tombstone_capacity = self
            .tombstones
            .len()
            .checked_add(rows.len())
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let mut tombstones = copy_accounted(accounting, &self.tombstones, tombstone_capacity)?;
        for row in &rows {
            let row = u32::try_from(*row)
                .map_err(|_| IngestError::Store(StoreError::ActiveRowOverflow))?;
            if !tombstones.contains(&row) {
                tombstones.push(row)?;
            }
        }
        Ok((
            Self {
                dims: self.dims,
                doc_ids: doc_ids_buffer,
                revisions,
                sequences,
                timestamps,
                metadata_end_offsets,
                metadata_bytes,
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
    ) -> Result<(Self, usize), StoreError> {
        let removed = self
            .doc_ids
            .iter()
            .filter(|doc_id| doc_ids.contains(doc_id))
            .count();
        if removed == 0 {
            return Ok((self.copy(accounting)?, 0));
        }
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
                .is_some_and(|doc_id| !doc_ids.contains(doc_id));
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
        let mut doc_id_rows =
            Accounted::try_with_capacity(accounting, rows, AllocationComponent::Active)?;
        let mut revisions =
            Accounted::try_with_capacity(accounting, rows, AllocationComponent::Active)?;
        let mut sequences =
            Accounted::try_with_capacity(accounting, rows, AllocationComponent::Active)?;
        let mut timestamps =
            Accounted::try_with_capacity(accounting, rows, AllocationComponent::Active)?;
        let mut metadata_end_offsets =
            Accounted::try_with_capacity(accounting, rows, AllocationComponent::Active)?;
        let mut metadata_bytes = Accounted::try_with_capacity(
            accounting,
            metadata_capacity,
            AllocationComponent::Active,
        )?;
        let mut vectors =
            Accounted::try_with_capacity(accounting, vector_capacity, AllocationComponent::Active)?;
        let mut codes =
            Accounted::try_with_capacity(accounting, code_capacity, AllocationComponent::Active)?;
        let mut factors =
            Accounted::try_with_capacity(accounting, rows, AllocationComponent::Active)?;
        let retained_tombstones = self
            .tombstones
            .iter()
            .filter(|old_row| {
                usize::try_from(**old_row)
                    .ok()
                    .and_then(|row| self.doc_ids.get(row))
                    .is_some_and(|doc_id| !doc_ids.contains(doc_id))
            })
            .count();
        let mut tombstones = Accounted::try_with_capacity(
            accounting,
            retained_tombstones,
            AllocationComponent::Active,
        )?;
        let mut next_row = 0_u32;
        for row in 0..self.row_count() {
            let doc_id = self
                .doc_ids
                .get(row)
                .copied()
                .ok_or(StoreError::ActiveRowOverflow)?;
            if doc_ids.contains(&doc_id) {
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
            if self.tombstones.contains(&old_row) {
                tombstones.push(next_row)?;
            }
            next_row = next_row
                .checked_add(1)
                .ok_or(StoreError::ActiveRowOverflow)?;
        }
        Ok((
            Self {
                dims: (rows != 0).then_some(dims),
                doc_ids: doc_id_rows,
                revisions,
                sequences,
                timestamps,
                metadata_end_offsets,
                metadata_bytes,
                vectors,
                codes,
                factors,
                tombstones,
            },
            removed,
        ))
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

    pub(crate) fn metadata(&self, row: usize) -> Option<&[u8]> {
        let end = self
            .metadata_end_offsets
            .get(row)
            .copied()
            .and_then(|value| usize::try_from(value).ok())?;
        let start = if row == 0 {
            0
        } else {
            self.metadata_end_offsets
                .get(row.saturating_sub(1))
                .copied()
                .and_then(|value| usize::try_from(value).ok())?
        };
        self.metadata_bytes.get(start..end)
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
        let row_count =
            u32::try_from(self.doc_ids.len()).map_err(|_| StoreError::ActiveRowOverflow)?;
        let mut alive = AliveSet::new(row_count);
        for row in self.tombstones.iter().copied() {
            alive
                .tombstone(row)
                .map_err(|_| StoreError::ActiveRowOverflow)?;
        }
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

    fn replaced_metadata(
        &self,
        row: usize,
        replacement: &[u8],
        accounting: &Arc<Accounting>,
    ) -> Result<AccountedMetadata, IngestError> {
        if row >= self.row_count() {
            return Err(IngestError::Store(StoreError::ActiveRowOverflow));
        }
        let current = self
            .metadata(row)
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let byte_capacity = self
            .metadata_bytes
            .len()
            .checked_sub(current.len())
            .and_then(|value| value.checked_add(replacement.len()))
            .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?;
        let mut end_offsets = Accounted::try_with_capacity(
            accounting,
            self.row_count(),
            AllocationComponent::Active,
        )?;
        let mut bytes =
            Accounted::try_with_capacity(accounting, byte_capacity, AllocationComponent::Active)?;
        for current_row in 0..self.row_count() {
            let value = if current_row == row {
                replacement
            } else {
                self.metadata(current_row)
                    .ok_or(IngestError::Store(StoreError::ActiveRowOverflow))?
            };
            for byte in value {
                bytes.push(*byte)?;
            }
            end_offsets.push(
                u64::try_from(bytes.len())
                    .map_err(|_| IngestError::Store(StoreError::ActiveRowOverflow))?,
            )?;
        }
        Ok((end_offsets, bytes))
    }

    fn copy(&self, accounting: &Arc<Accounting>) -> Result<Self, StoreError> {
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
            vectors: copy_accounted(accounting, &self.vectors, self.vectors.len())?,
            codes: copy_accounted(accounting, &self.codes, self.codes.len())?,
            factors: copy_accounted(accounting, &self.factors, self.factors.len())?,
            tombstones: copy_accounted(accounting, &self.tombstones, self.tombstones.len())?,
        })
    }
}

fn copy_accounted<T: Copy>(
    accounting: &Arc<Accounting>,
    source: &[T],
    capacity: usize,
) -> Result<Accounted<Vec<T>>, StoreError> {
    let mut destination =
        Accounted::try_with_capacity(accounting, capacity, AllocationComponent::Active)?;
    for value in source {
        destination.push(*value)?;
    }
    Ok(destination)
}

pub(crate) struct StoreWal {
    writer: WalWriter,
    retained: AccountedCounter,
}

impl StoreWal {
    pub(crate) fn create(
        path: &Path,
        policy: DurabilityPolicy,
        accounting: &Arc<Accounting>,
    ) -> Result<Self, StoreError> {
        let writer = WalWriter::create(&StdVfs, path, LogSeq::new(1), policy)
            .map_err(StoreError::WalWrite)?;
        Ok(Self {
            writer,
            retained: AccountedCounter::new(accounting, AllocationComponent::Wal)?,
        })
    }

    pub(crate) fn resume(
        path: &Path,
        recovered: CleanWalReader,
        policy: DurabilityPolicy,
        absorbed_through: u64,
        accounting: &Arc<Accounting>,
    ) -> Result<Self, StoreError> {
        let writer =
            WalWriter::resume(&StdVfs, path, recovered, policy).map_err(StoreError::WalWrite)?;
        if absorbed_through != 0 {
            writer
                .retire_visible_through(LogSeq::new(absorbed_through))
                .map_err(StoreError::WalRetire)?;
        }
        let mut retained = AccountedCounter::new(accounting, AllocationComponent::Wal)?;
        retained.set(writer.stats().map_err(StoreError::WalWrite)?.retained_bytes)?;
        Ok(Self { writer, retained })
    }

    pub(crate) fn durable_end(&self) -> u64 {
        use crate::manifest::io::DurableLog as _;

        self.writer.durable_end()
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
        vfs.rename(&temporary, &path)
            .map_err(|source| StoreError::Io {
                path: path.clone(),
                source,
            })?;
        if let SyncRequirement::Sync(kind) = policy.directory_sync() {
            vfs.sync(directory, kind).map_err(|source| StoreError::Io {
                path: directory.to_path_buf(),
                source,
            })?;
        }
        let reader = WalReader::open(vfs, &path).map_err(StoreError::Wal)?;
        let recovered = reader.into_clean().map_err(StoreError::WalRecovery)?;
        let replacement =
            WalWriter::resume(vfs, &path, recovered, policy).map_err(StoreError::WalWrite)?;
        let retained = replacement
            .stats()
            .map_err(StoreError::WalWrite)?
            .retained_bytes;
        self.retained.set(retained)?;
        self.writer = replacement;
        Ok(())
    }
}
