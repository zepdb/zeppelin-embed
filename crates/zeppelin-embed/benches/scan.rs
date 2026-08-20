use std::error::Error;
use std::time::{Duration, Instant};

use zeppelin_embed::quant::{Bit4Factors, Bit4Query, Int8Query, prepare_int8_query, quantize_bit4};
use zeppelin_embed::scan::pdx::PdxMatrix;
use zeppelin_embed::scan::{
    Int8Factors, ScanOptions, ScanOutcome, ScanQuery, ScanRequest, ScanRows, top_k_with_options,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scheme {
    F32,
    Int8,
    Bit4,
}

#[derive(Clone, Copy, Debug)]
struct Config {
    scheme: Scheme,
    rows: usize,
    dimensions: usize,
    k: usize,
    threads: usize,
    block_rows: usize,
    abandon: bool,
    iterations: usize,
    smoke: bool,
}

enum Fixture {
    F32 {
        query: Vec<f32>,
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

impl Fixture {
    fn scan(&self, config: Config) -> Result<ScanOutcome, Box<dyn Error>> {
        let request = match self {
            Self::F32 { query, rows } => ScanRequest {
                query: ScanQuery::F32(query),
                rows: ScanRows::F32Pdx(rows),
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
            rows: 100_000,
            dimensions: 768,
            k: 10,
            threads: 0,
            block_rows: 64,
            abandon: true,
            iterations: 10,
            smoke: false,
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let config = parse_args()?;
    let fixture = build_fixture(config)?;
    let first = fixture.scan(config)?;
    let second = fixture.scan(config)?;
    if first != second {
        return Err("scan benchmark results or counters were nondeterministic".into());
    }
    let exhaustive_dimensions = u64::try_from(config.rows)?
        .checked_mul(u64::try_from(config.dimensions)?)
        .ok_or("benchmark dimension count overflowed")?;
    if (!config.abandon || config.scheme != Scheme::F32)
        && first.stats.dims_touched != exhaustive_dimensions
    {
        return Err(format!(
            "exhaustive dims_touched mismatch: expected {exhaustive_dimensions}, got {}",
            first.stats.dims_touched
        )
        .into());
    }
    println!(
        "deterministic counters: scheme={:?} shape={}x{} block_rows={} dims_touched={} rows_abandoned={} threads_used={}",
        config.scheme,
        config.rows,
        config.dimensions,
        config.block_rows,
        first.stats.dims_touched,
        first.stats.rows_abandoned,
        first.stats.threads_used
    );
    if config.smoke {
        return Ok(());
    }

    let started = Instant::now();
    for _ in 0..config.iterations {
        std::hint::black_box(fixture.scan(config)?);
    }
    print_timing(started.elapsed(), config.iterations);
    Ok(())
}

fn build_fixture(config: Config) -> Result<Fixture, Box<dyn Error>> {
    match config.scheme {
        Scheme::F32 => build_f32(config),
        Scheme::Int8 => build_int8(config),
        Scheme::Bit4 => build_bit4(config),
    }
}

fn build_f32(config: Config) -> Result<Fixture, Box<dyn Error>> {
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
    let pdx =
        PdxMatrix::encode_f32_with_rows_per_block(&rows, config.dimensions, config.block_rows)?;
    Ok(Fixture::F32 { query, rows: pdx })
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
    let query_values = vec![1.0_f32; config.dimensions];
    let query = zeppelin_embed::quant::prepare_bit4_query(&query_values, 0x05)?;
    let row_width = config.dimensions.div_ceil(2);
    let source = (0..config.dimensions)
        .map(|index| if index.is_multiple_of(3) { 1.0 } else { -0.5 })
        .collect::<Vec<_>>();
    let mut template = vec![0_u8; row_width];
    let factor = quantize_bit4(&source, &mut template)?;
    let byte_count = config
        .rows
        .checked_mul(row_width)
        .ok_or("Bit4 fixture size overflowed")?;
    let mut codes = Vec::with_capacity(byte_count);
    for _ in 0..config.rows {
        codes.extend_from_slice(&template);
    }
    let factors = vec![factor; config.rows];
    let pdx =
        PdxMatrix::encode_bit4_with_rows_per_block(&codes, config.dimensions, config.block_rows)?;
    Ok(Fixture::Bit4 {
        query,
        rows: pdx,
        factors,
    })
}

fn scan_options(config: Config) -> ScanOptions {
    ScanOptions {
        early_abandon: config.abandon,
        thread_budget: config.threads,
    }
}

fn parse_args() -> Result<Config, Box<dyn Error>> {
    let mut config = Config::default();
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--test" => config.smoke = true,
            "--bench" => {}
            "--scheme" => {
                let value = arguments.next().ok_or("--scheme requires a value")?;
                config.scheme = match value.as_str() {
                    "f32" => Scheme::F32,
                    "int8" => Scheme::Int8,
                    "bit4" => Scheme::Bit4,
                    _ => return Err(format!("unsupported scheme {value:?}").into()),
                };
            }
            "--shape" => {
                let value = arguments.next().ok_or("--shape requires ROWSxDIMENSIONS")?;
                let (rows, dimensions) = value
                    .split_once('x')
                    .ok_or("--shape must be ROWSxDIMENSIONS")?;
                config.rows = rows.parse()?;
                config.dimensions = dimensions.parse()?;
            }
            "--k" => config.k = parse_next(&mut arguments, "--k")?,
            "--threads" => config.threads = parse_next(&mut arguments, "--threads")?,
            "--block-rows" => config.block_rows = parse_next(&mut arguments, "--block-rows")?,
            "--iterations" => config.iterations = parse_next(&mut arguments, "--iterations")?,
            "--abandon" => {
                let value = arguments.next().ok_or("--abandon requires on or off")?;
                config.abandon = match value.as_str() {
                    "on" => true,
                    "off" => false,
                    _ => return Err("--abandon requires on or off".into()),
                };
            }
            _ => return Err(format!("unknown scan benchmark argument {argument:?}").into()),
        }
    }
    if config.smoke {
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

fn parse_next(
    arguments: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<usize, Box<dyn Error>> {
    Ok(arguments
        .next()
        .ok_or_else(|| format!("{flag} requires a value"))?
        .parse()?)
}

fn print_timing(elapsed: Duration, iterations: usize) {
    let mean = elapsed.as_secs_f64() / iterations as f64;
    println!("mean wall time per scan: {mean:.6} s over {iterations} iterations");
}
