//! The document half of one coherent native admission.
use super::NativeReadLease;
use crate::ingest::{ActiveSegment, DocId, DocumentVersion};
use crate::lifecycle::{PublishedSnapshot, StoreError};
use crate::property_graph::NodeId;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct NativeDocuments {
    pub(crate) generation: u64,
    pub(crate) active: Arc<ActiveSegment>,
    pub(crate) snapshot: Arc<PublishedSnapshot>,
}

impl NativeReadLease {
    #[cfg(feature = "graph-cypher")]
    pub(crate) fn document_cardinality(
        &self,
        runtime: &mut crate::property_graph::query::runtime::RuntimeContext<'_, '_, '_>,
    ) -> Result<u64, crate::property_graph::storage::tree::directory::TreeError> {
        use crate::property_graph::query::runtime::{RuntimeError, WorkKind};
        use crate::property_graph::storage::tree::directory::TreeError;
        let Some(documents) = &self.documents else {
            return Ok(0);
        };
        // Both temporary roaring sets fit in bitset containers, including
        // container capacity and the temporary array-to-bitset transitions.
        let bytes = documents
            .active
            .row_count()
            .div_ceil(65_536)
            .checked_mul(3 * (8_192 + 128))
            .and_then(|bytes| bytes.checked_add(256))
            .ok_or(TreeError::Memory)?;
        let _charge = runtime
            .memory()
            .reserve(bytes)
            .map_err(RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        runtime
            .charge(WorkKind::Scans, documents.active.row_count() as u64)
            .map_err(TreeError::Runtime)?;
        let alive = documents
            .active
            .alive_controlled(&mut crate::fts::control::WorkCheck::new(|| {
                runtime
                    .checkpoint()
                    .map_err(|error| super::NativeGraphError::Read(TreeError::Runtime(error)))
            }))
            .map_err(|error| match error {
                super::NativeGraphError::Store(error) => tree_error(error),
                super::NativeGraphError::Read(error) => error,
                _ => TreeError::Invalid("unexpected document cardinality error"),
            })?;
        runtime.checkpoint().map_err(TreeError::Runtime)?;
        let mut count = alive.live_count();
        for segment in documents.snapshot.segments() {
            runtime
                .charge(WorkKind::Scans, 1)
                .map_err(TreeError::Runtime)?;
            count = count
                .checked_add(segment.query_alive().map_err(tree_error)?.live_count())
                .ok_or(TreeError::Runtime(RuntimeError::Value(
                    crate::property_graph::query::QueryError::ArithmeticOverflow,
                )))?;
        }
        Ok(count)
    }

    pub(crate) fn search_documents(&self) -> Result<&NativeDocuments, StoreError> {
        self.documents.as_ref().ok_or(StoreError::Closed)
    }

    pub(crate) fn document_version(
        &self,
        node: NodeId,
    ) -> Result<Option<DocumentVersion>, StoreError> {
        let Some(documents) = &self.documents else {
            return Ok(None);
        };
        let id = DocId::new(node.get());
        let mut version = documents
            .active
            .existing(id)
            .filter(|(row, _, _)| !documents.active.is_tombstoned(*row))
            .map(|(_, version, _)| version);
        for segment in documents.snapshot.segments() {
            for row in segment.query_rows_for_doc_id(id)? {
                if version.is_some() {
                    return Err(StoreError::DuplicateLiveDocument { doc_id: id });
                }
                version = segment.document_version(row).map_err(StoreError::Segment)?;
            }
        }
        Ok(version)
    }
}

/// Physical document position in one immutable admission, independent of the
/// numeric graph-directory resume key.
#[derive(Default)]
pub(crate) struct DocumentCursor {
    source: usize,
    row: usize,
}

impl NativeReadLease {
    pub(crate) fn document(
        &self,
        node: NodeId,
        fields: crate::lifecycle::DocumentFields,
    ) -> Result<Option<crate::lifecycle::StoredDocument>, StoreError> {
        let Some(documents) = &self.documents else {
            return Ok(None);
        };
        let id = DocId::new(node.get());
        let mut result = documents
            .active
            .existing(id)
            .filter(|(row, _, _)| !documents.active.is_tombstoned(*row))
            .map(|(row, _, _)| {
                crate::lifecycle::materialize_active_document(&documents.active, row, fields)
            })
            .transpose()?;
        for segment in documents.snapshot.segments() {
            for row in segment.query_rows_for_doc_id(id)? {
                if result.is_some() {
                    return Err(StoreError::DuplicateLiveDocument { doc_id: id });
                }
                result = Some(crate::lifecycle::materialize_sealed_document(
                    segment, row, fields,
                )?);
            }
        }
        Ok(result)
    }

