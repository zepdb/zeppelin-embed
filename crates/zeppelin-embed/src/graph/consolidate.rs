//! Copy-on-write consolidation of sealed graph segments (Task 19-M8).
//!
//! Consolidation merges every published sealed graph segment's live rows
//! into one new sealed segment with new dense row ids, rebuilds every
//! derived region (postings are retokenized from carried stored text, the
//! graph is rebuilt over the union), and atomically publishes one N-to-1
//! manifest replacement. Sealed inputs are never mutated in place; the
//! manifest commit precedes every unlink, exactly like purge replacement.
//!
//! The only persisted artifact this module owns is the private
//! `.consolidate.checkpoint` resumability file. It is never read by
//! queries and is refused-and-cleared on any validation failure.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use xxhash_rust::xxh3::{xxh3_64, xxh3_64_with_seed};

use crate::fts::index::{Document as LexicalDocument, SegmentIndex};
use crate::fts::sealed::SealedSegment;
use crate::fts::tokenizer::Analyzer;
use crate::ingest::DocId;
use crate::lifecycle::StoreError;
use crate::lifecycle::durability::{DurabilityPolicy, SyncRequirement};
use crate::meta::{AliveSet, ColumnStoreBuilder};
use crate::segment::layout::RegionKind;
use crate::segment::reader::SegmentReader;
use crate::segment::writer::{
    SegmentBuild, SegmentDocumentVersions, SegmentPayloads, SegmentPostings, SegmentStoredMetadata,
    SegmentStoredText, write_segment, write_segment_with_documents_payloads,
};
use crate::segment::{ClusteringKeyRange, SegmentError, SegmentId};
use crate::vfs::Vfs;

/// Fixed private name of the store's single consolidation checkpoint.
pub(crate) const CONSOLIDATION_CHECKPOINT_FILE: &str = ".consolidate.checkpoint";
/// Magic prefix of every consolidation checkpoint.
pub const CONSOLIDATION_CHECKPOINT_MAGIC: [u8; 8] = *b"ZECONCP1";
const CHECKPOINT_VERSION: u16 = 1;
/// magic + version + reserved + input_count.
const CHECKPOINT_PREFIX_BYTES: usize = 16;
/// merge id + output id + rows_emitted + cursor pair + output hash + checksum.
const CHECKPOINT_TAIL_BYTES: usize = 16 + 16 + 8 + 4 + 4 + 8 + 8;
/// Manifest transitions never carry more sealed segments than this.
const CHECKPOINT_MAX_INPUTS: u32 = 65_536;
const MERGE_ID_SEED: u64 = 0x0019_4d38_0000_0001;
const OUTPUT_ID_SEED: u64 = 0x0019_4d38_0000_0002;

/// Typed failure of the consolidation merge pass or its checkpoint.
#[derive(Debug)]
pub enum ConsolidateError {
    /// Reading or writing a segment failed its validated contract.
    Segment(SegmentError),
    /// Store lifecycle, accounting, or filesystem failure.
    Store(StoreError),
    /// Checked geometry over merged rows was violated.
    Geometry(String),
    /// Two live rows across the merge inputs share one document id.
    DuplicateDocument {
        /// Application document id present in more than one live input row.
        doc_id: u128,
    },
    /// Rebuilding the merged lexical region failed.
    Lexical(String),
    /// Reading, replacing, or removing the consolidation checkpoint failed.
    CheckpointIo {
        /// Checkpoint or checkpoint-temporary path.
        path: PathBuf,
        /// Underlying filesystem failure.
        source: std::io::Error,
    },
    /// Checkpoint bytes failed their closed contract.
    CheckpointCorrupt(String),
}

impl std::fmt::Display for ConsolidateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Segment(error) => write!(formatter, "consolidation segment: {error}"),
            Self::Store(error) => write!(formatter, "consolidation store: {error}"),
            Self::Geometry(detail) => write!(formatter, "consolidation geometry: {detail}"),
            Self::DuplicateDocument { doc_id } => write!(
                formatter,
                "consolidation found document {doc_id} live in more than one input segment"
            ),
            Self::Lexical(detail) => write!(formatter, "consolidation lexical rebuild: {detail}"),
            Self::CheckpointIo { path, source } => write!(
                formatter,
                "consolidation checkpoint I/O {}: {source}",
                path.display()
            ),
            Self::CheckpointCorrupt(detail) => {
                write!(formatter, "consolidation checkpoint is corrupt: {detail}")
            }
        }
    }
}

impl std::error::Error for ConsolidateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Segment(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::CheckpointIo { source, .. } => Some(source),
            Self::Geometry(_)
            | Self::DuplicateDocument { .. }
            | Self::Lexical(_)
            | Self::CheckpointCorrupt(_) => None,
        }
    }
}

impl From<SegmentError> for ConsolidateError {
    fn from(error: SegmentError) -> Self {
        Self::Segment(error)
    }
}

impl From<StoreError> for ConsolidateError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// Whether the merge pass can carry one source region kind.
///
/// Everything the merge re-encodes, rebuilds, or replaces is supported;
/// reserved accelerator regions and forward-compatible unknown kinds defer
/// consolidation because their bytes cannot be re-derived for new row ids.
#[must_use]
pub(crate) const fn consolidation_supports_region(kind: u16) -> bool {
    matches!(
        RegionKind::from_id(kind),
        Some(
            RegionKind::Columns
                | RegionKind::Alive
                | RegionKind::VectorCodes
                | RegionKind::VectorFactors
                | RegionKind::VectorRescore
                | RegionKind::DocumentVersions
                | RegionKind::GraphNodeBlocks
                | RegionKind::ChecksumTable
                | RegionKind::Postings
                | RegionKind::StoredMetadata
                | RegionKind::StoredText,
        )
    )
}

