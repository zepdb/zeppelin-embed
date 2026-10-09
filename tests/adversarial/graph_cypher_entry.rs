//! Directed ZE-56 proof: Cypher text through the compiler and the store's
//! structured statement seam, on a real persisted native graph store.
//!
//! The oracle never reads the engine's own planner or evaluator: expected
//! values are the seed's own arithmetic, and "nothing committed" is the
//! generation a read observes before and after each refusal.

use super::coverage::CoverageRegistry;
use std::collections::BTreeSet;
use std::path::PathBuf;
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::property_graph::query::QueryValue;
use zeppelin_embed::property_graph::query::completed::{
    CompletedGraphResult, GraphQueryErrorKind, GraphQueryOptions, Outcome, Value,
};
use zeppelin_embed::property_graph::query::plan::ParameterBinding;
use zeppelin_embed_cypher::{CompileLimits, ErrorKind, StatementError, execute};

pub const REQUIRED_COVERAGE: [&str; 25] = [
    "property-graph.cypher-entry.no-return.commit",
    "property-graph.cypher-entry.no-return-fault.fire",
    "property-graph.cypher-entry.profile-reject.fire",
    "property-graph.cypher-entry.reopen-read",
    "property-graph.cypher-entry.oracle.can-fire",
    "property-graph.cypher-entry.same-seed-control",
    "property-graph.cypher-entry.delete-connected.fire",
    "property-graph.cypher-entry.deleted-result.fire",
    "property-graph.cypher-entry.list-type.fire",
    "property-graph.cypher-entry.limit0-write.commit",
    "property-graph.cypher-entry.id-lookup",
    "property-graph.cypher-entry.id-excluded-scan",
    "property-graph.cypher-entry.id-lookup-limit.fire",
    "property-graph.cypher-entry.id-scan-limit.fire",
    "property-graph.cypher-entry.id-cancel.fire",
    "property-graph.cypher-entry.incident-source.read",
    "property-graph.cypher-entry.page-validation.statement-local",
    "property-graph.cypher-entry.incident-source.limit.fire",
    "property-graph.cypher-entry.incident-source.exclusions",
    "property-graph.cypher-entry.global-count",
    "property-graph.cypher-entry.count-filter-excluded",
    "property-graph.cypher-entry.count-limit.fire",
    "property-graph.cypher-entry.document-scan.full-drain",
    "property-graph.cypher-entry.document-scan.lazy-limit",
    "property-graph.cypher-entry.document-scan.work-limit.fire",
];

/// Everything one seed observed, compared across two runs of the same seed.
#[derive(Debug, PartialEq)]
struct Report {
    /// Values read back, in explicit ORDER BY order, before and after reopen.
    observations: Vec<i64>,
    /// Generation after setup, each refusal, LIMIT 0, clean commit and reopen.
    generations: Vec<u64>,
}

