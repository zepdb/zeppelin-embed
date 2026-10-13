//! Policy and bounded orchestration; reclamation itself remains in maintenance.
use super::NativeGraphError;
use crate::lifecycle::{QueryControl, Store, StoreError};
use crate::property_graph::{GraphMaintenancePolicy, GraphMaintenanceReport};
use std::sync::atomic::Ordering;

// Half the 64-manifest fold capacity leaves room for maintenance publications.
pub(super) const RECLAIM_AFTER_COMMITS: u64 = 32;

// Intent, completion and retirement may each require a checkpoint admission
// before publication. Stop at the first completed cycle, never start a second.
const WRITE_RECOVERY_STEPS: usize = 6;

pub(super) enum WriteMaintenance {
    Fold(u64),
    Reclaim,
}

impl Store {
    pub(super) fn maintain_native_graph_for_write(
        &self,
        control: &QueryControl,
        action: WriteMaintenance,
        recovered: &mut bool,
    ) -> Result<(), NativeGraphError> {
        if let WriteMaintenance::Fold(work) = action {
            match self.auto_maintain_native_graph(control, work) {
                // Only the bounded inventory fold's work refusal is reclaim
                // debt. An ordinary write's Work error never reaches here.
                Err(NativeGraphError::Read(
                    crate::property_graph::storage::tree::directory::TreeError::Work,
                )) if !*recovered => {}
                result => return result,
            }
        } else if *recovered {
            return Err(crate::property_graph::storage::tree::directory::TreeError::Memory.into());
        }
        *recovered = true;
        // The refused attempt and its writer guard have already been dropped.
        // Explicit maintenance supplies each step's own limits and locking.
        for _ in 0..WRITE_RECOVERY_STEPS {
            if self.maintain_native_graph_step(control)?.cycle_complete {
                return Ok(());
            }
        }
        Err(NativeGraphError::Invalid(
            "graph write recovery exceeded six steps",
        ))
    }

    // Caller holds the native writer lock after successful fold or retirement.
    pub(super) fn reset_native_graph_maintenance_due(&self) {
        self.native_graph
            .commits_since_reclaim
            .store(0, Ordering::Relaxed);
        self.native_graph
            .pack_bytes_since_reclaim
            .store(0, Ordering::Relaxed);
    }

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
        self.maintain_native_graph_step_with_budget(control, None)
    }

    pub(crate) fn maintain_native_graph_step_budgeted(
        &self,
        control: &QueryControl,
        budget: &super::maintenance::NativeMaintenanceBudget,
    ) -> Result<GraphMaintenanceReport, NativeGraphError> {
        self.maintain_native_graph_step_with_budget(control, Some(budget))
    }

    fn maintain_native_graph_step_with_budget(
        &self,
        control: &QueryControl,
        budget: Option<&super::maintenance::NativeMaintenanceBudget>,
    ) -> Result<GraphMaintenanceReport, NativeGraphError> {
        let admission = self.admit_native_graph_maintenance()?;
        #[allow(unused_mut)]
        let mut limits = super::maintenance::MaintenanceLimits::default();
        #[cfg(test)]
        if PARTIAL_FOLD.with(std::cell::Cell::get) {
            limits.inventory_additions = 1;
        }
        let result = if let Some(budget) = budget {
            super::maintenance::commit_with_limits(self, &admission, control, limits, Some(budget))
        } else {
            #[cfg(not(test))]
            {
                self.commit_native_graph_maintenance(&admission, control)
            }
            #[cfg(test)]
            {
                self.commit_native_graph_maintenance_with_limits(&admission, control, limits)
            }
        };
        let current = self.admit_native_read()?;
        // Retirement first checkpoints the completion and asks for a fresh
        // admission. That is one bounded, observable step, not a failed cycle.
        // Only recognize an actual checkpoint at the same logical generation;
        // a stale concurrent mutation remains an error.
        if matches!(result, Err(NativeGraphError::StalePreparation))
            && current.bundle().base().generation == admission.lease.bundle().base().generation
            && current.bundle().base().fold != admission.lease.bundle().base().fold
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
        Ok(total)
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
        work: u64,
    ) -> Result<(), NativeGraphError> {
        if self.native_graph_maintenance_due()? {
            // Writes fold a bounded inventory prefix. Reclamation's full-root
            // mark belongs to explicit maintenance, not to a fixed-size write.
            let admission = self.admit_native_graph_maintenance()?;
            #[allow(unused_mut)]
            let mut limits = super::maintenance::MaintenanceLimits {
                reclaim: false,
                relocation_bytes: 0,
                sweep_limit: 0,
                work: Some(work),
                ..super::maintenance::MaintenanceLimits::default()
            };
            #[cfg(test)]
            if PARTIAL_FOLD.with(std::cell::Cell::get) {
                limits.inventory_additions = 1;
            }
            match super::maintenance::commit_with_limits(self, &admission, control, limits, None) {
                Ok(_) | Err(NativeGraphError::StalePreparation) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

// The existing incomplete-retirement fault requires a partially folded manifest.
#[cfg(test)]
thread_local! { pub(crate) static PARTIAL_FOLD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
