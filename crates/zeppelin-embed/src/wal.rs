//! Write-ahead-log record framing and checked prefix replay.

pub mod header;
pub mod record;
pub mod replay;

/// Monotonic write-ahead-log sequence number.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct LogSeq(u64);

impl LogSeq {
    /// Creates a sequence number from its persisted integer.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the persisted integer.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}
