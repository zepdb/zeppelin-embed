//! Deterministic quantization recall curves; deliberately performs no timing.

use std::error::Error;
use std::path::PathBuf;

use zeppelin_embed::quant::QuantScheme;
use zeppelin_embed_bench::recall::datasets::{
    Dataset, SyntheticKind, load_bvecs, load_fvecs, synthetic,
};
use zeppelin_embed_bench::recall::{RecallReport, run_recall};

const DEFAULT_ROWS: usize = 512;
const DEFAULT_QUERIES: usize = 32;
const DEFAULT_DIMENSION: usize = 768;
const DEFAULT_SEED: u64 = 0x04_2026_0820;
const TOP_K: usize = 10;
const OVERSAMPLES: [usize; 8] = [1, 2, 3, 4, 6, 8, 12, 16];

fn main() {
    if let Err(error) = run() {
        eprintln!("recall-harness: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args().skip(1);
    let Some(mode) = arguments.next() else {
        print_usage();
        return Err("a dataset mode is required".into());
    };
    if mode == "--help" || mode == "-h" {
        print_usage();
        return Ok(());
    }

    let mut rows = DEFAULT_ROWS;
    let mut queries = DEFAULT_QUERIES;
    let mut dimension = DEFAULT_DIMENSION;
    let mut seed = DEFAULT_SEED;
    let mut remaining = arguments.collect::<Vec<_>>().into_iter();
    let file_pair = if mode == "--fvecs" || mode == "--bvecs" {
        let vectors = remaining.next().ok_or("file mode requires a corpus path")?;
        let query_path = remaining.next().ok_or("file mode requires a query path")?;
        Some((PathBuf::from(vectors), PathBuf::from(query_path)))
    } else {
        None
    };
    while let Some(option) = remaining.next() {
        match option.as_str() {
            "--rows" => rows = parse_usize("rows", remaining.next())?,
            "--queries" => queries = parse_usize("queries", remaining.next())?,
            "--dimension" => dimension = parse_usize("dimension", remaining.next())?,
            "--seed" => seed = parse_u64("seed", remaining.next())?,
            _ => return Err(format!("unknown option {option:?}").into()),
        }
    }

    let datasets = match mode.as_str() {
        "--synthetic" => {
            let kinds = [
                SyntheticKind::Uniform,
                SyntheticKind::Anisotropic,
                SyntheticKind::Clustered,
                SyntheticKind::HeavyTailed,
                SyntheticKind::Correlated,
            ];
            kinds
                .into_iter()
                .enumerate()
                .map(|(index, kind)| {
                    synthetic(
                        kind,
                        rows,
                        queries,
                        dimension,
                        seed.wrapping_add(index as u64),
                    )
                })
                .collect::<Result<Vec<_>, _>>()?
        }
        "--jagged" => SyntheticKind::JAGGED
            .into_iter()
            .enumerate()
            .map(|(index, kind)| {
                synthetic(
                    kind,
                    rows,
                    queries,
                    dimension,
                    seed.wrapping_add(index as u64),
                )
            })
            .collect::<Result<Vec<_>, _>>()?,
        "--uniform" => vec![synthetic(
            SyntheticKind::Uniform,
            rows,
            queries,
            dimension,
            seed,
        )?],
        "--fvecs" => {
            let (vectors, query_path) = file_pair.ok_or("missing fvecs paths")?;
            vec![Dataset::new(
                format!("fvecs:{}", vectors.display()),
                load_fvecs(vectors)?,
                load_fvecs(query_path)?,
                None,
            )?]
        }
        "--bvecs" => {
            let (vectors, query_path) = file_pair.ok_or("missing bvecs paths")?;
            vec![Dataset::new(
                format!("bvecs:{}", vectors.display()),
                load_bvecs(vectors)?,
                load_bvecs(query_path)?,
                None,
            )?]
        }
        _ => {
            print_usage();
            return Err(format!("unknown dataset mode {mode:?}").into());
        }
    };

    for dataset in datasets {
        let report = run_recall(&dataset, &OVERSAMPLES, TOP_K)?;
        print_report(&report);
    }
    Ok(())
}

fn parse_usize(name: &str, value: Option<String>) -> Result<usize, Box<dyn Error>> {
    let value = value.ok_or_else(|| format!("--{name} requires a value"))?;
    let parsed = value
        .parse::<usize>()
        .map_err(|error| format!("invalid --{name} value {value:?}: {error}"))?;
    Ok(parsed)
}

fn parse_u64(name: &str, value: Option<String>) -> Result<u64, Box<dyn Error>> {
    let value = value.ok_or_else(|| format!("--{name} requires a value"))?;
    let parsed = value
        .parse::<u64>()
        .map_err(|error| format!("invalid --{name} value {value:?}: {error}"))?;
    Ok(parsed)
}

fn print_report(report: &RecallReport) {
    println!(
        "DATASET {} rows={} queries={} dimension={} k={} ground_truth=brute-force-f64",
        report.dataset_name, report.row_count, report.query_count, report.dimension, report.k
    );
    println!("| scheme | oversample | recall@10 | coarse B/q | rescore B/q | total B/q |");
    println!("| --- | ---: | ---: | ---: | ---: | ---: |");
    for point in &report.points {
        println!(
            "| {} | {} | {:.6} | {} | {} | {} |",
            scheme_label(point.scheme),
            point.oversample,
            point.recall_at_10,
            point.bytes_per_query.coarse,
            point.bytes_per_query.rescore,
            point.bytes_per_query.total(),
        );
    }
    println!("RECOMMENDED target_recall=0.95");
    for recommendation in &report.recommendations {
        match (recommendation.oversample, recommendation.measured_recall) {
            (Some(oversample), Some(recall)) => println!(
                "{} oversample={} recall@10={recall:.6}",
                scheme_label(recommendation.scheme),
                oversample
            ),
            _ => println!(
                "{} oversample=NOT_REACHED recall@10<0.95",
                scheme_label(recommendation.scheme)
            ),
        }
    }
    println!();
}

const fn scheme_label(scheme: QuantScheme) -> &'static str {
    match scheme {
        QuantScheme::Bit4 => "Bit4",
        QuantScheme::Int8 => "Int8",
        QuantScheme::F32 => "F32",
        QuantScheme::F16 => "F16",
    }
}

fn print_usage() {
    eprintln!(
        "usage:\n  recall-harness --synthetic [--rows N --queries N --dimension N --seed N]\n  recall-harness --jagged [same options]\n  recall-harness --uniform [same options]\n  recall-harness --fvecs CORPUS.fvecs QUERIES.fvecs\n  recall-harness --bvecs CORPUS.bvecs QUERIES.bvecs"
    );
}
