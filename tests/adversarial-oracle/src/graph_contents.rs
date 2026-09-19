//! PG2: exact logical contents, expressed without engine types or byte codecs.

use std::collections::{BTreeMap, BTreeSet};

/// Primitive typed values; floats are original IEEE payloads, never arithmetic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Value<'a> {
    String(&'a str),
    Bool(bool),
    Integer(i64),
    Float(u64),
    Empty,
    Strings(Vec<&'a str>),
    Bools(Vec<bool>),
    Integers(Vec<i64>),
    Floats(Vec<u64>),
}

/// Primitive subset exercised by the seeded canonical-content campaign. The
/// model label is one independently varied document-interpretation field;
/// exhaustive tower/provenance field goldens live at the core public seam.
#[derive(Clone, Debug)]
pub struct Contents<'a> {
    pub relationship: Option<(u128, u128, &'a str)>,
    pub labels: Vec<&'a str>,
    pub properties: Vec<(&'a str, Value<'a>)>,
    pub text: Option<&'a str>,
    pub embedding: Option<(&'a str, Vec<u32>)>,
}

/// Facts observed through independent engine constructors and exact comparison.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Observation {
    pub left_accepted: bool,
    pub right_accepted: bool,
    pub exact_equal: Option<bool>,
}

fn properties<'a>(value: &'a Contents<'a>) -> Option<BTreeMap<&'a str, &'a Value<'a>>> {
    let mut map = BTreeMap::new();
    for (name, value) in &value.properties {
        if map.insert(*name, value).is_some() {
            return None;
        }
    }
    Some(map)
}

/// BTree set/map semantics independently specify ordering/deduplication. No
/// digest or production canonical encoding participates in this expectation.
pub fn check(
    left: &Contents<'_>,
    right: &Contents<'_>,
    observed: Observation,
) -> Result<(), String> {
    let left_properties = properties(left);
    let right_properties = properties(right);
    let expected = Observation {
        left_accepted: left_properties.is_some(),
        right_accepted: right_properties.is_some(),
        exact_equal: left_properties
            .as_ref()
            .zip(right_properties.as_ref())
            .map(|(lp, rp)| {
                let left_labels: BTreeSet<_> = left.labels.iter().copied().collect();
                let right_labels: BTreeSet<_> = right.labels.iter().copied().collect();
                left.relationship == right.relationship
                    && left_labels == right_labels
                    && lp == rp
                    && left.text == right.text
                    && left.embedding == right.embedding
            }),
    };
    if observed == expected {
        Ok(())
    } else {
        Err(format!(
            "PG2 exact contents expected={expected:?} observed={observed:?}; left={left:?} right={right:?}"
        ))
    }
}
