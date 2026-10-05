//! Private mutation preparation; caller owns lifecycle, WAL and active locks.
use super::*;
use crate::lifecycle::NamespaceMutation;
use crate::manifest::Manifest;

pub(crate) struct NamespaceStage {
    pub(crate) active: ActiveState,
    pub(crate) manifest: Manifest,
    pub(crate) snapshot: Arc<PublishedSnapshot>,
    pub(crate) records: Vec<(u16, Vec<u8>)>,
    pub(crate) rows: Vec<Vec<usize>>,
    pub(crate) replacements: Vec<crate::segment::SegmentId>,
}

impl Store {
    pub(crate) fn stage_namespace(
        &self,
        mutation: &NamespaceMutation,
        current: &ActiveState,
        snapshot: &PublishedSnapshot,
        durable_end: u64,
    ) -> Result<NamespaceStage, IngestError> {
        for document in &mutation.upserts {
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
        check_revision_conditions(
            &current.segment,
            snapshot,
            mutation
                .upserts
                .iter()
                .enumerate()
                .map(|(index, doc)| (index, doc.version().doc_id(), doc.expected_revision())),
        )?;
        let requested = mutation
            .upserts
            .iter()
            .map(|doc| doc.version().doc_id())
            .collect::<Vec<_>>();
        let sealed = purge::sealed_document_matches(snapshot, &requested)?;
        let mut progress = RevisionProgress {
            working: None,
            records: Vec::new(),
            replay_seq: None,
            #[cfg(any(test, feature = "test-support"))]
            replay_count: 0,
            sealed_tombstones: Vec::new(),
        };
        for (index, document) in mutation.upserts.iter().enumerate() {
            self.apply_revision_decision(
                &current.segment,
                &sealed,
                LogSeq::new(snapshot.absorbed_through()),
                document,
                mutation
                    .upserts
                    .get(index..)
                    .ok_or(StoreError::ActiveRowOverflow)?,
                &mut progress,
            )?;
        }
        let mut changed = progress.sealed_tombstones;
        let mut segment = match progress.working {
            Some(segment) => segment,
            None => current.segment.tombstone(&[], &self.accounting)?.0,
        };
        let mut records = Vec::new();
        let mut rows = Vec::new();
        for (row, op, payload) in progress.records {
            records.push((op, payload));
            rows.push(vec![row]);
        }
        let mut generation = current.generation;
        if !records.is_empty() {
            generation = generation
                .checked_add(1)
                .ok_or(StoreError::GenerationOverflow)?;
        }
        let mut deletes = mutation.deletes.clone();
        // Filter the provisional active rows separately: old sealed revisions
        // of changed IDs must not match the predicate after an upsert/delete.
        if let Some(predicate) = &mutation.delete_where {
            crate::planner::validate_predicate(predicate, &self.schema).map_err(|error| {
                crate::lifecycle::namespace_batch::invalid(&self.directory, &error.to_string())
            })?;
            let provisional = segment.tombstone(&deletes, &self.accounting)?.0;
            let mut sealed_ids = self
                .live_document_ids_matching(&ActiveSegment::empty(), snapshot.segments(), predicate)
                .map_err(|error| {
                    crate::lifecycle::namespace_batch::invalid(&self.directory, &error.to_string())
                })?;
            sealed_ids.retain(|id| !changed.contains(id) && !deletes.contains(id));
            sealed_ids.extend(
                self.live_document_ids_matching(&provisional, &[], predicate)
                    .map_err(|error| {
                        crate::lifecycle::namespace_batch::invalid(
                            &self.directory,
                            &error.to_string(),
                        )
                    })?,
            );
            deletes.extend(sealed_ids);
        }
        deletes.sort_unstable();
        deletes.dedup();
        if !deletes.is_empty() {
            let (next, deleted_rows) = segment.tombstone(&deletes, &self.accounting)?;
            segment = next;
            records.push((
                wal_payload::DELETE_V1,
                wal_payload::encode_delete(&deletes).map_err(IngestError::Payload)?,
            ));
            rows.push(deleted_rows);
            generation = generation
                .checked_add(1)
                .ok_or(StoreError::GenerationOverflow)?;
        }
        changed.extend(&deletes);
        let matches = purge::sealed_document_matches(snapshot, &changed)?;
        let prepared = purge::prepare_sealed_tombstones(
            self.vfs.as_ref(),
            &self.directory,
            snapshot,
            &matches,
            &changed,
            durable_end,
            generation,
            durable_end.saturating_add(1),
            crate::lifecycle::durability::DurabilityPolicy::new(
                crate::lifecycle::durability::DurabilityMode::Durable,
                crate::lifecycle::durability::CommitTier::Durable,
            )
            .map_err(StoreError::Durability)?,
            &self.accounting,
        )?;
        let (mut manifest, replacements) = match prepared {
            Some(prepared) => prepared.into_namespace_manifest(),
            None => (
                super::seal::load_current_manifest(
                    self.vfs.as_ref(),
                    &self.directory,
                    durable_end,
                    snapshot.generation(),
                    &self.schema,
                )?,
                Vec::new(),
            ),
        };
        manifest.generation = generation;
        manifest.epochs = self.epoch_registry(&manifest.epochs);
        let snapshot = Arc::new(PublishedSnapshot::from_manifest(
            self.vfs.as_ref(),
            &self.directory,
            &manifest,
            &self.accounting,
        )?);
        Ok(NamespaceStage {
            active: ActiveState {
                generation,
                segment: Arc::new(segment),
            },
            manifest,
            snapshot,
            records,
            rows,
            replacements,
        })
    }
}
