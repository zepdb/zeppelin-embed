//! Full-text indexing and retrieval.
//!
//! The lexical half of the engine. Task 12 builds the versioned analysis
//! pipeline whose identity is index metadata; tasks 13 through 15 build the
//! postings, the scorer, dynamic pruning, and the precision surface on top
//! of it.

pub mod tokenizer;

/// The BM25 scorer and corpus statistics.
pub mod bm25;

/// Persisted posting blocks with positions.
pub mod postings;

/// Byte-quantized document length norms.
pub mod norms;

/// The sorted, front-coded term dictionary.
pub mod dict;

/// The lexical index across segments.
pub mod index;

/// The exhaustive-OR scorer and task 14 oracle.
pub mod search;

#[cfg(test)]
mod properties;

/// Dynamic pruning: block-max MAXSCORE and WAND.
pub mod prune;
