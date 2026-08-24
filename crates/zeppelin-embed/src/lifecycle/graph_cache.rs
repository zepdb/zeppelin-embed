//! Per-segment reusable graph-search state bound without borrowing the mapping.

use std::sync::{Arc, Condvar, Mutex};

use crate::graph::block::{GraphNodeBlocks, ValidatedGraphNodeBlocks};
use crate::graph::search::{
    GraphSearchError, GraphSearchScratch, GraphSearcher, GraphSegmentNormRange,
};
use crate::segment::reader::SegmentReader;

use super::stats::{AccountedCounter, Accounting, AllocationComponent};
use super::{QueryCancellation, StoreError};

struct CachedScratch {
    scratch: GraphSearchScratch,
    // A reusable per-segment scratch is cache state, rather than snapshot data
    // or per-query temporary memory, because it survives completed queries.
    _memory: AccountedCounter,
}

struct CacheState {
    graph: Option<ValidatedGraphNodeBlocks>,
    entries: Option<[u32; 4]>,
    norm_range: Option<GraphSegmentNormRange>,
    scratch: Option<CachedScratch>,
    checked_out: bool,
}

pub(crate) struct SegmentGraphSearchCache {
    state: Mutex<CacheState>,
    changed: Condvar,
}

impl SegmentGraphSearchCache {
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new(CacheState {
                graph: None,
                entries: None,
                norm_range: None,
                scratch: None,
                checked_out: false,
            }),
            changed: Condvar::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn bind_graph<'a>(
        &self,
        segment: &'a SegmentReader,
    ) -> Result<(GraphNodeBlocks<'a>, bool), GraphCacheError> {
        let mut state = self.state.lock().map_err(|_| {
            GraphCacheError::Store(StoreError::Synchronization {
                component: "graph search cache",
            })
        })?;
        if let Some(descriptor) = state.graph {
            let graph = segment
                .bind_validated_graph_node_blocks(descriptor)
                .map_err(StoreError::Segment)?;
            return Ok((graph, false));
        }
        let graph = segment.graph_node_blocks().map_err(StoreError::Segment)?;
        state.graph = Some(graph.validated_descriptor());
        drop(state);
        Ok((graph, true))
    }

    pub(crate) fn prepare_shared<'a>(
        &self,
        segment: &'a SegmentReader,
        cancellation: &QueryCancellation<'_>,
    ) -> Result<SharedGraphPreparation<'a>, GraphCacheError> {
        let mut state = self.state.lock().map_err(|_| {
            GraphCacheError::Store(StoreError::Synchronization {
                component: "graph search cache",
            })
        })?;
        let (graph, graph_validated) = if let Some(descriptor) = state.graph {
            (
                segment
                    .bind_validated_graph_node_blocks(descriptor)
                    .map_err(StoreError::Segment)?,
                false,
            )
        } else {
            let graph = segment.graph_node_blocks().map_err(StoreError::Segment)?;
            state.graph = Some(graph.validated_descriptor());
            (graph, true)
        };
        let entry_seed_discovered = state.entries.is_none();
        if entry_seed_discovered {
            state.entries = Some(GraphSearcher::discover_entry_row_ids(graph)?);
        }
        let norm_range = match state.norm_range {
            Some(norm_range) => norm_range,
            None => {
                let norm_range = GraphSegmentNormRange::from_graph(graph, Some(cancellation))?;
                state.norm_range = Some(norm_range);
                norm_range
            }
        };
        Ok(SharedGraphPreparation {
            graph,
            graph_validated,
            entry_seed_discovered,
            norm_range,
        })
    }

    pub(crate) fn checkout<'a>(
        &'a self,
        graph: GraphNodeBlocks<'_>,
        ef: usize,
        accounting: &Arc<Accounting>,
    ) -> Result<GraphScratchLease<'a>, GraphCacheError> {
        let mut state = self.state.lock().map_err(|_| {
            GraphCacheError::Store(StoreError::Synchronization {
                component: "graph search cache",
            })
        })?;
        while state.checked_out {
            state = self.changed.wait(state).map_err(|_| {
                GraphCacheError::Store(StoreError::Synchronization {
                    component: "graph search cache",
                })
            })?;
        }

        let entry_seed_discovered = state.entries.is_none();
        if entry_seed_discovered {
            state.entries = Some(GraphSearcher::discover_entry_row_ids(graph)?);
        }
        let entries = state.entries.ok_or_else(|| {
            GraphCacheError::Search(GraphSearchError::Geometry(
                "graph entry cache did not retain discovered seeds".to_owned(),
            ))
        })?;

        let node_count = graph.node_count();
        let max_degree = graph.layout().max_degree();
        let reusable = state
            .scratch
            .as_ref()
            .is_some_and(|cached| cached.scratch.supports(node_count, max_degree, ef));
        if !reusable {
            let requested = GraphSearchScratch::allocation_bytes(node_count, max_degree, ef)?;
            let mut memory = AccountedCounter::new(accounting, AllocationComponent::Cache)?;
            memory.set(requested)?;
            let scratch = GraphSearchScratch::with_ef_capacity(node_count, max_degree, ef)?;
            let actual = scratch.resident_bytes()?;
            if actual != requested {
                return Err(GraphCacheError::Search(GraphSearchError::Geometry(
                    format!(
                        "scratch capacity is {actual} bytes, expected exact reservation {requested}"
                    ),
                )));
            }
            state.scratch = Some(CachedScratch {
                scratch,
                _memory: memory,
            });
        }

        let scratch = state.scratch.take().ok_or_else(|| {
            GraphCacheError::Search(GraphSearchError::Geometry(
                "graph scratch cache is unavailable".to_owned(),
            ))
        })?;
        state.checked_out = true;
        drop(state);
        Ok(GraphScratchLease {
            cache: self,
            scratch: Some(scratch),
            entries,
            entry_seed_discovered,
        })
    }
}

pub(crate) struct SharedGraphPreparation<'a> {
    pub(crate) graph: GraphNodeBlocks<'a>,
    pub(crate) graph_validated: bool,
    pub(crate) entry_seed_discovered: bool,
    pub(crate) norm_range: GraphSegmentNormRange,
}

pub(crate) enum GraphCacheError {
    Store(StoreError),
    Search(GraphSearchError),
}

impl From<StoreError> for GraphCacheError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<GraphSearchError> for GraphCacheError {
    fn from(error: GraphSearchError) -> Self {
        Self::Search(error)
    }
}

pub(crate) struct GraphScratchLease<'a> {
    cache: &'a SegmentGraphSearchCache,
    scratch: Option<CachedScratch>,
    entries: [u32; 4],
    entry_seed_discovered: bool,
}

impl GraphScratchLease<'_> {
    pub(crate) const fn entries(&self) -> [u32; 4] {
        self.entries
    }

    pub(crate) const fn entry_seed_discovered(&self) -> bool {
        self.entry_seed_discovered
    }

    pub(crate) fn scratch_mut(&mut self) -> Result<&mut GraphSearchScratch, GraphSearchError> {
        self.scratch
            .as_mut()
            .map(|cached| &mut cached.scratch)
            .ok_or_else(|| GraphSearchError::Geometry("checked-out scratch is absent".to_owned()))
    }
}

impl Drop for GraphScratchLease<'_> {
    fn drop(&mut self) {
        let mut state = match self.cache.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.scratch = self.scratch.take();
        state.checked_out = false;
        self.cache.changed.notify_one();
    }
}
