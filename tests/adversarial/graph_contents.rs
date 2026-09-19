//! Canonical logical contents have no publication or VFS mutation. These probes
//! exercise the new deterministic seam in every seeded fault-runner episode.

use rand::RngCore;
use zeppelin_embed::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower, Normalization};
use zeppelin_embed::property_graph::{
    CanonicalContents, CanonicalEmbedding, CanonicalError, CanonicalFingerprint, GraphName,
    GraphProperty, NodeId, PropertyData, PropertyValue, compare_canonical_streams,
};
use zeppelin_embed_adversarial_oracle::graph_contents::{Contents, Observation, Value, check};

use super::coverage::CoverageRegistry;

pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.contents.order",
    "property-graph.contents.duplicate",
    "property-graph.contents.ieee-bits",
    "property-graph.contents.typed-empty",
    "property-graph.contents.full-width",
    "property-graph.contents.vector-bits",
    "property-graph.contents.forced-collision",
    "property-graph.contents.cancel",
];

fn image(input: &Contents<'_>) -> Result<Vec<u8>, CanonicalError> {
    // Backing float arrays retain original IEEE bits and outlive all descriptors.
    let float_lists: Vec<Vec<f64>> = input
        .properties
        .iter()
        .map(|(_, value)| match value {
            Value::Floats(values) => values.iter().map(|bits| f64::from_bits(*bits)).collect(),
            _ => Vec::new(),
        })
        .collect();
    let mut properties = Vec::new();
    for ((name, value), floats) in input.properties.iter().zip(&float_lists) {
        let data = match value {
            Value::String(value) => PropertyData::String(value),
            Value::Bool(value) => PropertyData::Bool(*value),
            Value::Integer(value) => PropertyData::I64(*value),
            Value::Float(value) => PropertyData::F64(f64::from_bits(*value)),
            Value::Empty => PropertyData::EmptyList { count: 0 },
            Value::Strings(value) => PropertyData::Strings(value),
            Value::Bools(value) => PropertyData::Bools(value),
            Value::Integers(value) => PropertyData::Integers(value),
            Value::Floats(_) => PropertyData::Floats(floats),
        };
        properties.push(GraphProperty::new(
            GraphName::new(name).map_err(CanonicalError::Domain)?,
            PropertyValue::new(data).map_err(CanonicalError::Domain)?,
        ));
    }
    let mut labels: Vec<_> = input
        .labels
        .iter()
        .map(|name| GraphName::new(name))
        .collect::<Result<_, _>>()
        .map_err(CanonicalError::Domain)?;
    let coordinates: Vec<_> = input.embedding.as_ref().map_or_else(Vec::new, |(_, bits)| {
        bits.iter().map(|value| f32::from_bits(*value)).collect()
    });
    let tower = EmbeddingTower {
        model_id: input
            .embedding
            .as_ref()
            .map_or("", |(name, _)| *name)
            .into(),
        model_version: "1".into(),
        weights_digest: vec![1],
        dims: coordinates.len() as u32,
        normalization: Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 1,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    let embedding = input
        .embedding
        .as_ref()
        .map(|_| CanonicalEmbedding::new(&tower, &coordinates))
        .transpose()?;
    let canonical = if let Some((source, target, name)) = input.relationship {
        CanonicalContents::relationship(
            NodeId::new(source).map_err(CanonicalError::Domain)?,
            NodeId::new(target).map_err(CanonicalError::Domain)?,
            GraphName::new(name).map_err(CanonicalError::Domain)?,
            &mut properties,
        )?
    } else {
        CanonicalContents::node(&mut labels, &mut properties, input.text, embedding)?
    };
    let mut bytes = Vec::new();
    canonical.write_to(&mut bytes, &mut || Ok(()))?;
    Ok(bytes)
}

fn observe(left: &Contents<'_>, right: &Contents<'_>) -> Result<Observation, String> {
    let l = image(left);
    let r = image(right);
    for result in [&l, &r] {
        if let Err(error) = result
            && !matches!(error, CanonicalError::DuplicateProperty)
        {
            return Err(error.to_string());
        }
    }
    let exact_equal = if let (Ok(left), Ok(right)) = (&l, &r) {
        // All hashes deliberately collide. Length remains a valid mismatch
        // precheck; equal lengths must still compare every actual content byte.
        let left_fp = CanonicalFingerprint::new(left.len() as u64, 0).map_err(|e| e.to_string())?;
        let right_fp =
            CanonicalFingerprint::new(right.len() as u64, 0).map_err(|e| e.to_string())?;
        let compared = compare_canonical_streams(
            &mut left.as_slice(),
            left_fp,
            &mut right.as_slice(),
            right_fp,
            &mut [0; 31],
            &mut || Ok(()),
        )
        .map_err(|e| e.to_string())?;
        Some(compared.equal)
    } else {
        None
    };
    Ok(Observation {
        left_accepted: l.is_ok(),
        right_accepted: r.is_ok(),
        exact_equal,
    })
}

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let mut rng = super::test_support::seeded_rng("property_graph::canonical_probe", seed);
    for _ in 0..16 {
        let bits = rng.next_u64();
        let finite = rng.next_u32() & 0x807f_ffff;
        let left = Contents {
            relationship: None,
            labels: vec!["é", "A", "A"],
            properties: vec![
                ("s", Value::String("é\0")),
                ("b", Value::Bool(true)),
                ("i", Value::Integer(i64::MIN)),
                ("f", Value::Float(bits)),
                ("e", Value::Empty),
                ("ss", Value::Strings(vec!["", "é"])),
                ("bb", Value::Bools(vec![false, true])),
                ("ii", Value::Integers(vec![i64::MAX, -1])),
                ("ff", Value::Floats(vec![bits, 0x8000_0000_0000_0000])),
            ],
            text: Some("body"),
            embedding: Some(("document", vec![finite, 0x8000_0000])),
        };
        for variant in 0..7 {
            let mut right = left.clone();
            match variant {
                0 => {
                    right.properties.reverse();
                    right.labels = vec!["A", "é"];
                }
                1 => right.properties.push(("f", Value::Float(bits))),
                2 => right.properties[3] = ("f", Value::Float(bits ^ 1)),
                3 => right.properties[4] = ("e", Value::Floats(vec![])),
                4 => right.text = None,
                5 => right.embedding = Some(("document", vec![finite ^ 1, 0x8000_0000])),
                _ => right.embedding = Some(("documenT", vec![finite, 0x8000_0000])),
            }
            let observation = observe(&left, &right)?;
            check(&left, &right, observation)
                .map_err(|e| format!("{e}; seed={seed}; variant={variant}"))?;
            let key = match variant {
                0 => 0,
                1 => 1,
                2 => 2,
                3 => 3,
                5 => 5,
                _ => 6,
            };
            coverage.hit(REQUIRED_COVERAGE[key]);
        }
        let left = Contents {
            relationship: Some(((1_u128 << 64) | 1, 2, "TYPE")),
            labels: vec![],
            properties: vec![],
            text: None,
            embedding: None,
        };
        let mut right = left.clone();
        right.relationship = Some((1, 2, "TYPE"));
        check(&left, &right, observe(&left, &right)?)?;
        coverage.hit(REQUIRED_COVERAGE[4]);
    }
    let fp = CanonicalFingerprint::new(1, 0).map_err(|e| e.to_string())?;
    if !matches!(
        compare_canonical_streams(
            &mut &[1_u8][..],
            fp,
            &mut &[1_u8][..],
            fp,
            &mut [0; 2],
            &mut || Err(CanonicalError::Cancelled)
        ),
        Err(CanonicalError::Cancelled)
    ) {
        return Err("PG2 cancellation was ignored".into());
    }
    coverage.hit(REQUIRED_COVERAGE[7]);
    Ok(())
}

