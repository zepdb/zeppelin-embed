use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

pub fn seeded_rng(name: &str) -> ChaCha8Rng {
    let environment_seed = std::env::var("ZE_TEST_SEED").unwrap_or_else(|_| String::from("0"));
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
