//! Explicit active-segment sealing and WAL-prefix absorption.

use std::path::Path;
use std::sync::Arc;

use crate::fts::sealed::{SealedSegment, SealedSegmentError};
use crate::lifecycle::durability::{DurabilityPolicy, SyncRequirement};
use crate::lifecycle::{CancelToken, Store, StoreError, StoreState};
use crate::manifest::Manifest;
use crate::manifest::io::{MANIFEST_FILE, commit_manifest, load_manifest};
use crate::meta::{ColumnStore, ColumnStoreBuilder, Schema};
use crate::segment::writer::{
    SegmentBuild, SegmentDocumentVersions, SegmentFactors, SegmentPayloads, SegmentPostings,
    SegmentStoredMetadata, SegmentStoredText, write_segment_with_documents_payloads,
};
use crate::segment::{ClusteringKeyRange, SegmentId};
use crate::vfs::Vfs;
use crate::wal::LogSeq;

use super::ActiveState;

impl Store {
    /// Seals the current active segment into one appended immutable segment.
    pub fn seal(&self) -> Result<u64, StoreError> {
        self.seal_inner(None, self.vfs.as_ref())
    }

    /// Seals explicitly unless caller cancellation wins before manifest commit.
    pub fn seal_with_cancel(&self, cancel: &CancelToken) -> Result<u64, StoreError> {
        self.seal_inner(Some(cancel), self.vfs.as_ref())
    }

    /// Test-support seam for deterministic cancellation at filesystem boundaries.
    #[doc(hidden)]
    pub fn seal_with_cancel_on_vfs(
        &self,
        cancel: &CancelToken,
        vfs: &dyn Vfs,
    ) -> Result<u64, StoreError> {
        self.seal_inner(Some(cancel), vfs)
    }