    pub(crate) fn next_document(
        &self,
        cursor: &mut DocumentCursor,
        resources: &mut crate::property_graph::storage::tree::directory::TreeResources<'_>,
    ) -> Result<Option<DocumentVersion>, crate::property_graph::storage::tree::directory::TreeError>
    {
        use crate::property_graph::storage::tree::directory::NativeReadEvent;
        #[cfg(any(test, feature = "test-seams"))]
        crate::property_graph::query::note_document_visit();
        let Some(documents) = &self.documents else {
            return Ok(None);
        };
        loop {
            if cursor.source == 0 {
                while cursor.row < documents.active.row_count() {
                    resources.step(1)?;
                    resources.read_event(NativeReadEvent::Scan)?;
                    let row = cursor.row;
                    cursor.row += 1;
                    if !documents.active.is_tombstoned(row) {
                        return documents
                            .active
                            .document(row)
                            .map(Some)
                            .ok_or(StoreError::ActiveRowOverflow)
                            .map_err(tree_error);
                    }
                }
            } else if let Some(segment) = documents.snapshot.segments().get(cursor.source - 1) {
                let alive = segment.query_alive().map_err(tree_error)?;
                while cursor.row < segment.meta().row_count as usize {
                    resources.step(1)?;
                    resources.read_event(NativeReadEvent::Scan)?;
                    let row = cursor.row;
                    cursor.row += 1;
                    if alive.is_alive(row as u32) {
                        return segment
                            .document_version(row)
                            .map_err(StoreError::Segment)
                            .map_err(tree_error)?
                            .map(Some)
                            .ok_or(StoreError::ActiveRowOverflow)
                            .map_err(tree_error);
                    }
                }
            } else {
                return Ok(None);
            }
            cursor.source = cursor
                .source
                .checked_add(1)
                .ok_or(StoreError::ActiveRowOverflow)
                .map_err(tree_error)?;
            cursor.row = 0;
        }
    }

    pub(crate) fn visit_document_properties(
        &self,
        node: NodeId,
        mut visit: impl FnMut(
            &str,
            &crate::meta::PredicateValue,
        )
            -> Result<(), crate::property_graph::storage::tree::directory::TreeError>,
    ) -> Result<(), crate::property_graph::storage::tree::directory::TreeError> {
        let Some(documents) = &self.documents else {
            return Ok(());
        };
        let Some(document) = self
            .document(node, crate::lifecycle::DocumentFields::ATTRIBUTES)
            .map_err(tree_error)?
        else {
            return Ok(());
        };
        for definition in documents.snapshot.schema().columns() {
            if definition.id() == crate::meta::TIMESTAMP_COLUMN {
                visit(
                    definition.name(),
                    &crate::meta::PredicateValue::I64(document.timestamp),
                )?;
            } else if let Some((_, value)) = document
                .attributes
                .as_ref()
                .and_then(|values| values.iter().find(|(id, _)| *id == definition.id()))
            {
                visit(definition.name(), value)?;
            }
        }
        Ok(())
    }

    pub(crate) fn document_property(
        &self,
        node: NodeId,
        name: &str,
    ) -> Result<Option<crate::meta::PredicateValue>, StoreError> {
        let Some(documents) = &self.documents else {
            return Ok(None);
        };
        let Some(definition) = documents
            .snapshot
            .schema()
            .columns()
            .iter()
            .find(|column| column.name() == name)
        else {
            return Ok(None);
        };
        let Some(document) = self.document(node, crate::lifecycle::DocumentFields::ATTRIBUTES)?
        else {
            return Ok(None);
        };
        if definition.id() == crate::meta::TIMESTAMP_COLUMN {
            return Ok(Some(crate::meta::PredicateValue::I64(document.timestamp)));
        }
        Ok(document
            .attributes
            .unwrap_or_default()
            .into_iter()
            .find(|(id, _)| *id == definition.id())
            .map(|(_, value)| value))
    }
}

fn tree_error(error: StoreError) -> crate::property_graph::storage::tree::directory::TreeError {
    crate::property_graph::storage::tree::directory::TreeError::Control(
        crate::lifecycle::QueryError::Store(error),
    )
}

struct DeleteDocuments<'a>(&'a crate::ingest::DeleteBatch);
impl super::mutate::NativeMutationConsumer<()> for DeleteDocuments<'_> {
    fn document_delete(&self) -> Option<&crate::ingest::DeleteBatch> {
        Some(self.0)
    }
    fn consume<'lease, 'm, 'g, 'w, 'i>(
        &mut self,
        view: &'w crate::property_graph::storage::GraphReadView<'w, 'lease, 'm, 'g>,
        runtime: &mut crate::property_graph::query::runtime::RuntimeContext<'lease, 'm, 'g>,
        mut overlay: crate::property_graph::staging::GraphBatchReadView<'w, 'static>,
        _: &'w crate::property_graph::staging::StatementImages<'i>,
        control: &mut crate::property_graph::staging::WriteControl<'_>,
    ) -> Result<
        (
            (),
            crate::property_graph::staging::GraphBatchReadView<'w, 'static>,
        ),
        crate::property_graph::query::runtime::NativeExecutionError,
    > {
        let mut resources =
            crate::property_graph::storage::tree::directory::TreeResources::for_query(runtime)?;
        for id in self.0.doc_ids() {
            let node = NodeId::from(*id);
            if view.lookup_node(node, &mut resources)?.is_some() {
                overlay.delete(
                    crate::property_graph::staging::BatchEntityRef::Node(
                        crate::property_graph::NodeRef::Existing(node),
                    ),
                    crate::property_graph::GraphDeleteMode::Detach,
                    control,
                )?;
            }
        }
        Ok(((), overlay))
    }
}

