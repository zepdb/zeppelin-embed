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
// The float upper bound is exclusive: i64::MAX rounds up to 2^63 in f64.
fn truncated_integer(value: f64) -> Option<i64> {
    let value = value.trunc();
    (value.is_finite()
        && (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&value))
    .then_some(value as i64)
}

// Parse decimal strings without rounding their integer part through binary64.
fn decimal_integer(text: &str) -> Result<Option<i64>, QueryError> {
    let (negative, text) = if let Some(text) = text.strip_prefix('-') {
        (true, text)
    } else {
        (false, text.strip_prefix('+').unwrap_or(text))
    };
    let (mantissa, exponent) = match text.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => {
            let Ok(exponent) = exponent.parse::<i32>() else {
                return Ok(None);
            };
            (mantissa, i64::from(exponent))
        }
        None => (text, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if (whole.is_empty() && fraction.is_empty())
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return Ok(None);
    }
    let point = i64::try_from(whole.len()).map_err(|_| QueryError::ValueTooLarge)? + exponent;
    if point <= 0 {
        return Ok(Some(0));
    }
    // Accumulate negatively so i64::MIN is representable too.
    let mut value = 0_i64;
    let mut consumed = 0_i64;
    for byte in whole
        .bytes()
        .chain(fraction.bytes())
        .take(usize::try_from(point).map_err(|_| QueryError::ArithmeticOverflow)?)
    {
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_sub(i64::from(byte - b'0')))
            .ok_or(QueryError::ArithmeticOverflow)?;
        consumed += 1;
    }
    if value != 0 {
        // At most 18 added zeroes can fit a nonzero signed 64-bit number.
        if point - consumed > 18 {
            return Err(QueryError::ArithmeticOverflow);
        }
        for _ in consumed..point {
            value = value
                .checked_mul(10)
                .ok_or(QueryError::ArithmeticOverflow)?;
        }
    }
    Ok(Some(if negative {
        value
    } else {
        value.checked_neg().ok_or(QueryError::ArithmeticOverflow)?
    }))
}

fn charge_numeric_text(text: &str, context: &mut ValueContext<'_>) -> Result<(), QueryError> {
    for _ in text.bytes() {
        context.step()?;
    }
    Ok(())
}

impl QueryValue<'_> {
    /// Integer conversion truncates toward zero. Invalid numeric strings yield null.
    pub fn to_integer(self, context: &mut ValueContext<'_>) -> Result<Self, QueryError> {
        context.step()?;
        self.validate(context)?;
        Ok(match self {
            Self::Null | Self::I64(_) => self,
            Self::F64(value) => {
                Self::I64(truncated_integer(value).ok_or(QueryError::ArithmeticOverflow)?)
            }
            Self::String(text) => {
                charge_numeric_text(text, context)?;
                decimal_integer(text)?.map_or(Self::Null, Self::I64)
            }
            _ => return Err(QueryError::Type),
        })
    }
    /// Float conversion accepts numbers and numeric strings; unparseable text yields null.
    pub fn to_float(self, context: &mut ValueContext<'_>) -> Result<Self, QueryError> {
        context.step()?;
        self.validate(context)?;
        Ok(match self {
            Self::Null | Self::F64(_) => self,
            Self::I64(value) => Self::F64(value as f64),
            Self::String(text) => {
                charge_numeric_text(text, context)?;
                text.trim().parse::<f64>().map_or(Self::Null, Self::F64)
            }
            _ => return Err(QueryError::Type),
        })
    }
    /// Numeric floor returns a float and propagates null.
    pub fn floor(self, context: &mut ValueContext<'_>) -> Result<Self, QueryError> {
        context.step()?;
        self.validate(context)?;
        Ok(match self.positive()? {
            Self::Null => Self::Null,
            Self::I64(value) => Self::F64(value as f64),
            Self::F64(value) => Self::F64(value.floor()),
            _ => return Err(QueryError::Type),
        })
    }
    /// Numeric round returns a float, with exact half ties toward positive infinity.
    pub fn round(self, context: &mut ValueContext<'_>) -> Result<Self, QueryError> {
        context.step()?;
        self.validate(context)?;
        Ok(match self.positive()? {
            Self::Null => Self::Null,
            Self::I64(value) => Self::F64(value as f64),
            Self::F64(value) => {
                let floor = value.floor();
                Self::F64(if value - floor >= 0.5 {
                    floor + 1.0
                } else {
                    floor
                })
            }
            _ => return Err(QueryError::Type),
        })
    }
    /// Checked absolute value preserves integer/float kind and propagates null.
    pub fn abs(self, context: &mut ValueContext<'_>) -> Result<Self, QueryError> {
        context.step()?;
        self.validate(context)?;
        Ok(match self.positive()? {
            Self::Null => Self::Null,
            Self::I64(value) => {
                Self::I64(value.checked_abs().ok_or(QueryError::ArithmeticOverflow)?)
            }
            Self::F64(value) => Self::F64(value.abs()),
            _ => return Err(QueryError::Type),
        })
    }

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
