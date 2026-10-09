//! Statement-local folder bitmaps from the already admitted document columns.
use super::*;
use crate::meta::{
    Column, ColumnStore, ColumnStoreBuilder, ColumnType, DocBitmap, Predicate, PredicateValue,
};
use crate::property_graph::query::resources::{QueryArena, QueryReservation};
use crate::property_graph::query::runtime::{RuntimeContext, RuntimeError, WorkKind};
use crate::property_graph::storage::tree::directory::TreeError;

pub(crate) struct FolderCandidates<'m, 'g> {
    rows: QueryArena<'m, 'g, DocBitmap>,
    _charge: QueryArena<'m, 'g, QueryReservation<'m, 'g>>,
    source: usize,
    after: Option<u32>,
}

impl<'m, 'g> FolderCandidates<'m, 'g> {
    pub(crate) fn prepare(
        lease: &NativeReadLease,
        value: u64,
        context: &mut RuntimeContext<'_, 'm, 'g>,
    ) -> Result<Option<Self>, TreeError> {
        let Some(documents) = &lease.documents else {
            return Ok(None);
        };
        let schema = documents.snapshot.schema();
        let Some(definition) = schema
            .columns()
            .iter()
            .find(|column| column.name() == "folder")
        else {
            return Ok(None);
        };
        if definition.column_type() != ColumnType::U64 || definition.is_nullable() {
            return Ok(None);
        }
        let sources = documents
            .snapshot
            .segments()
            .len()
            .checked_add(1)
            .ok_or(TreeError::Memory)?;
        let mut result = Self {
            rows: QueryArena::new(context.memory(), sources)
                .map_err(RuntimeError::Memory)
                .map_err(TreeError::Runtime)?,
            _charge: QueryArena::new(context.memory(), sources)
                .map_err(RuntimeError::Memory)
                .map_err(TreeError::Runtime)?,
            source: 0,
            after: None,
        };
        let predicate = Predicate::Eq {
            column: definition.id(),
            value: PredicateValue::U64(value),
        };
        for source in 0..sources {
            context.checkpoint().map_err(TreeError::Runtime)?;
            let count = if source == 0 {
                documents.active.row_count()
            } else {
                documents
                    .snapshot
                    .segments()
                    .get(source - 1)
                    .ok_or(TreeError::Memory)?
                    .meta()
                    .row_count as usize
            };
            // Bound owned bitmap storage and the temporary single active column.
            let bytes = count
                .checked_mul(if source == 0 { 64 } else { 16 })
                .and_then(|bytes| bytes.checked_add(1024))
                .ok_or(TreeError::Memory)?;
            let temporary = context
                .memory()
                .reserve(bytes)
                .map_err(RuntimeError::Memory)
                .map_err(TreeError::Runtime)?;
            let (columns, alive) = if source == 0 {
                let mut definitions = Vec::new();
                definitions
                    .try_reserve_exact(1)
                    .map_err(|_| TreeError::Memory)?;
                definitions.push(definition.clone());
                let folder_schema = crate::meta::Schema::new(definitions)
                    .map_err(|_| TreeError::Invalid("folder schema"))?;
                let mut builder = ColumnStoreBuilder::new(folder_schema);
                for (row, timestamp) in documents.active.timestamps().iter().copied().enumerate() {
                    context.checkpoint().map_err(TreeError::Runtime)?;
                    let values = documents.active.column_values(row).map_err(tree_error)?;
                    let inputs = crate::ingest::column_inputs(&values);
                    let selected = inputs.iter().find(|input| input.column == definition.id());
                    let selected = selected.map_or(&[][..], std::slice::from_ref);
                    builder
                        .push_row(timestamp, selected)
                        .map_err(|_| TreeError::Invalid("folder active column"))?;
                }
                (
                    Arc::new(
                        builder
                            .finish()
                            .map_err(|_| TreeError::Invalid("folder active column"))?,
                    ),
                    Arc::new(documents.active.alive().map_err(tree_error)?),
                )
            } else {
                let segment = documents
                    .snapshot
                    .segments()
                    .get(source - 1)
                    .ok_or(TreeError::Memory)?;
                (
                    segment.query_columns().map_err(tree_error)?,
                    segment.query_alive().map_err(tree_error)?,
                )
            };
            if !fits_integer(&columns, definition.id(), &alive, context)? {
                // An unsupported value leaves the original evaluator responsible
                // for its error at the original row (including LIMIT prefixes).
                return Ok(None);
            }
            context
                .charge(WorkKind::Scans, count as u64)
                .map_err(TreeError::Runtime)?;
            let rows = crate::meta::evaluate(&predicate, &columns, &alive)
                .map_err(|_| TreeError::Invalid("folder metadata predicate"))?;
            drop(temporary);
            let bytes = rows.resident_bytes().ok_or(TreeError::Memory)?;
            result
                ._charge
                .push(
                    context
                        .memory()
                        .reserve(bytes)
                        .map_err(RuntimeError::Memory)
                        .map_err(TreeError::Runtime)?,
                )
                .map_err(RuntimeError::Memory)
                .map_err(TreeError::Runtime)?;
            result
                .rows
                .push(rows)
                .map_err(RuntimeError::Memory)
                .map_err(TreeError::Runtime)?;
        }
        Ok(Some(result))
    }

