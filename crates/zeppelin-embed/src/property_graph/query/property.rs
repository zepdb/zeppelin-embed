//! Fully validated assignment conversion into caller-reserved typed scratch.
use super::{QueryError, QueryList, QueryValue, ValueContext};
use crate::property_graph::{MAX_GRAPH_INPUT_BYTES, PropertyData, PropertyValue};

/// Caller-owned typed output. No conversion allocates an uncharged Vec.
pub enum PropertyScratch<'a> {
    /// Scalars, null removal and untyped empty lists require no array.
    None,
    /// Reserved string-reference descriptors; strings remain borrowed.
    Strings(&'a mut [&'a str]),
    /// Reserved boolean elements.
    Bools(&'a mut [bool]),
    /// Reserved integer elements.
    Integers(&'a mut [i64]),
    /// Reserved bit-preserving floating elements.
    Floats(&'a mut [f64]),
}
/// Complete property conversion result. None means removal, never stored null.
/// Staging owns/reserves copies and canonical framing; this owns no allocation.
#[derive(Clone, Copy, Debug)]
pub struct PropertyAssignment<'a> {
    data: Option<PropertyData<'a>>,
    bytes: usize,
}
impl<'a> PropertyAssignment<'a> {
    /// Validated homogeneous storage input or explicit property removal.
    pub const fn data(self) -> Option<PropertyData<'a>> {
        self.data
    }
    /// Raw payload bytes; staging must additionally charge canonical framing.
    pub const fn payload_bytes(self) -> usize {
        self.bytes
    }
}
impl<'a> QueryValue<'a> {
    /// Reads a validated property; missing values become query null. Typed empty
    /// lists become equivalent query empties without modifying their source tag.
    pub fn from_property(
        value: Option<PropertyValue<'a>>,
        context: &mut ValueContext<'_>,
    ) -> Result<Self, QueryError> {
        context.step()?;
        let Some(value) = value else {
            return Ok(Self::Null);
        };
        Ok(match value.data() {
            PropertyData::String(value) => Self::String(value),
            PropertyData::Bool(value) => Self::Bool(value),
            PropertyData::I64(value) => Self::I64(value),
            PropertyData::F64(value) => Self::F64(value),
            _ => Self::List(QueryList::property(value)?),
        })
    }
    /// Validates every element before changing scratch. Cancellation while
    /// copying can leave private scratch partially filled, but returns no value;
    /// the caller must not publish it. Backing strings and unused capacity remain
    /// accounted by the caller's reservation, not this allocation-free wrapper.
    pub fn to_property<'out>(
        self,
        scratch: PropertyScratch<'out>,
        context: &mut ValueContext<'_>,
    ) -> Result<PropertyAssignment<'out>, QueryError>
    where
        'a: 'out,
    {
        context.step()?;
        self.validate(context)?;
        let mut bytes = 0_usize;
        let data = match self {
            Self::Null => None,
            Self::String(value) => {
                bytes = value.len();
                Some(PropertyData::String(value))
            }
            Self::Bool(value) => {
                bytes = 1;
                Some(PropertyData::Bool(value))
            }
            Self::I64(value) => {
                bytes = 8;
                Some(PropertyData::I64(value))
            }
            Self::F64(value) => {
                bytes = 8;
                Some(PropertyData::F64(value))
            }
            Self::NodeRef(_) | Self::RelRef(_) => return Err(QueryError::Type),
            Self::List(list) if list.is_empty() => Some(PropertyData::EmptyList { count: 0 }),
            Self::List(list) => {
                let tag = scalar_tag(list.get(0).ok_or(QueryError::Type)?)?;
                for index in 0..list.len() {
                    context.step()?;
                    let value = list.get(index).ok_or(QueryError::Type)?;
                    if scalar_tag(value)? != tag {
                        return Err(QueryError::Type);
                    }
                    let size = match value {
                        Self::String(s) => s.len(),
                        Self::Bool(_) => 1,
                        _ => 8,
                    };
                    bytes = bytes.checked_add(size).ok_or(QueryError::PropertyLimit)?;
                    if bytes > MAX_GRAPH_INPUT_BYTES {
                        return Err(QueryError::PropertyLimit);
                    }
                }
                Some(match (tag, scratch) {
                    (0, PropertyScratch::Strings(output)) => {
                        PropertyData::Strings(copy(list, output, context, |value| match value {
                            Self::String(v) => Ok(v),
                            _ => Err(QueryError::Type),
                        })?)
                    }
                    (1, PropertyScratch::Bools(output)) => {
                        PropertyData::Bools(copy(list, output, context, |value| match value {
                            Self::Bool(v) => Ok(v),
                            _ => Err(QueryError::Type),
                        })?)
                    }
                    (2, PropertyScratch::Integers(output)) => {
                        PropertyData::Integers(copy(list, output, context, |value| match value {
                            Self::I64(v) => Ok(v),
                            _ => Err(QueryError::Type),
                        })?)
                    }
                    (3, PropertyScratch::Floats(output)) => {
                        PropertyData::Floats(copy(list, output, context, |value| match value {
                            Self::F64(v) => Ok(v),
                            _ => Err(QueryError::Type),
                        })?)
                    }
                    _ => return Err(QueryError::Type),
                })
            }
        };
        if bytes > MAX_GRAPH_INPUT_BYTES {
            return Err(QueryError::PropertyLimit);
        }
        Ok(PropertyAssignment { data, bytes })
    }
}
fn scalar_tag(value: QueryValue<'_>) -> Result<u8, QueryError> {
    match value {
        QueryValue::String(_) => Ok(0),
        QueryValue::Bool(_) => Ok(1),
        QueryValue::I64(_) => Ok(2),
        QueryValue::F64(_) => Ok(3),
        _ => Err(QueryError::Type),
    }
}
fn copy<'out, 'value: 'out, T>(
    list: QueryList<'value>,
    output: &'out mut [T],
    context: &mut ValueContext<'_>,
    extract: impl Fn(QueryValue<'value>) -> Result<T, QueryError>,
) -> Result<&'out [T], QueryError> {
    let target = output
        .get_mut(..list.len())
        .ok_or(QueryError::BufferTooSmall)?;
    for (index, slot) in target.iter_mut().enumerate() {
        context.step()?;
        *slot = extract(list.get(index).ok_or(QueryError::Type)?)?;
    }
    Ok(target)
}
