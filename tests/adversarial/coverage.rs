use std::collections::BTreeMap;

/// Exact product and fault paths that the 12-seed default smoke matrix must reach.
pub const REQUIRED_SMOKE_COVERAGE: &[&str] = &[
    #[cfg(feature = "graph-cypher")]
    "property-graph.response.aligned-owner",
    #[cfg(feature = "graph-cypher")]
    "property-graph.response.private-forged-stale",
    #[cfg(feature = "graph-cypher")]
    "property-graph.response.abort-cleanup",
    #[cfg(feature = "graph-cypher")]
    "property-graph.response.concurrent-single-owner",
    #[cfg(feature = "graph-cypher")]
    "property-graph.response.known-outcome",
    #[cfg(feature = "graph-cypher")]
    "property-graph.response.allocation.fire",
    #[cfg(feature = "graph-cypher")]
    "property-graph.response.cancel.fire",
    #[cfg(feature = "graph-cypher")]
    "property-graph.response.memory.fire",
    #[cfg(feature = "graph-cypher")]
    "property-graph.response.work.fire",
    #[cfg(feature = "graph-cypher")]
    "property-graph.response.registry.fire",
    #[cfg(feature = "graph-cypher")]
    "property-graph.response.same-seed-control",
    #[cfg(feature = "graph-cypher")]
    "property-graph.response.oracle.can-fire",
    "property-graph.completed.bits-and-bags",
    "property-graph.completed.full-id",
    "property-graph.completed.oracle.can-fire",
    "property-graph.completed.copy-limit.fire",
    "property-graph.completed.cancel.fire",
    "property-graph.completed.same-seed-control",
    "property-graph.completed.release",
    "property-graph.relational.order-bags",
    "property-graph.relational.collect",
    "property-graph.relational.eligible",
    "property-graph.relational.clock.sort",
    "property-graph.relational.clock.aggregate",
    "property-graph.relational.clock.eligibility",
    "property-graph.relational.same-seed-control",
    "property-graph.relational.release",
    "property-graph.adjacency.merge",
    "property-graph.adjacency.full-id",
    "property-graph.adjacency.model-paired",
    "property-graph.adjacency.oracle.can-fire",
    "property-graph.adjacency.corruption.fire",
    "property-graph.adjacency.cancel.fire",
    "property-graph.adjacency.budget.fire",
    "property-graph.adjacency.same-seed-control",
    // PG13 fixture comparator controls, not product I/O faults.
    "property-graph.fixture.missing-edge.fire",
    "property-graph.fixture.missing-edge.clean",
    "property-graph.fixture.narrowed-id.fire",
    "property-graph.fixture.narrowed-id.clean",
    "property-graph.fixture.resurrection.fire",
    "property-graph.fixture.resurrection.clean",
    "property-graph.fixture.multiplicity.fire",
    "property-graph.fixture.multiplicity.clean",
    "property-graph.binding.scope",
    "property-graph.binding.profile",
    "property-graph.binding.bits",
    "property-graph.binding.modes",
    "property-graph.binding.cancel.fire",
    "property-graph.binding.budget.fire",
    "property-graph.binding.same-seed-control",
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
    "property-graph.wal.prefix",
    "property-graph.wal.transition",
    "property-graph.wal.corruption.fire",
    "property-graph.wal.corruption.clean",
    "property-graph.wal.missing.fire",
    "property-graph.wal.missing.clean",
    "property-graph.wal.cancel.fire",
    "property-graph.wal.cancel.clean",
    "property-graph.wal.budget.fire",
    "property-graph.wal.budget.clean",
    "property-graph.artifact.clean",
    "property-graph.artifact.collision",
    "property-graph.artifact.entropy",
    "property-graph.artifact.before-create",
    "property-graph.artifact.after-create",
    "property-graph.artifact.torn",
    "property-graph.artifact.bit-flip",
    "property-graph.catalog.names",
    "property-graph.catalog.duplicates",
    "property-graph.catalog.high-waters",
    "property-graph.catalog.interpretation",
    "property-graph.catalog.store-identity",
    "property-graph.catalog.cancel",
    "property-graph.catalog.cancel.fire",
    "property-graph.catalog.cancel.clean",
    "property-graph.catalog.budget.fire",
    "property-graph.catalog.budget.clean",
    "property-graph.catalog.decode-error.fire",
    "property-graph.catalog.decode-error.clean",
    "property-graph.domain.zero",
    "property-graph.domain.full-width",
    "property-graph.domain.maximum",
    "property-graph.domain.random-bits",
    "property-graph.contents.order",
    "property-graph.contents.duplicate",
    "property-graph.contents.ieee-bits",
    "property-graph.contents.typed-empty",
    "property-graph.contents.full-width",
    "property-graph.contents.vector-bits",
    "property-graph.contents.forced-collision",
    "property-graph.contents.cancel",
    "property-graph.lifecycle.create",
    "property-graph.lifecycle.replay",
    "property-graph.lifecycle.conflict",
    "property-graph.lifecycle.stale",
    "property-graph.lifecycle.incarnation",
    "property-graph.lifecycle.delete",
    "property-graph.lifecycle.recreate",
    "property-graph.lifecycle.cypher",
    "property-graph.lifecycle.noop",
    "property-graph.lifecycle.overflow",
    "property-graph.lifecycle.duplicate",
    "property-graph.lifecycle.cancel",
    "property-graph.query.numeric",
    "property-graph.query.grouping",
    "property-graph.query.list",
    "property-graph.query.scope",
    "property-graph.query.clock.bytes",
    "property-graph.query.clock.list",
    "property-graph.query.clock.plan",
    "property-graph.query.same-seed-control",
    "property-graph.query.pattern.scope",
    "property-graph.query.pattern.metadata",
    "property-graph.query.clock.pattern",
    "property-graph.runtime.bags",
    "property-graph.runtime.counters",
    "property-graph.runtime.clock.pull",
    "property-graph.runtime.clock.final",
    "property-graph.runtime.same-seed-control",
    "property-graph.runtime.release",
    "op.open",
    "op.ingest",
    "op.epoch_mismatch_probe",
    "op.prepare_epoch_b",
    "op.switch_alias_to_b",
    "op.rollback_to_a",
    "op.drop_epoch_a",
    "op.rollback_dropped_a_probe",
    "op.upsert",
    "op.revise",
    "op.delete",
    "op.drop_partition",
    "op.purge",
    "op.seal",
    "op.maintain",
    "op.search",
    "op.filtered_search",
    "op.predicate_search",
    "op.hybrid_search",
    "op.deadline_probe",
    "op.fts_extras_probe",
    "op.stats",
    "op.close",
    "op.reopen",
    "op.crash",
    "crash.boundary.mid_wal_group",
    "crash.boundary.pre_manifest_rename",
    "crash.boundary.post_manifest_rename",
    "crash.boundary.mid_seal",
    "crash.boundary.mid_purge",
    "search.scan",
    "search.auto",
    "search.graph",
    "search.filtered_graph",
    "predicate.eq",
    "predicate.in",
    "predicate.range_two_sided",
    "predicate.range_half_open",
    "predicate.bool",
    "predicate.string",
    "predicate.exists",
    "predicate.is_null",
    "predicate.and",
    "predicate.or",
    "predicate.not",
    "fts.phrase",
    "fts.prefix",
    "fts.fuzzy",
    "fts.phonetic",
    "fts.snippet",
    "store.text_ingest",
    "store.lexical_search",
    "store.hybrid_search",
    "fault.profile.none",
    "fault.profile.io-errors",
    "fault.profile.content",
    "fault.profile.crash",
    "fault.profile.disk",
    "fault.profile.clock",
    "fault.profile.full",
    "fault.profile.random",
    "fault.site.open",
    "fault.site.read",
    "fault.site.write",
    "fault.site.append",
    "fault.site.sync",
    "fault.site.rename",
    "fault.site.list",
    "fault.site.delete",
    "fault.site.clock",
    "fault.mode.eio",
    "fault.mode.eacces",
    "fault.mode.enospc",
    "fault.mode.bit_flip",
    "fault.mode.torn_write",
    "fault.mode.truncate",
    "fault.mode.wrong_object",
    "fault.mode.misdirected_write",
    "fault.mode.zero_fill",
    "fault.mode.latency",
    "fault.mode.silent_drop",
    "fault.mode.post_commit_error",
    "fault.mode.second_opener_in_process",
    "fault.mode.spawn_in_flight",
    "fault.mode.cancel",
    "fault.mode.clock_jump",
    "fault.mode.clock_stall",
    "fault.mode.crash",
];

