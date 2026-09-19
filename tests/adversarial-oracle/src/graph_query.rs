//! PG6: primitive query predicates/grouping and lexical scopes. This std-only
//! oracle never imports engine values, hash encodings, or plan validators.
use std::cmp::Ordering;
use std::collections::BTreeSet;
#[derive(Clone, Debug)]
pub enum Value<'a> {
    Null,
    Bool(bool),
    Integer(i64),
    Float(u64),
    String(&'a str),
    Node(u128),
    Relationship(u128),
    List(Vec<Value<'a>>),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Observation {
    pub equal: Option<bool>,
    pub less: Option<bool>,
    pub order: Ordering,
    pub equivalent: bool,
    pub same_hash: bool,
}
fn rank(value: &Value<'_>) -> u8 {
    match value {
        Value::Node(_) => 0,
        Value::Relationship(_) => 1,
        Value::List(_) => 2,
        Value::String(_) => 3,
        Value::Bool(_) => 4,
        Value::Integer(_) | Value::Float(_) => 5,
        Value::Null => 6,
    }
}
// Independent route: bound the float, truncate it, then compare the exact
// integer parts and fractional sign. No mantissa/exponent decomposition.
fn mixed(integer: i64, value: f64) -> Option<Ordering> {
    if value.is_nan() {
        return None;
    }
    if value >= 9_223_372_036_854_775_808.0 {
        return Some(Ordering::Less);
    }
    if value < -9_223_372_036_854_775_808.0 {
        return Some(Ordering::Greater);
    }
    let whole = value.trunc() as i64;
    Some(match integer.cmp(&whole) {
        Ordering::Equal if value.fract() > 0.0 => Ordering::Less,
        Ordering::Equal if value.fract() < 0.0 => Ordering::Greater,
        other => other,
    })
}
fn number(left: &Value<'_>, right: &Value<'_>) -> Option<Ordering> {
    match (left, right) {
        (Value::Integer(a), Value::Integer(b)) => Some(a.cmp(b)),
        (Value::Integer(a), Value::Float(b)) => mixed(*a, f64::from_bits(*b)),
        (Value::Float(a), Value::Integer(b)) => {
            mixed(*b, f64::from_bits(*a)).map(Ordering::reverse)
        }
        (Value::Float(a), Value::Float(b)) => f64::from_bits(*a).partial_cmp(&f64::from_bits(*b)),
        _ => None,
    }
}
fn equal(left: &Value<'_>, right: &Value<'_>) -> Option<bool> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return None;
    }
    if rank(left) != rank(right) {
        return Some(false);
    }
    match (left, right) {
        (Value::List(a), Value::List(b)) => {
            if a.len() != b.len() {
                return Some(false);
            }
            let mut unknown = false;
            for (a, b) in a.iter().zip(b) {
                match equal(a, b) {
                    Some(false) => return Some(false),
                    None => unknown = true,
                    _ => {}
                }
            }
            if unknown { None } else { Some(true) }
        }
        _ if rank(left) == 5 => Some(number(left, right) == Some(Ordering::Equal)),
        _ => Some(order(left, right) == Ordering::Equal),
    }
}
fn less(left: &Value<'_>, right: &Value<'_>) -> Option<bool> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) || rank(left) != rank(right) {
        return None;
    }
    match (left, right) {
        (Value::List(a), Value::List(b)) => {
            for (a, b) in a.iter().zip(b) {
                if equal(a, b) != Some(true) {
                    return less(a, b);
                }
            }
            Some(a.len() < b.len())
        }
        _ if rank(left) == 5 => Some(number(left, right) == Some(Ordering::Less)),
        _ => Some(order(left, right) == Ordering::Less),
    }
}
fn order(left: &Value<'_>, right: &Value<'_>) -> Ordering {
    let class = rank(left).cmp(&rank(right));
    if class != Ordering::Equal {
        return class;
    }
    match (left, right) {
        (Value::Node(a), Value::Node(b)) | (Value::Relationship(a), Value::Relationship(b)) => {
            a.cmp(b)
        }
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Value::String(a), Value::String(b)) => a.chars().cmp(b.chars()),
        (Value::List(a), Value::List(b)) => {
            for (a, b) in a.iter().zip(b) {
                let order = order(a, b);
                if order != Ordering::Equal {
                    return order;
                }
            }
            a.len().cmp(&b.len())
        }
        (Value::Null, Value::Null) => Ordering::Equal,
        _ => number(left, right).unwrap_or_else(|| {
            let nan =
                |v: &Value<'_>| matches!(v,Value::Float(bits) if f64::from_bits(*bits).is_nan());
            nan(left).cmp(&nan(right))
        }),
    }
}
/// Hash equality is necessary for equivalence; unequal keys may collide.
pub fn check(left: &Value<'_>, right: &Value<'_>, observed: Observation) -> Result<(), String> {
    let expected_order = order(left, right);
    if observed.equal != equal(left, right)
        || observed.less != less(left, right)
        || observed.order != expected_order
        || observed.equivalent != (expected_order == Ordering::Equal)
        || expected_order == Ordering::Equal && !observed.same_hash
    {
        return Err(format!(
            "PG6 query value mismatch left={left:?} right={right:?} observed={observed:?}"
        ));
    }
    Ok(())
}
/// Each pair is destination plus optional source variable; None is a literal.
/// Projection replaces the complete lexical environment, using a plain set.
pub fn check_scope(
    projections: &[Vec<(u32, Option<u32>)>],
    final_variable: u32,
    accepted: bool,
) -> Result<(), String> {
    let mut scope = BTreeSet::new();
    let mut valid = true;
    for projection in projections {
        let mut next = BTreeSet::new();
        for (destination, source) in projection {
            valid &=
                source.is_none_or(|source| scope.contains(&source)) && next.insert(*destination);
        }
        scope = next;
    }
    valid &= scope.contains(&final_variable);
    if valid == accepted {
        Ok(())
    } else {
        Err(format!(
            "PG6 lexical scope mismatch expected={valid} observed={accepted}"
        ))
    }
}
