#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FaultProfile {
    None,
    IoErrors,
    Content,
    Crash,
    Disk,
    Clock,
    Full,
}

impl FaultProfile {
    pub const DEFAULTS: [Self; 5] = [
        Self::None,
        Self::IoErrors,
        Self::Content,
        Self::Crash,
        Self::Disk,
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
        }
    }

    #[must_use]
    pub fn from_env(value: &str) -> Self {
        match value {
            "none" => Self::None,
            "io" | "io-errors" => Self::IoErrors,
            "content" => Self::Content,
            "crash" => Self::Crash,
            "disk" => Self::Disk,
            "clock" => Self::Clock,
            "full" => Self::Full,
            other => panic!("invalid ZE_ADV_PROFILE={other:?}"),
        }
    }
}
