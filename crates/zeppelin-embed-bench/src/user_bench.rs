use std::collections::BTreeSet;

/// Nearest-rank latency summary in milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Summary {
    /// Median observation.
    pub p50: f64,
    /// 95th-percentile observation.
    pub p95: f64,
    /// 99th-percentile observation.
    pub p99: f64,
    /// Arithmetic mean.
    pub mean: f64,
}

/// One cold-start row.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ColdCell {
    /// `MLX GPU` or `ANE`.
    pub backend: String,
    /// First-ever or relaunch label.
    pub launch: String,
    /// Time until `TextStore::open` returned.
    pub open_ms: f64,
    /// Time spent in the first public query.
    pub first_query_ms: f64,
    /// Process-start through returned hits.
    pub total_ms: f64,
}

/// One steady-state row source.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SteadyCell {
    /// Dense, Lexical, or Hybrid.
    pub leg: String,
    /// `MLX GPU`, `ANE`, or `(none)`.
    pub backend: String,
    /// `scan` or `graph`.
    pub store_tier: String,
    /// Public-query latency distribution.
    pub summary: Summary,
}

/// One quality row source.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QualityCell {
    /// Dense, Lexical, or Hybrid.
    pub leg: String,
    /// `scan`, `graph`, or `n/a`.
    pub tier: String,
    /// Mean nDCG@10 against qrels.
    pub ndcg_at_10: f64,
    /// Auto-versus-exact recall for MLX, when applicable.
    pub recall_mlx: Option<f64>,
    /// Auto-versus-exact recall for ANE, when applicable.
    pub recall_ane: Option<f64>,
}

/// Supplemental open-component measurements.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Components {
    /// `.zem` mapping and validation.
    pub bundle_open_ms: f64,
    /// MLX document-tower load.
    pub document_tower_ms: f64,
    /// MLX query-tower load.
    pub mlx_query_ms: f64,
    /// CoreML query-tower load.
    pub ane_query_ms: Option<f64>,
    /// Core store open.
    pub store_open_ms: f64,
}

/// Inputs used to render the fixed owner-facing tables.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Results {
    /// Corpus chunk rows.
    pub chunk_rows: Option<u64>,
    /// Verified scan-store rows.
    pub scan_rows: Option<u64>,
    /// Verified graph-store rows.
    pub graph_rows: Option<u64>,
    /// Total promotion wall time.
    pub promote_seconds: Option<f64>,
    /// Cold-start rows.
    pub cold: Vec<ColdCell>,
    /// Steady-state cells.
    pub steady: Vec<SteadyCell>,
    /// Quality cells.
    pub quality: Vec<QualityCell>,
    /// Supplemental component measurements.
    pub components: Option<Components>,
    /// Dense/graph/MLX repeated medians.
    pub dense_graph_mlx_repetitions: Vec<f64>,
    /// FiQA queries truncated by the selected maximum.
    pub truncated_queries: Option<usize>,
}

/// Returns nearest-rank p50/p95/p99 and the mean, or `None` for no samples.
#[must_use]
pub fn percentiles(samples: &[f64]) -> Option<Summary> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    Some(Summary {
        p50: nearest_rank(&sorted, 50),
        p95: nearest_rank(&sorted, 95),
        p99: nearest_rank(&sorted, 99),
        mean,
    })
}

fn nearest_rank(sorted: &[f64], percentile: usize) -> f64 {
    let rank = percentile.saturating_mul(sorted.len()).div_ceil(100);
    let index = rank.saturating_sub(1).min(sorted.len().saturating_sub(1));
    sorted.get(index).copied().unwrap_or_default()
}

/// Returns shared top-k ids divided by the exact result length.
#[must_use]
pub fn recall_at_k(approximate: &[u128], exact: &[u128]) -> f64 {
    if exact.is_empty() {
        return 0.0;
    }
    let approximate = approximate.iter().copied().collect::<BTreeSet<_>>();
    let exact = exact.iter().copied().collect::<BTreeSet<_>>();
    approximate.intersection(&exact).count() as f64 / exact.len() as f64
}

/// Returns one deterministic Fisher-Yates ordering for `len` rows.
#[must_use]
pub fn shuffled_order(len: usize, seed: u64) -> Vec<usize> {
    let mut order = (0..len).collect::<Vec<_>>();
    let mut state = seed;
    for upper in (1..len).rev() {
        state = splitmix64(state);
        let modulus = u64::try_from(upper.saturating_add(1)).unwrap_or(u64::MAX);
        let selected = usize::try_from(state % modulus).unwrap_or_default();
        order.swap(upper, selected);
    }
    order
}

