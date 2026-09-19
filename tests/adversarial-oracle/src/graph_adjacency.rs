//! PG12 primitive edge oracle. No engine codecs, merge helpers or strong IDs.
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug)]
pub struct Observation {
    pub rel: u128,
    pub neighbor: u128,
    pub insert: bool,
}
/// Model versions by logical map updates, independent of the nine-way cursor.
pub fn expected(
    base: &[(u128, u128)],
    runs: &[(u64, Vec<Observation>)],
) -> Result<Vec<(u128, u128)>, String> {
    let mut topology = BTreeMap::new();
    let mut state = BTreeMap::new();
    for &(rel, neighbor) in base {
        topology.insert(rel, neighbor);
        state.insert(rel, neighbor);
    }
    let mut observations = BTreeMap::new();
    for (sequence, entries) in runs {
        for v in entries {
            if topology
                .insert(v.rel, v.neighbor)
                .is_some_and(|old| old != v.neighbor)
            {
                return Err("PG12 model topology conflict".into());
            }
            if observations
                .insert((*sequence, v.rel), v.insert)
                .is_some_and(|old| old != v.insert)
            {
                return Err("PG12 model equal-sequence conflict".into());
            }
            if v.insert {
                state.insert(v.rel, v.neighbor);
            } else {
                state.remove(&v.rel);
            }
        }
    }
    Ok(state.into_iter().collect())
}
pub fn check(expected: &[(u128, u128)], observed: &[(u128, u128)]) -> Result<(), String> {
    if expected == observed {
        Ok(())
    } else {
        Err(format!(
            "PG12 exact edge mismatch expected={expected:?} observed={observed:?}"
        ))
    }
}
/// Complete primitive (rel,source,target,type) observations. This can-fire model
/// is explicitly not an authoritative real-directory participant acceptance test.
pub fn check_paired(
    expected: &[(u128, u128, u128, u64)],
    outgoing: &[(u128, u128, u128, u64)],
    incoming: &[(u128, u128, u128, u64)],
) -> Result<(), String> {
    let mut want = expected.to_vec();
    let mut out = outgoing.to_vec();
    let mut input = incoming.to_vec();
    want.sort_unstable();
    out.sort_unstable();
    input.sort_unstable();
    if want == out && want == input {
        Ok(())
    } else {
        Err("PG12 paired model missing or changed directional edge".into())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adjacency_oracle_detects_ignored_delete_neighbor_and_missing_reverse() {
        let base = [(1, 2), (1 << 100, 3)];
        let runs = [(
            1,
            vec![Observation {
                rel: 1,
                neighbor: 2,
                insert: false,
            }],
        )];
        let want = expected(&base, &runs).unwrap();
        assert_eq!(want, vec![(1 << 100, 3)]);
        assert!(check(&want, &base).is_err());
        assert!(check(&want, &[(1 << 100, 4)]).is_err());
        assert!(check(&want, &want).is_ok());
        let model = [(1, 2, 2, 7), (3, 2, 4, 7), (4, 2, 4, 7)];
        assert!(check_paired(&model, &model, &model).is_ok());
        assert!(check_paired(&model, &model, &model[..2]).is_err());
    }
}
