//! Existing deterministic adversarial RNG derivation, shared by fixture tooling.
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

/// Stable test-only RNG derivation. Every adversarial draw goes through this seam.
#[must_use]
pub fn seeded_rng(name: &str, seed: u64) -> ChaCha8Rng {
    let mut derived = seed ^ 0x5eed_fa17_cafe_babe;
    for byte in name.bytes() {
        derived ^= u64::from(byte);
        derived = derived.wrapping_mul(0x0000_0100_0000_01b3);
        derived ^= derived.rotate_left(23);
    }
    ChaCha8Rng::seed_from_u64(derived)
}