    pub(crate) fn cardinality(&self) -> Result<u64, TreeError> {
        self.rows.as_slice().iter().try_fold(0_u64, |total, rows| {
            total.checked_add(rows.cardinality()).ok_or(TreeError::Work)
        })
    }

    pub(crate) fn next(
        &mut self,
        lease: &NativeReadLease,
        resources: &mut crate::property_graph::storage::tree::directory::TreeResources<'_>,
    ) -> Result<Option<DocumentVersion>, TreeError> {
        let documents = lease.search_documents().map_err(tree_error)?;
        while let Some(rows) = self.rows.as_slice().get(self.source) {
            use std::ops::Bound::{Excluded, Unbounded};
            let row = match self.after {
                None => rows.as_roaring().iter().next(),
                Some(after) => rows.as_roaring().range((Excluded(after), Unbounded)).next(),
            };
            if let Some(row) = row {
                self.after = Some(row);
                resources.step(1)?;
                resources.read_event(
                    crate::property_graph::storage::tree::directory::NativeReadEvent::Scan,
                )?;
                return if self.source == 0 {
                    documents
                        .active
                        .document(row as usize)
                        .map(Some)
                        .ok_or(TreeError::Invalid("folder active identity"))
                } else {
                    documents
                        .snapshot
                        .segments()
                        .get(self.source - 1)
                        .ok_or(TreeError::Memory)?
                        .document_version(row as usize)
                        .map_err(StoreError::Segment)
                        .map_err(tree_error)?
                        .map(Some)
                        .ok_or(TreeError::Invalid("folder sealed identity"))
                };
            }
            self.source = self.source.checked_add(1).ok_or(TreeError::Work)?;
            self.after = None;
        }
        Ok(None)
    }
}

fn fits_integer(
    columns: &ColumnStore,
    id: crate::meta::ColumnId,
    alive: &crate::meta::AliveSet,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<bool, TreeError> {
    let Some(Column::U64(column)) = columns.column(id) else {
        return Ok(false);
    };
    if !alive.alive_bitmap().is_subset(column.present()) {
        return Ok(false);
    }
    // Checking the complete physical column is conservative for deleted rows
    // and avoids a control/bitmap lookup for every scalar.
    for values in column.values().chunks(256) {
        context.checkpoint().map_err(TreeError::Runtime)?;
        context
            .charge(WorkKind::Scans, values.len() as u64)
            .map_err(TreeError::Runtime)?;
        if values.iter().any(|value| i64::try_from(*value).is_err()) {
            return Ok(false);
        }
    }
    Ok(true)
}
