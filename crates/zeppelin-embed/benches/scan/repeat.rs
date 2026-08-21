use std::error::Error;
use std::io::Write;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterleavedArm {
    A,
    B,
}

pub fn parse_repeats(
    arguments: &mut impl Iterator<Item = String>,
) -> Result<usize, Box<dyn Error>> {
    let repeats = arguments
        .next()
        .ok_or("--repeats requires a value")?
        .parse::<usize>()?;
    if repeats == 0 {
        return Err("--repeats must be greater than zero".into());
    }
    Ok(repeats)
}

pub fn with_single_fixture<T, B, R>(build: B, run: R) -> Result<(), Box<dyn Error>>
where
    B: FnOnce() -> Result<T, Box<dyn Error>>,
    R: FnOnce(&T) -> Result<(), Box<dyn Error>>,
{
    let fixture = build()?;
    run(&fixture)
}

pub fn write_timed_repeats<T, W, M>(
    output: &mut W,
    fixture: &T,
    repeats: usize,
    iterations: usize,
    mut measure: M,
) -> Result<(), Box<dyn Error>>
where
    W: Write,
    M: FnMut(&T) -> Result<Duration, Box<dyn Error>>,
{
    if repeats == 0 || iterations == 0 {
        return Err("repeats and iterations must be greater than zero".into());
    }
    if repeats == 1 {
        let elapsed = measure(fixture)?;
        write_timing(output, elapsed, iterations)?;
        return Ok(());
    }

    let mut repeat_means = Vec::with_capacity(repeats);
    for repeat in 1..=repeats {
        let elapsed = measure(fixture)?;
        let mean = mean_seconds(elapsed, iterations);
        writeln!(
            output,
            "repeat {repeat}/{repeats}: mean wall time per scan: {mean:.6} s over {iterations} iterations"
        )?;
        repeat_means.push(mean);
    }
    let mean = repeat_means.iter().sum::<f64>() / repeats as f64;
    let squared_deviations = repeat_means
        .iter()
        .map(|repeat_mean| {
            let deviation = repeat_mean - mean;
            deviation * deviation
        })
        .sum::<f64>();
    let standard_deviation = (squared_deviations / (repeats - 1) as f64).sqrt();
    let relative_standard_deviation = if mean == 0.0 {
        0.0
    } else {
        standard_deviation / mean * 100.0
    };
    writeln!(
        output,
        "summary: mean wall time per scan: {mean:.6} s across {repeats} repeats; relative standard deviation: {relative_standard_deviation:.3}%"
    )?;
    Ok(())
}

pub fn write_interleaved_layout_repeats<T, W, M>(
    output: &mut W,
    fixture: &T,
    repeats: usize,
    iterations: usize,
    mut measure: M,
) -> Result<(), Box<dyn Error>>
where
    W: Write,
    M: FnMut(&T, InterleavedArm) -> Result<Duration, Box<dyn Error>>,
{
    if repeats == 0 || iterations == 0 {
        return Err("repeats and iterations must be greater than zero".into());
    }
    let mut pdx_means = Vec::with_capacity(repeats);
    let mut row_major_means = Vec::with_capacity(repeats);
    for pair in 1..=repeats {
        let pdx_mean = mean_seconds(measure(fixture, InterleavedArm::A)?, iterations);
        let row_major_mean = mean_seconds(measure(fixture, InterleavedArm::B)?, iterations);
        writeln!(
            output,
            "pair {pair}/{repeats}: pdx mean wall time per scan: {pdx_mean:.6} s; row-major mean wall time per scan: {row_major_mean:.6} s over {iterations} iterations each"
        )?;
        pdx_means.push(pdx_mean);
        row_major_means.push(row_major_mean);
    }
    let (pdx_mean, pdx_rsd) = mean_and_rsd(&pdx_means)?;
    let (row_major_mean, row_major_rsd) = mean_and_rsd(&row_major_means)?;
    if pdx_mean == 0.0 {
        return Err("PDX mean was zero; row-major/PDX ratio is undefined".into());
    }
    writeln!(
        output,
        "layout summary: pdx mean wall time per scan: {pdx_mean:.6} s across {repeats} repeats; relative standard deviation: {pdx_rsd:.3}%"
    )?;
    writeln!(
        output,
        "layout summary: row-major mean wall time per scan: {row_major_mean:.6} s across {repeats} repeats; relative standard deviation: {row_major_rsd:.3}%"
    )?;
    writeln!(
        output,
        "layout ratio: row-major/pdx={:.6} across {repeats} pairs",
        row_major_mean / pdx_mean
    )?;
    Ok(())
}

fn write_timing(
    output: &mut impl Write,
    elapsed: Duration,
    iterations: usize,
) -> Result<(), std::io::Error> {
    let mean = mean_seconds(elapsed, iterations);
    writeln!(
        output,
        "mean wall time per scan: {mean:.6} s over {iterations} iterations"
    )
}

fn mean_seconds(elapsed: Duration, iterations: usize) -> f64 {
    elapsed.as_secs_f64() / iterations as f64
}

fn mean_and_rsd(values: &[f64]) -> Result<(f64, f64), Box<dyn Error>> {
    if values.is_empty() {
        return Err("cannot summarize zero timing repeats".into());
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    if values.len() == 1 || mean == 0.0 {
        return Ok((mean, 0.0));
    }
    let squared_deviations = values
        .iter()
        .map(|value| {
            let deviation = value - mean;
            deviation * deviation
        })
        .sum::<f64>();
    let standard_deviation = (squared_deviations / (values.len() - 1) as f64).sqrt();
    Ok((mean, standard_deviation / mean * 100.0))
}
