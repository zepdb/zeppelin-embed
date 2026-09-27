//! ZE-47 native storage fault classes 1-4.
//!
//! This family owns the seeded schedule and every comparator. The engine
//! crate executes the schedule against a real native store under a faulty VFS
//! and returns only actual observations; the expected answer comes from the
//! independent oracle crate.

use super::coverage::CoverageRegistry;
use rand::RngCore;
use zeppelin_embed::graph_storage_fault_test_support::{
    ObservedKeyedNode, StorageFaultProbeReport, StorageFaultSchedule, StorageFaultState,
    run_actual_probe,
};
use zeppelin_embed_adversarial_oracle::graph_directory::{KeyedModel, KeyedNode};

pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.storage-faults.artifact-ref.fire",
    "property-graph.storage-faults.artifact-ref.clean",
    "property-graph.storage-faults.split.fire",
    "property-graph.storage-faults.split.clean",
    "property-graph.storage-faults.out-in.fire",
    "property-graph.storage-faults.out-in.clean",
    "property-graph.storage-faults.root-replacement.fire",
    "property-graph.storage-faults.root-replacement.clean",
    "property-graph.storage-faults.oracle.can-fire",
];

/// Keys whose body must have fired at least one scheduled fault or refusal.
const FIRED: &[&str] = &[
    "property-graph.storage-faults.artifact-ref.fire",
    "property-graph.storage-faults.split.fire",
    "property-graph.storage-faults.out-in.fire",
    "property-graph.storage-faults.root-replacement.fire",
];

/// Smallest discovery cap that still reaches a real 16 KiB leaf split. A node
/// leaf cell is 8 framing bytes plus a 28-byte inline `u128` key plus the
/// encoded payload reference, so a 16,384-byte page with its 64-byte header
/// holds roughly 214 of them; the schedule always stays above that. Class 2
/// commits only as far as the split, not the whole cap.
const MINIMUM_SPLIT_KEYS: u32 = 240;

/// Keyed nodes per class-2 commit. The engine body discovers the splitting
/// chunk at this granularity and reports the pre-split population it then
/// committed; this family checks that report against the same constant rather
/// than trusting it.
const SPLIT_CHUNK_KEYS: u32 = 16;

/// Independently authored class-1 population: `ref-0` through `ref-3`.
const ARTIFACT_REF_KEYS: usize = 4;

/// Derives the per-class schedule from the campaign seed. Nothing in this
/// family is a fixed body: the damaged offset, the keyed population, the
/// probed key, the skipped operations and the root variant all move.
fn schedule_for(seed: u64) -> StorageFaultSchedule {
    let mut rng = super::test_support::seeded_rng("property_graph::storage_faults", seed);
    let split_keys = MINIMUM_SPLIT_KEYS + (rng.next_u64() % 48) as u32;
    StorageFaultSchedule {
        artifact_ref_offset: rng.next_u64(),
        split_keys,
        split_probe: (rng.next_u64() % u64::from(split_keys)) as u32,
        split_skip: (rng.next_u64() % 2) as u32,
        out_in_append: (rng.next_u64() % 3) as u32 + 1,
        root_variant: (rng.next_u64() % 4) as u8,
    }
}

/// The independently authored class-2 fixture: the same ascending key names
/// the engine body commits before the splitting chunk. The count is the one
/// observation this family cannot derive, because the splitting chunk is
/// discovered against the real page geometry; `check_split` bounds it against
/// the schedule and the chunk size before this model is built.
fn split_model(pre_split_keys: u32) -> Result<KeyedModel, String> {
    let mut model = KeyedModel::new();
    for index in 0..pre_split_keys {
        model
            .create(format!("split-{index:05}").as_bytes(), 1)
            .map_err(|difference| format!("split fixture: {difference:?}"))?;
    }
    Ok(model)
}

fn keyed(observed: &[ObservedKeyedNode]) -> Vec<KeyedNode> {
    observed
        .iter()
        .map(|entry| KeyedNode {
            key: entry.key.clone(),
            node: entry.node,
            revision: entry.revision,
        })
        .collect()
}

