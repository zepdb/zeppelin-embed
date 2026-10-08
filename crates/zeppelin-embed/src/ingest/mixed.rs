//! Document preparation for the native writer's single mixed WAL run.
use super::*;

pub(crate) struct PreparedMixedDocuments {
    pub(crate) generation: u64,
    next: Option<ActiveSegment>,
    published_next: Option<Arc<ActiveSegment>>,
    pub(crate) records: Vec<(usize, u16, Vec<u8>)>,
    snapshot: Arc<PublishedSnapshot>,
    sealed: Vec<purge::SealedDocumentMatch>,
    tombstones: Vec<DocId>,
}

impl Store {
    // Called under the shared WAL lock. No durable changes occur here.
    pub(crate) fn prepare_mixed_documents(
        &self,
        batch: &IngestBatch,
        current: &ActiveState,
    ) -> Result<PreparedMixedDocuments, IngestError> {
        match (self.epoch_identity(), batch.epoch) {
            (Some(expected), Some(declared)) if expected != declared => {
                return Err(IngestError::EpochMismatch(crate::epoch::EpochMismatch {
                    expected,
                    declared,
                }));
            }
            (Some(_), None) => return Err(IngestError::EpochUndeclared),
            (None, Some(_)) => return Err(IngestError::EpochUnstamped),
            (Some(_), Some(_)) | (None, None) => {}
        }
        for document in &batch.documents {
            validate_document_columns(&self.schema, document)?;
            if self.epoch.as_ref().is_some_and(|epoch| {
                epoch.embedding.document.normalization == crate::epoch::Normalization::L2
            }) && let Some(squared_norm) =
                crate::graph::search::non_unit_squared_norm(document.vector())
            {
                return Err(IngestError::Vector(crate::quant::QuantError::NonUnitNorm {
                    squared_norm_bits: squared_norm.to_bits(),
                    tolerance_bits: crate::graph::search::UNIT_NORM_SQUARED_TOLERANCE.to_bits(),
                }));
            }
        }
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?
            .as_ref()
            .cloned()
            .ok_or(StoreError::Closed)?;
        check_revision_conditions(
            &current.segment,
            &snapshot,
            batch.documents.iter().enumerate().map(|(index, document)| {
                (
                    index,
                    document.version().doc_id(),
                    document.expected_revision(),
                )
            }),
        )?;
        let ids = batch
            .documents
            .iter()
            .map(|document| document.version().doc_id())
            .collect::<Vec<_>>();
        let sealed = purge::sealed_document_matches(&snapshot, &ids)?;
        let mut progress = RevisionProgress {
            working: None,
            records: Vec::new(),
            replay_seq: None,
            #[cfg(any(test, feature = "test-seams"))]
            replay_count: 0,
            sealed_tombstones: Vec::new(),
        };
        for (index, document) in batch.documents.iter().enumerate() {
            self.apply_revision_decision(
                &current.segment,
                &sealed,
                LogSeq::new(snapshot.absorbed_through()),
                document,
                batch
                    .documents
                    .get(index..)
                    .ok_or(StoreError::ActiveRowOverflow)?,
                &mut progress,
            )?;
        }
        Ok(PreparedMixedDocuments {
            generation: current.generation,
            next: progress.working,
            published_next: None,
            records: progress.records,
            snapshot,
            sealed,
            tombstones: progress.sealed_tombstones,
        })
    }
}

impl PreparedMixedDocuments {
    pub(crate) fn prepare_active_publication(
        &mut self,
        first_seq: LogSeq,
    ) -> Result<(), StoreError> {
        if let Some(mut next) = self.next.take() {
            for (index, (row, _, _)) in self.records.iter().enumerate() {
                next.set_sequence(
                    *row,
                    LogSeq::new(first_seq.get().saturating_add(index as u64)),
                )?;
            }
            self.published_next = Some(Arc::new(next));
        }
        Ok(())
    }

    pub(crate) fn prepare_tombstones(
        &self,
        store: &Store,
        durable_end: u64,
        generation: u64,
        last_seq: u64,
    ) -> Result<Option<purge::PreparedSealedTombstones>, StoreError> {
        purge::prepare_sealed_tombstones(
            store.vfs.as_ref(),
            &store.directory,
            &self.snapshot,
            &self.sealed,
            &self.tombstones,
            durable_end,
            generation,
            durable_end.saturating_add(1),
            Some(last_seq),
            store.durability_policy,
            &store.accounting,
        )
    }

    pub(crate) fn publish(
        mut self,
        store: &Store,
        active: &mut Option<ActiveState>,
        prepared: Option<purge::PreparedSealedTombstones>,
        publication: &mut ManifestPublication,
        generation: u64,
    ) -> Result<Vec<PathBuf>, StoreError> {
        let Some(next) = self.published_next.take() else {
            return Ok(Vec::new());
        };
        let committed = prepared
            .map(|prepared| {
                prepared.commit(
                    publication,
                    store,
                    store.vfs.as_ref(),
                    &store.directory,
                    store.durability_policy,
                )
            })
            .transpose()?;
        let paths = if let Some((snapshot, paths)) = committed {
            let mut published =
                store
                    .snapshot
                    .write()
                    .map_err(|_| StoreError::Synchronization {
                        component: "published snapshot",
                    })?;
            *published = Some(Arc::new(snapshot));
            paths
        } else {
            Vec::new()
        };
        *active = Some(ActiveState {
            generation,
            segment: next,
        });
        Ok(paths)
    }
}

pub(crate) fn unlink_replaced(store: &Store, paths: &[PathBuf]) {
    purge::unlink_replaced_segments(
        store,
        store.vfs.as_ref(),
        &store.directory,
        paths,
        store.durability_policy,
    );
}
