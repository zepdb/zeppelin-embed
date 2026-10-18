#![allow(dead_code)]
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

pub mod artifacts;
pub mod campaign;
pub mod coverage;
pub mod diagnostics_health;
pub mod fault_vfs;
pub mod ffi_bindings;
pub mod fts;
#[cfg(feature = "graph-cypher")]
pub mod graph_catalog;
#[cfg(feature = "graph-cypher")]
pub mod graph_contents;
pub mod hybrid_fusion;
pub mod ingest_retention;
pub mod lifecycle_accounting;
pub mod metadata_filter_planner;
pub mod model;
pub mod oracle;
pub mod profiles;
pub mod program;
#[cfg(feature = "graph-cypher")]
pub mod property_graph;
#[cfg(feature = "graph-cypher")]
pub mod property_graph_storage;
pub mod runner;
pub mod storage_durability;
pub mod tiering_maintenance;
#[cfg(feature = "graph-cypher")]
pub mod unified_graph;
pub mod vamana_graph;
pub mod vector_execution;

use std::sync::{Mutex, OnceLock};

pub(crate) fn feature_process_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[path = "../tooling_seed.rs"]
pub mod test_support;

#[cfg(feature = "graph-cypher")]
pub mod graph_key_lifecycle;

#[cfg(feature = "graph-cypher")]
pub mod graph_query;

#[cfg(feature = "graph-cypher")]
pub mod graph_wal;

#[cfg(feature = "graph-cypher")]
pub mod graph_runtime;

#[cfg(feature = "graph-cypher")]
pub mod graph_directories;
#[cfg(feature = "graph-cypher")]
pub mod graph_staging;

#[cfg(feature = "graph-cypher")]
pub mod graph_fixture;

#[cfg(feature = "graph-cypher")]
pub mod graph_binding;

#[cfg(feature = "graph-cypher")]
pub mod graph_adjacency;

#[cfg(feature = "graph-cypher")]
pub mod graph_adjacency_store;

#[cfg(feature = "graph-cypher")]
pub mod graph_relational;

#[cfg(feature = "graph-cypher")]
pub mod graph_completed;
#[cfg(feature = "graph-cypher")]
pub mod graph_cypher_entry;
#[cfg(feature = "graph-cypher")]
pub mod graph_cypher_search;
#[cfg(feature = "graph-cypher")]
pub mod graph_native_result;
#[cfg(feature = "graph-cypher")]
pub mod graph_query_entry;
#[cfg(feature = "graph-cypher")]
pub mod graph_search_qualification;

#[cfg(feature = "graph-result-test-support")]
pub mod graph_response;

#[cfg(feature = "graph-cypher")]
pub mod graph_lowering;

#[cfg(feature = "graph-cypher")]
pub mod graph_search_lowering;

#[cfg(feature = "graph-cypher")]
pub mod graph_query_storage;

#[cfg(feature = "graph-cypher")]
pub mod graph_mutation_lowering;

#[cfg(feature = "graph-cypher")]
pub mod graph_native_vector_index;

#[cfg(feature = "graph-cypher")]
pub mod graph_read_view;

#[cfg(feature = "graph-cypher")]
pub mod graph_publication;

#[cfg(feature = "graph-cypher")]
pub mod graph_identity;
#[cfg(feature = "graph-cypher")]
pub mod graph_native_relational;
#[cfg(feature = "graph-cypher")]
pub mod graph_pattern;
#[cfg(feature = "graph-cypher")]
pub mod graph_reclaim;
#[cfg(feature = "graph-cypher")]
pub mod graph_recovery;
#[cfg(feature = "graph-cypher")]
pub mod graph_storage_faults;

#[cfg(unix)]
pub mod delete_where;

#[cfg(feature = "graph-cypher")]
pub mod graph_c_entry;
#[cfg(feature = "graph-cypher")]
pub mod graph_lifecycle;