/// A refusal classification is loud when it names an explicit refusal. A
/// silent answer, a silent absence, or a resolved substituted block kind is a
/// violated contract, not a fault the engine survived.
fn is_loud(classification: &str) -> bool {
    match classification.rsplit_once(':') {
        Some((_, outcome)) => matches!(
            outcome,
            "refused-open" | "refused-lookup" | "refused" | "Invalid" | "Missing" | "Format" | "Io"
        ),
        None => false,
    }
}

fn check_loud(label: &str, classifications: &[String]) -> Result<(), String> {
    if classifications.is_empty() {
        return Err(format!("ZE-47 {label} produced no refusal"));
    }
    for classification in classifications {
        if !is_loud(classification) {
            return Err(format!("ZE-47 {label} was not loud: {classification}"));
        }
    }
    Ok(())
}

/// Class 1: every damaged artifact reference refuses loudly, and every
/// untouched node still reads exactly as committed once the bytes are back.
fn check_artifact_ref(state: &StorageFaultState) -> Result<(), String> {
    check_loud("artifact-ref", &state.artifact_refusals)?;
    let mut model = KeyedModel::new();
    for index in 0..ARTIFACT_REF_KEYS {
        model
            .create(format!("ref-{index}").as_bytes(), 1)
            .map_err(|difference| format!("ZE-47 artifact-ref fixture: {difference:?}"))?;
    }
    model
        .check(&keyed(&state.surviving_nodes))
        .map_err(|difference| format!("ZE-47 untouched nodes changed: {difference:?}"))
}

/// Class 2: the faulted commit was the one that splits the node directory,
/// the lease retained from strictly before that split is still a pre-split
/// root and still answers every pre-split key identically to the new root,
/// and no refused split advanced a published generation.
fn check_split(schedule: StorageFaultSchedule, state: &StorageFaultState) -> Result<(), String> {
    if state.split_level == 0 {
        return Err("ZE-47 the splitting chunk never split the node directory".into());
    }
    if state.retained_split_level != 0 {
        return Err(format!(
            "ZE-47 the retained lease was not a pre-split root: level {}",
            state.retained_split_level
        ));
    }
    if state.retained_root_digests.0 != state.retained_root_digests.1 {
        return Err(format!(
            "ZE-47 the retained pre-split root page changed bytes: {:?}",
            state.retained_root_digests
        ));
    }
    if state.pre_split_keys == 0
        || !state.pre_split_keys.is_multiple_of(SPLIT_CHUNK_KEYS)
        || state.pre_split_keys + SPLIT_CHUNK_KEYS > schedule.split_keys
    {
        return Err(format!(
            "ZE-47 implausible pre-split population {} under a {} cap",
            state.pre_split_keys, schedule.split_keys
        ));
    }
    check_loud("split", &state.split_refusals)?;
    if state.split_generations.0 != state.split_generations.1 {
        return Err(format!(
            "ZE-47 a refused split advanced the published generation: {:?}",
            state.split_generations
        ));
    }
    if state.unsplit_generations.0 != state.unsplit_generations.1 {
        return Err(format!(
            "ZE-47 an indeterminate split survived a reopen: {:?}",
            state.unsplit_generations
        ));
    }
    if state.unsplit_reopen_level != 0 {
        return Err(format!(
            "ZE-47 a reopen exposed a split whose WAL envelope never landed: level {}",
            state.unsplit_reopen_level
        ));
    }
    split_model(state.pre_split_keys)?
        .check_retained(&keyed(&state.committed_keys), &keyed(&state.old_root_keys))
        .map_err(|difference| format!("ZE-47 split population differs: {difference:?}"))
}

/// Class 3: a half-prepared candidate never publishes and both directions of
/// the surviving relationship agree after a reopen.
fn check_out_in(state: &StorageFaultState) -> Result<(), String> {
    check_loud("out-in", &state.out_in_refusals)?;
    compare_directions(&state.out_rows, &state.in_rows)
}

