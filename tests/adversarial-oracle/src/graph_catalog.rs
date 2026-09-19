//! PG4: primitive name-assignment and interpretation facts, independent of codecs.

use std::collections::{BTreeMap, BTreeSet};

/// Primitive observation of a reconstructed dictionary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Dictionary<'a> {
    pub rows: Vec<(u8, u64, &'a str)>,
    pub high_waters: [u64; 4],
}

/// Checks exact stable assignments and retained high-waters. Names are map keys;
/// this model neither reads production framing nor calls a production predicate.
pub fn check_dictionary(
    rows: &[(u8, u64, &str)],
    high_waters: [u64; 4],
    observed: Option<Dictionary<'_>>,
) -> Result<(), String> {
    let mut names = BTreeMap::new();
    let mut ids = BTreeSet::new();
    let mut valid = true;
    for &(kind, id, name) in rows {
        let bound = kind
            .checked_sub(1)
            .and_then(|slot| high_waters.get(usize::from(slot)));
        valid &= bound.is_some_and(|bound| id != 0 && id <= *bound);
        valid &= ids.insert((kind, id));
        valid &= names.insert((kind, name), id).is_none();
    }
    let expected = valid.then(|| Dictionary {
        rows: names
            .into_iter()
            .map(|((kind, name), id)| (kind, id, name))
            .collect(),
        high_waters,
    });
    if observed == expected {
        Ok(())
    } else {
        Err(format!(
            "PG4 dictionary expected={expected:?}, observed={observed:?}"
        ))
    }
}

/// Primitive identity subset varied by the seeded admission campaign. Exhaustive
/// complete-tower field checks are separately frozen at the core public seam.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Interpretation<'a> {
    pub lexical: u64,
    pub document: Option<(&'a str, u32)>,
}

pub fn check_admission(
    stored: Interpretation<'_>,
    declared: Interpretation<'_>,
    accepted: bool,
) -> Result<(), String> {
    let expected = stored == declared;
    if accepted == expected {
        Ok(())
    } else {
        Err(format!(
            "PG4 admission expected={expected}, observed={accepted}"
        ))
    }
}

/// Independent sequential interning model, including exhaustion and repeated names.
pub fn check_interning(
    requests: &[(u8, &str)],
    initial: [u64; 4],
    observed: &[Option<u64>],
    final_high_waters: [u64; 4],
) -> Result<(), String> {
    let mut names = BTreeMap::new();
    let mut waters = initial;
    let mut expected = Vec::new();
    for &(kind, name) in requests {
        let result = if let Some(&id) = names.get(&(kind, name)) {
            Some(id)
        } else {
            kind.checked_sub(1)
                .and_then(|slot| waters.get_mut(usize::from(slot)))
                .and_then(|water| {
                    let next = water.checked_add(1)?;
                    *water = next;
                    names.insert((kind, name), next);
                    Some(next)
                })
        };
        expected.push(result);
    }
    if expected == observed && waters == final_high_waters {
        Ok(())
    } else {
        Err(format!(
            "PG4 interning expected={expected:?}/{waters:?}, observed={observed:?}/{final_high_waters:?}"
        ))
    }
}
