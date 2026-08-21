//! Temp-write/sync/rename manifest commit, bounded open, and reachability sweep.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::lifecycle::durability::{DurabilityPolicy, SyncRequirement};
use crate::segment::reader::validate_header_with_vfs;
use crate::vfs::Vfs;

use super::{Manifest, ManifestError, decode_manifest, encode_manifest};

/// Canonical committed-manifest filename.
pub const MANIFEST_FILE: &str = "manifest.ze";
/// Canonical unpublished temp-manifest filename.
pub const MANIFEST_TEMP_FILE: &str = ".manifest.ze.tmp";

/// Stub seam exposing the durable WAL end; Task 08 supplies the real implementation.
pub trait DurableLog: Send + Sync {
    /// Returns the largest sequence known durable.
    fn durable_end(&self) -> u64;
}

/// Result of an O(manifest)-bounded store open and orphan sweep.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenManifest {
    /// Fully decoded committed snapshot.
    pub manifest: Manifest,
    /// Bytes reclaimed from unreachable immutable segment files.
    pub bytes_reclaimed: u64,
    /// Exact bounded bytes read: manifest plus segment headers.
    pub bytes_read: u64,
}

/// Commits one manifest through temp-write, sync, rename, and directory sync.
pub fn commit_manifest(
    vfs: &dyn Vfs,
    directory: &Path,
    manifest: &Manifest,
    policy: DurabilityPolicy,
) -> Result<(), ManifestError> {
    let bytes = encode_manifest(manifest)?;
    let temporary = directory.join(MANIFEST_TEMP_FILE);
    let committed = directory.join(MANIFEST_FILE);
    vfs.write(&temporary, &bytes)
        .map_err(|error| ManifestError::io(&temporary, error))?;
    match policy.data_file_sync() {
        SyncRequirement::Skip => {}
        SyncRequirement::Sync(kind) => vfs
            .sync(&temporary, kind)
            .map_err(|error| ManifestError::io(&temporary, error))?,
    }
    vfs.rename(&temporary, &committed)
        .map_err(|error| ManifestError::io(&committed, error))?;
    match policy.directory_sync() {
        SyncRequirement::Skip => Ok(()),
        SyncRequirement::Sync(kind) => vfs
            .sync(directory, kind)
            .map_err(|error| ManifestError::io(directory, error)),
    }
}

/// Loads and checksums a manifest and refuses snapshots ahead of the supplied log.
pub fn load_manifest(
    vfs: &dyn Vfs,
    path: &Path,
    durable_log_end: u64,
) -> Result<Manifest, ManifestError> {
    let bytes = vfs
        .read(path)
        .map_err(|error| ManifestError::io(path, error))?;
    let manifest = decode_manifest(&path.display().to_string(), &bytes)?;
    if manifest.log_seq > durable_log_end {
        return Err(ManifestError::AheadOfLog {
            snapshot: manifest.log_seq,
            durable: durable_log_end,
        });
    }
    Ok(manifest)
}

/// Opens a store using only the manifest and referenced segment headers, then sweeps orphans.
pub fn open_manifest(
    vfs: &dyn Vfs,
    directory: &Path,
    log: &dyn DurableLog,
    writing_exclusions: &HashSet<PathBuf>,
) -> Result<OpenManifest, ManifestError> {
    let manifest_path = directory.join(MANIFEST_FILE);
    let manifest_file_length = vfs
        .open(&manifest_path)
        .map_err(|error| ManifestError::io(&manifest_path, error))?;
    let manifest = load_manifest(vfs, &manifest_path, log.durable_end())?;
    let mut header_bytes = 0_u64;
    for segment in &manifest.segments {
        let path = directory.join(segment.id.file_name());
        let touched = validate_header_with_vfs(vfs, &path, segment)?;
        header_bytes = header_bytes.saturating_add(touched as u64);
    }
    let bytes_reclaimed = sweep_orphans(vfs, directory, &manifest, writing_exclusions)?;
    Ok(OpenManifest {
        manifest,
        bytes_reclaimed,
        bytes_read: manifest_file_length.saturating_add(header_bytes),
    })
}

fn sweep_orphans(
    vfs: &dyn Vfs,
    directory: &Path,
    manifest: &Manifest,
    writing_exclusions: &HashSet<PathBuf>,
) -> Result<u64, ManifestError> {
    let reachable = manifest
        .segments
        .iter()
        .map(|segment| directory.join(segment.id.file_name()))
        .collect::<HashSet<_>>();
    let mut reclaimed = 0_u64;
    for path in vfs
        .list(directory)
        .map_err(|error| ManifestError::io(directory, error))?
    {
        if !is_segment_file(&path)
            || reachable.contains(&path)
            || writing_exclusions.contains(&path)
        {
            continue;
        }
        let length = vfs
            .open(&path)
            .map_err(|error| ManifestError::io(&path, error))?;
        vfs.delete(&path)
            .map_err(|error| ManifestError::io(&path, error))?;
        reclaimed = reclaimed.saturating_add(length);
    }
    Ok(reclaimed)
}

fn is_segment_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("segment-") && name.ends_with(".zseg"))
}
