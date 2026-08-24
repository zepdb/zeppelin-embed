//! BEIR corpus loading and nDCG evaluation.
//!
//! This lives in the bench crate, never in the engine. The loader needs
//! `serde_json`, which is blacklisted for core, and the engine never gains
//! an HTTP client: datasets are fetched by a separate script with pinned
//! URLs and checksums, into a caller-supplied directory.
//!
//! # Status of the gate
//!
//! The gate test is `tests/beir_gate.rs`, `#[ignore]`d because it needs
//! those datasets on disk. See that file for what is and is not measured.

pub mod eval;
pub mod loader;

pub use eval::{
    dcg_at_k, flat_targets, mean_ndcg_at_k, multifield_targets, ndcg_at_k_for_query, GateRow,
    Qrels, Run, RunEntry,
};
pub use loader::{load_corpus, BeirCorpus, BeirError, BeirQuery};