fn compare_directions(
    out_rows: &[zeppelin_embed::graph_read_view_test_support::ObservedRelationship],
    in_rows: &[zeppelin_embed::graph_read_view_test_support::ObservedRelationship],
) -> Result<(), String> {
    if out_rows.is_empty() {
        return Err("ZE-47 no OUT row survived the refused candidate".into());
    }
    if out_rows != in_rows {
        return Err(format!(
            "ZE-47 OUT/IN row equality broke: {out_rows:?} against {in_rows:?}"
        ));
    }
    Ok(())
}

/// Class 4: a reopen exposes exactly the previous generation and no stale
/// preparation publishes.
fn check_root(state: &StorageFaultState) -> Result<(), String> {
    check_loud("root-replacement", &state.root_refusals)?;
    if state.reopened_generation != state.previous_generation {
        return Err(format!(
            "ZE-47 reopen exposed generation {} instead of {}",
            state.reopened_generation, state.previous_generation
        ));
    }
    Ok(())
}

/// Proves each comparator can reject a wrong answer before the clean run is
/// allowed to count. A comparator that cannot fire proves nothing.
fn comparators_can_fire(
    schedule: StorageFaultSchedule,
    state: &StorageFaultState,
) -> Result<(), String> {
    let mut silent = state.clone();
    silent
        .artifact_refusals
        .push("bit-flip:silent-answer".to_owned());
    if check_artifact_ref(&silent).is_ok() {
        return Err("ZE-47 artifact-ref comparator accepted a silent answer".into());
    }

    let mut lost = state.clone();
    lost.surviving_nodes.pop();
    if check_artifact_ref(&lost).is_ok() {
        return Err("ZE-47 artifact-ref comparator accepted a lost node".into());
    }

    let mut flat = state.clone();
    flat.split_level = 0;
    if check_split(schedule, &flat).is_ok() {
        return Err("ZE-47 split comparator accepted an unsplit directory".into());
    }

    // The defect this class had before: a lease taken after the split was
    // treated as the pre-split oracle, which proves nothing about the split.
    let mut late = state.clone();
    late.retained_split_level = state.split_level;
    if check_split(schedule, &late).is_ok() {
        return Err("ZE-47 split comparator accepted a post-split retained root".into());
    }

    let mut rewritten = state.clone();
    rewritten.retained_root_digests.1 = state.retained_root_digests.0.wrapping_add(1);
    if check_split(schedule, &rewritten).is_ok() {
        return Err("ZE-47 split comparator accepted a rewritten retained root".into());
    }

    let mut short = state.clone();
    short.pre_split_keys = state.pre_split_keys + 1;
    if check_split(schedule, &short).is_ok() {
        return Err("ZE-47 split comparator accepted an implausible population".into());
    }

    let mut survived = state.clone();
    survived.unsplit_reopen_level = 1;
    if check_split(schedule, &survived).is_ok() {
        return Err("ZE-47 split comparator accepted a recovered indeterminate split".into());
    }

    let mut moved = state.clone();
    if let Some(entry) = moved.old_root_keys.first_mut() {
        entry.node = entry.node.wrapping_add(1);
    }
    if check_split(schedule, &moved).is_ok() {
        return Err("ZE-47 split comparator accepted a relocated identity".into());
    }

    let mut advanced = state.clone();
    advanced.split_generations.1 = advanced.split_generations.0 + 1;
    if check_split(schedule, &advanced).is_ok() {
        return Err("ZE-47 split comparator accepted a published partial split".into());
    }

    let mut half = state.clone();
    half.in_rows.clear();
    if check_out_in(&half).is_ok() {
        return Err("ZE-47 out-in comparator accepted a missing IN direction".into());
    }

    let mut stale = state.clone();
    stale.reopened_generation = stale.previous_generation + 1;
    if check_root(&stale).is_ok() {
        return Err("ZE-47 root comparator accepted a stale publication".into());
    }

    let mut quiet = state.clone();
    quiet.root_refusals = vec!["torn-tail:silent-absence".to_owned()];
    if check_root(&quiet).is_ok() {
        return Err("ZE-47 root comparator accepted a silent torn tail".into());
    }
    Ok(())
}