/// Deterministic unpublished merge-intermediate id for one input set.
#[must_use]
pub(crate) fn consolidation_merge_id(inputs: &[SegmentId]) -> SegmentId {
    derived_segment_id(inputs, MERGE_ID_SEED)
}

/// Deterministic published output id for one input set.
#[must_use]
pub(crate) fn consolidation_output_id(inputs: &[SegmentId]) -> SegmentId {
    derived_segment_id(inputs, OUTPUT_ID_SEED)
}

fn derived_segment_id(inputs: &[SegmentId], seed: u64) -> SegmentId {
    let mut input_bytes = Vec::with_capacity(inputs.len().saturating_mul(16));
    for id in inputs {
        input_bytes.extend_from_slice(id.as_bytes());
    }
    let first = xxh3_64_with_seed(&input_bytes, seed).to_be_bytes();
    let second = xxh3_64_with_seed(&input_bytes, !seed).to_be_bytes();
    let mut bytes = [0_u8; 16];
    if let Some(prefix) = bytes.get_mut(..8) {
        prefix.copy_from_slice(&first);
    }
    if let Some(suffix) = bytes.get_mut(8..) {
        suffix.copy_from_slice(&second);
    }
    SegmentId::from_bytes(bytes)
}

/// Durable progress marker between the merge pass and the graph build.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConsolidationCheckpoint {
    /// Sealed input segment ids, strictly ascending.
    pub(crate) inputs: Vec<SegmentId>,
    /// Unpublished merge-intermediate segment id.
    pub(crate) merge_id: SegmentId,
    /// Output segment id the eventual publish will use.
    pub(crate) output_id: SegmentId,
    /// Live rows emitted into the intermediate segment.
    pub(crate) rows_emitted: u64,
    /// Xxh3-64 over the complete intermediate segment file bytes.
    pub(crate) output_hash: u64,
}

