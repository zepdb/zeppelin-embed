//! Bounded maps reuse charged list storage for alternating keys and values.
use super::{QueryError, QueryList, QueryValue, ValueContext};
#[derive(Clone, Copy, Debug)]
/// Immutable map whose keys are unique and ordered by UTF-8 bytes.
pub struct QueryMap<'a> {
    pub(super) entries: QueryList<'a>,
}
impl<'a> QueryMap<'a> {
    /// Validate caller-backed alternating string keys and values.
    pub fn new(entries: QueryList<'a>, context: &mut ValueContext<'_>) -> Result<Self, QueryError> {
        QueryValue::List(entries).validate(context)?;
        if !entries.len().is_multiple_of(2) {
            return Err(QueryError::Type);
        }
        let mut previous: Option<&str> = None;
        for i in (0..entries.len()).step_by(2) {
            context.step()?;
            let Some(QueryValue::String(key)) = entries.get(i) else {
                return Err(QueryError::Type);
            };
            if let Some(old) = previous
                && super::value::compare_bytes(old.as_bytes(), key.as_bytes(), context)?
                    != std::cmp::Ordering::Less
            {
                return Err(QueryError::Type);
            }
            previous = Some(key);
        }
        Ok(Self { entries })
    }
    /// Number of keys.
    pub const fn len(self) -> usize {
        self.entries.len() / 2
    }
    /// Whether the map has no keys.
    pub const fn is_empty(self) -> bool {
        self.len() == 0
    }
    /// Canonical alternating keys and values, including nested values.
    pub const fn entries(self) -> QueryList<'a> {
        self.entries
    }
    /// Static key lookup with bounded work.
    pub fn get(
        self,
        key: &str,
        context: &mut ValueContext<'_>,
    ) -> Result<QueryValue<'a>, QueryError> {
        for i in (0..self.entries.len()).step_by(2) {
            context.step()?;
            let Some(QueryValue::String(name)) = self.entries.get(i) else {
                return Err(QueryError::Type);
            };
            if super::value::compare_bytes(name.as_bytes(), key.as_bytes(), context)?
                == std::cmp::Ordering::Equal
            {
                return self.entries.get(i + 1).ok_or(QueryError::Type);
            }
        }
        Ok(QueryValue::Null)
    }
}
