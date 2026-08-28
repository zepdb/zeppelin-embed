//! Atomic manifest commit point for every visible persisted artifact.

pub mod io;

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, EpochId, EpochIdentity,
    Normalization,
};
use crate::format::FormatFamily;
use crate::format::frame::{FormatError, decode_artifact, encode_artifact};
use crate::fts::tokenizer::TokenizerEpoch;
use crate::meta::{ColumnDefinition, ColumnId, ColumnType, Schema};
use crate::segment::{ClusteringKeyRange, SegmentId, SegmentMeta};

const CLUSTERING_RANGE_EXTENSION_MAGIC: [u8; 4] = *b"TSR1";
const CLUSTERING_RANGE_RECORD_LEN: usize = 24;

/// Interpretation-critical model/tokenizer identity for one coexistence epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EpochMeta {
    /// Stable epoch identifier.
    pub id: EpochId,
    /// Complete authoritative embedding-model identity.
    pub embedding: EmbeddingEpoch,
    /// Authoritative tokenizer identity.
    pub tokenizer: TokenizerEpoch,
}

/// The single atomic snapshot commit point.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Manifest {
    /// Monotonic committed generation.
    pub generation: u64,
    /// Durable WAL sequence covered by this snapshot.
    pub log_seq: u64,
    /// Complete reachable immutable segment set.
    pub segments: Vec<SegmentMeta>,
    /// Complete interpretation epoch set.
    pub epochs: Vec<EpochMeta>,
    /// Atomically published embedding/tokenizer identity used by queries and writes.
    pub epoch_alias: Option<EpochIdentity>,
    /// Typed metadata schema interpreted by every listed segment.
    pub schema: Schema,
}

/// Typed manifest encode/load/open failure.
#[derive(Debug)]
pub enum ManifestError {
    /// Filesystem operation failed for a named path.
    Io {
        /// Path involved in the operation.
        path: PathBuf,
        /// Underlying platform error.
        source: std::io::Error,
    },
    /// Framing, version, length, or checksum validation failed.
    Format(FormatError),
    /// Hand-written payload decoding rejected malformed bytes.
    Decode(String),
    /// Snapshot log sequence exceeds the durable WAL end.
    AheadOfLog {
        /// Sequence covered by the snapshot.
        snapshot: u64,
        /// Sequence known durable in the WAL.
        durable: u64,
    },
    /// A referenced segment header failed validation.
    Segment(crate::segment::SegmentError),
    /// The published alias did not name a registry entry.
    UnknownEpochAlias {
        /// Rejected alias.
        alias: EpochIdentity,
    },
    /// A segment epoch tag did not name any registry entry.
    UnknownSegmentEpoch {
        /// Segment carrying the bad tag.
        segment: SegmentId,
        /// Rejected embedding epoch id.
        epoch: EpochId,
    },
}

impl ManifestError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(formatter, "manifest I/O {}: {source}", path.display())
            }
            Self::Format(error) => error.fmt(formatter),
            Self::Decode(detail) => write!(formatter, "manifest decode failed: {detail}"),
            Self::AheadOfLog { snapshot, durable } => write!(
                formatter,
                "manifest log sequence {snapshot} is ahead of durable WAL end {durable}"
            ),
            Self::Segment(error) => {
                write!(formatter, "manifest segment validation failed: {error}")
            }
            Self::UnknownEpochAlias { alias } => write!(
                formatter,
                "manifest epoch alias ({}, {}) does not name a registry entry",
                alias.embedding, alias.tokenizer
            ),
            Self::UnknownSegmentEpoch { segment, epoch } => write!(
                formatter,
                "manifest segment {segment} names unknown embedding epoch {epoch}"
            ),
        }
    }
}

impl std::error::Error for ManifestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Format(error) => Some(error),
            Self::Segment(error) => Some(error),
            Self::Decode(_)
            | Self::AheadOfLog { .. }
            | Self::UnknownEpochAlias { .. }
            | Self::UnknownSegmentEpoch { .. } => None,
        }
    }
}

impl From<FormatError> for ManifestError {
    fn from(error: FormatError) -> Self {
        Self::Format(error)
    }
}

