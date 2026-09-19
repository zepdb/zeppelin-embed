//! Pure logical constructor probes. No I/O fault site or store mutation exists
//! at this seam; publication/recovery campaigns are added with those components.

use rand::RngCore;
use zeppelin_embed::property_graph::{
    GraphRevision, GraphVector, NodeId, PropertyData, PropertyValue, RelId, StoreInstanceId,
};
use zeppelin_embed_adversarial_oracle::property_graph::{DomainInput, DomainObservation, check};

use super::coverage::CoverageRegistry;

pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.domain.zero",
    "property-graph.domain.full-width",
    "property-graph.domain.maximum",
    "property-graph.domain.random-bits",
];

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let mut rng = super::test_support::seeded_rng("property_graph::domain_probe", seed);
    let fixed = [
        (
            REQUIRED_COVERAGE[0],
            DomainInput {
                identity: 0,
                revision: 0,
                scalar_bits: 0,
                vector_bits: 0,
            },
        ),
        (
            REQUIRED_COVERAGE[1],
            DomainInput {
                identity: (1_u128 << 64) + 1,
                revision: 1,
                scalar_bits: 0x8000_0000_0000_0000,
                vector_bits: 0x7f80_0000,
            },
        ),
        (
            REQUIRED_COVERAGE[2],
            DomainInput {
                identity: u128::MAX,
                revision: u64::MAX,
                scalar_bits: 0x7ff8_0000_0000_0042,
                vector_bits: 0x7fc0_0042,
            },
        ),
    ];
    for (key, input) in fixed.into_iter().chain((0..64).map(|_| {
        (
            REQUIRED_COVERAGE[3],
            DomainInput {
                identity: (u128::from(rng.next_u64()) << 64) | u128::from(rng.next_u64()),
                revision: rng.next_u64(),
                scalar_bits: rng.next_u64(),
                vector_bits: rng.next_u32(),
            },
        )
    })) {
        let scalar = PropertyValue::new(PropertyData::F64(f64::from_bits(input.scalar_bits)));
        let observation = DomainObservation {
            node: NodeId::new(input.identity).ok().map(NodeId::get),
            relationship: RelId::new(input.identity).ok().map(RelId::get),
            store: StoreInstanceId::new(input.identity)
                .ok()
                .map(StoreInstanceId::get),
            revision: GraphRevision::new(input.revision)
                .ok()
                .map(GraphRevision::get),
            successor: GraphRevision::new(input.revision)
                .and_then(GraphRevision::checked_next)
                .ok()
                .map(GraphRevision::get),
            scalar_bits: scalar.ok().and_then(|value| match value.data() {
                PropertyData::F64(value) => Some(value.to_bits()),
                _ => None,
            }),
            vector_accepted: GraphVector::new(&[f32::from_bits(input.vector_bits)], 1).is_ok(),
        };
        check(input, observation).map_err(|error| format!("{error}; seed={seed}; case={key}"))?;
        coverage.hit(key);
    }
    Ok(())
}

#[test]
fn primitive_oracle_rejects_changed_observations() {
    let input = DomainInput {
        identity: 0x10000000000000001,
        revision: 41,
        scalar_bits: 0x7ff8000000000042,
        vector_bits: 0xff800000,
    };
    let clean = DomainObservation {
        node: Some(0x10000000000000001),
        relationship: Some(0x10000000000000001),
        store: Some(0x10000000000000001),
        revision: Some(41),
        successor: Some(42),
        scalar_bits: Some(0x7ff8000000000042),
        vector_accepted: false,
    };
    assert!(check(input, clean).is_ok());
    for corrupt in [
        DomainObservation {
            node: Some(1),
            ..clean
        },
        DomainObservation {
            relationship: Some(1),
            ..clean
        },
        DomainObservation {
            store: Some(1),
            ..clean
        },
        DomainObservation {
            revision: Some(40),
            ..clean
        },
        DomainObservation {
            successor: Some(41),
            ..clean
        },
        DomainObservation {
            scalar_bits: Some(0x7ff8000000000000),
            ..clean
        },
        DomainObservation {
            vector_accepted: true,
            ..clean
        },
    ] {
        assert!(
            check(input, corrupt)
                .unwrap_err()
                .contains("PG1 domain preservation")
        );
    }
}
