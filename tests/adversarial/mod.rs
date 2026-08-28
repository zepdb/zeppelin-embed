#![allow(dead_code)]
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

pub mod artifacts;
pub mod campaign;
pub mod coverage;
pub mod fault_vfs;
pub mod fts;
pub mod ingest_retention;
pub mod metadata_filter_planner;
pub mod model;
pub mod oracle;
pub mod profiles;
pub mod program;
pub mod runner;
pub mod storage_durability;
pub mod vamana_graph;
pub mod vector_execution;

use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

use self::profiles::FaultProfile;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunMode {
    Deterministic,
    Chaos,
    Mixed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SeedAssignment {
    pub mode: RunMode,
    pub profile: FaultProfile,
}

#[must_use]
pub const fn effective_seed_assignment(
    mode: RunMode,
    forced_profile: Option<FaultProfile>,
    seed: u64,
) -> SeedAssignment {
    if let Some(profile) = forced_profile {
        return SeedAssignment {
            mode: if matches!(profile, FaultProfile::None) {
                RunMode::Deterministic
            } else {
                RunMode::Chaos
            },
            profile,
        };
    }
    match mode {
        RunMode::Deterministic => SeedAssignment {
            mode,
            profile: FaultProfile::None,
        },
        RunMode::Chaos => SeedAssignment {
            mode,
            profile: FaultProfile::IoErrors,
        },
        RunMode::Mixed => SeedAssignment {
            mode: RunMode::Chaos,
            profile: FaultProfile::DEFAULTS[(seed as usize) % FaultProfile::DEFAULTS.len()],
        },
    }
}

pub mod test_support {
    use super::*;

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
}
