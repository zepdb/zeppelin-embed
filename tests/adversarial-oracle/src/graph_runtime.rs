//! PG9: independent primitive bag and known-work execution model. No engine
//! values, allocation guards, batch representation or plan validator is imported.
use std::collections::BTreeMap;
#[derive(Debug, Clone)]
pub struct Observation {
    pub output: Vec<i64>,
    pub examined: u64,
    pub source_rows: u64,
    pub collected_rows: u64,
    pub prepared_bytes: u64,
    pub core_bytes: u64,
    pub copied_bytes: u64,
}
fn bag(values: &[i64]) -> BTreeMap<i64, usize> {
    let mut counts = BTreeMap::new();
    for value in values {
        *counts.entry(*value).or_insert(0) += 1;
    }
    counts
}
pub fn check(input: &[i64], observed: &Observation) -> Result<(), String> {
    let rows = input.len() as u64;
    // This fixture retains every i64 and performs three real eight-byte copies:
    // physical-source batch, private collection, then independent frozen output.
    if bag(input) != bag(&observed.output)
        || observed.examined != rows
        || observed.source_rows != rows
        || observed.collected_rows != rows
        || observed.prepared_bytes != rows * 8
        || observed.core_bytes != rows * 8
        || observed.copied_bytes != rows * 24
    {
        return Err(format!(
            "PG9 bag/work mismatch: input={input:?} observed={observed:?}"
        ));
    }
    Ok(())
}
pub fn check_failure(
    output_exists: bool,
    fault_fires: usize,
    expected_fires: usize,
    retained_delta: usize,
) -> Result<(), String> {
    if output_exists || fault_fires != expected_fires || retained_delta != 0 {
        return Err(format!(
            "PG9 failed execution exposed output or lost fault/release evidence: output={output_exists} fires={fault_fires}/{expected_fires} retained={retained_delta}"
        ));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn primitive_runtime_oracle_rejects_lost_duplicates_and_lying_counters() {
        let correct = Observation {
            output: vec![7, 7, -9],
            examined: 3,
            source_rows: 3,
            collected_rows: 3,
            prepared_bytes: 24,
            core_bytes: 24,
            copied_bytes: 72,
        };
        assert!(check(&[7, -9, 7], &correct).is_ok());
        let mut corrupt = correct.clone();
        corrupt.output.pop();
        assert!(check(&[7, -9, 7], &corrupt).is_err());
        corrupt = correct.clone();
        corrupt.examined -= 1;
        assert!(check(&[7, -9, 7], &corrupt).is_err());
        corrupt = correct.clone();
        corrupt.copied_bytes = 24;
        assert!(check(&[7, -9, 7], &corrupt).is_err());
        corrupt = correct;
        corrupt.core_bytes = 0;
        assert!(check(&[7, -9, 7], &corrupt).is_err());
        assert!(check_failure(true, 1, 1, 0).is_err());
        assert!(check_failure(false, 0, 1, 0).is_err());
        assert!(check_failure(false, 1, 1, 8).is_err());
    }
}
