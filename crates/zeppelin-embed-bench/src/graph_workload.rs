//! ZE-77 evidence contracts. Tooling only; missing observations are errors.
use crate::harness_json::{Value, json};
use std::collections::BTreeSet;

pub const READS: [&str; 6] = [
    "project-evidence",
    "semantic-context",
    "alice-project-ranking",
    "lexical-evidence",
    "hybrid-project-evidence",
    "bounded-evidence",
];
pub const RESOURCE_COUNTERS: [&str; 4] = [
    "engine_peak_bytes",
    "engine_bytes",
    "application_bytes",
    "application_peak_bytes",
];
pub const COUNTERS: [&str; 31] = [
    "completed_rows",
    "operator_rows",
    "adjacency_entries",
    "expressions",
    "hash_probes",
    "result_bytes",
    "prepared_payload_bytes",
    "completed_abi_bytes",
    "vector_coordinates",
    "vector_payload_bytes",
    "postings",
    "lexical_blocks",
    "search_calls",
    "directory_lookups",
    "scans",
    "paths",
    "rows_in",
    "rows_out",
    "join_probes",
    "group_keys",
    "eligibility_entries",
    "copied_bytes",
    "directory_pages_decoded",
    "directory_pages_copied",
    "property_values",
    "property_bytes",
    "adjacency_physical_entries",
    "adjacency_merged_visits",
    "adjacency_merge_runs",
    "eligibility_unique_entries",
    "candidate_window_peak",
];

