//! Host-scheduled, bounded copy-on-write merging of small scan segments.

use std::sync::Arc;

use crate::graph::consolidate::{ConsolidateError, merge_segments, merged_clustering};
use crate::lifecycle::durability::SyncRequirement;
use crate::lifecycle::{CancelToken, PublishedSnapshot, Store, StoreError, StoreState};
use crate::manifest::io::{MANIFEST_FILE, commit_manifest, load_manifest};
use crate::segment::layout::RegionKind;
use crate::segment::reader::SegmentReader;
use crate::segment::{SegmentError, SegmentId};
use crate::vfs::Vfs;

// Admission bounds the existing gather/re-encode implementation independently
// of corpus size. Decoded structures require more memory than the input bytes.
const MAX_INPUT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_INPUTS: usize = 16;
const MERGE_ID_SEED: u64 = 0x5a45_0234_6d65_7267;

impl Store {
    /// Merges small compatible sealed scan segments during host-selected idle time.
    ///
    /// Each atomic batch admits at most 16 segments and 8 MiB of input files.
    /// Decoding and rebuilding require additional bounded working memory. Large
    /// segments and graph segments stay separate. Repeats while a batch fits;
    /// this synchronous call blocks writers and does not seal active writes.
    /// Open snapshot views defer input unlinking until later orphan cleanup.
    /// Returns the last publication generation, unchanged if no merge is due.
    pub fn merge_sealed(&self) -> Result<u64, StoreError> {
        self.merge_sealed_with_cancel(&CancelToken::new())
    }

    /// Cancels between batches or before publication; completed batches remain.
    pub fn merge_sealed_with_cancel(&self, cancel: &CancelToken) -> Result<u64, StoreError> {
        self.merge_sealed_with_cancel_on_vfs(cancel, self.vfs.as_ref())
    }

    /// Filesystem seam for deterministic interrupted-publication tests.
    #[doc(hidden)]
    pub fn merge_sealed_with_cancel_on_vfs(
        &self,
        cancel: &CancelToken,
        vfs: &dyn Vfs,
    ) -> Result<u64, StoreError> {
        let state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(StoreError::Closing),
            StoreState::Closed => return Err(StoreError::Closed),
        }
        let writer = self
            .writer_lock
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "writer lock",
            })?;
        if writer.is_none() {
            return Err(StoreError::ReadOnly);
        }
        let wal = self
            .wal_writer
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "WAL writer",
            })?;
        let durable_end = wal.as_ref().ok_or(StoreError::ReadOnly)?.durable_end();
        let mut active = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        let current = active.as_mut().ok_or(StoreError::Closed)?;
        let path = self.directory.join(MANIFEST_FILE);
        check_cancel(cancel)?;
        match vfs.open(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(current.generation);
            }
            Err(source) => return Err(StoreError::Io { path, source }),
        }
        let mut manifest = load_manifest(vfs, &path, durable_end).map_err(StoreError::Manifest)?;
        loop {
            check_cancel(cancel)?;
            let inputs = select_inputs(vfs, &self.directory, &manifest.segments)?;
            if inputs.len() < 2 {
                return Ok(current.generation.max(manifest.generation));
            }
            let generation = current
                .generation
                .max(manifest.generation)
                .checked_add(1)
                .ok_or(StoreError::GenerationOverflow)?;
            let id = merge_id(generation, &inputs);
            let output_path = self.directory.join(id.file_name());
            // Never let the segment writer's rename overwrite a live artifact
            // or a leftover output from an interrupted invocation.
            match vfs.open(&output_path) {
                Ok(_) => {
                    return Err(StoreError::Segment(SegmentError::Geometry(format!(
                        "merge output {id} already exists; reopen before retrying"
                    ))));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(StoreError::Io {
                        path: output_path,
                        source,
                    });
                }
            }
            let readers = inputs.iter().collect::<Vec<_>>();
            let merged = merge_segments(
                vfs,
                &self.directory,
                &readers,
                id,
                &self.tokenizer,
                self.durability_policy,
            )
            .map_err(merge_error)?;
            let replacement = if merged.is_some() {
                let output =
                    SegmentReader::open(vfs, &output_path, id).map_err(StoreError::Segment)?;
                let mut meta = output.meta().clone();
                meta.clustering_key_range = merged_clustering(
                    &output.columns().map_err(StoreError::Segment)?,
                    &output.alive().map_err(StoreError::Segment)?,
                )
                .map_err(merge_error)?;
                meta.epoch_id = inputs
                    .first()
                    .and_then(|input| {
                        manifest
                            .segments
                            .iter()
                            .find(|meta| meta.id == input.meta().id)
                    })
                    .and_then(|meta| meta.epoch_id);
                Some(meta)
            } else {
                None // A batch containing only tombstones can be removed.
            };
            if cancel.is_cancelled() {
                if replacement.is_some() {
                    vfs.delete(&output_path).map_err(|source| StoreError::Io {
                        path: output_path,
                        source,
                    })?;
                }
                return Err(StoreError::SealCancelled);
            }
            manifest
                .segments
                .retain(|meta| !inputs.iter().any(|input| input.meta().id == meta.id));
            manifest.segments.extend(replacement);
            manifest.generation = generation;
            // Preserve log_seq: a merge absorbs no active or WAL records.
            commit_manifest(vfs, &self.directory, &manifest, self.durability_policy)
                .map_err(StoreError::Manifest)?;
            let remapped = PublishedSnapshot::load_on_vfs(&self.directory, &self.accounting, vfs)?;
            let mut published = self
                .snapshot
                .write()
                .map_err(|_| StoreError::Synchronization {
                    component: "published snapshot",
                })?;
            let previous = published.replace(Arc::new(remapped));
            current.generation = generation;
            drop(published);
            drop(previous);
            let old_paths = inputs
                .iter()
                .map(|input| self.directory.join(input.meta().id.file_name()))
                .collect::<Vec<_>>();
            drop(inputs);
            // An open view retains the input paths as well as their mappings.
            // After views close, writable-open orphan cleanup reclaims them.
            if !self.has_snapshot_views() {
                for path in old_paths {
                    vfs.delete(&path)
                        .map_err(|source| StoreError::Io { path, source })?;
                }
            }
            if let SyncRequirement::Sync(kind) = self.durability_policy.directory_sync() {
                vfs.sync(&self.directory, kind)
                    .map_err(|source| StoreError::Io {
                        path: self.directory.clone(),
                        source,
                    })?;
            }
        }
    }
}