fn splitmix64(mut state: u64) -> u64 {
    state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut mixed = state;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^ (mixed >> 31)
}

/// Renders the fixed section-4 tables without changing their structure.
#[must_use]
pub fn render_tables(results: &Results) -> String {
    let mut output = String::new();
    output.push_str(&format!(
        "Corpus: BEIR FiQA-2018, 57,638 documents, {} chunk rows, one sealed\nsegment, 648 test queries, k = 10, stored text returned on every hit.\nVerified tiers: scan store = `SealedScan` ({} rows); graph store =\n`SealedGraph` ({} rows), promoted in {} s of maintenance.\n\n",
        optional_integer(results.chunk_rows),
        optional_integer(results.scan_rows),
        optional_integer(results.graph_rows),
        optional_number(results.promote_seconds),
    ));
    output.push_str("### Table A. Cold start (fresh process, hybrid leg, graph store; ms)\n\n");
    output.push_str(
        "| backend | launch | TextStore::open | first query_text | total | budget ~1000 |\n",
    );
    output.push_str("| --- | --- | ---: | ---: | ---: | --- |\n");
    for (backend, launch) in [
        ("MLX GPU", "first-ever"),
        ("MLX GPU", "relaunch (median of 10)"),
        ("ANE", "first-ever (fresh model digest)"),
        ("ANE", "relaunch (median of 10)"),
    ] {
        if let Some(cell) = results
            .cold
            .iter()
            .find(|cell| cell.backend == backend && cell.launch == launch)
        {
            output.push_str(&format!(
                "| {backend} | {launch} | {:.3} | {:.3} | {:.3} | {} |\n",
                cell.open_ms,
                cell.first_query_ms,
                cell.total_ms,
                if cell.total_ms <= 1_000.0 {
                    "met"
                } else {
                    "missed"
                }
            ));
        } else {
            output.push_str(&format!("| {backend} | {launch} | | | | |\n"));
        }
    }
    let components = results.components;
    output.push_str(&format!(
        "\nOpen breakdown (`components`, ms): bundle open {} / document tower (MLX)\n{} / query runtime (MLX {}, ANE {}) / core store open {}.\n\n",
        optional_component(components.map(|value| value.bundle_open_ms)),
        optional_component(components.map(|value| value.document_tower_ms)),
        optional_component(components.map(|value| value.mlx_query_ms)),
        optional_component(components.and_then(|value| value.ane_query_ms)),
        optional_component(components.map(|value| value.store_open_ms)),
    ));
    output.push_str("### Table B. Steady state, user-felt `query_text` latency (ms, 1,944 samples per cell)\n\n");
    output.push_str("| leg | backend | scan p50 | graph p50 | graph payoff (scan - graph, ms) | speedup (scan / graph) | scan p95 | graph p95 | graph p99 |\n");
    output.push_str("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for (leg, backend) in [
        ("Dense", "MLX GPU"),
        ("Dense", "ANE"),
        ("Lexical", "(none)"),
        ("Hybrid", "MLX GPU"),
        ("Hybrid", "ANE"),
    ] {
        let scan = steady_cell(results, leg, backend, "scan");
        let graph = steady_cell(results, leg, backend, "graph");
        match (scan, graph) {
            (Some(scan), Some(graph)) => output.push_str(&format!(
                "| {leg} | {backend} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} |\n",
                scan.p50,
                graph.p50,
                scan.p50 - graph.p50,
                scan.p50 / graph.p50,
                scan.p95,
                graph.p95,
                graph.p99,
            )),
            _ => output.push_str(&format!("| {leg} | {backend} | | | | | | | |\n")),
        }
    }
    let repetitions = results
        .dense_graph_mlx_repetitions
        .iter()
        .skip(results.dense_graph_mlx_repetitions.len().saturating_sub(3))
        .map(|value| format!("{value:.3}"))
        .collect::<Vec<_>>()
        .join(" / ");
    output.push_str(&format!(
        "\nRep spread (Dense / graph / MLX, three runs): p50 {} ms. A\ndelta counts only if it exceeds 2x this spread.\n\n",
        repetitions
    ));
    output.push_str("### Table C. Quality on the same store\n\n");
    output.push_str(
        "| leg | tier | nDCG@10 | recall@10 vs Exact (MLX) | recall@10 vs Exact (ANE) |\n",
    );
    output.push_str("| --- | --- | ---: | ---: | ---: |\n");
    for (leg, tier) in [
        ("Dense", "scan"),
        ("Dense", "graph"),
        ("Lexical", "n/a"),
        ("Hybrid", "scan"),
        ("Hybrid", "graph"),
    ] {
        if leg == "Lexical" {
            let ndcg = results
                .quality
                .iter()
                .find(|cell| cell.leg == leg)
                .map(|cell| cell.ndcg_at_10);
            output.push_str(&format!(
                "| Lexical | n/a | {} | exact by construction | exact by construction |\n",
                optional_number(ndcg)
            ));
        } else if let Some(cell) = results
            .quality
            .iter()
            .find(|cell| cell.leg == leg && cell.tier == tier)
        {
            output.push_str(&format!(
                "| {leg} | {tier} | {:.6} | {} | {} |\n",
                cell.ndcg_at_10,
                optional_number(cell.recall_mlx),
                optional_number(cell.recall_ane),
            ));
        } else {
            output.push_str(&format!("| {leg} | {tier} | | | |\n"));
        }
    }
    output.push_str("\n### Table D. Power during a 20 s hybrid query loop (graph store)\n\n");
    output.push_str("| backend | CPU mW | GPU mW | ANE mW | `pmset -g therm` before / after |\n");
    output.push_str("| --- | ---: | ---: | ---: | --- |\n");
    output.push_str("| MLX GPU | not measured | not measured | not measured | not measured |\n");
    output.push_str("| ANE | not measured | not measured | not measured | not measured |\n\n");
    output.push_str("### Table E. Query-tower component (informs D1; ms)\n\n");
    output.push_str("| bucket set | first-ever load | relaunch load | p50 @32 | p50 @64 | p50 @128 | MLX GPU p50 @ same length | chosen |\n");
    output.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |\n");
    output.push_str("| {32} | | | | n/a | n/a | | |\n");
    output.push_str("| {32, 64} | | | | | n/a | | |\n");
    output.push_str("| {32, 64, 128} | | | | | | | |\n\n");
    output.push_str(&format!(
        "Queries truncated by `max_tokens` on FiQA: {} of 648.\n",
        results
            .truncated_queries
            .map(|value| value.to_string())
            .unwrap_or_default()
    ));
    output
}

