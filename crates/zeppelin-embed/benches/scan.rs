use std::error::Error;
use std::io::Write;
use std::mem::size_of;
use std::time::{Duration, Instant};

use zeppelin_embed::quant::{Bit4Factors, Bit4Query, Int8Query, prepare_int8_query, quantize_bit4};
use zeppelin_embed::scan::pdx::PdxMatrix;
use zeppelin_embed::scan::{
    Int8Factors, ScanCandidate, ScanOptions, ScanOutcome, ScanQuery, ScanRequest, ScanRows,
    top_k_with_options,
};

#[path = "scan/repeat.rs"]
pub(crate) mod scan_repeat;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scheme {
    F32,
    F16,
    Int8,
    Bit4,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FixtureKind {
    Degenerate,
    Clustered,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Layout {
    Pdx,
    RowMajor,
    Both,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScanLayout {
    Pdx,
    RowMajor,
}

#[derive(Clone, Copy, Debug)]
struct Config {
    scheme: Scheme,
    fixture: FixtureKind,
    layout: Layout,
    rows: usize,
    dimensions: usize,
    k: usize,
    threads: usize,
    block_rows: usize,
    iterations: usize,
    repeats: usize,
    smoke: bool,
    show_help: bool,
}

enum Fixture {
    F32 {
        query: Vec<f32>,
        rows: Representations<f32>,
    },
    F16 {
        query: Vec<u16>,
        rows: Representations<u16>,
    },
    Int8 {
        query: Int8Query,
        rows: Representations<i8>,
        factors: Vec<Int8Factors>,
    },
    Bit4 {
        query: Bit4Query,
        rows: Representations<u8>,
        factors: Vec<Bit4Factors>,
    },
}

struct Representations<T> {
    row_major: Option<Vec<T>>,
    pdx: Option<PdxMatrix>,
    pdx_encode_elapsed: Option<Duration>,
}

struct Bit4FixtureData {
    query: Vec<f32>,
    codes: Vec<u8>,
    factors: Vec<Bit4Factors>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ComparableCounters {
    dims_touched: u64,
    bytes_read: u64,
    threads_used: usize,
}

const HELP: &str = "scan benchmark\n\
\n\
USAGE:\n\
    cargo bench -p zeppelin-embed --bench scan -- [FLAGS]\n\
\n\
FLAGS:\n\
    --layout pdx|row-major|both  Scan layout (default: pdx). Both builds one source\n\
                                 fixture, retains both representations, and times\n\
                                 interleaved PDX/row-major pairs. Peak memory is\n\
                                 about 6 GiB for f32 at 1M x 768.\n\
    --scheme f32|f16|int8|bit4  Quantization scheme (default: bit4).\n\
    --shape ROWSxDIMENSIONS     Corpus shape (default: 100000x768).\n\
    --fixture degenerate|clustered\n\
                                 Source fixture (default: degenerate).\n\
    --k N                       Candidate count (default: 10).\n\
    --threads N                 Worker budget; zero selects detected P-cores.\n\
    --block-rows N              PDX rows per block (default: 64).\n\
    --iterations N              Scans per timed repeat (default: 10).\n\
    --repeats N                 Timed repeats over the one fixture (default: 1).\n\
    --test                      Smoke shape with no wall-clock measurements.\n\
    --help, -h                  Print this help.\n";

impl Fixture {
    fn scan(&self, config: Config, layout: ScanLayout) -> Result<ScanOutcome, Box<dyn Error>> {
        let request = match (self, layout) {
            (Self::F32 { query, rows }, ScanLayout::RowMajor) => ScanRequest {
                query: ScanQuery::F32(query),
                rows: ScanRows::F32RowMajor(rows.row_major()?),
                row_mask: None,
            },
            (Self::F32 { query, rows }, ScanLayout::Pdx) => ScanRequest {
                query: ScanQuery::F32(query),
                rows: ScanRows::F32Pdx(rows.pdx()?),
                row_mask: None,
            },
            (Self::F16 { query, rows }, ScanLayout::RowMajor) => ScanRequest {
                query: ScanQuery::F16(query),
                rows: ScanRows::F16RowMajor(rows.row_major()?),
                row_mask: None,
            },
            (Self::F16 { query, rows }, ScanLayout::Pdx) => ScanRequest {
                query: ScanQuery::F16(query),
                rows: ScanRows::F16Pdx(rows.pdx()?),
                row_mask: None,
            },
            (
                Self::Int8 {
                    query,
                    rows,
                    factors,
                },
                ScanLayout::RowMajor,
            ) => ScanRequest {
                query: ScanQuery::Int8(query),
                rows: ScanRows::Int8RowMajor {
                    codes: rows.row_major()?,
                    factors,
                },
                row_mask: None,
            },
            (
                Self::Int8 {
                    query,
                    rows,
                    factors,
                },
                ScanLayout::Pdx,
            ) => ScanRequest {
                query: ScanQuery::Int8(query),
                rows: ScanRows::Int8Pdx {
                    codes: rows.pdx()?,
                    factors,
                },
                row_mask: None,
            },
            (
                Self::Bit4 {
                    query,
                    rows,
                    factors,
                },
                ScanLayout::RowMajor,
            ) => ScanRequest {
                query: ScanQuery::Bit4(query),
                rows: ScanRows::Bit4RowMajor {
                    codes: rows.row_major()?,
                    factors,
                },
                row_mask: None,
            },
            (
                Self::Bit4 {
                    query,
                    rows,
                    factors,
                },
                ScanLayout::Pdx,
            ) => ScanRequest {
                query: ScanQuery::Bit4(query),
                rows: ScanRows::Bit4Pdx {
                    codes: rows.pdx()?,
                    factors,
                },
                row_mask: None,
            },
        };
        Ok(top_k_with_options(request, config.k, scan_options(config))?)
    }

    fn pdx_encode_elapsed(&self) -> Option<Duration> {
        match self {
            Self::F32 { rows, .. } => rows.pdx_encode_elapsed,
            Self::F16 { rows, .. } => rows.pdx_encode_elapsed,
            Self::Int8 { rows, .. } => rows.pdx_encode_elapsed,
            Self::Bit4 { rows, .. } => rows.pdx_encode_elapsed,
        }
    }

    fn has_pdx(&self) -> bool {
        match self {
            Self::F32 { rows, .. } => rows.pdx.is_some(),
            Self::F16 { rows, .. } => rows.pdx.is_some(),
            Self::Int8 { rows, .. } => rows.pdx.is_some(),
            Self::Bit4 { rows, .. } => rows.pdx.is_some(),
        }
    }

    fn assert_layout_candidates_equal(
        &self,
        pdx: &ScanOutcome,
        row_major: &ScanOutcome,
    ) -> Result<(), Box<dyn Error>> {
        match self {
            Self::F32 { query, rows } => assert_float_candidates_equal(
                Scheme::F32,
                &pdx.candidates,
                &row_major.candidates,
                |row_id| f32_score_bound(query, rows.row_major()?, row_id),
            ),
            Self::F16 { query, rows } => assert_float_candidates_equal(
                Scheme::F16,
                &pdx.candidates,
                &row_major.candidates,
                |row_id| f16_score_bound(query, rows.row_major()?, row_id),
            ),
            Self::Int8 { .. } => {
                assert_exact_candidates_equal(Scheme::Int8, &pdx.candidates, &row_major.candidates)
            }
            Self::Bit4 { .. } => {
                assert_exact_candidates_equal(Scheme::Bit4, &pdx.candidates, &row_major.candidates)
            }
        }
    }
}

impl<T> Representations<T> {
    fn row_major(&self) -> Result<&[T], Box<dyn Error>> {
        self.row_major
            .as_deref()
            .ok_or_else(|| "row-major representation was not built".into())
    }

    fn pdx(&self) -> Result<&PdxMatrix, Box<dyn Error>> {
        self.pdx
            .as_ref()
            .ok_or_else(|| "PDX representation was not built".into())
    }
}

fn assert_exact_candidates_equal(
    scheme: Scheme,
    pdx: &[ScanCandidate],
    row_major: &[ScanCandidate],
) -> Result<(), Box<dyn Error>> {
    if pdx.len() != row_major.len() {
        return Err(format!(
            "bitwise layout candidate length mismatch for scheme={scheme:?}: pdx={} row-major={}",
            pdx.len(),
            row_major.len()
        )
        .into());
    }
    for (position, (pdx_candidate, row_candidate)) in pdx.iter().zip(row_major).enumerate() {
        if pdx_candidate.row_id != row_candidate.row_id
            || pdx_candidate.score.to_bits() != row_candidate.score.to_bits()
        {
            return Err(format!(
                "bitwise layout candidate mismatch for scheme={scheme:?} position={position}: pdx={pdx_candidate:?} pdx_score_bits={:#010x} row-major={row_candidate:?} row_major_score_bits={:#010x}",
                pdx_candidate.score.to_bits(),
                row_candidate.score.to_bits()
            )
            .into());
        }
    }
    Ok(())
}

fn assert_float_candidates_equal<B>(
    scheme: Scheme,
    pdx: &[ScanCandidate],
    row_major: &[ScanCandidate],
    mut score_bound: B,
) -> Result<(), Box<dyn Error>>
where
    B: FnMut(usize) -> Result<f64, Box<dyn Error>>,
{
    if pdx.len() != row_major.len() {
        return Err(format!(
            "near-tie layout candidate length mismatch for scheme={scheme:?}: pdx={} row-major={}",
            pdx.len(),
            row_major.len()
        )
        .into());
    }
    for (position, (row_candidate, pdx_candidate)) in row_major.iter().zip(pdx).enumerate() {
        if pdx.iter().enumerate().any(|(other_position, candidate)| {
            other_position < position && candidate.row_id == pdx_candidate.row_id
        }) {
            return Err(format!(
                "near-tie layout output duplicated a candidate for scheme={scheme:?}: position={position} pdx_candidate={pdx_candidate:?} pdx={pdx:?}"
            )
            .into());
        }
        let tolerance = score_bound(row_candidate.row_id)?.max(score_bound(pdx_candidate.row_id)?);
        let error = (f64::from(pdx_candidate.score) - f64::from(row_candidate.score)).abs();
        if error > tolerance {
            return Err(format!(
                "near-tie layout score mismatch for scheme={scheme:?} position={position} pdx={pdx_candidate:?} row-major={row_candidate:?} error={error:?} tolerance={tolerance:?}"
            )
            .into());
        }
        let actual_rank = row_major
            .iter()
            .position(|candidate| candidate.row_id == pdx_candidate.row_id);
        let (cluster_start, cluster_end) = near_tie_cluster(row_major, position, &mut score_bound)?;
        match actual_rank {
            Some(rank) if rank >= cluster_start && rank <= cluster_end => {}
            Some(rank) => {
                return Err(format!(
                    "near-tie layout order mismatch for scheme={scheme:?}: candidate={pdx_candidate:?} pdx_position={position} row_major_position={rank} sanctioned_cluster={cluster_start}..={cluster_end}"
                )
                .into());
            }
            None if cluster_end + 1 == row_major.len() => {
                let boundary = row_major
                    .last()
                    .ok_or("near-tie boundary candidate was missing")?;
                let boundary_tolerance =
                    score_bound(boundary.row_id)?.max(score_bound(pdx_candidate.row_id)?);
                let boundary_error =
                    (f64::from(pdx_candidate.score) - f64::from(boundary.score)).abs();
                if boundary_error > 2.0 * boundary_tolerance {
                    return Err(format!(
                        "near-tie layout k-boundary exchange exceeded its bound for scheme={scheme:?}: boundary={boundary:?} exchanged={pdx_candidate:?} error={boundary_error:?} tolerance={boundary_tolerance:?}"
                    )
                    .into());
                }
            }
            None => {
                return Err(format!(
                    "near-tie layout candidate escaped its cluster for scheme={scheme:?}: candidate={pdx_candidate:?} pdx_position={position} sanctioned_cluster={cluster_start}..={cluster_end} row-major={row_major:?}"
                )
                .into());
            }
        }
    }
    Ok(())
}

fn near_tie_cluster<B>(
    candidates: &[ScanCandidate],
    position: usize,
    score_bound: &mut B,
) -> Result<(usize, usize), Box<dyn Error>>
where
    B: FnMut(usize) -> Result<f64, Box<dyn Error>>,
{
    let mut start = position;
    while start > 0 {
        let left = candidates
            .get(start - 1)
            .ok_or("near-tie cluster start was out of range")?;
        let right = candidates
            .get(start)
            .ok_or("near-tie cluster start was out of range")?;
        if !candidate_scores_are_near(left, right, score_bound)? {
            break;
        }
        start -= 1;
    }
    let mut end = position;
    while end + 1 < candidates.len() {
        let left = candidates
            .get(end)
            .ok_or("near-tie cluster end was out of range")?;
        let right = candidates
            .get(end + 1)
            .ok_or("near-tie cluster end was out of range")?;
        if !candidate_scores_are_near(left, right, score_bound)? {
            break;
        }
        end += 1;
    }
    Ok((start, end))
}

fn candidate_scores_are_near<B>(
    left: &ScanCandidate,
    right: &ScanCandidate,
    score_bound: &mut B,
) -> Result<bool, Box<dyn Error>>
where
    B: FnMut(usize) -> Result<f64, Box<dyn Error>>,
{
    let tolerance = score_bound(left.row_id)?.max(score_bound(right.row_id)?);
    Ok((f64::from(left.score) - f64::from(right.score)).abs() <= 2.0 * tolerance)
}

fn f32_score_bound(query: &[f32], rows: &[f32], row_id: usize) -> Result<f64, Box<dyn Error>> {
    let row = row_values(rows, query.len(), row_id)?;
    Ok(row
        .iter()
        .zip(query)
        .map(|(&row, &query)| f64::from(row).abs() * f64::from(query).abs())
        .sum::<f64>()
        * 1.0e-5)
}

fn f16_score_bound(query: &[u16], rows: &[u16], row_id: usize) -> Result<f64, Box<dyn Error>> {
    let row = row_values(rows, query.len(), row_id)?;
    Ok(row
        .iter()
        .zip(query)
        .map(|(&row, &query)| f16_to_f64(row).abs() * f16_to_f64(query).abs())
        .sum::<f64>()
        * 1.0e-5)
}

fn row_values<T>(rows: &[T], dimensions: usize, row_id: usize) -> Result<&[T], Box<dyn Error>> {
    let start = row_id
        .checked_mul(dimensions)
        .ok_or("row offset overflowed during layout comparison")?;
    let end = start
        .checked_add(dimensions)
        .ok_or("row end overflowed during layout comparison")?;
    rows.get(start..end)
        .ok_or_else(|| "candidate row was out of range during layout comparison".into())
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scheme: Scheme::Bit4,
            fixture: FixtureKind::Degenerate,
            layout: Layout::Pdx,
            rows: 100_000,
            dimensions: 768,
            k: 10,
            threads: 0,
            block_rows: 64,
            iterations: 10,
            repeats: 1,
            smoke: false,
            show_help: false,
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let config = parse_args()?;
    let stdout = std::io::stdout();
    if config.show_help {
        write!(stdout.lock(), "{HELP}")?;
        return Ok(());
    }
    scan_repeat::with_single_fixture(
        || build_fixture(config),
        |fixture| run_with_fixture(config, &mut stdout.lock(), fixture, measure_fixture),
    )
}

fn run_with_fixture<W, M>(
    config: Config,
    output: &mut W,
    fixture: &Fixture,
    mut measure: M,
) -> Result<(), Box<dyn Error>>
where
    W: Write,
    M: FnMut(&Fixture, Config, ScanLayout) -> Result<Duration, Box<dyn Error>>,
{
    write_pdx_encode_cost(output, config, fixture)?;
    match config.layout {
        Layout::Pdx => run_one_layout(config, output, fixture, ScanLayout::Pdx, &mut measure),
        Layout::RowMajor => {
            run_one_layout(config, output, fixture, ScanLayout::RowMajor, &mut measure)
        }
        Layout::Both => run_both_layouts(config, output, fixture, &mut measure),
    }
}

fn run_one_layout<W, M>(
    config: Config,
    output: &mut W,
    fixture: &Fixture,
    layout: ScanLayout,
    measure: &mut M,
) -> Result<(), Box<dyn Error>>
where
    W: Write,
    M: FnMut(&Fixture, Config, ScanLayout) -> Result<Duration, Box<dyn Error>>,
{
    let (_, counters) = validate_layout(config, fixture, layout)?;
    write_counters(output, config, layout, counters)?;
    if config.smoke {
        return Ok(());
    }
    scan_repeat::write_timed_repeats(
        output,
        fixture,
        config.repeats,
        config.iterations,
        |fixture| measure(fixture, config, layout),
    )
}

fn run_both_layouts<W, M>(
    config: Config,
    output: &mut W,
    fixture: &Fixture,
    measure: &mut M,
) -> Result<(), Box<dyn Error>>
where
    W: Write,
    M: FnMut(&Fixture, Config, ScanLayout) -> Result<Duration, Box<dyn Error>>,
{
    let (pdx, pdx_counters) = validate_layout(config, fixture, ScanLayout::Pdx)?;
    let (row_major, row_major_counters) = validate_layout(config, fixture, ScanLayout::RowMajor)?;
    fixture.assert_layout_candidates_equal(&pdx, &row_major)?;
    if pdx_counters.dims_touched != row_major_counters.dims_touched
        || pdx_counters.bytes_read != row_major_counters.bytes_read
    {
        return Err(format!(
            "layout comparison is void because deterministic work differs: pdx={pdx_counters:?} row-major={row_major_counters:?}"
        )
        .into());
    }
    write_counters(output, config, ScanLayout::Pdx, pdx_counters)?;
    write_counters(output, config, ScanLayout::RowMajor, row_major_counters)?;
    if config.smoke {
        return Ok(());
    }
    scan_repeat::write_interleaved_layout_repeats(
        output,
        fixture,
        config.repeats,
        config.iterations,
        |fixture, arm| {
            let layout = match arm {
                scan_repeat::InterleavedArm::A => ScanLayout::Pdx,
                scan_repeat::InterleavedArm::B => ScanLayout::RowMajor,
            };
            measure(fixture, config, layout)
        },
    )
}

fn validate_layout(
    config: Config,
    fixture: &Fixture,
    layout: ScanLayout,
) -> Result<(ScanOutcome, ComparableCounters), Box<dyn Error>> {
    let first = fixture.scan(config, layout)?;
    let second = fixture.scan(config, layout)?;
    if first != second {
        return Err(format!(
            "scan benchmark results or counters were nondeterministic for layout={layout:?}: first={first:?} second={second:?}"
        )
        .into());
    }
    let exhaustive_dimensions = u64::try_from(config.rows)?
        .checked_mul(u64::try_from(config.dimensions)?)
        .ok_or("benchmark dimension count overflowed")?;
    if first.stats.dims_touched != exhaustive_dimensions {
        return Err(format!(
            "exhaustive dims_touched mismatch: expected {exhaustive_dimensions}, got {}",
            first.stats.dims_touched
        )
        .into());
    }
    let payload_bytes_per_row = match config.scheme {
        Scheme::F32 => config
            .dimensions
            .checked_mul(size_of::<f32>())
            .ok_or("F32 payload width overflowed")?,
        Scheme::F16 => config
            .dimensions
            .checked_mul(size_of::<u16>())
            .ok_or("F16 payload width overflowed")?,
        Scheme::Int8 => config.dimensions,
        Scheme::Bit4 => config.dimensions.div_ceil(2),
    };
    let exhaustive_bytes = u64::try_from(config.rows)?
        .checked_mul(u64::try_from(payload_bytes_per_row)?)
        .ok_or("benchmark payload byte count overflowed")?;
    if layout == ScanLayout::Pdx && first.stats.bytes_read != exhaustive_bytes {
        return Err(format!(
            "exhaustive PDX bytes_read mismatch: expected {exhaustive_bytes}, got {}",
            first.stats.bytes_read
        )
        .into());
    }
    let counters = ComparableCounters {
        dims_touched: first.stats.dims_touched,
        // ScanStats::bytes_read is documented as a PDX payload counter. The
        // benchmark's row-major arm reports the equivalent logical payload
        // bytes so the layouts have a representation-neutral work contract.
        bytes_read: exhaustive_bytes,
        threads_used: first.stats.threads_used,
    };
    Ok((first, counters))
}

fn write_counters(
    output: &mut impl Write,
    config: Config,
    layout: ScanLayout,
    counters: ComparableCounters,
) -> Result<(), Box<dyn Error>> {
    writeln!(
        output,
        "deterministic counters: layout={} scheme={:?} fixture={:?} shape={}x{} block_rows={} dims_touched={} bytes_read={} threads_used={}",
        layout_name(layout),
        config.scheme,
        config.fixture,
        config.rows,
        config.dimensions,
        config.block_rows,
        counters.dims_touched,
        counters.bytes_read,
        counters.threads_used
    )?;
    Ok(())
}

fn write_pdx_encode_cost(
    output: &mut impl Write,
    config: Config,
    fixture: &Fixture,
) -> Result<(), Box<dyn Error>> {
    if !fixture.has_pdx() {
        return Ok(());
    }
    match fixture.pdx_encode_elapsed() {
        Some(elapsed) => writeln!(
            output,
            "PDX encode cost (outside scan timed region): {:.6} s",
            elapsed.as_secs_f64()
        )?,
        None if config.smoke => writeln!(
            output,
            "PDX encode cost (outside scan timed region): NOT MEASURED (--test)"
        )?,
        None => return Err("PDX encode cost was not recorded".into()),
    }
    Ok(())
}

const fn layout_name(layout: ScanLayout) -> &'static str {
    match layout {
        ScanLayout::Pdx => "pdx",
        ScanLayout::RowMajor => "row-major",
    }
}

fn measure_fixture(
    fixture: &Fixture,
    config: Config,
    layout: ScanLayout,
) -> Result<Duration, Box<dyn Error>> {
    let started = Instant::now();
    for _ in 0..config.iterations {
        std::hint::black_box(fixture.scan(config, layout)?);
    }
    Ok(started.elapsed())
}

fn build_fixture(config: Config) -> Result<Fixture, Box<dyn Error>> {
    match config.scheme {
        Scheme::F32 => build_f32(config),
        Scheme::F16 => build_f16(config),
        Scheme::Int8 => build_int8(config),
        Scheme::Bit4 => build_bit4(config),
    }
}

#[cfg(test)]
pub fn verify_test_layouts_equal_for_all_schemes() -> Result<(), Box<dyn Error>> {
    for scheme in [Scheme::F32, Scheme::F16, Scheme::Int8, Scheme::Bit4] {
        let config = Config {
            scheme,
            fixture: FixtureKind::Clustered,
            layout: Layout::Both,
            rows: 96,
            dimensions: 17,
            k: 10,
            threads: 1,
            block_rows: 8,
            iterations: 1,
            repeats: 1,
            smoke: true,
            show_help: false,
        };
        let fixture = build_fixture(config)?;
        let (pdx, pdx_counters) = validate_layout(config, &fixture, ScanLayout::Pdx)?;
        let (row_major, row_major_counters) =
            validate_layout(config, &fixture, ScanLayout::RowMajor)?;
        fixture.assert_layout_candidates_equal(&pdx, &row_major)?;
        if pdx_counters.dims_touched != row_major_counters.dims_touched
            || pdx_counters.bytes_read != row_major_counters.bytes_read
        {
            return Err(format!(
                "test layout work mismatch for scheme={scheme:?}: pdx={pdx_counters:?} row-major={row_major_counters:?}"
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
pub fn parse_layout_for_test(arguments: Vec<String>) -> Result<&'static str, Box<dyn Error>> {
    Ok(match parse_args_from(arguments)?.layout {
        Layout::Pdx => "pdx",
        Layout::RowMajor => "row-major",
        Layout::Both => "both",
    })
}

fn build_f32(config: Config) -> Result<Fixture, Box<dyn Error>> {
    let (query, rows) = build_float_values(config)?;
    let rows = build_representations(config, rows, |source| {
        PdxMatrix::encode_f32_with_rows_per_block(source, config.dimensions, config.block_rows)
    })?;
    Ok(Fixture::F32 { query, rows })
}

fn build_f16(config: Config) -> Result<Fixture, Box<dyn Error>> {
    let (query, rows) = build_float_values(config)?;
    let query = query.into_iter().map(f32_to_f16_bits).collect::<Vec<_>>();
    let rows = rows.into_iter().map(f32_to_f16_bits).collect::<Vec<_>>();
    let rows = build_representations(config, rows, |source| {
        PdxMatrix::encode_f16_with_rows_per_block(source, config.dimensions, config.block_rows)
    })?;
    Ok(Fixture::F16 { query, rows })
}

fn build_float_values(config: Config) -> Result<(Vec<f32>, Vec<f32>), Box<dyn Error>> {
    match config.fixture {
        FixtureKind::Degenerate => build_degenerate_float_values(config),
        FixtureKind::Clustered => build_clustered_float_values(config),
    }
}

fn build_degenerate_float_values(config: Config) -> Result<(Vec<f32>, Vec<f32>), Box<dyn Error>> {
    let query = vec![1.0_f32; config.dimensions];
    let first_block_values = config
        .block_rows
        .checked_mul(config.dimensions)
        .ok_or("f32 fixture size overflowed")?;
    let scalar_count = config
        .rows
        .checked_mul(config.dimensions)
        .ok_or("f32 fixture size overflowed")?;
    let rows = (0..scalar_count)
        .map(|index| {
            if index < first_block_values {
                1.0
            } else {
                -1.0
            }
        })
        .collect::<Vec<_>>();
    Ok((query, rows))
}

fn build_clustered_float_values(config: Config) -> Result<(Vec<f32>, Vec<f32>), Box<dyn Error>> {
    let scalar_count = config
        .rows
        .checked_mul(config.dimensions)
        .ok_or("clustered fixture size overflowed")?;
    let cluster_count = (config.rows / 8).clamp(2, 12);
    let mut random = SplitMix64::new(0x05_c1_a5_7e_ed);
    let mut centers = Vec::with_capacity(cluster_count);
    for _ in 0..cluster_count {
        let mut center = (0..config.dimensions)
            .map(|_| random.gaussian())
            .collect::<Vec<_>>();
        normalize(&mut center);
        centers.push(center);
    }
    let total_weight = cluster_count
        .checked_mul(cluster_count + 1)
        .ok_or("cluster weight overflowed")?
        / 2;
    let mut rows = Vec::with_capacity(scalar_count);
    for row_index in 0..config.rows {
        let ticket = row_index % total_weight;
        let mut cumulative = 0_usize;
        let mut label = 0_usize;
        for candidate in 0..cluster_count {
            cumulative = cumulative
                .checked_add(candidate + 1)
                .ok_or("cluster weight overflowed")?;
            if ticket < cumulative {
                label = candidate;
                break;
            }
        }
        let center = centers.get(label).ok_or("cluster label out of range")?;
        let spread = 0.015 + 0.012 * (label % 5) as f32;
        let mut row = center
            .iter()
            .map(|value| *value + spread * random.gaussian())
            .collect::<Vec<_>>();
        normalize(&mut row);
        rows.extend(row);
    }

    let center = centers.first().ok_or("clustered fixture has no centers")?;
    let mut query = center
        .iter()
        .map(|value| *value + 0.035 * random.gaussian())
        .collect::<Vec<_>>();
    normalize(&mut query);
    Ok((query, rows))
}

fn build_int8(config: Config) -> Result<Fixture, Box<dyn Error>> {
    let query_values = vec![1.0_f32; config.dimensions];
    let query = prepare_int8_query(&query_values)?;
    let scalar_count = config
        .rows
        .checked_mul(config.dimensions)
        .ok_or("Int8 fixture size overflowed")?;
    let codes = (0..scalar_count)
        .map(|index| {
            if index.is_multiple_of(3) {
                96_i8
            } else {
                -64_i8
            }
        })
        .collect::<Vec<_>>();
    let factors = (0..config.rows)
        .map(|_| Int8Factors::new(0.01, 0.0))
        .collect::<Result<Vec<_>, _>>()?;
    let rows = build_representations(config, codes, |source| {
        PdxMatrix::encode_int8_with_rows_per_block(source, config.dimensions, config.block_rows)
    })?;
    Ok(Fixture::Int8 {
        query,
        rows,
        factors,
    })
}

fn build_bit4(config: Config) -> Result<Fixture, Box<dyn Error>> {
    let row_width = config.dimensions.div_ceil(2);
    let byte_count = config
        .rows
        .checked_mul(row_width)
        .ok_or("Bit4 fixture size overflowed")?;
    let data = match config.fixture {
        FixtureKind::Degenerate => {
            let query = vec![1.0_f32; config.dimensions];
            let source = (0..config.dimensions)
                .map(|index| if index.is_multiple_of(3) { 1.0 } else { -0.5 })
                .collect::<Vec<_>>();
            let mut template = vec![0_u8; row_width];
            let factor = quantize_bit4(&source, &mut template)?;
            let mut codes = Vec::with_capacity(byte_count);
            for _ in 0..config.rows {
                codes.extend_from_slice(&template);
            }
            Bit4FixtureData {
                query,
                codes,
                factors: vec![factor; config.rows],
            }
        }
        FixtureKind::Clustered => build_clustered_bit4_values(config, row_width, byte_count)?,
    };
    let query = zeppelin_embed::quant::prepare_bit4_query(&data.query, 0x05)?;
    let rows = build_representations(config, data.codes, |source| {
        PdxMatrix::encode_bit4_with_rows_per_block(source, config.dimensions, config.block_rows)
    })?;
    Ok(Fixture::Bit4 {
        query,
        rows,
        factors: data.factors,
    })
}

fn build_representations<T, E>(
    config: Config,
    source: Vec<T>,
    encode: E,
) -> Result<Representations<T>, Box<dyn Error>>
where
    E: FnOnce(&[T]) -> Result<PdxMatrix, zeppelin_embed::scan::pdx::PdxError>,
{
    let build_pdx = matches!(config.layout, Layout::Pdx | Layout::Both);
    let encode_started = if config.smoke || !build_pdx {
        None
    } else {
        Some(Instant::now())
    };
    let pdx = if build_pdx {
        Some(encode(&source)?)
    } else {
        None
    };
    let pdx_encode_elapsed = encode_started.map(|started| started.elapsed());
    let row_major = if matches!(config.layout, Layout::RowMajor | Layout::Both) {
        Some(source)
    } else {
        None
    };
    Ok(Representations {
        row_major,
        pdx,
        pdx_encode_elapsed,
    })
}

fn build_clustered_bit4_values(
    config: Config,
    row_width: usize,
    byte_count: usize,
) -> Result<Bit4FixtureData, Box<dyn Error>> {
    let cluster_count = (config.rows / 8).clamp(2, 12);
    let mut random = SplitMix64::new(0x05_c1_a5_7e_ed);
    let mut centers = Vec::with_capacity(cluster_count);
    for _ in 0..cluster_count {
        let mut center = (0..config.dimensions)
            .map(|_| random.gaussian())
            .collect::<Vec<_>>();
        normalize(&mut center);
        centers.push(center);
    }
    let mut templates = Vec::with_capacity(cluster_count);
    for center in &centers {
        let mut encoded = vec![0_u8; row_width];
        let factor = quantize_bit4(center, &mut encoded)?;
        templates.push((encoded, factor));
    }
    let total_weight = cluster_count
        .checked_mul(cluster_count + 1)
        .ok_or("cluster weight overflowed")?
        / 2;
    let mut codes = vec![0_u8; byte_count];
    let mut factors = Vec::with_capacity(config.rows);
    for (row_index, encoded) in codes.chunks_exact_mut(row_width).enumerate() {
        let ticket = row_index % total_weight;
        let mut cumulative = 0_usize;
        let mut label = 0_usize;
        for candidate in 0..cluster_count {
            cumulative = cumulative
                .checked_add(candidate + 1)
                .ok_or("cluster weight overflowed")?;
            if ticket < cumulative {
                label = candidate;
                break;
            }
        }
        let (template, factor) = templates.get(label).ok_or("cluster label out of range")?;
        encoded.copy_from_slice(template);
        factors.push(*factor);
    }
    let center = centers.first().ok_or("clustered fixture has no centers")?;
    let mut query = center
        .iter()
        .map(|value| *value + 0.035 * random.gaussian())
        .collect::<Vec<_>>();
    normalize(&mut query);
    Ok(Bit4FixtureData {
        query,
        codes,
        factors,
    })
}

fn scan_options(config: Config) -> ScanOptions {
    ScanOptions {
        thread_budget: config.threads,
    }
}

fn parse_args() -> Result<Config, Box<dyn Error>> {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from(arguments: impl IntoIterator<Item = String>) -> Result<Config, Box<dyn Error>> {
    let mut config = Config::default();
    let mut shape_was_supplied = false;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--test" => config.smoke = true,
            "--help" | "-h" => config.show_help = true,
            "--bench" => {}
            "--scheme" => {
                let value = arguments.next().ok_or("--scheme requires a value")?;
                config.scheme = match value.as_str() {
                    "f32" => Scheme::F32,
                    "f16" => Scheme::F16,
                    "int8" => Scheme::Int8,
                    "bit4" => Scheme::Bit4,
                    _ => return Err(format!("unsupported scheme {value:?}").into()),
                };
            }
            "--shape" => {
                shape_was_supplied = true;
                let value = arguments.next().ok_or("--shape requires ROWSxDIMENSIONS")?;
                let (rows, dimensions) = value
                    .split_once('x')
                    .ok_or("--shape must be ROWSxDIMENSIONS")?;
                config.rows = rows.parse()?;
                config.dimensions = dimensions.parse()?;
            }
            "--fixture" => {
                let value = arguments
                    .next()
                    .ok_or("--fixture requires degenerate or clustered")?;
                config.fixture = match value.as_str() {
                    "degenerate" => FixtureKind::Degenerate,
                    "clustered" => FixtureKind::Clustered,
                    _ => return Err("--fixture requires degenerate or clustered".into()),
                };
            }
            "--layout" => {
                let value = arguments
                    .next()
                    .ok_or("--layout requires pdx, row-major, or both")?;
                config.layout = match value.as_str() {
                    "pdx" => Layout::Pdx,
                    "row-major" => Layout::RowMajor,
                    "both" => Layout::Both,
                    _ => return Err("--layout requires pdx, row-major, or both".into()),
                };
            }
            "--k" => config.k = parse_next(&mut arguments, "--k")?,
            "--threads" => config.threads = parse_next(&mut arguments, "--threads")?,
            "--block-rows" => config.block_rows = parse_next(&mut arguments, "--block-rows")?,
            "--iterations" => config.iterations = parse_next(&mut arguments, "--iterations")?,
            "--repeats" => config.repeats = scan_repeat::parse_repeats(&mut arguments)?,
            _ => return Err(format!("unknown scan benchmark argument {argument:?}").into()),
        }
    }
    if config.smoke && !shape_was_supplied {
        config.rows = config.rows.min(256);
        config.dimensions = config.dimensions.min(128);
        config.iterations = 1;
    }
    if config.rows == 0
        || config.dimensions == 0
        || config.block_rows == 0
        || config.iterations == 0
    {
        return Err(
            "rows, dimensions, block rows, and iterations must be greater than zero".into(),
        );
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

fn f32_to_f16_bits(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let fraction = ((bits >> 13) & 0x03ff) as u16;
    if exponent <= 0 {
        sign
    } else if exponent >= 0x1f {
        sign | 0x7c00
    } else {
        sign | ((exponent as u16) << 10) | fraction
    }
}

fn f16_to_f64(bits: u16) -> f64 {
    let sign = if bits & 0x8000 == 0 { 1.0 } else { -1.0 };
    let exponent = (bits >> 10) & 0x1f;
    let fraction = bits & 0x03ff;
    match exponent {
        0 => sign * f64::from(fraction) * 2.0_f64.powi(-24),
        _ => sign * (1.0 + f64::from(fraction) / 1_024.0) * 2.0_f64.powi(i32::from(exponent) - 15),
    }
}

fn parse_next(
    arguments: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<usize, Box<dyn Error>> {
    Ok(arguments
        .next()
        .ok_or_else(|| format!("{flag} requires a value"))?
        .parse()?)
}
