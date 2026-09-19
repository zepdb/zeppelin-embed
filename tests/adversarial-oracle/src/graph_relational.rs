//! PG14 primitive independent relational expectations. No production values,
//! query hash/order helpers, buffers or guards enter this model.
use std::collections::BTreeSet;

pub fn check(
    phase: usize,
    numbers: &[i64],
    nodes: &[u128],
    observed: &[u128],
) -> Result<(), String> {
    let expected: Vec<u128> = match phase {
        0 => {
            let mut values = numbers.to_vec();
            values.sort();
            values.into_iter().map(|v| v as u128).collect()
        }
        1 => {
            let mut seen = BTreeSet::new();
            numbers
                .iter()
                .filter_map(|v| seen.insert(*v).then_some(*v as u128))
                .collect()
        }
        2 => nodes
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        _ => return Err("PG14 unknown phase".into()),
    };
    if observed != expected {
        return Err(format!(
            "PG14 phase{phase}: expected={expected:?}, observed={observed:?}"
        ));
    }
    Ok(())
}
pub fn check_failure(success: bool, fires: usize, retained_delta: usize) -> Result<(), String> {
    if success || fires != 1 || retained_delta != 0 {
        return Err(format!(
            "PG14 failure evidence: success={success}, fires={fires}, retained={retained_delta}"
        ));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relational_oracle_orders_signed_values_before_transport_encoding() {
        let negative = (-3_i64) as u128;
        assert!(check(0, &[-3, 1, -3], &[], &[negative, negative, 1]).is_ok());
        assert!(check(0, &[-3, 1, -3], &[], &[1, negative, negative]).is_err());
    }
    #[test]
    fn relational_oracle_refuses_lost_bags_wrong_order_truncated_ids_and_false_faults() {
        assert!(check(0, &[3, 1, 3], &[], &[1, 3, 3]).is_ok());
        assert!(check(0, &[3, 1, 3], &[], &[1, 3]).is_err());
        assert!(check(1, &[3, 1, 3], &[], &[3, 1]).is_ok());
        assert!(check(1, &[3, 1, 3], &[], &[1, 3]).is_err());
        assert!(check(2, &[], &[7, (1 << 100) | 7, 7], &[7, (1 << 100) | 7]).is_ok());
        assert!(check(2, &[], &[7, (1 << 100) | 7, 7], &[7]).is_err());
        assert!(check_failure(false, 1, 0).is_ok());
        for (success, fires, retained) in [(true, 1, 0), (false, 0, 0), (false, 1, 8)] {
            assert!(check_failure(success, fires, retained).is_err());
        }
    }
}
