//! Unified authority fault operations, using the existing scheduled VFS.
use super::coverage::CoverageRegistry;
use super::storage_durability::release_fixture as fixture;

pub const REQUIRED: &[&str] = &[
    "storage-durability.graph-commit.enable",
    "storage-durability.graph-commit.artifact-write",
    "storage-durability.graph-commit.artifact-sync",
    "storage-durability.graph-commit.wal-append",
    "storage-durability.graph-commit.wal-sync",
    "storage-durability.graph-fold.manifest-rename",
    "storage-durability.mixed-batch.append-member-0",
    "storage-durability.mixed-batch.append-member-1",
    "storage-durability.mixed-batch.append-member-2",
    "storage-durability.mixed-batch.sync",
    "storage-durability.mixed-batch.comparator",
];

#[derive(Clone, Copy)]
pub enum Operation {
    EnableGraph,
    GraphApply,
    MixedBatch,
}

pub fn run(operation: Operation, seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    match operation {
        Operation::EnableGraph => enable_graph(seed, coverage),
        Operation::GraphApply => super::graph_recovery::probe_commit_boundaries(seed, coverage),
        Operation::MixedBatch => mixed_batch(seed, coverage),
    }
}

fn enable_graph(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    use super::fault_vfs::{FaultEvent, FaultMode, FaultSchedule, FaultSite, Layer, ScheduledVfs};
    use std::sync::Arc;
    use zeppelin_embed::lifecycle::{Store, StoreTestDependencies, SystemMonotonicClock};
    use zeppelin_embed::manifest::decode_manifest;
    use zeppelin_embed::vfs::StdVfs;
    let root = tempfile::tempdir().map_err(|error| error.to_string())?;
    for (ordinal, (site, needle)) in [
        (FaultSite::Write, ".zgraph"),
        (FaultSite::Sync, ".zgraph"),
        (FaultSite::Rename, "manifest.ze"),
    ]
    .into_iter()
    .enumerate()
    {
        for mode in [FaultMode::Eio, FaultMode::PostCommitError] {
            let path = root.path().join(format!("{seed}-{ordinal}-{mode:?}"));
            let event = FaultEvent {
                id: "unified-enable".into(),
                op_index: 1,
                layer: Layer::Io,
                site,
                mode,
                nth_match: 1,
                expected_matches: None,
                deadline_budget_seconds: None,
                path_contains: Some(needle.into()),
                fired: false,
                fire_count: 0,
                path: None,
            };
            let vfs = Arc::new(ScheduledVfs::new(StdVfs, FaultSchedule::single(event)));
            let store = Store::open_with_test_dependencies(
                &path,
                fixture::options(false),
                StoreTestDependencies::new(vfs.clone(), Arc::new(SystemMonotonicClock)),
            )
            .map_err(|error| error.to_string())?;
            vfs.set_operation(1);
            store.enable_graph().expect_err("enable fault must refuse");
            let events = vfs.events();
            if events.len() != 1 || events[0].fire_count != 1 || !events[0].fired {
                return Err("enable graph fault did not fire exactly once".into());
            }
            drop(store);
            let manifest_path = path.join("manifest.ze");
            let bytes = std::fs::read(&manifest_path).map_err(|error| error.to_string())?;
            let manifest =
                decode_manifest("enable fault", &bytes).map_err(|error| error.to_string())?;
            let committed = site == FaultSite::Rename && mode == FaultMode::PostCommitError;
            if manifest.graph.is_some() != committed || manifest.generation != u64::from(committed)
            {
                return Err("enable graph crossed its manifest commit boundary".into());
            }
            let reader =
                Store::open(&path, fixture::options(true)).map_err(|error| error.to_string())?;
            if reader
                .count_documents(None, None)
                .map_err(|error| error.to_string())?
                .count
                != 0
            {
                return Err("enable graph changed document population".into());
            }
            reader.close().map_err(|error| error.to_string())?;
            if std::fs::read(&manifest_path).map_err(|error| error.to_string())? != bytes {
                return Err("read-only recovery changed the manifest".into());
            }
            coverage.hit(REQUIRED[0]);
        }
    }
    let control = root.path().join("control");
    let store =
        Store::open(&control, fixture::options(false)).map_err(|error| error.to_string())?;
    if store.enable_graph().map_err(|error| error.to_string())? != 1 {
        return Err("enable control did not advance one Store generation".into());
    }
    store.close().map_err(|error| error.to_string())?;
    coverage.hit("op.EnableGraph");
    Ok(())
}

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    for operation in [
        Operation::EnableGraph,
        Operation::GraphApply,
        Operation::MixedBatch,
    ] {
        run(operation, seed, coverage)?;
    }
    Ok(())
}

