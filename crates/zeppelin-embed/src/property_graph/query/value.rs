use super::context::{MAX_QUERY_BYTES, QueryNodeRef, QueryRelRef, QueryView};
use super::{QueryError, QueryList, ValueContext};
use std::cmp::Ordering;

/// Profile arithmetic. Comparisons never use arithmetic's numeric coercion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Arithmetic {
    /// Checked addition.
    Add,
    /// Checked subtraction.
    Subtract,
    /// Checked multiplication.
    Multiply,
    /// Truncating integer division or IEEE floating division.
    Divide,
    /// Remainder with dividend sign.
    Remainder,
}

/// Nullable predicate comparisons, distinct from total ORDER BY semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Comparison {
    /// Query equality, with null unknown and NaN unequal.
    Equal,
    /// Negation of query equality.
    NotEqual,
    /// Strictly less within a comparable kind.
    Less,
    /// Less than or equal within a comparable kind.
    LessEqual,
    /// Strictly greater within a comparable kind.
    Greater,
    /// Greater than or equal within a comparable kind.
    GreaterEqual,
}

/// Nullable predicate truth. Filters retain only the explicit true case.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Truth {
    /// The predicate is certainly false.
    False,
    /// A null operand leaves the predicate unknown.
    Unknown,
    /// The predicate is certainly true.
    True,
}

impl Truth {
    /// Three-valued negation, preserving unknown.
    #[must_use]
    pub const fn not(self) -> Self {
        match self {
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
            Self::True => Self::False,
        }
    }

    /// Conjunction: a known false dominates an unknown operand.
    #[must_use]
    pub const fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::False, _) | (_, Self::False) => Self::False,
            (Self::True, Self::True) => Self::True,
            _ => Self::Unknown,
        }
    }

    /// Disjunction: a known true dominates an unknown operand.
    #[must_use]
    pub const fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::True, _) | (_, Self::True) => Self::True,
            (Self::False, Self::False) => Self::False,
            _ => Self::Unknown,
        }
    }

    /// Exclusive disjunction: an unknown operand remains unknown.
    #[must_use]
    pub const fn xor(self, other: Self) -> Self {
        self.and(other.not()).or(self.not().and(other))
    }

    /// Whether this predicate retains a row at a filter seam.
    #[must_use]
    pub const fn retained(self) -> bool {
        matches!(self, Self::True)
    }
}