impl From<crate::segment::SegmentError> for ManifestError {
    fn from(error: crate::segment::SegmentError) -> Self {
        Self::Segment(error)
    }
}

/// Encodes a complete manifest through a single checksummed frame.
pub fn encode_manifest(manifest: &Manifest) -> Result<Vec<u8>, ManifestError> {
    validate_epoch_references(manifest)?;
    let mut payload = Vec::new();
    payload.extend_from_slice(&manifest.generation.to_le_bytes());
    payload.extend_from_slice(&manifest.log_seq.to_le_bytes());
    append_u32_len(manifest.segments.len(), "segments", &mut payload)?;
    append_u32_len(manifest.epochs.len(), "epochs", &mut payload)?;
    append_u32_len(
        manifest.schema.user_column_count(),
        "schema columns",
        &mut payload,
    )?;
    payload.extend_from_slice(&0_u32.to_le_bytes());
    append_optional_identity(manifest.epoch_alias, &mut payload);
    for segment in &manifest.segments {
        payload.extend_from_slice(segment.id.as_bytes());
        payload.extend_from_slice(&segment.row_count.to_le_bytes());
        payload.extend_from_slice(&segment.scheme.to_le_bytes());
        payload.extend_from_slice(&0_u16.to_le_bytes());
        payload.extend_from_slice(&segment.dims.to_le_bytes());
        payload.extend_from_slice(&segment.file_size.to_le_bytes());
        append_optional_epoch_id(segment.epoch_id, &mut payload);
    }
    for epoch in &manifest.epochs {
        payload.extend_from_slice(&epoch.id.value().to_le_bytes());
        payload.extend_from_slice(&epoch.tokenizer.value().to_le_bytes());
        append_embedding_epoch(&epoch.embedding, &mut payload)?;
    }
    for definition in manifest.schema.columns().iter().skip(1) {
        payload.extend_from_slice(&definition.id().get().to_le_bytes());
        payload.extend_from_slice(&column_type_id(definition.column_type()).to_le_bytes());
        payload.extend_from_slice(&u16::from(definition.is_nullable()).to_le_bytes());
        append_string(definition.name(), &mut payload)?;
    }
    append_clustering_ranges(&manifest.segments, &mut payload)?;
    Ok(encode_artifact(FormatFamily::Manifest, 0, &payload))
}

fn append_clustering_ranges(
    segments: &[SegmentMeta],
    output: &mut Vec<u8>,
) -> Result<(), ManifestError> {
    if segments
        .iter()
        .all(|segment| segment.clustering_key_range == ClusteringKeyRange::Unstamped)
    {
        return Ok(());
    }
    output.extend_from_slice(&CLUSTERING_RANGE_EXTENSION_MAGIC);
    append_u32_len(segments.len(), "clustering ranges", output)?;
    for segment in segments {
        let (tag, min_ts, max_ts) = match segment.clustering_key_range {
            ClusteringKeyRange::Unstamped => (0_u8, 0_i64, 0_i64),
            ClusteringKeyRange::Empty => (1_u8, 0_i64, 0_i64),
            ClusteringKeyRange::Bounded { min_ts, max_ts } => {
                if min_ts > max_ts {
                    return Err(ManifestError::Decode(format!(
                        "segment {} clustering range {min_ts}..={max_ts} is inverted",
                        segment.id
                    )));
                }
                (2_u8, min_ts, max_ts)
            }
        };
        output.push(tag);
        output.extend_from_slice(&[0_u8; 7]);
        output.extend_from_slice(&min_ts.to_le_bytes());
        output.extend_from_slice(&max_ts.to_le_bytes());
    }
    Ok(())
}

