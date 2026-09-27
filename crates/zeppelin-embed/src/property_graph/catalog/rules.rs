//! Immutable relationship declarations carried by the graph catalog.
use super::CatalogError;
use crate::property_graph::{GraphName, MAX_GRAPH_CHANGES, MAX_GRAPH_INPUT_BYTES};

/// Action on deletion of the target of a child -> parent relationship.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum OnDelete {
    /// Refuse while a surviving child still references the target.
    Restrict = 1,
    /// Delete the source node, recursively, in the same mutation.
    Cascade = 2,
}

/// An immutable declaration for one exact relationship type name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RelationshipRule<'a> {
    /// Exact, case-sensitive relationship type.
    pub relationship_type: GraphName<'a>,
    /// Incoming-reference deletion action.
    pub on_delete: OnDelete,
}

#[derive(Clone, Copy, Debug)]
enum Backing<'a> {
    Declared(&'a [RelationshipRule<'a>]),
    Encoded(&'a [u8]),
}

/// Validated borrowed declarations; decoded backing stays in its catalog lease.
#[derive(Clone, Copy, Debug)]
pub struct RelationshipRules<'a>(Backing<'a>);
impl<'a> RelationshipRules<'a> {
    /// The v1 catalog has no declarations and retains its original DELETE rules.
    pub const EMPTY: Self = Self(Backing::Declared(&[]));

    /// Validates unique names and bounded input without allocating.
    pub fn new(rules: &'a [RelationshipRule<'a>]) -> Result<Self, CatalogError> {
        let value = Self(Backing::Declared(rules));
        value.validate(&mut || Ok(()))?;
        Ok(value)
    }

    pub(super) fn decode(
        bytes: &'a [u8],
        checkpoint: &mut dyn FnMut() -> Result<(), CatalogError>,
    ) -> Result<Self, CatalogError> {
        let value = Self(Backing::Encoded(bytes));
        value.validate(checkpoint)?;
        Ok(value)
    }

    /// True when no type has a declared policy.
    pub fn is_empty(self) -> bool {
        match self.0 {
            Backing::Declared(rules) => rules.is_empty(),
            Backing::Encoded(bytes) => bytes.is_empty(),
        }
    }

    /// Looks up an exact type, polling throughout catalog work.
    pub fn lookup(
        self,
        name: GraphName<'_>,
        checkpoint: &mut dyn FnMut() -> Result<(), CatalogError>,
    ) -> Result<Option<OnDelete>, CatalogError> {
        for rule in self.iter() {
            checkpoint()?;
            let rule = rule?;
            if super::work::compare_bytes(
                rule.relationship_type.as_str().as_bytes(),
                name.as_str().as_bytes(),
                checkpoint,
            )? == std::cmp::Ordering::Equal
            {
                return Ok(Some(rule.on_delete));
            }
        }
        Ok(None)
    }

    fn validate(
        self,
        checkpoint: &mut dyn FnMut() -> Result<(), CatalogError>,
    ) -> Result<(), CatalogError> {
        let mut bytes = 0usize;
        for (index, rule) in self.iter().enumerate() {
            checkpoint()?;
            let rule = rule?;
            bytes = bytes
                .checked_add(9)
                .and_then(|n| n.checked_add(rule.relationship_type.as_str().len()))
                .ok_or(CatalogError::Capacity)?;
            if index >= MAX_GRAPH_CHANGES || bytes > MAX_GRAPH_INPUT_BYTES {
                return Err(CatalogError::Capacity);
            }
            for prior in self.iter().take(index) {
                checkpoint()?;
                if super::work::compare_bytes(
                    prior?.relationship_type.as_str().as_bytes(),
                    rule.relationship_type.as_str().as_bytes(),
                    checkpoint,
                )? == std::cmp::Ordering::Equal
                {
                    return Err(CatalogError::Duplicate);
                }
            }
        }
        Ok(())
    }

    pub(super) fn iter(self) -> impl Iterator<Item = Result<RelationshipRule<'a>, CatalogError>> {
        let mut backing = self.0;
        std::iter::from_fn(move || match &mut backing {
            Backing::Declared(rules) => {
                let (first, rest) = rules.split_first()?;
                *rules = rest;
                Some(Ok(*first))
            }
            Backing::Encoded(bytes) => {
                if bytes.is_empty() {
                    return None;
                }
                let result = decode_one(bytes);
                match result {
                    Ok((rule, rest)) => {
                        *bytes = rest;
                        Some(Ok(rule))
                    }
                    Err(error) => {
                        *bytes = &[];
                        Some(Err(error))
                    }
                }
            }
        })
    }
}

fn decode_one(bytes: &[u8]) -> Result<(RelationshipRule<'_>, &[u8]), CatalogError> {
    let on_delete = match bytes.first() {
        Some(1) => OnDelete::Restrict,
        Some(2) => OnDelete::Cascade,
        _ => return Err(CatalogError::Unsupported),
    };
    let length = bytes
        .get(1..9)
        .and_then(|v| v.first_chunk::<8>())
        .ok_or(CatalogError::Malformed)?;
    let length =
        usize::try_from(u64::from_le_bytes(*length)).map_err(|_| CatalogError::Capacity)?;
    let end = 9usize.checked_add(length).ok_or(CatalogError::Capacity)?;
    let name = std::str::from_utf8(bytes.get(9..end).ok_or(CatalogError::Malformed)?)
        .map_err(|_| CatalogError::Malformed)?;
    Ok((
        RelationshipRule {
            relationship_type: GraphName::new(name).map_err(|_| CatalogError::Malformed)?,
            on_delete,
        },
        bytes.get(end..).ok_or(CatalogError::Malformed)?,
    ))
}
