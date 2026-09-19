//! PG1: primitive graph-domain preservation, independent of engine types.

/// Raw input carried by the deterministic constructor probe.
#[derive(Clone, Copy, Debug)]
pub struct DomainInput {
    pub identity: u128,
    pub revision: u64,
    pub scalar_bits: u64,
    pub vector_bits: u32,
}

/// Actual values returned through the engine's public logical constructors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DomainObservation {
    pub node: Option<u128>,
    pub relationship: Option<u128>,
    pub store: Option<u128>,
    pub revision: Option<u64>,
    pub successor: Option<u64>,
    pub scalar_bits: Option<u64>,
    pub vector_accepted: bool,
}

/// Compares with primitive expectations. IEEE finiteness is checked by the
/// exponent field rather than the production floating-point predicate.
pub fn check(input: DomainInput, observed: DomainObservation) -> Result<(), String> {
    let identity = if input.identity == 0 {
        None
    } else {
        Some(input.identity)
    };
    let revision = if input.revision == 0 {
        None
    } else {
        Some(input.revision)
    };
    let successor = if input.revision == 0 || input.revision == u64::MAX {
        None
    } else {
        Some(input.revision + 1)
    };
    let expected = DomainObservation {
        node: identity,
        relationship: identity,
        store: identity,
        revision,
        successor,
        scalar_bits: Some(input.scalar_bits),
        vector_accepted: input.vector_bits & 0x7f80_0000 != 0x7f80_0000,
    };
    if expected == observed {
        Ok(())
    } else {
        Err(format!(
            "PG1 domain preservation input={input:?} expected={expected:?} observed={observed:?}"
        ))
    }
}
