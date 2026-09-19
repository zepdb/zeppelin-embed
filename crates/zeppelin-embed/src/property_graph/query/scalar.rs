//! Bounded scalar/list expressions that require no storage access.
use super::value::compare_bytes;
use super::{QueryError, QueryValue, Truth, ValueContext};
use std::cmp::Ordering;
/// Case-sensitive, byte-exact UTF-8 predicates. No normalization or coercion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StringPredicate {
    /// Prefix match.
    StartsWith,
    /// Suffix match.
    EndsWith,
    /// Substring match under explicit work/deadline bounds.
    Contains,
}
impl QueryValue<'_> {
    /// Immediate list length or Unicode scalar count, with null propagation.
    pub fn size(self, context: &mut ValueContext<'_>) -> Result<Self, QueryError> {
        context.step()?;
        self.validate(context)?;
        let length = match self {
            Self::Null => return Ok(Self::Null),
            Self::List(list) => list.len(),
            Self::String(text) => {
                let mut count = 0_usize;
                for _ in text.chars() {
                    context.step()?;
                    count += 1;
                }
                count
            }
            _ => return Err(QueryError::Type),
        };
        Ok(Self::I64(
            i64::try_from(length).map_err(|_| QueryError::WorkLimit)?,
        ))
    }
    /// Zero-based or negative-from-end indexing. Out-of-range is null.
    pub fn index(self, index: Self, context: &mut ValueContext<'_>) -> Result<Self, QueryError> {
        context.step()?;
        self.validate(context)?;
        index.validate(context)?;
        if matches!(self, Self::Null) || matches!(index, Self::Null) {
            return Ok(Self::Null);
        }
        let (Self::List(list), Self::I64(index)) = (self, index) else {
            return Err(QueryError::Type);
        };
        let length = i128::try_from(list.len()).map_err(|_| QueryError::ListLimit)?;
        let position = if index < 0 {
            length + i128::from(index)
        } else {
            i128::from(index)
        };
        if position < 0 || position >= length {
            return Ok(Self::Null);
        }
        list.get(usize::try_from(position).map_err(|_| QueryError::ListLimit)?)
            .ok_or(QueryError::ListLimit)
    }
    /// Bounded exact match. A negative result never means work was exhausted;
    /// work/control failure returns an error instead of an approximate answer.
    pub fn string_predicate(
        self,
        pattern: Self,
        operation: StringPredicate,
        context: &mut ValueContext<'_>,
    ) -> Result<Truth, QueryError> {
        context.step()?;
        self.validate(context)?;
        pattern.validate(context)?;
        if matches!(self, Self::Null) || matches!(pattern, Self::Null) {
            return Ok(Truth::Unknown);
        }
        let (Self::String(text), Self::String(pattern)) = (self, pattern) else {
            return Err(QueryError::Type);
        };
        let text = text.as_bytes();
        let pattern = pattern.as_bytes();
        let matched = match operation {
            StringPredicate::StartsWith => match text.get(..pattern.len()) {
                Some(prefix) => compare_bytes(prefix, pattern, context)? == Ordering::Equal,
                None => false,
            },
            StringPredicate::EndsWith => match text
                .len()
                .checked_sub(pattern.len())
                .and_then(|start| text.get(start..))
            {
                Some(suffix) => compare_bytes(suffix, pattern, context)? == Ordering::Equal,
                None => false,
            },
            StringPredicate::Contains if pattern.is_empty() => true,
            StringPredicate::Contains => {
                let mut found = false;
                for window in text.windows(pattern.len()) {
                    context.step()?;
                    if compare_bytes(window, pattern, context)? == Ordering::Equal {
                        found = true;
                        break;
                    }
                }
                found
            }
        };
        Ok(if matched { Truth::True } else { Truth::False })
    }
}