fn mixed_batch(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    use super::fault_vfs::{FaultEvent, FaultMode, FaultSchedule, FaultSite, Layer, ScheduledVfs};
    use std::sync::Arc;
    use zeppelin_embed::graph_recovery_test_support::{apply_mixed_batch, mixed_node_present};
    use zeppelin_embed::lifecycle::{Store, StoreTestDependencies, SystemMonotonicClock};
    use zeppelin_embed::property_graph::NodeId;
    use zeppelin_embed::vfs::StdVfs;
    let root = tempfile::tempdir().map_err(|e| e.to_string())?;
    let batch = fixture::batch(vec![
        fixture::document(91, "mixed orchard"),
        fixture::document(92, "mixed harbor"),
    ]);
    let clean_path = root.path().join("control");
    let clean = Store::open(
        &clean_path,
        fixture::options(false)
            .with_durability(
                zeppelin_embed::lifecycle::durability::DurabilityMode::Durable,
                zeppelin_embed::lifecycle::durability::CommitTier::Durable,
            )
            .with_schema(fixture::schema()),
    )
    .map_err(|e| e.to_string())?;
    clean.enable_graph().map_err(|e| e.to_string())?;
    let (node, generation) = apply_mixed_batch(&clean, &batch, "first")?;
    if generation != 2
        || node != NodeId::new(1).map_err(|e| e.to_string())?
        || clean
            .count_documents(None, None)
            .map_err(|e| e.to_string())?
            .count
            != 2
        || !mixed_node_present(&clean, node)
    {
        return Err("mixed control differs from literal first-batch state".into());
    }
    drop(clean);
    for (site, nth, mode, key, present) in [
        (FaultSite::Append, 1, FaultMode::Eio, REQUIRED[6], false),
        (FaultSite::Append, 2, FaultMode::Eio, REQUIRED[7], false),
        (FaultSite::Append, 3, FaultMode::Eio, REQUIRED[8], false),
        (
            FaultSite::Append,
            3,
            FaultMode::PostCommitError,
            REQUIRED[8],
            true,
        ),
        (FaultSite::Sync, 1, FaultMode::Eio, REQUIRED[9], true),
    ] {
        let path = root.path().join(format!("{seed}-{site:?}-{nth}-{mode:?}"));
        let event = FaultEvent {
            id: "mixed-batch".into(),
            op_index: 1,
            layer: Layer::Io,
            site,
            mode,
            nth_match: nth,
            expected_matches: None,
            deadline_budget_seconds: None,
            path_contains: Some("wal.ze".into()),
            fired: false,
            fire_count: 0,
            path: None,
        };
        let vfs = Arc::new(ScheduledVfs::new(StdVfs, FaultSchedule::single(event)));
        let store = Store::open_with_test_dependencies(
            &path,
            fixture::options(false)
                .with_durability(
                    zeppelin_embed::lifecycle::durability::DurabilityMode::Durable,
                    zeppelin_embed::lifecycle::durability::CommitTier::Durable,
                )
                .with_schema(fixture::schema()),
            StoreTestDependencies::new(vfs.clone(), Arc::new(SystemMonotonicClock)),
        )
        .map_err(|e| e.to_string())?;
        store.enable_graph().map_err(|e| e.to_string())?;
        vfs.set_operation(1);
        if apply_mixed_batch(&store, &batch, "first").is_ok() {
            return Err(format!(
                "mixed {site:?}/{nth}/{mode:?} acknowledged a fault"
            ));
        }
        let events = vfs.events();
        if events.len() != 1 || events[0].fire_count != 1 {
            return Err(format!(
                "mixed member {nth} fault did not fire exactly once"
            ));
        }
        if store
            .ingest(fixture::batch(vec![fixture::document(93, "fenced")]))
            .is_ok()
        {
            return Err("mixed indeterminate failure did not fence the shared writer".into());
        }
        drop(store);
        for read_only in [false, true, false] {
            let recovered = Store::open(
                &path,
                fixture::options(read_only).with_durability(
                    zeppelin_embed::lifecycle::durability::DurabilityMode::Durable,
                    zeppelin_embed::lifecycle::durability::CommitTier::Durable,
                ),
            )
            .map_err(|e| e.to_string())?;
            let count = recovered
                .count_documents(None, None)
                .map_err(|e| e.to_string())?
                .count;
            let actual_generation = recovered
                .snapshot()
                .map_err(|e| e.to_string())?
                .generation();
            if count != if present { 2 } else { 0 }
                || mixed_node_present(&recovered, node) != present
                || actual_generation != if present { 2 } else { 1 }
            {
                return Err(format!(
                    "mixed member {nth} crossed atomic boundary: count={count}, gen={actual_generation}"
                ));
            }
        }
        coverage.hit(key);
    }
    coverage.hit(REQUIRED[10]);
    coverage.hit("op.MixedBatch");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mixed_batch_append_member_faults_fire() {
        let mut coverage = CoverageRegistry::default();
        mixed_batch(256, &mut coverage).unwrap();
        for key in &REQUIRED[6..] {
            assert!(coverage.count(key) > 0, "missing {key}");
        }
    }

    #[test]
    fn unified_graph_fault_sites_are_registered_and_fire() {
        let mut coverage = CoverageRegistry::default();
        probe(256, &mut coverage).unwrap();
        for key in REQUIRED {
            assert!(
                coverage.count(key) > 0,
                "missing unified fault receipt: {key}"
            );
        }
    }
}
