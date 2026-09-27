//! Live-document counts grouped by one typed attribute (ZE-229).

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use crate::meta::{Column, ColumnId, ColumnStore, ColumnType, DocBitmap};

use super::{AdmittedDocumentRead, QueryError, Store, StoreError};

/// Largest group `limit` accepted by [`Store::count_documents_grouped`].
pub const MAX_DOCUMENT_GROUP_LIMIT: usize = 1 << 16;

/// The value shared by every document of one group.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DocumentGroupValue {
    /// A `U64` attribute value.
    U64(u64),
    /// An `I64` attribute value.
    I64(i64),
    /// A `DictionaryString` or `RawString` attribute value.
    String(String),
}

/// One group and its exact live-document count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentGroup {
    /// Attribute value shared by the group's documents.
    pub value: DocumentGroupValue,
    /// Number of matching live documents with this value; never zero.
    pub count: u64,
}

/// Grouped live-document counts from one pinned store generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentGroupCounts {
    /// Groups in ascending value order: numeric for integers, byte order for
    /// strings.
    pub groups: Vec<DocumentGroup>,
    /// Matching live documents whose group attribute is null.
    pub missing: u64,
    /// All matching live documents; the group counts plus `missing`.
    pub count: u64,
    /// Store generation pinned for every group.
    pub generation: u64,
}

enum Tally {
    U64(BTreeMap<u64, u64>),
    I64(BTreeMap<i64, u64>),
    String(BTreeMap<String, u64>),
}

impl Store {
    /// Counts live documents matching an optional predicate and timestamp
    /// range, grouped by the value of `column`.
    ///
    /// `column` must be a `U64`, `I64`, `DictionaryString` or `RawString`
    /// schema attribute. Documents where it is null are counted in
    /// [`DocumentGroupCounts::missing`], not as a group. More than `limit`
    /// distinct values fail the whole call with
    /// [`StoreError::GroupLimitExceeded`]; no group is ever dropped.
    pub fn count_documents_grouped(
        &self,
        predicate: Option<&crate::meta::Predicate>,
        timestamp_range: Option<(i64, i64)>,
        column: ColumnId,
        limit: usize,
    ) -> Result<DocumentGroupCounts, QueryError> {
        let invalid = |detail: String| QueryError::Store(StoreError::InvalidScan { detail });
        if limit == 0 || limit > MAX_DOCUMENT_GROUP_LIMIT {
            return Err(invalid(format!(
                "group limit must be in 1..={MAX_DOCUMENT_GROUP_LIMIT}, received {limit}"
            )));
        }
        let definition = self.schema.column(column).ok_or_else(|| {
            invalid(format!(
                "group-by attribute {} is not in the schema",
                column.get()
            ))
        })?;
        let nullable = definition.is_nullable();
        let mut tally = match definition.column_type() {
            ColumnType::U64 => Tally::U64(BTreeMap::new()),
            ColumnType::I64 => Tally::I64(BTreeMap::new()),
            ColumnType::DictionaryString | ColumnType::RawString => Tally::String(BTreeMap::new()),
            other @ (ColumnType::Id128 | ColumnType::F64 | ColumnType::Bool) => {
                return Err(invalid(format!(
                    "group-by attribute {} has type {other:?}; grouping supports \
                     U64, I64, DictionaryString and RawString",
                    column.get()
                )));
            }
        };
        let predicate = super::document_scan_predicate(predicate, timestamp_range)?;
        if let Some(predicate) = &predicate {
            crate::planner::validate_predicate(predicate, &self.schema)
                .map_err(|error| invalid(error.to_string()))?;
        }
        let AdmittedDocumentRead {
            generation,
            active,
            snapshot,
            active_query,
        } = self.admit_document_read().map_err(QueryError::Store)?;
        let mut missing = 0_u64;
        for segment in snapshot.segments() {
            let rows = super::sealed_scan_rows(segment, predicate.as_ref())?;
            if rows.is_empty() {
                continue;
            }
            let columns = segment.query_columns().map_err(QueryError::Store)?;
            missing = add(
                missing,
                tally.add_rows(&columns, &rows, column, nullable, limit)?,
            )?;
        }
        let columns = super::active_scan_columns(&active, &self.schema)?;
        let alive = active.alive().map_err(QueryError::Store)?;
        let rows = match &predicate {
            Some(predicate) => {
                crate::meta::evaluate(predicate, &columns, &alive).map_err(|error| {
                    QueryError::Store(StoreError::Segment(crate::segment::SegmentError::Columns(
                        error.to_string(),
                    )))
                })?
            }
            None => alive.alive_bitmap().clone(),
        };
        missing = add(
            missing,
            tally.add_rows(&columns, &rows, column, nullable, limit)?,
        )?;
        drop(active_query);
        let groups = tally.into_groups()?;
        let count = groups
            .iter()
            .try_fold(missing, |total, group| add(total, group.count))?;
        Ok(DocumentGroupCounts {
            groups,
            missing,
            count,
            generation,
        })
    }
}

