//! Directed registration of the eight shared native vector-index receipt groups.

use super::coverage::CoverageRegistry;
use std::collections::BTreeSet;
use zeppelin_embed::graph_native_vector_index_test_support::{
    ActualProbeReport, CleanPreparationObservation, ControlProbeReport, IdentityProbeReport,
    KernelProbeReport, LimitProbeReport, NativeFailureObservation, OracleProbeReport,
    PhysicalReadReport, ReopenProbeReport, SmallWritesProbeReport, TraceProbeReport,
    run_actual_probe,
};

pub const REGISTERED_COVERAGE: &[&str] = &[
    "property-graph.native-vector-index.kernel",
    "property-graph.native-vector-index.small-writes",
    "property-graph.native-vector-index.identity",
    "property-graph.native-vector-index.limit.fire",
    "property-graph.native-vector-index.control.fire",
    "property-graph.native-vector-index.reopen",
    "property-graph.native-vector-index.trace",
    "property-graph.native-vector-index.oracle.can-fire",
];

fn require(condition: bool, detail: &str) -> Result<(), String> {
    condition
        .then_some(())
        .ok_or_else(|| format!("native vector-index {detail}"))
}

fn validate_query_reads(report: &PhysicalReadReport, sources: usize) -> Result<(), String> {
    require(
        report.receipts.len() == sources
            && report.index_resolutions == sources
            && report
                .receipts
                .iter()
                .enumerate()
                .all(|(position, receipt)| {
                    receipt.source_resolutions == 1
                        && receipt.index_resolutions == 1
                        && receipt.source != receipt.index
                        && report.receipts.iter().take(position).all(|prior| {
                            prior.source != receipt.source && prior.index != receipt.index
                        })
                }),
        "physical source/index resolution mismatch",
    )
}

fn validate_kernel(report: &KernelProbeReport) -> Result<(), String> {
    validate_query_reads(&report.query_reads, 1)?;
    require(
        report.query_seed == 7
            && report.expected_identity != 0
            && report.expected_identity == report.observed_identity
            && report.expected_identity == report.hit_identity
            && report.expected_coordinate_bits == [0x3f80_0001, 0x8000_0000]
            && report.observed_coordinate_bits == report.expected_coordinate_bits
            && report.score_bits == 0x3f80_0002
            && report.hit_row == 0
            && report.distance_bits == 0
            && report.visited == 1,
        "kernel observation mismatch",
    )
}

fn validate_small_writes(report: &SmallWritesProbeReport) -> Result<(), String> {
    validate_query_reads(&report.query_reads, 5)?;
    let expected = report
        .separate_receipts
        .iter()
        .chain(&report.cohort_receipts)
        .map(|id| (*id, 1))
        .collect::<BTreeSet<_>>();
    let actual = report
        .sources
        .iter()
        .flat_map(|source| source.identities.iter().copied())
        .collect::<Vec<_>>();
    require(
        report.query_seed == 0x158
            && report.separate_receipts.len() == 4
            && report.cohort_receipts.len() == 32
            && expected.len() == 36
            && actual.len() == 36
            && actual.iter().copied().collect::<BTreeSet<_>>() == expected
            && report.sources.len() == 5
            && report.prepared_images_per_apply == [1; 5]
            && report.preparation_index_resolutions == [0; 5]
            && report.query_prepare_events == 0,
        "small-write partition or preparation mismatch",
    )?;
    let mut singleton_count = 0;
    let mut cohort_count = 0;
    for source in &report.sources {
        require(
            source.live_rows == source.rows as usize
                && source.coordinate_bits
                    == [0x3e80_0000, 0xbf00_0000].repeat(source.rows as usize)
                && source.seed_count == (source.rows as usize).min(4),
            "small-write source values or seeds mismatch",
        )?;
        match source.rows {
            1 => {
                singleton_count += 1;
                require(
                    source.identities.len() == 1
                        && source
                            .identities
                            .iter()
                            .all(|(id, rev)| *rev == 1 && report.separate_receipts.contains(id))
                        && source.visited == 1
                        && source.filtered_rows == [0],
                    "small-write singleton traversal mismatch",
                )?;
            }
            32 => {
                cohort_count += 1;
                require(
                    source.identities
                        == report
                            .cohort_receipts
                            .iter()
                            .map(|id| (*id, 1))
                            .collect::<Vec<_>>()
                        && source.visited > 4
                        && source.filtered_rows == [31],
                    "small-write cohort traversal mismatch",
                )?;
            }
            _ => return Err("native vector-index unexpected source geometry".into()),
        }
    }
    require(
        singleton_count == 4 && cohort_count == 1,
        "small-write source counts",
    )
}