pub const REQUIRED_LAYERED_COVERAGE: &[&str] = &[
    "fault.layer.io",
    "fault.layer.content",
    "fault.layer.crash",
    "fault.layer.clock",
    "fault.layer.cancel",
    "fault.layer.busy",
    "fault.layer.count.2",
    "fault.layer.count.3",
    "fault.layered.io+feature",
    "fault.layered.content+feature",
    "fault.layered.crash+feature",
    "fault.layered.clock+feature",
    "fault.layered.cancel+feature",
    "fault.layered.busy+feature",
    "crash.after.bit_flip",
    "crash.after.torn_write",
    "crash.after.truncate",
    "crash.after.wrong_object",
    "crash.after.misdirected_write",
    "crash.after.zero_fill",
    "crash.after.silent_drop",
];

/// A mergeable, deterministic ledger of paths that genuinely ran.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CoverageRegistry {
    hits: BTreeMap<String, usize>,
}

impl CoverageRegistry {
    pub fn hit(&mut self, key: impl Into<String>) {
        let entry = self.hits.entry(key.into()).or_default();
        *entry = entry.saturating_add(1);
    }

    pub fn merge(&mut self, other: &Self) {
        for (key, count) in &other.hits {
            let entry = self.hits.entry(key.clone()).or_default();
            *entry = entry.saturating_add(*count);
        }
    }

    #[must_use]
    pub fn count(&self, key: &str) -> usize {
        self.hits.get(key).copied().unwrap_or(0)
    }

    #[must_use]
    pub fn missing_required_smoke(&self) -> Vec<&'static str> {
        REQUIRED_SMOKE_COVERAGE
            .iter()
            .copied()
            .filter(|key| self.count(key) == 0)
            .collect()
    }

    #[must_use]
    pub fn json(&self) -> String {
        let entries = self
            .hits
            .iter()
            .map(|(key, count)| format!("\"{}\":{count}", json_escape(key)))
            .collect::<Vec<_>>()
            .join(",");
        format!("{{{entries}}}\n")
    }
}

fn json_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
