use std::collections::BTreeMap;

/// Exact product and fault paths that the 12-seed default smoke matrix must reach.
pub const REQUIRED_SMOKE_COVERAGE: &[&str] = &[
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
