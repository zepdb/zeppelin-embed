//! Task 19-M4b cached graph builds over the three non-SIFT ANN datasets.

use std::error::Error;
use std::io;
use std::path::{Path, PathBuf};

use zeppelin_embed::graph::block::CACHE_LINE_BYTES;
use zeppelin_embed::graph::build::GraphBuildPasses;
use zeppelin_embed_bench::graph_recall::{CrossGraphDataset, build_cross_dataset_graph};
use zeppelin_embed_bench::platform::memory_graph::verify_bench_profile;
use zeppelin_embed_bench::platform::taint::{
    detect_taint, format_load1, format_taint_labels, print_taint_status,
};

const DEFAULT_SEED: u64 = 0x19_0003_51f7_1a00;
const LOAD_LIMIT: f64 = 1.0;

struct Config {
    dataset_name: String,
    data_directory: PathBuf,
    cache_directory: Option<PathBuf>,
    passes: GraphBuildPasses,
    seed: u64,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("graph-build: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    verify_bench_profile()?;
    let config = parse_config()?;
    let dataset = CrossGraphDataset::named(&config.dataset_name, &config.data_directory)?;
    let cache_directory = config
        .cache_directory
        .unwrap_or_else(|| match config.passes {
            GraphBuildPasses::One => PathBuf::from(format!(
                "/private/tmp/zeppelin-embed-m5-{}-one",
                dataset.name()
            )),
            GraphBuildPasses::Two => PathBuf::from(format!(
                "/private/tmp/zeppelin-embed-m4b-{}",
                dataset.name()
            )),
        });
    let pass_label = match config.passes {
        GraphBuildPasses::One => "one",
        GraphBuildPasses::Two => "two",
    };
    let taint = detect_taint(LOAD_LIMIT);
    println!(
        "GRAPH_BUILD_CONTEXT dataset={} rows={} dims={} metric={} build_profile=bench opt_level={} build_passes={pass_label} r_target=32 r_max=44 alpha=1.0/1.2 l_build=100 seed={} cache_dir={} load_limit={LOAD_LIMIT:.2}",
        dataset.name(),
        dataset.rows(),
        dataset.dimensions(),
        dataset.metric().label(),
        env!("ZEPPELIN_BENCH_OPT_LEVEL"),
        config.seed,
        cache_directory.display(),
    );
    print_taint_status(&taint, LOAD_LIMIT, "graph-build");
    let artifact =
        build_cross_dataset_graph(&dataset, &cache_directory, config.passes, config.seed)?;
    let graph = artifact.reader().graph_node_blocks()?;
    let layout = graph.layout();
    let score_bytes = layout
        .code_bytes()
        .checked_add(12)
        .ok_or_else(|| io::Error::other("scored candidate byte count overflow"))?;
    let score_cache_lines = score_bytes.div_ceil(CACHE_LINE_BYTES);
    let metadata = artifact.metadata();
    println!(
        "GRAPH_BUILD_RESULT dataset={} cache_hit={} build_passes={pass_label} build_wall_s={:.3} peak_rss_bytes={} zero_norm_rows={} padded_dims={} node_stride_bytes={} node_stride_cache_lines={} scored_candidate_bytes={} scored_candidate_cache_lines={} load1={} taint={}",
        dataset.name(),
        metadata.cache_hit(),
        metadata.build_wall_seconds(),
        metadata.peak_rss_bytes(),
        metadata.zero_norm_rows().len(),
        layout.padded_dims(),
        layout.stride(),
        (layout.stride() as usize).div_ceil(CACHE_LINE_BYTES),
        score_bytes,
        score_cache_lines,
        format_load1(taint.load1),
        format_taint_labels(&taint.taints),
    );
    Ok(())
}

fn parse_config() -> Result<Config, Box<dyn Error>> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut dataset_name = None;
    let mut data_directory = workspace.join("tasks/cross-benchmark/data");
    let mut cache_directory = None;
    let mut passes = GraphBuildPasses::default();
    let mut seed = DEFAULT_SEED;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let value = arguments
            .next()
            .ok_or_else(|| io::Error::other(format!("{argument} needs a value")))?;
        match argument.as_str() {
            "--dataset" => dataset_name = Some(value),
            "--data-dir" => data_directory = PathBuf::from(value),
            "--cache-dir" => cache_directory = Some(PathBuf::from(value)),
            "--passes" => {
                passes = match value.as_str() {
                    "one" => GraphBuildPasses::One,
                    "two" => GraphBuildPasses::Two,
                    _ => {
                        return Err(io::Error::other(format!(
                            "--passes must be one or two, got {value}"
                        ))
                        .into());
                    }
                }
            }
            "--seed" => seed = value.parse()?,
            _ => {
                return Err(io::Error::other(format!("unknown argument {argument}")).into());
            }
        }
    }
    Ok(Config {
        dataset_name: dataset_name.ok_or_else(|| {
            io::Error::other(
                "--dataset must name glove-100-angular, nytimes-256-angular, or mnist-784-euclidean",
            )
        })?,
        data_directory,
        cache_directory,
        passes,
        seed,
    })
}
