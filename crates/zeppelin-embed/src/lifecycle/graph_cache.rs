//! Per-segment reusable graph-search state bound without borrowing the mapping.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::graph::block::{GraphNodeBlocks, ValidatedGraphNodeBlocks};
use crate::graph::search::{
    GraphSearchError, GraphSearchScratch, GraphSearcher, GraphSegmentNormRange,
};
use crate::segment::reader::SegmentReader;

use super::stats::{AccountedCounter, Accounting, AllocationComponent};
use super::{QueryCancellation, StoreError};

const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(1);

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
    available: Vec<CachedScratch>,
    checked_out: usize,
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
                available: Vec::new(),
                checked_out: 0,
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
        cancellation: &QueryCancellation<'_>,
    ) -> Result<GraphScratchLease<'a>, GraphCacheError> {
        let capacity = crate::scan::physical_thread_capacity()
            .map_err(GraphSearchError::Scan)
            .map_err(GraphCacheError::Search)?;
        self.checkout_with_capacity(graph, ef, accounting, cancellation, capacity)
    }

    fn checkout_with_capacity<'a>(
        &'a self,
        graph: GraphNodeBlocks<'_>,
        ef: usize,
        accounting: &Arc<Accounting>,
        cancellation: &QueryCancellation<'_>,
        capacity: usize,
    ) -> Result<GraphScratchLease<'a>, GraphCacheError> {
        if capacity == 0 {
            return Err(GraphCacheError::Search(GraphSearchError::Geometry(
                "graph scratch pool capacity is zero".to_owned(),
            )));
        }
        let mut state = self.state.lock().map_err(|_| {
            GraphCacheError::Store(StoreError::Synchronization {
                component: "graph search cache",
            })
        })?;
        loop {
            check_cancellation(cancellation)?;
            if !state.available.is_empty() || state.checked_out < capacity {
                break;
            }
            let waited = self
                .changed
                .wait_timeout(state, WAIT_POLL_INTERVAL)
                .map_err(|_| {
                    GraphCacheError::Store(StoreError::Synchronization {
                        component: "graph search cache",
                    })
                })?;
            state = waited.0;
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
            .available
            .last()
            .is_some_and(|cached| cached.scratch.supports(node_count, max_degree, ef));
        let scratch = if reusable {
            state.available.pop().ok_or_else(|| {
                GraphCacheError::Search(GraphSearchError::Geometry(
                    "graph scratch pool lost an available scratch".to_owned(),
                ))
            })?
        } else {
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
            let _replaced = state.available.pop();
            CachedScratch {
                scratch,
                _memory: memory,
            }
        };
        let checked_out = state.checked_out.checked_add(1).ok_or_else(|| {
            GraphCacheError::Search(GraphSearchError::Geometry(
                "graph scratch checkout count overflowed".to_owned(),
            ))
        })?;
        state.checked_out = checked_out;
        drop(state);
        Ok(GraphScratchLease {
            cache: self,
            scratch: Some(scratch),
            entries,
            entry_seed_discovered,
        })
    }
}