#[test]
fn canonical_oracle_rejects_changed_observations() {
    let left = Contents {
        relationship: None,
        labels: vec!["A"],
        properties: vec![("x", Value::Float(0))],
        text: None,
        embedding: None,
    };
    let mut right = left.clone();
    assert!(check(&left, &right, observe(&left, &right).expect("control")).is_ok());
    for bad in [
        Observation {
            left_accepted: false,
            right_accepted: true,
            exact_equal: Some(true),
        },
        Observation {
            left_accepted: true,
            right_accepted: false,
            exact_equal: Some(true),
        },
        Observation {
            left_accepted: true,
            right_accepted: true,
            exact_equal: Some(false),
        },
    ] {
        assert!(
            check(&left, &right, bad)
                .expect_err("can fire")
                .contains("PG2 exact contents")
        );
    }
    right.properties[0] = ("x", Value::Float(0x8000_0000_0000_0000));
    assert!(
        check(
            &left,
            &right,
            Observation {
                left_accepted: true,
                right_accepted: true,
                exact_equal: Some(true)
            }
        )
        .expect_err("equal-hash shortcut detected")
        .contains("PG2 exact contents")
    );
    right.properties.push(("x", Value::Float(0)));
    assert!(
        check(
            &left,
            &right,
            observe(&left, &right).expect("duplicate control")
        )
        .is_ok()
    );
    assert!(
        check(
            &right,
            &left,
            observe(&right, &left).expect("left duplicate control")
        )
        .is_ok()
    );
}