impl crate::lifecycle::Store {
    pub(crate) fn native_document_writer(
        &self,
    ) -> Result<Option<std::sync::MutexGuard<'_, Option<super::write::NativeWriter>>>, StoreError>
    {
        if !self.native_graph.is_installed()? {
            return Ok(None);
        }
        let writer = self
            .native_graph
            .writer
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "native graph writer",
            })?;
        if writer.is_none() {
            return Err(StoreError::ReadOnly);
        }
        Ok(Some(writer))
    }

    pub(crate) fn delete_native_documents_locked(
        &self,
        batch: &crate::ingest::DeleteBatch,
        writer: &mut super::write::NativeWriter,
    ) -> Result<crate::ingest::IngestAck, crate::ingest::IngestError> {
        let (_, report) = self
            .with_native_document_mutation(batch.doc_ids().len(), DeleteDocuments(batch), writer)
            .map_err(document_mutation_error)?;
        let generation = report.changed.unwrap_or(report.admitted).get();
        Ok(crate::ingest::IngestAck::mixed(report.seq, generation))
    }
}

impl crate::lifecycle::Store {
    pub(crate) fn publish_native_documents(
        &self,
        current: &crate::ingest::ActiveState,
    ) -> Result<(), StoreError> {
        if !self.native_graph.is_installed()? {
            return Ok(());
        }
        let documents = self.native_documents(current)?;
        let mut state =
            self.native_graph
                .state
                .lock()
                .map_err(|_| StoreError::Synchronization {
                    component: "native graph publication",
                })?;
        state.documents = Some(documents);
        Ok(())
    }
    pub(super) fn native_documents(
        &self,
        current: &crate::ingest::ActiveState,
    ) -> Result<NativeDocuments, StoreError> {
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?
            .as_ref()
            .cloned()
            .ok_or(StoreError::Closed)?;
        Ok(NativeDocuments {
            generation: current.generation,
            active: Arc::clone(&current.segment),
            snapshot,
        })
    }
}

struct ExplicitDocumentNodes<'a>(&'a [DocId]);
impl super::NativeReadConsumer<bool> for ExplicitDocumentNodes<'_> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut crate::property_graph::query::runtime::RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<bool, crate::property_graph::storage::tree::directory::TreeError> {
        let mut resources =
            crate::property_graph::storage::tree::directory::TreeResources::for_query(runtime)?;
        for id in self.0 {
            if view
                .lookup_node(NodeId::from(*id), &mut resources)?
                .is_some()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
impl crate::lifecycle::Store {
    pub(crate) fn documents_have_native_nodes(
        &self,
        ids: &[DocId],
    ) -> Result<bool, crate::ingest::IngestError> {
        self.with_native_read(
            &crate::lifecycle::QueryControl::Cancel(crate::lifecycle::CancelToken::new()),
            crate::property_graph::query::runtime::RuntimeLimits::default(),
            8 * 1024 * 1024,
            crate::property_graph::storage::MAX_NATIVE_ARTIFACTS,
            ExplicitDocumentNodes(ids),
        )
        .map_err(|error| {
            crate::ingest::IngestError::Graph(Box::new(
                crate::property_graph::query::completed::GraphQueryError::from(error),
            ))
        })
    }
}

fn document_mutation_error(
    error: super::mutate::NativeMutationError,
) -> crate::ingest::IngestError {
    match error {
        super::mutate::NativeMutationError::Graph(super::NativeGraphError::Ingest(error)) => error,
        error => crate::ingest::IngestError::Graph(Box::new(
            crate::property_graph::query::completed::GraphQueryError::from(error),
        )),
    }
}
