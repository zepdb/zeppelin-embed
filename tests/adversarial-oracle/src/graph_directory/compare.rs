use super::{Fence, Key, Observation, Provenance, Record, Shape};
use std::fmt::Debug;

/// First exact mismatch. Values describe only the mismatching scalar/length,
/// never format an unbounded observed byte buffer into an error allocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Difference {
    pub path: String,
    pub expected: String,
    pub observed: String,
}

pub(super) fn check(expected: &Observation, observed: &Observation) -> Result<(), Difference> {
    field("generation", &expected.generation, &observed.generation)?;
    sequence("nodes", &expected.nodes, &observed.nodes, record)?;
    sequence(
        "relationships",
        &expected.relationships,
        &observed.relationships,
        record,
    )?;
    sequence("fences", &expected.fences, &observed.fences, fence)?;
    sequence("labels", &expected.labels, &observed.labels, field)?;
    sequence("types", &expected.types, &observed.types, field)?;
    sequence(
        "node_tombstones",
        &expected.node_tombstones,
        &observed.node_tombstones,
        provenance,
    )
}

fn record(path: &str, expected: &Record, observed: &Record) -> Result<(), Difference> {
    provenance(
        &format!("{path}.provenance"),
        &expected.provenance,
        &observed.provenance,
    )?;
    sequence(
        &format!("{path}.canonical"),
        &expected.image.canonical,
        &observed.image.canonical,
        field,
    )?;
    match (&expected.image.shape, &observed.image.shape) {
        (Shape::Node { labels: expected }, Shape::Node { labels: observed }) => {
            sequence(&format!("{path}.labels"), expected, observed, field)
        }
        (
            Shape::Relationship {
                source,
                target,
                rel_type,
            },
            Shape::Relationship {
                source: observed_source,
                target: observed_target,
                rel_type: observed_type,
            },
        ) => {
            field(&format!("{path}.source"), source, observed_source)?;
            field(&format!("{path}.target"), target, observed_target)?;
            field(&format!("{path}.rel_type"), rel_type, observed_type)
        }
        (Shape::Node { .. }, Shape::Relationship { .. }) => {
            field(&format!("{path}.shape"), &"node", &"relationship")
        }
        (Shape::Relationship { .. }, Shape::Node { .. }) => {
            field(&format!("{path}.shape"), &"relationship", &"node")
        }
    }
}

fn fence(path: &str, expected: &Fence, observed: &Fence) -> Result<(), Difference> {
    provenance(
        &format!("{path}.provenance"),
        &expected.provenance,
        &observed.provenance,
    )?;
    field(
        &format!("{path}.canonical.present"),
        &expected.canonical.is_some(),
        &observed.canonical.is_some(),
    )?;
    if let (Some(expected), Some(observed)) = (&expected.canonical, &observed.canonical) {
        sequence(&format!("{path}.canonical"), expected, observed, field)?;
    }
    Ok(())
}

fn provenance(path: &str, expected: &Provenance, observed: &Provenance) -> Result<(), Difference> {
    field(
        &format!("{path}.version"),
        &expected.version,
        &observed.version,
    )?;
    field(
        &format!("{path}.operation"),
        &expected.operation,
        &observed.operation,
    )?;
    key(&format!("{path}.key"), &expected.key, &observed.key)?;
    field(
        &format!("{path}.requested_revision"),
        &expected.requested_revision,
        &observed.requested_revision,
    )?;
    field(
        &format!("{path}.installed_revision"),
        &expected.installed_revision,
        &observed.installed_revision,
    )?;
    field(
        &format!("{path}.expected"),
        &expected.expected,
        &observed.expected,
    )?;
    field(
        &format!("{path}.incarnation"),
        &expected.incarnation,
        &observed.incarnation,
    )?;
    field(
        &format!("{path}.delete_mode"),
        &expected.delete_mode,
        &observed.delete_mode,
    )?;
    field(
        &format!("{path}.original_generation"),
        &expected.original_generation,
        &observed.original_generation,
    )
}

fn key(path: &str, expected: &Option<Key>, observed: &Option<Key>) -> Result<(), Difference> {
    field(
        &format!("{path}.present"),
        &expected.is_some(),
        &observed.is_some(),
    )?;
    if let (Some(expected), Some(observed)) = (expected, observed) {
        field(&format!("{path}.kind"), &expected.kind, &observed.kind)?;
        sequence(
            &format!("{path}.namespace"),
            &expected.namespace,
            &observed.namespace,
            field,
        )?;
        sequence(&format!("{path}.key"), &expected.key, &observed.key, field)?;
    }
    Ok(())
}

fn sequence<T>(
    path: &str,
    expected: &[T],
    observed: &[T],
    mut compare: impl FnMut(&str, &T, &T) -> Result<(), Difference>,
) -> Result<(), Difference> {
    field(&format!("{path}.length"), &expected.len(), &observed.len())?;
    for (index, (expected, observed)) in expected.iter().zip(observed).enumerate() {
        compare(&format!("{path}[{index}]"), expected, observed)?;
    }
    Ok(())
}

fn field<T: Eq + Debug>(path: &str, expected: &T, observed: &T) -> Result<(), Difference> {
    if expected == observed {
        Ok(())
    } else {
        Err(Difference {
            path: path.to_owned(),
            expected: format!("{expected:?}"),
            observed: format!("{observed:?}"),
        })
    }
}