/// Encodes one checkpoint in the frozen `ZECONCP1` little-endian layout.
pub(crate) fn encode_consolidation_checkpoint(
    checkpoint: &ConsolidationCheckpoint,
) -> Result<Vec<u8>, ConsolidateError> {
    let input_count = u32::try_from(checkpoint.inputs.len())
        .map_err(|_| ConsolidateError::Geometry("checkpoint input count exceeds u32".to_owned()))?;
    if input_count == 0 || input_count > CHECKPOINT_MAX_INPUTS {
        return Err(ConsolidateError::Geometry(format!(
            "checkpoint input count {input_count} is outside 1..={CHECKPOINT_MAX_INPUTS}"
        )));
    }
    let total = CHECKPOINT_PREFIX_BYTES
        .checked_add(checkpoint.inputs.len().saturating_mul(16))
        .and_then(|bytes| bytes.checked_add(CHECKPOINT_TAIL_BYTES))
        .ok_or_else(|| ConsolidateError::Geometry("checkpoint length overflow".to_owned()))?;
    let mut bytes = Vec::with_capacity(total);
    bytes.extend_from_slice(&CONSOLIDATION_CHECKPOINT_MAGIC);
    bytes.extend_from_slice(&CHECKPOINT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes.extend_from_slice(&input_count.to_le_bytes());
    for id in &checkpoint.inputs {
        bytes.extend_from_slice(id.as_bytes());
    }
    bytes.extend_from_slice(checkpoint.merge_id.as_bytes());
    bytes.extend_from_slice(checkpoint.output_id.as_bytes());
    bytes.extend_from_slice(&checkpoint.rows_emitted.to_le_bytes());
    // Version 1 admits the merge pass whole, so the persisted cursor is
    // always terminal: segment cursor at input_count, row cursor at zero.
    bytes.extend_from_slice(&input_count.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&checkpoint.output_hash.to_le_bytes());
    bytes.extend_from_slice(&xxh3_64(&bytes).to_le_bytes());
    if bytes.len() != total {
        return Err(ConsolidateError::Geometry(format!(
            "checkpoint encoded {} bytes, expected {total}",
            bytes.len()
        )));
    }
    Ok(bytes)
}

/// Decodes and fully validates one `ZECONCP1` checkpoint.
pub(crate) fn decode_consolidation_checkpoint(
    bytes: &[u8],
) -> Result<ConsolidationCheckpoint, ConsolidateError> {
    let corrupt = |detail: String| ConsolidateError::CheckpointCorrupt(detail);
    let minimum = CHECKPOINT_PREFIX_BYTES
        .checked_add(16)
        .and_then(|prefix| prefix.checked_add(CHECKPOINT_TAIL_BYTES))
        .ok_or_else(|| corrupt("minimum length overflow".to_owned()))?;
    if bytes.len() < minimum {
        return Err(corrupt(format!(
            "truncated: need at least {minimum} bytes, got {}",
            bytes.len()
        )));
    }
    if bytes.get(..8) != Some(CONSOLIDATION_CHECKPOINT_MAGIC.as_slice()) {
        return Err(corrupt("magic does not match ZECONCP1".to_owned()));
    }
    let checksum_start = bytes
        .len()
        .checked_sub(8)
        .ok_or_else(|| corrupt("checksum underflow".to_owned()))?;
    let stored_checksum = read_u64(bytes, checksum_start)?;
    let checksummed = bytes
        .get(..checksum_start)
        .ok_or_else(|| corrupt("checksummed prefix is unavailable".to_owned()))?;
    let actual_checksum = xxh3_64(checksummed);
    if stored_checksum != actual_checksum {
        return Err(corrupt(format!(
            "xxh3-64 expected {stored_checksum:#018x}, computed {actual_checksum:#018x}"
        )));
    }
    let version = read_u16(bytes, 8)?;
    if version != CHECKPOINT_VERSION {
        return Err(corrupt(format!(
            "version {version}, expected {CHECKPOINT_VERSION}"
        )));
    }
    if read_u16(bytes, 10)? != 0 {
        return Err(corrupt("reserved bytes are non-zero".to_owned()));
    }
    let input_count = read_u32(bytes, 12)?;
    if input_count == 0 || input_count > CHECKPOINT_MAX_INPUTS {
        return Err(corrupt(format!(
            "input count {input_count} is outside 1..={CHECKPOINT_MAX_INPUTS}"
        )));
    }
    let expected = CHECKPOINT_PREFIX_BYTES
        .checked_add((input_count as usize).saturating_mul(16))
        .and_then(|bytes| bytes.checked_add(CHECKPOINT_TAIL_BYTES))
        .ok_or_else(|| corrupt("declared length overflow".to_owned()))?;
    if bytes.len() != expected {
        return Err(corrupt(format!(
            "length {} bytes, declared layout needs {expected}",
            bytes.len()
        )));
    }
    let mut inputs = Vec::with_capacity(input_count as usize);
    let mut previous: Option<[u8; 16]> = None;
    for index in 0..input_count as usize {
        let offset = CHECKPOINT_PREFIX_BYTES
            .checked_add(index.saturating_mul(16))
            .ok_or_else(|| corrupt("input offset overflow".to_owned()))?;
        let id = read_id(bytes, offset)?;
        if previous.is_some_and(|earlier| earlier >= id) {
            return Err(corrupt("input ids are not strictly ascending".to_owned()));
        }
        previous = Some(id);
        inputs.push(SegmentId::from_bytes(id));
    }
    let tail = CHECKPOINT_PREFIX_BYTES
        .checked_add((input_count as usize).saturating_mul(16))
        .ok_or_else(|| corrupt("tail offset overflow".to_owned()))?;
    let merge_id = SegmentId::from_bytes(read_id(bytes, tail)?);
    let output_id = SegmentId::from_bytes(read_id(bytes, tail.saturating_add(16))?);
    let rows_emitted = read_u64(bytes, tail.saturating_add(32))?;
    let cursor_segment = read_u32(bytes, tail.saturating_add(40))?;
    let cursor_row = read_u32(bytes, tail.saturating_add(44))?;
    if cursor_segment != input_count || cursor_row != 0 {
        return Err(corrupt(format!(
            "cursor ({cursor_segment}, {cursor_row}) is not terminal for {input_count} inputs"
        )));
    }
    let output_hash = read_u64(bytes, tail.saturating_add(48))?;
    Ok(ConsolidationCheckpoint {
        inputs,
        merge_id,
        output_id,
        rows_emitted,
        output_hash,
    })
}

/// Validates arbitrary consolidation-checkpoint bytes without a store.
///
/// This is the shared parser seam for the dependency-free fuzz target.
pub fn validate_consolidation_checkpoint(bytes: &[u8]) -> Result<(), String> {
    match decode_consolidation_checkpoint(bytes) {
        Ok(_) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ConsolidateError> {
    bytes
        .get(offset..offset.saturating_add(2))
        .and_then(|value| value.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| ConsolidateError::CheckpointCorrupt(format!("u16 at {offset} is truncated")))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ConsolidateError> {
    bytes
        .get(offset..offset.saturating_add(4))
        .and_then(|value| value.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| ConsolidateError::CheckpointCorrupt(format!("u32 at {offset} is truncated")))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, ConsolidateError> {
    bytes
        .get(offset..offset.saturating_add(8))
        .and_then(|value| value.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| ConsolidateError::CheckpointCorrupt(format!("u64 at {offset} is truncated")))
}

fn read_id(bytes: &[u8], offset: usize) -> Result<[u8; 16], ConsolidateError> {
    bytes
        .get(offset..offset.saturating_add(16))
        .and_then(|value| value.try_into().ok())
        .ok_or_else(|| {
            ConsolidateError::CheckpointCorrupt(format!("segment id at {offset} is truncated"))
        })
}

/// Writes the checkpoint through the temp-sync-rename-dirsync discipline.
pub(crate) fn write_consolidation_checkpoint(
    vfs: &dyn Vfs,
    directory: &Path,
    checkpoint: &ConsolidationCheckpoint,
    policy: DurabilityPolicy,
) -> Result<(), ConsolidateError> {
    let bytes = encode_consolidation_checkpoint(checkpoint)?;
    let path = directory.join(CONSOLIDATION_CHECKPOINT_FILE);
    let temporary = directory.join(format!("{CONSOLIDATION_CHECKPOINT_FILE}.tmp"));
    if let Err(source) = vfs.write(&temporary, &bytes) {
        let _ = vfs.delete(&temporary);
        return Err(ConsolidateError::CheckpointIo {
            path: temporary,
            source,
        });
    }
    if let SyncRequirement::Sync(kind) = policy.data_file_sync()
        && let Err(source) = vfs.sync(&temporary, kind)
    {
        let _ = vfs.delete(&temporary);
        return Err(ConsolidateError::CheckpointIo {
            path: temporary,
            source,
        });
    }
    if let Err(source) = vfs.rename(&temporary, &path) {
        let _ = vfs.delete(&temporary);
        return Err(ConsolidateError::CheckpointIo { path, source });
    }
    match policy.directory_sync() {
        SyncRequirement::Skip => Ok(()),
        SyncRequirement::Sync(kind) => {
            vfs.sync(directory, kind)
                .map_err(|source| ConsolidateError::CheckpointIo {
                    path: directory.to_path_buf(),
                    source,
                })
        }
    }
}

/// Reads the checkpoint; absent is `Ok(None)`, invalid bytes are typed corrupt.
pub(crate) fn read_consolidation_checkpoint(
    vfs: &dyn Vfs,
    directory: &Path,
) -> Result<Option<ConsolidationCheckpoint>, ConsolidateError> {
    let path = directory.join(CONSOLIDATION_CHECKPOINT_FILE);
    match vfs.read(&path) {
        Ok(bytes) => decode_consolidation_checkpoint(&bytes).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConsolidateError::CheckpointIo { path, source }),
    }
}

/// Removes the checkpoint and its temporary; both tolerate absence.
pub(crate) fn remove_consolidation_checkpoint(
    vfs: &dyn Vfs,
    directory: &Path,
) -> Result<(), ConsolidateError> {
    for name in [
        format!("{CONSOLIDATION_CHECKPOINT_FILE}.tmp"),
        CONSOLIDATION_CHECKPOINT_FILE.to_owned(),
    ] {
        let path = directory.join(name);
        match vfs.delete(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(ConsolidateError::CheckpointIo { path, source }),
        }
    }
    Ok(())
}

/// Result of one completed merge pass, before any graph exists.
pub(crate) struct MergedIntermediate {
    /// Live rows copied out of the inputs.
    pub(crate) live_rows: u64,
    /// Xxh3-64 over the complete intermediate file bytes.
    pub(crate) file_hash: u64,
}

struct MergeGather {
    codes: Vec<u8>,
    factors: Vec<crate::quant::Bit4Factors>,
    rescore: Vec<f32>,
    doc_ids: Vec<DocId>,
    revisions: Vec<crate::ingest::Revision>,
    metadata: Option<(Vec<u64>, Vec<u8>)>,
    text: Option<(Vec<u8>, Vec<u64>, Vec<u8>)>,
    lexical: Option<SegmentIndex>,
}

/// Merges every input's live rows into one unpublished sealed segment.
///
/// Inputs must be ascending by segment id; rows are emitted in input order
/// then ascending source row id, so the output is deterministic for one
/// input set. Returns `Ok(None)` when no input holds a live row.
pub(crate) fn merge_segments(
    vfs: &dyn Vfs,
    directory: &Path,
    inputs: &[&SegmentReader],
    merge_id: SegmentId,
    analyzer: &Analyzer,
    policy: DurabilityPolicy,
) -> Result<Option<MergedIntermediate>, ConsolidateError> {
    validate_merge_inputs(inputs)?;
    let dims = merge_dims(inputs)?;
    let carry_documents = region_present(inputs, RegionKind::DocumentVersions)?;
    let carry_metadata = inputs
        .iter()
        .any(|reader| has_region(reader, RegionKind::StoredMetadata));
    let rebuild_lexical = inputs
        .iter()
        .any(|reader| has_region(reader, RegionKind::Postings));
    let survivors = live_rows(inputs)?;
    let total_rows: usize = survivors.iter().map(Vec::len).sum();
    if total_rows == 0 {
        return Ok(None);
    }
    let row_count = u32::try_from(total_rows).map_err(|_| {
        ConsolidateError::Geometry("merged rows exceed the u32 row space".to_owned())
    })?;
    let mut gather = MergeGather {
        codes: Vec::new(),
        factors: Vec::new(),
        rescore: Vec::new(),
        doc_ids: Vec::new(),
        revisions: Vec::new(),
        metadata: carry_metadata.then(|| (Vec::new(), Vec::new())),
        text: None,
        lexical: rebuild_lexical.then(SegmentIndex::new),
    };
    let merge_schema = inputs
        .first()
        .ok_or_else(|| ConsolidateError::Geometry("merge requires at least one input".to_owned()))?
        .columns()?
        .schema()
        .clone();
    let mut columns = ColumnStoreBuilder::new(merge_schema.clone());
    let carry_text = inputs
        .iter()
        .any(|reader| has_region(reader, RegionKind::StoredText));
    if carry_text {
        gather.text = Some((Vec::new(), Vec::new(), Vec::new()));
    }
    for (reader, rows) in inputs.iter().zip(&survivors) {
        gather_input(
            reader,
            rows,
            dims,
            carry_documents,
            analyzer,
            &merge_schema,
            &mut gather,
            &mut columns,
        )?;
    }
    if carry_documents {
        let mut seen = HashSet::with_capacity(gather.doc_ids.len());
        for doc_id in &gather.doc_ids {
            if !seen.insert(*doc_id) {
                return Err(ConsolidateError::DuplicateDocument {
                    doc_id: doc_id.get(),
                });
            }
        }
    }
    let columns = columns
        .finish()
        .map_err(|source| ConsolidateError::Segment(SegmentError::Columns(source.to_string())))?;
    let alive = AliveSet::new(row_count);
    let scheme = merge_scheme(inputs)?;
    let build = SegmentBuild {
        id: merge_id,
        scheme,
        dims,
        codes: &gather.codes,
        factors: crate::segment::writer::SegmentFactors::Bit4(&gather.factors),
        rescore: &gather.rescore,
        columns: &columns,
        alive: &alive,
    };
    let text_has_rows = gather
        .text
        .as_ref()
        .is_some_and(|(present, _, _)| present.contains(&1));
    let postings_bytes = match gather.lexical.as_ref() {
        Some(lexical) if text_has_rows => Some(
            SealedSegment::seal(lexical)
                .map_err(|error| ConsolidateError::Lexical(error.to_string()))?
                .encode_region()
                .map_err(|error| ConsolidateError::Lexical(error.to_string()))?,
        ),
        _ => None,
    };
    let _meta = if carry_documents {
        let documents = SegmentDocumentVersions {
            doc_ids: &gather.doc_ids,
            revisions: &gather.revisions,
        };
        let metadata = gather
            .metadata
            .as_ref()
            .map(|(end_offsets, bytes)| SegmentStoredMetadata { end_offsets, bytes });
        let text = gather
            .text
            .as_ref()
            .filter(|(present, _, _)| present.contains(&1))
            .map(|(present, end_offsets, bytes)| SegmentStoredText {
                present,
                end_offsets,
                bytes,
            });
        let postings = postings_bytes
            .as_deref()
            .map(|bytes| SegmentPostings { bytes });
        write_segment_with_documents_payloads(
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
        )?
    } else {
        if gather.metadata.is_some() || text_has_rows || postings_bytes.is_some() {
            return Err(ConsolidateError::Geometry(
                "merged payload regions require document identities".to_owned(),
            ));
        }
        write_segment(vfs, directory, build, policy)?
    };
    let path = directory.join(merge_id.file_name());
    let file_bytes = vfs
        .read(&path)
        .map_err(|source| ConsolidateError::Segment(SegmentError::io(&path, source)))?;
    Ok(Some(MergedIntermediate {
        live_rows: u64::from(row_count),
        file_hash: xxh3_64(&file_bytes),
    }))
}

/// Recomputes the clustering-key union over merged live rows (10-C rules).
pub(crate) fn merged_clustering(
    columns: &crate::meta::ColumnStore,
    alive: &AliveSet,
) -> Result<ClusteringKeyRange, ConsolidateError> {
    crate::ingest::purge_support::clustering_range(columns, alive).map_err(ConsolidateError::Store)
}

fn validate_merge_inputs(inputs: &[&SegmentReader]) -> Result<(), ConsolidateError> {
    if inputs.len() < 2 {
        return Err(ConsolidateError::Geometry(format!(
            "consolidation requires at least two inputs, got {}",
            inputs.len()
        )));
    }
    let mut previous: Option<&SegmentId> = None;
    for reader in inputs {
        let id = &reader.meta().id;
        if previous.is_some_and(|earlier| earlier.as_bytes() >= id.as_bytes()) {
            return Err(ConsolidateError::Geometry(
                "consolidation inputs are not strictly ascending by segment id".to_owned(),
            ));
        }
        previous = Some(id);
        for entry in reader.directory() {
            if !consolidation_supports_region(entry.kind) {
                return Err(ConsolidateError::Geometry(format!(
                    "consolidation cannot carry source region kind {}",
                    entry.kind
                )));
            }
        }
        if has_region(reader, RegionKind::Postings) && !has_region(reader, RegionKind::StoredText) {
            return Err(ConsolidateError::Lexical(format!(
                "segment {} carries postings without the stored text needed to rebuild them",
                reader.meta().id
            )));
        }
    }
    Ok(())
}

fn merge_dims(inputs: &[&SegmentReader]) -> Result<u32, ConsolidateError> {
    let mut dims = None;
    for reader in inputs {
        let candidate = reader.meta().dims;
        if dims.is_some_and(|expected| expected != candidate) {
            return Err(ConsolidateError::Geometry(
                "consolidation inputs disagree on vector dimensions".to_owned(),
            ));
        }
        dims = Some(candidate);
    }
    dims.ok_or_else(|| ConsolidateError::Geometry("consolidation has no inputs".to_owned()))
}

fn merge_scheme(inputs: &[&SegmentReader]) -> Result<u16, ConsolidateError> {
    for reader in inputs {
        if reader.meta().scheme != 4 {
            return Err(ConsolidateError::Geometry(format!(
                "consolidation requires Bit4 scheme 4, segment {} has {}",
                reader.meta().id,
                reader.meta().scheme
            )));
        }
    }
    Ok(4)
}

fn region_present(inputs: &[&SegmentReader], kind: RegionKind) -> Result<bool, ConsolidateError> {
    let mut present = None;
    for reader in inputs {
        let has = has_region(reader, kind);
        if present.is_some_and(|expected| expected != has) {
            return Err(ConsolidateError::Geometry(format!(
                "consolidation inputs disagree on region kind {} presence",
                kind.id()
            )));
        }
        present = Some(has);
    }
    Ok(present.unwrap_or(false))
}

fn has_region(reader: &SegmentReader, kind: RegionKind) -> bool {
    reader
        .directory()
        .iter()
        .any(|entry| entry.kind == kind.id())
}

fn live_rows(inputs: &[&SegmentReader]) -> Result<Vec<Vec<usize>>, ConsolidateError> {
    let mut survivors = Vec::with_capacity(inputs.len());
    for reader in inputs {
        let alive = reader.alive()?;
        survivors.push(alive.iter_alive().map(|row| row as usize).collect());
    }
    Ok(survivors)
}

#[allow(clippy::too_many_arguments)]
fn gather_input(
    reader: &SegmentReader,
    rows: &[usize],
    dims: u32,
    carry_documents: bool,
    analyzer: &Analyzer,
    merge_schema: &crate::meta::Schema,
    gather: &mut MergeGather,
    columns: &mut ColumnStoreBuilder,
) -> Result<(), ConsolidateError> {
    let dims = dims as usize;
    let (codes, factors) = crate::ingest::purge_support::gather_survivor_codes(reader, rows, dims)
        .map_err(ConsolidateError::Store)?;
    let crate::ingest::purge_support::OwnedFactors::Bit4(factors) = factors else {
        return Err(ConsolidateError::Geometry(
            "consolidation gathered non-Bit4 factors".to_owned(),
        ));
    };
    gather.codes.extend_from_slice(&codes);
    gather.factors.extend_from_slice(&factors);
    let rescore = crate::ingest::purge_support::gather_survivor_rescore(reader, rows, dims)
        .map_err(ConsolidateError::Store)?;
    gather.rescore.extend_from_slice(&rescore);
    let source_columns = reader.columns()?;
    if source_columns.schema() != merge_schema {
        return Err(ConsolidateError::Geometry(format!(
            "segment {} schema differs from the merge schema",
            reader.meta().id
        )));
    }
    crate::ingest::purge_support::append_survivor_columns(columns, &source_columns, rows)
        .map_err(ConsolidateError::Store)?;
    if carry_documents {
        let (doc_ids, revisions) =
            crate::ingest::purge_support::gather_survivor_documents(reader, rows)
                .map_err(ConsolidateError::Store)?;
        gather.doc_ids.extend_from_slice(&doc_ids);
        gather.revisions.extend_from_slice(&revisions);
    }
    if let Some((end_offsets, bytes)) = gather.metadata.as_mut() {
        let source = reader.stored_metadata()?;
        for row in rows {
            if let Some(source) = source {
                bytes.extend_from_slice(source.row(*row).ok_or_else(|| {
                    ConsolidateError::Segment(SegmentError::Geometry(format!(
                        "stored metadata row {row} is missing"
                    )))
                })?);
            }
            end_offsets.push(u64::try_from(bytes.len()).map_err(|_| {
                ConsolidateError::Geometry("stored metadata offset exceeds u64".to_owned())
            })?);
        }
    }
    let source_text = reader.stored_text()?;
    if let Some((present, end_offsets, bytes)) = gather.text.as_mut() {
        for row in rows {
            let text = match source_text {
                Some(view) => view.row(*row).ok_or_else(|| {
                    ConsolidateError::Segment(SegmentError::Geometry(format!(
                        "stored text row {row} is missing"
                    )))
                })?,
                None => None,
            };
            match text {
                Some(text) => {
                    present.push(1);
                    bytes.extend_from_slice(text.as_bytes());
                }
                None => present.push(0),
            }
            end_offsets.push(u64::try_from(bytes.len()).map_err(|_| {
                ConsolidateError::Geometry("stored text offset exceeds u64".to_owned())
            })?);
        }
    }
    if let Some(lexical) = gather.lexical.as_mut() {
        for row in rows {
            let text = match source_text {
                Some(view) => view.row(*row).ok_or_else(|| {
                    ConsolidateError::Segment(SegmentError::Geometry(format!(
                        "stored text row {row} is missing for the lexical rebuild"
                    )))
                })?,
                None => None,
            };
            let document = text.map_or_else(LexicalDocument::new, LexicalDocument::with_text);
            lexical
                .push_document(analyzer, &document)
                .map_err(|error| ConsolidateError::Lexical(error.to_string()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::{
        CONSOLIDATION_CHECKPOINT_FILE, ConsolidateError, ConsolidationCheckpoint,
        consolidation_merge_id, consolidation_output_id, consolidation_supports_region,
        decode_consolidation_checkpoint, encode_consolidation_checkpoint, merge_segments,
        read_consolidation_checkpoint, remove_consolidation_checkpoint,
        validate_consolidation_checkpoint, write_consolidation_checkpoint,
    };
    use crate::fts::tokenizer::{Analyzer, TokenizerConfig};
    use crate::graph::block::{GraphNodeBlockBuild, GraphNodeBlockInput, GraphNodeLayout};
    use crate::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
    use crate::meta::{AliveSet, ColumnStoreBuilder, Schema};
    use crate::quant::quantize_bit4;
    use crate::segment::SegmentId;
    use crate::segment::layout::RegionKind;
    use crate::segment::reader::SegmentReader;
    use crate::segment::writer::{SegmentBuild, SegmentFactors, write_segment_with_graph};
    use crate::vfs::StdVfs;

    fn checkpoint_fixture() -> ConsolidationCheckpoint {
        let inputs = vec![
            SegmentId::new(1, [1; 10]),
            SegmentId::new(2, [2; 10]),
            SegmentId::new(3, [3; 10]),
        ];
        ConsolidationCheckpoint {
            merge_id: consolidation_merge_id(&inputs),
            output_id: consolidation_output_id(&inputs),
            inputs,
            rows_emitted: 384,
            output_hash: 0x0102_0304_0506_0708,
        }
    }

    #[test]
    fn consolidation_checkpoint_round_trips_and_ids_are_deterministic() {
        let checkpoint = checkpoint_fixture();
        let bytes = encode_consolidation_checkpoint(&checkpoint).expect("encode checkpoint");
        let decoded = decode_consolidation_checkpoint(&bytes).expect("decode checkpoint");
        assert_eq!(decoded, checkpoint);
        assert!(validate_consolidation_checkpoint(&bytes).is_ok());

        assert_eq!(
            consolidation_merge_id(&checkpoint.inputs),
            checkpoint.merge_id
        );
        assert_ne!(checkpoint.merge_id, checkpoint.output_id);
        let mut reordered = checkpoint.inputs.clone();
        reordered.swap(0, 2);
        assert_ne!(
            consolidation_output_id(&reordered),
            consolidation_output_id(&checkpoint.inputs),
            "id derivation must bind the exact input order"
        );
    }

    #[test]
    fn consolidation_checkpoint_rejects_every_corruption_class() {
        let checkpoint = checkpoint_fixture();
        let valid = encode_consolidation_checkpoint(&checkpoint).expect("encode checkpoint");

        assert!(validate_consolidation_checkpoint(&[]).is_err());
        assert!(validate_consolidation_checkpoint(&valid[..valid.len() - 1]).is_err());

        let mut magic = valid.clone();
        magic[0] ^= 0xff;
        assert!(validate_consolidation_checkpoint(&magic).is_err());

        let mut version = valid.clone();
        version[8] = 9;
        assert!(
            validate_consolidation_checkpoint(&reseal(version)).is_err(),
            "future version must be refused"
        );

        let mut reserved = valid.clone();
        reserved[10] = 1;
        assert!(validate_consolidation_checkpoint(&reseal(reserved)).is_err());

        let mut checksum = valid.clone();
        let last = checksum.len() - 1;
        checksum[last] ^= 0xff;
        assert!(validate_consolidation_checkpoint(&checksum).is_err());

        let mut count = valid.clone();
        count[12..16].copy_from_slice(&0_u32.to_le_bytes());
        assert!(validate_consolidation_checkpoint(&reseal(count)).is_err());

        let mut unsorted = valid.clone();
        let (left, right) = (16, 32);
        for offset in 0..16 {
            unsorted.swap(left + offset, right + offset);
        }
        assert!(validate_consolidation_checkpoint(&reseal(unsorted)).is_err());

        let mut cursor = valid.clone();
        let tail = 16 + 3 * 16;
        let cursor_offset = tail + 16 + 16 + 8;
        cursor[cursor_offset..cursor_offset + 4].copy_from_slice(&1_u32.to_le_bytes());
        assert!(
            validate_consolidation_checkpoint(&reseal(cursor)).is_err(),
            "a non-terminal cursor cannot be resumed by version 1"
        );
    }

    fn reseal(mut bytes: Vec<u8>) -> Vec<u8> {
        let start = bytes.len() - 8;
        let checksum = xxhash_rust::xxh3::xxh3_64(&bytes[..start]).to_le_bytes();
        bytes[start..].copy_from_slice(&checksum);
        bytes
    }

    #[test]
    fn consolidation_checkpoint_file_round_trips_and_clears() {
        let directory = tempfile::tempdir().expect("checkpoint directory");
        let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
            .expect("derived policy");
        let checkpoint = checkpoint_fixture();

        assert!(
            read_consolidation_checkpoint(&StdVfs, directory.path())
                .expect("absent checkpoint reads clean")
                .is_none()
        );
        write_consolidation_checkpoint(&StdVfs, directory.path(), &checkpoint, policy)
            .expect("write checkpoint");
        let read = read_consolidation_checkpoint(&StdVfs, directory.path())
            .expect("read checkpoint")
            .expect("checkpoint present");
        assert_eq!(read, checkpoint);

        let path = directory.path().join(CONSOLIDATION_CHECKPOINT_FILE);
        let mut bytes = std::fs::read(&path).expect("checkpoint bytes");
        bytes[20] ^= 0xff;
        std::fs::write(&path, bytes).expect("corrupt checkpoint");
        assert!(read_consolidation_checkpoint(&StdVfs, directory.path()).is_err());

        remove_consolidation_checkpoint(&StdVfs, directory.path()).expect("clear checkpoint");
        assert!(!path.exists());
        remove_consolidation_checkpoint(&StdVfs, directory.path())
            .expect("second clear is idempotent");
    }

    const MERGE_DIMS: usize = 128;

    fn graph_segment(
        directory: &std::path::Path,
        id: SegmentId,
        rows: usize,
        base: f32,
        tombstones: &[u32],
    ) -> SegmentId {
        let vectors = (0..rows)
            .flat_map(|row| std::iter::repeat_n(base + row as f32, MERGE_DIMS))
            .collect::<Vec<_>>();
        let stride = MERGE_DIMS.div_ceil(2);
        let mut codes = vec![0_u8; rows * stride];
        let mut factors = Vec::with_capacity(rows);
        for (vector, encoded) in vectors
            .chunks_exact(MERGE_DIMS)
            .zip(codes.chunks_exact_mut(stride))
        {
            factors.push(quantize_bit4(vector, encoded).expect("finite merge fixture row"));
        }
        let schema = Schema::new(Vec::new()).expect("timestamp-only schema");
        let mut columns = ColumnStoreBuilder::new(schema);
        for row in 0..rows {
            columns
                .push_row(row as i64, &[])
                .expect("merge fixture column row");
        }
        let columns = columns.finish().expect("merge fixture columns");
        let mut alive = AliveSet::new(rows as u32);
        for row in tombstones {
            alive.tombstone(*row).expect("merge fixture tombstone");
        }
        let neighbors = (0..rows)
            .map(|row| {
                u32::try_from((row + 1) % rows)
                    .ok()
                    .into_iter()
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let nodes = codes
            .chunks_exact(stride)
            .zip(&factors)
            .zip(&neighbors)
            .map(|((codes, factors), neighbors)| GraphNodeBlockInput {
                codes,
                factors: *factors,
                flags: 1,
                neighbors,
            })
            .collect::<Vec<_>>();
        write_segment_with_graph(
            &StdVfs,
            directory,
            SegmentBuild {
                id,
                scheme: 4,
                dims: MERGE_DIMS as u32,
                codes: &codes,
                factors: SegmentFactors::Bit4(&factors),
                rescore: &vectors,
                columns: &columns,
                alive: &alive,
            },
            GraphNodeBlockBuild {
                layout: GraphNodeLayout::new(MERGE_DIMS as u32, 128, 1)
                    .expect("merge fixture layout"),
                nodes: &nodes,
            },
            DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
                .expect("derived policy"),
        )
        .expect("write merge fixture segment");
        id
    }

    #[test]
    fn merge_segments_without_documents_carries_live_rows_and_rejects_bad_inputs() {
        let directory = tempfile::tempdir().expect("merge fixture directory");
        let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
            .expect("derived policy");
        let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("default analyzer");
        let first_id = graph_segment(directory.path(), SegmentId::new(1, [1; 10]), 4, 0.0, &[1]);
        let second_id = graph_segment(directory.path(), SegmentId::new(2, [2; 10]), 3, 10.0, &[]);
        let first = SegmentReader::open(
            &StdVfs,
            &directory.path().join(first_id.file_name()),
            first_id,
        )
        .expect("open first merge input");
        let second = SegmentReader::open(
            &StdVfs,
            &directory.path().join(second_id.file_name()),
            second_id,
        )
        .expect("open second merge input");

        let merge_id = consolidation_merge_id(&[first_id, second_id]);
        let merged = merge_segments(
            &StdVfs,
            directory.path(),
            &[&first, &second],
            merge_id,
            &analyzer,
            policy,
        )
        .expect("merge without documents")
        .expect("live rows were carried");
        assert_eq!(merged.live_rows, 3 + 3, "one tombstoned row is dropped");
        let reader = SegmentReader::open(
            &StdVfs,
            &directory.path().join(merge_id.file_name()),
            merge_id,
        )
        .expect("open merged intermediate");
        assert_eq!(reader.meta().row_count, 6);
        assert_eq!(reader.alive().expect("merged alive").live_count(), 6);
        assert!(
            reader
                .document_version(0)
                .expect("merged document row")
                .is_none(),
            "inputs without document identities merge without them"
        );

        // Fewer than two inputs, and unsorted inputs, are typed geometry errors.
        assert!(matches!(
            merge_segments(
                &StdVfs,
                directory.path(),
                &[&first],
                merge_id,
                &analyzer,
                policy
            ),
            Err(ConsolidateError::Geometry(_))
        ));
        assert!(matches!(
            merge_segments(
                &StdVfs,
                directory.path(),
                &[&second, &first],
                merge_id,
                &analyzer,
                policy
            ),
            Err(ConsolidateError::Geometry(_))
        ));
    }

    #[test]
    fn consolidation_region_support_is_the_frozen_merge_set() {
        for kind in [
            RegionKind::Columns,
            RegionKind::Alive,
            RegionKind::VectorCodes,
            RegionKind::VectorFactors,
            RegionKind::VectorRescore,
            RegionKind::DocumentVersions,
            RegionKind::GraphNodeBlocks,
            RegionKind::ChecksumTable,
            RegionKind::Postings,
            RegionKind::StoredMetadata,
            RegionKind::StoredText,
        ] {
            assert!(consolidation_supports_region(kind.id()), "{kind:?}");
        }
        for kind in [
            RegionKind::GraphColocatedCodes,
            RegionKind::SignPlane,
            RegionKind::PdxClusteredBlocks,
            RegionKind::VectorSpaceN,
        ] {
            assert!(!consolidation_supports_region(kind.id()), "{kind:?}");
        }
        assert!(!consolidation_supports_region(65_000));
    }
}