fn validate_identity(report: &IdentityProbeReport) -> Result<(), String> {
    let high = 1_u128 << 80;
    require(
        report.write_identities == [high - 1, high, high + 1]
            && report.relationship == (1_u128 << 96) + 7
            && report.sources.len() == 3,
        "full-width identity receipts mismatch",
    )?;
    let expected = [
        (high - 1, [0x8000_0000, 0x3f00_0000]),
        (high, [0x3e80_0000, 0xbf40_0000]),
        (high + 1, [0x3f00_0000, 0x3f00_0000]),
    ];
    for (id, bits) in expected {
        require(
            report
                .sources
                .iter()
                .filter(|(identities, _, _, _)| identities == &[(id, 1)])
                .count()
                == 1,
            "missing or duplicated high identity source",
        )?;
        let source = report
            .sources
            .iter()
            .find(|(identities, _, _, _)| identities == &[(id, 1)])
            .ok_or("missing high identity source")?;
        require(
            source.1 == bits && source.2 == [0] && source.3 == 1,
            "high identity original bits or real kernel mapping mismatch",
        )?;
    }
    Ok(())
}

fn validate_clean(report: &CleanPreparationObservation) -> Result<(), String> {
    require(
        report.completed
            && report.event_count > 0
            && report.max_chunk_units > 0
            && report.max_chunk_units <= 256
            && report.max_work_observed > 0
            && report.prepared_images == 1
            && report.allocation_reserved_bytes > 0
            && report.allocation_requested_bytes > 0,
        "clean preparation observation mismatch",
    )
}

fn validate_failure(
    report: &NativeFailureObservation,
    kind: &str,
    closed: u64,
) -> Result<(), String> {
    require(
        report.fired
            && report.kind == kind
            && report.generation_before == report.generation_after
            && report.sequence_before == report.sequence_after
            && report.baseline_reserved_bytes == report.released_reserved_bytes
            && report.closed_reserved_bytes == closed,
        "scheduled failure, unchanged state, or owner release mismatch",
    )
}

fn validate_limit(report: &LimitProbeReport) -> Result<(), String> {
    validate_clean(&report.clean)?;
    require(
        report.work_limit > 0
            && report.work_limit < report.clean.max_work_observed
            && Some(report.memory_limit)
                == report
                    .clean
                    .allocation_reserved_bytes
                    .checked_add(report.clean.allocation_requested_bytes)
                    .and_then(|bytes| bytes.checked_sub(1)),
        "clean-derived limits mismatch",
    )?;
    validate_failure(&report.work, "work", report.clean.closed_reserved_bytes)?;
    validate_failure(&report.memory, "memory", report.clean.closed_reserved_bytes)
}

fn validate_control(report: &ControlProbeReport) -> Result<(), String> {
    validate_clean(&report.clean)?;
    validate_failure(
        &report.cancel,
        "cancelled",
        report.clean.closed_reserved_bytes,
    )?;
    validate_failure(
        &report.deadline,
        "timeout",
        report.clean.closed_reserved_bytes,
    )?;
    require(
        report.close.closing_observed
            && report.close.kind == "read-cancelled"
            && report.close.generation_before == report.close.generation_after
            && report.close.sequence_before == report.close.sequence_after
            && report.close.closed_reserved_bytes == report.clean.closed_reserved_bytes,
        "authentic close or settled ownership mismatch",
    )
}

