use std::path::Path;

use zeppelin_embed::graph::build::GraphBuildPasses;
use zeppelin_embed_bench::graph_recall::{Sift1mPaths, build_sift1m_graph, measure_sift1m_recall};

#[test]
#[ignore = "requires the external SIFT1M corpus and stable benchmark hardware"]
fn sift1m_recall_at_100_reaches_093_at_ef_le_240() {
    let seed = 0x19_0003_51f7_1a00;
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tasks/cross-benchmark/data");
    let paths = Sift1mPaths::in_directory(&data);
    let cache = Path::new("/private/tmp/zeppelin-embed-m3-sift1m");
    let reader = build_sift1m_graph(&paths, cache, GraphBuildPasses::Two, seed)
        .expect("two-pass SIFT-1M graph");
    let sweep = measure_sift1m_recall(&reader, &paths, &[200], seed)
        .expect("SIFT-1M recall gate measurement");
    assert!(
        sweep
            .iter()
            .any(|point| point.ef <= 240 && point.recall_at_100 >= 0.93),
        "recall gate not met: {sweep:?}"
    );
}
