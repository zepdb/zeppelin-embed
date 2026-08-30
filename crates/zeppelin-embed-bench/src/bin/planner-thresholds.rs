//! Control-armed calibration harness for Task 16 planner thresholds.

use std::collections::BTreeSet;
use std::error::Error;
use std::hint::black_box;
use std::io;
use std::time::Instant;

use tempfile::{TempDir, tempdir};
use zeppelin_embed::fts::bm25::Bm25Params;
use zeppelin_embed::fts::index::{DEFAULT_FIELD, Document, LexicalIndex, SegmentIndex};
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fts::tokenizer::{Analyzer, Profile};
use zeppelin_embed::graph::block::{GraphNodeBlockBuild, GraphNodeBlockInput, GraphNodeLayout};
use zeppelin_embed::graph::search::{
    FilteredGraphSearchOutcome, GraphSearchRequest, GraphSearchScratch, GraphSearcher,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, DocBitmap, Schema};
use zeppelin_embed::planner::{LexicalBranch, search_lexical_filtered};
use zeppelin_embed::quant::{Bit4Factors, Bit4Query, prepare_bit4_query, quantize_bit4};
use zeppelin_embed::scan::{
    ScanCandidate, ScanQuery, ScanRequest, ScanRows, calibration_gather_top_k,
    calibration_masked_top_k, top_k,
};
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::reader::SegmentReader;
use zeppelin_embed::segment::writer::{SegmentBuild, SegmentFactors, write_segment_with_graph};
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed_bench::platform::memory_graph::verify_bench_profile;
use zeppelin_embed_bench::platform::taint::{detect_taint, print_taint_status};

const TOP_K: usize = 10;
const CONTROL_SEED: u64 = 0x16_c011_7a01;
const GRAPH_SEED: u64 = 0x16_6a_4f_01;
const LOAD_LIMIT: f64 = 1.0;
const GRAPH_MULTIPLIERS: [usize; 3] = [1, 2, 3];

