#![no_main]

use libfuzzer_sys::fuzz_target;
use zeppelin_embed::graph::search::{GraphSearchProfile, GraphSearchRequest};
use zeppelin_embed::quant::{RescoreMetric, RescorePool, rescore_top_k};

fuzz_target!(|data: &[u8]| {
    let byte = |index: usize| data.get(index).copied().unwrap_or(0);
    let row_count = usize::from(byte(0) % 16) + 1;
    let dimensions = usize::from(byte(1) % 8) + 1;
    let k = usize::from(byte(2) % u8::try_from(row_count).unwrap_or(1)) + 1;
    let profile = if byte(3).is_multiple_of(2) {
        GraphSearchProfile::SiftClass
    } else {
        GraphSearchProfile::Angular
    };
    let query = vec![0.0_f32; dimensions];
    let adaptive = GraphSearchRequest::new(&query, k, u64::from(byte(4))).with_profile(profile);
    let ef = adaptive
        .effective_ef(row_count)
        .expect("bounded valid adaptive shape must resolve");
    assert!(ef >= k && ef <= row_count);
    let explicit_ef = usize::from(byte(5)) % row_count.saturating_add(1);
    let explicit = adaptive.with_ef(explicit_ef).effective_ef(row_count);
    assert_eq!(explicit.is_ok(), explicit_ef >= k);

    let pool_count = usize::from(byte(6) % 16) + 1;
    let result_k = usize::from(byte(7) % u8::try_from(pool_count).unwrap_or(1)) + 1;
    let rows = (0..row_count * dimensions)
        .map(|index| f32::from(byte(8 + index)) / 17.0)
        .collect::<Vec<_>>();
    let mut row_indices = (0..pool_count)
        .map(|position| u32::try_from(position % row_count).unwrap_or(0))
        .collect::<Vec<_>>();
    let inject_invalid_row = byte(8 + rows.len()).is_multiple_of(7);
    if inject_invalid_row {
        if let Some(first) = row_indices.first_mut() {
            *first = u32::MAX;
        }
    }
    let coarse_scores = (0..pool_count)
        .map(|position| -f32::from(byte(9 + rows.len() + position)))
        .collect::<Vec<_>>();
    let pool = RescorePool::retained(
        &row_indices,
        &coarse_scores,
        RescoreMetric::SquaredL2,
        pool_count,
        dimensions,
    );
    let result = rescore_top_k(&query, &rows, dimensions, pool, result_k);
    if inject_invalid_row {
        assert!(result.is_err());
    } else {
        let result = result.expect("bounded retained pool must rescore");
        assert_eq!(result.hits.len(), result_k);
        assert_eq!(result.candidates_rescored, pool_count);
    }
});