#[allow(
    clippy::result_large_err,
    reason = "preserve the typed public statement error in this test adapter"
)]
fn run(
    store: &Store,
    text: &str,
    parameters: &[ParameterBinding<'_>],
) -> Result<CompletedGraphResult, StatementError> {
    execute(
        store,
        &QueryControl::Cancel(CancelToken::new()),
        &GraphQueryOptions::default(),
        text,
        parameters,
        CompileLimits::default(),
    )
}

fn values(result: &CompletedGraphResult) -> Result<Vec<i64>, String> {
    (0..result.metadata().rows as usize)
        .map(|row| match result.cell(row, 0) {
            Some(Value::I64(value)) => Ok(*value),
            other => Err(format!("cypher entry: unexpected cell {other:?}")),
        })
        .collect()
}

fn read(store: &Store) -> Result<(Vec<i64>, u64), String> {
    let result = run(store, "MATCH (p:P) RETURN p.v AS v ORDER BY v", &[])
        .map_err(|error| format!("cypher entry read: {error}"))?;
    if result.metadata().outcome != Outcome::Read {
        return Err(String::from("cypher entry read changed the store"));
    }
    let rows = values(&result)?;
    // `once` ingests 32 implicit Document nodes as well as Guard and D.
    let expected = i64::try_from(rows.len()).map_err(|error| error.to_string())? + 2 + 32;
    for (text, count) in [
        ("MATCH (n) RETURN count(n)", expected),
        ("MATCH (n:Document) RETURN count(*)", 32),
        ("MATCH (n) WHERE true RETURN count(n)", expected),
    ] {
        let counted = run(store, text, &[]).map_err(|error| error.to_string())?;
        if values(&counted)? != [count]
            || counted.metadata().generation != result.metadata().generation
        {
            return Err(format!(
                "cypher count disagrees at retained generation: {text}"
            ));
        }
    }
    let limits = zeppelin_embed::property_graph::query::runtime::RuntimeLimits::default()
        .with_limit(
            zeppelin_embed::property_graph::query::runtime::WorkKind::Scans,
            0,
        )
        .map_err(|error| error.to_string())?;
    let options = GraphQueryOptions::default()
        .with_limits(24 * 1024 * 1024, limits)
        .map_err(|error| error.to_string())?;
    match execute(
        store,
        &QueryControl::Cancel(CancelToken::new()),
        &options,
        "MATCH (n) RETURN count(n)",
        &[],
        CompileLimits::default(),
    ) {
        Err(StatementError::Query(error))
            if error.kind() == GraphQueryErrorKind::Limit && error.nothing_committed() => {}
        Err(error) => return Err(format!("cypher count work refusal: {error}")),
        Ok(_) => return Err(String::from("cypher count bypassed its work limit")),
    }
    Ok((rows, result.metadata().generation.get()))
}

fn committed_no_rows(result: &CompletedGraphResult) -> Result<(), String> {
    if !matches!(result.metadata().outcome, Outcome::Committed { .. })
        || result.metadata().rows != 0
        || !result.pools().columns.is_empty()
    {
        return Err(String::from(
            "cypher entry: a write without RETURN must commit with no rows",
        ));
    }
    Ok(())
}

fn options() -> OpenOptions {
    OpenOptions::new()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024)
}