impl Tally {
    /// Adds every row of `rows` and returns how many had a null value.
    fn add_rows(
        &mut self,
        columns: &ColumnStore,
        rows: &DocBitmap,
        column: ColumnId,
        nullable: bool,
        limit: usize,
    ) -> Result<u64, QueryError> {
        let values = columns.column(column).ok_or_else(|| {
            corrupt(format!(
                "group-by attribute {} is missing from a segment",
                column.get()
            ))
        })?;
        let mut missing = 0_u64;
        for row in rows.iter() {
            let present = match (&mut *self, values) {
                (Self::U64(map), Column::U64(values)) => values
                    .get(row)
                    .map(|value| bump(map, value, limit))
                    .transpose()?,
                (Self::I64(map), Column::I64(values)) => values
                    .get(row)
                    .map(|value| bump(map, value, limit))
                    .transpose()?,
                (Self::String(map), Column::DictionaryString(values)) => values
                    .get(row)
                    .map(|value| bump_string(map, value, limit))
                    .transpose()?,
                (Self::String(map), Column::RawString(values)) => values
                    .get(row)
                    .map(|value| bump_string(map, value, limit))
                    .transpose()?,
                (_, values) => {
                    return Err(corrupt(format!(
                        "group-by attribute {} is stored as {:?}",
                        column.get(),
                        values.column_type()
                    )));
                }
            };
            if present.is_none() {
                if !nullable {
                    return Err(corrupt(format!(
                        "required group-by attribute {} is null at row {row}",
                        column.get()
                    )));
                }
                missing = add(missing, 1)?;
            }
        }
        Ok(missing)
    }

    fn into_groups(self) -> Result<Vec<DocumentGroup>, QueryError> {
        fn collect<K>(
            map: BTreeMap<K, u64>,
            value: impl Fn(K) -> DocumentGroupValue,
        ) -> Result<Vec<DocumentGroup>, QueryError> {
            let mut groups = Vec::new();
            groups.try_reserve_exact(map.len()).map_err(|_| {
                QueryError::Store(StoreError::AllocationFailed {
                    needed: super::allocation_bytes::<DocumentGroup>(map.len()),
                    component: "grouped count groups",
                })
            })?;
            groups.extend(map.into_iter().map(|(key, count)| DocumentGroup {
                value: value(key),
                count,
            }));
            Ok(groups)
        }
        match self {
            Self::U64(map) => collect(map, DocumentGroupValue::U64),
            Self::I64(map) => collect(map, DocumentGroupValue::I64),
            Self::String(map) => collect(map, DocumentGroupValue::String),
        }
    }
}

fn bump<K: Ord>(map: &mut BTreeMap<K, u64>, key: K, limit: usize) -> Result<(), QueryError> {
    let len = map.len();
    match map.entry(key) {
        Entry::Occupied(mut entry) => {
            *entry.get_mut() = add(*entry.get(), 1)?;
        }
        Entry::Vacant(_) if len >= limit => {
            return Err(QueryError::Store(StoreError::GroupLimitExceeded { limit }));
        }
        Entry::Vacant(entry) => {
            entry.insert(1);
        }
    }
    Ok(())
}

fn bump_string(map: &mut BTreeMap<String, u64>, key: &str, limit: usize) -> Result<(), QueryError> {
    if let Some(count) = map.get_mut(key) {
        *count = add(*count, 1)?;
        return Ok(());
    }
    if map.len() >= limit {
        return Err(QueryError::Store(StoreError::GroupLimitExceeded { limit }));
    }
    let key = super::copy_string(key, "grouped count value").map_err(QueryError::Store)?;
    map.insert(key, 1);
    Ok(())
}

fn add(total: u64, count: u64) -> Result<u64, QueryError> {
    total
        .checked_add(count)
        .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))
}

fn corrupt(detail: String) -> QueryError {
    QueryError::Store(StoreError::Segment(crate::segment::SegmentError::Geometry(
        detail,
    )))
}
