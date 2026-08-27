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
            other => Err(format!("invalid ZE_ADV_PROFILE={other:?}")),
        }
    }
}
