//! Benchmark, platform-truth, and optimization-frontier tooling.

/// BEIR corpus loading and nDCG evaluation for the task 13 gate.
pub mod beir;

/// Optimization-frontier measurement and search harness.
pub mod frontier;
/// Recall-only flat-Vamana construction and measurement scaffold for M3.
pub mod graph_recall;

/// Platform-truth measurement harnesses.
pub mod platform;
/// Quantization recall-retention and deterministic byte-count harness.
pub mod recall;
/// Full scheme-level coarse-scoring benchmark support.
pub mod scheme_level;