/// Intermediate query scalar. Query null is distinct from stored absence.
#[derive(Clone, Copy, Debug)]
pub enum QueryValue<'a> {
    /// Unknown or missing expression value.
    Null,
    /// Boolean value.
    Bool(bool),
    /// Exact signed integer, never coerced merely to test truth.
    I64(i64),
    /// Every scalar IEEE-754 pattern, including infinities and NaN payloads.
    F64(f64),
    /// Exact UTF-8, borrowed from caller or admitted query storage.
    String(&'a str),
    /// Bounded nullable heterogeneous query list, distinct from stored lists.
    List(QueryList<'a>),
    /// Same-view full-width node identity.
    NodeRef(QueryNodeRef<'a>),
    /// Same-view full-width relationship identity.
    RelRef(QueryRelRef<'a>),
}

impl QueryValue<'_> {
    /// Accepts only booleans or null; numeric truthiness is not supported.
    pub const fn truth(self) -> Result<Truth, QueryError> {
        match self {
            Self::Null => Ok(Truth::Unknown),
            Self::Bool(true) => Ok(Truth::True),
            Self::Bool(false) => Ok(Truth::False),
            _ => Err(QueryError::Type),
        }
    }

    /// Numeric unary plus: null propagates, nonfinite operands reject.
    pub fn positive(self) -> Result<Self, QueryError> {
        match self {
            Self::Null | Self::I64(_) => Ok(self),
            Self::F64(value) if value.is_finite() => Ok(self),
            Self::F64(_) => Err(QueryError::ArithmeticDomain),
            _ => Err(QueryError::Type),
        }
    }

    /// Numeric unary minus with checked integer and finite float semantics.
    pub fn negate(self) -> Result<Self, QueryError> {
        match self.positive()? {
            Self::I64(value) => value
                .checked_neg()
                .map(Self::I64)
                .ok_or(QueryError::ArithmeticOverflow),
            Self::F64(value) => Ok(Self::F64(-value)),
            value => Ok(value),
        }
    }

    /// Constant-work numeric operation; no integer coercion enters predicates.
    pub fn arithmetic(self, other: Self, operation: Arithmetic) -> Result<Self, QueryError> {
        if matches!(self, Self::Null) || matches!(other, Self::Null) {
            return Ok(Self::Null);
        }
        let left = self.positive()?;
        let right = other.positive()?;
        if let (Self::I64(left), Self::I64(right)) = (left, right) {
            if matches!(operation, Arithmetic::Divide | Arithmetic::Remainder) && right == 0 {
                return Err(QueryError::DivisionByZero);
            }
            return match operation {
                Arithmetic::Add => left.checked_add(right),
                Arithmetic::Subtract => left.checked_sub(right),
                Arithmetic::Multiply => left.checked_mul(right),
                Arithmetic::Divide => left.checked_div(right),
                Arithmetic::Remainder => left.checked_rem(right),
            }
            .map(Self::I64)
            .ok_or(QueryError::ArithmeticOverflow);
        }
        let float = |value| match value {
            Self::I64(value) => Ok(value as f64),
            Self::F64(value) => Ok(value),
            _ => Err(QueryError::Type),
        };
        let (left, right) = (float(left)?, float(right)?);
        if matches!(operation, Arithmetic::Divide | Arithmetic::Remainder) && right == 0.0 {
            return Err(QueryError::DivisionByZero);
        }
        let result = match operation {
            Arithmetic::Add => left + right,
            Arithmetic::Subtract => left - right,
            Arithmetic::Multiply => left * right,
            Arithmetic::Divide => left / right,
            Arithmetic::Remainder => left % right,
        };
        if result.is_finite() {
            Ok(Self::F64(result))
        } else {
            Err(QueryError::ArithmeticOverflow)
        }
    }

    pub(super) fn view(&self) -> Option<&QueryView> {
        match self {
            Self::NodeRef(value) => Some(value.view),
            Self::RelRef(value) => Some(value.view),
            Self::List(list) => list.view(),
            _ => None,
        }
    }
    pub(super) fn validate(self, context: &ValueContext<'_>) -> Result<(), QueryError> {
        if self
            .view()
            .is_some_and(|view| !std::ptr::eq(view, context.view))
        {
            return Err(QueryError::ForeignView);
        }
        if matches!(self, Self::String(text) if text.len() > MAX_QUERY_BYTES) {
            return Err(QueryError::ValueTooLarge);
        }
        Ok(())
    }
    /// Three-valued membership, with a definite match dominating unknown.
    pub fn in_list(self, list: Self, context: &mut ValueContext<'_>) -> Result<Truth, QueryError> {
        context.step()?;
        self.validate(context)?;
        list.validate(context)?;
        let list = match list {
            Self::Null => return Ok(Truth::Unknown),
            Self::List(list) => list,
            _ => return Err(QueryError::Type),
        };
        let mut answer = Truth::False;
        for index in 0..list.len() {
            let other = list.get(index).ok_or(QueryError::ListLimit)?;
            answer = answer.or(self.predicate(other, Comparison::Equal, context)?);
            if answer == Truth::True {
                break;
            }
        }
        Ok(answer)
    }

    /// Applies predicate semantics without rounding integers to floating point.
    pub fn predicate(
        self,
        other: Self,
        operation: Comparison,
        context: &mut ValueContext<'_>,
    ) -> Result<Truth, QueryError> {
        context.step()?;
        self.validate(context)?;
        other.validate(context)?;
        if matches!(self, Self::Null) || matches!(other, Self::Null) {
            return Ok(Truth::Unknown);
        }
        let order = match (self, other) {
            (Self::NodeRef(left), Self::NodeRef(right)) => {
                ValueOrder::Ordered(left.id.cmp(&right.id))
            }
            (Self::RelRef(left), Self::RelRef(right)) => {
                ValueOrder::Ordered(left.id.cmp(&right.id))
            }
            (Self::List(left), Self::List(right)) => {
                return list_predicate(left, right, operation, context);
            }
            (Self::String(left), Self::String(right)) => {
                ValueOrder::Ordered(compare_bytes(left.as_bytes(), right.as_bytes(), context)?)
            }
            (Self::Bool(left), Self::Bool(right)) => ValueOrder::Ordered(left.cmp(&right)),
            (Self::I64(left), Self::I64(right)) => ValueOrder::Ordered(left.cmp(&right)),
            (Self::I64(left), Self::F64(right)) => ValueOrder::number(integer_float(left, right)),
            (Self::F64(left), Self::I64(right)) => {
                ValueOrder::number(integer_float(right, left).map(Ordering::reverse))
            }
            (Self::F64(left), Self::F64(right)) => ValueOrder::number(left.partial_cmp(&right)),
            _ => ValueOrder::Incomparable,
        };
        Ok(order.predicate(operation))
    }
}

enum ValueOrder {
    Ordered(Ordering),
    NotANumber,
    Incomparable,
}

impl ValueOrder {
    fn number(order: Option<Ordering>) -> Self {
        match order {
            Some(order) => Self::Ordered(order),
            None => Self::NotANumber,
        }
    }

    fn predicate(self, operation: Comparison) -> Truth {
        let equal = matches!(self, Self::Ordered(Ordering::Equal));
        let result = match operation {
            Comparison::Equal => equal,
            Comparison::NotEqual => !equal,
            _ if matches!(self, Self::Incomparable) => return Truth::Unknown,
            Comparison::Less => matches!(self, Self::Ordered(Ordering::Less)),
            Comparison::LessEqual => equal || matches!(self, Self::Ordered(Ordering::Less)),
            Comparison::Greater => matches!(self, Self::Ordered(Ordering::Greater)),
            Comparison::GreaterEqual => equal || matches!(self, Self::Ordered(Ordering::Greater)),
        };
        if result { Truth::True } else { Truth::False }
    }
}

// Compare the exact unsigned integer against a binary significand and exponent.
// No integer-to-float cast participates, including at either I64 endpoint.
pub(super) fn integer_float(integer: i64, float: f64) -> Option<Ordering> {
    if float.is_nan() {
        return None;
    }
    if float == 0.0 {
        return Some(integer.cmp(&0));
    }
    if float.is_infinite() {
        return Some(if float.is_sign_negative() {
            Ordering::Greater
        } else {
            Ordering::Less
        });
    }
    let negative = float.is_sign_negative();
    if negative != (integer < 0) {
        return Some(if negative {
            Ordering::Greater
        } else {
            Ordering::Less
        });
    }
    let magnitude = integer.unsigned_abs();
    let bits = float.to_bits();
    let exponent = ((bits >> 52) & 0x7ff) as i32 - 1023;
    let order = if exponent < 0 {
        if magnitude == 0 {
            Ordering::Less
        } else {
            Ordering::Greater
        }
    } else if exponent > 63 {
        Ordering::Less
    } else {
        let significand = (bits & 0x000f_ffff_ffff_ffff) | (1_u64 << 52);
        if exponent >= 52 {
            // Guard bounds the shift to 0..=11, so the 53-bit value fits u64.
            magnitude.cmp(&(significand << (exponent - 52)))
        } else {
            // Guard bounds the shift to 1..=52, leaving an exact remainder.
            let shift = 52 - exponent;
            let whole = significand >> shift;
            match magnitude.cmp(&whole) {
                Ordering::Equal if significand & ((1_u64 << shift) - 1) != 0 => Ordering::Less,
                order => order,
            }
        }
    };
    Some(if negative { order.reverse() } else { order })
}

fn list_predicate(
    left: QueryList<'_>,
    right: QueryList<'_>,
    operation: Comparison,
    context: &mut ValueContext<'_>,
) -> Result<Truth, QueryError> {
    match operation {
        Comparison::Equal | Comparison::NotEqual => {
            let mut answer = if left.len() == right.len() {
                Truth::True
            } else {
                Truth::False
            };
            if answer == Truth::True {
                for index in 0..left.len() {
                    let a = left.get(index).ok_or(QueryError::ListLimit)?;
                    let b = right.get(index).ok_or(QueryError::ListLimit)?;
                    answer = answer.and(a.predicate(b, Comparison::Equal, context)?);
                    if answer == Truth::False {
                        break;
                    }
                }
            }
            Ok(if operation == Comparison::Equal {
                answer
            } else {
                answer.not()
            })
        }
        Comparison::Greater => list_predicate(right, left, Comparison::Less, context),
        Comparison::LessEqual => Ok(list_predicate(left, right, Comparison::Less, context)?
            .or(list_predicate(left, right, Comparison::Equal, context)?)),
        Comparison::GreaterEqual => list_predicate(right, left, Comparison::LessEqual, context),
        Comparison::Less => {
            for index in 0..left.len().min(right.len()) {
                let a = left.get(index).ok_or(QueryError::ListLimit)?;
                let b = right.get(index).ok_or(QueryError::ListLimit)?;
                if a.predicate(b, Comparison::Equal, context)? != Truth::True {
                    return a.predicate(b, Comparison::Less, context);
                }
            }
            Ok(if left.len() < right.len() {
                Truth::True
            } else {
                Truth::False
            })
        }
    }
}
pub(super) fn compare_bytes(
    left: &[u8],
    right: &[u8],
    context: &mut ValueContext<'_>,
) -> Result<Ordering, QueryError> {
    for (a, b) in left.chunks(65_536).zip(right.chunks(65_536)) {
        context.step()?;
        let order = a.cmp(b);
        if order != Ordering::Equal {
            return Ok(order);
        }
    }
    Ok(left.len().cmp(&right.len()))
}
