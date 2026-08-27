//! Monotonic time authority used by store-owned deadline checks.

use std::time::Instant;

/// A monotonic time source.
///
/// Production stores use [`SystemMonotonicClock`]. The trait is public so
/// deterministic test dependencies can advance time without sleeping.
pub trait MonotonicClock: Send + Sync {
    /// Returns the current monotonic instant.
    fn now(&self) -> Instant;
}

/// The standard-library monotonic clock used by ordinary stores.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemMonotonicClock;

impl MonotonicClock for SystemMonotonicClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A manually advanced monotonic clock for deterministic fault tests.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug)]
pub struct ManualMonotonicClock {
    base: Instant,
    offset: std::sync::Mutex<std::time::Duration>,
}

#[cfg(any(test, feature = "test-support"))]
impl ManualMonotonicClock {
    /// Creates a frozen clock at the current monotonic instant.
    #[must_use]
    pub fn new() -> Self {
        Self::starting_at(Instant::now())
    }

    /// Creates a frozen clock at an explicit monotonic instant.
    ///
    /// This supports deterministic tests whose logical clock predates other
    /// test setup without requiring a wall-clock sleep.
    #[must_use]
    pub fn starting_at(base: Instant) -> Self {
        Self {
            base,
            offset: std::sync::Mutex::new(std::time::Duration::ZERO),
        }
    }

    /// Advances the clock without sleeping.
    pub fn advance(&self, duration: std::time::Duration) {
        if let Ok(mut offset) = self.offset.lock() {
            *offset = offset.saturating_add(duration);
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Default for ManualMonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl MonotonicClock for ManualMonotonicClock {
    fn now(&self) -> Instant {
        let offset = self
            .offset
            .lock()
            .map_or(std::time::Duration::ZERO, |offset| *offset);
        self.base.checked_add(offset).unwrap_or(self.base)
    }
}
