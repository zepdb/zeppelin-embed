//! Text embedding and retrieval over Zeppelin Embed stores.

#![deny(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::unwrap_used,
    unsafe_op_in_unsafe_fn
)]
#![warn(missing_docs)]

/// Model architecture implementations.
pub mod arch;
/// Immutable `.zem` bundle loading and validation.
pub mod bundle;
mod epoch;
mod error;
mod ingest;
mod query;
/// Model runtime implementations.
pub mod runtime;
mod tokenizer;
/// Tower metadata and token batches.
pub mod tower;

pub use error::TextError;
pub use ingest::{
    ChunkPolicy, IngestControl, IngestOptions, TextDocument, TextFaultSite, TextIngestReport,
    TextOpenOptions, TextStore,
};
pub use query::{Legs, QueryOptions, TextHit};