#[derive(Clone, Copy, Debug)]
struct Config {
    smoke: bool,
    inject_control_drift: bool,
    iterations: usize,
    max_control_drift_percent: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            smoke: false,
            inject_control_drift: false,
            iterations: 5,
            max_control_drift_percent: 10.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Observation {
    nanoseconds_per_iteration: f64,
    checksum: u64,
    executed: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CellStatus {
    Valid,
    VoidControlDrift,
}

#[derive(Clone, Copy, Debug)]
struct Bracket {
    before: Observation,
    measurement: Observation,
    after: Observation,
    drift_percent: f64,
    status: CellStatus,
}

struct GraphBracket {
    bracket: Bracket,
    effective_ef: usize,
    budget: usize,
    disposition: &'static str,
    visited: usize,
}

struct VectorFixture {
    rows: usize,
    query: Bit4Query,
    codes: Vec<u8>,
    factors: Vec<Bit4Factors>,
}

struct LexicalFixture {
    rows: usize,
    index: LexicalIndex,
    query: TermQuery,
}

struct GraphFixture {
    _directory: TempDir,
    reader: SegmentReader,
    query: Vec<f32>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("planner-thresholds: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    verify_bench_profile()?;
    let config = parse_config()?;
    if config.iterations == 0 {
        return Err(io::Error::other("executed == 0: --iterations must be positive").into());
    }

    let (vector_rows, vector_dimensions, lexical_rows, graph_rows) = if config.smoke {
        (4_096, 128, 2_048, 1_024)
    } else {
        (1_000_000, 768, 100_000, 8_192)
    };
    let vector_points = point_counts(vector_rows, config.smoke);
    let lexical_points = point_counts(lexical_rows, config.smoke);
    let graph_points = point_counts(graph_rows, config.smoke)
        .into_iter()
        .filter(|point| *point > 64)
        .collect::<Vec<_>>();
    if vector_points.len() <= 2 || lexical_points.len() <= 2 || graph_points.len() <= 2 {
        return Err(
            io::Error::other("every calibration sweep requires more than two points").into(),
        );
    }

    println!(
        "PLANNER_THRESHOLDS_CONTEXT mode={} profile=bench opt_level={} iterations={} control_drift_limit_percent={:.3} vector_shape={}x{} vector_points={} vector_cells={} lexical_rows={} lexical_points={} lexical_cells={} graph_rows={} graph_points={} graph_multiplier_points={} graph_cells={} total_cells={} status={}",
        if config.smoke { "smoke" } else { "calibration" },
        env!("ZEPPELIN_BENCH_OPT_LEVEL"),
        config.iterations,
        config.max_control_drift_percent,
        vector_rows,
        vector_dimensions,
        vector_points.len(),
        vector_points.len() * 2,
        lexical_rows,
        lexical_points.len(),
        lexical_points.len() * 2,
        graph_rows,
        graph_points.len(),
        GRAPH_MULTIPLIERS.len(),
        graph_points.len() * GRAPH_MULTIPLIERS.len(),
        vector_points.len() * 2
            + lexical_points.len() * 2
            + graph_points.len() * GRAPH_MULTIPLIERS.len(),
        if config.smoke {
            "CONTAMINATED_NOT_EVIDENCE"
        } else {
            "CANDIDATE_EVIDENCE"
        }
    );
    println!(
        "PLANNER_THRESHOLDS_HARDWARE os={} arch={} logical_cpus={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism()?.get()
    );
    let taint = detect_taint(LOAD_LIMIT);
    print_taint_status(&taint, LOAD_LIMIT, "planner-thresholds");

    let vector = build_vector_fixture(vector_rows, vector_dimensions)?;
    let reference = run_control(&vector, 1)?;
    require_executed(reference, "control reference")?;
    println!(
        "PLANNER_THRESHOLDS_CONTROL_REFERENCE checksum={} executed={} ns_per_iteration={:.3} historical_protocol=tasks/evidence/05-scan-wallclock.md",
        reference.checksum, reference.executed, reference.nanoseconds_per_iteration
    );

    let mut void_cells = 0_usize;
    let mut executed_cells = 0_usize;
    let mut inject_pending = config.inject_control_drift;

    run_vector_sweep(
        config,
        &vector,
        &vector_points,
        reference.checksum,
        &mut inject_pending,
        &mut executed_cells,
        &mut void_cells,
    )?;

    let lexical = build_lexical_fixture(lexical_rows)?;
    run_lexical_sweep(
        config,
        &vector,
        &lexical,
        &lexical_points,
        reference.checksum,
        &mut inject_pending,
        &mut executed_cells,
        &mut void_cells,
    )?;

    let graph = build_graph_fixture(graph_rows)?;
    run_graph_sweep(
        config,
        &vector,
        &graph,
        &graph_points,
        reference.checksum,
        &mut inject_pending,
        &mut executed_cells,
        &mut void_cells,
    )?;

    if executed_cells == 0 {
        return Err(io::Error::other("executed == 0: no calibration cells ran").into());
    }
    if config.inject_control_drift && void_cells == 0 {
        return Err(io::Error::other("injected control drift was not detected").into());
    }
    if config.inject_control_drift {
        println!("CONTROL_DRIFT_SELF_TEST status=PASS detected_void_cells={void_cells}");
    }
    println!(
        "PLANNER_THRESHOLDS_SUMMARY executed_cells={executed_cells} void_control_drift={void_cells} status={}",
        if config.smoke {
            "CONTAMINATED_NOT_EVIDENCE"
        } else if void_cells == 0 {
            "VALID"
        } else {
            "FAILED_VOID_CELLS"
        }
    );
    if !config.smoke && void_cells != 0 {
        return Err(io::Error::other(format!(
            "{void_cells} calibration cells were void on control drift"
        ))
        .into());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_vector_sweep(
    config: Config,
    fixture: &VectorFixture,
    points: &[usize],
    control_checksum: u64,
    inject_pending: &mut bool,
    executed_cells: &mut usize,
    void_cells: &mut usize,
) -> Result<(), Box<dyn Error>> {
    for (point_index, allowed) in points.iter().copied().enumerate() {
        let allow_list = deterministic_allow_list(fixture.rows, allowed)?;
        let gather = vector_candidates(fixture, &allow_list, LexicalBranch::AllowListDrive)?;
        let masked = vector_candidates(fixture, &allow_list, LexicalBranch::PostCheck)?;
        if gather != masked {
            return Err(io::Error::other(format!(
                "vector branches disagree at allow-list cardinality {allowed}"
            ))
            .into());
        }
        for branch in [LexicalBranch::AllowListDrive, LexicalBranch::PostCheck] {
            let inject = take_injection(inject_pending);
            let bracket = bracket(fixture, config, control_checksum, inject, || {
                measure_vector(fixture, &allow_list, branch, config.iterations)
            })?;
            record_cell(bracket, executed_cells, void_cells)?;
            println!(
                "PLANNER_THRESHOLD_CELL family=vector point_index={point_index} point_count={} allowed={allowed} branch={} measurement_ns_per_iteration={:.3} measurement_checksum={} measurement_executed={} control_before_ns={:.3} control_after_ns={:.3} control_checksum={} control_executed_before={} control_executed_after={} control_drift_percent={:.3} status={}",
                points.len(),
                branch_label(branch),
                bracket.measurement.nanoseconds_per_iteration,
                bracket.measurement.checksum,
                bracket.measurement.executed,
                bracket.before.nanoseconds_per_iteration,
                bracket.after.nanoseconds_per_iteration,
                bracket.before.checksum,
                bracket.before.executed,
                bracket.after.executed,
                bracket.drift_percent,
                status_label(bracket.status),
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_lexical_sweep(
    config: Config,
    control: &VectorFixture,
    fixture: &LexicalFixture,
    points: &[usize],
    control_checksum: u64,
    inject_pending: &mut bool,
    executed_cells: &mut usize,
    void_cells: &mut usize,
) -> Result<(), Box<dyn Error>> {
    for (point_index, allowed) in points.iter().copied().enumerate() {
        let allow_list = deterministic_allow_list(fixture.rows, allowed)?;
        let allow_lists = [allow_list];
        let drive = search_lexical_filtered(
            &fixture.index,
            &fixture.query,
            TOP_K,
            Bm25Params::default(),
            &allow_lists,
            Some(LexicalBranch::AllowListDrive),
        )?;
        let post = search_lexical_filtered(
            &fixture.index,
            &fixture.query,
            TOP_K,
            Bm25Params::default(),
            &allow_lists,
            Some(LexicalBranch::PostCheck),
        )?;
        if drive.result.hits != post.result.hits {
            return Err(io::Error::other(format!(
                "lexical branches disagree at allow-list cardinality {allowed}"
            ))
            .into());
        }
        for branch in [LexicalBranch::AllowListDrive, LexicalBranch::PostCheck] {
            let inject = take_injection(inject_pending);
            let bracket = bracket(control, config, control_checksum, inject, || {
                measure_lexical(fixture, &allow_lists, branch, config.iterations)
            })?;
            record_cell(bracket, executed_cells, void_cells)?;
            println!(
                "PLANNER_THRESHOLD_CELL family=lexical point_index={point_index} point_count={} allowed={allowed} branch={} measurement_ns_per_iteration={:.3} measurement_checksum={} measurement_executed={} control_before_ns={:.3} control_after_ns={:.3} control_checksum={} control_executed_before={} control_executed_after={} control_drift_percent={:.3} status={}",
                points.len(),
                branch_label(branch),
                bracket.measurement.nanoseconds_per_iteration,
                bracket.measurement.checksum,
                bracket.measurement.executed,
                bracket.before.nanoseconds_per_iteration,
                bracket.after.nanoseconds_per_iteration,
                bracket.before.checksum,
                bracket.before.executed,
                bracket.after.executed,
                bracket.drift_percent,
                status_label(bracket.status),
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_graph_sweep(
    config: Config,
    control: &VectorFixture,
    fixture: &GraphFixture,
    points: &[usize],
    control_checksum: u64,
    inject_pending: &mut bool,
    executed_cells: &mut usize,
    void_cells: &mut usize,
) -> Result<(), Box<dyn Error>> {
    for (point_index, allowed) in points.iter().copied().enumerate() {
        let allow_list =
            deterministic_allow_list(fixture.reader.meta().row_count as usize, allowed)?;
        for multiplier in GRAPH_MULTIPLIERS {
            let inject = take_injection(inject_pending);
            let result = bracket_graph(
                control,
                fixture,
                &allow_list,
                multiplier,
                config,
                control_checksum,
                inject,
            )?;
            record_cell(result.bracket, executed_cells, void_cells)?;
            println!(
                "PLANNER_THRESHOLD_CELL family=filter_only_budget point_index={point_index} point_count={} allowed={allowed} multiplier={multiplier} multiplier_point_count={} effective_ef={} budget={} disposition={} visited={} measurement_ns_per_iteration={:.3} measurement_checksum={} measurement_executed={} control_before_ns={:.3} control_after_ns={:.3} control_checksum={} control_executed_before={} control_executed_after={} control_drift_percent={:.3} status={}",
                points.len(),
                GRAPH_MULTIPLIERS.len(),
                result.effective_ef,
                result.budget,
                result.disposition,
                result.visited,
                result.bracket.measurement.nanoseconds_per_iteration,
                result.bracket.measurement.checksum,
                result.bracket.measurement.executed,
                result.bracket.before.nanoseconds_per_iteration,
                result.bracket.after.nanoseconds_per_iteration,
                result.bracket.before.checksum,
                result.bracket.before.executed,
                result.bracket.after.executed,
                result.bracket.drift_percent,
                status_label(result.bracket.status),
            );
        }
    }
    Ok(())
}

fn bracket(
    control: &VectorFixture,
    config: Config,
    control_checksum: u64,
    inject_drift: bool,
    measure: impl FnOnce() -> Result<Observation, Box<dyn Error>>,
) -> Result<Bracket, Box<dyn Error>> {
    let before = run_control(control, config.iterations)?;
    let measurement = measure()?;
    let mut after = run_control(control, config.iterations)?;
    if inject_drift {
        after.nanoseconds_per_iteration *= 4.0;
    }
    require_executed(before, "control before")?;
    require_executed(measurement, "measurement")?;
    require_executed(after, "control after")?;
    if before.checksum != control_checksum || after.checksum != control_checksum {
        return Err(io::Error::other(format!(
            "known control checksum drifted: expected {control_checksum}, before={}, after={}",
            before.checksum, after.checksum
        ))
        .into());
    }
    let drift_percent = percent_drift(
        before.nanoseconds_per_iteration,
        after.nanoseconds_per_iteration,
    )?;
    let status = if drift_percent > config.max_control_drift_percent {
        CellStatus::VoidControlDrift
    } else {
        CellStatus::Valid
    };
    if inject_drift && status != CellStatus::VoidControlDrift {
        return Err(io::Error::other("injected control drift did not void its cell").into());
    }
    Ok(Bracket {
        before,
        measurement,
        after,
        drift_percent,
        status,
    })
}

fn bracket_graph(
    control: &VectorFixture,
    fixture: &GraphFixture,
    allow_list: &DocBitmap,
    multiplier: usize,
    config: Config,
    control_checksum: u64,
    inject_drift: bool,
) -> Result<GraphBracket, Box<dyn Error>> {
    let graph = fixture.reader.graph_node_blocks()?;
    let rows = graph.node_count() as usize;
    let allowed = usize::try_from(allow_list.cardinality())?;
    let k = TOP_K.min(allowed);
    let base_request = GraphSearchRequest::new(&fixture.query, k, GRAPH_SEED);
    let base_ef = base_request.effective_ef(rows)?;
    let effective_ef = base_ef
        .checked_mul(rows)
        .map(|value| value.div_ceil(allowed))
        .ok_or_else(|| io::Error::other("graph selectivity scaling overflowed"))?
        .min(allowed)
        .max(k);
    let budget = effective_ef
        .checked_mul(usize::from(graph.layout().max_degree()))
        .and_then(|value| value.checked_mul(multiplier))
        .ok_or_else(|| io::Error::other("graph filter budget overflowed"))?
        .min(rows);
    let mut disposition = "unset";
    let mut visited = 0_usize;
    let before = run_control(control, config.iterations)?;
    let measurement = measure_graph(
        fixture,
        allow_list,
        effective_ef,
        budget,
        config.iterations,
        &mut disposition,
        &mut visited,
    )?;
    let mut after = run_control(control, config.iterations)?;
    if inject_drift {
        after.nanoseconds_per_iteration *= 4.0;
    }
    require_executed(before, "control before")?;
    require_executed(measurement, "measurement")?;
    require_executed(after, "control after")?;
    if before.checksum != control_checksum || after.checksum != control_checksum {
        return Err(io::Error::other("known control checksum drifted around graph cell").into());
    }
    let drift_percent = percent_drift(
        before.nanoseconds_per_iteration,
        after.nanoseconds_per_iteration,
    )?;
    let status = if drift_percent > config.max_control_drift_percent {
        CellStatus::VoidControlDrift
    } else {
        CellStatus::Valid
    };
    if inject_drift && status != CellStatus::VoidControlDrift {
        return Err(io::Error::other("injected control drift did not void its graph cell").into());
    }
    Ok(GraphBracket {
        bracket: Bracket {
            before,
            measurement,
            after,
            drift_percent,
            status,
        },
        effective_ef,
        budget,
        disposition,
        visited,
    })
}

fn measure_vector(
    fixture: &VectorFixture,
    allow_list: &DocBitmap,
    branch: LexicalBranch,
    iterations: usize,
) -> Result<Observation, Box<dyn Error>> {
    let started = Instant::now();
    let mut expected_checksum = None;
    for _ in 0..iterations {
        let candidates = vector_candidates(fixture, allow_list, branch)?;
        let checksum = checksum_scan(&candidates);
        require_stable_checksum(&mut expected_checksum, checksum, "vector measurement")?;
        black_box(candidates);
    }
    observation(started, iterations, expected_checksum)
}

fn vector_candidates(
    fixture: &VectorFixture,
    allow_list: &DocBitmap,
    branch: LexicalBranch,
) -> Result<Vec<ScanCandidate>, Box<dyn Error>> {
    let query = ScanQuery::Bit4(&fixture.query);
    let rows = ScanRows::Bit4RowMajor {
        codes: &fixture.codes,
        factors: &fixture.factors,
    };
    match branch {
        LexicalBranch::AllowListDrive => {
            Ok(calibration_gather_top_k(query, rows, allow_list, TOP_K)?)
        }
        LexicalBranch::PostCheck => Ok(calibration_masked_top_k(query, rows, allow_list, TOP_K)?),
    }
}

fn measure_lexical(
    fixture: &LexicalFixture,
    allow_lists: &[DocBitmap],
    branch: LexicalBranch,
    iterations: usize,
) -> Result<Observation, Box<dyn Error>> {
    let started = Instant::now();
    let mut expected_checksum = None;
    for _ in 0..iterations {
        let outcome = search_lexical_filtered(
            &fixture.index,
            &fixture.query,
            TOP_K,
            Bm25Params::default(),
            allow_lists,
            Some(branch),
        )?;
        let checksum = outcome
            .result
            .hits
            .iter()
            .fold(0xcbf2_9ce4_8422_2325, |hash, hit| {
                mix(hash, u64::from(hit.doc.segment))
                    ^ mix(u64::from(hit.doc.row), hit.score.to_bits())
            });
        require_stable_checksum(&mut expected_checksum, checksum, "lexical measurement")?;
        black_box(outcome);
    }
    observation(started, iterations, expected_checksum)
}

#[allow(clippy::too_many_arguments)]
fn measure_graph(
    fixture: &GraphFixture,
    allow_list: &DocBitmap,
    effective_ef: usize,
    budget: usize,
    iterations: usize,
    disposition: &mut &'static str,
    visited: &mut usize,
) -> Result<Observation, Box<dyn Error>> {
    let graph = fixture.reader.graph_node_blocks()?;
    let rescore = fixture.reader.rescore_f32()?;
    let mut scratch = GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())?;
    let mut searcher = GraphSearcher::new(graph, rescore, &mut scratch)?;
    let k = TOP_K.min(usize::try_from(allow_list.cardinality())?);
    let request = GraphSearchRequest::new(&fixture.query, k, GRAPH_SEED).with_ef(effective_ef);
    let started = Instant::now();
    let mut expected_checksum = None;
    let mut expected_disposition = None;
    let mut expected_visited = None;
    for _ in 0..iterations {
        let outcome = searcher.search_filtered(request, allow_list, budget, None)?;
        let (cell_disposition, cell_visited, checksum) = match outcome {
            FilteredGraphSearchOutcome::Traversed(ref result) => {
                let work = result.counters().deterministic_work();
                let checksum =
                    result
                        .candidates()
                        .iter()
                        .fold(0xcbf2_9ce4_8422_2325, |hash, candidate| {
                            mix(hash, u64::from(candidate.row_id()))
                                ^ mix(
                                    u64::from(candidate.row_id()),
                                    candidate.distance().to_bits(),
                                )
                        });
                ("traversed", work.visited, checksum)
            }
            FilteredGraphSearchOutcome::VisitedBudgetExceeded { counters, .. } => {
                let fallback = calibration_gather_top_k(
                    ScanQuery::F32(&fixture.query),
                    ScanRows::F32BorrowedRowMajor(rescore),
                    allow_list,
                    k,
                )?;
                (
                    "exact_fallback",
                    counters.deterministic_work().visited,
                    checksum_scan(&fallback),
                )
            }
        };
        require_stable_value(
            &mut expected_disposition,
            cell_disposition,
            "graph disposition",
        )?;
        require_stable_value(&mut expected_visited, cell_visited, "graph visited count")?;
        require_stable_checksum(&mut expected_checksum, checksum, "graph measurement")?;
        black_box(outcome);
    }
    *disposition = expected_disposition
        .ok_or_else(|| io::Error::other("executed == 0: graph disposition absent"))?;
    *visited = expected_visited
        .ok_or_else(|| io::Error::other("executed == 0: graph visited count absent"))?;
    observation(started, iterations, expected_checksum)
}

fn run_control(fixture: &VectorFixture, iterations: usize) -> Result<Observation, Box<dyn Error>> {
    let request = ScanRequest {
        query: ScanQuery::Bit4(&fixture.query),
        rows: ScanRows::Bit4RowMajor {
            codes: &fixture.codes,
            factors: &fixture.factors,
        },
        row_mask: None,
    };
    let started = Instant::now();
    let mut expected_checksum = None;
    for _ in 0..iterations {
        let candidates = top_k(request, TOP_K)?;
        let checksum = checksum_scan(&candidates);
        require_stable_checksum(&mut expected_checksum, checksum, "control")?;
        black_box(candidates);
    }
    observation(started, iterations, expected_checksum)
}

fn observation(
    started: Instant,
    executed: usize,
    checksum: Option<u64>,
) -> Result<Observation, Box<dyn Error>> {
    if executed == 0 {
        return Err(io::Error::other("executed == 0").into());
    }
    let checksum = checksum.ok_or_else(|| io::Error::other("executed == 0: no checksum"))?;
    Ok(Observation {
        nanoseconds_per_iteration: started.elapsed().as_secs_f64() * 1e9 / executed as f64,
        checksum,
        executed,
    })
}

fn build_vector_fixture(rows: usize, dimensions: usize) -> Result<VectorFixture, Box<dyn Error>> {
    let row_bytes = dimensions.div_ceil(2);
    let cluster_count = 12_usize.min(rows.max(2));
    let mut random = SplitMix64::new(0x05_c1_a5_7e_ed);
    let mut centers = Vec::with_capacity(cluster_count);
    for _ in 0..cluster_count {
        let mut center = (0..dimensions)
            .map(|_| random.gaussian())
            .collect::<Vec<_>>();
        normalize(&mut center);
        centers.push(center);
    }
    let templates = centers
        .iter()
        .map(|center| {
            let mut encoded = vec![0_u8; row_bytes];
            let factor = quantize_bit4(center, &mut encoded)?;
            Ok((encoded, factor))
        })
        .collect::<Result<Vec<_>, zeppelin_embed::quant::QuantError>>()?;
    let total_weight = cluster_count * (cluster_count + 1) / 2;
    let mut codes = vec![0_u8; rows * row_bytes];
    let mut factors = Vec::with_capacity(rows);
    for (row, encoded) in codes.chunks_exact_mut(row_bytes).enumerate() {
        let ticket = row % total_weight;
        let mut cumulative = 0_usize;
        let mut label = 0_usize;
        for candidate in 0..cluster_count {
            cumulative += candidate + 1;
            if ticket < cumulative {
                label = candidate;
                break;
            }
        }
        let (template, factor) = templates
            .get(label)
            .ok_or_else(|| io::Error::other("cluster label escaped template table"))?;
        encoded.copy_from_slice(template);
        factors.push(*factor);
    }
    let mut query = centers
        .first()
        .ok_or_else(|| io::Error::other("vector fixture has no cluster center"))?
        .iter()
        .map(|value| *value + 0.035 * random.gaussian())
        .collect::<Vec<_>>();
    normalize(&mut query);
    Ok(VectorFixture {
        rows,
        query: prepare_bit4_query(&query, CONTROL_SEED)?,
        codes,
        factors,
    })
}

fn build_lexical_fixture(rows: usize) -> Result<LexicalFixture, Box<dyn Error>> {
    let analyzer = Analyzer::new(Profile::Code.config())?;
    let mut segment = SegmentIndex::new();
    for row in 0..rows {
        let text = if row.is_multiple_of(17) {
            "filter planner exact engine rare selective"
        } else if row.is_multiple_of(5) {
            "filter planner exact engine"
        } else if row.is_multiple_of(2) {
            "filter planner exact"
        } else {
            "filter planner"
        };
        segment.push_document(&analyzer, &Document::with_text(text))?;
    }
    let mut index = LexicalIndex::new();
    index.push_segment(segment)?;
    Ok(LexicalFixture {
        rows,
        index,
        query: TermQuery::flat(
            vec![
                b"filter".to_vec(),
                b"planner".to_vec(),
                b"exact".to_vec(),
                b"engine".to_vec(),
            ],
            &[DEFAULT_FIELD],
        ),
    })
}

fn build_graph_fixture(rows: usize) -> Result<GraphFixture, Box<dyn Error>> {
    const DIMENSIONS: usize = 128;
    const MAX_DEGREE: u8 = 16;
    let directory = tempdir()?;
    let id = SegmentId::new(0x0001_6000_ca11, [0xca; 10]);
    let mut vectors = Vec::with_capacity(rows * DIMENSIONS);
    for row in 0..rows {
        let mut vector = (0..DIMENSIONS)
            .map(|dimension| {
                let mixed = row
                    .wrapping_mul(31)
                    .wrapping_add(dimension.wrapping_mul(17))
                    .wrapping_add((row ^ dimension).wrapping_mul(7));
                (mixed % 257) as f32 / 128.0 - 1.0
            })
            .collect::<Vec<_>>();
        normalize(&mut vector);
        vectors.extend(vector);
    }
    let row_bytes = DIMENSIONS.div_ceil(2);
    let mut codes = vec![0_u8; rows * row_bytes];
    let mut factors = Vec::with_capacity(rows);
    for (vector, encoded) in vectors
        .chunks_exact(DIMENSIONS)
        .zip(codes.chunks_exact_mut(row_bytes))
    {
        factors.push(quantize_bit4(vector, encoded)?);
    }
    let mut columns = ColumnStoreBuilder::new(Schema::new(Vec::new())?);
    for row in 0..rows {
        columns.push_row(row as i64, &[])?;
    }
    let columns = columns.finish()?;
    let alive = AliveSet::new(u32::try_from(rows)?);
    let offsets = [1_usize, 2, 4, 8, 16, 32, 64, 128];
    let neighbors = (0..rows)
        .map(|row| {
            let mut values = Vec::with_capacity(usize::from(MAX_DEGREE));
            for offset in offsets {
                values.push(u32::try_from((row + offset) % rows)?);
                values.push(u32::try_from((row + rows - offset % rows) % rows)?);
            }
            Ok(values)
        })
        .collect::<Result<Vec<_>, std::num::TryFromIntError>>()?;
    let nodes = codes
        .chunks_exact(row_bytes)
        .zip(&factors)
        .zip(&neighbors)
        .enumerate()
        .map(|(row, ((codes, factors), neighbors))| GraphNodeBlockInput {
            codes,
            factors: *factors,
            flags: u8::from(row < 4),
            neighbors,
        })
        .collect::<Vec<_>>();
    write_segment_with_graph(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme: 4,
            dims: DIMENSIONS as u32,
            codes: &codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &vectors,
            columns: &columns,
            alive: &alive,
        },
        GraphNodeBlockBuild {
            layout: GraphNodeLayout::new(DIMENSIONS as u32, DIMENSIONS as u32, MAX_DEGREE)?,
            nodes: &nodes,
        },
        DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)?,
    )?;
    let reader = SegmentReader::open(&StdVfs, &directory.path().join(id.file_name()), id)?;
    let query = vectors
        .get(0..DIMENSIONS)
        .ok_or_else(|| io::Error::other("graph fixture has no query row"))?
        .to_vec();
    Ok(GraphFixture {
        _directory: directory,
        reader,
        query,
    })
}

fn point_counts(rows: usize, smoke: bool) -> Vec<usize> {
    let mut points = BTreeSet::new();
    let absolutes: &[usize] = if smoke {
        &[0, 1, 8, 64, 128, 512]
    } else {
        &[
            0, 1, 2, 4, 8, 16, 32, 64, 96, 128, 192, 256, 384, 512, 768, 1_024,
        ]
    };
    for point in absolutes {
        points.insert((*point).min(rows));
    }
    let divisors: &[usize] = if smoke {
        &[16, 4, 1]
    } else {
        &[256, 128, 64, 32, 16, 8, 4, 2, 1]
    };
    for divisor in divisors {
        points.insert(rows.div_ceil(*divisor));
    }
    points.into_iter().collect()
}

fn deterministic_allow_list(rows: usize, count: usize) -> Result<DocBitmap, Box<dyn Error>> {
    if count > rows || rows > u32::MAX as usize {
        return Err(io::Error::other("allow-list geometry exceeds u32 row space").into());
    }
    if rows == 0 {
        return Err(io::Error::other("allow-list row count must be positive").into());
    }
    let stride = coprime_stride(rows);
    let ids = (0..count)
        .map(|position| u32::try_from((position * stride) % rows))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DocBitmap::from_ids(ids))
}

fn coprime_stride(rows: usize) -> usize {
    let mut stride = (0x9e37_79b1_usize % rows).max(1);
    while gcd(stride, rows) != 1 {
        stride += 1;
    }
    stride
}

const fn gcd(mut left: usize, mut right: usize) -> usize {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

fn checksum_scan(candidates: &[ScanCandidate]) -> u64 {
    candidates
        .iter()
        .fold(0xcbf2_9ce4_8422_2325, |hash, candidate| {
            mix(hash, candidate.row_id as u64) ^ u64::from(candidate.score.to_bits())
        })
}

const fn mix(hash: u64, value: u64) -> u64 {
    (hash ^ value).wrapping_mul(0x0000_0100_0000_01b3)
}

fn require_stable_checksum(
    expected: &mut Option<u64>,
    actual: u64,
    label: &str,
) -> Result<(), Box<dyn Error>> {
    require_stable_value(expected, actual, label)
}

fn require_stable_value<T: Copy + Eq + std::fmt::Display>(
    expected: &mut Option<T>,
    actual: T,
    label: &str,
) -> Result<(), Box<dyn Error>> {
    match expected {
        Some(expected) if *expected != actual => Err(io::Error::other(format!(
            "{label} changed within one measurement: expected {expected}, got {actual}"
        ))
        .into()),
        Some(_) => Ok(()),
        None => {
            *expected = Some(actual);
            Ok(())
        }
    }
}

fn require_executed(observation: Observation, label: &str) -> Result<(), Box<dyn Error>> {
    if observation.executed == 0 {
        Err(io::Error::other(format!("executed == 0: {label}")).into())
    } else {
        Ok(())
    }
}

fn percent_drift(before: f64, after: f64) -> Result<f64, Box<dyn Error>> {
    if !before.is_finite() || !after.is_finite() || before <= 0.0 || after <= 0.0 {
        return Err(io::Error::other("control timing was non-positive or non-finite").into());
    }
    Ok((before - after).abs() / before.min(after) * 100.0)
}

fn record_cell(
    bracket: Bracket,
    executed_cells: &mut usize,
    void_cells: &mut usize,
) -> Result<(), Box<dyn Error>> {
    require_executed(bracket.measurement, "calibration cell")?;
    *executed_cells = executed_cells
        .checked_add(1)
        .ok_or_else(|| io::Error::other("executed cell count overflowed"))?;
    if bracket.status == CellStatus::VoidControlDrift {
        *void_cells = void_cells
            .checked_add(1)
            .ok_or_else(|| io::Error::other("void cell count overflowed"))?;
    }
    Ok(())
}

fn take_injection(pending: &mut bool) -> bool {
    let inject = *pending;
    *pending = false;
    inject
}

const fn branch_label(branch: LexicalBranch) -> &'static str {
    match branch {
        LexicalBranch::AllowListDrive => "allow_list",
        LexicalBranch::PostCheck => "masked_or_post_check",
    }
}

const fn status_label(status: CellStatus) -> &'static str {
    match status {
        CellStatus::Valid => "VALID",
        CellStatus::VoidControlDrift => "VOID_CONTROL_DRIFT",
    }
}

fn parse_config() -> Result<Config, Box<dyn Error>> {
    let mut config = Config::default();
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--smoke" => config.smoke = true,
            "--inject-control-drift" => config.inject_control_drift = true,
            "--iterations" => {
                config.iterations = arguments
                    .next()
                    .ok_or_else(|| io::Error::other("--iterations requires a value"))?
                    .parse()?;
            }
            "--max-control-drift-pct" => {
                config.max_control_drift_percent = arguments
                    .next()
                    .ok_or_else(|| io::Error::other("--max-control-drift-pct requires a value"))?
                    .parse()?;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: planner-thresholds [--smoke] [--inject-control-drift] [--iterations N] [--max-control-drift-pct PERCENT]"
                );
                std::process::exit(0);
            }
            _ => return Err(io::Error::other(format!("unknown argument {argument:?}")).into()),
        }
    }
    if !config.max_control_drift_percent.is_finite() || config.max_control_drift_percent < 0.0 {
        return Err(io::Error::other("control drift limit must be finite and non-negative").into());
    }
    if config.inject_control_drift && !config.smoke {
        return Err(io::Error::other("--inject-control-drift requires --smoke").into());
    }
    Ok(config)
}

#[derive(Clone, Copy, Debug)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn open_unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) * (1.0 / 9_007_199_254_740_992.0)
    }

    fn gaussian(&mut self) -> f32 {
        let radius = (-2.0 * self.open_unit().ln()).sqrt();
        let angle = std::f64::consts::TAU * self.open_unit();
        (radius * angle.cos()) as f32
    }
}

fn normalize(values: &mut [f32]) {
    let length = values
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    if length == 0.0 {
        return;
    }
    for value in values {
        *value = (f64::from(*value) / length) as f32;
    }
}

#[cfg(test)]
mod tests {
    use super::{CellStatus, percent_drift, point_counts};

    #[test]
    fn sweep_has_more_than_two_points() {
        assert!(point_counts(1_000_000, false).len() > 2);
        assert!(point_counts(4_096, true).len() > 2);
    }

    #[test]
    fn injected_control_drift_crosses_the_guard() {
        let drift = percent_drift(100.0, 400.0).expect("positive finite controls");
        let status = if drift > 10.0 {
            CellStatus::VoidControlDrift
        } else {
            CellStatus::Valid
        };
        assert_eq!(status, CellStatus::VoidControlDrift);
    }
}
