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

/// Binding faults are specified from commit boundaries, not native responses.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindingOutcome {
    pub status: String,
    pub disposition: u32,
    pub changed: u64,
    pub fires: u64,
    pub recovered_nodes: i64,
}
pub fn compare_binding(mode: u32, value: &BindingOutcome) -> Result<(), String> {
    let (status, disposition, changed, recovered) = match mode {
        0 | 4 => ("ZeOk", 2, 1, 1),
        1 => ("ZeErrOutOfMemory", 1, 0, 0),
        // Entering WAL append is uncertain even if its callback refuses first.
        2 => ("ZeErrIndeterminateCommit", 5, 0, 0),
        3 => ("ZeErrIndeterminateCommit", 5, 0, 1),
        5 => ("ZeErrPanic", 5, 0, 1),
        6 => ("ZeErrPanic", 2, 1, 1),
        _ => return Err("ZE-72 unknown binding fault".into()),
    };
    if value.status != status
        || value.disposition != disposition
        || value.changed != changed
        || value.fires != u64::from(mode != 0)
        || value.recovered_nodes != recovered
    {
        Err(format!(
            "ZE-72 binding mode {mode}: {value:?}, expected {status}/{disposition}/{changed}/{recovered}"
        ))
    } else {
        Ok(())
    }
}

/// The bounded application fixture declares either no source or one vector
/// source (call zero). Projection and empty results must retain that source.
pub fn compare_reports(
    expected_count: usize,
    generation: u64,
    observed: &[(u32, u32, u64)],
) -> Result<(), String> {
    let expected = match expected_count {
        0 => Vec::new(),
        1 => vec![(0, 0, generation)],
        _ => return Err("ZE-72 report expectation outside bounded fixture".into()),
    };
    if observed == expected {
        Ok(())
    } else {
        Err(format!(
            "ZE-72 source provenance differs: expected {expected:?}, observed {observed:?}"
        ))
    }
}
