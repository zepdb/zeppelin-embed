//! SIFT-1M recall gate for the M3 graph build; records no timing.

use std::path::PathBuf;

use zeppelin_embed::graph::build::GraphBuildPasses;
use zeppelin_embed_bench::graph_recall::{Sift1mPaths, build_sift1m_graph, measure_sift1m_recall};

const DEFAULT_SEED: u64 = 0x19_0003_51f7_1a00;
const EF_SWEEP: [usize; 8] = [100, 120, 140, 160, 180, 200, 220, 240];

fn main() {
    if let Err(error) = run() {
        eprintln!("vamana-recall: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let mut data_directory = PathBuf::from("tasks/cross-benchmark/data");
    let mut cache_directory = PathBuf::from("/private/tmp/zeppelin-embed-m3-sift1m");
    let mut passes = GraphBuildPasses::One;
    let mut seed = DEFAULT_SEED;
    while let Some(option) = arguments.next() {
        match option.as_str() {
            "--data-dir" => {
                data_directory = PathBuf::from(arguments.next().ok_or("--data-dir needs a path")?)
            }
            "--cache-dir" => {
                cache_directory = PathBuf::from(arguments.next().ok_or("--cache-dir needs a path")?)
            }
            "--passes" => {
                passes = match arguments.next().as_deref() {
                    Some("one") => GraphBuildPasses::One,
                    Some("two") => GraphBuildPasses::Two,
                    _ => return Err("--passes must be one or two".into()),
                }
            }
            "--seed" => {
                let value = arguments.next().ok_or("--seed needs a u64")?;
                seed = value.parse()?;
            }
            "--help" | "-h" => {
                eprintln!(
                    "usage: vamana-recall [--passes one|two] [--data-dir PATH] [--cache-dir PATH] [--seed U64]"
                );
                return Ok(());
            }
            _ => return Err(format!("unknown option {option:?}").into()),
        }
    }
    let paths = Sift1mPaths::in_directory(&data_directory);
    let reader = build_sift1m_graph(&paths, &cache_directory, passes, seed)?;
    let results = measure_sift1m_recall(&reader, &paths, &EF_SWEEP, seed)?;
    let pass_label = match passes {
        GraphBuildPasses::One => "alpha=1.0 one-pass",
        GraphBuildPasses::Two => "alpha=1.0 then alpha=1.2 two-pass",
    };
    println!("SIFT-1M {pass_label}; traversal=bit4; rescore=f32-whole-pool; timing=NOT_MEASURED");
    for point in &results {
        println!("ef={} recall@100={:.6}", point.ef, point.recall_at_100);
    }
    let gate = results.iter().find(|point| point.recall_at_100 >= 0.93);
    match gate {
        Some(point) => println!(
            "sift1m_recall_at_100_reaches_093_at_ef_le_240: PASS ef={} recall@100={:.6}",
            point.ef, point.recall_at_100
        ),
        None => {
            let best = results
                .iter()
                .max_by(|left, right| left.recall_at_100.total_cmp(&right.recall_at_100))
                .ok_or("empty recall sweep")?;
            println!(
                "sift1m_recall_at_100_reaches_093_at_ef_le_240: FAIL best_ef={} recall@100={:.6}",
                best.ef, best.recall_at_100
            );
            return Err("recall@100 did not reach 0.93 by ef=240".into());
        }
    }
    Ok(())
}
