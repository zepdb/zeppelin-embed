//! Benchmark, platform-truth, and optimization-frontier tooling.

/// JSON artifact support for workspace harnesses.
#[doc(hidden)]
pub mod harness_json {
    pub use serde_json::{Value, from_slice, from_str, json, to_string, to_vec, to_vec_pretty};
}

/// BEIR corpus loading and nDCG evaluation for the task 13 gate.
pub mod beir;

/// Optimization-frontier measurement and search harness.
pub mod frontier;
/// Recall-only flat-Vamana construction and measurement scaffold for M3.
pub mod graph_recall;
/// Task-03 hot-kernel roofline and cross-kernel regression contracts.
pub mod kernel_gate;

/// Platform-truth measurement harnesses.
pub mod platform;
/// Across-process aggregation for DRAM-regime measurements.
pub mod process_median;
/// Quantization recall-retention and deterministic byte-count harness.
pub mod recall;
/// Full scheme-level coarse-scoring benchmark support.
pub mod scheme_level;
/// Honest public-`TextStore` benchmark aggregation and report rendering.
pub mod user_bench;
