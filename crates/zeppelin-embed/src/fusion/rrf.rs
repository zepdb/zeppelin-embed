use super::RRF_K;

pub(crate) fn contribution(rank: usize) -> f64 {
    let one_based = rank.saturating_add(1) as f64;
    1.0 / (f64::from(RRF_K) + one_based)
}