fn check_cancellation(cancellation: &QueryCancellation<'_>) -> Result<(), GraphSearchError> {
    match cancellation.check_graph() {
        Ok(()) => Ok(()),
        Err(crate::scan::ScanError::Cancelled { partial }) => {
            Err(GraphSearchError::Cancelled { partial })
        }
        Err(crate::scan::ScanError::Timeout { partial }) => {
            Err(GraphSearchError::Timeout { partial })
        }
        Err(crate::scan::ScanError::ReadCancelled { partial }) => {
            Err(GraphSearchError::ReadCancelled { partial })
        }
        Err(error) => Err(GraphSearchError::Scan(error)),
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
        if let Some(scratch) = self.scratch.take() {
            state.available.push(scratch);
        }
        state.checked_out -= 1;
        self.cache.changed.notify_one();
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::{Arc, Barrier, mpsc};
    use std::time::Duration;

    use super::{GraphCacheError, SegmentGraphSearchCache};
    use crate::graph::block::{
        EncodedNodeBlocks, GraphNodeBlockBuild, GraphNodeBlockInput, GraphNodeLayout,
        decode_node_blocks, encode_node_blocks,
    };
    use crate::graph::search::{GraphSearchError, GraphSearchScratch};
    use crate::lifecycle::stats::Accounting;
    use crate::lifecycle::{
        CancelToken, Deadline, OpenOptions, QueryCancellation, QueryControl, Store,
    };
    use crate::quant::Bit4Factors;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum CheckoutOutcome {
        Acquired,
        Timeout,
        OtherError,
    }

    fn graph_fixture() -> EncodedNodeBlocks {
        let layout = GraphNodeLayout::new(128, 128, 0).expect("graph cache fixture layout");
        let codes = [0_u8; 64];
        let node = GraphNodeBlockInput {
            codes: &codes,
            factors: Bit4Factors::from_persisted(1.0, 0.0, 0.0),
            flags: 1,
            neighbors: &[],
        };
        encode_node_blocks(GraphNodeBlockBuild {
            layout,
            nodes: &[node; 4],
        })
        .expect("graph cache fixture encodes")
    }

    fn checkout_outcome(
        result: Result<super::GraphScratchLease<'_>, GraphCacheError>,
    ) -> CheckoutOutcome {
        match result {
            Ok(_lease) => CheckoutOutcome::Acquired,
            Err(GraphCacheError::Search(GraphSearchError::Timeout { partial: false })) => {
                CheckoutOutcome::Timeout
            }
            Err(_) => CheckoutOutcome::OtherError,
        }
    }

    #[test]
    fn two_graph_queries_on_one_segment_run_concurrently() {
        let encoded = graph_fixture();
        let graph = decode_node_blocks(encoded.as_bytes()).expect("graph cache fixture decodes");
        let cache = SegmentGraphSearchCache::new();
        let accounting = Arc::new(Accounting::new(u64::MAX, u64::MAX));
        let directory = tempfile::tempdir().expect("temporary store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("store opens");
        let first = {
            let first_lease = store.snapshot().expect("first snapshot lease");
            let first_control = QueryControl::Cancel(CancelToken::new());
            let cancellation = QueryCancellation::new(&first_control, &first_lease);
            match cache.checkout_with_capacity(graph, 1, &accounting, &cancellation, 2) {
                Ok(first) => first,
                Err(_) => panic!("first scratch checkout failed"),
            }
        };
        let second_lease = store.snapshot().expect("second snapshot lease");
        let second_control = QueryControl::Cancel(CancelToken::new());
        let start = Arc::new(Barrier::new(2));
        let (sender, receiver) = mpsc::sync_channel(1);

        let timely = std::thread::scope(|scope| {
            let worker_start = Arc::clone(&start);
            let cache = &cache;
            let accounting = Arc::clone(&accounting);
            scope.spawn(move || {
                let cancellation = QueryCancellation::new(&second_control, &second_lease);
                worker_start.wait();
                let outcome = checkout_outcome(cache.checkout_with_capacity(
                    graph,
                    1,
                    &accounting,
                    &cancellation,
                    2,
                ));
                sender.send(outcome).expect("report second checkout");
            });
            start.wait();
            let timely = receiver.recv_timeout(Duration::from_millis(250));
            drop(first);
            if timely.is_err() {
                let _ = receiver.recv_timeout(Duration::from_secs(1));
            }
            timely
        });

        store.close().expect("close store");
        assert_eq!(timely, Ok(CheckoutOutcome::Acquired));
        let scratch_bytes = u64::try_from(
            GraphSearchScratch::allocation_bytes(
                graph.node_count(),
                graph.layout().max_degree(),
                1,
            )
            .expect("scratch allocation bytes"),
        )
        .expect("scratch allocation bytes fit u64");
        assert_eq!(
            accounting.audit().expect("accounting audit").cache_bytes,
            scratch_bytes
                .checked_mul(2)
                .expect("two scratch byte counts")
        );
    }

    #[test]
    fn a_waiting_graph_query_observes_its_deadline_while_blocked() {
        let encoded = graph_fixture();
        let graph = decode_node_blocks(encoded.as_bytes()).expect("graph cache fixture decodes");
        let cache = SegmentGraphSearchCache::new();
        let accounting = Arc::new(Accounting::new(u64::MAX, u64::MAX));
        let directory = tempfile::tempdir().expect("temporary store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("store opens");
        let first = {
            let first_lease = store.snapshot().expect("first snapshot lease");
            let first_control = QueryControl::Cancel(CancelToken::new());
            let cancellation = QueryCancellation::new(&first_control, &first_lease);
            match cache.checkout_with_capacity(graph, 1, &accounting, &cancellation, 1) {
                Ok(first) => first,
                Err(_) => panic!("capacity-filling scratch checkout failed"),
            }
        };
        let waiting_lease = store.snapshot().expect("waiting snapshot lease");
        let start = Arc::new(Barrier::new(2));
        let (sender, receiver) = mpsc::sync_channel(1);

        let timely = std::thread::scope(|scope| {
            let worker_start = Arc::clone(&start);
            let cache = &cache;
            let accounting = Arc::clone(&accounting);
            scope.spawn(move || {
                worker_start.wait();
                let control = QueryControl::Deadline(
                    Deadline::after(Duration::from_millis(10)).expect("short deadline"),
                );
                let cancellation = QueryCancellation::new(&control, &waiting_lease);
                let outcome = checkout_outcome(cache.checkout_with_capacity(
                    graph,
                    1,
                    &accounting,
                    &cancellation,
                    1,
                ));
                sender.send(outcome).expect("report waiting checkout");
            });
            start.wait();
            let timely = receiver.recv_timeout(Duration::from_millis(250));
            drop(first);
            if timely.is_err() {
                let _ = receiver.recv_timeout(Duration::from_secs(1));
            }
            timely
        });

        store.close().expect("close store");
        assert_eq!(timely, Ok(CheckoutOutcome::Timeout));
    }
}