    fn seal_inner(&self, cancel: Option<&CancelToken>, vfs: &dyn Vfs) -> Result<u64, StoreError> {
        let state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(StoreError::Closing),
            StoreState::Closed => return Err(StoreError::Closed),
        }
        let writer_lock = self
            .writer_lock
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "writer lock",
            })?;
        if writer_lock.is_none() {
            return Err(StoreError::ReadOnly);
        }
        check_cancelled(cancel)?;
        let mut wal = self
            .wal_writer
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "WAL writer",
            })?;
        let writer = wal.as_mut().ok_or(StoreError::ReadOnly)?;
        let absorbed_through = writer.durable_end();
        let mut active = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        let current = active.as_ref().ok_or(StoreError::Closed)?;
        if current.segment.is_empty() {
            return Err(StoreError::EmptyActiveSegment);
        }
        let generation = current
            .generation
            .checked_add(1)
            .ok_or(StoreError::GenerationOverflow)?;
        let manifest = load_current_manifest(
            vfs,
            &self.directory,
            absorbed_through,
            current.generation,
            &self.schema,
        )?;
        let columns = active_columns(&manifest.schema, &current.segment, cancel)?;
        let alive = current.segment.alive()?;
        let clustering_key_range = clustering_key_range(current.segment.timestamps(), &alive)?;
        let dims = current
            .segment
            .dims()
            .ok_or(StoreError::ActiveRowOverflow)?;
        let dims = u32::try_from(dims).map_err(|_| StoreError::ActiveRowOverflow)?;
        let id = seal_id(generation, absorbed_through);
        #[cfg(any(test, feature = "test-support"))]
        let int8_storage = match self.vector_seal_scheme {
            Some(crate::quant::QuantScheme::Int8) => {
                let dimension = usize::try_from(dims).map_err(|_| StoreError::ActiveRowOverflow)?;
                let vectors = current.segment.vectors();
                if dimension == 0 || !vectors.len().is_multiple_of(dimension) {
                    return Err(StoreError::Segment(crate::segment::SegmentError::Geometry(
                        format!(
                            "Int8 seal vector length {} is not divisible by dimension {dimension}",
                            vectors.len()
                        ),
                    )));
                }
                let mut signed_codes = vec![0_i8; vectors.len()];
                let mut factors = Vec::with_capacity(vectors.len() / dimension);
                for (row, codes) in vectors
                    .chunks_exact(dimension)
                    .zip(signed_codes.chunks_exact_mut(dimension))
                {
                    let (scale, offset) =
                        crate::quant::quantize_int8(row, codes).map_err(|error| {
                            StoreError::Segment(crate::segment::SegmentError::Geometry(format!(
                                "Int8 seal quantization failed: {error}"
                            )))
                        })?;
                    factors.push(crate::segment::layout::Int8Factors { scale, offset });
                }
                let codes = signed_codes
                    .into_iter()
                    .map(|code| code as u8)
                    .collect::<Vec<_>>();
                Some((codes, factors))
            }
            Some(crate::quant::QuantScheme::Bit4) | None => None,
            Some(scheme) => {
                return Err(StoreError::Segment(crate::segment::SegmentError::Geometry(
                    format!("unsupported test vector seal scheme {scheme:?}"),
                )));
            }
        };
        #[cfg(any(test, feature = "test-support"))]
        let (scheme, codes, factors) = match int8_storage.as_ref() {
            Some((codes, factors)) => (
                u16::from(crate::quant::QuantScheme::Int8.id()),
                codes.as_slice(),
                SegmentFactors::Int8(factors.as_slice()),
            ),
            None => (
                u16::from(crate::quant::QuantScheme::Bit4.id()),
                current.segment.codes(),
                SegmentFactors::Bit4(current.segment.factors()),
            ),
        };
        #[cfg(not(any(test, feature = "test-support")))]
        let (scheme, codes, factors) = (
            u16::from(crate::quant::QuantScheme::Bit4.id()),
            current.segment.codes(),
            SegmentFactors::Bit4(current.segment.factors()),
        );
        let build = SegmentBuild {
            id,
            scheme,
            dims,
            codes,
            factors,
            rescore: current.segment.vectors(),
            columns: &columns,
            alive: &alive,
        };
        let documents = SegmentDocumentVersions {
            doc_ids: current.segment.doc_ids(),
            revisions: current.segment.revisions(),
        };
        let postings = if current.segment.has_text() {
            let sealed = SealedSegment::seal(current.segment.lexical())
                .map_err(SealedSegmentError::from)
                .map_err(crate::segment::SegmentError::from)
                .map_err(StoreError::Segment)?;
            Some(
                sealed
                    .encode_region()
                    .map_err(crate::segment::SegmentError::from)
                    .map_err(StoreError::Segment)?,
            )
        } else {
            None
        };
        let metadata =
            (!current.segment.metadata_bytes().is_empty()).then_some(SegmentStoredMetadata {
                end_offsets: current.segment.metadata_end_offsets(),
                bytes: current.segment.metadata_bytes(),
            });
        let text = current.segment.has_text().then_some(SegmentStoredText {
            present: current.segment.text_present(),
            end_offsets: current.segment.text_end_offsets(),
            bytes: current.segment.text_bytes(),
        });
        let written = write_segment_with_documents_payloads(
            vfs,
            &self.directory,
            build,
            SegmentPayloads {
                documents,
                metadata,
                text,
                postings: postings.as_deref().map(|bytes| SegmentPostings { bytes }),
            },
            self.durability_policy,
        );
        let mut meta = match written {
            Ok(meta) => meta,
            Err(error) => {
                cleanup_uncommitted_segment(vfs, &self.directory, id, self.durability_policy)?;
                return Err(StoreError::Segment(error));
            }
        };
        meta.clustering_key_range = clustering_key_range;
        meta.epoch_id = manifest.epoch_alias.map(|identity| identity.embedding);
        #[cfg(any(test, feature = "test-support"))]
        let forced_late_cancellation =
            match self.ingest_retention_fault_controller.as_ref() {
                Some(controller) => controller.seal_cancellation_plan().map_err(|_| {
                    StoreError::Synchronization {
                        component: "ingest-retention controller",
                    }
                })?,
                None => None,
            };
        #[cfg(not(any(test, feature = "test-support")))]
        let forced_late_cancellation: Option<u64> = None;
        if forced_late_cancellation.is_some() || check_cancelled(cancel).is_err() {
            cleanup_uncommitted_segment(vfs, &self.directory, id, self.durability_policy)?;
            #[cfg(any(test, feature = "test-support"))]
            if let Some(invocation_id) = forced_late_cancellation {
                let active_rows = u64::try_from(current.segment.row_count())
                    .map_err(|_| StoreError::ActiveRowOverflow)?;
                if let Some(controller) = self.ingest_retention_fault_controller.as_ref() {
                    controller
                        .push_receipt(super::IngestRetentionFaultReceiptV1::seal_cancellation(
                            invocation_id,
                            active_rows,
                            absorbed_through,
                            *id.as_bytes(),
                        ))
                        .map_err(|_| StoreError::Synchronization {
                            component: "ingest-retention controller",
                        })?;
                }
            }
            return Err(StoreError::SealCancelled);
        }
        let mut segments = manifest.segments;
        segments.push(meta);
        let epochs = self.epoch_registry(&manifest.epochs);
        commit_manifest(
            vfs,
            &self.directory,
            &Manifest {
                generation,
                log_seq: absorbed_through,
                segments,
                epochs,
                epoch_alias: manifest.epoch_alias,
                schema: manifest.schema,
            },
            self.durability_policy,
        )
        .map_err(StoreError::Manifest)?;
        let remapped = crate::lifecycle::PublishedSnapshot::load_on_vfs(
            &self.directory,
            &self.accounting,
            vfs,
        )?;
        let mut published = self
            .snapshot
            .write()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?;
        let previous = published.replace(Arc::new(remapped));
        drop(published);
        drop(previous);
        *active = Some(ActiveState::empty(generation));
        writer.retire_visible_through(LogSeq::new(absorbed_through))?;
        drop(active);
        drop(wal);
        drop(writer_lock);
        drop(state);
        Ok(generation)
    }
}

