//! Full-text indexing and retrieval.
//!
//! The lexical half of the engine. Task 12 builds the versioned analysis
//! pipeline whose identity is index metadata; tasks 13 through 15 build the
//! postings, the scorer, dynamic pruning, and the precision surface on top
//! of it.

pub mod tokenizer;
