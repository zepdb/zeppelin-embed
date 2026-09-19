use std::collections::BTreeMap;

/// Exact product and fault paths that the 12-seed default smoke matrix must reach.
pub const REQUIRED_SMOKE_COVERAGE: &[&str] = &[
    "property-graph.staging.changed",
    "property-graph.staging.replay",
    "property-graph.staging.mixed",
    "property-graph.staging.noop",
    "property-graph.staging.duplicate",
    "property-graph.staging.rejected",
    "property-graph.staging.cancel.fire",
    "property-graph.staging.cancel.clean",
    "property-graph.staging.budget.fire",
    "property-graph.staging.budget.clean",
    "property-graph.staging.oracle.can-fire",
    "property-graph.binding.scope",
    "property-graph.binding.profile",
    "property-graph.binding.bits",
    "property-graph.binding.modes",
    "property-graph.binding.cancel.fire",
    "property-graph.binding.budget.fire",
    "property-graph.binding.same-seed-control",
