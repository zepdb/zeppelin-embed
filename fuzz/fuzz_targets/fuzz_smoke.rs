#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use rand::RngCore;

#[path = "../../crates/zeppelin-embed/src/test_support.rs"]
mod test_support;

static SEEDED_DRAW: OnceLock<u64> = OnceLock::new();

fuzz_target!(|data: &[u8]| {
    let draw = SEEDED_DRAW.get_or_init(|| {
        let mut rng = test_support::seeded_rng("fuzz_smoke");
        rng.next_u64()
    });
    let mut accumulator = *draw;
    for lane in 0_u64..64 {
        accumulator = xxhash_rust::xxh3::xxh3_64_with_seed(data, accumulator ^ lane);
    }
    std::hint::black_box(accumulator);
});
