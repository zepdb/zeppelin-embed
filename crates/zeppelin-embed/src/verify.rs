//! Read-only end-to-end store verification.
//!
//! [`verify_store`](crate::verify::verify_store) walks one store directory with the decoders the engine
//! already uses to open it: the manifest frame, every referenced segment's
//! header, region checksums, file trailer and region decoders, and the WAL
//! prefix through the same replay that recovery runs. It reports every damaged
//! artifact it finds instead of stopping at the first, and it never writes:
//! the walk calls only the read half of [`Vfs`](crate::vfs::Vfs) (`open`, `read`,
//! `open_for_map` and `list`).
//!
//! A finding is damage, never a notice. Recovery refuses a WAL that does not
//! end cleanly, so a torn WAL tail is reported like any other WAL corruption.
//! Unreferenced segment files and leftover temporary files are not findings,
//! because a writable open removes them without losing data.
//!
//! The walk reads a point-in-time view of the directory. Verifying a store
//! while another process writes to it can report a write that is in flight.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::fts::tokenizer::{Analyzer, TokenizerConfig};
use crate::ingest::{PURGE_INTENT_FILE, read_intent};
use crate::lifecycle::stats::Accounting;
use crate::manifest::io::{DurableLog, MANIFEST_FILE};
use crate::manifest::{Manifest, decode_manifest};
use crate::meta::Schema;
use crate::segment::SegmentError;
use crate::segment::SegmentMeta;
use crate::segment::layout::RegionKind;
use crate::segment::reader::SegmentReader;
use crate::vfs::{StdVfs, Vfs};
use crate::wal::replay::ReplayTerminator;
use crate::wal::{WalReadError, WalReader};

/// Canonical WAL filename inside a store directory.
const WAL_FILE: &str = "wal.ze";

/// What is damaged. Values are stable; new kinds are only appended.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum FindingKind {
    /// `manifest.ze` is absent, but the WAL or segment files prove that a
    /// committed snapshot existed and data it covered is now unreachable.
    ManifestMissing,
    /// The manifest frame, checksum, or payload failed to decode.
    ManifestCorrupt,
    /// The manifest covers WAL sequences that the WAL does not hold.
    ManifestAheadOfWal,
    /// A segment the manifest references does not exist.
    SegmentMissing,
    /// A segment header, length, identity, or file trailer failed validation.
    SegmentCorrupt,
    /// A segment header disagrees with the manifest's record of it.
    SegmentMismatch,
    /// A segment region's checksum does not match its bytes.
    SegmentRegionCorrupt,
    /// A checksum-valid region failed its decoder or cross-structure checks.
    SegmentIndexInvalid,
    /// The WAL is absent although the manifest covers WAL sequences.
    WalMissing,
    /// The WAL file header is truncated or invalid.
    WalHeaderCorrupt,
    /// A WAL record failed framing, checksum, or sequence validation.
    WalRecordCorrupt,
    /// A checksum-valid WAL record cannot be replayed into the store.
    WalRecordInvalid,
    /// A store file exists but could not be read.
    Unreadable,
    /// The pending purge intent `purge.ze` failed its frame or decoder.
    PurgeIntentCorrupt,
}

impl FindingKind {
    /// Stable lower-camel-case name used by the bindings.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ManifestMissing => "manifestMissing",
            Self::ManifestCorrupt => "manifestCorrupt",
            Self::ManifestAheadOfWal => "manifestAheadOfWal",
            Self::SegmentMissing => "segmentMissing",
            Self::SegmentCorrupt => "segmentCorrupt",
            Self::SegmentMismatch => "segmentMismatch",
            Self::SegmentRegionCorrupt => "segmentRegionCorrupt",
            Self::SegmentIndexInvalid => "segmentIndexInvalid",
            Self::WalMissing => "walMissing",
            Self::WalHeaderCorrupt => "walHeaderCorrupt",
            Self::WalRecordCorrupt => "walRecordCorrupt",
            Self::WalRecordInvalid => "walRecordInvalid",
            Self::Unreadable => "unreadable",
            Self::PurgeIntentCorrupt => "purgeIntentCorrupt",
        }
    }
}

/// One damaged artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Finding {
    /// What is damaged.
    pub kind: FindingKind,
    /// File name relative to the store directory.
    pub file: String,
    /// Byte offset of the damage inside `file`, when the decoder knows it.
    pub offset: Option<u64>,
    /// Human-readable decoder detail.
    pub detail: String,
}