fn check_receipts(
    report: &StorageFaultProbeReport,
    coverage: &mut CoverageRegistry,
) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for receipt in &report.receipts {
        if !receipt.key.starts_with("property-graph.storage-faults.")
            || !seen.insert(receipt.key)
            || receipt.clean_controls == 0
        {
            return Err(format!(
                "ZE-47 invalid storage-fault receipt {}",
                receipt.key
            ));
        }
        if FIRED.contains(&receipt.key) && receipt.fires == 0 {
            return Err(format!("ZE-47 unfired storage boundary {}", receipt.key));
        }
        coverage.hit(receipt.key);
    }
    if seen.len() != 8 {
        return Err("ZE-47 missing storage-fault boundary receipts".into());
    }
    Ok(())
}

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let schedule = schedule_for(seed);
    let report = run_actual_probe(seed, schedule);
    comparators_can_fire(schedule, &report.state)?;
    coverage.hit(REQUIRED_COVERAGE[8]);
    check_artifact_ref(&report.state)?;
    check_split(schedule, &report.state)?;
    check_out_in(&report.state)?;
    check_root(&report.state)?;
    check_receipts(&report, coverage)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use zeppelin_embed::graph_storage_fault_test_support::{
        create_native_graph_with_infrastructure, open_native_graph_with_infrastructure,
    };
    use zeppelin_embed::lifecycle::OpenOptions;
    use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode};
    use zeppelin_embed::vfs::{StdVfs, Vfs};

    fn seam_options() -> OpenOptions {
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024)
    }

    /// The ZE-47 seam: an external crate can put its own `Vfs` under a real
    /// native store, create it, close it, reopen it, and still be refused
    /// loudly when the directory is not a native store.
    ///
    /// This uses `StdVfs` because `fault_vfs::ScheduledVfs` does not forward
    /// exclusive directory creation and therefore cannot host a native-store
    /// create today; the fault classes inject their faults inside the engine
    /// crate instead.
    #[test]
    fn storage_faults_seam_puts_a_caller_vfs_under_a_real_native_store() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let vfs: Arc<dyn Vfs> = Arc::new(StdVfs);
        let path = directory.path().join("native");
        let store =
            create_native_graph_with_infrastructure(&path, seam_options(), Arc::clone(&vfs))
                .expect("fresh native store through the seam");
        store.close().expect("close created store");
        let reopened =
            open_native_graph_with_infrastructure(&path, seam_options(), Arc::clone(&vfs))
                .expect("reopen through the seam");
        reopened.close().expect("close reopened store");
        let empty = directory.path().join("empty");
        std::fs::create_dir(&empty).expect("empty directory");
        assert!(
            open_native_graph_with_infrastructure(&empty, seam_options(), vfs).is_err(),
            "the seam must refuse a directory that holds no native store"
        );
    }

    /// Binds the real storage-fault classes to the independent comparators
    /// without running the whole campaign.
    #[test]
    fn storage_faults_probe_binds_actual_refusals_to_the_independent_comparator() {
        let mut coverage = super::CoverageRegistry::default();
        super::probe(0x5a45_0047, &mut coverage).expect("storage fault probe");
        for key in super::REQUIRED_COVERAGE {
            assert_eq!(coverage.count(key), 1, "missing {key}");
        }
    }

    /// The schedule must move with the seed; a fixed body would hide a fault
    /// site that only one ordering reaches.
    #[test]
    fn storage_faults_schedule_moves_with_the_seed() {
        let first = super::schedule_for(1);
        let second = super::schedule_for(2);
        assert_ne!(first, second);
        assert!(first.split_keys >= super::MINIMUM_SPLIT_KEYS);
        assert!(second.split_keys >= super::MINIMUM_SPLIT_KEYS);
    }
}
