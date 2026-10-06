//! Policy and bounded orchestration; reclamation itself remains in maintenance.
use super::NativeGraphError;
use crate::lifecycle::{QueryControl, Store, StoreError};
use crate::property_graph::{GraphMaintenancePolicy, GraphMaintenanceReport};
use std::sync::atomic::Ordering;

// Half the 64-manifest fold capacity leaves room for maintenance publications.
pub(super) const RECLAIM_AFTER_COMMITS: u64 = 32;

const CYCLE_STEP_LIMIT: &str = "graph maintenance cycle exceeded four steps";

impl Store {
    pub(crate) fn set_native_graph_maintenance_policy(
        &self,
        policy: GraphMaintenancePolicy,
    ) -> Result<(), NativeGraphError> {
        self.native_graph.require_writable()?;
        if policy.reclaim_after_bytes < 1024 * 1024 {
            return Err(NativeGraphError::Stage(
                crate::property_graph::staging::StageError::InvalidInput,
            ));
        }
        *self.native_graph.maintenance_policy.lock().map_err(|_| {
            StoreError::Synchronization {
                component: "graph maintenance policy",
            }
        })? = policy;
        Ok(())
    }

    pub(crate) fn maintain_native_graph_step(
        &self,
        control: &QueryControl,
    ) -> Result<GraphMaintenanceReport, NativeGraphError> {
        let admission = self.admit_native_graph_maintenance()?;
        #[cfg(not(test))]
        let result = self.commit_native_graph_maintenance(&admission, control);
        #[cfg(test)]
        let result = {
            let mut limits = super::maintenance::MaintenanceLimits::default();
            if PARTIAL_FOLD.with(std::cell::Cell::get) {
                limits.inventory_additions = 1;
            }
            self.commit_native_graph_maintenance_with_limits(&admission, control, limits)
        };
        let current = self.admit_native_read()?;
        // Retirement first checkpoints the completion and asks for a fresh
        // admission. That is one bounded, observable step, not a failed cycle.
        // Only recognize an actual checkpoint at the same logical generation;
        // a stale concurrent mutation remains an error.
        if matches!(result, Err(NativeGraphError::StalePreparation))
            && current.bundle().base().generation == admission.lease.bundle().base().generation
            && current.bundle().root_envelope() != admission.lease.bundle().root_envelope()
        {
            return Ok(GraphMaintenanceReport {
                generation: current.bundle().base().generation,
                replaced_physical_refs: 0,
                new_pack_bytes: 0,
                relocated_bytes: 0,
                drained_packs: 0,
                reclaimed_bytes: 0,
                removed_bytes: 0,
                cycle_complete: false,
            });
        }
        let report = result?;
        let cycle_complete = current.bundle().reclaim().is_none();
        Ok(GraphMaintenanceReport {
            generation: report.generation,
            replaced_physical_refs: report.replaced_physical_refs,
            new_pack_bytes: report.new_pack_bytes,
            relocated_bytes: report.relocated_bytes,
            drained_packs: report.drained_packs,
            reclaimed_bytes: report.reclaimed_bytes,
            removed_bytes: report.removed_bytes,
            cycle_complete,
        })
    }

    pub(crate) fn maintain_native_graph_cycle(
        &self,
        control: &QueryControl,
    ) -> Result<GraphMaintenanceReport, NativeGraphError> {
        let mut total = self.maintain_native_graph_step(control)?;
        for _ in 1..4 {
            if total.cycle_complete {
                return Ok(total);
            }
            let step = self.maintain_native_graph_step(control)?;
            total.generation = step.generation;
            total.cycle_complete = step.cycle_complete;
            total.replaced_physical_refs += step.replaced_physical_refs;
            total.new_pack_bytes += step.new_pack_bytes;
            total.relocated_bytes += step.relocated_bytes;
            total.drained_packs += step.drained_packs;
            total.reclaimed_bytes += step.reclaimed_bytes;
            total.removed_bytes += step.removed_bytes;
        }
        if total.cycle_complete {
            Ok(total)
        } else {
            Err(NativeGraphError::Invalid(CYCLE_STEP_LIMIT))
        }
    }

    pub(super) fn native_graph_maintenance_due(&self) -> Result<bool, NativeGraphError> {
        let policy = *self.native_graph.maintenance_policy.lock().map_err(|_| {
            StoreError::Synchronization {
                component: "graph maintenance policy",
            }
        })?;
        Ok(policy.automatic
            && (self
                .native_graph
                .pack_bytes_since_reclaim
                .load(Ordering::Relaxed)
                >= policy.reclaim_after_bytes
                || self
                    .native_graph
                    .commits_since_reclaim
                    .load(Ordering::Relaxed)
                    >= RECLAIM_AFTER_COMMITS))
    }

    pub(super) fn auto_maintain_native_graph(
        &self,
        control: &QueryControl,
    ) -> Result<(), NativeGraphError> {
        if self.native_graph_maintenance_due()? {
            // One cycle per trigger. A cycle that needs more steps, or a
            // concurrent commit, never refuses the caller's write: the
            // counter stays above the trigger and the next write resumes.
            match self.maintain_native_graph_cycle(control) {
                Ok(_) | Err(NativeGraphError::StalePreparation) => {}
                Err(NativeGraphError::Invalid(CYCLE_STEP_LIMIT)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

// The existing incomplete-retirement fault requires a partially folded manifest.
#[cfg(test)]
thread_local! { pub(crate) static PARTIAL_FOLD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
