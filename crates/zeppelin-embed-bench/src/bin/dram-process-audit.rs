//! Across-process DRAM audit and recurring traversal-prefetch measurement.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::io;
use std::path::Path;
use std::process::Command;

use zeppelin_embed_bench::platform::memory_graph::verify_bench_profile;
use zeppelin_embed_bench::process_median::ProcessMedian;

const DEFAULT_PROCESSES: usize = 5;

fn main() {
    if let Err(error) = run() {
        eprintln!("dram-process-audit: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    verify_bench_profile()?;
    let processes = parse_process_count()?;
    let binary_directory = std::env::current_exe()?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| io::Error::other("audit executable has no parent directory"))?;
    let gather_binary = binary_directory.join("gather-kernel");
    let graph_binary = binary_directory.join("graph-search");
    let platform_binary = binary_directory.join("platform-truth");
    require_binary(&gather_binary)?;
    require_binary(&graph_binary)?;
    require_binary(&platform_binary)?;

    let mut gather_values: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut gather_taints: BTreeSet<String> = BTreeSet::new();
    for run in 0..processes {
        let stdout = run_child(&gather_binary, &[], "gather-kernel", run)?;
        for line in stdout
            .lines()
            .filter(|line| line.starts_with("GATHER_KERNEL_RESULT "))
        {
            let case = field(line, "case")?;
            if !case.starts_with("dram_scattered") {
                continue;
            }
            gather_values
                .entry(case.to_owned())
                .or_default()
                .push(field(line, "median_ns_per_row")?.parse()?);
            gather_taints.insert(field(line, "taint")?.to_owned());
        }
    }
    let gather_summaries = print_summaries("gather", &gather_values, &gather_taints)?;
    let unprefetched = gather_summaries
        .get("dram_scattered")
        .ok_or_else(|| io::Error::other("gather output omitted dram_scattered"))?;
    let prefetched = gather_summaries
        .get("dram_scattered_prefetch_next_group")
        .ok_or_else(|| io::Error::other("gather output omitted prefetched DRAM case"))?;
    println!(
        "DRAM_PREFETCH_PAIR processes={processes} unprefetched_median_ns_per_row={:.6} prefetched_median_ns_per_row={:.6} prefetched_over_unprefetched={:.6} authority=recurring_measurement_not_gate",
        unprefetched.median(),
        prefetched.median(),
        prefetched.median() / unprefetched.median(),
    );

    let mut traversal_values: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut traversal_taints: BTreeSet<String> = BTreeSet::new();
    for run in 0..processes {
        let run_label = run.to_string();
        for prefetch in ["off", "on"] {
            let stdout = run_child(
                &graph_binary,
                &["--prefetch", prefetch, "--run", &run_label],
                "graph-search",
                run,
            )?;
            let line = stdout
                .lines()
                .find(|line| line.starts_with("GRAPH_SEARCH_RESULT "))
                .ok_or_else(|| io::Error::other("graph-search omitted GRAPH_SEARCH_RESULT"))?;
            traversal_values
                .entry(format!("traversal_prefetch_{prefetch}"))
                .or_default()
                .push(field(line, "p50_us")?.parse()?);
            traversal_taints.insert(field(line, "taint")?.to_owned());
        }
    }
    let traversal_summaries =
        print_summaries("graph_search", &traversal_values, &traversal_taints)?;
    let traversal_off = traversal_summaries
        .get("traversal_prefetch_off")
        .ok_or_else(|| io::Error::other("graph-search omitted traversal prefetch-off arm"))?;
    let traversal_on = traversal_summaries
        .get("traversal_prefetch_on")
        .ok_or_else(|| io::Error::other("graph-search omitted traversal prefetch-on arm"))?;
    println!(
        "TRAVERSAL_PREFETCH_PAIR processes={processes} off_median_p50_us={:.6} on_median_p50_us={:.6} on_over_off={:.6} authority=recurring_measurement_not_gate",
        traversal_off.median(),
        traversal_on.median(),
        traversal_on.median() / traversal_off.median(),
    );

    let mut graph_values: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut graph_taints: BTreeSet<String> = BTreeSet::new();
    for run in 0..processes {
        let stdout = run_child(
            &platform_binary,
            &["memory-graph", "h3", "--cache-line-bytes", "128"],
            "memory-graph-h3",
            run,
        )?;
        for line in stdout
            .lines()
            .filter(|line| line.starts_with("MEMORY_GRAPH_H3 "))
        {
            let key = format!(
                "working_set_{}_rank_{}_arm_{}",
                field(line, "working_set_bytes")?,
                field(line, "rank")?,
                field(line, "arm")?,
            );
            graph_values
                .entry(key)
                .or_default()
                .push(field(line, "mean_ns_per_hop")?.parse()?);
            graph_taints.insert(field(line, "taint")?.to_owned());
        }
    }
    print_summaries("memory_graph_h3", &graph_values, &graph_taints)?;
    Ok(())
}

fn print_summaries(
    source: &str,
    values: &BTreeMap<String, Vec<f64>>,
    taints: &BTreeSet<String>,
) -> Result<BTreeMap<String, ProcessMedian>, Box<dyn Error>> {
    if values.is_empty() {
        return Err(io::Error::other(format!("{source} produced no parseable DRAM cells")).into());
    }
    let taint = taints.iter().cloned().collect::<Vec<_>>().join("+");
    let mut summaries = BTreeMap::new();
    for (case, observations) in values {
        let summary = ProcessMedian::new(observations.clone())?;
        println!(
            "DRAM_PROCESS_RESULT source={source} case={case} process_values={} process_median={:.6} process_min={:.6} process_max={:.6} between_process_spread_percent={:.6} taint={taint}",
            format_values(summary.values()),
            summary.median(),
            summary.minimum(),
            summary.maximum(),
            summary.spread_percent(),
        );
        summaries.insert(case.clone(), summary);
    }
    Ok(summaries)
}

fn run_child(
    binary: &Path,
    arguments: &[&str],
    label: &str,
    run: usize,
) -> Result<String, Box<dyn Error>> {
    let output = Command::new(binary).args(arguments).output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{label} process {run} exited {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ))
        .into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn field<'a>(line: &'a str, key: &str) -> Result<&'a str, Box<dyn Error>> {
    let prefix = format!("{key}=");
    line.split_whitespace()
        .find_map(|part| part.strip_prefix(&prefix))
        .ok_or_else(|| io::Error::other(format!("result line omitted {key}: {line}")).into())
}

fn format_values(values: &[f64]) -> String {
    values
        .iter()
        .map(|value| format!("{value:.6}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn require_binary(path: &Path) -> Result<(), Box<dyn Error>> {
    if path.is_file() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "required sibling binary is absent: {}",
            path.display()
        ))
        .into())
    }
}

fn parse_process_count() -> Result<usize, Box<dyn Error>> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let processes = match arguments.as_slice() {
        [] => DEFAULT_PROCESSES,
        [flag, value] if flag == "--processes" => value.parse()?,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "usage: dram-process-audit [--processes ODD>=3]",
            )
            .into());
        }
    };
    if processes < 3 || processes.is_multiple_of(2) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("--processes must be odd and at least 3, got {processes}"),
        )
        .into());
    }
    Ok(processes)
}
