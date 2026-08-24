//! Typed, segment-aligned metadata columns and bitmap predicate evaluation.
//!
//! Metadata predicates are a closed data model. They identify schema columns
//! by [`crate::meta::ColumnId`]; this module contains no parser and accepts no
//! filter text.

mod alive;
mod bitmap;
mod columns;
mod dict;
mod predicate;

pub use alive::{AliveError, AliveSet};
pub use bitmap::DocBitmap;
pub use columns::{
    BoolColumn, BuildError, Column, ColumnInput, ColumnStore, ColumnStoreBuilder, ColumnValue,
    DictionaryColumn, NumericColumn, RawStringColumn,
};
pub use dict::{CodeWidth, DictionaryCodes, DictionaryError, StringDictionary};
pub use predicate::{EvalError, Predicate, PredicateValue, RangeBound, RangePredicate, evaluate};

/// The reserved identifier of the required per-row timestamp column.
pub const TIMESTAMP_COLUMN: ColumnId = ColumnId(0);

/// A stable, schema-local identifier used by structured predicates.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct ColumnId(u32);

impl ColumnId {
    /// Creates an identifier from its schema-local integer representation.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the schema-local integer representation.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// The physical type of a segment-aligned metadata column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColumnType {
    /// Unsigned 64-bit integers.
    U64,
    /// Signed 64-bit integers.
    I64,
    /// IEEE-754 64-bit floating-point values.
    F64,
    /// Boolean values stored as contiguous bytes.
    Bool,
    /// Low-cardinality UTF-8 strings encoded through a segment dictionary.
    DictionaryString,
    /// Raw UTF-8 strings retained as stored-field values.
    RawString,
}

/// One typed user-column declaration in a collection schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnDefinition {
    id: ColumnId,
    name: String,
    column_type: ColumnType,
    nullable: bool,
}

impl ColumnDefinition {
    /// Creates a typed column declaration.
    #[must_use]
    pub fn new(
        id: ColumnId,
        name: impl Into<String>,
        column_type: ColumnType,
        nullable: bool,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            column_type,
            nullable,
        }
    }

    /// Returns the column identifier.
    #[must_use]
    pub const fn id(&self) -> ColumnId {
        self.id
    }

    /// Returns the binding-facing column name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the physical column type.
    #[must_use]
    pub const fn column_type(&self) -> ColumnType {
        self.column_type
    }

    /// Returns whether a row may omit this column.
    #[must_use]
    pub const fn is_nullable(&self) -> bool {
        self.nullable
    }
}

/// A schema-construction failure detected before any rows are accepted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchemaError {
    /// Column zero is reserved for the required timestamp array.
    ReservedTimestampId,
    /// The name `ts` is reserved for the required timestamp array.
    ReservedTimestampName,
    /// Two declarations used the same identifier.
    DuplicateColumnId(ColumnId),
    /// Two declarations used the same binding-facing name.
    DuplicateColumnName(String),
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReservedTimestampId => formatter.write_str("column id 0 is reserved for ts"),
            Self::ReservedTimestampName => formatter.write_str("column name ts is reserved"),
            Self::DuplicateColumnId(id) => write!(formatter, "duplicate column id {}", id.get()),
            Self::DuplicateColumnName(name) => write!(formatter, "duplicate column name {name}"),
        }
    }
}

impl std::error::Error for SchemaError {}

/// An immutable collection schema including the required `ts: i64` column.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Schema {
    columns: Vec<ColumnDefinition>,
}

impl Schema {
    /// Creates the mandatory timestamp-only schema used by public ingest today.
    #[must_use]
    pub fn timestamp_only() -> Self {
        Self {
            columns: vec![ColumnDefinition::new(
                TIMESTAMP_COLUMN,
                "ts",
                ColumnType::I64,
                false,
            )],
        }
    }

    /// Validates user columns and prepends the reserved timestamp definition.
    pub fn new(user_columns: Vec<ColumnDefinition>) -> Result<Self, SchemaError> {
        for (position, definition) in user_columns.iter().enumerate() {
            if definition.id == TIMESTAMP_COLUMN {
                return Err(SchemaError::ReservedTimestampId);
            }
            if definition.name == "ts" {
                return Err(SchemaError::ReservedTimestampName);
            }
            if let Some(duplicate) = user_columns.iter().take(position).find(|candidate| {
                candidate.id == definition.id || candidate.name == definition.name
            }) {
                if duplicate.id == definition.id {
                    return Err(SchemaError::DuplicateColumnId(definition.id));
                }
                return Err(SchemaError::DuplicateColumnName(definition.name.clone()));
            }
        }

        let mut columns = Self::timestamp_only().columns;
        columns.reserve(user_columns.len());
        columns.extend(user_columns);
        Ok(Self { columns })
    }

    /// Returns all declarations, beginning with the required timestamp column.
    #[must_use]
    pub fn columns(&self) -> &[ColumnDefinition] {
        &self.columns
    }

    /// Finds a declaration by its typed identifier.
    #[must_use]
    pub fn column(&self, id: ColumnId) -> Option<&ColumnDefinition> {
        self.columns.iter().find(|definition| definition.id == id)
    }

    /// Returns the number of user-declared columns, excluding `ts`.
    #[must_use]
    pub fn user_column_count(&self) -> usize {
        self.columns.len().saturating_sub(1)
    }
}
