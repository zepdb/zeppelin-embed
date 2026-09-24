//! Typed, segment-aligned metadata columns and bitmap predicate evaluation.
//!
//! Metadata predicates are a closed data model. They identify schema columns
//! by [`crate::meta::ColumnId`]; this module contains no parser and accepts no
//! filter text.

mod alive;
mod bitmap;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod bitmap_observer;
mod columns;
mod dict;
mod predicate;

pub use alive::{AliveError, AliveSet};
pub use bitmap::DocBitmap;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub use columns::MetadataBuildTestLimits;
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

/// Why a declared schema is not an additive extension of a persisted one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchemaConflict {
    /// A persisted attribute is absent from the declaration.
    Removed(ColumnDefinition),
    /// A persisted attribute is declared with a different name, type, or
    /// nullability.
    Changed {
        /// Definition already persisted.
        persisted: ColumnDefinition,
        /// Definition declared with the same identifier.
        declared: ColumnDefinition,
    },
    /// A new attribute is declared non-nullable, so rows written before it
    /// existed could not read it as null.
    AddedNotNullable(ColumnDefinition),
}

struct DescribedColumn<'a>(&'a ColumnDefinition);

impl std::fmt::Display for DescribedColumn<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let definition = self.0;
        write!(
            formatter,
            "'{}' (id {}, {:?}, {})",
            definition.name,
            definition.id.get(),
            definition.column_type,
            if definition.nullable {
                "nullable"
            } else {
                "not nullable"
            }
        )
    }
}

impl std::fmt::Display for SchemaConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Removed(persisted) => write!(
                formatter,
                "attribute {} is persisted but not declared; attributes cannot be removed",
                DescribedColumn(persisted)
            ),
            Self::Changed {
                persisted,
                declared,
            } => write!(
                formatter,
                "attribute {} is declared as {}; persisted attributes cannot change",
                DescribedColumn(persisted),
                DescribedColumn(declared)
            ),
            Self::AddedNotNullable(declared) => write!(
                formatter,
                "added attribute {} must be nullable so existing documents read it as null",
                DescribedColumn(declared)
            ),
        }
    }
}

impl std::error::Error for SchemaConflict {}

/// An immutable collection schema including the required `ts: i64` column.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Schema {
    columns: Vec<ColumnDefinition>,
}

impl Schema {
    pub(crate) fn resident_bytes(&self) -> Option<usize> {
        let definitions = self
            .columns
            .capacity()
            .checked_mul(std::mem::size_of::<ColumnDefinition>())?;
        self.columns
            .iter()
            .try_fold(definitions, |total, definition| {
                total.checked_add(definition.name.capacity())
            })
    }

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

    /// Reconciles a declared schema with this persisted one by column id.
    ///
    /// Every persisted column must be declared unchanged, and every declared
    /// column this schema lacks must be nullable; declaration order does not
    /// matter. Returns `None` when both name the same columns, otherwise the
    /// evolved schema: this schema's columns in their order, then the added
    /// columns in declaration order.
    pub fn additive_evolution(&self, declared: &Self) -> Result<Option<Self>, SchemaConflict> {
        for persisted in &self.columns {
            match declared.column(persisted.id) {
                None => return Err(SchemaConflict::Removed(persisted.clone())),
                Some(candidate) if candidate != persisted => {
                    return Err(SchemaConflict::Changed {
                        persisted: persisted.clone(),
                        declared: candidate.clone(),
                    });
                }
                Some(_) => {}
            }
        }
        let mut added = Vec::new();
        for candidate in &declared.columns {
            if self.column(candidate.id).is_none() {
                if !candidate.nullable {
                    return Err(SchemaConflict::AddedNotNullable(candidate.clone()));
                }
                added.push(candidate.clone());
            }
        }
        if added.is_empty() {
            return Ok(None);
        }
        let mut columns = self.columns.clone();
        columns.extend(added);
        Ok(Some(Self { columns }))
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
