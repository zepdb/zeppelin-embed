//! Query order/equivalence/hash, deliberately unrelated to replay encoding.
use super::value::{compare_bytes, integer_float};
use super::{QueryError, QueryValue, ValueContext};
use std::cmp::Ordering;
use xxhash_rust::xxh3::Xxh3;

impl QueryValue<'_> {
    /// Total ascending supported-type order. Descending reverses this order.
    pub fn order(
        self,
        other: Self,
        context: &mut ValueContext<'_>,
    ) -> Result<Ordering, QueryError> {
        context.step()?;
        self.validate(context)?;
        other.validate(context)?;
        let rank = self.rank().cmp(&other.rank());
        if rank != Ordering::Equal {
            return Ok(rank);
        }
        Ok(match (self, other) {
            (Self::Null, Self::Null) => Ordering::Equal,
            (Self::NodeRef(a), Self::NodeRef(b)) => a.id().cmp(&b.id()),
            (Self::RelRef(a), Self::RelRef(b)) => a.id().cmp(&b.id()),
            (Self::Bool(a), Self::Bool(b)) => a.cmp(&b),
            (Self::String(a), Self::String(b)) => {
                compare_bytes(a.as_bytes(), b.as_bytes(), context)?
            }
            (Self::I64(a), Self::I64(b)) => a.cmp(&b),
            (Self::I64(a), Self::F64(b)) => integer_float(a, b).unwrap_or(Ordering::Less),
            (Self::F64(a), Self::I64(b)) => integer_float(b, a)
                .map(Ordering::reverse)
                .unwrap_or(Ordering::Greater),
            (Self::F64(a), Self::F64(b)) => match a.partial_cmp(&b) {
                Some(order) => order,
                None => a.is_nan().cmp(&b.is_nan()),
            },
            (Self::List(a), Self::List(b)) => {
                for index in 0..a.len().min(b.len()) {
                    let order = a
                        .get(index)
                        .ok_or(QueryError::ListLimit)?
                        .order(b.get(index).ok_or(QueryError::ListLimit)?, context)?;
                    if order != Ordering::Equal {
                        return Ok(order);
                    }
                }
                a.len().cmp(&b.len())
            }
            _ => return Err(QueryError::Type),
        })
    }
    /// Grouping/DISTINCT equivalence: nulls and NaNs coalesce recursively.
    pub fn equivalent(
        self,
        other: Self,
        context: &mut ValueContext<'_>,
    ) -> Result<bool, QueryError> {
        Ok(self.order(other, context)? == Ordering::Equal)
    }
    /// Hashes grouping equivalence, never canonical replay bytes. A matching
    /// digest is only a bucket selector; callers still compare equivalence.
    pub fn group_hash(self, context: &mut ValueContext<'_>) -> Result<u64, QueryError> {
        let mut hash = Xxh3::new();
        self.hash_into(&mut hash, context)?;
        Ok(hash.digest())
    }
    fn rank(self) -> u8 {
        match self {
            Self::NodeRef(_) => 0,
            Self::RelRef(_) => 1,
            Self::List(_) => 2,
            Self::String(_) => 3,
            Self::Bool(_) => 4,
            Self::I64(_) | Self::F64(_) => 5,
            Self::Null => 6,
        }
    }
    fn hash_into(self, hash: &mut Xxh3, context: &mut ValueContext<'_>) -> Result<(), QueryError> {
        context.step()?;
        self.validate(context)?;
        hash.update(&[self.rank()]);
        match self {
            Self::Null => {}
            Self::NodeRef(value) => hash.update(&value.id().get().to_le_bytes()),
            Self::RelRef(value) => hash.update(&value.id().get().to_le_bytes()),
            Self::Bool(value) => hash.update(&[u8::from(value)]),
            Self::String(value) => {
                hash.update(&(value.len() as u64).to_le_bytes());
                for bytes in value.as_bytes().chunks(65_536) {
                    context.step()?;
                    hash.update(bytes);
                }
            }
            Self::I64(value) => hash_integer(hash, value),
            Self::F64(value) => {
                // Rust's saturating conversion is only a candidate. Exact
                // comparison rejects rounded/out-of-range candidates.
                let integer = value as i64;
                if integer_float(integer, value) == Some(Ordering::Equal) {
                    hash_integer(hash, integer);
                } else {
                    hash.update(&[1]);
                    hash.update(
                        &if value.is_nan() {
                            0x7ff8_0000_0000_0000_u64
                        } else {
                            value.to_bits()
                        }
                        .to_le_bytes(),
                    );
                }
            }
            Self::List(value) => {
                hash.update(&(value.len() as u64).to_le_bytes());
                for index in 0..value.len() {
                    value
                        .get(index)
                        .ok_or(QueryError::ListLimit)?
                        .hash_into(hash, context)?;
                }
            }
        }
        Ok(())
    }
}
fn hash_integer(hash: &mut Xxh3, value: i64) {
    hash.update(&[0]);
    hash.update(&value.to_le_bytes());
}