fn row_bits(first: u32, second: u32) -> Vec<u32> {
    let mut bits = vec![0; 512];
    bits[0] = first;
    bits[1] = second;
    bits
}

fn validate_reopen(report: &ReopenProbeReport) -> Result<(), String> {
    let expected_bits = row_bits(0x3ec0_0000, 0xbf60_0000);
    require(
        report.expected_row_bits == expected_bits
            && report.wal_prepare_events == 0
            && report.checkpoint_prepare_events == 0
            && report.wal_before == report.wal_after
            && report.checkpoint_before == report.checkpoint_after,
        "WAL/checkpoint reopen or zero preparation mismatch",
    )?;
    for (receipts, source) in [
        (&report.wal_write_receipts, &report.wal_after),
        (&report.checkpoint_write_receipts, &report.checkpoint_after),
    ] {
        require(
            receipts.len() == 24
                && receipts.iter().collect::<BTreeSet<_>>().len() == 24
                && source.identities == receipts.iter().map(|id| (*id, 1)).collect::<Vec<_>>()
                && source.coordinate_bits == expected_bits.repeat(24)
                && source.rows == 24
                && source.live_rows == 24
                && source.dimensions == 512
                && source.profile == 1
                && source.max_degree == 44
                && source.seed_count == 4
                && !source.index_bytes.is_empty()
                && source.physical_references.len() > 1
                && source.physical_references.contains(&source.index_reference),
            "reopened index/source geometry or independent data mismatch",
        )?;
    }
    Ok(())
}

fn validate_trace(report: &TraceProbeReport) -> Result<(), String> {
    require(
        report.expected_row_bits == row_bits(0x3f20_0000, 0xbe00_0000)
            && report.write_receipts.len() == 2
            && report
                .write_receipts
                .iter()
                .map(Vec::len)
                .collect::<Vec<_>>()
                == [1, 24]
            && report.sources.len() == 2
            && report.sources[0].catalog != report.sources[1].catalog
            && report.small.complete
            && report.full.complete
            && report.small.references == report.full.references
            && report.small.batches > report.full.batches
            && report.small.max_batch == 1
            && report.full.max_batch > 1
            && report.full.max_batch <= 256
            && report.required_references.len() > 4
            && report
                .required_references
                .iter()
                .all(|reference| report.full.references.contains(reference))
            && report.baseline_reserved_bytes == report.released_reserved_bytes,
        "trace completion, historical catalog closure, or release mismatch",
    )?;
    let expected = report
        .write_receipts
        .iter()
        .flatten()
        .map(|id| (*id, 1))
        .collect::<BTreeSet<_>>();
    let actual = report
        .sources
        .iter()
        .flat_map(|source| source.identities.iter().copied())
        .collect::<Vec<_>>();
    require(
        expected.len() == 25
            && actual.len() == 25
            && actual.iter().copied().collect::<BTreeSet<_>>() == expected,
        "trace source identity mismatch",
    )?;
    for source in &report.sources {
        let entries = source.entry_points.iter().copied().collect::<BTreeSet<_>>();
        require(
            source.identities.len() == source.rows as usize
                && source.coordinate_bits == report.expected_row_bits.repeat(source.rows as usize)
                && entries.len() == (source.rows as usize).min(4)
                && source.seed_rows == entries.into_iter().collect::<Vec<_>>()
                && source.seed_rows.iter().all(|row| *row < source.rows),
            "trace source value or persisted seed mismatch",
        )?;
    }
    Ok(())
}