fn select_inputs(
    vfs: &dyn Vfs,
    directory: &std::path::Path,
    segments: &[crate::segment::SegmentMeta],
) -> Result<Vec<SegmentReader>, StoreError> {
    let mut candidates = segments
        .iter()
        .filter(|meta| meta.scheme == 4 && meta.file_size <= MAX_INPUT_BYTES)
        .collect::<Vec<_>>();
    candidates.sort_by_key(|meta| (meta.file_size, meta.id));
    // Group by immutable geometry/epoch, so one incompatible small segment
    // does not prevent another compatible group from being merged.
    let mut visited = Vec::new();
    for first in &candidates {
        let group = (first.dims, first.epoch_id);
        if visited.contains(&group) {
            continue;
        }
        visited.push(group);
        let mut inputs = Vec::new();
        let mut bytes = 0_u64;
        for meta in candidates
            .iter()
            .filter(|meta| meta.dims == first.dims && meta.epoch_id == first.epoch_id)
        {
            if inputs.len() == MAX_INPUTS || meta.file_size > MAX_INPUT_BYTES - bytes {
                break;
            }
            let reader = SegmentReader::open(vfs, &directory.join(meta.id.file_name()), meta.id)
                .map_err(StoreError::Segment)?;
            if reader
                .directory()
                .iter()
                .any(|entry| entry.kind == RegionKind::GraphNodeBlocks.id())
            {
                continue;
            }
            bytes += meta.file_size;
            inputs.push(reader);
        }
        if inputs.len() >= 2 {
            inputs.sort_by_key(|input| input.meta().id);
            return Ok(inputs);
        }
    }
    Ok(Vec::new())
}

fn merge_id(generation: u64, inputs: &[SegmentReader]) -> SegmentId {
    let mut source = Vec::with_capacity(inputs.len() * 16);
    for input in inputs {
        source.extend_from_slice(input.meta().id.as_bytes());
    }
    let hash = xxhash_rust::xxh3::xxh3_64_with_seed(&source, MERGE_ID_SEED);
    let mut bytes = [0_u8; 16];
    for (target, value) in bytes.iter_mut().take(8).zip(generation.to_be_bytes()) {
        *target = value;
    }
    for (target, value) in bytes.iter_mut().skip(8).zip(hash.to_be_bytes()) {
        *target = value;
    }
    SegmentId::from_bytes(bytes)
}

fn check_cancel(cancel: &CancelToken) -> Result<(), StoreError> {
    if cancel.is_cancelled() {
        Err(StoreError::SealCancelled)
    } else {
        Ok(())
    }
}

fn merge_error(error: ConsolidateError) -> StoreError {
    match error {
        ConsolidateError::Store(error) => error,
        ConsolidateError::Segment(error) => StoreError::Segment(error),
        error => StoreError::Segment(SegmentError::Geometry(error.to_string())),
    }
}
