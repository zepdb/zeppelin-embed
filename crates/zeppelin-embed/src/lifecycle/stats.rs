//! Exact engine-owned memory and kernel-backed process statistics.

use std::ops::Deref;
use std::sync::{Arc, Mutex};

use super::budget::Budgets;
use super::{Store, StoreError, StoreState};

#[allow(dead_code)]
#[derive(Clone, Copy)]
pub(crate) enum AllocationComponent {
    Snapshot,
    Wal,
    Cache,
    Temporary,
}

impl AllocationComponent {
    const fn name(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::Wal => "wal",
            Self::Cache => "cache",
            Self::Temporary => "temporary",
        }
    }

    const fn is_temporary(self) -> bool {
        matches!(self, Self::Temporary)
    }
}

#[derive(Default)]
struct AccountingState {
    resident_owned_bytes: u64,
    wal_bytes: u64,
    cache_bytes: u64,
    temporary_bytes: u64,
    snapshot_bytes: u64,
    mapped_bytes: u64,
}

pub(crate) struct Accounting {
    budgets: Budgets,
    state: Mutex<AccountingState>,
}

impl Accounting {
    pub(crate) const fn new(max_resident_bytes: u64, max_temp_bytes: u64) -> Self {
        Self {
            budgets: Budgets::new(max_resident_bytes, max_temp_bytes),
            state: Mutex::new(AccountingState {
                resident_owned_bytes: 0,
                wal_bytes: 0,
                cache_bytes: 0,
                temporary_bytes: 0,
                snapshot_bytes: 0,
                mapped_bytes: 0,
            }),
        }
    }

    fn reserve(
        self: &Arc<Self>,
        bytes: u64,
        component: AllocationComponent,
    ) -> Result<Reservation, StoreError> {
        self.add(bytes, component)?;
        Ok(Reservation {
            accounting: Arc::clone(self),
            bytes,
            component,
        })
    }

    fn add(&self, bytes: u64, component: AllocationComponent) -> Result<(), StoreError> {
        let mut state = self.state.lock().map_err(|_| StoreError::Synchronization {
            component: "memory accounting",
        })?;
        let (resident, temporary) = self.budgets.check(
            state.resident_owned_bytes,
            state.temporary_bytes,
            bytes,
            component.name(),
            component.is_temporary(),
        )?;
        state.resident_owned_bytes = resident;
        match component {
            AllocationComponent::Snapshot => {
                state.snapshot_bytes = state.snapshot_bytes.saturating_add(bytes);
            }
            AllocationComponent::Wal => state.wal_bytes = state.wal_bytes.saturating_add(bytes),
            AllocationComponent::Cache => {
                state.cache_bytes = state.cache_bytes.saturating_add(bytes);
            }
            AllocationComponent::Temporary => state.temporary_bytes = temporary,
        }
        drop(state);
        Ok(())
    }

    pub(crate) fn audit(&self) -> Result<AccountingAudit, StoreError> {
        let state = self.state.lock().map_err(|_| StoreError::Synchronization {
            component: "memory accounting",
        })?;
        Ok(AccountingAudit {
            resident_owned_bytes: state.resident_owned_bytes,
            wal_bytes: state.wal_bytes,
            cache_bytes: state.cache_bytes,
            temporary_bytes: state.temporary_bytes,
            snapshot_bytes: state.snapshot_bytes,
            mapped_bytes: state.mapped_bytes,
        })
    }