fn validate_oracle(report: &OracleProbeReport) -> Result<(), String> {
    let controls = [
        "missing-required-reference",
        "changed-revision",
        "changed-value",
        "changed-seed",
        "missing-write-receipt",
        "duplicate-write-receipt",
    ];
    require(
        report.query_seed == 0x158e
            && report.expected_identities.len() == 24
            && report
                .expected_identities
                .iter()
                .collect::<BTreeSet<_>>()
                .len()
                == 24
            && report
                .expected_identities
                .iter()
                .all(|(id, revision)| *id != 0 && *revision == 1)
            && report.observed_identities == report.expected_identities
            && report.expected_coordinate_bits == row_bits(0x3f00_0001, 0xbf40_0001).repeat(24)
            && report.observed_coordinate_bits == report.expected_coordinate_bits
            && report.declared_seed_rows.len() == 4
            && report
                .declared_seed_rows
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .len()
                == 4
            && report
                .observed_seed_rows
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                == report.declared_seed_rows.iter().copied().collect()
            && report.observed_seed_rows.len() == 4
            && report.observed_entry_points == report.declared_seed_rows
            && report.declared_seed_rows.iter().all(|row| *row < 24)
            && report.initial_complete
            && report.restored_complete
            && report.initial_clean
            && report.same_seed_clean
            && report
                .controls
                .iter()
                .map(|control| control.name)
                .collect::<Vec<_>>()
                == controls
            && report
                .controls
                .iter()
                .all(|control| control.rejected && control.restored)
            && report.fires == 6
            && report.restored_checks == 5
            && report.release_checks == 2
            && report.observed_references == report.restored_references
            && report.traced_reference_count
                == report.observed_references.len() + report.restored_references.len()
            && report.traced_reference_count > 0
            && report.trace_batches > 0
            && report.preparation_events > 0
            && report.traversal_visits > 0
            && report.baseline_reserved_bytes == report.released_reserved_bytes,
        "oracle actual observations or restored controls mismatch",
    )?;
    require(
        report.required_references.len() > 2
            && report
                .required_references
                .iter()
                .enumerate()
                .all(|(index, (reference, _))| {
                    !report.required_references[..index]
                        .iter()
                        .any(|(prior, _)| prior == reference)
                })
            && report.required_references.iter().all(|(reference, count)| {
                (*count == 1 || *count == 3)
                    && report
                        .observed_references
                        .iter()
                        .filter(|observed| *observed == reference)
                        .count()
                        == *count
            })
            && report
                .required_references
                .iter()
                .filter(|(_, count)| *count == 3)
                .count()
                == 1,
        "oracle independently enumerated reference multiplicity mismatch",
    )
}

/// Credits each exact key only after its own independently checked report succeeds.
pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<ActualProbeReport, String> {
    let report = run_actual_probe(seed)?;
    let receipts = [
        (report.kernel.key, report.kernel.seed),
        (report.small_writes.key, report.small_writes.seed),
        (report.identity.key, report.identity.seed),
        (report.limit.key, report.limit.seed),
        (report.control.key, report.control.seed),
        (report.reopen.key, report.reopen.seed),
        (report.trace.key, report.trace.seed),
        (report.oracle.key, report.oracle.seed),
    ];
    require(
        receipts.iter().map(|(key, _)| *key).collect::<Vec<_>>() == REGISTERED_COVERAGE
            && receipts.iter().all(|(_, actual_seed)| *actual_seed == seed),
        "receipt inventory or caller seed mismatch",
    )?;
    validate_kernel(&report.kernel)?;
    coverage.hit(report.kernel.key);
    validate_small_writes(&report.small_writes)?;
    coverage.hit(report.small_writes.key);
    validate_identity(&report.identity)?;
    coverage.hit(report.identity.key);
    validate_limit(&report.limit)?;
    coverage.hit(report.limit.key);
    validate_control(&report.control)?;
    coverage.hit(report.control.key);
    validate_reopen(&report.reopen)?;
    coverage.hit(report.reopen.key);
    validate_trace(&report.trace)?;
    coverage.hit(report.trace.key);
    validate_oracle(&report.oracle)?;
    coverage.hit(report.oracle.key);
    Ok(report)
}