fn number(v: &Value, key: &str) -> Result<u64, String> {
    v.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("missing/invalid {key}"))
}
fn cell_count(manifest: &Value, record: &Value, key: &str) -> Result<u64, String> {
    if let Some(cell) = record.get("cell").and_then(Value::as_str)
        && let Some(protocol) = manifest.get("cell_protocols").and_then(|p| p.get(cell))
    {
        return number(protocol, key);
    }
    number(manifest, key)
}
/// Rejects incomplete or environmentally tainted repetitions. Product target
/// failures are recorded separately and are never environmental taint.
pub fn validate_repetition(manifest: &Value, record: &Value) -> Result<(), String> {
    validate_payload(manifest, record)?;
    if record.get("tainted").and_then(Value::as_bool) != Some(false) {
        return Err("whole repetition tainted or missing taint decision".into());
    }
    validate_environment(record)
}
fn validate_payload(manifest: &Value, record: &Value) -> Result<(), String> {
    if record.get("error").is_some_and(|v| !v.is_null())
        || record.get("correct").and_then(Value::as_bool) != Some(true)
    {
        return Err("failed request or incorrect result".into());
    }
    if number(record, "warmups")? != cell_count(manifest, record, "warmups")? {
        return Err("partial warmups".into());
    }
    let samples = record
        .get("samples_ns")
        .and_then(Value::as_array)
        .ok_or("missing samples")?;
    if samples.len() as u64 != cell_count(manifest, record, "samples")?
        || samples.is_empty()
        || samples.iter().any(|v| v.as_u64().is_none())
    {
        return Err("missing/partial/invalid samples".into());
    }
    let checks = record
        .get("oracle_checks")
        .and_then(Value::as_array)
        .ok_or("missing independent oracle checks")?;
    if checks.len() != samples.len() {
        return Err("partial independent correctness checks".into());
    }
    for check in checks {
        validate_rows(&check["expected_rows"], &check["observed_rows"], 1e-6, 1e-6)?;
        let admitted = number(check, "admitted_generation")?;
        let generation_matches = if let Some(range) = check.get("truth_generation_range") {
            admitted >= number(range, "start")?
                && range
                    .get("end")
                    .and_then(Value::as_u64)
                    .is_none_or(|end| admitted < end)
        } else {
            check.get("admitted_generation") == check.get("truth_generation")
        };
        if !generation_matches {
            return Err("wrong state-at-admission truth".into());
        }
        let ids = |key: &str| -> Result<Vec<u128>, String> {
            check
                .get(key)
                .and_then(Value::as_array)
                .ok_or(format!("missing {key}"))?
                .iter()
                .map(|v| {
                    v.as_str()
                        .ok_or("full ID required")?
                        .parse::<u128>()
                        .map_err(|e| e.to_string())
                })
                .collect()
        };
        let recall = recall_at_k(&ids("truth_ids")?, &ids("hit_ids")?, 20)?;
        if check
            .get("recall_at_20")
            .is_some_and(|v| v.as_f64() != Some(recall))
        {
            return Err("incorrect reported recall".into());
        }
    }
    let disposal = record
        .get("disposal_ns")
        .and_then(Value::as_array)
        .ok_or("missing disposal")?;
    if disposal.len() != samples.len() || disposal.iter().any(|v| v.as_u64().is_none()) {
        return Err("partial disposal observations".into());
    }
    let counters = record
        .get("counters")
        .and_then(Value::as_array)
        .ok_or("missing ZE-76 counters")?;
    if counters.len() != samples.len() {
        return Err("partial ZE-76 counters".into());
    }
    for row in counters {
        for key in RESOURCE_COUNTERS {
            number(row, key).map_err(|e| format!("ZE-76: {e}"))?;
        }
        let writes = record["cell"].as_str().is_some_and(|c| {
            c.starts_with("structured-meeting-import")
                || c.starts_with("retention-")
                || c.starts_with("mixed-4r1w/") && row["participant"] == 4
        });
        for key in COUNTERS.into_iter().filter(|_| !writes) {
            number(row, key).map_err(|e| format!("ZE-76: {e}"))?;
        }
    }
    Ok(())
}
/// Environmental taint is separate from erroneous/partial product evidence.
pub fn validate_environment(record: &Value) -> Result<(), String> {
    if record.get("digests_before") != record.get("digests_after")
        || record
            .get("digests_before")
            .and_then(Value::as_object)
            .is_none_or(|v| v.is_empty())
    {
        return Err("fixture/source/config digest drift".into());
    }
    let monitor = record
        .get("monitor")
        .and_then(Value::as_array)
        .ok_or("missing monitor")?;
    let end = number(record, "duration_ms")?;
    let mut previous = 0;
    let mut busy = false;
    let mut baseline = None;
    for (i, observation) in monitor.iter().enumerate() {
        let t = number(observation, "elapsed_ms")?;
        if (i == 0 && t > 1000) || t < previous || t - previous > 1500 {
            return Err("monitor gap".into());
        }
        previous = t;
        if observation.get("thermal").and_then(Value::as_str) != Some("nominal") {
            return Err("thermal taint".into());
        }
        let state = (
            observation.get("power"),
            observation.get("ac"),
            observation.get("qos"),
        );
        if state.0.is_none() || state.1.and_then(Value::as_bool) != Some(true) || state.2.is_none()
        {
            return Err("missing power/AC/QoS observations".into());
        }
        if baseline.is_some_and(|b| b != state) {
            return Err("power/AC/QoS transition".into());
        }
        baseline = Some(state);
        let cpu = observation
            .get("background_cpu_fraction")
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && *v >= 0.0)
            .ok_or("missing background CPU")?;
        if busy && cpu > 0.1 {
            return Err("background CPU taint".into());
        }
        busy = cpu > 0.1;
    }
    if monitor.is_empty() || previous < end.saturating_sub(1000) {
        return Err("missing monitor coverage".into());
    }
    Ok(())
}
/// Approved normal read matrix; extra experiments are explicit cells too.
pub fn validate_manifest(manifest: &Value) -> Result<(), String> {
    if manifest["smoke"] == true {
        return Err("smoke evidence cannot qualify acceptance".into());
    }
    if manifest["blocked_inputs"]
        .as_object()
        .is_some_and(|b| !b.is_empty())
    {
        return Err("remaining campaign inputs are blocked".into());
    }
    for (key, expected) in [
        ("repetitions", 5),
        ("warmups", 50),
        ("samples", 1000),
        ("imports", 200),
        ("reader_requests", 1000),
        ("writer_requests", 200),
        ("recovery_opens", 20),
    ] {
        if number(manifest, key)? != expected {
            return Err(format!("accepted {key} must be {expected}"));
        }
    }
    let cells = manifest
        .get("cells")
        .and_then(Value::as_array)
        .ok_or("missing cells")?;
    let mut ids = BTreeSet::new();
    for cell in cells {
        let id = cell.as_str().ok_or("invalid cell ID")?;
        if !ids.insert(id) {
            return Err("duplicate cell".into());
        }
    }
    for id in required_cells() {
        if !ids.contains(id.as_str()) {
            return Err(format!("missing cell {id}"));
        }
    }

    Ok(())
}
/// Complete protocol inventory; native imports and mixed readers are explicit.
pub fn required_cells() -> Vec<String> {
    let mut cells = Vec::new();
    for experiment in ["baseline", "exact", "stress-10x"] {
        for state in ["A", "B"] {
            for language in ["rust", "c", "swift"] {
                for frontend in ["structured", "cypher"] {
                    for read in READS {
                        cells.push(format!("{experiment}/{state}/{language}/{frontend}/{read}"));
                    }
                }
            }
        }
    }
    for hop in [1, 2, 4, 8, 16] {
        for state in ["A", "B"] {
            for language in ["rust", "c", "swift"] {
                for frontend in ["structured", "cypher"] {
                    cells.push(format!(
                        "paths-{hop}/{state}/{language}/{frontend}/bounded-evidence"
                    ));
                }
            }
        }
    }
    for experiment in [
        "structured-meeting-import",
        "cypher-meeting-metadata",
        "mixed-4r1w",
    ] {
        for state in ["A", "B"] {
            for language in ["rust", "c", "swift"] {
                cells.push(format!("{experiment}/{state}/{language}"));
            }
        }
    }
    for experiment in [
        "retention-1x",
        "retention-5x",
        "retention-10x",
        "model-resident",
        "os-cold",
        "recovery-64",
        "recovery-16mib",
    ] {
        cells.push(experiment.into());
    }
    cells
}
/// Pipeline proof only. Acceptance continues to reject this manifest.
pub fn validate_smoke(manifest: &Value, records: &[Value]) -> Result<(), String> {
    if manifest["smoke"] != true
        || number(manifest, "repetitions")? != 1
        || number(manifest, "warmups")? != 5
        || number(manifest, "samples")? != 50
        || manifest["cells"] != json!(["baseline/A/rust/structured/project-evidence"])
        || records.len() != 1
        || records[0]["cell"] != manifest["cells"][0]
    {
        return Err("invalid single-cell smoke protocol".into());
    }
    validate_payload(manifest, &records[0])?;
    let monitor = records[0]["monitor"]
        .as_array()
        .ok_or("missing real smoke observer")?;
    if monitor.is_empty() {
        return Err("empty smoke observer".into());
    }
    for row in monitor {
        number(row, "elapsed_ms")?;
        if row["thermal"].as_str().is_none()
            || row["power"].as_str().is_none()
            || row["ac"].as_bool().is_none()
            || row["host_cpu_fraction"].as_f64().is_none()
        {
            return Err("partial real smoke observer".into());
        }
    }
    if records[0]["digests_before"] != records[0]["digests_after"] {
        return Err("smoke digest drift".into());
    }
    Ok(())
}
/// Smoke statistics retain environmental limitations; no acceptance targets.
pub fn summarize_smoke(records: &[Value]) -> Result<Value, String> {
    let record = records.first().ok_or("missing smoke record")?;
    let samples = record["samples_ns"]
        .as_array()
        .ok_or("missing smoke samples")?
        .iter()
        .map(|v| v.as_u64().ok_or("invalid smoke sample"))
        .collect::<Result<Vec<_>, _>>()?;
    let peak = record["counters"]
        .as_array()
        .ok_or("missing smoke counters")?
        .iter()
        .map(|c| number(c, "engine_peak_bytes"))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max();
    Ok(
        json!({"smoke":true,"qualification":false,"cell":record["cell"],"warmups":record["warmups"],
        "samples":samples.len(),"repetitions":1,"p50_ns":nearest_rank(&samples,50)?,
        "p95_ns":nearest_rank(&samples,95)?,"p99_ns":nearest_rank(&samples,99)?,
        "engine_peak_bytes":peak,"correct":record["correct"],"acceptance_environment_tainted":record["tainted"],
        "observer_scope":"real smoke observations; no worker QoS or isolated background CPU; acceptance rejected"}),
    )
}
/// Every required cell must have five valid fresh-process repetitions.
pub fn validate_matrix(manifest: &Value, records: &[Value]) -> Result<(), String> {
    validate_manifest(manifest)?;
    let mut nonces = BTreeSet::new();
    for row in records {
        let nonce = row
            .get("process_nonce")
            .and_then(Value::as_str)
            .ok_or("missing fresh-process identity")?;
        if !nonces.insert(nonce) {
            return Err("reused process across attempts/cells".into());
        }
    }
    for cell in manifest["cells"].as_array().ok_or("missing cells")? {
        let mut rows = Vec::new();
        for row in records.iter().filter(|r| r.get("cell") == Some(cell)) {
            validate_payload(manifest, row)?;
            let tainted = validate_environment(row).is_err();
            if row.get("tainted").and_then(Value::as_bool) != Some(tainted) {
                return Err("taint decision does not match actual observations".into());
            }
            if !tainted {
                rows.push(row);
            }
        }
        let mut processes = BTreeSet::new();
        let mut repetitions = BTreeSet::new();
        for row in &rows {
            validate_repetition(manifest, row)?;
            processes.insert(
                row.get("process_nonce")
                    .and_then(Value::as_str)
                    .ok_or("missing fresh-process identity")?,
            );
            repetitions.insert(number(row, "repetition")?);
        }
        if rows.len() != 5 || processes.len() != 5 || repetitions != BTreeSet::from([0, 1, 2, 3, 4])
        {
            return Err(format!("missing/duplicate repetitions for {cell}"));
        }
    }
    if records.iter().any(|r| {
        !manifest["cells"]
            .as_array()
            .is_some_and(|cells| cells.contains(&r["cell"]))
    }) {
        return Err("undeclared cell".into());
    }
    Ok(())
}
/// Nearest rank ceil(p*n)-1, integer arithmetic for percentages.
pub fn nearest_rank(samples: &[u64], percent: u8) -> Result<u64, String> {
    if samples.is_empty() || percent == 0 || percent > 100 {
        return Err("percentile requires samples and percent 1..100".into());
    }
    let rank = samples
        .len()
        .checked_mul(usize::from(percent))
        .ok_or("percentile overflow")?
        .div_ceil(100);
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted.get(rank - 1).copied().ok_or("invalid rank".into())
}
/// Recall counts unique full IDs against exact eligible truth, never low halves.
pub fn recall_at_k(truth: &[u128], hits: &[u128], k: usize) -> Result<f64, String> {
    if k == 0 {
        return Err("recall k must be positive".into());
    }
    let exact: BTreeSet<_> = truth.iter().take(k).copied().collect();
    let actual: BTreeSet<_> = hits.iter().copied().collect();
    if exact.len() != truth.len().min(k) || actual.len() != hits.len() || hits.len() > k {
        return Err("duplicate IDs or excessive hits".into());
    }
    if exact.is_empty() {
        return if hits.is_empty() {
            Ok(1.0)
        } else {
            Err("empty eligible domain returned hits".into())
        };
    }
    Ok(actual.intersection(&exact).count() as f64 / exact.len() as f64)
}
pub fn summarize(records: &[Value]) -> Result<Value, String> {
    let mut rows = Vec::new();
    let mut cohorts = Vec::new();
    for record in records.iter().filter(|r| r["tainted"] != true) {
        let samples = record["samples_ns"]
            .as_array()
            .ok_or("missing samples")?
            .iter()
            .map(|v| v.as_u64().ok_or("invalid sample"))
            .collect::<Result<Vec<_>, _>>()?;
        rows.push(json!({"cell":record["cell"],"repetition":record["repetition"],"p50_ns":nearest_rank(&samples,50)?,"p95_ns":nearest_rank(&samples,95)?,"p99_ns":nearest_rank(&samples,99)?,"max_ns":samples.iter().max()}));
        let mut groups = std::collections::BTreeMap::<(String, bool), (Vec<u64>, Vec<f64>)>::new();
        if let Some(checks) = record["oracle_checks"].as_array() {
            for (sample, check) in samples.iter().zip(checks) {
                if let Some(cohort) = check["cohort"].as_str() {
                    let group = groups
                        .entry((
                            cohort.into(),
                            check["normal_timing"]
                                .as_bool()
                                .ok_or("missing cohort timing classification")?,
                        ))
                        .or_default();
                    group.0.push(*sample);
                    if !check["truth_ids"].as_array().is_none_or(Vec::is_empty) {
                        group.1.push(
                            check["recall_at_20"]
                                .as_f64()
                                .ok_or("missing nonempty recall")?,
                        );
                    }
                }
            }
        }
        for ((cohort, normal), (times, recalls)) in groups {
            let recall = recalls.iter().copied().reduce(f64::min);
            cohorts.push(json!({"cell":record["cell"],"repetition":record["repetition"],"cohort":cohort,"normal_timing":normal,"samples":times.len(),"p50_ns":nearest_rank(&times,50)?,"p95_ns":nearest_rank(&times,95)?,"p99_ns":nearest_rank(&times,99)?,"max_ns":times.iter().max(),"minimum_nonempty_recall_at_20":recall,"recall_target_pass":recall.map(|r|r>=0.95)}));
        }
    }
    let mut by_cell = std::collections::BTreeMap::<String, Vec<Value>>::new();
    for row in &rows {
        by_cell
            .entry(row["cell"].as_str().unwrap_or("unlabeled").into())
            .or_default()
            .push(row.clone());
    }
    let cells = by_cell
        .into_iter()
        .map(|(cell, repetitions)| {
            let worst = repetitions
                .iter()
                .filter_map(|r| r["p95_ns"].as_u64())
                .max();
            let target = if cell.starts_with("baseline/") || cell.contains("meeting-") || cell == "unlabeled" { Some(250_000_000_u64) } else if cell.starts_with("recovery-") { Some(2_000_000_000_u64) } else { None };
            json!({"cell":cell,"repetitions":repetitions,"worst_repetition_p95_ns":worst,"p95_target_ns":target,"latency_target_pass":target.zip(worst).map(|(t,w)|w<=t)})
        })
        .collect::<Vec<_>>();
    let peak = records
        .iter()
        .filter(|r| r["tainted"] != true)
        .filter_map(|r| r["counters"].as_array())
        .flatten()
        .filter_map(|c| c["engine_peak_bytes"].as_u64())
        .max();
    let worst = rows
        .iter()
        .filter_map(|v| v["p95_ns"].as_u64())
        .max()
        .ok_or("no repetitions")?;
    Ok(
        json!({"cells":cells,"cohorts":cohorts,"repetitions":rows,"worst_repetition_p95_ns":worst,"engine_peak_bytes":peak,"memory_target_pass":peak.map(|p|p<=256<<20),"latency_target_pass":cells.iter().filter_map(|c|c["latency_target_pass"].as_bool()).all(|p|p),"tainted_attempts":records.iter().filter(|r|r["tainted"]==true).count()}),
    )
}
/// Compares complete typed tooling rows, preserving bags, optional modalities,
/// full IDs and exact scalar types. Only score cells use approved tolerances.
pub fn validate_rows(
    expected: &Value,
    observed: &Value,
    absolute: f64,
    relative: f64,
) -> Result<(), String> {
    if !absolute.is_finite() || !relative.is_finite() || absolute < 0.0 || relative < 0.0 {
        return Err("invalid numeric tolerance".into());
    }
    fn compare(a: &Value, b: &Value, abs: f64, rel: f64) -> Result<(), String> {
        if let (Some(a), Some(b)) = (a.as_array(), b.as_array()) {
            if a.len() != b.len() {
                return Err("partial/extra result rows or cells".into());
            }
            for (a, b) in a.iter().zip(b) {
                compare(a, b, abs, rel)?;
            }
            return Ok(());
        }
        let (a, b) = (
            a.as_object().ok_or("invalid expected cell")?,
            b.as_object().ok_or("invalid observed cell")?,
        );
        if a.len() != 1 || b.len() != 1 {
            return Err("invalid typed cell".into());
        }
        let (tag, x) = a.iter().next().ok_or("empty cell")?;
        let y = b.get(tag).ok_or("cell type differs")?;
        if tag == "f64_bits" {
            let score = |v: &Value| -> Result<f64, String> {
                let s = v.as_str().ok_or("invalid score bits")?;
                let f = f64::from_bits(u64::from_str_radix(s, 16).map_err(|e| e.to_string())?);
                if !f.is_finite() {
                    return Err("nonfinite score".into());
                }
                Ok(f)
            };
            let (x, y) = (score(x)?, score(y)?);
            if (x - y).abs() > abs + rel * x.abs().max(y.abs()) {
                return Err("independent full-domain score differs".into());
            }
            return Ok(());
        }
        if tag == "list" {
            return compare(x, y, abs, rel);
        }
        if x != y {
            return Err(format!("typed {tag} differs"));
        }
        Ok(())
    }
    compare(expected, observed, absolute, relative)
}