    pub(crate) fn track_mapping(
        self: &Arc<Self>,
        bytes: u64,
    ) -> Result<MappingReservation, StoreError> {
        let mut state = self.state.lock().map_err(|_| StoreError::Synchronization {
            component: "memory accounting",
        })?;
        state.mapped_bytes =
            state
                .mapped_bytes
                .checked_add(bytes)
                .ok_or_else(|| StoreError::Statistics {
                    component: "mapped bytes",
                    source: std::io::Error::other("mapped byte count overflow"),
                })?;
        drop(state);
        Ok(MappingReservation {
            accounting: Arc::clone(self),
            bytes,
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct AccountingAudit {
    pub(crate) resident_owned_bytes: u64,
    pub(crate) wal_bytes: u64,
    pub(crate) cache_bytes: u64,
    pub(crate) temporary_bytes: u64,
    pub(crate) snapshot_bytes: u64,
    pub(crate) mapped_bytes: u64,
}

impl AccountingAudit {
    pub(crate) const fn component_sum(self) -> u64 {
        self.wal_bytes
            .saturating_add(self.cache_bytes)
            .saturating_add(self.temporary_bytes)
            .saturating_add(self.snapshot_bytes)
    }
}

pub(crate) struct MappingReservation {
    accounting: Arc<Accounting>,
    bytes: u64,
}

impl Drop for MappingReservation {
    fn drop(&mut self) {
        let mut state = match self.accounting.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.mapped_bytes = state.mapped_bytes.saturating_sub(self.bytes);
    }
}

struct Reservation {
    accounting: Arc<Accounting>,
    bytes: u64,
    component: AllocationComponent,
}

impl Reservation {
    fn grow(&mut self, additional_bytes: u64) -> Result<(), StoreError> {
        self.accounting.add(additional_bytes, self.component)?;
        self.bytes =
            self.bytes
                .checked_add(additional_bytes)
                .ok_or(StoreError::BudgetExceeded {
                    needed: u64::MAX,
                    budget: u64::MAX,
                    component: self.component.name(),
                })?;
        Ok(())
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut state = match self.accounting.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.resident_owned_bytes = state.resident_owned_bytes.saturating_sub(self.bytes);
        match self.component {
            AllocationComponent::Snapshot => {
                state.snapshot_bytes = state.snapshot_bytes.saturating_sub(self.bytes);
            }
            AllocationComponent::Wal => {
                state.wal_bytes = state.wal_bytes.saturating_sub(self.bytes);
            }
            AllocationComponent::Cache => {
                state.cache_bytes = state.cache_bytes.saturating_sub(self.bytes);
            }
            AllocationComponent::Temporary => {
                state.temporary_bytes = state.temporary_bytes.saturating_sub(self.bytes);
            }
        }
    }
}

/// An owned value whose exact requested allocation bytes have one component owner.
pub(crate) struct Accounted<T> {
    value: T,
    _reservation: Option<Reservation>,
    element_limit: Option<usize>,
}

impl<T> Deref for Accounted<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> Accounted<Vec<T>> {
    pub(crate) fn unaccounted_empty() -> Self {
        Self {
            value: Vec::new(),
            _reservation: None,
            element_limit: None,
        }
    }

    pub(crate) fn try_with_capacity(
        accounting: &Arc<Accounting>,
        capacity: usize,
        component: AllocationComponent,
    ) -> Result<Self, StoreError> {
        let requested =
            capacity
                .checked_mul(std::mem::size_of::<T>())
                .ok_or(StoreError::BudgetExceeded {
                    needed: u64::MAX,
                    budget: u64::MAX,
                    component: component.name(),
                })?;
        let bytes = u64::try_from(requested).map_err(|_| StoreError::BudgetExceeded {
            needed: u64::MAX,
            budget: u64::MAX,
            component: component.name(),
        })?;
        let reservation = accounting.reserve(bytes, component)?;
        let mut value = Vec::new();
        #[cfg(feature = "allocation-audit")]
        let allocation = crate::allocation_audit::attributed(|| value.try_reserve_exact(capacity));
        #[cfg(not(feature = "allocation-audit"))]
        let allocation = value.try_reserve_exact(capacity);
        allocation.map_err(|_| StoreError::AllocationFailed {
            needed: bytes,
            component: component.name(),
        })?;
        Ok(Self {
            value,
            _reservation: Some(reservation),
            element_limit: Some(capacity),
        })
    }

    pub(crate) fn push(&mut self, value: T) -> Result<(), StoreError> {
        let accounted_elements = self.element_limit.unwrap_or(usize::MAX);
        if self.value.len() >= accounted_elements || self.value.len() == self.value.capacity() {
            return Err(StoreError::AllocationFailed {
                needed: u64::try_from(self.value.len().saturating_add(1)).unwrap_or(u64::MAX),
                component: "accounted vector capacity",
            });
        }
        self.value.push(value);
        Ok(())
    }

    pub(crate) fn reserve_additional_bytes(&mut self, additional: u64) -> Result<(), StoreError> {
        match self._reservation.as_mut() {
            Some(reservation) => reservation.grow(additional),
            None if additional == 0 => Ok(()),
            None => Err(StoreError::AllocationFailed {
                needed: additional,
                component: "unaccounted vector",
            }),
        }
    }
}

#[cfg(test)]
impl Accounted<Vec<u8>> {
    fn try_zeroed(
        accounting: &Arc<Accounting>,
        bytes: u64,
        component: AllocationComponent,
    ) -> Result<Self, StoreError> {
        let capacity = usize::try_from(bytes).map_err(|_| StoreError::BudgetExceeded {
            needed: bytes,
            budget: usize::MAX as u64,
            component: component.name(),
        })?;
        let mut accounted = Self::try_with_capacity(accounting, capacity, component)?;
        accounted.value.resize(capacity, 0);
        Ok(accounted)
    }
}

/// One point-in-time snapshot of resources owned by an open [`Store`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stats {
    /// Exact bytes in engine-owned anonymous allocation arenas and vectors.
    pub resident_owned_bytes: u64,
    /// Exact byte length of the store's live read-only mappings.
    pub mapped_bytes: u64,
    /// Bytes in resident mapped pages, counted by `mincore`.
    pub mapped_resident_bytes: u64,
    /// Exact bytes in immutable segment mappings.
    pub segment_bytes: u64,
    /// Exact number of in-memory tombstones. This is zero until task 10
    /// introduces tombstones, because no tombstone component exists yet.
    pub tombstone_count: u64,
    /// Exact bytes owned by tombstones. This is zero until task 10 introduces
    /// tombstones, because no tombstone allocation exists yet.
    pub tombstone_bytes: u64,
    /// Exact bytes retained by the store's in-memory WAL component. This is
    /// zero while [`Store`] has no WAL writer component.
    pub wal_bytes: u64,
    /// Exact bytes owned by caches. This is zero because no cache exists yet.
    pub cache_bytes: u64,
    /// Exact bytes in live, explicitly accounted temporary allocations.
    pub temporary_bytes: u64,
    /// Exact number of file descriptors retained by this store handle.
    pub open_files: u64,
    /// Darwin's `TASK_VM_INFO` physical-footprint kernel counter, or `None` on
    /// platforms where that counter does not exist.
    pub phys_footprint: Option<u64>,
}

impl Store {
    /// Returns exact accounting and kernel counters for this open handle.
    ///
    /// This operation never estimates. Engine-owned counters are maintained at
    /// allocation/mapping boundaries, mapped residency comes from `mincore`,
    /// and `phys_footprint` comes from Darwin `TASK_VM_INFO`.
    pub fn stats(&self) -> Result<Stats, StoreError> {
        let state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(StoreError::Closing),
            StoreState::Closed => return Err(StoreError::Closed),
        }
        let writer_lock = self
            .writer_lock
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "writer lock",
            })?;
        let open_files = u64::from(writer_lock.is_some());
        let accounting = self.accounting.audit()?;
        if accounting.resident_owned_bytes != accounting.component_sum() {
            return Err(StoreError::Statistics {
                component: "resident accounting conservation",
                source: std::io::Error::other(
                    "resident-owned total disagrees with component counters",
                ),
            });
        }
        let snapshot_guard = self
            .snapshot
            .read()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?;
        let snapshot = snapshot_guard.as_ref().ok_or(StoreError::Closed)?;
        let mut mapped_resident_bytes = 0_u64;
        for segment in snapshot.segments() {
            let resident =
                segment
                    .mapped_resident_bytes()
                    .map_err(|source| StoreError::Statistics {
                        component: "mapped resident bytes",
                        source,
                    })?;
            mapped_resident_bytes =
                mapped_resident_bytes.checked_add(resident).ok_or_else(|| {
                    StoreError::Statistics {
                        component: "mapped resident bytes",
                        source: std::io::Error::other("mapped resident byte count overflow"),
                    }
                })?;
        }
        #[cfg(target_os = "macos")]
        let phys_footprint =
            Some(
                crate::sys::darwin::phys_footprint().map_err(|error| StoreError::Statistics {
                    component: "phys_footprint",
                    source: std::io::Error::other(error),
                })?,
            );
        #[cfg(not(target_os = "macos"))]
        let phys_footprint = None;
        drop(snapshot_guard);
        drop(writer_lock);
        drop(state);
        Ok(Stats {
            resident_owned_bytes: accounting.resident_owned_bytes,
            mapped_bytes: accounting.mapped_bytes,
            mapped_resident_bytes,
            segment_bytes: accounting.mapped_bytes,
            tombstone_count: 0,
            tombstone_bytes: 0,
            wal_bytes: accounting.wal_bytes,
            cache_bytes: accounting.cache_bytes,
            temporary_bytes: accounting.temporary_bytes,
            open_files,
            phys_footprint,
        })
    }

