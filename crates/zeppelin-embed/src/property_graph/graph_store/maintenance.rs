//! Bounded public graph maintenance and its write-path policy.
use super::GraphStoreError;
use crate::lifecycle::QueryControl;
use crate::property_graph::GraphGeneration;

/// Automatic reclamation policy for this open writer (not persisted).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphMaintenancePolicy {
    /// Run maintenance before a write once the byte threshold or the internal
    /// 32-publication reclaim cadence is reached.
    pub automatic: bool,
    /// Committed artifact bytes between triggers; at least 1 MiB.
    pub reclaim_after_bytes: u64,
}
impl Default for GraphMaintenancePolicy {
    fn default() -> Self {
        Self {
            automatic: true,
            reclaim_after_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Work performed by one maintenance step, or accumulated over one cycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphMaintenanceReport {
    /// Last published generation.
    pub generation: GraphGeneration,
    /// Physical references replaced.
    pub replaced_physical_refs: u64,
    /// Artifact bytes written.
    pub new_pack_bytes: u64,
    /// Live bytes copied from selected packs.
    pub relocated_bytes: u64,
    /// Packs selected for draining.
    pub drained_packs: u32,
    /// Bytes covered by reclamation.
    pub reclaimed_bytes: u64,
    /// Bytes actually removed.
    pub removed_bytes: u64,
    /// This cycle finished; no reclaim intent, completion or checkpoint retry remains.
    pub cycle_complete: bool,
}

impl crate::lifecycle::Store {
    /// Sets this writer's policy.
    /// # Errors
    /// Refuses read-only stores and thresholds below 1 MiB.
    pub fn set_graph_maintenance_policy(
        &self,
        policy: GraphMaintenancePolicy,
    ) -> Result<(), GraphStoreError> {
        self.set_native_graph_maintenance_policy(policy)
            .map_err(Into::into)
    }
    /// Performs one bounded maintenance step.
    /// # Errors
    /// Returns the classified admission, preparation or storage failure.
    pub fn graph_maintain_step(
        &self,
        control: &QueryControl,
    ) -> Result<GraphMaintenanceReport, GraphStoreError> {
        self.maintain_native_graph_step(control).map_err(Into::into)
    }
    /// Runs at most four steps, stopping when the cycle completes.
    /// # Errors
    /// Returns the classified maintenance failure, including an exhausted step cap.
    pub fn graph_maintain_cycle(
        &self,
        control: &QueryControl,
    ) -> Result<GraphMaintenanceReport, GraphStoreError> {
        self.maintain_native_graph_cycle(control)
            .map_err(Into::into)
    }
}