/// Result of one verification walk.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VerifyReport {
    /// Every damaged artifact, in walk order: manifest, segments, WAL.
    pub findings: Vec<Finding>,
    /// Generation of the decoded manifest, or zero without one.
    pub generation: u64,
    /// Segments the manifest references, whether or not they were readable.
    pub segments_checked: u64,
    /// WAL records that passed checksum and sequence validation.
    pub wal_records_checked: u64,
}

impl VerifyReport {
    /// True when no damage was found.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Why a walk could not start. Damage inside the store is never an error; it
/// is a [`Finding`].
#[derive(Debug)]
pub enum VerifyError {
    /// The store path does not exist.
    NotFound {
        /// Path supplied by the caller.
        path: PathBuf,
    },
    /// The store path exists but is not a directory.
    NotDirectory {
        /// Path supplied by the caller.
        path: PathBuf,
    },
    /// The directory could not be inspected or listed.
    Io {
        /// Path involved in the operation.
        path: PathBuf,
        /// Underlying platform error.
        source: std::io::Error,
    },
    /// The default tokenizer could not be built for WAL replay.
    Tokenizer(String),
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { path } => {
                write!(formatter, "store path {} does not exist", path.display())
            }
            Self::NotDirectory { path } => {
                write!(
                    formatter,
                    "store path {} is not a directory",
                    path.display()
                )
            }
            Self::Io { path, source } => {
                write!(formatter, "store I/O {}: {source}", path.display())
            }
            Self::Tokenizer(detail) => write!(formatter, "verify tokenizer: {detail}"),
        }
    }
}

impl std::error::Error for VerifyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::NotFound { .. } | Self::NotDirectory { .. } | Self::Tokenizer(_) => None,
        }
    }
}

/// Verifies the store directory at `path` on the platform filesystem.
pub fn verify_store(path: impl AsRef<Path>) -> Result<VerifyReport, VerifyError> {
    verify_store_on_vfs(&StdVfs, path.as_ref())
}

