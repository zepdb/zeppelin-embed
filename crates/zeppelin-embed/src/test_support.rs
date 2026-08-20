//! Deterministic support for randomized tests and fuzz smoke targets.

use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

/// Creates a deterministic RNG from a test name and `ZE_TEST_SEED`.
///
/// The test harness captures the printed seed on success and exposes it when a
/// test fails, making the exact run reproducible.
pub(crate) fn seeded_rng(name: &str) -> ChaCha8Rng {
    let environment_seed = std::env::var("ZE_TEST_SEED").unwrap_or_else(|_| String::from("0"));
    seeded_rng_from(name, &environment_seed)
}

pub(crate) fn seeded_rng_from(name: &str, environment_seed: &str) -> ChaCha8Rng {
    let material = format!("{name}\0{environment_seed}");
    let mut seed = [0_u8; 32];

    for (lane, chunk) in seed.chunks_exact_mut(8).enumerate() {
        let lane_seed = u64::try_from(lane).unwrap_or_default();
        let hash = xxhash_rust::xxh3::xxh3_64_with_seed(material.as_bytes(), lane_seed);
        chunk.copy_from_slice(&hash.to_le_bytes());
    }

    eprintln!(
        "deterministic test seed: name={name:?} ZE_TEST_SEED={environment_seed:?} derived={seed:02x?}"
    );
    ChaCha8Rng::from_seed(seed)
}
