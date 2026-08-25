//! Immutable, checksummed, memory-mapped segment files.

pub mod layout;
pub mod reader;
pub mod writer;

use std::path::PathBuf;

use crate::format::frame::FormatError;

/// Sortable 128-bit segment identifier with a 48-bit millisecond prefix.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct SegmentId([u8; 16]);

impl SegmentId {
    /// Constructs an identifier from a 48-bit millisecond timestamp and entropy.
    #[must_use]
    pub fn new(timestamp_millis: u64, entropy: [u8; 10]) -> Self {
        let timestamp = timestamp_millis.to_be_bytes();
        let mut bytes = [0_u8; 16];
        if let (Some(destination), Some(source)) = (bytes.get_mut(..6), timestamp.get(2..)) {
            destination.copy_from_slice(source);
        }
        if let Some(destination) = bytes.get_mut(6..) {
            destination.copy_from_slice(&entropy);
        }
        Self(bytes)
    }

    /// Restores an identifier from its permanent 16-byte representation.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Returns the permanent 16-byte representation.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Returns the canonical segment filename.
    #[must_use]
    pub fn file_name(self) -> String {
        let mut name = String::with_capacity(8 + 32 + 5);
        name.push_str("segment-");
        for byte in self.0 {
            use std::fmt::Write as _;
            let _ = write!(name, "{byte:02x}");
        }
        name.push_str(".zseg");
        name
    }
}

impl std::fmt::Display for SegmentId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Manifest-visible immutable segment metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentMeta {
    /// Sortable segment identity.
    pub id: SegmentId,
    /// Dense segment-local row count.
    pub row_count: u32,
    /// Permanent per-segment quantization scheme id.
    pub scheme: u16,
    /// Logical vector dimension.
    pub dims: u32,
    /// Exact immutable file length.
    pub file_size: u64,
    /// Manifest-stamped range of live values in the canonical `ts` column.
    pub clustering_key_range: ClusteringKeyRange,
}

impl SegmentMeta {
    pub(crate) fn same_segment_file(&self, other: &Self) -> bool {
        self.id == other.id
            && self.row_count == other.row_count
            && self.scheme == other.scheme
            && self.dims == other.dims
            && self.file_size == other.file_size
    }
}

/// Manifest-stamped range of live canonical `ts` clustering keys.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClusteringKeyRange {
    /// The segment predates clustering-key assignment and cannot be range-pruned.
    Unstamped,
    /// The segment contains no live rows, so its clustering-key set is empty.
    Empty,
    /// Inclusive minimum and maximum live timestamp values.
    Bounded {
        /// Smallest live `ts` value.
        min_ts: i64,
        /// Largest live `ts` value.
        max_ts: i64,
    },
}

/// Typed segment encode/open/read failure.
#[derive(Debug)]
pub enum SegmentError {
    /// Filesystem operation failed for a named path.
    Io {
        /// Path involved in the operation.
        path: PathBuf,
        /// Underlying platform error.
        source: std::io::Error,
    },
    /// A framed or checksummed byte contract failed.
    Format(FormatError),
    /// The opened file was a different immutable object.
    WrongObject {
        /// Artifact path supplied by the caller.
        artifact: String,
        /// Expected manifest identity.
        expected: SegmentId,
        /// Identity stored in the file.
        actual: SegmentId,
    },
    /// A required region was absent from the directory.
    MissingRegion(layout::RegionKind),
    /// Region geometry or a cross-region shape was invalid.
    Geometry(String),
    /// Metadata-column bytes were invalid.
    Columns(String),
    /// Alive-set bytes were invalid.
    Alive(String),
    /// Fixed-stride graph node-block bytes were invalid.
    Graph(crate::graph::block::GraphNodeError),
    /// Whole-segment lexical bytes were invalid.
    Postings(crate::fts::sealed::SealedSegmentError),
}

impl SegmentError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

impl std::fmt::Display for SegmentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(formatter, "segment I/O {}: {source}", path.display())
            }
            Self::Format(error) => error.fmt(formatter),
            Self::WrongObject {
                artifact,
                expected,
                actual,
            } => {
                write!(
                    formatter,
                    "artifact {artifact} failed object identity: expected {expected}, got {actual}"
                )
            }
            Self::MissingRegion(kind) => write!(formatter, "segment is missing region {kind:?}"),
            Self::Geometry(detail) => write!(formatter, "segment geometry is invalid: {detail}"),
            Self::Columns(detail) => write!(formatter, "segment columns are invalid: {detail}"),
            Self::Alive(detail) => write!(formatter, "segment alive set is invalid: {detail}"),
            Self::Graph(error) => write!(formatter, "segment graph region is invalid: {error}"),
            Self::Postings(error) => {
                write!(formatter, "segment postings region is invalid: {error}")
            }
        }
    }
}

impl std::error::Error for SegmentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Format(error) => Some(error),
            Self::Graph(error) => Some(error),
            Self::Postings(error) => Some(error),
            Self::WrongObject { .. }
            | Self::MissingRegion(_)
            | Self::Geometry(_)
            | Self::Columns(_)
            | Self::Alive(_) => None,
        }
    }
}

impl From<FormatError> for SegmentError {
    fn from(error: FormatError) -> Self {
        Self::Format(error)
    }
}

impl From<crate::graph::block::GraphNodeError> for SegmentError {
    fn from(error: crate::graph::block::GraphNodeError) -> Self {
        Self::Graph(error)
    }
}

impl From<crate::fts::sealed::SealedSegmentError> for SegmentError {
    fn from(error: crate::fts::sealed::SealedSegmentError) -> Self {
        Self::Postings(error)
    }
}