/// Decodes a complete manifest only after both xxh3-64 checksums validate.
pub fn decode_manifest(artifact: &str, bytes: &[u8]) -> Result<Manifest, ManifestError> {
    #[cfg(any(test, feature = "test-support"))]
    let actual_family = bytes
        .get(8..10)
        .and_then(|family| <[u8; 2]>::try_from(family).ok())
        .map(u16::from_le_bytes);
    let framed = decode_artifact(artifact, FormatFamily::Manifest, bytes).map_err(|error| {
        #[cfg(any(test, feature = "test-support"))]
        crate::lifecycle::record_storage_manifest_format_fault(&error, actual_family);
        ManifestError::Format(error)
    })?;
    let mut cursor = ManifestCursor::new(framed.payload);
    let generation = cursor.u64()?;
    let log_seq = cursor.u64()?;
    let segment_count = cursor.usize_from_u32()?;
    let epoch_count = cursor.usize_from_u32()?;
    let schema_count = cursor.usize_from_u32()?;
    if cursor.u32()? != 0 {
        return Err(ManifestError::Decode(
            "manifest reserved field is non-zero".to_owned(),
        ));
    }
    let epoch_alias = cursor.optional_identity()?;
    let mut segments = Vec::with_capacity(segment_count);
    for _ in 0..segment_count {
        let id_bytes: [u8; 16] = cursor
            .take(16)?
            .try_into()
            .map_err(|_| ManifestError::Decode("invalid segment id".to_owned()))?;
        let row_count = cursor.u32()?;
        let scheme = cursor.u16()?;
        crate::format::FormatRegistry::require_scheme(scheme)
            .map_err(|error| ManifestError::Decode(error.to_string()))?;
        if cursor.u16()? != 0 {
            return Err(ManifestError::Decode(
                "segment reserved field is non-zero".to_owned(),
            ));
        }
        segments.push(SegmentMeta {
            id: SegmentId::from_bytes(id_bytes),
            row_count,
            scheme,
            dims: cursor.u32()?,
            file_size: cursor.u64()?,
            epoch_id: cursor.optional_epoch_id()?,
            clustering_key_range: crate::segment::ClusteringKeyRange::Unstamped,
        });
    }
    let mut epochs = Vec::with_capacity(epoch_count);
    for _ in 0..epoch_count {
        epochs.push(EpochMeta {
            id: EpochId::from_value(cursor.u64()?),
            tokenizer: TokenizerEpoch::from_value(cursor.u64()?),
            embedding: cursor.embedding_epoch()?,
        });
    }
    let mut definitions = Vec::with_capacity(schema_count);
    for _ in 0..schema_count {
        let id = ColumnId::new(cursor.u32()?);
        let column_type = column_type_from_id(cursor.u16()?)?;
        let nullable = match cursor.u16()? {
            0 => false,
            1 => true,
            value => {
                return Err(ManifestError::Decode(format!(
                    "invalid nullable flag {value}"
                )));
            }
        };
        definitions.push(ColumnDefinition::new(
            id,
            cursor.string()?,
            column_type,
            nullable,
        ));
    }
    if cursor.remaining() != 0 {
        decode_clustering_ranges(&mut cursor, &mut segments)?;
    }
    cursor.finish()?;
    let schema =
        Schema::new(definitions).map_err(|error| ManifestError::Decode(error.to_string()))?;
    let manifest = Manifest {
        generation,
        log_seq,
        segments,
        epochs,
        epoch_alias,
        schema,
    };
    validate_epoch_references(&manifest)?;
    Ok(manifest)
}

fn validate_epoch_references(manifest: &Manifest) -> Result<(), ManifestError> {
    let mut identities = BTreeSet::new();
    for epoch in &manifest.epochs {
        let identity = EpochIdentity::from_meta(epoch)
            .map_err(|error| ManifestError::Decode(error.to_string()))?;
        if !identities.insert(identity) {
            return Err(ManifestError::Decode(format!(
                "duplicate epoch registry identity ({}, {})",
                identity.embedding, identity.tokenizer
            )));
        }
    }

    match manifest.epoch_alias {
        Some(alias) if !identities.contains(&alias) => {
            return Err(ManifestError::UnknownEpochAlias { alias });
        }
        None if !identities.is_empty() => {
            return Err(ManifestError::Decode(
                "non-empty epoch registry has no published alias".to_owned(),
            ));
        }
        Some(_) | None => {}
    }

    for segment in &manifest.segments {
        match segment.epoch_id {
            Some(epoch)
                if !identities
                    .iter()
                    .any(|identity| identity.embedding == epoch) =>
            {
                return Err(ManifestError::UnknownSegmentEpoch {
                    segment: segment.id,
                    epoch,
                });
            }
            None if !identities.is_empty() => {
                return Err(ManifestError::Decode(format!(
                    "segment {} has no embedding epoch tag",
                    segment.id
                )));
            }
            Some(_) | None => {}
        }
    }
    Ok(())
}

