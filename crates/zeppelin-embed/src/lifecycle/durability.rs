//! Explicit per-store and per-commit durability policy.

use crate::vfs::SyncKind;

/// Selects whether this store must make acknowledged writes durable on its own.
///
/// The default is [`Derived`](Self::Derived). In that mode [`CommitTier`] is
/// ignored and no synchronization primitive is issued at any tier. An
/// application crash or process kill does not lose acknowledged writes because
/// the operating-system page cache retains them and writes them out afterward.
/// A power cut or kernel panic can lose recently acknowledged writes. Recovery
/// truncates the log at the first record whose checksum fails, so the store
/// remains structurally valid: the failure is a missing recent tail, never
/// silently wrong data.
///
/// Selecting `Derived` asserts that another store is authoritative and this
/// store can be rebuilt from it. A caller for whom this store is the only copy
/// of the data must select [`Durable`](Self::Durable) explicitly.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DurabilityMode {
    /// The default: another store is authoritative, this store is rebuildable,
    /// and synchronization is skipped regardless of [`CommitTier`].
    #[default]
    Derived,
    /// This store is the source of truth and applies the selected [`CommitTier`].
    Durable,
    /// Participates in a host transaction; v1 returns a typed unsupported error.
    Attached,
}

/// Synchronization strength for one commit.
///
/// This setting has no effect unless [`DurabilityMode::Durable`] is selected.
/// In the default [`DurabilityMode::Derived`] mode it is ignored entirely and
/// no synchronization primitive is issued, including when this value is
/// [`Ordered`](Self::Ordered) or [`Durable`](Self::Durable).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CommitTier {
    /// Buffer writes without synchronization; the page cache covers an
    /// application crash, but not a power cut.
    None,
    /// Order earlier writes before later writes with the platform barrier.
    ///
    /// This default tier chooses a cheaper median than full durability on the
    /// measured development device, with a materially wider latency tail:
    /// barrier p95 varied 7.40x while full-sync p95 varied 1.02x. That spread
    /// is a device property, not buffering or timing behavior in this code.
    #[default]
    Ordered,
    /// Flush writes through to durable media with the platform full sync.
    /// Its measured 4 KiB median was 20.8x the ordered barrier median, but its
    /// latency distribution was substantially more stable on the same device.
    Durable,
}

/// An explicit synchronization decision at one protocol boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncRequirement {
    /// Deliberately issue no synchronization primitive.
    Skip,
    /// Issue the named synchronization primitive.
    Sync(SyncKind),
}

/// Validated synchronization decisions for one store and commit.
///
/// Data-file and directory synchronization remain separate questions even
/// when the current policy resolves them to the same requirement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurabilityPolicy {
    data_file: SyncRequirement,
    directory: SyncRequirement,
}

impl DurabilityPolicy {
    /// Resolves one mode/tier pair without compatibility fallback.
    pub const fn new(
        mode: DurabilityMode,
        tier: CommitTier,
    ) -> Result<Self, DurabilityPolicyError> {
        let requirement = match (mode, tier) {
            (DurabilityMode::Attached, _) => {
                return Err(DurabilityPolicyError::AttachedNotYetSupported);
            }
            (DurabilityMode::Derived, _) | (DurabilityMode::Durable, CommitTier::None) => {
                SyncRequirement::Skip
            }
            (DurabilityMode::Durable, CommitTier::Ordered) => {
                SyncRequirement::Sync(SyncKind::Barrier)
            }
            (DurabilityMode::Durable, CommitTier::Durable) => SyncRequirement::Sync(SyncKind::Full),
        };
        Ok(Self {
            data_file: requirement,
            directory: requirement,
        })
    }

    /// Returns the synchronization requirement for a written data file.
    #[must_use]
    pub const fn data_file_sync(self) -> SyncRequirement {
        self.data_file
    }

    /// Returns the synchronization requirement for its containing directory.
    #[must_use]
    pub const fn directory_sync(self) -> SyncRequirement {
        self.directory
    }
}

/// A durability policy could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurabilityPolicyError {
    /// Attached host transactions are reserved but are not implemented in v1.
    AttachedNotYetSupported,
}

impl std::fmt::Display for DurabilityPolicyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AttachedNotYetSupported => {
                formatter.write_str("attached durability mode is not yet supported")
            }
        }
    }
}

impl std::error::Error for DurabilityPolicyError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::expect_used)]
    fn default_policy_skips_every_synchronization() {
        let policy = DurabilityPolicy::new(DurabilityMode::default(), CommitTier::default())
            .expect("default policy");

        assert_eq!(policy.data_file_sync(), SyncRequirement::Skip);
        assert_eq!(policy.directory_sync(), SyncRequirement::Skip);
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn derived_and_durable_none_resolve_to_the_same_policy() {
        let durable_none = DurabilityPolicy::new(DurabilityMode::Durable, CommitTier::None)
            .expect("durable none policy");

        for tier in [CommitTier::None, CommitTier::Ordered, CommitTier::Durable] {
            let derived =
                DurabilityPolicy::new(DurabilityMode::Derived, tier).expect("derived policy");
            assert_eq!(
                derived, durable_none,
                "Derived with {tier:?} must match Durable with None"
            );
        }
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn maps_every_mode_and_tier_without_attached_fallback() {
        let cases = [
            (
                DurabilityMode::Derived,
                CommitTier::None,
                SyncRequirement::Skip,
            ),
            (
                DurabilityMode::Derived,
                CommitTier::Ordered,
                SyncRequirement::Skip,
            ),
            (
                DurabilityMode::Derived,
                CommitTier::Durable,
                SyncRequirement::Skip,
            ),
            (
                DurabilityMode::Durable,
                CommitTier::None,
                SyncRequirement::Skip,
            ),
            (
                DurabilityMode::Durable,
                CommitTier::Ordered,
                SyncRequirement::Sync(SyncKind::Barrier),
            ),
            (
                DurabilityMode::Durable,
                CommitTier::Durable,
                SyncRequirement::Sync(SyncKind::Full),
            ),
        ];
        for (mode, tier, expected) in cases {
            let policy = DurabilityPolicy::new(mode, tier).expect("supported policy");
            assert_eq!(
                (policy.data_file_sync(), policy.directory_sync()),
                (expected, expected),
                "policy mismatch for ({mode:?}, {tier:?})"
            );
        }
        for tier in [CommitTier::None, CommitTier::Ordered, CommitTier::Durable] {
            let error = DurabilityPolicy::new(DurabilityMode::Attached, tier)
                .expect_err("Attached must fail rather than fall back");
            assert_eq!(error, DurabilityPolicyError::AttachedNotYetSupported);
            assert_eq!(
                error.to_string(),
                "attached durability mode is not yet supported"
            );
        }
        assert_eq!(DurabilityMode::default(), DurabilityMode::Derived);
        assert_eq!(CommitTier::default(), CommitTier::Ordered);
    }
}
