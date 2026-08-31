pub type Rate = u8;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum IoModeBias {
    #[default]
    Any,
    EnospcOnly,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Environment {
    pub io: Rate,
    pub io_mode: IoModeBias,
    pub content: Rate,
    pub crash: Rate,
    pub clock: Rate,
    pub cancel: Rate,
    pub busy: Rate,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FaultProfile {
    None,
    IoErrors,
    Content,
    Crash,
    Disk,
    Clock,
    Full,
    Random,
}

impl FaultProfile {
    pub const ALL: [Self; 8] = [
        Self::None,
        Self::IoErrors,
        Self::Content,
        Self::Crash,
        Self::Disk,
        Self::Clock,
        Self::Full,
        Self::Random,
    ];

    pub const DEFAULTS: [Self; 7] = [
        Self::None,
        Self::IoErrors,
        Self::Content,
        Self::Crash,
        Self::Disk,
        Self::Clock,
        Self::Full,
    ];

    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::IoErrors => "io-errors",
            Self::Content => "content",
            Self::Crash => "crash",
            Self::Disk => "disk",
            Self::Clock => "clock",
            Self::Full => "full",
            Self::Random => "random",
        }
    }

    #[must_use]
    pub fn from_env(value: &str) -> Self {
        Self::from_key(value).unwrap_or_else(|error| panic!("{error}"))
    }

    pub fn from_key(value: &str) -> Result<Self, String> {
        match value {
            "none" => Ok(Self::None),
            "io" | "io-errors" => Ok(Self::IoErrors),
            "content" => Ok(Self::Content),
            "crash" => Ok(Self::Crash),
            "disk" => Ok(Self::Disk),
            "clock" => Ok(Self::Clock),
            "full" => Ok(Self::Full),
            "random" => Ok(Self::Random),
            other => Err(format!("invalid ZE_ADV_PROFILE={other:?}")),
        }
    }
}

#[must_use]
pub const fn profile_for_seed(seed: u64) -> FaultProfile {
    match seed % 8 {
        0 => FaultProfile::None,
        1 => FaultProfile::IoErrors,
        2 => FaultProfile::Content,
        3 => FaultProfile::Crash,
        4 => FaultProfile::Disk,
        5 => FaultProfile::Clock,
        6 => FaultProfile::Full,
        _ => FaultProfile::Random,
    }
}

#[must_use]
pub fn environment_for_profile(profile: FaultProfile, seed: u64) -> Environment {
    match profile {
        FaultProfile::None => Environment::default(),
        FaultProfile::IoErrors => Environment {
            io: 64,
            ..Environment::default()
        },
        FaultProfile::Content => Environment {
            content: 64,
            ..Environment::default()
        },
        FaultProfile::Crash => Environment {
            crash: 64,
            ..Environment::default()
        },
        FaultProfile::Disk => Environment {
            io: 64,
            io_mode: IoModeBias::EnospcOnly,
            ..Environment::default()
        },
        FaultProfile::Clock => Environment {
            clock: 64,
            ..Environment::default()
        },
        FaultProfile::Full => Environment {
            io: 32,
            io_mode: IoModeBias::Any,
            content: 32,
            crash: 32,
            clock: 16,
            cancel: 32,
            busy: 16,
        },
        FaultProfile::Random => {
            let mut rng = test_support::seeded_rng("adversarial::environment", seed);
            Environment {
                io: rng.random_range(0..=96),
                io_mode: IoModeBias::Any,
                content: rng.random_range(0..=96),
                crash: rng.random_range(0..=96),
                clock: rng.random_range(0..=96),
                cancel: rng.random_range(0..=96),
                busy: rng.random_range(0..=96),
            }
        }
    }
}
use rand::Rng;

use super::test_support;