fn append_optional_identity(identity: Option<EpochIdentity>, output: &mut Vec<u8>) {
    let (tag, embedding, tokenizer) = match identity {
        Some(identity) => (1_u8, identity.embedding.value(), identity.tokenizer.value()),
        None => (0, 0, 0),
    };
    output.push(tag);
    output.extend_from_slice(&[0_u8; 7]);
    output.extend_from_slice(&embedding.to_le_bytes());
    output.extend_from_slice(&tokenizer.to_le_bytes());
}

fn append_optional_epoch_id(epoch: Option<EpochId>, output: &mut Vec<u8>) {
    let (tag, value) = epoch.map_or((0_u8, 0_u64), |epoch| (1, epoch.value()));
    output.push(tag);
    output.extend_from_slice(&[0_u8; 7]);
    output.extend_from_slice(&value.to_le_bytes());
}

fn append_embedding_epoch(
    epoch: &EmbeddingEpoch,
    output: &mut Vec<u8>,
) -> Result<(), ManifestError> {
    append_embedding_tower(&epoch.document, output)?;
    append_embedding_tower(&epoch.query, output)?;
    append_bytes(&epoch.alignment_digest, output)
}

fn append_embedding_tower(
    tower: &EmbeddingTower,
    output: &mut Vec<u8>,
) -> Result<(), ManifestError> {
    append_string(&tower.model_id, output)?;
    append_string(&tower.model_version, output)?;
    append_bytes(&tower.weights_digest, output)?;
    output.extend_from_slice(&tower.dims.to_le_bytes());
    output.extend_from_slice(&(tower.normalization as u16).to_le_bytes());
    append_string(&tower.prompt_prefix, output)?;
    output.extend_from_slice(&tower.max_tokens.to_le_bytes());
    output.extend_from_slice(&(tower.runtime as u16).to_le_bytes());
    output.extend_from_slice(&(tower.compute_units as u16).to_le_bytes());
    match &tower.os_build {
        Some(build) => {
            output.push(1);
            output.extend_from_slice(&[0_u8; 3]);
            append_string(build, output)?;
        }
        None => {
            output.push(0);
            output.extend_from_slice(&[0_u8; 3]);
        }
    }
    Ok(())
}

fn decode_clustering_ranges(
    cursor: &mut ManifestCursor<'_>,
    segments: &mut [SegmentMeta],
) -> Result<(), ManifestError> {
    if cursor.take(CLUSTERING_RANGE_EXTENSION_MAGIC.len())? != CLUSTERING_RANGE_EXTENSION_MAGIC {
        return Err(ManifestError::Decode(
            "unknown manifest extension after schema".to_owned(),
        ));
    }
    let count = cursor.usize_from_u32()?;
    if count != segments.len() {
        return Err(ManifestError::Decode(format!(
            "clustering range count {count} does not match segment count {}",
            segments.len()
        )));
    }
    let expected_bytes = count
        .checked_mul(CLUSTERING_RANGE_RECORD_LEN)
        .ok_or_else(|| ManifestError::Decode("clustering range bytes overflow".to_owned()))?;
    if cursor.remaining() != expected_bytes {
        return Err(ManifestError::Decode(format!(
            "clustering range extension has {} bytes, expected {expected_bytes}",
            cursor.remaining()
        )));
    }
    for segment in segments {
        let tag = cursor.u8()?;
        if cursor.take(7)?.iter().any(|byte| *byte != 0) {
            return Err(ManifestError::Decode(
                "clustering range reserved bytes are non-zero".to_owned(),
            ));
        }
        let min_ts = cursor.i64()?;
        let max_ts = cursor.i64()?;
        segment.clustering_key_range = match tag {
            0 if min_ts == 0 && max_ts == 0 => ClusteringKeyRange::Unstamped,
            1 if min_ts == 0 && max_ts == 0 => ClusteringKeyRange::Empty,
            2 if min_ts <= max_ts => ClusteringKeyRange::Bounded { min_ts, max_ts },
            0 | 1 => {
                return Err(ManifestError::Decode(format!(
                    "clustering range tag {tag} requires zero bounds"
                )));
            }
            2 => {
                return Err(ManifestError::Decode(format!(
                    "clustering range {min_ts}..={max_ts} is inverted"
                )));
            }
            unknown => {
                return Err(ManifestError::Decode(format!(
                    "unknown clustering range tag {unknown}"
                )));
            }
        };
    }
    Ok(())
}