/// Verifies the store directory at `path` through `vfs`, using only its read
/// operations.
pub fn verify_store_on_vfs(vfs: &dyn Vfs, path: &Path) -> Result<VerifyReport, VerifyError> {
    match vfs.ensure_directory(path, false) {
        Ok(true) => {}
        Ok(false) => {
            return Err(VerifyError::NotDirectory {
                path: path.to_path_buf(),
            });
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(VerifyError::NotFound {
                path: path.to_path_buf(),
            });
        }
        Err(source) => {
            return Err(VerifyError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    }
    let entries = vfs.list(path).map_err(|source| VerifyError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let has_segment_files = entries.iter().any(|entry| is_segment_file(entry));
    let mut walk = Walk {
        vfs,
        directory: path,
        report: VerifyReport::default(),
    };
    let manifest = walk.manifest();
    if let Some(decoded) = manifest.decoded() {
        walk.report.generation = decoded.generation;
        walk.segments(decoded);
    }
    walk.wal(&manifest, has_segment_files)?;
    walk.purge_intent();
    Ok(walk.report)
}

struct Walk<'a> {
    vfs: &'a dyn Vfs,
    directory: &'a Path,
    report: VerifyReport,
}

/// What the manifest walk established.
enum ManifestState {
    /// No `manifest.ze`: the store never committed a snapshot, or lost it.
    Absent,
    /// A manifest exists but is unreadable or corrupt; already reported.
    Damaged,
    /// A decoded manifest.
    Decoded(Manifest),
}

impl ManifestState {
    fn decoded(&self) -> Option<&Manifest> {
        match self {
            Self::Decoded(manifest) => Some(manifest),
            Self::Absent | Self::Damaged => None,
        }
    }
}

impl Walk<'_> {
    fn record(&mut self, kind: FindingKind, file: &str, offset: Option<u64>, detail: String) {
        self.report.findings.push(Finding {
            kind,
            file: file.to_owned(),
            offset,
            detail,
        });
    }

    fn manifest(&mut self) -> ManifestState {
        let bytes = match self.vfs.read(&self.directory.join(MANIFEST_FILE)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return ManifestState::Absent;
            }
            Err(error) => {
                self.record(
                    FindingKind::Unreadable,
                    MANIFEST_FILE,
                    None,
                    error.to_string(),
                );
                return ManifestState::Damaged;
            }
        };
        match decode_manifest(MANIFEST_FILE, &bytes) {
            Ok(manifest) => ManifestState::Decoded(manifest),
            Err(error) => {
                self.record(
                    FindingKind::ManifestCorrupt,
                    MANIFEST_FILE,
                    None,
                    error.to_string(),
                );
                ManifestState::Damaged
            }
        }
    }

    fn segments(&mut self, manifest: &Manifest) {
        // Snapshot readers decode columns against the manifest schema, so an
        // older segment reads an added nullable attribute as null and a
        // conflicting column fails; verify decodes the same way.
        let schema = Arc::new(manifest.schema.clone());
        for expected in &manifest.segments {
            self.report.segments_checked = self.report.segments_checked.saturating_add(1);
            self.segment(expected, &schema);
        }
    }

    /// A pending intent is a purge the next writable open completes, so only
    /// an intent that fails to decode is damage.
    fn purge_intent(&mut self) {
        let path = self.directory.join(PURGE_INTENT_FILE);
        match self.vfs.open(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                self.record(
                    FindingKind::Unreadable,
                    PURGE_INTENT_FILE,
                    None,
                    error.to_string(),
                );
                return;
            }
        }
        if let Err(error) = read_intent(self.vfs, self.directory) {
            self.record(
                FindingKind::PurgeIntentCorrupt,
                PURGE_INTENT_FILE,
                None,
                error.to_string(),
            );
        }
    }

    fn segment(&mut self, expected: &SegmentMeta, schema: &Arc<Schema>) {
        let file = expected.id.file_name();
        let path = self.directory.join(&file);
        let reader = match SegmentReader::open(self.vfs, &path, expected.id) {
            Ok(reader) => reader.with_collection_schema(Arc::clone(schema)),
            Err(SegmentError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                self.record(
                    FindingKind::SegmentMissing,
                    &file,
                    None,
                    format!(
                        "the manifest references segment {} with {} rows",
                        expected.id, expected.row_count
                    ),
                );
                return;
            }
            Err(SegmentError::Io { source, .. }) => {
                self.record(FindingKind::Unreadable, &file, None, source.to_string());
                return;
            }
            Err(error) => {
                self.record(FindingKind::SegmentCorrupt, &file, None, error.to_string());
                return;
            }
        };
        if !reader.meta().same_segment_file(expected) {
            self.record(
                FindingKind::SegmentMismatch,
                &file,
                None,
                format!(
                    "manifest records {expected:?}, segment header records {:?}",
                    reader.meta()
                ),
            );
            return;
        }
        let mut regions_valid = true;
        for entry in reader.directory() {
            if let Err(error) = reader.region_by_id(entry.kind) {
                regions_valid = false;
                self.record(
                    FindingKind::SegmentRegionCorrupt,
                    &file,
                    Some(entry.offset),
                    format!("{}: {error}", region_name(entry.kind)),
                );
            }
        }
        if !regions_valid {
            return;
        }
        if let Err(error) = reader.validate_all() {
            self.record(FindingKind::SegmentCorrupt, &file, None, error.to_string());
            return;
        }
        if let Err(error) = decode_regions(&reader) {
            self.record(
                FindingKind::SegmentIndexInvalid,
                &file,
                None,
                error.to_string(),
            );
        }
    }

    fn wal(
        &mut self,
        manifest: &ManifestState,
        has_segment_files: bool,
    ) -> Result<(), VerifyError> {
        let decoded = manifest.decoded();
        let absent = matches!(manifest, ManifestState::Absent);
        let absorbed_through = decoded.map_or(0, |manifest| manifest.log_seq);
        let wal_path = self.directory.join(WAL_FILE);
        match self.vfs.open(&wal_path) {
            Ok(length) if length > 0 => {}
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                self.record(FindingKind::Unreadable, WAL_FILE, None, error.to_string());
                return Ok(());
            }
            Ok(_) | Err(_) => {
                if absorbed_through > 0 {
                    self.record(
                        FindingKind::WalMissing,
                        WAL_FILE,
                        None,
                        format!("the manifest covers WAL sequences through {absorbed_through}"),
                    );
                } else if absent && has_segment_files {
                    self.record(
                        FindingKind::ManifestMissing,
                        MANIFEST_FILE,
                        None,
                        "segment files exist but neither a manifest nor a WAL holds their rows"
                            .to_owned(),
                    );
                }
                return Ok(());
            }
        }
        let reader = match WalReader::open(self.vfs, &wal_path) {
            Ok(reader) => reader,
            Err(WalReadError::Io(error)) => {
                self.record(FindingKind::Unreadable, WAL_FILE, None, error.to_string());
                return Ok(());
            }
            Err(error) => {
                self.record(
                    FindingKind::WalHeaderCorrupt,
                    WAL_FILE,
                    Some(0),
                    error.to_string(),
                );
                return Ok(());
            }
        };
        self.report.wal_records_checked = u64::try_from(reader.records().len()).unwrap_or(u64::MAX);
        match reader.terminator() {
            Some(ReplayTerminator::CleanEnd) | None => {}
            Some(ReplayTerminator::InvalidHeader(error)) => {
                self.record(
                    FindingKind::WalHeaderCorrupt,
                    WAL_FILE,
                    Some(0),
                    error.to_string(),
                );
                return Ok(());
            }
            Some(ReplayTerminator::CorruptAt { offset, reason }) => {
                self.record(
                    FindingKind::WalRecordCorrupt,
                    WAL_FILE,
                    Some(u64::try_from(offset).unwrap_or(u64::MAX)),
                    format!("{reason:?}"),
                );
                return Ok(());
            }
        }
        let durable_end = reader.durable_end();
        let first_seq = reader
            .records()
            .first()
            .map_or(durable_end.saturating_add(1), |record| record.seq.get());
        drop(reader);
        if durable_end < absorbed_through {
            self.record(
                FindingKind::ManifestAheadOfWal,
                MANIFEST_FILE,
                None,
                format!(
                    "the manifest covers WAL sequences through {absorbed_through}, \
                     the WAL ends at {durable_end}"
                ),
            );
            return Ok(());
        }
        if absent && first_seq > 1 {
            self.record(
                FindingKind::ManifestMissing,
                MANIFEST_FILE,
                None,
                format!(
                    "the WAL starts at sequence {first_seq}; earlier sequences were \
                     absorbed by a snapshot that is gone"
                ),
            );
            return Ok(());
        }
        if matches!(manifest, ManifestState::Damaged) {
            // Replay needs the manifest's schema and absorbed boundary.
            return Ok(());
        }
        let schema =
            decoded.map_or_else(Schema::timestamp_only, |manifest| manifest.schema.clone());
        let analyzer = Analyzer::new(TokenizerConfig::text_default())
            .map_err(|error| VerifyError::Tokenizer(error.to_string()))?;
        let accounting = Arc::new(Accounting::new(u64::MAX, u64::MAX));
        let generation = decoded.map_or(0, |manifest| manifest.generation);
        if let Err(error) = crate::ingest::ActiveState::recover(
            self.vfs,
            &wal_path,
            generation,
            absorbed_through,
            {
                #[cfg(feature = "graph-cypher")]
                {
                    decoded
                        .and_then(|manifest| manifest.graph.as_ref())
                        .map_or(absorbed_through, |graph| graph.graph_absorbed_through)
                }
                #[cfg(not(feature = "graph-cypher"))]
                {
                    absorbed_through
                }
            },
            &accounting,
            &schema,
            &analyzer,
            None,
        ) {
            if absent && has_segment_files {
                self.record(
                    FindingKind::ManifestMissing,
                    MANIFEST_FILE,
                    None,
                    format!("the WAL cannot be replayed without the manifest's schema: {error}"),
                );
            } else {
                self.record(
                    FindingKind::WalRecordInvalid,
                    WAL_FILE,
                    None,
                    error.to_string(),
                );
            }
        }
        Ok(())
    }
}

/// Runs every typed decoder whose region is present. Each decoder also checks
/// its row count against the segment header, which is the cross-structure
/// index consistency the engine relies on at query time.
fn decode_regions(reader: &SegmentReader) -> Result<(), SegmentError> {
    reader.columns()?;
    reader.alive()?;
    reader.postings()?;
    reader.stored_text()?;
    reader.stored_metadata()?;
    if reader
        .directory()
        .iter()
        .any(|entry| entry.kind == RegionKind::GraphNodeBlocks.id())
    {
        reader.graph_node_blocks()?;
    }
    Ok(())
}

fn region_name(kind: u16) -> String {
    RegionKind::from_id(kind).map_or_else(|| format!("region {kind}"), |kind| format!("{kind:?}"))
}

fn is_segment_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("segment-") && name.ends_with(".zseg"))
}
