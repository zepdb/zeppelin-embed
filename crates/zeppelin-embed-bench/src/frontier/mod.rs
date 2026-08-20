//! Reproducible optimization-frontier harness.

/// Short-lived operator machine-state attestations.
pub mod attestation;
/// Persisted, provenance-bearing compute calibration artifacts.
pub mod calibration;
/// Pure parsing and typed validation for the frontier command line.
pub mod cli;
/// Append-only campaign evidence ledger.
pub mod ledger;
/// Machine preflight, statistical measurement, and pluggable workloads.
pub mod measure;
/// Optional performance-counter capture and attribution.
pub mod pmu;
/// Memory and compute roofline denominators.
pub mod roofline;
/// Seeded exhaustive and hill-climbing search.
pub mod tune;
/// Task-03 kernel-knob registry and materialized variants.
pub mod variants;