fn append_u32_len(length: usize, name: &str, output: &mut Vec<u8>) -> Result<(), ManifestError> {
    let value = u32::try_from(length)
        .map_err(|_| ManifestError::Decode(format!("{name} length exceeds u32")))?;
    output.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn append_string(value: &str, output: &mut Vec<u8>) -> Result<(), ManifestError> {
    append_bytes(value.as_bytes(), output)
}

fn append_bytes(value: &[u8], output: &mut Vec<u8>) -> Result<(), ManifestError> {
    append_u32_len(value.len(), "byte field", output)?;
    output.extend_from_slice(value);
    Ok(())
}

fn column_type_id(column_type: ColumnType) -> u16 {
    match column_type {
        ColumnType::U64 => 1,
        ColumnType::I64 => 2,
        ColumnType::F64 => 3,
        ColumnType::Bool => 4,
        ColumnType::DictionaryString => 5,
        ColumnType::RawString => 6,
    }
}

fn column_type_from_id(id: u16) -> Result<ColumnType, ManifestError> {
    match id {
        1 => Ok(ColumnType::U64),
        2 => Ok(ColumnType::I64),
        3 => Ok(ColumnType::F64),
        4 => Ok(ColumnType::Bool),
        5 => Ok(ColumnType::DictionaryString),
        6 => Ok(ColumnType::RawString),
        _ => Err(ManifestError::Decode(format!("unknown column type {id}"))),
    }
}

struct ManifestCursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> ManifestCursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], ManifestError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| ManifestError::Decode("payload offset overflow".to_owned()))?;
        let value = self.bytes.get(self.position..end).ok_or_else(|| {
            ManifestError::Decode(format!(
                "payload truncated at {}, need {length}, total {}",
                self.position,
                self.bytes.len()
            ))
        })?;
        self.position = end;
        Ok(value)
    }

    fn u16(&mut self) -> Result<u16, ManifestError> {
        let raw: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| ManifestError::Decode("invalid u16".to_owned()))?;
        Ok(u16::from_le_bytes(raw))
    }

    fn u8(&mut self) -> Result<u8, ManifestError> {
        self.take(1)?
            .first()
            .copied()
            .ok_or_else(|| ManifestError::Decode("invalid u8".to_owned()))
    }

    fn u32(&mut self) -> Result<u32, ManifestError> {
        let raw: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| ManifestError::Decode("invalid u32".to_owned()))?;
        Ok(u32::from_le_bytes(raw))
    }

    fn u64(&mut self) -> Result<u64, ManifestError> {
        let raw: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| ManifestError::Decode("invalid u64".to_owned()))?;
        Ok(u64::from_le_bytes(raw))
    }

    fn optional_identity(&mut self) -> Result<Option<EpochIdentity>, ManifestError> {
        let tag = self.u8()?;
        if self.take(7)?.iter().any(|byte| *byte != 0) {
            return Err(ManifestError::Decode(
                "epoch alias reserved bytes are non-zero".to_owned(),
            ));
        }
        let embedding = self.u64()?;
        let tokenizer = self.u64()?;
        match (tag, embedding, tokenizer) {
            (0, 0, 0) => Ok(None),
            (1, embedding, tokenizer) => Ok(Some(EpochIdentity {
                embedding: EpochId::from_value(embedding),
                tokenizer: TokenizerEpoch::from_value(tokenizer),
            })),
            (0, _, _) => Err(ManifestError::Decode(
                "absent epoch alias carries non-zero identity bytes".to_owned(),
            )),
            (unknown, _, _) => Err(ManifestError::Decode(format!(
                "unknown epoch alias presence tag {unknown}"
            ))),
        }
    }

    fn optional_epoch_id(&mut self) -> Result<Option<EpochId>, ManifestError> {
        let tag = self.u8()?;
        if self.take(7)?.iter().any(|byte| *byte != 0) {
            return Err(ManifestError::Decode(
                "segment epoch reserved bytes are non-zero".to_owned(),
            ));
        }
        let epoch = self.u64()?;
        match (tag, epoch) {
            (0, 0) => Ok(None),
            (1, epoch) => Ok(Some(EpochId::from_value(epoch))),
            (0, _) => Err(ManifestError::Decode(
                "absent segment epoch tag carries a non-zero id".to_owned(),
            )),
            (unknown, _) => Err(ManifestError::Decode(format!(
                "unknown segment epoch presence tag {unknown}"
            ))),
        }
    }

    fn embedding_epoch(&mut self) -> Result<EmbeddingEpoch, ManifestError> {
        Ok(EmbeddingEpoch {
            document: self.embedding_tower()?,
            query: self.embedding_tower()?,
            alignment_digest: self.bytes()?,
        })
    }

    fn embedding_tower(&mut self) -> Result<EmbeddingTower, ManifestError> {
        let model_id = self.string()?;
        let model_version = self.string()?;
        let weights_digest = self.bytes()?;
        let dims = self.u32()?;
        let normalization = match self.u16()? {
            0 => Normalization::None,
            1 => Normalization::L2,
            unknown => {
                return Err(ManifestError::Decode(format!(
                    "unknown embedding normalization {unknown}"
                )));
            }
        };
        let prompt_prefix = self.string()?;
        let max_tokens = self.u32()?;
        let runtime = match self.u16()? {
            1 => EmbeddingRuntime::CoreMl,
            2 => EmbeddingRuntime::Mlx,
            3 => EmbeddingRuntime::CpuReference,
            unknown => {
                return Err(ManifestError::Decode(format!(
                    "unknown embedding runtime {unknown}"
                )));
            }
        };
        let compute_units = match self.u16()? {
            1 => ComputeUnits::Cpu,
            2 => ComputeUnits::CpuAndGpu,
            3 => ComputeUnits::CpuAndNeuralEngine,
            4 => ComputeUnits::All,
            unknown => {
                return Err(ManifestError::Decode(format!(
                    "unknown embedding compute units {unknown}"
                )));
            }
        };
        let os_tag = self.u8()?;
        if self.take(3)?.iter().any(|byte| *byte != 0) {
            return Err(ManifestError::Decode(
                "embedding OS-build reserved bytes are non-zero".to_owned(),
            ));
        }
        let os_build = match os_tag {
            0 => None,
            1 => Some(self.string()?),
            unknown => {
                return Err(ManifestError::Decode(format!(
                    "unknown embedding OS-build presence tag {unknown}"
                )));
            }
        };
        Ok(EmbeddingTower {
            model_id,
            model_version,
            weights_digest,
            dims,
            normalization,
            prompt_prefix,
            max_tokens,
            runtime,
            compute_units,
            os_build,
        })
    }

    fn i64(&mut self) -> Result<i64, ManifestError> {
        let raw: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| ManifestError::Decode("invalid i64".to_owned()))?;
        Ok(i64::from_le_bytes(raw))
    }

    const fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.position)
    }

    fn usize_from_u32(&mut self) -> Result<usize, ManifestError> {
        usize::try_from(self.u32()?)
            .map_err(|_| ManifestError::Decode("u32 length exceeds usize".to_owned()))
    }

    fn string(&mut self) -> Result<String, ManifestError> {
        let length = self.usize_from_u32()?;
        std::str::from_utf8(self.take(length)?)
            .map(str::to_owned)
            .map_err(|error| ManifestError::Decode(format!("invalid UTF-8: {error}")))
    }

    fn bytes(&mut self) -> Result<Vec<u8>, ManifestError> {
        let length = self.usize_from_u32()?;
        Ok(self.take(length)?.to_vec())
    }

    fn finish(&self) -> Result<(), ManifestError> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(ManifestError::Decode(format!(
                "{} trailing payload bytes",
                self.bytes.len().saturating_sub(self.position)
            )))
        }
    }
}