fn load_current_manifest(
    vfs: &dyn Vfs,
    directory: &Path,
    durable_end: u64,
    generation: u64,
    schema: &Schema,
) -> Result<Manifest, StoreError> {
    let path = directory.join(MANIFEST_FILE);
    match vfs.open(&path) {
        Ok(_) => load_manifest(vfs, &path, durable_end).map_err(StoreError::Manifest),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Manifest {
            generation,
            log_seq: 0,
            segments: Vec::new(),
            epochs: Vec::new(),
            epoch_alias: None,
            schema: schema.clone(),
        }),
        Err(source) => Err(StoreError::Io { path, source }),
    }
}

fn active_columns(
    schema: &Schema,
    active: &super::ActiveSegment,
    cancel: Option<&CancelToken>,
) -> Result<ColumnStore, StoreError> {
    let mut builder = ColumnStoreBuilder::new(schema.clone());
    for (row, timestamp) in active.timestamps().iter().copied().enumerate() {
        if row.is_multiple_of(64) {
            check_cancelled(cancel)?;
        }
        let values = active.column_values(row)?;
        builder
            .push_row(timestamp, &super::column_inputs(&values))
            .map_err(|error| {
                StoreError::Segment(crate::segment::SegmentError::Columns(error.to_string()))
            })?;
    }
    builder.finish().map_err(|error| {
        StoreError::Segment(crate::segment::SegmentError::Columns(error.to_string()))
    })
}

fn clustering_key_range(
    timestamps: &[i64],
    alive: &crate::meta::AliveSet,
) -> Result<ClusteringKeyRange, StoreError> {
    let mut bounds: Option<(i64, i64)> = None;
    for row in alive.iter_alive() {
        let index = usize::try_from(row).map_err(|_| StoreError::ActiveRowOverflow)?;
        let timestamp = timestamps
            .get(index)
            .copied()
            .ok_or(StoreError::ActiveRowOverflow)?;
        bounds = Some(match bounds {
            None => (timestamp, timestamp),
            Some((minimum, maximum)) => (minimum.min(timestamp), maximum.max(timestamp)),
        });
    }
    Ok(match bounds {
        Some((min_ts, max_ts)) => ClusteringKeyRange::Bounded { min_ts, max_ts },
        // An all-tombstoned segment has the empty key set. It is represented
        // explicitly rather than overloading a numeric sentinel or Unstamped.
        None => ClusteringKeyRange::Empty,
    })
}

fn check_cancelled(cancel: Option<&CancelToken>) -> Result<(), StoreError> {
    if cancel.is_some_and(CancelToken::is_cancelled) {
        Err(StoreError::SealCancelled)
    } else {
        Ok(())
    }
}

fn seal_id(generation: u64, absorbed_through: u64) -> SegmentId {
    let mut bytes = [0_u8; 16];
    if let Some(prefix) = bytes.get_mut(..8) {
        prefix.copy_from_slice(&generation.to_be_bytes());
    }
    if let Some(suffix) = bytes.get_mut(8..) {
        suffix.copy_from_slice(&absorbed_through.to_be_bytes());
    }
    SegmentId::from_bytes(bytes)
}

fn cleanup_uncommitted_segment(
    vfs: &dyn Vfs,
    directory: &Path,
    id: SegmentId,
    policy: DurabilityPolicy,
) -> Result<(), StoreError> {
    let final_path = directory.join(id.file_name());
    let temporary_path = directory.join(format!(".{}.tmp", id.file_name()));
    let mut deleted = false;
    for path in [temporary_path, final_path] {
        match vfs.open(&path) {
            Ok(_) => {
                vfs.delete(&path)
                    .map_err(|source| StoreError::Io { path, source })?;
                deleted = true;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(StoreError::Io { path, source }),
        }
    }
    if deleted && let SyncRequirement::Sync(kind) = policy.directory_sync() {
        vfs.sync(directory, kind).map_err(|source| StoreError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}
