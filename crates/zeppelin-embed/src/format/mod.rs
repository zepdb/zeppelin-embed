//! Append-only persisted-format registry.

pub mod frame;
pub mod golden;

/// A persisted artifact family identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum FormatFamily {
    /// Generic length-prefixed framing.
    Frame = 1,
    /// Immutable segment files.
    Segment = 2,
    /// Packed vector-code regions.
    VectorCodes = 3,
    /// Quantizer factor-record regions.
    VectorFactors = 4,
    /// Exact-rescore vector regions.
    VectorRescore = 5,
    /// Typed metadata-column regions.
    Columns = 6,
    /// Alive/tombstone regions.
    Alive = 7,
    /// Reserved full-text postings regions.
    Postings = 8,
    /// Per-chunk xxh3 checksum tables.
    ChecksumTable = 9,
    /// Store manifests.
    Manifest = 10,
    /// Append-only write-ahead-log records.
    Wal = 11,
    /// Fixed-stride graph node-block regions.
    GraphNodeBlocks = 12,
    /// Dense sealed-row document identifiers and revisions.
    DocumentVersions = 13,
    /// Dense length-delimited opaque stored metadata rows.
    StoredMetadata = 14,
    /// Durable request proving an unfinished physical purge must resume.
    PurgeIntent = 15,
}

impl FormatFamily {
    /// Returns the permanent numeric family identifier.
    #[must_use]
    pub const fn id(self) -> u16 {
        self as u16
    }

    pub(crate) const fn current_version(self) -> u16 {
        match self {
            Self::Manifest => 2,
            Self::Frame
            | Self::Segment
            | Self::VectorCodes
            | Self::VectorFactors
            | Self::VectorRescore
            | Self::Columns
            | Self::Alive
            | Self::Postings
            | Self::ChecksumTable
            | Self::Wal
            | Self::GraphNodeBlocks
            | Self::DocumentVersions
            | Self::StoredMetadata
            | Self::PurgeIntent => 1,
        }
    }
}

/// One append-only version declaration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FamilySpec {
    /// The persisted family.
    pub family: FormatFamily,
    /// The version emitted by writers.
    pub current_version: u16,
    /// Lowest version accepted by readers.
    pub minimum_accepted_version: u16,
    /// Highest version accepted by readers.
    pub maximum_accepted_version: u16,
}

/// Registry validation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryError {
    /// No persisted family owns this identifier.
    UnknownFamily(u16),
    /// The declared version is outside the reader's accepted interval.
    UnsupportedVersion {
        /// Artifact family identifier.
        family: u16,
        /// Version found in the artifact.
        version: u16,
        /// Lowest accepted version.
        minimum: u16,
        /// Highest accepted version.
        maximum: u16,
    },
    /// A retired quantization scheme identifier was requested.
    RetiredScheme(u16),
    /// No quantization scheme owns this identifier.
    UnknownScheme(u16),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownFamily(family) => write!(formatter, "unknown format family {family}"),
            Self::UnsupportedVersion {
                family,
                version,
                minimum,
                maximum,
            } => write!(
                formatter,
                "format family {family} version {version} is outside accepted range {minimum}..={maximum}"
            ),
            Self::RetiredScheme(scheme) => {
                write!(
                    formatter,
                    "quantization scheme id {scheme} is permanently retired"
                )
            }
            Self::UnknownScheme(scheme) => {
                write!(formatter, "unknown quantization scheme {scheme}")
            }
        }
    }
}

impl std::error::Error for RegistryError {}

/// Static registry for every persisted family and quantization identifier.
pub struct FormatRegistry;

const FAMILIES: [FamilySpec; 15] = [
    FamilySpec {
        family: FormatFamily::Frame,
        current_version: 1,
        minimum_accepted_version: 1,
        maximum_accepted_version: 1,
    },
    FamilySpec {
        family: FormatFamily::Segment,
        current_version: 1,
        minimum_accepted_version: 1,
        maximum_accepted_version: 1,
    },
    FamilySpec {
        family: FormatFamily::VectorCodes,
        current_version: 1,
        minimum_accepted_version: 1,
        maximum_accepted_version: 1,
    },
    FamilySpec {
        family: FormatFamily::VectorFactors,
        current_version: 1,
        minimum_accepted_version: 1,
        maximum_accepted_version: 1,
    },
    FamilySpec {
        family: FormatFamily::VectorRescore,
        current_version: 1,
        minimum_accepted_version: 1,
        maximum_accepted_version: 1,
    },
    FamilySpec {
        family: FormatFamily::Columns,
        current_version: 1,
        minimum_accepted_version: 1,
        maximum_accepted_version: 1,
    },
    FamilySpec {
        family: FormatFamily::Alive,
        current_version: 1,
        minimum_accepted_version: 1,
        maximum_accepted_version: 1,
    },
    FamilySpec {
        family: FormatFamily::Postings,
        current_version: 1,
        minimum_accepted_version: 1,
        maximum_accepted_version: 1,
    },
    FamilySpec {
        family: FormatFamily::ChecksumTable,
        current_version: 1,
        minimum_accepted_version: 1,
        maximum_accepted_version: 1,
    },
    FamilySpec {
        family: FormatFamily::Manifest,
        current_version: 2,
        minimum_accepted_version: 2,
        maximum_accepted_version: 2,
    },
    FamilySpec {
        family: FormatFamily::Wal,
        current_version: 1,
        minimum_accepted_version: 1,
        maximum_accepted_version: 1,
    },
    FamilySpec {
        family: FormatFamily::GraphNodeBlocks,
        current_version: 1,
        minimum_accepted_version: 1,
        maximum_accepted_version: 1,
    },
    FamilySpec {
        family: FormatFamily::DocumentVersions,
        current_version: 1,
        minimum_accepted_version: 1,
        maximum_accepted_version: 1,
    },
    FamilySpec {
        family: FormatFamily::StoredMetadata,
        current_version: 1,
        minimum_accepted_version: 1,
        maximum_accepted_version: 1,
    },
    FamilySpec {
        family: FormatFamily::PurgeIntent,
        current_version: 1,
        minimum_accepted_version: 1,
        maximum_accepted_version: 1,
    },
];

impl FormatRegistry {
    /// Returns every registered family in permanent identifier order.
    #[must_use]
    pub const fn families() -> &'static [FamilySpec] {
        &FAMILIES
    }

    /// Resolves and validates one family version.
    pub fn require(family: u16, version: u16) -> Result<&'static FamilySpec, RegistryError> {
        let spec = FAMILIES
            .iter()
            .find(|candidate| candidate.family.id() == family)
            .ok_or(RegistryError::UnknownFamily(family))?;
        if version < spec.minimum_accepted_version || version > spec.maximum_accepted_version {
            return Err(RegistryError::UnsupportedVersion {
                family,
                version,
                minimum: spec.minimum_accepted_version,
                maximum: spec.maximum_accepted_version,
            });
        }
        Ok(spec)
    }

    /// Rejects retired ids 3 and 5 and validates every live v1 scheme id.
    pub const fn require_scheme(scheme: u16) -> Result<(), RegistryError> {
        match scheme {
            0 | 1 | 2 | 4 => Ok(()),
            3 | 5 => Err(RegistryError::RetiredScheme(scheme)),
            _ => Err(RegistryError::UnknownScheme(scheme)),
        }
    }
}
