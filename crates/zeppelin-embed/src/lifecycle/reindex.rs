//! Explicit same-tokenizer reindex using immutable segment replacement.
use super::{PublishedSnapshot, Store, StoreError, StoreState};
use crate::manifest::io::{MANIFEST_FILE, commit_manifest, load_manifest};
use crate::segment::SegmentId;
use std::sync::Arc;

impl Store {
    /// Rebuilds every sealed text index from stored text under the current tokenizer.
    /// Active text is already indexed; seal it first. Publication replaces every
    /// sealed segment in one manifest commit, preserving all non-postings regions.
    pub fn reindex_text(&self) -> Result<u64, StoreError> {
        self.seal()?;
        let _maintenance = self
            .maintenance
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "maintenance",
            })?;
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
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?
            .clone()
            .ok_or(StoreError::Closed)?;
        if snapshot.all_segments().is_empty() {
            return Ok(current.generation);
        }
        let mut manifest = load_manifest(
            self.vfs.as_ref(),
            &self.directory.join(MANIFEST_FILE),
            durable_end,
        )
        .map_err(StoreError::Manifest)?;
        for epoch in &manifest.epochs {
            super::validate_tokenizer_epoch(
                Some(crate::epoch::EpochIdentity {
                    embedding: epoch.id,
                    tokenizer: epoch.tokenizer,
                }),
                &self.tokenizer,
            )?;
        }
        let generation = manifest
            .generation
            .max(current.generation)
            .checked_add(1)
            .ok_or(StoreError::GenerationOverflow)?;
        let mut replacements = Vec::new();
        for segment in snapshot.all_segments() {
            let mut bytes = *segment.meta().id.as_bytes();
            let hash = xxhash_rust::xxh3::xxh3_64_with_seed(&bytes, generation ^ 0x5245494e444558);
            bytes
                .get_mut(..8)
                .ok_or(StoreError::GenerationOverflow)?
                .copy_from_slice(&generation.to_be_bytes());
            bytes
                .get_mut(8..)
                .ok_or(StoreError::GenerationOverflow)?
                .copy_from_slice(&hash.to_be_bytes());
            replacements.push(
                crate::segment::writer::reindex_segment(
                    self.vfs.as_ref(),
                    &self.directory,
                    segment,
                    SegmentId::from_bytes(bytes),
                    &self.tokenizer,
                    self.durability_policy,
                )
                .map_err(StoreError::Segment)?,
            );
        }
        manifest.segments = replacements;
        manifest.generation = generation;
        commit_manifest(
            self.vfs.as_ref(),
            &self.directory,
            &manifest,
            self.durability_policy,
        )
        .map_err(StoreError::Manifest)?;
        let remapped =
            PublishedSnapshot::load_on_vfs(&self.directory, &self.accounting, self.vfs.as_ref())?;
        *self
            .snapshot
            .write()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })? = Some(Arc::new(remapped));
        current.generation = generation;
        // Keep old files for pinned readers; ordinary open reachability cleanup reclaims them.
        Ok(generation)
    }
}
