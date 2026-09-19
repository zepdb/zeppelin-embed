use super::{DomainError, MAX_GRAPH_INPUT_BYTES, MAX_PROPERTY_LIST_ELEMENTS};

/// Borrowed scalar or homogeneous scalar-list input. Stored null is absence,
/// never a value variant. These inputs have no nested/heterogeneous list form.
/// F64 bits are lossless; expression equality is a separate execution contract.
#[derive(Clone, Copy, Debug)]
pub enum PropertyData<'a> {
    /// Byte-exact UTF-8, including empty strings and embedded NUL.
    String(&'a str),
    /// Boolean scalar.
    Bool(bool),
    /// Signed 64-bit scalar.
    I64(i64),
    /// Every IEEE-754 bit pattern, including NaN payloads and signed zero.
    F64(f64),
    /// Untyped empty-list input; validation rejects any nonzero count.
    EmptyList {
        /// Must be zero, including when decoding external tagged values.
        count: usize,
    },
    /// String elements, retaining the type even with zero elements.
    Strings(&'a [&'a str]),
    /// Boolean elements, retaining the type even with zero elements.
    Bools(&'a [bool]),
    /// Signed integer elements, retaining the type even with zero elements.
    Integers(&'a [i64]),
    /// IEEE scalar elements, retaining every bit and the empty element type.
    Floats(&'a [f64]),
}

/// Validated, allocation-free property input. Its caller owns the borrowed
/// storage; write staging must reserve and own any copies before publication.
#[derive(Clone, Copy, Debug)]
pub struct PropertyValue<'a> {
    data: PropertyData<'a>,
    payload_bytes: usize,
}

impl<'a> PropertyValue<'a> {
    /// Validates the shared list and input byte limits without allocation.
    pub fn new(data: PropertyData<'a>) -> Result<Self, DomainError> {
        let value = Self {
            data,
            payload_bytes: 0,
        };
        if value
            .list_len()
            .is_some_and(|len| len > MAX_PROPERTY_LIST_ELEMENTS)
        {
            return Err(DomainError::ListTooLong);
        }
        let bytes = match data {
            PropertyData::String(value) => value.len(),
            PropertyData::Bool(_) => 1,
            PropertyData::I64(_) | PropertyData::F64(_) => 8,
            PropertyData::EmptyList { count: 0 } => 0,
            PropertyData::EmptyList { .. } => return Err(DomainError::NonemptyUntypedList),
            PropertyData::Strings(values) => values.iter().try_fold(0_usize, |bytes, value| {
                let total = bytes
                    .checked_add(value.len())
                    .ok_or(DomainError::InputTooLarge)?;
                bounded_bytes(total)
            })?,
            PropertyData::Bools(values) => values.len(),
            PropertyData::Integers(values) => scalar_bytes(values.len(), 8)?,
            PropertyData::Floats(values) => scalar_bytes(values.len(), 8)?,
        };
        Ok(Self {
            data,
            payload_bytes: bounded_bytes(bytes)?,
        })
    }

    /// Returns the original typed input, including its empty-list tag.
    #[must_use]
    pub const fn data(self) -> PropertyData<'a> {
        self.data
    }

    /// Returns raw scalar payload bytes. This excludes canonical tags/lengths
    /// and is not a claim about engine-owned allocation or encoded size.
    #[must_use]
    pub const fn payload_bytes(self) -> usize {
        self.payload_bytes
    }

    /// Returns None for a scalar, or the exact number of list elements.
    #[must_use]
    pub const fn list_len(self) -> Option<usize> {
        match self.data {
            PropertyData::EmptyList { count } => Some(count),
            PropertyData::Strings(values) => Some(values.len()),
            PropertyData::Bools(values) => Some(values.len()),
            PropertyData::Integers(values) => Some(values.len()),
            PropertyData::Floats(values) => Some(values.len()),
            _ => None,
        }
    }
}

fn bounded_bytes(bytes: usize) -> Result<usize, DomainError> {
    if bytes > MAX_GRAPH_INPUT_BYTES {
        Err(DomainError::InputTooLarge)
    } else {
        Ok(bytes)
    }
}

fn scalar_bytes(count: usize, width: usize) -> Result<usize, DomainError> {
    bounded_bytes(count.checked_mul(width).ok_or(DomainError::InputTooLarge)?)
}

/// Borrowed supplied vector, separate from nullable graph membership and
/// unrestricted IEEE scalar properties. Space/epoch admission belongs to catalog.
#[derive(Clone, Copy, Debug)]
pub struct GraphVector<'a>(&'a [f32]);

impl<'a> GraphVector<'a> {
    /// Validates dimensions, bounded bytes and finite coordinates without copying.
    pub fn new(coordinates: &'a [f32], expected_dimensions: u32) -> Result<Self, DomainError> {
        if expected_dimensions == 0
            || usize::try_from(expected_dimensions).ok() != Some(coordinates.len())
        {
            return Err(DomainError::VectorDimensions);
        }
        scalar_bytes(coordinates.len(), 4)?;
        if coordinates.iter().any(|value| !value.is_finite()) {
            return Err(DomainError::NonfiniteVector);
        }
        Ok(Self(coordinates))
    }

    /// Returns original f32 coordinates, retaining signed-zero bits.
    #[must_use]
    pub const fn coordinates(self) -> &'a [f32] {
        self.0
    }

    /// Returns the validated raw coordinate byte count.
    #[must_use]
    pub const fn payload_bytes(self) -> usize {
        std::mem::size_of_val(self.0)
    }
}
