//! Primitive, independently worked C-boundary expectations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observed {
    pub scalar: i64,
    pub tags: Vec<u32>,
    pub admitted: u64,
    pub changed: u64,
    pub disposition: u32,
}
pub fn expected_scalar(seed: u64) -> i64 {
    (seed % 31) as i64 + 17
}
pub fn compare(seed: u64, observed: &Observed) -> Result<(), String> {
    if observed.scalar != expected_scalar(seed)
        || observed.tags != [5, 0, 5]
        || observed.admitted != 1
        || observed.changed != 1
        || observed.disposition != 2
    {
        return Err(format!(
            "graph.c-entry.v1: seed {seed}, observed {observed:?}"
        ));
    }
    Ok(())
}
