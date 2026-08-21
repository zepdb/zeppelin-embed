use std::error::Error;
use std::io::Write;
use std::mem::size_of;
use std::time::{Duration, Instant};

use zeppelin_embed::quant::{Bit4Factors, Bit4Query, Int8Query, prepare_int8_query, quantize_bit4};
use zeppelin_embed::scan::pdx::PdxMatrix;
use zeppelin_embed::scan::{
    Int8Factors, ScanOptions, ScanOutcome, ScanQuery, ScanRequest, ScanRows, top_k_with_options,
};

#[path = "scan/repeat.rs"]
mod scan_repeat;

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

#[derive(Clone, Copy, Debug)]
struct Config {
    scheme: Scheme,
    fixture: FixtureKind,
    rows: usize,
    dimensions: usize,
    k: usize,
    threads: usize,
    block_rows: usize,
    iterations: usize,
    repeats: usize,
    smoke: bool,
}

enum Fixture {
    F32 {
        query: Vec<f32>,
        rows: PdxMatrix,
    },
    F16 {
        query: Vec<u16>,
        rows: PdxMatrix,
    },
    Int8 {
        query: Int8Query,
        rows: PdxMatrix,
        factors: Vec<Int8Factors>,
    },
    Bit4 {
        query: Bit4Query,
        rows: PdxMatrix,
        factors: Vec<Bit4Factors>,
    },
}

struct Bit4FixtureData {
    query: Vec<f32>,
    codes: Vec<u8>,
    factors: Vec<Bit4Factors>,
}

impl Fixture {
    fn scan(&self, config: Config) -> Result<ScanOutcome, Box<dyn Error>> {
        let request = match self {
            Self::F32 { query, rows } => ScanRequest {
                query: ScanQuery::F32(query),
                rows: ScanRows::F32Pdx(rows),
                row_mask: None,
            },
            Self::F16 { query, rows } => ScanRequest {
                query: ScanQuery::F16(query),
                rows: ScanRows::F16Pdx(rows),
                row_mask: None,
            },
            Self::Int8 {
                query,
                rows,
                factors,
            } => ScanRequest {
                query: ScanQuery::Int8(query),
                rows: ScanRows::Int8Pdx {
                    codes: rows,
                    factors,
                },
                row_mask: None,
            },
            Self::Bit4 {
                query,
                rows,
                factors,
            } => ScanRequest {
                query: ScanQuery::Bit4(query),
                rows: ScanRows::Bit4Pdx {
                    codes: rows,
                    factors,
                },
                row_mask: None,
            },
        };
        Ok(top_k_with_options(request, config.k, scan_options(config))?)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scheme: Scheme::Bit4,
            fixture: FixtureKind::Degenerate,
            rows: 100_000,
            dimensions: 768,
            k: 10,
            threads: 0,
            block_rows: 64,
            iterations: 10,
            repeats: 1,
            smoke: false,
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let config = parse_args()?;
    let stdout = std::io::stdout();
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
    M: FnMut(&Fixture, Config) -> Result<Duration, Box<dyn Error>>,
{
    let first = fixture.scan(config)?;
    let second = fixture.scan(config)?;
    if first != second {
        return Err("scan benchmark results or counters were nondeterministic".into());
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
    if first.stats.bytes_read != exhaustive_bytes {
        return Err(format!(
            "exhaustive bytes_read mismatch: expected {exhaustive_bytes}, got {}",
            first.stats.bytes_read
        )
        .into());
    }
    writeln!(
        output,
        "deterministic counters: scheme={:?} fixture={:?} shape={}x{} block_rows={} dims_touched={} bytes_read={} exhaustive_bytes={} threads_used={}",
        config.scheme,
        config.fixture,
        config.rows,
        config.dimensions,
        config.block_rows,
        first.stats.dims_touched,
        first.stats.bytes_read,
        exhaustive_bytes,
        first.stats.threads_used
    )?;
    if config.smoke {
        return Ok(());
    }

    scan_repeat::write_timed_repeats(
        output,
        fixture,
        config.repeats,
        config.iterations,
        |fixture| measure(fixture, config),
    )
}

fn measure_fixture(fixture: &Fixture, config: Config) -> Result<Duration, Box<dyn Error>> {
    let started = Instant::now();
    for _ in 0..config.iterations {
        std::hint::black_box(fixture.scan(config)?);
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

fn build_f32(config: Config) -> Result<Fixture, Box<dyn Error>> {
    let (query, rows) = build_float_values(config)?;
    let pdx =
        PdxMatrix::encode_f32_with_rows_per_block(&rows, config.dimensions, config.block_rows)?;
    Ok(Fixture::F32 { query, rows: pdx })
}

fn build_f16(config: Config) -> Result<Fixture, Box<dyn Error>> {
    let (query, rows) = build_float_values(config)?;
    let query = query.into_iter().map(f32_to_f16_bits).collect::<Vec<_>>();
    let rows = rows.into_iter().map(f32_to_f16_bits).collect::<Vec<_>>();
    let pdx =
        PdxMatrix::encode_f16_with_rows_per_block(&rows, config.dimensions, config.block_rows)?;
    Ok(Fixture::F16 { query, rows: pdx })
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
    let pdx =
        PdxMatrix::encode_int8_with_rows_per_block(&codes, config.dimensions, config.block_rows)?;
    Ok(Fixture::Int8 {
        query,
        rows: pdx,
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
    let pdx = PdxMatrix::encode_bit4_with_rows_per_block(
        &data.codes,
        config.dimensions,
        config.block_rows,
    )?;
    Ok(Fixture::Bit4 {
        query,
        rows: pdx,
        factors: data.factors,
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

fn parse_next(
    arguments: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<usize, Box<dyn Error>> {
    Ok(arguments
        .next()
        .ok_or_else(|| format!("{flag} requires a value"))?
        .parse()?)
}