    #[cfg(test)]
    fn allocate_bytes(
        &self,
        bytes: u64,
        component: AllocationComponent,
    ) -> Result<Accounted<Vec<u8>>, StoreError> {
        let state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(StoreError::Closing),
            StoreState::Closed => return Err(StoreError::Closed),
        }
        let allocation = Accounted::try_zeroed(&self.accounting, bytes, component)?;
        drop(state);
        Ok(allocation)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use tempfile::{TempDir, tempdir};

    use super::super::{OpenOptions, Store, StoreError};
    use super::{Accounted, Accounting, AllocationComponent};
    use crate::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
    use crate::manifest::Manifest;
    use crate::manifest::io::commit_manifest;
    use crate::meta::{AliveSet, ColumnStoreBuilder, Schema};
    use crate::quant::Bit4Factors;
    use crate::segment::SegmentId;
    use crate::segment::layout::RegionEntry;
    use crate::segment::reader::SegmentReader;
    use crate::segment::writer::{SegmentBuild, SegmentFactors, write_segment};
    use crate::vfs::StdVfs;

    #[test]
    fn stats_bytes_are_conserved() {
        let directory = tempdir().expect("store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
        let wal = store
            .allocate_bytes(37, AllocationComponent::Wal)
            .expect("accounted WAL bytes");
        let cache = store
            .allocate_bytes(19, AllocationComponent::Cache)
            .expect("accounted cache bytes");
        let temporary = store
            .allocate_bytes(11, AllocationComponent::Temporary)
            .expect("accounted temporary bytes");

        let stats = store.stats().expect("stats");
        let audit = store.accounting.audit().expect("accounting audit");
        assert_eq!(stats.wal_bytes, 37);
        assert_eq!(stats.cache_bytes, 19);
        assert_eq!(stats.temporary_bytes, 11);
        assert_eq!(stats.resident_owned_bytes, 67);
        assert_eq!(stats.resident_owned_bytes, audit.component_sum());
        assert_eq!(stats.resident_owned_bytes, audit.resident_owned_bytes);

        drop((wal, cache, temporary));
        assert_eq!(
            store.stats().expect("released stats").resident_owned_bytes,
            0
        );

        let published = published_store();
        let mapped = Store::open(published.path(), OpenOptions::default()).expect("mapped open");
        let snapshot = mapped.snapshot().expect("mapped snapshot");
        let segment = snapshot.segments().first().expect("fixture segment");
        let expected_snapshot_bytes =
            std::mem::size_of::<SegmentReader>() + std::mem::size_of_val(segment.directory());
        drop(snapshot);
        let mapped_stats = mapped.stats().expect("mapped stats");
        assert_eq!(
            mapped_stats.resident_owned_bytes,
            expected_snapshot_bytes as u64
        );
        assert_eq!(
            mapped_stats.resident_owned_bytes,
            mapped
                .accounting
                .audit()
                .expect("mapped accounting audit")
                .component_sum()
        );
        assert_eq!(mapped_stats.tombstone_count, 0);
        assert_eq!(mapped_stats.tombstone_bytes, 0);
        assert_eq!(mapped_stats.wal_bytes, 0);
        assert_eq!(mapped_stats.cache_bytes, 0);
        assert_eq!(mapped_stats.temporary_bytes, 0);
        assert_eq!(mapped_stats.open_files, 1);
        #[cfg(target_os = "macos")]
        assert!(mapped_stats.phys_footprint.is_some_and(|bytes| bytes > 0));

        mapped.close().expect("close mapped store");
        assert_eq!(
            mapped
                .accounting
                .audit()
                .expect("closed mapping accounting")
                .mapped_bytes,
            0,
            "close must release the exact mapped-byte accounting"
        );
        assert!(matches!(mapped.stats(), Err(StoreError::Closed)));
    }

    #[test]
    fn budget_exceeded_is_typed_and_pre_allocation() {
        let directory = tempdir().expect("store directory");
        let options = OpenOptions::new()
            .with_max_resident_bytes(64)
            .with_max_temp_bytes(32);
        let store = Store::open(directory.path(), options).expect("open");

        let rejected = store.allocate_bytes(40, AllocationComponent::Temporary);
        assert!(matches!(
            rejected,
            Err(StoreError::BudgetExceeded {
                needed: 40,
                budget: 32,
                component: "temporary",
            })
        ));
        let after_rejection = store.stats().expect("stats after rejection");
        assert_eq!(after_rejection.resident_owned_bytes, 0);
        assert_eq!(after_rejection.temporary_bytes, 0);

        let accepted = store
            .allocate_bytes(24, AllocationComponent::Temporary)
            .expect("smaller allocation on same handle");
        assert_eq!(accepted.len(), 24);
        assert_eq!(store.stats().expect("usable stats").temporary_bytes, 24);
        assert!(matches!(
            store.allocate_bytes(50, AllocationComponent::Wal),
            Err(StoreError::BudgetExceeded {
                needed: 74,
                budget: 64,
                component: "wal",
            })
        ));
        assert_eq!(
            store.stats().expect("resident rejection stats").wal_bytes,
            0
        );
        store.snapshot().expect("same handle remains usable");

        let published = published_store();
        let snapshot_bytes =
            std::mem::size_of::<SegmentReader>() + 6 * std::mem::size_of::<RegionEntry>();
        let budget =
            u64::try_from(snapshot_bytes.saturating_sub(1)).expect("snapshot budget fits u64");
        let needed = u64::try_from(snapshot_bytes).expect("snapshot bytes fit u64");
        let rejected_open = Store::open(
            published.path(),
            OpenOptions::new().with_max_resident_bytes(budget),
        );
        assert!(matches!(
            rejected_open,
            Err(StoreError::BudgetExceeded {
                needed: actual_needed,
                budget: actual_budget,
                component: "snapshot",
            }) if actual_needed == needed && actual_budget == budget
        ));
    }

    #[test]
    fn accounted_vector_rejects_growth_without_a_reservation() {
        let mut unaccounted = Accounted::<Vec<u8>>::unaccounted_empty();
        unaccounted
            .reserve_additional_bytes(0)
            .expect("zero bytes require no reservation");
        assert!(matches!(
            unaccounted.reserve_additional_bytes(1),
            Err(StoreError::AllocationFailed {
                needed: 1,
                component: "unaccounted vector",
            })
        ));

        let accounting = std::sync::Arc::new(Accounting::new(u64::MAX, u64::MAX));
        let mut fixed = Accounted::try_with_capacity(&accounting, 1, AllocationComponent::Wal)
            .expect("fixed-capacity vector");
        fixed.push(7_u8).expect("reserved element");
        assert!(matches!(
            fixed.push(8_u8),
            Err(StoreError::AllocationFailed {
                needed: 2,
                component: "accounted vector capacity",
            })
        ));
    }

    #[test]
    fn accounted_allocation_after_close_is_typed_closed() {
        let directory = tempdir().expect("store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
        store.close().expect("close");

        assert!(matches!(
            store.allocate_bytes(1, AllocationComponent::Wal),
            Err(StoreError::Closed)
        ));
    }

    #[cfg(feature = "allocation-audit")]
    #[test]
    fn nothing_allocates_outside_accounting() {
        let directory = tempdir().expect("store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("open");

        let (unaccounted, negative_control) =
            crate::allocation_audit::audit_engine_path(|| Vec::<u8>::with_capacity(13));
        assert_eq!(negative_control.attributed_bytes, 0);
        assert!(
            negative_control.unattributed_bytes >= 13,
            "the audit wrapper failed to detect an unattributed engine-path allocation"
        );
        drop(unaccounted);
        store.stats().expect("warm accounting synchronization");

        let (accounted, report) = crate::allocation_audit::audit_engine_path(|| {
            store.allocate_bytes(23, AllocationComponent::Wal)
        });
        let accounted = accounted.expect("accounted allocation");
        assert_eq!(report.unattributed_bytes, 0);
        assert_eq!(report.attributed_bytes, 23);
        assert_eq!(store.stats().expect("stats").wal_bytes, 23);
        drop(accounted);
    }

    fn published_store() -> TempDir {
        let directory = tempdir().expect("published store directory");
        let schema = Schema::new(Vec::new()).expect("timestamp-only schema");
        let mut columns = ColumnStoreBuilder::new(schema.clone());
        columns.push_row(17, &[]).expect("fixture row");
        let columns = columns.finish().expect("fixture columns");
        let alive = AliveSet::new(1);
        let id = SegmentId::new(0x0102_0304_0506, [0x09; 10]);
        let codes = [0x88_u8];
        let factors = [Bit4Factors::from_persisted(1.0, 1.0, 1.0)];
        let rescore = [0.0_f32, 0.0_f32];
        let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::Ordered)
            .expect("derived policy");
        let segment = write_segment(
            &StdVfs,
            directory.path(),
            SegmentBuild {
                id,
                scheme: 4,
                dims: 2,
                codes: &codes,
                factors: SegmentFactors::Bit4(&factors),
                rescore: &rescore,
                columns: &columns,
                alive: &alive,
            },
            policy,
        )
        .expect("fixture segment");
        commit_manifest(
            &StdVfs,
            directory.path(),
            &Manifest {
                generation: 1,
                log_seq: 0,
                segments: vec![segment],
                epochs: Vec::new(),
                schema,
            },
            policy,
        )
        .expect("fixture manifest");
        directory
    }
}
