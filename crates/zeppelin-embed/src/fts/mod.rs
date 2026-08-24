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

/// The sealed segment: the persisted posting format on the query path.
pub mod sealed;

/// The exhaustive-OR scorer and task 14 oracle.
pub mod search;

#[cfg(test)]
mod properties;

#[cfg(all(test, feature = "allocation-audit"))]
mod alloc_gate;

/// Dynamic pruning: block-max MAXSCORE and WAND.
pub mod prune;

/// Phrase matching over stored positions.
pub mod phrase;

/// Prefix queries over the sorted dictionary.
pub mod prefix;

/// Bounded fuzzy matching.
pub mod fuzzy;

/// Double Metaphone phonetic encoding.
pub mod phonetic;

/// Snippets and match offsets.
pub mod snippet;