fn once(seed: u64, run_index: u32) -> Result<Report, String> {
    let root: PathBuf = std::env::temp_dir().join(format!(
        "zeppelin-ze56-cypher-entry-{}-{seed}-{run_index}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    let path = root.join("graph");
    let outcome = (|| {
        let base = i64::try_from(seed % 1000).map_err(|error| error.to_string())? * 10;
        let store = Store::create_graph_store(&path, options()).map_err(|e| e.to_string())?;
        let mut generations = Vec::new();

        let setup = run(
            &store,
            &format!("CREATE (:P {{v: {base}, s: 'x'}})-[:R]->(:Guard), (:D {{v: {base}}})"),
            &[],
        )
        .map_err(|error| format!("cypher entry setup: {error}"))?;
        committed_no_rows(&setup)?;
        store
            .ingest(zeppelin_embed::ingest::IngestBatch::new(
                (10_000..10_032)
                    .map(|id| {
                        zeppelin_embed::ingest::IngestDocument::new(
                            zeppelin_embed::ingest::DocumentVersion::new(
                                zeppelin_embed::ingest::DocId::new(id),
                                zeppelin_embed::ingest::Revision::new(1),
                            ),
                            vec![1.0],
                        )
                    })
                    .collect(),
            ))
            .map_err(|error| error.to_string())?;
        page_memo_controls(&store)?;
        probe_document_scan(&store)?;
        generations.push(read(&store)?.1);
        probe_id_lookup(&store)?;

        // R2 uses the existing query work-limit seam on the sparse source.
        incident_source_controls(&store)?;

        // A write without RETURN whose second item divides by zero: refused
        // as an expression error, and its valid first item is not committed.
        let text = "CREATE (:P {v: $a}), (:P {v: $a / $d})";
        let fault = [
            ParameterBinding {
                name: "a",
                value: QueryValue::I64(base + 1),
            },
            ParameterBinding {
                name: "d",
                value: QueryValue::I64(0),
            },
        ];
        match run(&store, text, &fault) {
            Err(StatementError::Query(error))
                if error.kind() == GraphQueryErrorKind::Expression && error.nothing_committed() => {
            }
            Err(error) => return Err(format!("cypher entry fault: wrong refusal {error}")),
            Ok(_) => return Err(String::from("cypher entry fault: committed a zero divisor")),
        }
        let (after_fault, generation) = read(&store)?;
        if after_fault != [base] {
            return Err(String::from("cypher entry fault: a refused write leaked"));
        }
        generations.push(generation);

        // An unbounded path is outside the profile: refused by the compiler,
        // before any admission can run.
        match run(&store, "MATCH (a:P)-[*]->(b) RETURN b", &[]) {
            Err(StatementError::Compile(error)) if error.kind == ErrorKind::InvalidRange => {}
            Err(error) => return Err(format!("cypher entry reject: wrong refusal {error}")),
            Ok(_) => return Err(String::from("cypher entry reject: executed")),
        }
        generations.push(read(&store)?.1);

        // Seed-owned fixtures distinguish incident-edge refusal from a stale
        // result on an isolated node; mixed types follow a staged assignment.
        for (text, kind) in [
            ("MATCH (n:P) DELETE n", GraphQueryErrorKind::Constraint),
            (
                "MATCH (n:D), (m:D) DELETE n RETURN m",
                GraphQueryErrorKind::Constraint,
            ),
            (
                "MATCH (n:P) SET n.v = n.v + 1, n.l = [n.v, n.s]",
                GraphQueryErrorKind::Expression,
            ),
        ] {
            let before = read(&store)?;
            match run(&store, text, &[]) {
                Err(StatementError::Query(error))
                    if error.kind() == kind && error.nothing_committed() => {}
                Err(error) => return Err(format!("cypher write refusal {text}: {error}")),
                Ok(_) => {
                    return Err(format!(
                        "cypher write refusal unexpectedly executed: {text}"
                    ));
                }
            }
            let after = read(&store)?;
            if after != before {
                return Err(format!("cypher write refusal changed state: {text}"));
            }
            generations.push(after.1);
        }
        let before_limit = read(&store)?.1;
        let limited = run(
            &store,
            &format!("CREATE (:P {{v: {}}}) RETURN 1 LIMIT 0", base + 2),
            &[],
        )
        .map_err(|error| format!("cypher LIMIT 0 write: {error}"))?;
        if limited.metadata().rows != 0
            || !matches!(limited.metadata().outcome, Outcome::Committed { .. })
        {
            return Err(String::from("cypher LIMIT 0 skipped its write"));
        }
        let (limit_values, limit_generation) = read(&store)?;
        if limit_values != [base, base + 2] || limit_generation != before_limit + 1 {
            return Err(String::from(
                "cypher LIMIT 0 write did not publish exactly once",
            ));
        }
        generations.push(limit_generation);

        // The same statement with a nonzero divisor is the clean control.
        let clean = [
            ParameterBinding {
                name: "a",
                value: QueryValue::I64(base + 1),
            },
            ParameterBinding {
                name: "d",
                value: QueryValue::I64(1),
            },
        ];
        let committed = run(&store, text, &clean)
            .map_err(|error| format!("cypher entry clean control: {error}"))?;
        committed_no_rows(&committed)?;
        let (mut observations, generation) = read(&store)?;
        generations.push(generation);

        store.close().map_err(|error| error.to_string())?;
        let reopened = Store::open_graph_store(&path, options()).map_err(|e| e.to_string())?;
        let (again, generation) = read(&reopened)?;
        generations.push(generation);
        reopened.close().map_err(|error| error.to_string())?;
        observations.extend(again);
        Ok(Report {
            observations,
            generations,
        })
    })();
    let _ = std::fs::remove_dir_all(&root);
    outcome
}

fn probe_document_scan(store: &Store) -> Result<(), String> {
    use zeppelin_embed::property_graph::query::runtime::{RuntimeLimits, WorkKind};
    let full =
        run(store, "MATCH (n) RETURN count(n) AS c", &[]).map_err(|error| error.to_string())?;
    let lazy = run(
        store,
        "MATCH (n) WITH n LIMIT 100 RETURN count(n) AS c",
        &[],
    )
    .map_err(|error| error.to_string())?;
    if values(&full)? != [35]
        || values(&lazy)? != [35]
        || full.metadata().counters.get(WorkKind::Lookups)
            >= lazy.metadata().counters.get(WorkKind::Lookups)
    {
        return Err(String::from(
            "document scan full-drain/lazy oracle mismatch",
        ));
    }
    // The final graph-page work is the membership pass; consuming its budget
    // must fail the complete statement at the existing runtime work seam.
    let pages = full
        .metadata()
        .counters
        .get(WorkKind::DirectoryPagesDecoded);
    let options = GraphQueryOptions::default()
        .with_limits(
            8 * 1024 * 1024,
            RuntimeLimits::default()
                .with_limit(
                    WorkKind::DirectoryPagesDecoded,
                    pages
                        .checked_sub(1)
                        .ok_or("document scan page work missing")?,
                )
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
    match execute(
        store,
        &QueryControl::Cancel(CancelToken::new()),
        &options,
        "MATCH (n) RETURN count(n) AS c",
        &[],
        CompileLimits::default(),
    ) {
        Err(StatementError::Query(error))
            if error.kind() == GraphQueryErrorKind::Limit && error.nothing_committed() =>
        {
            Ok(())
        }
        Err(error) => Err(format!("document scan work-limit wrong refusal: {error}")),
        Ok(_) => Err(String::from("document scan work limit did not fire")),
    }
}

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let report = once(seed, 0)?;
    let base = i64::try_from(seed % 1000).map_err(|error| error.to_string())? * 10;
    // Independent oracle: LIMIT 0 adds base+2, then clean adds base+1 twice.
    let expected = vec![
        base,
        base + 1,
        base + 1,
        base + 2,
        base,
        base + 1,
        base + 1,
        base + 2,
    ];
    if report.observations != expected {
        return Err(format!(
            "cypher entry oracle mismatch: {:?}",
            report.observations
        ));
    }
    // Refusals publish nothing; the clean write publishes exactly once and
    // reopen preserves it.
    let [
        setup,
        fault,
        reject,
        connected,
        deleted,
        list,
        limit,
        commit,
        reopen,
    ] = report.generations[..]
    else {
        return Err(String::from("cypher entry generation count mismatch"));
    };
    if [fault, reject, connected, deleted, list]
        .iter()
        .any(|value| *value != setup)
        || limit != setup + 1
        || commit != limit + 1
        || reopen != commit
    {
        return Err(format!("cypher entry generations {:?}", report.generations));
    }
    let mut perturbed = report.observations.clone();
    if let Some(first) = perturbed.first_mut() {
        *first ^= 1;
    }
    if perturbed == expected {
        return Err(String::from(
            "cypher entry oracle accepted a perturbed value",
        ));
    }
    if once(seed, 1)? != report {
        return Err(String::from("cypher entry paired clean mismatch"));
    }
    if REQUIRED_COVERAGE.into_iter().collect::<BTreeSet<_>>().len() != REQUIRED_COVERAGE.len() {
        return Err(String::from("duplicate cypher entry coverage key"));
    }
    for key in REQUIRED_COVERAGE {
        coverage.hit(key);
    }
    Ok(())
}

fn probe_id_lookup(store: &Store) -> Result<(), String> {
    use zeppelin_embed::property_graph::query::runtime::{RuntimeLimits, WorkKind};
    let all = run(store, "MATCH (p:P) RETURN ze.node_id(p) AS id", &[])
        .map_err(|error| error.to_string())?;
    let Some(Value::String(span)) = all.cell(0, 0) else {
        return Err(String::from("ID lookup fixture identity"));
    };
    let id = all.string(*span).ok_or("ID lookup fixture text")?;
    let parameters = [ParameterBinding {
        name: "id",
        value: QueryValue::String(id),
    }];
    for (text, point, work_kind) in [
        (
            "MATCH (p:P) WHERE ze.node_id(p) = $id RETURN ze.node_id(p) AS id",
            true,
            WorkKind::Lookups,
        ),
        (
            "MATCH (p:P) WHERE ze.node_id(p) = $id AND true RETURN ze.node_id(p) AS id",
            false,
            WorkKind::Scans,
        ),
    ] {
        let result = run(store, text, &parameters).map_err(|error| error.to_string())?;
        let Some(Value::String(span)) = result.cell(0, 0) else {
            return Err(String::from("ID lookup result identity"));
        };
        if result.metadata().rows != 1
            || result.string(*span) != Some(id)
            || (result.metadata().counters.get(WorkKind::Scans) == 0) != point
        {
            return Err(String::from("ID lookup rows or access path differ"));
        }
        let options = GraphQueryOptions::default()
            .with_limits(
                24 * 1024 * 1024,
                RuntimeLimits::default()
                    .with_limit(work_kind, 0)
                    .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
        match execute(
            store,
            &QueryControl::Cancel(CancelToken::new()),
            &options,
            text,
            &parameters,
            CompileLimits::default(),
        ) {
            Err(StatementError::Query(error))
                if error.kind() == GraphQueryErrorKind::Limit && error.nothing_committed() => {}
            _ => return Err(String::from("ID lookup work limit did not fire")),
        }
        let cancelled = CancelToken::new();
        cancelled.cancel();
        match execute(
            store,
            &QueryControl::Cancel(cancelled),
            &GraphQueryOptions::default(),
            text,
            &parameters,
            CompileLimits::default(),
        ) {
            Err(StatementError::Query(error))
                if error.kind() == GraphQueryErrorKind::Cancelled && error.nothing_committed() => {}
            _ => return Err(String::from("ID lookup cancellation did not fire")),
        }
    }
    Ok(())
}

fn incident_source_controls(store: &Store) -> Result<(), String> {
    use zeppelin_embed::property_graph::query::runtime::{RuntimeLimits, WorkKind};
    let outgoing = "MATCH ()-[r]->() RETURN count(r)";
    for text in [
        outgoing,
        "MATCH ()<-[r]-() RETURN count(r)",
        "OPTIONAL MATCH ()-[r]->() RETURN count(r)",
        "MATCH (p:P)-[r]->() RETURN count(r)",
    ] {
        let result = run(store, text, &[]).map_err(|error| error.to_string())?;
        if values(&result)? != [1] || result.metadata().outcome != Outcome::Read {
            return Err(format!("incident source/exclusion mismatch: {text}"));
        }
    }
    let limits = RuntimeLimits::default()
        .with_limit(WorkKind::Scans, 0)
        .map_err(|error| format!("incident source limits: {error:?}"))?;
    let options = GraphQueryOptions::default()
        .with_limits(24 * 1024 * 1024, limits)
        .map_err(|error| format!("incident source options: {error:?}"))?;
    match execute(
        store,
        &QueryControl::Cancel(CancelToken::new()),
        &options,
        outgoing,
        &[],
        CompileLimits::default(),
    ) {
        Err(StatementError::Query(error))
            if error.kind() == GraphQueryErrorKind::Limit && error.nothing_committed() => {}
        Err(error) => return Err(format!("incident source wrong limit refusal: {error}")),
        Ok(_) => return Err(String::from("incident source work limit did not fire")),
    }
    let zero =
        run(store, "MATCH ()-[r]->() RETURN r LIMIT 0", &[]).map_err(|error| error.to_string())?;
    if zero.metadata().rows != 0 || zero.metadata().counters.get(WorkKind::Scans) != 0 {
        return Err(String::from("incident source LIMIT 0 scanned"));
    }
    // Paired clean execution after refusal uses the same retained storage path.
    if values(&run(store, outgoing, &[]).map_err(|error| error.to_string())?)? != [1] {
        return Err(String::from("incident source clean control changed"));
    }
    Ok(())
}

/// Receipt for repeated reads of immutable directory pages within one statement,
/// followed by a fresh statement that must validate those pages again.
pub fn page_memo_controls(store: &Store) -> Result<(), String> {
    use zeppelin_embed::property_graph::query::runtime::WorkKind;
    for query in [
        "MATCH ()-[r]->() RETURN count(r)",
        "MATCH ()-[r]->(), ()-[s]->() RETURN count(r)",
    ] {
        for _ in 0..2 {
            let result = run(store, query, &[]).map_err(|error| error.to_string())?;
            let pages = result
                .metadata()
                .counters
                .get(WorkKind::DirectoryPagesDecoded);
            if !(1..=3).contains(&pages) {
                return Err(format!(
                    "page validation statement receipt: {pages} for {query}"
                ));
            }
        }
    }
    Ok(())
}