fn steady_cell<'a>(
    results: &'a Results,
    leg: &str,
    backend: &str,
    tier: &str,
) -> Option<&'a Summary> {
    results
        .steady
        .iter()
        .find(|cell| cell.leg == leg && cell.backend == backend && cell.store_tier == tier)
        .map(|cell| &cell.summary)
}

fn optional_integer(value: Option<u64>) -> String {
    value.map(|number| number.to_string()).unwrap_or_default()
}

fn optional_number(value: Option<f64>) -> String {
    value
        .map(|number| format!("{number:.6}"))
        .unwrap_or_default()
}

fn optional_component(value: Option<f64>) -> String {
    value
        .map(|number| format!("{number:.3}"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_are_nearest_rank_on_the_sorted_sample() {
        let summary = percentiles(&[9.0, 1.0, 5.0, 3.0, 7.0]).expect("summary");
        assert_eq!(summary.p50, 5.0);
        assert_eq!(summary.p95, 9.0);
        assert_eq!(summary.p99, 9.0);
        assert_eq!(summary.mean, 5.0);
    }

    #[test]
    fn recall_at_k_counts_shared_ids_over_k() {
        assert_eq!(recall_at_k(&[1, 2, 3, 4], &[4, 3, 9, 8]), 0.5);
    }

    #[test]
    fn render_tables_emits_every_cell_row_with_blank_placeholders_for_missing_results() {
        let rendered = render_tables(&Results::default());
        for row in [
            "| Dense | MLX GPU |",
            "| Dense | ANE |",
            "| Lexical | (none) |",
            "| Hybrid | MLX GPU |",
            "| Hybrid | ANE |",
        ] {
            assert!(rendered.contains(row), "missing row {row}");
        }
        assert!(rendered.contains("| MLX GPU | first-ever | | | | |"));
        assert!(rendered.contains("| {32, 64, 128} | | | | | | | |"));
    }
}
